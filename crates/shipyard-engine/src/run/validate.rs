//! A request checked against itself ([`RunRequest::validate`]): what can't
//! go together, and what's asked without what it needs.

use super::RunRequest;
use crate::error::{Result, bail};

impl RunRequest {
    /// Refuses what can't go together, or what needs something it doesn't
    /// have, as the CLI's flags can't (the CLI's parser refuses them
    /// first): what planning takes for granted, so that nothing asked is
    /// quietly left out (a capability dropped, an image's root, an
    /// isolation).
    ///
    /// - A bundle says everything itself: nothing else may be asked with
    ///   one.
    /// - Without isolation (`no_isolation`), nothing a container alone has:
    ///   its devices, names, settings, network, root, capabilities, security
    ///   options, user namespace, volumes or limits.
    /// - One root filesystem: `rootfs` or `image`.
    /// - What needs another: `entrypoint` and `publish_all` an image;
    ///   `read_only` a root; `gidmap` a `uidmap`; `ip`, `ip6`, `mac_address`
    ///   and `network_alias` a `network`.
    /// - A command, unless an image or a bundle gives one.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest), naming
    /// what's wrong.
    pub fn validate(&self) -> Result<()> {
        let (process, container) = self.asked();
        if self.bundle.is_some() {
            if given(&process).or_else(|| given(&container)).is_some() {
                bail!(
                    InvalidRequest,
                    "a bundle says what to run and how, in its config.json: nothing else can be \
                     asked with one"
                );
            }
            return Ok(());
        }
        if self.no_isolation
            && let Some(what) = given(&container)
        {
            bail!(
                InvalidRequest,
                "{what} needs isolation: it's a container's, and --no-isolation runs the command \
                 on the host"
            );
        }
        if self.rootfs.is_some() && self.image.is_some() {
            bail!(
                InvalidRequest,
                "--rootfs and an image both give the container a root filesystem: give one"
            );
        }
        self.check_needs()
    }

    /// What's asked of the process (and of a run, beside its container),
    /// and what only a container has: each with whether it's asked. Every
    /// field is named here: one added must be placed.
    fn asked(&self) -> (Asked<14>, Asked<25>) {
        let Self {
            default_mode: _,
            detached: _,
            container: _,
            joined: _,
            ipc: _,
            streams: _,
            // A process's: `asked_of_its_process`.
            env: _,
            caller_env: _,
            workdir: _,
            user: _,
            group_add: _,
            ulimit: _,
            oom_score_adj: _,
            device,
            device_cgroup_rule,
            stop_signal: _,
            stop_timeout: _,
            restart: _,
            health: _,
            // Kept by the record, and checked as it keeps them (`Labels`).
            labels: _,
            no_isolation: _,
            hostname,
            domainname,
            sysctl,
            network,
            name,
            network_alias: _,
            ip: _,
            ip6: _,
            mac_address: _,
            add_host,
            dns,
            dns_search,
            dns_option,
            publish,
            publish_all,
            expose,
            cidfile,
            rootfs,
            image,
            entrypoint: _,
            pull: _,
            read_only: _,
            cap_add,
            cap_drop,
            security_opt,
            uidmap,
            gidmap,
            bundle: _,
            limits,
            volume,
            tmpfs,
            make_sources: _, // Made at the create; checked as any bind's source after.
            made_sources: _,
            command: _,
        } = self;
        let container = [
            ("--device", !device.is_empty()),
            ("--device-cgroup-rule", !device_cgroup_rule.is_empty()),
            ("--hostname", hostname.is_some()),
            ("--domainname", domainname.is_some()),
            ("--sysctl", !sysctl.is_empty()),
            ("--network", !network.is_empty()),
            ("--name", name.is_some()),
            ("--add-host", !add_host.is_empty()),
            ("--dns", !dns.is_empty()),
            ("--dns-search", !dns_search.is_empty()),
            ("--dns-option", !dns_option.is_empty()),
            ("--publish", !publish.is_empty()),
            ("--publish-all", *publish_all),
            ("--expose", !expose.is_empty()),
            ("--cidfile", cidfile.is_some()),
            ("--rootfs", rootfs.is_some()),
            ("an image", image.is_some()),
            ("--cap-add", !cap_add.is_empty()),
            ("--cap-drop", !cap_drop.is_empty()),
            ("--security-opt", !security_opt.is_empty()),
            ("--uidmap", !uidmap.is_empty()),
            ("--gidmap", !gidmap.is_empty()),
            ("--volume", !volume.is_empty()),
            ("--tmpfs", !tmpfs.is_empty()),
            ("a resource limit", limits.given()),
        ];
        (self.asked_of_its_process(), container)
    }

    /// What `asked` says of a process's settings.
    fn asked_of_its_process(&self) -> Asked<14> {
        let Self {
            env,
            workdir,
            user,
            group_add,
            ulimit,
            oom_score_adj,
            no_isolation,
            network_alias,
            ip,
            ip6,
            mac_address,
            read_only,
            entrypoint,
            command,
            ..
        } = self;
        [
            ("a command", !command.is_empty()),
            ("--env", !env.is_empty()),
            ("--workdir", workdir.is_some()),
            ("--user", user.is_some()),
            ("--group-add", !group_add.is_empty()),
            ("--ulimit", !ulimit.is_empty()),
            ("--oom-score-adj", oom_score_adj.is_some()),
            ("--no-isolation", *no_isolation),
            ("--network-alias", !network_alias.is_empty()),
            ("--ip", ip.is_some()),
            ("--ip6", ip6.is_some()),
            ("--mac-address", mac_address.is_some()),
            ("--read-only", *read_only),
            ("--entrypoint", entrypoint.is_some()),
        ]
    }

    /// Refuses what's asked without what it needs, and a run without a
    /// command (an image gives one of its own).
    fn check_needs(&self) -> Result<()> {
        let has_image = self.image.is_some();
        let has_network = !self.network.is_empty();
        let needs = [
            (
                "--entrypoint",
                self.entrypoint.is_some(),
                has_image,
                "an image",
            ),
            ("--publish-all", self.publish_all, has_image, "an image"),
            ("--pull", self.pull.is_some(), has_image, "an image"),
            (
                "--read-only",
                self.read_only,
                self.has_root(),
                "an image or --rootfs",
            ),
            (
                "--tmpfs",
                !self.tmpfs.is_empty(),
                self.has_root(),
                "an image or --rootfs",
            ),
            (
                "--gidmap",
                !self.gidmap.is_empty(),
                !self.uidmap.is_empty(),
                "--uidmap",
            ),
            ("--ip", self.ip.is_some(), has_network, "--network"),
            ("--ip6", self.ip6.is_some(), has_network, "--network"),
            (
                "--mac-address",
                self.mac_address.is_some(),
                has_network,
                "--network",
            ),
            (
                "--network-alias",
                !self.network_alias.is_empty(),
                has_network,
                "--network",
            ),
        ];
        if let Some((what, _, _, needed)) = needs.iter().find(|(_, given, has, _)| *given && !*has)
        {
            bail!(InvalidRequest, "{what} needs {needed}");
        }
        if self.command.is_empty() && !has_image {
            bail!(
                InvalidRequest,
                "a command is needed (an image gives one of its own)"
            );
        }
        Ok(())
    }
}

/// What a request asks, by name, each with whether it's asked.
type Asked<const N: usize> = [(&'static str, bool); N];

/// The first of `list` that's asked, by name.
fn given(list: &[(&'static str, bool)]) -> Option<&'static str> {
    list.iter().find(|(_, given)| *given).map(|(what, _)| *what)
}
