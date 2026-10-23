//! Every start's logs of a container by name (stage 9, step 9.4c1): the
//! runtime's reader ([`shipyard_runtime::logs::reader`]) over each start's
//! log directory, oldest start first.

use std::path::{Path, PathBuf};

use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::logs::info::InfoError;
use shipyard_runtime::logs::reader::{BeforeTail, POLL, read, read_then};
pub use shipyard_runtime::logs::reader::{
    Entry, Gone, LineWriter, LogsError, LogsOptions, ReadSummary,
};
use shipyard_runtime::logs::time::Timestamp;

/// Every start's logs of the container `reference` names (stage 9, step
/// 9.4c1), oldest start first, as `options` say, each line to `sink`.
///
/// Each start has a log directory of its own, numbered (`logs/0000000001`,
/// …). [`LogsOptions::tail`] counts entries across them: the newest start's
/// last ones, and as many of the one before as are still wanted, and so on
/// (at most that many held). A line a start left open (it ended in the
/// middle of one) is ended before the next start's. [`LogsOptions::follow`]
/// follows the newest start until the container stops (as Docker's does):
/// it ends once its runtime has it neither up nor being started, or once
/// it has been started again (that's a new start: `logs -f` again for it).
///
/// # Errors
///
/// No such container; logs that can't be read; `sink` (its
/// [`LogsError::Sink`], found with [`Error::find`](crate::Error::find)).
pub fn of_container(
    engine: &crate::Engine,
    reference: &str,
    options: &LogsOptions,
    sink: impl FnMut(Entry) -> std::io::Result<()>,
) -> crate::Result<ReadSummary> {
    use crate::containers;
    let record = containers::find(engine, reference)?;
    let logs = containers::logs_dir(engine, &record.id)?;
    let starts = |logs: &Path| -> crate::Result<Vec<(u64, PathBuf)>> {
        containers::starts(logs)
            .map_err(|error| LogsError::File {
                file: logs.to_owned(),
                error,
            })
            .map_err(crate::Error::from)
    };
    let up = || containers::has_monitor(engine, &record.id);
    // Its reader gone (F78): a follower ends at its next look, not only at
    // its next write.
    if options.follow {
        // Not started yet: its first start's, if it comes while it's up.
        while starts(&logs)?.is_empty() {
            if !up() || options.follow_over() {
                return Ok(ReadSummary::default());
            }
            std::thread::sleep(POLL);
        }
    }
    let mut starts = starts(&logs)?;
    let ends = std::cell::RefCell::new(LineEnds::new(sink));
    let whole = LogsOptions {
        tail: None,
        follow: false,
        identity: None,
        ..options.clone()
    };
    if !options.follow {
        return Ok(older_starts(&starts, options.tail, &whole, &ends)?);
    }
    let Some((number, newest)) = starts.pop() else {
        return Ok(ReadSummary::default());
    };
    let following = LogsOptions {
        identity: None,
        ..options.clone()
    };
    // Done once it isn't up, or once it has been started again.
    let done = || {
        options.follow_over()
            || !up()
            || containers::starts(&logs)
                .is_ok_and(|starts| starts.last().is_some_and(|(last, _)| *last > number))
    };
    let older = std::cell::Cell::new(None);
    let mut sink = |entry| ends.borrow_mut().pass(number, entry);
    let followed = match options.tail {
        None => {
            older.set(Some(older_starts(&starts, None, &whole, &ends)?));
            read(&newest, &following, done, &mut sink)
        }
        // The newest start's last lines are known once it's read to its
        // end (it's read once: what comes meanwhile isn't lost); the older
        // starts give the rest, handed on first.
        Some(wanted) => {
            let before_tail: BeforeTail<'_> = Box::new(|kept| {
                older.set(Some(older_starts(
                    &starts,
                    Some(wanted.saturating_sub(kept)),
                    &whole,
                    &ends,
                )?));
                Ok(())
            });
            read_then(&newest, &following, done, Some(before_tail), &mut sink)
        }
    };
    let summary = match followed {
        // Its newest start never wrote its `log.json` (it failed, and the
        // container isn't up): it has nothing of its own; the older starts
        // give all the tail.
        Err(LogsError::Info {
            source: InfoError::Read(error),
            ..
        }) if error.kind() == std::io::ErrorKind::NotFound => ReadSummary::default(),
        followed => followed?,
    };
    let older = match older.take() {
        Some(older) => older,
        None => older_starts(&starts, options.tail, &whole, &ends)?,
    };
    Ok(ReadSummary {
        skipped: summary.skipped + older.skipped,
    })
}

