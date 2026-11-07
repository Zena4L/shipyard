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
/// --ensure`, no one's: its own process group; through the user's systemd
/// manager where one runs, the manager's: [`launcher`]). What's sent there (compose's
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
    #[cfg(target_os = "linux")]
    if let Some((program, args)) = launcher(socket) {
        let launch = || {
            let mut command = std::process::Command::new(&program);
            command.args(&args);
            command
        };
        shipyard_engine::api_service::ensure_launched(socket, &launch, &|| service(socket))?;
        return Ok(());
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

/// `shipyard system service --socket SOCKET`, this binary, as the
/// caller's child.
#[cfg(not(target_os = "macos"))]
fn service(socket: &Path) -> std::process::Command {
    // (`/proc/self/exe` on Linux: this very binary, wherever it's been
    // moved since.)
    let mut command = std::process::Command::new("/proc/self/exe");
    command.args(["system", "service", "--socket"]).arg(socket);
    command
}

/// `systemd-run --user …` for the service (F198), so it runs with the user
/// manager's limits and `PATH`, not those of whoever started it (a desktop
/// session's, a terminal's), where it can be: not root, the caller's user
/// manager running (its private socket in `XDG_RUNTIME_DIR`) and this
/// process under it (not in a container sharing the host's runtime
/// directory, a toolbox's: the manager would run the host's `shipyard`, or
/// none), `systemd-run` on `PATH`, this binary's path known. A launch that
/// fails falls back to the caller's child (`ensure_launched`).
#[cfg(target_os = "linux")]
fn launcher(socket: &Path) -> Option<(PathBuf, Vec<OsString>)> {
    if nix::unistd::geteuid().is_root() {
        return None;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.join("systemd/private").exists() {
        return None;
    }
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    if !under_user_manager(&own) {
        return None;
    }
    let systemd_run =
        shipyard_sys::path::find_program("systemd-run", std::env::var_os("PATH").as_deref())?;
    let exe = std::env::current_exe().ok()?;
    let log = socket
        .parent()?
        .join(shipyard_engine::api_service::SERVICE_LOG);
    let args = user_unit_args(socket, &exe, &log, std::env::vars_os())?;
    Some((systemd_run, args))
}

/// Whether `/proc/self/cgroup`'s `own` says this process is in a user
/// manager's tree (`…/user@UID.service/…`): in a container's cgroup
/// namespace it's `/` or the container's own.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux has a user manager")
)]
fn under_user_manager(own: &str) -> bool {
    own.lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|path| {
            path.split('/')
                .any(|part| part.starts_with("user@") && part.ends_with(".service"))
        })
}

/// The variables a service started through the user's manager is given
/// from its caller's: Shipyard's own (its stores, its socket, its idle
/// time), where Docker's config and the user's data are, and the proxies
/// and certificates a pull goes through, so it serves what its caller
/// would have. Nothing else: its `PATH` and limits are the manager's.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux has a user manager")
)]
fn passed_on(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.starts_with("SHIPYARD_")
            || matches!(
                name,
                "DOCKER_CONFIG"
                    | "XDG_DATA_HOME"
                    | "XDG_CONFIG_HOME"
                    | "RUST_LOG"
                    | "HTTP_PROXY"
                    | "HTTPS_PROXY"
                    | "NO_PROXY"
                    | "ALL_PROXY"
                    | "http_proxy"
                    | "https_proxy"
                    | "no_proxy"
                    | "all_proxy"
                    | "SSL_CERT_FILE"
                    | "SSL_CERT_DIR"
            )
    })
}

