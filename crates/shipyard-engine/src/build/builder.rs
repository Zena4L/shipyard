//! A build's run (Linux): its places, its context's lock, the builder
//! container (made, started, followed, waited for, removed) and what it
//! made, read as hostile.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shipyard_image::ImageReference;
use shipyard_image::index::Index;
use shipyard_runtime::isolation::{IdMap, IdMaps};

use super::args::{buildctl_args, places};
use super::cache::{BUILDER_LABEL, builder_dir_of, remove_builders, state_name};
use super::{BUILDKIT_IMAGE, BuildRequest, Built, Dockerfile, MAX_FILE, Output};
use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::report::{Message, Progress, WarningKind};

/// The first host ID of root's builder (its user namespace's root): far
/// above what `useradd` gives users as subordinate IDs (100000 up to
/// 600100000), so no user can become it (12.3's Murphy review: 100000 is
/// the first user's own by default); checked against `/etc/subuid` and
/// `/etc/subgid` at each build.
pub(super) const ROOT_FIRST: u32 = 2_100_000_000;

/// How many IDs root's builder maps.
const ROOT_SIZE: u32 = 65_536;

/// How long a build waits for another build of its context.
const LOCK_WAIT: Duration = Duration::from_hours(1);

/// The builder's exit code when `buildkitd` never got ready.
const NOT_READY: i32 = 90;

/// The most of BuildKit's last lines kept, said with a failure.
const LAST_LINES: usize = 20;

/// The builder's script, the same every build: `buildkitd` started, waited
/// for (each look bounded, 30 s in all), `buildctl` run with the arguments
/// given (`"$@"`: argv, never read by this shell), its code kept; BuildKit's
/// own GC kept to 10 GB (`--keep-storage` is in MB); `buildkitd` asked to
/// end and waited for, so its state is written whole; the code.
const SCRIPT: &str = r#"set -u
buildkitd --oci-worker-no-process-sandbox --root /var/lib/buildkit >/out/buildkitd.log 2>&1 &
daemon=$!
tries=0
until timeout 5 buildctl debug workers >/dev/null 2>&1; do
  tries=$((tries + 1))
  if [ "$tries" -gt 300 ] || ! kill -0 "$daemon" 2>/dev/null; then
    tail -n 20 /out/buildkitd.log >&2
    exit 90
  fi
  sleep 0.1
done
buildctl "$@"
code=$?
timeout 60 buildctl prune --keep-storage 10240 >/dev/null 2>&1
kill -TERM "$daemon" 2>/dev/null
waited=0
while kill -0 "$daemon" 2>/dev/null && [ "$waited" -lt 100 ]; do
  sleep 0.1
  waited=$((waited + 1))
done
exit "$code"
"#;

/// See [`super::build`].
pub(super) fn build(
    engine: &Engine,
    request: &BuildRequest,
    names: &[ImageReference],
    tar_out: Option<&mut dyn std::io::Write>,
) -> Result<Built> {
    let root = nix::unistd::geteuid().is_root();
    let context = std::fs::canonicalize(&request.context)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(|| format!("the build context {}", request.context.display()))?;
    if !context.is_dir() {
        bail!(
            InvalidRequest,
            "the build context {} isn't a directory",
            context.display()
        );
    }
    let store = crate::image::open_store(engine)?;
    if store.root().starts_with(&context) {
        bail!(
            InvalidRequest,
            "the build context {} holds Shipyard's own store ({}): build a directory of the \
             project alone",
            context.display(),
            store.root().display()
        );
    }
    let builder = builder_dir_of(store.root(), root);
    for path in [&context, &builder] {
        bindable(path)?;
    }
    // On a Mac's home, shared into its VM, every user of the guest reads as
    // the Mac's user does (each file seen as the asker's own: probed): the
    // modes say nothing there, so it's checked as a rootless caller's is.
    readable_to_the_builder(&context, root && !on_the_macs_home(&context))?;
    let maps = root.then(root_maps).transpose()?;
    // Before Ctrl-C is held off: a first pull of BuildKit's image can be
    // stopped.
    let image = builder_image(engine)?;
    builder_dir_ready(&builder, root)?;
    let state = state_name(&context);
    // One build of a context at a time: its cache is BuildKit's alone.
    let lock = shipyard_sys::fs::open_lock_file(&builder.join(format!("{state}.lock")))
        .context("opening the build's lock")?;
    let mut said = false;
    let began = Instant::now();
    let locked = shipyard_sys::lock::lock_until_with(
        lock,
        shipyard_sys::lock::Mode::Exclusive,
        began + LOCK_WAIT,
        || {
            // Said only past a moment: `system df` holds each state's lock
            // for one, to count it.
            if !said && began.elapsed() >= NOTICE_AFTER {
                engine.report(Message::Notice(
                    "waiting for another build of this context to end".to_owned(),
                ));
                said = true;
            }
            std::thread::sleep(Duration::from_millis(250));
        },
    )
    .map_err(|errno| Error::wrap(ErrorKind::Busy, std::io::Error::from(errno)))
    .context("another build of this context kept it past an hour")?;
    sweep(engine, &builder, &state);
    super::cache::mark_used(&locked);
    unused_swept(engine, &builder, &state);
    // Ctrl-C and its kin held off from here to the end: one sent meanwhile
    // ends the follow, the builder goes, this build's directory goes (its
    // copied secrets with it), and only then does the signal act.
    let held = crate::run::HeldOff::new();
    let run = builder.join(format!(
        "{state}.run-{}",
        shipyard_sys::hex::lower(&crate::random::bytes::<6>()?)
    ));
    let job = Job {
        engine,
        request,
        names,
        context: &context,
        state: &state,
        maps: maps.as_ref(),
        image,
        held: &held,
    };
    let made =
        Places::make(&builder.join(&state), &run).and_then(|places| job.run(&places, tar_out));
    // This build's own directory goes whatever happened (its files are
    // the caller's, or root's builder's: removable here).
    if let Err(error) = std::fs::remove_dir_all(&run)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        engine.warn(
            WarningKind::LeftForLater,
            format!(
                "{}: {error} (a later build or `builder prune` removes it)",
                run.display()
            ),
        );
    }
    made
}

