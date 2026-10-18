//! `doctor`'s probe helper (this binary again, as `__shipyard-probe`): it
//! makes a user namespace, a mount namespace, a tmpfs and an overlay as a
//! run would, in a scratch directory it's given, and says how each went on
//! stdout, as one line. An entry point (it prints, reads its arguments).

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;

use nix::errno::Errno;
use shipyard_runtime::process::probe::{Probe, Step};

/// The hidden first argument the probe helper is started with.
pub(super) const PROBE_ARG: &str = "__shipyard-probe";

/// One step as the helper writes it: `-` (not tried), `0` (worked), or
/// the errno.
fn encode_step(step: Step) -> String {
    match step {
        None => "-".to_owned(),
        Some(Ok(())) => "0".to_owned(),
        Some(Err(errno)) => (errno as i32).to_string(),
    }
}

fn decode_step(text: &str) -> Option<Step> {
    Some(match text {
        "-" => None,
        "0" => Some(Ok(())),
        errno => Some(Err(Errno::from_raw(errno.parse().ok()?))),
    })
}

/// The helper's line: `probe USERNS MOUNT OVERLAY`.
pub(super) fn encode(probe: &Probe) -> String {
    format!(
        "probe {} {} {}",
        encode_step(probe.user_namespace),
        encode_step(probe.mount),
        encode_step(probe.overlay)
    )
}

pub(super) fn decode(line: &str) -> Option<Probe> {
    let mut words = line.split(' ');
    if words.next()? != "probe" {
        return None;
    }
    let probe = Probe {
        user_namespace: decode_step(words.next()?)?,
        mount: decode_step(words.next()?)?,
        overlay: decode_step(words.next()?)?,
    };
    words.next().is_none().then_some(probe)
}

/// Runs this process as the probe helper, if that's what it was started
/// as; otherwise returns at once. **Call it first thing in `main`**,
/// before anything starts a thread: making a user namespace needs a
/// single-threaded process.
pub fn probe_entry_point() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).map(OsString::as_os_str) != Some(PROBE_ARG.as_ref()) {
        return;
    }
    // As for the CLI itself: a privileged start (setuid, file
    // capabilities) gets no further, not even to its arguments.
    if let Err(err) = shipyard_runtime::process::refuse_privileged_start() {
        let _ = writeln!(std::io::stderr(), "shipyard-probe: {err}");
        std::process::exit(125);
    }
    let (Some(scratch), Some(rootless)) = (
        args.get(2),
        args.get(3).and_then(|mode| match mode.to_str()? {
            "rootless" => Some(true),
            "root" => Some(false),
            _ => None,
        }),
    ) else {
        let _ = writeln!(std::io::stderr(), "shipyard-probe: bad arguments");
        std::process::exit(125);
    };
    let probe = shipyard_runtime::process::probe::probe(Path::new(scratch), rootless);
    let code = i32::from(writeln!(std::io::stdout(), "{}", encode(&probe)).is_err());
    std::process::exit(code);
}
