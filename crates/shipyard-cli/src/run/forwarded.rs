//! What a run's arguments ask of the Mac when they're forwarded to its VM
//! (step 10.3b): their input, the host paths they name, the variables they
//! copy from the shell.

use shipyard_engine::run::SecurityOpt;

use super::{CreateCommand, RunArgs, RunCommand};
use crate::forward::{HostPath, Needs};

impl RunArgs {
    /// The host paths these arguments name: bind sources (`-v`'s and
    /// `--mount`'s), the root
    /// filesystem, the bundle, a seccomp profile.
    fn host_paths(&self) -> Vec<HostPath> {
        let (volumes, _) = self.mounts();
        let binds = volumes
            .into_iter()
            .filter_map(|spec| shipyard_storage::volume::VolumeMount::parse(&spec).ok())
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
