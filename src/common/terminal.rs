// The terminal: colors of the output, and asking in it, a line of free input, or one of fixed
// choices, picked like in fzf. Typing filters the choices, the arrow keys move between them, enter
// takes one, escape cancels.
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

/// Choices shown at the same time, the others follow while moving
const SHOWN_CHOICES: usize = 10;

/// Few colors: orange for the modules and what stands out, yellow for the tools build-script brings,
/// red only when something failed
#[derive(Clone, Copy)]
pub enum Color {
    Primary,
    Secondary,
    Failure,
    /// for what explains, like what a choice is
    Dim,
}

#[derive(Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Erase,
    Typed(char),
    Other,
}

/// `text` in the color, unless the output gets none
pub fn paint(text: &str, color: Color) -> String {
    if !colors() {
        return text.to_string();
    }

    // orange and yellow of the 256 colors
    let code = match color {
        Color::Primary => "38;5;208",
        Color::Secondary => "38;5;220",
        Color::Failure => "31",
        Color::Dim => "2",
    };

    return format!("\x1b[{code}m{text}\x1b[0m");
}

/// Colors only go to a terminal, NO_COLOR (no-color.org) or TERM=dumb turn them off. Never in the
/// tests, their expected output has none.
pub fn colors() -> bool {
    static COLORS: LazyLock<bool> = LazyLock::new(|| {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");

        return !cfg!(test) && !no_color && !dumb && io::stdout().is_terminal();
    });

    return *COLORS;
}

/// One line, trimmed, None at the end of the input (ctrl-d)
pub fn read_line(question: &str) -> Result<Option<String>, String> {
    print!("{question}");
    io::stdout()
        .flush()
        .map_err(|error| format!("could not write to the terminal: {error}"))?;

    let mut answer = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| format!("could not read the answer: {error}"))?;

    return Ok((read > 0).then(|| answer.trim().to_string()));
}

/// A line that is not empty, asked again until it is one
pub fn ask(question: &str) -> Result<String, String> {
    loop {
        match read_line(&format!("{question}: "))? {
            Some(answer) if !answer.is_empty() => return Ok(answer),
            Some(_) => {}
            None => return Err("cancelled".to_string()),
        }
    }
}

/// One of `choices`, those matching `filter` first in view: a value and what it is, "" for nothing to say.
/// The filter goes by the value. Without a terminal that reads key by key, like on Windows, from a numbered
/// list.
pub fn choose(question: &str, choices: &[(&str, &str)], filter: &str) -> Result<String, String> {
    let width = choices.iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);

    let Some(_raw_mode) = io::stdout().is_terminal().then(RawMode::enter).flatten() else {
        return choose_by_number(question, choices, width);
    };

    let mut stdout = io::stdout();
    let mut stdin = io::stdin().lock();
    let mut filter = filter.to_string();
    let mut selected = 0;
    let mut buffer = [0u8; 64];

    let chosen = 'keys: loop {
        let matches = matching(choices, &filter);
        selected = selected.min(matches.len().saturating_sub(1));
        let first = selected.saturating_sub(SHOWN_CHOICES - 1);

        let mut screen = String::from("\r\x1b[J");
        let question_line = format!("{question}: {filter}");
        screen.push_str(&question_line);

        for (index, (value, about)) in matches.iter().enumerate().skip(first).take(SHOWN_CHOICES) {
            let about = described(about, width.saturating_sub(value.chars().count()));

            match index == selected {
                true => screen.push_str(&format!("\n{}{about}", paint(&format!("> {value}"), Color::Primary))),
                false => screen.push_str(&format!("\n  {value}{about}")),
            }
        }

        if matches.is_empty() {
            screen.push_str("\n  nothing matches");
        }

        // back up to the question, its cursor after the filter
        let drawn = screen.matches('\n').count();
        screen.push_str(&format!("\x1b[{drawn}A\r\x1b[{}C", question_line.chars().count()));
        write!(stdout, "{screen}").and_then(|_| stdout.flush()).map_err(|error| format!("could not write to the terminal: {error}"))?;

        let read = stdin.read(&mut buffer).map_err(|error| format!("could not read the answer: {error}"))?;

        if read == 0 {
            break None;
        }

        for key in keys(&buffer[..read]) {
            let matches = matching(choices, &filter);

            match key {
                Key::Up => selected = selected.saturating_sub(1),
                Key::Down => selected = (selected + 1).min(matches.len().saturating_sub(1)),
                Key::Enter if !matches.is_empty() => break 'keys Some(matches[selected].0.to_string()),
                Key::Cancel => break 'keys None,
                Key::Erase => {
                    filter.pop();
                    selected = 0;
                }
                Key::Typed(char) => {
                    filter.push(char);
                    selected = 0;
                }
                Key::Enter | Key::Other => {}
            }
        }
    };

    // the question keeps the answer, the choices go
    let answer = chosen.as_deref().unwrap_or("");
    let _ = write!(stdout, "\r\x1b[J{question}: {answer}\n").and_then(|_| stdout.flush());

    return chosen.ok_or_else(|| "cancelled".to_string());
}

/// The choices in a list, the answer their number or one of them
fn choose_by_number(question: &str, choices: &[(&str, &str)], width: usize) -> Result<String, String> {
    println!("{question}:");

    for (index, (value, about)) in choices.iter().enumerate() {
        println!("  {}) {value}{}", index + 1, described(about, width.saturating_sub(value.chars().count())));
    }

    loop {
        let answer = ask("Number or name")?;
        let by_number = answer.parse::<usize>()
            .ok()
            .and_then(|number| choices.get(number.wrapping_sub(1)));

        match by_number.or_else(|| choices.iter().find(|(value, _)| *value == answer)) {
            Some((value, _)) => return Ok(value.to_string()),
            None => println!("{answer} is none of them"),
        }
    }
}

