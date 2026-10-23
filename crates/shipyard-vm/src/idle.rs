//! The VM's idle stop (step 10.3c2, D20, D10.3-11): it stops itself once
//! nothing has used it for a while: no command (a client of the host's
//! socket), no container active (running, paused, stopping or waiting to
//! restart), the guest answering. The host's clients are counted here, and
//! the decision is taken under their lock (amendment 7): from the moment
//! it's taken, no client is let in (one that came is dropped before its
//! command reaches the guest, and its CLI starts the VM again).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the VM may be idle before it stops itself (D10.3-11): no
/// command, no container active.
pub(crate) const IDLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// The idle limit `SHIPYARD_VM_IDLE` sets (whole seconds, at least 1):
/// [`IDLE_LIMIT`] when it's unset, else what it says, or `Err` with the
/// default if it isn't such a number (said in the host's log).
pub(crate) fn idle_limit(
    setting: Option<&str>,
) -> Result<std::time::Duration, std::time::Duration> {
    match setting.map(str::parse::<u64>) {
        None => Ok(IDLE_LIMIT),
        Some(Ok(seconds @ 1..)) => Ok(std::time::Duration::from_secs(seconds)),
        Some(_) => Err(IDLE_LIMIT),
    }
}

/// Whether the VM has been idle long enough to stop: quiet (no command,
/// no container active, the guest answering) since `quiet_since`, for
/// `limit` by `now`. Never without a quiet start.
#[must_use]
pub(crate) fn idle(
    quiet_since: Option<std::time::Instant>,
    now: std::time::Instant,
    limit: std::time::Duration,
) -> bool {
    quiet_since.is_some_and(|since| now.saturating_duration_since(since) >= limit)
}

/// The host's clients: how many it serves, whether it's stopping, and when
/// one last came or went.
#[derive(Debug)]
pub(crate) struct Clients {
    state: Mutex<Served>,
}

#[derive(Debug)]
struct Served {
    count: usize,
    stopping: bool,
    last: Instant,
}

impl Clients {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Served {
                count: 0,
                stopping: false,
                last: Instant::now(),
            }),
        }
    }

    /// A client come: counted, unless the host is stopping (`false`: it's
    /// to be dropped, its command never sent on).
    pub(crate) fn come(&self) -> bool {
        let mut state = self.lock();
        if state.stopping {
            return false;
        }
        state.count += 1;
        state.last = Instant::now();
        true
    }

    /// A client gone.
    pub(crate) fn gone(&self) {
        let mut state = self.lock();
        state.count = state.count.saturating_sub(1);
        state.last = Instant::now();
    }

    /// The host is stopping (`vm stop`, a logout): no client is let in from
    /// here (each refused, its CLI waits for the stop and starts the VM
    /// again).
    pub(crate) fn stop(&self) {
        self.lock().stopping = true;
    }

    /// Whether the host is stopping.
    pub(crate) fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// The state, even past a thread that panicked holding it (nothing
    /// under it can be left half done: plain counts): a poisoned lock read
    /// as "stopping" would refuse every client for good.
    fn lock(&self) -> std::sync::MutexGuard<'_, Served> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the host does after a look at the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Nothing yet.
    Wait,
    /// Idle past the limit, but this many containers are active: said once
    /// a stretch (the VM stays up).
    KeptUp(u32),
    /// Idle for the limit: stop. No client is let in from here.
    Stop,
    /// Quiet but for a guest that hasn't answered for the limit: never
    /// stopped as idle (what it runs isn't known), said once a stretch.
    NoAnswer,
}

/// The quiet stretch: since when nothing has used the VM, and since when
/// containers have kept it up.
#[derive(Debug)]
pub(crate) struct Idle {
    limit: Duration,
    quiet_since: Option<Instant>,
    active_since: Option<Instant>,
    kept_up_said: bool,
    unanswered_since: Option<Instant>,
}

impl Idle {
    pub(crate) fn new(limit: Duration) -> Self {
        Self {
            limit,
            quiet_since: None,
            active_since: None,
            kept_up_said: false,
            unanswered_since: None,
        }
    }

    /// The limit.
    pub(crate) fn limit(&self) -> Duration {
        self.limit
    }

