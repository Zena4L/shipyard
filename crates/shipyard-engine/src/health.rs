//! Health checks (stage 9, step 9.6b): a command run in a running
//! container now and then, whose exit code says whether the container is
//! well (`0`) or not (anything else), as Docker's `HEALTHCHECK`.
//!
//! Here, what's the same on every system: what a check is
//! ([`HealthCheck`], kept in the container's config), how its results
//! make a status ([`Health::record`]: `starting`, then `healthy` or
//! `unhealthy`), and the file a container's companion keeps them in, one
//! per start ([`FILE`], versioned: H2). The companion that runs the
//! checks is `run::health` (Linux).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Result, bail};

/// The health file's version: what's written.
pub const VERSION: u32 = 1;
/// The health file, in a start's log directory.
pub const FILE: &str = "health.json";
/// How many results the file keeps (the latest), as Docker.
pub const KEPT: usize = 5;
/// How much of a check's output is kept, in bytes (the rest is read and
/// dropped), as Docker.
pub const MAX_OUTPUT: usize = 4096;
/// The most of the health file that's read, in bytes: [`KEPT`] results of
/// [`MAX_OUTPUT`] each are a few dozen KiB at most; more isn't a file the
/// companion wrote.
const MAX_FILE: u64 = 1024 * 1024;

/// The test of an image's check that can't be used ([`HealthCheck::merged`]),
/// then why: [`HealthCheck::command`] refuses it, saying so. Not a form an
/// image would say (it starts with a NUL).
const UNUSABLE: &str = "\0unusable";

/// Docker's defaults, for a setting given as zero or not at all.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_START_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_RETRIES: u32 = 3;

/// A container's health check (`--health-cmd` and the rest), as its
/// config keeps it: the image's own merged with the flags at create
/// ([`merged`](Self::merged)). A zero means Docker's default (none, for
/// the start period). As asked, before the merge, an empty `test` tunes the
/// image's check, and [`disabled`](Self::disabled) turns it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// What to run, in Docker's form: `["CMD-SHELL", "script"]` (run by
    /// `/bin/sh -c`) or `["CMD", program, args…]`.
    pub test: Vec<String>,
    /// Between two checks, in milliseconds (30 s).
    pub interval_ms: u64,
    /// The longest a check may take, in milliseconds (30 s): then it's a
    /// failure.
    pub timeout_ms: u64,
    /// After the start, how long failures don't count, in milliseconds
    /// (none).
    pub start_period_ms: u64,
    /// Between two checks during the start period, in milliseconds (5 s).
    pub start_interval_ms: u64,
    /// How many failures in a row make it unhealthy (3).
    pub retries: u32,
    /// The shell a `CMD-SHELL` test runs with: its image's (`SHELL`), or,
    /// empty, `/bin/sh -c`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell: Vec<String>,
}

impl HealthCheck {
    /// A check of `script`, run by the container's shell (`/bin/sh -c`,
    /// unless its image says another) (`--health-cmd`), with Docker's
    /// defaults.
    #[must_use]
    pub fn shell(script: &str) -> Self {
        Self {
            test: vec!["CMD-SHELL".to_owned(), script.to_owned()],
            interval_ms: 0,
            timeout_ms: 0,
            start_period_ms: 0,
            start_interval_ms: 0,
            retries: 0,
            shell: Vec::new(),
        }
    }

