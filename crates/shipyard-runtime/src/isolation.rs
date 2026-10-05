//! What a command is isolated from: Linux namespaces (`namespaces(7)`).
//!
//! A namespace gives a process its own copy of one global resource: its own
//! process IDs (PID namespace), mount table (mount), hostname (UTS), System V
//! IPC objects (IPC), and so on. A container is a process in a fresh set of
//! them. This module only *describes* the isolation and checks that it's
//! safe to create; creating it is the process code's job.
//!
//! The shape follows the OCI runtime spec's `linux.namespaces` and `hostname`,
//! so stage 3 can fill it from a `config.json`.
//!
//! [`Join`] describes the other direction: a new process entering the
//! namespaces of one that's already running (`setns(2)`), as `exec` into a
//! container does.

use std::fmt;

/// A kind of Linux namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Namespace {
    /// Process IDs: the first process inside is PID 1 (`pid_namespaces(7)`).
    Pid,
    /// The mount table (`mount_namespaces(7)`).
    Mount,
    /// Hostname and NIS domain name (`uts_namespaces(7)`).
    Uts,
    /// System V IPC objects and POSIX message queues (`ipc_namespaces(7)`).
    Ipc,
    /// Network devices, addresses, ports, routes (`network_namespaces(7)`).
    Network,
    /// User and group IDs (`user_namespaces(7)`).
    User,
    /// The cgroup root (`cgroup_namespaces(7)`).
    Cgroup,
    /// The boot and monotonic clocks (`time_namespaces(7)`).
    Time,
}

impl Namespace {
    /// Every kind, sorted by their `/proc/PID/ns` names.
    pub const ALL: [Self; 8] = [
        Self::Cgroup,
        Self::Ipc,
        Self::Mount,
        Self::Network,
        Self::Pid,
        Self::Time,
        Self::User,
        Self::Uts,
    ];

    /// Its name in `/proc/PID/ns/` (and in OCI's `linux.namespaces[].type`,
    /// except that OCI says `mount` and `network`).
    #[must_use]
    pub fn proc_name(self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::Mount => "mnt",
            Self::Uts => "uts",
            Self::Ipc => "ipc",
            Self::Network => "net",
            Self::User => "user",
            Self::Cgroup => "cgroup",
            Self::Time => "time",
        }
    }

    /// The `CLONE_NEW*` flag that creates it.
    ///
    /// `CLONE_NEWTIME` (0x80) only works with `clone3` and `unshare`: in the
    /// older `clone` call the low byte of the flags is the exit signal, so
    /// it would be misread there.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_flag(self) -> libc::c_int {
        match self {
            Self::Pid => libc::CLONE_NEWPID,
            Self::Mount => libc::CLONE_NEWNS,
            Self::Uts => libc::CLONE_NEWUTS,
            Self::Ipc => libc::CLONE_NEWIPC,
            Self::Network => libc::CLONE_NEWNET,
            Self::User => libc::CLONE_NEWUSER,
            Self::Cgroup => libc::CLONE_NEWCGROUP,
            Self::Time => libc::CLONE_NEWTIME,
        }
    }

    /// Its position in [`Namespace::ALL`], which is how a forked child names
    /// it in a failure report.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn index(self) -> u64 {
        // Every kind is in ALL; the fallback can't happen.
        Self::ALL
            .iter()
            .position(|&kind| kind == self)
            .and_then(|index| u64::try_from(index).ok())
            .unwrap_or(u64::MAX)
    }

    /// Whether this stage can create it. The others come with the stages that
    /// give them meaning: network (stage 7), user (rootless, stage 4), cgroup
    /// (stage 5), time (not planned yet).
    fn is_supported(self) -> bool {
        matches!(self, Self::Pid | Self::Mount | Self::Uts | Self::Ipc)
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.proc_name())
    }
}

/// The longest hostname the kernel accepts (`HOST_NAME_MAX`).
pub const MAX_HOSTNAME_LEN: usize = 64;
/// The longest part between dots that RFC 1123 allows.
const MAX_LABEL_LEN: usize = 63;

/// How a command is isolated: which new namespaces it gets, and the hostname
/// inside them.
///
/// Start from [`Isolation::standard`] and change fields: it will gain more
/// (a domain name, then OCI's namespace paths), so it can't be written out
/// in full outside this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Isolation {
    /// The namespaces to create. Each kind at most once.
    pub namespaces: Vec<Namespace>,
    /// The hostname inside the container. Needs a UTS namespace. `None` keeps
    /// the host's (which the new UTS namespace starts with a copy of).
    pub hostname: Option<String>,
}

