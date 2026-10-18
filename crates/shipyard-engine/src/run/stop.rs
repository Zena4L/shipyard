//! Ending containers by name (stage 9, step 9.4b): `stop`, `kill`, `wait`
//! and `restart`, on containers another process started.
//!
//! **`stop`** asks the container's monitor to stop it, through its control
//! channel (the runtime's [`stop`](runtime::stop)): the stop signal now,
//! SIGKILL at the timeout. The monitor sees it through, so a `stop` killed
//! meanwhile changes nothing. `stop` holds the container's operation lock
//! throughout (a `start` or `rm` waits), and returns once the container's
//! end is settled (its `running` file free): a `start` right after works.
//!
//! **`kill`** signals the container's init and returns: no lock, so it
//! reaches a container a long `stop` is waiting on.
//!
//! **`wait`** stays until the container has ended, as its monitor says
//! ([`wait`](runtime::wait)); one that has ended already says how from its
//! record.
//!
//! **`restart`** is a `stop`, then a `start`, under one hold of the
//! operation lock: nothing comes between.

use std::time::Duration;

use shipyard_runtime::process::Signal;

#[cfg(target_os = "linux")]
use shipyard_runtime::container as runtime;

use crate::Engine;
use crate::containers::Record;
use crate::error::{Error, ErrorKind, Result};

/// How a container is stopped: its own stop signal and timeout
/// (`--stop-signal`, `--stop-timeout` when it was made), unless these are
/// given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopOptions {
    /// The signal sent first.
    pub signal: Option<Signal>,
    /// How long before SIGKILL.
    pub timeout: Option<Duration>,
}

/// The longest stop timeout: what a control message carries (seconds, 32
/// bits; over a century).
const MAX_TIMEOUT: Duration = Duration::from_secs(u32::MAX as u64);

impl StopOptions {
    /// Whether the signal given (if one is) can stop a container: checked
    /// before anything is taken, so a wrong one is said whatever the
    /// container's state.
    ///
    /// # Errors
    ///
    /// One that stops or continues it, or that its init can't pass on
    /// ([`ErrorKind::InvalidRequest`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux's"))]
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(signal) = self.signal {
            shipyard_runtime::process::StopPolicy {
                signal,
                timeout: Duration::ZERO,
            }
            .validate()?;
        }
        Ok(())
    }

    /// The policy for the container `record` is: these, else its own.
    ///
    /// # Errors
    ///
    /// Its stored stop signal can't be read ([`ErrorKind::Storage`]), or the
    /// signal can't stop it ([`ErrorKind::InvalidRequest`], as
    /// [`check`](Self::check)).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux's"))]
    pub(crate) fn policy(&self, record: &Record) -> Result<shipyard_runtime::process::StopPolicy> {
        let signal = match self.signal {
            Some(signal) => signal,
            None => record.config.stop_signal.parse().map_err(|_| {
                Error::new(
                    ErrorKind::Storage,
                    format!(
                        "container {}'s stop signal {:?} can't be read",
                        record.name, record.config.stop_signal
                    ),
                )
            })?,
        };
        let timeout = self
            .timeout
            .unwrap_or(Duration::from_secs(record.config.stop_timeout))
            .min(MAX_TIMEOUT);
        let policy = shipyard_runtime::process::StopPolicy { signal, timeout };
        policy.validate()?;
        Ok(policy)
    }
}

/// Stops the container `reference` names, as `options` say: its stop
/// signal, then SIGKILL at the timeout (its monitor sees that through).
/// Returns once its end is settled. One that isn't up is left as it is.
///
/// # Errors
///
/// No such container ([`ErrorKind::NotFound`]);
/// a signal that can't stop it
/// ([`ErrorKind::InvalidRequest`]);
/// another command holds it, or its end takes longer than its timeout and a
/// minute ([`ErrorKind::Busy`]).
pub fn stop(engine: &Engine, reference: &str, options: StopOptions) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::stop(engine, reference, options);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, options);
        Err(crate::error::linux_only("containers"))
    }
}

/// Sends `signal` to the init of the container `reference` names, and
/// returns: nothing is waited for, and no lock taken (it reaches a
/// container a `stop` waits on).
///
/// # Errors
///
/// No such container; one that isn't running
/// ([`ErrorKind::NotRunning`]); SIGSTOP
/// ([`ErrorKind::Runtime`]: it would stop the
/// init, not its command).
pub fn kill(engine: &Engine, reference: &str, signal: Signal) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::kill(engine, reference, signal);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, signal);
        Err(crate::error::linux_only("containers"))
    }
}

/// Waits for the container `reference` names to end: its exit code (125
/// if how it ended isn't known). One that isn't up says how it last ended
/// at once (0 if it never ran, as Docker).
///
/// # Errors
///
/// No such container, or one removed (`--rm`) before its end could be
/// read ([`ErrorKind::NotFound`]).
pub fn wait(engine: &Engine, reference: &str) -> Result<i32> {
    #[cfg(target_os = "linux")]
    return linux::wait(engine, reference);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference);
        Err(crate::error::linux_only("containers"))
    }
}

/// Stops the container `reference` names (as [`stop`]), then starts it,
/// under one hold of its operation lock. One that isn't up is started.
///
/// # Errors
///
/// As [`stop`] and [`start`](fn@crate::run::start); a container removed as it
/// stopped (`--rm`) ([`ErrorKind::NotFound`]).
pub fn restart(engine: &Engine, reference: &str, options: StopOptions) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::restart(engine, reference, options);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, options);
        Err(crate::error::linux_only("containers"))
    }
}

