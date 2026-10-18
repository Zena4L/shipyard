//! On a Mac, the commands run in Shipyard's VM (step 10.3b, D24): the same
//! `shipyard`, in the guest, given the same arguments in the same directory
//! (the home is shared at its own path, D10.3-3). What stays here: `vm`,
//! `doctor` (they're about this Mac), and clap's help and version.
//!
//! Before a command goes, what it names on the Mac is checked: a host path
//! (a bind's source, a root filesystem, a bundle, a seccomp profile, `cp`'s
//! side here) must be under the home once its links are followed, or the
//! guest would find something else at that path (its own files, through a
//! link in a project). The variables a bare `-e KEY` copies are read here.
//!
//! `SHIPYARD_VM=off` runs every command on the Mac itself, as before the VM:
//! no isolation, no images (the macOS tests' and the differential's Mac
//! pass, through `.cargo/config.toml` for cargo's runs).

use std::path::{Path, PathBuf};

use crate::cli::{Command, NetworkCommand, SystemCommand, VolumeCommand};

/// Where a command runs on a Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// Here: it's about this Mac (`vm`, `doctor`).
    Here,
    /// In the VM's guest.
    Guest,
    /// Nowhere: it has no meaning on a Mac.
    Refused(&'static str),
}

/// Whether `SHIPYARD_VM`'s value turns the VM off: `off` (or `0`, `false`),
/// so commands run on the Mac itself.
pub(crate) fn vm_off(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| matches!(value.to_str(), Some("off" | "0" | "false")))
}

/// Where `command` runs on a Mac.
pub(crate) fn place(command: &Command) -> Place {
    match command {
        Command::Vm(_) | Command::Doctor => Place::Here,
        Command::System(SystemCommand::Recover | SystemCommand::Stop) => {
            Place::Refused("on a Mac, the VM's own start and stop settle and stop its containers")
        }
        _ => Place::Guest,
    }
}

/// A host path a command names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPath {
    /// As given (relative to the working directory, or absolute).
    pub(crate) path: PathBuf,
    /// Whether it may not exist yet (`cp`'s destination): then its
    /// directory is what's checked.
    pub(crate) may_be_new: bool,
}

impl HostPath {
    /// A path that must already be there to mean anything.
    pub(crate) fn existing(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            may_be_new: false,
        }
    }

    /// A path that may be made by the command.
    pub(crate) fn new_or_existing(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            may_be_new: true,
        }
    }
}

/// What a forwarded command needs of the Mac.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Needs {
    /// Whether it reads its input (else stdin isn't read, nor passed).
    pub(crate) input: bool,
    /// The host paths it names.
    pub(crate) paths: Vec<HostPath>,
    /// The variables it copies from the shell (a bare `-e KEY`).
    pub(crate) copied_env: Vec<String>,
    /// Whether it asks before it acts (a prune without `-f`): it reads its
    /// input when it has a terminal to ask on.
    pub(crate) asks: bool,
}

impl Needs {
    /// Whether the command reads its input, given whether it runs on a
    /// `terminal` in the guest.
    pub(crate) fn reads_input(&self, terminal: bool) -> bool {
        self.input || (self.asks && terminal)
    }
}

/// The terminal a forwarded command gets in the guest (D10.3-7): one when
/// this process's stdin and stdout are both terminals, at their window's
/// `size` (the runtime's default if it can't be read); else none (pipes).
pub(crate) fn guest_terminal(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    size: Option<shipyard_runtime::terminal::Size>,
) -> Option<shipyard_runtime::terminal::Size> {
    (stdin_is_terminal && stdout_is_terminal)
        .then(|| size.unwrap_or(shipyard_runtime::terminal::DEFAULT_SIZE))
}

