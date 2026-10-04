//! Resource limits (`setrlimit`, TLPI ch. 36), in the shapes Docker and OCI use.

use std::fmt;
use std::str::FromStr;

/// Which resource a limit applies to.
///
/// The variants mirror OCI's `RLIMIT_*` names. Six of them exist only on Linux
/// and are rejected when parsed on other platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RlimitKind {
    /// Address space (virtual memory) size, in bytes.
    As,
    /// Largest core dump file, in bytes. `0` disables core dumps.
    Core,
    /// CPU time, in seconds. The process gets `SIGXCPU` when it passes the soft limit.
    Cpu,
    /// Size of the data segment (heap), in bytes.
    Data,
    /// Largest file the process may create, in bytes.
    Fsize,
    /// Number of file locks (Linux only).
    Locks,
    /// Bytes of memory that may be locked into RAM.
    Memlock,
    /// Bytes in POSIX message queues (Linux only).
    Msgqueue,
    /// Ceiling for raising scheduling priority (Linux only).
    Nice,
    /// Number of open file descriptors.
    Nofile,
    /// Number of processes for the user.
    Nproc,
    /// Resident set size, in bytes. Linux only: macOS defines it as an alias
    /// of `As`, so accepting both would let one silently override the other.
    Rss,
    /// Ceiling on real-time scheduling priority (Linux only).
    Rtprio,
    /// CPU time under real-time scheduling, in microseconds (Linux only).
    Rttime,
    /// Number of queued signals (Linux only).
    Sigpending,
    /// Size of the main thread's stack, in bytes.
    Stack,
}

impl RlimitKind {
    const ALL: [Self; 16] = [
        Self::As,
        Self::Core,
        Self::Cpu,
        Self::Data,
        Self::Fsize,
        Self::Locks,
        Self::Memlock,
        Self::Msgqueue,
        Self::Nice,
        Self::Nofile,
        Self::Nproc,
        Self::Rss,
        Self::Rtprio,
        Self::Rttime,
        Self::Sigpending,
        Self::Stack,
    ];

    /// The Docker-style name, e.g. `nofile`. The OCI name is `RLIMIT_` plus
    /// the same word in capitals.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::As => "as",
            Self::Core => "core",
            Self::Cpu => "cpu",
            Self::Data => "data",
            Self::Fsize => "fsize",
            Self::Locks => "locks",
            Self::Memlock => "memlock",
            Self::Msgqueue => "msgqueue",
            Self::Nice => "nice",
            Self::Nofile => "nofile",
            Self::Nproc => "nproc",
            Self::Rss => "rss",
            Self::Rtprio => "rtprio",
            Self::Rttime => "rttime",
            Self::Sigpending => "sigpending",
            Self::Stack => "stack",
        }
    }

    /// Whether this platform's kernel has the limit.
    #[must_use]
    pub fn is_supported(self) -> bool {
        cfg!(target_os = "linux")
            || !matches!(
                self,
                Self::Locks
                    | Self::Msgqueue
                    | Self::Nice
                    | Self::Rss
                    | Self::Rtprio
                    | Self::Rttime
                    | Self::Sigpending
            )
    }
}

impl fmt::Display for RlimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Accepts Docker names (`nofile`) and OCI names (`RLIMIT_NOFILE`), in any case.
impl FromStr for RlimitKind {
    type Err = RlimitError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        let name = lower.strip_prefix("rlimit_").unwrap_or(&lower);
        let kind = Self::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| RlimitError::UnknownKind(s.to_owned()))?;
        if !kind.is_supported() {
            return Err(RlimitError::unsupported(kind));
        }
        Ok(kind)
    }
}

/// The largest finite limit: one below the kernel's "infinity" value, which is
/// `2^64 - 1` on Linux but `2^63 - 1` on macOS.
const MAX_FINITE: u64 = libc::RLIM_INFINITY - 1;

/// One side (soft or hard) of a limit.
///
/// The derived ordering compares variants in declaration order, so
/// `Unlimited` sorts above every finite value. Keep `Finite` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RlimitValue {
    /// A specific amount, in the resource's unit.
    Finite(u64),
    /// No limit.
    Unlimited,
}

