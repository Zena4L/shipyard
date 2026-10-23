//! A request's method and path, as the endpoint it names: the version
//! prefix (`/v1.43/…`) checked and taken off first (D11.2-3).

use hyper::Method;
use percent_encoding::percent_decode_str;

use crate::errors::ApiError;

/// The Engine API versions served and taken (D11.2-3): the engine's.
pub use shipyard_engine::docker::{API_VERSION, MIN_API_VERSION};

const NEWEST: (u32, u32) = (1, 43);
const OLDEST: (u32, u32) = (1, 24);

/// An endpoint, with the object (container, exec, image) its path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// `GET|HEAD /_ping`.
    Ping,
    /// `POST /{containers,images,volumes,networks,build}/prune`.
    Prune(Prune),
    /// `GET /version`.
    Version,
    /// `GET /info`.
    Info,
    /// `GET /containers/json`.
    ListContainers,
    /// `POST /containers/create`.
    CreateContainer,
    /// `GET /containers/{id}/json`.
    Inspect(String),
    /// `GET /containers/{id}/logs`.
    Logs(String),
    /// `GET /containers/{id}/top`.
    Top(String),
    /// `GET /containers/{id}/stats`.
    Stats(String),
    /// `POST /containers/{id}/attach`.
    Attach(String),
    /// `POST /containers/{id}/resize`.
    Resize(String),
    /// `POST /containers/{id}/exec`.
    ExecCreate(String),
    /// `POST /exec/{id}/start`.
    ExecStart(String),
    /// `GET /exec/{id}/json`.
    ExecInspect(String),
    /// `POST /exec/{id}/resize`.
    ExecResize(String),
    /// `GET /events`.
    Events,
    /// `POST /containers/{id}/start`.
    Start(String),
    /// `POST /containers/{id}/stop`.
    Stop(String),
    /// `POST /containers/{id}/restart`.
    Restart(String),
    /// `POST /containers/{id}/kill`.
    Kill(String),
    /// `POST /containers/{id}/pause`.
    Pause(String),
    /// `POST /containers/{id}/unpause`.
    Unpause(String),
    /// `POST /containers/{id}/rename`.
    Rename(String),
    /// `POST /containers/{id}/update`.
    Update(String),
    /// `POST /containers/{id}/wait`.
    Wait(String),
    /// `DELETE /containers/{id}`.
    Remove(String),
    /// `POST /images/create` (a pull).
    PullImage,
    /// `POST /auth` (`docker login`).
    Auth,
    /// `GET /images/json`.
    ListImages,
    /// `GET /images/{name}/json` (a name may hold `/`s).
    InspectImage(String),
    /// `POST /images/{name}/tag`.
    TagImage(String),
    /// `DELETE /images/{name}`.
    RemoveImage(String),
    /// `GET /volumes`.
    ListVolumes,
    /// `POST /volumes/create`.
    CreateVolume,
    /// `GET /volumes/{name}`.
    InspectVolume(String),
    /// `DELETE /volumes/{name}`.
    RemoveVolume(String),
    /// `GET /containers/{id}/archive`.
    ArchiveGet(String),
    /// `HEAD /containers/{id}/archive`.
    ArchiveHead(String),
    /// `PUT /containers/{id}/archive`.
    ArchivePut(String),
    /// `GET /shipyard/containers/{id}`: Shipyard's own view.
    OwnContainer(String),
    /// `GET /shipyard/networks/{name}`: Shipyard's own view.
    OwnNetwork(String),
    /// `GET /networks`.
    ListNetworks,
    /// `POST /networks/create`.
    CreateNetwork,
    /// `GET /networks/{id}`.
    InspectNetwork(String),
    /// `DELETE /networks/{id}`.
    RemoveNetwork(String),
    /// `POST /networks/{id}/connect`.
    ConnectNetwork(String),
    /// `POST /networks/{id}/disconnect`.
    DisconnectNetwork(String),
}

/// What a prune removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prune {
    /// Stopped containers.
    Containers,
    /// Unused images.
    Images,
    /// Unused volumes.
    Volumes,
    /// Unused networks.
    Networks,
    /// The build cache (none yet).
    Build,
}