    /// `--no-healthcheck`: no check, not even the image's (`["NONE"]`).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            test: vec!["NONE".to_owned()],
            ..Self::shell("")
        }
    }

    /// Whether it turns checks off (`["NONE"]`).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.test.first().is_some_and(|form| form == "NONE")
    }

    /// The check a container gets from what's `asked` (the flags) and its
    /// `image`'s own, Docker's rules: nothing asked, the image's;
    /// `--no-healthcheck`, none; else what's asked, its empty test and its
    /// zeros taken from the image's. None if that leaves no test (an image
    /// without a check, or whose check is `NONE`, and no `--health-cmd`).
    ///
    /// An image's check that can't be used (a duration under a millisecond,
    /// [`ImageHealthcheck::unusable`](shipyard_image::ImageHealthcheck::unusable))
    /// gives nothing to a test asked, and is otherwise a check whose
    /// [`command`](Self::command) refuses it, saying why: the caller leaves
    /// it out, with a warning, as any image's check that can't be run.
    #[must_use]
    pub fn merged(
        asked: Option<&Self>,
        image: Option<&shipyard_image::ImageHealthcheck>,
    ) -> Option<Self> {
        let image = image.filter(|image| !image.is_none()).map(|image| {
            if let Some(why) = image.unusable() {
                return Self {
                    test: vec![UNUSABLE.to_owned(), why],
                    ..Self::shell("")
                };
            }
            Self {
                test: image.test.clone(),
                interval_ms: millis(image.interval),
                timeout_ms: millis(image.timeout),
                start_period_ms: millis(image.start_period),
                start_interval_ms: millis(image.start_interval),
                retries: image.retries,
                shell: Vec::new(),
            }
        });
        let Some(asked) = asked else {
            return image;
        };
        if asked.is_disabled() {
            return None;
        }
        let base = image.unwrap_or_else(|| Self {
            test: Vec::new(),
            ..Self::shell("")
        });
        let test = if asked.test.is_empty() {
            base.test
        } else {
            asked.test.clone()
        };
        (!test.is_empty()).then(|| Self {
            test,
            interval_ms: pick(asked.interval_ms, base.interval_ms),
            timeout_ms: pick(asked.timeout_ms, base.timeout_ms),
            start_period_ms: pick(asked.start_period_ms, base.start_period_ms),
            start_interval_ms: pick(asked.start_interval_ms, base.start_interval_ms),
            retries: pick(asked.retries, base.retries),
            shell: Vec::new(),
        })
    }

    /// The command to run in the container.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a
    /// test in neither form, or with nothing to run.
    pub fn command(&self) -> Result<Vec<String>> {
        match self.test.split_first() {
            Some((form, rest)) if form == "CMD-SHELL" && rest.len() == 1 && !rest[0].is_empty() => {
                let mut command = if self.shell.is_empty() {
                    vec!["/bin/sh".to_owned(), "-c".to_owned()]
                } else {
                    self.shell.clone()
                };
                command.push(rest[0].clone());
                Ok(command)
            }
            Some((form, rest)) if form == "CMD" && !rest.is_empty() => Ok(rest.to_vec()),
            Some((form, [why])) if form == UNUSABLE => bail!(InvalidRequest, "{why}"),
            _ => bail!(
                InvalidRequest,
                "a health check {:?}: give [\"CMD-SHELL\", script] or [\"CMD\", program, …]",
                self.test
            ),
        }
    }

    /// Between two checks.
    #[must_use]
    pub fn interval(&self) -> Duration {
        or_default(self.interval_ms, DEFAULT_INTERVAL)
    }

    /// The longest a check may take.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        or_default(self.timeout_ms, DEFAULT_TIMEOUT)
    }

    /// How long failures don't count, after the start.
    #[must_use]
    pub fn start_period(&self) -> Duration {
        Duration::from_millis(self.start_period_ms)
    }

    /// Between two checks during the start period.
    #[must_use]
    pub fn start_interval(&self) -> Duration {
        or_default(self.start_interval_ms, DEFAULT_START_INTERVAL)
    }

    /// How many failures in a row make it unhealthy.
    #[must_use]
    pub fn retries(&self) -> u32 {
        if self.retries == 0 {
            DEFAULT_RETRIES
        } else {
            self.retries
        }
    }
}

/// A duration flag's value (`--health-interval` and the others, named
/// `flag`) in milliseconds, as a check keeps it: zero, or not given, for the
/// default.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): one
/// under a millisecond that isn't zero (Docker refuses it too: kept in
/// milliseconds, it would be zero, the default, or rounded up).
pub fn flag_millis(flag: &str, given: Option<Duration>) -> Result<u64> {
    let given = given.unwrap_or_default();
    if !given.is_zero() && given < Duration::from_millis(1) {
        bail!(
            InvalidRequest,
            "{flag} {given:?}: give at least 1ms (or 0 for the default)"
        );
    }
    Ok(millis(given))
}

/// `mine`, unless it's zero (not given): then `theirs` (the image's).
fn pick<T: Default + PartialEq>(mine: T, theirs: T) -> T {
    if mine == T::default() { theirs } else { mine }
}

