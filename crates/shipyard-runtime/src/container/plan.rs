//! A planned container: what [`create_planned`](super::create_planned)
//! runs in place of a bundle, written into the container's state directory
//! (`plan.json`) for its monitor to read.
//!
//! An OCI `config.json` can't say all a caller like Shipyard's engine
//! plans: an image's overlay as the root, users and groups by name (looked
//! up in the container's own files), the defaults (capabilities by user,
//! Docker's seccomp profile, the masked paths). The plan is the runtime's
//! own types, as they are, so nothing is translated, and nothing lost.
//!
//! It's read only by the monitor `create_planned` starts: the same binary,
//! in the same call. Its format needn't last across versions, then; its
//! version is a check, and any other is refused.

#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "written and read by the Linux lifecycle only")
)]

#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use super::{ContainerError, Result};
use crate::isolation::Isolation;
use crate::process::ProcessSpec;
#[cfg(target_os = "linux")]
use crate::process::setup::SpecSetup;

/// The plan's file in the state directory.
pub(crate) const PLAN_FILE: &str = "plan.json";

/// The plan's version: the one this binary writes and reads.
pub(crate) const PLAN_VERSION: u32 = 1;

/// The most of a plan read: far more than a real one takes. Its arguments
/// and environment are written as arrays of numbers (they may be any
/// bytes), about four bytes for one, so this leaves room for more than the
/// kernel's own limit on them.
pub(crate) const MAX_PLAN_SIZE: u64 = 16 * 1024 * 1024;

/// What a planned container runs: its process, its isolation, and a
/// terminal of its own if it has one. Checked as a bundle's would be, by
/// [`create_planned`](super::create_planned). Its paths must be UTF-8 (the
/// plan is JSON); the process's arguments and environment may be any bytes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Plan {
    /// The process.
    pub spec: ProcessSpec,
    /// Its namespaces, root filesystem (an image's overlay, say), security
    /// and limits.
    pub isolation: Isolation,
    /// A terminal of the container's own, if it has one (and its size, if
    /// known).
    pub terminal: Option<crate::oci::BundleTerminal>,
}

impl Plan {
    /// A plan to run `spec` isolated as `isolation`, with no terminal.
    #[must_use]
    pub fn new(spec: ProcessSpec, isolation: Isolation) -> Self {
        Self {
            spec,
            isolation,
            terminal: None,
        }
    }

    /// The plan as this caller runs it, checked as a bundle is when it's
    /// read, before anything is made: its isolation, its process, its
    /// limits, its capability sets and its seccomp profile; a root
    /// filesystem (a bundle always has one; without, the container would
    /// share the host's, its own state directory included); a terminal only
    /// with a mount namespace, and a size with a zero in it as no size
    /// (runc's rule, as a bundle's); no hooks (a bundle's, through
    /// [`create`](super::create), have them); rootless, a user namespace of
    /// its own (a plan says it, nothing adds it). Its cgroup mount is left
    /// out where the container can't have a cgroup of its own, as a
    /// bundle's is (the warning, if so).
    ///
    /// # Errors
    ///
    /// [`ContainerError::InvalidPlan`], saying what's wrong.
    #[cfg(target_os = "linux")]
    pub(crate) fn for_caller(&self) -> Result<(Self, Option<String>)> {
        use crate::isolation::Namespace;
        use crate::security::Capabilities;
        let mut plan = self.clone();
        if plan.isolation.hooks.is_some() {
            return Err(PlanError::Hooks.into());
        }
        if plan.isolation.root.is_none() {
            return Err(PlanError::NoRoot.into());
        }
        if !nix::unistd::geteuid().is_root() && !plan.isolation.has(Namespace::User) {
            return Err(PlanError::NeedsUserNamespace.into());
        }
        if let Some(terminal) = &mut plan.terminal {
            if !plan.isolation.has(Namespace::Mount) {
                return Err(PlanError::TerminalWithoutMount.into());
            }
            terminal.size = terminal.size.filter(|size| size.rows > 0 && size.cols > 0);
        }
        let warning = crate::oci::without_unusable_cgroup_mount(&mut plan.isolation);
        plan.isolation.validate().map_err(PlanError::from)?;
        plan.spec.validate().map_err(PlanError::from)?;
        plan.isolation
            .resources
            .validate()
            .map_err(PlanError::from)?;
        if let Capabilities::Exact(sets) = plan.isolation.security.capabilities {
            sets.check().map_err(PlanError::Capabilities)?;
        }
        if let crate::security::Seccomp::Profile(profile) = &plan.isolation.security.seccomp {
            profile.validate().map_err(PlanError::from)?;
        }
        Ok((plan, warning))
    }

