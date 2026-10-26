//! `shipyard login` and `logout` (step 12.4): a sign-in is checked against
//! the registry first (as `docker login` asks its daemon to), then kept
//! where Docker keeps it (`Credentials::keep`, D12.4-1); a sign-out
//! removes it.

use shipyard_image::Credentials;

use super::{GivenCredential, Login};
use crate::error::{Context as _, Result};

/// Where a sign-in was kept ([`login_and_keep`]).
pub use shipyard_image::credentials::Kept;

/// What a sign-out did ([`logout`]).
pub use shipyard_image::credentials::Forgotten;

/// What `shipyard login` came to ([`login_and_keep`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signed {
    /// Taken by the registry, and kept there.
    Kept(Kept),
    /// Refused by the registry: why, in words that never hold the secret.
    Refused(String),
}

/// `shipyard login`: `given` checked against its registry (as
/// [`login`](super::login) checks it), then kept where Docker keeps it
/// (D12.4-1). A refusal keeps nothing.
///
/// # Errors
///
/// As [`login`](super::login)'s; the credential couldn't be kept
/// ([`ErrorKind::Environment`](crate::ErrorKind::Environment)).
pub fn login_and_keep(given: GivenCredential) -> Result<Signed> {
    let server = given.registry.clone();
    let credential = given.credential.clone();
    match super::login(given)? {
        Login::Refused(why) => Ok(Signed::Refused(why)),
        Login::Succeeded => {
            let registry = server
                .as_deref()
                .unwrap_or(shipyard_image::reference::DOCKER_HUB_NAME);
            Credentials::docker()
                .keep(registry, &credential)
                .map(Signed::Kept)
                .map_err(|error| crate::Error::wrap(crate::ErrorKind::Environment, error))
                .context("keeping the sign-in")
        }
    }
}

/// `shipyard logout`: the sign-in for `server` (Docker Hub if none)
/// removed where Docker keeps it.
///
/// # Errors
///
/// The config can't be read or written
/// ([`ErrorKind::Environment`](crate::ErrorKind::Environment)).
pub fn logout(server: Option<&str>) -> Result<Forgotten> {
    let registry = server
        .filter(|server| !server.trim().is_empty())
        .unwrap_or(shipyard_image::reference::DOCKER_HUB_NAME);
    Credentials::docker()
        .forget(registry)
        .map_err(|error| crate::Error::wrap(crate::ErrorKind::Environment, error))
        .context("removing the sign-in")
}
