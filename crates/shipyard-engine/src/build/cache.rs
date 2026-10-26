//! The build cache (step 12.3, D12.3-5): BuildKit's state, a directory per
//! build context (`state-<16 hex>/` in the builder's directory: a rootless
//! caller's `builder/` in its store, root's `<store>-builder/`), so a
//! project's builds share theirs and nothing crosses projects. Its size for
//! `system df`'s `Build Cache` row ([`usage`]); emptied by `builder prune`,
//! `system prune` and the API's `/build/prune` ([`prune`]), each state under
//! its lock (a build running keeps its own).

use std::path::{Path, PathBuf};

use crate::Engine;
use crate::error::{Context as _, Result};

/// The builder's directory in the image store.
pub(crate) const BUILDER_DIR: &str = "builder";

/// The prefix of a context's state directory.
pub(crate) const STATE_PREFIX: &str = "state-";

/// The name of the state directory of the context at `context` (its
/// canonical path): `state-` and its digest's first 16 hex digits.
#[must_use]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a build runs on Linux only")
)]
pub(super) fn state_name(context: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let digest = shipyard_image::Digest::sha256(context.as_os_str().as_bytes());
    format!("{STATE_PREFIX}{}", &digest.hex()[..16])
}

/// The builder's directory of the store `engine` uses: a rootless caller's
/// in its store (`builder/`: the caller is its builder's root); root's
/// beside it (`<store>-builder/`, 0711), since root's builder is another
/// ID (from 2100000000) that can't enter the store (root's 0700), and
/// every place in it is that ID's, 0700.
pub(crate) fn builder_dir(engine: &Engine) -> Result<PathBuf> {
    let store = crate::image::open_store(engine)?;
    Ok(builder_dir_of(
        store.root(),
        nix::unistd::geteuid().is_root(),
    ))
}

/// [`builder_dir`] of the store at `store`, for root or not.
pub(crate) fn builder_dir_of(store: &Path, root: bool) -> PathBuf {
    if !root {
        return store.join(BUILDER_DIR);
    }
    // By its components: `/srv/store/` is `/srv/store` (a trailing `/`
    // would put it inside the store, which root's builder can't enter).
    let store: PathBuf = store.components().collect();
    let mut beside = store.into_os_string();
    beside.push("-builder");
    PathBuf::from(beside)
}

/// Every state in `builder` (a state's directory, or only what a killed
/// build of it left: its lock, a `….run-…` directory), with its entries: its directory (if it has
/// one) and every build directory of it (`state.run-…`), by name; never its
/// lock. Read in one pass: a host that builds in a new directory each time
/// leaves a lock a context, and a listing a state made each build's sweep
/// grow as their square (12.5c3's Murphy review).
fn entries_by_state(builder: &Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut by_state: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for name in std::fs::read_dir(builder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(STATE_PREFIX))
    {
        let (state, rest) = name.split_once('.').unwrap_or((&name, ""));
        let entries = by_state.entry(state.to_owned()).or_default();
        if rest.is_empty() || rest.starts_with("run-") {
            entries.push(name.clone());
        }
    }
    by_state
}

/// The entries of `state` in `builder` ([`entries_by_state`]'s).
fn entries_of(builder: &Path, state: &str) -> Vec<String> {
    entries_by_state(builder).remove(state).unwrap_or_default()
}

/// Moves `state`'s directory in `builder` aside, to a build directory's
/// name of its own (`state.run-<12 hex>`), under its lock: a removal killed
/// half way (a Ctrl-C during another build's sweep) then leaves a build
/// directory the state's next build removes, never half a state that build
/// would use (12.5c3's Murphy review). The name it has now.
fn set_aside(builder: &Path, state: &str) -> Result<String> {
    let aside = format!(
        "{state}.run-{}",
        shipyard_sys::hex::lower(&crate::random::bytes::<6>()?)
    );
    std::fs::rename(builder.join(state), builder.join(&aside))
        .map_err(|error| crate::Error::wrap(crate::ErrorKind::Storage, error))
        .with_context(|| format!("setting the build cache {state} aside"))?;
    Ok(aside)
}

