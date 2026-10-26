//! A run's volumes and `/etc` files, bound into its root filesystem.

use std::path::{Path, PathBuf};

use shipyard_runtime::isolation::{Mount, Root};
use shipyard_storage::{VolumeHold, VolumeMount};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::network::{ETC_FILES, RunDir};

/// Binds the run's `/etc` files (`etc`) into `root`, before the volumes, so a
/// volume a user puts on one of them wins. An image's root takes them all
/// (one that's missing is made in the overlay's writable layer, which goes
/// with the run); a root filesystem of the user's own (`--rootfs`) only
/// those it has as regular files: Shipyard never writes into it. (A link,
/// like Debian's `resolv.conf` to systemd's, is followed inside the root to
/// bind on its target, which would be made if it's missing; and `etc`
/// itself is looked at without following a link.)
pub(super) fn bind_etc_files(root: &mut Root, etc: &RunDir) {
    let is_file = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
    };
    let etc_dir = root.path.join("etc");
    let has_etc = std::fs::symlink_metadata(&etc_dir).is_ok_and(|metadata| metadata.is_dir());
    for name in ETC_FILES {
        if root.overlay.is_none() && !(has_etc && is_file(&etc_dir.join(name))) {
            continue;
        }
        root.mounts
            .push(Mount::bind(etc.file(name), Path::new("/etc").join(name)));
    }
}

/// `source` made absolute, symlinks resolved, as UTF-8.
pub(super) fn canonical_source(source: &str) -> Result<String> {
    std::fs::canonicalize(source)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(|| source.to_owned())?
        .into_os_string()
        .into_string()
        .map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                format!("{source}: a path that isn't UTF-8"),
            )
        })
}

/// Binds `volumes` into `root`: a host path as it is (resolved again, as
/// close to the run as can be), a named volume's directory (made or not:
/// [`hold_volumes`] makes it). Returns the named volumes' names.
pub(super) fn bind_volumes(
    engine: &Engine,
    root: &mut Root,
    volumes: Vec<VolumeMount>,
    made: &[MadeSource],
) -> Result<Vec<String>> {
    let mut named = Vec::new();
    let mut store = None;
    for mount in volumes {
        // One Shipyard made: by descriptor from where it was made, never by
        // its path (11.7).
        if let Some(made) = made
            .iter()
            .find(|made| made.source == Path::new(&mount.source))
        {
            let mut bind = Mount::bind_made(&made.dir, &made.below, mount.destination);
            if mount.read_only {
                bind.flags
                    .push(shipyard_runtime::isolation::MountFlag::ReadOnly);
            }
            root.mounts.push(bind);
            continue;
        }
        let source = if shipyard_storage::volume::is_named_volume(&mount.source) {
            let store = match &store {
                Some(store) => store,
                None => store.insert(crate::volumes::open(engine)?),
            };
            let data = store.data_path(&mount.source)?;
            named.push(mount.source);
            data
        } else {
            PathBuf::from(canonical_source(&mount.source)?)
        };
        let mut bind = Mount::bind(source, mount.destination);
        if mount.read_only {
            bind.flags
                .push(shipyard_runtime::isolation::MountFlag::ReadOnly);
        }
        root.mounts.push(bind);
    }
    Ok(named)
}

/// Makes the named volumes `names` if new, and holds them: nothing removes
/// a volume while its hold lives.
pub(super) fn hold_volumes(engine: &Engine, names: &[String]) -> Result<Vec<VolumeHold>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::volumes::open(engine)?;
    names
        .iter()
        .map(|name| {
            let held = store
                .hold(name)
                .with_context(|| format!("the volume {name}"))?;
            if held.created() {
                crate::events::emit_about(engine, crate::events::Kind::Volume, "create", name);
            }
            Ok(held)
        })
        .collect()
}

/// Holds the named volumes `names`, as [`hold_volumes`] does, without
/// making any: for a copy from a stopped container, whose volume may have
/// been removed since.
///
/// # Errors
///
/// A volume that doesn't exist ([`ErrorKind::NotFound`]), or can't be held.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "copies need Linux")
)]
pub(super) fn hold_existing_volumes(engine: &Engine, names: &[String]) -> Result<Vec<VolumeHold>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::volumes::open(engine)?;
    names
        .iter()
        .map(|name| {
            store
                .hold_existing(name)
                .with_context(|| format!("the container's volume {name}"))
        })
        .collect()
}

