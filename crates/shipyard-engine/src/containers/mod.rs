//! Containers' records: the engine's own, persistent account of each
//! container (who it is, how to run it, how it last ran), beside the
//! runtime's state, which lasts only while the container does.
//!
//! **On disk** ([`Paths::containers`](crate::Paths::containers)): one
//! directory per container, named by its ID, holding `record.json`
//! ([`Record`], versioned), its starts' `logs/<N>`, `cleanup.log` (what its
//! last `__cleanup` couldn't do), `previous-start.json` while a start is in
//! flight (what the start before it left), `end-unsaid` from a start until
//! its `die` is said (F88), and three lock files: `op.lock`, which
//! `start` and `rm` take in turn; `running`, held by a start's monitor and
//! its `__cleanup` until all of that start is let go; and `restarting`,
//! held by the `__cleanup` that waits to start the container again. The
//! store's own `store-id` names it in the image store's areas. The store is
//! the caller's alone (checked), and a record is only theirs to read
//! (0600: it holds the container's environment).
//!
//! **The locks' order:** a container's `op.lock`, then its `running`, then
//! networks' locks (one at a time, or in name order), then the image store's
//! lock, then this store's (briefly). A volume's fill lock (a create's
//! copy-up) is innermost: taken while its volume is held shared, and
//! nothing else is taken under it. `__cleanup` takes
//! neither of a container's: nothing waits on it holding what it needs.
//! `restarting` is outside the order: its holder only tries the others, and
//! a holder of the others only tries it, never waiting.
//!
//! **Writers take turns, readers never wait.** Making and removing a
//! container hold the store's lock (`.lock`, `flock`): names and serials
//! stay unique. A container appears whole (its directory made under a
//! temporary name, then renamed into place) and goes whole (renamed out,
//! then removed), so a reader ([`list`], `find`) that takes no lock sees
//! a container or nothing, never part of one. What a writer killed half
//! way leaves (`.new-…`, `.gone-…`) is swept by the next.
//!
//! **A writer reads strictly, a reader leniently.** [`list`] leaves out a
//! record it can't read (a newer Shipyard's, a damaged one), and says so;
//! `create_record` refuses to go on, since that record's name and serial
//! are still its container's.
//!
//! Known limits: the store's own directory is checked, not its parents'
//! (as the runtime's state root); a serial or a name is free again once
//! its container is removed, as Docker's names are.

mod config;
mod created;
mod id;
pub mod isolation;
mod labels;
pub mod links;
#[cfg(target_os = "linux")]
mod locks;
mod lookup;
mod names;
mod record;
mod starts;
mod state;
mod store;
pub mod usage;
mod view;

use std::time::Duration;