/// Root's builder's ID maps: [`ROOT_FIRST`] on, refused if a user's
/// subordinate range in `/etc/subuid` or `/etc/subgid` overlaps them (that
/// user could become the builder's root, and reach its cache).
fn root_maps() -> Result<IdMaps> {
    let last = ROOT_FIRST + (ROOT_SIZE - 1);
    for file in ["/etc/subuid", "/etc/subgid"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        if let Some(owner) = overlapping(&text) {
            bail!(
                Environment,
                "the builder's IDs ({ROOT_FIRST}–{last}) overlap {owner}'s subordinate IDs in \
                 {file}: a build as root would give that user its cache; move that range"
            );
        }
    }
    let map = IdMap {
        container: 0,
        host: ROOT_FIRST,
        size: ROOT_SIZE,
    };
    Ok(IdMaps {
        uids: vec![map],
        gids: vec![map],
    })
}

/// The owner of the first range in `text` (`/etc/subuid`'s lines,
/// `NAME:FIRST:COUNT`) that overlaps root's builder's IDs.
fn overlapping(text: &str) -> Option<String> {
    let last = u64::from(ROOT_FIRST) + u64::from(ROOT_SIZE - 1);
    text.lines().find_map(|line| {
        let mut fields = line.trim().split(':');
        let (Some(owner), Some(Ok(first)), Some(Ok(count))) = (
            fields.next(),
            fields.next().map(str::parse::<u64>),
            fields.next().map(str::parse::<u64>),
        ) else {
            return None;
        };
        let end = first.saturating_add(count);
        (count > 0 && first <= last && end > u64::from(ROOT_FIRST)).then(|| owner.to_owned())
    })
}

/// Makes the builder's directory if it's missing, then checks it's what it
/// should be: a real directory of the caller's that no one else can write
/// (as root, beside the store, where another user could have made one
/// first); root's enterable by the builder's ID (0711), a rootless
/// caller's its alone.
fn builder_dir_ready(builder: &Path, root: bool) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mode = if root { 0o711 } else { 0o700 };
    match std::fs::DirBuilder::new().mode(mode).create(builder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(Error::wrap(ErrorKind::Storage, error))
                .context("making the builder's directory");
        }
    }
    if let Some(why) = shipyard_sys::fs::unsafe_dir(builder)
        .map_err(|error| Error::wrap(ErrorKind::Storage, error))
        .context("looking at the builder's directory")?
    {
        bail!(
            Environment,
            "the builder's directory {} isn't to be trusted: {why} (remove it, and build again)",
            builder.display()
        );
    }
    Ok(())
}

/// Refuses a path a bind can't name (a `--volume` string: `:` splits it).
fn bindable(path: &Path) -> Result<()> {
    match path.to_str() {
        Some(text) if !text.contains(':') => Ok(()),
        _ => bail!(
            InvalidRequest,
            "{} can't be given to the builder (a path with ':' or that isn't text): move it, \
             or build from another place",
            path.display()
        ),
    }
}

/// How long a build waits on its context's lock before it says so.
const NOTICE_AFTER: Duration = Duration::from_secs(1);

/// The most entries [`readable_to_the_builder`] looks at.
const MAX_CONTEXT_ENTRIES: usize = 1_000_000;

