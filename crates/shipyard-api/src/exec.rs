//! Exec (step 11.2b2b, D11.2b-2, D11.2b-3): `POST /containers/{id}/exec`
//! makes an instance, `POST /exec/{id}/start` runs it (the connection taken
//! over, or detached), `GET /exec/{id}/json` says how it went, and `POST
//! /exec/{id}/resize` resizes its terminal: as `docker exec` asks.
//!
//! An exec runs as a child `shipyard exec` (this very binary), not in the
//! service: the engine's exec installs a process-wide supervisor (signal
//! handlers, a subreaper) that a server of many requests can't host. Its
//! environment's values go to the child in `SHIPYARD_CALLER_ENV`, its argv
//! naming only the keys (`--env=KEY`, copied from the caller), so a value
//! is never in `ps`. With a terminal, the child gets a pseudo-terminal of
//! its own, raw (a transport, not a user's terminal), as its controlling
//! terminal, whose master is the client's stream; without one, pipes,
//! framed as Docker frames them. Each attached session runs on threads of
//! its own and counts among the streams served at once; a client that
//! hangs up gets its exec hung up too (SIGHUP to its process group, then
//! SIGKILL).
//!
//! Instances live in the service's memory, lost when it exits, as Docker's
//! are at a restart (D11.2b-3); one never started, or ended, is dropped
//! after [`KEPT`]. `Pid` is the command's, as the host sees it: the child
//! writes it to a file of the exec's once it runs (F117).
//!
//! A detached exec (`docker exec -d`) holds the service up a while at most
//! ([`DETACHED_HOLD`]): a `sleep infinity` never keeps it from its idle
//! exit. The child, and so the command, outlive the service (its unit's
//! `KillMode=process`), as a Docker exec does its daemon's restart; its
//! instance goes with the service (404 then). One its client detached from
//! with its keys holds the service up until it ends: its terminal is the
//! service's, whose exit would hang it up.

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hyper::StatusCode;
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};
use shipyard_runtime::attach::DetachKeys;
use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::terminal::{Size, set_window_size};

use crate::Shared;
use crate::create::Words;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body};

mod session;
mod spawn;

pub(crate) use session::start;
#[cfg(any(test, fuzzing))]
pub(crate) use spawn::child_command;
pub(crate) use spawn::sweep_pid_files;

type Reply = Result<hyper::Response<Body>, ApiError>;

/// How long an instance never started, or ended, is kept.
const KEPT: Duration = Duration::from_mins(5);

/// How long a detached exec holds the service up at most (F117): a short
/// one's code is kept while it's still asked for, a long one never keeps
/// the service from its idle exit.
const DETACHED_HOLD: Duration = Duration::from_secs(10);

/// The most a PID file holds.
const PID_FILE_MAX: u64 = 32;

/// After a SIGHUP to an exec whose client hung up, how long before SIGKILL.
const HANG_UP_GRACE: Duration = Duration::from_secs(10);

/// How often a session asks whether its exec has ended or its client hung
/// up; and how long an output read waits before it asks again.
const LOOK: Duration = Duration::from_millis(200);

/// After the exec's end, how long its outputs are drained at most: a
/// process it left running in the background may hold them open for good.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// The most an exec's variables may hold: they travel to the child in one
/// environment string, and the kernel takes 128 KiB at most in one.
const ENV_BYTES: usize = 120 * 1024;

/// `POST /containers/{id}/exec`'s body (Docker's `ExecConfig`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct CreateBody {
    attach_stdin: Option<bool>,
    attach_stdout: Option<bool>,
    attach_stderr: Option<bool>,
    tty: Option<bool>,
    cmd: Option<Words>,
    env: Option<Vec<String>>,
    user: Option<String>,
    working_dir: Option<String>,
    privileged: Option<bool>,
    detach_keys: Option<String>,
}

/// `POST /exec/{id}/start`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct StartBody {
    detach: Option<bool>,
    console_size: Option<[u16; 2]>,
}

/// What an exec runs, as made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecConfig {
    /// The command and its arguments.
    cmd: Vec<String>,
    /// `KEY=VALUE`s.
    env: Vec<String>,
    /// `USER[:GROUP]`.
    user: Option<String>,
    /// Its working directory.
    workdir: Option<String>,
    /// A terminal of its own.
    tty: bool,
    /// Its input attached.
    stdin: bool,
    /// Its stdout and stderr attached, by [`Stream::index`].
    streams: [bool; 2],
    /// The keys that end its session and leave it running (Docker's
    /// ctrl-p,ctrl-q unless it gives others), typed into its terminal: a
    /// session without one passes every byte on, as Docker's.
    detach_keys: DetachKeys,
    /// Its `DetachKeys` as given, for its inspect.
    detach_keys_text: String,
}

