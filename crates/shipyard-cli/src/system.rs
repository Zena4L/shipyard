//! `shipyard system recover`, `stop`, `df` and `service`.

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;

use crate::reporter::{engine, report};

/// `system recover`'s and `system stop`'s code when some containers (not
/// all) couldn't be started or stopped: the boot units' `SuccessExitStatus`
/// (contrib/systemd), so a unit that started the rest counts as started
/// (or systemd would end what it started). Shipyard's own commands, with no
/// Docker twin: Docker's 1 is theirs for nothing done at all.
pub(crate) const SOME_NOT_SETTLED: u8 = 125;

/// `shipyard system recover`: containers that ended unwatched settled,
/// those their restart policy brings back started, each one's name printed;
/// each that couldn't be started said, and the exit code
/// [`SOME_NOT_SETTLED`] (the boot unit's success too: what it did start
/// stays). Nothing done at all (the store unreadable) is a failure: 1.
pub(crate) fn recover() -> Result<ExitCode> {
    let booted = shipyard_engine::run::boot(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &booted.started {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in booted.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, SOME_NOT_SETTLED);
    }
    Ok(code)
}

/// `shipyard system stop`: every container that's up stopped for the
/// host's shutdown, each one's name printed; each that couldn't be said,
/// and the exit code [`SOME_NOT_SETTLED`]. Nothing done at all is a
/// failure: 1.
pub(crate) fn stop() -> Result<ExitCode> {
    let stopped = shipyard_engine::run::stop_for_the_host(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &stopped.stopped {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in stopped.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, SOME_NOT_SETTLED);
    }
    Ok(code)
}

/// `shipyard system service`: Docker's API served on systemd's socket, or
/// on `socket` (the engine's default if none), until it's idle.
pub(crate) fn service(socket: Option<&std::path::Path>, revision: &str) -> Result<ExitCode> {
    use anyhow::Context as _;
    // SAFETY: nothing has started a thread yet (the engine starts none
    // before it's asked), nor taken descriptor 3.
    let passed = unsafe { shipyard_sys::listen::from_systemd() }
        .context("taking the socket systemd passed")?;
    // Its own descriptor limit raised (many creates and connections at
    // once, F114), the one it was started with kept for its containers.
    #[cfg(target_os = "linux")]
    shipyard_runtime::container::raise_own_descriptor_limit();
    let paths = shipyard_engine::Paths::from_env();
    let (listener, bound) = if let Some(socket) = passed {
        let listener = std::os::unix::net::UnixListener::from(socket);
        listener
            .local_addr()
            .context("the socket systemd passed isn't a UNIX socket")?;
        (listener, None)
    } else {
        let path = match socket {
            Some(path) => path.to_owned(),
            None => paths.api_socket()?,
        };
        // Still one thread: the socket's made with the mode mask narrowed.
        let (listener, bound) = shipyard_api::bind(&path)?;
        (listener, Some(bound))
    };
    let setting = std::env::var("SHIPYARD_API_IDLE").ok();
    let idle = shipyard_api::idle_time(setting.as_deref()).unwrap_or_else(|default| {
        let _ = writeln!(
            std::io::stderr().lock(),
            "shipyard: SHIPYARD_API_IDLE={}: not whole seconds (at least 1): {} s it is",
            crate::output::printable_message(setting.as_deref().unwrap_or_default()),
            default.as_secs()
        );
        default
    });
    let service = shipyard_api::Service {
        paths,
        idle,
        build: shipyard_api::Build {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            revision: revision.to_owned(),
        },
    };
    shipyard_api::serve(listener, service).context("serving the API")?;
    drop(bound);
    Ok(ExitCode::SUCCESS)
}

/// `shipyard system df`: what the images, the containers and the volumes
/// take, and how much of it nothing uses, as Docker's (`--format` its
/// rows). There's no build cache yet (stage 12 builds images).
pub(crate) fn df(format: Option<&str>) -> Result<ExitCode> {
    use crate::format::rows::{self, DfRow};
    let engine = engine();
    let store = shipyard_engine::image::disk_usage(&engine)?;
    let volumes = shipyard_engine::volumes::usage(&engine)?;
    let rows = [
        DfRow::new("Images", &store.images),
        DfRow::new("Containers", &store.containers),
        DfRow::new("Local Volumes", &volumes),
    ];
    crate::format::Output::parse(format, rows::DF_TABLE)?
        .write(&mut std::io::stdout().lock(), &rows)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The boot units count a partial recovery as their success: their
    /// `SuccessExitStatus` is [`super::SOME_NOT_SETTLED`], and 1 (nothing
    /// done) isn't.
    #[test]
    fn the_boot_units_take_a_partial_recovery_as_success() {
        let contrib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd");
        for unit in ["shipyard-recover.service", "user/shipyard-recover.service"] {
            let text = std::fs::read_to_string(contrib.join(unit)).unwrap();
            let statuses: Vec<&str> = text
                .lines()
                .filter_map(|line| line.strip_prefix("SuccessExitStatus="))
                .collect();
            assert_eq!(statuses, [super::SOME_NOT_SETTLED.to_string()], "{unit}");
        }
        assert_ne!(super::SOME_NOT_SETTLED, crate::reporter::EXIT_FAILURE);
    }
}
