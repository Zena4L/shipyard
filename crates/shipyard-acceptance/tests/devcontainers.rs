//! Docker's devcontainer CLI against Shipyard's Docker API (step 15.6a, D30):
//! `shipyard system service` on a socket of the test's own, `DOCKER_HOST`
//! pointing at it, and `devcontainer up` of a small `devcontainer.json` (the
//! fixture image, no build), its `postCreateCommand` run in the bound
//! workspace, `devcontainer exec` into it, then the container removed.
//!
//! Linux only, as the CLI needs Docker's; it's in the test image (pinned).
//! Without it the test says so and returns, unless
//! `SHIPYARD_TEST_DEVCONTAINERS=required` (test-linux.sh sets it; CI's
//! runners haven't the CLI, so there it skips, said).

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::Api;

/// `devcontainer ARGS`, against `api`'s service, within 5 minutes (a CLI
/// stuck on a stream would otherwise hold the whole pass).
fn devcontainer(api: &Api, args: &[&str]) -> Output {
    Command::new("timeout")
        .arg("300")
        .arg("devcontainer")
        .args(args)
        .env("DOCKER_HOST", format!("unix://{}", api.socket().display()))
        .output()
        .unwrap()
}

/// The devcontainers of a workspace, removed when dropped, the test passing
/// or not: found by the label the CLI gives them (an `up` that failed part
/// way may have made one).
struct Removed<'a> {
    api: &'a Api,
    workspace: PathBuf,
}

impl Drop for Removed<'_> {
    fn drop(&mut self) {
        for id in made_for(self.api, &self.workspace) {
            let _ = self.api.command(&["rm", "-f", &id]).output();
        }
    }
}

/// The containers the CLI made for `workspace`, by its label.
fn made_for(api: &Api, workspace: &Path) -> Vec<String> {
    let filter = format!("label=devcontainer.local_folder={}", workspace.display());
    let listed = api
        .command(&["ps", "-aq", "--no-trunc", "--filter", &filter])
        .output()
        .unwrap();
    assert!(listed.status.success(), "{listed:?}");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Whether the devcontainer CLI is here (required where the image has it).
fn devcontainer_cli() -> bool {
    let here = Command::new("devcontainer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !here {
        assert!(
            std::env::var("SHIPYARD_TEST_DEVCONTAINERS").as_deref() != Ok("required"),
            "the devcontainer CLI isn't on PATH, and it's required"
        );
        eprintln!("skipped: no devcontainer CLI on PATH");
    }
    here
}

/// The fixture image with an `/etc/passwd` and `/etc/group` (the CLI
/// execs as `root` by name, which a lookup with no passwd file refuses, as
/// Docker's does): the fixture run once to write them, its files exported
/// and imported as a local image, by Shipyard's own commands.
fn with_users(api: &Api) -> String {
    const IMAGE: &str = "shipyard-devcontainer:test";
    let ran = api
        .command(&[
            "run", "--name", "with-users", "--network", "none", &api.image, "sh", "-c",
            "echo root:x:0:0:root:/root:/bin/sh >/etc/passwd; echo root:x:0: >/etc/group; mkdir -p /root",
        ])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{ran:?}");
    let tar = api.scratch.0.join("with-users.tar");
    let exported = api
        .command(&["export", "-o", tar.to_str().unwrap(), "with-users"])
        .output()
        .unwrap();
    assert!(exported.status.success(), "{exported:?}");
    let imported = api
        .command(&["import", tar.to_str().unwrap(), IMAGE])
        .output()
        .unwrap();
    assert!(imported.status.success(), "{imported:?}");
    let removed = api.command(&["rm", "with-users"]).output().unwrap();
    assert!(removed.status.success(), "{removed:?}");
    IMAGE.to_owned()
}

/// D30: a devcontainer comes up against the API, its lifecycle command runs
/// in the workspace it binds, `exec` reaches it, and it's removed: root's
/// and a user's (rootless), as each runs it.
#[test]
fn a_devcontainer_comes_up_against_shipyard() {
    if !devcontainer_cli() {
        return;
    }
    let Some(api) = Api::new() else {
        return;
    };
    let image = with_users(&api);
    let _service = api.serve();
    let workspace = api.scratch.0.join("workspace");
    std::fs::create_dir_all(workspace.join(".devcontainer")).unwrap();
    // No build (the CLI builds an image to change the user's UID unless
    // told not to), no network (none to reach: root's runs needn't make a
    // bridge here).
    let config = serde_json::json!({
        "name": "shipyard-acceptance",
        "image": image,
        "updateRemoteUserUID": false,
        "runArgs": ["--network", "none"],
        "postCreateCommand": "echo made-inside > made-inside",
    });
    std::fs::write(
        workspace.join(".devcontainer/devcontainer.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let workspace_arg = workspace.to_str().unwrap();
    let _removed = Removed {
        api: &api,
        workspace: workspace.clone(),
    };

    let up = devcontainer(&api, &["up", "--workspace-folder", workspace_arg]);
    let said = String::from_utf8_lossy(&up.stdout);
    assert!(up.status.success(), "devcontainer up: {up:?}");
    // Its last line is the result, as JSON.
    let result: serde_json::Value =
        serde_json::from_str(said.lines().last().unwrap_or_default()).unwrap();
    assert_eq!(result["outcome"], "success", "{said}");
    let id = result["containerId"].as_str().unwrap().to_owned();
    assert_eq!(
        made_for(&api, &workspace),
        [id.as_str()],
        "found by its label"
    );

    // The lifecycle command ran in the workspace, bound from here.
    assert_eq!(
        std::fs::read_to_string(workspace.join("made-inside")).unwrap(),
        "made-inside\n",
        "postCreateCommand's file, seen on the host"
    );
    let exec = devcontainer(
        &api,
        &[
            "exec",
            "--workspace-folder",
            workspace_arg,
            "cat",
            "made-inside",
        ],
    );
    assert!(exec.status.success(), "devcontainer exec: {exec:?}");
    assert_eq!(String::from_utf8_lossy(&exec.stdout).trim(), "made-inside");

    // Removed through the API, as the CLI's users do with Docker's.
    let removed = api.command(&["rm", "-f", &id]).output().unwrap();
    assert!(removed.status.success(), "{removed:?}");
    assert!(made_for(&api, &workspace).is_empty(), "removed");
}
