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
            VmCommand::Install => {
                let installed = shipyard_vm::launchd::install(&paths)?;
                let mut out = std::io::stdout().lock();
                writeln!(
                    out,
                    "launchd holds Docker's socket: {}",
                    printable(&paths.docker_socket().to_string_lossy())
                )?;
                writeln!(
                    out,
                    "(its agent {}: {})",
                    installed.label,
                    printable(&installed.plist.to_string_lossy())
                )?;
                Ok(ExitCode::SUCCESS)
            }
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
                    for line in super::port_lines(&ports) {
                        writeln!(out, "{line}")?;
                    }
                }
                Ok(None) => {}
                Err(error) => writeln!(out, "ports: {}", printable(&error.to_string()))?,
            }
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
        if shipyard_vm::launchd::loaded(paths) {
            writeln!(out, "docker: {docker} (launchd's: any time)")?;
        } else {
            writeln!(
                out,
                "docker: {docker} (while the VM runs: `shipyard vm install` for any time)"
            )?;
        }
        writeln!(out, "files: {}", printable(&paths.dir.to_string_lossy()))?;
        Ok(ExitCode::SUCCESS)
    }
}

/// What `vm status` says of the guest's ports on this Mac: those forwarded
/// (its loopback's only), those that can't be and why, and the UDP ones,
/// which aren't.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "a Mac's; tested everywhere")
)]
fn port_lines(ports: &shipyard_vm::ports::PortsFile) -> Vec<String> {
    let mut lines = Vec::new();
    if !ports.forwarded.is_empty() {
        let forwarded: Vec<String> = ports.forwarded.iter().map(ToString::to_string).collect();
        lines.push(format!("ports: {}", forwarded.join(", ")));
    }
    for (address, why) in &ports.unbound {
        lines.push(format!(
            "not forwarded: {address} ({})",
            crate::output::printable(&why.to_string())
        ));
    }
    if ports.udp > 0 {
        lines.push(format!(
            "not forwarded: {} UDP port{} (only TCP is, for now)",
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
    /// why, the UDP ones counted; nothing when there's none.
    #[test]
    fn ports_are_said_with_what_isnt_forwarded() {
        use shipyard_vm::ports::{PortsFile, Unbound};
        let file = PortsFile::new(
            vec![
                "127.0.0.1:8080".parse().unwrap(),
                "[::1]:8080".parse().unwrap(),
            ],
            vec![("127.0.0.1:5432".parse().unwrap(), Unbound::InUse)],
            2,
            std::time::Duration::from_secs(300),
        );
        assert_eq!(
            port_lines(&file),
            [
                "ports: 127.0.0.1:8080, [::1]:8080",
                "not forwarded: 127.0.0.1:5432 (in use on this Mac)",
                "not forwarded: 2 UDP ports (only TCP is, for now)",
            ]
        );
        assert!(
            port_lines(&PortsFile::new(
                Vec::new(),
                Vec::new(),
                0,
                std::time::Duration::from_secs(300)
            ))
            .is_empty()
        );
    }
}
