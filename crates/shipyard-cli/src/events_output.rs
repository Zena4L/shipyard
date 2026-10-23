//! `shipyard events`: the engine's journal of what happened (containers
//! made, started, ended; images pulled; volumes and networks made and
//! removed), printed as Docker prints it: its line, or a Go template over
//! Docker's event message (`json`: one object per line).

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
    /// How to print them: Docker's line without it; `json` (Docker's event
    /// message, an object per line), or a Go TEMPLATE over it
    /// (`{{.Type}} {{.Action}} {{.Actor.Attributes.name}}`).
    #[arg(long, value_name = "FORMAT")]
    format: Option<String>,
}

/// An event as `--format` reads it: Docker's message, its JSON for
/// `json`, its Go field names for a template.
#[derive(serde::Serialize)]
#[serde(transparent)]
struct EventRow(shipyard_engine::docker::EventMessage);

/// An event's Go field names (Docker's `events.Message`), each there with
/// its zero value where the message has none, as Go's are.
#[derive(serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct GoMessage<'a> {
    status: &'a str,
    #[serde(rename = "ID")]
    id: &'a str,
    from: &'a str,
    #[serde(rename = "Type")]
    kind: &'a str,
    action: &'a str,
    actor: GoActor<'a>,
    scope: &'a str,
    time: i64,
    time_nano: i64,
}

/// An event's actor by its Go field names.
#[derive(serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct GoActor<'a> {
    #[serde(rename = "ID")]
    id: &'a str,
    attributes: &'a std::collections::BTreeMap<String, String>,
}

impl crate::format::Row for EventRow {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn id(&self) -> &str {
        &self.0.actor.id
    }

    fn fields(&self) -> gotmpl::Result<gotmpl::Value> {
        let message = &self.0;
        gotmpl::to_value(&GoMessage {
            status: message.status.as_deref().unwrap_or_default(),
            id: message.id.as_deref().unwrap_or_default(),
            from: message.from.as_deref().unwrap_or_default(),
            kind: &message.kind,
            action: &message.action,
            actor: GoActor {
                id: &message.actor.id,
                attributes: &message.actor.attributes,
            },
            scope: message.scope,
            time: message.time,
            time_nano: message.time_nano,
        })
    }
}

/// `shipyard events`: the journal's events, as `args` say, until `--until`
/// (or an interrupt, or a reader that closes its end: not an error).
pub(crate) fn events(args: &EventsArgs) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let options = EventsOptions {
        since: args.since.and_then(events::system_time),
        until: args.until.and_then(events::system_time),
        filters: Filters::parse(&args.filter)?,
        gone: Some(shipyard_engine::logs::Gone::new(
            shipyard_cli::pipe::stdout_gone,
        )),
    };
    // Docker's: a template, never a table.
    let output = args
        .format
        .as_deref()
        .map(crate::format::Output::lines)
        .transpose()?;
    let mut out = std::io::stdout().lock();
    let read = events::read(&engine, &options, |event| {
        use std::io::Write as _;
        match &output {
            None => writeln!(out, "{}", line(&event))?,
            // A failed write is the `io::Error` it was (a reader gone
            // stays quiet: `main`'s `broken_pipe`), a template's failure
            // one of its own.
            Some(output) => output
                .write(&mut out, &[EventRow((&event).into())])
                .map_err(|error| {
                    error
                        .downcast::<std::io::Error>()
                        .unwrap_or_else(std::io::Error::other)
                })?,
        }
        out.flush()
    });
    // A reader gone (`events | head`) ends it quietly: `main`'s `fail`.
    read?;
    Ok(ExitCode::SUCCESS)
}

/// `event` as Docker's line prints it. It's meant for a terminal: a
/// control character in it is escaped (`\u{1b}`), never sent.
fn line(event: &Event) -> String {
    {
        {
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
            line(&event),
            "2025-10-09T08:53:20.000000005Z container die 0123abcd (exitCode=3, name=web)"
        );
        let prune = Event::new(Kind::Image, "prune", "")
            .at(SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 0));
        assert_eq!(line(&prune), "2025-10-09T08:53:20.000000000Z image prune");
        // A terminal's escapes are printed, never sent.
        let named = Event::new(Kind::Volume, "create", "v\x1b[2Jx")
            .with("note", "a\nb")
            .at(SystemTime::UNIX_EPOCH);
        assert_eq!(
            line(&named),
            "1970-01-01T00:00:00.000000000Z volume create v\\u{1b}[2Jx (note=a\\u{a}b)"
        );
    }
}
