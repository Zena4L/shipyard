//! Resolving user and group names ([`Names`]) where the command runs: in
//! the container's own `/etc/passwd` and `/etc/group`.
//!
//! **The files are read directly**, as runc and Docker do, not through the
//! C library's `getpwnam`: that goes through NSS, which follows the host's
//! `nsswitch.conf` and loads its modules (LDAP, systemd, …). Inside a
//! container that's the wrong database, and after the switch to its root
//! filesystem it would load the container's libraries into the init.
//!
//! The formats (`passwd(5)`, `group(5)`):
//!
//! ```text
//! name:password:uid:gid:comment:home:shell      /etc/passwd
//! name:password:gid:member,member,…              /etc/group
//! ```
//!
//! Comments, NIS lines (`+` and `-`) and malformed lines are skipped, and
//! the first entry for a name wins, as `getpwnam` does.
//!
//! The rules, as Docker's:
//! - a user name must be in `/etc/passwd`, and gives the uid, the primary
//!   group and the home directory; a number is used as it is, with its
//!   entry's group and home if there is one (group 0 otherwise);
//! - a group name must be in `/etc/group`;
//! - with [`Names::memberships`], every group listing the user is added;
//! - `HOME`, if the environment has none: the user's home directory, or
//!   `/root` for root and `/` for anyone else.
//!
//! **Only names fail.** A name given must be found (and the files read);
//! what's looked up only to fill in (a numeric user's entry, `HOME`, the
//! user's groups) falls back to the defaults when a file can't be read, so
//! an image with a broken `/etc/passwd` still runs `--user 1000`.
//!
//! **An ID of 4294967295 is refused:** the kernel reads it as "leave
//! unchanged", so a `passwd` entry with it would leave the command running
//! as root. (Negative IDs, which macOS uses for `nobody`, are read as the
//! kernel does: `-2` is 4294967294.)

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use nix::errno::Errno;

use super::spec::{Names, ProcessSpec};

/// The largest `passwd` or `group` file read.
const MAX_TABLE_SIZE: u64 = 16 * 1024 * 1024;

/// Which name couldn't be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// [`Names::user`].
    User,
    /// [`Names::group`].
    Group,
    /// [`Names::additional_groups`], by index.
    AdditionalGroup(usize),
}

impl Which {
    /// As a failure's detail number (see `failure.rs`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only the container's init reports it")
    )]
    pub(crate) fn detail(self) -> usize {
        match self {
            Self::User => 0,
            Self::Group => 1,
            Self::AdditionalGroup(index) => index.saturating_add(2),
        }
    }

    /// From a failure's detail number.
    pub(crate) fn from_detail(detail: u64) -> Self {
        match detail {
            0 => Self::User,
            1 => Self::Group,
            n => Self::AdditionalGroup(usize::try_from(n - 2).unwrap_or(usize::MAX)),
        }
    }

    /// The name it stands for, in `names`.
    pub(crate) fn name(self, names: &Names) -> Option<&str> {
        match self {
            Self::User => names.user.as_deref(),
            Self::Group => names.group.as_deref(),
            Self::AdditionalGroup(index) => names.additional_groups.get(index).map(String::as_str),
        }
    }

    /// The file it's looked up in.
    pub(crate) fn file(self) -> &'static str {
        match self {
            Self::User => "passwd",
            Self::Group | Self::AdditionalGroup(_) => "group",
        }
    }
}

/// A name that couldn't be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", describe(*which, name, *errno, "/etc"))]
pub struct LookupError {
    /// Which name.
    pub which: Which,
    /// The name.
    pub name: String,
    /// ENOENT: not in the file; anything else: the file couldn't be read.
    pub errno: Errno,
}

/// "user \"alice\" isn't in /etc/passwd", or why the file couldn't be read.
/// `etc` says whose `/etc` it is, for the message.
pub(crate) fn describe(which: Which, name: &str, errno: Errno, etc: &str) -> String {
    let what = match which {
        Which::User => "user",
        Which::Group | Which::AdditionalGroup(_) => "group",
    };
    let file = which.file();
    if errno == Errno::ENOENT {
        format!("{what} \"{name}\" isn't in {etc}/{file}")
    } else if errno == Errno::ERANGE {
        format!(
            "{what} \"{name}\" has the ID {} in {etc}/{file}, which the kernel reads as \
             \"leave unchanged\"",
            u32::MAX
        )
    } else {
        format!(
            "looking up {what} \"{name}\" in {etc}/{file}: {}",
            errno.desc()
        )
    }
}

