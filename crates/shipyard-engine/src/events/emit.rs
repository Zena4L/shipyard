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

/// An event about the container `record` is: `action`, now, with its
/// name and image.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn container(record: &crate::containers::Record, action: &str) -> Event {
    let event =
        Event::new(Kind::Container, action, record.id.as_str()).with("name", record.name.as_str());
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
