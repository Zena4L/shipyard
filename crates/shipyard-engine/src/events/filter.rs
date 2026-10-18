//! Which events a reader wants: Docker's `--filter`s.

use super::{Event, Kind};
use crate::error::Result;

/// Which events [`read`](super::read()) gives: those of every key given (a
/// key given several times: any of its values), between `since` and `until`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Filters {
    /// Their types.
    pub kinds: Vec<Kind>,
    /// Their actions.
    pub actions: Vec<String>,
    /// About these containers (an ID's start, or a name).
    pub containers: Vec<String>,
    /// About these images (as a container's `image` too).
    pub images: Vec<String>,
    /// About these volumes.
    pub volumes: Vec<String>,
    /// About these networks.
    pub networks: Vec<String>,
}

impl Filters {
    /// Filters from `KEY=VALUE`s (`type`, `event`, `container`, `image`,
    /// `volume`, `network`).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a key it doesn't know,
    /// no `=`, or no value.
    pub fn parse(given: &[String]) -> Result<Self> {
        let mut filters = Self::default();
        for filter in given {
            let Some((key, value)) = filter.split_once('=') else {
                crate::error::bail!(InvalidRequest, "filter {filter:?}: give KEY=VALUE");
            };
            if value.is_empty() {
                crate::error::bail!(InvalidRequest, "filter {filter:?}: give a value");
            }
            let value = value.to_owned();
            match key {
                "type" => filters.kinds.push(value.parse()?),
                "event" => filters.actions.push(value),
                "container" => filters.containers.push(value),
                "image" => filters.images.push(value),
                "volume" => filters.volumes.push(value),
                "network" => filters.networks.push(value),
                _ => crate::error::bail!(
                    InvalidRequest,
                    "filter {key:?}: give type, event, container, image, volume or network"
                ),
            }
        }
        Ok(filters)
    }

    /// Whether `event` is a network's, about a container these filters
    /// want (`connect`, `disconnect`: its `container` attribute, its ID or
    /// a start of it), as Docker's `--filter container=` takes them.
    fn about_its_networks(&self, event: &Event) -> bool {
        event.kind == Kind::Network
            && event.attributes.get("container").is_some_and(|id| {
                self.containers
                    .iter()
                    .any(|want| id.starts_with(want.as_str()))
            })
    }

    /// Whether `event` passes.
    #[must_use]
    pub fn pass(&self, event: &Event) -> bool {
        // A container by its name or its ID's start; an image by its
        // reference however it's written (`alpine` is
        // `docker.io/library/alpine:latest`); the rest by name.
        let about = |kind: Kind, wanted: &[String]| {
            wanted.is_empty()
                || (event.kind == kind
                    && wanted.iter().any(|want| {
                        event.id == *want
                            || (kind == Kind::Container && event.id.starts_with(want.as_str()))
                            || (kind == Kind::Image && same_image(&event.id, want))
                            || event.attributes.get("name") == Some(want)
                    }))
        };
        (self.kinds.is_empty() || self.kinds.contains(&event.kind))
            && (self.actions.is_empty() || self.actions.contains(&event.action))
            && (about(Kind::Container, &self.containers) || self.about_its_networks(event))
            && (about(Kind::Image, &self.images)
                || self.images.iter().any(|want| {
                    event
                        .attributes
                        .get("image")
                        .is_some_and(|image| same_image(image, want))
                }))
            && about(Kind::Volume, &self.volumes)
            && about(Kind::Network, &self.networks)
    }
}

/// Whether two image references name the same image (each as written,
/// or both read the same way).
fn same_image(one: &str, other: &str) -> bool {
    use shipyard_image::ImageReference;
    one == other
        || ImageReference::parse(one)
            .ok()
            .zip(ImageReference::parse(other).ok())
            .is_some_and(|(one, other)| one.to_string() == other.to_string())
}
