//! `shipyard events`: the engine's journal of what happened (containers
//! made, started, ended; images pulled; volumes and networks made and
//! removed), printed as Docker prints it, or as JSON lines.

use std::process::ExitCode;

use anyhow::Result;
use shipyard_engine::events::{self, Event, EventsOptions, Filters};
use shipyard_runtime::logs::time::Timestamp;

use crate::output::printable;

/// `shipyard events`' flags.
#[derive(Debug, clap::Args)]
pub(crate) struct EventsArgs {
    /// Past events too, from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix
    /// seconds, or a duration ago (10m, 2h, 3d). Without it, only new ones.
    #[arg(long, value_name = "TIME", value_parser = shipyard_cli::logs_output::parse_since)]
    since: Option<Timestamp>,
    /// Stop at TIME (as --since): with a past TIME, print and exit. Without
    /// it, wait for more until interrupted.
    #[arg(long, value_name = "TIME", value_parser = shipyard_cli::logs_output::parse_since)]
    until: Option<Timestamp>,
    /// Only the events that match: type=container|image|volume|network,
    /// event=ACTION, container=NAME|ID, image=REF, volume=NAME,
    /// network=NAME. Repeat a key for any of its values.
    #[arg(short, long, value_name = "KEY=VALUE")]
    filter: Vec<String>,
    /// How to print them: Docker's line, or JSON (one object per line, as
    /// the journal keeps it).
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

/// `events`' output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Format {
    /// `TIME TYPE ACTION ID (KEY=VALUE, …)`.
    #[default]
    Text,
    /// The journal's line.
    Json,
}

/// `shipyard events`: the journal's events, as `args` say, until `--until`
/// (or an interrupt, or a reader that closes its end: not an error).
pub(crate) fn events(args: &EventsArgs) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let options = EventsOptions {
        since: args.since.and_then(events::system_time),
        until: args.until.and_then(events::system_time),
        filters: Filters::parse(&args.filter)?,
        gone: Some(shipyard_cli::pipe::stdout_gone),
    };
    let mut out = std::io::stdout().lock();
    let read = events::read(&engine, &options, |event| {
        use std::io::Write as _;
        writeln!(out, "{}", line(&event, args.format))?;
        out.flush()
    });
    // A reader gone (`events | head`) ends it quietly: `main`'s `fail`.
    read?;
    Ok(ExitCode::SUCCESS)
}

/// `event` as `format` prints it. Text is meant for a terminal: a control
/// character in it is escaped (`\u{1b}`), never sent.
fn line(event: &Event, format: Format) -> String {
    match format {
        Format::Json => event.to_line(),
        Format::Text => {
            let mut text = format!(
                "{} {} {}",
                events::rfc3339(event.time),
                event.kind,
                printable(&event.action)
            );
            if !event.id.is_empty() {
                text.push(' ');
                text.push_str(&printable(&event.id));
            }
            if !event.attributes.is_empty() {
                let attributes: Vec<String> = event
                    .attributes
                    .iter()
                    .map(|(key, value)| format!("{}={}", printable(key), printable(value)))
                    .collect();
                text.push_str(" (");
                text.push_str(&attributes.join(", "));
                text.push(')');
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shipyard_engine::events::Kind;
    use std::time::{Duration, SystemTime};

    /// Docker's line: time, type, action, ID, then the attributes sorted.
    #[test]
    fn text_is_dockers_line() {
        let event = Event::new(Kind::Container, "die", "0123abcd")
            .with("name", "web")
            .with("exitCode", 3)
            .at(SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 5));
        assert_eq!(
            line(&event, Format::Text),
            "2025-10-09T08:53:20.000000005Z container die 0123abcd (exitCode=3, name=web)"
        );
        assert_eq!(line(&event, Format::Json), event.to_line());
        let prune = Event::new(Kind::Image, "prune", "")
            .at(SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 0));
        assert_eq!(
            line(&prune, Format::Text),
            "2025-10-09T08:53:20.000000000Z image prune"
        );
        // A terminal's escapes are printed, never sent.
        let named = Event::new(Kind::Volume, "create", "v\x1b[2Jx")
            .with("note", "a\nb")
            .at(SystemTime::UNIX_EPOCH);
        assert_eq!(
            line(&named, Format::Text),
            "1970-01-01T00:00:00.000000000Z volume create v\\u{1b}[2Jx (note=a\\u{a}b)"
        );
    }
}