impl Route {
    /// Whether its request's body is read: else it's let go at once, so
    /// that the connection's end (the client gone) is seen while the answer
    /// is awaited (a `wait`'s).
    pub(crate) fn takes_body(&self) -> bool {
        matches!(
            self,
            Self::CreateContainer
                | Self::ExecCreate(_)
                | Self::ExecStart(_)
                | Self::CreateVolume
                | Self::CreateNetwork
                | Self::ArchivePut(_)
                | Self::Update(_)
                | Self::ConnectNetwork(_)
                | Self::DisconnectNetwork(_)
                | Self::Auth
        )
    }
}

/// The endpoint `method` and `path` name.
///
/// # Errors
///
/// A version prefix newer than [`API_VERSION`] or older than
/// [`MIN_API_VERSION`] (400, Docker's words); a path that names no endpoint,
/// or one that's served for another method (404, Docker's `page not
/// found`); a name part that isn't UTF-8 once decoded (400).
pub(crate) fn route(method: &Method, path: &str) -> Result<Route, ApiError> {
    let path = unversioned(path)?;
    let parts: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    let decoded = |text: &str| {
        percent_decode_str(text)
            .decode_utf8()
            .map(std::borrow::Cow::into_owned)
            .map_err(|_| ApiError::bad(format!("the name {text:?} isn't UTF-8")))
    };
    // An image's name, its `/`s kept (Docker's clients don't escape them).
    let image = |parts: &[&str]| -> Result<String, ApiError> {
        Ok(parts
            .iter()
            .map(|part| decoded(part))
            .collect::<Result<Vec<_>, _>>()?
            .join("/"))
    };
    let route = match (method, parts.as_slice()) {
        (&Method::GET | &Method::HEAD, ["_ping"]) => Route::Ping,
        (&Method::GET, ["version"]) => Route::Version,
        (&Method::GET, ["info"]) => Route::Info,
        (&Method::GET, ["events"]) => Route::Events,
        (&Method::GET, ["containers", "json"]) => Route::ListContainers,
        (&Method::POST, ["containers", "create"]) => Route::CreateContainer,
        (&Method::GET, ["containers", id, "json"]) => Route::Inspect(decoded(id)?),
        (&Method::GET, ["containers", id, "logs"]) => Route::Logs(decoded(id)?),
        (&Method::GET, ["containers", id, "top"]) => Route::Top(decoded(id)?),
        (&Method::GET, ["containers", id, "stats"]) => Route::Stats(decoded(id)?),
        (&Method::GET, ["containers", id, "archive"]) => Route::ArchiveGet(decoded(id)?),
        (&Method::HEAD, ["containers", id, "archive"]) => Route::ArchiveHead(decoded(id)?),
        (&Method::PUT, ["containers", id, "archive"]) => Route::ArchivePut(decoded(id)?),
        (&Method::POST, ["containers", id, action]) => {
            let id = decoded(id)?;
            match *action {
                "start" => Route::Start(id),
                "stop" => Route::Stop(id),
                "restart" => Route::Restart(id),
                "kill" => Route::Kill(id),
                "pause" => Route::Pause(id),
                "unpause" => Route::Unpause(id),
                "rename" => Route::Rename(id),
                "update" => Route::Update(id),
                "wait" => Route::Wait(id),
                "attach" => Route::Attach(id),
                "resize" => Route::Resize(id),
                "exec" => Route::ExecCreate(id),
                _ => return Err(not_found()),
            }
        }
        (&Method::DELETE, ["containers", id]) => Route::Remove(decoded(id)?),
        (&Method::POST, ["exec", id, "start"]) => Route::ExecStart(decoded(id)?),
        (&Method::POST, ["exec", id, "resize"]) => Route::ExecResize(decoded(id)?),
        (&Method::GET, ["exec", id, "json"]) => Route::ExecInspect(decoded(id)?),
        (&Method::POST, ["containers", "prune"]) => Route::Prune(Prune::Containers),
        (&Method::POST, ["images", "prune"]) => Route::Prune(Prune::Images),
        (&Method::POST, ["build", "prune"]) => Route::Prune(Prune::Build),
        (&Method::POST, ["images", "create"]) => Route::PullImage,
        (&Method::POST, ["auth"]) => Route::Auth,
        (&Method::GET, ["images", "json"]) => Route::ListImages,
        (&Method::GET, ["images", name @ .., "json"]) if !name.is_empty() => {
            Route::InspectImage(image(name)?)
        }
        (&Method::POST, ["images", name @ .., "tag"]) if !name.is_empty() => {
            Route::TagImage(image(name)?)
        }
        (&Method::DELETE, ["images", name @ ..]) if !name.is_empty() => {
            Route::RemoveImage(image(name)?)
        }
        (&Method::GET, ["volumes"]) => Route::ListVolumes,
        (&Method::POST, ["volumes", "create"]) => Route::CreateVolume,
        (&Method::POST, ["volumes", "prune"]) => Route::Prune(Prune::Volumes),
        (&Method::GET, ["volumes", name]) => Route::InspectVolume(decoded(name)?),
        (&Method::DELETE, ["volumes", name]) => Route::RemoveVolume(decoded(name)?),
        (&Method::GET, ["shipyard", "containers", id]) => Route::OwnContainer(decoded(id)?),
        (&Method::GET, ["shipyard", "networks", name]) => Route::OwnNetwork(decoded(name)?),
        (&Method::GET, ["networks"]) => Route::ListNetworks,
        (&Method::POST, ["networks", "create"]) => Route::CreateNetwork,
        (&Method::POST, ["networks", "prune"]) => Route::Prune(Prune::Networks),
        (&Method::GET, ["networks", id]) => Route::InspectNetwork(decoded(id)?),
        (&Method::DELETE, ["networks", id]) => Route::RemoveNetwork(decoded(id)?),
        (&Method::POST, ["networks", id, "connect"]) => Route::ConnectNetwork(decoded(id)?),
        (&Method::POST, ["networks", id, "disconnect"]) => Route::DisconnectNetwork(decoded(id)?),
        _ => return Err(not_found()),
    };
    Ok(route)
}

