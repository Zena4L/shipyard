//! The Run image sheet's run (step 14.4b): an image run detached with the
//! few settings the sheet has, as `shipyard run -d` does (D14.4b-3, -7). The
//! spec comes from the page and is all it may say: the create body is built
//! here, so no bind, privilege or network the page names can reach it. An
//! image the store lacks is pulled first, its progress handed on a line at
//! a time.

use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use futures_util::StreamExt as _;

use crate::api::{Client, container_ref, image_ref, within};
use crate::{ClientError, Result};

/// The memory limits the sheet offers, MiB (the mock's).
pub const MEMORY_CHOICES: [u32; 5] = [128, 256, 512, 1024, 2048];

/// How long a pull may go without a line before it's given up: the API
/// says a layer once it's fetched, and nothing while it unpacks one.
pub const PULL_QUIET: Duration = Duration::from_secs(600);

/// How long a create may take: a stored image unpacked the first time it
/// runs, a `VOLUME`'s files copied in.
pub const CREATE_WAIT: Duration = Duration::from_secs(600);

/// The most of a pull's line handed on.
const MAX_LINE: usize = 200;

/// What the sheet asks for, as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunSpec {
    /// The image's reference.
    pub image: String,
    /// The container's name; none (or empty): the API's own.
    pub name: Option<String>,
    /// Its memory limit, MiB: one of [`MEMORY_CHOICES`].
    pub memory_mib: u32,
    /// One port published, `HOST:CONTAINER` or `CONTAINER` (TCP).
    pub port: Option<String>,
}

/// A [`RunSpec`] whose every field was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSpec {
    image: String,
    name: Option<String>,
    memory_mib: u32,
    port: Option<(u16, u16)>,
}

impl RunSpec {
    /// The spec, each field in shape.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadReference`] for an image or a name of another
    /// shape; [`ClientError::BadSpec`] for a memory limit not offered or a
    /// port that isn't one.
    pub fn checked(&self) -> Result<CheckedSpec> {
        let image = image_ref(self.image.trim())?.to_owned();
        let name = match self.name.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(name) => Some(container_ref(name)?.to_owned()),
        };
        if !MEMORY_CHOICES.contains(&self.memory_mib) {
            return Err(ClientError::BadSpec(format!(
                "a memory limit of {} MiB isn't one offered",
                self.memory_mib
            )));
        }
        let port = match self.port.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(port) => Some(published(port)?),
        };
        Ok(CheckedSpec {
            image,
            name,
            memory_mib: self.memory_mib,
            port,
        })
    }
}

/// `HOST:CONTAINER` or `CONTAINER` (the host's the same), each 1–65535.
fn published(port: &str) -> Result<(u16, u16)> {
    let number = |text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(|| ClientError::BadSpec(format!("{port:?} isn't a port to publish")))
    };
    match port.split_once(':') {
        Some((host, container)) => Ok((number(host)?, number(container)?)),
        None => number(port).map(|port| (port, port)),
    }
}

