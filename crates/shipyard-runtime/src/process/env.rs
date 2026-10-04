//! The environment a process starts with.
//!
//! Containers never inherit the host's environment: only what the user asks
//! for, plus `PATH` and `HOME` defaults. That keeps host secrets out.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;

/// `PATH` when the user doesn't set one. Same as Docker and runc.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Environment variables, in the order they were first set.
///
/// Keys and values are raw bytes (`OsString`), because on Unix neither has to
/// be UTF-8. Every entry is validated on the way in, so an `Env` can always be
/// passed to `execve` as is.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Env {
    vars: Vec<(OsString, OsString)>,
}

/// Why an environment variable was rejected. Errors name the variable but
/// never include its value, which may be a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvError {
    /// The entry has no `=`, so it isn't `KEY=VALUE`. The entry itself isn't
    /// included: without an `=` there's no telling name from value.
    #[error("expected KEY=VALUE (the entry has no '=')")]
    MissingEquals,
    /// The name before `=` is empty.
    #[error("environment variable name is empty")]
    EmptyKey,
    /// The name contains `=`, which would make the entry ambiguous.
    #[error("environment variable name \"{0}\" contains '='")]
    KeyContainsEquals(String),
    /// The name or value contains a NUL byte, which C strings can't hold.
    #[error("environment variable \"{0}\" contains a NUL byte")]
    ContainsNul(String),
}

impl Env {
    /// An empty environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a `KEY=VALUE` entry. The split is at the first `=`, so `A==b`
    /// sets `A` to `=b`, and `A=` sets `A` to the empty string.
    pub fn push_entry(&mut self, entry: &OsStr) -> Result<(), EnvError> {
        let bytes = entry.as_bytes();
        // A bare `KEY` means "copy from the host" in Docker's CLI. That's the
        // caller's job, so here it's an error.
        let Some(eq) = bytes.iter().position(|&b| b == b'=') else {
            return Err(EnvError::MissingEquals);
        };
        let (key, value) = bytes.split_at(eq);
        let value = value.get(1..).unwrap_or_default();
        self.set(OsStr::from_bytes(key), OsStr::from_bytes(value))
    }

    /// Sets `key` to `value`, replacing any earlier value in place.
    pub fn set(&mut self, key: &OsStr, value: &OsStr) -> Result<(), EnvError> {
        validate(key, value)?;
        match self.vars.iter_mut().find(|(k, _)| k == key) {
            Some((_, old)) => value.clone_into(old),
            None => self.vars.push((key.to_owned(), value.to_owned())),
        }
        Ok(())
    }

    /// The value of `key`, if it is set.
    #[must_use]
    pub fn get(&self, key: &OsStr) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }

    /// Adds `PATH` and `HOME` if they aren't set. The user's values always win.
    pub fn fill_defaults(&mut self, uid: u32) {
        let home = if uid == 0 { "/root" } else { "/" };
        for (key, value) in [("PATH", DEFAULT_PATH), ("HOME", home)] {
            if self.get(OsStr::new(key)).is_none() {
                self.vars.push((key.into(), value.into()));
            }
        }
    }

    /// The variables as `(key, value)` pairs, in the order they were first set.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    /// How many variables are set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether no variables are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

fn validate(key: &OsStr, value: &OsStr) -> Result<(), EnvError> {
    let key_bytes = key.as_bytes();
    if key_bytes.is_empty() {
        return Err(EnvError::EmptyKey);
    }
    let name = || key.to_string_lossy().into_owned();
    if key_bytes.contains(&b'=') {
        return Err(EnvError::KeyContainsEquals(name()));
    }
    if key_bytes.contains(&0) || value.as_bytes().contains(&0) {
        return Err(EnvError::ContainsNul(name()));
    }
    Ok(())
}

