//! The Mac's places as its VM shares them (step 13.4, D13.4-3): what a
//! bind in the guest may name. A bind's source outside them would be the
//! VM's own path, made empty there and written to unseen by the Mac
//! (F143), so it's refused as Docker Desktop refuses it ("mounts denied"),
//! by both doors (Docker's API and a forwarded command), and again at each
//! start for a source given in a share (a link in it may have changed). Off
//! the guest (a Linux host) nothing is checked.
//!
//! A source is judged by the mount it lands on once its links are
//! followed, not by its path's text: a share that failed to mount, or the
//! VM's own places hidden in one (a tmpfs over them), are then no share at
//! all. Its path's text still says which hidden place it names, in any
//! case (the Mac's file system ignores case), and a hidden place a source
//! holds must really be hidden (its tmpfs on it), else the source is refused.

use std::path::{Component, Path, PathBuf};

use shipyard_runtime::attach::forward;

use crate::error::{Error, ErrorKind, Result};

/// The places a guest's kernel command line says the Mac shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// The home, first.
    home: PathBuf,
    /// Every shared place: the home, those beyond it, and macOS's links to
    /// them ([`forward::MAC_ALIASES`]) as the guest binds them.
    places: Vec<PathBuf>,
    /// The VM's own places in them, hidden, at each of their paths.
    hidden: Vec<PathBuf>,
}

impl Shared {
    /// The places this guest shares: `None` off Shipyard's own VM.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Storage`] if the kernel's command line can't be read in
    /// the guest (its ready mark there): the check fails closed, never
    /// taken for "not the guest".
    pub fn here() -> Result<Option<Self>> {
        match std::fs::read_to_string("/proc/cmdline") {
            Ok(cmdline) => Ok(Self::of_cmdline(&cmdline)),
            Err(error) if Path::new(super::READY).exists() => Err(Error::new(
                ErrorKind::Storage,
                format!("the guest's command line can't be read ({error}): binds can't be checked"),
            )),
            Err(_) => Ok(None),
        }
    }

    /// The Mac's home, as the guest has it.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The places a kernel booted with `cmdline` shares: `None` unless
    /// Shipyard booted it (its home on the line).
    #[must_use]
    pub fn of_cmdline(cmdline: &str) -> Option<Self> {
        if !super::in_the_guest(0, cmdline, false) {
            return None;
        }
        let home = PathBuf::from(super::home_of(cmdline)?);
        let mut places = vec![home.clone()];
        places.extend(
            forward::places_of(cmdline, forward::SHARES_KEY)
                .into_iter()
                .filter(|place| super::is_plain_place(place))
                .map(PathBuf::from),
        );
        let hidden_at = forward::places_of(cmdline, forward::HIDE_KEY);
        let mut hidden: Vec<PathBuf> = hidden_at
            .iter()
            .map(PathBuf::from)
            .filter(|place| places.iter().any(|shared| place.starts_with(shared)))
            .collect();
        for (alias, place) in forward::MAC_ALIASES {
            if places.iter().any(|shared| shared == Path::new(place)) {
                places.push(PathBuf::from(alias));
                hidden.extend(
                    hidden_at
                        .iter()
                        .filter_map(|at| Path::new(at).strip_prefix(place).ok())
                        .map(|inside| Path::new(alias).join(inside)),
                );
            }
        }
        Some(Self {
            home,
            places,
            hidden,
        })
    }

    /// Refuses `source` (a bind's, absolute) unless, its links followed in
    /// the guest, it lands on a share's mount (a `virtiofs` one by a share's
    /// tag, by its mount ID) and names
    /// none of the VM's own places; a path that isn't there is judged by
    /// the nearest of its directories that is (where it would be made).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`]: mounts denied, said with what is
    /// shared; a socket (the Mac's can't be reached from the VM); or a
    /// folder macOS guards, said with the permission.
    pub fn check(&self, source: &Path) -> Result<()> {
        self.check_with(source, on_a_share).map(drop)
    }

    /// Where a start reaches the bind source `given` from (13.8c, F173a):
    /// `None` for one not given in a share (the engine's own: bound as
    /// it is); else, judged as [`Shared::check`] judges it, the share's
    /// place its real path is under and the rest of that path. The start
    /// binds it from that place by descriptor, never following a link
    /// (`Mount::bind_made`), so one swapped in after this judgement (a
    /// container writing in the share) fails the start instead of binding
    /// the VM's own files. One resolution is judged and split: never one
    /// judged and another bound.
    ///
    /// # Errors
    ///
    /// [`Shared::check`]'s.
    pub fn bind_of(&self, given: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
        self.bind_of_with(given, on_a_share)
    }

