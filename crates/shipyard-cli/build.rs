//! The build's revision, compiled in as `SHIPYARD_REVISION` (step 10.3b):
//! on a Mac, the CLI compares it with the revision its VM's guest was built
//! from (`scripts/guest/build.sh` writes the same, the same way). It's the
//! repository's commit, with `-dirty` while it has changes; `unknown` out
//! of a repository.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let revision = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(commit) if git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) => {
            format!("{commit}-dirty")
        }
        Some(commit) => commit,
        None => "unknown".to_owned(),
    };
    println!("cargo:rustc-env=SHIPYARD_REVISION={revision}");
    // Again when the commit, the index or any source changes (cargo looks
    // at every file under a directory given): `-dirty` follows the tree.
    for watched in [
        "../../.git/HEAD",
        "../../.git/index",
        "../../crates",
        "../../Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={watched}");
    }
}
