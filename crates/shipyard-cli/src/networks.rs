//! `shipyard network …`.

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::Result;
use shipyard_engine::network;

use crate::cli::NetworkCommand;
use crate::format::rows::{self, NetworkRow};
use crate::output::{confirm, each};
use crate::reporter::{EXIT_FAILURE, engine, report};

/// `shipyard network …`: create, list, inspect, remove, prune networks.
pub(crate) fn command(command: &NetworkCommand) -> Result<ExitCode> {
    let engine = engine();
    match command {
        NetworkCommand::Create(args) => create(&engine, args)?,
        NetworkCommand::Ls(args) => {
            use shipyard_engine::docker::filters::{Given, NetworkFilters};
            let filters =
                NetworkFilters::parse(Given::from_pairs(args.list.filter.iter().cloned()))?;
            let networks = shipyard_engine::docker::list_networks_where(&engine, &filters)?;
            let rows: Vec<NetworkRow> = networks
                .iter()
                .map(|network| NetworkRow::new(network, args.no_trunc))
                .collect();
            crate::format::print(&args.list, rows::NETWORKS_TABLE, &rows)?;
        }
        NetworkCommand::Inspect {
            shipyard: true,
            names,
            ..
        } => {
            return crate::output::shown(names, None, |name| {
                let found = network::inspect(&engine, &[name.to_owned()])?;
                Ok(serde_json::to_value(found.first())?)
            });
        }
        NetworkCommand::Inspect { names, shown, .. } => {
            return crate::output::shown(names, shown.format.as_deref(), |name| {
                Ok(serde_json::to_value(
                    shipyard_engine::docker::inspect_network(&engine, name)?,
                )?)
            });
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
        code = report(error, EXIT_FAILURE);
    }
    Ok(code)
}

/// `shipyard network create`: its flags as Docker's create request
/// (`--isolate-host`, `--conntrack-limit` as the options they are),
/// checked and made as the API's (`docker::create_network`); its name
/// printed.
fn create(engine: &shipyard_engine::Engine, args: &crate::cli::NetworkCreateArgs) -> Result<()> {
    let crate::cli::NetworkCreateArgs {
        subnet,
        gateway,
        ipv6,
        internal,
        isolate_host,
        conntrack_limit,
        labels,
        driver,
        options,
        name,
    } = args;
    shipyard_engine::docker::check_network_driver(Some(driver))?;
    // Its flags as the options they are: checked, and kept, as the
    // API's (`docker::create_network`).
    let mut options: std::collections::BTreeMap<String, String> = options.iter().cloned().collect();
    for (on, key) in [
        (*isolate_host, shipyard_engine::docker::ISOLATE_HOST),
        (*conntrack_limit, shipyard_engine::docker::CONNTRACK_LIMIT),
    ] {
        if on {
            options.insert(key.to_owned(), "true".to_owned());
        }
    }
    let request = shipyard_engine::docker::NetworkCreate {
        name: name.clone(),
        internal: *internal,
        ipv6: *ipv6,
        subnets: subnet.clone(),
        gateways: gateway.clone(),
        labels: labels.iter().cloned().collect(),
        options,
    };
    shipyard_engine::docker::create_network(engine, &request)?;
    writeln!(std::io::stdout().lock(), "{name}")?;
    Ok(())
}
