//! How `shipyard-runtime` says what went wrong: on stderr, its control
//! characters escaped, and in the `--log` file if one was given.

use std::io::Write as _;
use std::path::Path;

use crate::args::{Cli, LogFormat};

/// How serious a message is, for stderr and the log.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Level {
    Error,
    Warning,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Prints a message to stderr (`shipyard-runtime: …` for an error,
/// `shipyard-runtime: warning: …` for a warning) and appends it to the log.
pub(crate) fn report(cli: &Cli, level: Level, message: &str) {
    // A container's file names, IDs in its records, its program's name: what
    // a message quotes can hold anything. Shown, never obeyed.
    let message = &escape_controls(message);
    let prefix = match level {
        Level::Error => "",
        Level::Warning => "warning: ",
    };
    let _ = writeln!(
        std::io::stderr().lock(),
        "shipyard-runtime: {prefix}{message}"
    );
    if let Some(log) = &cli.log {
        append_log(log, cli.log_format, level, message);
    }
}

/// Appends a message to the log file, as text or as runc's JSON lines. Best
/// effort: the message is on stderr anyway.
fn append_log(log: &Path, format: LogFormat, level: Level, message: &str) {
    let level = level.as_str();
    let line = match format {
        LogFormat::Text => format!("{level}: {message}\n"),
        LogFormat::Json => {
            let mut line = serde_json::json!({"level": level, "msg": message}).to_string();
            line.push('\n');
            line
        }
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// `text` with its control characters escaped (`\u{1b}`), so it can't move
/// the cursor or change the terminal it's printed on. Newlines stay: a
/// message's own lines.
fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped: Vec<char> = if c.is_control() && c != '\n' {
                c.escape_default().collect()
            } else {
                vec![c]
            };
            escaped
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a message quotes of a container's can't drive the terminal; a
    /// message's own lines stay lines.
    #[test]
    fn control_characters_are_shown_escaped() {
        assert_eq!(
            escape_controls("a\u{1b}[2Jb\rc\nd é"),
            "a\\u{1b}[2Jb\\rc\nd é"
        );
    }
}
