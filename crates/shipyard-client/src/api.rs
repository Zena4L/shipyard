//! The API, through its socket: Docker's endpoints by `bollard` (a
//! maintained client, D14-6, negotiated down to the API's version), and
//! Shipyard's own `/shipyard/…` by hyper over the same socket, each answer's
//! `version` checked before it's handed on (D14.2-5). Every request is
//! bounded: a guest that's starting answers late, never forever.

use std::path::PathBuf;
use std::time::Duration;

use http_body_util::{BodyExt as _, Empty, Limited};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;

pub use bollard::models::{ContainerSummary, SystemInfo};

use crate::place::Endpoint;
use crate::{ClientError, Result};

/// How long a request may take.
pub const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// How long the engine waits for a container's other operation to end
/// (its `OP_WAIT`: a restart policy's start, a `cp`) before it acts.
const OP_WAIT: Duration = Duration::from_secs(120);

/// How long an action may take: the engine's wait for the container, then
/// the request's own. Past it, the action may still be under way.
pub const ACTION_WAIT: Duration = Duration::from_secs(OP_WAIT.as_secs() + REQUEST_WAIT.as_secs());

/// How long the engine waits for a stopped container's end to settle.
const SETTLE_WAIT: Duration = Duration::from_secs(60);

/// Docker's stop timeout for a container that sets none, seconds.
const DEFAULT_STOP_TIMEOUT: u64 = 10;

/// How long a stop may take for a container whose stop timeout is
/// `timeout` seconds (a nonsense one held to a day).
fn stop_wait_for(timeout: u64) -> Duration {
    ACTION_WAIT + SETTLE_WAIT + Duration::from_secs(timeout.min(86_400))
}

/// The newest links answer this reads (`GET /shipyard/links`).
pub const LINKS_VERSION: u32 = 1;

/// The newest isolation answer this reads.
pub const ISOLATION_VERSION: u32 = 1;

/// The newest volumes answer this reads.
pub const VOLUMES_VERSION: u32 = 1;

/// The newest network view this reads (`/shipyard/networks/{name}`).
pub const NETWORK_VERSION: u32 = 1;

/// The newest usage answer this reads (`/shipyard/usage`).
pub const USAGE_VERSION: u32 = 1;

/// The most of a refusal's message kept.
const MAX_MESSAGE: usize = 512;

/// The most one of Shipyard's own answers is read as.
const MAX_ANSWER: usize = 32 * 1024 * 1024;

/// A connection to the API: `bollard`'s for Docker's endpoints, the
/// socket's path for Shipyard's own.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    docker: bollard::Docker,
}

