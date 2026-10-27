//! An internal request asked of the guest (its ports, its checks): the answer
//! read whole, bounded, within a time limit.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use shipyard_runtime::attach::forward::{self, Connection, Request};

use crate::forwarding::Unanswered;
use crate::{VmError, VmPaths};

use super::{NOT_READY_REST, SAY_NOT_READY_AFTER, Status, status};

/// How long an internal request's answer may take ([`ask`]).
const ASK_LIMIT: Duration = Duration::from_secs(30);

/// The output of an internal request (`args`: `__guest-doctor`, say) to the
/// guest, read up to `cap`, if the VM runs: never started for it (`None`
/// when it isn't running, or is stopping), so asking changes nothing. A
/// guest not ready yet (its data disk checked: D10.4-4) is asked again for
/// up to `ASK_LIMIT`.
///
/// # Errors
///
/// [`VmError::Gone`]: the guest didn't answer, or answered as an older
/// guest does (the request unknown to it), or still wasn't ready.
pub fn ask(paths: &VmPaths, args: &[&str], cap: usize) -> Result<Option<Vec<u8>>, VmError> {
    let began = Instant::now();
    loop {
        match ask_once(paths, args, cap) {
            Err(VmError::NotReady) if began.elapsed() < ASK_LIMIT => {}
            asked => return asked,
        }
        std::thread::sleep(NOT_READY_REST);
    }
}

/// Waits until the running VM's guest is ready for commands (its data
/// disk checked or repaired: D10.4-4, so no bound cuts a repair short),
/// said on stderr once it's taken a few seconds: what `vm start` promises.
/// A VM that isn't running (stopped meanwhile) has nothing to wait for.
///
/// # Errors
///
/// [`VmError::Gone`]: the guest didn't answer.
pub fn until_guest_ready(paths: &VmPaths) -> Result<(), VmError> {
    let began = Instant::now();
    let mut said = false;
    loop {
        match ask_once(paths, &[forward::PORTS_REQUEST], forward::MAX_PORTS_ANSWER) {
            Err(VmError::NotReady) => {}
            asked => return asked.map(drop),
        }
        if !said && began.elapsed() >= SAY_NOT_READY_AFTER {
            said = true;
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("shipyard: {}: waiting\n", forward::NOT_READY),
            );
        }
        std::thread::sleep(NOT_READY_REST);
    }
}

/// What `stream` (connected to the host's `vm.sock` or `host.sock`)
/// answers internal request `args`: its stdout, up to `cap`, each read
/// within `wait`. (A host that's stopping may refuse, and close, before
/// the request is written: its answer is read either way.)
pub(super) fn answer_on(
    mut stream: UnixStream,
    args: &[&str],
    cap: usize,
    wait: Duration,
) -> Result<Vec<u8>, Unanswered> {
    // (macOS refuses a timeout, EINVAL, on a socket whose peer has closed
    // already: its reads end at once then, and need none.)
    let _ = stream.set_read_timeout(Some(wait));
    let hello =
        forward::hello(&Request::internal(args)).map_err(|e| Unanswered::Broken(e.to_string()))?;
    let _ = forward::send(&mut stream, &hello);
    crate::forwarding::output(&mut Connection::new(stream), cap)
}

/// [`ask`]'s one try.
fn ask_once(paths: &VmPaths, args: &[&str], cap: usize) -> Result<Option<Vec<u8>>, VmError> {
    if status(paths)? == Status::Stopped {
        return Ok(None);
    }
    let Ok(stream) = UnixStream::connect(paths.socket()) else {
        return Ok(None);
    };
    match answer_on(stream, args, cap, ASK_LIMIT) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(Unanswered::Refused(forward::Refusal::Stopping)) => Ok(None),
        Err(Unanswered::Refused(forward::Refusal::NotReady)) => Err(VmError::NotReady),
        Err(Unanswered::Refused(forward::Refusal::Other(why)) | Unanswered::Broken(why)) => {
            Err(VmError::Gone(why))
        }
        // Another build's guest (F167): what to do said.
        Err(Unanswered::Refused(newer @ forward::Refusal::Newer)) => {
            Err(VmError::Gone(newer.to_string()))
        }
        Err(failed @ Unanswered::Failed(_)) => Err(VmError::Gone(failed.to_string())),
    }
}
