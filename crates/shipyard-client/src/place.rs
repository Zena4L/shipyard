//! Where Shipyard's API is, and which `shipyard` the app runs.
//!
//! One transport today, a UNIX socket (D14-2: a Windows host would add a
//! variant, not a rewrite). On the Mac it's `docker.sock` in the VM's
//! directory (`shipyard-vm`'s own places, never a copy of them); on Linux,
//! the user's own (rootless) API socket.

use std::path::{Path, PathBuf};

use crate::settings::Settings;
use crate::{ClientError, Result};

/// Where the API answers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A UNIX socket, by its path.
    Unix(PathBuf),
}

impl Endpoint {
    /// Its socket's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Unix(path) => path,
        }
    }
}

/// The Mac's VM directory: the settings' (the app's environment is
/// launchd's, without the user's `SHIPYARD_VM_DIR`), else the default.
///
/// # Errors
///
/// As `shipyard-vm`'s places.
#[cfg(target_os = "macos")]
pub fn vm_paths(settings: &Settings) -> Result<shipyard_vm::VmPaths> {
    let paths = match &settings.vm_dir {
        Some(dir) => {
            let default = shipyard_vm::VmPaths::from_env().map_err(|error| vm_error(&error))?;
            shipyard_vm::VmPaths::at(dir.clone(), default.guest)
                .map_err(|error| vm_error(&error))?
        }
        None => shipyard_vm::VmPaths::from_env().map_err(|error| vm_error(&error))?,
    };
    Ok(paths)
}

/// `shipyard-vm`'s error as the client's.
#[cfg(target_os = "macos")]
fn vm_error(error: &shipyard_vm::VmError) -> ClientError {
    ClientError::Place(error.to_string())
}

/// Where the API answers for `settings`: the Mac's VM's `docker.sock`;
/// Linux's `SHIPYARD_API_SOCKET`, else root's or the user's own socket.
///
/// # Errors
///
/// Without a place to look (no runtime directory, no home).
pub fn api_endpoint(settings: &Settings) -> Result<Endpoint> {
    #[cfg(target_os = "macos")]
    {
        Ok(Endpoint::Unix(vm_paths(settings)?.docker_socket()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = settings;
        linux_socket(
            std::env::var_os("SHIPYARD_API_SOCKET"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            nix::unistd::geteuid().is_root(),
        )
        .map(Endpoint::Unix)
    }
}

/// Linux's API socket: `explicit` (`SHIPYARD_API_SOCKET`), else root's
/// `/run/shipyard/api.sock`, else the user's in `runtime_dir`
/// (`$XDG_RUNTIME_DIR/shipyard/api.sock`), as `Paths::api_socket` finds it.
#[cfg_attr(
    target_os = "macos",
    allow(dead_code, reason = "Linux's; tested everywhere")
)]
fn linux_socket(
    explicit: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
    root: bool,
) -> Result<PathBuf> {
    if let Some(path) = explicit
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path);
    }
    if root {
        return Ok(PathBuf::from("/run/shipyard/api.sock"));
    }
    runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("shipyard").join("api.sock"))
        .ok_or_else(|| {
            ClientError::Place(
                "no runtime directory ($XDG_RUNTIME_DIR) to find the API's socket in".to_owned(),
            )
        })
}

/// The `shipyard` binary the app runs (D14.2-3): the settings' choice
/// (refused, never replaced, if it can't be run), else `SHIPYARD_BIN`, else
/// one beside the app's own binary, else one in the usual places, else
/// `PATH`'s (its absolute directories alone); each one [`trusted`].
///
/// # Errors
///
/// [`ClientError::Shipyard`] if none is found, or the chosen one isn't.
pub fn shipyard(settings: &Settings) -> Result<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("shipyard")));
    find_shipyard(
        settings.shipyard.clone(),
        std::env::var_os("SHIPYARD_BIN").map(PathBuf::from),
        beside,
        &USUAL_PLACES,
        std::env::var_os("PATH").as_deref(),
    )
}

/// The places [`shipyard`] looks in, after the settings, `SHIPYARD_BIN` and
/// beside the app.
const USUAL_PLACES: [&str; 2] = ["/opt/homebrew/bin/shipyard", "/usr/local/bin/shipyard"];

/// Whether the app may run `candidate`: an absolute path to a regular file
/// with an execute bit, which, with every directory above it, is the user's
/// or root's and writable by no one else (a sticky directory, `/tmp`, aside:
/// no one else can replace an entry in it). Another user who could put a
/// `shipyard` there would have it run as this one, the VM started with it.
#[must_use]
pub fn trusted(candidate: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let uid = nix::unistd::geteuid().as_raw();
    let Ok(real) = std::fs::canonicalize(candidate) else {
        return false;
    };
    let Ok(file) = std::fs::metadata(&real) else {
        return false;
    };
    let owned = |metadata: &std::fs::Metadata| metadata.uid() == uid || metadata.uid() == 0;
    if !candidate.is_absolute()
        || !file.is_file()
        || file.permissions().mode() & 0o111 == 0
        || !owned(&file)
        || file.mode() & 0o022 != 0
    {
        return false;
    }
    real.ancestors().skip(1).all(|dir| {
        std::fs::metadata(dir).is_ok_and(|metadata| {
            owned(&metadata) && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0)
        })
    })
}