/// A `--volume` spec's parts, as Docker reads them: `SOURCE:DEST[:MODES]`
/// (`SOURCE` a name or a host path), or a lone place `DEST[:MODES]` (an
/// anonymous volume), `DEST` absolute; `MODES` comma-joined (`ro`,
/// `nocopy`). `None` for anything else (the checks refuse it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpecParts<'a> {
    /// Its source, if it names one.
    pub(crate) source: Option<&'a str>,
    /// Where it mounts.
    pub(crate) destination: &'a str,
    /// Its modes, if it has any.
    pub(crate) modes: Option<&'a str>,
}

impl<'a> SpecParts<'a> {
    /// `spec`'s parts, if it has the shape of one.
    pub(crate) fn of(spec: &'a str) -> Option<Self> {
        let mut parts = spec.splitn(3, ':');
        let first = parts.next()?;
        let (source, destination, modes) = match (parts.next(), parts.next()) {
            (Some(second), modes) if second.starts_with('/') => (Some(first), second, modes),
            (modes, None) if first.starts_with('/') => (None, first, modes),
            _ => return None,
        };
        Some(Self {
            source,
            destination,
            modes,
        })
    }

    /// Its source, if it's a host path (a bind's).
    pub(crate) fn host_path(&self) -> Option<&'a str> {
        self.source.filter(|source| source.starts_with('/'))
    }
}

/// A `--volume` that asks for an anonymous volume: a lone absolute
/// destination, `/data` or `/data:ro` (Docker's `-v /data`); its text after
/// the name a new volume gets.
pub(crate) fn anonymous_form(spec: &str) -> Option<&str> {
    SpecParts::of(spec)
        .filter(|parts| parts.source.is_none() && matches!(parts.modes, None | Some("ro" | "rw")))
        .map(|_| spec)
}

/// A `Binds` entry's host path, if it binds one (`/host:/path[:modes]`):
/// not a named volume's, nor an anonymous one's lone place.
#[must_use]
pub fn bind_source(spec: &str) -> Option<PathBuf> {
    SpecParts::of(spec)
        .and_then(|parts| parts.host_path())
        .map(PathBuf::from)
}

/// Names each anonymous volume `specs` asks for (a new 64-hex name, its
/// `NAME:/path[:ro]` in the spec's place); the names given.
///
/// # Errors
///
/// The kernel's random bytes couldn't be read.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a create is Linux's")
)]
pub(super) fn name_anonymous(specs: &mut [String]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for spec in specs {
        if let Some(rest) = anonymous_form(spec) {
            let name = crate::docker::new_id()?;
            *spec = format!("{name}:{rest}");
            names.push(name);
        }
    }
    Ok(names)
}

/// Takes Docker's `nocopy` out of `specs`' modes (`-v NAME:/path:nocopy`,
/// `…:ro,nocopy`, a lone place's `/path:nocopy`): the record's volume
/// strings never hold it. The places it was asked for, which the create's
/// fill leaves alone (D11.4a2b-3).
///
/// # Errors
///
/// `nocopy` on a bind ([`ErrorKind::InvalidRequest`]): a host directory
/// is never filled.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a create is Linux's")
)]
pub(super) fn take_no_copy(specs: &mut [String]) -> Result<Vec<String>> {
    let mut places = Vec::new();
    for spec in specs {
        let Some(parts) = SpecParts::of(spec) else {
            continue;
        };
        let asked: Vec<&str> = parts
            .modes
            .map(|modes| modes.split(',').collect())
            .unwrap_or_default();
        if !asked.contains(&"nocopy") {
            continue;
        }
        if parts
            .source
            .is_some_and(|source| !shipyard_storage::volume::is_named_volume(source))
        {
            bail!(
                InvalidRequest,
                "nocopy is a volume's, not a bind's ({spec}): a host directory is never filled"
            );
        }
        let kept: Vec<&str> = asked.into_iter().filter(|mode| *mode != "nocopy").collect();
        let place = parts.destination.to_owned();
        let rest = match parts.source {
            Some(source) => format!("{source}:{place}"),
            None => place.clone(),
        };
        *spec = if kept.is_empty() {
            rest
        } else {
            format!("{rest}:{}", kept.join(","))
        };
        places.push(place);
    }
    Ok(places)
}

