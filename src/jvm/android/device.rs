// Devices: apps installed and started with adb, instrumented tests run on them, on an emulator when no device is connected
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::system::{self, HostArchitecture};
use crate::common::xml::{self, XmlElement};
use crate::language::runtime::{TestCounts, TestOutcome, TestRunner};

use super::sdk::{self, AndroidSdk, SDK_REPOSITORY, SdkCatalog};

/// catalogs of system images: directory below sys-img/ and how it is shown.
/// "android" holds the plain open source Android (AOSP).
const IMAGE_SOURCES: &[(&str, &str)] = &[
    ("android", "Android without Google services"),
    ("google_apis", "Google services without Play Store"),
    ("google_apis_playstore", "Google services with Play Store"),
];

/// newest images offered per source
const IMAGES_PER_SOURCE: usize = 3;
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);

/// An apk that instruments itself: installed, run with `am instrument`, results
/// written as JUnit XML, then uninstalled like the Gradle plugin does it
#[derive(Debug)]
pub struct InstrumentedTests {
    pub apk: PathBuf,
    pub sdk_root: PathBuf,
    /// application id of the apk
    pub package: String,
    pub runner: String,
    /// lowest API level an emulator for the tests may have
    pub min_sdk: i64,
    pub reports_dir: PathBuf,
}

/// Installs the apk with adb and starts its launcher activity. Without a
/// connected device the user is guided to an emulator.
pub fn install_and_start(
    apk: &Path,
    sdk_root: &Path,
    application_id: &str,
    activity: Option<&str>,
    min_sdk: i64,
) -> Result<(), String> {
    let Some(activity) = activity else {
        return Err(format!(
            "{application_id} has no activity with a MAIN/LAUNCHER intent filter"
        ));
    };

    let adb = AndroidSdk::adb(sdk_root)?;
    let device = Adb::of(&adb, target_device(&adb, sdk_root, min_sdk)?);
    println!("Installing {} on {}", apk.display(), device.serial);
    system::run_command(&mut device.install(apk), "adb install")?;

    println!("Starting {application_id}/{activity}");
    let component = format!("{application_id}/{activity}");

    return system::run_command(&mut device.shell(&["am", "start", "-W", "-n", &component]), "adb shell am start");
}

/// adb for one device, `adb -s <serial>`
struct Adb<'a> {
    adb: &'a Path,
    serial: String,
}

impl<'a> Adb<'a> {
    fn of(adb: &'a Path, serial: String) -> Self {
        return Adb { adb, serial };
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.adb);
        command.arg("-s").arg(&self.serial);
        return command;
    }

    /// Replaces an installed version, test apks included
    fn install(&self, apk: &Path) -> Command {
        let mut command = self.command();
        command.args(["install", "-r", "-t"]).arg(apk);
        return command;
    }

    fn shell(&self, arguments: &[&str]) -> Command {
        let mut command = self.command();
        command.arg("shell").args(arguments);
        return command;
    }
}

impl TestRunner for InstrumentedTests {
    fn run(&self, module: &str) -> TestOutcome {
        return self.install_and_instrument(module)
            .unwrap_or_else(|error| TestOutcome::Failed {
                counts: None,
                error,
            });
    }
}

impl InstrumentedTests {
    fn install_and_instrument(&self, module: &str) -> Result<TestOutcome, String> {
        let adb = AndroidSdk::adb(&self.sdk_root)?;
        let device = Adb::of(&adb, target_device(&adb, &self.sdk_root, self.min_sdk)?);
        system::recreate_dir(&self.reports_dir)?;
        system::log(
            module,
            &format!("Installing {} on {}", self.apk.display(), device.serial),
        );
        system::run_command_quietly(&mut device.install(&self.apk), "adb install")?;

        system::log(
            module,
            &format!(
                "Running instrumented tests with {} on {}",
                self.runner,
                device.serial
            ),
        );
        let instrumentation = instrument(&device, &format!("{}/{}", self.package, self.runner));

        let mut uninstall = device.command();
        uninstall.args(["uninstall", &self.package]);
        let uninstalled = system::run_command_quietly(&mut uninstall, "adb uninstall");
        let instrumentation = instrumentation?;
        uninstalled?;

        let report = instrumentation.junit_report(&format!("{module} on {}", device.serial));
        system::write_file(
            &self.reports_dir.join(format!("TEST-{}.xml", self.package)),
            &xml::write(&report, &|_| false),
        )?;
        let counts = TestCounts::read_junit_reports(&self.reports_dir, "TEST-");

        if let Some(error) = instrumentation.problem() {
            return Ok(TestOutcome::Failed { counts, error });
        }

        return Ok(TestOutcome::Passed(counts));
    }
}

