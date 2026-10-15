//! A run's network as a whole: how its container is connected (bridge
//! networks, pasta, or not at all) and its directory, set up before the
//! container starts and undone when the run ends.

#[cfg(target_os = "linux")]
use super::NetworkMode;
use super::attach::port_requests;
#[cfg(target_os = "linux")]
use super::attach::{Attachment, resolver, take_all};
#[cfg(target_os = "linux")]
use super::pasta::PastaPlan;
use super::{NetworkPlan, RunDir};
use crate::Engine;
use crate::error::Result;
use crate::report::WarningKind;

/// How a run's container is connected.
#[derive(Debug)]
enum Connection {
    /// It isn't (the host's network, or none at all).
    Nothing,
    /// On bridge networks, in order.
    #[cfg(target_os = "linux")]
    Bridges(Vec<Attachment>),
    /// Through pasta.
    #[cfg(target_os = "linux")]
    Pasta(PastaPlan),
}

/// A run's network, set up before its container starts: how it's
/// connected, and its directory (the `/etc` files, pasta's files). Dropped
/// when the run ends, in that order (fields drop in order): the directory
/// goes last.
#[derive(Debug)]
pub struct RunNetwork {
    connection: Connection,
    dir: Option<RunDir>,
}

impl RunNetwork {
    /// Sets up what `plan` asks for (nothing, without one: an unisolated
    /// run), with `exposed` (the image's ports) for `-P`.
    ///
    /// # Errors
    ///
    /// If a network can't be joined (taken, refused by the kernel), its
    /// ports can't be had, or the run's files can't be made.
    pub fn set_up(
        engine: &Engine,
        plan: Option<&NetworkPlan>,
        exposed: &[String],
    ) -> Result<RunNetwork> {
        let Some(plan) = plan else {
            return Ok(RunNetwork {
                connection: Connection::Nothing,
                dir: None,
            });
        };
        let requests = port_requests(
            &plan.ports,
            plan.publish_all,
            exposed,
            engine.reporter().as_ref(),
        );
        if !plan.etc_files && plan.mode.own_namespace() {
            warn_about_host_resolvers(engine);
        }
        #[cfg(target_os = "linux")]
        {
            // A bridge network's address is taken now (each network's lock
            // is held briefly); the container is connected once its
            // namespace exists.
            if let NetworkMode::Bridges(joined) = &plan.mode {
                let attachments = take_all(engine, plan, joined, &requests)?;
                let addresses = attachments
                    .iter()
                    .find(|attachment| attachment.routes.ipv4)
                    .map(Attachment::addresses)
                    .unwrap_or_default();
                let dir = plan
                    .etc_files
                    .then(|| {
                        let configs: Vec<_> = attachments
                            .iter()
                            .map(|attachment| &attachment.config)
                            .collect();
                        RunDir::make(engine.paths(), plan, &addresses, resolver(&configs), false)
                    })
                    .transpose()?;
                return Ok(RunNetwork {
                    connection: Connection::Bridges(attachments),
                    dir,
                });
            }
            if plan.mode == NetworkMode::Pasta {
                let (program, features) = plan.pasta.clone().ok_or_else(|| {
                    crate::Error::new(
                        crate::ErrorKind::Environment,
                        "no pasta was found for the run",
                    )
                })?;
                // pasta publishes on both families, as the firewall does.
                let held = shipyard_network::ports::hold(&requests)?;
                let ports = held.iter().map(|held| held.published).collect();
                // pasta answers DNS itself, for the host's resolver.
                let resolver = std::net::IpAddr::V4(shipyard_network::pasta::DNS_ADDRESS);
                let dir = RunDir::make(engine.paths(), plan, &[], Some(resolver), true)?;
                let pasta = PastaPlan {
                    program,
                    features,
                    held: std::cell::RefCell::new(held),
                    ports,
                    requests,
                    dir: dir.path.clone(),
                    defaulted: plan.defaulted,
                    running: std::cell::RefCell::new(None),
                    reporter: std::sync::Arc::clone(engine.reporter()),
                };
                return Ok(RunNetwork {
                    connection: Connection::Pasta(pasta),
                    dir: Some(dir),
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = requests;
        let dir = plan
            .etc_files
            .then(|| RunDir::make(engine.paths(), plan, &[], None, false))
            .transpose()?;
        Ok(RunNetwork {
            connection: Connection::Nothing,
            dir,
        })
    }

    /// The host ports it publishes, as a container's record keeps them
    /// (see [`Attachment::bound`]; pasta's on every address are on
    /// `0.0.0.0`, and on `::` too where the host has IPv6: pasta listens on
    /// both).
    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn bound(&self) -> Vec<crate::containers::BoundPort> {
        match &self.connection {
            Connection::Nothing => Vec::new(),
            Connection::Bridges(attachments) => {
                attachments.iter().flat_map(Attachment::bound).collect()
            }
            Connection::Pasta(pasta) => {
                let host_ipv6 = std::fs::read("/proc/net/if_inet6")
                    .is_ok_and(|addresses| !addresses.is_empty());
                pasta
                    .ports
                    .iter()
                    .flat_map(|port| {
                        let ipv6 = (host_ipv6 && port.host_ip.is_none()).then(|| {
                            super::attach::bound(port, std::net::Ipv6Addr::UNSPECIFIED.into())
                        });
                        std::iter::once(super::attach::bound(
                            port,
                            std::net::Ipv4Addr::UNSPECIFIED.into(),
                        ))
                        .chain(ipv6)
                    })
                    .collect()
            }
        }
    }

    /// Lets go of the host ports pasta will publish: call it just before
    /// the container is started (see `PastaPlan::held`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::unused_self, reason = "networks are Linux's")
    )]
    pub fn release_ports(&self) {
        #[cfg(target_os = "linux")]
        if let Connection::Pasta(pasta) = &self.connection {
            pasta.held.borrow_mut().clear();
        }
    }

    /// The run's `/etc` files, if it gets them.
    #[must_use]
    pub fn etc(&self) -> Option<&RunDir> {
        self.dir.as_ref()
    }

    /// Adds what keeps the run's network alive to `holds` (its leases and
    /// ports, then its directory's locks), for a detached container's
    /// monitor (see [`into_detached`](Self::into_detached)). pasta's are
    /// its own (it keeps its ports, and ends with the container's init): only
    /// its files, in the run directory, are held.
    #[cfg(target_os = "linux")]
    pub(crate) fn holds<'a>(&'a self, holds: &mut crate::run::holds::Holds<'a>) {
        use crate::run::holds::HoldKind;
        match &self.connection {
            // Nothing; or pasta, which keeps its ports itself and ends with
            // the init: only its files (the run directory) are held.
            Connection::Nothing | Connection::Pasta(_) => {}
            Connection::Bridges(attachments) => {
                for attachment in attachments {
                    attachment.holds(holds);
                }
            }
        }
        if let Some(dir) = &self.dir {
            let mut locks = dir.holds();
            if let (Some(lock), Some(directory)) = (locks.next(), locks.next()) {
                holds.add(HoldKind::RunDirLock, lock);
                holds.add(HoldKind::RunDir, directory);
            }
        }
    }

    /// Gives the network up to whoever holds copies of its
    /// [`holds`](Self::holds): this process's are closed, never unlocked,
    /// and nothing is released (the holder does that). What connecting the
    /// container needs is kept.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn into_detached(self) -> DetachedNetwork {
        let RunNetwork { connection, dir } = self;
        let (connects, pasta) = match connection {
            Connection::Nothing => (Vec::new(), None),
            Connection::Pasta(pasta) => (Vec::new(), Some(pasta)),
            Connection::Bridges(attachments) => (
                attachments
                    .into_iter()
                    .filter_map(Attachment::into_connect)
                    .collect(),
                None,
            ),
        };
        // Its holder removes it (`__cleanup` is told where).
        if let Some(dir) = dir {
            dir.give_up();
        }
        DetachedNetwork {
            connects,
            interface_sysctls: Vec::new(),
            pasta,
        }
    }

