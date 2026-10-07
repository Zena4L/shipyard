//! Which devices a container may use (stage 5, step 7): rules, as cgroup
//! v1's `devices.allow` and runc take them, and the nodes made in its `/dev`.
//!
//! On cgroup v2 the rules become an eBPF program attached to the container's
//! cgroup (`program.rs`; loaded by `cgroup/devices.rs`). The kernel runs it
//! on every `open()` and `mknod()` of a device node by a process in the
//! cgroup, and a 0 from it makes the call fail with EPERM (`cgroups(7)`,
//! `bpf(2)`'s `BPF_PROG_TYPE_CGROUP_DEVICE`).
//!
//! **How rules combine** (as cgroup v1 had it, and runc keeps): later rules
//! win, access bit by access bit. A request (`open(O_RDWR)` asks to read and
//! write) is allowed only if, for each bit it asks for, the last rule that
//! covers that device and bit allows it. A bit no rule covers is denied.

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux loads device programs")
)]
mod ebpf;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux loads device programs")
)]
mod program;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub(crate) use ebpf::Instruction;
#[cfg(target_os = "linux")]
pub(crate) use program::program;

/// The largest major number the kernel has (12 bits), and minor (20 bits).
pub const MAX_MAJOR: u32 = (1 << 12) - 1;
/// See [`MAX_MAJOR`].
pub const MAX_MINOR: u32 = (1 << 20) - 1;

/// The most rules a container may have. The program takes up to eight
/// instructions per rule and access bit, and the verifier's work grows with
/// it; real lists are a dozen or so, and a node each for `--device`.
pub const MAX_RULES: usize = 256;

/// A kind of device node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DeviceKind {
    /// A character device (`c`): a terminal, `/dev/null`, a GPU.
    Char,
    /// A block device (`b`): a disk.
    Block,
}

impl DeviceKind {
    /// The kernel's number for it in a device program's context
    /// (`BPF_DEVCG_DEV_BLOCK`, `BPF_DEVCG_DEV_CHAR`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux loads device programs")
    )]
    pub(crate) fn code(self) -> u32 {
        match self {
            Self::Block => 1,
            Self::Char => 2,
        }
    }

    /// Its letter: `c` or `b`.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::Char => 'c',
            Self::Block => 'b',
        }
    }
}

/// What a rule covers or a request asks for: any of read, write and
/// mknod, the kernel's bits (`BPF_DEVCG_ACC_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Access(u8);

impl Access {
    /// Making a node of it (`mknod`).
    pub const MKNOD: Self = Self(1);
    /// Opening it for reading.
    pub const READ: Self = Self(2);
    /// Opening it for writing.
    pub const WRITE: Self = Self(4);
    /// All three: `rwm`.
    pub const ALL: Self = Self(7);

    /// The access a string of `r`, `w` and `m` says (each at most once, in
    /// any order).
    ///
    /// # Errors
    ///
    /// An empty string, another letter, or one twice.
    pub fn parse(text: &str) -> Result<Self, DeviceError> {
        let bad = || DeviceError::BadAccess(text.to_owned());
        if text.is_empty() {
            return Err(bad());
        }
        let mut access = Self::default();
        for letter in text.chars() {
            let bit = match letter {
                'r' => Self::READ,
                'w' => Self::WRITE,
                'm' => Self::MKNOD,
                _ => return Err(bad()),
            };
            if access.contains(bit) {
                return Err(bad());
            }
            access = access.union(bit);
        }
        Ok(access)
    }

    /// Both.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit of `other` is in it.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether it has no bit.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The kernel's bits.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux loads device programs")
    )]
    pub(crate) fn bits(self) -> u32 {
        u32::from(self.0)
    }

    /// From the kernel's bits (only the three it defines).
    #[cfg(test)]
    pub(crate) fn from_bits(bits: u32) -> Self {
        Self(u8::try_from(bits & 7).unwrap_or(0))
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (bit, letter) in [(Self::READ, 'r'), (Self::WRITE, 'w'), (Self::MKNOD, 'm')] {
            if self.contains(bit) {
                write!(f, "{letter}")?;
            }
        }
        Ok(())
    }
}

