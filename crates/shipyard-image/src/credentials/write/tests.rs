#![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// A throwaway directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-keep-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn config_path(&self) -> PathBuf {
        self.0.join("docker").join("config.json")
    }

    /// Credentials at this config (written first, unless `None`), helpers
    /// from this directory.
    fn credentials(&self, config: Option<&str>) -> Credentials {
        if let Some(config) = config {
            std::fs::create_dir_all(self.config_path().parent().unwrap()).unwrap();
            std::fs::write(self.config_path(), config).unwrap();
        }
        Credentials::at(self.config_path()).with_helper_dir(&self.0)
    }

    /// A helper `name` that logs its action and input to `helper.log`.
    fn logging_helper(&self, name: &str) -> PathBuf {
        let log = self.0.join("helper.log");
        let path = self.0.join(format!("docker-credential-{name}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{0}'\ncat >> '{0}'\necho >> '{0}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    fn config(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.config_path()).unwrap()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn basic(password: &str) -> Credential {
    Credential::Basic {
        username: "me".to_owned(),
        password: password.to_owned(),
    }
}

/// A sign-in in the file: every other key kept as it was, the registry's
/// other spellings gone, read back as the new one.
#[test]
fn a_sign_in_keeps_the_rest_of_the_config() {
    let scratch = Scratch::new();
    let credentials = scratch.credentials(Some(
        r#"{"auths":{"https://ghcr.io":{"auth":"b2xkOm9sZA=="},"quay.io":{"auth":"cTpx"}},
            "currentContext":"desktop","proxies":{"default":{"httpProxy":"http://p"}}}"#,
    ));
    let kept = credentials.keep("ghcr.io", &basic("pw")).unwrap();
    assert_eq!(kept, Kept::File(scratch.config_path()));
    let config = scratch.config();
    assert_eq!(config["currentContext"], "desktop");
    assert_eq!(config["proxies"]["default"]["httpProxy"], "http://p");
    assert_eq!(config["auths"]["quay.io"]["auth"], "cTpx");
    assert!(config["auths"].get("https://ghcr.io").is_none(), "{config}");
    assert_eq!(
        credentials.lookup("ghcr.io").unwrap().unwrap().credential,
        basic("pw")
    );
}

/// With a helper: the secret goes to its `store` on its input, the file
/// keeps `{}` for the registry (no plain-text `auth` left); a sign-out
/// asks it to `erase`, and removes the file's.
#[test]
fn a_helper_keeps_and_forgets_it() {
    let scratch = Scratch::new();
    let log = scratch.logging_helper("fake");
    let credentials = scratch.credentials(Some(
        r#"{"credsStore":"fake","auths":{"ghcr.io":{"auth":"b2xkOm9sZA=="}}}"#,
    ));
    let kept = credentials.keep("ghcr.io", &basic("s3cret")).unwrap();
    assert_eq!(kept, Kept::Helper("fake".to_owned()));
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.starts_with("store\n"), "{logged}");
    assert!(logged.contains(r#""Secret":"s3cret""#), "{logged}");
    assert!(logged.contains(r#""ServerURL":"ghcr.io""#), "{logged}");
    assert_eq!(scratch.config()["auths"]["ghcr.io"], serde_json::json!({}));
    let forgotten = credentials.forget("https://ghcr.io/").unwrap();
    assert!(forgotten.removed && forgotten.helper_failed.is_none());
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("erase\nghcr.io")
    );
    assert!(scratch.config()["auths"].get("ghcr.io").is_none());
}

/// A sign-out removes every key naming the registry; none there: nothing
/// removed, the file untouched.
#[test]
fn a_sign_out_removes_every_spelling() {
    let scratch = Scratch::new();
    let credentials = scratch.credentials(Some(
        r#"{"auths":{"ghcr.io":{"auth":"bWU6cHc="},"https://GHCR.io/":{"auth":"bWU6cHc="},"quay.io":{}}}"#,
    ));
    assert!(credentials.forget("ghcr.io").unwrap().removed);
    let auths = scratch.config()["auths"].clone();
    assert_eq!(auths, serde_json::json!({"quay.io": {}}));
    let before = std::fs::read(scratch.config_path()).unwrap();
    assert!(!credentials.forget("ghcr.io").unwrap().removed);
    assert_eq!(std::fs::read(scratch.config_path()).unwrap(), before);
}

/// A config that isn't Docker's JSON is refused and left as it was; a
/// registry's own token isn't kept.
#[test]
fn a_broken_config_is_left_as_it_is() {
    let scratch = Scratch::new();
    for broken in ["{not json", "[1,2]", r#"{"auths":[]}"#] {
        let credentials = scratch.credentials(Some(broken));
        assert!(
            matches!(
                credentials.keep("ghcr.io", &basic("pw")),
                Err(CredentialError::Config { .. })
            ),
            "{broken}"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.config_path()).unwrap(),
            broken
        );
    }
    let credentials = scratch.credentials(Some("{}"));
    assert!(matches!(
        credentials.keep("ghcr.io", &Credential::RegistryToken("t".to_owned())),
        Err(CredentialError::Config { .. })
    ));
}

/// A config that's a link stays one, its target written; the old file's
/// mode is kept; a new file is `0600` in a `0700` directory.
#[test]
fn the_file_keeps_its_link_and_mode() {
    let scratch = Scratch::new();
    let target = scratch.0.join("dotfiles").join("docker.json");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, r#"{"currentContext":"x"}"#).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::create_dir_all(scratch.config_path().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, scratch.config_path()).unwrap();
    let credentials = scratch.credentials(None);
    credentials.keep("ghcr.io", &basic("pw")).unwrap();
    let link = std::fs::symlink_metadata(scratch.config_path()).unwrap();
    assert!(link.file_type().is_symlink());
    let written = std::fs::metadata(&target).unwrap();
    assert_eq!(written.permissions().mode() & 0o777, 0o640);
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("ghcr.io")
    );

    let fresh = Scratch::new();
    let credentials = fresh.credentials(None);
    credentials.keep("ghcr.io", &basic("pw")).unwrap();
    let file = std::fs::metadata(fresh.config_path()).unwrap();
    let dir = std::fs::metadata(fresh.config_path().parent().unwrap()).unwrap();
    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

/// A temporary a killed write left long ago is removed by the next write;
/// a fresh one (another write's, perhaps under way) is left.
#[test]
fn a_killed_write_s_temporary_is_swept() {
    let scratch = Scratch::new();
    let credentials = scratch.credentials(Some("{}"));
    let dir = scratch.config_path().parent().unwrap().to_owned();
    let old = dir.join(".config.json.4242.0.tmp");
    let young = dir.join(".config.json.4243.0.tmp");
    for left in [&old, &young] {
        std::fs::write(left, "secret").unwrap();
    }
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - LEFT_BEHIND * 2)
        .unwrap();
    credentials.keep("ghcr.io", &basic("pw")).unwrap();
    assert!(!old.exists(), "the old one swept");
    assert!(young.exists(), "the fresh one left");
}

/// A FIFO at the config's place is refused at once, never waited on (a
/// reader with no writer would wait for ever), by a lookup and a sign-in
/// alike; a config that's a link is read through it.
#[test]
fn a_config_is_read_bounded_through_its_link() {
    let scratch = Scratch::new();
    std::fs::create_dir_all(scratch.config_path().parent().unwrap()).unwrap();
    nix::unistd::mkfifo(&scratch.config_path(), nix::sys::stat::Mode::S_IRWXU).unwrap();
    let credentials = scratch.credentials(None);
    assert!(matches!(
        credentials.lookup("ghcr.io"),
        Err(CredentialError::Read { .. })
    ));
    assert!(matches!(
        credentials.keep("ghcr.io", &basic("pw")),
        Err(CredentialError::Read { .. })
    ));
    std::fs::remove_file(scratch.config_path()).unwrap();
    let target = scratch.0.join("elsewhere.json");
    std::fs::write(&target, r#"{"auths":{"ghcr.io":{"auth":"bWU6cHc="}}}"#).unwrap();
    std::os::unix::fs::symlink(&target, scratch.config_path()).unwrap();
    assert_eq!(
        credentials.lookup("ghcr.io").unwrap().unwrap().credential,
        basic("pw")
    );
}

/// As root (`sudo shipyard login` keeping the user's `HOME`), a user's
/// config stays the user's: its owner kept, not made root's.
#[test]
fn a_config_keeps_its_owner() {
    use std::os::unix::fs::MetadataExt as _;
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let scratch = Scratch::new();
    let credentials = scratch.credentials(Some("{}"));
    std::os::unix::fs::chown(scratch.config_path(), Some(65534), Some(65534)).unwrap();
    credentials.keep("ghcr.io", &basic("pw")).unwrap();
    let written = std::fs::metadata(scratch.config_path()).unwrap();
    assert_eq!((written.uid(), written.gid()), (65534, 65534));
    assert!(
        scratch.config()["auths"].get("ghcr.io").is_some(),
        "written"
    );
}