/// What a choice is, dimmed behind it and aligned with the others: `padding` spaces fill up to the longest
fn described(about: &str, padding: usize) -> String {
    if about.is_empty() {
        return String::new();
    }

    return format!("{}  {}", " ".repeat(padding), paint(about, Color::Dim));
}

/// The choices whose values have the characters of the filter in their order, like fzf: the one that is the
/// filter first, then those it starts, then the others in their order
fn matching<'a>(choices: &[(&'a str, &'a str)], filter: &str) -> Vec<(&'a str, &'a str)> {
    let filter = filter.to_ascii_lowercase();

    let mut matches: Vec<(&str, &str)> = choices.iter()
        .copied()
        .filter(|(value, _)| {
            let mut rest = value.chars();
            return filter.chars().all(|wanted| rest.any(|char| char.eq_ignore_ascii_case(&wanted)));
        })
        .collect();

    matches.sort_by_key(|(value, _)| {
        let value = value.to_ascii_lowercase();
        return (value != filter, !value.starts_with(&filter));
    });

    return matches;
}

/// What a read of the terminal holds, pasted text is several keys
fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = vec![];
    let mut index = 0;

    while index < bytes.len() {
        let (key, length) = match &bytes[index..] {
            [27, b'[' | b'O', b'A', ..] => (Key::Up, 3),
            [27, b'[' | b'O', b'B', ..] => (Key::Down, 3),
            // other sequences end in a letter or ~, like ctrl-right: ESC [ 1 ; 5 C
            [27, b'[', rest @ ..] => {
                let end = rest.iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .map_or(rest.len(), |end| end + 1);
                (Key::Other, 2 + end)
            }
            [27, ..] | [3, ..] | [4, ..] => (Key::Cancel, 1),
            [b'\r' | b'\n', ..] => (Key::Enter, 1),
            [127 | 8, ..] => (Key::Erase, 1),
            // ctrl-p, ctrl-n and tab, like in fzf
            [16, ..] => (Key::Up, 1),
            [14 | 9, ..] => (Key::Down, 1),
            [byte, ..] if byte.is_ascii_graphic() || *byte == b' ' => (Key::Typed(*byte as char), 1),
            _ => (Key::Other, 1),
        };

        keys.push(key);
        index += length;
    }

    return keys;
}

/// While it lives the terminal hands over every key without showing it, ctrl-c included
struct RawMode {
    saved: String,
}

impl RawMode {
    /// None without stty, like on Windows
    fn enter() -> Option<RawMode> {
        let saved = stty(&["-g"])?;
        stty(&["-icanon", "-echo", "-isig", "min", "1"])?;

        return Some(RawMode { saved });
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        stty(&[&self.saved]);
    }
}

/// stty works on the terminal of its standard input, its output trimmed
fn stty(args: &[&str]) -> Option<String> {
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    return output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string());
}

/// A line of the build log: `[app] Compiling`, the module in orange
pub fn log(module: &str, message: &str) {
    println!("[{}] {message}", paint(module, Color::Primary));
}

/// A line about a tool build-script brings itself: `[vcpkg] Installing`, the tool in yellow
pub fn log_tool(tool: &str, message: &str) {
    println!("[{}] {message}", paint(tool, Color::Secondary));
}

/// Reads one answer from the terminal, trimmed and lowercase, empty at the end of the input
pub fn prompt(question: &str) -> Result<String, String> {
    let answer = read_line(question)?.unwrap_or_default();
    return Ok(answer.to_ascii_lowercase());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_from_what_the_terminal_sends() {
        assert_eq!(
            keys(b"\x1b[Akot\x7f\r"),
            [Key::Up, Key::Typed('k'), Key::Typed('o'), Key::Typed('t'), Key::Erase, Key::Enter]
        );
        assert_eq!(keys(b"\x1bOB\x1b[1;5Cx"), [Key::Down, Key::Other, Key::Typed('x')]);
        assert_eq!(keys(b"\x1b"), [Key::Cancel]);
        assert_eq!(keys(b"\x03"), [Key::Cancel]);
    }

    #[test]
    fn choices_match_like_in_fzf() {
        let kinds = ["java", "kotlin", "android", "multi", "cpp", "c"].map(|kind| (kind, ""));
        let values = |matches: Vec<(&str, &str)>| matches.iter().map(|(value, _)| value.to_string()).collect::<Vec<_>>();

        assert_eq!(values(matching(&kinds, "c")), ["c", "cpp"]);
        assert_eq!(values(matching(&kinds, "KTN")), ["kotlin"]);
        assert_eq!(values(matching(&kinds, "a")), ["android", "java"]);
        assert_eq!(matching(&kinds, "").len(), kinds.len());
        assert!(matching(&kinds, "rust").is_empty());

        // what a choice is does not count for the filter
        let described = [("go", "a Go program (plugin)"), ("cpp", "a C++ program")];
        assert_eq!(values(matching(&described, "prog")), Vec::<String>::new());
        assert_eq!(values(matching(&described, "go")), ["go"]);
    }

    #[test]
    fn what_choices_are_is_aligned_behind_them() {
        assert_eq!(described("a Go program", 4), "      a Go program");
        assert_eq!(described("", 4), "");
    }
}
