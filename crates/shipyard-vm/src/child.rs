//! A tool the host or the CLI runs (`launchctl`, `codesign`, `tmutil`),
//! waited for within a bound: one that stalls is killed, never waited on for
//! good (stage 13's close: `vm status` and `doctor` ask `launchctl`).

use std::io::Read as _;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// `command`'s output, its stdin nothing, if it ends within `limit`: `None`
/// if it's killed for not ending. Its stdout and stderr are read as it runs,
/// so a full pipe never holds it.
///
/// # Errors
///
/// It can't be run.
pub(crate) fn output_within(
    command: &mut Command,
    limit: Duration,
) -> std::io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Some(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// One that ends is read whole; one past its bound is killed, `None`.
    #[test]
    fn a_tool_past_its_bound_is_killed() {
        let said = output_within(
            Command::new("/bin/echo").arg("said"),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
        assert_eq!(said.stdout, b"said\n");
        let began = Instant::now();
        let stalled = output_within(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(stalled.is_none());
        assert!(began.elapsed() < Duration::from_secs(10));
    }
}
