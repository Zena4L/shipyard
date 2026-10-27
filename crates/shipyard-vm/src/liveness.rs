//! Whether the guest is still the one the host booted (step 13.2, F90):
//! Virtualization.framework restarts a guest that reboots (a kernel panic,
//! with `panic=5`) in place, and keeps one that hangs `Up`, so the host's
//! watch on the VM's state sees neither. The host asks the guest's agent for
//! its boot ID every [`EVERY`]: a new ID is a reboot in place, [`MISSES`] asks
//! in a row unanswered a hang; either stops the VM, so the next command boots
//! a fresh one.

use std::time::Duration;

/// How often the guest is asked.
pub(crate) const EVERY: Duration = Duration::from_secs(10);

/// How many asks in a row may go unanswered before the guest is taken for
/// hung (a Mac swapping hard answers late, not never). An ask that fails
/// fast (refused connections) makes six about a minute; one that waits for
/// everything (two connects of 10 s, the answer's 15 s, then [`EVERY`]) up
/// to about 45 s, so six at most ~4.5 minutes.
pub(crate) const MISSES: u32 = 6;

/// What an ask heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Heard {
    /// The guest's boot ID.
    Id(String),
    /// An answer without an ID: a refusal (the guest stopping), alive.
    Alive,
    /// A guest older than this Shipyard: it doesn't know the ask.
    Unknown,
    /// Nothing: no connection, or no answer in time.
    Nothing,
}

/// What the host makes of what it heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The same guest, up.
    Up,
    /// An older guest: nothing more is asked (said once).
    Off,
    /// Another boot of it: the guest rebooted in place.
    Rebooted,
    /// [`MISSES`] asks in a row unanswered.
    Hung,
}

/// The asks so far.
#[derive(Debug, Default)]
pub(crate) struct Liveness {
    /// The boot ID first heard.
    first: Option<String>,
    /// The asks unanswered since the last answer.
    misses: u32,
}

impl Liveness {
    /// What `heard` says, with the asks before it.
    pub(crate) fn heard(&mut self, heard: Heard) -> Verdict {
        match heard {
            Heard::Id(id) => {
                self.misses = 0;
                match &self.first {
                    None => {
                        self.first = Some(id);
                        Verdict::Up
                    }
                    Some(first) if *first == id => Verdict::Up,
                    Some(_) => Verdict::Rebooted,
                }
            }
            Heard::Alive => {
                self.misses = 0;
                Verdict::Up
            }
            Heard::Unknown => Verdict::Off,
            Heard::Nothing => {
                self.misses += 1;
                if self.misses >= MISSES {
                    Verdict::Hung
                } else {
                    Verdict::Up
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same ID is the same guest; another is a reboot in place.
    #[test]
    fn another_boot_id_is_a_reboot() {
        let mut liveness = Liveness::default();
        assert_eq!(liveness.heard(Heard::Id("a".to_owned())), Verdict::Up);
        assert_eq!(liveness.heard(Heard::Id("a".to_owned())), Verdict::Up);
        assert_eq!(liveness.heard(Heard::Id("b".to_owned())), Verdict::Rebooted);
    }

    /// Only [`MISSES`] asks in a row unanswered are a hang: an answer
    /// between them (an ID, or a refusal) starts the count again.
    #[test]
    fn only_misses_in_a_row_are_a_hang() {
        let mut liveness = Liveness::default();
        assert_eq!(liveness.heard(Heard::Id("a".to_owned())), Verdict::Up);
        for _ in 1..MISSES {
            assert_eq!(liveness.heard(Heard::Nothing), Verdict::Up);
        }
        assert_eq!(liveness.heard(Heard::Alive), Verdict::Up);
        for _ in 1..MISSES {
            assert_eq!(liveness.heard(Heard::Nothing), Verdict::Up);
        }
        assert_eq!(liveness.heard(Heard::Id("a".to_owned())), Verdict::Up);
        for _ in 1..MISSES {
            assert_eq!(liveness.heard(Heard::Nothing), Verdict::Up);
        }
        assert_eq!(liveness.heard(Heard::Nothing), Verdict::Hung);
    }

    /// An older guest turns the asking off.
    #[test]
    fn an_older_guest_is_left_unasked() {
        assert_eq!(Liveness::default().heard(Heard::Unknown), Verdict::Off);
    }
}