/// Docker's answer for a path it doesn't serve.
fn not_found() -> ApiError {
    ApiError::new(hyper::StatusCode::NOT_FOUND, "page not found")
}

/// `path` without its version prefix (`/v1.43`), if it has one, the
/// version checked: Docker's, any from [`MIN_API_VERSION`] to
/// [`API_VERSION`].
fn unversioned(path: &str) -> Result<&str, ApiError> {
    let Some(rest) = path.strip_prefix("/v") else {
        return Ok(path);
    };
    let (version, after) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let Some(asked) = parse_version(version) else {
        // Not a version (`/volumes`): a path of its own.
        return Ok(path);
    };
    if asked > NEWEST {
        return Err(ApiError::bad(format!(
            "client version {version} is too new. Maximum supported API version is {API_VERSION}"
        )));
    }
    if asked < OLDEST {
        return Err(ApiError::bad(format!(
            "client version {version} is too old. Minimum supported API version is \
             {MIN_API_VERSION}, please upgrade your client to a newer version"
        )));
    }
    Ok(after)
}

/// `MAJOR.MINOR`, both numbers.
/// The API version `path`'s prefix asks for (`/v1.41/…`: 1.41), if it has
/// one; the newest served if not (as Docker's daemon takes a path without
/// one).
pub(crate) fn asked_version(path: &str) -> (u32, u32) {
    path.strip_prefix("/v")
        .and_then(|rest| parse_version(&rest[..rest.find('/').unwrap_or(rest.len())]))
        .unwrap_or(NEWEST)
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.split_once('.')?;
    let number = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    Some((number(major)?, number(minor)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prefix up to 1.43 and down to 1.24 is taken off; none is fine.
    #[test]
    fn the_version_prefix_is_checked_and_taken_off() {
        for path in [
            "/containers/json",
            "/v1.43/containers/json",
            "/v1.24/containers/json",
        ] {
            assert_eq!(
                route(&Method::GET, path),
                Ok(Route::ListContainers),
                "{path}"
            );
        }
        let too_new = route(&Method::GET, "/v1.44/containers/json").unwrap_err();
        assert_eq!(too_new.status, hyper::StatusCode::BAD_REQUEST);
        assert_eq!(
            too_new.message,
            "client version 1.44 is too new. Maximum supported API version is 1.43"
        );
        assert_eq!(
            route(&Method::GET, "/v2.0/info").unwrap_err().status,
            hyper::StatusCode::BAD_REQUEST
        );
        let too_old = route(&Method::GET, "/v1.23/info").unwrap_err();
        assert!(
            too_old
                .message
                .starts_with("client version 1.23 is too old")
        );
        // Not a version: a path, and not one served.
        for path in ["/plugins", "/v1/info", "/v1.x/info", "/v.1/info", "/v1.43"] {
            assert_eq!(
                route(&Method::GET, path).unwrap_err().status,
                hyper::StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    /// The version a path asks for: its prefix's, else the newest.
    #[test]
    fn the_version_asked_is_the_prefix_s() {
        assert_eq!(asked_version("/v1.41/volumes/prune"), (1, 41));
        assert_eq!(asked_version("/volumes/prune"), (1, 43));
        assert_eq!(asked_version("/v1.43"), (1, 43));
        assert_eq!(asked_version("/volumes"), (1, 43), "a path, not a version");
    }

    /// Each kind's prune, by its path.
    #[test]
    fn prunes_are_found() {
        for (path, kind) in [
            ("/containers/prune", Prune::Containers),
            ("/images/prune", Prune::Images),
            ("/volumes/prune", Prune::Volumes),
            ("/networks/prune", Prune::Networks),
            ("/v1.43/build/prune", Prune::Build),
        ] {
            assert_eq!(route(&Method::POST, path), Ok(Route::Prune(kind)), "{path}");
        }
        assert!(
            route(&Method::GET, "/containers/prune").is_err(),
            "a POST's"
        );
    }

    /// The image, volume and archive endpoints: an image's name keeps its `/`s.
    #[test]
    fn image_volume_and_archive_endpoints_are_found() {
        let cases = [
            (Method::POST, "/images/create", Route::PullImage),
            (Method::POST, "/v1.43/auth", Route::Auth),
            (Method::GET, "/volumes", Route::ListVolumes),
            (
                Method::GET,
                "/containers/web/archive",
                Route::ArchiveGet("web".to_owned()),
            ),
            (
                Method::HEAD,
                "/containers/web/archive",
                Route::ArchiveHead("web".to_owned()),
            ),
            (
                Method::PUT,
                "/containers/web/archive",
                Route::ArchivePut("web".to_owned()),
            ),
            (
                Method::GET,
                "/shipyard/containers/web",
                Route::OwnContainer("web".to_owned()),
            ),
            (
                Method::GET,
                "/v1.43/shipyard/networks/app",
                Route::OwnNetwork("app".to_owned()),
            ),
            (Method::GET, "/networks", Route::ListNetworks),
            (Method::POST, "/networks/create", Route::CreateNetwork),
            (
                Method::GET,
                "/networks/app",
                Route::InspectNetwork("app".to_owned()),
            ),
            (
                Method::DELETE,
                "/networks/app",
                Route::RemoveNetwork("app".to_owned()),
            ),
            (
                Method::POST,
                "/networks/app/connect",
                Route::ConnectNetwork("app".to_owned()),
            ),
            (
                Method::POST,
                "/networks/app/disconnect",
                Route::DisconnectNetwork("app".to_owned()),
            ),
            (Method::GET, "/v1.43/volumes", Route::ListVolumes),
            (Method::POST, "/volumes/create", Route::CreateVolume),
            (
                Method::GET,
                "/volumes/data",
                Route::InspectVolume("data".to_owned()),
            ),
            (
                Method::DELETE,
                "/volumes/data",
                Route::RemoveVolume("data".to_owned()),
            ),
            (Method::GET, "/images/json", Route::ListImages),
            (
                Method::GET,
                "/images/alpine:3.20/json",
                Route::InspectImage("alpine:3.20".to_owned()),
            ),
            (
                Method::GET,
                "/v1.43/images/ghcr.io/owner/app:v1/json",
                Route::InspectImage("ghcr.io/owner/app:v1".to_owned()),
            ),
            (
                Method::GET,
                "/images/owner/json/json",
                Route::InspectImage("owner/json".to_owned()),
            ),
            (
                Method::POST,
                "/images/localhost:5000/app/tag",
                Route::TagImage("localhost:5000/app".to_owned()),
            ),
            (
                Method::DELETE,
                "/images/ghcr.io/owner/app%3Av1",
                Route::RemoveImage("ghcr.io/owner/app:v1".to_owned()),
            ),
        ];
        for (method, path, expected) in cases {
            assert_eq!(route(&method, path), Ok(expected), "{method} {path}");
        }
    }

    /// Each endpoint by its method and path; the container part decoded.
    #[test]
    fn endpoints_are_found_by_method_and_path() {
        let web = || "web".to_owned();
        let cases = [
            (Method::GET, "/_ping", Route::Ping),
            (Method::HEAD, "/_ping", Route::Ping),
            (Method::GET, "/version", Route::Version),
            (Method::GET, "/info", Route::Info),
            (Method::GET, "/events", Route::Events),
            (Method::GET, "/containers/web/logs", Route::Logs(web())),
            (Method::GET, "/containers/web/top", Route::Top(web())),
            (Method::GET, "/containers/web/stats", Route::Stats(web())),
            (Method::POST, "/containers/create", Route::CreateContainer),
            (Method::GET, "/containers/web/json", Route::Inspect(web())),
            (Method::POST, "/containers/web/start", Route::Start(web())),
            (Method::POST, "/containers/web/stop", Route::Stop(web())),
            (
                Method::POST,
                "/containers/web/restart",
                Route::Restart(web()),
            ),
            (Method::POST, "/containers/web/kill", Route::Kill(web())),
            (Method::POST, "/containers/web/pause", Route::Pause(web())),
            (
                Method::POST,
                "/containers/web/unpause",
                Route::Unpause(web()),
            ),
            (Method::POST, "/containers/web/update", Route::Update(web())),
            (Method::POST, "/containers/web/rename", Route::Rename(web())),
            (Method::POST, "/containers/web/wait", Route::Wait(web())),
            (Method::POST, "/containers/web/attach", Route::Attach(web())),
            (Method::POST, "/containers/web/resize", Route::Resize(web())),
            (
                Method::POST,
                "/containers/web/exec",
                Route::ExecCreate(web()),
            ),
            (Method::POST, "/exec/web/start", Route::ExecStart(web())),
            (Method::POST, "/exec/web/resize", Route::ExecResize(web())),
            (Method::GET, "/exec/web/json", Route::ExecInspect(web())),
            (Method::DELETE, "/containers/web", Route::Remove(web())),
            (Method::DELETE, "/containers/w%65b", Route::Remove(web())),
        ];
        for (method, path, expected) in cases {
            assert_eq!(route(&method, path), Ok(expected), "{method} {path}");
        }
        for (method, path) in [
            (Method::POST, "/_ping"),
            (Method::POST, "/containers/json"),
            (Method::GET, "/containers/web/start"),
            (Method::GET, "/containers/web/pause"),
            (Method::GET, "/containers/web/json/extra"),
            (Method::GET, "/"),
            (Method::DELETE, "/images"),
            (Method::GET, "/images/alpine/history"),
            (Method::POST, "/images/tag"),
            (Method::GET, "/shipyard/doctor"),
            (Method::GET, "/shipyard/containers"),
        ] {
            let refused = route(&method, path).unwrap_err();
            assert_eq!(
                refused.status,
                hyper::StatusCode::NOT_FOUND,
                "{method} {path}"
            );
            assert_eq!(refused.message, "page not found");
        }
        assert_eq!(
            route(&Method::DELETE, "/containers/%ff")
                .unwrap_err()
                .status,
            hyper::StatusCode::BAD_REQUEST
        );
    }
}
