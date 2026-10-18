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

/// Whether an `e2fsck` exit code (a set of bits) lets the disk be mounted:
/// none but 1 (errors fixed) and 2 (a reboot asked for: nothing is mounted
/// yet to reboot for). Any other bit: errors left, or the check failed.
pub(super) fn checked(code: i32) -> bool {
    code >= 0 && code & !3 == 0
}

/// The marker a repaired disk carries, in its root, for [`BOOTS_SAID`]
/// boots from the one that repaired it (D10.4-2): what the guest's
/// `doctor` says. Its first line is how many boots are left
/// (`boots_left N`), the rest what was done.
pub(super) const REPAIRED: &str = ".repaired";

/// For how many boots a repair is said: long enough to be seen (the VM
/// stops itself after a few idle minutes), never for good. Counted in
/// boots: the guest's clock isn't the Mac's until a command sets it.
pub(super) const BOOTS_SAID: u32 = 10;

/// The marker as this boot leaves it, from the one there (`existing`) and
/// this boot's repair (`repaired`, what was done): a new repair said for
/// [`BOOTS_SAID`] boots; an earlier one for one boot fewer; `None` once
/// none are left (or what's there can't be read: removed).
pub(super) fn next_marker(existing: Option<&str>, repaired: Option<&str>) -> Option<String> {
    if let Some(how) = repaired {
        return Some(format!("boots_left {BOOTS_SAID}\n{how}"));
    }
    let (count, how) = existing?.split_once('\n')?;
    let left = count.strip_prefix("boots_left ")?.parse::<u32>().ok()?;
    (left > 1).then(|| format!("boots_left {}\n{how}", left - 1))
}

/// What a marker says was done (past its count), for `doctor`.
pub(super) fn marker_says(marker: &str) -> &str {
    marker
        .split_once('\n')
        .map_or(marker, |(_, how)| how)
        .trim()
}

/// What follows `e2fsck -p` (its exit `code`, a set of bits): mount it;
/// repair it (`e2fsck -y`, once: only errors left, bit 4, and none of the
/// check's own failures, bits 8, 16, 32, 128); or refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// Clean, or fixed.
    Mount,
    /// Errors the preen pass may not fix alone: fixed with `-y`, saying so.
    Repair,
    /// The check itself failed: nothing more is tried.
    Refuse,
}

/// The data disk checked (D10.4-2): `e2fsck` run by `e2fsck` (its mode,
/// `-p` or `-y`; its exit code), a preen pass, then, if it left errors,
/// one repair (`-y`: what it can't place goes to `lost+found`), each step
/// said by `say`. `Some` with what was done if it was repaired; `Err` with
/// why if the disk can't be mounted.
pub(super) fn check(
    disk: &str,
    mut e2fsck: impl FnMut(&str) -> Result<i32, String>,
    mut say: impl FnMut(&str),
) -> Result<Option<String>, String> {
    let preened = e2fsck("-p")?;
    match after_preen(preened) {
        Next::Mount => Ok(None),
        Next::Refuse => Err(format!("{disk}'s check failed (e2fsck exit {preened})")),
        Next::Repair => {
            say(&format!(
                "{disk} has errors the check couldn't fix (e2fsck -p exit {preened}): \
                 repairing it (e2fsck -y)"
            ));
            let repaired = e2fsck("-y")?;
            if !checked(repaired) {
                return Err(format!(
                    "{disk}'s file system has errors e2fsck couldn't repair (exit {repaired})"
                ));
            }
            say("repaired: what e2fsck couldn't place is in lost+found");
            Ok(Some(format!(
                "e2fsck -p exit {preened}, then -y exit {repaired}\n"
            )))
        }
    }
}

/// The repair's marker in the data disk's root `dir`, as this boot leaves
/// it ([`next_marker`]): written whole, or removed.
///
/// # Errors
///
/// It couldn't be written or removed.
pub(super) fn mark(dir: &std::path::Path, repaired: Option<&str>) -> std::io::Result<()> {
    let marker = dir.join(REPAIRED);
    let existing = shipyard_sys::fs::read_bounded(&marker, REPAIRED_MAX)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    match next_marker(existing.as_deref(), repaired) {
        Some(text) => shipyard_sys::fs::write_atomically(
            &marker,
            text.as_bytes(),
            &shipyard_sys::fs::Write::new(0o644),
        )
        .map_err(|error| error.source),
        None => match std::fs::remove_file(&marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        },
    }
}

/// The most of the marker read: a line.
pub(super) const REPAIRED_MAX: u64 = 4096;

