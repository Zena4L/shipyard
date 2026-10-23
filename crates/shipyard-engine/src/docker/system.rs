//! What Shipyard says of itself and its host, in Docker's shapes: the
//! Engine API's `SystemVersion` (`GET /version`, `shipyard version`) and
//! the part of its `SystemInfo` Shipyard can fill (`GET /info`, `shipyard
//! info`). Moved from the API in step 11.3c2, so the CLI and the API say
//! the same (H1).

use serde::Serialize;

use crate::Engine;
use crate::containers::{self, ContainerState};
use crate::error::Result;

/// The Engine API version served: Docker 24's (D11.2-3).
pub const API_VERSION: &str = "1.43";
/// The oldest version taken, as Docker 24's daemon takes it.
pub const MIN_API_VERSION: &str = "1.24";

/// What a build of Shipyard says of itself.
#[derive(Debug, Clone)]
pub struct Build {
    /// Shipyard's version (`0.3.0`).
    pub version: String,
    /// The revision it was built from.
    pub revision: String,
}

/// Docker's `SystemVersion`, the engine named Shipyard.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct SystemVersion {
    /// `{"Name": "Shipyard"}`.
    pub platform: Platform,
    /// The engine, its one component.
    pub components: Vec<Component>,
    /// Shipyard's version.
    pub version: String,
    /// The API version served.
    pub api_version: &'static str,
    /// The oldest API version taken.
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: &'static str,
    /// The revision it was built from.
    pub git_commit: String,
    /// Empty: there's no Go here (Docker's clients print it).
    pub go_version: &'static str,
    /// `linux`.
    pub os: &'static str,
    /// The architecture as Go names it (`arm64`, `amd64`).
    pub arch: &'static str,
    /// The kernel's release.
    pub kernel_version: String,
    /// Always false: nothing experimental is served.
    pub experimental: bool,
}

/// A version's platform.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Platform {
    /// `Shipyard`.
    pub name: &'static str,
}

/// A version's component (Docker lists the engine, containerd, runc…).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Component {
    /// `Engine`.
    pub name: &'static str,
    /// Its version.
    pub version: String,
    /// The rest of what it says.
    pub details: ComponentDetails,
}

/// A component's details.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ComponentDetails {
    /// The API version served.
    pub api_version: &'static str,
    /// The oldest API version taken.
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: &'static str,
    /// The revision it was built from.
    pub git_commit: String,
    /// `linux`.
    pub os: &'static str,
    /// The architecture as Go names it.
    pub arch: &'static str,
    /// The kernel's release.
    pub kernel_version: String,
}

/// What `build` says of itself, on this host, as Docker's `/version`.
#[must_use]
pub fn system_version(build: &Build) -> SystemVersion {
    let kernel = kernel_version();
    SystemVersion {
        platform: Platform { name: "Shipyard" },
        components: vec![Component {
            name: "Engine",
            version: build.version.clone(),
            details: ComponentDetails {
                api_version: API_VERSION,
                min_api_version: MIN_API_VERSION,
                git_commit: build.revision.clone(),
                os: "linux",
                arch: go_arch(),
                kernel_version: kernel.clone(),
            },
        }],
        version: build.version.clone(),
        api_version: API_VERSION,
        min_api_version: MIN_API_VERSION,
        git_commit: build.revision.clone(),
        go_version: "",
        os: "linux",
        arch: go_arch(),
        kernel_version: kernel,
        experimental: false,
    }
}

/// The part of Docker's `SystemInfo` Shipyard can fill.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Docker's shape: its yes-or-no settings"
)]
pub struct Info {
    /// Empty: Shipyard keeps no engine ID.
    #[serde(rename = "ID")]
    pub id: String,
    /// Every container.
    pub containers: usize,
    /// Those running (stopping and waiting to restart too, as Docker's).
    pub containers_running: usize,
    /// Those paused.
    pub containers_paused: usize,
    /// Those made or ended.
    pub containers_stopped: usize,
    /// The images in the store (by manifest, as `system df` counts them).
    pub images: usize,
    /// `overlay`.
    pub driver: &'static str,
    /// `json-file`.
    pub logging_driver: &'static str,
    /// `cgroupfs`.
    pub cgroup_driver: &'static str,
    /// `2`.
    pub cgroup_version: &'static str,
    /// The kernel's release.
    pub kernel_version: String,
    /// The distribution (`os-release`'s `PRETTY_NAME`).
    pub operating_system: String,
    /// `linux`.
    #[serde(rename = "OSType")]
    pub os_type: &'static str,
    /// The architecture as the kernel names it (`aarch64`).
    pub architecture: &'static str,
    /// The CPUs this process may use.
    #[serde(rename = "NCPU")]
    pub ncpu: usize,
    /// The host's memory, in bytes.
    pub mem_total: u64,
    /// The host's name.
    pub name: String,
    /// Shipyard's version.
    pub server_version: String,
    /// `shipyard-runtime`.
    pub default_runtime: &'static str,
    /// `seccomp`'s profile, `cgroupns`, and `rootless` when it is.
    pub security_options: Vec<&'static str>,
    /// Always false.
    pub experimental_build: bool,
    /// Always false: a container's end is its monitor's, whatever runs.
    pub live_restore_enabled: bool,
    /// Always false.
    pub debug: bool,
    /// None.
    pub warnings: Option<Vec<String>>,
}