impl Client {
    /// Connected to the API at `endpoint`, its version negotiated.
    ///
    /// # Errors
    ///
    /// [`ClientError::Unreachable`] if nothing answers; the negotiation's.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let socket = endpoint.path().to_path_buf();
        let unreachable = |source| ClientError::Unreachable {
            path: socket.clone(),
            source,
        };
        tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(unreachable)?;
        let path = socket
            .to_str()
            .ok_or_else(|| unreachable(std::io::Error::other("the socket's path isn't UTF-8")))?;
        let docker = bollard::Docker::connect_with_unix(
            path,
            REQUEST_WAIT.as_secs(),
            bollard::API_DEFAULT_VERSION,
        )?;
        let docker = bounded("the API's version", docker.negotiate_version()).await??;
        Ok(Self { socket, docker })
    }

    /// Docker's endpoints.
    #[must_use]
    pub fn docker(&self) -> &bollard::Docker {
        &self.docker
    }

    /// The API's socket.
    pub(crate) fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Every container, as `docker ps -a` lists them.
    ///
    /// # Errors
    ///
    /// The API's.
    pub async fn containers(&self) -> Result<Vec<ContainerSummary>> {
        let options = bollard::query_parameters::ListContainersOptionsBuilder::default()
            .all(true)
            .build();
        Ok(bounded(
            "listing containers",
            self.docker.list_containers(Some(options)),
        )
        .await??)
    }

    /// What `docker info` says: the host's kernel, memory, architecture,
    /// storage and Shipyard's version.
    ///
    /// # Errors
    ///
    /// The API's.
    pub async fn info(&self) -> Result<SystemInfo> {
        Ok(bounded("the system's info", self.docker.info()).await??)
    }

    /// How many volumes there are (the rail's count; the images' is
    /// `info`'s, the networks' [`Client::networks`]'s).
    ///
    /// # Errors
    ///
    /// The API's.
    pub async fn volume_count(&self) -> Result<usize> {
        Ok(bounded(
            "listing volumes",
            self.docker
                .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>),
        )
        .await??
        .volumes
        .map_or(0, |volumes| volumes.len()))
    }

    /// Every network, as the map's key shows it.
    ///
    /// # Errors
    ///
    /// The API's.
    pub async fn networks(&self) -> Result<Vec<Network>> {
        let listed = bounded(
            "listing networks",
            self.docker
                .list_networks(None::<bollard::query_parameters::ListNetworksOptions>),
        )
        .await??;
        Ok(listed.into_iter().map(Network::from).collect())
    }

    /// Container `id`'s Docker inspect (its state, an out-of-memory kill,
    /// its mounts), as it came.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadReference`] for an ID of another shape; the API's.
    pub async fn inspect(&self, id: &str) -> Result<serde_json::Value> {
        let id = container_ref(id)?;
        let inspected = bounded(
            "inspecting a container",
            self.docker.inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            ),
        )
        .await??;
        Ok(serde_json::to_value(inspected)?)
    }

    /// Image `reference`'s Docker inspect (its layers, its size), as it came.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadReference`] for a reference of another shape; the
    /// API's.
    pub async fn image(&self, reference: &str) -> Result<serde_json::Value> {
        let reference = image_ref(reference)?;
        let inspected =
            bounded("inspecting an image", self.docker.inspect_image(reference)).await??;
        Ok(serde_json::to_value(inspected)?)
    }

    /// Starts the container `id` (an ID or a name), as `shipyard start`.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadReference`] for an ID or name of another shape;
    /// the API's; [`ClientError::Timeout`] past [`ACTION_WAIT`].
    pub async fn start(&self, id: &str) -> Result<()> {
        let id = container_ref(id)?;
        let docker = self.docker.clone().with_timeout(ACTION_WAIT);
        let start =
            docker.start_container(id, None::<bollard::query_parameters::StartContainerOptions>);
        within(ACTION_WAIT, "starting a container", start).await??;
        Ok(())
    }

    /// Stops the container `id`, as `shipyard stop`: its stop signal, then
    /// SIGKILL past its own stop timeout.
    ///
    /// # Errors
    ///
    /// As [`Client::start`]; [`ClientError::Timeout`] past the engine's
    /// wait, the container's own stop timeout and its end's settling.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let id = container_ref(id)?;
        let wait = self.stop_wait(id).await?;
        let docker = self.docker.clone().with_timeout(wait);
        let stop =
            docker.stop_container(id, None::<bollard::query_parameters::StopContainerOptions>);
        within(wait, "stopping a container", stop).await??;
        Ok(())
    }

    /// Restarts the container `id`, as `shipyard restart`.
    ///
    /// # Errors
    ///
    /// As [`Client::stop`].
    pub async fn restart(&self, id: &str) -> Result<()> {
        let id = container_ref(id)?;
        let wait = self.stop_wait(id).await?;
        let docker = self.docker.clone().with_timeout(wait);
        let restart = docker.restart_container(
            id,
            None::<bollard::query_parameters::RestartContainerOptions>,
        );
        within(wait, "restarting a container", restart).await??;
        Ok(())
    }

    /// Pauses the container `id` (its cgroup frozen), as `shipyard pause`.
    ///
    /// # Errors
    ///
    /// As [`Client::start`].
    pub async fn pause(&self, id: &str) -> Result<()> {
        let id = container_ref(id)?;
        let docker = self.docker.clone().with_timeout(ACTION_WAIT);
        within(
            ACTION_WAIT,
            "pausing a container",
            docker.pause_container(id),
        )
        .await??;
        Ok(())
    }

    /// Resumes the paused container `id`, as `shipyard unpause`.
    ///
    /// # Errors
    ///
    /// As [`Client::start`].
    pub async fn unpause(&self, id: &str) -> Result<()> {
        let id = container_ref(id)?;
        let docker = self.docker.clone().with_timeout(ACTION_WAIT);
        within(
            ACTION_WAIT,
            "resuming a container",
            docker.unpause_container(id),
        )
        .await??;
        Ok(())
    }

    /// Removes the container `id`, as `shipyard rm` (`force`: `rm -f`, a
    /// running one stopped first). Its named volumes stay.
    ///
    /// # Errors
    ///
    /// As [`Client::stop`].
    pub async fn remove(&self, id: &str, force: bool) -> Result<()> {
        let id = container_ref(id)?;
        let options = bollard::query_parameters::RemoveContainerOptionsBuilder::default()
            .force(force)
            .build();
        let wait = if force {
            self.stop_wait(id).await?
        } else {
            ACTION_WAIT
        };
        let docker = self.docker.clone().with_timeout(wait);
        let remove = docker.remove_container(id, Some(options));
        within(wait, "removing a container", remove).await??;
        Ok(())
    }

    /// How long a stop of `id` may take: the engine's wait for the
    /// container ([`ACTION_WAIT`]), its own stop timeout (compose's
    /// `stop_grace_period`, Docker's 10 s if none), the wait for its end.
    async fn stop_wait(&self, id: &str) -> Result<Duration> {
        let inspected = bounded(
            "inspecting a container",
            self.docker.inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            ),
        )
        .await??;
        let timeout = inspected
            .config
            .and_then(|config| config.stop_timeout)
            .and_then(|seconds| u64::try_from(seconds).ok())
            .unwrap_or(DEFAULT_STOP_TIMEOUT);
        Ok(stop_wait_for(timeout))
    }

    /// Every container's links, for the map (`/shipyard/links`), as it
    /// came, its version known.
    ///
    /// # Errors
    ///
    /// The API's; [`ClientError::Newer`] for a newer answer.
    pub async fn links(&self) -> Result<serde_json::Value> {
        older_if_missing(
            self.own("/shipyard/links", "the links answer", LINKS_VERSION)
                .await,
        )
    }

    /// A running container's isolation (`/shipyard/containers/{id}/
    /// isolation`: its namespaces, cgroup, command and user mapping), its
    /// version known.
    ///
    /// # Errors
    ///
    /// The API's (409: not running); [`ClientError::Newer`] for a newer
    /// answer; [`ClientError::BadReference`] for an ID of another shape.
    pub async fn isolation(&self, id: &str) -> Result<serde_json::Value> {
        let path = format!("/shipyard/containers/{}/isolation", container_ref(id)?);
        self.own(&path, "the isolation answer", ISOLATION_VERSION)
            .await
    }

    /// Every volume's size and top level (`/shipyard/volumes`), its version
    /// known.
    ///
    /// # Errors
    ///
    /// The API's; [`ClientError::Newer`] for a newer answer.
    pub async fn volumes(&self) -> Result<serde_json::Value> {
        older_if_missing(
            self.own("/shipyard/volumes", "the volumes answer", VOLUMES_VERSION)
                .await,
        )
    }

    /// `GET path`, one of Shipyard's own documents (`what`), at most
    /// version `known`.
    pub(crate) async fn own(
        &self,
        path: &str,
        what: &'static str,
        known: u32,
    ) -> Result<serde_json::Value> {
        let value = self.json(path, what).await?;
        check_version(&value, what, known)?;
        Ok(value)
    }

    /// `GET path`'s JSON (`what`), as it came: Docker's own answers where
    /// `bollard`'s types differ with its features (a time a `String`, or
    /// another type when a crate beside this one asks for `time`).
    ///
    /// # Errors
    ///
    /// [`ClientError::Api`] for a refusal; the request's.
    pub(crate) async fn json(&self, path: &str, what: &'static str) -> Result<serde_json::Value> {
        let (status, body) = bounded(what, get(&self.socket, path)).await??;
        if status != 200 {
            // Docker's `{"message"}`, else the body as it came (a proxy's
            // page, a truncated answer): a refusal either way.
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned())
                .chars()
                .take(MAX_MESSAGE)
                .collect();
            return Err(ClientError::Api {
                path: path.to_owned(),
                status,
                message,
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

/// A 404 from one of Shipyard's own endpoints that names nothing (no ID in
/// its path: it can't be "no such …") said as what it is: the Shipyard that
/// answers is older than this app.
pub(crate) fn older_if_missing(answer: Result<serde_json::Value>) -> Result<serde_json::Value> {
    match answer {
        Err(ClientError::Api {
            path, status: 404, ..
        }) => Err(ClientError::Older(format!(
            "the Shipyard that answers doesn't serve {path}: it's older than this app. \
             Update it; on a Mac, a VM started before the update runs the old one \
             until `shipyard vm stop`"
        ))),
        answer => answer,
    }
}

/// `id` if it's a container's ID or name in shape (letters, digits, `_.-`):
/// it goes into a path as it is, so anything else would be another path,
/// never quietly another container.
///
/// # Errors
///
/// [`ClientError::BadReference`] for anything else.
pub fn container_ref(id: &str) -> Result<&str> {
    name_ref(id)
}

/// `name` if it's a container's, volume's or network's name (or ID) in
/// Docker's shape: a letter or digit, then letters, digits and `_.-` (so
/// never `.` or `..`, which would be another path).
///
/// # Errors
///
/// [`ClientError::BadReference`] for anything else.
pub(crate) fn name_ref(name: &str) -> Result<&str> {
    if name
        .bytes()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        Ok(name)
    } else {
        Err(ClientError::BadReference(format!("{name:?}")))
    }
}

/// `reference` if it's an image's reference in shape (letters, digits and
/// `._-/:@`, no path segment empty, `.` or `..`): as [`container_ref`], it
/// goes into a path as it is.
pub(crate) fn image_ref(reference: &str) -> Result<&str> {
    // No path segment a reference never has: none empty, none `.` or `..`
    // (it goes into a path: `../containers/x` is another one).
    let segments_sound = reference
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if segments_sound
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/:@".contains(&byte))
    {
        Ok(reference)
    } else {
        Err(ClientError::BadReference(format!("an image {reference:?}")))
    }
}

/// A network, as the map's key shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Network {
    /// Its name.
    pub name: String,
    /// Compose's name for it in its project (`com.docker.compose.network`),
    /// else its name.
    pub short: String,
    /// Its first subnet, if it has one.
    pub subnet: Option<String>,
    /// That subnet's gateway, if it says one.
    pub gateway: Option<String>,
    /// Whether nothing on it reaches beyond it.
    pub internal: bool,
    /// Its driver: `bridge`, or a built-in's (`host`, `null`, `pasta`).
    pub driver: String,
}

impl From<bollard::models::Network> for Network {
    fn from(network: bollard::models::Network) -> Self {
        let name = network.name.unwrap_or_default();
        let short = network
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.docker.compose.network"))
            .cloned()
            .unwrap_or_else(|| name.clone());
        let pool = network
            .ipam
            .and_then(|ipam| ipam.config)
            .and_then(|config| config.into_iter().find(|pool| pool.subnet.is_some()));
        Self {
            name,
            short,
            subnet: pool.as_ref().and_then(|pool| pool.subnet.clone()),
            gateway: pool.and_then(|pool| pool.gateway),
            internal: network.internal.unwrap_or(false),
            driver: network.driver.unwrap_or_default(),
        }
    }
}

/// How many of each the rail's other sections hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    /// Images in the store.
    pub images: usize,
    /// Named and anonymous volumes.
    pub volumes: usize,
    /// Bridge networks (the Networks section's; the built-ins aren't).
    pub networks: usize,
}