/// [`Next`] after `e2fsck -p` exited with `code`.
pub(super) fn after_preen(code: i32) -> Next {
    /// The bits that say the check itself failed (an operational error, a
    /// usage error, cancelled, a library's).
    const FAILED: i32 = 8 | 16 | 32 | 128;
    if checked(code) {
        Next::Mount
    } else if code > 0 && code & 4 != 0 && code & FAILED == 0 {
        Next::Repair
    } else {
        Next::Refuse
    }
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

    /// `e2fsck`'s codes: fixed or clean mounts (3: fixed, a reboot asked
    /// for), left errors or a failed check don't.
    #[test]
    fn a_checked_disk_mounts_only_when_fixed() {
        for code in [0, 1, 2, 3] {
            assert!(checked(code), "{code}");
        }
        for code in [4, 8, 12, 16, 32, 128, -1] {
            assert!(!checked(code), "{code}");
        }
    }

    /// After a preen pass: clean or fixed mounts; errors left (alone, or
    /// with some fixed) are repaired with `-y`; the check's own failure,
    /// or a code that isn't one, refuses.
    #[test]
    fn errors_left_are_repaired_once() {
        for code in [0, 1, 2, 3] {
            assert_eq!(after_preen(code), Next::Mount, "{code}");
        }
        for code in [4, 5, 6, 7] {
            assert_eq!(after_preen(code), Next::Repair, "{code}");
        }
        for code in [8, 12, 16, 32, 128, 4 | 8, -1] {
            assert_eq!(after_preen(code), Next::Refuse, "{code}");
        }
    }

    /// A preen that fixed all mounts at once; one that left errors is
    /// repaired once, said, and what was done kept; a repair that can't
    /// fix it, or a check that failed, refuses the disk.
    #[test]
    fn a_disk_is_repaired_once_and_said() {
        let run = |codes: &[i32]| {
            let mut codes = codes.iter().copied();
            let (mut modes, mut said) = (Vec::new(), Vec::new());
            let result = check(
                "/dev/vdb",
                |mode| {
                    modes.push(mode.to_owned());
                    Ok(codes.next().unwrap())
                },
                |line| said.push(line.to_owned()),
            );
            (result, modes, said)
        };
        let (clean, modes, said) = run(&[0]);
        assert_eq!(
            (clean, modes, said.len()),
            (Ok(None), vec!["-p".to_owned()], 0)
        );
        let (repaired, modes, said) = run(&[4, 1]);
        assert_eq!(
            repaired.unwrap().as_deref(),
            Some("e2fsck -p exit 4, then -y exit 1\n")
        );
        assert_eq!(modes, ["-p", "-y"]);
        assert!(said[0].contains("repairing it"), "{said:?}");
        let (unrepaired, ..) = run(&[4, 4]);
        assert!(unrepaired.unwrap_err().contains("couldn't repair (exit 4)"));
        let (failed, modes, _) = run(&[8]);
        assert!(failed.unwrap_err().contains("check failed (e2fsck exit 8)"));
        assert_eq!(modes, ["-p"], "never repaired");
    }

    /// A repair is said for ten boots, counted down by each boot after it;
    /// a new repair starts the count again; an unreadable marker goes.
    #[test]
    fn a_repair_is_said_for_ten_boots() {
        let how = "e2fsck -p exit 4, then -y exit 1\n";
        let mut marker = next_marker(None, Some(how));
        assert_eq!(
            marker.as_deref(),
            Some("boots_left 10\ne2fsck -p exit 4, then -y exit 1\n")
        );
        let mut boots = 0;
        while let Some(text) = marker {
            assert_eq!(marker_says(&text), how.trim());
            marker = next_marker(Some(&text), None);
            boots += 1;
        }
        assert_eq!(
            boots, BOOTS_SAID,
            "said at the repair's boot and the nine after"
        );
        assert_eq!(
            next_marker(Some("boots_left 3\nold"), Some(how)).as_deref(),
            Some("boots_left 10\ne2fsck -p exit 4, then -y exit 1\n")
        );
        for unreadable in ["", "boots_left x\nold", "old"] {
            assert_eq!(next_marker(Some(unreadable), None), None, "{unreadable:?}");
        }
        assert_eq!(next_marker(None, None), None);
    }

    /// The marker is written whole, counted down, and removed once done
    /// (one gone already is fine).
    #[test]
    fn the_marker_says_a_recent_repair() {
        let dir = std::env::temp_dir().join(format!("shipyard-repaired-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        mark(&dir, Some("e2fsck -p exit 4, then -y exit 1\n")).unwrap();
        let read = || std::fs::read_to_string(dir.join(REPAIRED)).unwrap();
        assert!(read().starts_with("boots_left 10\n"), "{}", read());
        mark(&dir, None).unwrap();
        assert!(read().starts_with("boots_left 9\n"), "{}", read());
        std::fs::write(dir.join(REPAIRED), "boots_left 1\nold\n").unwrap();
        mark(&dir, None).unwrap();
        assert!(!dir.join(REPAIRED).exists());
        mark(&dir, None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
