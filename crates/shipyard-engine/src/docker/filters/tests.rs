use super::*;

/// What Docker's JSON `text` asks for.
fn json(text: &str) -> Given {
    Given::from_json(Some(text)).unwrap()
}

/// The image filters: a reference's pattern over the name with its tag
/// or without, `[…]` refused; labels; nothing with a name dangling.
#[test]
fn images_are_filtered_as_docker_filters_them() {
    let tags = ["alpine:3.20".to_owned(), "localhost:5000/app:v1".to_owned()];
    let digests = ["alpine@sha256:ab".to_owned()];
    let labels = BTreeMap::from([("tier".to_owned(), "web".to_owned())]);
    let passes = |filters: &str| {
        ImageFilters::parse(json(filters))
            .unwrap()
            .passes(&tags, &digests, &labels)
    };
    assert!(passes(r#"{"reference":["alpine"]}"#));
    assert!(passes(r#"{"reference":["alp*:3.*"]}"#));
    assert!(passes(r#"{"reference":["localhost:5000/app"]}"#));
    assert!(!passes(r#"{"reference":["busybox"]}"#));
    assert!(passes(r#"{"label":["tier=web"]}"#));
    assert!(!passes(r#"{"label":["tier=db"]}"#));
    assert!(passes(r#"{"dangling":["false"]}"#));
    assert!(!passes(r#"{"dangling":["true"]}"#));
    let by_digest_only = ImageFilters::parse(json(r#"{"dangling":["true"]}"#))
        .unwrap()
        .passes(&[], &digests, &labels);
    assert!(!by_digest_only, "a digest is a name");
    for refused in [
        r#"{"reference":["alpine:3.[0-9]*"]}"#,
        r#"{"before":["alpine"]}"#,
        r#"{"dangling":["maybe"]}"#,
    ] {
        assert_eq!(
            ImageFilters::parse(json(refused)).unwrap_err().kind(),
            ErrorKind::InvalidRequest,
            "{refused}"
        );
    }
}

/// Volumes: by what the name contains, labels, dangling, the driver.
#[test]
fn volumes_are_filtered_as_docker_filters_them() {
    let volume = super::super::Volume {
        created_at: String::new(),
        driver: "local".to_owned(),
        labels: Some(BTreeMap::from([("tier".to_owned(), "db".to_owned())])),
        mountpoint: String::new(),
        name: "app_data".to_owned(),
        options: None,
        scope: "local".to_owned(),
    };
    let passes = |filters: &str, mounted: bool| {
        VolumeFilters::parse(json(filters))
            .unwrap()
            .pass(&volume, mounted)
    };
    assert!(
        VolumeFilters::parse(json(r#"{"dangling":["true"]}"#))
            .unwrap()
            .wants_mounted()
    );
    assert!(
        !VolumeFilters::parse(json(r#"{"name":["x"]}"#))
            .unwrap()
            .wants_mounted(),
        "no dangling, no need to read what's used"
    );
    assert!(passes(r#"{"name":["data"]}"#, false));
    assert!(!passes(r#"{"name":["cache"]}"#, false));
    assert!(passes(r#"{"label":["tier=db"]}"#, false));
    assert!(
        !passes(r#"{"label":["tier=db","zone=a"]}"#, false),
        "every label asked for, as Docker's"
    );
    assert!(
        passes(r#"{"name":["data","cache"]}"#, false),
        "one name of them"
    );
    assert!(passes(r#"{"dangling":["true"]}"#, false));
    assert!(!passes(r#"{"dangling":["true"]}"#, true));
    assert!(passes(r#"{"dangling":["false"]}"#, true));
    assert!(passes(r#"{"driver":["local"]}"#, false));
    assert!(!passes(r#"{"driver":["nfs"]}"#, false));
    for refused in [r#"{"scope":["local"]}"#, r#"{"dangling":["yes"]}"#] {
        assert!(VolumeFilters::parse(json(refused)).is_err(), "{refused}");
    }
}

/// Networks: by name, ID, label, driver, built-in or custom, and dangling
/// (F120: not in use, never a built-in, as Docker's `filterNetworkByUse`).
#[test]
fn networks_are_filtered_as_docker_filters_them() {
    let labels = BTreeMap::from([("tier".to_owned(), "web".to_owned())]);
    let passes = |filters: &str, network: [&str; 3]| {
        NetworkFilters::parse(json(filters))
            .unwrap()
            .passes(network, &labels, false)
    };
    let used = |filters: &str, network: [&str; 3], in_use: bool| {
        NetworkFilters::parse(json(filters))
            .unwrap()
            .passes(network, &labels, in_use)
    };
    let app = ["app_net", "ab12cd", "bridge"];
    let host = ["host", "ff00", "host"];
    let default = ["bridge", "ee00", "bridge"];
    assert!(passes(r#"{"name":["net"]}"#, app));
    assert!(passes(r#"{"id":["ab1"]}"#, app));
    assert!(!passes(r#"{"id":["b1"]}"#, app), "the start of it only");
    assert!(passes(r#"{"label":["tier=web"]}"#, app));
    assert!(passes(r#"{"driver":["bridge"]}"#, app));
    assert!(passes(r#"{"type":["custom"]}"#, app));
    assert!(!passes(r#"{"type":["custom"]}"#, host));
    assert!(passes(r#"{"type":["builtin"]}"#, host));
    assert!(
        passes(r#"{"type":["builtin"]}"#, default),
        "the default network"
    );
    assert!(!passes(r#"{"type":["custom"]}"#, default));
    let dangling = r#"{"dangling":["true"]}"#;
    let kept = r#"{"dangling":["false"]}"#;
    assert!(used(dangling, app, false), "nothing up on it");
    assert!(!used(dangling, app, true), "a container up on it");
    assert!(!used(dangling, default, false), "a built-in never is");
    assert!(!used(dangling, host, false), "a built-in never is");
    assert!(used(kept, app, true));
    assert!(!used(kept, app, false));
    assert!(used(kept, default, false), "a built-in always is kept");
    assert!(
        NetworkFilters::parse(json(dangling))
            .unwrap()
            .wants_in_use()
    );
    assert!(
        !NetworkFilters::parse(json(r#"{"name":["a"]}"#))
            .unwrap()
            .wants_in_use()
    );
    for refused in [r#"{"dangling":["maybe"]}"#, r#"{"type":["other"]}"#] {
        assert!(NetworkFilters::parse(json(refused)).is_err(), "{refused}");
    }
}

/// A prune takes only anonymous volumes unless `all`; labels narrow
/// it, `label!` keeps.
#[test]
fn a_prune_takes_what_docker_s_takes() {
    let plain = BTreeMap::from([("tier".to_owned(), "db".to_owned())]);
    let anonymous = BTreeMap::from([(ANONYMOUS_VOLUME.to_owned(), String::new())]);
    let wants = |filters: &str, labels| PruneFilters::parse(json(filters)).unwrap().wants(labels);
    assert!(!wants("", &plain), "named: not without all");
    assert!(wants("", &anonymous));
    assert!(wants(r#"{"all":["true"]}"#, &plain));
    assert!(wants(r#"{"all":["true"],"label":["tier=db"]}"#, &plain));
    assert!(!wants(r#"{"all":["true"],"label":["tier=web"]}"#, &plain));
    assert!(!wants(r#"{"all":["true"],"label!":["tier"]}"#, &plain));
    assert!(PruneFilters::parse(json(r#"{"until":["1h"]}"#)).is_err());
}

/// `reference` is Go's `path.Match`: `*` stops at a `/`.
#[test]
fn a_reference_pattern_is_a_glob() {
    assert!(glob("alpine", "alpine"));
    assert!(glob("alp*", "alpine"));
    assert!(glob("alpine:3*", "alpine:3.20"));
    assert!(glob("a?pine", "alpine"));
    assert!(glob("*", "alpine"));
    assert!(glob("owner/*", "owner/app"));
    assert!(!glob("*", "owner/app"), "a star doesn't cross a slash");
    assert!(!glob("alp", "alpine"));
    assert!(!glob("alpine", "alp"));
    assert!(!glob("?", ""));
    assert!(glob("", ""));
    assert!(glob("*/*:v?", "owner/app:v1"));
}

#[test]
fn both_of_docker_s_forms_are_read() {
    let list =
        ContainerFilters::parse(json(r#"{"label":["app=web"],"status":["running"]}"#)).unwrap();
    let set = ContainerFilters::parse(json(
        r#"{"label":{"app=web":true,"off":false},"status":{"running":true}}"#,
    ))
    .unwrap();
    assert_eq!(list, set);
    assert_eq!(
        ContainerFilters::parse(Given::default()).unwrap(),
        ContainerFilters::default()
    );
    assert_eq!(
        ContainerFilters::parse(json("")).unwrap(),
        ContainerFilters::default()
    );
}

/// A key not read yet is refused, never ignored; so is a bad state.
#[test]
fn what_can_t_be_filtered_is_refused() {
    for text in [
        r#"{"publish":["80"]}"#,
        r#"{"exited":["one"]}"#,
        r#"{"health":["fine"]}"#,
        r#"{"status":["sleeping"]}"#,
        "not json",
        r#"{"label":"app"}"#,
    ] {
        let refused = Given::from_json(Some(text))
            .and_then(ContainerFilters::parse)
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidRequest, "{text}");
    }
}

/// Both forms flattened to the engine's `KEY=VALUE`s; nothing for none.
#[test]
fn filters_are_flattened_for_the_engine() {
    assert_eq!(
        json(r#"{"type":["container"],"event":{"die":true,"start":false}}"#).flattened(),
        vec!["event=die".to_owned(), "type=container".to_owned()]
    );
    assert!(Given::from_json(None).unwrap().flattened().is_empty());
    assert!(json(" ").flattened().is_empty());
    assert_eq!(
        Given::from_json(Some("[1]")).unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
}

#[test]
fn names_match_as_docker_s_patterns() {
    assert!(name_matches("/web", "web"));
    assert!(name_matches("/my-web-1", "web"));
    assert!(name_matches("/web", "^/web$"));
    assert!(name_matches("/web", "^web$"));
    assert!(!name_matches("/web-1", "^web$"));
    assert!(name_matches("/web-1", "^web"));
    assert!(name_matches("/my-web", "web$"));
    assert!(!name_matches("/db", "web"));
}

#[test]
fn labels_match_by_key_or_by_key_and_value() {
    let labels = BTreeMap::from([("app".to_owned(), "web".to_owned())]);
    assert!(label_matches(&labels, "app"));
    assert!(label_matches(&labels, "app=web"));
    assert!(!label_matches(&labels, "app=db"));
    assert!(!label_matches(&labels, "tier"));
    assert!(!label_matches(&labels, "app=web=1"));
}

/// A container as a filter sees it: `web`, from `alpine:3.20`, on `net`
/// (ID `abcd…`), with the volume `data` at `/data`, exited 3.
fn exited_web() -> (ContainerSummary, Facts<'static>) {
    let endpoint = super::super::EndpointSettings {
        network_id: "abcdef0123".to_owned(),
        ..Default::default()
    };
    let summary = ContainerSummary {
        id: "0123456789ab".repeat(5),
        names: vec!["/web".to_owned()],
        image: "alpine:3.20".to_owned(),
        image_id: "sha256:fedcba9876".to_owned(),
        command: "sh".to_owned(),
        created: 0,
        ports: Vec::new(),
        labels: BTreeMap::from([("tier".to_owned(), "web".to_owned())]),
        state: "exited",
        status: "Exited (3) 1 second ago".to_owned(),
        host_config: super::super::SummaryHostConfig {
            network_mode: "net".to_owned(),
        },
        network_settings: super::super::SummaryNetworks {
            networks: BTreeMap::from([("net".to_owned(), endpoint)]),
        },
        mounts: vec![super::super::MountPoint {
            kind: "volume",
            name: "data".to_owned(),
            source: String::new(),
            destination: "/data".to_owned(),
            mode: String::new(),
            rw: true,
        }],
        health: "none",
    };
    let facts = Facts {
        exit_code: Some(3),
        ancestors: &[],
    };
    (summary, facts)
}

/// Docker's other container keys: the image it runs, how it ended, its
/// health, its networks and volumes; each with a value that doesn't
/// match beside it.
#[test]
fn containers_are_filtered_as_docker_filters_them() {
    let (summary, facts) = exited_web();
    let passes = |filters: &str| {
        ContainerFilters::parse(json(filters))
            .unwrap()
            .pass(&summary, facts)
    };
    for (yes, no) in [
        (
            r#"{"ancestor":["alpine:3.20"]}"#,
            r#"{"ancestor":["alpine"]}"#,
        ),
        (r#"{"ancestor":["sha256:fedc"]}"#, r#"{"ancestor":["fed"]}"#),
        (r#"{"ancestor":["fedcba"]}"#, r#"{"ancestor":["abcd"]}"#),
        (r#"{"exited":["3"]}"#, r#"{"exited":["0"]}"#),
        (r#"{"health":["none"]}"#, r#"{"health":["healthy"]}"#),
        (r#"{"network":["net"]}"#, r#"{"network":["other"]}"#),
        (r#"{"network":["abcd"]}"#, r#"{"network":["bcd"]}"#),
        (r#"{"volume":["data"]}"#, r#"{"volume":["cache"]}"#),
        (r#"{"volume":["/data"]}"#, r#"{"volume":["/cache"]}"#),
        (r#"{"status":["exited"]}"#, r#"{"status":["running"]}"#),
        (r#"{"name":["web"]}"#, r#"{"name":["db"]}"#),
        (r#"{"id":["0123"]}"#, r#"{"id":["1234"]}"#),
        (r#"{"label":["tier=web"]}"#, r#"{"label":["tier=db"]}"#),
    ] {
        assert!(passes(yes), "{yes}");
        assert!(!passes(no), "{no}");
    }
    // The image the store has under a name asked for (`ancestor=base`,
    // where `base` is another name of `alpine:3.20`'s image).
    let resolved = ["sha256:fedcba9876".to_owned()];
    let by_store = ContainerFilters::parse(json(r#"{"ancestor":["base"]}"#)).unwrap();
    assert!(!by_store.pass(&summary, facts));
    assert!(by_store.pass(
        &summary,
        Facts {
            ancestors: &resolved,
            ..facts
        }
    ));
    // `before` and `since` are the list's to decide.
    assert!(passes(r#"{"since":["anything"]}"#));
    let tagless = ContainerSummary {
        image: "alpine".to_owned(),
        ..summary.clone()
    };
    let latest = ContainerFilters::parse(json(r#"{"ancestor":["alpine:latest"]}"#)).unwrap();
    assert!(latest.pass(&tagless, facts), "a tag left out is latest");
    let running = ContainerFilters::parse(json(r#"{"exited":["3"]}"#)).unwrap();
    let up = ContainerSummary {
        state: "running",
        ..summary.clone()
    };
    assert!(!running.pass(&up, facts), "exited names ended ones only");
}

/// `status` and `exited` look at every container, as `-a` does; nothing
/// else does.
#[test]
fn an_ended_state_asked_for_lists_all() {
    for (text, all) in [
        (r#"{"status":["exited"]}"#, true),
        (r#"{"exited":["0"]}"#, true),
        (r#"{"name":["web"]}"#, false),
        ("{}", false),
    ] {
        assert_eq!(
            ContainerFilters::parse(json(text)).unwrap().wants_all(),
            all,
            "{text}"
        );
    }
    let bounds = ContainerFilters::parse(json(r#"{"before":["a"],"since":["b","c"]}"#)).unwrap();
    assert_eq!(
        bounds.bounds().collect::<Vec<_>>(),
        [("before", "a"), ("since", "b"), ("since", "c")]
    );
}

/// The CLI's `-f KEY=VALUE`s are the same filters as Docker's JSON.
#[test]
fn pairs_are_the_json_s_filters() {
    let pairs = Given::from_pairs([
        ("label".to_owned(), "a=b".to_owned()),
        ("status".to_owned(), "running".to_owned()),
        ("label".to_owned(), "c".to_owned()),
    ]);
    assert_eq!(pairs, json(r#"{"label":["a=b","c"],"status":["running"]}"#));
    assert_eq!(
        ContainerFilters::parse(Given::from_pairs([("bogus".to_owned(), "1".to_owned())]))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidRequest
    );
}

/// A name filter is Docker's regular expression where Shipyard can read it
/// (text, `^`, `$`, `.` any one character); the rest is refused, never
/// matched as text. `before` and `since` name something.
#[test]
fn a_name_pattern_is_read_or_refused() {
    assert!(name_matches("/web.1", "web.1"));
    assert!(name_matches("/webx1", "web.1"), "`.` is any character");
    assert!(name_matches("/web-1", "^web.1$"));
    assert!(!name_matches("/web-12", "^web.1$"));
    assert!(!name_matches("/we", "web"));
    for refused in [
        r#"{"name":["web.*"]}"#,
        r#"{"name":["(a|b)"]}"#,
        r#"{"name":["web[0-9]"]}"#,
        r#"{"before":[""]}"#,
        r#"{"since":[""]}"#,
    ] {
        assert_eq!(
            Given::from_json(Some(refused))
                .and_then(ContainerFilters::parse)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidRequest,
            "{refused}"
        );
    }
}

/// Docker's `until`: a time, or a Go duration counted back from now;
/// anything else refused.
#[test]
fn until_is_a_time_or_a_go_duration() {
    let now = 1_792_504_484;
    assert_eq!(until("1760000000", now).unwrap(), 1_760_000_000);
    assert_eq!(until("2025-10-09T08:53:20Z", now).unwrap(), 1_760_000_000);
    assert_eq!(until("10m", now).unwrap(), now - 600);
    assert_eq!(until("1h30m", now).unwrap(), now - 5_400);
    assert_eq!(until("1.5h", now).unwrap(), now - 5_400);
    assert_eq!(until("90s", now).unwrap(), now - 90);
    assert_eq!(until("1500ms", now).unwrap(), now - 1);
    for refused in ["", "h", "-10m", "10x", "1h-", "yesterday", "1000000h"] {
        assert_eq!(
            until(refused, now).unwrap_err().kind(),
            ErrorKind::InvalidRequest,
            "{refused:?}"
        );
    }
}

/// A prune's filters: made before `until`, every `label`, no `label!`;
/// an unknown key refused.
#[test]
fn prunes_take_what_docker_s_take() {
    let now = 1_792_504_484;
    let parse = |text: &str| ContainerPruneFilters::parse(json(text), now).unwrap().0;
    let labels = BTreeMap::from([("tier".to_owned(), "web".to_owned())]);
    let old = now - 7_200;
    assert!(parse("{}").wants(now, &labels));
    assert!(parse(r#"{"until":["1h"]}"#).wants(old, &labels));
    assert!(!parse(r#"{"until":["1h"]}"#).wants(now - 60, &labels));
    assert!(
        parse(r#"{"until":["1h"]}"#).wants(now - 3_600, &labels),
        "made at `until` itself: Docker keeps only what's made after"
    );
    assert!(parse(r#"{"label":["tier=web"]}"#).wants(old, &labels));
    assert!(!parse(r#"{"label":["tier=db"]}"#).wants(old, &labels));
    assert!(!parse(r#"{"label!":["tier"]}"#).wants(old, &labels));
    assert!(parse(r#"{"label!":["tier=db"]}"#).wants(old, &labels));
    let labelled =
        ContainerPruneFilters::parse(json(r#"{"until":["1h"],"label":["tier"]}"#), now).unwrap();
    assert!(labelled.wants_labels(&labels), "until isn't a label's");
    assert!(!labelled.wants_labels(&BTreeMap::new()));
    for refused in [r#"{"dangling":["true"]}"#, r#"{"status":["exited"]}"#] {
        assert_eq!(
            ContainerPruneFilters::parse(json(refused), now)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidRequest,
            "{refused}"
        );
    }
}

/// `image prune`: dangling ones only by default, every one with
/// `dangling=false` (`-a`), by the image's own time and labels.
#[test]
fn image_prunes_take_dangling_ones_unless_asked_for_all() {
    let now = 1_792_504_484;
    let image = |tags: &[&str]| ImageSummary {
        id: "sha256:ab".to_owned(),
        parent_id: String::new(),
        repo_tags: tags.iter().map(|&tag| tag.to_owned()).collect(),
        repo_digests: Vec::new(),
        created: now - 7_200,
        size: 0,
        shared_size: -1,
        virtual_size: 0,
        labels: Some(BTreeMap::from([("tier".to_owned(), "web".to_owned())])),
        containers: -1,
    };
    let wants = |text: &str, tags: &[&str]| {
        ImagePruneFilters::parse(json(text), now)
            .unwrap()
            .wants(&image(tags))
    };
    assert!(wants("{}", &[]), "a dangling one");
    let mut digest_only = image(&[]);
    digest_only.repo_digests = vec!["alpine@sha256:ab".to_owned()];
    assert!(
        ImagePruneFilters::parse(json("{}"), now)
            .unwrap()
            .wants(&digest_only),
        "a digest isn't a tag: dangling, as Docker's prune reads it"
    );
    assert!(!wants("{}", &["alpine:3"]), "a named one isn't dangling");
    assert!(wants(r#"{"dangling":["false"]}"#, &["alpine:3"]));
    assert!(!wants(
        r#"{"dangling":["false"],"until":["3h"]}"#,
        &["alpine:3"]
    ));
    assert!(!wants(
        r#"{"dangling":["false"],"label":["tier=db"]}"#,
        &["alpine:3"]
    ));
    assert_eq!(
        ImagePruneFilters::parse(json(r#"{"dangling":["no"]}"#), now)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidRequest
    );
}
