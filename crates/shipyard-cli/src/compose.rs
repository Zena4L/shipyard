//! `shipyard compose` (step 11.4a1, D27): Docker's own `docker compose`,
//! run against Shipyard's API. Nothing of compose is Shipyard's: this
//! starts the API's service if nothing serves its socket yet (D11.4-2: it
//! exits once idle, daemonless; on a Mac, the VM, whose socket is
//! `docker.sock`: step 11.5b2a), then becomes `docker compose` (`execve`),
//! its signals, terminal and exit code compose's own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result};

/// `shipyard compose`'s arguments: compose's own, passed on as they are.
#[derive(Debug, clap::Args)]
pub(crate) struct ComposeArgs {
    /// `docker compose`'s arguments: its command and flags (`up -d`, `ps`,
    /// `down -v`…), as Docker's compose takes them.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMPOSE ARGS"
    )]
    pub(crate) args: Vec<OsString>,
}

/// Runs `docker compose ARGS` against Shipyard's socket, served first
/// ([`served_socket`]). Returns only if it couldn't.
pub(crate) fn compose(args: &ComposeArgs) -> Result<ExitCode> {
    let docker = on_path("docker", std::env::var_os("PATH").as_deref()).context(
        "docker compose needs Docker's CLI and its compose plugin: install them (no Docker \
         daemon is needed)",
    )?;
    let socket = served_socket()?;
    Err(run(&docker, &socket, &args.args)).context("running docker compose")
}

/// On a Mac, the VM's Docker socket (step 11.5b2a), the VM started first
/// as any command starts it (its guest ready), in a directory that's the
/// user's alone (the VM's own check).
#[cfg(target_os = "macos")]
fn served_socket() -> Result<PathBuf> {
    let paths = shipyard_vm::VmPaths::from_env()?;
    shipyard_vm::client::start(&paths)?;
    Ok(paths.docker_socket())
}

/// On a Linux host, the API's socket, its service started if nothing
/// serves it (a socket systemd serves answers already).
#[cfg(not(target_os = "macos"))]
fn served_socket() -> Result<PathBuf> {
    let engine = crate::reporter::engine();
    let socket = engine.paths().api_socket()?;
    // Compose's project, its variables and a registry's credentials go to
    // whatever answers there: never through a directory another user can
    // change (one not there yet, the service makes as its own).
    if let Some(dir) = socket.parent()
        && let Ok(Some(why)) = shipyard_sys::fs::unsafe_dir(dir)
    {
        anyhow::bail!(
            "the API's socket's directory {} isn't yours alone: {why}",
            dir.display()
        );
    }
    // (Left to run on: after the `exec`, it's compose's child, and ends
    // once idle.)
    let _service = shipyard_engine::api_service::ensure(&socket, &|| service(&socket))?;
    Ok(socket)
}

/// Becomes `docker compose ARGS` against `socket`: returns only its
/// failure to start.
fn run(docker: &Path, socket: &Path, args: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    let (program, argv, host) = invocation(docker, socket, args);
    std::process::Command::new(program)
        .args(argv)
        .env("DOCKER_HOST", host)
        // Shipyard's socket is a plain UNIX one: TLS settings meant for
        // another daemon would make compose speak TLS to it.
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_TLS")
        .env_remove("DOCKER_CERT_PATH")
        .exec()
}

/// What runs: `docker`, `compose` then `args`, with `DOCKER_HOST` at
/// `socket`.
fn invocation<'a>(
    docker: &'a Path,
    socket: &Path,
    args: &'a [OsString],
) -> (&'a Path, Vec<&'a std::ffi::OsStr>, OsString) {
    let mut argv = vec![std::ffi::OsStr::new("compose")];
    argv.extend(args.iter().map(OsString::as_os_str));
    let mut host = OsString::from("unix://");
    host.push(socket);
    (docker, argv, host)
}

/// `shipyard system service --socket SOCKET`, this binary.
#[cfg(not(target_os = "macos"))]
fn service(socket: &Path) -> std::process::Command {
    // (`/proc/self/exe` on Linux: this very binary, wherever it's been
    // moved since.)
    let mut command = std::process::Command::new("/proc/self/exe");
    command.args(["system", "service", "--socket"]).arg(socket);
    command
}

/// The first `name` on `path` (`PATH`'s directories, the absolute ones
/// only: never one found in the working directory) that's an executable
/// file.
fn on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docker compose` and the arguments as given, `DOCKER_HOST` the
    /// socket's.
    #[test]
    fn compose_runs_against_the_socket() {
        let args = [OsString::from("up"), OsString::from("-d")];
        let (program, argv, host) = invocation(
            Path::new("/usr/bin/docker"),
            Path::new("/run/shipyard/api.sock"),
            &args,
        );
        assert_eq!(program, Path::new("/usr/bin/docker"));
        assert_eq!(argv, ["compose", "up", "-d"]);
        assert_eq!(host, "unix:///run/shipyard/api.sock");
    }

    /// `docker` found on `PATH` as an executable file only, and in an
    /// absolute directory only: a relative entry that holds one (written
    /// from the working directory up, as the repository may be read-only)
    /// is skipped.
    #[test]
    fn docker_is_found_on_the_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("compose-path-{}", std::process::id()));
        let executable = |at: &Path| {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("docker"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(at.join("docker"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        };
        let (bin, other, elsewhere) = (dir.join("bin"), dir.join("other"), dir.join("elsewhere"));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("docker"), "not executable").unwrap();
        executable(&bin);
        executable(&elsewhere);
        let path = std::env::join_paths([&other, &bin]).unwrap();
        assert_eq!(on_path("docker", Some(&path)), Some(bin.join("docker")));
        assert_eq!(on_path("docker", None), None);
        let only_other = std::env::join_paths([&other]).unwrap();
        assert_eq!(on_path("docker", Some(&only_other)), None);
        let up = std::env::current_dir().unwrap().components().count() - 1;
        let relative: PathBuf = std::iter::repeat_n(std::path::Component::ParentDir, up)
            .map(|parent| parent.as_os_str().to_owned())
            .chain(
                elsewhere
                    .components()
                    .skip(1)
                    .map(|part| part.as_os_str().to_owned()),
            )
            .collect();
        assert!(
            relative.is_relative() && relative.join("docker").exists(),
            "{relative:?}"
        );
        let path = std::env::join_paths([relative, bin.clone()]).unwrap();
        assert_eq!(
            on_path("docker", Some(&path)),
            Some(bin.join("docker")),
            "a relative entry skipped"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