/// One exec: its container, what it runs, and how it's going.
#[derive(Debug)]
struct Instance {
    container: String,
    config: ExecConfig,
    state: Mutex<State>,
    /// Where its child writes the command's PID once it runs (in the
    /// runtime's state root, the service user's alone).
    pid_file: Option<std::path::PathBuf>,
}

/// How an exec is going.
#[derive(Debug)]
struct State {
    /// When it was made, or ended (what [`KEPT`] counts from).
    since: Instant,
    started: bool,
    running: bool,
    exit_code: Option<i32>,
    /// Its command's PID on the host, once its PID file is read (never the
    /// child `shipyard exec`'s).
    pid: Option<u32>,
    /// Its terminal's master, while it runs with one.
    master: Option<Arc<OwnedFd>>,
}

impl State {
    /// An exec made at `since`, not started.
    fn new(since: Instant) -> Self {
        Self {
            since,
            started: false,
            running: false,
            exit_code: None,
            pid: None,
            master: None,
        }
    }
}

/// The exec instances, by ID.
#[derive(Debug, Default)]
pub(crate) struct Execs(Mutex<HashMap<String, Arc<Instance>>>);

impl Execs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Instance>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The instance `id`, or Docker's 404.
    fn get(&self, id: &str) -> Result<Arc<Instance>, ApiError> {
        self.lock().get(id).cloned().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("No such exec instance: {id}"),
            )
        })
    }

    /// Keeps `instance` as `id`, dropping those kept long enough.
    fn insert(&self, id: String, instance: Arc<Instance>) {
        let mut execs = self.lock();
        let now = Instant::now();
        execs.retain(|_, kept| !kept.expired(now));
        execs.insert(id, instance);
    }
}

impl Instance {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims its one start: `false` if another has (two at once: one
    /// wins, under the state's lock).
    fn claim(&self) -> bool {
        let mut state = self.state();
        !std::mem::replace(&mut state.started, true)
    }

    /// Gives a claimed start back: its spawn failed, nothing runs (F148).
    fn unclaim(&self) {
        self.state().started = false;
    }

    /// Whether it's kept no more at `now`: not running, and made (never
    /// started) or ended [`KEPT`] ago.
    fn expired(&self, now: Instant) -> bool {
        let state = self.state();
        !state.running && now.saturating_duration_since(state.since) >= KEPT
    }
}

/// `POST /containers/{id}/exec`: an instance for a running container
/// (409 otherwise), its ID (201).
pub(crate) async fn create(shared: &Arc<Shared>, id: String, body: Incoming) -> Reply {
    #[derive(Serialize)]
    struct Created {
        #[serde(rename = "Id")]
        id: String,
    }
    let body: CreateBody = crate::read_json(body).await?;
    let config = config(body)?;
    let standing = running(shared, id).await?;
    let exec_id = shipyard_engine::docker::new_id()?;
    // Without one, its `Pid` stays 0: said in the log, never the exec's.
    let pid_file = shared
        .paths
        .exec_pid_file(&exec_id)
        .inspect_err(|error| tracing::warn!("exec {exec_id}'s PID won't be known: {error:#}"))
        .ok();
    let instance = Arc::new(Instance {
        container: standing.id.as_str().to_owned(),
        config,
        state: Mutex::new(State::new(Instant::now())),
        pid_file,
    });
    shared.execs.insert(exec_id.clone(), instance);
    Ok(reply::json(StatusCode::CREATED, &Created { id: exec_id }))
}

/// Docker's exec config as Shipyard's: a command needed; `Privileged`
/// refused (Shipyard has no privileged containers yet); variables no
/// bigger than the child's environment takes; detach keys that are keys.
pub(crate) fn config(body: CreateBody) -> Result<ExecConfig, ApiError> {
    if body.privileged == Some(true) {
        return Err(ApiError::bad(
            "Privileged isn't supported: give the container the capabilities it needs",
        ));
    }
    let cmd = body.cmd.map(|words| words.0).unwrap_or_default();
    if cmd.is_empty() {
        return Err(ApiError::bad("No exec command specified"));
    }
    let env = body.env.unwrap_or_default();
    // Measured as the child gets them: one encoded string (escapes grow
    // it), never past what the kernel takes in one (11.6b's API review).
    let values: Vec<(String, String)> = env
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    let encoded = shipyard_runtime::attach::forward::caller_env_value(&values);
    if env.iter().map(String::len).sum::<usize>() > ENV_BYTES || encoded.len() > ENV_BYTES {
        return Err(ApiError::bad(format!(
            "an exec's variables may hold {} KiB at most",
            ENV_BYTES / 1024
        )));
    }
    let nonempty = |text: Option<String>| text.filter(|text| !text.is_empty());
    let detach_keys_text = body.detach_keys.unwrap_or_default();
    let detach_keys = if detach_keys_text.is_empty() {
        DetachKeys::default_keys()
    } else {
        DetachKeys::parse(&detach_keys_text).map_err(|error| ApiError::bad(error.to_string()))?
    };
    Ok(ExecConfig {
        cmd,
        env,
        user: nonempty(body.user),
        workdir: nonempty(body.working_dir),
        tty: body.tty.unwrap_or(false),
        stdin: body.attach_stdin.unwrap_or(false),
        streams: [
            body.attach_stdout.unwrap_or(false),
            body.attach_stderr.unwrap_or(false),
        ],
        detach_keys,
        detach_keys_text,
    })
}

/// Container `reference`'s standing, if it's running.
///
/// # Errors
///
/// Not found (404); not running (409, Docker's words).
async fn running(
    shared: &Arc<Shared>,
    reference: String,
) -> Result<shipyard_engine::docker::Standing, ApiError> {
    let standing = shared.standing(reference).await?;
    if !standing.started() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("Container {} is not running", standing.id),
        ));
    }
    Ok(standing)
}