pub use config::{
    Amount, Bridge, CgroupLimits, Config, Device, DeviceCgroupRule, Host, IdRange, IoLimit,
    Network, Port, RestartPolicy, Security, StoredConfig, Sysctl, Ulimit, User,
};
pub use created::Created;
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_imports, reason = "a container's create is Linux's")
)]
pub(crate) use created::MAX_EXPOSED;
pub(crate) use created::{created, exposed_ports, network_from, write_created};
pub use id::{ContainerId, Serial};
pub use labels::Labels;
#[cfg(target_os = "linux")]
pub(crate) use locks::{
    Locked, OP_WAIT, SETTLE_WAIT, lock_again, lock_found, operation_lock, restart_pending,
    restarting_lock, running_lock, running_lock_until, settled, wait_settled,
};
pub(crate) use lookup::exists;
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_imports, reason = "its users are Linux's")
)]
pub(crate) use lookup::find;
pub use lookup::list;
#[cfg(target_os = "linux")]
pub(crate) use lookup::list_every;
#[cfg(target_os = "linux")]
pub(crate) use lookup::read_one;
#[cfg(test)]
use lookup::record_path;
pub use names::Name;
#[cfg(target_os = "linux")]
pub(crate) use record::UNKNOWN_EXIT;
pub use record::{BoundPort, Exit, Record};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use starts::Kept;
#[cfg(target_os = "linux")]
pub(crate) use starts::keep_run_dir;
#[cfg(any(test, target_os = "linux"))]
pub(crate) use starts::next_logs_name;
#[cfg(target_os = "linux")]
pub(crate) use starts::{
    PreviousStart, done_with_previous_start, end_said, end_unsaid, forget_previous_start,
    keep_previous_start, mark_end_unsaid, previous_start, settle_never_ran, settle_unstarted,
};
pub(crate) use starts::{etc_dir, last_logs_name, starts};
pub use starts::{health, last_start_lost, network_lost};
pub use state::ContainerState;
pub use state::state;
#[cfg(target_os = "linux")]
pub(crate) use state::unmet;
pub(crate) use state::{Needs, has_monitor, require};
pub use store::NewContainer;
#[cfg(test)]
pub(crate) use store::create_record;
#[cfg(target_os = "linux")]
pub(crate) use store::require_store_id;
pub(crate) use store::store_id;
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_imports, reason = "their users are Linux's")
)]
pub(crate) use store::{
    check_name, create_record_avoiding, logs_dir, remove_record, rename, update,
    with_records_locked,
};

/// The store's own writers and lookups, beneath the locks and checks of
/// the commands that use them: for the engine's tests and example only, no
/// caller's API (records are made, changed and removed through `run`).
#[doc(hidden)]
pub mod testing {
    pub use super::lookup::find;
    pub use super::store::{create_record, logs_dir, record_dir, remove_record, update};
}
#[cfg(test)]
use store::{Store, create_drawing};
#[cfg(test)]
pub(crate) use view::shown_command;
pub(crate) use view::{Gathered, gathered, shown_entrypoint_and_cmd};
pub use view::{
    Inspected, InspectedState, Ports, SOCKETS_VERSION, Sock, Sockets, Summary, inspect, ports,
    published, sockets, summaries,
};
#[cfg(target_os = "linux")]
pub(crate) use view::{init_of, init_sockets};

/// A record's file in its container's directory.
const RECORD: &str = "record.json";

/// The directory of a container's logs, in its own (see [`logs_dir`]).
const LOGS: &str = "logs";
/// The store's lock, taken by writers.
const LOCK: &str = ".lock";
/// A container being made: its directory before it's renamed into place.
const NEW: &str = ".new-";
/// A container being removed: its directory once renamed out of place.
const GONE: &str = ".gone-";
/// A container's operation lock, in its directory: taken by `start` and
/// `rm` (9.4a), so two of them on one container never interleave.
#[cfg(target_os = "linux")]
const OP_LOCK: &str = "op.lock";
/// A container's liveness, in its directory: locked by a start and handed
/// to its monitor, then to `__cleanup`, which closes it last (9.4a). Free,
/// everything of the last start has been let go.
#[cfg(target_os = "linux")]
const RUNNING: &str = "running";
/// The lock file held while a container's `__cleanup` decides whether to
/// restart it, and while it waits to (stage 9, step 9.5b): free, no restart
/// waits.
#[cfg(target_os = "linux")]
const RESTARTING: &str = "restarting";
/// The store's identity (random, made once), written into the writable
/// layers its containers own in an image store: an engine sweeps only its
/// own store's (two container stores may share an image store).
const STORE_ID: &str = "store-id";

/// How long a writer waits for the store's lock: far longer than any
/// writer holds it.
const LOCK_WAIT: Duration = Duration::from_secs(15);

/// How many serials and generated names are drawn before giving up on
/// finding a free one (the serials), or numbering one (the names).
const DRAWS: usize = 64;

/// How many matches an ambiguous reference's error names.
const MATCHES_SHOWN: usize = 10;

#[cfg(test)]
pub(crate) mod tests;
