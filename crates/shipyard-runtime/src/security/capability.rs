//! Linux capabilities: root's power, split into about forty separate
//! privileges (`capabilities(7)`, TLPI ch. 39). A process holds five sets of
//! them; Shipyard gives a container's command only the ones it should have.
//!
//! The names are the kernel's, so they parse and print the same on macOS,
//! where nothing applies them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One capability, by its kernel number (`CAP_CHOWN` is 0).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Capability(u8);

/// How many capabilities Shipyard knows: `CAP_CHOWN` (0) to
/// `CAP_CHECKPOINT_RESTORE` (40). Kernels add new ones at the end; one past
/// this can't be named.
const COUNT: u8 = 41;

/// The capabilities, in kernel order: the index is the number.
const NAMES: [&str; COUNT as usize] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

/// Why a capability name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CapabilityError {
    /// Not a capability this table knows.
    #[error("unknown capability \"{0}\" (names look like CAP_NET_ADMIN or NET_ADMIN)")]
    Unknown(String),
}

impl Capability {
    /// `CAP_KILL`: signal processes of other users.
    pub const KILL: Self = Self(5);
    /// `CAP_MKNOD`: make device nodes.
    pub const MKNOD: Self = Self(27);
    /// `CAP_SYS_PTRACE`: trace any process, and read a non-dumpable one's
    /// memory.
    pub const SYS_PTRACE: Self = Self(19);

    /// The capability with kernel number `number`, if this table knows it.
    #[must_use]
    pub fn from_number(number: u8) -> Option<Self> {
        (number < COUNT).then_some(Self(number))
    }

    /// Its kernel number.
    #[must_use]
    pub fn number(self) -> u8 {
        self.0
    }

    /// Its name, like `CAP_NET_ADMIN`.
    #[must_use]
    pub fn name(self) -> &'static str {
        NAMES
            .get(usize::from(self.0))
            .copied()
            .unwrap_or("CAP_UNKNOWN")
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Debug for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Capability({})", self.name())
    }
}

/// `CAP_NET_ADMIN`, `NET_ADMIN` or `net_admin`: Docker accepts all three.
impl FromStr for Capability {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let upper = s.to_ascii_uppercase();
        let name = if upper.starts_with("CAP_") {
            upper
        } else {
            format!("CAP_{upper}")
        };
        NAMES
            .iter()
            .position(|known| *known == name)
            .and_then(|index| u8::try_from(index).ok())
            .map(Self)
            .ok_or_else(|| CapabilityError::Unknown(s.to_owned()))
    }
}

/// A set of capabilities, as the kernel keeps them: one bit per number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(u64);

impl CapabilitySet {
    /// No capabilities.
    pub const EMPTY: Self = Self(0);

    /// Every capability Shipyard can name.
    pub const ALL: Self = Self((1 << COUNT) - 1);

    /// Docker's default set for a container's process: enough for ordinary
    /// programs that start as root (changing file owners, switching users,
    /// binding low ports), and none of the ones that reach the host (no
    /// `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, `CAP_SYS_MODULE`, `CAP_SYS_PTRACE`…).
    pub const DOCKER_DEFAULT: Self = Self::of(&[
        0,  // CAP_CHOWN
        1,  // CAP_DAC_OVERRIDE
        3,  // CAP_FOWNER
        4,  // CAP_FSETID
        5,  // CAP_KILL
        6,  // CAP_SETGID
        7,  // CAP_SETUID
        8,  // CAP_SETPCAP
        10, // CAP_NET_BIND_SERVICE
        13, // CAP_NET_RAW
        18, // CAP_SYS_CHROOT
        27, // CAP_MKNOD
        29, // CAP_AUDIT_WRITE
        31, // CAP_SETFCAP
    ]);

    /// A helper's (`cp`'s): reading and writing any file, and giving it
    /// its owner and mode; nothing else.
    #[cfg(target_os = "linux")]
    pub(crate) const HELPER: Self = Self::of(&[
        0, // CAP_CHOWN
        1, // CAP_DAC_OVERRIDE
        2, // CAP_DAC_READ_SEARCH
        3, // CAP_FOWNER
        4, // CAP_FSETID
    ]);

