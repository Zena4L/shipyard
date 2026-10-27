//! A build request checked (before anything runs), and `buildctl`'s
//! arguments made from it: argv, each an argument of its own, never read by
//! a shell.

use shipyard_image::ImageReference;

use super::{BuildRequest, MAX_EACH, Output};
use crate::error::{Result, bail};

/// Where the builder sees what a build gives it.
pub(super) mod places {
    /// The context, read-only.
    pub(in crate::build) const CONTEXT: &str = "/context";
    /// This build's `in/`, read-only: the Dockerfile and its
    /// `.dockerignore` (BuildKit's `dockerfile` local: a frontend reads it
    /// whole).
    pub(in crate::build) const IN: &str = "/in";
    /// This build's secrets, read-only: apart from `in/`, so no frontend
    /// syncs them as the Dockerfile's files, nor keeps them in the cache.
    pub(in crate::build) const SECRETS: &str = "/secrets";
    /// This build's `out/`: what it writes.
    pub(in crate::build) const OUT: &str = "/out";
    /// The context's cache: BuildKit's root.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a build runs on Linux only")
    )]
    pub(in crate::build) const STATE: &str = "/var/lib/buildkit";
    /// This build's OCI layout of the store's images, read-only
    /// ([`local`](crate::build::local)).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a build runs on Linux only")
    )]
    pub(in crate::build) const LAYOUT: &str = "/layout";
    /// The Dockerfile's name in `in/`.
    pub(in crate::build) const DOCKERFILE: &str = "Dockerfile";
}

