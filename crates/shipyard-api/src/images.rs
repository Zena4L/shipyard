//! Images (step 11.2c1): `POST /images/create` pulls, its progress
//! streamed as Docker's JSON lines; `GET /images/json` lists; `GET
//! /images/{name}/json` inspects; `POST /images/{name}/tag` tags;
//! `DELETE /images/{name}` removes. A name is a reference or an ID (the
//! manifest's digest, or the config's, or a prefix of either: D11.2c-2).
//!
//! A pull signs in with the credentials the client hands in
//! (`X-Registry-Auth`, D11.2c-3), else with the service's own Docker
//! config. They're handed to the pull alone: never logged, kept, or put in
//! an answer. Progress comes as the engine has it, once per layer, when
//! it's in the store (never byte by byte). A client that goes doesn't cut
//! a pull short (it's one blocking call): what's left of it isn't written.

use std::sync::Arc;

use base64::Engine as _;
use hyper::header::HeaderMap;
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use shipyard_engine::image::{Credential, GivenCredential, ImageReference};
use shipyard_engine::logs::Gone;
use shipyard_engine::report::{Message, Progress, Reporter};

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body, Sender};
use crate::streams::{self, send};
use shipyard_engine::docker::filters::ImageFilters;

type Reply = Result<Response<Body>, ApiError>;

/// The header a Docker client hands its credentials in.
pub(crate) const AUTH_HEADER: &str = "x-registry-auth";

/// The most of an `X-Registry-Auth` read: a username, a password or a
/// token, and an address.
const MAX_AUTH: usize = 16 * 1024;

/// `POST /images/create` (`fromImage`, `tag`): the image pulled, its
/// progress as it comes. `fromSrc` (an import) and a `platform` other than
/// this host's are refused (400), as is a reference that isn't one.
///
/// # Errors
///
/// A request refused before the pull (400, 503); once it's under way, its
/// failure is written in the stream (Docker's `errorDetail`).
pub(crate) fn pull(shared: &Arc<Shared>, query: &Query, headers: &HeaderMap) -> Reply {
    if query
        .get("fromSrc")
        .is_some_and(|source| !source.is_empty())
    {
        return Err(ApiError::bad(
            "importing an image (fromSrc) isn't supported: pull it from a registry",
        ));
    }
    if let Some(platform) = query
        .get("platform")
        .filter(|platform| !platform.is_empty())
        && !host_platform(platform)
    {
        return Err(ApiError::bad(format!(
            "only this host's platform is pulled ({}), not {platform}",
            host_platform_name()
        )));
    }
    let reference = reference(query)?;
    let given = credentials(headers)?;
    let permit = streams::stream_permit(shared)?;
    let (sender, body) = reply::channel();
    let shown = reference.familiar();
    // The service's warnings (an index found corrupt) aren't a pull's
    // lines, as Docker's has none: the reporter keeps progress only.
    let progress = Arc::new(PullProgress::new(sender.clone(), shared));
    let puller = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        let canonical = reference.to_string();
        let pulled = puller
            .call_reporting(Arc::clone(&progress) as _, move |engine| {
                shipyard_engine::image::pull_with(engine, &canonical, given)
            })
            .await;
        let lines: Vec<Line> = match pulled {
            Ok(pulled) => vec![
                Line::status(format!("Digest: {}", pulled.digest)),
                Line::status(if pulled.newer {
                    format!("Status: Downloaded newer image for {shown}")
                } else {
                    format!("Status: Image is up to date for {shown}")
                }),
            ],
            Err(error) => vec![Line::error(&plain(&error.message))],
        };
        // Awaited, never `send`'s sleep: this task is on the service's one
        // async thread, which must go on serving (this client's body too).
        for line in lines {
            if sender.send(line.bytes().into()).await.is_err() {
                break;
            }
        }
    });
    Ok(reply::channelled(StatusCode::OK, "application/json", body))
}

/// The reference `fromImage` and `tag` name: `tag` (a tag, or a digest:
/// `sha256:…`) in place of any `fromImage` has, as Docker's daemon takes
/// it; neither, `latest` (Docker's client always sends one; Docker's
/// daemon would pull every tag).
///
/// # Errors
///
/// No `fromImage`, or what they make isn't a reference (400).
fn reference(query: &Query) -> Result<ImageReference, ApiError> {
    let invalid = |error: shipyard_engine::image::ReferenceError| {
        ApiError::bad(format!("invalid reference format: {error}"))
    };
    let image = query
        .get("fromImage")
        .filter(|image| !image.is_empty())
        .ok_or_else(|| ApiError::bad("a pull needs fromImage"))?;
    let image = ImageReference::parse(image).map_err(invalid)?;
    let Some(tag) = query.get("tag").filter(|tag| !tag.is_empty()) else {
        return Ok(image);
    };
    let name = format!("{}/{}", image.registry, image.repository);
    let text = if tag.contains(':') {
        format!("{name}@{tag}")
    } else {
        format!("{name}:{tag}")
    };
    ImageReference::parse(&text).map_err(invalid)
}