/// An `/etc/passwd` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// An `/etc/group` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupEntry {
    name: String,
    gid: u32,
    members: Vec<String>,
}

/// Resolves `spec`'s [`Names`] in the `passwd` and `group` files under
/// `etc`, and sets `HOME` if the environment has none. The names are
/// cleared once resolved. The files are read only when something needs
/// them; with no `etc`, never (there must be no names then).
///
/// # Errors
///
/// A name that isn't there, or a file that can't be read.
pub(crate) fn resolve(spec: &mut ProcessSpec, etc: Option<&Path>) -> Result<(), LookupError> {
    resolve_from(spec, etc.map_or(Etc::None, Etc::Path))
}

/// [`resolve`] in the `etc` of a root filesystem open at `root`: another
/// process's (a container's, through `/proc/<pid>/root`). Its files are
/// found as that process sees them (symlinks followed inside the root,
/// never out of it), and must be regular files before they're opened.
///
/// # Errors
///
/// As [`resolve`].
#[cfg(target_os = "linux")]
pub(crate) fn resolve_in_root(
    spec: &mut ProcessSpec,
    root: &std::os::fd::OwnedFd,
) -> Result<(), LookupError> {
    resolve_from(spec, Etc::InRoot(root))
}

fn resolve_from(spec: &mut ProcessSpec, etc: Etc<'_>) -> Result<(), LookupError> {
    let names = std::mem::take(&mut spec.user.names);
    let wants_home = spec.env.get(OsStr::new("HOME")).is_none();
    let mut tables = Tables::new(etc);
    let fail = |(which, errno): (Which, Errno)| LookupError {
        which,
        name: which.name(&names).unwrap_or_default().to_owned(),
        errno,
    };
    // Without a user to look up, the entry is only wanted for HOME.
    let entry = match names.user.as_deref() {
        Some(user) => user_entry(&mut tables, user).map_err(|errno| fail((Which::User, errno)))?,
        None if wants_home => entry_by_uid(&mut tables, spec.user.uid),
        None => None,
    };
    if let Some(user) = &names.user {
        // A name has an entry (or `user_entry` failed); a number may not.
        (spec.user.uid, spec.user.gid) = match &entry {
            Some(entry) => (entry.uid, entry.gid),
            None => (parse_id(user).unwrap_or(spec.user.uid), 0),
        };
    }
    if let Some(group) = &names.group {
        spec.user.gid =
            group_id(&mut tables, group).map_err(|errno| fail((Which::Group, errno)))?;
    }
    let base = std::mem::take(&mut spec.user.additional_gids);
    spec.user.additional_gids =
        supplementary_groups(&mut tables, base, &names, entry.as_ref()).map_err(fail)?;
    if wants_home {
        fill_home(spec, entry.as_ref());
    }
    Ok(())
}

/// The `/etc/passwd` entry for `user`: a name must have one (with usable
/// IDs); a number may.
fn user_entry(tables: &mut Tables<'_>, user: &str) -> Result<Option<PasswdEntry>, Errno> {
    if let Some(uid) = parse_id(user) {
        return Ok(entry_by_uid(tables, uid));
    }
    let entry = tables
        .passwd()?
        .iter()
        .find(|entry| entry.name == user)
        .cloned()
        .ok_or(Errno::ENOENT)?;
    if entry.uid == u32::MAX || entry.gid == u32::MAX {
        return Err(Errno::ERANGE);
    }
    Ok(Some(entry))
}

/// The entry for `uid`, if there is one and the file can be read: only to
/// fill in, so a file that can't be read just has no entry.
fn entry_by_uid(tables: &mut Tables<'_>, uid: u32) -> Option<PasswdEntry> {
    tables
        .passwd()
        .ok()?
        .iter()
        .find(|entry| entry.uid == uid && entry.gid != u32::MAX)
        .cloned()
}