/// What `command` needs of the Mac.
pub(crate) fn needs(command: &Command) -> Needs {
    match command {
        Command::Run(run) => run.needs(),
        Command::Create(create) => create.needs(),
        Command::Start {
            attach,
            interactive,
            ..
        } => Needs {
            input: *attach && *interactive,
            ..Needs::default()
        },
        Command::Attach { no_stdin, .. } => Needs {
            input: !no_stdin,
            ..Needs::default()
        },
        Command::Exec(exec) => exec.needs(),
        Command::Cp(cp) => cp.needs(),
        Command::Volume(VolumeCommand::Prune { force })
        | Command::Network(NetworkCommand::Prune { force }) => Needs {
            asks: !force,
            ..Needs::default()
        },
        _ => Needs::default(),
    }
}

/// Checks that `path` (relative to `cwd`) is under `home` once its links
/// are followed (both canonical). A path that isn't there is judged by
/// where it would be: its directory if it may be new, else the nearest of
/// its directories that is there (the guest has files of its own outside
/// the home, `/var/lib/shipyard` or `/run`, that the Mac hasn't: never
/// sent); one under the home is the guest's to refuse. A link on its way
/// that leads nowhere here is refused (it may lead somewhere in the
/// guest), as is any other error resolving it.
///
/// # Errors
///
/// Where it really is, and why that's refused.
pub(crate) fn within_home(path: &HostPath, cwd: &Path, home: &Path) -> Result<(), String> {
    let absolute = cwd.join(&path.path);
    let refused = |why: &str| {
        Err(format!(
            "{} {why}: on a Mac, only your home ({}) is shared with the VM, at the same path",
            path.path.display(),
            home.display()
        ))
    };
    let real = match std::fs::canonicalize(&absolute) {
        Ok(real) => real,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if has_dangling_link(&absolute) {
                return refused("goes through a link that leads nowhere on this Mac");
            }
            match (path.may_be_new, absolute.parent(), absolute.file_name()) {
                (true, Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
                    Ok(parent) => parent.join(name),
                    Err(error) => return refused(&format!("can't be resolved ({error})")),
                },
                _ => match where_it_would_be(&absolute) {
                    Some(real) => real,
                    None => return refused("can't be resolved: it isn't there"),
                },
            }
        }
        Err(error) => return refused(&format!("can't be resolved ({error})")),
    };
    if real.starts_with(home) {
        Ok(())
    } else {
        refused(&format!("is {}, outside your home", real.display()))
    }
}

/// Where `path`, which isn't there, would be: the nearest of its
/// directories that is, canonical, with the rest of it. `None` if the rest
/// climbs (`..` past what isn't there can't be judged here).
fn where_it_would_be(path: &Path) -> Option<std::path::PathBuf> {
    let (there, real) = path
        .ancestors()
        .skip(1)
        .find_map(|ancestor| Some((ancestor, std::fs::canonicalize(ancestor).ok()?)))?;
    let rest = path.strip_prefix(there).ok()?;
    let climbs = rest
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir));
    (!climbs).then(|| real.join(rest))
}

/// Whether one of `path`'s components is a symbolic link whose target
/// isn't there.
fn has_dangling_link(path: &Path) -> bool {
    path.ancestors().any(|prefix| {
        std::fs::symlink_metadata(prefix).is_ok_and(|m| m.file_type().is_symlink())
            && std::fs::metadata(prefix).is_err()
    })
}

/// The directory a forwarded command runs in: the caller's, if it's under
/// the home (both canonical: the guest has it at the same path), else `/`
/// (and every relative path it names is then outside the home, refused).
/// `None` for one under the home whose name isn't text.
pub(crate) fn guest_cwd(cwd: &Path, home: &Path) -> Option<String> {
    if cwd.starts_with(home) {
        cwd.to_str().map(str::to_owned)
    } else {
        Some("/".to_owned())
    }
}

/// Checks the variables a command copies from the shell: none the guest's
/// own `shipyard` reads (`forward::reserved`).
///
/// # Errors
///
/// The first reserved one, and why.
pub(crate) fn copyable(names: &[String]) -> Result<(), String> {
    match names
        .iter()
        .find(|name| shipyard_runtime::attach::forward::reserved(name))
    {
        Some(name) => Err(format!(
            "-e {name}: on a Mac, {name} is the VM's own (its shipyard's), not copied from \
             your shell; give it a value (-e {name}=...)"
        )),
        None => Ok(()),
    }
}