/// Whether `platform` (`os[/arch[/variant]]`) is this host's.
fn host_platform(platform: &str) -> bool {
    let mut parts = platform.split('/');
    let os = parts.next().unwrap_or_default();
    let arch = parts.next();
    os == "linux"
        && arch.is_none_or(|arch| {
            arch == std::env::consts::ARCH
                || (arch == "arm64" && std::env::consts::ARCH == "aarch64")
                || (arch == "amd64" && std::env::consts::ARCH == "x86_64")
        })
}

/// This host's platform, as Docker names it.
fn host_platform_name() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("linux/{arch}")
}

/// What a Docker client puts in `X-Registry-Auth` (base64url of its JSON).
/// No `Debug`: it holds the secrets in clear.
#[derive(Default, Deserialize)]
#[serde(default)]
struct AuthConfig {
    username: String,
    password: String,
    auth: String,
    serveraddress: String,
    identitytoken: String,
    registrytoken: String,
}

/// The credential the request hands in, if it hands one: a registry's own
/// bearer token (`registrytoken`, first, as Docker reads it: F118), an
/// identity token, or a username and a password (or Docker's `auth`,
/// base64 of `user:password`). An empty one (`{}`, Docker's for "none")
/// is none.
///
/// # Errors
///
/// A header too long, or that isn't base64 of Docker's JSON (400: never
/// echoing it).
pub(crate) fn credentials(headers: &HeaderMap) -> Result<Option<GivenCredential>, ApiError> {
    let Some(header) = headers.get(AUTH_HEADER) else {
        return Ok(None);
    };
    let refused = || ApiError::bad("X-Registry-Auth isn't base64 of Docker's credentials JSON");
    if header.len() > MAX_AUTH {
        return Err(ApiError::bad(format!(
            "X-Registry-Auth is over {MAX_AUTH} bytes"
        )));
    }
    let text = header
        .to_str()
        .map_err(|_| refused())?
        .trim()
        .trim_end_matches('=');
    if text.is_empty() {
        return Ok(None);
    }
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text))
        .map_err(|_| refused())?;
    let config: AuthConfig = serde_json::from_slice(&json).map_err(|_| refused())?;
    given(config).map_err(|()| refused())
}

/// The credential `config` holds, if it holds one (see [`credentials`]);
/// `Err` for an `auth` that isn't base64 of `user:password`.
fn given(config: AuthConfig) -> Result<Option<GivenCredential>, ()> {
    let registry = Some(config.serveraddress).filter(|address| !address.is_empty());
    let credential = if !config.registrytoken.is_empty() {
        Credential::RegistryToken(config.registrytoken)
    } else if !config.identitytoken.is_empty() {
        Credential::IdentityToken(config.identitytoken)
    } else if !config.username.is_empty() || !config.password.is_empty() {
        Credential::Basic {
            username: config.username,
            password: config.password,
        }
    } else if !config.auth.is_empty() {
        let pair = base64::engine::general_purpose::STANDARD
            .decode(config.auth.trim())
            .ok()
            .and_then(|pair| String::from_utf8(pair).ok())
            .ok_or(())?;
        let (username, password) = pair.split_once(':').ok_or(())?;
        Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(GivenCredential {
        credential,
        registry,
    }))
}