    /// After a look at the guest at `now`: how many containers are active,
    /// or `None` if it didn't answer (a guest that can't answer is never
    /// stopped as idle: what it runs isn't known). Takes `clients`' lock:
    /// a [`Decision::Stop`] is taken with none served, and from it none is
    /// let in.
    pub(crate) fn after(
        &mut self,
        clients: &Clients,
        active: Option<u32>,
        now: Instant,
    ) -> Decision {
        let mut state = clients.lock();
        if state.stopping {
            return Decision::Wait;
        }
        let quiet = active == Some(0) && state.count == 0;
        if quiet {
            let since = self.quiet_since.get_or_insert(now);
            // A command that came and went since: quiet from its end.
            *since = (*since).max(state.last);
        } else {
            self.quiet_since = None;
        }
        match active {
            Some(0) => {
                self.active_since = None;
                self.kept_up_said = false;
            }
            Some(count) => {
                let since = *self.active_since.get_or_insert(now);
                if !self.kept_up_said && idle(Some(since), now, self.limit) {
                    self.kept_up_said = true;
                    return Decision::KeptUp(count);
                }
            }
            None => {}
        }
        if active.is_some() {
            self.unanswered_since = None;
        } else if self.unanswered_since.is_none() {
            self.unanswered_since = Some(now);
        } else if idle(self.unanswered_since, now, self.limit) {
            // Said once a stretch: from here, as if just begun.
            self.unanswered_since = Some(now);
            return Decision::NoAnswer;
        }
        // (`quiet_since` is set only with no client served.)
        if idle(self.quiet_since, now, self.limit) {
            state.stopping = true;
            return Decision::Stop;
        }
        Decision::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(300);

    /// Idle once quiet for the limit, not a moment before; never without
    /// a quiet start.
    #[test]
    fn idle_is_quiet_for_the_limit() {
        let start = std::time::Instant::now();
        let limit = std::time::Duration::from_secs(300);
        let early = start
            + limit
                .checked_sub(std::time::Duration::from_millis(1))
                .unwrap();
        assert!(!idle(Some(start), early, limit));
        assert!(idle(Some(start), start + limit, limit));
        assert!(idle(Some(start), start + limit * 2, limit));
        assert!(!idle(None, start + limit * 2, limit));
    }

    /// `SHIPYARD_VM_IDLE` is whole seconds, at least 1; anything else is
    /// the default, said.
    #[test]
    fn the_idle_limit_is_whole_seconds() {
        use std::time::Duration;
        assert_eq!(idle_limit(None), Ok(IDLE_LIMIT));
        assert_eq!(idle_limit(Some("3")), Ok(Duration::from_secs(3)));
        for wrong in ["0", "-1", "5m", "", "1.5"] {
            assert_eq!(idle_limit(Some(wrong)), Err(IDLE_LIMIT), "{wrong:?}");
        }
    }

    /// Quiet for the limit (no client, nothing active, answering): stop,
    /// and from then no client is let in.
    #[test]
    fn quiet_for_the_limit_stops_and_lets_no_one_in() {
        let (clients, mut idle) = (Clients::new(), Idle::new(LIMIT));
        let start = Instant::now();
        assert_eq!(idle.after(&clients, Some(0), start), Decision::Wait);
        assert_eq!(
            idle.after(&clients, Some(0), start + LIMIT / 2),
            Decision::Wait
        );
        assert_eq!(idle.after(&clients, Some(0), start + LIMIT), Decision::Stop);
        assert!(!clients.come(), "no client once stopping");
        assert_eq!(
            idle.after(&clients, Some(0), start + LIMIT * 2),
            Decision::Wait
        );
    }

    /// A client served, an active container, or a guest that doesn't
    /// answer: never idle, and the quiet stretch starts again after.
    #[test]
    fn use_keeps_it_up() {
        let start = Instant::now();
        let (clients, mut idle) = (Clients::new(), Idle::new(LIMIT));
        assert!(clients.come());
        assert_eq!(
            idle.after(&clients, Some(0), start + LIMIT * 2),
            Decision::Wait
        );
        clients.gone();

        // (After every client's end: only the interruption can begin the
        // stretch again.)
        let start = Instant::now() + Duration::from_secs(1);
        for interruption in [Some(1), None] {
            let mut idle = Idle::new(LIMIT);
            assert_eq!(idle.after(&clients, Some(0), start), Decision::Wait);
            let _ = idle.after(&clients, interruption, start + LIMIT / 2);
            assert_eq!(
                idle.after(&clients, Some(0), start + LIMIT),
                Decision::Wait,
                "{interruption:?} began the stretch again"
            );
        }
    }

    /// A command that came and went inside a quiet stretch begins it again
    /// from its end: the VM never stops right after a command.
    #[test]
    fn a_command_since_counts() {
        let (clients, mut idle) = (Clients::new(), Idle::new(LIMIT));
        let start = Instant::now();
        assert_eq!(idle.after(&clients, Some(0), start), Decision::Wait);
        // (The command's end after the stretch's start, on any clock.)
        std::thread::sleep(Duration::from_millis(2));
        assert!(clients.come());
        clients.gone();
        let ended = Instant::now();
        assert_eq!(idle.after(&clients, Some(0), start + LIMIT), Decision::Wait);
        assert_eq!(idle.after(&clients, Some(0), ended + LIMIT), Decision::Stop);
    }

    /// A guest that doesn't answer for the limit is never stopped as idle,
    /// and that's said.
    #[test]
    fn a_guest_that_doesnt_answer_is_said_not_stopped() {
        let (clients, mut idle) = (Clients::new(), Idle::new(LIMIT));
        let start = Instant::now();
        assert_eq!(idle.after(&clients, Some(0), start), Decision::Wait);
        assert_eq!(
            idle.after(&clients, None, start + LIMIT / 2),
            Decision::Wait
        );
        assert_eq!(
            idle.after(&clients, None, start + LIMIT * 2),
            Decision::NoAnswer
        );
        assert!(clients.come(), "not stopping");
    }

    /// `vm stop`: from it, no client is let in.
    #[test]
    fn a_stop_lets_no_one_in() {
        let clients = Clients::new();
        assert!(clients.come());
        clients.stop();
        assert!(!clients.come());
    }

    /// Containers active past the limit: said once a stretch, never a stop.
    #[test]
    fn active_containers_are_said_once() {
        let (clients, mut idle) = (Clients::new(), Idle::new(LIMIT));
        let start = Instant::now();
        assert_eq!(idle.after(&clients, Some(2), start), Decision::Wait);
        assert_eq!(
            idle.after(&clients, Some(2), start + LIMIT),
            Decision::KeptUp(2)
        );
        assert_eq!(
            idle.after(&clients, Some(2), start + LIMIT * 3),
            Decision::Wait
        );
        assert_eq!(
            idle.after(&clients, Some(0), start + LIMIT * 3),
            Decision::Wait
        );
        assert_eq!(
            idle.after(&clients, Some(1), start + LIMIT * 4),
            Decision::Wait
        );
        assert_eq!(
            idle.after(&clients, Some(1), start + LIMIT * 5),
            Decision::KeptUp(1)
        );
    }
}