/// `state`'s lock, tried once: `None` if a build holds it.
///
/// # Errors
///
/// The lock file can't be opened, or `flock` failed otherwise.
fn try_state_lock(builder: &Path, state: &str) -> Result<Option<nix::fcntl::Flock<std::fs::File>>> {
    let lock = shipyard_sys::fs::open_lock_file(&builder.join(format!("{state}.lock")))
        .with_context(|| format!("opening the lock of the build cache {state}"))?;
    match shipyard_sys::lock::lock_until(
        lock,
        shipyard_sys::lock::Mode::Exclusive,
        std::time::Instant::now(),
        std::time::Duration::ZERO,
    ) {
        Ok(held) => Ok(Some(held)),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
        Err(errno) => Err(crate::Error::wrap(
            crate::ErrorKind::Storage,
            std::io::Error::from(errno),
        ))
        .with_context(|| format!("locking the build cache {state}")),
    }
}

/// What the build cache takes: one entry a context's state, its bytes
/// (`complete` false where a directory couldn't be read: a rootless
/// caller's subordinate IDs' files); a state a build is using is active,
/// the rest reclaimable.
///
/// # Errors
///
/// The store can't be opened.
pub fn usage(engine: &Engine) -> Result<crate::image::Usage> {
    let builder = builder_dir(engine)?;
    let mut usage = crate::image::Usage {
        total: 0,
        active: 0,
        bytes: 0,
        reclaimable: 0,
        complete: true,
    };
    let Ok(dir) = shipyard_image::fsat::open_dir(&builder) else {
        return Ok(usage);
    };
    for (state, entries) in entries_by_state(&builder) {
        // Its lock alone (pruned, or never built): no cache.
        if entries.is_empty() {
            continue;
        }
        let mut bytes = 0_u64;
        for name in entries {
            let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&name));
            bytes = bytes.saturating_add(taken.bytes);
            usage.complete &= taken.complete;
        }
        usage.total += 1;
        usage.bytes = usage.bytes.saturating_add(bytes);
        if matches!(try_state_lock(&builder, &state), Ok(None)) {
            usage.active += 1;
        } else {
            usage.reclaimable = usage.reclaimable.saturating_add(bytes);
        }
    }
    Ok(usage)
}

/// The label a builder carries: its context's state, so a later build of
/// that context, or a prune of the cache, finds (and removes) one a killed
/// build left.
pub(crate) const BUILDER_LABEL: &str = "com.shipyard.builder";

/// Removes (forced) every builder of the context's `state`: a killed
/// build's, still running (a hung `RUN` runs on, unbounded) or never
/// started. Called with `state`'s lock held, so never a running build's.
/// What couldn't be removed, said.
pub(crate) fn remove_builders(engine: &Engine, state: &str) -> Vec<String> {
    let records = match crate::containers::list(engine) {
        Ok(records) => records,
        Err(error) => {
            return vec![format!(
                "the builders a killed build may have left: {error:#}"
            )];
        }
    };
    records
        .into_iter()
        .filter(|record| {
            record
                .labels
                .as_map()
                .get(BUILDER_LABEL)
                .map(String::as_str)
                == Some(state)
        })
        .filter_map(|record| {
            crate::run::remove(engine, record.id.as_str(), true)
                .err()
                .map(|error| {
                    format!(
                        "the builder {} a killed build left: {error:#} (shipyard rm -f {})",
                        record.name, record.name
                    )
                })
        })
        .collect()
}

/// What a prune of the build cache did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    /// The states removed, by name.
    pub removed: Vec<String>,
    /// The states a build was using, left.
    pub busy: Vec<String>,
    /// The bytes they took (at least: `complete` false).
    pub bytes: u64,
    /// Whether every byte could be counted.
    pub complete: bool,
}

/// Empties the build cache: every context's state no build is using (its
/// lock only tried: a build running keeps its own, said), removed whole
/// with what killed builds of it left (their directories, copied secrets
/// among them), never pulling BuildKit to do it; a rootless caller's
/// through the storage helper (its files are subordinate IDs').
///
/// # Errors
///
/// The store can't be opened; a lock can't be tried; a state couldn't be
/// removed.
pub fn prune(engine: &Engine) -> Result<Pruned> {
    prune_where(engine, &builder_dir(engine)?, |_| true)
}

/// How long a context's state may go unused before a build of another
/// context removes it (12.5's review: a host that builds in a new directory
/// each time, as CI does, would keep each one's cache for good).
pub(crate) const UNUSED_FOR: std::time::Duration = std::time::Duration::from_hours(7 * 24);

