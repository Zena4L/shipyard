//! `shipyard network …`.

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::Result;
use shipyard_engine::network;
use shipyard_runtime::process::EXIT_ENGINE_ERROR;

use crate::cli::NetworkCommand;
use crate::output::{confirm, each, table};
use crate::reporter::{engine, report};

/// `shipyard network …`: create, list, inspect, remove, prune networks.
pub(crate) fn command(command: &NetworkCommand) -> Result<ExitCode> {
    let engine = engine();
    match command {
        NetworkCommand::Create {
            subnet,
            gateway,
            ipv6,
            internal,
            isolate_host,
            conntrack_limit,
            name,
        } => {
            let mut options = network::SpecOptions::default();
            options.ipv6 = *ipv6;
            options.internal = *internal;
            options.isolate_host = *isolate_host;
            options.conntrack_limit = *conntrack_limit;
            let config = network::create(&engine, &network::spec(name, subnet, gateway, options)?)?;
            writeln!(std::io::stdout().lock(), "{}", config.name)?;
        }
        NetworkCommand::Ls => {
            let rows: Vec<Vec<String>> = network::list(&engine, &[])?
                .into_iter()
                .map(|listed| {
                    vec![
                        listed.config.name,
                        listed.config.subnet.to_string(),
                        listed
                            .config
                            .subnet6
                            .map_or_else(|| "-".to_owned(), |subnet6| subnet6.to_string()),
                        listed.config.gateway.to_string(),
                        listed.config.bridge,
                        listed.leases.len().to_string(),
                    ]
                })
                .collect();
            table(
                &mut std::io::stdout().lock(),
                &[
                    "NETWORK",
                    "SUBNET",
                    "SUBNET6",
                    "GATEWAY",
                    "BRIDGE",
                    "CONTAINERS",
                ],
                &rows,
            )?;
        }
        NetworkCommand::Inspect { names } => {
            let networks = network::inspect(&engine, names)?;
            let mut out = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut out, &networks)?;
            writeln!(out)?;
        }
        NetworkCommand::Rm { names } => {
            return Ok(each(names, |name| {
                network::remove(&engine, name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        NetworkCommand::Prune { force } => return prune(*force),
        NetworkCommand::Connect {
            ip,
            ip6,
            aliases,
            network,
            container,
        } => {
            let mut request = shipyard_engine::run::ConnectRequest::new(network, container);
            request.ip = ip.map(std::net::IpAddr::V4);
            request.ip6 = *ip6;
            request.aliases.clone_from(aliases);
            shipyard_engine::run::connect(&engine, &request)?;
        }
        NetworkCommand::Disconnect {
            force,
            network,
            container,
        } => shipyard_engine::run::disconnect(&engine, network, container, *force)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard network prune`: asks first (on a terminal; elsewhere `-f` is
/// needed), then removes every network no container uses but the default
/// one.
fn prune(force: bool) -> Result<ExitCode> {
    const QUESTION: &str = "Remove every network no container uses (but the default one)?";
    if !force {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            anyhow::bail!(
                "network prune removes networks: run it on a terminal to be asked, or give -f"
            );
        }
        if !confirm(QUESTION, &mut stdin.lock(), &mut std::io::stderr().lock())? {
            writeln!(std::io::stderr().lock(), "Nothing removed")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let pruned = network::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &pruned.removed {
        writeln!(out, "{name}")?;
    }
    writeln!(out, "Removed {} network(s)", pruned.removed.len())?;
    if !pruned.in_use.is_empty() {
        writeln!(
            out,
            "Kept {} in use: {}",
            pruned.in_use.len(),
            pruned.in_use.join(", ")
        )?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for error in &pruned.failed {
        code = report(error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}
