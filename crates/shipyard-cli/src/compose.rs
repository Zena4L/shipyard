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
    let docker = shipyard_sys::path::find_program("docker", std::env::var_os("PATH").as_deref())
        .context(
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
    ensure_served(&socket)?;
    Ok(socket)
}

/// Makes sure something serves `socket`: `shipyard system service` started
/// if nothing answers (the engine's rule), left to run on to its idle end
/// (after compose's `exec` it's compose's child; after `system service
/// --ensure`, no one's: its own process group). What's sent there (compose's
/// project, its variables, a registry's credentials) goes to whatever
/// answers: never through a directory another user can change (one not
/// there yet, the service makes as its own).
#[cfg(not(target_os = "macos"))]
pub(crate) fn ensure_served(socket: &Path) -> Result<()> {
    if let Some(dir) = socket.parent() {
        match shipyard_sys::fs::unsafe_dir(dir) {
            Ok(Some(why)) => anyhow::bail!(
                "the API's socket's directory {} isn't yours alone: {why}",
                dir.display()
            ),
            // Not there yet: the service makes it, as its own.
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!(
                "the API's socket's directory {} can't be checked: {error}",
                dir.display()
            ),
        }
    }
    let _service = shipyard_engine::api_service::ensure(socket, &|| service(socket))?;
    Ok(())
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
}