/// `base`, then the additional groups by name or number, then (with
/// memberships) the groups listing the user: each once, in that order.
fn supplementary_groups(
    tables: &mut Tables<'_>,
    base: Vec<u32>,
    names: &Names,
    entry: Option<&PasswdEntry>,
) -> Result<Vec<u32>, (Which, Errno)> {
    let mut groups = base;
    for (index, group) in names.additional_groups.iter().enumerate() {
        groups
            .push(group_id(tables, group).map_err(|errno| (Which::AdditionalGroup(index), errno))?);
    }
    // Only to fill in: a `group` file that can't be read adds nothing, and a
    // group with the "leave unchanged" ID can't be joined.
    if names.memberships
        && let Some(entry) = entry
        && let Ok(table) = tables.group()
    {
        groups.extend(
            table
                .iter()
                .filter(|group| group.members.contains(&entry.name) && group.gid != u32::MAX)
                .map(|group| group.gid),
        );
    }
    let mut seen = HashSet::new();
    groups.retain(|gid| seen.insert(*gid));
    Ok(groups)
}

/// Sets `HOME` to the entry's home directory, or the default for the uid.
fn fill_home(spec: &mut ProcessSpec, entry: Option<&PasswdEntry>) {
    let home = match entry {
        // (A NUL can't be in an environment value: such a home is ignored.)
        Some(entry) if !entry.home.is_empty() && !entry.home.contains('\0') => entry.home.as_str(),
        _ if spec.user.uid == 0 => "/root",
        _ => "/",
    };
    // Can't fail: `HOME` is a valid name, and the value has no NUL.
    let _ = spec.env.set(OsStr::new("HOME"), OsStr::new(home));
}

/// A group's ID: the number itself, or the name looked up.
fn group_id(tables: &mut Tables<'_>, group: &str) -> Result<u32, Errno> {
    match parse_id(group) {
        Some(gid) => Ok(gid),
        None => match tables.group()?.iter().find(|entry| entry.name == group) {
            Some(entry) if entry.gid == u32::MAX => Err(Errno::ERANGE),
            Some(entry) => Ok(entry.gid),
            None => Err(Errno::ENOENT),
        },
    }
}

/// A user or group given as a number (digits only).
fn parse_id(s: &str) -> Option<u32> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// An ID in `passwd` or `group`: a number, or a negative one as the kernel
/// stores it (macOS lists `nobody` as `-2`, which is 4294967294).
fn parse_table_id(field: &str) -> Option<u32> {
    field
        .parse::<u32>()
        .ok()
        .or_else(|| field.parse::<i32>().ok().map(i32::cast_unsigned))
}

/// Where the `passwd` and `group` files are.
#[derive(Clone, Copy)]
enum Etc<'a> {
    /// Nowhere: there are none.
    None,
    /// In this directory, by path.
    Path(&'a Path),
    /// In `etc` of the root filesystem open here.
    #[cfg(target_os = "linux")]
    InRoot(&'a std::os::fd::OwnedFd),
}

/// The two files, each read the first time it's needed.
struct Tables<'a> {
    etc: Etc<'a>,
    passwd: Option<Vec<PasswdEntry>>,
    group: Option<Vec<GroupEntry>>,
}

impl<'a> Tables<'a> {
    fn new(etc: Etc<'a>) -> Self {
        Self {
            etc,
            passwd: None,
            group: None,
        }
    }

    fn passwd(&mut self) -> Result<&[PasswdEntry], Errno> {
        if self.passwd.is_none() {
            self.passwd = Some(parse_passwd(&self.read("passwd")?));
        }
        Ok(self.passwd.as_deref().unwrap_or_default())
    }

    fn group(&mut self) -> Result<&[GroupEntry], Errno> {
        if self.group.is_none() {
            self.group = Some(parse_group(&self.read("group")?));
        }
        Ok(self.group.as_deref().unwrap_or_default())
    }

    fn read(&self, file: &str) -> Result<String, Errno> {
        match self.etc {
            Etc::None => Ok(String::new()),
            Etc::Path(etc) => read_table(&etc.join(file)),
            #[cfg(target_os = "linux")]
            Etc::InRoot(root) => read_table_in_root(root, file),
        }
    }
}

