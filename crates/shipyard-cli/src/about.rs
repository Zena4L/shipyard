//! `shipyard version` and `shipyard info`: what Shipyard is, and what it
//! has, as Docker's commands print them (their text, or `--format` over
//! their objects). The objects are the engine's (`docker::system_version`,
//! `docker::system_info`), as the API's `/version` and `/info` serve them;
//! the client's part is this binary's. On a Mac both run in the VM's
//! guest (D11.3c2-1).

use std::fmt::Write as _;
use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use shipyard_engine::docker::{self, API_VERSION, Build, Info, SystemVersion};

use crate::format::{Output, Row};
use crate::output::printable;

/// What this binary says of itself, Docker's `ClientVersion`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ClientVersion {
    version: String,
    api_version: &'static str,
    #[serde(rename = "DefaultAPIVersion")]
    default_api_version: &'static str,
    git_commit: String,
    go_version: &'static str,
    os: &'static str,
    arch: &'static str,
    build_time: &'static str,
    context: &'static str,
}

/// `version`'s object: the client and the server.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Versions {
    client: ClientVersion,
    server: SystemVersion,
}

impl Row for Versions {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn id(&self) -> &'static str {
        ""
    }

    /// Docker's Go field names: its JSON's `ApiVersion` is `.APIVersion`
    /// to a template (`{{.Server.APIVersion}}`), so both are there.
    fn fields(&self) -> gotmpl::Result<gotmpl::Value> {
        let both = match gotmpl::to_value(self)? {
            gotmpl::Value::Map(both) => both,
            other => return Ok(other),
        };
        let aliased = both
            .iter()
            .map(|(side, part)| {
                let part = match part {
                    gotmpl::Value::Map(fields) => {
                        let mut fields = (**fields).clone();
                        if let Some(api) = fields.get("ApiVersion").cloned() {
                            fields.insert(Arc::from("APIVersion"), api);
                        }
                        gotmpl::Value::Map(Arc::new(fields))
                    }
                    other => other.clone(),
                };
                (Arc::clone(side), part)
            })
            .collect();
        Ok(gotmpl::Value::Map(Arc::new(aliased)))
    }
}

/// `info`'s object, Docker's `SystemInfo` (Shipyard's part of it).
#[derive(Debug, Serialize)]
#[serde(transparent)]
struct InfoRow(Info);

impl Row for InfoRow {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn id(&self) -> &str {
        &self.0.id
    }
}

/// This build, as the engine says it.
fn build(revision: &str) -> Build {
    Build {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        revision: revision.to_owned(),
    }
}