/// `am instrument -r` with its results read while the tests run, each test shown once it finished
fn instrument(device: &Adb, component: &str) -> Result<Instrumentation, String> {
    let mut child = device.shell(&["am", "instrument", "-w", "-r", component])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", device.adb.display()))?;

    let mut instrumentation = Instrumentation::default();
    let stdout = child.stdout.take().expect("stdout is piped");

    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|error| format!("could not read from adb: {error}"))?;
        if let Some(test) = instrumentation.read_line(line.trim_end_matches('\r')) {
            println!("  {}", test.describe());
        }
    }

    let status = child.wait()
        .map_err(|error| format!("adb failed: {error}"))?;

    if !status.success() && instrumentation.code.is_none() {
        return Err(format!("am instrument failed ({status})"));
    }

    return Ok(instrumentation);
}

/// The device adb works with: the one ANDROID_SERIAL names, the only one connected,
/// or an emulator when none is
fn target_device(adb: &Path, sdk_root: &Path, min_sdk: i64) -> Result<String, String> {
    match choose_device(adb_devices(adb)?, std::env::var("ANDROID_SERIAL").ok())? {
        Some(serial) => return Ok(serial),
        None => return start_or_set_up_emulator(adb, sdk_root, min_sdk),
    }
}

/// Of the serials and states of `adb devices`: the device `serial` (ANDROID_SERIAL) names,
/// or the only one connected. None when no device is connected.
fn choose_device(attached: Vec<(String, String)>, serial: Option<String>) -> Result<Option<String>, String> {
    if let Some((serial, _)) = attached.iter().find(|(_, state)| state == "unauthorized") {
        return Err(format!(
            "{serial} does not allow USB debugging yet, confirm the prompt on the device and run again"
        ));
    }

    let devices: Vec<String> = attached.into_iter()
        .filter(|(_, state)| state == "device")
        .map(|(serial, _)| serial)
        .collect();

    // adb picks the device from ANDROID_SERIAL itself
    match (devices.as_slice(), serial) {
        ([], _) => return Ok(None),
        (_, Some(serial)) => return Ok(Some(serial)),
        ([device], None) => return Ok(Some(device.clone())),
        (_, None) => {
            return Err(format!(
                "several devices are connected ({}), choose one with ANDROID_SERIAL",
                devices.join(", ")
            ));
        }
    }
}

/// The index of what the user chose of `count` numbered entries, the first without an answer
fn choose(answer: &str, count: usize) -> Result<usize, String> {
    if answer.is_empty() {
        return Ok(0);
    }

    return answer.parse::<usize>()
        .ok()
        .filter(|number| (1..=count).contains(number))
        .map(|number| number - 1)
        .ok_or_else(|| "cancelled".to_string());
}

/// When no device is connected: an emulator build-script set up, or a new one
/// the user sets up here. Returns its serial once it booted.
fn start_or_set_up_emulator(adb: &Path, sdk_root: &Path, min_sdk: i64) -> Result<String, String> {
    if !io::stdin().is_terminal() {
        return Err("no Android device or emulator is connected (see adb devices), run in a terminal to set up an emulator".to_string());
    }

    println!("\nNo Android device or emulator is connected.");
    // AVDs live in the build home instead of ~/.android/avd
    let home = system::data_dir("android/avd");
    let avds = list_virtual_devices(&home);

    let name = if avds.is_empty() {
        set_up_virtual_device(sdk_root, &home, min_sdk)?
    } else {
        println!("\nEmulators set up by build-script:");
        for (index, avd) in avds.iter().enumerate() {
            println!("  {}) {}", index + 1, avd.display);
        }

        println!("  n) Set up a new emulator");

        match system::prompt("Choose [1]: ")?.as_str() {
            "n" => set_up_virtual_device(sdk_root, &home, min_sdk)?,
            answer => avds[choose(answer, avds.len())?].name.clone(),
        }
    };

    let emulator = sdk::ensure_package(sdk_root, "emulator", false)?
        .join(format!("emulator{}", std::env::consts::EXE_SUFFIX));

    return start_emulator(adb, &emulator, sdk_root, &home, &name);
}

