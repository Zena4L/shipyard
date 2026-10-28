//! What can go wrong for the app's client.

use std::path::PathBuf;

/// The client's result.
pub type Result<T, E = ClientError> = std::result::Result<T, E>;

/// Why the client couldn't do what it was asked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Nothing answers at the API's place (the service isn't running, or
    /// the Mac's VM is stopped).
    #[error("Shipyard's API doesn't answer at {path}: {source}")]
    Unreachable {
        /// The socket's path.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A format of Shipyard's own is newer than this app reads.
    #[error("{what} is version {found}, newer than this app reads ({known}): update the app")]
    Newer {
        /// What it is.
        what: &'static str,
        /// The version found.
        found: u32,
        /// The newest this reads.
        known: u32,
    },
    /// Shipyard is older than this app: an endpoint the app reads isn't
    /// there (a VM's guest started before an update, a service still
    /// running from an older build).
    #[error("{0}")]
    Older(String),
    /// The API answered with an error.
    #[error("the API refused {path} ({status}): {message}")]
    Api {
        /// The request's path.
        path: String,
        /// Its HTTP status.
        status: u16,
        /// Its message.
        message: String,
    },
    /// Docker's endpoints, through `bollard`.
    #[error("Docker's API: {0}")]
    Docker(#[from] bollard::errors::Error),
    /// An answer that isn't what it says.
    #[error("an answer that can't be read: {0}")]
    Unreadable(#[from] serde_json::Error),
    /// One of Shipyard's own answers carries no version.
    #[error("{0} has no version")]
    NoVersion(&'static str),
    /// HTTP itself failed on the API's socket (Shipyard's own endpoints).
    #[error("the API's connection: {0}")]
    Http(String),
    /// What was given as a container's ID or name, or an image's reference,
    /// isn't one in shape (no request sent).
    #[error("not a container's ID or name, or an image's reference: {0}")]
    BadReference(String),
    /// A run's spec asks for what the sheet doesn't offer (no request sent).
    #[error("{0}")]
    BadSpec(String),
    /// A stream ended: the events' (the service or the VM stopped), or a
    /// container's stats (it stopped).
    #[error("the stream ended")]
    StreamEnded,
    /// It took longer than its bound (the API may still finish it).
    #[error("{what} took longer than {seconds} s: it may still be under way")]
    Timeout {
        /// What did.
        what: String,
        /// Its bound.
        seconds: u64,
    },
    /// The VM couldn't be started (said as `shipyard` said it).
    #[error("the VM didn't start: {0}")]
    VmStart(String),
    /// Linux's API service couldn't be started (said as `shipyard` said
    /// it): what answers is still nothing.
    #[error("Shipyard's service didn't start: {0}")]
    ServiceStart(String),
    /// The `shipyard` binary found can't run the VM (unsigned: it lacks the
    /// virtualization entitlement), or none was found.
    #[error("{0}")]
    Shipyard(String),
    /// The app's settings can't be read or written.
    #[error("the app's settings: {0}")]
    Settings(String),
    /// Where Shipyard is can't be told (the VM's places, no runtime
    /// directory for Linux's socket).
    #[error("where Shipyard is: {0}")]
    Place(String),
    /// Off the platforms the app runs on (Linux, the Mac).
    #[error("{0}")]
    Unsupported(&'static str),
    /// Anything else of the system's.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What kind of failure, for the app's window to say it (serialized in
/// kebab-case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// Nothing answers: the service or the VM isn't running.
    Unreachable,
    /// Shipyard is newer than the app.
    Newer,
    /// Shipyard is older than the app.
    Older,
    /// The API refused, or answered what can't be read.
    Api,
    /// Past a bound.
    Timeout,
    /// The VM didn't start.
    VmStart,
    /// The `shipyard` program: none, or unsigned.
    Shipyard,
    /// The app's settings, or where Shipyard is.
    Settings,
    /// A stream ended (the events', a container's stats).
    Ended,
    /// Anything else.
    Other,
}

impl ClientError {
    /// Its kind.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Unreachable { .. } | Self::ServiceStart(_) => ErrorKind::Unreachable,
            Self::Newer { .. } => ErrorKind::Newer,
            Self::Older(_) => ErrorKind::Older,
            Self::Api { .. } | Self::Docker(_) | Self::Unreadable(_) | Self::NoVersion(_) => {
                ErrorKind::Api
            }
            Self::Timeout { .. } => ErrorKind::Timeout,
            Self::VmStart(_) => ErrorKind::VmStart,
            Self::Shipyard(_) => ErrorKind::Shipyard,
            Self::Settings(_) | Self::Place(_) => ErrorKind::Settings,
            Self::StreamEnded => ErrorKind::Ended,
            Self::Http(_)
            | Self::BadReference(_)
            | Self::BadSpec(_)
            | Self::Unsupported(_)
            | Self::Io(_) => ErrorKind::Other,
        }
    }
}