/// Refuses a context the builder couldn't read whole: BuildKit copies a
/// file it can't read as an empty one, silently (found as root in 12.3), a
/// wrong image. As `root` the builder's root is another user (its ID
/// [`ROOT_FIRST`]): the context and everything in it must be readable to
/// every user, its parents enterable; a rootless caller's builder is the
/// caller, so what the caller can't read. Every entry is looked at,
/// `.dockerignore` or not (D12.3-3, F158), never through a symlink.
fn readable_to_the_builder(context: &Path, root: bool) -> Result<()> {
    if root {
        // The way to it, then itself (o+x up, o+rx here).
        for up in context.ancestors().skip(1) {
            if !mode_allows(up, false)? {
                bail!(InvalidRequest, "{}", unreachable_context(context, up));
            }
        }
        if !mode_allows(context, true)? {
            bail!(InvalidRequest, "{}", unreadable(context, context, root));
        }
    } else if nix::unistd::access(
        context,
        nix::unistd::AccessFlags::R_OK | nix::unistd::AccessFlags::X_OK,
    )
    .is_err()
    {
        bail!(InvalidRequest, "{}", unreadable(context, context, root));
    }
    let mut pending = vec![(context.to_owned(), 0_usize)];
    let mut seen = 0_usize;
    while let Some((dir, depth)) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
            .with_context(|| unreadable(context, &dir, root))?;
        for entry in entries {
            let entry = entry.map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))?;
            seen += 1;
            if seen > MAX_CONTEXT_ENTRIES {
                bail!(
                    InvalidRequest,
                    "the build context holds more than {MAX_CONTEXT_ENTRIES} entries"
                );
            }
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))?;
            if !kind.is_file() && !kind.is_dir() {
                continue;
            }
            let readable = if root {
                mode_allows(&path, true)?
            } else {
                let mut wanted = nix::unistd::AccessFlags::R_OK;
                if kind.is_dir() {
                    wanted |= nix::unistd::AccessFlags::X_OK;
                }
                nix::unistd::access(&path, wanted).is_ok()
            };
            if !readable {
                bail!(InvalidRequest, "{}", unreadable(context, &path, root));
            }
            if kind.is_dir() {
                if depth >= 256 {
                    bail!(
                        InvalidRequest,
                        "the build context is more than 256 directories deep"
                    );
                }
                pending.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

/// Whether `path` is on the Mac's home as Shipyard's VM shares it: the
/// `virtiofs` mount tagged [`HOME_TAG`](shipyard_runtime::attach::forward::HOME_TAG)
/// (only ever in the guest), found by the mount `path` is on.
fn on_the_macs_home(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(id) = shipyard_sys::fs::mount_id(&file) else {
        return false;
    };
    std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|mountinfo| is_the_home_share(&mountinfo, id))
}

/// Whether the mount `id` in `mountinfo` (`/proc/PID/mountinfo`'s lines:
/// its ID first, its type and source after the ` - `) is the Mac's shared
/// home.
fn is_the_home_share(mountinfo: &str, id: u64) -> bool {
    mountinfo.lines().any(|line| {
        let mut fields = line.split(' ');
        fields.next().and_then(|first| first.parse::<u64>().ok()) == Some(id)
            && line.split_once(" - ").is_some_and(|(_, rest)| {
                let mut after = rest.split(' ');
                after.next() == Some("virtiofs")
                    && after.next() == Some(shipyard_runtime::attach::forward::HOME_TAG)
            })
    })
}

/// Whether every user may read `path` (`read`: a file `o+r`, a directory
/// `o+rx`) or only enter it (a directory `o+x`), by its own mode (never
/// through a symlink).
fn mode_allows(path: &Path, read: bool) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(|| format!("looking at {}", path.display()))?;
    let mode = metadata.permissions().mode();
    let enter = !metadata.is_dir() || mode & 0o001 != 0;
    Ok(if read {
        mode & 0o004 != 0 && enter
    } else {
        enter
    })
}

/// Why `path` in `context` can't go to the builder.
fn unreadable(context: &Path, path: &Path, root: bool) -> String {
    let shown = path
        .strip_prefix(context)
        .ok()
        .filter(|inside| !inside.as_os_str().is_empty())
        .map_or_else(
            || "the build context itself".to_owned(),
            |inside| inside.display().to_string(),
        );
    if root {
        format!(
            "{shown} isn't readable to the builder (as root, it runs as another user, ID \
             {ROOT_FIRST}: BuildKit would copy it empty): make it readable to every user (chmod \
             o+r, a directory o+rx), or move it out of the context"
        )
    } else {
        format!(
            "{shown} isn't readable to you, so not to the builder (BuildKit would copy it \
             empty): make it readable, or move it out of the context"
        )
    }
}

/// Why a root build's context at `context` can't be reached through `up`.
fn unreachable_context(context: &Path, up: &Path) -> String {
    format!(
        "the build context {} is under {}, which the builder (as root, another user, ID \
         {ROOT_FIRST}) can't enter: build from a place every user can reach (chmod o+x on the \
         way)",
        context.display(),
        up.display()
    )
}

