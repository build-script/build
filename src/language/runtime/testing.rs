// Tests of modules: test programs and JUnit runs, their counts from the reports, up to date once they passed
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::common::up_to_date::InputFingerprint;
use crate::common::{system, xml};

/// Tests of one source set of a module, like src/test or src/androidTest
#[derive(Debug)]
pub struct TestSuite {
    pub source_set: &'static str,
    pub tests: ModuleTests,
}

/// The tests `build test` selected, run once the whole script is built
#[derive(Debug)]
pub enum ModuleTests {
    Run(Box<dyn TestRunner>),
    /// clear without running them: no tests, up to date, not compiled
    Known(TestOutcome),
}

/// Runs compiled tests: a command line, or what a platform needs besides, like a device
pub trait TestRunner: fmt::Debug + Send + Sync {
    /// A failure is the outcome, not an error
    fn run(&self, module: &str) -> TestOutcome;
}

/// The command line that runs the compiled tests of a module
#[derive(Debug)]
pub struct TestRun {
    /// for the log: JUnit Platform 1.14.4
    pub runner: String,
    program: OsString,
    arguments: Vec<OsString>,
    /// None removes the variable
    environment: Vec<(OsString, Option<OsString>)>,
    directory: Option<PathBuf>,
    /// emptied before the run, so only the JUnit XML reports of this run count
    reports_dir: PathBuf,
    /// stored once the tests passed, they do not run again until their inputs change
    fingerprint: InputFingerprint,
    /// tests that run longer are stopped and failed
    timeout: Option<Duration>,
}

/// What `build test` found for a module. A failure is an outcome, not an
/// error: the other modules still run their tests.
#[derive(Debug, Clone, PartialEq)]
pub enum TestOutcome {
    NoTests,
    /// neither the tests nor what they test changed since they last passed
    UpToDate,
    Passed(Option<TestCounts>),
    /// the tests failed, or could not be compiled or started
    Failed {
        counts: Option<TestCounts>,
        error: String,
    },
}

/// From JUnit XML reports, which the test runners of most languages can write
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TestCounts {
    pub tests: u32,
    /// failures and errors
    pub failed: u32,
    pub skipped: u32,
}

impl ModuleTests {
    pub fn run(&self, module: &str) -> TestOutcome {
        match self {
            ModuleTests::Run(run) => return run.run(module),
            ModuleTests::Known(outcome) => return outcome.clone(),
        }
    }
}

impl TestRun {
    pub fn new(
        runner: String,
        command: &Command,
        reports_dir: PathBuf,
        fingerprint: InputFingerprint,
    ) -> Self {
        return TestRun {
            runner,
            program: command.get_program().to_os_string(),
            arguments: command.get_args().map(OsStr::to_os_string).collect(),
            environment: command.get_envs()
                .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
                .collect(),
            directory: command.get_current_dir().map(Path::to_path_buf),
            reports_dir,
            fingerprint,
            timeout: None,
        };
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        return self;
    }
}

impl TestRunner for TestRun {
    fn run(&self, module: &str) -> TestOutcome {
        system::log(module, &format!("Running tests with {}", self.runner));

        if let Err(error) = system::recreate_dir(&self.reports_dir) {
            return TestOutcome::Failed {
                counts: None,
                error,
            };
        }

        let mut command = Command::new(&self.program);
        command.args(&self.arguments);

        for (key, value) in &self.environment {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }

        if let Some(directory) = &self.directory {
            command.current_dir(directory);
        }

        let result = match self.timeout {
            Some(timeout) => system::run_command_within(&mut command, "tests", timeout),
            None => system::run_command(&mut command, "tests"),
        };
        let counts = TestCounts::read_junit_reports(&self.reports_dir);

        if let Err(error) = result {
            return TestOutcome::Failed { counts, error };
        }

        if let Err(error) = self.fingerprint.store() {
            system::log(
                module,
                &format!("the tests passed, but run again next time: {error}"),
            );
        }

        return TestOutcome::Passed(counts);
    }
}

