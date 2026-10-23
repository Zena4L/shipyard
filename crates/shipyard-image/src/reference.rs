//! Image references: turning `alpine`, `alpine:3.20`, `ghcr.io/owner/app@sha256:…`
//! into a registry, a repository path, and the tag or digest to fetch.
//!
//! A reference is `[registry[:port]/]repository[:tag][@digest]`. The rules
//! that make a bare `alpine` mean Docker Hub's `library/alpine:latest` are
//! Docker's, and every client reimplements them:
//!
//! - if the part before the first `/` has no `.` or `:` and isn't
//!   `localhost`, there's no registry: it's all repository, and the registry
//!   is Docker Hub (`docker.io`; `index.docker.io` and `registry-1.docker.io`
//!   are other names for it);
//! - on Docker Hub a single-segment repository (`alpine`) is an official
//!   image, so `library/` is prepended (`library/alpine`);
//! - with neither a tag nor a digest, the tag is `latest`;
//! - a digest, if present, is what's actually fetched (it pins the content);
//!   a tag is a mutable name the registry resolves.
//!
//! Every part is checked against the distribution spec's grammar: a name
//! goes into the registry's URLs as it is, so nothing in it may change what
//! they mean (`..`, `?`, `#`, spaces). A reference displays in one canonical
//! form (`docker.io/library/alpine:3.20`), which is how the store's index
//! names images: `alpine:3.20` and `docker.io/library/alpine:3.20` are the
//! same image.

use std::fmt;

use crate::digest::{Digest, DigestError};

/// Docker Hub's canonical name in a reference.
pub(crate) const DOCKER_HUB_NAME: &str = "docker.io";
/// Other names Docker Hub goes by.
const DOCKER_HUB_ALIASES: [&str; 2] = ["index.docker.io", DOCKER_HUB_HOST];
/// The host that actually serves Docker Hub's registry API.
const DOCKER_HUB_HOST: &str = "registry-1.docker.io";
/// The namespace official (single-name) Docker Hub images live under.
const DOCKER_HUB_LIBRARY: &str = "library";
/// The tag used when a reference names neither a tag nor a digest.
const DEFAULT_TAG: &str = "latest";
/// The longest repository path (the distribution spec's limit on a name).
const MAX_NAME: usize = 255;
/// The longest tag.
const MAX_TAG: usize = 128;
// The tag error spells it out.
const _: () = assert!(MAX_TAG == 128);

/// A fully resolved image reference: which registry, what repository, and
/// which tag and/or digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageReference {
    /// The registry's name (and port, if any), lowercase, Docker Hub as
    /// `docker.io`. Where to connect is [`host`](Self::host).
    pub registry: String,
    /// The repository path, for example `library/alpine` or `owner/app`.
    pub repository: String,
    /// The tag, if one was given (or `latest`, if neither a tag nor a
    /// digest was).
    pub tag: Option<String>,
    /// The digest, if one was given: then it's what's fetched.
    pub digest: Option<Digest>,
}

/// Why an image reference couldn't be parsed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReferenceError {
    /// The reference was empty.
    #[error("an image reference cannot be empty")]
    Empty,
    /// The repository part was empty (for example `alpine:` or `:tag`).
    #[error("the image reference `{0}` has no repository")]
    NoRepository(String),
    /// The `@digest` part wasn't a valid digest.
    #[error("the digest in `{reference}`: {source}")]
    BadDigest {
        /// The whole reference, for context.
        reference: String,
        /// Why the digest was invalid.
        source: DigestError,
    },
    /// A repository with capital letters (which the spec doesn't allow).
    #[error("the repository in `{0}` must be lowercase")]
    Uppercase(String),
    /// A part that doesn't follow the grammar.
    #[error("`{reference}` isn't a valid image reference: {why}")]
    Invalid {
        /// The whole reference.
        reference: String,
        /// What's wrong with it.
        why: &'static str,
    },
}