/// `GET /exec/{id}/json`'s answer (Docker's `ExecInspectResponse`).
#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a mirror of Docker's answer, which has these booleans"
)]
#[serde(rename_all = "PascalCase")]
struct ExecInspect {
    #[serde(rename = "ID")]
    id: String,
    running: bool,
    exit_code: Option<i32>,
    process_config: ProcessConfig,
    open_stdin: bool,
    open_stdout: bool,
    open_stderr: bool,
    can_remove: bool,
    #[serde(rename = "ContainerID")]
    container_id: String,
    detach_keys: String,
    /// The command's, as the host sees it (0 until it runs).
    pid: u32,
}

/// An exec's process, as Docker says it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessConfig {
    privileged: bool,
    user: String,
    tty: bool,
    entrypoint: String,
    arguments: Vec<String>,
}

/// `GET /exec/{id}/json`.
pub(crate) fn inspect(shared: &Shared, exec_id: String) -> Reply {
    let instance = shared.execs.get(&exec_id)?;
    Ok(reply::json(StatusCode::OK, &inspected(exec_id, &instance)))
}

/// What `GET /exec/{id}/json` says of `instance`.
fn inspected(id: String, instance: &Instance) -> ExecInspect {
    let state = instance.state();
    let config = &instance.config;
    let (entrypoint, arguments) = config
        .cmd
        .split_first()
        .map_or((String::new(), Vec::new()), |(first, rest)| {
            (first.clone(), rest.to_vec())
        });
    ExecInspect {
        id,
        running: state.running,
        exit_code: state.exit_code,
        process_config: ProcessConfig {
            privileged: false,
            user: config.user.clone().unwrap_or_default(),
            tty: config.tty,
            entrypoint,
            arguments,
        },
        open_stdin: config.stdin,
        open_stdout: config.streams[Stream::Stdout.index()],
        open_stderr: config.streams[Stream::Stderr.index()],
        can_remove: false,
        container_id: instance.container.clone(),
        detach_keys: config.detach_keys_text.clone(),
        pid: state.pid.unwrap_or(0),
    }
}

/// `POST /exec/{id}/resize` (`h`, `w`): its terminal resized (the child's
/// own resizes the container's at the SIGWINCH the kernel sends it). 409
/// if it has no terminal, or isn't running.
pub(crate) fn resize(shared: &Shared, exec_id: &str, query: &Query) -> Reply {
    let rows: u16 = query
        .parsed("h")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let cols: u16 = query
        .parsed("w")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let instance = shared.execs.get(exec_id)?;
    let master = instance.state().master.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            format!("exec {exec_id} has no terminal running"),
        )
    })?;
    set_window_size(master.as_raw_fd(), Size::new(rows, cols))
        .map_err(|error| ApiError::internal(format!("the exec's terminal: {error}")))?;
    Ok(reply::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests;