/// `POST /auth` (`docker login`): the body's credentials (Docker's
/// `AuthConfig`) checked against the registry its `serveraddress` names
/// (Docker Hub if none), never kept, logged or echoed (F118). 200 `Login
/// Succeeded`: the client keeps them, as Docker's CLI does.
///
/// # Errors
///
/// No credential, or a body that isn't Docker's JSON (400, never echoing
/// it); refused by the registry (401, Docker's `unauthorized: …`); the
/// registry can't be reached (500).
pub(crate) async fn auth(shared: &Arc<Shared>, body: hyper::body::Incoming) -> Reply {
    use http_body_util::{BodyExt as _, Limited};
    let refused = || ApiError::bad("the body isn't Docker's credentials JSON");
    let bytes = Limited::new(body, MAX_AUTH)
        .collect()
        .await
        .map_err(|_| refused())?
        .to_bytes();
    let config: AuthConfig = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    let given = given(config)
        .map_err(|()| refused())?
        .ok_or_else(|| ApiError::bad("a username and a password, or a token, is needed"))?;
    let (login, _) = shared
        .call(move |_| shipyard_engine::image::login(given))
        .await?;
    match login {
        shipyard_engine::image::Login::Succeeded => Ok(reply::json(
            StatusCode::OK,
            &serde_json::json!({"Status": "Login Succeeded", "IdentityToken": ""}),
        )),
        shipyard_engine::image::Login::Refused(why) => {
            Err(ApiError::new(StatusCode::UNAUTHORIZED, why))
        }
    }
}

/// One of a pull's JSON lines, Docker's `JSONMessage`.
#[derive(Debug, Serialize)]
struct Line {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "progressDetail", skip_serializing_if = "Option::is_none")]
    progress_detail: Option<ProgressDetail>,
    #[serde(rename = "errorDetail", skip_serializing_if = "Option::is_none")]
    error_detail: Option<ErrorDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A layer's bytes in a progress line: none once it's done.
#[derive(Debug, Serialize)]
struct ProgressDetail {}

/// A failure, in a stream already under way.
#[derive(Debug, Serialize)]
struct ErrorDetail {
    message: String,
}

impl Line {
    fn status(status: String) -> Self {
        Self {
            status: Some(status),
            id: None,
            progress_detail: None,
            error_detail: None,
            error: None,
        }
    }

    fn error(message: &str) -> Self {
        Self {
            status: None,
            id: None,
            progress_detail: None,
            error_detail: Some(ErrorDetail {
                message: message.to_owned(),
            }),
            error: Some(message.to_owned()),
        }
    }

    /// The line as sent: its JSON and a newline.
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    /// The line the engine's `progress` makes.
    fn of(progress: &Progress) -> Line {
        match progress {
            Progress::Pulling { image } => {
                let (name, tag) = ImageReference::parse(image).map_or_else(
                    |_| (image.clone(), String::new()),
                    |reference| {
                        let tag = reference
                            .tag
                            .clone()
                            .or_else(|| reference.digest.as_ref().map(ToString::to_string))
                            .unwrap_or_default();
                        (reference.repository.clone(), tag)
                    },
                );
                Line {
                    id: Some(tag),
                    ..Line::status(format!("Pulling from {name}"))
                }
            }
            Progress::Layer {
                short_digest,
                downloaded,
            } => Line {
                id: Some(short_digest.clone()),
                // Empty, as Docker's once a layer's done (with its bytes,
                // its CLI would draw a bar after it).
                progress_detail: Some(ProgressDetail {}),
                ..Line::status(
                    if downloaded.is_some() {
                        "Pull complete"
                    } else {
                        "Already exists"
                    }
                    .to_owned(),
                )
            },
        }
    }
}

/// A pull's reporter: each progress message a line on its stream.
struct PullProgress {
    sender: Sender,
    gone: Gone,
}

impl PullProgress {
    fn new(sender: Sender, shared: &Arc<Shared>) -> Self {
        let gone = streams::gone(shared, &sender);
        Self { sender, gone }
    }

    /// Writes `line` to the client: from the pull's blocking thread only
    /// (`send` sleeps while the client is behind).
    fn write(&self, line: &Line) -> std::io::Result<()> {
        send(&self.sender, &self.gone, line.bytes())
    }
}

impl Reporter for PullProgress {
    fn report(&self, message: Message) {
        if let Message::Progress(progress) = message {
            // A client gone: the pull goes on, unwritten.
            let _ = self.write(&Line::of(&progress));
        }
    }
}

/// `GET /images/json` (`filters`: `reference`, `label`, `dangling`).
///
/// # Errors
///
/// A filter that can't be read (400); the store's errors.
pub(crate) async fn list(shared: &Arc<Shared>, query: &Query) -> Reply {
    let filters = ImageFilters::parse(crate::filters::given(query.get("filters"))?)?;
    let (images, _) = shared
        .call(move |engine| shipyard_engine::docker::list_images_where(engine, &filters))
        .await?;
    Ok(reply::json(StatusCode::OK, &images))
}