    const fn of(numbers: &[u8]) -> Self {
        let mut bits = 0_u64;
        let mut i = 0;
        while i < numbers.len() {
            bits |= 1 << numbers[i];
            i += 1;
        }
        Self(bits)
    }

    /// The set as the kernel's bit mask (bit N is capability N).
    #[must_use]
    pub fn bits(self) -> u64 {
        self.0
    }

    /// Whether `capability` is in the set.
    #[must_use]
    pub fn contains(self, capability: Capability) -> bool {
        self.0 & (1 << capability.0) != 0
    }

    /// Adds `capability`.
    pub fn insert(&mut self, capability: Capability) {
        self.0 |= 1 << capability.0;
    }

    /// Takes `capability` out.
    pub fn remove(&mut self, capability: Capability) {
        self.0 &= !(1 << capability.0);
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The capabilities in the set, in kernel order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        (0..COUNT)
            .map(Capability)
            .filter(move |capability| self.contains(*capability))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(capabilities: I) -> Self {
        let mut set = Self::EMPTY;
        for capability in capabilities {
            set.insert(capability);
        }
        set
    }
}

/// Lists the names: `{CAP_CHOWN, CAP_KILL}`.
impl fmt::Debug for CapabilitySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set()
            .entries(self.iter().map(Capability::name))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_in_every_spelling() {
        for (number, name) in NAMES.iter().enumerate() {
            let capability: Capability = name.parse().unwrap();
            assert_eq!(usize::from(capability.number()), number);
            assert_eq!(capability.to_string(), *name);
            let short = name.trim_start_matches("CAP_");
            assert_eq!(short.parse::<Capability>(), Ok(capability));
            assert_eq!(
                short.to_ascii_lowercase().parse::<Capability>(),
                Ok(capability)
            );
        }
        assert!("CAP_NOPE".parse::<Capability>().is_err());
        assert!("".parse::<Capability>().is_err());
        assert!("CAP_".parse::<Capability>().is_err());
    }

    #[test]
    fn the_kernel_numbers_are_right() {
        // Spot checks against linux/capability.h.
        for (name, number) in [
            ("CAP_CHOWN", 0),
            ("CAP_SETPCAP", 8),
            ("CAP_NET_BIND_SERVICE", 10),
            ("CAP_SYS_PTRACE", 19),
            ("CAP_SYS_ADMIN", 21),
            ("CAP_SETFCAP", 31),
            ("CAP_CHECKPOINT_RESTORE", 40),
        ] {
            assert_eq!(
                name.parse::<Capability>().unwrap().number(),
                number,
                "{name}"
            );
        }
        assert_eq!(Capability::from_number(41), None);
    }

    #[test]
    fn docker_default_is_its_fourteen() {
        let names: Vec<_> = CapabilitySet::DOCKER_DEFAULT
            .iter()
            .map(Capability::name)
            .collect();
        assert_eq!(names.len(), 14, "{names:?}");
        for absent in [
            "CAP_SYS_ADMIN",
            "CAP_NET_ADMIN",
            "CAP_SYS_PTRACE",
            "CAP_SYS_MODULE",
        ] {
            assert!(!names.contains(&absent), "{absent}");
        }
        assert_eq!(CapabilitySet::ALL.iter().count(), NAMES.len());
    }

    #[test]
    fn sets_add_and_remove() {
        let admin: Capability = "SYS_ADMIN".parse().unwrap();
        let mut set = CapabilitySet::EMPTY;
        assert!(set.is_empty());
        set.insert(admin);
        assert!(set.contains(admin));
        assert_eq!(set.bits(), 1 << 21);
        set.remove(admin);
        assert!(set.is_empty());
        let collected: CapabilitySet = CapabilitySet::DOCKER_DEFAULT.iter().collect();
        assert_eq!(collected, CapabilitySet::DOCKER_DEFAULT);
    }
}
