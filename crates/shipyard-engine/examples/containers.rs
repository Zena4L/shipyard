//! Containers' records by hand, for a manual check of the store alone
//! (no image, nothing run): `cargo run -p shipyard-engine --example
//! containers`. Makes three
//! records in the store `Paths::from_env` gives (`SHIPYARD_CONTAINER_DIR`
//! to put it elsewhere), finds each by every kind of reference, removes
//! one, and leaves the other two for a look at the files. Run it on a
//! store of its own: a second run finds the name `web` taken.
//!
//! It writes records through `containers::testing`, the store's own
//! writers, past the locks a run takes: it shows the store, not the API
//! (that's `run::create`, `run::remove`, `containers::list`).

use std::io::Write as _;
use std::sync::Arc;

use shipyard_engine::containers::{self, Config, Labels, Name, NewContainer};
use shipyard_engine::report::{Collect, Reporter};
use shipyard_engine::run::RunRequest;
use shipyard_engine::{Engine, Paths};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let collect = Arc::new(Collect::default());
    let engine = Engine::new(Paths::from_env(), Arc::clone(&collect) as Arc<dyn Reporter>);
    let mut out = std::io::stdout().lock();
    let mut request = RunRequest::default();
    request.image = Some("alpine:3.20".to_owned());
    request.command = vec!["sh".into(), "-c".into(), "echo hi".into()];
    let config = Config::from_request(&request)?;
    let mut made = Vec::new();
    for name in [Some("web"), None, None] {
        let record = containers::testing::create_record(
            &engine,
            NewContainer {
                name: name.map(Name::parse).transpose()?,
                config: config.clone(),
                labels: Labels::default(),
                image_id: None,
                auto_remove: false,
            },
        )?;
        writeln!(
            out,
            "made {} {} {}",
            record.id.short(),
            record.serial,
            record.name
        )?;
        made.push(record);
    }
    let web = &made[0];
    for reference in [
        web.id.as_str().to_owned(),
        web.name.to_string(),
        web.serial.to_string(),
        format!("shpu{:06}", web.serial.number()),
        web.id.short().to_owned(),
    ] {
        let found = containers::testing::find(&engine, &reference)?;
        writeln!(out, "find {reference:?} -> {}", found.name)?;
    }
    match containers::testing::find(&engine, "SHPU 000000 9") {
        Ok(found) => writeln!(out, "a wrong check digit found {}", found.name)?,
        Err(err) => writeln!(out, "a wrong check digit: {err}")?,
    }
    containers::testing::remove_record(&engine, &made[2].id)?;
    for record in containers::list(&engine)? {
        let state = containers::state(&engine, &record)?;
        writeln!(
            out,
            "listed {} {} {state:?}",
            record.id.short(),
            record.name
        )?;
    }
    for event in collect.events() {
        writeln!(out, "reported {event:?}")?;
    }
    Ok(())
}