    /// [`Shared::bind_of`], with `shared` as [`Shared::check_with`]'s.
    fn bind_of_with(
        &self,
        given: &Path,
        shared: impl Fn(&Path) -> std::io::Result<bool>,
    ) -> Result<Option<(PathBuf, PathBuf)>> {
        let given = lexical(given);
        if !self.places.iter().any(|place| given.starts_with(place)) {
            return Ok(None);
        }
        let real = self.check_with(&given, shared)?;
        let place = self
            .places
            .iter()
            .filter(|place| real.starts_with(place))
            .max_by_key(|place| place.as_os_str().len())
            .ok_or_else(|| {
                self.denied(
                    &given,
                    &format!("is {}, not shared from the Mac", real.display()),
                )
            })?;
        let rest = real.strip_prefix(place).unwrap_or(&real).to_path_buf();
        Ok(Some((place.clone(), rest)))
    }

    /// [`Shared::check`] again at a start, for a source given in a share
    /// (one outside them is the engine's own, or a container's from before
    /// step 13.4: left as it was).
    ///
    /// # Errors
    ///
    /// [`Shared::check`]'s.
    pub fn check_again(&self, source: &Path) -> Result<()> {
        self.check_again_with(source, on_a_share)
    }

    /// [`Shared::check_again`], with `shared` as [`Shared::check_with`]'s.
    fn check_again_with(
        &self,
        source: &Path,
        shared: impl Fn(&Path) -> std::io::Result<bool>,
    ) -> Result<()> {
        // As the create judged it: `/../Users/me` is `/Users/me`.
        let source = lexical(source);
        if self.places.iter().any(|place| source.starts_with(place)) {
            self.check_with(&source, shared).map(drop)
        } else {
            Ok(())
        }
    }

    /// [`Shared::check`], with `shared` saying whether a path that is there
    /// is on a share's mount: the real path judged.
    fn check_with(
        &self,
        source: &Path,
        shared: impl Fn(&Path) -> std::io::Result<bool>,
    ) -> Result<PathBuf> {
        let unreadable = |error: std::io::Error| {
            let guarded = (error.kind() == std::io::ErrorKind::PermissionDenied)
                .then(|| privacy_hint(&self.home, source))
                .flatten();
            Error::new(
                ErrorKind::InvalidRequest,
                guarded.unwrap_or_else(|| format!("{}: {error}", source.display())),
            )
        };
        let (real, there) = resolved(source).map_err(unreadable)?;
        if self.names_a_hidden_place(&real) {
            return Err(self.denied(source, "is the VM's own"));
        }
        // A hidden place it holds must be hidden: a bind carries the tmpfs
        // over it, and only if it's there (its hiding at boot may have
        // failed).
        for hidden in self
            .hidden
            .iter()
            .filter(|hidden| hidden.starts_with(&real))
        {
            if std::fs::symlink_metadata(hidden).is_ok() && shared(hidden).map_err(unreadable)? {
                return Err(self.denied(
                    source,
                    &format!(
                        "holds the VM's own {}, which isn't hidden",
                        hidden.display()
                    ),
                ));
            }
        }
        if !shared(&there).map_err(unreadable)? {
            let why = if real == source {
                "isn't shared from the Mac".to_owned()
            } else {
                format!("is {}, not shared from the Mac", real.display())
            };
            return Err(self.denied(source, &why));
        }
        if std::fs::metadata(&real).is_ok_and(|metadata| {
            use std::os::unix::fs::FileTypeExt as _;
            metadata.file_type().is_socket()
        }) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                format!(
                    "{} is a socket on the Mac: the VM can't reach it",
                    source.display()
                ),
            ));
        }
        Ok(real)
    }

    /// Whether `real` names one of the VM's own places, in any case: in
    /// one, or a directory holding one by a name the hiding doesn't cover
    /// (another case of its path: the Mac's file system finds it, the
    /// guest's tmpfs over it isn't on that path). A directory holding one
    /// by its own path is let through: a bind carries the hiding with it.
    fn names_a_hidden_place(&self, real: &Path) -> bool {
        let folded = fold(real);
        self.hidden.iter().any(|hidden| {
            let hidden_folded = fold(hidden);
            folded.starts_with(&hidden_folded)
                || (hidden_folded.starts_with(&folded) && !hidden.starts_with(real))
        })
    }

    /// The refusal of `source`, `why`.
    fn denied(&self, source: &Path, why: &str) -> Error {
        let shared: Vec<String> = self
            .places
            .iter()
            .map(|place| place.display().to_string())
            .collect();
        Error::new(
            ErrorKind::InvalidRequest,
            format!(
                "mounts denied: the path {} {why} (shared: {})",
                source.display(),
                shared.join(", ")
            ),
        )
    }
}