/// A bind source Shipyard made (step 11.7): `source` as it was asked,
/// `below` the deepest directory that was there already, `dir`, resolved
/// as Docker resolves it. Kept beside the record (`created.json` v6), so
/// each start makes and binds it again by descriptor from `dir`: a symlink
/// put on its path later (a container sharing a directory above it) is
/// refused, never followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeSource {
    /// The source as the bind asks for it.
    pub source: PathBuf,
    /// The directory that was there, canonical.
    pub dir: PathBuf,
    /// What was made below it: plain, relative.
    pub below: PathBuf,
}

impl MadeSource {
    /// One read back from a file: `dir` absolute and `below` plain and
    /// relative, or `None` (a file edited by hand never reaches a mount).
    #[must_use]
    pub fn checked(source: PathBuf, dir: PathBuf, below: PathBuf) -> Option<Self> {
        use std::path::Component;
        let plain = |path: &Path| path.components().all(|c| matches!(c, Component::Normal(_)));
        let ok = source.is_absolute()
            && dir.is_absolute()
            && dir
                .components()
                .skip(1)
                .all(|c| matches!(c, Component::Normal(_)))
            && below.components().next().is_some()
            && plain(&below);
        ok.then_some(Self { source, dir, below })
    }

    /// Makes what's missing below its directory again (a start's: it may
    /// have been removed), every component a directory reached without
    /// following a symlink.
    ///
    /// # Errors
    ///
    /// A symlink, or not a directory, where it made one
    /// ([`ErrorKind::InvalidRequest`]); its directory gone, or one that
    /// can't be made.
    pub fn make_again(&self) -> Result<()> {
        let dir = std::fs::File::open(&self.dir)
            .with_context(|| format!("opening {} for the bind's source", self.dir.display()))?;
        make_below(&dir, &self.below, &self.source)
    }

    /// [`MadeSource::make_again`] where its directory is still there, else
    /// made as a missing source is ([`make_bind_source`]: a `/tmp` a reboot
    /// emptied, as Docker makes it again): how this start binds it.
    ///
    /// # Errors
    ///
    /// As those two.
    pub fn made_for_this_start(&self) -> Result<Option<Self>> {
        if self.dir.is_dir() {
            self.make_again()?;
            return Ok(Some(self.clone()));
        }
        make_bind_source(&self.source)
    }
}

/// Makes the missing bind source `path` (the API's
/// `BindOptions.CreateMountpoint` and `Binds`, D11.4a2b-4): a directory,
/// and its parents, as the caller, mode 0755 less the umask; what it made,
/// if it made one. Anything there already (a file, a link) is left as it
/// is, and bound as Docker binds it (its path followed). What's missing is
/// made below the deepest directory that's there (resolved as Docker
/// would), each component reached without following a symlink: one put
/// there meanwhile is refused (11.7).
///
/// # Errors
///
/// A relative path ([`ErrorKind::InvalidRequest`]); a symlink where it
/// makes a directory (the same); one that can't be made.
pub fn make_bind_source(path: &Path) -> Result<Option<MadeSource>> {
    if !path.is_absolute() {
        bail!(
            InvalidRequest,
            "a bind's source must be an absolute path, not {}",
            path.display()
        );
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(None);
    }
    // The deepest directory there (`/` at worst), and what's below it.
    let Some(there) = path.ancestors().skip(1).find(|dir| dir.is_dir()) else {
        bail!(InvalidRequest, "{}: nothing of it is there", path.display());
    };
    let below = path
        .strip_prefix(there)
        .map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "a bind's source outside its parent",
            )
        })?
        .to_path_buf();
    let dir = std::fs::canonicalize(there)
        .with_context(|| format!("resolving {} for the bind's source", there.display()))?;
    let made = MadeSource::checked(path.to_path_buf(), dir, below).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidRequest,
            format!("{}: not a plain path to make", path.display()),
        )
    })?;
    made.make_again()?;
    Ok(Some(made))
}

