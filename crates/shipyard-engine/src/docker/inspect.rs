//! `docker inspect` of any object (step 11.3c2): a name is looked for as
//! a container, an image, a network and a volume, in docker/cli's order,
//! the first kind that has it answering, or as one kind only.

use serde::Serialize;

use super::{ContainerInspect, ImageInspect, NetworkResource, Volume};
use crate::Engine;
use crate::error::{Error, ErrorKind, Result};

/// A kind of object `inspect` looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A container.
    Container,
    /// An image.
    Image,
    /// A network.
    Network,
    /// A volume.
    Volume,
}

impl Kind {
    /// Every kind, in the order a name is looked for (docker/cli's).
    pub const ALL: [Self; 4] = [Self::Container, Self::Image, Self::Network, Self::Volume];
}

/// An object as Docker's API shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Inspected {
    /// A container's `ContainerInspect`.
    Container(Box<ContainerInspect>),
    /// An image's `ImageInspect`.
    Image(Box<ImageInspect>),
    /// A network's `NetworkResource`.
    Network(Box<NetworkResource>),
    /// A volume's `Volume`.
    Volume(Box<Volume>),
}

/// The object `name` names: as `kind` only, or as the first kind (in
/// [`Kind::ALL`]'s order) that has it. A kind that hasn't it, that can't
/// be named so (`MyVolume` isn't an image's reference), or that this user
/// hasn't (rootless has no networks but the built-ins) passes it on, as
/// Docker's does. A start of more than one container's ID stops the look,
/// as Docker's does: it names something, but not one thing.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: no kind has it (`no such object: NAME`), or
///   `kind`'s hasn't.
/// - [`ErrorKind::InvalidRequest`]: a start of more than one container's
///   ID, or a name `kind` can't take.
/// - Any other error a lookup gave (a store that can't be read).
pub fn inspect_any(engine: &Engine, name: &str, kind: Option<Kind>) -> Result<Inspected> {
    if let Some(kind) = kind {
        return inspect_as(engine, name, kind);
    }
    for kind in Kind::ALL {
        match inspect_as(engine, name, kind) {
            Ok(found) => return Ok(found),
            Err(error) if passed_on(kind, error.kind()) => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::new(
        ErrorKind::NotFound,
        format!("no such object: {name}"),
    ))
}

/// Whether `kind`'s lookup failing with `error` means it hasn't the name:
/// none there; a name it can't take (an image's reference, a volume's
/// name); no networks of this user's.
fn passed_on(kind: Kind, error: ErrorKind) -> bool {
    matches!(
        (kind, error),
        (_, ErrorKind::NotFound)
            | (
                Kind::Image | Kind::Network | Kind::Volume,
                ErrorKind::InvalidRequest
            )
            | (Kind::Network, ErrorKind::NeedsRoot | ErrorKind::Unsupported)
    )
}

/// `name` as `kind`. Images and volumes live in the image store: none
/// there yet, neither has it (and a look never makes the store).
fn inspect_as(engine: &Engine, name: &str, kind: Kind) -> Result<Inspected> {
    let stored = || {
        engine
            .paths()
            .image_store()
            .is_ok_and(std::path::Path::exists)
    };
    if matches!(kind, Kind::Image | Kind::Volume) && !stored() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("no such object: {name}"),
        ));
    }
    Ok(match kind {
        Kind::Container => Inspected::Container(Box::new(super::inspect_container(engine, name)?)),
        Kind::Image => Inspected::Image(Box::new(super::inspect_image(engine, name)?)),
        Kind::Network => Inspected::Network(Box::new(super::inspect_network(engine, name)?)),
        Kind::Volume => Inspected::Volume(Box::new(super::inspect_volume(engine, name)?)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What passes a name on to the next kind, and what stops the look.
    #[test]
    fn a_lookup_passes_on_only_what_isn_t_its() {
        for kind in Kind::ALL {
            assert!(passed_on(kind, ErrorKind::NotFound), "{kind:?}");
            assert!(!passed_on(kind, ErrorKind::Storage), "{kind:?}");
        }
        assert!(
            !passed_on(Kind::Container, ErrorKind::InvalidRequest),
            "an ambiguous container prefix stops it"
        );
        assert!(passed_on(Kind::Image, ErrorKind::InvalidRequest));
        assert!(passed_on(Kind::Volume, ErrorKind::InvalidRequest));
        assert!(passed_on(Kind::Network, ErrorKind::NeedsRoot));
        assert!(passed_on(Kind::Network, ErrorKind::Unsupported));
        assert!(!passed_on(Kind::Volume, ErrorKind::NeedsRoot));
    }
}