/// Stops container `record`, up (its runtime state under `runtime_root`),
/// as `policy` says, through its monitor, which sees it through: returns
/// once it's asked. A monitor that can't be asked is gone or letting go (the
/// container has ended: a monitor killed takes its init with it, its
/// parent-death signal): nothing to stop. Either way the caller waits for
/// the end to settle.
///
/// # Errors
///
/// The monitor couldn't be asked: too many control clients (busy), a
/// signal that can't stop it.
#[cfg(target_os = "linux")]
pub(crate) fn stop_up(
    runtime_root: &std::path::Path,
    record: &Record,
    policy: shipyard_runtime::process::StopPolicy,
) -> Result<()> {
    runtime::stop(
        runtime_root,
        record.id.as_str(),
        policy.signal,
        policy.timeout,
    )
    .map(drop)
    .map_err(|error| Error::from(error).context(format!("stopping container {}", record.name)))
}

#[cfg(target_os = "linux")]
mod linux {
    use shipyard_runtime::container::{self as runtime, ContainerError, Status};
    use shipyard_runtime::process::Signal;

    use super::StopOptions;
    use crate::Engine;
    use crate::containers::{self, ContainerState, Record};
    use crate::error::{Error, ErrorKind, Result, bail};
    use crate::run::start::Input;
    use crate::run::start::start_taken;
    use crate::run::take::{Taken, Took, WhenUp, take};

    pub(super) fn stop(engine: &Engine, reference: &str, options: StopOptions) -> Result<Record> {
        options.check()?;
        match take(engine, reference, WhenUp::Stop(options))? {
            Took::Taken(Taken { record, .. }) | Took::Gone(record) => {
                crate::events::emit(engine, &crate::events::container(&record, "stop"));
                Ok(record)
            }
        }
    }

    pub(super) fn kill(engine: &Engine, reference: &str, signal: Signal) -> Result<Record> {
        let record = containers::find(engine, reference)?;
        let runtime_root = engine.paths.runtime_root()?;
        // Only one that runs: one still starting (its init at the gate)
        // would get it when started.
        if !runtime::peek_status(runtime_root, record.id.as_str())?.is_some_and(Status::is_live) {
            bail!(NotRunning, "container {} is not running", record.name);
        }
        // SIGKILL or its stop signal is the user's stop: recorded before
        // the signal goes (a restart policy leaves it so). Another (a
        // reload's SIGHUP) isn't.
        if signal.as_raw() == Signal::KILL.as_raw()
            || StopOptions::default()
                .policy(&record)
                .is_ok_and(|policy| policy.signal.as_raw() == signal.as_raw())
        {
            crate::run::take::mark_stopped_by_user(engine, &record.id);
        }
        match runtime::kill(runtime_root, record.id.as_str(), signal) {
            Ok(()) => {
                crate::events::emit(
                    engine,
                    &crate::events::container(&record, "kill").with("signal", signal),
                );
                Ok(record)
            }
            Err(ContainerError::NotFound(_) | ContainerError::WrongStatus { .. }) => {
                bail!(NotRunning, "container {} is not running", record.name)
            }
            Err(error) => Err(Error::from(error)),
        }
    }

    pub(super) fn wait(engine: &Engine, reference: &str) -> Result<i32> {
        let record = containers::find(engine, reference)?;
        match containers::state(engine, &record)? {
            ContainerState::Running { .. } | ContainerState::Paused => {
                let runtime_root = engine.paths.runtime_root()?;
                match runtime::wait(runtime_root, record.id.as_str()) {
                    Ok(Some(code)) => return Ok(code),
                    // Ended meanwhile, or its monitor too busy to let one
                    // more in: its settled end says.
                    Ok(None) | Err(ContainerError::MonitorRefused { .. }) => {}
                    Err(error) => return Err(Error::from(error)),
                }
            }
            ContainerState::Stopping => {}
            ContainerState::Created => return Ok(0),
            // Ended (how it last ended, while its policy restarts it).
            ContainerState::Exited { code, .. } | ContainerState::Restarting { code, .. } => {
                return Ok(code.unwrap_or(containers::UNKNOWN_EXIT));
            }
        }
        // Ended, its monitor letting go: once its end is settled (its
        // `running` free), the record says how.
        let removed = || {
            Error::new(
                ErrorKind::NotFound,
                format!(
                    "container {} was removed (--rm) before its end could be read",
                    record.name
                ),
            )
        };
        // (It waits on: `wait` waits as long as the container runs.)
        loop {
            match containers::settled(engine, &record.id, containers::SETTLE_WAIT) {
                Ok(Some(settled)) => {
                    return Ok(settled
                        .exit
                        .map_or(containers::UNKNOWN_EXIT, |exit| exit.code_or_unknown()));
                }
                Ok(None) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => return Err(removed()),
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn restart(
        engine: &Engine,
        reference: &str,
        options: StopOptions,
    ) -> Result<Record> {
        options.check()?;
        crate::run::recover::recover_others(engine, None);
        match take(engine, reference, WhenUp::Stop(options))? {
            Took::Taken(taken) => {
                let record = start_taken(engine, taken, |_| {}, None, Input::AsMade, |_| {})?;
                crate::events::emit(engine, &crate::events::container(&record, "restart"));
                Ok(record)
            }
            Took::Gone(record) => bail!(
                NotFound,
                "container {} was removed as it stopped (--rm): nothing to start",
                record.name
            ),
        }
    }
}
