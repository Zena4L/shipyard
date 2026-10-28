//! A running container's isolation as the kernel has it (step 14.1b): the
//! desktop UI's inspector shows it ("show the kernel"). Each of the eight
//! namespace kinds, on (not the host's: the API service's own namespaces,
//! the host's on Linux and the guest's on a Mac) or off, with its ID (two
//! containers that share one, `--network container:`, have the same); its
//! cgroup; its command (the init's child: Shipyard's init is PID 1, the
//! command PID 2, unless there's no PID namespace) and the host PIDs of
//! both; its user mapping. Read from `/proc` through its init, found by PID
//! and start time, which is checked again once read: a PID reused meanwhile
//! is never read as the container's. A stopped container has none of this:
//! refused, never guessed from its configuration (D14.1b-1).

use serde::Serialize;

use crate::Engine;
use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::error::{Error, ErrorKind};

/// The answer's version (`GET /shipyard/containers/{id}/isolation`).
pub const ISOLATION_VERSION: u32 = 1;

/// A namespace kind, as `/proc/PID/ns` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NamespaceKind {
    /// Its cgroup root.
    Cgroup,
    /// System V IPC and POSIX message queues.
    Ipc,
    /// Its mounts.
    Mnt,
    /// Its network.
    Net,
    /// Its process IDs.
    Pid,
    /// Its user and group IDs.
    User,
    /// Its host and domain names.
    Uts,
    /// Its clocks (Linux 5.6).
    Time,
}

impl NamespaceKind {
    /// Every kind, in the order the answer says them.
    pub const ALL: [Self; 8] = [
        Self::Cgroup,
        Self::Ipc,
        Self::Mnt,
        Self::Net,
        Self::Pid,
        Self::User,
        Self::Uts,
        Self::Time,
    ];

    /// Its name in `/proc/PID/ns`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cgroup => "cgroup",
            Self::Ipc => "ipc",
            Self::Mnt => "mnt",
            Self::Net => "net",
            Self::Pid => "pid",
            Self::User => "user",
            Self::Uts => "uts",
            Self::Time => "time",
        }
    }
}

/// The most of a `/proc` file read (a command line, a cgroup, a map, the
/// init's children): past it, the rest is left out and said.
#[cfg(target_os = "linux")]
const MOST: u64 = 16 * 1024;

/// A container's isolation: see the module's doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Isolation {
    /// [`ISOLATION_VERSION`].
    pub version: u32,
    /// Its name.
    pub name: String,
    /// Each kind of [`NamespaceKind::ALL`], in that order.
    pub namespaces: Vec<NamespaceUse>,
    /// Its cgroup (v2's path), if the kernel says it.
    pub cgroup: Option<String>,
    /// Its init's host PID (the container's PID 1).
    pub init_pid: i32,
    /// Its command: the init's child, or the init itself without a PID
    /// namespace; `None` between the init's start and its command's, or
    /// after the command's end.
    pub command: Option<ContainerCommand>,
    /// Its user namespace's first mapping, if it has its own and it's
    /// written (a start writes it a moment after the namespace is made).
    pub user: Option<UserMap>,
}

/// One namespace kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NamespaceUse {
    /// The kind.
    pub kind: NamespaceKind,
    /// Whether the container's is not the host's.
    pub own: bool,
    /// Whether this kernel has the kind (`time` came in Linux 5.6).
    pub present: bool,
    /// The namespace's ID (its inode): the same in containers that share
    /// it.
    pub id: Option<u64>,
}

/// A container's command, as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ContainerCommand {
    /// Its host PID.
    pub pid: i32,
    /// Its arguments (bytes that aren't UTF-8 replaced).
    pub args: Vec<String>,
    /// Whether its command line was longer than what's read (16 KiB): the
    /// last arguments left out.
    pub truncated: bool,
}

/// A user namespace's mapping: `count` IDs from `inside` are the host's
/// from `outside` (`0 100000 65536`: root inside is UID 100000).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct UserMap {
    /// The first ID inside.
    pub inside: u32,
    /// The host's ID it is.
    pub outside: u32,
    /// How many.
    pub count: u32,
}

