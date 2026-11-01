//! Turning `run`'s and `create`'s flags into the engine's [`RunRequest`].

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::time::Duration;

use anyhow::Result;
use shipyard_engine::run::{Limits, RunRequest, RunStreams};

use super::RunArgs;

impl RunArgs {
    /// The image these arguments name, and the command: the first argument
    /// is the image unless a flag says there's none (`--no-image`,
    /// `--rootfs`, `--bundle`, `--no-isolation`) or gives it (`--image`).
    /// Then the rules that need an image or a root filesystem.
    ///
    /// # Errors
    ///
    /// A usage error (printed as clap's, exit 125): no image where one is needed, or a
    /// flag that needs one (or a root filesystem, or a command) without it.
    pub(super) fn image_and_command(
        &mut self,
    ) -> Result<(Option<String>, Vec<OsString>), clap::Error> {
        use clap::error::ErrorKind;
        let usage = |kind, message: &str| clap::Error::raw(kind, message.to_owned());
        let args = std::mem::take(&mut self.args);
        let without_image =
            self.no_image || self.rootfs.is_some() || self.bundle.is_some() || self.no_isolation;
        let (image, command) = if let Some(image) = self.image.take() {
            (Some(image), args)
        } else if without_image {
            (None, args)
        } else {
            let mut args = args.into_iter();
            let Some(image) = args.next() else {
                return Err(usage(
                    ErrorKind::MissingRequiredArgument,
                    "an IMAGE is needed (or --no-image, --rootfs, --bundle or --no-isolation to run \
                     without one)",
                ));
            };
            let image = image.into_string().map_err(|image| {
                usage(
                    ErrorKind::InvalidValue,
                    &format!("the image {} isn't UTF-8", image.display()),
                )
            })?;
            (Some(image), args.collect())
        };
        let needs_image = |flag: &str| {
            usage(
                ErrorKind::MissingRequiredArgument,
                &format!("{flag} needs an image"),
            )
        };
        if image.is_none() {
            if self.publish_all {
                return Err(needs_image("--publish-all"));
            }
            if self.entrypoint.is_some() {
                return Err(needs_image("--entrypoint"));
            }
            if self.pull.is_some() {
                return Err(needs_image("--pull"));
            }
            if self.read_only && self.rootfs.is_none() {
                return Err(usage(
                    ErrorKind::MissingRequiredArgument,
                    "--read-only needs an image or --rootfs",
                ));
            }
        }
        if self.bundle.is_some() && !command.is_empty() {
            return Err(usage(
                ErrorKind::ArgumentConflict,
                "--bundle takes no COMMAND: its config.json says what to run",
            ));
        }
        if image.is_none() && self.bundle.is_none() && command.is_empty() {
            return Err(usage(
                ErrorKind::MissingRequiredArgument,
                "a COMMAND is needed without an image",
            ));
        }
        Ok((image, command))
    }