/// Whether `command` would write a tar archive to this terminal (`cp
/// CONTAINER:PATH -` with stdout a terminal): refused here, as on Linux
/// (in the guest, its stdout isn't a terminal, so it can't tell).
pub(crate) fn tar_to_terminal(command: &Command, stdout_is_terminal: bool) -> bool {
    matches!(command, Command::Cp(cp) if cp.writes_to_stdout()) && stdout_is_terminal
}

/// Forwards the parsed command to the VM's guest, if it goes there: its
/// exit code, or `None` for one that runs here.
#[cfg(target_os = "macos")]
pub(crate) fn elsewhere(command: &Command) -> Option<std::process::ExitCode> {
    if vm_off(std::env::var_os("SHIPYARD_VM").as_deref()) {
        return None;
    }
    match place(command) {
        Place::Here => None,
        Place::Refused(why) => Some(crate::reporter::report(
            &anyhow::anyhow!("{why}"),
            shipyard_runtime::process::EXIT_ENGINE_ERROR,
        )),
        Place::Guest => Some(mac::forward(command).unwrap_or_else(|error| {
            crate::reporter::report(&error, shipyard_runtime::process::EXIT_ENGINE_ERROR)
        })),
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::process::ExitCode;

    use std::io::IsTerminal as _;

    use anyhow::{Context as _, Result, bail};
    use shipyard_runtime::attach::forward::{self, Request};
    use shipyard_vm::client::{self, Outcome, Status};
    use shipyard_vm::{Guest, VmPaths};

    use super::{guest_cwd, needs, within_home};
    use crate::cli::Command;

    use crate::REVISION;

    pub(super) fn forward(command: &Command) -> Result<ExitCode> {
        let paths = VmPaths::from_env()?;
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .context("HOME isn't set: no home to share with the VM")?;
        let home = std::fs::canonicalize(&home)
            .with_context(|| format!("your home, {}", home.display()))?;
        let cwd = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .context("this directory")?;
        let needs = needs(command);
        for path in &needs.paths {
            if let Err(why) = within_home(path, &cwd, &home) {
                bail!(why);
            }
        }
        if let Err(why) = super::copyable(&needs.copied_env) {
            bail!(why);
        }
        if super::tar_to_terminal(command, std::io::stdout().is_terminal()) {
            bail!("refusing to write a tar archive to a terminal: redirect it (> file.tar)");
        }
        let Some(cwd) = guest_cwd(&cwd, &home) else {
            bail!("this directory's name isn't text: it can't be sent to the VM");
        };
        let args = std::env::args_os()
            .skip(1)
            .map(|arg| {
                arg.into_string().map_err(|arg| {
                    anyhow::anyhow!("{} isn't text: it can't be sent to the VM", arg.display())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut request = Request::new(args, cwd, forward::passed_env(std::env::vars_os()));
        request.named_env = needs
            .copied_env
            .iter()
            .filter_map(|name| Some((name.clone(), std::env::var(name).ok()?)))
            .collect();
        request.terminal = super::guest_terminal(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            shipyard_runtime::terminal::terminal_size(),
        );
        request.input = needs.reads_input(request.terminal.is_some());
        if client::status(&paths)? == Status::Stopped {
            warn_if_another_build(&paths);
        }
        let code = |code: i32| {
            ExitCode::from(
                u8::try_from(code).unwrap_or(shipyard_runtime::process::EXIT_ENGINE_ERROR),
            )
        };
        Ok(match client::forward(&paths, &request) {
            Ok(Outcome::Exited(exited)) => code(exited),
            Ok(Outcome::ReaderGone) => ExitCode::SUCCESS,
            // Ended by a signal before the command was sent: as a shell
            // says a command the signal ended (said on stderr first).
            Err(error @ shipyard_vm::VmError::Interrupted(signalled)) => {
                let _ = crate::reporter::warn(error.to_string());
                code(signalled)
            }
            Err(error) => return Err(error.into()),
        })
    }

    /// Says, as the VM starts, if its guest is another build's: the same
    /// wire still works (another is refused by the guest), but what it does
    /// may differ (D10.3-6).
    fn warn_if_another_build(paths: &VmPaths) {
        let Ok(guest) = Guest::find(&paths.guest) else {
            return;
        };
        if guest.manifest.shipyard.revision != REVISION {
            let _ = crate::reporter::warn(format!(
                "the VM's guest is from build {}, this shipyard from {REVISION}: \
                 scripts/guest/build.sh makes it this build's",
                guest.manifest.shipyard.revision
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shipyard-fwd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// A path in the home passes, relative or not; one a link takes out of
    /// it (to the guest's own files, say), or one outside, is refused; a
    /// new destination is judged by its directory; one that isn't there,
    /// by where it would be: in the home, the guest's to refuse; outside
    /// (the guest's own `/var/lib/shipyard`, which the Mac hasn't), refused.
    #[test]
    fn only_paths_in_the_home_are_forwarded() {
        let root = scratch("paths");
        let home = root.join("home");
        let project = home.join("project");
        std::fs::create_dir_all(project.join("data")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("/var/lib", project.join("escape")).unwrap();
        // Dangling here, it could lead somewhere in the guest.
        std::os::unix::fs::symlink("/var/lib/shipyard", project.join("guest-state")).unwrap();
        let check = |path: HostPath| within_home(&path, &project, &home);
        assert!(check(HostPath::existing("data")).is_ok());
        assert!(check(HostPath::existing(project.join("data"))).is_ok());
        assert!(check(HostPath::existing("escape")).is_err(), "a link out");
        assert!(check(HostPath::existing("../../elsewhere")).is_err());
        assert!(check(HostPath::new_or_existing("new-file")).is_ok());
        assert!(check(HostPath::new_or_existing("../../elsewhere/new")).is_err());
        assert!(
            check(HostPath::existing("missing")).is_ok(),
            "the guest's to refuse"
        );
        assert!(check(HostPath::existing("missing/deeper")).is_ok());
        let nowhere = format!("/shipyard-nowhere-{}/state", std::process::id());
        assert!(check(HostPath::existing(&nowhere)).is_err(), "{nowhere}");
        assert!(check(HostPath::existing(root.join("elsewhere/missing"))).is_err());
        assert!(
            check(HostPath::existing("missing/../../../elsewhere/x")).is_err(),
            "a climb past what isn't there"
        );
        assert!(
            check(HostPath::existing("guest-state")).is_err(),
            "a dangling link"
        );
        assert!(check(HostPath::existing("guest-state/sub")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The guest's directory is the caller's under the home, else `/`.
    #[test]
    fn the_directory_is_the_callers_under_the_home() {
        let home = Path::new("/Users/me");
        let cwd = |dir: &str| guest_cwd(Path::new(dir), home);
        assert_eq!(cwd("/Users/me/app").as_deref(), Some("/Users/me/app"));
        assert_eq!(cwd("/Users/me").as_deref(), Some("/Users/me"));
        assert_eq!(cwd("/private/tmp").as_deref(), Some("/"));
        assert_eq!(cwd("/Users/meow").as_deref(), Some("/"));
    }

    /// `SHIPYARD_VM=off` (or 0, false) runs here; unset, empty, or another
    /// value, doesn't.
    #[test]
    fn the_vm_is_turned_off_only_when_asked() {
        use std::ffi::OsStr;
        for off in ["off", "0", "false"] {
            assert!(vm_off(Some(OsStr::new(off))), "{off}");
        }
        for on in ["on", "1", "", "OFF"] {
            assert!(!vm_off(Some(OsStr::new(on))), "{on}");
        }
        assert!(!vm_off(None));
    }

    /// `vm` and `doctor` stay here, `system recover`/`stop` mean nothing on
    /// a Mac, everything else goes to the guest; what reads input is told
    /// by its flags.
    #[test]
    fn commands_go_where_they_mean_something() {
        use clap::Parser as _;
        let parsed = |words: &[&str]| crate::cli::Cli::try_parse_from(words).unwrap().command;
        assert_eq!(place(&parsed(&["shipyard", "doctor"])), Place::Here);
        assert_eq!(place(&parsed(&["shipyard", "vm", "status"])), Place::Here);
        assert!(matches!(
            place(&parsed(&["shipyard", "system", "recover"])),
            Place::Refused(_)
        ));
        assert_eq!(place(&parsed(&["shipyard", "ps"])), Place::Guest);
        assert_eq!(place(&parsed(&["shipyard", "system", "df"])), Place::Guest);
        for (words, input) in [
            (&["shipyard", "run", "-i", "alpine", "cat"][..], true),
            (&["shipyard", "run", "--rm", "alpine", "cat"], false),
            (&["shipyard", "attach", "web"], true),
            (&["shipyard", "attach", "--no-stdin", "web"], false),
            (&["shipyard", "exec", "-i", "web", "sh"], true),
            (&["shipyard", "cp", "-", "web:/x"], true),
            (&["shipyard", "cp", "web:/x", "-"], false),
            (&["shipyard", "volume", "prune"], false),
            (&["shipyard", "images"], false),
        ] {
            assert_eq!(needs(&parsed(words)).input, input, "{words:?}");
        }
        // A prune asks on a terminal only: there it reads its input.
        for (words, asks) in [
            (&["shipyard", "volume", "prune"][..], true),
            (&["shipyard", "network", "prune"], true),
            (&["shipyard", "volume", "prune", "-f"], false),
            (&["shipyard", "network", "prune", "--force"], false),
            (&["shipyard", "ps"], false),
        ] {
            let needs = needs(&parsed(words));
            assert_eq!(needs.reads_input(true), asks, "{words:?}");
            assert!(!needs.reads_input(false), "{words:?}");
        }
    }

    /// A command gets a terminal in the guest only when both this stdin and
    /// this stdout are terminals, at this window's size.
    #[test]
    fn a_terminal_only_when_both_ends_are_one() {
        use shipyard_runtime::terminal::Size;
        let size = Some(Size::new(50, 132));
        assert_eq!(guest_terminal(true, true, size), size);
        assert_eq!(guest_terminal(true, true, None), Some(Size::new(24, 80)));
        assert_eq!(guest_terminal(false, true, size), None);
        assert_eq!(guest_terminal(true, false, size), None);
        assert_eq!(guest_terminal(false, false, size), None);
    }

    /// A run names its bind sources (not its named volumes), its root
    /// filesystem and its seccomp profile; a bare `-e KEY` is copied, a
    /// `KEY=VALUE` isn't.
    #[test]
    fn a_run_names_its_host_paths_and_copied_variables() {
        use clap::Parser as _;
        let parsed = crate::cli::Cli::try_parse_from([
            "shipyard",
            "run",
            "--volume",
            "/Users/me/app:/app",
            "--volume",
            "cache:/cache",
            "--security-opt",
            "seccomp=profile.json",
            "-e",
            "TOKEN",
            "-e",
            "MODE=dev",
            "alpine",
            "true",
        ])
        .unwrap()
        .command;
        let needs = needs(&parsed);
        assert_eq!(
            needs.paths,
            [
                HostPath::existing("/Users/me/app"),
                HostPath::existing("profile.json"),
            ]
        );
        assert_eq!(needs.copied_env, ["TOKEN"]);
        assert!(copyable(&needs.copied_env).is_ok());
        assert!(copyable(&["TOKEN".to_owned(), "PATH".to_owned()]).is_err());
    }

    /// `cp C:/x -` onto a terminal is refused here; into a pipe, or `cp`
    /// the other way, it isn't.
    #[test]
    fn a_tar_stream_never_reaches_a_terminal() {
        use clap::Parser as _;
        let parsed = |words: &[&str]| crate::cli::Cli::try_parse_from(words).unwrap().command;
        let out = parsed(&["shipyard", "cp", "web:/x", "-"]);
        assert!(tar_to_terminal(&out, true));
        assert!(!tar_to_terminal(&out, false));
        assert!(!tar_to_terminal(
            &parsed(&["shipyard", "cp", "-", "web:/x"]),
            true
        ));
    }
}