/// What a killed build of `state` left: its builder (still running, or
/// never started), removed; its own directories. One that can't be removed
/// is said, and the build goes on (its name is another).
fn sweep(engine: &Engine, builder: &Path, state: &str) {
    for left in remove_builders(engine, state) {
        engine.warn(WarningKind::LeftForLater, left);
    }
    let prefix = format!("{state}.run-");
    for entry in std::fs::read_dir(builder).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Other contexts' caches no build has used for a week removed
/// ([`cache::sweep_unused`](super::cache::sweep_unused)), said; one that
/// can't be is left for a later build or `builder prune`, said.
fn unused_swept(engine: &Engine, builder: &Path, state: &str) {
    match super::cache::sweep_unused(engine, builder, state, std::time::SystemTime::now()) {
        Ok(swept) if swept.removed.is_empty() => {}
        Ok(swept) => engine.report(Message::Notice(format!(
            "removed the build cache of {} other build context{} unused for a week",
            swept.removed.len(),
            if swept.removed.len() == 1 { "" } else { "s" }
        ))),
        Err(error) => engine.warn(
            WarningKind::LeftForLater,
            format!("the build caches unused for a week: {error:#} (`builder prune` removes them)"),
        ),
    }
}

/// A build's places on the host.
struct Places {
    state: PathBuf,
    run: PathBuf,
    input: PathBuf,
    secrets: PathBuf,
    out: PathBuf,
    layout: PathBuf,
}

impl Places {
    /// Makes them, the caller's for now (the state kept from a build
    /// before: a real directory, never a symlink); [`Job::run`] gives them
    /// to the builder's root once the inputs are written.
    fn make(state: &Path, run: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let places = Self {
            state: state.to_owned(),
            run: run.to_owned(),
            input: run.join("in"),
            secrets: run.join("secrets"),
            out: run.join("out"),
            layout: run.join("layout"),
        };
        let mut dirs = std::fs::DirBuilder::new();
        dirs.mode(0o700);
        match dirs.create(&places.state) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let kept = std::fs::symlink_metadata(&places.state)
                    .with_context(|| format!("looking at {}", places.state.display()))?;
                if !kept.is_dir() {
                    bail!(
                        Environment,
                        "{} isn't a directory (or it's a symbolic link): remove it",
                        places.state.display()
                    );
                }
            }
            Err(error) => {
                return Err(Error::wrap(ErrorKind::Storage, error))
                    .with_context(|| format!("making {}", places.state.display()));
            }
        }
        for dir in [
            &places.run,
            &places.input,
            &places.secrets,
            &places.out,
            &places.layout,
        ] {
            dirs.create(dir)
                .with_context(|| format!("making {}", dir.display()))?;
        }
        Ok(places)
    }

    /// Gives them, and `written`, to the builder's root (root's builder:
    /// its mapped ID; a rootless caller is its builder's root already),
    /// never through a symlink.
    fn give(&self, written: &[PathBuf], maps: Option<&IdMaps>) -> Result<()> {
        let Some(maps) = maps else {
            return Ok(());
        };
        let (Some(uid), Some(gid)) = (maps.host_uid(0), maps.host_gid(0)) else {
            bail!(Runtime, "the builder's maps give its root no ID");
        };
        let places = [
            &self.state,
            &self.run,
            &self.input,
            &self.secrets,
            &self.out,
            &self.layout,
        ];
        for path in places.into_iter().chain(written) {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))
                .with_context(|| format!("giving {} to the builder", path.display()))?;
        }
        Ok(())
    }
}

/// Copies what the builder reads besides the context into `in/`: the
/// Dockerfile (and its own `.dockerignore`), the secrets; read bounded (a
/// regular file, never a FIFO waited on, at most [`MAX_FILE`]), written 0400.
/// The Dockerfile and its `.dockerignore` come from a repository, maybe
/// anyone's: a link is followed only while it stays inside the context or
/// `-f`'s own directory ([`read_within`]); a secret is the caller's own
/// file, named on the command line (a link followed, as the caller would).
/// What was written.
fn copy_inputs(request: &BuildRequest, context: &Path, places: &Places) -> Result<Vec<PathBuf>> {
    let read = |path: &Path, what: &str| {
        std::fs::canonicalize(path)
            .and_then(|real| shipyard_sys::fs::read_bounded(&real, MAX_FILE))
            .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
            .with_context(|| format!("reading {what} {}", path.display()))
    };
    // Where the Dockerfile and its `.dockerignore` may lead.
    let within: Vec<&Path> = match &request.dockerfile {
        Dockerfile::File(path) => vec![context, path.parent().unwrap_or(Path::new("/"))],
        _ => vec![context],
    };
    let (dockerfile, beside) = match &request.dockerfile {
        Dockerfile::Default => {
            let path = context.join(places::DOCKERFILE);
            (read_within(&path, &within, "the Dockerfile")?, Some(path))
        }
        Dockerfile::File(path) => (
            read_within(path, &within, "the Dockerfile")?,
            Some(path.clone()),
        ),
        Dockerfile::Text(text) => {
            if text.len() as u64 > MAX_FILE {
                bail!(InvalidRequest, "a Dockerfile past {MAX_FILE} bytes");
            }
            (text.clone(), None)
        }
    };
    let mut written = vec![write_input(
        &places.input.join(places::DOCKERFILE),
        &dockerfile,
    )?];
    // Its own `.dockerignore`, as BuildKit reads it (`Dockerfile.dockerignore`
    // beside it), if it has one.
    if let Some(path) = beside {
        let mut ignore = path.into_os_string();
        ignore.push(".dockerignore");
        let ignore = PathBuf::from(ignore);
        if std::fs::metadata(&ignore).is_ok() {
            let bytes = read_within(&ignore, &within, "the Dockerfile's .dockerignore")?;
            let mut name = OsString::from(places::DOCKERFILE);
            name.push(".dockerignore");
            written.push(write_input(&places.input.join(name), &bytes)?);
        }
    }
    for secret in &request.secrets {
        let bytes = read(&secret.src, "the secret")?;
        written.push(write_input(&places.secrets.join(&secret.id), &bytes)?);
    }
    Ok(written)
}