/// `systemd-run`'s arguments for the service: a unit of its own, named by
/// its socket (a second start while it runs is refused by the manager, and
/// the engine asks the socket again), collected once it ends, which stops
/// only its own process (the containers it started run on, as the shipped
/// unit's `KillMode=process`), its output appended to `log`, given the
/// caller's variables [`passed_on`] says. `None` for a path that isn't
/// [`plain`], or a value passed on that isn't UTF-8 or holds a control
/// character (systemd refuses it): the caller's child then.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux has a user manager")
)]
fn user_unit_args(
    socket: &Path,
    exe: &Path,
    log: &Path,
    environment: impl Iterator<Item = (OsString, OsString)>,
) -> Option<Vec<OsString>> {
    let (socket, exe, log) = (plain(socket)?, plain(exe)?, plain(log)?);
    let mut args: Vec<OsString> = [
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!(
            "--unit=shipyard-api-{:016x}",
            shipyard_sys::hash::fnv1a(socket.as_bytes())
        ),
        "--description=Shipyard: the Docker API, started on demand".to_owned(),
        "--property=KillMode=process".to_owned(),
        format!("--property=StandardOutput=append:{log}"),
        format!("--property=StandardError=append:{log}"),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for (name, value) in environment.filter(|(name, _)| passed_on(name)) {
        let value = value.to_str()?;
        if value.chars().any(char::is_control) {
            return None;
        }
        let mut set = OsString::from("--setenv=");
        set.push(name);
        set.push("=");
        set.push(value);
        args.push(set);
    }
    args.extend(
        ["--", exe, "system", "service", "--socket", socket]
            .into_iter()
            .map(OsString::from),
    );
    Some(args)
}

/// `path` as text systemd reads as given: none for one with `%`
/// specifiers, `$` variables, spaces, quotes, backslashes, control
/// characters, or not UTF-8.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux has a user manager")
)]
fn plain(path: &Path) -> Option<&str> {
    path.to_str().filter(|text| {
        !text.is_empty()
            && !text.chars().any(|c| {
                c.is_whitespace() || c.is_control() || matches!(c, '%' | '$' | '\\' | '"' | '\'')
            })
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

    /// Under a user manager by its own cgroup: a session's or a service's
    /// of `user@UID.service`, not a container's or a login session's.
    #[test]
    fn a_process_under_a_user_manager_is_told() {
        assert!(under_user_manager(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome.scope\n"
        ));
        assert!(!under_user_manager("0::/\n"));
        assert!(!under_user_manager(
            "0::/user.slice/user-1000.slice/session-2.scope\n"
        ));
        assert!(!under_user_manager("0::/system.slice/docker-abc.scope\n"));
    }

    /// Through the user's manager (F198): a unit named by its socket,
    /// only its own process stopped, its output to the log, Shipyard's
    /// variables and Docker's config passed on and nothing else (its
    /// `PATH` the manager's), then the service's own command.
    #[test]
    fn the_service_is_a_unit_of_the_user_s_manager() {
        let environment = [
            ("SHIPYARD_IMAGE_STORE", "/tmp/images"),
            ("DOCKER_CONFIG", "/home/u/.docker"),
            ("PATH", "/opt/gui/bin"),
            ("https_proxy", "http://proxy:3128"),
            ("LD_PRELOAD", "/x.so"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let args = user_unit_args(
            Path::new("/run/user/1000/shipyard/api.sock"),
            Path::new("/usr/local/bin/shipyard"),
            Path::new("/run/user/1000/shipyard/service.log"),
            environment.clone().into_iter(),
        )
        .unwrap();
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        let unit = format!(
            "--unit=shipyard-api-{:016x}",
            shipyard_sys::hash::fnv1a(b"/run/user/1000/shipyard/api.sock")
        );
        assert_eq!(
            args,
            [
                "--user",
                "--quiet",
                "--collect",
                &unit,
                "--description=Shipyard: the Docker API, started on demand",
                "--property=KillMode=process",
                "--property=StandardOutput=append:/run/user/1000/shipyard/service.log",
                "--property=StandardError=append:/run/user/1000/shipyard/service.log",
                "--setenv=SHIPYARD_IMAGE_STORE=/tmp/images",
                "--setenv=DOCKER_CONFIG=/home/u/.docker",
                "--setenv=https_proxy=http://proxy:3128",
                "--",
                "/usr/local/bin/shipyard",
                "system",
                "service",
                "--socket",
                "/run/user/1000/shipyard/api.sock",
            ]
        );
        // Another socket, another unit.
        assert_ne!(
            shipyard_sys::hash::fnv1a(b"/run/user/1000/shipyard/api.sock"),
            shipyard_sys::hash::fnv1a(b"/tmp/s/api.sock")
        );
        // A value systemd would refuse: the caller's child instead.
        let control = [(
            OsString::from("SHIPYARD_IMAGE_STORE"),
            OsString::from("/tmp/a\nb"),
        )];
        assert!(
            user_unit_args(
                Path::new("/tmp/api.sock"),
                Path::new("/usr/local/bin/shipyard"),
                Path::new("/tmp/service.log"),
                control.into_iter(),
            )
            .is_none()
        );
        // A path systemd would read otherwise: the caller's child instead.
        for odd in [
            "/tmp/a b/api.sock",
            "/tmp/%h/api.sock",
            "/tmp/$X/api.sock",
            "/tmp/a\\b",
        ] {
            assert!(
                user_unit_args(
                    Path::new(odd),
                    Path::new("/usr/local/bin/shipyard"),
                    Path::new("/tmp/service.log"),
                    environment.clone().into_iter(),
                )
                .is_none(),
                "{odd}"
            );
        }
    }
}
