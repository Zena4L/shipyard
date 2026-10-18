//! The guest's data disk (D10.3-2): `/var/lib/shipyard`, the one thing the
//! guest keeps across boots, on a virtio disk the Mac made as an empty
//! sparse file. The guest formats it the first time, and never again: a
//! disk that holds anything that isn't Shipyard's file system is refused,
//! not wiped. A first format cut short is the Mac's to redo: the VM host
//! keeps a new disk as `data.img.new` until a guest has booted with it.

/// The file system's label: a disk formatted by Shipyard says so.
pub(super) const LABEL: &str = "shipyard-data";

/// What the disk looks like before it's mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Found {
    /// Its first MiB is all zeros: never written.
    Empty,
    /// It has a file system labelled [`LABEL`].
    Ours,
    /// Anything else: data that isn't Shipyard's, or a format cut short
    /// before its label was written.
    Other,
}

/// What to do with the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Format it, then mount it and write the marker.
    Format,
    /// Check it (`e2fsck -p`), then mount it.
    CheckAndMount,
    /// Refuse it: the guest powers off, saying why.
    Refuse,
}

/// The step for a disk as `found`.
pub(super) fn step(found: Found) -> Step {
    match found {
        Found::Empty => Step::Format,
        Found::Ours => Step::CheckAndMount,
        Found::Other => Step::Refuse,
    }
}

/// Whether `e2fsck -p`'s exit code lets the disk be mounted: 0 (clean), 1
/// (errors fixed) or 2 (fixed, and a reboot asked for: there's nothing
/// mounted yet to reboot for). 4 and above: errors left, or it failed.
pub(super) fn checked(code: i32) -> bool {
    matches!(code, 0..=2)
}

/// Whether `bytes` (a disk's start) were never written.
pub(super) fn never_written(bytes: &[u8]) -> bool {
    bytes.iter().all(|&byte| byte == 0)
}

/// Where an ext2/3/4 superblock starts, and its fields this reads.
const SUPERBLOCK: usize = 1024;
const MAGIC_AT: usize = SUPERBLOCK + 56;
const MAGIC: [u8; 2] = 0xEF53_u16.to_le_bytes();
const LABEL_AT: usize = SUPERBLOCK + 120;
const LABEL_LEN: usize = 16;

/// What a disk's start (its first MiB, read) holds: never written, an ext4
/// file system labelled [`LABEL`], or anything else. Read from the
/// superblock itself: no tool to be missing, or to say something else.
pub(super) fn found(start: &[u8]) -> Found {
    if never_written(start) {
        return Found::Empty;
    }
    let label = start
        .get(MAGIC_AT..MAGIC_AT + 2)
        .filter(|magic| *magic == MAGIC)
        .and_then(|_| start.get(LABEL_AT..LABEL_AT + LABEL_LEN))
        .map(|name| name.split(|&byte| byte == 0).next().unwrap_or_default());
    if label == Some(LABEL.as_bytes()) {
        Found::Ours
    } else {
        Found::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty disk is formatted; Shipyard's is checked and mounted;
    /// anything else is refused, never formatted.
    #[test]
    fn a_disk_is_formatted_only_when_empty() {
        assert_eq!(step(Found::Empty), Step::Format);
        assert_eq!(step(Found::Ours), Step::CheckAndMount);
        assert_eq!(step(Found::Other), Step::Refuse);
        assert!(never_written(&[0; 4096]));
        let mut written = vec![0; 4096];
        written[4095] = 1;
        assert!(!never_written(&written));
    }

    /// A disk's start: zeros are empty; an ext4 superblock labelled
    /// Shipyard's is ours; another label, no magic, or a disk too short to
    /// hold a superblock, is someone else's.
    #[test]
    fn a_disk_is_read_by_its_superblock() {
        let mut start = vec![0; 1 << 20];
        assert_eq!(found(&start), Found::Empty);
        start[MAGIC_AT..MAGIC_AT + 2].copy_from_slice(&MAGIC);
        start[LABEL_AT..LABEL_AT + LABEL.len()].copy_from_slice(LABEL.as_bytes());
        assert_eq!(found(&start), Found::Ours);
        let mut other = start.clone();
        other[LABEL_AT] = b'X';
        assert_eq!(found(&other), Found::Other);
        let mut no_magic = start.clone();
        no_magic[MAGIC_AT] = 0;
        assert_eq!(found(&no_magic), Found::Other);
        assert_eq!(found(&start[..1100]), Found::Other);
        // A label that only starts with Shipyard's isn't it.
        let mut longer = start.clone();
        longer[LABEL_AT + LABEL.len()] = b'x';
        assert_eq!(found(&longer), Found::Other);
    }

    /// `e2fsck -p`'s codes: fixed or clean mounts, left errors don't.
    #[test]
    fn a_checked_disk_mounts_only_when_fixed() {
        for code in [0, 1, 2] {
            assert!(checked(code), "{code}");
        }
        for code in [4, 8, 12, 16, 32, 128, -1] {
            assert!(!checked(code), "{code}");
        }
    }
}
