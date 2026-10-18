//! A run, start to end, through the engine (`run::run`), without
//! isolation (so on macOS too). One test alone in its own binary: a run
//! installs the supervisor, which takes the process's signals and reaps its
//! children, so nothing else may run beside it here.

#![allow(
    clippy::unwrap_used,
    clippy::field_reassign_with_default,
    reason = "a panic in a test is a clear failure; a request is made as callers make it"
)]

use std::ffi::OsString;
use std::sync::Arc;

use shipyard_engine::report::{Collect, Message, Reporter};
use shipyard_engine::run::{Outcome, RunRequest, run};
use shipyard_engine::{Engine, Paths};
use shipyard_runtime::process::passed::PassedFds;

/// `command` run without isolation, to its outcome.
fn unisolated(engine: &Engine, command: &[&str]) -> shipyard_engine::Result<Outcome> {
    // Field by field, as a caller outside the engine makes one.
    let mut request = RunRequest::default();
    request.command = command.iter().map(OsString::from).collect();
    request.no_isolation = true;
    run(engine, request, PassedFds::default())
}

/// A run ends as its command did; one that can't start is said to have
/// failed as it happened (before its cleanup), and ends with the runtime's
/// code for it.
#[test]
fn a_run_ends_as_its_command_does() {
    let collect = Arc::new(Collect::default());
    let engine = Engine::new(Paths::from_env(), Arc::clone(&collect) as Arc<dyn Reporter>);
    assert_eq!(unisolated(&engine, &["true"]).unwrap(), Outcome::Exited(0));
    assert_eq!(
        unisolated(&engine, &["sh", "-c", "exit 3"]).unwrap(),
        Outcome::Exited(3)
    );
    assert!(collect.events().is_empty(), "{:?}", collect.events());
    assert_eq!(
        unisolated(&engine, &["no-such-command-xyz"]).unwrap(),
        Outcome::Failed(127)
    );
    let events = collect.events();
    assert!(
        matches!(&events[..], [Message::Failure(message)] if message.contains("not found")),
        "{events:?}"
    );
}