/// What `request` is checked for, before anything runs: its names (each a
/// reference with no digest, canonical; at least one unless `-o` says where
/// its files go: D12.3-2), its bounds, its secrets' IDs, its platform (this
/// host's). The names.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest) for each.
pub(super) fn checked(request: &BuildRequest) -> Result<Vec<ImageReference>> {
    if !request.tags.is_empty() && request.output.is_some() {
        bail!(
            InvalidRequest,
            "-t names an image and -o writes its files instead: give one of them"
        );
    }
    if request.tags.is_empty() && request.output.is_none() {
        bail!(
            InvalidRequest,
            "give the image a name: shipyard build -t NAME[:TAG] … (Shipyard keeps no untagged \
             images), or -o to write its files"
        );
    }
    let mut names = Vec::new();
    for tag in &request.tags {
        let name = ImageReference::parse(tag)?;
        if name.digest.is_some() {
            bail!(InvalidRequest, "a build's name can't have a digest: {tag}");
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for (what, count) in [
        ("build args", request.build_args.len()),
        ("labels", request.labels.len()),
        ("secrets", request.secrets.len()),
        ("names", request.tags.len()),
    ] {
        if count > MAX_EACH {
            bail!(InvalidRequest, "more than {MAX_EACH} {what}");
        }
    }
    for (key, _) in &request.build_args {
        if key.is_empty() || key.contains('=') {
            bail!(InvalidRequest, "a build arg's name can't be {key:?}");
        }
    }
    for (key, _) in &request.labels {
        if key.is_empty() || key.contains('=') {
            bail!(InvalidRequest, "a label's name can't be {key:?}");
        }
    }
    for secret in &request.secrets {
        let plain = !secret.id.is_empty()
            && secret.id.len() <= 128
            && secret
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            && !matches!(secret.id.as_str(), "." | "..");
        if !plain {
            bail!(
                InvalidRequest,
                "a secret's id is letters, digits, '.', '_' and '-': not {:?}",
                secret.id
            );
        }
        if request
            .secrets
            .iter()
            .filter(|other| other.id == secret.id)
            .count()
            > 1
        {
            bail!(InvalidRequest, "two secrets with the id {:?}", secret.id);
        }
    }
    if let Some(platform) = &request.platform {
        crate::image::platform_here(platform, "built")?;
    }
    if let Some(target) = &request.target
        && target.is_empty()
    {
        bail!(InvalidRequest, "--target needs a stage's name");
    }
    Ok(names)
}

/// `buildctl build`'s arguments for `request`, its image named `names`.
#[must_use]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a build runs on Linux only")
)]
pub(super) fn buildctl_args(request: &BuildRequest, names: &[ImageReference]) -> Vec<String> {
    use places::{CONTEXT, DOCKERFILE, IN, OUT, SECRETS};
    let mut args: Vec<String> = [
        "build",
        "--frontend",
        "dockerfile.v0",
        "--local",
        &format!("context={CONTEXT}"),
        "--local",
        &format!("dockerfile={IN}"),
        "--opt",
        &format!("filename={DOCKERFILE}"),
        "--progress",
        "plain",
    ]
    .iter()
    .map(|&arg| arg.to_owned())
    .collect();
    let mut opt = |value: String| {
        args.push("--opt".to_owned());
        args.push(value);
    };
    if let Some(target) = &request.target {
        opt(format!("target={target}"));
    }
    for (key, value) in &request.build_args {
        opt(format!("build-arg:{key}={value}"));
    }
    for (key, value) in &request.labels {
        opt(format!("label:{key}={value}"));
    }
    if request.pull {
        opt("image-resolve-mode=pull".to_owned());
    }
    // The platform asked for (checked to run here: `checked`), as BuildKit
    // names it; its `RUN` steps run through Rosetta's `binfmt_misc` entry.
    if let Some(platform) = request
        .platform
        .as_deref()
        .and_then(|asked| crate::image::named_in(asked, &crate::image::platforms_here()))
    {
        opt(format!("platform={platform}"));
    }
    if request.no_cache {
        args.push("--no-cache".to_owned());
    }
    for secret in &request.secrets {
        args.push("--secret".to_owned());
        args.push(format!("id={},src={SECRETS}/{}", secret.id, secret.id));
    }
    args.push("--output".to_owned());
    args.push(match &request.output {
        // Either form comes as a tar: what's in it is unpacked as the
        // caller's own (a build's files may belong to IDs the caller can't
        // read on the host).
        Some(Output::Local(_) | Output::Tar) => format!("type=tar,dest={OUT}/files.tar"),
        None => {
            // CSV: the names quoted, as one field (a name holds no `"`).
            let joined = names
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!("type=docker,\"name={joined}\",dest={OUT}/image.tar")
        }
    });
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Output, Secret};

    /// A request's flags, each its own argument for `buildctl` (never a
    /// shell's words): the names quoted as one field of the output's CSV.
    #[test]
    fn a_request_is_buildctl_s_arguments() {
        let mut request = BuildRequest::new("/src");
        request.tags = vec!["app:1".to_owned(), "registry.example/app:latest".to_owned()];
        request.build_args = vec![("VERSION".to_owned(), "1; rm -rf /".to_owned())];
        request.labels = vec![("org.example".to_owned(), "x y".to_owned())];
        request.target = Some("final".to_owned());
        request.no_cache = true;
        request.pull = true;
        request.secrets = vec![Secret {
            id: "token".to_owned(),
            src: "/home/me/token".into(),
        }];
        let names = checked(&request).unwrap();
        let args = buildctl_args(&request, &names);
        let has = |pair: [&str; 2]| args.windows(2).any(|w| w == pair);
        assert!(has(["--opt", "build-arg:VERSION=1; rm -rf /"]), "{args:?}");
        assert!(has(["--opt", "label:org.example=x y"]));
        assert!(has(["--opt", "target=final"]));
        assert!(has(["--opt", "image-resolve-mode=pull"]));
        // A platform asked for reaches BuildKit as it's named here.
        let mut other = request.clone();
        other.platform = Some("linux".to_owned());
        let args = buildctl_args(&other, &names);
        let named = crate::image::platforms_here()[0].to_string();
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--opt" && pair[1] == format!("platform={named}")),
            "{args:?}"
        );
        assert!(has(["--secret", "id=token,src=/secrets/token"]));
        assert!(args.contains(&"--no-cache".to_owned()));
        assert_eq!(
            args.last().unwrap(),
            "type=docker,\"name=docker.io/library/app:1,registry.example/app:latest\",dest=/out/image.tar"
        );
        request.output = Some(Output::Local("/tmp/x".into()));
        assert_eq!(
            buildctl_args(&request, &names).last().unwrap(),
            "type=tar,dest=/out/files.tar"
        );
    }

    /// A request with nothing set asks `buildctl` for none of the options
    /// (each one only when asked).
    #[test]
    fn options_are_given_only_when_asked() {
        let mut request = BuildRequest::new("/src");
        request.tags = vec!["app:1".to_owned()];
        let names = checked(&request).unwrap();
        let args = buildctl_args(&request, &names);
        for absent in ["--no-cache", "--secret"] {
            assert!(!args.iter().any(|arg| arg == absent), "{absent}: {args:?}");
        }
        for prefix in ["target=", "build-arg:", "label:", "image-resolve-mode="] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(prefix)),
                "{prefix}: {args:?}"
            );
        }
    }

    /// What's refused before anything runs, by kind; each control taken.
    #[test]
    fn what_a_build_refuses_before_it_runs() {
        let refused = |request: &BuildRequest| {
            matches!(
                checked(request).map_err(|error| error.kind()),
                Err(crate::ErrorKind::InvalidRequest)
            )
        };
        let named = || {
            let mut request = BuildRequest::new("/src");
            request.tags = vec!["app:1".to_owned()];
            request
        };
        assert!(!refused(&named()), "the control");
        assert!(refused(&BuildRequest::new("/src")), "no name, no -o");
        let mut files = BuildRequest::new("/src");
        files.output = Some(Output::Tar);
        assert!(!refused(&files), "no name, but -o");
        let mut both = named();
        both.output = Some(Output::Tar);
        assert!(refused(&both), "-t and -o");
        let mut digest = named();
        digest.tags = vec![format!("app@sha256:{}", "a".repeat(64))];
        assert!(refused(&digest));
        let mut arg = named();
        arg.build_args = vec![(String::new(), "x".to_owned())];
        assert!(refused(&arg));
        for id in ["../x", "a/b", "", "a b", ".."] {
            let mut secret = named();
            secret.secrets = vec![Secret {
                id: id.to_owned(),
                src: "/x".into(),
            }];
            assert!(refused(&secret), "{id:?}");
        }
        let mut two = named();
        two.secrets = vec![
            Secret {
                id: "a".to_owned(),
                src: "/x".into(),
            },
            Secret {
                id: "a".to_owned(),
                src: "/y".into(),
            },
        ];
        assert!(refused(&two));
        let mut many = named();
        many.labels = vec![("k".to_owned(), String::new()); MAX_EACH + 1];
        assert!(refused(&many));
        let mut platform = named();
        platform.platform = Some("linux/s390x".to_owned());
        assert!(refused(&platform));
        let mut host = named();
        host.platform = Some(format!(
            "linux/{}",
            shipyard_image::Platform::host().architecture
        ));
        assert!(!refused(&host));
    }
}