/// Reads `path` (`what`, for errors) bounded, if it is, or leads through its
/// links to, a file inside one of the directories `within`: a hostile
/// repository's `Dockerfile` linked to `~/.ssh/id_ed25519`, or as root to
/// `/etc/shadow`, is refused, never read (BuildKit would be handed it: a
/// `# syntax=` frontend reads it, a parse error prints it).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`]: it leads outside them, or can't be read.
fn read_within(path: &Path, within: &[&Path], what: &str) -> Result<Vec<u8>> {
    let reading = || format!("reading {what} {}", path.display());
    let real = std::fs::canonicalize(path)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(reading)?;
    let inside = within
        .iter()
        .any(|dir| std::fs::canonicalize(dir).is_ok_and(|dir| real.starts_with(dir)));
    if !inside {
        bail!(
            InvalidRequest,
            "{what} {} is a link that leads outside the build context: it's refused (a \
             repository's link could hand BuildKit any file of yours)",
            path.display()
        );
    }
    shipyard_sys::fs::read_bounded(&real, MAX_FILE)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(reading)
}

/// Writes `bytes` to a new file at `path`, read-only.
fn write_input(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_owned())
}

/// One build, what travels with it.
struct Job<'a> {
    engine: &'a Engine,
    request: &'a BuildRequest,
    names: &'a [ImageReference],
    context: &'a Path,
    state: &'a str,
    maps: Option<&'a IdMaps>,
    image: String,
    held: &'a crate::run::HeldOff,
}

impl Job<'_> {
    /// Its inputs copied in, its places given to the builder, the builder
    /// run, its output read.
    fn run(&self, places: &Places, tar_out: Option<&mut dyn std::io::Write>) -> Result<Built> {
        let mut written = copy_inputs(self.request, self.context, places)?;
        let (laid, layout) = self.local_images(places)?;
        written.extend(layout);
        places.give(&written, self.maps)?;
        self.builder(places, &laid)?;
        outputs(self.engine, self.request, self.names, places, tar_out)
    }

    /// The store's images the Dockerfile could name, laid out in the
    /// build's `layout/` ([`super::local`]): none if `--pull` asks for each
    /// base image again (Docker's rule). The images, and the layout's own
    /// files (the store's blobs, linked, stay the store's).
    fn local_images(&self, places: &Places) -> Result<(Vec<super::local::Local>, Vec<PathBuf>)> {
        if self.request.pull {
            return Ok((Vec::new(), Vec::new()));
        }
        let dockerfile =
            shipyard_sys::fs::read_bounded(&places.input.join(places::DOCKERFILE), MAX_FILE)
                .map_err(|error| Error::wrap(ErrorKind::Storage, error))
                .context("reading the build's Dockerfile again")?;
        let store = crate::image::open_store(self.engine)?;
        // Under the store's lock: no collection takes a blob before it's
        // linked (once linked, it's the build's too).
        let _shared = store.lock(shipyard_image::LockMode::Shared, shipyard_image::USE_WAIT)?;
        let images = match Index::open(store.root()) {
            Ok(index) => index.images(),
            Err(error) => {
                self.engine.warn(
                    WarningKind::LeftForLater,
                    format!(
                        "the store's images aren't given to the build ({error}): a base image \
                         is pulled"
                    ),
                );
                Vec::new()
            }
        };
        let wanted = super::local::wanted(&images, &dockerfile, &self.request.build_args);
        if wanted.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let blobs = store.root().join("blobs").join("sha256");
        let laid = super::local::lay_out(&wanted, &blobs, &places.layout)?;
        if !laid.left_out.is_empty() {
            self.engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "images of the store not given to the build (a base image of theirs is \
                     pulled): {}",
                    laid.left_out.join("; ")
                ),
            );
        }
        Ok((laid.images, laid.written))
    }

    /// The builder's request.
    fn request(
        &self,
        places: &Places,
        laid: &[super::local::Local],
    ) -> Result<crate::run::RunRequest> {
        let mut run = crate::run::RunRequest {
            image: Some(self.image.clone()),
            name: Some(format!(
                "shipyard-build-{}",
                shipyard_sys::hex::lower(&crate::random::bytes::<6>()?)
            )),
            entrypoint: Some(OsString::from("/bin/sh")),
            cap_add: vec![crate::run::CapabilityArg::One(
                "SYS_ADMIN"
                    .parse()
                    .map_err(|_| Error::new(ErrorKind::Runtime, "CAP_SYS_ADMIN"))?,
            )],
            security_opt: vec![crate::run::SecurityOpt::SeccompUnconfined],
            labels: BTreeMap::from([(BUILDER_LABEL.to_owned(), self.state.to_owned())]),
            ..crate::run::RunRequest::default()
        };
        if let Some(maps) = self.maps {
            run.uidmap.clone_from(&maps.uids);
            run.gidmap.clone_from(&maps.gids);
        }
        let bind = |from: &Path, to: &str, mode: &str| format!("{}:{to}{mode}", from.display());
        run.volume = vec![
            bind(self.context, places::CONTEXT, ":ro"),
            bind(&places.input, places::IN, ":ro"),
            bind(&places.secrets, places::SECRETS, ":ro"),
            bind(&places.out, places::OUT, ""),
            bind(&places.state, places::STATE, ""),
            bind(&places.layout, places::LAYOUT, ":ro"),
        ];
        let mut command = vec![
            OsString::from("-c"),
            OsString::from(SCRIPT),
            OsString::from("sh"),
        ];
        command.extend(
            buildctl_args(self.request, self.names)
                .into_iter()
                .chain(super::local::buildctl_args(laid, places::LAYOUT))
                .map(OsString::from),
        );
        run.command = command;
        Ok(run)
    }

    /// The builder made, started, followed (its progress to the reporter,
    /// its last lines kept), waited for and removed.
    fn builder(&self, places: &Places, laid: &[super::local::Local]) -> Result<()> {
        let engine = self.engine;
        let record = crate::run::create(engine, self.request(places, laid)?, false)?;
        let id = record.id.to_string();
        let _builder = Removed { engine, id: &id };
        crate::run::start(engine, &id)?;
        let mut last: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let options = crate::logs::LogsOptions {
            follow: true,
            gone: Some(crate::logs::Gone::new(crate::run::stop_pending)),
            ..crate::logs::LogsOptions::default()
        };
        crate::logs::of_container(engine, &id, &options, |entry| {
            let line = String::from_utf8_lossy(&entry.text).into_owned();
            engine.report(Message::Progress(Progress::Build { line: line.clone() }));
            if last.len() == LAST_LINES {
                last.pop_front();
            }
            last.push_back(line);
            Ok(())
        })?;
        if self.held.asked_to_stop() {
            bail!(
                Interrupted,
                "the build was interrupted: its builder was removed"
            );
        }
        let code = match crate::run::wait(engine, &id) {
            Ok(code) => code,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                bail!(Build, "the builder was removed during the build")
            }
            Err(error) => return Err(error),
        };
        let said = last.into_iter().collect::<Vec<_>>().join("\n");
        match code {
            0 => Ok(()),
            NOT_READY => bail!(Environment, "BuildKit didn't start in the builder:\n{said}"),
            _ => bail!(Build, "the build failed:\n{said}"),
        }
    }
}

