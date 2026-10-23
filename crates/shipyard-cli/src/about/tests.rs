use super::*;

/// A version as Docker's `version` lays it out (2026-10-20's capture):
/// the client's part, then the server's engine.
#[test]
fn a_version_reads_as_docker_s() {
    let versions = Versions {
        client: ClientVersion {
            version: "0.3.0".to_owned(),
            api_version: API_VERSION,
            default_api_version: API_VERSION,
            git_commit: "abc".to_owned(),
            go_version: "",
            os: "linux",
            arch: "arm64",
            build_time: "",
            context: "default",
        },
        server: docker::system_version(&build("abc")),
    };
    let text = version_text(&versions);
    assert!(
        text.starts_with("Client:\n Version:           0.3.0\n"),
        "{text}"
    );
    assert!(text.contains("\nServer: Shipyard\n Engine:\n"), "{text}");
    assert!(
        text.contains("  API version:      1.43 (minimum version 1.24)\n"),
        "{text}"
    );
    // A template reads Go's names (`.APIVersion`) and the JSON's.
    let mut out = Vec::new();
    Output::strict("{{.Server.APIVersion}} {{.Client.ApiVersion}} {{.Client.Context}}")
        .unwrap()
        .write(&mut out, std::slice::from_ref(&versions))
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "1.43 1.43 default\n");
    // A field it hasn't is an error, as Docker's Go struct's.
    let nope = Output::strict("{{.Server.Nope}}").unwrap();
    assert!(nope.write(&mut Vec::new(), &[versions]).is_err());
}

/// Security options as Docker's `info` lists them; memory in binary units.
#[test]
fn info_s_parts_read_as_docker_s() {
    assert_eq!(
        security_option("name=seccomp,profile=builtin"),
        "  seccomp\n   Profile: builtin\n"
    );
    assert_eq!(security_option("name=rootless"), "  rootless\n");
    let binary_size = crate::format::rows::binary_size;
    assert_eq!(binary_size(12_594_438_144), "11.73GiB");
    assert_eq!(binary_size(512), "512B");
    assert_eq!(binary_size(1024), "1KiB");
}
