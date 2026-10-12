//! Starting a helper in a container (see `container/helper.rs`).

use std::os::fd::OwnedFd;
use std::path::Path;

use super::{ContainerError, Result, Status, existing, lock, read_record, status_of, wrong};
use crate::container::{Helper, RunningHelper};
use crate::isolation::{Join, Namespace};

/// Runs a helper, if this process was started as one: **call it first thing
/// in `main`** (after [`init_entry_point`](crate::process::init_entry_point)),
/// with every helper the binary has. Otherwise it returns at once.
pub fn helper_entry_point(helpers: &[Helper]) {
    crate::process::helper::entry_point(helpers);
}

/// Starts helper `name` (one the binary passes to [`helper_entry_point`]) in
/// container `id`'s user and mount namespaces, given `args`, with `stdin`
/// and `stdout` as its standard input and output. `action` says what it's
/// for, in a refusal ("copying files": "… needs it created, running or
/// paused"). The container must be
/// created, running or paused (a helper isn't in its cgroup: a frozen
/// container doesn't stop it).
///
/// Call it from the process's main thread: the helper dies with the thread
/// that started it.
///
/// # Errors
///
/// No such container; one in another state
/// ([`ContainerError::WrongStatus`]); one without a mount namespace of its
/// own ([`ContainerError::HelperNeedsMount`]); one that isn't the caller's,
/// or a helper that couldn't start ([`ContainerError::Helper`]).
pub fn start_helper(
    root: &Path,
    id: &str,
    name: &str,
    action: &'static str,
    args: &[u8],
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> Result<RunningHelper> {
    const WANTED: &str = "created, running or paused";
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !matches!(status, Status::Created | Status::Running | Status::Paused) {
        return Err(wrong(id, status, action, WANTED));
    }
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    let Some((pid, started)) = record.init else {
        return Err(wrong(id, Status::Creating, action, WANTED));
    };
    let target = u32::try_from(pid).map_err(|_| ContainerError::NotFound(id.to_owned()))?;
    let stopped = |err| match status_of(&dir) {
        Ok(Status::Stopped) => wrong(id, Status::Stopped, action, WANTED),
        _ => err,
    };
    let namespaces = super::unshared(target, &[Namespace::User, Namespace::Mount])
        .map_err(|err| ContainerError::Helper(Box::new(err)))?;
    if !namespaces.contains(&Namespace::Mount) {
        // Its init gone meanwhile reads as no namespaces at all.
        return Err(stopped(ContainerError::HelperNeedsMount(id.to_owned())));
    }
    let mut join = Join::standard(target);
    join.namespaces = namespaces;
    join.start_time = Some(started);
    let joining = crate::process::join::Joining::open(&join)
        .map_err(|err| stopped(ContainerError::Helper(Box::new(err))))?;
    let helper = crate::process::helper::launch(&joining, name, args, stdin, stdout)
        .map_err(|err| ContainerError::Helper(Box::new(err)))?;
    // It holds the namespaces now: the container may stop meanwhile.
    drop(lock);
    Ok(helper)
}
