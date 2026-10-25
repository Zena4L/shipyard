//! A credential as a Docker client hands one over: its `AuthConfig` JSON,
//! and that JSON base64url-encoded (`X-Registry-Auth`). The API reads it
//! from Docker's clients; the Mac hands a forwarded `push` or `pull` the
//! Mac's own in the same form (step 12.4, D12.4-2).

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::{Credential, GivenCredential};

/// What isn't Docker's credential form: said without echoing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not Docker's credentials (base64 of its auth JSON)")]
pub struct NotDockerAuth;

/// Docker's `AuthConfig`. No `Debug`: it holds the secrets in clear.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct AuthConfig {
    #[serde(skip_serializing_if = "String::is_empty")]
    username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    password: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    auth: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    serveraddress: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    identitytoken: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    registrytoken: String,
}

impl GivenCredential {
    /// The credentials a pull or a push signs in with: `given`, for its
    /// registry alone, else Docker's config (as a pull's always were).
    pub(crate) fn or_docker(given: Option<Self>) -> shipyard_image::Credentials {
        match given {
            Some(given) => {
                shipyard_image::Credentials::given(given.credential, given.registry.as_deref())
            }
            None => shipyard_image::Credentials::docker(),
        }
    }

    /// The credential `X-Registry-Auth`'s value holds (base64url, or
    /// standard base64, of Docker's JSON), if it holds one (`{}`, or
    /// nothing: none).
    ///
    /// # Errors
    ///
    /// [`NotDockerAuth`].
    pub fn from_docker_auth(text: &str) -> Result<Option<Self>, NotDockerAuth> {
        let text = text.trim().trim_end_matches('=');
        if text.is_empty() {
            return Ok(None);
        }
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text))
            .map_err(|_| NotDockerAuth)?;
        Self::from_auth_json(&json)
    }

    /// The credential Docker's auth JSON holds, if it holds one: a
    /// registry's own token first (as Docker reads it), an identity token,
    /// a username and password, or `auth` (base64 of `user:password`).
    ///
    /// # Errors
    ///
    /// [`NotDockerAuth`]: not its JSON, or an `auth` that isn't base64 of
    /// `user:password`.
    pub fn from_auth_json(json: &[u8]) -> Result<Option<Self>, NotDockerAuth> {
        let config: AuthConfig = serde_json::from_slice(json).map_err(|_| NotDockerAuth)?;
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
            let (username, password) =
                shipyard_image::credentials::basic_from_auth(&config.auth).ok_or(NotDockerAuth)?;
            Credential::Basic { username, password }
        } else {
            return Ok(None);
        };
        Ok(Some(Self {
            credential,
            registry,
        }))
    }

    /// This credential as `X-Registry-Auth`'s value (base64url of
    /// Docker's JSON), as a Docker client hands it in.
    #[must_use]
    pub fn to_docker_auth(&self) -> String {
        let mut config = AuthConfig {
            serveraddress: self.registry.clone().unwrap_or_default(),
            ..AuthConfig::default()
        };
        match &self.credential {
            Credential::Basic { username, password } => {
                config.username.clone_from(username);
                config.password.clone_from(password);
            }
            Credential::IdentityToken(token) => config.identitytoken.clone_from(token),
            Credential::RegistryToken(token) => config.registrytoken.clone_from(token),
        }
        let json = serde_json::to_vec(&config).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }
}

/// How long a helper may take to hand a forwarded command its credential
/// (a Docker Desktop helper with Desktop gone may hang till killed).
const HANDED_HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Docker's credential (its config, its helpers) for the registry of
/// `image`, if it has one: what a Mac hands its forwarded `push` or `pull`
/// (D12.4-2).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): not a
/// reference; [`ErrorKind::Environment`](crate::ErrorKind::Environment):
/// the config or its helper can't be read.
pub fn docker_credential_for(image: &str) -> crate::Result<Option<GivenCredential>> {
    let reference = super::ImageReference::parse(image)?;
    let found = shipyard_image::Credentials::docker()
        .with_helper_timeout(HANDED_HELPER_TIMEOUT)
        .lookup(&reference.registry)
        .map_err(|error| crate::Error::wrap(crate::ErrorKind::Environment, error))?;
    Ok(found.map(|found| GivenCredential {
        credential: found.credential,
        registry: Some(reference.registry),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind of credential, encoded as a client hands it in, reads
    /// back as itself, its registry with it; `{}` and nothing are none;
    /// what isn't Docker's form is refused.
    #[test]
    fn a_credential_goes_and_comes_back_as_docker_s() {
        for credential in [
            Credential::Basic {
                username: "me".to_owned(),
                password: "p:w".to_owned(),
            },
            Credential::IdentityToken("refresh".to_owned()),
            Credential::RegistryToken("bearer".to_owned()),
        ] {
            let given = GivenCredential {
                credential: credential.clone(),
                registry: Some("ghcr.io".to_owned()),
            };
            let back = GivenCredential::from_docker_auth(&given.to_docker_auth())
                .unwrap()
                .unwrap();
            assert_eq!(back.credential, credential);
            assert_eq!(back.registry.as_deref(), Some("ghcr.io"));
        }
        assert!(GivenCredential::from_docker_auth("").unwrap().is_none());
        let empty = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        assert!(GivenCredential::from_docker_auth(&empty).unwrap().is_none());
        assert_eq!(
            GivenCredential::from_docker_auth("!!").map(|_| ()),
            Err(NotDockerAuth)
        );
    }
}