impl fmt::Display for RlimitValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finite(n) => write!(f, "{n}"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

/// Accepts a number, `unlimited`, or `-1` (Docker's spelling of unlimited).
///
/// Finite values must stay below the kernel's "infinity" value, which is
/// `2^64 - 1` on Linux but `2^63 - 1` on macOS.
impl FromStr for RlimitValue {
    type Err = RlimitError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "-1" || s.eq_ignore_ascii_case("unlimited") {
            return Ok(Self::Unlimited);
        }
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(RlimitError::InvalidValue(s.to_owned()));
        }
        match s.parse::<u64>() {
            Ok(n) if n <= MAX_FINITE => Ok(Self::Finite(n)),
            _ => Err(RlimitError::TooLarge(s.to_owned())),
        }
    }
}

/// A resource limit: the soft limit is what the kernel enforces, and the hard
/// limit is the ceiling an unprivileged process may raise the soft limit to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rlimit {
    /// Which resource is limited.
    pub kind: RlimitKind,
    /// The limit the kernel enforces.
    pub soft: RlimitValue,
    /// The ceiling for the soft limit.
    pub hard: RlimitValue,
}

impl Rlimit {
    /// A validated limit; see [`Rlimit::validate`].
    pub fn new(
        kind: RlimitKind,
        soft: RlimitValue,
        hard: RlimitValue,
    ) -> Result<Self, RlimitError> {
        let limit = Self { kind, soft, hard };
        limit.validate()?;
        Ok(limit)
    }

    /// Rejects limits the kernel would refuse or misread: a kind this platform
    /// doesn't have, a finite value at or above the kernel's "infinity" (which
    /// it would silently treat as unlimited), or a soft limit above the hard one.
    pub fn validate(&self) -> Result<(), RlimitError> {
        if !self.kind.is_supported() {
            return Err(RlimitError::unsupported(self.kind));
        }
        for value in [self.soft, self.hard] {
            if let RlimitValue::Finite(n) = value
                && n > MAX_FINITE
            {
                return Err(RlimitError::TooLarge(n.to_string()));
            }
        }
        if cfg!(target_os = "linux")
            && self.kind == RlimitKind::Nofile
            && self.hard == RlimitValue::Unlimited
        {
            return Err(RlimitError::NofileUnlimited);
        }
        if self.soft > self.hard {
            return Err(RlimitError::SoftAboveHard {
                kind: self.kind,
                soft: self.soft,
                hard: self.hard,
            });
        }
        Ok(())
    }
}

/// Prints the `--ulimit` form, e.g. `nofile=1024:2048`.
impl fmt::Display for Rlimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}:{}", self.kind, self.soft, self.hard)
    }
}

/// Parses the `--ulimit` form: `name=soft[:hard]`. With only a soft value,
/// the hard limit is set to the same value.
impl FromStr for Rlimit {
    type Err = RlimitError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, values) = s
            .split_once('=')
            .ok_or_else(|| RlimitError::MissingValue(s.to_owned()))?;
        let (soft, hard) = values.split_once(':').unwrap_or((values, values));
        Self::new(kind.parse()?, soft.parse()?, hard.parse()?)
    }
}

/// Why a resource limit was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RlimitError {
    /// Not a resource name Docker or OCI uses.
    #[error("unknown resource limit \"{0}\" (try nofile, nproc, core, stack, …)")]
    UnknownKind(String),
    /// A real resource, but this platform's kernel doesn't have it.
    #[error("resource limit \"{kind}\" is not supported on {os}")]
    Unsupported {
        /// The resource.
        kind: RlimitKind,
        /// The host operating system.
        os: &'static str,
    },
    /// The text has no `=`, so it isn't `NAME=SOFT[:HARD]`.
    #[error("expected NAME=SOFT[:HARD], got \"{0}\"")]
    MissingValue(String),
    /// A value that isn't a number, `unlimited` or `-1`.
    #[error("invalid limit value \"{0}\" (expected a number, \"unlimited\" or -1)")]
    InvalidValue(String),
    /// A number at or above the platform's "infinity" value.
    #[error("limit value \"{0}\" is too large for this platform")]
    TooLarge(String),
    /// Linux caps open files at `fs.nr_open`, so it refuses "unlimited" even
    /// for root.
    #[error(
        "nofile can't be unlimited on Linux (the kernel caps it at fs.nr_open); \
         give a number, such as nofile=1048576"
    )]
    NofileUnlimited,
    /// The soft limit is higher than the hard limit, which the kernel refuses.
    #[error("soft limit {soft} is above hard limit {hard} for \"{kind}\"")]
    SoftAboveHard {
        /// The resource.
        kind: RlimitKind,
        /// The soft limit given.
        soft: RlimitValue,
        /// The hard limit given.
        hard: RlimitValue,
    },
}