/// When `state` was last used: the later of its lock's time (each build
/// stamps it once it holds it, [`mark_used`]) and its directory's; `None`
/// if neither can be looked at.
fn last_used(builder: &Path, state: &str) -> Option<std::time::SystemTime> {
    let time = |name: &str| {
        std::fs::symlink_metadata(builder.join(name))
            .and_then(|metadata| metadata.modified())
            .ok()
    };
    time(&format!("{state}.lock")).max(time(state))
}

/// Stamps a build's use of its state on its lock file, which the build
/// holds (`lock`): what [`sweep_unused`] reads. One that can't be stamped
/// is the state's age as it was (said nowhere: the cache stays, at worst
/// a week early gone).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a build runs on Linux only")
)]
pub(crate) fn mark_used(lock: &std::fs::File) {
    let _ = lock.set_modified(std::time::SystemTime::now());
}

/// Removes the states of other contexts than `own` that no build has used
/// for [`UNUSED_FOR`] by `now`, as [`prune`] does (each only if its lock
/// is free at once: a build of it goes on); what it removed.
///
/// # Errors
///
/// As [`prune`]'s.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a build runs on Linux only")
)]
pub(crate) fn sweep_unused(
    engine: &Engine,
    builder: &Path,
    own: &str,
    now: std::time::SystemTime,
) -> Result<Pruned> {
    prune_where(engine, builder, |state| unused(builder, state, own, now))
}

/// Whether [`sweep_unused`] takes `state`: another context's than `own`,
/// last used [`UNUSED_FOR`] or more before `now` (one stamped later than
/// `now`, by a clock set back, isn't).
fn unused(builder: &Path, state: &str, own: &str, now: std::time::SystemTime) -> bool {
    state != own
        && last_used(builder, state).is_some_and(|used| {
            now.duration_since(used)
                .is_ok_and(|unused| unused >= UNUSED_FOR)
        })
}

