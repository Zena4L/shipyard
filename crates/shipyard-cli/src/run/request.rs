//! Turning `run`'s and `create`'s flags into the engine's [`RunRequest`].

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::time::Duration;

use anyhow::Result;
use shipyard_engine::run::{Limits, RunRequest, RunStreams};

use super::RunArgs;

impl RunArgs {
    /// The keys of a bare `-e KEY`: the variables copied from the caller's
    /// shell (its value then, if it has one).
    pub(super) fn bare_env_keys(&self) -> impl Iterator<Item = &OsString> {
        self.env
            .iter()
            .filter(|entry| !entry.as_bytes().contains(&b'='))
    }

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
        let (image, command) = self.image_and_command()?;
        // A bare `--env KEY` copies the caller's value: this process's
        // environment is the caller's, read here, never by the engine.
        let caller_env = self
            .bare_env_keys()
            .filter_map(|key| Some((key.clone(), std::env::var_os(key)?)))
            .collect();
        let limits = Limits {
            first_given: self.limits.first_given(),
            resources: self
                .limits
                .resources()
                .map_err(|error| format!("{error:#}")),
        };
        let health = self.health()?;
        let mut request = RunRequest::default();
        request.streams = RunStreams {
            interactive: self.streams.interactive,
            tty: self.streams.tty,
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
        request.rootfs = self.rootfs;
        request.image = image;
        request.entrypoint = self.entrypoint;
        request.pull = self.pull.map(Into::into);
        request.read_only = self.read_only;
        request.cap_add = self.cap_add;
        request.cap_drop = self.cap_drop;
        request.security_opt = self.security_opt;
        request.uidmap = self.uidmap;
        request.gidmap = self.gidmap;
        request.bundle = self.bundle;
        request.limits = limits;
        request.volume = self.volume;
        request.command = command;
        Ok(request)
    }
}

impl RunArgs {
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