impl ImageReference {
    /// Parses an image reference, applying Docker Hub's defaults.
    ///
    /// # Errors
    ///
    /// Returns a [`ReferenceError`] if the reference is empty, has no
    /// repository, has a part outside the grammar, or carries a `@digest`
    /// that isn't a valid digest.
    pub fn parse(input: &str) -> Result<Self, ReferenceError> {
        if input.is_empty() {
            return Err(ReferenceError::Empty);
        }
        let invalid = |why| ReferenceError::Invalid {
            reference: input.to_owned(),
            why,
        };

        // `@digest` first: it can only be at the very end, and the digest's
        // own colon (`sha256:…`) must not be mistaken for a tag's.
        let (name_and_tag, digest) = match input.split_once('@') {
            Some((head, digest_text)) => {
                let digest =
                    Digest::parse(digest_text).map_err(|source| ReferenceError::BadDigest {
                        reference: input.to_owned(),
                        source,
                    })?;
                (head, Some(digest))
            }
            None => (input, None),
        };

        // Does the first path segment look like a registry? It does if it
        // contains `.` or `:` (a domain or a port), starts with `[` (an IPv6
        // address), or is exactly `localhost`.
        let (registry, remainder) = match name_and_tag.split_once('/') {
            Some((head, rest))
                if head == "localhost"
                    || head.contains('.')
                    || head.contains(':')
                    || head.starts_with('[') =>
            {
                (
                    registry_name(head)
                        .ok_or_else(|| invalid("its registry isn't a host[:port]"))?,
                    rest,
                )
            }
            _ => (DOCKER_HUB_NAME.to_owned(), name_and_tag),
        };

        // The tag is a `:` in the *remainder* (after any registry was removed),
        // so a registry port (`host:5000/repo`) is never read as a tag.
        let (repository, tag) = match remainder.rsplit_once(':') {
            Some((repository, tag)) if !tag.contains('/') => (repository, Some(tag)),
            _ => (remainder, None),
        };
        if repository.is_empty() {
            return Err(ReferenceError::NoRepository(input.to_owned()));
        }
        if repository.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(ReferenceError::Uppercase(input.to_owned()));
        }
        if !repository.split('/').all(valid_component) {
            return Err(invalid(
                "its repository path has a part that isn't lowercase letters and digits \
                 separated by `.`, `_`, `__` or `-`",
            ));
        }
        if let Some(tag) = tag
            && !valid_tag(tag)
        {
            return Err(invalid(
                "its tag must be up to 128 letters, digits, `_`, `.` and `-`, not starting \
                 with `.` or `-`",
            ));
        }

        let repository = if registry == DOCKER_HUB_NAME && !repository.contains('/') {
            format!("{DOCKER_HUB_LIBRARY}/{repository}")
        } else {
            repository.to_owned()
        };
        // Measured as stored, `library/` included: a name that only fits
        // without it would be refused when read back in its canonical form.
        if repository.len() > MAX_NAME {
            return Err(invalid("its repository path is longer than 255 bytes"));
        }
        let tag = match (tag, &digest) {
            (Some(tag), _) => Some(tag.to_owned()),
            (None, None) => Some(DEFAULT_TAG.to_owned()),
            (None, Some(_)) => None,
        };
        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// The host (and port) to connect to: the registry, but Docker Hub's API
    /// is served from `registry-1.docker.io`.
    #[must_use]
    pub fn host(&self) -> &str {
        api_host(&self.registry)
    }

    /// The repository as Docker shows it: Docker Hub's registry left out,
    /// and its `library/` too (`alpine`, `owner/app`, `ghcr.io/owner/app`).
    #[must_use]
    pub fn familiar_name(&self) -> String {
        if self.registry != DOCKER_HUB_NAME {
            return format!("{}/{}", self.registry, self.repository);
        }
        self.repository
            .strip_prefix(&format!("{DOCKER_HUB_LIBRARY}/"))
            .unwrap_or(&self.repository)
            .to_owned()
    }

    /// The reference as Docker shows it ([`familiar_name`](Self::familiar_name),
    /// then its tag and digest): `alpine:3.20`.
    #[must_use]
    pub fn familiar(&self) -> String {
        let mut shown = self.familiar_name();
        if let Some(tag) = &self.tag {
            shown.push(':');
            shown.push_str(tag);
        }
        if let Some(digest) = &self.digest {
            shown.push('@');
            shown.push_str(&digest.to_string());
        }
        shown
    }