/// `GET /images/{name}/json`: 404 `No such image: NAME`.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn inspect(shared: &Arc<Shared>, name: String) -> Reply {
    let shown = name.clone();
    let inspected = shared
        .call(move |engine| shipyard_engine::docker::inspect_image(engine, &name))
        .await;
    match inspected {
        Ok((image, _)) => Ok(reply::json(StatusCode::OK, &image)),
        Err(error) if error.status == StatusCode::NOT_FOUND => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("No such image: {shown}"),
        )),
        Err(error) => Err(error),
    }
}

/// `POST /images/{name}/tag` (`repo`, `tag`: `latest` if none): 201.
///
/// # Errors
///
/// No `repo` (400); as the engine's.
pub(crate) async fn tag(shared: &Arc<Shared>, name: String, query: &Query) -> Reply {
    let repo = query
        .get("repo")
        .filter(|repo| !repo.is_empty())
        .ok_or_else(|| ApiError::bad("a tag needs repo"))?;
    let target = match query.get("tag").filter(|tag| !tag.is_empty()) {
        Some(tag) => format!("{repo}:{tag}"),
        None => repo.to_owned(),
    };
    shared
        .call(move |engine| shipyard_engine::image::tag(engine, &name, &target))
        .await?;
    Ok(reply::empty(StatusCode::CREATED))
}

/// `DELETE /images/{name}` (`force`; `noprune` taken and ignored: nothing
/// is kept for a parent): Docker's `[{"Untagged"}, {"Deleted"}]`.
///
/// # Errors
///
/// As the engine's: 404, 409 in use or several references (without
/// `force`).
pub(crate) async fn remove(shared: &Arc<Shared>, name: String, query: &Query) -> Reply {
    #[derive(Serialize)]
    enum Item {
        Untagged(String),
        Deleted(String),
    }
    let force = query.flag("force");
    let (removed, _) = shared
        .call(move |engine| shipyard_engine::image::remove(engine, &name, force))
        .await?;
    let items: Vec<Item> = removed
        .untagged
        .iter()
        .map(|reference| Item::Untagged(familiar(reference)))
        .chain(removed.deleted.into_iter().map(Item::Deleted))
        .collect();
    Ok(reply::json(StatusCode::OK, &items))
}

/// `reference` as Docker shows it (`alpine:3.20`), or as it is if it
/// isn't one.
fn familiar(reference: &str) -> String {
    ImageReference::parse(reference)
        .map_or_else(|_| reference.to_owned(), |parsed| parsed.familiar())
}

/// `text` without control characters: a registry's words, on their way to
/// a terminal (Docker's CLI prints a pull's error as it comes).
fn plain(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// `POST /images/prune`: the images `filters` takes that nothing uses
/// (`dangling`: `true` by default, `false` every unused one; `until`,
/// `label`, `label!`), then what nothing needs in the store; Docker's
/// `ImagesDeleted` (`{"Untagged"}` then `{"Deleted"}`, `null` for none)
/// and `SpaceReclaimed`.
pub(crate) async fn prune(shared: &Arc<Shared>, query: &Query) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        images_deleted: Option<Vec<Deleted>>,
        space_reclaimed: u64,
    }
    #[derive(Serialize)]
    enum Deleted {
        Untagged(String),
        Deleted(String),
    }
    let filters = shipyard_engine::docker::filters::ImagePruneFilters::parse(
        crate::filters::given(query.get("filters"))?,
        shipyard_engine::docker::unix_now(),
    )?;
    let (pruned, _) = shared
        .call(move |engine| shipyard_engine::prune::images(engine, &filters))
        .await?;
    let deleted: Vec<Deleted> = pruned
        .untagged
        .into_iter()
        .map(Deleted::Untagged)
        .chain(pruned.deleted.into_iter().map(Deleted::Deleted))
        .collect();
    Ok(reply::json(
        StatusCode::OK,
        &Pruned {
            images_deleted: (!deleted.is_empty()).then_some(deleted),
            space_reclaimed: pruned.bytes,
        },
    ))
}

/// `POST /build/prune`: no build cache yet (stage 12 builds images), so
/// nothing deleted, as Docker answers one without a cache (its `system
/// prune` asks for it).
pub(crate) fn prune_build() -> hyper::Response<crate::reply::Body> {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        caches_deleted: Option<Vec<String>>,
        space_reclaimed: u64,
    }
    reply::json(
        StatusCode::OK,
        &Pruned {
            caches_deleted: None,
            space_reclaimed: 0,
        },
    )
}

#[cfg(test)]
mod tests;