    /// The runtime's network hook: it connects a container on a bridge
    /// network, or starts its pasta.
    #[must_use]
    pub fn hook(
        &self,
    ) -> Option<impl FnMut(shipyard_runtime::process::NetworkTarget<'_>) -> Result<(), String> + '_>
    {
        let connected = !matches!(self.connection, Connection::Nothing);
        connected.then_some(
            move |target: shipyard_runtime::process::NetworkTarget<'_>| -> Result<(), String> {
                match &self.connection {
                    #[cfg(target_os = "linux")]
                    Connection::Bridges(attachments) => attachments
                        .iter()
                        .try_for_each(|attachment| attachment.connect(target.netns)),
                    #[cfg(target_os = "linux")]
                    Connection::Pasta(pasta) => pasta.start(target.pid),
                    Connection::Nothing => {
                        let _ = target;
                        Ok(())
                    }
                }
            },
        )
    }
}

#[cfg(target_os = "linux")]
/// A detached container's network, its holds given up to its monitor:
/// what connecting it needs.
#[derive(Debug)]
pub(crate) struct DetachedNetwork {
    /// One per bridge network, in order.
    pub(crate) connects: Vec<super::attach::Connect>,
    /// Its interfaces' own settings (`net.ipv4.conf.eth1.*`), set once
    /// they're made.
    pub(crate) interface_sysctls: Vec<(String, String)>,
    /// Its pasta, to start once the container exists.
    pasta: Option<PastaPlan>,
}

#[cfg(target_os = "linux")]
impl DetachedNetwork {
    /// Starts its pasta (if it has one) for the container whose init is
    /// `init`, following it; its launcher holds `hold` (a lock) until pasta
    /// has ended.
    pub(crate) fn start_pasta(
        &self,
        init: &shipyard_network::pasta::InitToFollow<'_>,
        hold: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> Result<(), String> {
        self.pasta
            .as_ref()
            .map_or(Ok(()), |pasta| pasta.start_detached(init, hold))
    }

    /// Whether it has a pasta to start.
    pub(crate) fn has_pasta(&self) -> bool {
        self.pasta.is_some()
    }
}

/// Warns that a container with a network of its own but no root of its own
/// sees the host's `/etc/resolv.conf`, if its resolvers are only the host's
/// own (`127.0.0.53`, say): they're unreachable from the container's
/// network, and its DNS lookups will fail.
fn warn_about_host_resolvers(engine: &Engine) {
    let host = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let servers: Vec<std::net::IpAddr> = host
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|server| server.trim().parse().ok())
        .collect();
    if !servers.is_empty() && servers.iter().all(std::net::IpAddr::is_loopback) {
        engine.warn(
            WarningKind::HostResolvers,
            "the container sees the host's /etc/resolv.conf, whose resolvers are the host's own \
             (on its loopback), which a network of its own can't reach: its DNS lookups will \
             fail. Give it a root of its own (an image or --rootfs, then it gets resolvers it can \
             reach) or the host's network (--network host)",
        );
    }
}
