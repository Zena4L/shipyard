//! The Mac's places its VM shares (step 13.4, D13.4-1): the home, and
//! Docker Desktop's set beyond it ([`forward::MAC_SHARES`]), each at its
//! real path, so a path means the same on the Mac and in the guest.

use std::path::{Path, PathBuf};

use shipyard_runtime::attach::forward;

/// The places beyond the home this Mac shares: each of
/// [`forward::MAC_SHARES`] that's a directory at its own real path (one
/// that isn't, or a link, is left out: its path in the guest would mean
/// something else).
#[must_use]
pub fn beyond_home() -> Vec<PathBuf> {
    forward::MAC_SHARES
        .iter()
        .map(Path::new)
        .filter(|place| is_real_dir(place))
        .map(Path::to_path_buf)
        .collect()
}

/// Every place this Mac shares: `home` (canonical) first, then
/// [`beyond_home`]'s, but those under the home (shared already).
#[must_use]
pub fn all(home: &Path) -> Vec<PathBuf> {
    std::iter::once(home.to_path_buf())
        .chain(
            beyond_home()
                .into_iter()
                .filter(|place| !place.starts_with(home)),
        )
        .collect()
}

/// Whether `place` is a directory whose real path is itself.
fn is_real_dir(place: &Path) -> bool {
    place.is_dir() && std::fs::canonicalize(place).is_ok_and(|real| real == place)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The home comes first; a place that's a link, or isn't there, is
    /// left out; one under the home isn't shared twice.
    #[test]
    fn the_shares_are_real_directories_beside_the_home() {
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("shipyard-shares-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        assert!(is_real_dir(&dir.join("real")));
        assert!(!is_real_dir(&dir.join("link")));
        assert!(!is_real_dir(&dir.join("missing")));
        let home = Path::new("/Users/me");
        let all = all(home);
        assert_eq!(all.first().map(PathBuf::as_path), Some(home));
        assert_eq!(all.len(), 1 + beyond_home().len());
        assert!(all.iter().skip(1).all(|place| !place.starts_with(home)));
        // Under the home: shared already.
        assert_eq!(super::all(Path::new("/")), [PathBuf::from("/")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
