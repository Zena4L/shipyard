//! The environment a process starts with.
//!
//! Containers never inherit the host's environment: only what the user asks
//! for, plus `PATH` and `HOME` defaults. That keeps host secrets out.

use std::collections::HashMap;
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
///
/// Lookups are by an index from key to position, so building one from an
/// OCI `config.json` with many entries stays linear.
#[derive(Clone, Default)]
pub struct Env {
    vars: Vec<(OsString, OsString)>,
    /// Where each key is in `vars`.
    index: HashMap<OsString, usize>,
}

/// Equal when the variables and their order are: the index follows from them.
impl PartialEq for Env {
    fn eq(&self, other: &Self) -> bool {
        self.vars == other.vars
    }
}

impl Eq for Env {}

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
        self.insert(key, value);
        Ok(())
    }

    /// The value of `key`, if it is set.
    #[must_use]
    pub fn get(&self, key: &OsStr) -> Option<&OsStr> {
        let &position = self.index.get(key)?;
        self.vars.get(position).map(|(_, value)| value.as_os_str())
    }

    /// Adds [`DEFAULT_PATH`] as `PATH` if it isn't set. (`HOME` is set
    /// where the command runs, from the user's `/etc/passwd` entry.)
    pub fn fill_default_path(&mut self) {
        let path = OsStr::new("PATH");
        if self.get(path).is_none() {
            self.insert(path, OsStr::new(DEFAULT_PATH));
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

    /// Sets a validated `key`, in place if it's already there.
    fn insert(&mut self, key: &OsStr, value: &OsStr) {
        if let Some((_, old)) = self.index.get(key).and_then(|&at| self.vars.get_mut(at)) {
            value.clone_into(old);
        } else {
            self.index.insert(key.to_owned(), self.vars.len());
            self.vars.push((key.to_owned(), value.to_owned()));
        }
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

    /// Big environments (an OCI config can list many) are built in linear
    /// time: each `set` is a lookup, not a scan.
    #[test]
    fn many_variables_stay_fast() {
        let mut env = Env::new();
        let start = std::time::Instant::now();
        for round in 0..2 {
            for i in 0..100_000 {
                let value = format!("{round}");
                env.set(OsStr::new(&format!("VAR{i}")), OsStr::new(&value))
                    .unwrap();
            }
        }
        assert_eq!(env.len(), 100_000);
        assert_eq!(get(&env, "VAR99999"), Some(OsStr::new("1")));
        // A scan per `set` would take minutes; the index, well under this.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn equality_is_the_variables_in_order() {
        let a = env_of(&["A=1", "B=2"]).unwrap();
        let b = env_of(&["A=0", "B=2", "A=1"]).unwrap();
        assert_eq!(a, b, "same variables, same order");
        assert_ne!(a, env_of(&["B=2", "A=1"]).unwrap(), "order matters");
    }

    #[test]
    fn the_default_path_never_overrides_the_users() {
        let mut env = env_of(&["PATH=/mine"]).unwrap();
        env.fill_default_path();
        assert_eq!(get(&env, "PATH"), Some(OsStr::new("/mine")));
        let mut empty = Env::new();
        empty.fill_default_path();
        assert_eq!(get(&empty, "PATH"), Some(OsStr::new(DEFAULT_PATH)));
        assert_eq!(get(&empty, "HOME"), None, "set where the command runs");
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