/// One rule: whether the devices it names may be used, for its access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeviceRule {
    /// Allowed (`true`) or denied.
    pub allow: bool,
    /// The kind it covers; `None` for both (`a`).
    pub kind: Option<DeviceKind>,
    /// The major number it covers; `None` for any (`*`).
    pub major: Option<u32>,
    /// The minor number it covers; `None` for any (`*`).
    pub minor: Option<u32>,
    /// What it allows or denies.
    pub access: Access,
}

impl DeviceRule {
    /// Allows (`allow`) or denies `access` to `kind` `major`:`minor`
    /// (`None`: any).
    #[must_use]
    pub fn new(
        allow: bool,
        kind: Option<DeviceKind>,
        major: Option<u32>,
        minor: Option<u32>,
        access: Access,
    ) -> Self {
        Self {
            allow,
            kind,
            major,
            minor,
            access,
        }
    }

    /// Allows `access` to `kind` `major`:`minor` (`None`: any).
    #[must_use]
    pub fn allow(
        kind: Option<DeviceKind>,
        major: Option<u32>,
        minor: Option<u32>,
        access: Access,
    ) -> Self {
        Self::new(true, kind, major, minor, access)
    }

    /// Denies everything: the list's usual start.
    #[must_use]
    pub fn deny_all() -> Self {
        Self::new(false, None, None, None, Access::ALL)
    }

    /// Whether it covers every device (`a *:*`).
    #[must_use]
    pub fn covers_every_device(&self) -> bool {
        self.kind.is_none() && self.major.is_none() && self.minor.is_none()
    }

    /// Whether it covers the device `kind` `major`:`minor`.
    fn covers(&self, kind: DeviceKind, major: u32, minor: u32) -> bool {
        self.kind.is_none_or(|own| own == kind)
            && self.major.is_none_or(|own| own == major)
            && self.minor.is_none_or(|own| own == minor)
    }
}

impl std::str::FromStr for DeviceRule {
    type Err = DeviceError;

    /// An allow rule in Docker's `--device-cgroup-rule` syntax: `TYPE
    /// MAJOR:MINOR ACCESS`, with `a` for both types and `*` for any number
    /// (`c 1:3 rwm`, `b *:* m`, `a *:* rwm`).
    fn from_str(text: &str) -> Result<Self, DeviceError> {
        let bad = || DeviceError::BadRule(text.to_owned());
        let mut fields = text.split_whitespace();
        let (Some(kind), Some(numbers), Some(access), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad());
        };
        let kind = match kind {
            "a" => None,
            "c" => Some(DeviceKind::Char),
            "b" => Some(DeviceKind::Block),
            _ => return Err(bad()),
        };
        let (major, minor) = numbers.split_once(':').ok_or_else(bad)?;
        let number = |field: &str| match field {
            "*" => Ok(None),
            digits => digits.parse().map(Some).map_err(|_| bad()),
        };
        Ok(Self::allow(
            kind,
            number(major)?,
            number(minor)?,
            Access::parse(access)?,
        ))
    }
}

impl std::fmt::Display for DeviceRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let number = |n: Option<u32>| n.map_or_else(|| "*".to_owned(), |n| n.to_string());
        write!(
            f,
            "{} {} {}:{} {}",
            if self.allow { "allow" } else { "deny" },
            self.kind.map_or('a', DeviceKind::letter),
            number(self.major),
            number(self.minor),
            self.access
        )
    }
}

/// A device node made in the container, at `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeviceNode {
    /// Where, inside the container (absolute).
    pub path: PathBuf,
    /// Its kind.
    pub kind: DeviceKind,
    /// Its major number.
    pub major: u32,
    /// Its minor number.
    pub minor: u32,
    /// Its permission bits (`0o666`, say).
    pub mode: u32,
    /// Its owner, inside the container.
    pub uid: u32,
    /// Its group, inside the container.
    pub gid: u32,
    /// Where it is on the host, if not at `path`: in a user namespace,
    /// where `mknod` is refused, the host's node is bound in from there.
    pub source: Option<PathBuf>,
}

