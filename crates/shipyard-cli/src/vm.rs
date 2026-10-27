//! `shipyard vm`: the Linux VM containers run in on a Mac (step 10.3).
//! On Linux there's no VM: containers run on the host itself.

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::VmCommand;

/// `shipyard vm COMMAND`.
pub(crate) fn command(command: &VmCommand) -> Result<ExitCode> {
    #[cfg(target_os = "macos")]
    {
        mac::command(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = command;
        anyhow::bail!("there's no VM here: on Linux, containers run on this host itself")
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::io::Write as _;
    use std::process::ExitCode;

    use anyhow::Result;
    use shipyard_vm::client::{self, Status};
    use shipyard_vm::ports::PortsFile;
    use shipyard_vm::{Guest, VmPaths};

    use crate::cli::VmCommand;
    use crate::output::printable;

    pub(super) fn command(command: &VmCommand) -> Result<ExitCode> {
        let paths = VmPaths::from_env()?;
        match command {
            VmCommand::Start => {
                client::start(&paths)?;
                Ok(ExitCode::SUCCESS)
            }
            VmCommand::Status => status(&paths),
            VmCommand::Stop => {
                client::stop(&paths)?;
                Ok(ExitCode::SUCCESS)
            }
            VmCommand::Install(args) => {
                let installed = shipyard_vm::launchd::install(&paths, args.at_login)?;
                let mut out = std::io::stdout().lock();
                writeln!(
                    out,
                    "launchd holds Docker's socket: {}",
                    printable(&paths.docker_socket().to_string_lossy())
                )?;
                if args.at_login {
                    writeln!(
                        out,
                        "at login: the VM starts when containers with a restart policy would \
                         come back"
                    )?;
                }
                writeln!(
                    out,
                    "(its agent {}: {})",
                    installed.label,
                    printable(&installed.plist.to_string_lossy())
                )?;
                Ok(ExitCode::SUCCESS)
            }
            VmCommand::Set(args) => set(&paths, args),
            VmCommand::Uninstall => {
                let done = shipyard_vm::launchd::uninstall(&paths)?;
                let mut out = std::io::stdout().lock();
                if done.was_installed {
                    writeln!(out, "launchd no longer holds Docker's socket")?;
                    if done.stopped_the_vm {
                        writeln!(out, "(the VM it had started is stopped)")?;
                    }
                } else {
                    writeln!(
                        out,
                        "nothing to uninstall: launchd held no socket of this VM's"
                    )?;
                }
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    /// `vm set`: the size kept in `vm.json`, said, with when it's taken.
    fn set(paths: &VmPaths, args: &crate::cli::VmSetArgs) -> Result<ExitCode> {
        use anyhow::Context as _;
        use shipyard_vm::settings::{self, Change};
        let memory = match args.memory.as_deref() {
            None => Change::Keep,
            Some("default") => Change::Default,
            Some(text) => Change::To(
                shipyard_runtime::resources::parse_size(text)
                    .map_err(|error| anyhow::anyhow!("--memory {text}: {error}"))?,
            ),
        };
        let cpus = match args.cpus.as_deref() {
            None => Change::Keep,
            Some("default") => Change::Default,
            Some(text) => Change::To(
                text.parse()
                    .with_context(|| format!("--cpus {text}: a number, or default"))?,
            ),
        };
        paths.prepare(nix::unistd::getuid().as_raw())?;
        let mac = settings::this_mac()?;
        let kept = settings::VmSettings::change(&paths.settings(), mac, memory, cpus)?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", size_line(&settings::resolve(Some(&kept), mac)))?;
        if client::status(paths)? != Status::Stopped {
            writeln!(
                out,
                "the VM runs: it takes this at its next start (`shipyard vm stop` first)"
            )?;
        }
        Ok(ExitCode::SUCCESS)
    }

    /// The size a start takes, and where each part came from.
    fn size_line(size: &shipyard_vm::settings::Size) -> String {
        use shipyard_vm::settings::{Source, shown};
        let from = |source: Source| match source {
            Source::Default => "this Mac's default",
            Source::Set => "vm set",
        };
        format!(
            "size: {} ({}), {} CPUs ({})",
            shown(size.memory_mib.0),
            from(size.memory_mib.1),
            size.cpus.0,
            from(size.cpus.1)
        )
    }

    fn status(paths: &VmPaths) -> Result<ExitCode> {
        let mut out = std::io::stdout().lock();
        let running = client::status(paths)?;
        match running {
            Status::Running(Some(pid)) => writeln!(out, "running (host PID {pid})")?,
            Status::Running(None) => writeln!(out, "running")?,
            Status::Stopped => writeln!(out, "stopped")?,
        }
        // (A stopped host's file, if a killed one left it, says nothing.)
        if running != Status::Stopped {
            match PortsFile::read(&paths.ports()) {
                Ok(Some(ports)) => {
                    let shadowed = shipyard_vm::ports::shadowed(&ports);
                    for line in super::port_lines(&ports, &shadowed) {
                        writeln!(out, "{line}")?;
                    }
                }
                Ok(None) => {}
                Err(error) => writeln!(out, "ports: {}", printable(&error.to_string()))?,
            }
        }
        if let Some(another) = shipyard_vm::booted::running_elsewhere(paths) {
            writeln!(out, "build: {}", printable(&another))?;
        }
        match Guest::find(&paths.guest) {
            Ok(guest) => writeln!(
                out,
                "guest: Alpine {}, kernel {}, shipyard {} ({})",
                printable(&guest.manifest.alpine),
                printable(&guest.manifest.kernel.version),
                printable(&guest.manifest.shipyard.revision),
                printable(&paths.guest.to_string_lossy())
            )?,
            Err(error) => writeln!(out, "guest: {}", printable(&error.to_string()))?,
        }
        let socket = paths.docker_socket();
        let socket = socket.to_string_lossy();
        let docker = printable(&socket);
        match shipyard_vm::launchd::state(paths) {
            shipyard_vm::launchd::AgentState::Loaded => {
                writeln!(out, "docker: {docker} (launchd's: any time)")?;
            }
            // (Docker's socket is then no one's, the VM's host leaving it to
            // launchd: F152.)
            shipyard_vm::launchd::AgentState::NotLoaded => writeln!(
                out,
                "docker: {docker} ({})",
                shipyard_vm::launchd::NOT_LOADED
            )?,
            shipyard_vm::launchd::AgentState::NotInstalled => writeln!(
                out,
                "docker: {docker} (while the VM runs: `shipyard vm install` for any time)"
            )?,
        }
        if shipyard_vm::launchd::installed_by_older(paths) {
            writeln!(out, "launchd: {}", shipyard_vm::launchd::OLDER_AGENT)?;
        }
        if let Some(line) = super::at_login_line(paths) {
            writeln!(out, "{line}")?;
        }
        match shipyard_vm::settings::VmSettings::read(&paths.settings()).and_then(|kept| {
            Ok(shipyard_vm::settings::resolve(
                kept.as_ref(),
                shipyard_vm::settings::this_mac()?,
            ))
        }) {
            Ok(size) => writeln!(out, "{} (taken at each start)", size_line(&size))?,
            Err(error) => writeln!(out, "size: {}", printable(&error.to_string()))?,
        }
        writeln!(out, "files: {}", printable(&paths.dir.to_string_lossy()))?;
        Ok(ExitCode::SUCCESS)
    }
}

/// What `vm status` says of the start at login (`vm install --at-login`),
/// if it was asked: how many containers the guest last said would come
/// back.
#[cfg(target_os = "macos")]
fn at_login_line(paths: &shipyard_vm::VmPaths) -> Option<String> {
    use crate::output::printable;
    if !shipyard_vm::launchd::starts_at_login(paths) {
        return None;
    }
    Some(
        match shipyard_vm::comes_back::ComesBack::read(&paths.comes_back()) {
            Ok(_)
                if shipyard_vm::launchd::state(paths)
                    != shipyard_vm::launchd::AgentState::Loaded =>
            {
                "at login: nothing starts until launchd loads its agent".to_owned()
            }
            Ok(Some(back)) if back.containers > 0 => format!(
                "at login: starts the VM ({})",
                shipyard_vm::comes_back::would_come_back(back.containers)
            ),
            Ok(_) => "at login: nothing to start (no container would come back)".to_owned(),
            Err(error) => format!("at login: {}", printable(&error.to_string())),
        },
    )
}

/// What `vm status` says of the guest's ports on this Mac: those forwarded
/// (its loopback's only, UDP's marked), those another program shadows
/// (`shadowed`: it holds every address of that port, F98), those that can't
/// be and why, and an older guest's UDP ones, which aren't.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "a Mac's; tested everywhere")
)]
fn port_lines(
    ports: &shipyard_vm::ports::PortsFile,
    shadowed: &[(u16, shipyard_runtime::attach::forward::Protocol)],
) -> Vec<String> {
    let mut lines = Vec::new();
    let forwarded: Vec<String> = ports
        .forwarded
        .iter()
        .map(ToString::to_string)
        .chain(
            ports
                .forwarded_udp
                .iter()
                .map(|address| format!("{address}/udp")),
        )
        .collect();
    if !forwarded.is_empty() {
        lines.push(format!("ports: {}", forwarded.join(", ")));
    }
    let unbound = ports
        .unbound
        .iter()
        .map(|(address, why)| (address.to_string(), why));
    let unbound_udp = ports
        .unbound_udp
        .iter()
        .map(|(address, why)| (format!("{address}/udp"), why));
    for (address, why) in unbound.chain(unbound_udp) {
        lines.push(format!(
            "not forwarded: {address} ({})",
            crate::output::printable(&why.to_string())
        ));
    }
    for (port, protocol) in shadowed {
        let shown = match protocol {
            shipyard_runtime::attach::forward::Protocol::Tcp => port.to_string(),
            shipyard_runtime::attach::forward::Protocol::Udp => format!("{port}/udp"),
        };
        lines.push(format!(
            "shadowed: {shown}: another program on this Mac listens on every address of it, \
             yet localhost:{port} reaches the container (stop that program, or publish the \
             container on another port)"
        ));
    }
    if ports.udp > 0 {
        lines.push(format!(
            "not forwarded: {} UDP port{} (the running guest is older than this shipyard: \
             `shipyard vm stop` starts it again as this build's)",
            ports.udp,
            if ports.udp == 1 { "" } else { "s" }
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forwarded ports on one line, each that can't be on its own with
    /// why, each another program shadows, the UDP ones counted; nothing
    /// when there's none.
    #[test]
    fn ports_are_said_with_what_isnt_forwarded() {
        use shipyard_runtime::attach::forward::Protocol::{Tcp, Udp};
        use shipyard_vm::ports::{PortsFile, Unbound};
        let file = PortsFile::new(
            &[
                (Tcp, "127.0.0.1:8080".parse().unwrap()),
                (Tcp, "[::1]:8080".parse().unwrap()),
                (Udp, "127.0.0.1:53".parse().unwrap()),
            ],
            &[
                ((Tcp, "127.0.0.1:5432".parse().unwrap()), Unbound::InUse),
                ((Udp, "[::1]:53".parse().unwrap()), Unbound::InUse),
            ],
            2,
            std::time::Duration::from_secs(300),
        );
        assert_eq!(
            port_lines(&file, &[(8080, Tcp), (53, Udp)]),
            [
                "ports: 127.0.0.1:8080, [::1]:8080, 127.0.0.1:53/udp",
                "not forwarded: 127.0.0.1:5432 (in use on this Mac)",
                "not forwarded: [::1]:53/udp (in use on this Mac)",
                "shadowed: 8080: another program on this Mac listens on every address of it, yet \
                 localhost:8080 reaches the container (stop that program, or publish the \
                 container on another port)",
                "shadowed: 53/udp: another program on this Mac listens on every address of it, \
                 yet localhost:53 reaches the container (stop that program, or publish the \
                 container on another port)",
                "not forwarded: 2 UDP ports (the running guest is older than this shipyard: \
                 `shipyard vm stop` starts it again as this build's)",
            ]
        );
        assert!(
            port_lines(
                &PortsFile::new(&[], &[], 0, std::time::Duration::from_secs(300)),
                &[]
            )
            .is_empty()
        );
    }
}