/// The builder's image, by its digest (never a name another `tag` could
/// move meanwhile): the pinned one found in the store by its manifest's or
/// index's digest, whatever it's named there (a `load` of it serves a host
/// offline); else pulled now (before Ctrl-C is held off), then found.
fn builder_image(engine: &Engine) -> Result<String> {
    if let Some(found) = stored_builder_image(engine)? {
        return Ok(found);
    }
    crate::image::pull(engine, BUILDKIT_IMAGE)?;
    stored_builder_image(engine)?.ok_or_else(|| {
        Error::new(
            ErrorKind::Image,
            "BuildKit's image isn't in the store after pulling it",
        )
    })
}

/// [`builder_image`]'s look in the store: `repository@<manifest digest>`.
fn stored_builder_image(engine: &Engine) -> Result<Option<String>> {
    let pinned = BUILDKIT_IMAGE
        .rsplit_once('@')
        .map_or(BUILDKIT_IMAGE, |(_, digest)| digest);
    let store = crate::image::open_store(engine)?;
    let Ok(index) = Index::open(store.root()) else {
        return Ok(None);
    };
    Ok(index.images().into_iter().find_map(|image| {
        let matches =
            image.record.manifest == pinned || image.record.index_digest.as_deref() == Some(pinned);
        let named = ImageReference::parse(&image.reference).ok()?;
        let digest = shipyard_image::Digest::parse(&image.record.manifest).ok()?;
        matches.then(|| {
            ImageReference {
                tag: None,
                digest: Some(digest),
                ..named
            }
            .to_string()
        })
    }))
}

/// Removes the builder (forced) when the build is done with it, whatever
/// happened: a failed or interrupted build leaves none. One already gone
/// (a user's `rm -f`) is nothing to say.
struct Removed<'a> {
    engine: &'a Engine,
    id: &'a str,
}

impl Drop for Removed<'_> {
    fn drop(&mut self) {
        match crate::run::remove(self.engine, self.id, true) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => self.engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "the builder {}: {error:#} (the next build of this context removes it)",
                    self.id
                ),
            ),
        }
    }
}

/// The build's output file `name` in `out`: opened by that name in that
/// directory alone, never through a symlink, never a FIFO waited on, and
/// only if it's a regular file (the builder wrote it: whatever it planted
/// there is never what the caller reads instead).
fn open_output(out: &std::os::fd::OwnedFd, name: &str) -> Result<std::fs::File> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl, openat};
    let refused = |why: &str| Error::new(ErrorKind::Build, format!("the build's {name}: {why}"));
    let fd = openat(
        out,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|errno| refused(&errno.desc().to_lowercase()))?;
    let kind = nix::sys::stat::fstat(&fd).map_err(|errno| refused(errno.desc()))?;
    if kind.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(refused("not a regular file"));
    }
    fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_RDONLY)).map_err(|errno| refused(errno.desc()))?;
    Ok(std::fs::File::from(fd))
}

