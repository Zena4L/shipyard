use super::*;

/// Go's `os.FileMode` bits (Docker's API's `TmpfsOptions.Mode`) as the
/// kernel's octal mode: the sticky bit Go keeps high up becomes `01000`.
#[test]
fn go_s_modes_are_the_kernel_s() {
    assert_eq!(go_mode(0o1777), "1777");
    assert_eq!(go_mode((1 << 20) | 0o777), "1777", "Go's sticky bit");
    assert_eq!(go_mode((1 << 31) | 0o755), "755", "a directory's type bit");
    assert_eq!(go_mode((1 << 23) | (1 << 22) | 0o700), "6700");
}
