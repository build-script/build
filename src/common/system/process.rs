// Running programs: with the output in the terminal, quietly, in one piece, with a timeout, for their output
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// With the output in the terminal, but no stdin: a tool must not swallow what
/// the user types for a prompt. `what` names it in errors.
pub fn run_command(command: &mut Command, what: &str) -> Result<(), String> {
    let status = command.stdin(Stdio::null())
        .status()
        .map_err(|error| start_error(command, error))?;

    if !status.success() {
        return Err(format!("{what} failed ({status})"));
    }

    return Ok(());
}

/// Like run_command, but what the tool prints only shows when it fails
pub fn run_command_quietly(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.stdin(Stdio::null())
        .output()
        .map_err(|error| start_error(command, error))?;

    if !output.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(format!(
            "{what} failed ({}): {}",
            output.status,
            text.trim()
        ));
    }

    return Ok(());
}

/// Like run_command, but what the tool prints comes in one piece once it ended: compilers running at
/// the same time do not mix their messages
pub fn run_command_in_one_piece(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.stdin(Stdio::null())
        .output()
        .map_err(|error| start_error(command, error))?;

    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(&output.stdout);
    let _ = stderr.write_all(&output.stderr);

    if !output.status.success() {
        return Err(format!("{what} failed ({})", output.status));
    }

    return Ok(());
}

/// What the command printed, once it ended successfully. `what` names it in errors, which have what it
/// printed to stderr.
pub fn command_output(command: &mut Command, what: &str) -> Result<String, String> {
    let output = command.stdin(Stdio::null())
        .output()
        .map_err(|error| start_error(command, error))?;

    if !output.status.success() {
        return Err(format!("{what} failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
    }

    return Ok(String::from_utf8_lossy(&output.stdout).to_string());
}

/// Whether the command started and ended successfully, for questions like whether git has a commit
pub fn command_succeeds(command: &mut Command) -> bool {
    return command.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
}

/// A program with the terminal, like `run` starts it: its exit code, 1 when a signal stopped it, like
/// a shell says
pub fn run_interactive(command: &mut Command) -> Result<i32, String> {
    let status = command.status().map_err(|error| start_error(command, error))?;
    return Ok(status.code().unwrap_or(1));
}

fn start_error(command: &Command, error: std::io::Error) -> String {
    return format!("could not start {}: {error}", command.get_program().to_string_lossy());
}

/// Like run_command, but stopped once it ran longer than the timeout, like tests that hang
pub fn run_command_within(command: &mut Command, what: &str, timeout: Duration) -> Result<(), String> {
    let mut child = command.stdin(Stdio::null())
        .spawn()
        .map_err(|error| start_error(command, error))?;

    let started = Instant::now();

    loop {
        let status = child.try_wait().map_err(|error| format!("{what}: {error}"))?;

        match status {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("{what} failed ({status})")),
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} timed out after {}s", timeout.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn commands_that_run_too_long_are_stopped() {
        let started = Instant::now();
        let result = run_command_within(Command::new("sleep").arg("10"), "tests", Duration::from_millis(200));

        assert!(result.is_err_and(|error| error.starts_with("tests timed out")));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(run_command_within(&mut Command::new("true"), "tests", Duration::from_secs(5)), Ok(()));
        assert!(run_command_within(&mut Command::new("false"), "tests", Duration::from_secs(5)).is_err());
    }
}