/// Lets the user pick a system image, installs it with the emulator and creates the AVD
fn set_up_virtual_device(sdk_root: &Path, home: &Path, min_sdk: i64) -> Result<String, String> {
    // what runs fast on this machine
    let abi = match system::host_architecture() {
        Ok(HostArchitecture::Aarch64) => "arm64-v8a",
        _ => "x86_64",
    };
    println!("Loading the list of system images ...");
    let (catalogs, images) = offered_images(abi, min_sdk)?;

    println!("\nSystem images for {abi} (the app needs API {min_sdk} or newer):");
    let source_width = IMAGE_SOURCES.iter()
        .map(|(_, label)| label.len())
        .max()
        .unwrap_or(0);

    for (number, (catalog_index, image)) in images.iter().enumerate() {
        let source = IMAGE_SOURCES[*catalog_index].1;
        println!(
            "  {}) API {:<6} {:<source_width$} {}",
            number + 1,
            image.version,
            source,
            system::format_size(image.size)
        );
    }

    let chosen = choose(&system::prompt("Choose [1]: ")?, images.len())?;
    let (catalog_index, mut image) = images[chosen].clone();

    image.tag_display = IMAGE_SOURCES[catalog_index].1.to_string();

    let main_catalog = SdkCatalog::load()?;
    confirm_download(sdk_root, &main_catalog, &image)?;

    main_catalog.install(sdk_root, "emulator", false)?;
    catalogs[catalog_index].install(sdk_root, &image.path, false)?;

    let name = create_virtual_device(home, &image)?;
    println!("Created emulator {name}");

    return Ok(name);
}

/// The newest images of every source with the index of their catalog, and the catalogs
fn offered_images(abi: &str, min_sdk: i64) -> Result<(Vec<SdkCatalog>, Vec<(usize, SystemImage)>), String> {
    let mut catalogs = vec![];

    for (source, _) in IMAGE_SOURCES {
        catalogs.push(SdkCatalog::load_from(&format!(
            "{SDK_REPOSITORY}/sys-img/{source}/sys-img2-3.xml"
        ))?);
    }

    let mut images = vec![];

    for (index, catalog) in catalogs.iter().enumerate() {
        for image in system_images(catalog, abi, min_sdk)
            .into_iter()
            .take(IMAGES_PER_SOURCE)
        {
            images.push((index, image));
        }
    }

    if images.is_empty() {
        return Err(format!(
            "there is no system image for {abi} with API {min_sdk} or newer"
        ));
    }

    return Ok((catalogs, images));
}

/// Asks before the emulator and the image are downloaded, unless both are there
fn confirm_download(sdk_root: &Path, main_catalog: &SdkCatalog, image: &SystemImage) -> Result<(), String> {
    let emulator_size = if sdk_root.join("emulator").is_dir() {
        0
    } else {
        main_catalog.size("emulator").unwrap_or(0)
    };

    let image_size = if sdk_root.join(image.directory()).is_dir() { 0 } else { image.size };

    if emulator_size + image_size == 0 {
        return Ok(());
    }

    println!(
        "\nThis downloads {} to {}",
        system::format_size(emulator_size + image_size),
        sdk_root.display()
    );

    if !matches!(
        system::prompt("Continue? (Y/n): ")?.as_str(),
        "" | "y" | "yes"
    ) {
        return Err("cancelled".to_string());
    }

    return Ok(());
}

/// Starts the emulator in the background, it keeps running after build-script
/// ends. Waits until Android booted.
fn start_emulator(
    adb: &Path,
    emulator: &Path,
    sdk_root: &Path,
    home: &Path,
    name: &str,
) -> Result<String, String> {
    // the console port decides the serial: emulator-<port>
    let used: Vec<String> = adb_devices(adb)?
        .into_iter()
        .map(|(serial, _)| serial)
        .collect();

    let port = (5554..=5584).step_by(2)
        .find(|port| !used.contains(&format!("emulator-{port}")))
        .ok_or("all emulator ports are in use")?;

    let device = Adb::of(adb, format!("emulator-{port}"));
    let serial = &device.serial;

    let log_path = home.join(format!("{name}.log"));

    let log = fs::File::create(&log_path)
        .map_err(|error| format!("could not create {}: {error}", log_path.display()))?;

    let log_err = log.try_clone().map_err(|error| error.to_string())?;

    let mut command = Command::new(emulator);
    command.arg("-avd")
        .arg(name)
        .arg("-port")
        .arg(port.to_string())
        .args(["-no-snapshot-save", "-no-boot-anim"])
        // e.g. -no-window for machines without a display
        .args(
            std::env::var("BUILD_SCRIPT_EMULATOR_ARGS")
                .unwrap_or_default()
                .split_whitespace(),
        )
        .env("ANDROID_SDK_ROOT", sdk_root)
        .env("ANDROID_HOME", sdk_root)
        .env("ANDROID_AVD_HOME", home)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);

    // its own process group, so Ctrl+C in build-script does not stop it
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let child = command.spawn()
        .map_err(|error| format!("could not start {}: {error}", emulator.display()))?;

    println!(
        "Starting emulator {name} as {serial} (log: {})",
        log_path.display()
    );

    wait_for_boot(&device, child, &log_path)?;

    return Ok(device.serial);
}

