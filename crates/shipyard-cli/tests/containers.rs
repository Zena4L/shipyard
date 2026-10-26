//! End-to-end containers by name (stage 9, step 9.4a): `create`, `start`,
//! `run -d`, `ps`, `inspect` and `rm`, through the real `shipyard` binary,
//! with an image from the fixture registry.
//!
//! Each test has stores of its own (images, containers, networks and, for a
//! normal user, its runtime directory), and its containers have no network
//! (the engine's own tests cover networks). Linux only; a host where an
//! image can't run (rootless, without overlay) says why and returns.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use shipyard_test_support::{Shipyard, is_root};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// One test's stores and fixture image: test-support's, with what only
/// these tests do.
struct Stores(shipyard_test_support::stores::Stores);

impl std::ops::Deref for Stores {
    type Target = shipyard_test_support::stores::Stores;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Stores {
    /// Its stores, if an image runs here (else it says why).
    fn new() -> Option<Self> {
        shipyard_test_support::stores::Stores::new(SHIPYARD).map(Self)
    }

    /// Stores whose image has `runtime` as its config's `config` (its
    /// `Entrypoint`, `Cmd`, `Env`…).
    fn with_image_config(runtime: &str) -> Option<Self> {
        shipyard_test_support::stores::Stores::serving(
            SHIPYARD,
            shipyard_test_support::registry::fixture_with_config(runtime),
        )
        .map(Self)
    }

    /// Stores whose image has the layer `extra` over the fixture's, and
    /// `runtime` as its config's `config`.
    fn with_layer(extra: (Vec<u8>, String), runtime: &str) -> Option<Self> {
        shipyard_test_support::stores::Stores::serving(
            SHIPYARD,
            shipyard_test_support::registry::fixture_with_layer(extra, runtime),
        )
        .map(Self)
    }

    /// `logs -f REFERENCE`, running in the background: it, and the next
    /// line it prints (waited for, up to 20 s).
    fn follow(&self, reference: &str) -> (std::process::Child, impl Fn() -> String) {
        use std::io::BufRead as _;
        let mut follow = self
            .command(&["logs", "-f", reference])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let followed = std::io::BufReader::new(follow.stdout.take().unwrap());
        let (lines, read) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in followed.lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let next_line = move || read.recv_timeout(Duration::from_secs(20)).unwrap();
        (follow, next_line)
    }

    /// `attach REFERENCE`, typing `typed`, then (once `answered` returns)
    /// the detach keys: what it printed, and how it ended.
    fn attach_and_type(&self, reference: &str, typed: &[u8], answered: impl FnOnce()) -> Output {
        use std::io::Write as _;
        use std::process::Stdio;
        let mut attach = self
            .command(&["attach", reference])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = attach.stdin.take().unwrap();
        input.write_all(typed).unwrap();
        answered();
        input.write_all(b"\x10\x11").unwrap();
        drop(input);
        attach.wait_with_output().unwrap()
    }

    /// `inspect --shipyard REFERENCE`'s one object (Shipyard's own view).
    fn inspect(&self, reference: &str) -> serde_json::Value {
        let shown: serde_json::Value =
            serde_json::from_str(&self.ok(&["inspect", "--shipyard", reference])).unwrap();
        shown[0].clone()
    }

    /// `inspect REFERENCE`'s one object, as Docker shows it.
    fn docker_inspect(&self, reference: &str) -> serde_json::Value {
        let shown: serde_json::Value =
            serde_json::from_str(&self.ok(&["inspect", reference])).unwrap();
        shown[0].clone()
    }
}

/// Waits (up to 20 s) for `found`, naming `what` if it never comes.
fn eventually(what: &str, found: impl FnMut() -> bool) {
    shipyard_test_support::eventually(Duration::from_secs(20), what, found);
}

/// A container is made, started, listed, looked at and removed by its name,
/// its serial and its ID's prefix, each finding the same one; a running one
/// isn't removed without `-f`.
#[test]
fn a_container_is_found_by_name_serial_and_prefix() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "create",
            "--name",
            "web",
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    assert_eq!(id.len(), 64, "the full ID: {id}");
    let listed = stores.ok(&["ps", "-a"]);
    assert!(
        listed.contains("Created") && listed.contains("web"),
        "{listed}"
    );
    assert!(
        !stores.ok(&["ps"]).contains("web"),
        "not up: not without -a"
    );
    let shown = stores.inspect("web");
    assert_eq!(shown["id"], id.as_str());
    assert_eq!(shown["state"]["status"], "created");
    let serial = shown["serial"].as_str().unwrap().to_owned();
    assert_eq!(stores.inspect(&serial)["id"], id.as_str(), "by serial");
    assert_eq!(stores.inspect(&id[..8])["name"], "web", "by prefix");
    assert_eq!(
        stores.ok(&["start", &id[..8]]).trim(),
        &id[..8],
        "said as given"
    );
    // Docker's row, an object per line (step 11.3c).
    let listed = stores.ok(&["ps", "--format", "json"]);
    let up: serde_json::Value = serde_json::from_str(listed.lines().next().unwrap()).unwrap();
    assert_eq!(up["State"], "running", "{up}");
    assert_eq!(up["Names"], "web", "{up}");
    assert_eq!(up["ID"], &id[..12], "{up}");
    assert_eq!(stores.ok(&["ps", "-q"]).trim(), &id[..12]);
    // `inspect` itself is Docker's shape (step 11.1): what a client
    // written for `docker inspect` reads.
    let docker = stores.docker_inspect("web");
    assert_eq!(docker["Id"], id.as_str(), "{docker}");
    assert_eq!(docker["Name"], "/web");
    assert_eq!(docker["State"]["Status"], "running");
    assert_eq!(docker["State"]["Running"], true);
    assert!(docker["State"]["Pid"].as_i64().unwrap() > 0, "{docker}");
    assert_eq!(docker["Config"]["Image"], stores.image.as_str());
    assert_eq!(docker["Config"]["Cmd"][0], "sleep");
    assert_eq!(docker["HostConfig"]["NetworkMode"], "none");
    let refused = stores.shipyard(&["rm", "web"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is running"),
        "{refused:?}"
    );
    stores.ok(&["rm", "-f", &serial]);
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
    let gone = stores.shipyard(&["inspect", "web"]);
    assert_eq!(gone.status.code(), Some(1), "{gone:?}");
    assert_eq!(String::from_utf8_lossy(&gone.stdout).trim(), "[]");
}

/// The lists are Docker's (step 11.3c): `ps`'s columns, Go templates,
/// `json` an object per line, `-f` filters (a `status` looks at every
/// container, as `-a` does), `-q`, `--no-trunc`, `-n`; a bad template or
/// filter key fails with 1, a filter without `=` is a usage error (125).
#[test]
fn lists_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let made = |name: &str, label: &str, command: &str| {
        let output = stores.shipyard(&[
            "run",
            "--name",
            name,
            "--label",
            label,
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            command,
        ]);
        output.status.code()
    };
    assert_eq!(made("first", "tier=db", "exit 3"), Some(3));
    let up = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "second",
            "--label",
            "tier=web",
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    let header = stores.ok(&["ps"]);
    let header = header.lines().next().unwrap();
    let titles: Vec<&str> = header
        .split("   ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    assert_eq!(
        titles,
        [
            "CONTAINER ID",
            "IMAGE",
            "COMMAND",
            "CREATED",
            "STATUS",
            "PORTS",
            "NAMES"
        ],
        "{header}"
    );
    assert_eq!(
        stores.ok(&[
            "ps",
            "-a",
            "--format",
            "{{.Names}} {{.State}} {{.Label \"tier\"}}"
        ]),
        "second running web\nfirst exited db\n",
        "newest first"
    );
    let json = stores.ok(&["ps", "--format", "json"]);
    let row: serde_json::Value = serde_json::from_str(json.trim_end()).unwrap();
    assert_eq!(row["Names"], "second", "{json}");
    assert_eq!(row["Command"], "\"sleep 300\"", "{json}");
    assert_eq!(row["Labels"], "tier=web", "{json}");
    assert_eq!(row["ID"], &up[..12], "{json}");
    // Filters: a status asked for looks at every one; label, exited, since.
    let names = |args: &[&str]| {
        let mut argv = args.to_vec();
        argv.extend(["--format", "{{.Names}}"]);
        stores.ok(&argv)
    };
    assert_eq!(names(&["ps", "-f", "status=exited"]), "first\n");
    assert_eq!(names(&["ps", "-a", "-f", "exited=3"]), "first\n");
    assert_eq!(names(&["ps", "-a", "-f", "exited=0"]), "");
    assert_eq!(names(&["ps", "-a", "-f", "label=tier=web"]), "second\n");
    assert_eq!(names(&["ps", "-a", "-f", "since=first"]), "second\n");
    assert_eq!(names(&["ps", "-a", "-f", "before=second"]), "first\n");
    assert_eq!(names(&["ps", "-n", "1"]), "second\n", "-n: of all of them");
    assert_eq!(
        stores.ok(&["ps", "-q", "-f", "name=second"]).trim(),
        &up[..12]
    );
    assert_eq!(stores.ok(&["ps", "-q", "--no-trunc"]).trim(), up);
    // Failures, by Docker's codes.
    for (args, code) in [
        (&["ps", "--format", "{{.Names"][..], 1),
        (&["ps", "--format", "{{.Nope}}"], 1),
        (&["ps", "-f", "bogus=1"], 1),
        (&["ps", "-f", "since=nothing-here"], 1),
        (&["ps", "-f", "name"], 125),
    ] {
        let failed = stores.shipyard(args);
        assert_eq!(failed.status.code(), Some(code), "{args:?}: {failed:?}");
    }
    stores.ok(&["rm", "-f", "first", "second"]);
}

/// The lists' other flags, as Docker's: `-q` with a format warns that it
/// ignores it; `-l` the last made; a volume mounted shows in `Mounts`
/// and `LocalVolumes`; `images --digests` adds its column; `network ls -f`.
#[test]
fn list_flags_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    // Longer than 15: `Mounts` cuts it.
    let volume = format!("list-flags-volume-{}", std::process::id());
    let mount = format!("{volume}:/data");
    let id = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "mounted",
            "--network",
            "none",
            "-v",
            &mount,
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    let quiet = stores.shipyard(&["ps", "-q", "--format", "{{.Names}}"]);
    assert!(quiet.status.success(), "{quiet:?}");
    assert_eq!(String::from_utf8_lossy(&quiet.stdout).trim(), &id[..12]);
    assert!(
        String::from_utf8_lossy(&quiet.stderr).contains(
            "WARNING: Ignoring custom format, because both --format and --quiet are set."
        ),
        "{quiet:?}"
    );
    assert_eq!(
        stores.ok(&["ps", "-l", "--format", "{{.Names}} {{.LocalVolumes}}"]),
        "mounted 1\n"
    );
    let mounts = stores.ok(&["ps", "--format", "{{.Mounts}}"]);
    assert_eq!(mounts.trim().chars().count(), 15, "cut at 15: {mounts}");
    assert!(mounts.trim().ends_with('…'), "{mounts}");
    assert_eq!(
        stores
            .ok(&["ps", "--no-trunc", "--format", "{{.Mounts}}"])
            .trim(),
        volume
    );
    let digests = stores.ok(&["images", "--digests"]);
    let header = digests.lines().next().unwrap();
    assert!(
        header.starts_with("REPOSITORY") && header.contains("   DIGEST   "),
        "{header}"
    );
    assert_eq!(
        stores.ok(&[
            "network",
            "ls",
            "-f",
            "name=none",
            "--format",
            "{{.Name}} {{.Driver}}"
        ]),
        "none null\n"
    );
    stores.ok(&["rm", "-f", "mounted"]);
    stores.ok(&["volume", "rm", &volume]);
}

/// `inspect` finds any object as Docker's does (step 11.3c2): the first of
/// a container, an image, a network and a volume with the name, or the
/// `--type` asked; one found nowhere said, the rest shown, exit 1; `-f`
/// a template, `json` one array; `volume`/`network inspect -f`.
#[test]
fn inspect_finds_any_object() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let volume = format!("inspected-{}", std::process::id());
    stores.ok(&["volume", "create", "--label", "k=v", &volume]);
    stores.ok(&[
        "create",
        "--name",
        "made",
        "--network",
        "none",
        &stores.image,
        "true",
    ]);
    let names = [stores.image.as_str(), "made", "none", volume.as_str()];
    let mut argv = vec!["inspect", "-f", "json"];
    argv.extend(names);
    let found: serde_json::Value = serde_json::from_str(&stores.ok(&argv)).unwrap();
    // Each by its kind's own key: an image, a container, a network, a
    // volume, in the order asked.
    for (at, key) in ["RepoTags", "State", "IPAM", "Mountpoint"]
        .iter()
        .enumerate()
    {
        assert!(found[at].get(key).is_some(), "{at} has {key}: {found}");
    }
    let mixed = stores.shipyard(&["inspect", "-f", "{{.Name}}", "nothing-here", &volume]);
    assert_eq!(mixed.status.code(), Some(1), "{mixed:?}");
    assert_eq!(
        String::from_utf8_lossy(&mixed.stdout),
        format!("{volume}\n")
    );
    assert!(
        String::from_utf8_lossy(&mixed.stderr).contains("no such object: nothing-here"),
        "{mixed:?}"
    );
    let typed = stores.shipyard(&["inspect", "--type", "volume", "made"]);
    assert_eq!(typed.status.code(), Some(1), "{typed:?}");
    // A container and a volume of one name: the container first (Docker's
    // order); `container inspect` finds containers only, `volume inspect`
    // the volume.
    stores.ok(&["volume", "create", "made"]);
    let kind = |argv: &[&str]| {
        let found: serde_json::Value = serde_json::from_str(&stores.ok(argv)).unwrap();
        ["State", "Mountpoint"]
            .into_iter()
            .find(|key| found[0].get(key).is_some())
    };
    assert_eq!(kind(&["inspect", "made"]), Some("State"));
    assert_eq!(kind(&["container", "inspect", "made"]), Some("State"));
    assert_eq!(kind(&["volume", "inspect", "made"]), Some("Mountpoint"));
    assert_eq!(
        kind(&["inspect", "--type", "volume", "made"]),
        Some("Mountpoint")
    );
    let not_a_container = stores.shipyard(&["container", "inspect", &volume]);
    assert_eq!(
        not_a_container.status.code(),
        Some(1),
        "a volume isn't a container: {not_a_container:?}"
    );
    let no_type = stores.shipyard(&["container", "inspect", "--type", "volume", "made"]);
    assert_eq!(
        no_type.status.code(),
        Some(125),
        "Docker's has no --type there"
    );
    stores.ok(&["volume", "rm", "made"]);
    let json = stores.ok(&["inspect", "-f", "json", &volume, "none"]);
    assert_eq!(json.lines().count(), 1, "one compact array: {json}");
    let array: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(array.as_array().map(Vec::len), Some(2));
    let pretty = stores.ok(&["inspect", &volume]);
    assert!(
        pretty.contains("\n        \"Driver\": \"local\""),
        "{pretty}"
    );
    let missing = stores.shipyard(&["inspect", "-f", "{{.Labels.nope}}", &volume]);
    assert_eq!(missing.status.code(), Some(1), "D11.3c2-2: {missing:?}");
    assert_eq!(
        stores.ok(&[
            "volume",
            "inspect",
            "-f",
            "{{.Name}} {{json .Labels}}",
            &volume
        ]),
        format!("{volume} {{\"k\":\"v\"}}\n")
    );
    assert_eq!(
        stores.ok(&["network", "inspect", "-f", "{{.Name}} {{.Driver}}", "none"]),
        "none null\n"
    );
    stores.ok(&["rm", "made"]);
    stores.ok(&["volume", "rm", &volume]);
}

