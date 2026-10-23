//! `/_ping`, `/version` and `/info`: what a client asks first, to find the
//! API's version and the host's.

use std::sync::Arc;

use hyper::StatusCode;
use hyper::header::{CACHE_CONTROL, HeaderValue, PRAGMA};
use shipyard_engine::docker;

use crate::Shared;
use crate::errors::ApiError;
use crate::reply::{self, Body};

type Reply = Result<hyper::Response<Body>, ApiError>;

/// `GET|HEAD /_ping`: `OK`, never cached (the API version is in every
/// response's headers).
pub(crate) fn ping() -> hyper::Response<Body> {
    let mut response =
        reply::with_type(StatusCode::OK, "text/plain; charset=utf-8", b"OK".to_vec());
    let headers = response.headers_mut();
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

/// `GET /version`: the engine's `SystemVersion`.
pub(crate) fn version(shared: &Shared) -> hyper::Response<Body> {
    reply::json(StatusCode::OK, &docker::system_version(&shared.build))
}

/// `GET /info`: the engine's `SystemInfo`.
pub(crate) async fn info(shared: &Arc<Shared>) -> Reply {
    let build = shared.build.clone();
    let (info, _) = shared
        .call(move |engine| docker::system_info(engine, &build))
        .await?;
    Ok(reply::json(StatusCode::OK, &info))
}
