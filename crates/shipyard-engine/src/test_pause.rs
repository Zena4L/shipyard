//! A test build's pauses (named by `Paths`' `SHIPYARD_TEST_*_PAUSE`, debug
//! builds only): where a process reaches a window a test acts in (a kill
//! point, F119's start), it says so in a file the test waits for, then
//! waits itself.

use std::path::Path;

/// Says the pause is reached: `text` written at `path` whole (a new file,
/// never one there already, never through a link; renamed into place, so a
/// test never reads half of it). `false` if it couldn't be.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the pauses are Linux's")
)]
pub(crate) fn announce(path: &Path, text: &str) -> bool {
    let partial = path.with_extension("partial");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
        .and_then(|()| std::fs::rename(&partial, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// Said whole at the path, never over a file there already.
    #[test]
    fn a_pause_is_said_once_whole() {
        let dir = std::env::temp_dir().join(format!("shipyard-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("here");
        assert!(announce(&path, "42"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "42");
        std::fs::write(path.with_extension("partial"), "").unwrap();
        assert!(!announce(&path, "43"), "a partial there already");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "42");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