/// `version` and `info` are Docker's text, and their objects for a
/// template (step 11.3c2).
#[test]
fn version_and_info_say_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "create",
        "--name",
        "made",
        "--network",
        "none",
        &stores.image,
        "true",
    ]);
    let version = stores.ok(&["version"]);
    assert!(version.starts_with("Client:\n Version:"), "{version}");
    assert!(version.contains("\nServer: Shipyard\n"), "{version}");
    assert_eq!(
        stores.ok(&["version", "--format", "{{.Server.APIVersion}}"]),
        "1.43\n"
    );
    let info = stores.ok(&["info"]);
    assert!(info.contains("\n Containers: 1\n"), "{info}");
    assert!(info.contains("\n Storage Driver: overlay\n"), "{info}");
    assert_eq!(
        stores.ok(&["info", "--format", "{{.Containers}} {{.ContainersStopped}}"]),
        "1 1\n"
    );
    stores.ok(&["rm", "made"]);
}

/// `system df` counts what's in use, as Docker's: an image a container
/// runs (once, however many names it has), a container running, a volume a
/// container names; `info` counts images as `df` does.
#[test]
fn system_df_counts_what_is_in_use() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let registry = stores.image.rsplit_once('/').unwrap().0;
    stores.ok(&["tag", &stores.image, &format!("{registry}/second:name")]);
    let volume = format!("df-{}", std::process::id());
    stores.ok(&["volume", "create", "df-unused"]);
    let mount = format!("{volume}:/data");
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "busy",
        "--network",
        "none",
        "-v",
        &mount,
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.ok(&[
        "create",
        "--name",
        "idle",
        "--network",
        "none",
        &stores.image,
        "true",
    ]);
    let df = stores.ok(&[
        "system",
        "df",
        "--format",
        "{{.Type}}={{.TotalCount}}/{{.Active}}",
    ]);
    assert_eq!(
        df, "Images=1/1\nContainers=2/1\nLocal Volumes=2/1\nBuild Cache=0/0\n",
        "{df}"
    );
    assert_eq!(stores.ok(&["info", "--format", "{{.Images}}"]), "1\n");
    stores.ok(&["rm", "-f", "busy", "idle"]);
    stores.ok(&["volume", "rm", &volume, "df-unused"]);
}

/// `--tmpfs` and `--mount`'s three types, as Docker's: a tmpfs of 1 MiB
/// (`noexec,nosuid,nodev`, its size kept: 2 MiB doesn't fit, 512 KiB
/// does), one of mode 1770, a read-only bind (read, not written), a named
/// volume; `inspect` shows the tmpfs.
#[test]
fn tmpfs_and_mounts_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let shared = stores.scratch.0.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("file"), "from the host\n").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(shared.join("file"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let write = |count: u32, path: &str| {
        format!(
            "(i=0; while [ $i -lt {count} ]; do echo LINE || exit 1; i=$((i+1)); done) > {path} 2>/dev/null"
        )
    };
    let script = format!(
        "cat /proc/mounts; cat /d/file; \
         (touch /d/new 2>/dev/null && echo bind-written) || echo bind-read-only; \
         echo kept > /v/x && echo volume-written; \
         ({small} && echo small-fits) || echo small-refused; \
         ({big} && echo big-fits) || echo big-refused",
        small = write(2048, "/scratch/small"),
        big = write(8192, "/scratch/big"),
    )
    .replace("LINE", "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let shared_mount = format!("type=bind,source={},target=/d,readonly", shared.display());
    let out = stores.ok(&[
        "run",
        "--name",
        "mounted",
        "--network",
        "none",
        "--tmpfs",
        "/scratch:size=1m",
        "--mount",
        "type=tmpfs,target=/t,tmpfs-mode=1770",
        "--mount",
        &shared_mount,
        "--mount",
        "type=volume,source=named,target=/v",
        &stores.image,
        "sh",
        "-c",
        &script,
    ]);
    let scratch = out
        .lines()
        .find(|line| line.contains(" /scratch "))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(scratch.starts_with("tmpfs /scratch tmpfs "), "{scratch}");
    for option in ["nosuid", "nodev", "noexec", "size=1024k"] {
        assert!(scratch.contains(option), "{option}: {scratch}");
    }
    let t = out.lines().find(|line| line.contains(" /t ")).unwrap();
    assert!(t.contains("mode=1770"), "{t}");
    for said in [
        "from the host",
        "bind-read-only",
        "volume-written",
        "small-fits",
        "big-refused",
    ] {
        assert!(out.lines().any(|line| line == said), "{said}: {out}");
    }
    assert!(
        !shared.join("new").exists(),
        "nothing written through the bind"
    );
    let inspected = stores.docker_inspect("mounted");
    assert_eq!(
        inspected["HostConfig"]["Tmpfs"],
        serde_json::json!({"/scratch": "size=1m", "/t": "mode=1770"})
    );
    assert_eq!(
        inspected["HostConfig"]["Binds"],
        serde_json::json!([format!("{}:/d:ro", shared.display()), "named:/v"])
    );
}

/// The anonymous volumes among `stores`' volumes, sorted.
fn anonymous_volumes(stores: &Stores) -> Vec<String> {
    let mut names: Vec<String> = stores
        .ok(&["volume", "ls", "-q"])
        .lines()
        .filter(|name| shipyard_test_support::is_anonymous_volume(name))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// Docker's anonymous volumes, made and kept: `-v /data` gets a new
/// volume, labelled anonymous, the same one at each start (what's written
/// there is read back after a second start); a plain `rm` keeps it.
#[test]
fn anonymous_volumes_are_made_and_kept() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let script = "cat /data/f 2>/dev/null; echo again >> /data/f";
    stores.ok(&[
        "run",
        "--name",
        "kept",
        "--network",
        "none",
        "-v",
        "/data",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let kept = anonymous_volumes(&stores);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let labels = stores.ok(&["volume", "inspect", "-f", "{{json .Labels}}", &kept[0]]);
    assert_eq!(labels.trim(), r#"{"com.docker.volume.anonymous":""}"#);
    assert_eq!(
        stores.ok(&["start", "-a", "kept"]),
        "again\n",
        "the same volume at its next start"
    );
    assert_eq!(anonymous_volumes(&stores), kept, "no other made");
    stores.ok(&["rm", "kept"]);
    assert_eq!(anonymous_volumes(&stores), kept, "kept by a plain rm");
    stores.ok(&["volume", "rm", &kept[0]]);
}

/// `rm -v` takes the anonymous volumes a container's create made, but one
/// another container names; a volume it was given by name (an anonymous
/// one too) or a named one is never taken; `--rm` takes its own at its
/// end, there while it runs.
#[test]
fn anonymous_volumes_go_with_rm_v_and_dash_dash_rm() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&["volume", "create", "named"]);
    // An anonymous volume of its own, given by name below.
    let given = stores.ok(&["volume", "create"]);
    let given = given.trim();
    let out = stores.ok(&[
        "run",
        "--name",
        "both",
        "--network",
        "none",
        "-v",
        "/a",
        "--mount",
        "type=volume,target=/b,readonly",
        "-v",
        "named:/n",
        "-v",
        &format!("{given}:/g"),
        &stores.image,
        "sh",
        "-c",
        "touch /a/x && (touch /b/x 2>/dev/null || echo b-read-only)",
    ]);
    assert_eq!(out, "b-read-only\n");
    let made: Vec<String> = anonymous_volumes(&stores)
        .into_iter()
        .filter(|name| name != given)
        .collect();
    assert_eq!(made.len(), 2, "{made:?}");
    stores.ok(&[
        "create",
        "--name",
        "sharer",
        "-v",
        &format!("{}:/s", made[0]),
        &stores.image,
    ]);
    stores.ok(&["rm", "-v", "both"]);
    let left = anonymous_volumes(&stores);
    assert!(
        left.contains(&made[0]),
        "another container names it: {left:?}"
    );
    assert!(!left.contains(&made[1]), "its own, unused: {left:?}");
    assert!(
        left.iter().any(|name| name == given),
        "given by name: {left:?}"
    );
    assert!(
        stores
            .ok(&["volume", "ls", "-q"])
            .lines()
            .any(|name| name == "named")
    );
    // `sharer` was given it by name: kept, as Docker keeps it.
    stores.ok(&["rm", "-v", "sharer"]);
    assert!(anonymous_volumes(&stores).contains(&made[0]));
    stores.ok(&["volume", "rm", &made[0], given]);
    stores.ok(&[
        "run",
        "-d",
        "--rm",
        "--name",
        "brief",
        "--network",
        "none",
        "-v",
        "/data",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(anonymous_volumes(&stores).len(), 1, "there while it runs");
    stores.ok(&["stop", "-t", "1", "brief"]);
    eventually("--rm's volume removed", || {
        anonymous_volumes(&stores).is_empty()
    });
    stores.ok(&["volume", "rm", "named"]);
}

/// An image's own `VOLUME`s get an anonymous volume each, but where a mount
/// covers the place (`-v`, `--tmpfs`), or the place is the runtime's or the
/// run's own (`/proc`, `/dev`, `/etc/hosts`: said, kept in the container).
#[test]
fn an_image_s_volumes_are_anonymous_ones() {
    let Some(stores) = Stores::with_image_config(
        r#"{"Cmd": ["sh"], "Volumes": {"/var/data": {}, "/covered/": {}, "/scratch": {},
            "/proc/x": {}, "/dev/shm": {}, "/etc/hosts": {}}}"#,
    ) else {
        return;
    };
    let created = stores
        .command(&[
            "create",
            "--name",
            "declared",
            "-v",
            "named:/covered",
            "--tmpfs",
            "/scratch",
            &stores.image,
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let said = String::from_utf8_lossy(&created.stderr);
    for place in ["/proc/x", "/dev/shm", "/etc/hosts"] {
        assert!(
            said.contains(&format!(
                "the image's volume {place} is kept in the container instead"
            )),
            "{place}: {said}"
        );
    }
    let made = anonymous_volumes(&stores);
    assert_eq!(made.len(), 1, "{made:?}");
    let binds = stores.ok(&["inspect", "-f", "{{json .HostConfig.Binds}}", "declared"]);
    assert!(binds.contains(&format!("{}:/var/data", made[0])), "{binds}");
    stores.ok(&["rm", "-v", "declared"]);
    assert!(anonymous_volumes(&stores).is_empty());
}

/// An image's `/data` for the fills: owned by 1000 and 02750, a file
/// (1000, 0640), a link to it, a directory of root's with a file.
fn data_layer() -> (Vec<u8>, String) {
    use shipyard_test_support::registry::{LayerEntry, built_layer};
    built_layer(&[
        LayerEntry::Dir {
            path: "data",
            owner: (1000, 1000),
            mode: 0o2750,
        },
        LayerEntry::File {
            path: "data/f",
            owner: (1000, 1000),
            mode: 0o640,
            content: b"from the image\n",
        },
        LayerEntry::Link {
            path: "data/link",
            target: "f",
        },
        LayerEntry::Dir {
            path: "data/sub",
            owner: (0, 0),
            mode: 0o755,
        },
        LayerEntry::File {
            path: "data/sub/g",
            owner: (0, 0),
            mode: 0o644,
            content: b"g\n",
        },
    ])
}

/// Docker's copy-up, at the create: a new volume, anonymous or named, gets
/// what the image has at its place, owners and modes kept (the volume's
/// own directory too, setgid included), a link as a link; a volume holding
/// something already isn't filled, nor one asked `nocopy` (`-v …:nocopy`,
/// `--mount …,volume-nocopy`), nor a bind; a container's later starts
/// never fill.
#[test]
fn a_volume_is_filled_from_its_image() {
    let Some(stores) = Stores::with_layer(data_layer(), r#"{"Cmd": ["sh"]}"#) else {
        return;
    };
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm", "--network", "none"];
        all.extend_from_slice(args);
        stores.ok(&all)
    };
    let look = "stat -c '%u %g %a' /data /data/f; cat /data/f; readlink /data/link; \
                cat /data/sub/g; ls /data";
    let filled = "1000 1000 2750\n1000 1000 640\nfrom the image\nf\ng\nf\nlink\nsub\n";
    assert_eq!(
        run(&["-v", "/data", &stores.image, "sh", "-c", look]),
        filled
    );
    assert_eq!(
        run(&["-v", "named:/data", &stores.image, "sh", "-c", look]),
        filled
    );
    // Filled once: what's there is kept at the next run.
    run(&[
        "-v",
        "named:/data",
        &stores.image,
        "sh",
        "-c",
        "echo more > /data/more",
    ]);
    assert_eq!(
        run(&["-v", "named:/data", &stores.image, "ls", "/data"]),
        "f\nlink\nmore\nsub\n"
    );
    run(&[
        "-v",
        "held:/x",
        &stores.image,
        "sh",
        "-c",
        "echo mine > /x/m",
    ]);
    assert_eq!(
        run(&["-v", "held:/data", &stores.image, "ls", "/data"]),
        "m\n",
        "not empty: not filled"
    );
    for asked in [
        ["-v", "nc:/data:nocopy"],
        [
            "--mount",
            "type=volume,source=nc2,target=/data,volume-nocopy",
        ],
    ] {
        let listed = run(&[asked[0], asked[1], &stores.image, "ls", "-A", "/data"]);
        assert_eq!(listed, "", "{asked:?}");
    }
    stores.ok(&[
        "create",
        "--name",
        "kept-empty",
        "--network",
        "none",
        "-v",
        "nc3:/data:ro,nocopy",
        &stores.image,
        "ls",
        "-A",
        "/data",
    ]);
    assert_eq!(stores.ok(&["start", "-a", "kept-empty"]), "");
    assert_eq!(
        stores.ok(&["start", "-a", "kept-empty"]),
        "",
        "at every start"
    );
    let bound = stores.scratch.0.join("bound");
    std::fs::create_dir(&bound).unwrap();
    let bind = format!("{}:/data", bound.display());
    assert_eq!(run(&["-v", &bind, &stores.image, "ls", "-A", "/data"]), "");
    stores.ok(&["rm", "kept-empty"]);
    stores.ok(&["volume", "rm", "named", "held", "nc", "nc2", "nc3"]);
}

/// A tmpfs mounted for a test, unmounted when it's dropped.
struct Mounted(std::path::PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

/// An image for the fills that fail: `/data/f`, and a 96 MiB file of
/// zeros in `/big`.
fn fill_layer() -> (Vec<u8>, String) {
    use shipyard_test_support::registry::{LayerEntry, built_layer};
    let big = vec![0_u8; 96 << 20];
    built_layer(&[
        LayerEntry::Dir {
            path: "data",
            owner: (0, 0),
            mode: 0o755,
        },
        LayerEntry::File {
            path: "data/f",
            owner: (0, 0),
            mode: 0o644,
            content: b"from the image\n",
        },
        LayerEntry::Dir {
            path: "big",
            owner: (0, 0),
            mode: 0o755,
        },
        LayerEntry::File {
            path: "big/zeros",
            owner: (0, 0),
            mode: 0o644,
            content: &big,
        },
    ])
}

/// A volume emptied since its fill is filled again by a new container's
/// create, and never by a container's start (Docker's: a start never
/// fills). Root: it empties the volume's directory itself.
#[test]
#[ignore = "needs root"]
fn an_emptied_volume_is_filled_by_a_create_never_a_start() {
    let Some(stores) = Stores::with_layer(fill_layer(), r#"{"Cmd": ["sh"]}"#) else {
        return;
    };
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm", "--network", "none"];
        all.extend_from_slice(args);
        stores.shipyard(&all)
    };
    // Emptied, filled again.
    let first = run(&["-v", "named:/data", &stores.image, "cat", "/data/f"]);
    assert_eq!(
        String::from_utf8_lossy(&first.stdout),
        "from the image\n",
        "{first:?}"
    );
    let data = stores.ok(&["volume", "inspect", "-f", "{{.Mountpoint}}", "named"]);
    std::fs::remove_file(std::path::Path::new(data.trim()).join("f")).unwrap();
    let again = run(&["-v", "named:/data", &stores.image, "cat", "/data/f"]);
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        "from the image\n",
        "{again:?}"
    );
    // A container's starts never fill (Docker's): emptied after its
    // create, it stays empty.
    stores.ok(&[
        "create",
        "--name",
        "later",
        "--network",
        "none",
        "-v",
        "later:/data",
        &stores.image,
        "ls",
        "-A",
        "/data",
    ]);
    let later = stores.ok(&["volume", "inspect", "-f", "{{.Mountpoint}}", "later"]);
    std::fs::remove_file(std::path::Path::new(later.trim()).join("f")).unwrap();
    assert_eq!(
        stores.ok(&["start", "-a", "later"]),
        "",
        "never filled at a start"
    );
    stores.ok(&["rm", "later"]);
}

/// A fill that fails fails the create (`run`), saying which volume (here
/// the volumes' file system fills up), and leaves the volume marked:
/// another container's create says so and leaves it as it is, and once
/// removed and made again it's filled anew. Root: it mounts a tmpfs.
#[test]
#[ignore = "needs root"]
fn a_fill_that_fails_fails_the_create() {
    let Some(stores) = Stores::with_layer(fill_layer(), r#"{"Cmd": ["sh"]}"#) else {
        return;
    };
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm", "--network", "none"];
        all.extend_from_slice(args);
        stores.shipyard(&all)
    };
    // The volumes on a file system of 1074 MiB: room before a fill for
    // the image store's 1 GiB reserve, but the 96 MiB file fills the disk.
    let volumes = stores.scratch.store().join("volumes");
    let status = Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=1074m,mode=0700", "tmpfs"])
        .arg(&volumes)
        .status()
        .unwrap();
    assert!(status.success());
    let _mounted = Mounted(volumes.clone());
    let control = run(&["-v", "small:/data", &stores.image, "cat", "/data/f"]);
    assert_eq!(
        String::from_utf8_lossy(&control.stdout),
        "from the image\n",
        "{control:?}"
    );
    let failed = run(&["-v", "full:/big", &stores.image, "sh", "-c", "exit 0"]);
    assert_eq!(failed.status.code(), Some(125), "{failed:?}");
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("filling the volume full from the image's /big"),
        "{failed:?}"
    );
    // Left marked unfinished (its container undone): room made, another
    // container's create never copies into it, as it holds part of a file
    // (Docker's rule), and says so; removed and made again, it's filled.
    let marker = volumes.join("full/filling");
    assert!(marker.exists(), "the cut-short fill is marked");
    let status = Command::new("mount")
        .args(["-o", "remount,size=1300m"])
        .arg(&volumes)
        .status()
        .unwrap();
    assert!(status.success());
    let size = "stat -c %s /big/zeros";
    let other = run(&["-v", "full:/big", &stores.image, "sh", "-c", size]);
    assert_ne!(
        String::from_utf8_lossy(&other.stdout),
        "100663296\n",
        "{other:?}"
    );
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("left unfinished"),
        "{other:?}"
    );
    stores.ok(&["volume", "rm", "full"]);
    let anew = run(&["-v", "full:/big", &stores.image, "sh", "-c", size]);
    assert_eq!(
        String::from_utf8_lossy(&anew.stdout),
        "100663296\n",
        "{anew:?}"
    );
    assert!(!marker.exists(), "done: unmarked");
}