/// Reads a `passwd` or `group` file. A missing one is empty (a minimal
/// root filesystem may have none). It must be a regular file, checked on
/// the file as opened (non-blocking, so a FIFO can't stall the open), and
/// at most [`MAX_TABLE_SIZE`].
fn read_table(path: &Path) -> Result<String, Errno> {
    let errno = |err: std::io::Error| err.raw_os_error().map_or(Errno::EIO, Errno::from_raw);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(errno(err)),
    };
    if !file.metadata().map_err(errno)?.is_file() {
        return Err(Errno::EINVAL);
    }
    let mut bytes = Vec::new();
    file.take(MAX_TABLE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(errno)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_TABLE_SIZE) {
        return Err(Errno::EFBIG);
    }
    // Entries that aren't UTF-8 can't match a name given as a string.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads `etc/FILE` of the root open at `root`, as [`read_table`] does. It's
/// looked at before it's opened (through its `O_PATH` descriptor): opening
/// a device runs its driver, and the root is another's to fill.
#[cfg(target_os = "linux")]
fn read_table_in_root(root: &std::os::fd::OwnedFd, file: &str) -> Result<String, Errno> {
    use std::os::fd::AsRawFd as _;
    let found = match super::resolve::open_in_root(root, &Path::new("etc").join(file), true) {
        Ok(found) => found,
        Err(Errno::ENOENT) => return Ok(String::new()),
        Err(errno) => return Err(errno),
    };
    let stat = nix::sys::stat::fstat(&found)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(Errno::EINVAL);
    }
    read_table(Path::new(&format!("/proc/self/fd/{}", found.as_raw_fd())))
}

/// The lines of a `passwd` or `group` file that are entries, split at `:`.
fn entries(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with(['#', '+', '-']))
        .map(|line| line.split(':').collect())
}

fn parse_passwd(text: &str) -> Vec<PasswdEntry> {
    entries(text)
        .filter_map(|fields| match fields.as_slice() {
            [name, _, uid, gid, _, home, ..] if !name.is_empty() => Some(PasswdEntry {
                name: (*name).to_owned(),
                uid: parse_table_id(uid)?,
                gid: parse_table_id(gid)?,
                home: (*home).to_owned(),
            }),
            _ => None,
        })
        .collect()
}