/// Until Android on the emulator `child` reports it booted, or it stopped
fn wait_for_boot(device: &Adb, mut child: Child, log_path: &Path) -> Result<(), String> {
    let serial = &device.serial;
    let started = Instant::now();
    let mut reported = 0;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "the emulator stopped ({status}), see {}",
                log_path.display()
            ));
        }

        let booted = system::command_output(&mut device.shell(&["getprop", "sys.boot_completed"]), "adb getprop")
            .is_ok_and(|output| output.trim() == "1");

        if booted {
            println!("{serial} booted after {}s", started.elapsed().as_secs());
            return Ok(());
        }

        let elapsed = started.elapsed();

        if elapsed > BOOT_TIMEOUT {
            return Err(format!(
                "{serial} did not boot within {}s, see {}",
                BOOT_TIMEOUT.as_secs(),
                log_path.display()
            ));
        }

        if elapsed.as_secs() / 15 > reported {
            reported = elapsed.as_secs() / 15;
            println!("Waiting for {serial} to boot ({}s)", elapsed.as_secs());
        }

        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Serial and state of every device adb knows: device, offline, unauthorized
fn adb_devices(adb: &Path) -> Result<Vec<(String, String)>, String> {
    let output = system::command_output(Command::new(adb).arg("devices"), "adb devices")?;

    return Ok(output.lines()
        .skip(1)
        .filter_map(|line| line.split_once('\t'))
        .map(|(serial, state)| (serial.to_string(), state.trim().to_string()))
        .collect());
}

/// Stable images of a catalog for the ABI with an API level of at least
/// `min_sdk`, newest first. Previews and 16 KB page size variants are left out.
fn system_images(catalog: &SdkCatalog, abi: &str, min_sdk: i64) -> Vec<SystemImage> {
    let mut images: Vec<SystemImage> = catalog.packages.iter()
        .filter_map(|remote| {
            let parts: Vec<&str> = remote.path.split(';').collect();

            if parts.len() != 4 || parts[0] != "system-images" || parts[3] != abi {
                return None;
            }

            let preview =
                parts[1].contains("-beta") || parts[2].ends_with("_ps16k") || remote.obsolete;

            if !remote.is_stable() || preview {
                return None;
            }

            // newer images write 37.0, older ones 36
            let api: i64 = remote.details.api_level.as_deref()?
                .trim()
                .split('.')
                .next()?
                .parse()
                .ok()?;

            if api < min_sdk {
                return None;
            }

            let size = remote.archive()?.complete.as_ref()?.size;

            let tag_display = remote.details.tags.first()
                .and_then(|tag| tag.display.as_deref())
                .map(str::trim);

            return Some(SystemImage {
                path: remote.path.clone(),
                api,
                version: parts[1].trim_start_matches("android-").to_string(),
                tag: parts[2].to_string(),
                tag_display: tag_display.filter(|display| !display.is_empty())
                    .unwrap_or(parts[2])
                    .to_string(),
                abi: abi.to_string(),
                size,
            });
        })
        .collect();

    images.sort_by(|left, right| {
        right.api.cmp(&left.api)
            .then(right.version.cmp(&left.version))
    });

    return images;
}

/// The AVDs build-script set up
fn list_virtual_devices(home: &Path) -> Vec<AndroidVirtualDevice> {
    let Ok(entries) = fs::read_dir(home) else {
        return vec![];
    };

    let mut avds: Vec<AndroidVirtualDevice> = entries.flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "ini"))
        .filter_map(|path| {
            let name = path.file_stem()?
                .to_string_lossy()
                .to_string();

            let config = fs::read_to_string(home.join(format!("{name}.avd/config.ini"))).ok()?;
            let display = config.lines()
                .find_map(|line| line.strip_prefix("avd.ini.displayname="))
                .unwrap_or(&name)
                .to_string();

            return Some(AndroidVirtualDevice { name, display });
        })
        .collect();

    avds.sort_by(|left, right| left.name.cmp(&right.name));

    return avds;
}