/// A create killed mid-fill (`cut`, its volume `name`): its container's ID
/// and the volume's data directory, the fill marked unfinished and naming
/// it, and a file in the volume as a copy cut short would have left it.
fn a_create_killed_mid_fill(stores: &Stores, name: &str) -> (String, std::path::PathBuf) {
    let pause = stores.scratch.0.join(format!("fill-pause-{name}"));
    let mut create = stores
        .command(&[
            "create",
            "--name",
            name,
            "--network",
            "none",
            "-v",
            &format!("{name}:/data"),
            &stores.image,
            "cat",
            "/data/f",
        ])
        .env("SHIPYARD_TEST_FILL_PAUSE", &pause)
        .spawn()
        .unwrap();
    eventually("the fill's pause", || pause.exists());
    let pid: i32 = std::fs::read_to_string(&pause).unwrap().parse().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    create.wait().unwrap();
    let id = stores
        .ok(&["inspect", "-f", "{{.Id}}", name])
        .trim()
        .to_owned();
    let data = stores.ok(&["volume", "inspect", "-f", "{{.Mountpoint}}", name]);
    let data = std::path::PathBuf::from(data.trim());
    let marker = data.parent().unwrap().join("filling");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), id, "marked");
    std::fs::write(data.join("partial"), "half\n").unwrap();
    (id, data)
}

/// A create killed mid-fill leaves its volume marked: its container's first
/// start finishes the fill (the image's file there) when nothing else used
/// the volume meanwhile. Once another container's create finds it in use,
/// the fill is given up (said once): what that container wrote stays, never
/// copied over at the first one's start (11.6b's engine review).
#[test]
#[ignore = "needs root"]
fn a_killed_create_s_fill_is_finished_by_its_first_start() {
    let Some(stores) = Stores::with_layer(data_layer(), r#"{"Cmd": ["sh"]}"#) else {
        return;
    };
    let (_, data) = a_create_killed_mid_fill(&stores, "cut");
    assert_eq!(stores.ok(&["start", "-a", "cut"]), "from the image\n");
    assert!(
        !data.parent().unwrap().join("filling").exists(),
        "finished: unmarked"
    );
    stores.ok(&["rm", "cut"]);
    stores.ok(&["volume", "rm", "cut"]);

    let (_, data) = a_create_killed_mid_fill(&stores, "used");
    let other = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        "used:/data",
        &stores.image,
        "sh",
        "-c",
        "ls /data; echo mine > /data/f",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&other.stdout),
        "partial\n",
        "{other:?}"
    );
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("left unfinished"),
        "{other:?}"
    );
    assert!(
        !data.parent().unwrap().join("filling").exists(),
        "given up: unmarked"
    );
    // Its first start copies nothing over what the other wrote.
    assert_eq!(stores.ok(&["start", "-a", "used"]), "mine\n");
    stores.ok(&["rm", "used"]);
    stores.ok(&["volume", "rm", "used"]);
}

/// A create killed mid-fill, then another container made with `nocopy`
/// that writes there: the first one's start gives the fill up (said), its
/// image's file never copied over the other's (11.7's engine review: only
/// another create's fill gave it up before). The control: alone, the start
/// finishes it (`a_killed_create_s_fill_is_finished_by_its_first_start`).
#[test]
#[ignore = "needs root"]
fn a_finish_fill_leaves_a_volume_another_uses() {
    let Some(stores) = Stores::with_layer(data_layer(), r#"{"Cmd": ["sh"]}"#) else {
        return;
    };
    let (_, data) = a_create_killed_mid_fill(&stores, "shared");
    let other = stores.shipyard(&[
        "run",
        "--name",
        "writer",
        "--network",
        "none",
        "-v",
        "shared:/data:nocopy",
        &stores.image,
        "sh",
        "-c",
        "echo theirs > /data/f",
    ]);
    assert!(other.status.success(), "{other:?}");
    let started = stores.shipyard(&["start", "-a", "shared"]);
    assert_eq!(
        String::from_utf8_lossy(&started.stdout),
        "theirs\n",
        "{started:?}"
    );
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("is given up: "),
        "{started:?}"
    );
    assert!(
        !data.parent().unwrap().join("filling").exists(),
        "given up: unmarked"
    );
    stores.ok(&["rm", "shared", "writer"]);
    stores.ok(&["volume", "rm", "shared"]);
}

/// Before its first check, a container's health is `starting`, as
/// Docker's is from its start (Testcontainers waits on it, and refuses a
/// health it can't read); one without a check has no `Health` at all, nor
/// one whose health file is damaged.
#[test]
fn a_health_check_starts_as_starting() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "slow",
        "--network",
        "none",
        "--health-cmd",
        "true",
        "--health-interval",
        "1h",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "unchecked",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    let status = |name: &str| stores.ok(&["inspect", "-f", "{{json .State.Health}}", name]);
    // Its companion's own `starting` taken away (it writes again only
    // after an hour): what's shown is the engine's, before any is written.
    let start_dir = stores.inspect("slow")["log_dir"]
        .as_str()
        .unwrap()
        .to_owned();
    let file = std::path::Path::new(&start_dir).join("health.json");
    eventually("its companion's first word", || file.exists());
    std::fs::remove_file(&file).unwrap();
    let slow: serde_json::Value = serde_json::from_str(&status("slow")).unwrap();
    assert_eq!(slow["Status"], "starting", "{slow}");
    assert_eq!(slow["FailingStreak"], 0, "{slow}");
    // No `Health` at all, as Docker leaves it out (so a template naming
    // it fails, as `docker inspect -f` does).
    let unchecked: serde_json::Value =
        serde_json::from_str(&stores.ok(&["inspect", "-f", "{{json .State}}", "unchecked"]))
            .unwrap();
    assert!(
        unchecked.get("Health").is_none(),
        "no check, no health: {unchecked}"
    );
    assert!(
        unchecked.get("Status").is_some(),
        "the state itself: {unchecked}"
    );
    let listed = stores.ok(&["ps", "--format", "{{.Names}} {{.Status}}"]);
    assert!(
        listed
            .lines()
            .any(|line| line.starts_with("slow Up") && line.ends_with("(health: starting)")),
        "{listed}"
    );
    // A file there but damaged: unknown, never a lasting `starting`.
    std::fs::write(&file, "not a health file").unwrap();
    let damaged: serde_json::Value =
        serde_json::from_str(&stores.ok(&["inspect", "-f", "{{json .State}}", "slow"])).unwrap();
    assert!(damaged.get("Health").is_none(), "{damaged}");
    stores.ok(&["rm", "-f", "slow", "unchecked"]);
}

/// A tmpfs never hides a mount inside it, whatever the flags' order (a
/// parent first, Docker's): a volume on `/data/db` under a tmpfs on `/data`
/// keeps what's written to it; a tmpfs on `/etc` leaves the run's own
/// `/etc/hosts` seen.
#[test]
fn a_tmpfs_never_hides_a_mount_inside_it() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores
        .ok(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--mount",
            "type=volume,source=nested,target=/data/db",
            "--tmpfs",
            "/data",
            "--tmpfs",
            "/etc",
            &stores.image,
            "sh",
            "-c",
            "echo kept > /data/db/x && cat /etc/hosts",
        ])
        .lines()
        .find(|line| line.contains("localhost"))
        .expect("the run's own /etc/hosts, under the tmpfs on /etc");
    let read = stores.ok(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        "nested:/n",
        &stores.image,
        "sh",
        "-c",
        "cat /n/x",
    ]);
    assert_eq!(read, "kept\n", "written into the volume, not the tmpfs");
}

/// A tmpfs is fresh at each start (its files gone, the mount there
/// again); a `created.json` that can't be read refuses the start
/// (D11.3e2-2: it may hold tmpfs mounts), and says which file.
#[test]
fn a_tmpfs_is_fresh_at_each_start() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "create",
        "--name",
        "fresh",
        "--network",
        "none",
        "--tmpfs",
        "/k",
        &stores.image,
        "sh",
        "-c",
        "cat /proc/mounts; cat /k/x 2>/dev/null && echo had-a-file; \
         echo left > /k/x && echo wrote-a-file",
    ]);
    for start in ["first", "again"] {
        let out = stores.ok(&["start", "-a", "fresh"]);
        assert!(
            out.lines().any(|line| line.starts_with("tmpfs /k tmpfs ")),
            "{start}: its tmpfs: {out}"
        );
        assert!(!out.contains("had-a-file"), "{start}: empty: {out}");
        assert!(out.contains("wrote-a-file"), "{start}: its control: {out}");
    }
    let id = stores.docker_inspect("fresh")["Id"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = stores
        .scratch
        .0
        .join("containers")
        .join(&id)
        .join("created.json");
    assert!(created.exists(), "its tmpfs kept beside the record");
    std::fs::write(&created, "{not json").unwrap();
    let refused = stores.shipyard(&["start", "fresh"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("create can't be read"), "{said}");
    assert!(said.contains("created.json"), "{said}");
}

/// `container prune` removes the stopped containers (made, or ended) the
/// filters take, Docker's sections and total; it keeps a running one and
/// one its restart policy brings back. Without `-f`, off a terminal, it's
/// refused, and removes nothing.
#[test]
fn container_prune_is_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.clone();
    let id = |name: &str| {
        stores.docker_inspect(name)["Id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    stores.ok(&[
        "create", "--name", "made", "-l", "keep=no", &image, "sh", "-c", "exit 0",
    ]);
    let ended = stores.shipyard(&[
        "run",
        "--name",
        "ended",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "i=0; while [ $i -lt 2048 ]; do echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef; i=$((i+1)); done > /written; exit 3",
    ]);
    assert_eq!(ended.status.code(), Some(3), "{ended:?}");
    stores.ok(&[
        "create", "--name", "kept", "-l", "keep=yes", &image, "sh", "-c", "exit 0",
    ]);
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "up",
        "--network",
        "none",
        &image,
        "sleep",
        "300",
    ]);
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "again",
        "--network",
        "none",
        "--restart",
        "always",
        &image,
        "sh",
        "-c",
        "exit 1",
    ]);
    let (made, ended, kept) = (id("made"), id("ended"), id("kept"));

    let asked = stores.shipyard(&["container", "prune"]);
    assert_eq!(asked.status.code(), Some(1), "{asked:?}");
    assert!(
        String::from_utf8_lossy(&asked.stderr)
            .contains("run it on a terminal to be asked, or give -f")
    );
    let by_label = stores.ok(&["container", "prune", "-f", "--filter", "label=keep=no"]);
    assert_eq!(
        by_label,
        format!("Deleted Containers:\n{made}\n\nTotal reclaimed space: 0B\n")
    );
    let by_age = stores.ok(&["container", "prune", "-f", "--filter", "until=1h"]);
    assert_eq!(
        by_age, "Total reclaimed space: 0B\n",
        "none made an hour ago"
    );
    let rest = stores.ok(&["container", "prune", "-f"]);
    assert!(rest.starts_with("Deleted Containers:\n"), "{rest}");
    for pruned in [&ended, &kept] {
        assert!(rest.contains(pruned.as_str()), "{pruned}: {rest}");
    }
    // `ended` wrote 512 KiB into its own layer: reclaimed.
    let total = rest.lines().last().unwrap();
    assert!(total.starts_with("Total reclaimed space: "), "{rest}");
    assert_ne!(total, "Total reclaimed space: 0B", "{rest}");
    let mut left: Vec<String> = stores
        .ok(&["ps", "-a", "--format", "{{.Names}}"])
        .lines()
        .map(str::to_owned)
        .collect();
    left.sort();
    assert_eq!(left, ["again", "up"], "a running one, and one coming back");
    stores.ok(&["rm", "-f", "up", "again"]);
}

/// `image prune -a` keeps an image a container was made from, and removes
/// it once none is; `system prune -a --volumes` removes the stopped
/// containers, the unused anonymous volumes (a named one kept) and the
/// unused images, each in Docker's sections.
#[test]
fn image_and_system_prune_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.clone();
    stores.ok(&["create", "--name", "user", &image, "sh", "-c", "exit 0"]);
    let images = || stores.ok(&["images", "-q"]);
    assert!(!images().is_empty());
    let kept = stores.ok(&["image", "prune", "-a", "-f"]);
    assert_eq!(
        kept, "Total reclaimed space: 0B\n",
        "the container's image kept"
    );
    assert!(!images().is_empty());
    let anonymous = stores.ok(&["volume", "create"]).trim().to_owned();
    stores.ok(&["volume", "create", "named"]);
    let user = stores.docker_inspect("user")["Id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pruned = stores.ok(&["system", "prune", "-a", "--volumes", "-f"]);
    let deleted_containers = format!("Deleted Containers:\n{user}\n\n");
    let deleted_volumes = format!("Deleted Volumes:\n{anonymous}\n\n");
    for section in [
        deleted_containers.as_str(),
        deleted_volumes.as_str(),
        "Deleted Images:\nuntagged: ",
    ] {
        assert!(pruned.contains(section), "{section:?}: {pruned}");
    }
    assert!(
        pruned
            .lines()
            .last()
            .unwrap()
            .starts_with("Total reclaimed space: "),
        "{pruned}"
    );
    assert!(images().is_empty(), "no container uses it now");
    assert_eq!(stores.ok(&["volume", "ls", "-q"]), "named\n");
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
}