/// What the build made, read as hostile: its image loaded under the
/// build's names alone; its files unpacked as the caller's, or written to
/// `tar_out`.
fn outputs(
    engine: &Engine,
    request: &BuildRequest,
    names: &[ImageReference],
    places: &Places,
    tar_out: Option<&mut dyn std::io::Write>,
) -> Result<Built> {
    let out = shipyard_image::fsat::open_dir(&places.out).context("opening the build's output")?;
    let open = |name: &str| open_output(&out, name);
    match &request.output {
        None => {
            let loaded = crate::image::load_named(
                engine,
                std::io::BufReader::new(open("image.tar")?),
                names,
            )?;
            Ok(Built {
                id: loaded.ids.first().cloned(),
                names: loaded.loaded,
                moved: loaded.moved,
            })
        }
        Some(Output::Tar) => {
            let Some(writer) = tar_out else {
                bail!(InvalidRequest, "-o type=tar has nowhere to write");
            };
            std::io::copy(&mut open("files.tar")?, writer).context("writing the build's files")?;
            Ok(Built::default())
        }
        Some(Output::Local(dest)) => {
            let dir = output_dir(dest, nix::unistd::geteuid().is_root())?;
            let options = shipyard_image::CopyOptions {
                ownership: shipyard_image::Ownership::Keep,
                owner: None,
                limits: shipyard_image::Limits::default(),
                top: None,
            };
            shipyard_image::unpack_tar(std::io::BufReader::new(open("files.tar")?), &dir, &options)
                .map_err(|error| Error::new(ErrorKind::Build, error.to_string()))
                .with_context(|| format!("writing the build's files to {}", dest.display()))?;
            Ok(Built::default())
        }
    }
}

