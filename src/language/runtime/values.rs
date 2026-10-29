// Values: what expressions of a build script evaluate to, their types, and the modules builds produce
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};

use crate::common::up_to_date::InputFingerprint;
use crate::common::{system, xml};
use crate::jvm::{self, JvmOutput};
use crate::native::NativeOutput;
use crate::language::lexer::Span;

use super::functions::{BuiltinFunction, FunctionSignature, UserFunction};

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Module(Arc<PendingModule>),
    Unit,
    Builtin {
        name: &'static str,
        signature: Arc<FunctionSignature>,
        function: BuiltinFunction,
    },
    /// defined by the script with fn (...) { ... }
    Function(Arc<UserFunction>),
}

/// Types of values, checked whenever arguments are bound to parameters. They
/// are written like in scripts: str, list<str>, fn(str) -> bool
#[derive(Debug, Clone)]
pub enum Type {
    Any,
    Unit,
    Bool,
    Int,
    String,
    Module,
    List(Box<Type>),
    Function(Arc<FunctionSignature>),
    /// one of several types: str | module
    Union(Vec<Type>),
}

/// What building a module produced. Other modules can depend on it, like
/// `implementation(project(":core"))` in Gradle.
#[derive(Debug)]
pub struct BuiltModule {
    pub name: String,
    /// where the module lives, programs run there
    pub directory: PathBuf,
    /// the file to run or install, what the module is in a script
    pub artifact: PathBuf,
    /// modules of the script it depends on
    pub modules: Vec<Arc<BuiltModule>>,
    pub output: ModuleOutput,
    /// empty when `build-script test` did not select it
    pub tests: Vec<TestSuite>,
}

/// A module that may still be building on its own thread, whatever needs it waits for it
#[derive(Debug)]
pub struct PendingModule {
    /// what builds it and where the script calls it, for its error
    pub function: &'static str,
    pub span: Span,
    result: Mutex<Option<Result<Arc<BuiltModule>, BuildFailure>>>,
    done: Condvar,
}

#[derive(Debug, Clone)]
pub enum BuildFailure {
    /// what the build function said
    Failed(String),
    /// a module it depends on failed, that one says why
    DependencyFailed,
}

/// Tests of one source set of a module, like src/test or src/androidTest
#[derive(Debug)]
pub struct TestSuite {
    pub source_set: &'static str,
    pub tests: ModuleTests,
}

/// The tests `build-script test` selected, run once the whole script is built
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
}

/// What `build-script test` found for a module. A failure is an outcome, not an
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

/// What a language category keeps of a module for the modules depending on it and for `build-script run`
#[derive(Debug)]
pub enum ModuleOutput {
    Jvm(JvmOutput),
    Native(NativeOutput),
}

/// Starts what a module built and returns the exit code for the process
pub fn launch(module: &BuiltModule, args: &[String]) -> Result<i32, String> {
    match &module.output {
        ModuleOutput::Jvm(output) => return jvm::launch(module, output, args),
        ModuleOutput::Native(output) => {
            let program = output.program.as_ref()
                .ok_or(format!("{} is a library, only programs can be run", module.name))?;

            // like jars, in the module's directory
            let status = Command::new(program)
                .args(args)
                .current_dir(&module.directory)
                .status()
                .map_err(|error| format!("could not start {}: {error}", program.display()))?;

            return Ok(status.code().unwrap_or(1));
        }
    }
}

impl Value {
    pub fn type_of(&self) -> Type {
        match self {
            Value::String(_) => return Type::String,
            Value::Int(_) => return Type::Int,
            Value::Bool(_) => return Type::Bool,
            Value::Module(_) => return Type::Module,
            Value::Unit => return Type::Unit,
            Value::List(items) => {
                let element = match items.split_first() {
                    Some((first, rest)) => {
                        let first = first.type_of();
                        if rest.iter().all(|item| item.type_of() == first) {
                            first
                        } else {
                            Type::Any
                        }
                    }
                    None => Type::Any,
                };
                return Type::list(element);
            }
            Value::Builtin { signature, .. } => return Type::Function(signature.clone()),
            Value::Function(function) => return Type::Function(function.signature.clone()),
        }
    }

    pub fn string_list(values: &[&str]) -> Value {
        return Value::List(values.iter().map(|value| Value::from(*value)).collect());
    }

    /// How the value is written in a script: strings with quotes
    pub fn literal(&self) -> String {
        match self {
            Value::String(str) => {
                let escaped = str.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\t', "\\t");
                return format!("\"{escaped}\"");
            }
            other => return other.to_string(),
        }
    }
}

impl Type {
    pub fn list(element: Type) -> Self {
        return Type::List(Box::new(element));
    }

    pub fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (Type::Any, _) => return true,
            (Type::Unit, Value::Unit) => return true,
            (Type::Bool, Value::Bool(_)) => return true,
            (Type::Int, Value::Int(_)) => return true,
            (Type::String, Value::String(_)) => return true,
            (Type::Module, Value::Module(_)) => return true,
            (Type::List(element), Value::List(items)) => {
                return items.iter().all(|item| element.accepts(item));
            }
            (Type::Union(types), _) => {
                return types.iter().any(|member_type| member_type.accepts(value));
            }
            (Type::Function(_), Value::Builtin { .. } | Value::Function(_)) => {
                return value.type_of() == *self;
            }
            _ => return false,
        }
    }
}