/// `below` made in `dir`, as [`MadeSource::make_again`] says.
fn make_below(dir: &std::fs::File, below: &Path, source: &Path) -> Result<()> {
    use std::os::fd::AsFd as _;
    match shipyard_sys::fs::make_below(dir.as_fd(), below, 0o755) {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => bail!(
            InvalidRequest,
            "the bind's source {} has a symlink or a file where Shipyard makes a directory: \
             give the path it names instead",
            source.display()
        ),
        Err(error) => Err(Error::from(error))
            .with_context(|| format!("making the bind's source {}", source.display())),
    }
}

/// How long a fill waits for another one of the same volume.
#[cfg(target_os = "linux")]
const FILL_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Which fill [`fill_volumes`] does.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fill {
    /// A create's: each volume still empty (Docker's copy-up, at the
    /// create: D11.4a2b-1). One another create cut short and that holds
    /// something is never copied into (said, left as it is).
    Create,
    /// A first start's: only what this container's own create began and
    /// was cut short (its marker naming it), finished over what's there.
    Finish,
}

/// Fills container `record`'s volumes from its image, as `fill` says:
/// `record`'s root as its start would mount it (its own layer made, its
/// volumes made and held), each named or anonymous volume at a place not
/// in `no_copy`. One fill of a volume at a time (its fill lock, waited
/// for unless `stop` says to stop); each marked for `record` until it's
/// done. What the copy left out or changed is said.
///
/// # Errors
///
/// A fill that failed (D11.4a2b-2): what was copied stays, marked
/// unfinished (this container's first start finishes it); interrupted
/// ([`ErrorKind::Interrupted`]) when `stop` asked.
#[cfg(target_os = "linux")]
pub(super) fn fill_volumes(
    engine: &Engine,
    record: &crate::containers::Record,
    no_copy: &[String],
    fill: Fill,
    stop: &dyn Fn() -> bool,
) -> Result<()> {
    let store = crate::volumes::open(engine)?;
    let skipped = |place: &Path| {
        no_copy
            .iter()
            .any(|asked| Path::new(asked).components().eq(place.components()))
    };
    let ours = |name: &str| store.unfinished_fill(name).as_deref() == Some(record.id.as_str());
    if fill == Fill::Create {
        // A volume this create doesn't fill (`nocopy`): its fill lock taken
        // for a moment, once this container is placed, so a fill another's
        // first start finishes under that lock either sees this one among
        // the volume's users or is done before this one can use it (11.7).
        for spec in &record.config.volume {
            if let Ok(mount) = VolumeMount::parse(spec)
                && shipyard_storage::volume::is_named_volume(&mount.source)
                && skipped(&mount.destination)
            {
                drop(
                    store
                        .lock_fill(&mount.source, FILL_WAIT, stop)
                        .with_context(|| format!("the volume {}", mount.source))?,
                );
            }
        }
    }
    let wanted = record.config.volume.iter().any(|spec| {
        VolumeMount::parse(spec).is_ok_and(|mount| {
            shipyard_storage::volume::is_named_volume(&mount.source)
                && !skipped(&mount.destination)
                && (fill == Fill::Create || ours(&mount.source))
        })
    });
    if !wanted {
        return Ok(());
    }
    let (view, held) =
        super::files::stopped_view(engine, record, super::files::Wanted::WithMounts)?;
    if view.root.overlay.is_none() {
        return Ok(());
    }
    // The image's files alone: no volume over what's copied.
    let mut alone = Root::new(view.root.path.clone());
    alone.overlay.clone_from(&view.root.overlay);
    let images = shipyard_runtime::container::RootView {
        root: alone,
        id_maps: view.id_maps.clone(),
    };
    for mount in &view.root.mounts {
        let shipyard_runtime::isolation::MountKind::Bind { source, .. } = &mount.kind else {
            continue;
        };
        let Some(volume) = held
            .volumes
            .iter()
            .map(VolumeHold::volume)
            .find(|volume| &volume.data == source)
        else {
            continue;
        };
        if skipped(&mount.destination) {
            continue;
        }
        if stop() {
            bail!(Interrupted, "interrupted before its volumes were filled");
        }
        fill_one(
            engine,
            &store,
            record,
            (&images, volume, &mount.destination),
            fill,
            stop,
        )?;
    }
    Ok(())
}

