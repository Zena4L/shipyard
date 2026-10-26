use super::*;

/// Docker 29.8's sizes (2026-10-20's captures): `images` at 3 significant
/// digits, `system df` at 4.
#[test]
fn sizes_are_docker_s() {
    assert_eq!(human_size(138_200_000, 3), "138MB");
    assert_eq!(human_size(207_000_000, 3), "207MB");
    assert_eq!(human_size(4_096, 4), "4.096kB");
    assert_eq!(human_size(8_192, 4), "8.192kB");
    assert_eq!(human_size(20_350_000_000, 4), "20.35GB");
    assert_eq!(human_size(114_600_000_000, 4), "114.6GB");
    assert_eq!(human_size(4_100, 3), "4.1kB");
    assert_eq!(human_size(0, 3), "0B");
    assert_eq!(human_size(999, 3), "999B");
    assert_eq!(human_size(1_000, 3), "1kB");
    // Go's `%.3g` of 999.6 rounds to 4 digits: an exponent.
    assert_eq!(human_size(999_600, 3), "1e+03kB");
}

/// Go's `%g`: significant digits, no trailing zeros, an exponent beyond.
#[test]
fn go_g_is_go_s() {
    assert_eq!(go_g(1.0, 3), "1");
    assert_eq!(go_g(1.5, 3), "1.5");
    assert_eq!(go_g(12.345, 3), "12.3");
    assert_eq!(go_g(123.45, 3), "123");
    assert_eq!(go_g(0.000_012_3, 3), "1.23e-05");
    assert_eq!(go_g(0.001_23, 3), "0.00123");
}
