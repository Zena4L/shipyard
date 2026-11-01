//! A run's isolation: namespaces, security, its user namespace, devices
//! and limits.

use std::path::Path;

use shipyard_runtime::isolation::{
    HOST_WIDE_NET_SYSCTLS, IdMaps, Isolation, IsolationError, Namespace, Root, RootlessMaps,
};
#[cfg(target_os = "linux")]
use shipyard_runtime::process::User;
use shipyard_runtime::security::seccomp::Profile;
use shipyard_runtime::security::{Capabilities, CapabilitySet, Seccomp, Security};

use super::{CapabilityArg, RunRequest, SecurityOpt};
use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::network::RunId;

impl RunRequest {
    /// The isolation for this run (the standard namespaces on Linux unless
    /// `--no-isolation`; none on macOS, where namespaces don't exist yet:
    /// the VM comes in a later version), and a warning if a rootless
    /// caller's user namespace maps fewer IDs than it could.
    pub(super) fn isolation(
        &self,
        engine: &Engine,
        run: &RunId,
    ) -> Result<(Option<Isolation>, Option<String>)> {
        if self.no_isolation {
            return Ok((None, None));
        }
        if !cfg!(target_os = "linux") {
            for (given, flag) in [
                (self.hostname.is_some(), "--hostname"),
                (self.domainname.is_some(), "--domainname"),
                (!self.sysctl.is_empty(), "--sysctl"),
                (!self.network.is_empty(), "--network"),
                (self.name.is_some(), "--name"),
                (!self.network_alias.is_empty(), "--network-alias"),
                (!self.add_host.is_empty(), "--add-host"),
                (!self.dns.is_empty(), "--dns"),
                (!self.dns_search.is_empty(), "--dns-search"),
                (!self.dns_option.is_empty(), "--dns-option"),
                (!self.publish.is_empty(), "--publish"),
                (self.publish_all, "--publish-all"),
                (self.rootfs.is_some(), "--rootfs"),
                (!self.cap_add.is_empty(), "--cap-add"),
                (!self.cap_drop.is_empty(), "--cap-drop"),
                (!self.security_opt.is_empty(), "--security-opt"),
                (!self.uidmap.is_empty(), "--uidmap"),
                (!self.device.is_empty(), "--device"),
                (!self.device_cgroup_rule.is_empty(), "--device-cgroup-rule"),
                (!self.volume.is_empty(), "--volume"),
            ] {
                if given {
                    bail!(
                        Unsupported,
                        "{flag} needs Linux namespaces, which aren't available on {} yet",
                        std::env::consts::OS
                    );
                }
            }
            if self.limits.given() {
                let flag = self.limits.first_given.unwrap_or("a resource limit");
                bail!(
                    Unsupported,
                    "{flag} needs Linux cgroups, which aren't available on {} yet",
                    std::env::consts::OS
                );
            }
            tracing::debug!("no namespaces on this system; running without isolation");
            return Ok((None, None));
        }
        let mode = self.network_mode(engine)?;
        let mut isolation = Isolation::standard();
        // Its own UTS namespace, with the other's name in it (Docker's).
        isolation.hostname = match &self.joined.network {
            Some(joined) => Some(joined.hostname.clone()),
            None => self.hostname_for(run, &mode),
        };
        isolation.domainname.clone_from(&self.domainname);
        isolation.security = self.security()?;
        isolation.resources = self
            .limits
            .resources
            .clone()
            .map_err(|why| Error::new(ErrorKind::InvalidRequest, why))?;
        // Shipyard refuses to start setuid (`refuse_privileged_start`), so
        // the real user is the effective one.
        #[cfg(target_os = "linux")]
        let (maps, short_maps) = self.user_namespace(
            User::current().uid == 0,
            shipyard_runtime::process::rootless_id_maps,
        )?;
        #[cfg(not(target_os = "linux"))]
        let (maps, short_maps) = (self.id_maps(), None);
        isolation.id_maps = maps;
        if isolation.id_maps.is_some() {
            isolation.namespaces.push(Namespace::User);
        }
        self.add_network(&mut isolation, &mode)?;
        self.add_ipc(&mut isolation)?;
        self.add_pid(&mut isolation)?;
        // An image's root comes once it's pulled (`Planned::finish`).
        if let Some(dir) = &self.rootfs {
            isolation.root = Some(self.rootfs_root(dir)?);
        }
        #[cfg(target_os = "linux")]
        super::devices::add(
            &mut isolation.devices,
            &self.device,
            &self.device_cgroup_rule,
            self.has_root(),
        )?;
        let checked = if self.image.is_some() {
            isolation.validate_expecting_root()
        } else {
            isolation.validate()
        };
        match checked {
            // A network setting refused for want of a network of its own:
            // the runtime can't name the flag that gives one.
            Err(error @ IsolationError::Sysctl { .. })
                if !mode.own_namespace() && wants_own_network(&error) =>
            {
                bail!(
                    InvalidRequest,
                    "{error} (any --network but host gives it one: the default, bridge, none, pasta)"
                )
            }
            checked => checked?,
        }
        Ok((Some(isolation), short_maps))
    }
}