impl std::fmt::Display for DeviceNode {
    /// `/dev/fuse (c 10:229)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} {}:{})",
            self.path.display(),
            self.kind.letter(),
            self.major,
            self.minor
        )
    }
}

impl DeviceNode {
    /// The node `kind` `major`:`minor` at `path`, root's, read-write for
    /// everyone (`0o666`, as Docker makes them unless told otherwise).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, kind: DeviceKind, major: u32, minor: u32) -> Self {
        Self {
            path: path.into(),
            kind,
            major,
            minor,
            mode: 0o666,
            uid: 0,
            gid: 0,
            source: None,
        }
    }

    /// The rule that lets it be used for `access`.
    #[must_use]
    pub fn rule(&self, access: Access) -> DeviceRule {
        DeviceRule::allow(Some(self.kind), Some(self.major), Some(self.minor), access)
    }
}

/// A container's devices: its rules and the nodes made in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Devices {
    /// The rules, in order (see the module's docs).
    pub rules: Vec<DeviceRule>,
    /// Nodes made in the container, beyond Shipyard's own `/dev`.
    pub nodes: Vec<DeviceNode>,
    /// Whether rules or nodes were asked for (a flag, a bundle's fields), not
    /// just the defaults: where they can't be enforced (rootless), that's
    /// worth a warning.
    pub asked: bool,
}

/// runc's `AllowedDevices`, which it appends to every container's rules:
/// `mknod` of any node, then read, write and mknod for `null`, `random`,
/// `full`, `tty`, `zero`, `urandom`, the container's own terminals
/// (`/dev/pts/*`, `ptmx`), and `/dev/net/tun` (runc keeps it "for backward
/// compatibility"). They cover every node Shipyard makes in `/dev`.
fn runc_allowed() -> [DeviceRule; 11] {
    let mknod = |kind| DeviceRule::allow(Some(kind), None, None, Access::MKNOD);
    let char = |major, minor: Option<u32>| {
        DeviceRule::allow(Some(DeviceKind::Char), Some(major), minor, Access::ALL)
    };
    [
        mknod(DeviceKind::Char),
        mknod(DeviceKind::Block),
        char(1, Some(3)),
        char(1, Some(8)),
        char(1, Some(7)),
        char(5, Some(0)),
        char(1, Some(5)),
        char(1, Some(9)),
        char(136, None),
        char(5, Some(2)),
        char(10, Some(200)),
    ]
}

impl Devices {
    /// Docker's defaults, as Docker on runc ends up with them: everything
    /// denied, then runc's own list (see `runc_allowed`).
    #[must_use]
    pub fn docker_default() -> Self {
        Self::bundle(vec![DeviceRule::deny_all()], Vec::new()).not_asked()
    }

    /// A bundle's: its `rules` exactly, then runc's own list, as runc builds
    /// them. Its `nodes` get no rule of their own: a bundle allows them in
    /// its rules (Docker and Podman write both), and a read-only or denied
    /// one stays so. `asked` if it gave either.
    #[must_use]
    pub fn bundle(rules: Vec<DeviceRule>, nodes: Vec<DeviceNode>) -> Self {
        let asked = !rules.is_empty() || !nodes.is_empty();
        let mut all = rules;
        all.extend(runc_allowed());
        Self {
            rules: all,
            nodes,
            asked,
        }
    }

    /// The same, as defaults: nothing asked for.
    fn not_asked(self) -> Self {
        Self {
            asked: false,
            ..self
        }
    }

    /// Whether the rules allow `access` to `kind` `major`:`minor`: the
    /// meaning the generated program must have (see the module's docs).
    #[must_use]
    pub fn allows(&self, kind: DeviceKind, major: u32, minor: u32, access: Access) -> bool {
        [Access::READ, Access::WRITE, Access::MKNOD]
            .into_iter()
            .filter(|&bit| access.contains(bit))
            .all(|bit| {
                self.rules
                    .iter()
                    .rev()
                    .find(|rule| rule.covers(kind, major, minor) && rule.access.contains(bit))
                    .is_some_and(|rule| rule.allow)
            })
    }