/// Writes the files avdmanager would write, returns the AVD name
fn create_virtual_device(home: &Path, image: &SystemImage) -> Result<String, String> {
    let name = format!(
        "build-script-api{}-{}",
        image.version.replace('.', "_"),
        image.tag
    );

    let avd_dir = home.join(format!("{name}.avd"));

    let cpu = if image.abi == "arm64-v8a" {
        "arm64"
    } else {
        "x86_64"
    };

    let play_store = image.tag.contains("playstore");

    let config = [
        format!("AvdId={name}"),
        format!(
            "avd.ini.displayname=Android API {} ({}, {})",
            image.version, image.tag_display, image.abi
        ),
        "avd.ini.encoding=UTF-8".to_string(),
        format!("abi.type={}", image.abi),
        format!("hw.cpu.arch={cpu}"),
        format!("image.sysdir.1={}/", image.directory()),
        format!("tag.id={}", image.tag),
        format!("tag.display={}", image.tag_display),
        format!("target=android-{}", image.api),
        format!("PlayStore.enabled={play_store}"),
        // a phone like a Pixel
        "hw.lcd.width=1080".to_string(),
        "hw.lcd.height=2400".to_string(),
        "hw.lcd.density=420".to_string(),
        "hw.ramSize=2048".to_string(),
        "hw.keyboard=yes".to_string(),
        "hw.gpu.enabled=yes".to_string(),
        "hw.gpu.mode=auto".to_string(),
        "disk.dataPartition.size=6442450944".to_string(),
        "showDeviceFrame=no".to_string(),
    ];

    system::write_file(&avd_dir.join("config.ini"), &(config.join("\n") + "\n"))?;

    let ini = format!(
        "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{name}.avd\ntarget=android-{}\n",
        avd_dir.display(),
        image.api,
    );

    system::write_file(&home.join(format!("{name}.ini")), &ini)?;

    return Ok(name);
}

/// What `am instrument -r` reports line by line: INSTRUMENTATION_STATUS blocks per test
/// event ending in a status code, then INSTRUMENTATION_RESULT lines and the run's code.
#[derive(Debug, Default)]
struct Instrumentation {
    tests: Vec<InstrumentedTest>,
    /// INSTRUMENTATION_RESULT values, shortMsg says why the tests did not finish
    result: Vec<(String, String)>,
    /// INSTRUMENTATION_FAILED, the runner did not start
    failed: Option<String>,
    code: Option<i64>,
    /// of the status block being read
    status: Vec<(String, String)>,
    reading_result: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct InstrumentedTest {
    class: String,
    name: String,
    outcome: InstrumentedOutcome,
}

#[derive(Debug, Clone, PartialEq)]
enum InstrumentedOutcome {
    Passed,
    /// with the stack trace
    Failed(String),
    /// ignored, or an assumption did not hold
    Skipped,
}

impl Instrumentation {
    /// Returns the test that just finished
    fn read_line(&mut self, line: &str) -> Option<&InstrumentedTest> {
        let value_of = |pair: &str| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        };

        if let Some(pair) = line.strip_prefix("INSTRUMENTATION_STATUS: ") {
            self.status.push(value_of(pair));
            self.reading_result = false;
        } else if let Some(code) = line.strip_prefix("INSTRUMENTATION_STATUS_CODE: ") {
            let status = std::mem::take(&mut self.status);
            let value = |key: &str| {
                status.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default()
            };

            let outcome = match code.trim() {
                "0" => InstrumentedOutcome::Passed,
                // -2 a failed assertion, -1 an exception
                "-1" | "-2" => InstrumentedOutcome::Failed(value("stack")),
                "-3" | "-4" => InstrumentedOutcome::Skipped,
                // 1: the test started
                _ => return None,
            };

            self.tests.push(InstrumentedTest {
                class: value("class"),
                name: value("test"),
                outcome,
            });

            return self.tests.last();
        } else if let Some(pair) = line.strip_prefix("INSTRUMENTATION_RESULT: ") {
            self.result.push(value_of(pair));
            self.reading_result = true;
        } else if let Some(code) = line.strip_prefix("INSTRUMENTATION_CODE: ") {
            self.code = code.trim()
                .parse()
                .ok();
        } else if let Some(message) = line.strip_prefix("INSTRUMENTATION_FAILED: ") {
            self.failed = Some(message.to_string());
        } else {
            let last = if self.reading_result {
                self.result.last_mut()
            } else {
                self.status.last_mut()
            };

            if let Some((_, value)) = last {
                value.push('\n');
                value.push_str(line);
            }
        }