/// `-o type=local`'s directory `dest`, made if it's missing. As `root`,
/// walked and made from `/` (or the working directory) without following a
/// symlink: a `/tmp/out` another user linked to `/etc` would have the
/// build's files written there as root (12.5's review); a link on the way
/// is refused, said. A rootless caller's, and one on a Mac's home (every
/// file there the Mac's user's), are followed as the caller would.
fn output_dir(dest: &Path, root: bool) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsFd as _;
    let making = || format!("making {}", dest.display());
    let on_the_home = || {
        dest.ancestors()
            .find(|up| !up.as_os_str().is_empty() && up.exists())
            .is_some_and(on_the_macs_home)
    };
    if !root || on_the_home() {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dest)
            .with_context(making)?;
        return shipyard_image::fsat::open_dir(dest)
            .with_context(|| format!("opening {}", dest.display()));
    }
    let (start, below) = match dest.strip_prefix("/") {
        Ok(below) => (Path::new("/"), below),
        Err(_) => (Path::new("."), dest),
    };
    // `./out`, `.`: a leading `.` is the start itself (the walk takes plain
    // names alone).
    let below: PathBuf = below
        .components()
        .filter(|part| !matches!(part, std::path::Component::CurDir))
        .collect();
    let start = shipyard_image::fsat::open_dir(start).with_context(making)?;
    let made =
        shipyard_sys::fs::make_below(start.as_fd(), &below, 0o755).map_err(|error| match error
            .raw_os_error()
            .map(nix::errno::Errno::from_raw)
        {
            Some(nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR) => Error::new(
                ErrorKind::InvalidRequest,
                format!(
                    "{}: as root, -o's directory is reached without following a symbolic link \
                     (another user's could lead the build's files anywhere), and one is on its \
                     way: give its real path",
                    dest.display()
                ),
            ),
            None if error.kind() == std::io::ErrorKind::InvalidInput => Error::new(
                ErrorKind::InvalidRequest,
                format!(
                    "{}: as root, -o's directory is a plain path (no `..`)",
                    dest.display()
                ),
            ),
            _ => Error::wrap(ErrorKind::Storage, error).context(making()),
        })?;
    // Made for reference only (`O_PATH`): opened again for the unpacker.
    nix::fcntl::openat(
        &made,
        ".",
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|errno| Error::wrap(ErrorKind::Storage, std::io::Error::from(errno)))
    .with_context(|| format!("opening {}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Mac's shared home is told by its mount's ID, type and tag: the
    /// share (`virtiofs` tagged `home`) is; another mount's ID, another
    /// virtiofs tag, and a FUSE mount of a user's aren't.
    #[test]
    fn the_macs_home_is_told_by_its_mount() {
        let mountinfo = "22 1 0:20 / / rw - rootfs rootfs rw\n\
                         31 22 0:27 / /Users/me rw,nosuid,nodev - virtiofs home rw\n\
                         32 22 0:28 / /mnt/x rw - virtiofs other rw\n\
                         33 22 0:29 / /home/u/s rw,nosuid,nodev shared:5 - fuse.sshfs u@h:/ rw\n";
        assert!(is_the_home_share(mountinfo, 31));
        for other in [22, 32, 33, 99] {
            assert!(!is_the_home_share(mountinfo, other), "{other}");
        }
    }

    /// A repository's Dockerfile is read only from inside the context, or
    /// `-f`'s own directory: a link inside (`Dockerfile ->
    /// docker/Dockerfile.prod`) is read, the controls; one leading out (to
    /// a file of the caller's) is refused as a bad request, never read.
    #[test]
    fn a_dockerfile_link_leading_out_is_refused() {
        let scratch = std::env::temp_dir().join(format!("shipyard-within-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let context = scratch.join("repo");
        std::fs::create_dir_all(context.join("docker")).unwrap();
        std::fs::write(context.join("docker/Dockerfile.prod"), "FROM a\n").unwrap();
        std::os::unix::fs::symlink("docker/Dockerfile.prod", context.join("Dockerfile")).unwrap();
        let private = scratch.join("id_ed25519");
        std::fs::write(&private, "a private key\n").unwrap();
        std::os::unix::fs::symlink(&private, context.join("docker/Dockerfile.leak")).unwrap();
        std::os::unix::fs::symlink("../../id_ed25519", context.join("Dockerfile.dockerignore"))
            .unwrap();
        assert_eq!(
            read_within(&context.join("Dockerfile"), &[&context], "the Dockerfile").unwrap(),
            b"FROM a\n"
        );
        for leak in ["docker/Dockerfile.leak", "Dockerfile.dockerignore"] {
            let refused = read_within(&context.join(leak), &[&context], "the Dockerfile");
            assert_eq!(
                refused.as_ref().map_err(Error::kind).err(),
                Some(ErrorKind::InvalidRequest),
                "{leak}: {refused:?}"
            );
        }
        // `-f` names a file outside the context: its own directory is allowed.
        let elsewhere = scratch.join("dockerfiles");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("app"), "FROM b\n").unwrap();
        assert_eq!(
            read_within(
                &elsewhere.join("app"),
                &[&context, &elsewhere],
                "the Dockerfile"
            )
            .unwrap(),
            b"FROM b\n"
        );
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// Root's builder's IDs are refused where a user's subordinate range
    /// overlaps them (that user could become the builder's root), at
    /// either end; the controls: Ubuntu's default first user, a range just
    /// below, a range just past, nonsense lines.
    #[test]
    fn a_subordinate_range_overlapping_the_builder_s_is_found() {
        let last = u64::from(ROOT_FIRST) + u64::from(ROOT_SIZE) - 1;
        assert_eq!(overlapping("alice:100000:65536\n"), None);
        assert_eq!(
            overlapping(&format!("bob:{}:1\n", ROOT_FIRST - 1)),
            None,
            "just below"
        );
        assert_eq!(
            overlapping(&format!("carol:{}:10\n", last + 1)),
            None,
            "just past"
        );
        assert_eq!(overlapping("garbage\n:x:y\n"), None);
        assert_eq!(
            overlapping(&format!("alice:100000:65536\neve:{}:2\n", ROOT_FIRST - 1)).as_deref(),
            Some("eve"),
            "its first ID"
        );
        assert_eq!(
            overlapping(&format!("mallory:{last}:1\n")).as_deref(),
            Some("mallory"),
            "its last ID"
        );
        assert_eq!(
            overlapping("wide:0:4294967295\n").as_deref(),
            Some("wide"),
            "a range over all of it"
        );
    }

    /// The build's output is read only as a regular file of that name: a
    /// symlink the builder planted (to a file of the caller's) or a FIFO
    /// (never written: a wait for ever) refused at once; the control, a
    /// file.
    #[test]
    #[ignore = "needs root"]
    fn a_root_build_s_files_never_go_through_a_link() {
        let dir = std::env::temp_dir().join(format!("shipyard-output-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // A real path is made and opened (the positive control), as is one
        // a rootless caller gives through a link.
        let real = dir.join("real/out");
        output_dir(&real, true).unwrap();
        assert!(real.is_dir());
        let linked = dir.join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        output_dir(&linked.join("rootless"), false).unwrap();
        assert!(elsewhere.join("rootless").is_dir());
        // As root, a link at its end or on its way is refused, and nothing
        // is made where it leads.
        for dest in [linked.clone(), linked.join("below")] {
            let error = output_dir(&dest, true).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{error:#}");
        }
        assert!(!elsewhere.join("below").exists());
        // A path from the working directory (the crate's, in a test): `.`
        // and `./src` are made and opened, as root too.
        output_dir(Path::new("."), true).unwrap();
        output_dir(Path::new("./src"), true).unwrap();
        let error = output_dir(&dir.join("real/../real"), true).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{error:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_build_s_output_is_only_a_file() {
        let dir = std::env::temp_dir().join(format!("shipyard-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), b"tar").unwrap();
        std::os::unix::fs::symlink(dir.join("file"), dir.join("link")).unwrap();
        nix::unistd::mkfifo(&dir.join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let out = std::os::fd::OwnedFd::from(std::fs::File::open(&dir).unwrap());
        let mut read = String::new();
        std::io::Read::read_to_string(&mut open_output(&out, "file").unwrap(), &mut read).unwrap();
        assert_eq!(read, "tar");
        for planted in ["link", "fifo"] {
            assert_eq!(
                open_output(&out, planted)
                    .map(drop)
                    .map_err(|error| error.kind()),
                Err(ErrorKind::Build),
                "{planted}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
