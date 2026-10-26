use base64::Engine as _;
use shipyard_engine::image::Credential;

use super::*;

fn query(text: &str) -> Query {
    Query::parse(Some(text)).unwrap()
}

/// `fromImage` and `tag` as one reference: a tag, a digest, or `latest`.
#[test]
fn a_pull_s_reference_is_its_image_and_tag() {
    let named = |text: &str| reference(&query(text)).unwrap().familiar();
    assert_eq!(named("fromImage=alpine&tag=3.20"), "alpine:3.20");
    assert_eq!(named("fromImage=alpine"), "alpine:latest");
    assert_eq!(named("fromImage=alpine%3A3.19&tag="), "alpine:3.19");
    assert_eq!(
        named("fromImage=alpine%3A3.19&tag=3.20"),
        "alpine:3.20",
        "the tag wins"
    );
    assert_eq!(
        named("fromImage=localhost%3A5000%2Fapp%3Av1&tag=v2"),
        "localhost:5000/app:v2"
    );
    let digest = format!("sha256:{}", "a".repeat(64));
    assert_eq!(
        named(&format!("fromImage=alpine&tag={digest}")),
        format!("alpine@{digest}")
    );
    for refused in [
        "tag=1",
        "fromImage=",
        "fromImage=Alpine&tag=1",
        "fromImage=a&tag=b%20c",
    ] {
        assert_eq!(
            reference(&query(refused)).unwrap_err().status,
            StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
}

/// `header` as `X-Registry-Auth`.
fn auth(header: &str) -> Result<Option<GivenCredential>, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTH_HEADER, header.parse().unwrap());
    credentials(&headers)
}

/// Docker's credentials JSON as its client encodes it (base64url).
fn encoded(json: &str) -> String {
    base64::engine::general_purpose::URL_SAFE.encode(json)
}

/// The credentials a client hands in: user and password, Docker's `auth`,
/// or an identity token; base64url or standard, padded or not; none when
/// empty; a bad header refused without echoing it.
#[test]
fn credentials_are_read_as_docker_s_client_sends_them() {
    let basic = |given: Option<GivenCredential>| match given.map(|given| given.credential) {
        Some(Credential::Basic { username, password }) => Some((username, password)),
        _ => None,
    };
    let given = auth(&encoded(
        r#"{"username":"me","password":"s3?cr>t","serveraddress":"localhost:5000"}"#,
    ))
    .unwrap();
    assert_eq!(
        given.as_ref().unwrap().registry.as_deref(),
        Some("localhost:5000")
    );
    assert_eq!(basic(given), Some(("me".to_owned(), "s3?cr>t".to_owned())));
    let standard = base64::engine::general_purpose::STANDARD
        .encode(r#"{"username":"me","password":"s3?cr>t"}"#);
    assert_eq!(
        basic(auth(&standard).unwrap()),
        Some(("me".to_owned(), "s3?cr>t".to_owned()))
    );
    let pair = base64::engine::general_purpose::STANDARD.encode("me:pw:x");
    assert_eq!(
        basic(auth(&encoded(&format!(r#"{{"auth":"{pair}"}}"#))).unwrap()),
        Some(("me".to_owned(), "pw:x".to_owned()))
    );
    let token = auth(&encoded(r#"{"identitytoken":"t0k"}"#))
        .unwrap()
        .unwrap();
    assert_eq!(
        token.credential,
        Credential::IdentityToken("t0k".to_owned())
    );
    assert_eq!(token.registry, None);
    // A registry's own bearer token, read first (Docker's order, F118).
    let bearer = auth(&encoded(
        r#"{"registrytoken":"b3ar","identitytoken":"t0k","username":"me","password":"pw"}"#,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(
        bearer.credential,
        Credential::RegistryToken("b3ar".to_owned())
    );
    for none in ["", &encoded("{}"), &encoded(r#"{"serveraddress":"x"}"#)] {
        assert!(auth(none).unwrap().is_none(), "{none:?}");
    }
    for bad in [
        "not base64!",
        &encoded("[1]"),
        &encoded(r#"{"auth":"bm9jb2xvbg=="}"#),
    ] {
        let refused = auth(bad).unwrap_err();
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(!refused.message.contains(bad), "{}", refused.message);
    }
    let long = "a".repeat(MAX_AUTH + 1);
    assert_eq!(auth(&long).unwrap_err().status, StatusCode::BAD_REQUEST);
}

/// A pull's lines, as Docker's client reads them.
#[test]
fn a_pull_s_progress_is_docker_s_lines() {
    let line = |progress| serde_json::to_value(Line::of(&progress)).unwrap();
    assert_eq!(
        line(Progress::Pulling {
            image: "docker.io/library/alpine:3.20".to_owned()
        }),
        serde_json::json!({"status": "Pulling from library/alpine", "id": "3.20"})
    );
    assert_eq!(
        line(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: Some(42)
        }),
        serde_json::json!({"status": "Pull complete", "id": "0123456789ab", "progressDetail": {}})
    );
    assert_eq!(
        line(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: None
        }),
        serde_json::json!({"status": "Already exists", "id": "0123456789ab", "progressDetail": {}})
    );
    assert_eq!(
        serde_json::to_value(Line::error("no such image")).unwrap(),
        serde_json::json!({"errorDetail": {"message": "no such image"}, "error": "no such image"})
    );
}

/// A registry's words reach a terminal without its control characters.
#[test]
fn a_registry_s_words_are_made_plain() {
    assert_eq!(plain("denied\u{1b}[2J: \u{7}no\r\nway"), "denied[2J: noway");
    assert_eq!(familiar("docker.io/library/alpine:3.20"), "alpine:3.20");
    assert_eq!(familiar("not a reference"), "not a reference");
}