/// `duration` in milliseconds, as a check keeps it: never zero for one
/// that isn't (a zero is "the default"), nor past `u64`.
fn millis(duration: Duration) -> u64 {
    if duration.is_zero() {
        return 0;
    }
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

/// `millis` as a duration, or `default` for zero.
fn or_default(millis: u64, default: Duration) -> Duration {
    if millis == 0 {
        default
    } else {
        Duration::from_millis(millis)
    }
}

/// What the checks say, Docker's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// No check has passed yet, and not enough have failed.
    Starting,
    /// The last check passed.
    Healthy,
    /// [`HealthCheck::retries`] checks in a row failed (after the start
    /// period).
    Unhealthy,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        })
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    /// When it began (RFC 3339).
    pub start: String,
    /// When it ended (RFC 3339).
    pub end: String,
    /// Its exit code (`-1`: it ran out of time).
    pub exit_code: i32,
    /// The start of its output, stdout and stderr together (at most
    /// [`MAX_OUTPUT`] bytes).
    pub output: String,
}

/// A container's health, for this start: what the checks say, and their
/// latest results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    /// What they say.
    pub status: Status,
    /// How many failed in a row (not counting the start period's).
    pub failing_streak: u32,
    /// The latest results, oldest first (at most [`KEPT`]).
    pub log: Vec<Probe>,
}

/// Version 1 of the file, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    v: u32,
    status: Status,
    failing_streak: u32,
    log: Vec<Probe>,
}

impl Health {
    /// Before any check.
    #[must_use]
    pub fn starting() -> Self {
        Self {
            status: Status::Starting,
            failing_streak: 0,
            log: Vec::new(),
        }
    }

    /// `probe`'s result counted (`passed`: its exit code was 0;
    /// `in_start_period`: it began in the start period), with `retries`
    /// failures in a row making it unhealthy. Whether the status changed.
    ///
    /// Docker's rules: a pass is healthy at once (in the start period
    /// too), and ends a failing streak; a failure in the start period
    /// changes nothing while it's still starting (once it has passed, it
    /// counts); otherwise failures count, and `retries` of them in a row
    /// make it unhealthy.
    pub fn record(
        &mut self,
        probe: Probe,
        passed: bool,
        in_start_period: bool,
        retries: u32,
    ) -> bool {
        let before = self.status;
        if passed {
            self.failing_streak = 0;
            self.status = Status::Healthy;
        } else if !(in_start_period && self.status == Status::Starting) {
            self.failing_streak = self.failing_streak.saturating_add(1);
            if self.failing_streak >= retries {
                self.status = Status::Unhealthy;
            }
        }
        self.log.push(probe);
        let over = self.log.len().saturating_sub(KEPT);
        self.log.drain(..over);
        self.status != before
    }