/// `path` in lower case, for a comparison as the Mac's file system makes
/// it.
fn fold(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// `path` with `.` and `..` taken out by its text alone (`..` at the root
/// is the root), as the kernel reads it before any link.
fn lexical(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other),
        }
    }
    clean
}

/// Why `path`, unreadable, may be: a folder in `home` macOS guards, or a
/// volume under `/Volumes` (removable and network volumes are guarded too),
/// named with the permission that lets the VM read it.
#[must_use]
pub fn privacy_hint(home: &Path, path: &Path) -> Option<String> {
    let folder = forward::GUARDED_FOLDERS
        .iter()
        .map(|(folder, _)| home.join(folder))
        .chain(
            path.strip_prefix("/Volumes")
                .ok()
                .and_then(|inside| inside.components().next())
                .map(|volume| Path::new("/Volumes").join(volume)),
        )
        .find(|folder| path.starts_with(folder))?;
    Some(privacy_message(&folder.display().to_string()))
}

/// What says that macOS hasn't let the VM read `folders`, and how to.
pub(crate) fn privacy_message(folders: &str) -> String {
    format!(
        "macOS hasn't let the VM read {folders}: allow `shipyard` in System Settings → \
         Privacy & Security → Files and Folders (or Full Disk Access)"
    )
}

/// Every bind source of `specs` (`-v`'s and `Binds`' entries) checked
/// against what this guest shares ([`Shared::check`]); nothing off the
/// guest.
///
/// # Errors
///
/// [`Shared::check`]'s, for the first refused.
pub fn check_binds(specs: &[String]) -> Result<()> {
    let Some(shared) = Shared::here()? else {
        return Ok(());
    };
    specs
        .iter()
        .filter_map(|spec| crate::run::bind_source(spec))
        .try_for_each(|source| shared.check(&source))
}

/// `path` with its links followed, and the part of it that is there: both
/// `path` canonical; or, if it isn't there, the nearest of its directories
/// that is, canonical, with the rest of it (a rest that climbs can't be
/// judged: refused). Only a path that isn't there is walked up: one that
/// can't be read (a folder macOS guards) is an error.
fn resolved(path: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok((real.clone(), real)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for ancestor in path.ancestors().skip(1) {
                match std::fs::canonicalize(ancestor) {
                    Ok(there) => {
                        let rest = path.strip_prefix(ancestor).unwrap_or(path);
                        if rest.components().any(|part| part == Component::ParentDir) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "it climbs (`..`) past what isn't there",
                            ));
                        }
                        return Ok((there.join(rest), there));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// Whether `path` (there) is on one of the Mac's shares: its mount a
/// `virtiofs` one by a share's tag ([`forward::is_mac_share_tag`]).
#[cfg_attr(
    not(target_os = "linux"),
    allow(clippy::unnecessary_wraps, reason = "the guest's reads can fail")
)]
fn on_a_share(path: &Path) -> std::io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        mount_is_a_share(path, forward::is_mac_share_tag)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Whether the mount `path` is on is a share of the Mac's whose tag is one
/// `tagged` takes (by its ID, `statx`'s, in `/proc/self/mountinfo`).
#[cfg(target_os = "linux")]
fn mount_is_a_share(path: &Path, tagged: fn(&str) -> bool) -> std::io::Result<bool> {
    let id = shipyard_sys::fs::mount_id(open_path(path)?)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(is_a_macs_share(&mountinfo, id, tagged))
}

/// `path` opened `O_PATH`: its mount's ID read without reading it.
#[cfg(target_os = "linux")]
fn open_path(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, open};
    open(
        path,
        OFlag::O_PATH | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(std::io::Error::from)
}

/// Whether `path` is on a place of the Mac's as Shipyard's VM shares it: a
/// `virtiofs` mount whose tag is one `tagged` takes
/// ([`forward::is_mac_share_tag`], any; or the home's alone: only ever in
/// the guest), found by the mount `path` is on.
#[cfg(target_os = "linux")]
pub(crate) fn on_a_macs_share(path: &Path, tagged: fn(&str) -> bool) -> bool {
    mount_is_a_share(path, tagged).unwrap_or(false)
}

