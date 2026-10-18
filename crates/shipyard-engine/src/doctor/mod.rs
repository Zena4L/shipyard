//! `doctor`: whether this host can run containers, and what to do where it
//! can't, as a [`Report`] its caller shows.
//!
//! Each check comes out [`Level::Ok`]; [`Level::Warn`], when something is
//! limited but a plain `shipyard run` works; [`Level::Fail`], when a plain
//! `run --no-image`, or `run IMAGE`, fails here.
//!
//! Some answers only trying can give. A host's settings can allow a user
//! namespace that a security policy (AppArmor's restriction, Ubuntu's
//! default since 23.10) then takes the capabilities from; an image store on
//! a filesystem overlayfs can't write to (inside another container) shows
//! nothing until a mount fails. So on Linux, the checks start a probe helper
//! (this binary again, as `__shipyard-probe`: [`probe_entry_point`]) that
//! makes a user namespace, a mount namespace, a tmpfs and an overlay, as a
//! run would, in a scratch directory next to the image store. Everything it
//! made goes with it.

use crate::Engine;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod probe;

#[cfg(target_os = "linux")]
pub use probe::probe_entry_point;

/// Runs this process as the probe helper, if that's what it was started as;
/// otherwise returns at once. Called first thing in `main` (by
/// [`entry_points`](crate::entry_points)).
#[cfg(not(target_os = "linux"))]
pub fn probe_entry_point() {}

/// How a check came out (as the guest's checks carry it: `ok`, `warn`,
/// `fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Level {
    /// Works.
    Ok,
    /// Limited: a plain `run` works, something asked for may not.
    Warn,
    /// A plain `run --no-image`, or `run IMAGE`, fails here.
    Fail,
}

impl Level {
    /// How it's said: `ok`, `warn` or `fail`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// How it came out.
    pub level: Level,
    /// What's checked: "network".
    pub name: &'static str,
    /// What was found, and the fix where it isn't `ok`: one finding a line
    /// (the firewall's may be several).
    pub text: String,
}

impl Check {
    /// A check `name` that came out `level`, saying `text`.
    #[must_use]
    pub fn new(level: Level, name: &'static str, text: impl Into<String>) -> Self {
        Check {
            level,
            name,
            text: text.into(),
        }
    }
}

/// Who ran the checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    /// Their user ID.
    pub uid: u32,
    /// Not root.
    pub rootless: bool,
}

/// What `doctor` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The operating system (`linux`, `macos`).
    pub os: &'static str,
    /// The architecture (`aarch64`, `x86_64`).
    pub arch: &'static str,
    /// The kernel's release (Linux).
    pub kernel: Option<String>,
    /// How containers run here: `native (host kernel)`, `linux-vm` (a
    /// Mac: its CLI says which VM), or `unknown`.
    pub backend: &'static str,
    /// Who ran the checks (Linux).
    pub user: Option<User>,
    /// Off Linux, how commands are isolated: not at all (here), or, a
    /// Mac's CLI says, in Shipyard's VM.
    pub isolation: Option<&'static str>,
    /// The checks, in the order a run meets them (none off Linux: a Mac's
    /// CLI adds its VM's).
    pub checks: Vec<Check>,
}

/// The checks' sum: what it says, and whether a check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// "containers run here", or what's limited, or what's to fix.
    pub text: String,
    /// A check failed: containers can't run here as they are.
    pub failed: bool,
}

/// What `checks` add up to.
#[must_use]
pub fn verdict(checks: &[Check]) -> Verdict {
    let count = |level| checks.iter().filter(|check| check.level == level).count();
    let (fails, warns) = (count(Level::Fail), count(Level::Warn));
    if fails > 0 {
        Verdict {
            text: format!(
                "containers can't run here as they are: {fails} {} to fix",
                plural(fails, "problem", "problems")
            ),
            failed: true,
        }
    } else if warns > 0 {
        Verdict {
            text: format!(
                "containers run here; {warns} {} limited",
                plural(warns, "thing", "things")
            ),
            failed: false,
        }
    } else {
        Verdict {
            text: "containers run here".to_owned(),
            failed: false,
        }
    }
}

