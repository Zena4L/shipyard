//! Where the VM's host is in its life (step 13.6b1, F93, F100): asked over
//! its own socket (`host.sock`), never guessed from a PID. Each phase comes
//! with what's left of its bound as the host knows it, so a CLI waits by the
//! host's own limits, never by one of its own: what a stop waits for, and
//! what a start does once its own time is up.

use std::time::Duration;

/// Where the host is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPhase {
    /// Before its guest is started (its disk made, its settings read): a
    /// stop asked now ends it at once.
    Preparing,
    /// Its guest started, its agent not answering yet.
    Booting,
    /// Its agent answers, but the guest isn't ready: its data disk is
    /// checked or repaired, which no stop cuts short.
    Checking,
    /// Serving.
    Up,
    /// Stopping its guest in order.
    Stopping,
}

impl HostPhase {
    /// Its word on the wire.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Booting => "booting",
            Self::Checking => "checking",
            Self::Up => "up",
            Self::Stopping => "stopping",
        }
    }

    /// The phase a word names.
    #[must_use]
    pub fn of_word(word: &str) -> Option<Self> {
        [
            Self::Preparing,
            Self::Booting,
            Self::Checking,
            Self::Up,
            Self::Stopping,
        ]
        .into_iter()
        .find(|phase| phase.word() == word)
    }
}

/// The host's answer: its phase, and what's left of that phase's bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseAnswer {
    /// Where it is.
    pub phase: HostPhase,
    /// How long it may still take, by the host's own bound (none for `Up`).
    pub left: Duration,
}

impl PhaseAnswer {
    /// As the host says it: `PHASE SECONDS`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{} {}\n", self.phase.word(), self.left.as_secs())
    }

    /// As a CLI reads it; anything else is `None`.
    #[must_use]
    pub fn decode(text: &str) -> Option<Self> {
        let (word, seconds) = text.trim_end().split_once(' ')?;
        Some(Self {
            phase: HostPhase::of_word(word)?,
            left: Duration::from_secs(seconds.parse().ok()?),
        })
    }
}

/// The host's exit code when a stop came before it served anyone (as it
/// prepared or booted): the CLI waiting for that start says it was
/// stopped, and starts no other.
pub const STOPPED_BEFORE_SERVING: i32 = 3;

/// How long the host has to stop its guest in order (`host::STOP_LIMIT`).
pub const STOP_BOUND: Duration = Duration::from_secs(90);

/// What a CLI allows beyond the host's own bounds (its asks, its exit).
pub const MARGIN: Duration = Duration::from_secs(30);

/// How long `vm stop` waits for the host to be gone, once it has asked it to
/// stop and been told `answer`: whatever is left of a start (its guest is
/// stopped once up: a disk check is never cut), then the stop; or what's
/// left of a stop under way.
#[must_use]
pub fn stop_wait(answer: PhaseAnswer) -> Duration {
    match answer.phase {
        // (Preparing: it ends at once, nothing to stop yet.)
        HostPhase::Preparing | HostPhase::Stopping => answer.left + MARGIN,
        HostPhase::Booting | HostPhase::Checking => answer.left + STOP_BOUND + MARGIN,
        HostPhase::Up => STOP_BOUND + MARGIN,
    }
}

/// What a starting CLI does once its own time is up, by the host's answer
/// (`None`: no answer, an older host or none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtDeadline {
    /// It's up, or nearly: connect again (it serves its socket now).
    Connect,
    /// It's still within its own bound: wait that much more.
    Wait(Duration),
    /// It's past its own bound, or can't be asked: give up (asking it to
    /// stop, if it can be asked). Never a signal: a PID may be another's.
    GiveUp {
        /// Whether it's to be asked to stop.
        ask_stop: bool,
    },
}

/// [`AtDeadline`] for the host's answer: up, connect again; stopping or
/// checking, wait out what's left (a healthy VM is never stopped by a CLI
/// that gave up waiting); preparing or booting within their bound, wait;
/// past it, give up and ask it to stop.
#[must_use]
pub fn at_deadline(answer: Option<PhaseAnswer>) -> AtDeadline {
    let Some(answer) = answer else {
        return AtDeadline::GiveUp { ask_stop: false };
    };
    match answer.phase {
        HostPhase::Up => AtDeadline::Connect,
        HostPhase::Stopping | HostPhase::Checking => AtDeadline::Wait(answer.left + MARGIN),
        HostPhase::Preparing | HostPhase::Booting if answer.left > Duration::ZERO => {
            AtDeadline::Wait(answer.left + MARGIN)
        }
        HostPhase::Preparing | HostPhase::Booting => AtDeadline::GiveUp { ask_stop: true },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(phase: HostPhase, seconds: u64) -> PhaseAnswer {
        PhaseAnswer {
            phase,
            left: Duration::from_secs(seconds),
        }
    }

    /// Every phase's word both ways; an answer round-trips; anything else
    /// isn't one.
    #[test]
    fn a_phase_is_said_and_read() {
        for phase in [
            HostPhase::Preparing,
            HostPhase::Booting,
            HostPhase::Checking,
            HostPhase::Up,
            HostPhase::Stopping,
        ] {
            assert_eq!(HostPhase::of_word(phase.word()), Some(phase));
            let said = answer(phase, 42);
            assert_eq!(PhaseAnswer::decode(&said.encode()), Some(said));
        }
        for not in ["", "up", "up x", "sleeping 3", "up -1", "up 3 4"] {
            assert_eq!(PhaseAnswer::decode(not), None, "{not}");
        }
    }

    /// A stop waits out a start's rest then the stop; a stop under way, its
    /// rest; one that's up, the stop alone.
    #[test]
    fn a_stop_waits_by_the_hosts_phase() {
        let check = answer(HostPhase::Checking, 500);
        assert_eq!(
            stop_wait(check),
            Duration::from_secs(500) + STOP_BOUND + MARGIN
        );
        let boot = answer(HostPhase::Booting, 100);
        assert_eq!(
            stop_wait(boot),
            Duration::from_secs(100) + STOP_BOUND + MARGIN
        );
        assert_eq!(stop_wait(answer(HostPhase::Up, 0)), STOP_BOUND + MARGIN);
        let stop = answer(HostPhase::Stopping, 590);
        assert_eq!(stop_wait(stop), Duration::from_secs(590) + MARGIN);
        assert_eq!(
            stop_wait(answer(HostPhase::Preparing, 5)),
            Duration::from_secs(5) + MARGIN
        );
    }

    /// At a start's deadline: an up host is connected to, a stopping or
    /// checking one waited for, a booting one waited for while within its
    /// bound and asked to stop past it; no answer gives up, asking nothing.
    #[test]
    fn a_start_never_stops_a_healthy_host() {
        assert_eq!(
            at_deadline(Some(answer(HostPhase::Up, 0))),
            AtDeadline::Connect
        );
        for phase in [HostPhase::Stopping, HostPhase::Checking] {
            assert_eq!(
                at_deadline(Some(answer(phase, 0))),
                AtDeadline::Wait(MARGIN),
                "{phase:?}"
            );
        }
        assert_eq!(
            at_deadline(Some(answer(HostPhase::Booting, 20))),
            AtDeadline::Wait(Duration::from_secs(20) + MARGIN)
        );
        for phase in [HostPhase::Booting, HostPhase::Preparing] {
            assert_eq!(
                at_deadline(Some(answer(phase, 0))),
                AtDeadline::GiveUp { ask_stop: true },
                "{phase:?}"
            );
        }
        assert_eq!(at_deadline(None), AtDeadline::GiveUp { ask_stop: false });
    }
}