        return None;
    }

    /// Why the run failed, None if every test passed
    fn problem(&self) -> Option<String> {
        if let Some(failed) = &self.failed {
            return Some(format!("the instrumentation did not start: {failed}"));
        }

        if let Some((_, message)) = self.result.iter().find(|(key, _)| key == "shortMsg") {
            return Some(format!("the tests did not finish: {}", message.trim()));
        }

        if self.code.is_none() {
            return Some("the instrumentation ended without a result".to_string());
        }

        let failed = self.tests.iter()
            .filter(|test| matches!(test.outcome, InstrumentedOutcome::Failed(_)))
            .count();

        if failed > 0 {
            return Some(format!("{failed} instrumented test(s) failed"));
        }

        return None;
    }

    /// One <testsuite> like the JUnit Platform writes them
    fn junit_report(&self, name: &str) -> XmlElement {
        let count = |wanted: fn(&InstrumentedOutcome) -> bool| {
            self.tests.iter()
                .filter(|test| wanted(&test.outcome))
                .count()
                .to_string()
        };

        let mut suite = XmlElement {
            name: "testsuite".to_string(),
            attributes: vec![
                ("name".to_string(), name.to_string()),
                ("tests".to_string(), self.tests.len().to_string()),
                (
                    "failures".to_string(),
                    count(|outcome| matches!(outcome, InstrumentedOutcome::Failed(_))),
                ),
                ("errors".to_string(), "0".to_string()),
                (
                    "skipped".to_string(),
                    count(|outcome| *outcome == InstrumentedOutcome::Skipped),
                ),
            ],
            ..XmlElement::default()
        };

        for test in &self.tests {
            let mut case = XmlElement {
                name: "testcase".to_string(),
                attributes: vec![
                    ("classname".to_string(), test.class.clone()),
                    ("name".to_string(), test.name.clone()),
                ],
                ..XmlElement::default()
            };

            match &test.outcome {
                InstrumentedOutcome::Passed => {}
                InstrumentedOutcome::Failed(stack) => case.children.push(XmlElement {
                    name: "failure".to_string(),
                    attributes: vec![(
                        "message".to_string(),
                        stack.lines().next().unwrap_or_default().to_string(),
                    )],
                    text: stack.clone(),
                    ..XmlElement::default()
                }),
                InstrumentedOutcome::Skipped => case.children.push(XmlElement {
                    name: "skipped".to_string(),
                    ..XmlElement::default()
                }),
            }

            suite.children.push(case);
        }

        return suite;
    }
}

impl InstrumentedTest {
    /// A line of the test log, failures with the first line of their stack trace
    fn describe(&self) -> String {
        let test = format!(
            "{} > {}",
            self.class.rsplit('.').next().unwrap_or_default(),
            self.name
        );

        match &self.outcome {
            InstrumentedOutcome::Passed => return format!("✔ {test}"),
            InstrumentedOutcome::Failed(stack) => {
                return format!("✘ {test}: {}", stack.lines().next().unwrap_or_default());
            }
            InstrumentedOutcome::Skipped => return format!("↷ {test} skipped"),
        }
    }
}

/// A system image the emulator can boot
#[derive(Debug, Clone, PartialEq)]
struct SystemImage {
    /// system-images;android-36;default;arm64-v8a
    path: String,
    api: i64,
    /// as in the path: 36 or 37.0
    version: String,
    tag: String,
    tag_display: String,
    abi: String,
    size: u64,
}

impl SystemImage {
    /// Where the image is installed in the SDK, relative to its root
    fn directory(&self) -> String {
        return self.path.replace(';', "/");
    }
}

/// An emulator (AVD) set up by build-script
#[derive(Debug, PartialEq)]
struct AndroidVirtualDevice {
    name: String,
    display: String,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::common::test_support::ScratchDir;
    use crate::jvm::android::sdk::tests::catalog;

    use super::*;

