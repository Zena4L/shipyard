//! When the service ends: once idle (D11.2-2), no connection open and no
//! engine call running for its idle time (systemd starts it again at the
//! next connection), or once asked to stop (systemd's SIGTERM) and the
//! engine calls under way have ended. What's under way is counted under one
//! lock, and the end is decided under it: an engine call is never cut,
//! whether its connection has gone or the service is stopping, and none
//! starts once the stop is decided.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often a stopping service looks whether its calls have ended.
const STOPPING_LOOK: Duration = Duration::from_millis(50);

/// The idle time `SHIPYARD_API_IDLE` sets (whole seconds, at least 1):
/// [`DEFAULT_IDLE`](crate::DEFAULT_IDLE) when it's unset, else what it
/// says; `Err` with the default if it isn't such a number (for the caller
/// to say).
///
/// # Errors
///
/// The default, for a setting that isn't whole seconds of at least 1.
pub fn idle_time(setting: Option<&str>) -> Result<Duration, Duration> {
    match setting.map(str::parse::<u64>) {
        None => Ok(crate::DEFAULT_IDLE),
        Some(Ok(seconds @ 1..)) => Ok(Duration::from_secs(seconds)),
        Some(_) => Err(crate::DEFAULT_IDLE),
    }
}

/// What's under way, since when nothing has been, and whether the service
/// was asked to stop.
#[derive(Debug)]
pub(crate) struct Idle {
    after: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Connections open.
    connections: usize,
    /// Engine calls running.
    calls: usize,
    /// When the last of them ended (or the service started).
    since: Instant,
    /// Asked to stop: no call starts any more.
    stopping: bool,
}

/// What a [`Busy`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Connection,
    Call,
}

/// One thing under way: a connection, or an engine call. Dropped, it's
/// done.
#[derive(Debug)]
pub(crate) struct Busy(Arc<Idle>, What);

impl Idle {
    /// Nothing under way since `now`, the service to end `after` that long
    /// so.
    pub(crate) fn new(after: Duration, now: Instant) -> Arc<Self> {
        Arc::new(Self {
            after,
            state: Mutex::new(State {
                connections: 0,
                calls: 0,
                since: now,
                stopping: false,
            }),
        })
    }

    /// A connection opens.
    pub(crate) fn connection(self: &Arc<Self>) -> Busy {
        self.lock().connections += 1;
        Busy(Arc::clone(self), What::Connection)
    }

    /// An engine call starts: `None` once the service is stopping (it
    /// would be cut).
    pub(crate) fn call(self: &Arc<Self>) -> Option<Busy> {
        let mut state = self.lock();
        if state.stopping {
            return None;
        }
        state.calls += 1;
        Some(Busy(Arc::clone(self), What::Call))
    }

    /// Asked to stop: no call starts from now on.
    pub(crate) fn stop(&self) {
        self.lock().stopping = true;
    }

    /// Whether it's stopping: a stream ends then (a follow would hold the
    /// stop up for good).
    pub(crate) fn is_stopping(&self) -> bool {
        self.lock().stopping
    }

    /// At `now`, how long until the service may end: `None` to end now,
    /// else when to look again (soon, while it's stopping).
    pub(crate) fn left(&self, now: Instant) -> Option<Duration> {
        let state = self.lock();
        if state.stopping {
            // Its calls are waited for; its connections aren't (they end
            // with the service).
            return (state.calls > 0).then_some(STOPPING_LOOK);
        }
        if state.connections + state.calls > 0 {
            return Some(self.after);
        }
        let quiet = now.saturating_duration_since(state.since);
        self.after.checked_sub(quiet).filter(|left| !left.is_zero())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        match self.1 {
            What::Connection => state.connections = state.connections.saturating_sub(1),
            What::Call => state.calls = state.calls.saturating_sub(1),
        }
        if state.connections + state.calls == 0 {
            state.since = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AFTER: Duration = Duration::from_secs(60);

    /// Whole seconds, at least 1; anything else is the default, said.
    #[test]
    fn the_idle_time_is_whole_seconds() {
        assert_eq!(idle_time(None), Ok(crate::DEFAULT_IDLE));
        assert_eq!(idle_time(Some("3")), Ok(Duration::from_secs(3)));
        for wrong in ["0", "-1", "5m", "", "1.5"] {
            assert_eq!(
                idle_time(Some(wrong)),
                Err(crate::DEFAULT_IDLE),
                "{wrong:?}"
            );
        }
    }

    /// Quiet for its idle time: end. Before, look again when it would be.
    #[test]
    fn it_ends_once_quiet_for_its_idle_time() {
        let start = Instant::now();
        let idle = Idle::new(AFTER, start);
        assert_eq!(idle.left(start), Some(AFTER));
        assert_eq!(
            idle.left(start + Duration::from_secs(45)),
            Some(Duration::from_secs(15))
        );
        assert_eq!(idle.left(start + AFTER), None);
        assert_eq!(idle.left(start + AFTER * 2), None);
    }

    /// Something under way holds it up however long it has been, and its
    /// end starts the idle time again (from the end, not from the start:
    /// the service started long ago here).
    #[test]
    fn something_under_way_holds_it_up() {
        let start = Instant::now().checked_sub(AFTER * 10).unwrap();
        let idle = Idle::new(AFTER, start);
        assert_eq!(idle.left(Instant::now()), None, "quiet since long ago");
        let open = idle.connection();
        assert_eq!(idle.left(Instant::now()), Some(AFTER), "a connection alone");
        drop(open);
        let connection = idle.connection();
        let call = idle.call().unwrap();
        assert_eq!(idle.left(Instant::now()), Some(AFTER));
        drop(connection);
        // The call a closed connection left still holds it up.
        assert_eq!(idle.left(Instant::now()), Some(AFTER));
        drop(call);
        let ended = Instant::now();
        assert!(
            idle.left(ended).is_some(),
            "its idle time starts again at the end"
        );
        assert_eq!(idle.left(ended + AFTER), None);
    }

    /// Asked to stop: it waits for its calls, not for its connections, and
    /// no call starts once it's stopping.
    #[test]
    fn a_stop_waits_for_the_calls_alone() {
        let now = Instant::now();
        let idle = Idle::new(AFTER, now);
        let _connection = idle.connection();
        let call = idle.call().unwrap();
        idle.stop();
        assert_eq!(idle.left(now), Some(STOPPING_LOOK), "a call under way");
        assert!(idle.call().is_none(), "no call starts once stopping");
        drop(call);
        assert_eq!(idle.left(now), None, "an open connection doesn't hold it");
    }
}