    /// What's written.
    ///
    /// # Errors
    ///
    /// A value JSON can't hold (a path that isn't UTF-8: a plan's paths
    /// must be), or a plan bigger than [`MAX_PLAN_SIZE`].
    #[cfg(target_os = "linux")]
    pub(crate) fn to_json(&self) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(&PlanFile {
            version: PLAN_VERSION,
            spec: SpecSetup::new(&self.spec),
            isolation: self.isolation.clone(),
            terminal: self.terminal.map(|terminal| TerminalFile {
                size: terminal.size,
            }),
        })
        .map_err(|err| PlanError::Encoding(err.to_string()))?;
        if json.len() as u64 > MAX_PLAN_SIZE {
            return Err(PlanError::TooLarge { size: json.len() }.into());
        }
        Ok(json)
    }

    /// A plan read back.
    ///
    /// # Errors
    ///
    /// Not this binary's plan (another version, or not a plan at all).
    #[cfg(target_os = "linux")]
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let unreadable =
            |why: String| ContainerError::Setup(format!("the container's {PLAN_FILE}: {why}"));
        match super::versions::version_of(bytes) {
            Some(PLAN_VERSION) => {}
            // A file with no version reads as 0: not a plan this writes.
            Some(found) => {
                return Err(ContainerError::UnknownPlanVersion { found });
            }
            None => return Err(unreadable("not a plan".to_owned())),
        }
        let file: PlanFile =
            serde_json::from_slice(bytes).map_err(|err| unreadable(err.to_string()))?;
        Ok(Self {
            spec: file
                .spec
                .spec()
                .map_err(|err| unreadable(err.to_string()))?,
            isolation: file.isolation,
            terminal: file.terminal.map(|terminal| crate::oci::BundleTerminal {
                size: terminal.size,
            }),
        })
    }

    /// The plan as a bundle would give it, for what `create` and the
    /// monitor do with one: no warnings, annotations or hooks of its own,
    /// its cgroup the runtime's.
    pub(crate) fn into_bundle(self) -> crate::oci::Bundle {
        crate::oci::Bundle {
            spec: self.spec,
            isolation: self.isolation,
            warnings: Vec::new(),
            annotations: std::collections::BTreeMap::new(),
            hooks: crate::hooks::Hooks::default(),
            cgroups_path: None,
            terminal: self.terminal,
        }
    }
}

