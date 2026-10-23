//! Writing the journal: an event appended as one line under a shared lock,
//! and the journal rotated by size.

use std::io::Write as _;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::SystemTime;

use super::{Event, FILE, Kind, LOCK, ROTATE_AT, ROTATED};
use crate::Engine;
use crate::error::{Context as _, Result};

/// Writes that `kind` `id` had `action` done to it, now.
pub(crate) fn emit_about(engine: &Engine, kind: Kind, action: &str, id: &str) {
    emit(engine, &Event::new(kind, action, id));
}

/// Writes that a prune of `kind`s removed `removed` things.
pub(crate) fn emit_pruned(engine: &Engine, kind: Kind, removed: usize) {
    emit(
        engine,
        &Event::new(kind, "prune", "").with("removed", removed),
    );
}

/// Writes that a prune of `kind`s removed `removed` things, reclaiming
/// `reclaimed` bytes (Docker's `prune` event's attribute).
pub(crate) fn emit_reclaimed(engine: &Engine, kind: Kind, removed: usize, reclaimed: u64) {
    emit(
        engine,
        &Event::new(kind, "prune", "")
            .with("removed", removed)
            .with("reclaimed", reclaimed),
    );
}

/// The most bytes of a container's labels its events carry: the journal's
/// line is bounded (`MAX_LINE`), and what the engine says of the container
/// (its name, image, exit code) must fit beside them, whole.
const LABELS_BUDGET: usize = 2048;

/// The labels a container's events carry, of `labels`: compose's own
/// first (`com.docker.compose.*`: what compose's filters follow), then the
/// rest in order, each whole, while they fit in [`LABELS_BUDGET`].
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "read by `container`, which is Linux's (and the tests)"
    )
)]
pub(crate) fn event_labels(
    labels: &std::collections::BTreeMap<String, String>,
) -> Vec<(&str, &str)> {
    let compose = |key: &str| key.starts_with("com.docker.compose.");
    let ordered = labels
        .iter()
        .filter(|(key, _)| compose(key))
        .chain(labels.iter().filter(|(key, _)| !compose(key)));
    let mut left = LABELS_BUDGET;
    let mut kept = Vec::new();
    for (key, value) in ordered {
        // Quoted, a colon and a comma, in the line's JSON.
        let size = key.len() + value.len() + 6;
        if size <= left {
            left -= size;
            kept.push((key.as_str(), value.as_str()));
        }
    }
    kept
}

/// An event about the container `record` is: `action`, now, with its
/// labels (as many as fit: [`event_labels`]), then its name and image
/// (Docker's attributes: a label of either name gives way).
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn container(record: &crate::containers::Record, action: &str) -> Event {
    let event = event_labels(record.labels.as_map())
        .into_iter()
        .fold(
            Event::new(Kind::Container, action, record.id.as_str()),
            |event, (key, value)| event.with(key, value),
        )
        .with("name", record.name.as_str());
    match &record.config.image {
        Some(image) => event.with("image", image),
        None => event,
    }
}

/// Writes the end of the container `record` is (its exit, if it has one):
/// at the time its monitor saw it (else now), an OOM kill first, as
/// Docker says it, then `die` with its exit code.
#[cfg(target_os = "linux")]
pub(crate) fn container_ended(engine: &Engine, record: &crate::containers::Record) {
    let Some(exit) = &record.exit else {
        return;
    };
    let at = record.finished.unwrap_or_else(SystemTime::now);
    if exit.oom.is_some() {
        emit(engine, &container(record, "oom").at(at));
    }
    let mut died = container(record, "die").at(at);
    if let Some(code) = exit.code {
        died = died.with("exitCode", code);
    }
    emit(engine, &died);
}

/// Appends `event` to `engine`'s journal. Never fails what it's said
/// about: a journal that can't be written is warned about (once a
/// process), and the event is lost.
pub(crate) fn emit(engine: &Engine, event: &Event) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if let Err(error) = append(engine, event)
        && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        engine.warn(
            crate::report::WarningKind::LeftForLater,
            format!("the events journal couldn't be written: {error:#}"),
        );
    }
}

/// [`emit`]'s work: the line appended under the shared lock, then the
/// journal rotated if it's grown past [`ROTATE_AT`] (only if the lock can
/// be had alone at once).
fn append(engine: &Engine, event: &Event) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = made_journal_dir(engine)?;
    let mut line = event.to_line();
    line.push('\n');
    let lock = crate::fs::open_lock_file(&dir.join(LOCK))?;
    let shared = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockShared)
        .map_err(|(_, errno)| errno)
        .context("locking the events journal")?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(FILE))
        .context("opening the events journal")?;
    // One write: the line whole at the end, whoever else appends.
    file.write_all(line.as_bytes())
        .context("writing the events journal")?;
    let size = file.metadata().map_or(0, |meta| meta.len());
    drop(file);
    if size > ROTATE_AT {
        let lock = shared
            .unlock()
            .map_err(|(_, errno)| errno)
            .context("unlocking the events journal")?;
        // Alone, or not now: the next appender rotates.
        if let Ok(_alone) =
            nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
            && std::fs::metadata(dir.join(FILE)).is_ok_and(|meta| meta.len() > ROTATE_AT)
        {
            std::fs::rename(dir.join(FILE), dir.join(ROTATED))
                .context("rotating the events journal")?;
        }
    }
    Ok(())
}

/// The journal's directory, made if it isn't there (private, as the
/// stores are), for an appender.
fn made_journal_dir(engine: &Engine) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = engine.paths.events()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("making {}", dir.display()))?;
    crate::fs::check_private_dir(&dir, "the events journal's directory")?;
    Ok(dir)
}