/// A container `busy` of a shell and two `sleep`s, its memory limited to
/// 64 MiB, once `top` shows them all (`run -d` returns once its init
/// runs: its children come after); what `top` printed then.
fn busy(stores: &Stores) -> String {
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "busy",
        "--network",
        "none",
        "-m",
        "64m",
        &stores.image,
        "sh",
        "-c",
        "sleep 300 & sleep 301 & wait",
    ]);
    let mut top = String::new();
    eventually("its sleeps forked", || {
        top = stores.ok(&["top", "busy"]);
        top.lines().any(|row| row.ends_with("sleep 301"))
    });
    top
}

/// `top` lists a container's processes in `ps -ef`'s columns, by their
/// host PIDs (its init's is `inspect`'s); `ps` options but `-ef` and a
/// stopped container are refused (1). Root, or a delegated cgroup.
#[test]
fn top_is_docker_s() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    let top = busy(&stores);
    let mut lines = top.lines();
    let titles: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        titles,
        ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]
    );
    let rows: Vec<&str> = lines.collect();
    for command in [
        "sh -c sleep 300 & sleep 301 & wait",
        "sleep 300",
        "sleep 301",
    ] {
        assert!(
            rows.iter().any(|row| row.ends_with(command)),
            "{command}: {top}"
        );
    }
    let init = stores.docker_inspect("busy")["State"]["Pid"]
        .as_i64()
        .unwrap()
        .to_string();
    assert!(
        rows.iter()
            .any(|row| row.split_whitespace().nth(1) == Some(init.as_str())),
        "{init}: {top}"
    );
    assert_eq!(
        stores.ok(&["top", "busy", "-ef"]).lines().next(),
        top.lines().next(),
        "-ef is what's shown anyway"
    );
    let options = stores.shipyard(&["top", "busy", "-o", "pid"]);
    assert_eq!(options.status.code(), Some(1), "{options:?}");
    assert!(String::from_utf8_lossy(&options.stderr).contains("ps options aren't supported"));

    stores.ok(&["stop", "-t", "0", "busy"]);
    let stopped = stores.shipyard(&["top", "busy"]);
    assert_eq!(stopped.status.code(), Some(1), "{stopped:?}");
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("is not running"));
}

/// `stats --no-stream` prints Docker's table once, its memory limit
/// `-m`'s; live, a row a second with no screen cleared off a terminal; a
/// stopped container's as zeros; one that isn't there exits 1. Root, or a
/// delegated cgroup.
#[test]
fn stats_is_docker_s() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    busy(&stores);
    let table = stores.ok(&["stats", "--no-stream"]);
    let header: Vec<&str> = table
        .lines()
        .next()
        .unwrap()
        .split("  ")
        .filter(|cell| !cell.is_empty())
        .map(str::trim)
        .collect();
    assert_eq!(
        header,
        [
            "CONTAINER ID",
            "NAME",
            "CPU %",
            "MEM USAGE / LIMIT",
            "MEM %",
            "NET I/O",
            "BLOCK I/O",
            "PIDS"
        ]
    );
    assert!(table.lines().nth(1).unwrap().contains(" busy "), "{table}");
    // Live, off a terminal: a table each second, no screen cleared.
    let mut live = stores
        .command(&["stats", "--format", "{{.Name}} {{.PIDs}}", "busy"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let out = live.stdout.take().unwrap();
    let (sent, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(out)).map_while(Result::ok) {
            if sent.send(line).is_err() {
                break;
            }
        }
    });
    let next = || {
        lines
            .recv_timeout(Duration::from_secs(20))
            .expect("a row a second")
    };
    let (first, second) = (next(), next());
    live.kill().unwrap();
    live.wait().unwrap();
    for line in [&first, &second] {
        assert!(line.starts_with("busy "), "{line:?}");
    }
    let row: serde_json::Value = serde_json::from_str(
        stores
            .ok(&["stats", "--no-stream", "--format", "json", "busy"])
            .trim(),
    )
    .unwrap();
    assert_eq!(row["Name"], "busy");
    assert_eq!(row["Container"], "busy");
    assert!(
        row["MemUsage"].as_str().unwrap().ends_with(" / 64MiB"),
        "{row}"
    );
    let pids: u64 = row["PIDs"].as_str().unwrap().parse().unwrap();
    assert!(pids >= 3, "{row}");
    assert!(row["CPUPerc"].as_str().unwrap().ends_with('%'), "{row}");

    stores.ok(&["stop", "-t", "0", "busy"]);
    assert_eq!(
        stores
            .ok(&[
                "stats",
                "--no-stream",
                "--format",
                "{{.MemUsage}} {{.PIDs}}",
                "busy"
            ])
            .trim(),
        "0B / 0B 0"
    );
    let missing = stores.shipyard(&["stats", "--no-stream", "nothing-here"]);
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
}

/// `pause` and `unpause` freeze and thaw a running container, as Docker's
/// (step 11.3d1): `ps` says `(Paused)`, `exec` is refused meanwhile, a
/// second pause or unpause fails (1), the events are said. `update`
/// changes a running container's limits in its cgroup at once and keeps
/// them; a stopped one's for its next start. Root, or a delegated cgroup.
#[test]
fn pause_unpause_and_update_are_docker_s() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    let cgroup_file = |name: &str, file: &str| -> String {
        let pid = stores.docker_inspect(name)["State"]["Pid"]
            .as_i64()
            .unwrap();
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let path = cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap();
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/{file}"))
            .unwrap()
            .trim()
            .to_owned()
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "live",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(stores.ok(&["pause", "live"]), "live\n");
    assert!(
        stores
            .ok(&["ps", "--format", "{{.Status}}"])
            .contains("(Paused)")
    );
    assert_eq!(stores.docker_inspect("live")["State"]["Status"], "paused");
    assert_eq!(cgroup_file("live", "cgroup.freeze"), "1");
    let again = stores.shipyard(&["pause", "live"]);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
    assert!(String::from_utf8_lossy(&again.stderr).contains("is already paused"));
    let exec = stores.shipyard(&["exec", "live", "sh", "-c", "exit 0"]);
    assert_eq!(
        exec.status.code(),
        Some(1),
        "refused while paused: {exec:?}"
    );
    assert_eq!(stores.ok(&["container", "unpause", "live"]), "live\n");
    assert_eq!(cgroup_file("live", "cgroup.freeze"), "0");
    stores.ok(&["exec", "live", "sh", "-c", "exit 0"]);
    let not_paused = stores.shipyard(&["unpause", "live"]);
    assert_eq!(not_paused.status.code(), Some(1), "{not_paused:?}");
    assert!(String::from_utf8_lossy(&not_paused.stderr).contains("is not paused"));
    let actions = stores.ok(&["events", "--until", "0s", "--format", "{{.Action}}"]);
    let actions: Vec<&str> = actions.lines().collect();
    assert!(
        actions.contains(&"pause") && actions.contains(&"unpause"),
        "{actions:?}"
    );
    // Its limits, at once.
    stores.ok(&[
        "update",
        "-m",
        "64m",
        "--cpus",
        "0.5",
        "--pids-limit",
        "50",
        "live",
    ]);
    assert_eq!(
        cgroup_file("live", "memory.max"),
        (64_u64 << 20).to_string()
    );
    assert_eq!(cgroup_file("live", "cpu.max"), "50000 100000");
    assert_eq!(cgroup_file("live", "pids.max"), "50");
    let host = &stores.docker_inspect("live")["HostConfig"];
    assert_eq!(host["Memory"], 64_u64 << 20, "{host}");
    assert_eq!(host["PidsLimit"], 50, "{host}");
    // Stopped: kept for its next start.
    stores.ok(&["stop", "-t", "1", "live"]);
    stores.ok(&["update", "--pids-limit", "40", "live"]);
    stores.ok(&["start", "live"]);
    assert_eq!(cgroup_file("live", "pids.max"), "40");
    assert_eq!(
        cgroup_file("live", "memory.max"),
        (64_u64 << 20).to_string(),
        "kept"
    );
    let nothing = stores.shipyard(&["update", "live"]);
    assert_eq!(nothing.status.code(), Some(1), "{nothing:?}");
    stores.ok(&["rm", "-f", "live"]);
}

/// A paused container still stops, and takes a signal: a stop thaws it
/// first (F46), a `kill` with another signal than SIGKILL lets it run so
/// its processes see it (Docker's). Root, or a delegated cgroup.
#[test]
fn a_paused_container_stops_and_takes_signals() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    for name in ["stopped", "killed"] {
        stores.ok(&[
            "run",
            "-d",
            "--name",
            name,
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ]);
        stores.ok(&["pause", name]);
    }
    let started = Instant::now();
    stores.ok(&["stop", "-t", "10", "stopped"]);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "its stop signal, not the timeout"
    );
    assert_eq!(
        stores.docker_inspect("stopped")["State"]["Status"],
        "exited"
    );
    stores.ok(&["kill", "-s", "TERM", "killed"]);
    eventually("the signalled one ends", || {
        stores.docker_inspect("killed")["State"]["Status"] == "exited"
    });
    stores.ok(&["rm", "stopped", "killed"]);
}

/// A limit the kernel refuses fails the update and leaves the container as
/// it was: a CPU the host hasn't (refused before anything is written), and
/// a process limit past the kernel's (refused after `memory.max` is
/// written: put back). Its record, its cgroup. Root, or a delegated cgroup.
#[test]
fn a_refused_limit_leaves_the_container_as_it_was() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "kept",
        "--network",
        "none",
        "-m",
        "64m",
        &stores.image,
        "sleep",
        "300",
    ]);
    for limits in [
        &["--cpuset-cpus", "999"][..],
        // Past the kernel's `PIDS_MAX`: written after `memory.max`.
        &["--pids-limit", "9999999999"],
    ] {
        let mut argv = vec!["update", "-m", "96m"];
        argv.extend_from_slice(limits);
        argv.push("kept");
        let refused = stores.shipyard(&argv);
        assert_eq!(refused.status.code(), Some(1), "{limits:?}: {refused:?}");
    }
    let host = &stores.docker_inspect("kept")["HostConfig"];
    assert_eq!(host["Memory"], 64_u64 << 20, "the record put back: {host}");
    assert_eq!(host["CpusetCpus"], "", "{host}");
    let pid = stores.docker_inspect("kept")["State"]["Pid"]
        .as_i64()
        .unwrap();
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap();
    let memory = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).unwrap();
    assert_eq!(
        memory.trim(),
        (64_u64 << 20).to_string(),
        "its cgroup put back"
    );
    stores.ok(&["rm", "-f", "kept"]);
}

/// `images` and `volume ls` are Docker's too: a row per name, Docker's
/// fields, `-q`, `-f`.
#[test]
fn images_and_volumes_list_as_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    // Images: a row per name, its fields Docker's.
    let images = stores.ok(&["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    assert_eq!(images, format!("{}:latest\n", stores.image), "{images}");
    let image_id = stores.ok(&["images", "-q"]);
    assert_eq!(image_id.trim().len(), 12, "{image_id}");
    let image: serde_json::Value =
        serde_json::from_str(stores.ok(&["images", "--format", "json"]).trim_end()).unwrap();
    assert_eq!(image["Tag"], "latest", "{image}");
    assert_eq!(image["Containers"], "N/A", "{image}");
    // Volumes: by label.
    let label = format!("lists={}", std::process::id());
    stores.ok(&["volume", "create", "--label", &label, "lists-volume"]);
    assert_eq!(
        stores.ok(&["volume", "ls", "-q", "-f", &format!("label={label}")]),
        "lists-volume\n"
    );
    stores.ok(&["volume", "rm", "lists-volume"]);
}

/// `run -d` prints the full ID; the container's end shows in `ps -a`;
/// `rm` of several goes on past one that isn't there, and says so.
#[test]
fn run_d_prints_the_id_and_rm_goes_on() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "once",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 7",
        ])
        .trim()
        .to_owned();
    assert!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    eventually("Exited (7)", || {
        stores.ok(&["ps", "-a"]).contains("Exited (7)")
    });
    let removed = stores.shipyard(&["rm", "nothing-here", "once"]);
    assert_eq!(removed.status.code(), Some(1), "{removed:?}");
    assert_eq!(String::from_utf8_lossy(&removed.stdout).trim(), "once");
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
}

/// `rmi` refuses the image a container was made from, naming it; once the
/// container is removed, it goes.
#[test]
fn rmi_refuses_a_containers_image() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "create",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 0",
        ])
        .trim()
        .to_owned();
    let refused = stores.shipyard(&["rmi", &stores.image]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&id[..12]),
        "{refused:?}"
    );
    stores.ok(&["rm", &id]);
    stores.ok(&["rmi", &stores.image]);
}

/// `create` and `run -d` refuse what only a foreground run can have:
/// `create` exits 1, `run` 125 (Docker's codes).
#[test]
fn containers_refuse_what_only_a_foreground_run_has() {
    let Some(stores) = Stores::new() else {
        return;
    };
    // A real bundle: refused for having no record, not for being missing.
    let bundle = shipyard_test_support::bundle::Bundle::new();
    for (args, code, why) in [
        (
            &["create", "--no-isolation", "true"][..],
            1,
            "a container needs isolation",
        ),
        (
            &["create", "--no-image", "sh"],
            1,
            "a container needs a root filesystem of its own",
        ),
        (
            &["run", "-d", "--bundle", bundle.arg()],
            125,
            "a container from a bundle has no record",
        ),
        (
            &["run", "-d", "--preserve-fds", "1", &stores.image],
            125,
            "only a foreground run can",
        ),
    ] {
        let refused = stores.shipyard(args);
        assert_eq!(refused.status.code(), Some(code), "{args:?}: {refused:?}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(why), "{args:?}: {stderr}");
    }
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "nothing made");
}

/// `stop`, `restart`, `kill` and `wait` by name: each prints the reference
/// (`wait`, the exit code); `kill` of one not running exits 1; several
/// references go on past one that fails.
#[test]
fn stop_restart_kill_and_wait_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(stores.ok(&["stop", "-t", "5", "web"]).trim(), "web");
    assert_eq!(stores.ok(&["wait", "web"]).trim(), "143");
    let not_running = stores.shipyard(&["kill", "web"]);
    assert_eq!(not_running.status.code(), Some(1), "{not_running:?}");
    assert!(
        String::from_utf8_lossy(&not_running.stderr).contains("is not running"),
        "{not_running:?}"
    );
    assert_eq!(stores.ok(&["restart", "web"]).trim(), "web");
    assert!(stores.ok(&["ps"]).contains("web"), "up again");
    assert_eq!(stores.ok(&["kill", "-s", "TERM", "web"]).trim(), "web");
    assert_eq!(stores.ok(&["wait", "web"]).trim(), "143");
    let several = stores.shipyard(&["stop", "nothing-here", "web"]);
    assert_eq!(several.status.code(), Some(1), "{several:?}");
    assert_eq!(String::from_utf8_lossy(&several.stdout).trim(), "web");
    stores.ok(&["rm", "web"]);
}