/// `shipyard version [--format]`.
pub(crate) fn version(revision: &str, format: Option<&str>) -> Result<ExitCode> {
    let versions = Versions {
        client: ClientVersion {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            api_version: API_VERSION,
            default_api_version: API_VERSION,
            git_commit: revision.to_owned(),
            go_version: "",
            os: std::env::consts::OS,
            arch: docker::go_arch(),
            build_time: "",
            context: "default",
        },
        server: docker::system_version(&build(revision)),
    };
    let mut out = std::io::stdout().lock();
    // An empty format is none, as Docker's.
    match format.filter(|format| !format.is_empty()) {
        Some(format) => Output::strict(format)?.write(&mut out, &[versions])?,
        None => out.write_all(version_text(&versions).as_bytes())?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `versions` as Docker's `version` prints them.
fn version_text(versions: &Versions) -> String {
    let (client, server) = (&versions.client, &versions.server);
    let mut text = String::new();
    let _ = writeln!(text, "Client:");
    let _ = writeln!(text, " Version:           {}", client.version);
    let _ = writeln!(text, " API version:       {}", client.api_version);
    let _ = writeln!(text, " Git commit:        {}", client.git_commit);
    let _ = writeln!(text, " OS/Arch:           {}/{}", client.os, client.arch);
    let _ = writeln!(text, " Context:           {}", client.context);
    let _ = writeln!(text);
    let _ = writeln!(text, "Server: {}", server.platform.name);
    let _ = writeln!(text, " Engine:");
    let _ = writeln!(text, "  Version:          {}", server.version);
    let _ = writeln!(
        text,
        "  API version:      {} (minimum version {})",
        server.api_version, server.min_api_version
    );
    let _ = writeln!(text, "  Git commit:       {}", server.git_commit);
    let _ = writeln!(text, "  OS/Arch:          {}/{}", server.os, server.arch);
    let _ = writeln!(text, "  Experimental:     {}", server.experimental);
    text
}

/// `shipyard info [--format]`.
pub(crate) fn info(revision: &str, format: Option<&str>) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let info = docker::system_info(&engine, &build(revision))?;
    let mut out = std::io::stdout().lock();
    match format.filter(|format| !format.is_empty()) {
        Some(format) => Output::strict(format)?.write(&mut out, &[InfoRow(info)])?,
        None => out.write_all(info_text(&info).as_bytes())?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `info` as Docker's `info` prints it: what Shipyard has of it.
fn info_text(info: &Info) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "Client:");
    // The client is this binary: on a Linux host the engine's own.
    let _ = writeln!(text, " Version:    {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(text, " Context:    default");
    let _ = writeln!(text, " Debug Mode: {}", info.debug);
    let _ = writeln!(text);
    let _ = writeln!(text, "Server:");
    let _ = writeln!(text, " Containers: {}", info.containers);
    let _ = writeln!(text, "  Running: {}", info.containers_running);
    let _ = writeln!(text, "  Paused: {}", info.containers_paused);
    let _ = writeln!(text, "  Stopped: {}", info.containers_stopped);
    let _ = writeln!(text, " Images: {}", info.images);
    let _ = writeln!(text, " Server Version: {}", info.server_version);
    let _ = writeln!(text, " Storage Driver: {}", info.driver);
    let _ = writeln!(text, " Logging Driver: {}", info.logging_driver);
    let _ = writeln!(text, " Cgroup Driver: {}", info.cgroup_driver);
    let _ = writeln!(text, " Cgroup Version: {}", info.cgroup_version);
    let _ = writeln!(text, " Default Runtime: {}", info.default_runtime);
    let _ = writeln!(text, " Security Options:");
    for option in &info.security_options {
        text.push_str(&security_option(option));
    }
    let _ = writeln!(text, " Kernel Version: {}", printable(&info.kernel_version));
    let _ = writeln!(
        text,
        " Operating System: {}",
        printable(&info.operating_system)
    );
    let _ = writeln!(text, " OSType: {}", info.os_type);
    let _ = writeln!(text, " Architecture: {}", info.architecture);
    let _ = writeln!(text, " CPUs: {}", info.ncpu);
    let _ = writeln!(
        text,
        " Total Memory: {}",
        crate::format::rows::binary_size(info.mem_total)
    );
    let _ = writeln!(text, " Name: {}", printable(&info.name));
    let _ = writeln!(text, " ID: {}", info.id);
    let _ = writeln!(text, " Debug Mode: {}", info.debug);
    let _ = writeln!(text, " Experimental: {}", info.experimental_build);
    let _ = writeln!(text, " Live Restore Enabled: {}", info.live_restore_enabled);
    text
}

/// A security option (`name=seccomp,profile=builtin`) as Docker's `info`
/// lists it: its name, then each setting under it (`Profile: builtin`).
fn security_option(option: &str) -> String {
    let mut text = String::new();
    for (index, setting) in option.split(',').enumerate() {
        let (key, value) = setting.split_once('=').unwrap_or((setting, ""));
        if index == 0 && key == "name" {
            let _ = writeln!(text, "  {value}");
        } else {
            let mut title = key.chars();
            let key: String = title
                .next()
                .map(|first| first.to_uppercase().chain(title).collect())
                .unwrap_or_default();
            let _ = writeln!(text, "   {key}: {value}");
        }
    }
    text
}

#[cfg(test)]
mod tests;
