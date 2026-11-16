// How this machine starts the programs of other targets: itself where it can, like with Rosetta, otherwise through
// the runner the setting runner.<target> names, like qemu, wine or a container
use std::path::Path;
use std::process::Command;

use super::settings::Settings;
use super::target::{OperatingSystem, Target};

/// runner.<target> = <command>: the program and its arguments come after it, {directory} in it is where it runs
pub const RUNNER: &str = "runner.";

/// What starts the programs of a target on this machine
#[derive(Debug, Clone, PartialEq)]
pub struct Runner {
    /// the words in front of the program, none when the machine starts it itself
    words: Vec<String>,
}

impl Runner {
    /// This machine itself, for its own programs
    pub fn direct() -> Runner {
        return Runner { words: vec![] };
    }

    /// The runner of the settings file for `target`, else this machine where it starts the programs itself,
    /// else why they do not run
    pub fn for_target(target: Target) -> Result<Runner, String> {
        let settings = Settings::load()?;
        return Self::of(target, Target::host(), settings.get(&format!("{RUNNER}{}", target.name())));
    }

    /// For `target` on `machine` with the setting, if there is one: the setting wins, it may run the programs
    /// of the machine's own target in a container too
    pub fn of(target: Target, machine: Target, setting: Option<&str>) -> Result<Runner, String> {
        if let Some(command) = setting {
            let words = words(command)?;
            if words.is_empty() {
                return Err(format!("{RUNNER}{} names no program", target.name()));
            }
            return Ok(Runner { words });
        }

        if target.runs_on(machine) {
            return Ok(Runner { words: vec![] });
        }

        let name = target.name();
        let mut message = format!("programs for {name} do not run on {}, a runner starts them: build config {RUNNER}{name} <command>", machine.name());
        if let Some(example) = example(target) {
            message.push_str(&format!(", like '{example}'"));
        }
        return Err(message);
    }

    /// Starts `program` in `directory`
    pub fn command(&self, program: &Path, directory: &Path) -> Command {
        let Some((first, rest)) = self.words.split_first() else {
            let mut command = Command::new(program);
            command.current_dir(directory);
            return command;
        };

        let directory_text = directory.display().to_string();
        let mut command = Command::new(first.replace("{directory}", &directory_text));
        command.args(rest.iter().map(|word| word.replace("{directory}", &directory_text)))
            .arg(program)
            .current_dir(directory);
        return command;
    }
}

/// A runner that starts the programs of the target on most machines that can run one at all
fn example(target: Target) -> Option<String> {
    let qemu = match target.arch.name() {
        "armv7" => "arm",
        "x86" => "i386",
        arch => arch,
    };

    return match (target.os, target.musl) {
        (OperatingSystem::Linux, true) => Some(format!("qemu-{qemu}")),
        (OperatingSystem::Linux, false) => Some(format!("qemu-{qemu} -L /usr/{}-linux-gnu", target.arch.name())),
        (OperatingSystem::Windows, _) => Some("wine".to_string()),
        (OperatingSystem::MacOs | OperatingSystem::Android, _) => None,
    };
}

/// The words of a command line: separated by spaces, '...' and "..." hold spaces
pub fn words(command: &str) -> Result<Vec<String>, String> {
    let mut words = vec![];
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;

    for char in command.chars() {
        match (quote, char) {
            (Some(open), char) if char == open => quote = None,
            (Some(_), char) => word.get_or_insert_default().push(char),
            (None, '"' | '\'') => {
                quote = Some(char);
                word.get_or_insert_default();
            }
            (None, char) if char.is_whitespace() => words.extend(word.take()),
            (None, char) => word.get_or_insert_default().push(char),
        }
    }

    if quote.is_some() {
        return Err(format!("{command}: a quote is not closed"));
    }
    words.extend(word);

    return Ok(words);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Target {
        return Target::named(name).unwrap();
    }

    #[test]
    fn machines_run_what_they_can_and_runners_the_rest() {
        let mac = named("macos-aarch64");

        // Rosetta, and a runner wins over the machine
        assert_eq!(Runner::of(named("macos-x86_64"), mac, None), Ok(Runner { words: vec![] }));
        assert_eq!(Runner::of(mac, mac, Some("arch -arm64")).unwrap().words, ["arch", "-arm64"]);
        assert!(named("macos-aarch64").runs_on(mac) && !mac.runs_on(named("macos-x86_64")));

        // Windows on ARM, static musl on Linux of the same processor
        assert!(named("windows-x86_64").runs_on(named("windows-aarch64")));
        assert!(named("linux-x86_64-musl").runs_on(named("linux-x86_64")));
        assert!(!named("linux-aarch64").runs_on(named("linux-x86_64")));

        let error = Runner::of(named("linux-aarch64"), named("linux-x86_64"), None).unwrap_err();
        assert_eq!(error, "programs for linux-aarch64 do not run on linux-x86_64, a runner starts them: build config runner.linux-aarch64 <command>, like 'qemu-aarch64 -L /usr/aarch64-linux-gnu'");
        let error = Runner::of(named("windows-x86_64"), mac, None).unwrap_err();
        assert!(error.ends_with("like 'wine'"), "{error}");
        assert!(Runner::of(named("windows-x86_64"), mac, Some("  ")).is_err());
    }

    #[test]
    fn runners_put_the_program_after_them() {
        let runner = Runner::of(named("linux-aarch64"), named("linux-x86_64"), Some("docker run -v '{directory}:{directory}' -w \"{directory}\" debian")).unwrap();
        let command = runner.command(Path::new("/p/app/build/linux-aarch64/bin/app"), Path::new("/p/app"));

        assert_eq!(command.get_program(), "docker");
        let arguments: Vec<_> = command.get_args().map(|argument| argument.to_string_lossy().to_string()).collect();
        assert_eq!(arguments, ["run", "-v", "/p/app:/p/app", "-w", "/p/app", "debian", "/p/app/build/linux-aarch64/bin/app"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/p/app")));

        assert_eq!(words("a 'b c' \"\" d").unwrap(), ["a", "b c", "", "d"]);
        assert!(words("a 'b").is_err());
    }
}
