//! A refused or failed request, as Docker answers one: a status and a JSON
//! body, `{"message": "…"}` (moby's `errdefs` statuses).

use hyper::StatusCode;
use shipyard_engine::ErrorKind;

/// A request's failure: its status and what's said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    /// The status it's answered with.
    pub(crate) status: StatusCode,
    /// The message, one line.
    pub(crate) message: String,
}

impl ApiError {
    /// A failure with `status`.
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// The request is wrong (400).
    pub(crate) fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// The service itself failed (500).
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

/// An engine error answered with its kind's status, its message with its
/// causes (`{:#}`), as the CLI prints it.
impl From<shipyard_engine::Error> for ApiError {
    fn from(error: shipyard_engine::Error) -> Self {
        Self::new(status_of(error.kind()), format!("{error:#}"))
    }
}

/// The status Docker answers for an error of `kind`: moby's for the same
/// failure (`errdefs`: invalid parameter 400, not found 404, conflict 409,
/// forbidden 403, not implemented 501, the rest a system error, 500).
pub(crate) fn status_of(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        // A container that isn't running is moby's conflict too (`kill`
        // of a stopped one: "is not running", 409).
        ErrorKind::Exists | ErrorKind::InUse | ErrorKind::Busy | ErrorKind::NotRunning => {
            StatusCode::CONFLICT
        }
        ErrorKind::NeedsRoot => StatusCode::FORBIDDEN,
        ErrorKind::Unsupported => StatusCode::NOT_IMPLEMENTED,
        // Environment, Network, Image, Storage, Runtime, Io, Interrupted,
        // and any kind added since until it's given its own.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind has Docker's status for the same failure.
    #[test]
    fn each_kind_is_answered_with_docker_s_status() {
        let cases = [
            (ErrorKind::InvalidRequest, 400),
            (ErrorKind::NotFound, 404),
            (ErrorKind::Exists, 409),
            (ErrorKind::InUse, 409),
            (ErrorKind::Busy, 409),
            (ErrorKind::NotRunning, 409),
            (ErrorKind::NeedsRoot, 403),
            (ErrorKind::Unsupported, 501),
            (ErrorKind::Environment, 500),
            (ErrorKind::Network, 500),
            (ErrorKind::Image, 500),
            (ErrorKind::Storage, 500),
            (ErrorKind::Runtime, 500),
            (ErrorKind::Io, 500),
            (ErrorKind::Interrupted, 500),
        ];
        for (kind, status) in cases {
            assert_eq!(status_of(kind).as_u16(), status, "{kind:?}");
        }
    }

    /// The message is the CLI's: the error, then its causes.
    #[test]
    fn an_engine_error_keeps_its_message() {
        let error = shipyard_engine::Error::new(ErrorKind::NotFound, "no such container: web");
        let answered = ApiError::from(error);
        assert_eq!(answered.status, StatusCode::NOT_FOUND);
        assert_eq!(answered.message, "no such container: web");
    }
}