/// Container `reference`'s isolation, while it runs.
///
/// # Errors
///
/// No such container ([`crate::ErrorKind::NotFound`]);
/// not running or ended as it was read
/// ([`crate::ErrorKind::NotRunning`]). What `/proc`
/// won't say is left out (a kind the kernel lacks, a command not yet
/// forked).
pub fn isolation(engine: &Engine, reference: &str) -> Result<Isolation> {
    let record = super::find(engine, reference)?;
    super::require(
        engine,
        &record,
        super::Needs::Attachable,
        "see its isolation",
    )?;
    #[cfg(target_os = "linux")]
    {
        let ended = || {
            Error::new(
                ErrorKind::NotRunning,
                format!(
                    "container {} has ended: its isolation is the kernel's while it runs",
                    record.name
                ),
            )
        };
        let (pid, started) = super::init_of(engine, record.id.as_str()).ok_or_else(ended)?;
        let read = read(pid);
        if !shipyard_runtime::container::is_process(pid, started) {
            return Err(ended());
        }
        Ok(Isolation {
            version: ISOLATION_VERSION,
            name: record.name.as_str().to_owned(),
            namespaces: read.namespaces,
            cgroup: read.cgroup,
            init_pid: pid,
            command: read.command,
            user: read.user,
        })
    }
    #[cfg(not(target_os = "linux"))]
    Err(crate::error::linux_only(
        "readings of a container's isolation",
    ))
}

/// What [`read`] finds of an init.
#[cfg(target_os = "linux")]
struct Read {
    namespaces: Vec<NamespaceUse>,
    cgroup: Option<String>,
    command: Option<ContainerCommand>,
    user: Option<UserMap>,
}

/// An init's isolation, from `/proc` (its start time checked by the
/// caller after).
#[cfg(target_os = "linux")]
fn read(pid: i32) -> Read {
    let link = |path: String| {
        std::fs::read_link(path)
            .ok()
            .map(|target| target.to_string_lossy().into_owned())
    };
    let namespaces: Vec<NamespaceUse> = NamespaceKind::ALL
        .iter()
        .map(|&kind| {
            let theirs = link(format!("/proc/{pid}/ns/{}", kind.name()));
            let ours = link(format!("/proc/self/ns/{}", kind.name()));
            NamespaceUse {
                kind,
                own: own(theirs.as_deref(), ours.as_deref()),
                present: theirs.is_some(),
                id: theirs.as_deref().and_then(namespace_id),
            }
        })
        .collect();
    let has = |kind| namespaces.iter().any(|ns| ns.kind == kind && ns.own);
    let cgroup = capped(&format!("/proc/{pid}/cgroup"))
        .and_then(|(bytes, _)| cgroup_path(&String::from_utf8_lossy(&bytes)));
    let user = has(NamespaceKind::User)
        .then(|| capped(&format!("/proc/{pid}/uid_map")))
        .flatten()
        .and_then(|(bytes, _)| user_map(&String::from_utf8_lossy(&bytes)));
    let command_pid = if has(NamespaceKind::Pid) {
        child_of(pid)
    } else {
        Some(pid)
    };
    // Its command line, then its parent checked: a PID that ended and was
    // reused meanwhile has another parent (only the init's own children,
    // and orphans it took in, have it), and a command that ended (a zombie)
    // is none.
    let command = command_pid.and_then(|child| {
        let (bytes, truncated) = capped(&format!("/proc/{child}/cmdline"))?;
        let stat = shipyard_sys::proc::stat(child)?;
        (!stat.gone() && (child == pid || stat.ppid == pid)).then(|| ContainerCommand {
            pid: child,
            args: arguments(&bytes),
            truncated,
        })
    });
    Read {
        namespaces,
        cgroup,
        command,
        user,
    }
}

/// `path` read up to [`MOST`] bytes (a `/proc` file: its size isn't
/// known before it's read), and whether there was more.
#[cfg(target_os = "linux")]
fn capped(path: &str) -> Option<(Vec<u8>, bool)> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MOST + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST;
    bytes.truncate(usize::try_from(MOST).unwrap_or(usize::MAX));
    Some((bytes, truncated))
}