/// `one` for 1, else `many`.
fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Checks this host, as a run would meet it (on Linux; elsewhere, says why
/// there's nothing to check yet). On Linux it starts the probe helper (this
/// binary again): the calling binary must run
/// [`entry_points`](crate::entry_points) first thing in `main`, or the
/// probe's line says it couldn't try what a run needs.
#[must_use]
pub fn checks(engine: &Engine) -> Report {
    let backend = match shipyard_runtime::host_backend() {
        shipyard_runtime::Backend::Native => "native (host kernel)",
        shipyard_runtime::Backend::LinuxVm => "linux-vm",
        _ => "unknown",
    };
    let mut report = Report {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        kernel: None,
        backend,
        user: None,
        isolation: None,
        checks: Vec::new(),
    };
    #[cfg(target_os = "linux")]
    {
        report.kernel = Some(shipyard_runtime::kernel::release());
        let (user, checks) = linux::checks(engine);
        report.user = Some(user);
        report.checks = checks;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        report.isolation = Some(
            "none here: commands run as ordinary processes (a Mac's containers run in \
             Shipyard's VM: `shipyard doctor` with the VM on checks it)",
        );
    }
    report
}

/// The format of [`GuestDoctor`], written in it from the first (H2).
pub const GUEST_DOCTOR_VERSION: u32 = 1;

/// The macOS guest's own checks, as its `__guest-doctor` prints them for
/// the Mac's `doctor` (stage 10's close): the kernel, and each check's
/// level, name and text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestDoctor {
    /// [`GUEST_DOCTOR_VERSION`] when written.
    pub version: u32,
    /// The guest kernel's release.
    pub kernel: Option<String>,
    /// The checks, in order.
    pub checks: Vec<GuestCheck>,
}

/// One of [`GuestDoctor`]'s checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestCheck {
    /// How it came out.
    pub level: Level,
    /// What's checked.
    pub name: String,
    /// What was found.
    pub text: String,
}

/// Why a [`GuestDoctor`] can't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GuestDoctorError {
    /// A newer version than this reads.
    #[error(
        "the guest's checks are of version {0}, newer than this shipyard's {GUEST_DOCTOR_VERSION}"
    )]
    Newer(u32),
    /// Not this format.
    #[error("the guest's checks can't be read: {0}")]
    Unreadable(String),
    /// They can't be written (they always can).
    #[error("the guest's checks can't be written: {0}")]
    Unwritable(String),
}

impl GuestDoctor {
    /// `report`'s kernel and checks, at this version.
    #[must_use]
    pub fn of(report: &Report) -> Self {
        Self {
            version: GUEST_DOCTOR_VERSION,
            kernel: report.kernel.clone(),
            checks: report
                .checks
                .iter()
                .map(|check| GuestCheck {
                    level: check.level,
                    name: check.name.to_owned(),
                    text: check.text.clone(),
                })
                .collect(),
        }
    }

    /// Whether a check failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| check.level == Level::Fail)
    }

    /// It, as its output carries it.
    ///
    /// # Errors
    ///
    /// [`GuestDoctorError::Unwritable`] if it can't be written (it always
    /// can).
    pub fn encode(&self) -> std::result::Result<Vec<u8>, GuestDoctorError> {
        serde_json::to_vec(self).map_err(|e| GuestDoctorError::Unwritable(e.to_string()))
    }

    /// One read back.
    ///
    /// # Errors
    ///
    /// [`GuestDoctorError::Newer`] for a newer version, else
    /// [`GuestDoctorError::Unreadable`].
    pub fn decode(bytes: &[u8]) -> std::result::Result<Self, GuestDoctorError> {
        #[derive(serde::Deserialize)]
        struct Versioned {
            version: u32,
        }
        let unreadable = |e: serde_json::Error| GuestDoctorError::Unreadable(e.to_string());
        let versioned: Versioned = serde_json::from_slice(bytes).map_err(unreadable)?;
        if versioned.version > GUEST_DOCTOR_VERSION {
            return Err(GuestDoctorError::Newer(versioned.version));
        }
        serde_json::from_slice(bytes).map_err(unreadable)
    }
}

#[cfg(test)]
mod tests;