impl Isolation {
    /// The standard set: new PID, mount, UTS and IPC namespaces, and the
    /// host's hostname.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            namespaces: vec![
                Namespace::Pid,
                Namespace::Mount,
                Namespace::Uts,
                Namespace::Ipc,
            ],
            hostname: None,
        }
    }

    /// The `CLONE_NEW*` flags for all its namespaces together.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_flags(&self) -> libc::c_int {
        self.namespaces
            .iter()
            .fold(0, |flags, namespace| flags | namespace.clone_flag())
    }

    /// Whether it includes a new `namespace`.
    #[must_use]
    pub fn has(&self, namespace: Namespace) -> bool {
        self.namespaces.contains(&namespace)
    }

    /// Rejects what the kernel would refuse or what this stage can't do yet,
    /// before anything starts.
    ///
    /// # Errors
    ///
    /// A namespace given twice or not supported yet, a PID namespace without
    /// a mount namespace, a hostname without a UTS namespace, or a hostname
    /// that isn't valid.
    pub fn validate(&self) -> Result<(), IsolationError> {
        check_kinds(&self.namespaces)?;
        // The container gets a fresh /proc for its PID namespace. Without its
        // own mount namespace that mount would replace the host's /proc, and
        // every process on the machine would see the container's PIDs.
        if self.has(Namespace::Pid) && !self.has(Namespace::Mount) {
            return Err(IsolationError::PidNeedsMount);
        }
        if let Some(hostname) = &self.hostname {
            if !self.has(Namespace::Uts) {
                return Err(IsolationError::HostnameNeedsUts);
            }
            validate_hostname(hostname)?;
        }
        Ok(())
    }
}

/// Which running process's namespaces a command joins, and which of them.
///
/// The target is normally a container's init (PID 1 inside, seen from the
/// host). Start from [`Join::standard`]: like [`Isolation`], it will gain
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Join {
    /// The process whose namespaces to join, by its PID as shipyard sees it
    /// (in shipyard's own PID namespace; on a plain host, the host's PID).
    pub target: u32,
    /// The namespaces to join. Each kind at most once.
    pub namespaces: Vec<Namespace>,
}

impl Join {
    /// Joins `target`'s PID, mount, UTS and IPC namespaces: the ones a
    /// [standard](Isolation::standard) container has.
    #[must_use]
    pub fn standard(target: u32) -> Self {
        Self {
            target,
            namespaces: Isolation::standard().namespaces,
        }
    }

    /// Whether it joins `namespace`.
    #[must_use]
    pub fn has(&self, namespace: Namespace) -> bool {
        self.namespaces.contains(&namespace)
    }

    /// Rejects what can't work, before anything starts.
    ///
    /// The mount namespace may be joined without the PID namespace (to work
    /// on the container's files): the process then sees the container's
    /// `/proc`, whose PIDs aren't its own, so `ps` and `/proc/self` mislead
    /// it.
    ///
    /// # Errors
    ///
    /// No namespaces, a namespace given twice or not supported yet, or a PID
    /// namespace without the mount namespace (whose `/proc` shows its PIDs).
    pub fn validate(&self) -> Result<(), IsolationError> {
        if self.namespaces.is_empty() {
            return Err(IsolationError::NothingToJoin);
        }
        check_kinds(&self.namespaces)?;
        // In the container's PID namespace but with the host's /proc, the
        // process would see the wrong processes: `ps`, `kill $(pidof …)` and
        // `/proc/self` would all mislead it.
        if self.has(Namespace::Pid) && !self.has(Namespace::Mount) {
            return Err(IsolationError::PidNeedsMount);
        }
        Ok(())
    }
}

/// Each namespace at most once, and only the kinds this stage supports.
fn check_kinds(namespaces: &[Namespace]) -> Result<(), IsolationError> {
    for (index, namespace) in namespaces.iter().enumerate() {
        if namespaces
            .get(..index)
            .is_some_and(|before| before.contains(namespace))
        {
            return Err(IsolationError::Duplicate(*namespace));
        }
        if !namespace.is_supported() {
            return Err(IsolationError::Unsupported(*namespace));
        }
    }
    Ok(())
}

/// A hostname as RFC 1123 describes one: dot-separated labels of letters,
/// digits and hyphens, each 1–63 characters and not starting or ending with a
/// hyphen; at most [`MAX_HOSTNAME_LEN`] bytes in all.
///
/// Stricter than the kernel (which takes any bytes but NUL): a hostname that
/// other programs can't resolve or print only causes trouble later.
///
/// # Errors
///
/// [`IsolationError::InvalidHostname`], saying what's wrong.
pub fn validate_hostname(hostname: &str) -> Result<(), IsolationError> {
    let invalid = |reason: &'static str| IsolationError::InvalidHostname {
        hostname: hostname.to_owned(),
        reason,
    };
    if hostname.is_empty() {
        return Err(invalid("it is empty"));
    }
    if hostname.len() > MAX_HOSTNAME_LEN {
        return Err(invalid("it is longer than 64 bytes"));
    }
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err(invalid(
                "it has an empty part (two dots, or a dot at an end)",
            ));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(invalid("a part between dots is longer than 63 characters"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(invalid(
                "only letters, digits, hyphens and dots are allowed",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("a part can't start or end with a hyphen"));
        }
    }
    Ok(())
}

