//! The engine's errors (H6: tests match the kind, not the wording).
//!
//! Most of the engine's operations fail with [`Error`]: a [`kind`](Error::kind)
//! a caller can act on (a missing image, a busy store, a host that lacks
//! something), and a message for people, with the errors that led to it
//! kept as its sources. Printed with `{:#}`, it reads "doing X: because Y",
//! the same chain a CLI prints for any error. The messages may be reworded
//! freely; the kinds are the stable part.
//!
//! A few operations keep an enum of their own, where each case is something
//! a caller handles differently: [`StreamsError`] here, and
//! `attach::AttachError`, `logs::LogsError`, `cp::CpError` in their modules.

use std::error::Error as StdError;
use std::fmt;

/// A result whose error is the engine's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What kind of failure an [`Error`] is: what a caller can tell apart
/// without reading the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The request itself is wrong: a bad name, a reference that can't be
    /// one, options that can't go together.
    InvalidRequest,
    /// What it names doesn't exist (an image, a volume, a network).
    NotFound,
    /// What it would make exists already.
    Exists,
    /// It's in use, so it's left alone (a volume a running container
    /// mounts, an image a container runs).
    InUse,
    /// Another process holds what it needs for longer than it waits: try
    /// again.
    Busy,
    /// It needs a running container, and this one isn't, or ended as it
    /// was asked (`kill`, `exec`, `attach`, `cp`).
    NotRunning,
    /// It needs root.
    NeedsRoot,
    /// Not on this platform, or not with this kernel.
    Unsupported,
    /// The caller's environment or the host lacks something it needs (no
    /// place to keep images, a missing program).
    Environment,
    /// Setting up or using a network failed.
    Network,
    /// Finding, pulling or extracting an image failed.
    Image,
    /// The store or a volume couldn't be read or changed.
    Storage,
    /// The runtime failed: making the container, or running its command.
    Runtime,
    /// A system call or file failed, outside any of the above.
    Io,
    /// The caller was told to stop (a Ctrl-C) before it was done: what it
    /// had made was undone.
    Interrupted,
}

/// An engine error: a [kind](ErrorKind), a message, and the errors that led
/// to it.
///
/// Ask it with [`kind`](Self::kind), or [`find`](Self::find) for another
/// crate's error inside it: not with `downcast_ref` or by walking
/// `source()`. Another crate's error is shown as this one (its message is
/// this error's), so a walker of sources starts below it.
pub struct Error {
    kind: ErrorKind,
    repr: Repr,
}

enum Repr {
    /// A message of the engine's own.
    Message(String),
    /// Another crate's error, shown as it is: its message is this one's,
    /// its sources this one's.
    Foreign(Box<dyn StdError + Send + Sync + 'static>),
    /// What was being done when `source` happened.
    Context { message: String, source: Box<Error> },
}

impl Error {
    /// An error of `kind`, saying `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            repr: Repr::Message(message.into()),
        }
    }

    /// Another crate's error `source`, of `kind`, with its own message and
    /// sources.
    pub fn wrap(kind: ErrorKind, source: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            kind,
            repr: Repr::Foreign(Box::new(source)),
        }
    }

    /// What kind of failure this is.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// This error, as the cause of what was being done (`message`): the
    /// same kind.
    #[must_use]
    pub fn context(self, message: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            repr: Repr::Context {
                message: message.to_string(),
                source: Box::new(self),
            },
        }
    }

    /// This error, as `kind` if it's only [`ErrorKind::Io`] (a system call
    /// that failed while doing something of that kind: the storage helper's
    /// pipes are part of storage).
    #[must_use]
    pub(crate) fn io_as(mut self, kind: ErrorKind) -> Self {
        if self.kind == ErrorKind::Io {
            self.kind = kind;
        }
        self
    }

    /// The first error of type `T` in this one's chain, another crate's
    /// included (a `StoreError` that says which network is in use).
    #[must_use]
    pub fn find<T: StdError + 'static>(&self) -> Option<&T> {
        match &self.repr {
            Repr::Message(_) => None,
            Repr::Context { source, .. } => source.find(),
            Repr::Foreign(error) => {
                let mut next: Option<&(dyn StdError + 'static)> = Some(error.as_ref());
                while let Some(error) = next {
                    if let Some(found) = error.downcast_ref::<T>() {
                        return Some(found);
                    }
                    next = error.source();
                }
                None
            }
        }
    }
}

