//! A program found on a `PATH` (as `execvp` finds one, TLPI §27.2), one way
//! for every crate (step 12.5b1, F145: four lookups had two rules for the
//! execute bit, and two took a relative entry: a program run from wherever
//! the caller stood).

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

/// The first regular file named `name` with an execute bit in one of the
/// absolute directories of `path` (a `PATH`'s value); `None` if there's
/// none, `path` is `None`, or `name` is a path itself (holds a `/`) or
/// empty. A relative entry (`.`, an empty one) is never searched.
#[must_use]
pub fn find_program(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The first of two is found; one without an execute bit, a directory,
    /// a relative entry's, a name that's a path: not.
    #[test]
    fn a_program_is_found_where_execvp_would() {
        let dir = std::env::temp_dir().join(format!("shipyard-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["a", "b", "c"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let program = |at: &std::path::Path, mode: u32| {
            std::fs::write(at, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(at, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        program(&dir.join("a/tool"), 0o644);
        program(&dir.join("b/tool"), 0o755);
        program(&dir.join("c/tool"), 0o755);
        std::fs::create_dir(dir.join("a/sub")).unwrap();
        let path = std::env::join_paths([dir.join("a"), dir.join("b"), dir.join("c")]).unwrap();
        assert_eq!(find_program("tool", Some(&path)), Some(dir.join("b/tool")));
        assert_eq!(find_program("sub", Some(&path)), None, "a directory");
        assert_eq!(
            find_program("b/tool", Some(dir.as_os_str())),
            None,
            "a path"
        );
        assert_eq!(find_program("", Some(&path)), None);
        assert_eq!(find_program("tool", None), None);
        // A relative entry, even one that leads to it from wherever this
        // runs (enough `..` to reach `/`, then its place): never searched.
        let mut relative = PathBuf::from("../".repeat(64));
        relative.push(dir.join("b").strip_prefix("/").unwrap());
        assert!(
            relative.join("tool").is_file(),
            "the control: it leads there"
        );
        let entries = std::env::join_paths([relative.as_path(), std::path::Path::new("")]).unwrap();
        assert_eq!(find_program("tool", Some(&entries)), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
