//! Readers of a command's output that go away (F70): how a command that
//! writes to both streams must end when one reader, the other, or the one
//! pipe both share (`2>&1 | head`) is gone.

use std::process::Command;

/// The three ways a reader of `logs` can go (F70), for `command` (a `logs`
/// of a container that wrote 20,000 lines to each stream, more than a
/// pipe holds): stderr's gone while stdout's is read fails with `failed`
/// (the lines lost); stdout's gone, or the one pipe of `2>&1 | head -1`,
/// is quiet (0).
///
/// # Panics
///
/// When the command doesn't end as said.
pub fn logs_end_as_their_readers_do(command: impl Fn() -> Command, failed: i32) {
    use std::io::{BufRead as _, Read as _};
    use std::process::Stdio;
    // Stderr's reader gone, stdout's still reading: a failure.
    let mut logs = command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(logs.stderr.take());
    let mut out = String::new();
    logs.stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(
        logs.wait().unwrap().code(),
        Some(failed),
        "stderr's reader gone"
    );
    // Stdout's reader gone (the control): quiet.
    let mut logs = command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(logs.stdout.take());
    let output = logs.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    // `2>&1 | head -1`: both streams one pipe, its reader gone after a
    // line, whichever stream's write meets it first: quiet.
    // `std`'s pipe is close-on-exec: the command mustn't inherit its read
    // end, or the pipe never has its reader gone (TLPI §27.4).
    let (read, write) = std::io::pipe().unwrap();
    let mut logs = command()
        .stdout(Stdio::from(write.try_clone().unwrap()))
        .stderr(Stdio::from(write))
        .spawn()
        .unwrap();
    let mut first = String::new();
    std::io::BufReader::new(read).read_line(&mut first).unwrap();
    assert!(!first.is_empty());
    assert_eq!(logs.wait().unwrap().code(), Some(0), "`2>&1 | head -1`");
}
