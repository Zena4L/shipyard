//! A process's `/proc/PID/stat` line (TLPI §12.1), read and parsed one way
//! for every crate: what's dead (`Z` or `X`), its parent, its terminal, its
//! CPU times and its start time (step 12.5b1, F145: four parsers agreed on
//! less than this).
//!
//! The command name (field 2) is the process's to choose: spaces, `)` and
//! bytes that aren't text included. The fields after it are counted from its
//! last `)`, and only they need be text.

/// The fields of a `/proc/PID/stat` line Shipyard reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Its state (field 3): `R`, `S`, `D`, `T`, `Z`…
    pub state: char,
    /// Its parent's PID (field 4).
    pub ppid: i32,
    /// Its process group (field 5).
    pub pgrp: i32,
    /// Its session (field 6).
    pub session: i32,
    /// Its controlling terminal's device number (field 7; 0 for none).
    pub tty: i64,
    /// Its user CPU time, in clock ticks (field 14).
    pub utime: u64,
    /// Its system CPU time, in clock ticks (field 15).
    pub stime: u64,
    /// When it started, in clock ticks since the boot (field 22): with the
    /// PID, what tells it from a process given the same PID later.
    pub start: u64,
}

impl Stat {
    /// The fields of `line` (a `/proc/PID/stat`'s whole text); `None` for
    /// one that isn't such a line (cut short, a field not a number).
    #[must_use]
    pub fn parse(line: &[u8]) -> Option<Self> {
        let after = line.iter().rposition(|&byte| byte == b')')?;
        let rest = std::str::from_utf8(line.get(after + 1..)?).ok()?;
        // Fields 3 (the state) on, from index 0.
        let fields: Vec<&str> = rest.split_whitespace().take(20).collect();
        let field = |index: usize| fields.get(index).copied();
        let mut state = field(0)?.chars();
        let (Some(state), None) = (state.next(), state.next()) else {
            return None;
        };
        Some(Self {
            state,
            ppid: field(1)?.parse().ok()?,
            pgrp: field(2)?.parse().ok()?,
            session: field(3)?.parse().ok()?,
            tty: field(4)?.parse().ok()?,
            utime: field(11)?.parse().ok()?,
            stime: field(12)?.parse().ok()?,
            start: field(19)?.parse().ok()?,
        })
    }

    /// Whether it has ended: a zombie (`Z`, not yet reaped) or dead (`X`).
    #[must_use]
    pub fn gone(&self) -> bool {
        matches!(self.state, 'Z' | 'X')
    }
}

/// The most a `stat` line is read as (a name is at most 16 bytes, 64 for a
/// kernel thread; the rest, numbers).
const MOST: u64 = 4096;

/// The `/proc/PID/stat` of `pid`, if it can be read and parsed (never
/// where there's no `/proc`: a Mac).
#[must_use]
pub fn stat(pid: i32) -> Option<Stat> {
    let line =
        crate::fs::read_bounded(std::path::Path::new(&format!("/proc/{pid}/stat")), MOST).ok()?;
    Stat::parse(&line)
}

/// The start time of `pid` (field 22), unless it's gone (a zombie, or
/// dead): with the PID, which process it is, for as long as it's one.
#[must_use]
pub fn start_time(pid: i32) -> Option<u64> {
    stat(pid).filter(|stat| !stat.gone()).map(|stat| stat.start)
}

/// [`start_time`] of the process whose `/proc/PID` directory is open at
/// `dir`.
#[must_use]
pub fn start_time_at(dir: std::os::fd::BorrowedFd<'_>) -> Option<u64> {
    stat_at(dir)
        .filter(|stat| !stat.gone())
        .map(|stat| stat.start)
}

/// The `stat` in the `/proc/PID` directory open at `dir`: read under it, so
/// a PID reused since it was opened is never read instead.
#[must_use]
pub fn stat_at(dir: std::os::fd::BorrowedFd<'_>) -> Option<Stat> {
    let line = crate::fs::read_bounded_at(dir, "stat", MOST).ok()?;
    Stat::parse(&line)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn line(name: &[u8], state: &str) -> Vec<u8> {
        let mut line = b"42 (".to_vec();
        line.extend_from_slice(name);
        line.extend_from_slice(
            format!(") {state} 7 42 42 34817 42 4194560 1 2 3 4 15 16 0 0 20 0 1 0 98765 0 0")
                .as_bytes(),
        );
        line
    }

    /// Each field where `proc(5)` puts it, the name whatever it holds (a
    /// `)` and spaces, bytes that aren't text): counted from its last `)`.
    #[test]
    fn a_stat_line_s_fields_come_from_after_the_name() {
        for name in [&b"sh"[..], b"a) b (c", b"\xff\xfe)) x", b""] {
            let stat = Stat::parse(&line(name, "S")).unwrap();
            assert_eq!(
                stat,
                Stat {
                    state: 'S',
                    ppid: 7,
                    pgrp: 42,
                    session: 42,
                    tty: 34817,
                    utime: 15,
                    stime: 16,
                    start: 98765
                },
                "{name:?}"
            );
            assert!(!stat.gone());
        }
    }

    /// Zombies and the dead are gone; a line cut short, or a field that
    /// isn't a number, isn't one.
    #[test]
    fn what_s_gone_and_what_isn_t_a_line() {
        assert!(Stat::parse(&line(b"sh", "Z")).unwrap().gone());
        assert!(Stat::parse(&line(b"sh", "X")).unwrap().gone());
        let whole = line(b"sh", "S");
        let cut = &whole[..whole.len() - 12];
        assert_eq!(Stat::parse(cut), None, "cut before the start time");
        assert_eq!(Stat::parse(b"42 sh S 7"), None, "no name's parentheses");
        assert_eq!(
            Stat::parse(&line(b"sh", "SS")),
            None,
            "a state of two letters"
        );
        let mut bad = line(b"sh", "S");
        let at = bad.len() - 9;
        bad[at] = b'x';
        assert_eq!(Stat::parse(&bad), None, "the start time not a number");
    }

    /// This process's own line reads, its parent ours.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_s_stat_reads() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let stat = stat(pid).unwrap();
        assert_eq!(stat.ppid, nix::unistd::getppid().as_raw());
        assert!(!stat.gone());
    }
}
