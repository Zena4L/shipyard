//! Running a command in a container by name (stage 9, step 9.4c1): the
//! runtime's `exec` (it joins the container's namespaces and cgroup, never
//! with more than the container's own process has), with Docker's flags.
//!
//! `-u` takes a name or a number (`USER[:GROUP]`), looked up in the
//! container's own `/etc/passwd` and `/etc/group`; its home becomes `HOME`
//! unless `-e HOME=` is given. Without `-i`, the command's input is
//! `/dev/null`.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Engine;
use crate::error::Result;

/// What to run, and how (`shipyard exec`'s flags).
#[derive(Debug, Clone, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "exec's flags (-i, -t, -d), each its own"
)]
pub struct ExecRequest {
    /// The command and its arguments.
    pub args: Vec<OsString>,
    /// Pass this process's input on (`-i`); else its input is `/dev/null`.
    pub interactive: bool,
    /// A terminal for it (`-t`).
    pub tty: bool,
    /// Return once it runs (`-d`).
    pub detach: bool,
    /// Its user (`-u USER[:GROUP]`, names or numbers); else the container's.
    pub user: Option<String>,
    /// `KEY=VALUE` entries added to the container's environment (`-e`).
    pub env: Vec<OsString>,
    /// Its working directory (`-w`), inside the container.
    pub workdir: Option<PathBuf>,
    /// How a stop request (SIGTERM or SIGINT to this process) ends it: its
    /// signal, then SIGKILL to all it started after its timeout (by default
    /// SIGTERM, then 10 s).
    pub stop: shipyard_runtime::process::StopPolicy,
}

/// How an exec went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ran {
    /// The command ended, with this exit code (128 + N for signal N).
    Exited(u8),
    /// Detached (`-d`): running, with this PID (as the host sees it).
    Detached {
        /// Its PID.
        pid: u32,
    },
}

/// Runs `request` in the container `reference` names.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`](crate::ErrorKind::NotFound): no such
///   container.
/// - [`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning): it isn't
///   running.
/// - [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): no
///   command, or `-d` with `-t` (its terminal would have nowhere to go).
/// - [`ErrorKind::Runtime`](crate::ErrorKind::Runtime): the command
///   couldn't be run ([`exit_code`] says which code that is: 127 not found,
///   126 not executable).
pub fn in_container(engine: &Engine, reference: &str, request: &ExecRequest) -> Result<Ran> {
    #[cfg(target_os = "linux")]
    return linux::in_container(engine, reference, request);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, request);
        Err(crate::error::linux_only("containers"))
    }
}

/// The exit code for `error`, from [`in_container`]: 127 when the command
/// wasn't found, 126 when it couldn't be executed (as Docker's `exec`), 125
/// for anything else (Shipyard's own failure).
#[must_use]
pub fn exit_code(error: &crate::Error) -> u8 {
    match error.find::<shipyard_runtime::container::ContainerError>() {
        Some(shipyard_runtime::container::ContainerError::Exec(cause)) => cause.exit_code(),
        _ => shipyard_runtime::process::EXIT_ENGINE_ERROR,
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::in_record;

#[cfg(target_os = "linux")]
mod linux {
    use shipyard_runtime::container::{self as runtime, ExecOptions, ExecOutcome};

    use super::{ExecRequest, Ran};
    use crate::Engine;
    use crate::containers;
    use crate::error::{Result, bail};

    pub(super) fn in_container(
        engine: &Engine,
        reference: &str,
        request: &ExecRequest,
    ) -> Result<Ran> {
        if request.args.is_empty() {
            bail!(InvalidRequest, "give a command to run");
        }
        if request.detach && request.tty {
            bail!(
                InvalidRequest,
                "-d and -t together: a detached command's terminal would have nowhere to go"
            );
        }
        let record = containers::find(engine, reference)?;
        in_record(engine, &record, request)
    }

    /// [`in_container`]'s, in the container `record` (read already: a
    /// health check's, by its ID).
    pub(crate) fn in_record(
        engine: &Engine,
        record: &containers::Record,
        request: &ExecRequest,
    ) -> Result<Ran> {
        let root = engine.paths.runtime_root()?;
        containers::require(
            engine,
            record,
            containers::Needs::Running,
            "run a command in it",
        )?;
        let mut options = ExecOptions::new(request.args.clone());
        options.cwd.clone_from(&request.workdir);
        options.env.clone_from(&request.env);
        options.user_spec.clone_from(&request.user);
        options.no_input = !request.interactive;
        options.detach = request.detach;
        options.terminal = request.tty;
        options.stop = request.stop;
        match runtime::exec(root, record.id.as_str(), &options, None)? {
            ExecOutcome::Exited(status) => Ok(Ran::Exited(status.code())),
            ExecOutcome::Detached { pid } => Ok(Ran::Detached { pid }),
            _ => bail!(Runtime, "exec ended in a way this Shipyard doesn't know"),
        }
    }
}
