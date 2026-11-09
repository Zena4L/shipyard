//! What a run's arguments ask of the Mac when they're forwarded to its VM
//! (step 10.3b): their input, the host paths they name, the variables they
//! copy from the shell.

use shipyard_engine::run::SecurityOpt;

use super::{CreateCommand, RunArgs, RunCommand};
use crate::forward::{HostPath, Needs};

/// Whether `source` names Docker's own socket (`/var/run/docker.sock`,
/// `/run/docker.sock`, or this Mac's `DOCKER_HOST`, `docker_host`): the VM
/// gives a bind of it its own socket (F171), so it's never judged as one of
/// the Mac's paths, which would refuse it (`/var/run` isn't shared).
/// Compared by their parts, as the guest compares them (`/var/run/./docker.sock`
/// is `/var/run/docker.sock`).
fn names_dockers_socket(source: &str, docker_host: Option<&std::path::Path>) -> bool {
    let same = |named: &std::path::Path| {
        std::path::Path::new(source)
            .components()
            .eq(named.components())
    };
    shipyard_runtime::attach::forward::DOCKER_SOCKET_NAMES
        .iter()
        .any(|named| same(std::path::Path::new(named)))
        || docker_host.is_some_and(same)
}

/// This Mac's `DOCKER_HOST` socket: the VM's `docker.sock`.
fn docker_host() -> Option<std::path::PathBuf> {
    shipyard_vm::VmPaths::from_env()
        .ok()
        .map(|paths| paths.docker_socket())
}

impl RunArgs {
    /// The host paths these arguments name: bind sources (`-v`'s and
    /// `--mount`'s), the root
    /// filesystem, the bundle, a seccomp profile.
    fn host_paths(&self) -> Vec<HostPath> {
        let (volumes, _) = self.mounts();
        let docker_host = docker_host();
        let binds = volumes
            .into_iter()
            .filter_map(|spec| shipyard_storage::volume::VolumeMount::parse(&spec).ok())
            .filter(|mount| !shipyard_storage::volume::is_named_volume(&mount.source))
            .filter(|mount| !names_dockers_socket(&mount.source, docker_host.as_deref()))
            .map(|mount| HostPath::existing(mount.source));
        let profiles = self.security_opt.iter().filter_map(|option| match option {
            SecurityOpt::SeccompProfile(path) => Some(HostPath::existing(path)),
            _ => None,
        });
        binds
            .chain(profiles)
            .chain(self.rootfs.iter().map(HostPath::existing))
            .chain(self.bundle.iter().map(HostPath::existing))
            .chain(self.env_file.iter().map(HostPath::existing))
            .chain(self.label_file.iter().map(HostPath::existing))
            .chain(self.cidfile.iter().map(HostPath::new_or_existing))
            .collect()
    }

    /// The variables a bare `-e KEY` (or an env file's bare `KEY`) copies
    /// from the shell (a key that isn't text is the guest's to refuse: it's
    /// sent as given). A file that can't be read here is the guest's to
    /// refuse too (one read here but not there loses its bare keys: the
    /// guest says why it can't read it).
    fn copied_env(&self) -> Vec<String> {
        let from_files: Vec<std::ffi::OsString> = self
            .env_file
            .iter()
            .filter_map(|path| super::files::env_file(path).ok())
            .flatten()
            .collect();
        let mut copied = super::request::copied_env(&from_files);
        copied.extend(super::request::copied_env(&self.env));
        copied
    }
}

impl RunCommand {
    /// What a forwarded `run` needs: its input with `-i`.
    pub(crate) fn needs(&self) -> Needs {
        Needs {
            input: self.args.streams.interactive,
            paths: self.args.host_paths(),
            copied_env: self.args.copied_env(),
            ..Needs::default()
        }
    }
}

impl CreateCommand {
    /// What a forwarded `create` needs: no input (it runs nothing).
    pub(crate) fn needs(&self) -> Needs {
        Needs {
            input: false,
            paths: self.args.host_paths(),
            copied_env: self.args.copied_env(),
            ..Needs::default()
        }
    }
}

#[cfg(test)]
mod docker_socket_tests {
    use super::*;

    /// Docker's socket by its names, and this Mac's `DOCKER_HOST`, is left
    /// to the VM (F171); any other path is judged as the Mac's.
    #[test]
    fn docker_s_socket_is_left_to_the_vm() {
        let ours =
            std::path::Path::new("/Users/me/Library/Application Support/Shipyard/vm/docker.sock");
        assert!(names_dockers_socket("/var/run/docker.sock", None));
        assert!(names_dockers_socket("/var/run/./docker.sock", None));
        assert!(names_dockers_socket("/run/docker.sock", Some(ours)));
        assert!(names_dockers_socket(
            "/Users/me/Library/Application Support/Shipyard/vm/docker.sock",
            Some(ours)
        ));
        assert!(!names_dockers_socket("/var/run/other.sock", Some(ours)));
        assert!(!names_dockers_socket("/Users/me/docker.sock", Some(ours)));
    }
}