fn parse_group(text: &str) -> Vec<GroupEntry> {
    entries(text)
        .filter_map(|fields| match fields.as_slice() {
            [name, _, gid, rest @ ..] if !name.is_empty() => Some(GroupEntry {
                name: (*name).to_owned(),
                gid: parse_table_id(gid)?,
                members: rest
                    .first()
                    .map(|members| {
                        members
                            .split(',')
                            .filter(|member| !member.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::process::User;

    const PASSWD: &str = "\
# a comment
root:x:0:0:root:/root:/bin/sh
+nisuser:x:5000:5000::/nis:/bin/sh
broken line
alice:x:1000:1000:Alice:/home/alice:/bin/sh
alice:x:1999:1999:second alice:/elsewhere:/bin/sh
nohome:x:1001:1001:::/bin/sh
";
    const GROUP: &str = "\
root:x:0:
staff:x:50:alice,bob
wheel:x:10:carol
audio:x:29:alice
alice:x:1000:
";

    /// An `/etc` with `passwd` and `group`, removed on drop.
    struct Etc {
        dir: PathBuf,
    }

    impl Etc {
        fn new(passwd: Option<&str>, group: Option<&str>) -> Self {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "shipyard-etc-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(passwd) = passwd {
                std::fs::write(dir.join("passwd"), passwd).unwrap();
            }
            if let Some(group) = group {
                std::fs::write(dir.join("group"), group).unwrap();
            }
            Self { dir }
        }
    }

    impl Drop for Etc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn spec_for(user: User) -> ProcessSpec {
        let mut spec = ProcessSpec::new(vec!["sh".into()]);
        spec.user = user;
        spec
    }

    fn resolved(user: User) -> Result<ProcessSpec, LookupError> {
        let etc = Etc::new(Some(PASSWD), Some(GROUP));
        let mut spec = spec_for(user);
        resolve(&mut spec, Some(&etc.dir)).map(|()| spec)
    }

    fn home(spec: &ProcessSpec) -> Option<&str> {
        spec.env.get(OsStr::new("HOME")).and_then(OsStr::to_str)
    }

    #[test]
    fn a_name_gives_the_uid_group_home_and_memberships() {
        let spec = resolved(User::named("alice", None)).unwrap();
        assert_eq!(
            (spec.user.uid, spec.user.gid),
            (1000, 1000),
            "the first alice"
        );
        assert_eq!(spec.user.additional_gids, [50, 29]);
        assert_eq!(home(&spec), Some("/home/alice"));
        assert!(spec.user.names.is_empty(), "resolved");
    }

    #[test]
    fn a_group_and_more_groups_by_name_or_number() {
        let mut user = User::named("alice", Some("wheel".into()));
        user.additional_gids = vec![50, 7];
        user.names.additional_groups = vec!["staff".into(), "60".into()];
        let spec = resolved(user).unwrap();
        assert_eq!(spec.user.gid, 10);
        assert_eq!(spec.user.additional_gids, [50, 7, 60, 29], "no duplicates");
        let spec = resolved(User::named("alice", Some("123".into()))).unwrap();
        assert_eq!(spec.user.gid, 123);
    }

    #[test]
    fn a_number_uses_its_entry_if_there_is_one() {
        let spec = resolved(User::named("1000", None)).unwrap();
        assert_eq!((spec.user.uid, spec.user.gid), (1000, 1000));
        assert_eq!(home(&spec), Some("/home/alice"));
        assert_eq!(spec.user.additional_gids, [50, 29], "alice's groups");
        let spec = resolved(User::named("4242", None)).unwrap();
        assert_eq!((spec.user.uid, spec.user.gid), (4242, 0));
        assert_eq!(home(&spec), Some("/"));
        assert!(spec.user.additional_gids.is_empty());
    }

    #[test]
    fn home_is_kept_or_defaulted() {
        let mut spec = spec_for(User::named("alice", None));
        spec.env
            .set(OsStr::new("HOME"), OsStr::new("/mine"))
            .unwrap();
        let etc = Etc::new(Some(PASSWD), Some(GROUP));
        resolve(&mut spec, Some(&etc.dir)).unwrap();
        assert_eq!(home(&spec), Some("/mine"), "the user's own HOME wins");
        assert_eq!(home(&resolved(User::new(0, 0)).unwrap()), Some("/root"));
        assert_eq!(
            home(&resolved(User::named("nohome", None)).unwrap()),
            Some("/")
        );
        // A home that can't be an environment value: the default.
        let odd = Etc::new(Some("bob:x:1002:1002::/home/\0bob:/bin/sh\n"), None);
        let mut spec = spec_for(User::named("bob", None));
        resolve(&mut spec, Some(&odd.dir)).unwrap();
        assert_eq!(home(&spec), Some("/"));
        // No passwd at all: the defaults.
        let empty = Etc::new(None, None);
        let mut spec = spec_for(User::new(0, 0));
        resolve(&mut spec, Some(&empty.dir)).unwrap();
        assert_eq!(home(&spec), Some("/root"));
    }

    #[test]
    fn missing_names_say_which() {
        let err = resolved(User::named("mallory", None)).unwrap_err();
        assert_eq!((err.which, err.errno), (Which::User, Errno::ENOENT));
        assert_eq!(err.to_string(), "user \"mallory\" isn't in /etc/passwd");
        let err = resolved(User::named("alice", Some("nogroup".into()))).unwrap_err();
        assert_eq!(err.which, Which::Group);
        let mut user = User::new(0, 0);
        user.names.additional_groups = vec!["wheel".into(), "missing".into()];
        let err = resolved(user).unwrap_err();
        assert_eq!(
            (err.which, err.name.as_str()),
            (Which::AdditionalGroup(1), "missing")
        );
        assert_eq!(Which::from_detail(err.which.detail() as u64), err.which);
    }

    #[test]
    fn a_table_that_isnt_a_small_regular_file_is_an_error() {
        let etc = Etc::new(None, Some(GROUP));
        std::fs::create_dir(etc.dir.join("passwd")).unwrap();
        let err = resolve(&mut spec_for(User::named("alice", None)), Some(&etc.dir)).unwrap_err();
        assert_eq!(err.errno, Errno::EINVAL);
        std::fs::remove_dir(etc.dir.join("passwd")).unwrap();
        nix::unistd::mkfifo(&etc.dir.join("passwd"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let err = resolve(&mut spec_for(User::named("alice", None)), Some(&etc.dir)).unwrap_err();
        assert_eq!(err.errno, Errno::EINVAL, "and no hang");
        std::fs::remove_file(etc.dir.join("passwd")).unwrap();
        std::fs::File::create(etc.dir.join("passwd"))
            .unwrap()
            .set_len(MAX_TABLE_SIZE + 1)
            .unwrap();
        let err = resolve(&mut spec_for(User::named("alice", None)), Some(&etc.dir)).unwrap_err();
        assert_eq!(err.errno, Errno::EFBIG);
    }

    /// What's only filled in survives a file that can't be read; a name
    /// doesn't.
    #[test]
    fn only_names_need_readable_files() {
        let etc = Etc::new(None, None);
        std::fs::create_dir(etc.dir.join("passwd")).unwrap();
        std::fs::create_dir(etc.dir.join("group")).unwrap();
        let mut spec = spec_for(User::new(0, 0));
        resolve(&mut spec, Some(&etc.dir)).unwrap();
        assert_eq!(home(&spec), Some("/root"), "the default, not an error");
        let mut spec = spec_for(User::named("1000", None));
        resolve(&mut spec, Some(&etc.dir)).unwrap();
        assert_eq!(
            (spec.user.uid, spec.user.gid, home(&spec)),
            (1000, 0, Some("/"))
        );
        let err = resolve(&mut spec_for(User::named("alice", None)), Some(&etc.dir)).unwrap_err();
        assert_eq!(err.errno, Errno::EINVAL);
        let err = resolve(
            &mut spec_for(User::named("0", Some("staff".into()))),
            Some(&etc.dir),
        )
        .unwrap_err();
        assert_eq!((err.which, err.errno), (Which::Group, Errno::EINVAL));
    }

    /// The kernel's "leave unchanged" ID never comes out of the files: it
    /// would leave the command running as root.
    #[test]
    fn the_leave_unchanged_id_is_refused() {
        let etc = Etc::new(
            Some(
                "evil:x:4294967295:0::/:/bin/sh\nodd:x:-1:-1::/:/bin/sh\nnobody:*:-2:-2::/:/bin/sh\n",
            ),
            Some("bad:x:4294967295:nobody\nok:x:7:nobody\n"),
        );
        for user in ["evil", "odd"] {
            let err = resolve(&mut spec_for(User::named(user, None)), Some(&etc.dir)).unwrap_err();
            assert_eq!(err.errno, Errno::ERANGE, "{user}");
            assert!(err.to_string().contains("leave unchanged"), "{err}");
        }
        let err = resolve(
            &mut spec_for(User::named("0", Some("bad".into()))),
            Some(&etc.dir),
        )
        .unwrap_err();
        assert_eq!((err.which, err.errno), (Which::Group, Errno::ERANGE));
        // macOS's nobody (-2), and its groups without the unusable one.
        let mut spec = spec_for(User::named("nobody", None));
        resolve(&mut spec, Some(&etc.dir)).unwrap();
        assert_eq!(spec.user.uid, 4_294_967_294);
        assert_eq!(spec.user.additional_gids, [7]);
    }

    #[test]
    fn the_parsers_skip_what_isnt_an_entry() {
        let users = parse_passwd(PASSWD);
        let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["root", "alice", "alice", "nohome"]);
        let groups = parse_group(GROUP);
        assert_eq!(
            groups.get(1).map(|g| g.members.clone()),
            Some(vec!["alice".into(), "bob".into()])
        );
        assert_eq!(groups.last().map(|g| g.members.len()), Some(0));
    }
}