impl RunRequest {
    /// The root `--rootfs DIR` names: absolute and without symlinks, as the
    /// runtime wants it, and a directory (a mistyped path is caught here,
    /// with its name).
    fn rootfs_root(&self, dir: &std::path::Path) -> Result<Root> {
        let path = std::fs::canonicalize(dir)
            .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
            .with_context(|| format!("--rootfs {}", dir.display()))?;
        if !path.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                format!("--rootfs {}: it isn't a directory", dir.display()),
            ));
        }
        let mut root = Root::new(path);
        root.read_only = self.read_only;
        Ok(root)
    }

    /// The user namespace for this run, and why its maps are short if they
    /// are: for root, the one `--uidmap` asks for (if any); for anyone else,
    /// always one, with maps chosen for them by `rootless` (their own ID,
    /// and their `/etc/subuid` ranges when the helpers are there).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux has user namespaces")
    )]
    pub(super) fn user_namespace(
        &self,
        root: bool,
        rootless: impl FnOnce() -> RootlessMaps,
    ) -> Result<(Option<IdMaps>, Option<String>)> {
        if root {
            return Ok((self.id_maps(), None));
        }
        if !self.uidmap.is_empty() {
            bail!(
                NeedsRoot,
                "--uidmap and --gidmap need root: a rootless container's maps are chosen for \
                 you (your own ID, and your range in /etc/subuid)"
            );
        }
        let rootless = rootless();
        Ok((Some(rootless.maps), rootless.shortfall))
    }

    /// The user namespace's maps, if `--uidmap` asks for one: `--gidmap`, or
    /// the same ranges for groups.
    pub(super) fn id_maps(&self) -> Option<IdMaps> {
        if self.uidmap.is_empty() {
            return None;
        }
        let gids = if self.gidmap.is_empty() {
            &self.uidmap
        } else {
            &self.gidmap
        };
        Some(IdMaps {
            uids: self.uidmap.clone(),
            gids: gids.clone(),
        })
    }

    /// The container's capabilities and `no_new_privs`, from Docker's default
    /// set and `--cap-drop`, `--cap-add` and `--security-opt`.
    pub(super) fn security(&self) -> Result<Security> {
        let named = |args: &[CapabilityArg]| -> CapabilitySet {
            args.iter()
                .filter_map(|arg| match arg {
                    CapabilityArg::One(capability) => Some(*capability),
                    CapabilityArg::All => None,
                })
                .collect()
        };
        let (dropped, added) = (named(&self.cap_drop), named(&self.cap_add));
        if let Some(both) = dropped
            .iter()
            .find(|&capability| added.contains(capability))
        {
            bail!(InvalidRequest, "--cap-add and --cap-drop both name {both}");
        }
        let mut kept = if self.cap_drop.contains(&CapabilityArg::All) {
            CapabilitySet::EMPTY
        } else {
            CapabilitySet::DOCKER_DEFAULT
        };
        for capability in dropped.iter() {
            kept.remove(capability);
        }
        let added = if self.cap_add.contains(&CapabilityArg::All) {
            CapabilitySet::ALL
        } else {
            added
        };
        let mut security = Security::standard();
        security.capabilities = Capabilities::Default { kept, added };
        for option in &self.security_opt {
            match option {
                SecurityOpt::NoNewPrivileges(on) => security.no_new_privileges = *on,
                SecurityOpt::SeccompUnconfined => security.seccomp = Seccomp::Unconfined,
                SecurityOpt::SeccompProfile(path) => {
                    security.seccomp = Seccomp::Profile(Box::new(read_profile(path)?));
                }
            }
        }
        Ok(security)
    }
}

/// A seccomp profile file, checked (`--security-opt seccomp=FILE`): a
/// mistake shows here, naming the file, not when the container starts.
fn read_profile(path: &Path) -> Result<Profile> {
    Profile::from_file(path).with_context(|| format!("seccomp profile {}", path.display()))
}

/// Whether `error` refuses a network setting only because the container
/// shares the host's network (not one that's the whole machine's anyway).
fn wants_own_network(error: &IsolationError) -> bool {
    matches!(error, IsolationError::Sysctl { key, .. }
        if key.starts_with("net.") && !HOST_WIDE_NET_SYSCTLS.contains(&key.as_str()))
}