    /// What goes in the registry URL's `manifests/<here>`: the digest if
    /// there is one, else the tag.
    #[must_use]
    pub fn manifest_path(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(digest), _) => digest.to_string(),
            (None, Some(tag)) => tag.clone(),
            (None, None) => DEFAULT_TAG.to_owned(),
        }
    }
}

impl fmt::Display for ImageReference {
    /// The canonical form: `registry/repository[:tag][@digest]`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(formatter, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(formatter, "@{digest}")?;
        }
        Ok(())
    }
}

/// A registry's canonical name, if `text` is a `host[:port]`: a domain (or
/// `localhost`) or a bracketed IPv6 address, lowercased, Docker Hub's
/// aliases as `docker.io`.
fn registry_name(text: &str) -> Option<String> {
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (address, after) = rest.split_once(']')?;
        address.parse::<std::net::Ipv6Addr>().ok()?;
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?)
        };
        (&text[..=address.len() + 1], port)
    } else {
        match text.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    if let Some(port) = port
        && (!port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err())
    {
        return None;
    }
    if !host.starts_with('[')
        && !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
    {
        return None;
    }
    let name = text.to_ascii_lowercase();
    if DOCKER_HUB_ALIASES.contains(&name.as_str()) {
        Some(DOCKER_HUB_NAME.to_owned())
    } else {
        Some(name)
    }
}

/// Whether `component` is one repository path component:
/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn valid_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    let alphanumeric = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let mut at = 0;
    let mut expect_word = true;
    while at < bytes.len() {
        if expect_word {
            let start = at;
            while at < bytes.len() && alphanumeric(&bytes[at]) {
                at += 1;
            }
            if at == start {
                return false;
            }
        } else {
            let separator = &bytes[at..];
            let length = if separator.starts_with(b"__") {
                2
            } else if separator[0] == b'.' || separator[0] == b'_' {
                1
            } else if separator[0] == b'-' {
                separator.iter().take_while(|&&b| b == b'-').count()
            } else {
                return false;
            };
            at += length;
        }
        expect_word = !expect_word;
    }
    // Ends on a word, never on a separator.
    !bytes.is_empty() && !expect_word
}

