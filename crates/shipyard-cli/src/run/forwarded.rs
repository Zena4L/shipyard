//! What a run's arguments ask of the Mac when they're forwarded to its VM
//! (step 10.3b): their input, the host paths they name, the variables they
//! copy from the shell.

use shipyard_engine::run::SecurityOpt;

use super::{CreateCommand, RunArgs, RunCommand};
use crate::forward::{HostPath, Needs};

impl RunArgs {
    /// The host paths these arguments name: bind sources, the root
    /// filesystem, the bundle, a seccomp profile.
    fn host_paths(&self) -> Vec<HostPath> {
        let binds = self
            .volume
            .iter()
            .filter_map(|spec| shipyard_storage::volume::VolumeMount::parse(spec).ok())
            .filter(|mount| !shipyard_storage::volume::is_named_volume(&mount.source))
            .map(|mount| HostPath::existing(mount.source));
        let profiles = self.security_opt.iter().filter_map(|option| match option {
            SecurityOpt::SeccompProfile(path) => Some(HostPath::existing(path)),
            _ => None,
        });
        binds
            .chain(profiles)
            .chain(self.rootfs.iter().map(HostPath::existing))
            .chain(self.bundle.iter().map(HostPath::existing))
            .collect()
    }

    /// The variables a bare `-e KEY` copies from the shell (a key that
    /// isn't text is the guest's to refuse: it's sent as given).
    fn copied_env(&self) -> Vec<String> {
        self.bare_env_keys()
            .filter_map(|key| key.to_str().map(str::to_owned))
            .collect()
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