    /// As the file keeps it.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&V1 {
            v: VERSION,
            status: self.status,
            failing_streak: self.failing_streak,
            log: self.log.clone(),
        })
        .unwrap_or_default()
    }

    /// What `bytes` say, if they're a file of a version this reads.
    #[must_use]
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        let read: V1 = serde_json::from_slice(bytes).ok()?;
        (read.v == VERSION).then_some(Self {
            status: read.status,
            failing_streak: read.failing_streak,
            log: read.log,
        })
    }

    /// The health file in the start's log directory `dir`, if there's one
    /// this reads: a regular file (opened without following a symlink or
    /// waiting on a FIFO) of at most 1 MiB.
    #[must_use]
    pub fn read(dir: &Path) -> Option<Self> {
        Self::from_json(&shipyard_sys::fs::read_bounded(&dir.join(FILE), MAX_FILE).ok()?)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn probe(code: i32) -> Probe {
        Probe {
            start: "2026-10-14T09:00:00.000000000Z".to_owned(),
            end: "2026-10-14T09:00:00.100000000Z".to_owned(),
            exit_code: code,
            output: String::new(),
        }
    }

    /// Docker's rules: starting until a pass (healthy at once, start period
    /// or not) or `retries` failures in a row after the start period;
    /// failures in it don't count; a pass ends a streak.
    #[test]
    fn the_status_follows_dockers_rules() {
        let mut health = Health::starting();
        assert!(
            !health.record(probe(1), false, true, 3),
            "in the start period"
        );
        assert!(!health.record(probe(1), false, true, 3));
        assert_eq!(
            (health.status, health.failing_streak),
            (Status::Starting, 0)
        );
        assert!(
            health.record(probe(0), true, true, 3),
            "a pass: healthy at once"
        );
        assert_eq!(health.status, Status::Healthy);
        // Once it has passed, a failure counts, start period or not.
        assert!(!health.record(probe(1), false, true, 3));
        assert!(!health.record(probe(1), false, false, 3));
        assert_eq!((health.status, health.failing_streak), (Status::Healthy, 2));
        assert!(health.record(probe(1), false, false, 3), "the third");
        assert_eq!(
            (health.status, health.failing_streak),
            (Status::Unhealthy, 3)
        );
        assert!(!health.record(probe(2), false, false, 3), "still");
        assert!(health.record(probe(0), true, false, 3), "a pass ends it");
        assert_eq!((health.status, health.failing_streak), (Status::Healthy, 0));
        // Never more than the latest five.
        assert_eq!(health.log.len(), KEPT);
        assert_eq!(health.log.last().unwrap().exit_code, 0);
        // Without a start period, failures count from the first.
        let mut health = Health::starting();
        health.record(probe(1), false, false, 1);
        assert_eq!(health.status, Status::Unhealthy);
    }

    /// A pinned version 1 file reads as written; another version doesn't.
    #[test]
    fn a_health_file_reads_back_and_version_1_is_pinned() {
        let mut health = Health::starting();
        health.record(probe(0), true, false, 3);
        let pinned = r#"{"v":1,"status":"healthy","failing_streak":0,"log":[{"start":"2026-10-14T09:00:00.000000000Z","end":"2026-10-14T09:00:00.100000000Z","exit_code":0,"output":""}]}"#;
        assert_eq!(String::from_utf8(health.to_json()).unwrap(), pinned);
        assert_eq!(Health::from_json(pinned.as_bytes()), Some(health));
        assert_eq!(
            Health::from_json(pinned.replace(r#""v":1"#, r#""v":2"#).as_bytes()),
            None
        );
        assert_eq!(Health::from_json(b"{"), None);
    }

    /// What's asked merged with an image's own check, Docker's table.
    #[test]
    fn a_check_merges_as_dockers() {
        use shipyard_image::ImageHealthcheck;
        let image = ImageHealthcheck {
            test: vec!["CMD".into(), "probe".into()],
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            start_period: Duration::ZERO,
            start_interval: Duration::from_millis(1) + Duration::from_micros(300),
            retries: 4,
        };
        // Nothing asked, nothing in the image: none; the image's alone.
        assert_eq!(HealthCheck::merged(None, None), None);
        let theirs = HealthCheck::merged(None, Some(&image)).unwrap();
        assert_eq!(theirs.test, ["CMD", "probe"]);
        assert_eq!(
            (theirs.interval_ms, theirs.timeout_ms, theirs.retries),
            (10_000, 2000, 4)
        );
        assert_eq!(theirs.start_interval_ms, 1);
        // Asked alone: as asked.
        let shell = HealthCheck::shell("true");
        assert_eq!(HealthCheck::merged(Some(&shell), None), Some(shell.clone()));
        // Asked in part (no test): the image's test, the asked fields, the
        // image's for the zeros.
        let tuned = HealthCheck {
            test: Vec::new(),
            retries: 1,
            ..HealthCheck::shell("")
        };
        let both = HealthCheck::merged(Some(&tuned), Some(&image)).unwrap();
        assert_eq!(both.test, ["CMD", "probe"]);
        assert_eq!((both.retries, both.interval_ms), (1, 10_000));
        // A test asked wins, the image's settings filling its zeros.
        let over = HealthCheck::merged(Some(&shell), Some(&image)).unwrap();
        assert_eq!(over.test, ["CMD-SHELL", "true"]);
        assert_eq!(over.interval_ms, 10_000);
        // In part, with no image check: none (the caller warns).
        assert_eq!(HealthCheck::merged(Some(&tuned), None), None);
        // --no-healthcheck: none, the image's too.
        assert_eq!(
            HealthCheck::merged(Some(&HealthCheck::disabled()), Some(&image)),
            None
        );
        // The image's NONE: none, unless a test is asked.
        let none = ImageHealthcheck {
            test: vec!["NONE".into()],
            ..ImageHealthcheck::default()
        };
        assert_eq!(HealthCheck::merged(None, Some(&none)), None);
        assert_eq!(HealthCheck::merged(Some(&tuned), Some(&none)), None);
        assert!(HealthCheck::merged(Some(&shell), Some(&none)).is_some());
    }

    /// An image's duration under a millisecond (not zero) isn't rounded:
    /// the check can't be run (the caller leaves it out, warning why), and
    /// gives nothing to a test asked.
    #[test]
    fn an_image_check_under_a_millisecond_is_left_out() {
        use shipyard_image::ImageHealthcheck;
        let image = ImageHealthcheck {
            test: vec!["CMD".into(), "probe".into()],
            interval: Duration::from_secs(10),
            timeout: Duration::from_micros(500),
            ..ImageHealthcheck::default()
        };
        let theirs = HealthCheck::merged(None, Some(&image)).unwrap();
        let error = theirs.command().unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidRequest);
        assert!(
            error.to_string().contains("Timeout of 500µs is under 1 ms"),
            "{error}"
        );
        // Tuned only: still the image's, so still refused.
        let tuned = HealthCheck {
            test: Vec::new(),
            retries: 1,
            ..HealthCheck::shell("")
        };
        assert!(
            HealthCheck::merged(Some(&tuned), Some(&image))
                .unwrap()
                .command()
                .is_err()
        );
        // A test asked: none of the image's settings taken.
        let asked = HealthCheck::shell("true");
        assert_eq!(HealthCheck::merged(Some(&asked), Some(&image)), Some(asked));
    }

    /// A flag's duration under a millisecond is refused; zero, absent and a
    /// millisecond or more are kept.
    #[test]
    fn a_flag_under_a_millisecond_is_refused() {
        let error = flag_millis("--health-interval", Some(Duration::from_micros(999))).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidRequest);
        assert!(error.to_string().contains("--health-interval"), "{error}");
        assert!(flag_millis("--health-timeout", Some(Duration::from_nanos(1))).is_err());
        assert_eq!(flag_millis("--health-timeout", None).unwrap(), 0);
        assert_eq!(
            flag_millis("--health-timeout", Some(Duration::ZERO)).unwrap(),
            0
        );
        assert_eq!(
            flag_millis("--health-timeout", Some(Duration::from_millis(1))).unwrap(),
            1
        );
        assert_eq!(
            flag_millis("--health-timeout", Some(Duration::from_micros(2500))).unwrap(),
            2
        );
    }

    /// The health file is read only if it's a regular file, not through a
    /// symlink, and not past its size cap.
    #[test]
    fn a_health_file_that_isnt_plain_or_is_too_big_is_not_read() {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-health-read-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut health = Health::starting();
        health.record(probe(0), true, false, 3);
        // Plain: read.
        std::fs::write(dir.join(FILE), health.to_json()).unwrap();
        assert_eq!(Health::read(&dir), Some(health.clone()));
        // Through a symlink: not.
        std::fs::rename(dir.join(FILE), dir.join("real.json")).unwrap();
        std::os::unix::fs::symlink(dir.join("real.json"), dir.join(FILE)).unwrap();
        assert_eq!(Health::read(&dir), None);
        std::fs::remove_file(dir.join(FILE)).unwrap();
        // A directory: not.
        std::fs::create_dir(dir.join(FILE)).unwrap();
        assert_eq!(Health::read(&dir), None);
        std::fs::remove_dir(dir.join(FILE)).unwrap();
        // Past the cap (valid JSON, padded with spaces): not.
        let mut big = health.to_json();
        big.resize(usize::try_from(MAX_FILE).unwrap() + 1, b' ');
        std::fs::write(dir.join(FILE), &big).unwrap();
        assert_eq!(Health::read(&dir), None);
        // At the cap: read.
        big.truncate(usize::try_from(MAX_FILE).unwrap());
        std::fs::write(dir.join(FILE), &big).unwrap();
        assert_eq!(Health::read(&dir), Some(health));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A check's command in either of Docker's forms; zeros are defaults.
    #[test]
    fn a_check_is_in_dockers_forms() {
        let shell = HealthCheck::shell("curl -f localhost");
        assert_eq!(
            shell.command().unwrap(),
            ["/bin/sh", "-c", "curl -f localhost"]
        );
        assert_eq!(shell.interval(), Duration::from_secs(30));
        assert_eq!(shell.timeout(), Duration::from_secs(30));
        assert_eq!(shell.start_period(), Duration::ZERO);
        assert_eq!(shell.start_interval(), Duration::from_secs(5));
        assert_eq!(shell.retries(), 3);
        let exec = HealthCheck {
            test: vec!["CMD".into(), "pg_isready".into(), "-q".into()],
            interval_ms: 1500,
            retries: 5,
            ..shell.clone()
        };
        assert_eq!(exec.command().unwrap(), ["pg_isready", "-q"]);
        assert_eq!(exec.interval(), Duration::from_millis(1500));
        assert_eq!(exec.retries(), 5);
        for test in [
            vec![],
            vec!["CMD".to_owned()],
            vec!["NONE".to_owned()],
            vec!["CMD-SHELL".to_owned(), String::new()],
        ] {
            let bad = HealthCheck {
                test,
                ..shell.clone()
            };
            assert!(bad.command().is_err(), "{:?}", bad.test);
        }
    }
}
