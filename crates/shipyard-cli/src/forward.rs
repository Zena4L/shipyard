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

use crate::cli::{
    Command, ContainerCommand, ImageCommand, NetworkCommand, SystemCommand, VolumeCommand,
};

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
        // A sign-in is the Mac's own: its helpers, its keychain (D12.4-2).
        Command::Vm(_)
        | Command::Doctor
        | Command::Compose(_)
        | Command::Login(_)
        | Command::Logout { .. } => Place::Here,
        Command::System(SystemCommand::Recover | SystemCommand::Stop) => {
            Place::Refused("on a Mac, the VM's own start and stop settle and stop its containers")
        }
        Command::System(SystemCommand::Service { .. }) => Place::Refused(
            "on a Mac, Docker's API is the VM's, served at its `docker.sock` (`shipyard vm \
             status` says where: point DOCKER_HOST at it; `shipyard vm install` keeps it there \
             any time)",
        ),
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

/// The image whose registry a forwarded command signs in to with the
/// Mac's credential (D12.4-2): a `push`'s or `pull`'s alone.
pub(crate) fn signs_in_to(command: &Command) -> Option<&str> {
    match command {
        Command::Push(args) => Some(&args.name),
        Command::Pull(args) => Some(&args.reference),
        _ => None,
    }
}