    #[test]
    fn the_named_or_only_connected_device_is_chosen() {
        let attached = |devices: &[(&str, &str)]| -> Vec<(String, String)> {
            return devices.iter()
                .map(|(serial, state)| (serial.to_string(), state.to_string()))
                .collect();
        };

        let one = attached(&[("emulator-5580", "device"), ("emulator-5582", "offline")]);
        assert_eq!(choose_device(one, None).unwrap(), Some("emulator-5580".to_string()));

        let several = attached(&[("emulator-5580", "device"), ("R58M", "device")]);
        let error = choose_device(several.clone(), None).unwrap_err();
        assert!(error.contains("ANDROID_SERIAL"));
        assert_eq!(choose_device(several, Some("R58M".to_string())).unwrap(), Some("R58M".to_string()));

        // without a connected device an emulator starts, ANDROID_SERIAL or not
        assert_eq!(choose_device(attached(&[("emulator-5582", "offline")]), Some("R58M".to_string())).unwrap(), None);

        let unauthorized = attached(&[("emulator-5580", "device"), ("R58M", "unauthorized")]);
        let error = choose_device(unauthorized, None).unwrap_err();
        assert!(error.contains("R58M"));
    }

    #[test]
    fn a_choice_is_one_of_the_numbers_shown() {
        assert_eq!(choose("", 3), Ok(0));
        assert_eq!(choose("3", 3), Ok(2));
        assert!(choose("0", 3).is_err());
        assert!(choose("4", 3).is_err());
        assert!(choose("x", 3).is_err());
    }

    #[test]
    fn instrumentation_output_becomes_tests_and_a_report() {
        let output = "INSTRUMENTATION_STATUS: class=com.example.GreetingTest
INSTRUMENTATION_STATUS: current=1
INSTRUMENTATION_STATUS: id=AndroidJUnitRunner
INSTRUMENTATION_STATUS: numtests=3
INSTRUMENTATION_STATUS: stream=
com.example.GreetingTest:
INSTRUMENTATION_STATUS: test=greets
INSTRUMENTATION_STATUS_CODE: 1
INSTRUMENTATION_STATUS: class=com.example.GreetingTest
INSTRUMENTATION_STATUS: stream=.
INSTRUMENTATION_STATUS: test=greets
INSTRUMENTATION_STATUS_CODE: 0
INSTRUMENTATION_STATUS: class=com.example.GreetingTest
INSTRUMENTATION_STATUS: test=fails
INSTRUMENTATION_STATUS_CODE: 1
INSTRUMENTATION_STATUS: class=com.example.GreetingTest
INSTRUMENTATION_STATUS: stack=java.lang.AssertionError: expected:<Hi> but was:<Hello>
\tat org.junit.Assert.fail(Assert.java:89)
INSTRUMENTATION_STATUS: stream=
Error in fails(com.example.GreetingTest):
INSTRUMENTATION_STATUS: test=fails
INSTRUMENTATION_STATUS_CODE: -2
INSTRUMENTATION_STATUS: class=com.example.GreetingTest
INSTRUMENTATION_STATUS: test=later
INSTRUMENTATION_STATUS_CODE: -3
INSTRUMENTATION_RESULT: stream=

Time: 0.05
FAILURES!!!
Tests run: 2,  Failures: 1

INSTRUMENTATION_CODE: -1";

        let mut instrumentation = Instrumentation::default();

        let finished: Vec<String> = output.lines()
            .filter_map(|line| {
                instrumentation.read_line(line)
                    .map(InstrumentedTest::describe)
            })
            .collect();

        assert_eq!(
            finished,
            [
                "✔ GreetingTest > greets",
                "✘ GreetingTest > fails: java.lang.AssertionError: expected:<Hi> but was:<Hello>",
                "↷ GreetingTest > later skipped",
            ]
        );
        assert_eq!(
            instrumentation.problem().as_deref(),
            Some("1 instrumented test(s) failed")
        );

        let report = instrumentation.junit_report("app");
        assert_eq!(report.attribute("tests"), Some("3"));
        assert_eq!(report.attribute("failures"), Some("1"));
        assert_eq!(report.attribute("skipped"), Some("1"));
        let failure = report.children[1].child("failure").unwrap();
        assert!(
            failure
                .text
                .ends_with("\tat org.junit.Assert.fail(Assert.java:89)"),
            "{}",
            failure.text
        );

        // a crash ends the run without a test failing
        let mut crashed = Instrumentation::default();

        for line in [
            "INSTRUMENTATION_RESULT: shortMsg=Process crashed.",
            "INSTRUMENTATION_CODE: 0",
        ] {
            crashed.read_line(line);
        }

        assert_eq!(
            crashed.problem().as_deref(),
            Some("the tests did not finish: Process crashed.")
        );
        let mut not_started = Instrumentation::default();
        not_started.read_line(
            "INSTRUMENTATION_FAILED: com.example.test/androidx.test.runner.AndroidJUnitRunner",
        );
        assert!(
            not_started
                .problem()
                .unwrap()
                .starts_with("the instrumentation did not start")
        );
    }