/// `port` shows what a running container publishes, by name: every
/// binding (each address family on a network with IPv6), or one port's host
/// side; nothing published there exits 1, as Docker's; a stopped container
/// publishes nothing. `ps` shows both families too, and `network inspect`
/// names the container by its ID. As root, in a network namespace of the
/// test's own.
#[test]
fn port_shows_what_a_container_publishes() {
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        // The namespace's loopback, up (a host's is): `127.0.0.1` can be
        // bound.
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let id = stores
            .ok(&[
                "run",
                "-d",
                "--name",
                "web",
                "--network",
                "app",
                "-p",
                "80",
                "-p",
                "127.0.0.1::53/udp",
                &stores.image,
                "sleep",
                "300",
            ])
            .trim()
            .to_owned();
        // In Docker's order: by the container's port.
        let all = stores.ok(&["port", "web"]);
        let lines: Vec<&str> = all.lines().collect();
        assert!(lines[0].starts_with("53/udp -> 127.0.0.1:"), "{all}");
        let host_port = lines[1]
            .strip_prefix("80/tcp -> 0.0.0.0:")
            .unwrap_or_else(|| panic!("{all}"));
        // Its IPv6 side, where the kernel has IPv6.
        let ipv6 = std::net::TcpListener::bind("[::]:0").is_ok();
        if ipv6 {
            assert_eq!(lines.len(), 3, "{all}");
            assert_eq!(lines[2], format!("80/tcp -> [::]:{host_port}"), "{all}");
            assert_eq!(
                stores.ok(&["port", "web", "80"]),
                format!("0.0.0.0:{host_port}\n[::]:{host_port}\n")
            );
        } else {
            assert_eq!(lines.len(), 2, "{all}");
        }
        // The same number over UDP is another port: none published.
        let udp = stores.shipyard(&["port", "web", "80/udp"]);
        assert_eq!(udp.status.code(), Some(1), "{udp:?}");
        let none = stores.shipyard(&["port", "web", "81"]);
        assert_eq!(none.status.code(), Some(1), "{none:?}");
        assert_eq!(
            String::from_utf8_lossy(&none.stderr).trim(),
            "shipyard: no public port '81/tcp' published for web"
        );
        let listed = stores.ok(&["ps"]);
        assert!(
            listed.contains(&format!("0.0.0.0:{host_port}->80/tcp")),
            "{listed}"
        );
        if ipv6 {
            assert!(
                listed.contains(&format!(
                    "0.0.0.0:{host_port}->80/tcp, [::]:{host_port}->80/tcp"
                )),
                "{listed}"
            );
        }
        let networks: serde_json::Value =
            serde_json::from_str(&stores.ok(&["network", "inspect", "--shipyard", "app"])).unwrap();
        assert_eq!(networks[0]["Containers"][0]["Container"], id.as_str());
        assert_eq!(networks[0]["Containers"][0]["Name"], "web");
        stores.ok(&["stop", "-t", "1", "web"]);
        assert_eq!(stores.ok(&["port", "web"]), "");
        let stopped = stores.shipyard(&["port", "web", "80"]);
        assert_eq!(stopped.status.code(), Some(1), "{stopped:?}");
        assert_eq!(
            String::from_utf8_lossy(&stopped.stderr).trim(),
            "shipyard: no public port '80/tcp' published for web"
        );
        stores.ok(&["rm", "web"]);
    });
}

/// A health check by its flags: `ps` says `(healthy)`, `ps --format json`
/// and `inspect` say it too.
#[test]
fn health_shows_in_ps_and_inspect() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores.ok(&[
        "run",
        "-d",
        "--name",
        "checked",
        "--network",
        "none",
        "--health-cmd",
        "true",
        "--health-interval",
        "300ms",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert!(!id.trim().is_empty());
    eventually("(healthy) in ps", || {
        stores
            .ok(&["ps"])
            .lines()
            .any(|line| line.contains("(healthy)"))
    });
    assert_eq!(
        stores.ok(&["ps", "--format", "{{.HealthStatus}}"]).trim(),
        "healthy"
    );
    let shown = stores.inspect("checked");
    assert_eq!(shown["state"]["health"]["status"], "healthy", "{shown}");
    assert_eq!(
        shown["state"]["health"]["log"][0]["exit_code"], 0,
        "{shown}"
    );
    assert_eq!(shown["config"]["health"]["interval_ms"], 300, "{shown}");
    stores.ok(&["rm", "-f", "checked"]);
}

/// `--no-healthcheck` with a `--health-*` flag is refused; tuning flags
/// alone tune the image's check, and with an image without one, they're
/// said to be ignored.
#[test]
fn health_flags_and_no_healthcheck() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let refused = stores.shipyard(&[
        "run",
        "--no-healthcheck",
        "--health-cmd",
        "true",
        &stores.image,
        "true",
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--no-healthcheck"),
        "{refused:?}"
    );
    let tuned = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--health-interval",
        "1s",
        &stores.image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(tuned.status.success(), "{tuned:?}");
    assert!(
        String::from_utf8_lossy(&tuned.stderr).contains("there's no health check to tune"),
        "{tuned:?}"
    );
    // `--no-healthcheck` alone: nothing to say.
    let off = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--no-healthcheck",
        &stores.image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(off.status.success(), "{off:?}");
    assert!(
        !String::from_utf8_lossy(&off.stderr).contains("health"),
        "{off:?}"
    );
}

/// `rename`: the record renamed (found by its new name, not its old one),
/// nothing printed; another container's name, its own name, or a name that
/// can't be one: refused (1), nothing changed.
#[test]
fn rename_gives_a_container_a_new_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    for name in ["web", "db"] {
        stores.ok(&[
            "create",
            "--name",
            name,
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ]);
    }
    assert_eq!(stores.ok(&["rename", "web", "site"]), "");
    assert_eq!(stores.inspect("site")["name"], "site");
    assert_eq!(stores.shipyard(&["inspect", "web"]).status.code(), Some(1));
    for (new, why) in [
        ("db", "is taken"),
        ("site", "is named site already"),
        ("no/slash", "can't be a container's name"),
        ("my.site", "can't be a container's name"),
    ] {
        let output = stores.shipyard(&["rename", "site", new]);
        assert_eq!(output.status.code(), Some(1), "{new}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(why),
            "{new}: {output:?}"
        );
    }
    assert_eq!(stores.inspect("site")["name"], "site");
    stores.ok(&["rm", "site", "db"]);
}

/// `shipyard logs` ends as its readers do (F70): 1 when stderr's reader
/// goes while stdout's is read.
#[test]
fn logs_end_as_their_readers_do_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "both",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "n=0; while [ $n -lt 20000 ]; do echo out $n; echo err $n >&2; n=$((n+1)); done",
    ]);
    assert_eq!(stores.ok(&["wait", "both"]).trim(), "0");
    shipyard_test_support::pipes::logs_end_as_their_readers_do(
        || stores.command(&["logs", "both"]),
        1,
    );
}

/// `logs`, `exec`, `cp` and `attach` by name: a stopped container's logs,
/// and its files copied out and in; exec's exit codes (the command's, 127
/// for one that isn't there); a running container's file copied out;
/// attaching to a container that isn't up exits 1.
#[test]
fn logs_exec_cp_and_attach_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "once",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo said-once",
    ]);
    assert_eq!(stores.ok(&["wait", "once"]).trim(), "0");
    assert_eq!(stores.ok(&["logs", "once"]), "said-once\n");
    let attach = stores.shipyard(&["attach", "once"]);
    assert_eq!(attach.status.code(), Some(1), "{attach:?}");
    // A stopped container's files: out, and in (its next start sees them).
    let marker = stores.scratch.0.join("stopped-marker");
    stores.ok(&["cp", "once:/marker", marker.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "shipyard test root filesystem\n"
    );
    let missing = stores.shipyard(&["cp", "once:/no/such", marker.to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    stores.ok(&["cp", marker.to_str().unwrap(), "once:/copied"]);
    let back = stores.scratch.0.join("copied-back");
    stores.ok(&["cp", "once:/copied", back.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&back).unwrap(),
        "shipyard test root filesystem\n"
    );
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    let ran = stores.shipyard(&["exec", "web", "sh", "-c", "exit 5"]);
    assert_eq!(ran.status.code(), Some(5), "{ran:?}");
    let missing = stores.shipyard(&["exec", "web", "/no/such/command"]);
    assert_eq!(missing.status.code(), Some(127), "{missing:?}");
    let copied = stores.scratch.0.join("marker-copy");
    stores.ok(&["cp", "web:/marker", copied.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "shipyard test root filesystem\n"
    );
    stores.ok(&["rm", "-f", "web", "once"]);
}

/// `logs -f` ends, quietly, once its reader has gone (`logs -f web | head
/// -n1`), though the container runs on and writes nothing more: between
/// its looks it asks whether stdout's reader is there (F78).
#[test]
fn logs_follow_ends_when_its_reader_goes() {
    use std::io::BufRead as _;
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "quiet",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo first; exec sleep 300",
    ]);
    let mut follow = stores
        .command(&["logs", "-f", "quiet"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Its first line read (the control: it follows), then its reader gone.
    let mut stdout = std::io::BufReader::new(follow.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "first\n");
    drop(stdout);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = follow.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = follow.kill();
            panic!("it followed with no reader");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(follow.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "{status:?}: {stderr}");
    assert_eq!(stderr, "");
    // It ended for its reader, not its container's end.
    assert!(stores.ok(&["ps"]).contains("Up"));
    stores.ok(&["rm", "-f", "quiet"]);
}

/// Stage 9's milestone (v0.2), through the `shipyard` binary, on one
/// container by its name: `run -d` returns while the container runs on;
/// `logs -f` follows it; `exec`, `attach` (typing reaches it, the keys
/// detach) and `cp` reach it; its process killed from outside, its restart
/// policy starts it again (`die` then `start` in the events, its logs
/// showing both starts); `system recover` leaves a running one as it is;
/// `stop` and `rm` leave nothing. Rootless or root, as the tests run. (A
/// reboot is the engine's tests' and the stage's manual check's: here the
/// runtime's state would have to be faked behind its back.)
#[test]
fn the_v0_2_milestone_a_named_container_end_to_end() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let started = Instant::now();
    stores.ok(&[
        "run",
        "-d",
        "-i",
        "--name",
        "web",
        "--restart",
        "on-failure:3",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo ready; while read line; do echo got-$line; done; sleep 300",
    ]);
    // `run -d` has returned: the CLI is gone, the container runs on.
    assert!(started.elapsed() < Duration::from_secs(20));
    eventually("web up", || stores.ok(&["ps"]).contains("Up"));
    let (mut follow, next_line) = stores.follow("web");
    assert_eq!(next_line(), "ready");
    // exec: a command beside it, its exit code its own.
    let ran = stores.shipyard(&["exec", "web", "sh", "-c", "echo $((40 + 2)); exit 4"]);
    assert_eq!(ran.status.code(), Some(4), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "42\n");
    // attach: what's typed reaches it; Ctrl-P Ctrl-Q leaves it running.
    let attached = stores.attach_and_type("web", b"one\n", || {
        assert_eq!(next_line(), "got-one");
    });
    assert_eq!(attached.status.code(), Some(0), "{attached:?}");
    assert!(
        String::from_utf8_lossy(&attached.stdout).contains("got-one"),
        "{attached:?}"
    );
    // cp: in, then back out, the same.
    let file = stores.scratch.0.join("milestone-in");
    std::fs::write(&file, "carried\n").unwrap();
    stores.ok(&["cp", file.to_str().unwrap(), "web:/carried"]);
    let back = stores.scratch.0.join("milestone-out");
    stores.ok(&["cp", "web:/carried", back.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "carried\n");
    // Its process killed from outside: the policy starts it again.
    let pid = stores.inspect("web")["state"]["pid"].as_i64().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    // The follow ends with the start it followed.
    assert!(follow.wait().unwrap().success());
    eventually("restarted once", || {
        let shown = stores.inspect("web");
        shown["restart_count"] == 1 && shown["state"]["running"] == true
    });
    // As Docker's: a restart by its policy is a `die`, then a `start`.
    let events = stores.ok(&["events", "--since", "1h", "--until", "0s"]);
    let actions: Vec<&str> = events
        .lines()
        .filter(|line| line.contains("container") && line.contains("name=web"))
        .filter_map(|line| line.split(' ').nth(2))
        .collect();
    let died = actions.iter().position(|&action| action == "die");
    assert!(
        died.is_some_and(|died| actions[died..].contains(&"start")),
        "{events}"
    );
    eventually("both starts in its logs", || {
        stores.ok(&["logs", "web"]).matches("ready").count() == 2
    });
    // Recovery leaves a running container as it is.
    stores.ok(&["system", "recover"]);
    assert_eq!(stores.inspect("web")["restart_count"], 1);
    assert!(stores.ok(&["ps"]).contains("Up"));
    // Stopped by its user, it stays stopped; removed, nothing is left.
    stores.ok(&["stop", "-t", "1", "web"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(stores.inspect("web")["state"]["running"], false);
    stores.ok(&["rm", "web"]);
    assert!(
        stores.ok(&["ps", "-a"]).lines().count() <= 1,
        "nothing left"
    );
    let records = std::fs::read_dir(stores.scratch.0.join("containers"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("record.json").exists())
        .count();
    assert_eq!(records, 0);
}

/// A container recorded by an earlier Shipyard (a version 1 record, from
/// 9.2a, before images' IDs, ports and `--rm` were kept) is listed, looked
/// at, recovered around and removed by today's: records are read in every
/// version they were written in (H2).
#[test]
fn a_container_an_earlier_version_recorded_is_managed() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let dir = stores.scratch.0.join("containers").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("record.json"),
        format!(
            r#"{{
        "version": 1,
        "id": "{id}",
        "serial": 304812,
        "name": "brave_tugboat",
        "created": {{"secs_since_epoch": 1760000000, "nanos_since_epoch": 0}},
        "config": {{
            "interactive": false, "tty": false, "env": ["A=1"], "workdir": null,
            "user": null, "group_add": [], "ulimit": [], "oom_score_adj": null,
            "device": [], "device_cgroup_rule": [], "stop_signal": "SIGTERM",
            "stop_timeout": 10, "no_isolation": false, "hostname": null,
            "domainname": null, "sysctl": [], "network": ["none"],
            "network_alias": [], "ip": null, "ip6": null, "mac_address": null,
            "add_host": [], "dns": [], "dns_search": [], "dns_option": [],
            "publish": [], "publish_all": false, "rootfs": null, "image": "{image}",
            "entrypoint": null, "pull": "missing", "read_only": false,
            "cap_add": [], "cap_drop": [], "security_opt": [],
            "uidmap": [], "gidmap": [], "bundle": null,
            "resources": {{"swap_is_default": false, "cpu_max": false, "io_max": [],
                          "unified": {{}}}},
            "volume": [], "command": ["sh", "-c", "exit 0"]
        }},
        "labels": {{"tier": "web"}},
        "started": null,
        "finished": null,
        "exit": null
    }}"#,
            image = stores.image
        ),
    )
    .unwrap();
    let listed = stores.ok(&["ps", "-a"]);
    assert!(listed.contains("brave_tugboat"), "{listed}");
    let shown = stores.inspect("brave_tugboat");
    assert_eq!(shown["labels"]["tier"], "web");
    assert_eq!(shown["state"]["status"], "created");
    stores.ok(&["system", "recover"]);
    // Started by today's Shipyard, from what it recorded then.
    stores.ok(&["start", "brave_tugboat"]);
    assert_eq!(stores.ok(&["wait", "brave_tugboat"]).trim(), "0");
    stores.ok(&["rm", "brave_tugboat"]);
    assert!(!dir.exists(), "its record removed");
}

/// A container stage 8's `shipyard-runtime` made, still running when today's
/// Shipyard is installed (stage 9, step 9.9a, H2). Stage 8 had no records,
/// and its containers share today's state root: `ps`, `system recover` and
/// (rootless: root's state root is shared with other tests) `system stop`
/// leave it alone, and today's `shipyard-runtime` still reads its state
/// (`state.json` without a version), its end (a line of text) and deletes
/// it. Its monitor and init are stand-ins, as stage 8 recorded them; a real
/// stage 8 monitor was checked by hand (the stage notes).
#[test]
fn a_container_stage_8s_runtime_made_is_left_to_it() {
    const RUNTIME: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard-runtime"));
    let Some(stores) = Stores::new() else {
        return;
    };
    let root = if is_root() {
        std::path::PathBuf::from("/run/shipyard")
    } else {
        stores.scratch.0.join("xdg/shipyard")
    };
    let start_time = |pid: u32| -> u64 {
        shipyard_sys::proc::stat(i32::try_from(pid).unwrap())
            .unwrap()
            .start
    };
    let mut monitor = Command::new("sleep").arg("300").spawn().unwrap();
    let mut init = Command::new("sleep").arg("300").spawn().unwrap();
    let id = format!("stage8-{}", std::process::id());
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // As stage 8 wrote it: no version.
    let record = serde_json::json!({
        "bundle": "/nonexistent",
        "monitor": [monitor.id(), start_time(monitor.id())],
        "init": [init.id(), start_time(init.id())],
        "annotations": {},
    });
    std::fs::write(dir.join("state.json"), record.to_string()).unwrap();
    let runtime = |args: &[&str]| -> Output {
        Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&root)
            .args(args)
            .output()
            .unwrap()
    };
    let status = || -> String {
        let state = runtime(&["state", &id]);
        assert!(state.status.success(), "{state:?}");
        let state: serde_json::Value = serde_json::from_slice(&state.stdout).unwrap();
        state["status"].as_str().unwrap().to_owned()
    };
    assert_eq!(status(), "running");
    // Today's engine: none of its containers, so none of its business.
    let listed = stores.ok(&["ps", "-a"]);
    assert!(!listed.contains(&id), "{listed}");
    stores.ok(&["system", "recover"]);
    if !is_root() {
        stores.ok(&["system", "stop"]);
        stores.ok(&["system", "recover"]);
    }
    assert!(dir.join("state.json").exists(), "its state kept");
    assert_eq!(status(), "running", "left running");
    // Its end, as stage 8's monitor recorded it.
    init.kill().unwrap();
    init.wait().unwrap();
    std::fs::write(dir.join("exit"), "exited (exit code 0)").unwrap();
    assert_eq!(status(), "stopped");
    let deleted = runtime(&["delete", &id]);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(!dir.exists());
    let _ = monitor.kill();
    let _ = monitor.wait();
}

/// A container's monitor runs in a cgroup of its own (`shipyard/monitors/<id>`),
/// out of its caller's (a login session's, a unit's), so a shutdown that ends
/// those doesn't end it before `system stop` reaches it (stage 9, step 9.9b).
/// Its init stays in its own cgroup; a policy restart's monitor joins the same
/// one; `rm` removes it. Root, or a user with a delegated cgroup tree.
#[test]
fn a_monitor_lives_in_a_cgroup_of_its_own() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    // `/proc/PID/cgroup`'s v2 line, and a process's parent.
    let cgroup_of = |pid: i64| -> String {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap()
            .to_owned()
    };
    let parent_of = |pid: i64| -> i64 {
        i64::from(
            shipyard_sys::proc::stat(i32::try_from(pid).unwrap())
                .unwrap()
                .ppid,
        )
    };
    let monitor_of = |name: &str| -> (i64, i64) {
        let init = stores.inspect(name)["state"]["pid"].as_i64().unwrap();
        let monitor = parent_of(init);
        let cmdline = std::fs::read(format!("/proc/{monitor}/cmdline")).unwrap();
        assert!(
            String::from_utf8_lossy(&cmdline).contains("__shipyard-monitor"),
            "{monitor} isn't a monitor"
        );
        (init, monitor)
    };
    let id = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "watched",
            "--restart",
            "on-failure",
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    let own = cgroup_of(i64::from(std::process::id()));
    let leaf = format!("shipyard/monitors/{id}");
    assert!(!own.ends_with(&leaf), "the test's own cgroup: {own}");
    let (init, monitor) = monitor_of("watched");
    let monitors = cgroup_of(monitor);
    assert!(monitors.ends_with(&leaf), "{monitors}");
    assert!(
        cgroup_of(init).ends_with(&format!("shipyard/runtime/{id}")),
        "{}",
        cgroup_of(init)
    );
    // A policy restart (its end from outside): its new monitor, there too.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(init).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    eventually("restarted", || {
        let shown = stores.inspect("watched");
        shown["restart_count"] == 1 && shown["state"]["status"] == "running"
    });
    let (_, restarted) = monitor_of("watched");
    assert_ne!(restarted, monitor);
    assert_eq!(cgroup_of(restarted), monitors);
    stores.ok(&["rm", "-f", "watched"]);
    let path = std::path::Path::new("/sys/fs/cgroup").join(monitors.trim_start_matches('/'));
    assert!(!path.exists(), "{} left", path.display());
}