/// Why an [`Isolation`] can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IsolationError {
    /// The same namespace is listed twice.
    #[error("the {0} namespace is listed more than once")]
    Duplicate(Namespace),
    /// A namespace kind a later stage adds.
    #[error("the {0} namespace isn't supported yet")]
    Unsupported(Namespace),
    /// A PID namespace without a mount namespace: a new one's `/proc` would
    /// be mounted over the host's; a joined one would be seen through the
    /// host's `/proc`.
    #[error("a PID namespace needs a mount namespace too (for its own /proc)")]
    PidNeedsMount,
    /// A hostname was given, but no UTS namespace to set it in: it would
    /// change the host's hostname.
    #[error("setting the hostname needs a UTS namespace")]
    HostnameNeedsUts,
    /// A [`Join`] with no namespaces to join.
    #[error("no namespaces to join")]
    NothingToJoin,
    /// The hostname isn't valid.
    #[error("invalid hostname \"{hostname}\": {reason}")]
    InvalidHostname {
        /// The hostname given.
        hostname: String,
        /// What's wrong with it.
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_isolation_is_valid() {
        let standard = Isolation::standard();
        assert_eq!(standard.validate(), Ok(()));
        for namespace in [
            Namespace::Pid,
            Namespace::Mount,
            Namespace::Uts,
            Namespace::Ipc,
        ] {
            assert!(standard.has(namespace), "{namespace}");
        }
    }

    #[test]
    fn duplicates_and_later_stages_are_rejected() {
        let mut twice = Isolation::standard();
        twice.namespaces.push(Namespace::Pid);
        assert_eq!(
            twice.validate(),
            Err(IsolationError::Duplicate(Namespace::Pid))
        );

        for later in [
            Namespace::Network,
            Namespace::User,
            Namespace::Cgroup,
            Namespace::Time,
        ] {
            let mut isolation = Isolation::standard();
            isolation.namespaces.push(later);
            assert_eq!(
                isolation.validate(),
                Err(IsolationError::Unsupported(later))
            );
        }
    }

    #[test]
    fn a_pid_namespace_needs_a_mount_namespace() {
        let isolation = Isolation {
            namespaces: vec![Namespace::Pid, Namespace::Uts],
            hostname: None,
        };
        assert_eq!(isolation.validate(), Err(IsolationError::PidNeedsMount));
        // A mount namespace alone is fine: its /proc still shows the host's
        // processes, but nothing leaks.
        let mount_only = Isolation {
            namespaces: vec![Namespace::Mount],
            hostname: None,
        };
        assert_eq!(mount_only.validate(), Ok(()));
    }

    #[test]
    fn joins_are_checked_like_isolations() {
        assert_eq!(Join::standard(42).validate(), Ok(()));
        let join = |namespaces: Vec<Namespace>| Join {
            target: 42,
            namespaces,
        };
        assert_eq!(
            join(Vec::new()).validate(),
            Err(IsolationError::NothingToJoin)
        );
        assert_eq!(
            join(vec![Namespace::Uts, Namespace::Uts]).validate(),
            Err(IsolationError::Duplicate(Namespace::Uts))
        );
        assert_eq!(
            join(vec![Namespace::Network]).validate(),
            Err(IsolationError::Unsupported(Namespace::Network))
        );
        assert_eq!(
            join(vec![Namespace::Pid, Namespace::Uts]).validate(),
            Err(IsolationError::PidNeedsMount)
        );
        assert_eq!(join(vec![Namespace::Uts]).validate(), Ok(()));
    }

    #[test]
    fn a_hostname_needs_a_uts_namespace() {
        let isolation = Isolation {
            namespaces: vec![Namespace::Pid, Namespace::Mount],
            hostname: Some("box".into()),
        };
        assert_eq!(isolation.validate(), Err(IsolationError::HostnameNeedsUts));
    }

    #[test]
    fn good_hostnames() {
        for hostname in [
            "a",
            "box",
            "web-1",
            "db.internal",
            "A1",
            &"x".repeat(63),
            &format!("{}.{}", "a".repeat(31), "b".repeat(32)),
        ] {
            assert_eq!(validate_hostname(hostname), Ok(()), "{hostname}");
        }
    }

    #[test]
    fn bad_hostnames() {
        for hostname in [
            "",
            "-box",
            "box-",
            "a..b",
            ".box",
            "box.",
            "under_score",
            "spa ce",
            "nul\0byte",
            "émoji",
            &"x".repeat(64),
            &format!("{}.{}", "a".repeat(32), "b".repeat(32)),
        ] {
            assert!(
                matches!(
                    validate_hostname(hostname),
                    Err(IsolationError::InvalidHostname { .. })
                ),
                "{hostname:?} was accepted"
            );
        }
    }

    #[test]
    fn proc_names_are_unique_and_cover_every_kind() {
        let mut names: Vec<&str> = Namespace::ALL.iter().map(|n| n.proc_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Namespace::ALL.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_standard_flags() {
        assert_eq!(
            Isolation::standard().clone_flags(),
            libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clone_flags_are_distinct() {
        let flags: Vec<libc::c_int> = Namespace::ALL.iter().map(|n| n.clone_flag()).collect();
        for (i, a) in flags.iter().enumerate() {
            for b in &flags[i + 1..] {
                assert_eq!(a & b, 0, "{a:#x} and {b:#x} overlap");
            }
        }
    }
}