/// [`fill_volumes`]' work for one `volume` at `place`, from `images`.
#[cfg(target_os = "linux")]
fn fill_one(
    engine: &Engine,
    store: &shipyard_storage::Volumes,
    record: &crate::containers::Record,
    (images, volume, place): (
        &shipyard_runtime::container::RootView,
        &shipyard_storage::Volume,
        &Path,
    ),
    fill: Fill,
    stop: &dyn Fn() -> bool,
) -> Result<()> {
    let filling = store
        .lock_fill(&volume.name, FILL_WAIT, stop)
        .map_err(|error| match error {
            shipyard_storage::VolumeError::Busy(_) if stop() => Error::new(
                ErrorKind::Interrupted,
                "interrupted while another fill of its volume ran",
            ),
            error => Error::from(error),
        })
        .with_context(|| format!("the volume {}", volume.name))?;
    let unfinished = filling.unfinished();
    let refill = match (fill, unfinished.as_deref()) {
        (Fill::Finish, Some(owner)) if owner == record.id.as_str() => {
            // Another container that mounts it may have written there since
            // (one made with `nocopy`, or before, and started): the fill is
            // given up rather than copied over its files (11.7's engine
            // review). Read under the fill lock.
            if let Some(who) = others_mounting(engine, record, &volume.name) {
                engine.warn(
                    crate::report::WarningKind::Runtime,
                    format!(
                        "the volume {}'s fill, left unfinished by this container's create, is \
                         given up: {who} mounts it too, and may have written there (its \
                         image's files may be missing there; a volume removed and made again \
                         is filled anew)",
                        volume.name
                    ),
                );
                return filling
                    .done()
                    .with_context(|| format!("the volume {}", volume.name));
            }
            true
        }
        (Fill::Finish, _) => return Ok(()),
        (Fill::Create, unfinished) => {
            // One the host can't read (rootless: the image's owner, a
            // subordinate ID here) was filled, or used, already.
            let empty =
                std::fs::read_dir(&volume.data).is_ok_and(|mut entries| entries.next().is_none());
            if !empty {
                // Used as it is now, by this container too: the fill left
                // unfinished is given up, never finished over what may be
                // written there from now on (11.6b's engine review: its
                // owner's first start would copy the image's files over
                // this one's).
                if let Some(owner) = unfinished {
                    engine.warn(
                        crate::report::WarningKind::Runtime,
                        format!(
                            "the volume {} holds a fill another container's create left \
                             unfinished ({owner}): it's used as it is, and that fill is given \
                             up (its image's files may be missing there; a volume removed and \
                             made again is filled anew)",
                            volume.name
                        ),
                    );
                    filling
                        .done()
                        .with_context(|| format!("the volume {}", volume.name))?;
                }
                return Ok(());
            }
            false
        }
    };
    let what = || {
        format!(
            "filling the volume {} from the image's {}",
            volume.name,
            place.display()
        )
    };
    filling.begin(record.id.as_str()).with_context(what)?;
    pause_for_a_test(engine);
    let said = crate::cp::fill(images, place, &volume.data, refill)
        .map_err(Error::from)
        .with_context(what)?;
    // On the disk before its marker goes (11.7): a power cut after a create
    // never leaves a volume marked whole that isn't. The volume's own
    // directory (the user's; `_data` may be a subordinate ID's alone,
    // rootless) is on the same filesystem. Failed: the marker stays, and
    // this container's first start fills it again.
    synced(&volume.data).with_context(what)?;
    for warning in said {
        engine.warn(
            crate::report::WarningKind::Runtime,
            format!("the volume {}: {warning}", volume.name),
        );
    }
    filling.done().with_context(what)
}