/// A container publishing a range of ports runs: its monitor holds every
/// socket (two a port, on a network with IPv6). One publishing more than a
/// monitor holds is refused before anything is made, saying why. As root, in
/// a network namespace of the test's own.
#[test]
fn a_range_of_ports_is_published_and_too_many_are_refused_first() {
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "ranged",
            "--network",
            "app",
            "-p",
            "20000-20099:20000-20099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(stores.ok(&["port", "ranged"]).lines().count(), 200);
        stores.ok(&["rm", "-f", "ranged"]);
        let refused = stores.shipyard(&[
            "run",
            "-d",
            "--name",
            "too-many",
            "--network",
            "app",
            // 1,100 ports (a range has at most 1,024): two sockets each,
            // past what a monitor holds.
            "-p",
            "21000-21999:21000-21999",
            "-p",
            "22000-22099:22000-22099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(
            said.contains("sockets, which a container's monitor holds"),
            "{said}"
        );
        assert!(
            !stores.ok(&["ps", "-a"]).contains("too-many"),
            "nothing made"
        );
    });
}

/// A container whose published ports fit in a monitor's holds only without
/// its other holds (its lease, run directory, writable area, `running`) is
/// refused before anything is made: 1,050 ports on both families are 2,100
/// sockets, every hold a monitor takes, with nothing left for the rest. Its
/// descriptor limit raised as far as it goes, so the monitor's own bound is
/// the one met. 100 ports run (the control). As root, in a network
/// namespace of the test's own.
#[test]
fn ports_that_leave_no_room_for_the_other_holds_are_refused_first() {
    use std::os::unix::process::CommandExt as _;
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use nix::sys::resource::{Resource, getrlimit, setrlimit};
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let raised = |args: &[&str]| {
            let mut command = stores.command(args);
            // SAFETY: `getrlimit` and `setrlimit` are async-signal-safe
            // system calls; nothing is allocated.
            unsafe {
                command.pre_exec(|| {
                    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
                    setrlimit(Resource::RLIMIT_NOFILE, hard, hard)?;
                    Ok(())
                });
            }
            command.output().expect("running shipyard")
        };
        let ran = raised(&[
            "run",
            "-d",
            "--name",
            "fits",
            "--network",
            "app",
            "-p",
            "20000-20099:20000-20099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(stores.ok(&["port", "fits"]).lines().count(), 200);
        stores.ok(&["rm", "-f", "fits"]);
        let refused = raised(&[
            "run",
            "-d",
            "--name",
            "no-room",
            "--network",
            "app",
            // 1,050 ports: 2,100 sockets, and the other holds on top.
            "-p",
            "21000-22023:21000-22023",
            "-p",
            "23000-23025:23000-23025",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(
            said.contains("sockets, which a container's monitor holds with up to"),
            "{said}"
        );
        assert!(
            !stores.ok(&["ps", "-a"]).contains("no-room"),
            "nothing made"
        );
    });
}

/// Under a hard descriptor limit of 1,024, a run asks only for the holds
/// its monitor can hand its exit command (each hold's copy sits above it
/// while they're placed: 478 here), where its soft limit alone would let
/// 960 by: 300 ports on both families (605 holds) are refused before
/// anything is made, saying the bound; 200 (405) run (the control). As
/// root, in a network namespace of the test's own.
#[test]
fn ports_are_bound_by_the_room_to_hand_them_on() {
    use std::os::unix::process::CommandExt as _;
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use nix::sys::resource::{Resource, setrlimit};
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let limited = |args: &[&str]| {
            let mut command = stores.command(args);
            // SAFETY: `setrlimit` is an async-signal-safe system call;
            // nothing is allocated.
            unsafe {
                command.pre_exec(|| {
                    setrlimit(Resource::RLIMIT_NOFILE, 1_024, 1_024)?;
                    Ok(())
                });
            }
            command.output().expect("running shipyard")
        };
        let ran = limited(&[
            "run",
            "-d",
            "--name",
            "fits",
            "--network",
            "app",
            "-p",
            "20000-20199:20000-20199",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert!(ran.status.success(), "{ran:?}");
        stores.ok(&["rm", "-f", "fits"]);
        let refused = limited(&[
            "run",
            "-d",
            "--name",
            "no-room",
            "--network",
            "app",
            "-p",
            "21000-21299:21000-21299",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(said.contains("at most 478 in all here"), "{said}");
        assert!(
            !stores.ok(&["ps", "-a"]).contains("no-room"),
            "nothing made"
        );
    });
}

/// A volume a container mounts is kept while the container is, running or
/// stopped (Docker's): `volume rm` refuses it naming the container (1),
/// `volume prune` keeps it as in use; once the container is removed,
/// `volume rm` removes it.
#[test]
fn a_stopped_containers_volume_is_kept_until_it_is_removed() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        "--volume",
        "data:/data",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.ok(&["stop", "-t", "1", "web"]);
    let refused = stores.shipyard(&["volume", "rm", "data"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("the volume data is in use by web"), "{said}");
    let pruned = stores.ok(&["volume", "prune", "-f"]);
    assert!(pruned.contains("Kept 1 in use: data"), "{pruned}");
    assert!(stores.ok(&["volume", "ls"]).contains("data"), "pruned");
    stores.ok(&["rm", "web"]);
    stores.ok(&["volume", "rm", "data"]);
    assert!(!stores.ok(&["volume", "ls"]).contains("data"), "kept");
}

/// A foreground run of an image is a container (Docker's): kept once it
/// ends, its exit code `run`'s, its logs and a new start there; `--rm`
/// leaves nothing; a caller without a state directory for containers runs
/// it here instead, saying so.
#[test]
fn a_foreground_run_is_a_container_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.clone();
    let ran = stores.shipyard(&[
        "run",
        "--name",
        "fg",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "echo once; exit 3",
    ]);
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "once\n");
    assert!(stores.ok(&["ps", "-a"]).contains("fg"));
    assert_eq!(stores.ok(&["logs", "fg"]), "once\n");
    let again = stores.shipyard(&["start", "-a", "fg"]);
    assert_eq!(again.status.code(), Some(3), "{again:?}");
    assert_eq!(stores.ok(&["logs", "fg"]), "once\nonce\n");
    // Kept, it keeps its image: `rmi` refuses it until it's removed.
    let refused = stores.shipyard(&["rmi", &image]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("used by container"),
        "{refused:?}"
    );
    stores.ok(&["rm", "fg"]);
    let removed = stores.shipyard(&[
        "run",
        "--rm",
        "--sig-proxy=false",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "echo gone",
    ]);
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "--rm left it");
    if !shipyard_test_support::is_root() {
        let here = Command::new(SHIPYARD.path())
            .args(["run", "--network", "none", &image, "sh", "-c", "echo here"])
            .env("SHIPYARD_IMAGE_STORE", stores.scratch.store())
            .env(
                "SHIPYARD_CONTAINER_DIR",
                stores.scratch.0.join("containers"),
            )
            .env("SHIPYARD_NETWORK_DIR", stores.scratch.0.join("networks"))
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .expect("running shipyard");
        assert!(here.status.success(), "{here:?}");
        assert!(
            String::from_utf8_lossy(&here.stderr).contains("this run isn't a container"),
            "{here:?}"
        );
        assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "no record without one");
    }
}

/// Docker's exit codes (step 11.3a, checked against Docker 29.8): a
/// command's own failure exits 1, `run`'s 125, a command line that can't be
/// taken 125.
#[test]
fn exit_codes_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let registry = stores.image.rsplit_once('/').unwrap().0;
    let missing = format!("{registry}/not-there:1");
    for (args, code) in [
        (&["rm", "nope"][..], 1),
        (&["inspect", "nope"], 1),
        (&["stop", "nope"], 1),
        (&["logs", "nope"], 1),
        (&["exec", "nope", "true"], 1),
        (&["start", "-a", "nope"], 1),
        (&["container", "rm", "nope"], 1),
        (&["image", "inspect", "nope:1"], 1),
        (&["create", "--network", "none", &missing], 1),
        (&["run", "--network", "none", &missing], 125),
        (&["container", "run", "--network", "none", &missing], 125),
        (&["rm", "--no-such-flag", "nope"], 125),
        (&["run", "-h"], 125),
    ] {
        let failed = stores.shipyard(args);
        assert_eq!(failed.status.code(), Some(code), "{args:?}: {failed:?}");
        assert!(!failed.stderr.is_empty(), "{args:?} says why");
    }
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "nothing made");
}

/// Docker's names: the `container` and `image` groups, `run`'s `-v` and
/// `-h`, `tag` and `image inspect`, `logs -n`, `rm -v`.
#[test]
fn docker_s_names_and_short_forms_work() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "container",
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        "-v",
        "data:/d",
        "-h",
        "box",
        &stores.image,
        "sh",
        "-c",
        "echo hi > /d/f; echo one; echo two; sleep 300",
    ]);
    assert_eq!(
        stores
            .ok(&["container", "exec", "web", "cat", "/etc/hostname"])
            .trim(),
        "box"
    );
    assert_eq!(stores.ok(&["exec", "web", "cat", "/d/f"]).trim(), "hi");
    assert!(stores.ok(&["container", "ls"]).contains("web"));
    eventually("its output", || stores.ok(&["logs", "web"]).contains("two"));
    assert_eq!(
        stores.ok(&["container", "logs", "-n", "1", "web"]).trim(),
        "two"
    );

    let registry = stores.image.rsplit_once('/').unwrap().0;
    let tagged = format!("{registry}/base:copy");
    stores.ok(&["tag", &stores.image, &tagged]);
    let named = ["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"];
    assert!(
        stores.ok(&named).lines().any(|line| line == tagged),
        "{}",
        stores.ok(&named)
    );
    let shown: serde_json::Value =
        serde_json::from_str(&stores.ok(&["image", "inspect", &tagged])).unwrap();
    assert_eq!(
        shown[0]["Id"],
        stores.docker_inspect("web")["Image"],
        "{shown}"
    );
    stores.ok(&["image", "rm", &tagged]);
    let left = stores.ok(&named);
    assert!(
        !left.lines().any(|line| line == tagged) && !left.is_empty(),
        "untagged, the image itself kept: {left}"
    );
    stores.ok(&["container", "rm", "-fv", "web"]);
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
    stores.ok(&["volume", "rm", "data"]);
}

/// `system recover` with a container it can't start (its bind source gone
/// since the shutdown) starts the rest and exits 125, the boot unit's
/// success (`SuccessExitStatus`): not Docker's 1, which would fail the
/// unit and end what it started.
#[test]
fn a_partial_recovery_exits_as_the_boot_unit_expects() {
    // Rootless only: `system stop` marks the runtime's state root as
    // stopping until a `system recover`, and root's (`/run/shipyard`) is
    // every root test's, whose restart policies it would hold off.
    if is_root() {
        eprintln!("skipping: root's runtime state is every root test's");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    let source = stores.scratch.0.join("bound");
    std::fs::create_dir(&source).unwrap();
    let bind = format!("{}:/b", source.display());
    for (name, extra) in [("kept", None), ("lost", Some(bind.as_str()))] {
        let mut args = vec!["run", "-d", "--restart", "always", "--name", name];
        args.extend(["--network", "none"]);
        if let Some(bind) = extra {
            args.extend(["-v", bind]);
        }
        args.extend([stores.image.as_str(), "sleep", "300"]);
        stores.ok(&args);
    }
    // A shutdown's stop, then the bind's source gone before the boot.
    stores.ok(&["system", "stop"]);
    std::fs::remove_dir(&source).unwrap();
    let recovered = stores.shipyard(&["system", "recover"]);
    assert_eq!(recovered.status.code(), Some(125), "{recovered:?}");
    assert_eq!(String::from_utf8_lossy(&recovered.stdout).trim(), "kept");
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("container lost"),
        "{recovered:?}"
    );
    stores.ok(&["rm", "-f", "kept", "lost"]);
}