impl TestCounts {
    /// The <testcase> elements of the TEST-*.xml reports in the directory, None without any. Not the
    /// counts of the suites: GoogleTest counts disabled tests there, doctest assertions.
    pub fn read_junit_reports(directory: &Path) -> Option<Self> {
        let mut counts: Option<TestCounts> = None;

        for report in system::files_with_extension(&[directory.to_path_buf()], "xml") {
            if !report.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("TEST-"))
            {
                continue;
            }

            let Ok(root) = system::read_file(&report).and_then(|content| xml::parse(&content))
            else {
                continue;
            };

            let total = counts.get_or_insert_with(TestCounts::default);

            // in a <testsuite>, which may be in <testsuites> or in another suite
            let mut pending = vec![&root];
            while let Some(element) = pending.pop() {
                if element.name != "testcase" {
                    pending.extend(&element.children);
                    continue;
                }

                total.tests += 1;

                // GoogleTest marks disabled tests with status="notrun"
                if element.child("skipped").is_some() || element.attribute("status") == Some("notrun") {
                    total.skipped += 1;
                } else if element.child("failure").is_some() || element.child("error").is_some() {
                    total.failed += 1;
                }
            }
        }

        return counts;
    }
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn junit_reports_are_summed() {
        let dir = ScratchDir::new("reports");
        system::recreate_dir(&dir).unwrap();
        assert_eq!(TestCounts::read_junit_reports(&dir), None);

        // one suite per engine, like the JUnit Platform writes them
        system::write_file(
            &dir.join("TEST-junit-jupiter.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="JUnit Jupiter" tests="3" skipped="1" failures="1" errors="0">
<testcase name="adds()"><system-out><![CDATA[<not a tag>]]></system-out></testcase>
<testcase name="later()"><skipped/></testcase>
<testcase name="fails()"><failure message="expected 2"/></testcase>
</testsuite>"#,
        )
        .unwrap();
        // GoogleTest counts the disabled test among the tests of the suite, doctest the assertions
        system::write_file(
            &dir.join("TEST-gtest.xml"),
            r#"<testsuites tests="2"><testsuite tests="2"><testcase name="a"/><testcase name="DISABLED_b" status="notrun"/></testsuite></testsuites>"#,
        )
        .unwrap();
        system::write_file(
            &dir.join("TEST-doctest.xml"),
            r#"<testsuites><testsuite tests="7"><testcase name="c"><error message="thrown"/></testcase></testsuite></testsuites>"#,
        )
        .unwrap();
        // not a report
        system::write_file(&dir.join("junit-platform-events.xml"), "<events/>").unwrap();

        assert_eq!(
            TestCounts::read_junit_reports(&dir),
            Some(TestCounts {
                tests: 6,
                failed: 2,
                skipped: 2
            })
        );
        system::remove_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_runs_count_their_own_reports_and_remember_passing() {
        let dir = ScratchDir::new("test-run");

        system::recreate_dir(&dir).unwrap();
        let fingerprint = dir.join("fingerprint");

        let test_run = |script: &str| {
            let mut command = Command::new("sh");
            command.arg("-c")
                .arg(script)
                .current_dir(&dir);

            return TestRun::new(
                "sh".to_string(),
                &command,
                dir.join("reports"),
                InputFingerprint::new(fingerprint.clone()),
            );
        };

        let failing =
            test_run(r#"echo '<testsuite><testcase/><testcase><failure/></testcase></testsuite>' > reports/TEST-sh.xml; exit 1"#);

        let counts = Some(TestCounts {
            tests: 2,
            failed: 1,
            skipped: 0,
        });

        assert_eq!(
            failing.run("core"),
            TestOutcome::Failed {
                counts,
                error: "tests failed (exit status: 1)".to_string()
            }
        );
        assert!(!fingerprint.exists());

        // the report of the failed run is gone
        assert_eq!(test_run("true").run("core"), TestOutcome::Passed(None));
        assert!(fingerprint.exists());
        system::remove_dir(&dir).unwrap();
    }
}