/// Who besides `record` mounts the volume `name`, if anyone may: their
/// names, or that one can't be told (a record that can't be read may be
/// such a one).
#[cfg(target_os = "linux")]
fn others_mounting(
    engine: &Engine,
    record: &crate::containers::Record,
    name: &str,
) -> Option<String> {
    match crate::containers::list_every(engine) {
        Ok(records) => {
            let others: Vec<String> = crate::volumes::users_of(&records, name)
                .into_iter()
                .filter(|user| user != record.name.as_str())
                .collect();
            (!others.is_empty()).then(|| others.join(", "))
        }
        Err(_) => Some("a container whose record can't be read".to_owned()),
    }
}

/// `syncfs(2)` on the filesystem `data` is on, through its parent (the
/// volume's own directory).
#[cfg(target_os = "linux")]
fn synced(data: &Path) -> Result<()> {
    let dir = data.parent().unwrap_or(data);
    let opened = std::fs::File::open(dir)
        .with_context(|| format!("opening {} to sync it", dir.display()))?;
    nix::unistd::syncfs(&opened)
        .map_err(std::io::Error::from)
        .with_context(|| format!("syncing {}", dir.display()))
}

/// A test build's pause in a fill, once it's marked (`Paths::fill_pause`):
/// this process's PID written there, then a minute's wait, for a test to
/// kill it mid-fill. A release has none.
#[cfg(target_os = "linux")]
fn pause_for_a_test(engine: &Engine) {
    if let Some(path) = engine.paths.fill_pause() {
        crate::test_pause::announce(path, &std::process::id().to_string());
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// Whether a mount of `specs` (`--volume`'s, an anonymous one's lone place
/// too) or of `tmpfs` is at `place` (`/data/` is `/data`): Docker's
/// `VOLUME`s and API `Volumes` there get no volume of their own.
#[must_use]
pub fn covers(specs: &[String], tmpfs: &[super::TmpfsMount], place: &Path) -> bool {
    let place: PathBuf = place.components().collect();
    let at = |destination: &Path| destination.components().collect::<PathBuf>() == place;
    specs
        .iter()
        .filter_map(|spec| SpecParts::of(spec).map(|parts| parts.destination))
        .any(|destination| at(Path::new(destination)))
        || tmpfs.iter().any(|mount| at(mount.destination()))
}

/// The most `VOLUME`s an image may declare: each is a volume made and a
/// hold at each start.
const MAX_IMAGE_VOLUMES: usize = 128;

/// The image's `VOLUME`s (`declared`) that no mount covers (one of
/// `specs`' destinations, or of `tmpfs`'), each an anonymous volume: its
/// spec added to `specs`, its name given. A path the runtime would refuse
/// (in `/proc`, say) is said and left in the container's layer.
///
/// # Errors
///
/// The kernel's random bytes couldn't be read.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a create is Linux's")
)]
pub(super) fn image_volumes(
    engine: &Engine,
    declared: &[String],
    specs: &mut Vec<String>,
    tmpfs: &[super::TmpfsMount],
) -> Result<Vec<String>> {
    if declared.len() > MAX_IMAGE_VOLUMES {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            format!(
                "the image declares {} volumes, more than a container takes ({MAX_IMAGE_VOLUMES})",
                declared.len()
            ),
        ));
    }
    let mut names = Vec::new();
    for path in declared {
        let destination: PathBuf = Path::new(path).components().collect();
        if covers(specs, tmpfs, &destination) {
            continue;
        }
        // Where a volume would hide what every container has: the
        // devices' file systems, or a run's own `/etc` files.
        let reserved = destination.starts_with("/dev")
            || ETC_FILES
                .iter()
                .any(|file| destination == Path::new("/etc").join(file));
        if reserved {
            engine.warn(
                crate::report::WarningKind::Runtime,
                format!(
                    "the image's volume {path} is kept in the container instead: it's the run's own"
                ),
            );
            continue;
        }
        // The runtime's rules for a mount's place (its source is moot).
        if let Err(error) = Mount::bind("/", &destination).validate() {
            engine.warn(
                crate::report::WarningKind::Runtime,
                format!("the image's volume {path} is kept in the container instead: {error}"),
            );
            continue;
        }
        let name = crate::docker::new_id()?;
        specs.push(format!("{name}:{}", destination.display()));
        names.push(name);
    }
    Ok(names)
}

