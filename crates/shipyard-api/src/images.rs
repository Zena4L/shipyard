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

use hyper::header::HeaderMap;
use hyper::{Response, StatusCode};
use serde::Serialize;
use shipyard_engine::image::{GivenCredential, ImageReference};
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
/// progress as it comes (an import, `fromSrc`, is [`import`]'s). A
/// `platform` other than this host's is refused (400), as is a reference
/// that isn't one.
///
/// # Errors
///
/// A request refused before the pull (400, 503); once it's under way, its
/// failure is written in the stream (Docker's `errorDetail`).
pub(crate) fn pull(shared: &Arc<Shared>, query: &Query, headers: &HeaderMap) -> Reply {
    // A platform asked for must run here (step 13.3: amd64 on a Mac's VM,
    // through Rosetta); none asked, the image's own among what does.
    let platform = match query
        .get("platform")
        .filter(|platform| !platform.is_empty())
    {
        None => None,
        Some(asked) => Some(
            shipyard_engine::image::platform_here(asked, "pulled")
                .map_err(|error| ApiError::bad(error.to_string()))?,
        ),
    };
    let reference = reference(query)?;
    let given = credentials(headers)?;
    let permit = streams::stream_permit(shared)?;
    let (sender, body) = reply::channel();
    let shown = reference.familiar();
    // The service's warnings (an index found corrupt) aren't a pull's
    // lines, as Docker's has none: the reporter keeps progress only.
    let progress = Arc::new(StreamedProgress::new(sender.clone(), shared));
    let puller = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        let canonical = reference.to_string();
        let pulled = puller
            .call_reporting(Arc::clone(&progress) as _, move |engine| {
                shipyard_engine::image::pull_with(engine, &canonical, given, platform)
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
    let text = header.to_str().map_err(|_| refused())?;
    GivenCredential::from_docker_auth(text).map_err(|_| refused())
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
    let given = GivenCredential::from_auth_json(&bytes)
        .map_err(|_| refused())?
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
    /// A push's last word for a tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    aux: Option<PushAux>,
}

/// Docker's `aux` for a pushed tag.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct PushAux {
    tag: String,
    digest: String,
    size: u64,
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
    fn empty() -> Self {
        Self {
            status: None,
            id: None,
            progress_detail: None,
            error_detail: None,
            error: None,
            aux: None,
        }
    }

    fn status(status: String) -> Self {
        Self {
            status: Some(status),
            ..Self::empty()
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
            aux: None,
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
            // (No build through the API: a line said as a status if one came.)
            Progress::Build { line } => Line::status(line.clone()),
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
            Progress::Pushing { name } => {
                let repository = ImageReference::parse(name).map_or_else(
                    |_| name.clone(),
                    |reference| format!("{}/{}", reference.registry, reference.repository),
                );
                Line::status(format!(
                    "The push refers to repository [{}]",
                    plain(&repository)
                ))
            }
            Progress::Pushed { short_digest, sent } => Line {
                id: Some(short_digest.clone()),
                progress_detail: Some(ProgressDetail {}),
                ..Line::status(
                    if sent.is_some() {
                        "Pushed"
                    } else {
                        "Layer already exists"
                    }
                    .to_owned(),
                )
            },
        }
    }
}

/// A pull's reporter: each progress message a line on its stream.
struct StreamedProgress {
    sender: Sender,
    gone: Gone,
}

impl StreamedProgress {
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

impl Reporter for StreamedProgress {
    fn report(&self, message: Message) {
        if let Message::Progress(progress) = message {
            // A client gone: the pull goes on, unwritten.
            let _ = self.write(&Line::of(&progress));
        }
    }
}

/// `POST /images/{name}/push` (`tag`; none, and the name none: every tag
/// of the repository, as Docker's `push -a`): pushed, signing in with the
/// client's `X-Registry-Auth` (else the service's Docker config), its
/// progress as Docker's lines, then each tag's `TAG: digest: … size: …`
/// and its `aux`. A client that goes doesn't cut it short (as a pull).
///
/// # Errors
///
/// A name that isn't one, or `X-Registry-Auth` that can't be read (400);
/// once under way, the failure in the stream.
pub(crate) fn push(shared: &Arc<Shared>, name: &str, query: &Query, headers: &HeaderMap) -> Reply {
    let tag = query.get("tag").filter(|tag| !tag.is_empty());
    let parsed = ImageReference::parse(name)
        .map_err(|error| ApiError::bad(format!("invalid reference format: {error}")))?;
    let all_tags = tag.is_none() && !shipyard_engine::image::names_a_tag(name);
    let name = match tag {
        Some(tag) => format!("{}/{}:{tag}", parsed.registry, parsed.repository),
        None => name.to_owned(),
    };
    let given = credentials(headers)?;
    let permit = streams::stream_permit(shared)?;
    let (sender, body) = reply::channel();
    let progress = Arc::new(StreamedProgress::new(sender.clone(), shared));
    let pusher = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        let request = shipyard_engine::image::PushRequest {
            name,
            all_tags,
            given,
        };
        let pushed = pusher
            .call_reporting(Arc::clone(&progress) as _, move |engine| {
                shipyard_engine::image::push(engine, &request)
            })
            .await;
        let lines: Vec<Line> = match pushed {
            Ok(pushed) => pushed
                .iter()
                .flat_map(|name| {
                    [
                        Line::status(format!(
                            "{}: digest: {} size: {}",
                            plain(&name.tag),
                            name.digest,
                            name.size
                        )),
                        Line {
                            progress_detail: Some(ProgressDetail {}),
                            aux: Some(PushAux {
                                tag: plain(&name.tag),
                                digest: name.digest.clone(),
                                size: name.size,
                            }),
                            ..Line::empty()
                        },
                    ]
                })
                .collect(),
            Err(error) => vec![Line::error(&plain(&error.message))],
        };
        for line in lines {
            if sender.send(line.bytes().into()).await.is_err() {
                break;
            }
        }
    });
    Ok(reply::channelled(StatusCode::OK, "application/json", body))
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
    let (image, _) = inspected.map_err(no_such_image(&shown))?;
    Ok(reply::json(StatusCode::OK, &image))
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

/// `POST /build/prune` (`docker builder prune`, `system prune`'s last
/// ask): every context's build cache no build is using, removed (step
/// 12.3); Docker's `CachesDeleted` (`null` for none) and `SpaceReclaimed`.
/// A cache a build is using is kept, as Docker keeps one in use. With
/// `filters` or `keep-storage` (a context's cache has no labels or times to
/// read, nor a part to keep), nothing is removed: `docker system prune
/// --filter …` asks with its filters, and never wipes the cache for them.
///
/// # Errors
///
/// Filters that aren't Docker's JSON (400); the store's.
pub(crate) async fn prune_build(shared: &Arc<Shared>, query: &Query) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        caches_deleted: Option<Vec<String>>,
        space_reclaimed: u64,
    }
    let filtered = !shipyard_engine::docker::filters::Given::from_json(query.get("filters"))
        .map_err(|error| ApiError::bad(error.to_string()))?
        .is_empty()
        || query
            .get("keep-storage")
            .is_some_and(|keep| !matches!(keep, "" | "0"));
    let pruned = if filtered {
        shipyard_engine::build::cache::Pruned::default()
    } else {
        shared.call(shipyard_engine::build::cache::prune).await?.0
    };
    Ok(reply::json(
        StatusCode::OK,
        &Pruned {
            caches_deleted: Some(pruned.removed).filter(|removed| !removed.is_empty()),
            space_reclaimed: pruned.bytes,
        },
    ))
}