    /// The run these flags ask for, as the engine takes it.
    ///
    /// # Errors
    ///
    /// A usage error: see [`image_and_command`](Self::image_and_command).
    pub(super) fn into_request(mut self) -> Result<RunRequest, clap::Error> {
        self.refuse_what_is_not_supported()?;
        let (image, command) = self.image_and_command()?;
        // The files' variables first: `--env` wins (Docker's order).
        let mut env = Vec::new();
        for path in &self.env_file {
            env.extend(super::files::env_file(path).map_err(|error| file_error(&error))?);
        }
        env.append(&mut self.env);
        self.env = env;
        let labels = self.labels()?;
        // A bare `--env KEY` copies the caller's value: this process's
        // environment is the caller's, read here, never by the engine; in
        // the macOS guest, the Mac's, as the agent passed them on.
        let caller_env = caller_env_of(&self.env);
        let limits = Limits {
            first_given: self.limits.first_given(),
            pids_given: self.limits.pids_given(),
            resources: self
                .limits
                .resources()
                .map_err(|error| format!("{error:#}")),
        };
        let health = self.health()?;
        let mut request = RunRequest::default();
        // Before the fields move out.
        let (volume, tmpfs) = self.mounts();
        request.streams = RunStreams {
            interactive: self.streams.interactive,
            tty: self.streams.tty,
            once: false,
        };
        request.env = self.env;
        request.caller_env = caller_env;
        request.workdir = self.workdir;
        request.user = self.user;
        request.group_add = self.group_add;
        request.ulimit = self.ulimit;
        request.oom_score_adj = self.oom_score_adj;
        request.device = self.device;
        request.device_cgroup_rule = self.device_cgroup_rule;
        request.stop_signal = self.stop_signal;
        request.stop_timeout = self.stop_timeout;
        request.restart = self.restart;
        request.health = health;
        request.no_isolation = self.no_isolation;
        request.hostname = self.hostname;
        request.domainname = self.domainname;
        request.sysctl = self.sysctl;
        request.network = self.network;
        request.ipc = self.ipc.unwrap_or_default();
        request.pid = self.pid.unwrap_or_default();
        request.name = self.name;
        request.network_alias = self.network_alias;
        request.ip = self.ip;
        request.ip6 = self.ip6;
        request.mac_address = self.mac_address;
        request.add_host = self.add_host;
        request.dns = self.dns;
        request.dns_search = self.dns_search;
        request.dns_option = self.dns_option;
        request.publish = self.publish.into_iter().flat_map(|spec| spec.0).collect();
        request.publish_all = self.publish_all;
        request.expose = self.expose.into_iter().flat_map(|spec| spec.0).collect();
        request.labels = labels;
        request.cidfile = self.cidfile;
        request.rootfs = self.rootfs;
        request.image = image;
        request.entrypoint = self.entrypoint;
        request.pull = self.pull.map(Into::into);
        request.platform = self
            .platform
            .as_deref()
            .map(|asked| shipyard_engine::image::platform_here(asked, "run"))
            .transpose()
            .map_err(|error| {
                clap::Error::raw(
                    clap::error::ErrorKind::ValueValidation,
                    format!("{error}\n"),
                )
            })?;
        request.read_only = self.read_only;
        request.cap_add = self.cap_add;
        request.cap_drop = self.cap_drop;
        request.security_opt = self.security_opt;
        request.uidmap = self.uidmap;
        request.gidmap = self.gidmap;
        request.bundle = self.bundle;
        request.limits = limits;
        request.volume = volume;
        request.tmpfs = tmpfs;
        request.command = command;
        Ok(request)
    }
}

impl RunArgs {
    /// Every bind and volume (`-v`'s, then `--mount`'s, as `-v` strings),
    /// and every tmpfs (`--tmpfs`'s, then `--mount`'s).
    pub(super) fn mounts(&self) -> (Vec<String>, Vec<shipyard_engine::run::TmpfsMount>) {
        let mut volume = self.volume.clone();
        let mut tmpfs = self.tmpfs.clone();
        for mount in &self.mount {
            match mount {
                shipyard_engine::run::DockerMount::Volume(spec) => volume.push(spec.clone()),
                shipyard_engine::run::DockerMount::Tmpfs(mount) => tmpfs.push(mount.clone()),
            }
        }
        (volume, tmpfs)
    }