impl fmt::Display for Error {
    /// The message; with `{:#}`, followed by each source's (`: `-separated),
    /// as a CLI prints any error.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            Repr::Message(message) | Repr::Context { message, .. } => f.write_str(message)?,
            Repr::Foreign(error) => write!(f, "{error}")?,
        }
        if f.alternate() {
            let mut next = self.source();
            while let Some(error) = next {
                write!(f, ": {error}")?;
                next = error.source();
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {self:#}", self.kind)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &self.repr {
            Repr::Message(_) => None,
            Repr::Foreign(error) => error.source(),
            Repr::Context { source, .. } => Some(source.as_ref()),
        }
    }
}

/// Adds what was being done to a failure: `.context("opening the store")`.
pub(crate) trait Context<T> {
    /// The error, as the cause of `message`.
    fn context(self, message: impl fmt::Display) -> Result<T>;

    /// The error, as the cause of what `message` says (made only on an
    /// error).
    fn with_context<M: fmt::Display>(self, message: impl FnOnce() -> M) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.map_err(|error| error.into().context(message))
    }

    fn with_context<M: fmt::Display>(self, message: impl FnOnce() -> M) -> Result<T> {
        self.map_err(|error| error.into().context(message()))
    }
}

/// Returns an engine error of a kind from the enclosing function:
/// `bail!(NotFound, "no image {name}")`.
macro_rules! bail {
    ($kind:ident, $($message:tt)+) => {
        return Err($crate::error::Error::new(
            $crate::error::ErrorKind::$kind,
            format!($($message)+),
        ))
    };
}
pub(crate) use bail;

/// The error for what isn't on this platform: "`what` need Linux".
#[cfg(not(target_os = "linux"))]
pub(crate) fn linux_only(what: &str) -> Error {
    Error::new(ErrorKind::Unsupported, format!("{what} need Linux"))
}

// --- Other crates' errors, and their kinds ---------------------------------

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::wrap(ErrorKind::Io, error)
    }
}

impl From<nix::errno::Errno> for Error {
    fn from(error: nix::errno::Errno) -> Self {
        Self::wrap(ErrorKind::Io, error)
    }
}

/// JSON the engine reads is a record or a helper's answer: storage.
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::wrap(ErrorKind::Storage, error)
    }
}