impl CheckedSpec {
    /// The create body: the image, the memory limit, the one port, and
    /// nothing else.
    #[must_use]
    pub fn body(&self) -> ContainerCreateBody {
        let exposed = self.port.map(|(_, container)| format!("{container}/tcp"));
        let bindings = self.port.map(|(host, container)| {
            [(
                format!("{container}/tcp"),
                Some(vec![PortBinding {
                    host_ip: None,
                    host_port: Some(host.to_string()),
                }]),
            )]
            .into_iter()
            .collect()
        });
        ContainerCreateBody {
            image: Some(self.image.clone()),
            exposed_ports: exposed.map(|port| vec![port]),
            host_config: Some(HostConfig {
                memory: Some(i64::from(self.memory_mib) * 1024 * 1024),
                port_bindings: bindings,
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        }
    }
}

/// Runs `spec` detached: created (the image pulled first if the store
/// lacks it, each line of the pull handed to `on_line`), then started (a
/// start that fails removes what the create made). Its container's ID.
///
/// # Errors
///
/// The spec's refusal ([`RunSpec::checked`]); the API's (a name in use is
/// its 409); [`ClientError::Timeout`] for a pull quiet past [`PULL_QUIET`].
pub async fn run(
    client: &Client,
    spec: &RunSpec,
    mut on_line: impl FnMut(String),
) -> Result<String> {
    let checked = spec.checked()?;
    // The body names an image and nothing else that could be missing (no
    // network, no volume): a create's 404 is the image's alone.
    let created = match create(client, &checked).await {
        Err(ClientError::Docker(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        })) => {
            pull(client, &checked.image, &mut on_line).await?;
            create(client, &checked).await?
        }
        created => created?,
    };
    // Not started: the sheet's container goes (it ran nothing), so the
    // run can be tried again by the same name.
    if let Err(failed) = client.start(&created).await {
        let _ = client.remove(&created, true).await;
        return Err(failed);
    }
    Ok(created)
}

/// The container `checked` makes, its ID.
async fn create(client: &Client, checked: &CheckedSpec) -> Result<String> {
    let options = checked.name.as_deref().map(|name| {
        bollard::query_parameters::CreateContainerOptionsBuilder::default()
            .name(name)
            .build()
    });
    let docker = client.docker().clone().with_timeout(CREATE_WAIT);
    let created = within(
        CREATE_WAIT,
        "creating a container",
        docker.create_container(options, checked.body()),
    )
    .await??;
    Ok(created.id)
}

/// Pulls `image`, each line of its progress to `on_line`.
async fn pull(client: &Client, image: &str, on_line: &mut impl FnMut(String)) -> Result<()> {
    let options = bollard::query_parameters::CreateImageOptionsBuilder::default()
        .from_image(image)
        .build();
    let mut lines = client.docker().create_image(Some(options), None, None);
    loop {
        let Ok(next) = tokio::time::timeout(PULL_QUIET, lines.next()).await else {
            return Err(ClientError::Timeout {
                what: format!("pulling {image}"),
                seconds: PULL_QUIET.as_secs(),
            });
        };
        let Some(info) = next else { return Ok(()) };
        on_line(line_of(&info?));
    }
}

/// A pull's line as the sheet says it: its layer, its status, how far it
/// got, bounded.
fn line_of(info: &bollard::models::CreateImageInfo) -> String {
    let done = info
        .progress_detail
        .as_ref()
        .and_then(|detail| Some((detail.current?, detail.total?)))
        .filter(|&(_, total)| total > 0)
        .map(|(current, total)| format!("{}%", current.saturating_mul(100) / total));
    let line = [info.id.clone(), info.status.clone(), done]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    line.chars().take(MAX_LINE).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn spec(port: Option<&str>) -> RunSpec {
        RunSpec {
            image: "alpine:3.20".to_owned(),
            name: Some("box".to_owned()),
            memory_mib: 256,
            port: port.map(str::to_owned),
        }
    }

    /// The sheet's spec is checked whole: each field's shape, the memory
    /// one offered, the port a port.
    #[test]
    fn a_spec_is_checked_field_by_field() {
        assert!(spec(Some("8081:80")).checked().is_ok());
        assert_eq!(spec(Some("80")).checked().unwrap().port, Some((80, 80)));
        assert_eq!(spec(Some("")).checked().unwrap().port, None);
        for bad in ["0", "70000", "a:b", "80:", ":80", "8081:80:1", "80/udp"] {
            assert!(
                matches!(spec(Some(bad)).checked(), Err(ClientError::BadSpec(_))),
                "{bad:?}"
            );
        }
        let odd_memory = RunSpec {
            memory_mib: 300,
            ..spec(None)
        };
        assert!(matches!(odd_memory.checked(), Err(ClientError::BadSpec(_))));
        let bad_name = RunSpec {
            name: Some("../x".to_owned()),
            ..spec(None)
        };
        assert!(matches!(
            bad_name.checked(),
            Err(ClientError::BadReference(_))
        ));
        let bad_image = RunSpec {
            image: "alpine?x=1".to_owned(),
            ..spec(None)
        };
        assert!(matches!(
            bad_image.checked(),
            Err(ClientError::BadReference(_))
        ));
        let unnamed = RunSpec {
            name: Some(" ".to_owned()),
            ..spec(None)
        };
        assert_eq!(unnamed.checked().unwrap().name, None);
    }

    /// A page can't add to the spec (an unknown field is refused), and the
    /// body says the image, its memory and its port, nothing else.
    #[test]
    fn the_body_is_the_spec_alone() {
        let given = r#"{"image":"a","name":null,"memoryMib":256,"port":null,"privileged":true}"#;
        assert!(serde_json::from_str::<RunSpec>(given).is_err());
        let body = spec(Some("8081:80")).checked().unwrap().body();
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "Image": "alpine:3.20",
                "ExposedPorts": {"80/tcp": {}},
                "HostConfig": {
                    "Memory": 268_435_456,
                    "PortBindings": {"80/tcp": [{"HostPort": "8081"}]}
                }
            })
        );
    }

    /// A pull's line: what it says, bounded.
    #[test]
    fn a_pull_line_is_bounded() {
        let info = bollard::models::CreateImageInfo {
            id: Some("a1b2".to_owned()),
            status: Some("Downloading".to_owned()),
            progress_detail: Some(bollard::models::ProgressDetail {
                current: Some(512),
                total: Some(2048),
            }),
            ..Default::default()
        };
        assert_eq!(line_of(&info), "a1b2 Downloading 25%");
        let long = bollard::models::CreateImageInfo {
            status: Some("x".repeat(500)),
            ..Default::default()
        };
        assert_eq!(line_of(&long).chars().count(), MAX_LINE);
    }
}
