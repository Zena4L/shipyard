//! What the engine tells its caller while it works: warnings (something
//! asked for that it goes on without), notices, hints, a run's failure and
//! a pull's progress.
//!
//! The engine never prints. Some of what it has to say happens in the
//! middle of an operation, or after it (a run's network cleaned up as it
//! ends, a pull's layers as they arrive), so it can't all wait to be
//! returned: the caller passes a [`Reporter`], and the engine reports each
//! [`Message`] as it happens. The CLI prints them; an API can send them on, or
//! [`Collect`] them.

use std::sync::{Mutex, PoisonError};

/// Where the engine's [`Message`]s go. Called from any thread the engine
/// works on, and from destructors: it must not panic.
pub trait Reporter: Send + Sync {
    /// Takes one message.
    fn report(&self, message: Message);
}

/// Something the engine has to say. Not `non_exhaustive`: a caller that
/// prints should be told, when it's compiled, of a message it doesn't print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Something asked for that the engine goes on without, or something
    /// it couldn't clean up.
    Warning(Warning),
    /// Something the user should know that isn't a problem (a port the
    /// kernel chose).
    Notice(String),
    /// What the user can do about an error (one just reported, or about to
    /// be returned).
    Hint(String),
    /// A run's command couldn't be started or supervised (not found, an
    /// isolation refused): said when it happens, before the run's cleanup;
    /// the run ends with [`Outcome::Failed`](crate::run::Outcome::Failed).
    Failure(String),
    /// How a pull is going.
    Progress(Progress),
}

/// A warning: its kind, and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// What it's about.
    pub kind: WarningKind,
    /// What it says, for people.
    pub message: String,
}

/// What a [`Warning`] is about (the stable part: the message may change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WarningKind {
    /// Something couldn't be cleaned up (a run's writable layer, what a
    /// removal moved aside, a gone container's ports, veth or connections):
    /// a later run's sweep, or a prune, will.
    LeftForLater,
    /// The store's copy of an image couldn't be read: it's pulled again.
    StoredImageUnreadable,
    /// An image's files all belong to the caller, not to its users.
    OwnersCollapsed,
    /// Extracting a layer left something out (a file it couldn't make as
    /// the image says).
    UnpackSkipped,
    /// The store's index was corrupt, and a new one was started.
    IndexReset,
    /// Containers on a network may send as one another (no bridge
    /// filtering, or a guard that couldn't be set).
    AntiSpoofing,
    /// Another program's firewall would drop the containers' traffic; the
    /// message says the fix in its own terms.
    OtherFirewall,
    /// `--publish-all`: one of the image's exposed ports can't be read.
    ExposedPortUnreadable,
    /// A container with a network of its own sees the host's resolvers, on
    /// the host's loopback: its lookups will fail.
    HostResolvers,
    /// The host's root can't have the default bridge network here (no
    /// `CAP_NET_ADMIN`, a read-only `/proc/sys`): the container gets the
    /// host's network.
    HostNetworkFallback,
    /// The record of the host's routing Shipyard closed can't be read: the
    /// firewall closes routing for the forwarding that's on now instead
    /// (F65); the message names the file and when it's safe to delete.
    RoutingRecordUnreadable,
    /// A bundle asks for what `shipyard run` doesn't do (its
    /// `cgroupsPath`, settings it ignores).
    BundleIgnored,
    /// A rootless caller's user namespace maps fewer IDs than it could.
    ShortIdMaps,
    /// An image's `Env` entry without a value, left out.
    ImageEnvIgnored,
    /// The container is root on the host's files (no root filesystem and
    /// no user namespace, or maps of the host's ID 0).
    RootOnHostFiles,
    /// A change to the container store was made, and couldn't be synced
    /// to the disk (a file system that doesn't sync directories): it may
    /// not survive a power cut.
    NotSynced,
    /// A record that can't be read (damaged, or a newer Shipyard's): a
    /// container's, left out of what's listed; a volume's making time,
    /// its directory's said instead.
    RecordUnreadable,
    /// What the runtime said making a container (its cgroup mount left
    /// out, say).
    Runtime,
    /// `start -a` (or a foreground run) couldn't be let in before the
    /// container's start: what it wrote first may be missing (in its logs:
    /// a foreground run then just waits for its end).
    AttachedLate,
    /// A container that ended while nothing recorded its end (its monitor
    /// killed, a reboot, a logout) was settled: its end recorded, or
    /// (`--rm`) it removed; or a start that died was cleaned up.
    Recovered,
    /// A foreground run's client fell too far behind the container's
    /// output (stopped, or a slow reader) and was let go of: the rest of
    /// the output is in its logs.
    FellBehind,
    /// `--health-*` flags that tune a health check there isn't (no
    /// `--health-cmd`, and an image without one of its own): ignored.
    HealthCheckIgnored,
    /// A container lost its network while it ran (its pasta died): it ran
    /// on with only its loopback.
    NetworkLost,
    /// A container others share the network or IPC namespace of stopped or
    /// went: they lose them until they're started again (D11.4b2-4).
    SharedNamespacesLost,
    /// An export of a running container whose root couldn't be cloned
    /// alone: what its volumes cover was exported empty (step 12.2b).
    ExportPartial,
    /// An import's tarball looks like an image archive (`load`'s), not a
    /// root filesystem.
    ImportedArchive,
}

/// A step of a pull or a push, or a line of a build's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// A line of BuildKit's progress (step 12.3): as BuildKit wrote it
    /// (escape it before a terminal).
    Build {
        /// The line, without its newline.
        line: String,
    },
    /// Pulling this image starts.
    Pulling {
        /// The image, as its canonical reference.
        image: String,
    },
    /// One of its layers is in the store.
    Layer {
        /// The layer's digest, its first 12 hex digits.
        short_digest: String,
        /// How many bytes were downloaded for it, if it was (`None`: the
        /// store had it already).
        downloaded: Option<u64>,
    },
    /// Pushing this image to this name starts (step 12.4).
    Pushing {
        /// The name, as its canonical reference.
        name: String,
    },
    /// One of its blobs is on the registry.
    Pushed {
        /// The blob's digest, its first 12 hex digits.
        short_digest: String,
        /// How many bytes were sent, if it was (`None`: the registry had
        /// it already).
        sent: Option<u64>,
    },
}

/// Reports a warning of `kind` to `reporter`.
pub(crate) fn warn(reporter: &dyn Reporter, kind: WarningKind, message: impl Into<String>) {
    reporter.report(Message::Warning(Warning {
        kind,
        message: message.into(),
    }));
}

/// A [`Reporter`] that keeps every message, in order: for a caller that
/// wants them as data (and for tests).
#[derive(Debug, Default)]
pub struct Collect {
    messages: Mutex<Vec<Message>>,
}

impl Collect {
    /// The messages reported so far.
    #[must_use]
    pub fn events(&self) -> Vec<Message> {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The warnings reported so far.
    #[must_use]
    pub fn warnings(&self) -> Vec<Warning> {
        self.events()
            .into_iter()
            .filter_map(|message| match message {
                Message::Warning(warning) => Some(warning),
                _ => None,
            })
            .collect()
    }
}

impl Reporter for Collect {
    fn report(&self, message: Message) {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message);
    }
}