/// `--env-file` and `--label-file` (Docker's format), with `-e` and `-l`
/// winning; `--cidfile` written with the ID, and refused once it's there.
#[test]
fn files_labels_and_the_id_file_are_docker_s() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let env = stores.scratch.0.join("env");
    std::fs::write(&env, "# vars\nA=from-file\nB=b c\nCOPIED\n").unwrap();
    let labels = stores.scratch.0.join("labels");
    std::fs::write(&labels, "tier=file\nempty\n").unwrap();
    let cid = stores.scratch.0.join("cid");
    let mut command = stores.command(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        "--env-file",
    ]);
    command
        .arg(&env)
        .args(["-e", "A=from-flag", "--label-file"])
        .arg(&labels);
    command
        .args(["-l", "tier=flag", "-l", "app=x", "--cidfile"])
        .arg(&cid);
    command
        .args([stores.image.as_str(), "sleep", "300"])
        .env("COPIED", "yes");
    let made = command.output().unwrap();
    assert!(made.status.success(), "{made:?}");
    let id = String::from_utf8_lossy(&made.stdout).trim().to_owned();
    assert_eq!(std::fs::read_to_string(&cid).unwrap(), id, "the ID file");
    let shown = stores.ok(&["exec", "web", "sh", "-c", "echo $A,$B,$COPIED"]);
    assert_eq!(shown.trim(), "from-flag,b c,yes");
    let docker = stores.docker_inspect("web");
    let labels_shown = &docker["Config"]["Labels"];
    assert_eq!(labels_shown["tier"], "flag", "{labels_shown}");
    assert_eq!(labels_shown["app"], "x");
    assert_eq!(labels_shown["empty"], "");
    // The ID file there: refused before anything is made (a run's 125).
    let again = stores.shipyard(&[
        "run",
        "-d",
        "--network",
        "none",
        "--cidfile",
        cid.to_str().unwrap(),
        &stores.image,
        "true",
    ]);
    assert_eq!(again.status.code(), Some(125), "{again:?}");
    assert!(String::from_utf8_lossy(&again.stderr).contains("is there already"));
    assert_eq!(
        stores.ok(&["ps", "-a", "-q"]).lines().count(),
        1,
        "nothing more made"
    );
    // A bad env file: a usage error, its value never said.
    std::fs::write(&env, "MY KEY=secret\n").unwrap();
    let bad = stores.shipyard(&["create", "--env-file", env.to_str().unwrap(), &stores.image]);
    assert_eq!(bad.status.code(), Some(125), "{bad:?}");
    assert!(!String::from_utf8_lossy(&bad.stderr).contains("secret"));
    stores.ok(&["rm", "-f", "web"]);
}

/// `-a` attaches only the streams asked; `-q` pulls without progress;
/// `--privileged` and `--init` are refused, saying what to use.
#[test]
fn attach_quiet_and_the_refused_flags() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let script = "echo out; echo err >&2";
    let only_out = stores.shipyard(&[
        "run",
        "--rm",
        "-q",
        "-a",
        "stdout",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    assert!(only_out.status.success(), "{only_out:?}");
    assert_eq!(String::from_utf8_lossy(&only_out.stdout).trim(), "out");
    let stderr = String::from_utf8_lossy(&only_out.stderr);
    assert!(
        !stderr.contains("err") && !stderr.contains("Pulling"),
        "{stderr}"
    );
    let only_err = stores.shipyard(&[
        "run",
        "--rm",
        "-a",
        "STDERR",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    assert_eq!(String::from_utf8_lossy(&only_err.stdout), "");
    assert!(String::from_utf8_lossy(&only_err.stderr).contains("err"));
    // `-a stdin` and `-i` go together: one without the other is refused.
    for args in [&["-i", "-a", "stdout"][..], &["-a", "stdin"]] {
        let mut argv = vec!["run", "--rm", "--network", "none"];
        argv.extend_from_slice(args);
        argv.extend([stores.image.as_str(), "true"]);
        let refused = stores.shipyard(&argv);
        assert_eq!(refused.status.code(), Some(125), "{args:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("-a stdin and -i go together"),
            "{refused:?}"
        );
    }
    for (flag, words) in [
        (
            "--privileged",
            "--privileged isn't supported: give the container the capabilities",
        ),
        ("--init", "--init isn't supported yet"),
    ] {
        let refused = stores.shipyard(&["run", flag, "--network", "none", &stores.image, "true"]);
        assert_eq!(refused.status.code(), Some(125), "{flag}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(words),
            "{refused:?}"
        );
    }
}

/// `-q` pulls without progress; without it, the pull is said (`--pull
/// always`: the stores' probe pulled the image already).
#[test]
fn a_quiet_create_says_no_pull() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let create = |quiet: bool| {
        let mut args = vec!["create", "--pull", "always", "--network", "none"];
        if quiet {
            args.push("-q");
        }
        args.push(&stores.image);
        let made = stores.shipyard(&args);
        assert!(made.status.success(), "{made:?}");
        String::from_utf8_lossy(&made.stderr).contains("Pulling")
    };
    assert!(create(false), "the pull said");
    assert!(!create(true), "the pull not said");
}

/// `--expose` adds to the image's ports: shown by `inspect`, published by
/// `-P` (rootless through pasta; as root on the default bridge, in a
/// network namespace of the test's own).
#[test]
fn exposed_ports_are_shown_and_published_with_dash_p() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let check = move || {
        let made = stores.ok(&[
            "create",
            "--name",
            "listed",
            "--network",
            "none",
            "--expose",
            "7001/udp",
            "--expose",
            "8000-8001",
            &stores.image,
            "true",
        ]);
        assert!(!made.is_empty());
        let exposed = &stores.docker_inspect("listed")["Config"]["ExposedPorts"];
        for port in ["7001/udp", "8000/tcp", "8001/tcp"] {
            assert!(exposed.get(port).is_some(), "{port}: {exposed}");
        }
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "published",
            "--expose",
            "7002",
            "-P",
            &stores.image,
            "sleep",
            "300",
        ]);
        let ports = stores.ok(&["port", "published"]);
        assert!(ports.contains("7002/tcp"), "{ports}");
        // Without -P, an exposed port is published nowhere.
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "unpublished",
            "--expose",
            "7003",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(stores.ok(&["port", "unpublished"]), "");
        stores.ok(&["rm", "-f", "listed", "published", "unpublished"]);
    };
    if is_root() {
        shipyard_test_support::in_new_netns(move || {
            use shipyard_network::route::{LinkChange, Rtnetlink};
            let mut host = Rtnetlink::new().unwrap();
            let lo = host.link("lo").unwrap().unwrap().index;
            host.change(lo, LinkChange::Up(true)).unwrap();
            check();
        });
    } else {
        check();
    }
}

/// F62: a container that asks no process limit gets 2,048 where the
/// `pids` controller reaches it; `--pids-limit -1` asks none.
#[test]
fn a_container_gets_a_process_limit_unless_it_asks_none() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let pids_max = |name: &str| -> Option<String> {
        let pid = stores.docker_inspect(name)["State"]["Pid"]
            .as_i64()
            .unwrap();
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let path = cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .to_owned();
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/pids.max"))
            .ok()
            .map(|text| text.trim().to_owned())
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "limited",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "unlimited",
        "--network",
        "none",
        "--pids-limit",
        "-1",
        &stores.image,
        "sleep",
        "300",
    ]);
    // `0` is unset (as Docker's CLI sends it): the default.
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "zero",
        "--network",
        "none",
        "--pids-limit",
        "0",
        &stores.image,
        "sleep",
        "300",
    ]);
    // Where the controller reaches the containers' cgroups (root; a
    // tester whose delegation has it), as the engine's `Tree::offers`
    // decides it: the base's `cgroup.controllers`.
    let base = if is_root() {
        Some(std::path::PathBuf::from("/sys/fs/cgroup"))
    } else {
        shipyard_test_support::delegated_cgroup()
    };
    let offered = base.is_some_and(|base| {
        std::fs::read_to_string(base.join("cgroup.controllers"))
            .is_ok_and(|text| text.split_whitespace().any(|name| name == "pids"))
    });
    if offered {
        assert_eq!(pids_max("limited").as_deref(), Some("2048"));
        assert_eq!(pids_max("zero").as_deref(), Some("2048"));
        assert_eq!(pids_max("unlimited").as_deref(), Some("max"));
        // Never the container's own: none asked, as inspect shows.
        let asked = &stores.docker_inspect("limited")["HostConfig"]["PidsLimit"];
        assert!(matches!(asked.as_i64(), None | Some(0)), "{asked}");
    } else {
        // Without it, no limit (no `pids.max`, or one that says `max`:
        // a cgroup it shares), and nothing refused.
        let limit = pids_max("limited");
        assert!(matches!(limit.as_deref(), None | Some("max")), "{limit:?}");
    }
    stores.ok(&["rm", "-f", "limited", "unlimited", "zero"]);
}

/// What a create settled beside the record (`created.json`) that can't be
/// read refuses the start (D11.3e2-2: it may hold tmpfs mounts), naming the
/// file; `inspect` still shows the container, saying what it can't; `rm`
/// is the way out. (11.3b1 started it without: reversed in 11.3e2.)
#[test]
fn an_unreadable_created_file_refuses_the_start() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "create",
            "--name",
            "web",
            "--network",
            "none",
            "--expose",
            "7004",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    let file = stores
        .scratch
        .0
        .join("containers")
        .join(&id)
        .join("created.json");
    assert!(file.exists(), "--expose is kept beside the record");
    std::fs::write(&file, "not json").unwrap();
    let refused = stores.shipyard(&["start", "web"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("create can't be read"), "{said}");
    assert!(said.contains("created.json"), "{said}");
    let inspected = stores.shipyard(&["inspect", "web"]);
    assert!(inspected.status.success(), "{inspected:?}");
    assert!(
        String::from_utf8_lossy(&inspected.stderr).contains(
            "its exposed ports, tmpfs mounts and the namespaces it may join can't be shown"
        ),
        "{inspected:?}"
    );
    stores.ok(&["rm", "web"]);
}

/// F101: `ps` and `inspect` show the command and variables a start runs,
/// its image's where it gave none (by the start's own rules).
#[test]
fn ps_and_inspect_show_the_image_s_own_command() {
    let Some(stores) = Stores::with_image_config(
        r#"{"Entrypoint":["/bin/sh","-c"],"Cmd":["echo from-image; sleep 300"],
            "Env":["MODE=image","FROM_IMAGE=yes"],"WorkingDir":"/tmp"}"#,
    ) else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "own",
        "--network",
        "none",
        "-e",
        "MODE=mine",
        &stores.image,
    ]);
    let listed = stores.ok(&["ps"]);
    assert!(listed.contains("\"/bin/sh -c echo"), "{listed}");
    let shown = stores.docker_inspect("own");
    assert_eq!(shown["Path"], "/bin/sh", "{shown}");
    assert_eq!(
        shown["Args"],
        serde_json::json!(["-c", "echo from-image; sleep 300"])
    );
    let config = &shown["Config"];
    assert_eq!(config["Entrypoint"], serde_json::json!(["/bin/sh", "-c"]));
    assert_eq!(
        config["Cmd"],
        serde_json::json!(["echo from-image; sleep 300"])
    );
    assert_eq!(config["WorkingDir"], "/tmp");
    let env: Vec<&str> = config["Env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        env.contains(&"MODE=mine") && env.contains(&"FROM_IMAGE=yes"),
        "{env:?}"
    );
    assert!(!env.contains(&"MODE=image"), "its own wins: {env:?}");
    // And it's what runs.
    assert_eq!(
        stores
            .ok(&["exec", "own", "sh", "-c", "echo $MODE,$FROM_IMAGE"])
            .trim(),
        "mine,yes"
    );
    // Its tag removed (as a pull that moves it would leave it): still
    // the image it runs, read by its digest, not by a tag.
    stores.ok(&["rmi", "-f", &stores.image]);
    let listed = stores.ok(&["ps"]);
    assert!(listed.contains("\"/bin/sh -c echo"), "after rmi: {listed}");
    assert_eq!(
        stores.docker_inspect("own")["Config"]["Cmd"],
        serde_json::json!(["echo from-image; sleep 300"])
    );
    stores.ok(&["rm", "-f", "own"]);
}

/// `exec --env-file` (before `-e`, which wins), `--detach-keys` refused;
/// `logs -n all` and `--until`; `stop -t -1` waits for as long as it takes.
#[test]
fn exec_logs_and_stop_take_docker_s_flags() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo one; echo two; trap 'exit 0' TERM; while :; do sleep 0.1; done",
    ]);
    let env = stores.scratch.0.join("exec.env");
    std::fs::write(&env, "A=1\nB=2\n").unwrap();
    let mut command = stores.command(&["exec", "--env-file"]);
    command
        .arg(&env)
        .args(["-e", "B=3", "web", "sh", "-c", "echo $A$B"]);
    let ran = command.output().unwrap();
    assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "13", "{ran:?}");
    let refused = stores.shipyard(&["exec", "--detach-keys", "ctrl-x", "web", "true"]);
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--detach-keys isn't supported for exec"),
        "{refused:?}"
    );
    eventually("its two lines", || {
        stores.ok(&["logs", "web"]).contains("two")
    });
    assert_eq!(stores.ok(&["logs", "-n", "all", "web"]), "one\ntwo\n");
    assert_eq!(stores.ok(&["logs", "--tail", "1", "web"]), "two\n");
    // Up to an hour ago: before it ran, so nothing.
    assert_eq!(stores.ok(&["logs", "--until", "1h", "web"]), "");
    // Up to now: all of it; and a follow up to a moment from now ends
    // there, with what came.
    assert_eq!(stores.ok(&["logs", "--until", "0s", "web"]), "one\ntwo\n");
    let soon = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 2;
    let followed = Instant::now();
    let shown = stores.ok(&["logs", "-f", "--until", &soon.to_string(), "web"]);
    assert_eq!(shown, "one\ntwo\n");
    assert!(
        followed.elapsed() < Duration::from_secs(15),
        "the follow ended at --until"
    );
    // `exec --privileged` refused; an env file that can't be read is
    // refused by its path, before anything runs.
    let refused = stores.shipyard(&["exec", "--privileged", "web", "true"]);
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--privileged isn't supported"),
        "{refused:?}"
    );
    let missing = stores.shipyard(&["exec", "--env-file", "/nonexistent.env", "web", "true"]);
    assert_eq!(missing.status.code(), Some(125), "{missing:?}");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("/nonexistent.env"),
        "{missing:?}"
    );
    let started = Instant::now();
    stores.ok(&["stop", "-t", "-1", "web"]);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "its TERM trap ended it"
    );
    assert!(stores.ok(&["ps", "-a"]).contains("Exited (0)"));
    stores.ok(&["rm", "web"]);
}

/// The `labels.json` of the volume or network `name` in `stores`, if any.
fn labels_file(stores: &Stores, kind: &str, name: &str) -> Option<serde_json::Value> {
    let mut dirs = vec![stores.scratch.0.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.ends_with(format!("{kind}/{name}/labels.json")) {
                return serde_json::from_slice(&std::fs::read(&path).ok()?).ok();
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(path);
            }
        }
    }
    None
}

/// `volume create` takes Docker's `-l`, `-d local` and no name (a new
/// one); driver options and other drivers are refused. As root,
/// `network create` takes `-l` and Docker's `-o` options as the API does.
#[test]
fn volume_and_network_create_take_labels_and_options() {
    let Some(stores) = Stores::new() else {
        return;
    };
    assert_eq!(
        stores
            .ok(&["volume", "create", "-l", "tier=db", "-l", "bare", "data"])
            .trim(),
        "data"
    );
    let labels = labels_file(&stores, "volumes", "data").expect("its labels.json");
    assert_eq!(
        labels["labels"],
        serde_json::json!({"tier": "db", "bare": ""})
    );
    let anonymous = stores.ok(&["volume", "create"]);
    let anonymous = anonymous.trim();
    assert!(
        anonymous.len() == 64 && anonymous.bytes().all(|b| b.is_ascii_hexdigit()),
        "{anonymous}"
    );
    for (args, words) in [
        (
            &["volume", "create", "-o", "type=nfs", "nfs"][..],
            "volume driver options aren't supported",
        ),
        (
            &["volume", "create", "-d", "nfs", "other"],
            "the volume driver nfs isn't supported: only local",
        ),
    ] {
        let refused = stores.shipyard(args);
        assert_eq!(refused.status.code(), Some(1), "{args:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(words),
            "{refused:?}"
        );
    }
    stores.ok(&["volume", "rm", "data", anonymous]);
    if !is_root() {
        return;
    }
    shipyard_test_support::in_new_netns(move || {
        stores.ok(&[
            "network",
            "create",
            "-l",
            "team=a",
            "-o",
            "com.shipyard.network.isolate-host=true",
            "--subnet",
            "10.251.7.0/24",
            "labelled",
        ]);
        let meta = labels_file(&stores, "networks", "labelled").expect("its labels.json");
        assert_eq!(meta["labels"]["team"], "a", "{meta}");
        assert_eq!(meta["options"]["com.shipyard.network.isolate-host"], "true");
        let refused = stores.shipyard(&[
            "network",
            "create",
            "-o",
            "com.docker.network.bridge.name=x",
            "other",
        ]);
        assert_eq!(refused.status.code(), Some(1), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("com.docker.network.bridge.name"),
            "{refused:?}"
        );
        stores.ok(&["network", "rm", "labelled"]);
    });
}