/// The init's first child: from its `children` file (a kernel with
/// `CONFIG_PROC_CHILDREN`), else the lowest PID whose parent it is.
#[cfg(target_os = "linux")]
fn child_of(pid: i32) -> Option<i32> {
    if let Some((bytes, _)) = capped(&format!("/proc/{pid}/task/{pid}/children")) {
        return first_child(&String::from_utf8_lossy(&bytes));
    }
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&other| shipyard_sys::proc::stat(other).is_some_and(|stat| stat.ppid == pid))
        .min()
}

/// Whether a namespace (its `/proc/PID/ns` link, `theirs`) is the
/// container's own: there, and not the host's (`ours`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn own(theirs: Option<&str>, ours: Option<&str>) -> bool {
    theirs.is_some() && theirs != ours
}

/// A namespace's ID from its link (`net:[4026531840]`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn namespace_id(link: &str) -> Option<u64> {
    link.split_once(":[")?.1.strip_suffix(']')?.parse().ok()
}

/// The path of a `/proc/PID/cgroup`: cgroup v2's one line (`0::/PATH`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn cgroup_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
}

/// A `uid_map`'s first mapping (`inside outside count`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn user_map(text: &str) -> Option<UserMap> {
    let mut fields = text.lines().next()?.split_whitespace().map(str::parse);
    match (fields.next()?, fields.next()?, fields.next()?) {
        (Ok(inside), Ok(outside), Ok(count)) => Some(UserMap {
            inside,
            outside,
            count,
        }),
        _ => None,
    }
}

/// The first PID of a `children` file (space-separated).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn first_child(text: &str) -> Option<i32> {
    text.split_whitespace().next()?.parse().ok()
}

/// A `cmdline`'s arguments: NUL-separated, a last NUL or not; an empty
/// one is no arguments (an empty argument within one is kept).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read on Linux; tested everywhere")
)]
fn arguments(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .strip_suffix(b"\0")
        .unwrap_or(bytes)
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cgroup v2's line read; v1's lines alone, none.
    #[test]
    fn a_cgroup_file_s_path_is_v2_s() {
        assert_eq!(
            cgroup_path("0::/shipyard/runtime/abc\n").as_deref(),
            Some("/shipyard/runtime/abc")
        );
        assert_eq!(cgroup_path("12:cpu:/x\n"), None);
    }

    /// A user mapping's first line; a malformed one, none.
    #[test]
    fn a_uid_map_is_read_by_its_first_line() {
        assert_eq!(
            user_map("         0     100000      65536\n1 2 3\n"),
            Some(UserMap {
                inside: 0,
                outside: 100_000,
                count: 65536
            })
        );
        assert_eq!(user_map("0 x 1"), None);
        assert_eq!(user_map(""), None);
    }

    /// A command line's arguments, as text (bytes that aren't UTF-8
    /// replaced, markup kept as text, an empty argument kept); a child
    /// file's first PID.
    #[test]
    fn a_command_line_is_said_as_its_arguments() {
        assert_eq!(arguments(b"sleep\x00300\x00"), ["sleep", "300"]);
        assert_eq!(
            arguments(b"sh\x00-c\x00<b>caf\xe9</b>"),
            ["sh", "-c", "<b>caf\u{fffd}</b>"]
        );
        assert_eq!(arguments(b"echo\x00\x00x\x00"), ["echo", "", "x"]);
        assert!(arguments(b"").is_empty());
        assert_eq!(first_child("42 43 \n"), Some(42));
        assert_eq!(first_child(""), None);
    }

    /// A namespace is the container's own when it has one and it isn't the
    /// host's; its ID is its link's inode.
    #[test]
    fn a_namespace_is_own_unless_it_s_the_host_s() {
        assert!(own(Some("net:[2]"), Some("net:[1]")));
        assert!(!own(Some("net:[1]"), Some("net:[1]")));
        assert!(!own(None, Some("time:[1]")), "a kind this kernel lacks");
        assert!(own(Some("time:[2]"), None), "one the host can't say");
        assert_eq!(namespace_id("net:[4026531840]"), Some(4_026_531_840));
        assert_eq!(namespace_id("net:4026531840"), None);
        let kinds: Vec<serde_json::Value> = NamespaceKind::ALL
            .iter()
            .map(|kind| serde_json::to_value(kind).unwrap_or_default())
            .collect();
        assert_eq!(
            kinds,
            ["cgroup", "ipc", "mnt", "net", "pid", "user", "uts", "time"]
        );
    }
}