/// `GET /images/{name}/history`: Docker's `HistoryResponseItem`s, newest
/// first; 404 `No such image: NAME`.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn history(shared: &Arc<Shared>, name: String) -> Reply {
    let shown = name.clone();
    let history = shared
        .call(move |engine| shipyard_engine::docker::image_history(engine, &name))
        .await;
    let (items, _) = history.map_err(no_such_image(&shown))?;
    Ok(reply::json(StatusCode::OK, &items))
}

/// `GET /images/{name}/get` and `GET /images/get?names=…` (`docker save`):
/// one archive of every image named, as it's written (Docker 25's form,
/// D12.2-2). Each name is found first (404 `No such image: NAME` before any
/// byte); an archive that fails after its headers breaks off, never ends as
/// a whole one.
///
/// # Errors
///
/// No name (400); a name that names no image (404); more streams than are
/// served (503).
pub(crate) async fn save(shared: &Arc<Shared>, names: Vec<String>) -> Reply {
    if names.is_empty() {
        return Err(ApiError::bad("no image to save: give names=NAME"));
    }
    // Found as the save finds them (a repository alone is every tag of
    // it), before the headers: a 404 then, never a cut archive.
    let wanted = names.clone();
    shared
        .call(move |engine| shipyard_engine::image::check_saved(engine, &wanted))
        .await
        .map_err(|error| match error.status {
            // Docker's words (`No such image: NAME`), the name the engine's.
            StatusCode::NOT_FOUND => ApiError::new(
                StatusCode::NOT_FOUND,
                plain(&error.message).replacen("no such image", "No such image", 1),
            ),
            _ => error,
        })?;
    let permit = streams::stream_permit(shared)?;
    let (sender, cut, body) = reply::cuttable();
    let writer = crate::archive::ChannelWriter::new(shared, sender);
    let saver = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        let saved = saver
            .call(move |engine| shipyard_engine::image::save(engine, &names, writer))
            .await;
        if let Err(error) = saved {
            tracing::warn!(error = %error.message, "a save broke off: its archive was cut");
            cut.cut();
        }
    });
    Ok(reply::channelled(StatusCode::OK, "application/x-tar", body))
}