impl RlimitError {
    pub(crate) fn unsupported(kind: RlimitKind) -> Self {
        Self::Unsupported {
            kind,
            os: std::env::consts::OS,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::RlimitValue::{Finite, Unlimited};
    use super::*;

    fn parse(s: &str) -> Result<Rlimit, RlimitError> {
        s.parse()
    }

    #[test]
    fn accepts_docker_and_oci_names_in_any_case() {
        for name in [
            "nofile",
            "NOFILE",
            "RLIMIT_NOFILE",
            "rlimit_nofile",
            "Rlimit_NoFile",
        ] {
            assert_eq!(name.parse(), Ok(RlimitKind::Nofile), "{name}");
        }
    }

    #[test]
    fn unlimited_nofile_is_refused_only_on_linux() {
        let parsed = parse("nofile=unlimited");
        if cfg!(target_os = "linux") {
            assert_eq!(parsed, Err(RlimitError::NofileUnlimited));
        } else {
            assert!(parsed.is_ok(), "{parsed:?}");
        }
        assert!(parse("core=unlimited").is_ok());
    }

    #[test]
    fn parses_soft_and_hard() {
        let limit = parse("nofile=1024:2048").unwrap();
        assert_eq!((limit.soft, limit.hard), (Finite(1024), Finite(2048)));
    }

    #[test]
    fn soft_only_sets_both() {
        let limit = parse("core=0").unwrap();
        assert_eq!((limit.soft, limit.hard), (Finite(0), Finite(0)));
    }

    #[test]
    fn unlimited_spellings() {
        for form in ["stack=unlimited", "stack=-1", "stack=UNLIMITED:unlimited"] {
            let limit = parse(form).unwrap();
            assert_eq!((limit.soft, limit.hard), (Unlimited, Unlimited), "{form}");
        }
        assert_eq!(parse("nofile=1024:-1").unwrap().hard, Unlimited);
    }

    #[test]
    fn soft_above_hard_is_rejected() {
        assert!(matches!(
            parse("nofile=10:5"),
            Err(RlimitError::SoftAboveHard { .. })
        ));
        assert!(matches!(
            parse("nofile=unlimited:5"),
            Err(RlimitError::SoftAboveHard { .. })
        ));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(matches!(parse("nofile"), Err(RlimitError::MissingValue(_))));
        assert!(matches!(parse("bogus=1"), Err(RlimitError::UnknownKind(_))));
        for bad in [
            "nofile=",
            "nofile=:",
            "nofile=1:",
            "nofile=+1",
            "nofile= 1",
            "nofile=1.5",
            "nofile=-2",
        ] {
            assert!(
                matches!(parse(bad), Err(RlimitError::InvalidValue(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            parse("nofile=18446744073709551616"),
            Err(RlimitError::TooLarge(_))
        ));
    }

    #[test]
    fn validate_catches_hand_built_values_at_infinity() {
        let limit = Rlimit {
            kind: RlimitKind::Core,
            soft: Finite(u64::MAX),
            hard: Finite(u64::MAX),
        };
        assert!(matches!(limit.validate(), Err(RlimitError::TooLarge(_))));
    }

    #[test]
    fn rejects_values_at_the_platform_infinity() {
        let infinity = libc::RLIM_INFINITY.to_string();
        assert!(matches!(
            infinity.parse::<RlimitValue>(),
            Err(RlimitError::TooLarge(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn linux_only_kinds_are_rejected_on_macos() {
        for name in [
            "locks",
            "msgqueue",
            "rss",
            "nice",
            "rtprio",
            "rttime",
            "sigpending",
        ] {
            assert!(
                matches!(
                    name.parse::<RlimitKind>(),
                    Err(RlimitError::Unsupported { .. })
                ),
                "{name}"
            );
        }
        assert!(matches!(
            "9223372036854775807".parse::<RlimitValue>(),
            Err(RlimitError::TooLarge(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_only_kinds_are_accepted_on_linux() {
        assert_eq!("rtprio".parse(), Ok(RlimitKind::Rtprio));
    }

    proptest! {
        #[test]
        fn parsing_never_panics(s in any::<String>()) {
            let _ = parse(&s);
        }

        #[test]
        fn valid_limits_round_trip(soft in 0u64..1 << 40, extra in 0u64..1 << 40) {
            let limit = Rlimit::new(RlimitKind::Nofile, Finite(soft), Finite(soft + extra)).unwrap();
            prop_assert_eq!(parse(&limit.to_string()), Ok(limit));
        }
    }
}
