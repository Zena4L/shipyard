//! The container store's tests, by subject, and the helpers they share.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::field_reassign_with_default,
    reason = "a panic in a test is a clear failure; requests are built as the CLI builds them"
)]

use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::Engine;
use crate::Paths;
use crate::error::Result;
use crate::report::{Collect, Reporter};
use crate::run::RunRequest;

mod creating;
#[cfg(target_os = "linux")]
mod linux;
mod naming;
mod networks;
mod requests;
mod states;
mod versions;

/// An engine on a container store of its own, in a fresh directory.
fn engine(name: &str) -> (Engine, Arc<Collect>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "shipyard-containers-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let collect = Arc::new(Collect::default());
    let engine = Engine::new(
        Paths::from_env().with_containers(dir.join("containers")),
        Arc::clone(&collect) as Arc<dyn Reporter>,
    );
    (engine, collect, dir)
}

fn name() -> Name {
    Name::parse("app").unwrap()
}

fn config() -> Config {
    let mut request = RunRequest::default();
    request.image = Some("alpine".to_owned());
    request.command = vec!["true".into()];
    Config::from_request(&request).unwrap()
}

/// Words for made-up names, from the kernel's random numbers.
fn random_words() -> Result<[u16; 2]> {
    let bytes = crate::random::bytes::<4>()?;
    Ok([
        u16::from_le_bytes([bytes[0], bytes[1]]),
        u16::from_le_bytes([bytes[2], bytes[3]]),
    ])
}

fn new(name: Option<&str>) -> NewContainer {
    NewContainer {
        name: name.map(|name| Name::parse(name).unwrap()),
        config: config(),
        labels: Labels::default(),
        image_id: None,
        auto_remove: false,
    }
}