/// The starts `starts`' logs, read whole (`tail`: their last this many
/// entries in all), oldest first, to `ends`. A start without its
/// `log.json` yet (made, and not written to: being started) has none.
fn older_starts<S: FnMut(Entry) -> std::io::Result<()>>(
    starts: &[(u64, PathBuf)],
    tail: Option<usize>,
    whole: &LogsOptions,
    ends: &std::cell::RefCell<LineEnds<S>>,
) -> Result<ReadSummary, LogsError> {
    let mut summary = ReadSummary::default();
    let mut read_start = |dir: &Path,
                          tail: Option<usize>,
                          sink: &mut dyn FnMut(Entry) -> std::io::Result<()>|
     -> Result<(), LogsError> {
        match read(
            dir,
            &LogsOptions {
                tail,
                ..whole.clone()
            },
            || true,
            sink,
        ) {
            Err(LogsError::Info {
                source: InfoError::Read(error),
                ..
            }) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            read => {
                summary.skipped += read?.skipped;
                Ok(())
            }
        }
    };
    match tail {
        None => {
            for (number, dir) in starts {
                read_start(dir, None, &mut |entry| {
                    ends.borrow_mut().pass(*number, entry)
                })?;
            }
        }
        // From the newest backwards, as many as are still wanted from each
        // (at most that many held), then handed on oldest first.
        Some(mut left) => {
            let mut kept = Vec::new();
            for (number, dir) in starts.iter().rev() {
                if left == 0 {
                    break;
                }
                let mut entries = Vec::new();
                read_start(dir, Some(left), &mut |entry| {
                    entries.push(entry);
                    Ok(())
                })?;
                left = left.saturating_sub(entries.len());
                kept.push((*number, entries));
            }
            for (number, entries) in kept.into_iter().rev() {
                for entry in entries {
                    ends.borrow_mut()
                        .pass(number, entry)
                        .map_err(LogsError::Sink)?;
                }
            }
        }
    }
    Ok(summary)
}

/// Hands entries on to a sink, ending a line one start left open (an
/// entry not [`complete`](Entry::complete)), on either stream, before
/// another start's: one start's output never runs on into the next's.
struct LineEnds<S> {
    sink: S,
    /// The start the last entry came from.
    start: Option<u64>,
    /// Each stream's line left open, by its time: stdout's, then stderr's.
    open: [Option<Timestamp>; 2],
}

impl<S: FnMut(Entry) -> std::io::Result<()>> LineEnds<S> {
    fn new(sink: S) -> Self {
        Self {
            sink,
            start: None,
            open: [None, None],
        }
    }

    /// Hands on `entry`, of start `start`.
    fn pass(&mut self, start: u64, entry: Entry) -> std::io::Result<()> {
        if self.start.is_some_and(|previous| previous != start) {
            for stream in [Stream::Stdout, Stream::Stderr] {
                if let Some(time) = self.open[stream.index()].take() {
                    (self.sink)(Entry {
                        time,
                        stream,
                        text: Vec::new(),
                        complete: true,
                    })?;
                }
            }
        }
        self.start = Some(start);
        self.open[entry.stream.index()] = (!entry.complete).then_some(entry.time);
        (self.sink)(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line one start left open is ended before another start's
    /// output, never before more of its own.
    #[test]
    fn a_start_never_runs_on_into_the_next() {
        let mut got = Vec::new();
        let mut ends = LineEnds::new(|entry: Entry| {
            got.push((String::from_utf8(entry.text).unwrap(), entry.complete));
            Ok(())
        });
        let entry = |text: &str, complete| Entry {
            time: Timestamp::from_unix(0, 0).unwrap(),
            stream: Stream::Stdout,
            text: text.as_bytes().to_vec(),
            complete,
        };
        ends.pass(1, entry("$ ", false)).unwrap();
        ends.pass(1, entry("typed", true)).unwrap();
        ends.pass(1, entry("prompt> ", false)).unwrap();
        // A whole line on the other stream leaves this one open.
        ends.pass(
            1,
            Entry {
                stream: Stream::Stderr,
                ..entry("warned", true)
            },
        )
        .unwrap();
        ends.pass(2, entry("next", true)).unwrap();
        ends.pass(3, entry("whole", true)).unwrap();
        let expected = [
            ("$ ", false),
            ("typed", true),
            ("prompt> ", false),
            ("warned", true),
            ("", true),
            ("next", true),
            ("whole", true),
        ];
        assert_eq!(
            got,
            expected.map(|(text, complete)| (text.to_owned(), complete))
        );
    }
}