/// The Mac's credential for the registry of the image `command` names, to
/// hand the guest's command, if it signs in and the Mac has one. One that
/// can't be read (a helper that's gone) is warned of, and the command goes
/// on without: a registry that asks for none needs none, and one that asks
/// says the command isn't signed in.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only a Mac forwards commands")
)]
fn handed_credential(
    command: &Command,
) -> anyhow::Result<Option<shipyard_runtime::attach::forward::RegistryAuth>> {
    let Some(image) = signs_in_to(command) else {
        return Ok(None);
    };
    match shipyard_engine::image::docker_credential_for(image) {
        Ok(found) => Ok(found
            .map(|given| shipyard_runtime::attach::forward::RegistryAuth(given.to_docker_auth()))),
        Err(error) => {
            crate::reporter::warn(format!("{error}; going on without signing in"))?;
            Ok(None)
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
        Command::Start(start) => Needs {
            input: start.attach && start.interactive,
            ..Needs::default()
        },
        Command::Attach(attach) => Needs {
            input: !attach.no_stdin,
            ..Needs::default()
        },
        Command::Exec(exec) => exec.needs(),
        Command::Cp(cp) => cp.needs(),
        Command::Export(export) => Needs {
            paths: export
                .output
                .iter()
                .map(HostPath::new_or_existing)
                .collect(),
            ..Needs::default()
        },
        Command::Import(import) if import.reads_stdin() => Needs {
            input: true,
            ..Needs::default()
        },
        // A URL is refused in the guest, never taken for a path here.
        Command::Import(import) if !import.source_is_url() => Needs {
            paths: vec![HostPath::existing(&import.source)],
            ..Needs::default()
        },
        Command::Build(build) => {
            let mut paths = vec![HostPath::existing(&build.context)];
            paths.extend(
                build
                    .file
                    .iter()
                    .filter(|_| !build.reads_stdin())
                    .map(HostPath::existing),
            );
            paths.extend(build.secret_files().into_iter().map(HostPath::existing));
            if let Ok(Some(
                crate::building::OutputTo::Local(dest) | crate::building::OutputTo::Tar(Some(dest)),
            )) = build.output_to()
            {
                paths.push(HostPath::new_or_existing(dest));
            }
            Needs {
                input: build.reads_stdin(),
                paths,
                ..Needs::default()
            }
        }
        Command::Save(save) => Needs {
            paths: save.output.iter().map(HostPath::new_or_existing).collect(),
            ..Needs::default()
        },
        Command::Load(load) => Needs {
            input: load.input.is_none(),
            paths: load.input.iter().map(HostPath::existing).collect(),
            ..Needs::default()
        },
        Command::Volume(VolumeCommand::Prune { force })
        | Command::Network(NetworkCommand::Prune { force }) => Needs {
            asks: !force,
            ..Needs::default()
        },
        Command::Container(ContainerCommand::Prune(args)) => Needs {
            asks: !args.force,
            ..Needs::default()
        },
        Command::Image(ImageCommand::Prune(args)) => Needs {
            asks: !args.prune.force,
            ..Needs::default()
        },
        Command::System(SystemCommand::Prune(args)) => Needs {
            asks: !args.prune.force,
            ..Needs::default()
        },
        _ => Needs::default(),
    }
}

/// Checks that `path` (relative to `cwd`) is in one of the places the VM
/// shares (`shares`, the home first: step 13.4) once its links are
/// followed (all canonical). A path that isn't there is judged by
/// where it would be: its directory if it may be new, else the nearest of
/// its directories that is there (the guest has files of its own outside
/// the home, `/var/lib/shipyard` or `/run`, that the Mac hasn't: never
/// sent); one in a share is the guest's to refuse. A link on its way
/// that leads nowhere here is refused (it may lead somewhere in the
/// guest), as is any other error resolving it.
///
/// # Errors
///
/// Where it really is, and why that's refused. Shared, whether it's
/// beyond the home (`shares`' first): a guest that shares only the home
/// mustn't be sent it (`Request::beyond_home`).
pub(crate) fn within_shares(
    path: &HostPath,
    cwd: &Path,
    shares: &[PathBuf],
) -> Result<bool, String> {
    let absolute = cwd.join(&path.path);
    let refused = |why: &str| {
        let shared: Vec<String> = shares
            .iter()
            .map(|place| place.display().to_string())
            .collect();
        Err(format!(
            "{} {why}: on a Mac, only your home and a few places beside it ({}) are shared \
             with the VM, at the same paths",
            path.path.display(),
            shared.join(", ")
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
    if shares.iter().any(|shared| real.starts_with(shared)) {
        Ok(!shares.first().is_some_and(|home| real.starts_with(home)))
    } else {
        refused(&format!("is {}, not shared", real.display()))
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

/// The directory a forwarded command runs in: the caller's, if it's in a
/// share (all canonical: the guest has it at the same path), else `/`
/// (and every relative path it names is then outside them, refused).
/// `None` for one in a share whose name isn't text.
pub(crate) fn guest_cwd(cwd: &Path, shares: &[PathBuf]) -> Option<String> {
    if shares.iter().any(|shared| cwd.starts_with(shared)) {
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
/// CONTAINER:PATH -`, `save` without `-o`, with stdout a terminal): refused
/// here, as on Linux (in the guest, its stdout isn't a terminal, so it
/// can't tell).
pub(crate) fn tar_to_terminal(command: &Command, stdout_is_terminal: bool) -> bool {
    let writes = match command {
        Command::Cp(cp) => cp.writes_to_stdout(),
        Command::Save(save) => save.output.is_none(),
        Command::Export(export) => export.output.is_none(),
        Command::Build(build) => {
            matches!(
                build.output_to(),
                Ok(Some(crate::building::OutputTo::Tar(None)))
            )
        }
        _ => false,
    };
    writes && stdout_is_terminal
}

/// The hint a terminal refusal of `command` gives (what to name instead),
/// as on Linux.
pub(crate) fn terminal_hint(command: &Command) -> &'static str {
    match command {
        Command::Save(_) | Command::Export(_) => crate::output::GIVE_OUTPUT,
        Command::Build(_) => crate::output::GIVE_TAR_DEST,
        Command::Load(_) => crate::output::GIVE_INPUT,
        Command::Import(_) => crate::output::GIVE_FILE,
        _ => "",
    }
}

/// Whether `command` reads its Dockerfile from this terminal (`build -f -`):
/// refused here, as on Linux (the guest's standard input is a pipe: it
/// would take what's typed as the Dockerfile, to Ctrl-D).
pub(crate) fn dockerfile_from_terminal(command: &Command, stdin_is_terminal: bool) -> bool {
    matches!(command, Command::Build(build) if build.reads_stdin()) && stdin_is_terminal
}

/// Whether `command` would read a tar archive from this terminal (`load`
/// without `-i`, with stdin a terminal): refused here, as on Linux.
pub(crate) fn tar_from_terminal(command: &Command, stdin_is_terminal: bool) -> bool {
    let reads = match command {
        Command::Load(load) => load.input.is_none(),
        Command::Import(import) => import.reads_stdin(),
        _ => false,
    };
    reads && stdin_is_terminal
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
        Place::Refused(why) => Some(crate::reporter::report(&why, command.failure_code())),
        Place::Guest => {
            Some(mac::forward(command).unwrap_or_else(|error| {
                crate::reporter::report_error(&error, command.failure_code())
            }))
        }
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

    use super::{guest_cwd, needs, within_shares};
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
        let shares = shipyard_vm::shares::all(&home);
        let needs = needs(command);
        // (A directory or a path in a share beyond the home: a guest that
        // shares only the home refuses the request, never runs it in a
        // directory of its own: stage 13's close.)
        let mut beyond_home =
            !cwd.starts_with(&home) && shares.iter().any(|shared| cwd.starts_with(shared));
        for path in &needs.paths {
            match within_shares(path, &cwd, &shares) {
                Ok(beyond) => beyond_home |= beyond,
                Err(why) => bail!(why),
            }
        }
        if let Err(why) = super::copyable(&needs.copied_env) {
            bail!(why);
        }
        let hint = super::terminal_hint(command);
        if super::tar_to_terminal(command, std::io::stdout().is_terminal()) {
            return Err(crate::output::tar_to_terminal_refused(hint));
        }
        if super::tar_from_terminal(command, std::io::stdin().is_terminal()) {
            return Err(crate::output::tar_from_terminal_refused(hint));
        }
        if super::dockerfile_from_terminal(command, std::io::stdin().is_terminal()) {
            bail!(crate::building::DOCKERFILE_FROM_TERMINAL);
        }
        let Some(cwd) = guest_cwd(&cwd, &shares) else {
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
        if let Some(handed) = super::handed_credential(command)? {
            request = request.with_registry_auth(handed);
        }
        if beyond_home {
            request = request.beyond_home();
        }
        if client::status(&paths)? == Status::Stopped {
            warn_if_another_build(&paths);
        } else if let Some(another) = shipyard_vm::booted::running_elsewhere(&paths) {
            // A VM an older build started (F173b): what this command meets
            // there may not be what this build would do.
            let _ = crate::reporter::warn(another);
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

    /// A path in a share passes, relative or not; one a link takes out of
    /// them (to the guest's own files, say), or one outside, is refused; a
    /// new destination is judged by its directory; one that isn't there,
    /// by where it would be: in a share, the guest's to refuse; outside
    /// (the guest's own `/var/lib/shipyard`, which the Mac hasn't), refused.
    #[test]
    fn only_paths_in_the_shares_are_forwarded() {
        let root = scratch("paths");
        let home = root.join("home");
        let project = home.join("project");
        std::fs::create_dir_all(project.join("data")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::create_dir_all(root.join("volume/data")).unwrap();
        std::os::unix::fs::symlink("/var/lib", project.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("volume"), project.join("to-volume")).unwrap();
        // Dangling here, it could lead somewhere in the guest.
        std::os::unix::fs::symlink("/var/lib/shipyard", project.join("guest-state")).unwrap();
        let shares = [home.clone(), root.join("volume")];
        let check = |path: HostPath| within_shares(&path, &project, &shares);
        assert_eq!(check(HostPath::existing("data")), Ok(false), "in the home");
        assert_eq!(
            check(HostPath::existing(root.join("volume/data"))),
            Ok(true),
            "beyond the home"
        );
        assert!(
            check(HostPath::existing("to-volume/data")).is_ok(),
            "a link to a share"
        );
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

    /// The guest's directory is the caller's in a share, else `/`.
    #[test]
    fn the_directory_is_the_callers_in_a_share() {
        let shares = [PathBuf::from("/Users/me"), PathBuf::from("/private/tmp")];
        let cwd = |dir: &str| guest_cwd(Path::new(dir), &shares);
        assert_eq!(cwd("/Users/me/app").as_deref(), Some("/Users/me/app"));
        assert_eq!(cwd("/Users/me").as_deref(), Some("/Users/me"));
        assert_eq!(cwd("/private/tmp/x").as_deref(), Some("/private/tmp/x"));
        assert_eq!(cwd("/opt").as_deref(), Some("/"));
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
        // A sign-in is the Mac's own (D12.4-2); a push or pull is the
        // guest's, handed the Mac's credential for its image, alone.
        assert_eq!(
            place(&parsed(&["shipyard", "login", "ghcr.io"])),
            Place::Here
        );
        assert_eq!(place(&parsed(&["shipyard", "logout"])), Place::Here);
        assert_eq!(
            place(&parsed(&["shipyard", "push", "ghcr.io/me/a:1"])),
            Place::Guest
        );
        assert_eq!(
            signs_in_to(&parsed(&["shipyard", "image", "push", "ghcr.io/me/a:1"]).flattened()),
            Some("ghcr.io/me/a:1")
        );
        assert_eq!(
            signs_in_to(&parsed(&["shipyard", "pull", "alpine"])),
            Some("alpine")
        );
        for words in [
            &["shipyard", "run", "ghcr.io/me/a:1"][..],
            &["shipyard", "build", "-t", "a", "."],
            &["shipyard", "images"],
        ] {
            assert_eq!(signs_in_to(&parsed(words)), None, "{words:?}");
        }
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
            (&["shipyard", "container", "prune"], true),
            (&["shipyard", "image", "prune", "-a"], true),
            (&["shipyard", "system", "prune", "--volumes"], true),
            (&["shipyard", "container", "prune", "-f"], false),
            (&["shipyard", "image", "prune", "-a", "-f"], false),
            (&["shipyard", "system", "prune", "-f"], false),
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

    /// `build -f -` with a terminal for its input is refused here, as on
    /// Linux; from a pipe, or with a Dockerfile named, it isn't; each
    /// refusal gives Linux's hint.
    #[test]
    fn a_dockerfile_is_never_read_from_a_terminal() {
        use clap::Parser as _;
        let parsed = |words: &[&str]| crate::cli::Cli::try_parse_from(words).unwrap().command;
        let piped = parsed(&["shipyard", "build", "-f", "-", "-t", "a", "."]);
        assert!(dockerfile_from_terminal(&piped, true));
        assert!(!dockerfile_from_terminal(&piped, false));
        let named = parsed(&["shipyard", "build", "-f", "Dockerfile", "-t", "a", "."]);
        assert!(!dockerfile_from_terminal(&named, true));
        assert_eq!(
            terminal_hint(&parsed(&["shipyard", "save", "x"])),
            crate::output::GIVE_OUTPUT
        );
        assert_eq!(
            terminal_hint(&parsed(&["shipyard", "load"])),
            crate::output::GIVE_INPUT
        );
        assert_eq!(
            terminal_hint(&parsed(&["shipyard", "import", "-", "x:1"])),
            crate::output::GIVE_FILE
        );
        assert_eq!(terminal_hint(&parsed(&["shipyard", "cp", "c:/x", "-"])), "");
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
        // `save`'s archive (step 12.2a): to stdout, never with -o.
        assert!(tar_to_terminal(
            &parsed(&["shipyard", "save", "alpine"]),
            true
        ));
        assert!(!tar_to_terminal(
            &parsed(&["shipyard", "save", "alpine"]),
            false
        ));
        assert!(!tar_to_terminal(
            &parsed(&["shipyard", "image", "save", "-o", "a.tar", "alpine"]).flattened(),
            true
        ));
        // `export`'s to stdout; `import -`'s from stdin (step 12.2b).
        assert!(tar_to_terminal(&parsed(&["shipyard", "export", "c"]), true));
        assert!(!tar_to_terminal(
            &parsed(&["shipyard", "export", "-o", "x.tar", "c"]),
            true
        ));
        assert!(tar_from_terminal(
            &parsed(&["shipyard", "import", "-", "a:1"]),
            true
        ));
        assert!(!tar_from_terminal(
            &parsed(&["shipyard", "import", "x.tar", "a:1"]),
            true
        ));
        // `load`'s, from stdin: never with -i.
        assert!(tar_from_terminal(&parsed(&["shipyard", "load"]), true));
        assert!(!tar_from_terminal(&parsed(&["shipyard", "load"]), false));
        assert!(!tar_from_terminal(
            &parsed(&["shipyard", "load", "-i", "a.tar"]),
            true
        ));
    }

    /// `save -o` and `load -i` name host paths, checked as `cp`'s (step
    /// 12.2a); `load` from stdin reads its input, `-i`'s doesn't.
    #[test]
    fn save_and_load_name_their_files() {
        use clap::Parser as _;
        let parsed = |words: &[&str]| {
            crate::cli::Cli::try_parse_from(words)
                .unwrap()
                .command
                .flattened()
        };
        let save = needs(&parsed(&[
            "shipyard", "image", "save", "-o", "out.tar", "a",
        ]));
        assert_eq!(save.paths, [HostPath::new_or_existing("out.tar")]);
        assert!(!save.input);
        assert!(needs(&parsed(&["shipyard", "save", "a"])).paths.is_empty());
        // `export -o` and `import FILE` (step 12.2b): their files; `import
        // -` its input; never `import`'s name, nor a URL.
        let export = needs(&parsed(&[
            "shipyard",
            "container",
            "export",
            "-o",
            "x.tar",
            "c",
        ]));
        assert_eq!(export.paths, [HostPath::new_or_existing("x.tar")]);
        let import = needs(&parsed(&["shipyard", "import", "rootfs.tar", "app:1"]));
        assert_eq!(import.paths, [HostPath::existing("rootfs.tar")]);
        assert!(!import.input);
        let stdin = needs(&parsed(&["shipyard", "image", "import", "-", "app:1"]));
        assert!(stdin.input && stdin.paths.is_empty());
        let url = needs(&parsed(&["shipyard", "import", "https://x/y.tar", "app:1"]));
        assert!(url.paths.is_empty() && !url.input);
        // `build` (step 12.3): its context, `-f`, each secret's file and
        // `-o`'s destination; `-f -` its input.
        let build = needs(&parsed(&[
            "shipyard",
            "build",
            "-t",
            "a:1",
            "-f",
            "ci/Dockerfile",
            "--secret",
            "id=t,src=tok",
            "-o",
            "type=local,dest=out",
            "ctx",
        ]));
        assert_eq!(
            build.paths,
            [
                HostPath::existing("ctx"),
                HostPath::existing("ci/Dockerfile"),
                HostPath::existing("tok"),
                HostPath::new_or_existing("out"),
            ]
        );
        assert!(!build.input);
        let stdin = needs(&parsed(&[
            "shipyard", "image", "build", "-f", "-", "-t", "a:1", ".",
        ]));
        assert!(stdin.input);
        assert_eq!(stdin.paths, [HostPath::existing(".")]);
        assert!(tar_to_terminal(
            &parsed(&["shipyard", "build", "-o", "-", "."]),
            true
        ));
        let load = needs(&parsed(&["shipyard", "load", "-i", "in.tar"]));
        assert_eq!(load.paths, [HostPath::existing("in.tar")]);
        assert!(!load.input);
        assert!(needs(&parsed(&["shipyard", "load"])).input);
    }
}