impl From<shipyard_runtime::Error> for Error {
    fn from(error: shipyard_runtime::Error) -> Self {
        let kind = match &error {
            shipyard_runtime::Error::LinuxOnly { .. } => ErrorKind::Unsupported,
            _ => ErrorKind::Runtime,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_runtime::container::ContainerError> for Error {
    fn from(error: shipyard_runtime::container::ContainerError) -> Self {
        use shipyard_runtime::container::{ContainerError, Status};
        let kind = match &error {
            ContainerError::LinuxOnly
            | ContainerError::NoControl(_)
            | ContainerError::MonitorTooOld(_)
            // Made without logs (`--log-dir`): nothing to attach to, or
            // to read. Asked of it, it's the container's, not a failure.
            | ContainerError::NotAttachable(_)
            | ContainerError::NotCaptured(_) => ErrorKind::Unsupported,
            ContainerError::NotFound(_) => ErrorKind::NotFound,
            ContainerError::Busy(_) | ContainerError::MonitorRefused { .. } => ErrorKind::Busy,
            ContainerError::InvalidId(_)
            | ContainerError::CantStop { .. }
            | ContainerError::BadHoldLabel(_) => ErrorKind::InvalidRequest,
            // Ended: what needed it running raced its end. (Running when it
            // had to be stopped, or created, is the runtime's to say.)
            ContainerError::WrongStatus {
                status: Status::Stopped | Status::Stopping,
                ..
            } => ErrorKind::NotRunning,
            ContainerError::NoStateRoot(_) => ErrorKind::Environment,
            _ => ErrorKind::Runtime,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_image::StoreError> for Error {
    fn from(error: shipyard_image::StoreError) -> Self {
        let kind = match &error {
            shipyard_image::StoreError::Busy => ErrorKind::Busy,
            _ => ErrorKind::Storage,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_image::IndexError> for Error {
    fn from(error: shipyard_image::IndexError) -> Self {
        let kind = match &error {
            shipyard_image::IndexError::Busy => ErrorKind::Busy,
            shipyard_image::IndexError::NoSuchImage(_) => ErrorKind::NotFound,
            _ => ErrorKind::Storage,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_image::AreaError> for Error {
    fn from(error: shipyard_image::AreaError) -> Self {
        Self::wrap(ErrorKind::Storage, error)
    }
}

impl From<shipyard_image::ExtractError> for Error {
    fn from(error: shipyard_image::ExtractError) -> Self {
        Self::wrap(ErrorKind::Image, error)
    }
}

impl From<shipyard_image::PullError> for Error {
    fn from(error: shipyard_image::PullError) -> Self {
        use shipyard_image::{IndexError, PullError, RegistryError, StoreError};
        let kind = match &error {
            PullError::Store(StoreError::Busy) | PullError::Index(IndexError::Busy) => {
                ErrorKind::Busy
            }
            PullError::Index(IndexError::NoSuchImage(_))
            | PullError::Registry(RegistryError::Status { status: 404, .. }) => ErrorKind::NotFound,
            _ => ErrorKind::Image,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_image::ReferenceError> for Error {
    fn from(error: shipyard_image::ReferenceError) -> Self {
        Self::wrap(ErrorKind::InvalidRequest, error)
    }
}

impl From<shipyard_storage::VolumeError> for Error {
    fn from(error: shipyard_storage::VolumeError) -> Self {
        let kind = match &error {
            shipyard_storage::VolumeError::InvalidName(_) => ErrorKind::InvalidRequest,
            shipyard_storage::VolumeError::NotFound(_) => ErrorKind::NotFound,
            shipyard_storage::VolumeError::Exists(_) => ErrorKind::Exists,
            shipyard_storage::VolumeError::InUse(_) => ErrorKind::InUse,
            shipyard_storage::VolumeError::Io { .. } => ErrorKind::Storage,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_network::store::StoreError> for Error {
    fn from(error: shipyard_network::store::StoreError) -> Self {
        use shipyard_network::store::StoreError;
        // `Address` is coarse: an address outside the subnet, one taken, or
        // none left all say so in its message; it's the network's answer.
        let kind = match &error {
            StoreError::InvalidName(_) => ErrorKind::InvalidRequest,
            StoreError::NotFound(_) => ErrorKind::NotFound,
            StoreError::Exists(_) | StoreError::NameTaken { .. } => ErrorKind::Exists,
            StoreError::InUse { .. } => ErrorKind::InUse,
            StoreError::Busy(_) => ErrorKind::Busy,
            StoreError::Io { .. } | StoreError::Corrupt { .. } => ErrorKind::Storage,
            _ => ErrorKind::Network,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_network::NetlinkError> for Error {
    fn from(error: shipyard_network::NetlinkError) -> Self {
        Self::wrap(ErrorKind::Network, error)
    }
}

impl From<shipyard_network::ports::PortError> for Error {
    fn from(error: shipyard_network::ports::PortError) -> Self {
        Self::wrap(ErrorKind::Network, error)
    }
}

#[cfg(target_os = "linux")]
impl From<shipyard_network::attach::AttachError> for Error {
    fn from(error: shipyard_network::attach::AttachError) -> Self {
        Self::wrap(ErrorKind::Network, error)
    }
}

#[cfg(target_os = "linux")]
impl From<shipyard_network::route::RtnetlinkError> for Error {
    fn from(error: shipyard_network::route::RtnetlinkError) -> Self {
        Self::wrap(ErrorKind::Network, error)
    }
}

#[cfg(target_os = "linux")]
impl From<shipyard_network::dns::server::ServerError> for Error {
    fn from(error: shipyard_network::dns::server::ServerError) -> Self {
        Self::wrap(ErrorKind::Network, error)
    }
}

/// Errors that are the request's mistake, as another crate tells it.
macro_rules! invalid_request {
    ($($error:ty),+ $(,)?) => {$(
        impl From<$error> for Error {
            fn from(error: $error) -> Self {
                Self::wrap(ErrorKind::InvalidRequest, error)
            }
        }
    )+};
}

invalid_request!(
    shipyard_runtime::isolation::IsolationError,
    shipyard_runtime::process::EnvError,
    shipyard_runtime::process::SpecError,
    shipyard_runtime::process::StopSignalError,
    shipyard_runtime::security::seccomp::SeccompError,
);

impl From<crate::attach::AttachError> for Error {
    fn from(error: crate::attach::AttachError) -> Self {
        let kind = match &error {
            crate::attach::AttachError::Io(_) => ErrorKind::Io,
            // Too many clients: as a monitor's refusal, try again.
            crate::attach::AttachError::Refused(_) => ErrorKind::Busy,
            _ => ErrorKind::Runtime,
        };
        Self::wrap(kind, error)
    }
}

impl From<crate::cp::CpError> for Error {
    fn from(error: crate::cp::CpError) -> Self {
        use crate::cp::CpError;
        let kind = match error {
            // The runtime's own kinds (no such container, the wrong state).
            CpError::Container(error) => return error.into(),
            CpError::Usage(_)
            | CpError::DirectoryOntoFile(_)
            | CpError::KindClash(_)
            | CpError::Special(_)
            | CpError::NotADirectory(_)
            | CpError::ParentMissing(_)
            | CpError::NoSuchDirectory(_) => ErrorKind::InvalidRequest,
            CpError::NotFound(_) => ErrorKind::NotFound,
            CpError::InContainer(_) => ErrorKind::Runtime,
            CpError::Unpack(_) | CpError::Io { .. } => ErrorKind::Io,
            #[cfg(target_os = "linux")]
            CpError::Pack(_) => ErrorKind::Io,
        };
        Self::wrap(kind, error)
    }
}

impl From<crate::logs::LogsError> for Error {
    fn from(error: crate::logs::LogsError) -> Self {
        let kind = match &error {
            crate::logs::LogsError::Sink(_) => ErrorKind::Io,
            _ => ErrorKind::Storage,
        };
        Self::wrap(kind, error)
    }
}

impl From<shipyard_runtime::oci::BundleError> for Error {
    fn from(error: shipyard_runtime::oci::BundleError) -> Self {
        Self::wrap(ErrorKind::Runtime, error)
    }
}

/// pasta that can't be asked what it can do: the host's.
impl From<shipyard_network::pasta::PastaError> for Error {
    fn from(error: shipyard_network::pasta::PastaError) -> Self {
        Self::wrap(ErrorKind::Environment, error)
    }
}

impl From<StreamsError> for Error {
    fn from(error: StreamsError) -> Self {
        Self::wrap(ErrorKind::InvalidRequest, error)
    }
}

/// Why a run's standard input and output can't be set up as asked (see
/// [`streams::plan`](crate::streams::plan)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StreamsError {
    /// `-it` asks to type into a terminal, and stdin isn't one. Docker's
    /// words, which scripts already look for.
    #[error(
        "the input device is not a TTY (-it needs a terminal to type on; use -i alone to pipe input in)"
    )]
    InputNotATerminal,
    /// `-t` with an OCI bundle: the bundle says whether it wants a terminal
    /// (`process.terminal` in its `config.json`).
    #[error("-t: a bundle says whether it wants a terminal, in its config.json (process.terminal)")]
    TerminalFromBundle,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    #[test]
    fn the_chain_prints_as_a_cli_prints_any_error() {
        let error = Error::wrap(ErrorKind::Io, Outer(Inner)).context("doing x");
        assert_eq!(error.to_string(), "doing x");
        assert_eq!(format!("{error:#}"), "doing x: outer: inner");
        // Through `anyhow`, as the CLI holds it: the same line.
        let held = anyhow::Error::from(Error::wrap(ErrorKind::Io, Outer(Inner)).context("doing x"));
        assert_eq!(format!("{held:#}"), "doing x: outer: inner");
        let plain = Error::new(ErrorKind::NotFound, "no such thing").context("finding it");
        assert_eq!(format!("{plain:#}"), "finding it: no such thing");
    }

    #[test]
    fn context_keeps_the_kind_and_finds_the_foreign_error() {
        let error = Error::wrap(ErrorKind::Busy, Outer(Inner))
            .context("a")
            .context("b");
        assert_eq!(error.kind(), ErrorKind::Busy);
        assert!(error.find::<Outer>().is_some());
        assert!(error.find::<Inner>().is_some(), "its sources too");
        assert!(Error::new(ErrorKind::Io, "x").find::<Inner>().is_none());
        // A walker of sources starts below the foreign error (it's shown as
        // this one): `find` is how to ask, as the type's documentation says.
        let held = anyhow::Error::from(error);
        assert!(!held.chain().any(<dyn StdError>::is::<Outer>));
        assert!(held.chain().any(<dyn StdError>::is::<Inner>));
    }

    #[test]
    fn only_a_plain_io_failure_takes_the_kind_of_what_it_was_doing() {
        let io = Error::new(ErrorKind::Io, "pipe").io_as(ErrorKind::Storage);
        assert_eq!(io.kind(), ErrorKind::Storage);
        let busy = Error::new(ErrorKind::Busy, "busy").io_as(ErrorKind::Storage);
        assert_eq!(busy.kind(), ErrorKind::Busy);
    }

    #[test]
    fn other_crates_errors_have_their_kinds() {
        use shipyard_network::store::StoreError;
        use shipyard_runtime::container::{ContainerError, Status};
        let kind = |error: Error| error.kind();
        assert_eq!(
            kind(shipyard_storage::VolumeError::NotFound("v".into()).into()),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind(shipyard_storage::VolumeError::InUse("v".into()).into()),
            ErrorKind::InUse
        );
        assert_eq!(
            kind(shipyard_storage::VolumeError::Exists("v".into()).into()),
            ErrorKind::Exists
        );
        assert_eq!(
            kind(shipyard_image::IndexError::NoSuchImage("i".into()).into()),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind(shipyard_image::IndexError::Busy.into()),
            ErrorKind::Busy
        );
        assert_eq!(
            kind(shipyard_image::StoreError::Busy.into()),
            ErrorKind::Busy
        );
        assert_eq!(
            kind(
                shipyard_image::ImageReference::parse("")
                    .unwrap_err()
                    .into()
            ),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(StoreError::NotFound("n".into()).into()),
            ErrorKind::NotFound
        );
        let in_use = StoreError::InUse {
            name: "n".into(),
            users: vec!["web".into()],
        };
        assert_eq!(kind(in_use.into()), ErrorKind::InUse);
        assert_eq!(kind(StoreError::Busy("n".into()).into()), ErrorKind::Busy);
        assert_eq!(
            kind(StoreError::Full("n".into()).into()),
            ErrorKind::Network
        );
        assert_eq!(
            kind(StoreError::Address("taken".into()).into()),
            ErrorKind::Network
        );
        // A container made without logs: attached to, or its logs read.
        for without_logs in [
            ContainerError::NotAttachable("web".into()),
            ContainerError::NotCaptured("web".into()),
        ] {
            assert_eq!(kind(without_logs.into()), ErrorKind::Unsupported);
        }
        // One kind per situation (stage 9, step 9.9a, D3): ended as it was
        // asked is "not running"; running when it mustn't be stays the
        // runtime's.
        let wrong = |status| ContainerError::WrongStatus {
            id: "web".into(),
            status,
            action: "exec",
            wanted: "created or running",
        };
        for ended in [Status::Stopped, Status::Stopping] {
            assert_eq!(kind(wrong(ended).into()), ErrorKind::NotRunning);
        }
        assert_eq!(kind(wrong(Status::Running).into()), ErrorKind::Runtime);
        assert_eq!(
            kind(ContainerError::MonitorTooOld("web".into()).into()),
            ErrorKind::Unsupported
        );
        assert_eq!(
            kind(ContainerError::BadHoldLabel("x y".into()).into()),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(crate::attach::AttachError::Refused("too many".into()).into()),
            ErrorKind::Busy
        );
    }

    #[test]
    fn bail_returns_an_error_of_its_kind() {
        fn refuse(name: &str) -> Result<()> {
            bail!(NotFound, "no volume {name}");
        }
        let error = refuse("v").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "no volume v");
    }
}