/// [`shipyard`]'s rule, given what it reads: the chosen one if there's one
/// (an error if it isn't [`trusted`]), else the first of the others that is,
/// the `usual` places after them, then `PATH`'s.
fn find_shipyard(
    chosen: Option<PathBuf>,
    from_env: Option<PathBuf>,
    beside: Option<PathBuf>,
    usual: &[&str],
    path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(chosen) = chosen {
        return if trusted(&chosen) {
            Ok(chosen)
        } else {
            Err(ClientError::Shipyard(format!(
                "the shipyard chosen in Settings, {}, can't be run: it's gone, isn't a \
                 program, or someone else could change it (choose it again)",
                chosen.display()
            )))
        };
    }
    from_env
        .into_iter()
        .chain(beside)
        .chain(usual.iter().map(PathBuf::from))
        .find(|candidate| trusted(candidate))
        .or_else(|| {
            shipyard_sys::path::find_program("shipyard", path).filter(|found| trusted(found))
        })
        .ok_or_else(|| {
            ClientError::Shipyard(
                "no `shipyard` found: choose it in Settings (an app opened from the Finder \
                 doesn't see your shell's PATH)"
                    .to_owned(),
            )
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A scratch directory of the user's alone, its programs `names`.
    fn programs(dir: &Path, names: &[&str]) {
        use std::os::unix::fs::PermissionsExt as _;
        for name in names {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            let program = dir.join(name).join("shipyard");
            std::fs::write(&program, b"").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// The settings' choice first (refused, never replaced, when it can't
    /// be run), then `SHIPYARD_BIN`, then beside the app, then `PATH`; a
    /// relative `PATH` entry never searched, even one that leads to one.
    #[test]
    fn shipyard_is_found_in_its_order() {
        let dir =
            std::env::temp_dir().join(format!("shipyard-client-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        programs(&dir, &["chosen", "env", "beside", "path"]);
        let at = |name: &str| dir.join(name).join("shipyard");
        let path = std::env::join_paths([dir.join("path")]).unwrap();
        let find = |chosen: Option<PathBuf>,
                    env: Option<PathBuf>,
                    beside: Option<PathBuf>,
                    path| { find_shipyard(chosen, env, beside, &[], path).ok() };
        assert_eq!(
            find(
                Some(at("chosen")),
                Some(at("env")),
                Some(at("beside")),
                Some(&path)
            ),
            Some(at("chosen"))
        );
        assert_eq!(
            find(None, Some(at("env")), Some(at("beside")), Some(&path)),
            Some(at("env"))
        );
        assert_eq!(
            find(None, None, Some(at("beside")), Some(&path)),
            Some(at("beside"))
        );
        assert_eq!(
            find(Some(dir.join("gone/shipyard")), Some(at("env")), None, None),
            None,
            "a chosen one that's gone is refused, not replaced"
        );
        assert_eq!(find(None, None, None, Some(&path)), Some(at("path")));
        // A relative entry that does lead to it from here: never searched.
        let mut relative = PathBuf::from("../".repeat(64));
        relative.push(dir.join("path").strip_prefix("/").unwrap());
        assert!(relative.join("shipyard").is_file());
        let relative = std::env::join_paths([relative]).unwrap();
        assert_eq!(find(None, None, None, Some(&relative)), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A program someone else could replace is never run: one writable by
    /// others, or below a directory that is (not sticky); the user's own is
    /// (the control).
    #[test]
    fn a_program_others_could_change_is_never_trusted() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("shipyard-client-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        programs(&dir, &["mine"]);
        let program = dir.join("mine/shipyard");
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(&dir, 0o700);
        assert!(trusted(&program));
        mode(&program, 0o775);
        assert!(!trusted(&program), "group-writable");
        mode(&program, 0o755);
        mode(&dir.join("mine"), 0o777);
        assert!(!trusted(&program), "below a directory anyone may write");
        mode(&dir.join("mine"), 0o1777);
        assert!(trusted(&program), "a sticky one");
        mode(&dir.join("mine"), 0o755);
        mode(&program, 0o644);
        assert!(!trusted(&program), "no execute bit");
        assert!(!trusted(Path::new("mine/shipyard")), "relative");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Linux's socket: the variable's, else root's, else the user's.
    #[test]
    fn linux_s_socket_is_found_as_the_api_s() {
        assert_eq!(
            linux_socket(
                Some("/tmp/x.sock".into()),
                Some("/run/user/1000".into()),
                false
            )
            .unwrap(),
            PathBuf::from("/tmp/x.sock")
        );
        assert_eq!(
            linux_socket(None, Some("/run/user/1000".into()), true).unwrap(),
            PathBuf::from("/run/shipyard/api.sock")
        );
        assert_eq!(
            linux_socket(None, Some("/run/user/1000".into()), false).unwrap(),
            PathBuf::from("/run/user/1000/shipyard/api.sock")
        );
        assert!(linux_socket(None, None, false).is_err());
        assert_eq!(
            linux_socket(
                Some("rel.sock".into()),
                Some("/run/user/1000".into()),
                false
            )
            .unwrap(),
            PathBuf::from("/run/user/1000/shipyard/api.sock"),
            "a relative variable skipped"
        );
    }
}