/// Whether `tag` is a tag: `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
fn valid_tag(tag: &str) -> bool {
    let mut bytes = tag.bytes();
    tag.len() <= MAX_TAG
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// The host (and port) the registry `registry` serves its API from: its
/// own, but Docker Hub's (`docker.io`) is `registry-1.docker.io`.
pub(crate) fn api_host(registry: &str) -> &str {
    if registry == DOCKER_HUB_NAME {
        DOCKER_HUB_HOST
    } else {
        registry
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;

    const D: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn parse(input: &str) -> ImageReference {
        ImageReference::parse(input).unwrap()
    }

    #[test]
    fn docker_hub_defaults_apply() {
        let r = parse("alpine");
        assert_eq!(r.to_string(), "docker.io/library/alpine:latest");
        assert_eq!(r.host(), "registry-1.docker.io");
        assert_eq!(r.manifest_path(), "latest");
        assert_eq!(parse("owner/app:v1").to_string(), "docker.io/owner/app:v1");
    }

    #[test]
    fn docker_hubs_other_names_are_docker_hub() {
        for input in [
            "docker.io/alpine:3.20",
            "index.docker.io/alpine:3.20",
            "registry-1.docker.io/library/alpine:3.20",
            "Docker.IO/alpine:3.20",
        ] {
            assert_eq!(
                parse(input).to_string(),
                "docker.io/library/alpine:3.20",
                "{input}"
            );
        }
    }

    #[test]
    fn registries_ports_and_digests() {
        let r = parse("localhost:5000/team/app:v2");
        assert_eq!(
            (r.registry.as_str(), r.host()),
            ("localhost:5000", "localhost:5000")
        );
        assert_eq!(r.repository, "team/app");
        assert_eq!(parse("[::1]:5000/app").to_string(), "[::1]:5000/app:latest");
        let pinned = parse(&format!("ghcr.io/owner/app@{D}"));
        assert_eq!(pinned.tag, None);
        assert_eq!(pinned.manifest_path(), D);
        assert_eq!(pinned.to_string(), format!("ghcr.io/owner/app@{D}"));
        let both = parse(&format!("app:v1@{D}"));
        assert_eq!(both.tag.as_deref(), Some("v1"));
        assert_eq!(both.manifest_path(), D, "the digest is what's fetched");
        assert_eq!(both.to_string(), format!("docker.io/library/app:v1@{D}"));
    }

    #[test]
    fn what_the_grammar_refuses_is_refused() {
        for input in [
            "alpine:",
            "alpine:.hidden",
            "alpine:with space",
            "alpine:a?b",
            "alp?ine",
            "alpine#x",
            "a//b",
            "a/../b",
            "-alpine",
            "alpine-",
            "a..b",
            "host:99999/app",
            "bad_host.com:x/app",
            "[zz]:5000/app",
            "[:::]/app",
            "[::1]x/app",
            "[::1]:/app",
        ] {
            assert!(ImageReference::parse(input).is_err(), "{input}");
        }
        assert!(matches!(
            ImageReference::parse("Alpine"),
            Err(ReferenceError::Uppercase(_))
        ));
        assert!(ImageReference::parse(&format!("a:{}", "t".repeat(129))).is_err());
        assert!(ImageReference::parse(&"a".repeat(256)).is_err());
        assert!(matches!(
            ImageReference::parse(":tag"),
            Err(ReferenceError::NoRepository(_))
        ));
        assert_eq!(ImageReference::parse(""), Err(ReferenceError::Empty));
    }

    #[test]
    fn separators_the_grammar_allows() {
        for input in ["a.b", "a_b", "a__b", "a-b", "a---b", "a1/b2.c_d"] {
            assert!(ImageReference::parse(input).is_ok(), "{input}");
        }
    }

    /// The longest name is counted as it's stored: a Hub name gets
    /// `library/` first, so one that fits only without it is refused, and
    /// whatever is accepted reads back the same. (Found by the `reference`
    /// fuzz target.)
    #[test]
    fn the_name_length_limit_counts_library() {
        let longest = "a".repeat(MAX_NAME - "library/".len());
        let reference = ImageReference::parse(&longest).unwrap();
        assert_eq!(
            ImageReference::parse(&reference.to_string()).unwrap(),
            reference
        );
        assert!(ImageReference::parse(&format!("{longest}a")).is_err());
        assert!(ImageReference::parse(&format!("x/{}", "a".repeat(MAX_NAME - 2))).is_ok());
    }

    proptest::proptest! {
        /// Whatever parses displays as something that parses back the same.
        #[test]
        fn the_canonical_form_round_trips(input in "[a-z0-9./:@_-]{1,40}") {
            if let Ok(reference) = ImageReference::parse(&input) {
                proptest::prop_assert_eq!(
                    ImageReference::parse(&reference.to_string()).unwrap(),
                    reference
                );
            }
        }
    }

    /// Docker's familiar form: Docker Hub and its `library/` left out,
    /// any other registry kept.
    #[test]
    fn a_reference_is_shown_as_docker_shows_it() {
        let familiar = |text: &str| ImageReference::parse(text).unwrap().familiar();
        assert_eq!(familiar("alpine"), "alpine:latest");
        assert_eq!(familiar("docker.io/library/alpine:3.20"), "alpine:3.20");
        assert_eq!(familiar("owner/app:v1"), "owner/app:v1");
        assert_eq!(familiar("ghcr.io/owner/app:v1"), "ghcr.io/owner/app:v1");
        assert_eq!(
            familiar("localhost:5000/library/x"),
            "localhost:5000/library/x:latest"
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            familiar(&format!("alpine@{digest}")),
            format!("alpine@{digest}")
        );
        assert_eq!(
            ImageReference::parse("alpine:3.20")
                .unwrap()
                .familiar_name(),
            "alpine"
        );
    }
}
