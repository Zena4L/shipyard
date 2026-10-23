//! A path's links followed in a container's root, as Docker follows them
//! for a link's `linkTarget` (F121): the walk alone, its lookups the
//! caller's (the `cp` helper's, in the container).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use nix::errno::Errno;

/// The most links [`in_scope`] follows: moby's `FollowSymlinkInScope`'s.
const MOST_LINKS: usize = 255;

/// `path` with every link on the way followed, in the container's root, as
/// Docker's `statPath` gives a link's `linkTarget` (moby's
/// `FollowSymlinkInScope`, F121): `..` takes the last part off (never past
/// `/`), a part that isn't there is kept as it is, and a link's target,
/// walked in its place (from `/` if it's absolute). `lookup` says what
/// a path (its links before the last already followed) is: a link's
/// target, `None` for anything else, or the errno.
///
/// # Errors
///
/// `ELOOP` past [`MOST_LINKS`], or `lookup`'s errno (but `ENOENT`).
pub(crate) fn in_scope(
    path: &Path,
    mut lookup: impl FnMut(&Path) -> Result<Option<PathBuf>, Errno>,
) -> Result<PathBuf, Errno> {
    use std::path::Component;
    let parts = |path: &Path| -> Vec<OsString> {
        path.components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name.to_owned()),
                Component::ParentDir => Some(OsString::from("..")),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
            })
            .collect()
    };
    let mut resolved = PathBuf::from("/");
    let mut rest = parts(path);
    rest.reverse();
    let mut links = 0;
    while let Some(part) = rest.pop() {
        if part == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&part);
        match lookup(&next) {
            Ok(Some(target)) => {
                links += 1;
                if links > MOST_LINKS {
                    return Err(Errno::ELOOP);
                }
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                rest.extend(parts(&target).into_iter().rev());
            }
            Ok(None) | Err(Errno::ENOENT) => resolved = next,
            Err(errno) => return Err(errno),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake root: its links by path, and the paths that are there.
    fn walk(path: &str, links: &[(&str, &str)], there: &[&str]) -> Result<PathBuf, Errno> {
        in_scope(Path::new(path), |at| {
            let at = at.to_str().unwrap();
            if let Some((_, target)) = links.iter().find(|(link, _)| *link == at) {
                return Ok(Some(PathBuf::from(target)));
            }
            if there.contains(&at) {
                Ok(None)
            } else {
                Err(Errno::ENOENT)
            }
        })
    }

    /// A link to a link is followed to the second's target (Docker's `cp
    /// -L` copies what it names), relative to where each link is; absolute
    /// targets start again from `/`; a link in a directory on the way is
    /// followed too.
    #[test]
    fn links_are_followed_link_by_link_in_scope() {
        let there = ["/etc", "/etc/real", "/data", "/data/file"];
        let links = [("/etc/second", "real"), ("/etc/first", "second")];
        assert_eq!(
            walk("/etc/first", &links, &there).unwrap(),
            Path::new("/etc/real")
        );
        let links = [("/srv/link", "/data/file")];
        assert_eq!(
            walk("/srv/link", &links, &["/srv"]).unwrap(),
            Path::new("/data/file")
        );
        let links = [("/lib", "usr/lib"), ("/usr/lib/x", "../share/y")];
        assert_eq!(
            walk("/lib/x", &links, &["/usr", "/usr/lib"]).unwrap(),
            Path::new("/usr/share/y")
        );
    }

    /// Never above `/`: `..` there stays at `/`; a part not there is kept
    /// as it is; another error is said.
    #[test]
    fn the_walk_stays_in_the_root() {
        let links = [("/a/up", "../../../../etc/passwd")];
        assert_eq!(
            walk("/a/up", &links, &["/a"]).unwrap(),
            Path::new("/etc/passwd")
        );
        assert_eq!(
            walk("/a/gone/x", &[], &["/a"]).unwrap(),
            Path::new("/a/gone/x")
        );
        let refused = in_scope(Path::new("/a/b"), |_| Err(Errno::EACCES));
        assert_eq!(refused, Err(Errno::EACCES));
    }

    /// A loop ends after Docker's 255 links (`ELOOP`); 255 are followed.
    #[test]
    fn a_loop_ends() {
        let links = [("/a", "/b"), ("/b", "/a")];
        assert_eq!(walk("/a", &links, &[]), Err(Errno::ELOOP));
        let chain: Vec<(String, String)> = (0..MOST_LINKS)
            .map(|n| (format!("/l{n}"), format!("/l{}", n + 1)))
            .collect();
        let chain: Vec<(&str, &str)> = chain
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let last = format!("/l{MOST_LINKS}");
        assert_eq!(walk("/l0", &chain, &[&last]).unwrap(), Path::new(&last));
    }
}