/// [`prune`] of the states in `builder` that `wanted` names.
fn prune_where(engine: &Engine, builder: &Path, wanted: impl Fn(&str) -> bool) -> Result<Pruned> {
    let mut pruned = Pruned {
        complete: true,
        ..Pruned::default()
    };
    let Ok(dir) = shipyard_image::fsat::open_dir(builder) else {
        return Ok(pruned);
    };
    let mut names = Vec::new();
    let mut held = Vec::new();
    for (state, entries) in entries_by_state(builder) {
        if entries.is_empty() || !wanted(&state) {
            continue;
        }
        let Some(lock) = try_state_lock(builder, &state)? else {
            pruned.busy.push(state);
            continue;
        };
        // A killed build's builder first: it may still run, on these files.
        let left = remove_builders(engine, &state);
        if !left.is_empty() {
            for what in left {
                engine.warn(crate::report::WarningKind::LeftForLater, what);
            }
            pruned.busy.push(state);
            continue;
        }
        // Read again under its lock (a build of it may have ended
        // meanwhile), its directory set aside first.
        let mut entries = entries_of(builder, &state);
        if let Some(own) = entries.iter_mut().find(|name| **name == state) {
            *own = set_aside(builder, &state)?;
        }
        for name in entries {
            let usage = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&name));
            pruned.bytes = pruned.bytes.saturating_add(usage.bytes);
            pruned.complete &= usage.complete;
            names.push(name);
        }
        held.push(lock);
        pruned.removed.push(state);
    }
    if !names.is_empty() {
        crate::image::remove_builder_states(engine, &names)?;
    }
    drop(held);
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context's state is named by its path alone: the same path the same
    /// name, another path another, 16 hex digits after the prefix.
    #[test]
    fn a_context_s_state_is_named_by_its_path() {
        let one = state_name(Path::new("/srv/app"));
        assert_eq!(one, state_name(Path::new("/srv/app")));
        assert_ne!(one, state_name(Path::new("/srv/app2")));
        let hex = one.strip_prefix(STATE_PREFIX).unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    /// A rootless caller's builder is in its store; root's beside it.
    #[test]
    fn root_s_builder_is_beside_the_store() {
        let store = Path::new("/var/lib/shipyard/images");
        assert_eq!(builder_dir_of(store, false), store.join("builder"));
        assert_eq!(
            builder_dir_of(store, true),
            Path::new("/var/lib/shipyard/images-builder")
        );
        assert_eq!(
            builder_dir_of(Path::new("/var/lib/shipyard/images/"), true),
            Path::new("/var/lib/shipyard/images-builder"),
            "a trailing slash"
        );
    }

    /// A state is unused by the later of its lock's and its directory's
    /// times, a week or more before now; never the build's own, and never
    /// one stamped after now.
    #[test]
    fn a_state_unused_for_a_week_is_found() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("shipyard-unused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let ago = |days: u64| now - Duration::from_hours(days * 24);
        let stamp = |name: &str, when: SystemTime| {
            let path = dir.join(name);
            if std::path::Path::new(name).extension().is_some() {
                std::fs::write(&path, b"").unwrap();
            } else {
                std::fs::create_dir(&path).unwrap();
            }
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        // Old by both, old lock and recent directory, recent lock and old
        // directory, one past the week by a minute, one just short of it,
        // one from a clock set back.
        stamp("state-01.lock", ago(30));
        stamp("state-01", ago(30));
        stamp("state-02.lock", ago(30));
        stamp("state-02", ago(1));
        stamp("state-03.lock", ago(1));
        stamp("state-03", ago(30));
        stamp("state-04.lock", ago(7) - Duration::from_secs(60));
        stamp("state-05.lock", ago(7) + Duration::from_secs(60));
        stamp("state-06.lock", now + Duration::from_hours(48));
        let taken: Vec<&str> = [
            "state-01", "state-02", "state-03", "state-04", "state-05", "state-06", "state-07",
        ]
        .into_iter()
        .filter(|state| unused(&dir, state, "state-xx", now))
        .collect();
        assert_eq!(taken, ["state-01", "state-04"]);
        assert!(
            !unused(&dir, "state-01", "state-01", now),
            "the build's own"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One listing groups every state's entries (its lock never one of
    /// them, a state of a lock alone has none); a state set aside is a
    /// build directory of its own, what its next build or a prune removes.
    #[test]
    fn a_state_is_listed_once_and_set_aside_whole() {
        let dir = std::env::temp_dir().join(format!("shipyard-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state-aa/cache")).unwrap();
        std::fs::write(dir.join("state-aa.lock"), b"").unwrap();
        std::fs::create_dir_all(dir.join("state-aa.run-01")).unwrap();
        std::fs::write(dir.join("state-bb.lock"), b"").unwrap();
        let by_state = entries_by_state(&dir);
        let mut aa = by_state["state-aa"].clone();
        aa.sort();
        assert_eq!(aa, ["state-aa", "state-aa.run-01"]);
        assert!(by_state["state-bb"].is_empty());
        let aside = set_aside(&dir, "state-aa").unwrap();
        assert!(aside.starts_with("state-aa.run-"), "{aside}");
        assert!(!dir.join("state-aa").exists());
        assert!(dir.join(&aside).join("cache").is_dir());
        assert!(dir.join("state-aa.lock").exists(), "its lock kept");
        let mut aa = entries_of(&dir, "state-aa");
        aa.sort();
        assert_eq!(aa.len(), 2, "{aa:?}");
        // What a removal takes: a build directory's name.
        crate::image::remove_states_in(&dir, &aa).unwrap();
        assert!(entries_of(&dir, "state-aa").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The states are found from every entry of theirs (a killed build's
    /// lock or directory alone too), each once.
    #[test]
    fn a_state_is_found_from_what_it_left() {
        let dir = std::env::temp_dir().join(format!("shipyard-states-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state-aa")).unwrap();
        std::fs::write(dir.join("state-aa.lock"), b"").unwrap();
        std::fs::create_dir_all(dir.join("state-bb.run-01")).unwrap();
        std::fs::write(dir.join("other"), b"").unwrap();
        assert_eq!(
            entries_by_state(&dir).into_keys().collect::<Vec<_>>(),
            ["state-aa", "state-bb"]
        );
        let mut of = entries_of(&dir, "state-bb");
        of.sort();
        assert_eq!(of, ["state-bb.run-01"]);
        // A removal takes those names alone, never a path or another name.
        for bad in [
            "../x",
            "state-aa/..",
            "state-",
            "state-zz",
            "other",
            "state-aa.lock",
            "state-aa.run-x/y",
        ] {
            assert!(
                crate::image::remove_states_in(&dir, &[bad.to_owned()]).is_err(),
                "{bad}"
            );
        }
        crate::image::remove_states_in(
            &dir,
            &["state-aa".to_owned(), "state-bb.run-01".to_owned()],
        )
        .unwrap();
        assert!(!dir.join("state-aa").exists() && !dir.join("state-bb.run-01").exists());
        assert!(dir.join("other").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