/// `--network container:NAME` (step 11.4b2b1): a container joins another's
/// network as it is (the same namespace, its hostname), with its own mount
/// namespace; it's shown as Docker shows it, keeps the other by its ID
/// through a rename, is said to lose it when the other stops, and can't
/// start while the other isn't running. A join of a join, and a container
/// that isn't there, are refused. Rootless, the other's user namespace is
/// joined too.
#[test]
fn a_container_joins_another_s_network() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.as_str();
    let first = stores.ok(&[
        "run",
        "-d",
        "--name",
        "first",
        "--network",
        "none",
        image,
        "sleep",
        "300",
    ]);
    let first = first.trim().to_owned();
    let pid = stores.ok(&["inspect", "-f", "{{.State.Pid}}", "first"]);
    let link = |kind: &str| {
        std::fs::read_link(format!("/proc/{}/ns/{kind}", pid.trim()))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let seen = stores.ok(&[
        "run",
        "--rm",
        "--network",
        "container:first",
        image,
        "sh",
        "-c",
        "readlink /proc/self/ns/net; readlink /proc/self/ns/mnt; cat /etc/hostname; \
         cat /proc/sys/kernel/hostname",
    ]);
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(lines.first(), Some(&link("net").as_str()), "{seen}");
    assert_ne!(
        lines.get(1),
        Some(&link("mnt").as_str()),
        "its own mounts: {seen}"
    );
    assert_eq!(
        lines.get(2),
        Some(&&first[..12]),
        "the first's hostname: {seen}"
    );
    assert_eq!(
        lines.get(3),
        Some(&&first[..12]),
        "its UTS namespace's too: {seen}"
    );
    // The control: the first's network isn't this process's.
    assert_ne!(
        link("net"),
        std::fs::read_link("/proc/self/ns/net")
            .unwrap()
            .to_string_lossy()
    );

    stores.ok(&[
        "create",
        "--name",
        "third",
        "--network",
        "container:first",
        image,
        "sleep",
        "300",
    ]);
    assert_eq!(
        stores
            .ok(&[
                "inspect",
                "-f",
                "{{.HostConfig.NetworkMode}} {{json .NetworkSettings.Networks}}",
                "third"
            ])
            .trim(),
        format!("container:{first} {{}}")
    );
    // By ID: a rename of the first changes nothing.
    stores.ok(&["rename", "first", "renamed"]);
    stores.ok(&["start", "third"]);
    let third_pid = stores.ok(&["inspect", "-f", "{{.State.Pid}}", "third"]);
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/ns/net", third_pid.trim()))
            .unwrap()
            .to_string_lossy(),
        link("net")
    );
    if is_root() {
        let connected = stores.shipyard(&["network", "connect", "bridge", "third"]);
        assert_eq!(connected.status.code(), Some(1), "{connected:?}");
        assert!(
            String::from_utf8_lossy(&connected.stderr)
                .contains("container sharing network namespace with another container"),
            "{connected:?}"
        );
    }
    joins_of_nothing_are_refused(&stores);
    the_joined_one_stops(&stores);
}

/// [`a_container_joins_another_s_network`]'s end: `renamed` (the one
/// `third` joins) stopped, said, and `third` can't start again without it.
fn the_joined_one_stops(stores: &Stores) {
    // The first stopped: said, and the third can't start again without it.
    let stopped = stores.shipyard(&["stop", "-t", "1", "renamed"]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(
        String::from_utf8_lossy(&stopped.stderr)
            .contains("third shares its namespaces, and loses them until started again"),
        "{stopped:?}"
    );
    stores.ok(&["stop", "-t", "1", "third"]);
    let started = stores.shipyard(&["start", "third"]);
    assert_eq!(started.status.code(), Some(1), "{started:?}");
    assert!(
        String::from_utf8_lossy(&started.stderr)
            .contains("cannot join network of a non running container: renamed"),
        "{started:?}"
    );
    stores.ok(&["rm", "-f", "third", "renamed"]);
}

/// [`a_container_joins_another_s_network`]'s refusals: a join of `third`
/// (which joins another's), and one of nothing.
fn joins_of_nothing_are_refused(stores: &Stores) {
    let image = stores.image.as_str();
    // Refused: a join of a join, and one of nothing.
    for (network, said) in [
        (
            "container:third",
            "joins another container's network itself",
        ),
        ("container:nothing-here", "no container \"nothing-here\""),
    ] {
        let refused = stores.shipyard(&["run", "--rm", "--network", network, image, "true"]);
        assert_eq!(refused.status.code(), Some(125), "{network}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(said),
            "{network}: {refused:?}"
        );
    }
}

/// `--ipc container:NAME` (step 11.4b2b2): a container joins another's IPC
/// namespace and its `/dev/shm` (a file one writes there, the other
/// reads), with its own mount namespace; shown as Docker shows it. IPC
/// settings are the other's (refused); rootless, the network and the IPC
/// namespace must be one container's.
#[test]
fn a_container_joins_another_s_ipc() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.as_str();
    let first = stores.ok(&[
        "run",
        "-d",
        "--name",
        "first",
        "--network",
        "none",
        image,
        "sh",
        "-c",
        "echo from-a > /dev/shm/marker; sleep 300",
    ]);
    let first = first.trim().to_owned();
    shipyard_test_support::eventually(Duration::from_secs(10), "the first's marker", || {
        stores
            .shipyard(&["exec", "first", "cat", "/dev/shm/marker"])
            .status
            .success()
    });
    let pid = stores.ok(&["inspect", "-f", "{{.State.Pid}}", "first"]);
    let link = |kind: &str| {
        std::fs::read_link(format!("/proc/{}/ns/{kind}", pid.trim()))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let seen = stores.ok(&[
        "run",
        "--rm",
        "--network",
        "container:first",
        "--ipc",
        "container:first",
        image,
        "sh",
        "-c",
        "readlink /proc/self/ns/ipc; readlink /proc/self/ns/mnt; cat /dev/shm/marker; \
         echo from-b > /dev/shm/back",
    ]);
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(lines.first(), Some(&link("ipc").as_str()), "{seen}");
    assert_ne!(
        lines.get(1),
        Some(&link("mnt").as_str()),
        "its own mounts: {seen}"
    );
    assert_eq!(
        lines.get(2),
        Some(&"from-a"),
        "the first's /dev/shm: {seen}"
    );
    assert_eq!(
        stores.ok(&["exec", "first", "cat", "/dev/shm/back"]),
        "from-b\n"
    );
    // The control: the first's IPC namespace isn't this process's.
    assert_ne!(
        link("ipc"),
        std::fs::read_link("/proc/self/ns/ipc")
            .unwrap()
            .to_string_lossy()
    );
    stores.ok(&[
        "create",
        "--name",
        "second",
        "--ipc",
        "container:first",
        image,
        "true",
    ]);
    assert_eq!(
        stores
            .ok(&["inspect", "-f", "{{.HostConfig.IpcMode}}", "second"])
            .trim(),
        format!("container:{first}")
    );
    ipc_joins_refused(&stores);
    if is_root() {
        lets_go_of_their_mounts(&stores);
    }
    ipc_join_ends(&stores);
    if !is_root() {
        stores.ok(&["rm", "-f", "other"]);
    }
}

/// As root: a container that joins `first`'s IPC namespace keeps nothing
/// of `first`'s mount namespace once it runs (its init's descriptors name
/// no mount namespace), so `first`'s mounts go when it ends.
fn lets_go_of_their_mounts(stores: &Stores) {
    let image = stores.image.as_str();
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "holder",
        "--ipc",
        "container:first",
        image,
        "sleep",
        "300",
    ]);
    let pid = stores.ok(&["inspect", "-f", "{{.State.Pid}}", "holder"]);
    let links: Vec<String> = std::fs::read_dir(format!("/proc/{}/fd", pid.trim()))
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .map(|link| link.to_string_lossy().into_owned())
        .collect();
    // The control: it has descriptors to read.
    assert!(!links.is_empty());
    assert!(
        !links.iter().any(|link| link.starts_with("mnt:[")),
        "{links:?}"
    );
    stores.ok(&["rm", "-f", "holder"]);
}

/// [`a_container_joins_another_s_ipc`]'s end: `first`'s IPC namespace
/// joined alone; then `second` (which joins it) can't start while `first`
/// is stopped, nor once it's gone (said so).
fn ipc_join_ends(stores: &Stores) {
    let image = stores.image.as_str();
    // Its IPC namespace alone (compose's `ipc: service:`), a network of its
    // own beside it.
    assert_eq!(
        stores.ok(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--ipc",
            "container:first",
            image,
            "cat",
            "/dev/shm/marker",
        ]),
        "from-a\n"
    );
    stores.ok(&["stop", "-t", "1", "first"]);
    let started = stores.shipyard(&["start", "second"]);
    assert_eq!(started.status.code(), Some(1), "{started:?}");
    assert!(
        String::from_utf8_lossy(&started.stderr)
            .contains("cannot join IPC of a non running container: first"),
        "{started:?}"
    );
    stores.ok(&["rm", "first"]);
    let started = stores.shipyard(&["start", "second"]);
    assert_eq!(started.status.code(), Some(1), "{started:?}");
    assert!(
        String::from_utf8_lossy(&started.stderr)
            .contains("the container whose IPC namespace it joins (")
            && String::from_utf8_lossy(&started.stderr).contains(") is gone: make this one again"),
        "{started:?}"
    );
    stores.ok(&["rm", "second"]);
}

/// [`a_container_joins_another_s_ipc`]'s refusals: IPC settings with
/// another's IPC namespace; a join of `second` (which joins `first`'s);
/// rootless, a network and an IPC namespace of two containers; as root,
/// `--uidmap` with it; `--ipc host`.
fn ipc_joins_refused(stores: &Stores) {
    let image = stores.image.as_str();
    let mut refused = vec![
        (
            vec!["--ipc", "container:first", "--sysctl", "kernel.shmmax=1"],
            "--sysctl kernel.shmmax can't be given with --ipc container:",
        ),
        (
            vec!["--ipc", "container:second"],
            "second joins another container's IPC namespace itself: join that one",
        ),
    ];
    if is_root() {
        refused.push((
            vec!["--uidmap", "0:100000:65536", "--ipc", "container:first"],
            "--uidmap can't go with --ipc container:first",
        ));
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "mapped",
            "--uidmap",
            "0:100000:65536",
            "--network",
            "none",
            image,
            "sleep",
            "300",
        ]);
        refused.push((
            vec!["--ipc", "container:mapped"],
            "mapped has a user namespace of its own (--uidmap)",
        ));
    }
    if !is_root() {
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "other",
            "--network",
            "none",
            image,
            "sleep",
            "300",
        ]);
        refused.push((
            vec!["--network", "container:other", "--ipc", "container:first"],
            "--network container: and --ipc container: name two containers",
        ));
    }
    for (flags, said) in refused {
        let mut args = vec!["run", "--rm"];
        args.extend(&flags);
        args.extend([image, "true"]);
        let output = stores.shipyard(&args);
        assert_eq!(output.status.code(), Some(125), "{flags:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(said),
            "{flags:?}: {output:?}"
        );
    }
    if is_root() {
        stores.ok(&["rm", "-f", "mapped"]);
    }
    ipc_modes_refused(stores);
}

/// [`ipc_joins_refused`]'s end: a join without a root filesystem of its
/// own, and `--ipc host`.
fn ipc_modes_refused(stores: &Stores) {
    let image = stores.image.as_str();
    // Without a root of its own, its /dev/shm has nowhere to go.
    let no_root = stores.shipyard(&[
        "run",
        "--rm",
        "--no-image",
        "--ipc",
        "container:first",
        "true",
    ]);
    assert_eq!(no_root.status.code(), Some(125), "{no_root:?}");
    assert!(
        String::from_utf8_lossy(&no_root.stderr).contains("needs a root filesystem of its own"),
        "{no_root:?}"
    );
    let host = stores.shipyard(&["run", "--rm", "--ipc", "host", image, "true"]);
    assert_eq!(host.status.code(), Some(125), "{host:?}");
    assert!(
        String::from_utf8_lossy(&host.stderr).contains("give private, shareable or container:NAME"),
        "{host:?}"
    );
}

/// A test pause's file, removed when dropped: the start it holds goes on
/// whatever the test does.
struct Release<'a>(&'a std::path::Path);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// F119: a container whose init ended while it waited to be started (here
/// killed, in the start's window a test build holds open; in the one seen,
/// during F132's guest crashes) is said so: its command never ran, with
/// how it ended, not "is stopped; connect needs it created".
#[test]
fn a_container_ended_before_its_start_is_said() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.clone();
    let pause = stores.scratch.0.join("start-pause");
    let mut run = stores.command(&["run", "-d", "--name", "gone", &image, "sleep", "60"]);
    run.env("SHIPYARD_TEST_START_PAUSE", &pause);
    let run = run
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    while !pause.exists() {
        assert!(
            began.elapsed() < std::time::Duration::from_secs(60),
            "no pause"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // (Its start let go whatever this test does after: no run held a minute.)
    let release = Release(&pause);
    let id = std::fs::read_to_string(&pause).unwrap();
    let root = if is_root() {
        std::path::PathBuf::from("/run/shipyard")
    } else {
        stores.scratch.0.join("xdg/shipyard")
    };
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(&id).join("state.json")).unwrap()).unwrap();
    let init = i32::try_from(state["init"][0].as_i64().unwrap()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(init),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    // Its end seen before the start goes on: recorded, or (a container
    // never started) its state cleared.
    let ended = || {
        let dir = root.join(&id);
        dir.join("exit").exists() || !dir.exists()
    };
    let began = std::time::Instant::now();
    while !ended() {
        assert!(
            began.elapsed() < std::time::Duration::from_secs(10),
            "no end"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(release);
    let ran = run.wait_with_output().unwrap();
    // A run's engine failure (Docker's 125), said as one of its two forms:
    // how it ended, if its exit was still recorded, or that nothing of it
    // is left (a container never started is cleared at its end).
    assert_eq!(ran.status.code(), Some(125), "{ran:?}");
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(
        said.contains("its process ended before it could start (killed by SIGKILL")
            || said.contains(
                "its process ended before it could start (how isn't known: nothing of it is \
                 left)"
            ),
        "{said}"
    );
    assert!(said.contains("its command never ran"), "{said}");
    let _ = stores.shipyard(&["rm", "-f", "gone"]);
}

/// A foreground `run -i` (no terminal) makes its container `StdinOnce`, as
/// `docker run -i` does (11.6a1's Murphy review): inspect says so, and a
/// later `start -ai` ends its input, and so `cat`, at its first end of
/// file; the control: `run -d -i` doesn't.
#[test]
fn a_foreground_run_i_closes_its_input_once() {
    use std::io::Write as _;
    use std::process::Stdio;
    let Some(stores) = Stores::new() else {
        return;
    };
    let typed = |args: &[&str], input: &[u8]| {
        let mut child = stores
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        // Bounded: an input that never ends leaves `cat` running.
        let began = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if began.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("{args:?}: its input never ended");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        child.wait_with_output().unwrap()
    };
    let image = stores.image.clone();
    let ran = typed(
        &[
            "run",
            "-i",
            "--name",
            "once",
            "--network",
            "none",
            &image,
            "cat",
        ],
        b"first\n",
    );
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "first\n");
    assert_eq!(
        stores.docker_inspect("once")["Config"]["StdinOnce"],
        true,
        "a foreground run -i"
    );
    let again = typed(&["start", "-a", "-i", "once"], b"again\n");
    assert!(again.status.success(), "{again:?}");
    assert_eq!(String::from_utf8_lossy(&again.stdout), "again\n");
    stores.ok(&[
        "run",
        "-d",
        "-i",
        "--name",
        "open",
        "--network",
        "none",
        &image,
        "cat",
    ]);
    assert_eq!(
        stores.docker_inspect("open")["Config"]["StdinOnce"],
        false,
        "run -d -i"
    );
    let _ = stores.shipyard(&["rm", "-f", "once", "open"]);
}