impl PendingModule {
    pub fn new(function: &'static str, span: Span) -> Arc<Self> {
        return Arc::new(PendingModule {
            function,
            span,
            result: Mutex::new(None),
            done: Condvar::new(),
        });
    }

    /// Of a build function that built it right away
    pub fn built(module: Arc<BuiltModule>) -> Arc<Self> {
        let pending = Self::new("", Span::new(0, 0));
        pending.finish(Ok(module));

        return pending;
    }

    pub fn finish(&self, result: Result<Arc<BuiltModule>, BuildFailure>) {
        *self.result.lock().unwrap() = Some(result);
        self.done.notify_all();
    }

    /// What the build gave, once it is done
    pub fn wait(&self) -> Result<Arc<BuiltModule>, BuildFailure> {
        let mut result = self.result.lock().unwrap();

        while result.is_none() {
            result = self.done.wait(result).unwrap();
        }

        return result.clone().unwrap();
    }
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
        };
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

        let result = system::run_command(&mut command, "tests");
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

impl BuiltModule {
    /// The given modules and everything they depend on, each once, dependencies first
    pub fn transitive(modules: &[Arc<BuiltModule>]) -> Vec<Arc<BuiltModule>> {
        // a module reached again is not descended into again: modules many
        // others depend on would be visited once per path to them
        fn visit(
            module: &Arc<BuiltModule>,
            seen: &mut HashSet<*const BuiltModule>,
            out: &mut Vec<Arc<BuiltModule>>,
        ) {
            if !seen.insert(Arc::as_ptr(module)) {
                return;
            }

            for dependency in &module.modules {
                visit(dependency, seen, out);
            }

            out.push(module.clone());
        }

        let mut seen = HashSet::new();
        let mut out = vec![];

        for module in modules {
            visit(module, &mut seen, &mut out);
        }

        return out;
    }

    /// Whether `build-script run` can start it
    pub fn can_launch(&self) -> bool {
        match &self.output {
            ModuleOutput::Jvm(output) => return output.launch.is_some(),
            ModuleOutput::Native(output) => return output.program.is_some(),
        }
    }
}

/// What fingerprints of build arguments see: modules by name and artifact,
/// what they produced is hashed once per module by the build that uses them
impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::String(str) => str.hash(state),
            Value::Int(int) => int.hash(state),
            Value::Bool(bool) => bool.hash(state),
            Value::List(items) => items.hash(state),
            Value::Module(module) => match module.wait() {
                Ok(module) => {
                    module.name.hash(state);
                    module.artifact.hash(state);
                }
                Err(_) => module.function.hash(state),
            },
            Value::Unit => {}
            Value::Builtin { name, .. } => name.hash(state),
            Value::Function(function) => function.name.hash(state),
        }
    }
}

/// Structural. Function types are equal with the same parameter types, required in the
/// same places; names and default values do not matter.
impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Type::Any, Type::Any)
            | (Type::Unit, Type::Unit)
            | (Type::Bool, Type::Bool)
            | (Type::Int, Type::Int)
            | (Type::String, Type::String)
            | (Type::Module, Type::Module) => return true,
            (Type::List(l), Type::List(r)) => return l == r,
            (Type::Union(l), Type::Union(r)) => return l == r,
            (Type::Function(l), Type::Function(r)) => return l.same_shape(r),
            _ => return false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::String(str) => return write!(f, "{str}"),
            Value::Int(int) => return write!(f, "{int}"),
            Value::Bool(bool) => return write!(f, "{bool}"),
            Value::Module(module) => match module.wait() {
                Ok(module) => return write!(f, "{}", module.artifact.display()),
                Err(_) => return write!(f, "<failed {}>", module.function),
            },
            Value::List(items) => {
                let items: Vec<String> = items.iter()
                    .map(|item| item.to_string())
                    .collect();
                return write!(f, "[{}]", items.join(", "));
            }
            Value::Unit => return write!(f, "()"),
            Value::Builtin { name, .. } => return write!(f, "<builtin {name}>"),
            Value::Function(function) => return write!(f, "<fn {}>", function.name),
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Type::Any => return write!(f, "any"),
            Type::Unit => return write!(f, "unit"),
            Type::Bool => return write!(f, "bool"),
            Type::Int => return write!(f, "int"),
            Type::String => return write!(f, "str"),
            Type::Module => return write!(f, "module"),
            Type::List(element) => return write!(f, "list<{element}>"),
            Type::Function(signature) => return write!(f, "{signature}"),
            Type::Union(types) => {
                let names: Vec<String> = types.iter()
                    .map(|member_type| member_type.to_string())
                    .collect();
                return write!(f, "{}", names.join(" | "));
            }
        }
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        return Value::String(value.to_string());
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        return Value::String(value);
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        return Value::Bool(value);
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        return Value::Int(value);
    }
}

impl From<Arc<BuiltModule>> for Value {
    fn from(value: Arc<BuiltModule>) -> Self {
        return Value::Module(PendingModule::built(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junit_reports_are_summed() {
        let dir = std::env::temp_dir().join(format!("build-script-reports-{}", std::process::id()));
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

    #[test]
    fn test_runs_count_their_own_reports_and_remember_passing() {
        let dir =
            std::env::temp_dir().join(format!("build-script-test-run-{}", std::process::id()));

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