/// What `engine`'s stores and this host say, as Docker's `/info`, for
/// `build`.
///
/// # Errors
///
/// The container or image store can't be read.
pub fn system_info(engine: &Engine, build: &Build) -> Result<Info> {
    let summaries = containers::summaries(engine, true)?;
    let images = crate::image::count_images(engine)?;
    let count = |wanted: fn(&ContainerState) -> bool| {
        summaries
            .iter()
            .filter(|summary| wanted(&summary.state))
            .count()
    };
    let mut security_options = vec!["name=seccomp,profile=builtin", "name=cgroupns"];
    if !nix::unistd::geteuid().is_root() {
        security_options.push("name=rootless");
    }
    Ok(Info {
        id: String::new(),
        containers: summaries.len(),
        containers_running: count(|state| {
            matches!(
                state,
                ContainerState::Running { .. }
                    | ContainerState::Stopping
                    | ContainerState::Restarting { .. }
            )
        }),
        containers_paused: count(|state| *state == ContainerState::Paused),
        containers_stopped: count(|state| {
            matches!(
                state,
                ContainerState::Created | ContainerState::Exited { .. }
            )
        }),
        images,
        driver: "overlay",
        logging_driver: "json-file",
        cgroup_driver: "cgroupfs",
        cgroup_version: "2",
        kernel_version: kernel_version(),
        operating_system: operating_system(),
        os_type: "linux",
        architecture: std::env::consts::ARCH,
        ncpu: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        mem_total: mem_total(),
        name: nix::unistd::gethostname()
            .ok()
            .and_then(|name| name.into_string().ok())
            .unwrap_or_default(),
        server_version: build.version.clone(),
        default_runtime: "shipyard-runtime",
        security_options,
        experimental_build: false,
        live_restore_enabled: false,
        debug: false,
        warnings: None,
    })
}

/// The architecture as Go names it (Docker's clients compare it with
/// theirs): `amd64`, `arm64`.
#[must_use]
pub fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The kernel's release (`uname -r`), if it can be read.
fn kernel_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_owned())
        .unwrap_or_default()
}

/// The distribution's name (`os-release`'s `PRETTY_NAME`), if it has one.
fn operating_system() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|name| name.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_default()
}

/// The host's memory in bytes (`/proc/meminfo`'s `MemTotal`), 0 if it
/// can't be read.
pub(crate) fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let kib = line.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?;
                kib.trim().parse::<u64>().ok()?.checked_mul(1024)
            })
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architectures_are_go_s_names() {
        let arch = go_arch();
        assert!(!arch.is_empty());
        if cfg!(target_arch = "aarch64") {
            assert_eq!(arch, "arm64");
        }
        if cfg!(target_arch = "x86_64") {
            assert_eq!(arch, "amd64");
        }
    }

    /// The version's keys are Docker 29.8's `Server` part's (2026-10-20,
    /// `docker version --format json`), but `BuildTime`, which Shipyard
    /// doesn't keep.
    #[test]
    fn a_version_is_docker_s() {
        let build = Build {
            version: "0.3.0".to_owned(),
            revision: "abc".to_owned(),
        };
        let shown = serde_json::to_value(system_version(&build)).unwrap();
        let mut keys: Vec<&str> = shown
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "ApiVersion",
                "Arch",
                "Components",
                "Experimental",
                "GitCommit",
                "GoVersion",
                "KernelVersion",
                "MinAPIVersion",
                "Os",
                "Platform",
                "Version"
            ]
        );
        assert_eq!(shown["ApiVersion"], API_VERSION);
        assert_eq!(shown["Components"][0]["Name"], "Engine");
    }
}