    fn image_package(path: &str, channel: &str, api: &str, size: &str) -> String {
        return format!(
            r#"<remotePackage path="{path}"><channelRef ref="{channel}"/>
            <type-details><api-level>{api}</api-level><tag><id>default</id><display>AOSP</display></tag></type-details>
            <archives><archive><complete><size>{size}</size><url>image.zip</url></complete></archive></archives>
        </remotePackage>"#
        );
    }

    #[test]
    fn emulator_images_are_filtered_for_the_host() {
        let packages = [
            image_package(
                "system-images;android-36;default;arm64-v8a",
                "channel-0",
                "36",
                "810000000",
            ),
            image_package(
                "system-images;android-35;default;arm64-v8a",
                "channel-0",
                "35",
                "769000000",
            ),
            image_package(
                "system-images;android-37.0;default;arm64-v8a",
                "channel-0",
                "37.0",
                "900000000",
            ),
            image_package(
                "system-images;android-23;default;arm64-v8a",
                "channel-0",
                "23",
                "400000000",
            ),
            image_package(
                "system-images;android-37;default;arm64-v8a",
                "channel-2",
                "37",
                "900000000",
            ),
            image_package(
                "system-images;android-37.2-beta1;default;arm64-v8a",
                "channel-0",
                "37",
                "900000000",
            ),
            image_package(
                "system-images;android-36;google_apis_ps16k;arm64-v8a",
                "channel-0",
                "36",
                "900000000",
            ),
            image_package(
                "system-images;android-36;default;x86_64",
                "channel-0",
                "36",
                "800000000",
            ),
        ]
        .concat();

        let images = system_images(&catalog(&packages), "arm64-v8a", 24);

        let found: Vec<(&str, i64, &str)> = images.iter()
            .map(|image| (image.path.as_str(), image.api, image.tag_display.as_str()))
            .collect();

        assert_eq!(
            found,
            [
                ("system-images;android-37.0;default;arm64-v8a", 37, "AOSP"),
                ("system-images;android-36;default;arm64-v8a", 36, "AOSP"),
                ("system-images;android-35;default;arm64-v8a", 35, "AOSP"),
            ]
        );
        assert_eq!(
            images[1].directory(),
            "system-images/android-36/default/arm64-v8a"
        );
        assert_eq!(system::format_size(images[1].size), "810 MB");
        assert_eq!(system::format_size(2_194_000_000), "2.2 GB");
    }

    #[test]
    fn avds_are_written_and_listed() {
        let home = ScratchDir::new("avd");

        let image = SystemImage {
            path: "system-images;android-36;google_apis_playstore;arm64-v8a".to_string(),
            api: 36,
            version: "36".to_string(),
            tag: "google_apis_playstore".to_string(),
            tag_display: "Google Play".to_string(),
            abi: "arm64-v8a".to_string(),
            size: 1,
        };

        let name = create_virtual_device(&home, &image).unwrap();
        assert_eq!(name, "build-script-api36-google_apis_playstore");

        let config = fs::read_to_string(home.join(format!("{name}.avd/config.ini"))).unwrap();

        for line in [
            "image.sysdir.1=system-images/android-36/google_apis_playstore/arm64-v8a/",
            "hw.cpu.arch=arm64",
            "PlayStore.enabled=true",
        ] {
            assert!(
                config.lines().any(|existing| existing == line),
                "{line} missing in {config}"
            );
        }

        assert!(
            fs::read_to_string(home.join(format!("{name}.ini")))
                .unwrap()
                .contains("target=android-36")
        );

        let avds = list_virtual_devices(&home);
        assert_eq!(
            avds,
            [AndroidVirtualDevice {
                name,
                display: "Android API 36 (Google Play, arm64-v8a)".to_string()
            }]
        );
    }
}
