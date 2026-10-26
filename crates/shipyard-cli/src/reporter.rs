//! The CLI's reporter: what the engine says, as lines on stderr; and the
//! engine it's given to.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use shipyard_engine::report::{Message, Progress, Reporter};
use shipyard_engine::{Engine, Paths};

use crate::format::rows::human_size;
use crate::output::printable_message;

/// The engine, as every command uses it: its places from the environment,
/// what it has to say printed on stderr.
pub(crate) fn engine() -> Engine {
    engine_with(CliReporter { progress: true })
}

/// The engine, reporting to `reporter`.
pub(crate) fn engine_with(reporter: CliReporter) -> Engine {
    Engine::new(Paths::from_env(), Arc::new(reporter))
}

/// Prints what the engine reports, as the CLI's own lines: warnings,
/// notices, hints and a run's failure with their `shipyard:` prefixes, a
/// pull's progress (unless `progress` is off: `pull -q`).
pub(crate) struct CliReporter {
    pub(crate) progress: bool,
}

impl CliReporter {
    /// The line `message` prints, if any.
    fn line(&self, message: Message) -> Option<String> {
        Some(match message {
            Message::Warning(warning) => format!("shipyard: warning: {}", warning.message),
            // A failure prints as the CLI's errors do.
            Message::Notice(text) | Message::Failure(text) => format!("shipyard: {text}"),
            Message::Hint(text) => format!("shipyard: hint: {text}"),
            Message::Progress(step) if self.progress => match step {
                Progress::Pulling { image } => format!("Pulling {image}"),
                // BuildKit's own line, as it wrote it (escaped as it's printed).
                Progress::Build { line } => line,
                Progress::Layer {
                    short_digest,
                    downloaded: Some(bytes),
                } => format!("{short_digest}: downloaded ({})", human_size(bytes, 4)),
                Progress::Layer {
                    short_digest,
                    downloaded: None,
                } => format!("{short_digest}: already here"),
                Progress::Pushing { name } => format!("Pushing {name}"),
                Progress::Pushed {
                    short_digest,
                    sent: Some(bytes),
                } => format!("{short_digest}: pushed ({})", human_size(bytes, 4)),
                Progress::Pushed {
                    short_digest,
                    sent: None,
                } => format!("{short_digest}: already there"),
            },
            Message::Progress(_) => return None,
        })
    }
}

impl Reporter for CliReporter {
    fn report(&self, message: Message) {
        if let Some(line) = self.line(message) {
            // Not `eprintln!`: it panics if stderr is a closed pipe.
            let _ = writeln!(std::io::stderr().lock(), "{}", printable_message(&line));
        }
    }
}

/// The exit code of a command's own failure (Docker's CLI's): every
/// command's but `run`'s, which is 125
/// ([`EXIT_ENGINE_ERROR`](shipyard_runtime::process::EXIT_ENGINE_ERROR)).
pub(crate) const EXIT_FAILURE: u8 = 1;

/// Prints an error with its full cause chain ("doing X: because Y") and
/// returns `code` as the exit code.
pub(crate) fn report(err: &dyn std::fmt::Display, code: u8) -> ExitCode {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(
        std::io::stderr().lock(),
        "shipyard: {}",
        printable_message(&format!("{err:#}"))
    );
    ExitCode::from(code)
}

/// [`report`] of an error and its causes, each cause said once
/// ([`said_once`](shipyard_engine::said_once): anyhow's own `{:#}` would
/// repeat a cause an error already wrote into its words).
pub(crate) fn report_error(err: &anyhow::Error, code: u8) -> ExitCode {
    report(
        &shipyard_engine::said_once(err.chain().map(ToString::to_string)),
        code,
    )
}

/// Prints `message` as a warning (`shipyard: warning: …`).
pub(crate) fn warn(message: impl std::fmt::Display) -> std::io::Result<()> {
    writeln!(
        std::io::stderr().lock(),
        "shipyard: warning: {}",
        printable_message(&message.to_string())
    )
}

/// Prints `message` as a hint (`shipyard: hint: …`).
pub(crate) fn hint(message: impl std::fmt::Display) -> std::io::Result<()> {
    writeln!(
        std::io::stderr().lock(),
        "shipyard: hint: {}",
        printable_message(&message.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("the archive: {0}")]
    struct WritesItsSource(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("an unsafe path")]
    struct Inner;

    /// Through anyhow, as the CLI reports: a cause an error already wrote
    /// into its words is said once (the stage-12 book's `load` of
    /// `../outside` said it twice); the context and an error that doesn't
    /// write its cause, the control, keep the whole chain.
    #[test]
    fn a_cause_already_said_is_said_once() {
        let error = anyhow::Error::from(WritesItsSource(Inner)).context("loading");
        assert_eq!(
            shipyard_engine::said_once(error.chain().map(ToString::to_string)),
            "loading: the archive: an unsafe path"
        );
        assert_eq!(
            format!("{error:#}"),
            "loading: the archive: an unsafe path: an unsafe path",
            "anyhow's own, what the CLI no longer prints"
        );
        let plain = anyhow::Error::from(Inner).context("loading");
        assert_eq!(
            shipyard_engine::said_once(plain.chain().map(ToString::to_string)),
            "loading: an unsafe path"
        );
    }

    #[test]
    fn the_engines_messages_print_as_the_clis_lines() {
        use shipyard_engine::report::{Warning, WarningKind};
        let loud = CliReporter { progress: true };
        let warning = Message::Warning(Warning {
            kind: WarningKind::UnpackSkipped,
            message: "left out x".to_owned(),
        });
        assert_eq!(
            loud.line(warning.clone()).as_deref(),
            Some("shipyard: warning: left out x")
        );
        assert_eq!(
            loud.line(Message::Notice("published 80".to_owned()))
                .as_deref(),
            Some("shipyard: published 80")
        );
        assert_eq!(
            loud.line(Message::Hint("try -f".to_owned())).as_deref(),
            Some("shipyard: hint: try -f")
        );
        assert_eq!(
            loud.line(Message::Failure("not found".to_owned()))
                .as_deref(),
            Some("shipyard: not found")
        );
        let pulling = Message::Progress(Progress::Pulling {
            image: "docker.io/library/alpine:latest".to_owned(),
        });
        assert_eq!(
            loud.line(pulling.clone()).as_deref(),
            Some("Pulling docker.io/library/alpine:latest")
        );
        let downloaded = Message::Progress(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: Some(2048),
        });
        assert_eq!(
            loud.line(downloaded.clone()).as_deref(),
            Some("0123456789ab: downloaded (2.048kB)")
        );
        let stored = Message::Progress(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: None,
        });
        assert_eq!(
            loud.line(stored.clone()).as_deref(),
            Some("0123456789ab: already here")
        );
        // `pull -q`: no progress, the rest as ever.
        let quiet = CliReporter { progress: false };
        for event in [pulling, downloaded, stored] {
            assert_eq!(quiet.line(event), None);
        }
        assert_eq!(
            quiet.line(warning).as_deref(),
            Some("shipyard: warning: left out x")
        );
    }
}