    /// Why they can't be used, if they can't.
    ///
    /// # Errors
    ///
    /// Too many rules, a rule without access, or a node whose path isn't
    /// absolute and plain (no `.` or `..`).
    pub fn validate(&self) -> Result<(), DeviceError> {
        if self.rules.len() > MAX_RULES {
            return Err(DeviceError::TooManyRules(self.rules.len()));
        }
        if let Some(rule) = self.rules.iter().find(|rule| rule.access.is_empty()) {
            return Err(DeviceError::BadRule(rule.to_string()));
        }
        let numbers =
            self.rules
                .iter()
                .flat_map(|rule| [(rule.major, MAX_MAJOR), (rule.minor, MAX_MINOR)])
                .chain(self.nodes.iter().flat_map(|node| {
                    [(Some(node.major), MAX_MAJOR), (Some(node.minor), MAX_MINOR)]
                }));
        for (number, most) in numbers {
            if let Some(number) = number.filter(|&number| number > most) {
                return Err(DeviceError::BadNumber(number));
            }
        }
        match self
            .nodes
            .iter()
            .find(|node| !is_plain_absolute(&node.path))
        {
            Some(node) => Err(DeviceError::BadPath(node.path.clone())),
            None => Ok(()),
        }
    }
}

/// An absolute path with no `.` or `..` in it.
fn is_plain_absolute(path: &Path) -> bool {
    use std::path::Component;
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && path != Path::new("/")
}