/// Why a plan can't be run, found before anything is made.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// It has hooks: a plan's would never run (a bundle's do, through
    /// [`create`](super::create)).
    #[error("a plan has no hooks (a bundle's are run through create)")]
    Hooks,
    /// It has no root filesystem: the container would share the host's.
    #[error("a plan needs a root filesystem (without one, the container would share the host's)")]
    NoRoot,
    /// Rootless, it has no user namespace of its own.
    #[error("without root, a container needs a user namespace of its own")]
    NeedsUserNamespace,
    /// A terminal without a mount namespace: its devpts would be mounted
    /// over the host's.
    #[error("a terminal needs a mount namespace of the container's own")]
    TerminalWithoutMount,
    /// Its isolation.
    #[error(transparent)]
    Isolation(#[from] crate::isolation::IsolationError),
    /// Its process.
    #[error(transparent)]
    Spec(#[from] crate::process::SpecError),
    /// Its limits.
    #[error(transparent)]
    Resources(#[from] crate::resources::ResourceError),
    /// Its capability sets.
    #[error("its capabilities: {0}")]
    Capabilities(String),
    /// Its seccomp profile.
    #[error("its seccomp profile: {0}")]
    Seccomp(#[from] crate::security::seccomp::SeccompError),
    /// Bigger than its monitor reads.
    #[error("it takes {size} bytes, more than {MAX_PLAN_SIZE}")]
    TooLarge {
        /// Its size, written.
        size: usize,
    },
    /// Something JSON can't hold (a path that isn't UTF-8).
    #[error("it can't be written: {0}")]
    Encoding(String),
}

/// `plan.json`.
#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    version: u32,
    spec: SpecSetup,
    isolation: Isolation,
    terminal: Option<TerminalFile>,
}

/// A terminal, in `plan.json`: its size, if known.
#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalFile {
    /// `[rows, cols]` (a [`Size`](crate::process::terminal::Size) keeps that shape).
    size: Option<crate::process::terminal::Size>,
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::isolation::{Mount, Namespace, Overlay, Root};

    /// A plan comes back as it was written: its overlay root (read-only,
    /// with masked paths), maps, sysctls, mounts, limits, security, user
    /// names, rlimits and terminal.
    #[test]
    fn a_plan_reads_as_written() {
        let mut isolation = Isolation::standard();
        let mut root = Root::new("/merged");
        root.overlay = Some(Overlay {
            lower: vec![PathBuf::from("/layers/b"), PathBuf::from("/layers/a")],
            upper: PathBuf::from("/area/upper"),
            work: PathBuf::from("/area/work"),
        });
        root.mounts.push(Mount::bind("/srv/data", "/data"));
        root.read_only = true;
        root.masked_paths.push(PathBuf::from("/proc/kcore"));
        isolation.root = Some(root);
        isolation.resources.memory_max = Some(64 << 20);
        isolation.resources.cpu_weight = Some(50);
        isolation.security.no_new_privileges = !isolation.security.no_new_privileges;
        isolation.security.capabilities =
            crate::security::Capabilities::Exact(crate::security::CapabilitySets::default());
        isolation.security.seccomp = crate::security::Seccomp::Unconfined;
        isolation.hostname = Some("box".to_owned());
        isolation.namespaces.push(Namespace::Network);
        isolation.sysctls = vec![("net.ipv4.ip_forward".to_owned(), "1".to_owned())];
        isolation.id_maps = Some(crate::isolation::IdMaps {
            uids: vec![crate::isolation::IdMap {
                container: 0,
                host: 1000,
                size: 1,
            }],
            gids: Vec::new(),
        });
        let mut spec = ProcessSpec::new(vec!["sh".into(), "-c".into(), "echo hi".into()]);
        spec.cwd = PathBuf::from("/work");
        spec.env
            .set(std::ffi::OsStr::new("KEY"), std::ffi::OsStr::new("value"))
            .unwrap();
        spec.user = crate::process::User::new(1000, 1000);
        spec.oom_score_adj = Some(5);
        spec.user.names.user = Some("app".to_owned());
        spec.user.names.memberships = true;
        spec.rlimits.push(crate::process::Rlimit {
            kind: crate::process::RlimitKind::Core,
            soft: crate::process::RlimitValue::Finite(0),
            hard: crate::process::RlimitValue::Unlimited,
        });
        let mut plan = Plan::new(spec, isolation.clone());
        plan.terminal = Some(crate::oci::BundleTerminal::sized(Some(
            crate::process::terminal::Size::new(24, 80),
        )));
        let json = plan.to_json().unwrap();
        // The size's shape on disk, as earlier versions wrote it (H2):
        // `[rows, cols]`.
        let file: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(file["terminal"]["size"], serde_json::json!([24, 80]));
        let read = Plan::parse(&json).unwrap();
        assert_eq!(read.isolation, isolation);
        assert_eq!(read.spec, plan.spec);
        assert_eq!(
            read.terminal.and_then(|terminal| terminal.size),
            Some(crate::process::terminal::Size::new(24, 80))
        );
    }

    /// A plan bigger than the monitor would read is refused when it's
    /// written, before anything is made.
    #[test]
    fn a_plan_too_big_is_refused_when_written() {
        let mut spec = ProcessSpec::new(vec!["true".into()]);
        spec.args
            .push("x".repeat(usize::try_from(MAX_PLAN_SIZE).unwrap()).into());
        let plan = Plan::new(spec, Isolation::standard());
        assert!(matches!(
            plan.to_json(),
            Err(ContainerError::InvalidPlan(PlanError::TooLarge { .. }))
        ));
    }

    /// A plan has no hooks: they'd be written and never run.
    #[test]
    fn a_plan_with_hooks_is_refused() {
        let mut isolation = Isolation::standard();
        // Everything else a rootless plan needs: only the hooks are wrong.
        isolation.namespaces.push(crate::isolation::Namespace::User);
        isolation.id_maps = Some(crate::id_map::IdMaps::single((1000, 1000)));
        isolation.root = Some(Root::new("/rootfs"));
        let without_hooks = Plan::new(ProcessSpec::new(vec!["true".into()]), isolation.clone());
        assert!(without_hooks.for_caller().is_ok(), "the control");
        isolation.hooks = Some(crate::hooks::ContainerHooks::new(
            crate::hooks::Hooks::default(),
            "x",
            std::path::Path::new("/b"),
            std::collections::BTreeMap::new(),
        ));
        let plan = Plan::new(ProcessSpec::new(vec!["true".into()]), isolation);
        assert!(matches!(
            plan.for_caller(),
            Err(ContainerError::InvalidPlan(PlanError::Hooks))
        ));
    }

    /// A terminal size with a zero in it is no size (runc's rule, as a
    /// bundle's): the default then, never a 0x0 terminal.
    #[test]
    fn a_zero_terminal_size_is_no_size() {
        let mut isolation = Isolation::standard();
        isolation.namespaces.push(Namespace::User);
        isolation.id_maps = Some(crate::id_map::IdMaps::single((1000, 1000)));
        isolation.root = Some(Root::new("/rootfs"));
        let mut plan = Plan::new(ProcessSpec::new(vec!["true".into()]), isolation);
        for (size, kept) in [((24, 80), Some((24, 80))), ((0, 80), None), ((24, 0), None)] {
            plan.terminal = Some(crate::oci::BundleTerminal::sized(Some(size.into())));
            let (checked, _) = plan.for_caller().unwrap();
            assert_eq!(
                checked.terminal.and_then(|terminal| terminal.size),
                kept.map(Into::into)
            );
        }
    }

    /// Another version, or no plan at all, is refused.
    #[test]
    fn another_plan_is_refused() {
        let plan = Plan::new(ProcessSpec::new(vec!["true".into()]), Isolation::standard());
        let json = String::from_utf8(plan.to_json().unwrap()).unwrap();
        assert!(Plan::parse(json.as_bytes()).is_ok(), "the control");
        let newer = json.replacen("\"version\":1", "\"version\":2", 1);
        assert!(matches!(
            Plan::parse(newer.as_bytes()),
            Err(ContainerError::UnknownPlanVersion { found: 2 })
        ));
        assert!(matches!(
            Plan::parse(b"{}"),
            Err(ContainerError::UnknownPlanVersion { found: 0 })
        ));
        assert!(Plan::parse(b"not json").is_err());
    }
}