    /// `--privileged` and `--init` refused, saying what to use instead
    /// (D11.3-2), as the API refuses them.
    fn refuse_what_is_not_supported(&self) -> Result<(), clap::Error> {
        let refused = |message: &str| {
            Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                format!("{message}\n"),
            ))
        };
        if self.privileged {
            return refused(
                "--privileged isn't supported: give the container the capabilities (--cap-add) \
                 and devices (--device) it needs",
            );
        }
        if self.init {
            return refused("--init isn't supported yet: run the command without it");
        }
        Ok(())
    }

    /// The labels asked: each `--label-file`'s, then each `--label`
    /// (the later of a key wins).
    fn labels(&self) -> Result<std::collections::BTreeMap<String, String>, clap::Error> {
        let mut labels = std::collections::BTreeMap::new();
        for path in &self.label_file {
            labels.extend(super::files::label_file(path).map_err(|error| file_error(&error))?);
        }
        for label in &self.labels {
            let (key, value) = label.split_once('=').unwrap_or((label, ""));
            labels.insert(key.to_owned(), value.to_owned());
        }
        Ok(labels)
    }

    /// The health check asked: `--no-healthcheck`'s none; `--health-cmd`'s
    /// (tuned by the other flags); the other flags alone, tuning the
    /// image's own (an empty test); or nothing asked. A duration under a
    /// millisecond (not zero) is refused, as Docker refuses it.
    fn health(&self) -> Result<Option<shipyard_engine::health::HealthCheck>, clap::Error> {
        use shipyard_engine::health::{HealthCheck, flag_millis};
        if self.no_healthcheck {
            return Ok(Some(HealthCheck::disabled()));
        }
        let tuned = self.health_interval.is_some()
            || self.health_timeout.is_some()
            || self.health_start_period.is_some()
            || self.health_start_interval.is_some()
            || self.health_retries.is_some();
        if self.health_cmd.is_none() && !tuned {
            return Ok(None);
        }
        let millis = |flag: &str, given: Option<Duration>| {
            flag_millis(flag, given).map_err(|error| {
                clap::Error::raw(
                    clap::error::ErrorKind::ValueValidation,
                    format!("{error}\n"),
                )
            })
        };
        Ok(Some(HealthCheck {
            test: self
                .health_cmd
                .as_deref()
                .map(|script| HealthCheck::shell(script).test)
                .unwrap_or_default(),
            interval_ms: millis("--health-interval", self.health_interval)?,
            timeout_ms: millis("--health-timeout", self.health_timeout)?,
            start_period_ms: millis("--health-start-period", self.health_start_period)?,
            start_interval_ms: millis("--health-start-interval", self.health_start_interval)?,
            retries: self.health_retries.unwrap_or(0),
            shell: Vec::new(),
        }))
    }
}

/// An env or label file that can't be taken, as a usage error (exit 125,
/// as Docker's).
fn file_error(error: &super::files::FileError) -> clap::Error {
    clap::Error::raw(clap::error::ErrorKind::Io, format!("{error}\n"))
}

/// The bare `-e KEY`s of `env`: the variables copied from the caller's
/// shell (`run`'s, `create`'s and `exec`'s alike).
pub(crate) fn bare_env_keys(env: &[OsString]) -> impl Iterator<Item = &OsString> {
    env.iter().filter(|entry| !entry.as_bytes().contains(&b'='))
}

/// The bare `KEY`s of `env` that are text, as a forwarded command names
/// them to the Mac (a key that isn't text is the guest's to refuse).
pub(crate) fn copied_env(env: &[OsString]) -> Vec<String> {
    bare_env_keys(env)
        .filter_map(|key| key.to_str().map(str::to_owned))
        .collect()
}

/// The caller's values for `env`'s bare `KEY`s (those it has), as the
/// engine takes them: read here, never by the engine. In the macOS guest,
/// the Mac's, as its agent passed them on.
pub(crate) fn caller_env_of(env: &[OsString]) -> Vec<(OsString, OsString)> {
    let forwarded = std::env::var(shipyard_runtime::attach::forward::CALLER_ENV).ok();
    bare_env_keys(env)
        .filter_map(|key| Some((key.clone(), caller_var(forwarded.as_deref(), key)?)))
        .collect()
}

/// The caller's value of `key`: from `forwarded` (the Mac's, a forwarded
/// command's [`CALLER_ENV`](shipyard_runtime::attach::forward::CALLER_ENV))
/// if there is one, else this process's environment.
pub(super) fn caller_var(forwarded: Option<&str>, key: &OsString) -> Option<OsString> {
    match forwarded {
        Some(forwarded) => {
            shipyard_runtime::attach::forward::caller_var(forwarded, key.to_str()?).map(Into::into)
        }
        None => std::env::var_os(key),
    }
}