/// Why devices can't be used as given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeviceError {
    /// An access that isn't letters from `rwm`, each once.
    #[error("{0:?} isn't an access: one or more of r, w and m")]
    BadAccess(String),
    /// A rule that can't be read, or allows nothing.
    #[error("{0:?} isn't a device rule (such as \"c 1:3 rwm\" or \"b *:* m\")")]
    BadRule(String),
    /// A device number past the kernel's (12-bit majors, 20-bit minors).
    #[error(
        "{0} isn't a device number the kernel has (majors to {MAX_MAJOR}, minors to {MAX_MINOR})"
    )]
    BadNumber(u32),
    /// More rules than [`MAX_RULES`].
    #[error("{0} device rules are too many (at most {MAX_RULES})")]
    TooManyRules(usize),
    /// A node's path that isn't absolute and plain.
    #[error("a device's path must be absolute, without . or .. (got {})", .0.display())]
    BadPath(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    const NULL: (DeviceKind, u32, u32) = (DeviceKind::Char, 1, 3);
    const DISK: (DeviceKind, u32, u32) = (DeviceKind::Block, 8, 0);

    fn allows(
        devices: &Devices,
        (kind, major, minor): (DeviceKind, u32, u32),
        access: &str,
    ) -> bool {
        devices.allows(kind, major, minor, Access::parse(access).unwrap())
    }

    #[test]
    fn rules_read_as_docker_writes_them() {
        let rule: DeviceRule = "c 1:3 rwm".parse().unwrap();
        assert_eq!(
            rule,
            DeviceRule::allow(Some(DeviceKind::Char), Some(1), Some(3), Access::ALL)
        );
        let any: DeviceRule = "a *:* m".parse().unwrap();
        assert_eq!((any.kind, any.major, any.minor), (None, None, None));
        assert_eq!(any.access, Access::MKNOD);
        assert_eq!(
            "b 8:* rw".parse::<DeviceRule>().unwrap().to_string(),
            "allow b 8:* rw"
        );
        for bad in [
            "",
            "c 1:3",
            "x 1:3 r",
            "c 1-3 r",
            "c 1:3 rx",
            "c 1:3 rr",
            "c a:3 r",
            "c 1:3 r extra",
        ] {
            assert!(bad.parse::<DeviceRule>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn dockers_defaults() {
        let devices = Devices::docker_default();
        assert!(allows(&devices, NULL, "rw"));
        assert!(
            allows(&devices, (DeviceKind::Char, 136, 4), "rw"),
            "a terminal"
        );
        assert!(
            allows(&devices, (DeviceKind::Char, 10, 200), "rw"),
            "tun, as runc keeps"
        );
        assert!(
            !allows(&devices, (DeviceKind::Char, 5, 1), "r"),
            "not the console"
        );
        assert!(allows(&devices, DISK, "m"), "a node may be made");
        assert!(!allows(&devices, DISK, "r"), "but not opened");
        assert!(!allows(&devices, DISK, "rm"));
        assert!(
            !allows(&devices, (DeviceKind::Char, 1, 11), "r"),
            "/dev/kmsg"
        );
        assert!(!devices.asked);
    }

    #[test]
    fn later_rules_win_bit_by_bit() {
        let mut devices = Devices::bundle(Vec::new(), Vec::new());
        devices.rules = vec![
            DeviceRule::allow(None, None, None, Access::ALL),
            DeviceRule::new(false, Some(DeviceKind::Block), Some(8), None, Access::WRITE),
        ];
        assert!(allows(&devices, DISK, "r"));
        assert!(!allows(&devices, DISK, "w"));
        assert!(
            !allows(&devices, DISK, "rw"),
            "every bit asked must be allowed"
        );
        assert!(allows(&devices, (DeviceKind::Block, 9, 0), "rw"));
        devices.rules.clear();
        assert!(!allows(&devices, NULL, "r"), "nothing covers it: denied");
    }

    #[test]
    fn a_bundle_gets_runcs_list_after_its_rules_and_nothing_for_its_nodes() {
        // Docker's `--device /dev/sdb:/dev/xvdc:r`, as it writes it.
        let node = DeviceNode::new("/dev/xvdc", DeviceKind::Block, 8, 16);
        let read = DeviceRule::allow(Some(DeviceKind::Block), Some(8), Some(16), Access::READ);
        let devices = Devices::bundle(vec![DeviceRule::deny_all(), read], vec![node]);
        assert!(devices.asked);
        assert!(allows(&devices, (DeviceKind::Block, 8, 16), "r"));
        assert!(
            !allows(&devices, (DeviceKind::Block, 8, 16), "w"),
            "read-only stays so"
        );
        assert!(allows(&devices, NULL, "rwm"), "Shipyard's own /dev");
        assert!(allows(&devices, DISK, "m"), "mknod, as runc allows");
        // A node its rules don't allow can't be used.
        let fuse = DeviceNode::new("/dev/fuse", DeviceKind::Char, 10, 229);
        let devices = Devices::bundle(vec![DeviceRule::deny_all()], vec![fuse]);
        assert!(!allows(&devices, (DeviceKind::Char, 10, 229), "r"));
        assert!(!Devices::bundle(Vec::new(), Vec::new()).asked);
    }

    #[test]
    fn devices_are_checked() {
        let mut devices = Devices::docker_default();
        assert_eq!(devices.validate(), Ok(()));
        devices
            .nodes
            .push(DeviceNode::new("dev/x", DeviceKind::Char, 1, 1));
        assert!(matches!(devices.validate(), Err(DeviceError::BadPath(_))));
        devices.nodes = vec![DeviceNode::new("/dev/../x", DeviceKind::Char, 1, 1)];
        assert!(matches!(devices.validate(), Err(DeviceError::BadPath(_))));
        devices.nodes = vec![DeviceNode::new(
            "/dev/x",
            DeviceKind::Char,
            MAX_MAJOR + 1,
            0,
        )];
        assert_eq!(
            devices.validate(),
            Err(DeviceError::BadNumber(MAX_MAJOR + 1))
        );
        devices.nodes.clear();
        devices.rules = vec!["c 1:2147483648 r".parse().unwrap()];
        assert_eq!(
            devices.validate(),
            Err(DeviceError::BadNumber(2_147_483_648))
        );
        devices.rules = vec![DeviceRule::deny_all(); MAX_RULES + 1];
        assert_eq!(
            devices.validate(),
            Err(DeviceError::TooManyRules(MAX_RULES + 1))
        );
    }
}