/// `value` read only if its `version` is at most `known`.
///
/// # Errors
///
/// [`ClientError::Newer`] for a newer one; [`ClientError::NoVersion`]
/// without one.
pub(crate) fn check_version(
    value: &serde_json::Value,
    what: &'static str,
    known: u32,
) -> Result<()> {
    let found = value["version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or(ClientError::NoVersion(what))?;
    if found > known {
        return Err(ClientError::Newer { what, found, known });
    }
    Ok(())
}

/// `future`, within [`REQUEST_WAIT`].
pub(crate) async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> Result<T> {
    within(REQUEST_WAIT, what, future).await
}

/// `future`, within `wait`.
pub(crate) async fn within<T>(
    wait: Duration,
    what: &str,
    future: impl Future<Output = T>,
) -> Result<T> {
    tokio::time::timeout(wait, future)
        .await
        .map_err(|_| ClientError::Timeout {
            what: what.to_owned(),
            seconds: wait.as_secs(),
        })
}

/// HTTP's failure, as the client's.
pub(crate) fn http(error: impl std::fmt::Display) -> ClientError {
    ClientError::Http(error.to_string())
}

/// A response on the API's socket, its connection driven while it's kept
/// (dropped, the connection goes).
#[derive(Debug)]
pub(crate) struct Answer {
    /// Its status.
    pub(crate) status: u16,
    /// Its body, as it comes.
    pub(crate) body: hyper::body::Incoming,
    _driven: Driven,
}

/// The task driving a connection, ended when dropped.
#[derive(Debug)]
struct Driven(tokio::task::JoinHandle<()>);

impl Drop for Driven {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `GET path` on the API's socket, once its headers have come.
pub(crate) async fn request(socket: &std::path::Path, path: &str) -> Result<Answer> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|source| ClientError::Unreachable {
            path: socket.to_path_buf(),
            source,
        })?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(http)?;
    let driven = Driven(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let request = hyper::Request::get(path)
        .header(hyper::header::HOST, "localhost")
        .body(Empty::<Bytes>::new())
        .map_err(http)?;
    let response = sender.send_request(request).await.map_err(http)?;
    Ok(Answer {
        status: response.status().as_u16(),
        body: response.into_body(),
        _driven: driven,
    })
}

/// `GET path` on the API's socket: its status and body, whole.
async fn get(socket: &std::path::Path, path: &str) -> Result<(u16, Bytes)> {
    let Answer {
        status,
        body,
        _driven,
    } = request(socket, path).await?;
    let body = Limited::new(body, MAX_ANSWER)
        .collect()
        .await
        .map_err(http)?
        .to_bytes();
    Ok((status, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A stop is waited for past the container's own stop timeout and the
    /// engine's waits, never Docker's default alone.
    #[test]
    fn a_stop_waits_for_the_container_s_own_timeout() {
        assert_eq!(stop_wait_for(10), Duration::from_secs(120 + 10 + 60 + 10));
        assert_eq!(stop_wait_for(300), Duration::from_secs(120 + 10 + 60 + 300));
        assert_eq!(
            stop_wait_for(u64::MAX),
            Duration::from_secs(120 + 10 + 60 + 86_400)
        );
    }

    /// A container is named in a path only by an ID or a name: never a
    /// path of the caller's.
    #[test]
    fn only_a_container_s_id_or_name_is_put_in_a_path() {
        assert_eq!(container_ref("a1b2").unwrap(), "a1b2");
        assert_eq!(container_ref("my_app.web-1").unwrap(), "my_app.web-1");
        for bad in [
            "",
            "../images/x",
            "a/b",
            "a?x=1",
            "a b",
            "a%2f",
            ".",
            "..",
            "-x",
        ] {
            assert!(
                matches!(container_ref(bad), Err(ClientError::BadReference(_))),
                "{bad:?}"
            );
            assert!(
                matches!(name_ref(bad), Err(ClientError::BadReference(_))),
                "{bad:?}"
            );
        }
        // A volume's or network's name: compose's too.
        assert_eq!(name_ref("shop_pgdata").unwrap(), "shop_pgdata");
        assert_eq!(name_ref("bridge").unwrap(), "bridge");
        // An image's reference: a registry, a tag and a digest are its own.
        // An image's ID, as the Images section removes one of several tags.
        let id = "sha256:0123456789abcdef";
        assert_eq!(image_ref(id).unwrap(), id);
        let pinned = "registry.example:5000/team/app:1.0@sha256:ab12";
        assert_eq!(image_ref(pinned).unwrap(), pinned);
        for bad in [
            "",
            "app?x=1",
            "app#x",
            "a b",
            "app%2f",
            "app\\x",
            "../containers/x",
            "a/./b",
            "/app",
            "a//b",
        ] {
            assert!(
                matches!(image_ref(bad), Err(ClientError::BadReference(_))),
                "{bad:?}"
            );
        }
    }

    /// An answer of a version this knows is read; a newer one is refused,
    /// as is one with no version.
    #[test]
    fn a_newer_answer_is_refused() {
        let at = |version: u64| serde_json::json!({"version": version, "links": []});
        assert!(check_version(&at(1), "the links answer", LINKS_VERSION).is_ok());
        assert!(matches!(
            check_version(&at(2), "the links answer", LINKS_VERSION),
            Err(ClientError::Newer {
                found: 2,
                known: 1,
                ..
            })
        ));
        assert!(matches!(
            check_version(&serde_json::json!({"links": []}), "the links answer", 1),
            Err(ClientError::NoVersion(_))
        ));
        // An endpoint that names nothing, missing: an older Shipyard.
        let missing = older_if_missing(Err(ClientError::Api {
            path: "/shipyard/links".to_owned(),
            status: 404,
            message: "page not found".to_owned(),
        }));
        assert_eq!(missing.unwrap_err().kind(), crate::ErrorKind::Older);
        // Another refusal stays the API's.
        let refused = older_if_missing(Err(ClientError::Api {
            path: "/shipyard/links".to_owned(),
            status: 500,
            message: "broken".to_owned(),
        }));
        assert_eq!(refused.unwrap_err().kind(), crate::ErrorKind::Api);
        // The network view, as each of Shipyard's own answers.
        let view = serde_json::json!({"version": 2, "Name": "bridge"});
        assert!(matches!(
            check_version(&view, "the network view", NETWORK_VERSION),
            Err(ClientError::Newer { found: 2, .. })
        ));
    }
}
