//! The running kernel: its release, as `uname -r` prints it, and the
//! `MAJOR.MINOR` version features are checked against.

/// The running kernel's release (`6.8.0-142-generic`), from `uname(2)`:
/// no `/proc` needed, so it works inside a container's setup too. Empty if
/// it can't be read.
#[must_use]
pub fn release() -> String {
    nix::sys::utsname::uname()
        .ok()
        .and_then(|name| name.release().to_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `MAJOR.MINOR` from a version or a kernel release (`6.12.0-1-arm64` is
/// 6.12; `5.10` is 5.10; `5.15rc5` is 5.15). Docker compares only those two
/// numbers.
#[must_use]
pub fn version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?;
    let digits = minor
        .find(|c: char| !c.is_ascii_digit())
        .map_or(minor, |end| &minor[..end]);
    Some((major, digits.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_major_and_minor() {
        assert_eq!(version("4.8"), Some((4, 8)));
        assert_eq!(version("6.12.0-1-arm64"), Some((6, 12)));
        assert_eq!(version("5.15-rc5"), Some((5, 15)));
        assert_eq!(version("6.10.14-linuxkit"), Some((6, 10)));
        assert_eq!(version("5.10.0-26-amd64"), Some((5, 10)));
        assert_eq!(version("7.0.14-orbstack-00380"), Some((7, 0)));
        assert_eq!(version("garbage"), None);
        assert_eq!(version("6"), None);
        assert_eq!(version("six.1"), None);
        assert_eq!(version("6.x"), None);
    }

    #[test]
    fn this_kernel_has_a_version() {
        assert!(version(&release()).is_some(), "{}", release());
    }
}