/// `POST /images/load` (`quiet`): the body, an archive (plain or gzip), read
/// whole before anything is loaded; then Docker's JSON lines: `Loaded
/// image: NAME` for each name, what was left out said too. A tag that moved
/// to the archive's image is said, as Docker's load says it.
///
/// # Errors
///
/// The archive is cut, malformed, past a bound, or holds no image to load
/// (400); the store's errors.
pub(crate) async fn load(
    shared: &Arc<Shared>,
    query: &Query,
    body: hyper::body::Incoming,
) -> Reply {
    let quiet = query.flag("quiet");
    let (permit, input) = crate::archive::body_input(shared, body)?;
    let (loaded, _) = shared
        .call(move |engine| {
            let _permit = permit;
            shipyard_engine::image::load(engine, input)
        })
        .await
        // Its refusal quotes the archive (a member's path, a left-out
        // image's words): never sent raw either.
        .map_err(|error| ApiError::new(error.status, plain(&error.message)))?;
    // An archive's words, its names and its config's strings among them:
    // never sent raw (Docker's CLI prints `stream` as it comes).
    let mut lines = Vec::new();
    for text in loaded.lines(quiet) {
        let line = serde_json::json!({ "stream": format!("{}\n", plain(&text)) });
        lines.extend(serde_json::to_vec(&line).unwrap_or_default());
        lines.push(b'\n');
    }
    Ok(reply::with_type(StatusCode::OK, "application/json", lines))
}

/// `POST /images/create?fromSrc=-` (`docker import`, step 12.2b): the
/// body, a root filesystem's tar (plain or gzip), made a one-layer image
/// named `repo` (a whole reference, as Docker's CLI sends it; `tag` added
/// to one without a tag), its config set by `changes` (repeated), its
/// history's comment `message`. Docker's JSON line `{"status": ID}`. A
/// `fromSrc` that's a URL is refused (400), as is no name (D12.2-9), a
/// change import doesn't take, another `platform`.
///
/// # Errors
///
/// As the engine's `image::import`'s; more streams than are served (503).
pub(crate) async fn import(
    shared: &Arc<Shared>,
    query: &Query,
    body: hyper::body::Incoming,
) -> Reply {
    if query.get("fromSrc") != Some("-") {
        return Err(ApiError::bad(
            "importing from a URL isn't supported: download it, then import it (fromSrc=-)",
        ));
    }
    let name = query
        .get("repo")
        .filter(|repo| !repo.is_empty())
        .map(|repo| {
            match query.get("tag").filter(|tag| !tag.is_empty()) {
                // `repo` names no tag of its own (a port isn't one).
                Some(tag) if !repo.rsplit('/').next().unwrap_or(repo).contains(':') => {
                    format!("{repo}:{tag}")
                }
                _ => repo.to_owned(),
            }
        });
    let request = shipyard_engine::image::ImportRequest {
        name,
        changes: query.get_all("changes"),
        // Docker's CLI sends it empty without `-m`: Docker's comment then.
        message: query
            .get("message")
            .filter(|message| !message.is_empty())
            .map(str::to_owned),
        platform: query
            .get("platform")
            .filter(|platform| !platform.is_empty())
            .map(str::to_owned),
    };
    let (permit, input) = crate::archive::body_input(shared, body)?;
    let (imported, _) = shared
        .call(move |engine| {
            let _permit = permit;
            shipyard_engine::image::import(engine, input, &request)
        })
        .await
        .map_err(|error| ApiError::new(error.status, plain(&error.message)))?;
    let line = serde_json::json!({ "status": plain(&imported.id) });
    let mut bytes = serde_json::to_vec(&line).unwrap_or_default();
    bytes.push(b'\n');
    Ok(reply::with_type(StatusCode::OK, "application/json", bytes))
}

/// An engine's 404 as Docker's `No such image: NAME`, for `shown`.
fn no_such_image(shown: &str) -> impl FnOnce(ApiError) -> ApiError + '_ {
    move |error| {
        if error.status == StatusCode::NOT_FOUND {
            ApiError::new(StatusCode::NOT_FOUND, format!("No such image: {shown}"))
        } else {
            error
        }
    }
}

#[cfg(test)]
mod tests;