/// Makes the anonymous volumes `names` (labelled as Docker labels them),
/// each pushed to `made` once it is (for the caller to undo); their holds,
/// for the caller to keep until the record names them.
///
/// # Errors
///
/// One couldn't be made (or was there already), or held.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a create is Linux's")
)]
pub(super) fn make_anonymous(
    engine: &Engine,
    names: &[String],
    made: &mut Vec<String>,
) -> Result<Vec<VolumeHold>> {
    let mut holds = Vec::new();
    for name in names {
        let hold = crate::volumes::create_anonymous(engine, name)
            .with_context(|| format!("the anonymous volume {name}"))?;
        made.push(name.clone());
        holds.push(hold);
    }
    Ok(holds)
}

/// The anonymous volumes container `id`'s create made (its
/// `created.json`), read before it's removed: none if that can't be read
/// (they're kept, for `volume prune`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a removal is Linux's")
)]
pub(crate) fn made_anonymous(engine: &Engine, id: &crate::containers::ContainerId) -> Vec<String> {
    crate::containers::created(engine, id)
        .map(|created| created.anonymous)
        .unwrap_or_default()
}

/// Removes the anonymous volumes a removed container's create `made`
/// (`rm -v`, `--rm`; its `created.json`): each still labelled anonymous
/// that no other container names (one that's in use is kept, said
/// nowhere: Docker's). A volume it was given by name, anonymous or not,
/// is never touched (Docker's: a mount with a source is kept).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a removal is Linux's")
)]
pub(crate) fn remove_anonymous(engine: &Engine, made: &[String]) {
    for name in made {
        let anonymous = crate::volumes::labels(engine, name)
            .is_ok_and(|labels| labels.contains_key(crate::docker::ANONYMOUS_VOLUME));
        if !anonymous {
            continue;
        }
        match crate::volumes::remove(engine, name) {
            Ok(()) => {}
            Err(error) if matches!(error.kind(), ErrorKind::InUse | ErrorKind::NotFound) => {}
            Err(error) => engine.warn(
                crate::report::WarningKind::LeftForLater,
                format!("its volume {name} couldn't be removed (volume prune will): {error:#}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

    /// A lone absolute place, read-only or not, asks for an anonymous
    /// volume; a named volume, a bind or a relative place doesn't. Named,
    /// each gets a 64-hex name of its own in its spec's place.
    #[test]
    fn a_lone_place_is_an_anonymous_volume() {
        for (spec, form) in [
            ("/data", Some("/data")),
            ("/data:ro", Some("/data:ro")),
            ("/data:rw", Some("/data:rw")),
            ("db:/data", None),
            ("/srv:/data", None),
            ("/srv:/data:ro", None),
            ("data", None),
            ("/data:z", None),
        ] {
            assert_eq!(super::anonymous_form(spec), form, "{spec}");
        }
        let mut specs = vec!["/a".to_owned(), "db:/b".to_owned(), "/c:ro".to_owned()];
        let names = super::name_anonymous(&mut specs).unwrap();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert!(names.iter().all(|name| name.len() == 64));
        assert_eq!(
            specs,
            [
                format!("{}:/a", names[0]),
                "db:/b".to_owned(),
                format!("{}:/c:ro", names[1])
            ]
        );
    }

    /// A place is covered by a volume, a bind, an anonymous volume's lone
    /// place or a tmpfs there, however its slashes are written; a place
    /// beside or under one isn't.
    #[test]
    fn a_place_is_covered_by_any_mount_there() {
        let specs = [
            "db:/var/db".to_owned(),
            "/srv:/srv/".to_owned(),
            "/cache:ro".to_owned(),
            "logs:/logs:ro,nocopy".to_owned(),
            "/tmpdata:nocopy".to_owned(),
        ];
        let tmpfs = [crate::run::TmpfsMount::parse("/run").unwrap()];
        let covers = |place: &str| super::covers(&specs, &tmpfs, std::path::Path::new(place));
        for place in [
            "/var/db", "/var/db/", "/srv", "/cache", "/cache/", "/run", "/logs", "/tmpdata",
        ] {
            assert!(covers(place), "{place}");
        }
        for place in ["/var", "/var/db/x", "/srv2", "/data"] {
            assert!(!covers(place), "{place}");
        }
    }

    /// `nocopy` is taken off a volume's modes, its place kept: alone, with
    /// `ro`, on a lone place; a bind's is refused; a spec without it is
    /// left as it is.
    #[test]
    fn no_copy_is_taken_off_the_spec() {
        let mut specs = [
            "db:/data:nocopy",
            "logs:/logs:ro,nocopy",
            "/cache:nocopy",
            "keep:/keep:ro",
            "/srv:/srv",
        ]
        .map(str::to_owned);
        let places = super::take_no_copy(&mut specs).unwrap();
        assert_eq!(places, ["/data", "/logs", "/cache"]);
        assert_eq!(
            specs,
            [
                "db:/data",
                "logs:/logs:ro",
                "/cache",
                "keep:/keep:ro",
                "/srv:/srv"
            ]
        );
        let mut bind = ["/srv:/srv:nocopy".to_owned()];
        assert_eq!(
            super::take_no_copy(&mut bind).unwrap_err().kind(),
            crate::error::ErrorKind::InvalidRequest
        );
    }

    /// A bind's source is made with its parents, 0755, and said made (below
    /// the deepest directory there, canonical); one that's there is kept,
    /// and not said; a relative one is refused. Made again after a symlink
    /// took a made directory's place (a container sharing a directory above
    /// it): refused, nothing made through it (11.7).
    #[test]
    fn a_bind_source_is_made_when_asked() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "shipyard-bind-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let made = dir.join("a/b");
        let said = super::make_bind_source(&made).unwrap().unwrap();
        assert_eq!(said.source, made);
        assert_eq!(
            said.dir,
            std::fs::canonicalize(std::env::temp_dir()).unwrap()
        );
        assert!(said.below.ends_with("a/b"), "{said:?}");
        let mode = std::fs::metadata(&made).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o700, 0o700, "{mode:o}");
        assert_eq!(mode & 0o022, 0, "never writable by others: {mode:o}");
        assert!(made.is_dir());
        std::fs::write(made.join("kept"), "x").unwrap();
        assert_eq!(super::make_bind_source(&made).unwrap(), None);
        assert!(made.join("kept").exists(), "one that's there is kept");
        std::fs::remove_dir_all(dir.join("a")).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("a")).unwrap();
        assert_eq!(
            said.make_again().unwrap_err().kind(),
            crate::error::ErrorKind::InvalidRequest
        );
        assert!(!outside.join("b").exists(), "nothing made through the link");
        // Its directory gone (a `/tmp` a reboot emptied): made again from
        // what's there now, and said so (11.7's Murphy review).
        std::fs::remove_file(dir.join("a")).unwrap();
        let gone = super::MadeSource::checked(made.clone(), dir.join("a"), "b".into()).unwrap();
        let again = gone.made_for_this_start().unwrap().unwrap();
        assert!(made.is_dir());
        assert_eq!(again.dir, std::fs::canonicalize(&dir).unwrap());
        assert_eq!(
            super::make_bind_source(std::path::Path::new("rel/dir"))
                .unwrap_err()
                .kind(),
            crate::error::ErrorKind::InvalidRequest
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A spec's parts, as Docker reads them: a source and a destination,
    /// a lone place, modes after either; anything else none.
    #[test]
    fn a_spec_has_docker_s_parts() {
        use super::SpecParts;
        let parts =
            |spec| SpecParts::of(spec).map(|parts| (parts.source, parts.destination, parts.modes));
        assert_eq!(
            parts("db:/data:ro,nocopy"),
            Some((Some("db"), "/data", Some("ro,nocopy")))
        );
        assert_eq!(parts("/srv:/s"), Some((Some("/srv"), "/s", None)));
        assert_eq!(parts("/data:ro"), Some((None, "/data", Some("ro"))));
        assert_eq!(parts("/data"), Some((None, "/data", None)));
        for odd in ["data", "db:rel", "db:rel:ro", ""] {
            assert_eq!(parts(odd), None, "{odd}");
        }
        assert_eq!(super::bind_source("/srv:/s:ro"), Some("/srv".into()));
        assert_eq!(super::bind_source("db:/s"), None);
    }
}