/// Shows names only. Values often hold secrets, and specs end up in logs.
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.vars.iter().map(|(k, _)| k))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn env_of(entries: &[&str]) -> Result<Env, EnvError> {
        let mut env = Env::new();
        for entry in entries {
            env.push_entry(OsStr::new(entry))?;
        }
        Ok(env)
    }

    fn get<'a>(env: &'a Env, key: &str) -> Option<&'a OsStr> {
        env.get(OsStr::new(key))
    }

    #[test]
    fn splits_at_the_first_equals() {
        let env = env_of(&["A=1", "B=", "C==x", "D=a=b"]).unwrap();
        assert_eq!(get(&env, "A"), Some(OsStr::new("1")));
        assert_eq!(get(&env, "B"), Some(OsStr::new("")));
        assert_eq!(get(&env, "C"), Some(OsStr::new("=x")));
        assert_eq!(get(&env, "D"), Some(OsStr::new("a=b")));
    }

    #[test]
    fn rejects_bad_entries() {
        assert_eq!(env_of(&["=x"]), Err(EnvError::EmptyKey));
        assert_eq!(env_of(&[""]), Err(EnvError::MissingEquals));
        assert_eq!(env_of(&["NOEQUALS"]), Err(EnvError::MissingEquals));
        assert!(matches!(env_of(&["A=b\0c"]), Err(EnvError::ContainsNul(_))));
        assert!(matches!(env_of(&["A\0=b"]), Err(EnvError::ContainsNul(_))));
        let mut env = Env::new();
        assert!(matches!(
            env.set(OsStr::new("A=B"), OsStr::new("c")),
            Err(EnvError::KeyContainsEquals(_))
        ));
    }

    #[test]
    fn keeps_non_utf8_values() {
        let mut env = Env::new();
        let raw = OsStr::from_bytes(b"A=\xff\xfe");
        env.push_entry(raw).unwrap();
        assert_eq!(get(&env, "A").map(OsStr::as_bytes), Some(&b"\xff\xfe"[..]));
    }

    #[test]
    fn last_value_wins_and_keeps_first_position() {
        let env = env_of(&["A=1", "B=2", "A=3"]).unwrap();
        let keys: Vec<_> = env.iter().map(|(k, _)| k.to_owned()).collect();
        assert_eq!(keys, ["A", "B"]);
        assert_eq!(get(&env, "A"), Some(OsStr::new("3")));
    }

    #[test]
    fn defaults_never_override_user_values() {
        let mut env = env_of(&["PATH=/mine"]).unwrap();
        env.fill_defaults(1000);
        assert_eq!(get(&env, "PATH"), Some(OsStr::new("/mine")));
        assert_eq!(get(&env, "HOME"), Some(OsStr::new("/")));
    }

    #[test]
    fn home_depends_on_the_user() {
        let mut root = Env::new();
        root.fill_defaults(0);
        assert_eq!(get(&root, "HOME"), Some(OsStr::new("/root")));
        assert_eq!(get(&root, "PATH"), Some(OsStr::new(DEFAULT_PATH)));
    }

    #[test]
    fn debug_hides_values() {
        let env = env_of(&["TOKEN=hunter2"]).unwrap();
        let shown = format!("{env:?}");
        assert!(shown.contains("TOKEN"));
        assert!(!shown.contains("hunter2"));
    }

    proptest! {
        #[test]
        fn push_entry_never_panics(bytes in any::<Vec<u8>>()) {
            let _ = Env::new().push_entry(OsStr::from_bytes(&bytes));
        }

        #[test]
        fn valid_entries_are_stored_exactly(
            key in "[A-Za-z_][A-Za-z0-9_]{0,20}",
            value in any::<Vec<u8>>().prop_filter("no NUL", |v| !v.contains(&0)),
        ) {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(&value);
            let mut env = Env::new();
            env.push_entry(OsStr::from_bytes(&entry)).unwrap();
            prop_assert_eq!(env.get(OsStr::new(&key)).map(OsStr::as_bytes), Some(&value[..]));
        }
    }
}