/// Whether the mount `id` in `mountinfo` (`/proc/PID/mountinfo`'s lines:
/// its ID first, its type and source after the ` - `) is a share of the
/// Mac's whose tag is one `tagged` takes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the guest's"))]
fn is_a_macs_share(mountinfo: &str, id: u64, tagged: fn(&str) -> bool) -> bool {
    mountinfo.lines().any(|line| {
        let mut fields = line.split(' ');
        fields.next().and_then(|first| first.parse::<u64>().ok()) == Some(id)
            && line.split_once(" - ").is_some_and(|(_, rest)| {
                let mut after = rest.split(' ');
                after.next() == Some("virtiofs") && after.next().is_some_and(tagged)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest's line: its home, the shares beyond it, the hidden places.
    fn line(home: &str, shares: &[&str], hidden: &[&str]) -> String {
        format!(
            "console=hvc0 {}{home} {}{} {}{}",
            forward::HOME_KEY,
            forward::SHARES_KEY,
            forward::places_value(shares),
            forward::HIDE_KEY,
            forward::places_value(hidden)
        )
    }

    /// The places are the home, the shares and the aliases of those
    /// shared; the hidden places are those in them, at each path.
    #[test]
    fn a_guests_shares_are_read_from_its_command_line() {
        let shared = Shared::of_cmdline(&line(
            "/Users/me",
            &["/Volumes", "/private/var/folders"],
            &["/Users/me/vm", "/private/var/folders/x/T/vm", "/opt/vm"],
        ))
        .unwrap();
        let paths = |places: &[PathBuf]| -> Vec<String> {
            places.iter().map(|p| p.display().to_string()).collect()
        };
        assert_eq!(
            paths(&shared.places),
            [
                "/Users/me",
                "/Volumes",
                "/private/var/folders",
                "/var/folders"
            ]
        );
        assert_eq!(
            paths(&shared.hidden),
            [
                "/Users/me/vm",
                "/private/var/folders/x/T/vm",
                "/var/folders/x/T/vm"
            ]
        );
        assert_eq!(Shared::of_cmdline("console=hvc0"), None, "off the guest");
    }

    /// A source on a share is let through, as is one not there yet, judged
    /// by its directory; one off the shares, a link out, a hidden place
    /// (in any case, or a directory holding one by another case), one that
    /// climbs past what isn't there, and a socket are refused, each as a
    /// bad request. A directory holding a hidden place by its own path is
    /// let through.
    #[test]
    fn a_bind_is_let_through_only_from_a_share() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("shipyard-shares-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join("project")).unwrap();
        std::fs::create_dir_all(home.join("Library/vm")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), home.join("out")).unwrap();
        let socket = home.join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let shared = Shared::of_cmdline(&line(
            home.to_str().unwrap(),
            &[],
            &[home.join("Library/vm").to_str().unwrap()],
        ))
        .unwrap();
        // A stand-in for the mount: what's under the home is on the share,
        // but the hidden place (its tmpfs over it).
        let hidden = home.join("Library/vm");
        let on_home = |path: &Path| Ok(path.starts_with(&home) && !path.starts_with(&hidden));
        for let_through in [
            home.join("project"),
            home.clone(),
            home.join("Library"),
            home.join("new/deeper"),
        ] {
            assert!(
                shared.check_with(&let_through, on_home).is_ok(),
                "{}",
                let_through.display()
            );
        }
        for refused in [
            root.join("elsewhere"),
            home.join("out"),
            home.join("out/new"),
            home.join("Library/vm"),
            home.join("Library/vm/disk"),
            home.join("library/VM/disk"),
            home.join("new/../../elsewhere"),
            socket.clone(),
        ] {
            let error = shared.check_with(&refused, on_home).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidRequest,
                "{}",
                refused.display()
            );
        }
        // A directory holding a hidden place by another case: the hiding
        // isn't on its path (the Mac's file system finds it all the same).
        assert!(shared.names_a_hidden_place(&home.join("library")));
        // In it by another case, judged by the text alone (APFS would give
        // the stored case back to a resolution on a Mac).
        assert!(shared.names_a_hidden_place(&home.join("library/VM/disk")));
        assert!(!shared.names_a_hidden_place(&home.join("Library")));
        assert!(!shared.names_a_hidden_place(&home.join("project")));
        // A hidden place whose hiding failed (on the share's mount still):
        // a bind holding it is refused, the one beside it isn't.
        let unhidden = |path: &Path| Ok(path.starts_with(&home));
        assert!(shared.check_with(&home, unhidden).is_err());
        assert!(shared.check_with(&home.join("Library"), unhidden).is_err());
        assert!(shared.check_with(&home.join("project"), unhidden).is_ok());
        // At a start: only what's given in a share is judged again, spelled
        // as the create read it.
        assert!(
            shared
                .check_again_with(
                    &Path::new("/..")
                        .join(home.strip_prefix("/").unwrap())
                        .join("out"),
                    on_home
                )
                .is_err()
        );
        assert!(
            shared
                .check_again_with(&root.join("elsewhere"), on_home)
                .is_ok()
        );
        assert!(
            shared
                .check_again_with(&home.join("project"), on_home)
                .is_ok()
        );
        assert!(shared.check_again_with(&home.join("out"), on_home).is_err());
        // A share's place that isn't on its mount (it failed to mount).
        let off = |_: &Path| Ok(false);
        assert!(shared.check_with(&home.join("project"), off).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// What a start binds from (13.8c, F173a): the share's place and the
    /// rest of the real path, its links followed once and judged; the whole
    /// share an empty rest; one given outside the shares left as it is; a
    /// link out refused, never split.
    #[test]
    fn a_bind_is_reached_from_its_share() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("shipyard-shares-from-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join("project")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), home.join("out")).unwrap();
        let shared = Shared::of_cmdline(&line(home.to_str().unwrap(), &[], &[])).unwrap();
        let on_home = |path: &Path| Ok(path.starts_with(&home));
        std::os::unix::fs::symlink(home.join("project"), home.join("linked")).unwrap();
        for (given, rest) in [
            (home.join("project"), "project"),
            (home.join("linked"), "project"),
            (home.clone(), ""),
        ] {
            assert_eq!(
                shared.bind_of_with(&given, on_home).unwrap(),
                Some((home.clone(), PathBuf::from(rest))),
                "{}",
                given.display()
            );
        }
        assert_eq!(
            shared
                .bind_of_with(&root.join("elsewhere"), on_home)
                .unwrap(),
            None
        );
        assert_eq!(
            shared
                .bind_of_with(&home.join("out"), on_home)
                .map_err(|error| error.kind()),
            Err(ErrorKind::InvalidRequest)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Only a folder macOS guards is named with the permission.
    #[test]
    fn a_guarded_folder_names_the_permission() {
        let home = Path::new("/Users/me");
        assert!(
            privacy_hint(home, Path::new("/Users/me/Desktop/app"))
                .is_some_and(|hint| hint.contains("/Users/me/Desktop: allow `shipyard`"))
        );
        assert!(privacy_hint(home, Path::new("/Users/me/Library/Mobile Documents/x")).is_some());
        assert!(
            privacy_hint(home, Path::new("/Volumes/USB/project"))
                .is_some_and(|hint| hint.contains("read /Volumes/USB: allow"))
        );
        assert_eq!(privacy_hint(home, Path::new("/Volumes")), None);
        assert_eq!(
            privacy_hint(home, Path::new("/Users/me/Projects/app")),
            None
        );
        assert_eq!(privacy_hint(home, Path::new("/Users/meDesktop")), None);
    }

    /// The Mac's shares are told by their mount's ID, type and tag: the
    /// home (`virtiofs` tagged `home`) and another (`share1`) are; another
    /// mount's ID, another virtiofs tag, and a FUSE mount of a user's
    /// aren't; the home's alone is asked for by its tag.
    #[test]
    fn the_macs_shares_are_told_by_their_mount() {
        let mountinfo = "22 1 0:20 / / rw - rootfs rootfs rw\n\
                         31 22 0:27 / /Users/me rw,nosuid,nodev - virtiofs home rw\n\
                         34 22 0:30 / /private/tmp rw,nosuid,nodev - virtiofs share1 rw\n\
                         32 22 0:28 / /mnt/x rw - virtiofs other rw\n\
                         33 22 0:29 / /home/u/s rw,nosuid,nodev shared:5 - fuse.sshfs u@h:/ rw\n";
        let any = forward::is_mac_share_tag;
        let home = |tag: &str| tag == forward::HOME_TAG;
        assert!(is_a_macs_share(mountinfo, 31, any));
        assert!(is_a_macs_share(mountinfo, 34, any));
        assert!(is_a_macs_share(mountinfo, 31, home));
        assert!(!is_a_macs_share(mountinfo, 34, home));
        for other in [22, 32, 33, 99] {
            assert!(!is_a_macs_share(mountinfo, other, any), "{other}");
        }
    }
}
