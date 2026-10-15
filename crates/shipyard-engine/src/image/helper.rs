//! The storage helper: this binary, re-executed in a user namespace of its
//! own, to extract or remove image layers for a rootless caller (Linux).
//!
//! An image's files belong to its users (root, `postgres`, `nobody`). A
//! caller without root can't `chown` a file to anyone but themselves, but
//! root *inside a user namespace* can, to any ID the namespace maps: with
//! the caller's subordinate ranges (`/etc/subuid`, `newuidmap`, stage 4),
//! that's the image's IDs, shifted into the caller's range on the host.
//! Files made that way can only be removed from inside such a namespace
//! again. Podman does the same (`podman unshare`).
//!
//! The exchange, on two pipes passed as descriptors (stdout stays the
//! caller's; stderr is shared for diagnostics):
//!
//! 1. the helper makes its user namespace and says `Ready` (or `Refused`,
//!    and why: a host that forbids user namespaces);
//! 2. the caller writes its maps (`newuidmap`/`newgidmap`) and sends one
//!    `Request`;
//! 3. the helper becomes root in the namespace, does the work, and answers.
//!
//! The store is passed as an open descriptor, not a path: nothing the
//! helper is told can name anything outside it, and layers are named by
//! digest.

#[cfg(target_os = "linux")]
pub use linux::entry_point;
#[cfg(target_os = "linux")]
pub(crate) use linux::{Outcome, Request, ranges, run};

/// Runs this process as the storage helper, if that's what it was started
/// as; otherwise returns at once. Called first thing in `main`, with what
/// starts the binary's logging (the helper's warnings go to stderr).
#[cfg(not(target_os = "linux"))]
pub fn entry_point(init_logging: impl FnOnce()) {
    let _ = init_logging;
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use nix::fcntl::OFlag;
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use serde::{Deserialize, Serialize};
    use shipyard_image::{
        IdRanges, LayerReport, LayerSpec, Limits, Ownership, Store, StoreLock, UnpackOptions,
    };
    use shipyard_runtime::isolation::{IdMap, IdMaps};

    use crate::error::{Context as _, Error, ErrorKind, Result, bail};

    /// The hidden first argument the helper is started with.
    const HELPER_ARG: &str = "__shipyard-storage";

    /// How long the helper may take to make its namespace.
    const READY_DEADLINE: Duration = Duration::from_secs(10);

    /// The helper's first word.
    #[derive(Debug, Serialize, Deserialize)]
    enum Ready {
        /// In its namespace, waiting for maps.
        Ready,
        /// The host refused it a user namespace, and why.
        Refused(String),
    }

    /// What the helper is asked to do.
    #[derive(Debug, Serialize, Deserialize)]
    pub(crate) enum Request {
        /// Extract these layers (base first) into the store.
        Extract(Vec<LayerSpec>),
        /// Remove these layer directories (by name, as garbage collection
        /// lists them).
        RemoveLayers(Vec<String>),
        /// Remove these containers' writable areas (by name): what a
        /// container wrote as its other users belongs to subordinate IDs.
        RemoveAreas(Vec<String>),
        /// Remove these volumes moved aside (by their names in the volume
        /// trash), for the same reason.
        RemoveVolumes(Vec<String>),
    }

    /// The helper's answer.
    #[derive(Debug, Serialize, Deserialize)]
    enum Answer {
        /// Done; what extracting did (nothing, for a removal).
        Done(LayerReport),
        /// The namespace was made but gives no privileges, and why.
        Refused(String),
        /// Failed, and why.
        Failed(String),
    }

    /// What running the helper came to.
    #[derive(Debug)]
    pub(crate) enum Outcome {
        /// The work is done.
        Done(LayerReport),
        /// No user namespace could be made here; the reason.
        Refused(String),
    }

    // --- The caller's side -----------------------------------------------

    /// Runs `request` in the helper, with the store open at `store` and the
    /// user namespace mapped with `maps`. The helper inherits `held`, the
    /// store's lock, so it lasts as long as the helper does: if this process
    /// dies first (a Ctrl-C), the helper's work is still covered, and no
    /// garbage collection removes what it's writing.
    ///
    /// # Errors
    ///
    /// If the helper can't be started or mapped, or the work fails.
    pub(crate) fn run(
        store: &OwnedFd,
        held: &StoreLock,
        maps: &IdMaps,
        request: &Request,
    ) -> Result<Outcome> {
        let (from_helper, helper_out) =
            nix::unistd::pipe2(OFlag::O_CLOEXEC).context("making the storage helper's pipe")?;
        let (helper_in, to_helper) =
            nix::unistd::pipe2(OFlag::O_CLOEXEC).context("making the storage helper's pipe")?;
        // The helper's ends must survive its exec: copies without
        // close-on-exec, closed here once it's started.
        let inheritable =
            |fd: &OwnedFd| nix::unistd::dup(fd).context("passing the storage helper a descriptor");
        let (store_fd, in_fd, out_fd) = (
            inheritable(store)?,
            inheritable(&helper_in)?,
            inheritable(&helper_out)?,
        );
        let lock_fd = held
            .inheritable()
            .context("passing the storage helper the store's lock")?;
        let mut command = Command::new("/proc/self/exe");
        // A session of its own: a Ctrl-C meant for the caller doesn't stop it
        // half-way through a layer (it finishes, or fails, and cleans up).
        // SAFETY: runs between `fork` and `exec`; only calls `setsid`, which
        // is async-signal-safe (nix's is the bare system call), and reads
        // `errno`.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()?;
                Ok(())
            });
        }
        let mut helper = command
            .arg0("shipyard-storage")
            .arg(HELPER_ARG)
            .args(
                [
                    store_fd.as_raw_fd(),
                    in_fd.as_raw_fd(),
                    out_fd.as_raw_fd(),
                    lock_fd.as_raw_fd(),
                ]
                .map(|fd| fd.to_string()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .context("starting the storage helper")?;
        drop((store_fd, in_fd, out_fd, lock_fd, helper_in, helper_out));

        let result = exchange(&mut helper, &from_helper, to_helper, maps, request);
        if result.is_err() {
            let _ = helper.kill();
        }
        let status = helper.wait().context("waiting for the storage helper")?;
        let outcome = result?;
        if matches!(outcome, Outcome::Done(_)) && !status.success() {
            bail!(Storage, "the storage helper failed ({status})");
        }
        Ok(outcome)
    }

    /// The exchange with a started helper.
    fn exchange(
        helper: &mut Child,
        from_helper: &OwnedFd,
        to_helper: OwnedFd,
        maps: &IdMaps,
        request: &Request,
    ) -> Result<Outcome> {
        let mut lines = Lines::default();
        let ready: Ready = lines.read(
            from_helper,
            helper,
            Some(Instant::now() + READY_DEADLINE),
            look,
        )?;
        if let Ready::Refused(reason) = ready {
            return Ok(Outcome::Refused(reason));
        }
        shipyard_runtime::process::map_user_namespace(helper.id(), maps)
            .context("mapping the storage helper's user namespace")?;
        let mut to_helper = std::fs::File::from(to_helper);
        let mut line = serde_json::to_vec(request).context("encoding the storage request")?;
        line.push(b'\n');
        to_helper
            .write_all(&line)
            .context("sending the storage helper its request")?;
        drop(to_helper);
        match lines.read(from_helper, helper, None, look)? {
            Answer::Done(report) => Ok(Outcome::Done(report)),
            Answer::Refused(reason) => Ok(Outcome::Refused(reason)),
            Answer::Failed(message) => Err(Error::new(ErrorKind::Storage, message)),
        }
    }

    /// One look at the helper's pipe: up to 100 ms, then the helper is
    /// checked on.
    fn look(fds: &mut [PollFd<'_>]) -> nix::Result<libc::c_int> {
        poll(fds, PollTimeout::from(100_u16))
    }

    /// JSON lines read from the helper, watching it doesn't die silently.
    #[derive(Default)]
    struct Lines {
        pending: Vec<u8>,
    }

    impl Lines {
        fn read<T: for<'de> Deserialize<'de>>(
            &mut self,
            from: &OwnedFd,
            helper: &mut Child,
            deadline: Option<Instant>,
            mut look: impl FnMut(&mut [PollFd<'_>]) -> nix::Result<libc::c_int>,
        ) -> Result<T> {
            // Once it's gone: what it wrote before it went is still in the
            // pipe, so it's read to the end (its end of file, now nothing
            // else holds the pipe) before it counts as not answering.
            let mut gone = None;
            loop {
                if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.pending.drain(..=end).collect();
                    return serde_json::from_slice(&line[..end])
                        .context("the storage helper answered nonsense");
                }
                if deadline.is_some_and(|deadline| Instant::now() > deadline) {
                    bail!(Storage, "the storage helper didn't start in time");
                }
                let mut fds = [PollFd::new(
                    std::os::fd::AsFd::as_fd(from),
                    PollFlags::POLLIN,
                )];
                match look(&mut fds) {
                    Ok(0) | Err(nix::errno::Errno::EINTR) => {
                        if let Some(status) = gone {
                            // Gone, and its pipe still open elsewhere.
                            bail!(
                                Storage,
                                "the storage helper ended without answering ({status})"
                            );
                        }
                        gone = helper
                            .try_wait()
                            .context("waiting for the storage helper")?;
                        continue;
                    }
                    Ok(_) => {}
                    Err(errno) => return Err(errno).context("waiting for the storage helper"),
                }
                let mut buf = [0_u8; 4096];
                let read =
                    nix::unistd::read(from, &mut buf).context("reading the storage helper")?;
                if read == 0 {
                    if let Some(status) = gone {
                        bail!(
                            Storage,
                            "the storage helper ended without answering ({status})"
                        );
                    }
                    bail!(Storage, "the storage helper ended without answering");
                }
                self.pending.extend_from_slice(&buf[..read]);
            }
        }
    }

    // --- The helper's side -----------------------------------------------

    /// Runs this process as the storage helper, if that's what it was
    /// started as; otherwise returns at once. **Call it first thing in
    /// `main`**, before anything starts a thread: making a user namespace
    /// needs a single-threaded process. `init_logging` starts the binary's
    /// logging, once it's known to be the helper (its warnings go to
    /// stderr).
    pub fn entry_point(init_logging: impl FnOnce()) {
        let args: Vec<OsString> = std::env::args_os().collect();
        if args.get(1).map(OsString::as_os_str) != Some(HELPER_ARG.as_ref()) {
            return;
        }
        // As for the CLI itself: a privileged start (setuid, file
        // capabilities) gets no further, not even to its arguments.
        if let Err(err) = shipyard_runtime::process::refuse_privileged_start() {
            let _ = writeln!(std::io::stderr(), "shipyard-storage: {err}");
            std::process::exit(125);
        }
        let fd = |at: usize| -> Option<RawFd> {
            args.get(at)?
                .to_str()?
                .parse()
                .ok()
                .filter(|&fd: &RawFd| fd > 2)
        };
        let (Some(store), Some(input), Some(output), Some(lock)) = (fd(2), fd(3), fd(4), fd(5))
        else {
            let _ = writeln!(std::io::stderr(), "shipyard-storage: bad arguments");
            std::process::exit(125);
        };
        // SAFETY: the caller passed these four descriptors, open and ours
        // alone (dups made for this process, closed on its side).
        let (store, input, output, held) = unsafe {
            (
                OwnedFd::from_raw_fd(store),
                OwnedFd::from_raw_fd(input),
                OwnedFd::from_raw_fd(output),
                OwnedFd::from_raw_fd(lock),
            )
        };
        // The caller's store lock: held (open) until this process ends, with
        // nothing more to do with it; close-on-exec, now it's here.
        let _ = nix::fcntl::fcntl(
            &held,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
        init_logging();
        // 0 on success, 1 otherwise (the caller reads the answer, not this).
        let succeeded = helper(&store, input, output);
        drop(held);
        std::process::exit(i32::from(!succeeded));
    }

    /// The helper's work, start to end; whether it succeeded.
    fn helper(store: &OwnedFd, input: OwnedFd, output: OwnedFd) -> bool {
        let mut output = std::fs::File::from(output);
        if let Err(err) = shipyard_runtime::process::unshare_user_namespace() {
            send(&mut output, &Ready::Refused(err.to_string()));
            return false;
        }
        if !send(&mut output, &Ready::Ready) {
            return false;
        }
        let answer = match read_request(input) {
            Err(err) => Answer::Failed(format!("{err:#}")),
            Ok(request) => match shipyard_runtime::process::become_namespace_root() {
                // A namespace that gives no privileges: a host that confines
                // unprivileged ones (Ubuntu's AppArmor restriction).
                Err(shipyard_runtime::Error::Sys {
                    source: nix::errno::Errno::EPERM,
                    ..
                }) => Answer::Refused(
                    "the user namespace has no privileges here (a policy, such as \
                     AppArmor's on unprivileged user namespaces, confines it)"
                        .to_owned(),
                ),
                Err(err) => Answer::Failed(format!(
                    "becoming root in the storage helper's user namespace: {err}"
                )),
                Ok(()) => match work(store, request) {
                    Ok(report) => Answer::Done(report),
                    Err(err) => Answer::Failed(format!("{err:#}")),
                },
            },
        };
        let done = matches!(answer, Answer::Done(_));
        send(&mut output, &answer) && done
    }

    /// Writes one JSON line to the caller; whether it went.
    fn send(output: &mut std::fs::File, message: &impl Serialize) -> bool {
        let Ok(mut line) = serde_json::to_vec(message) else {
            return false;
        };
        line.push(b'\n');
        output.write_all(&line).is_ok()
    }

    /// Reads the one request line (after the maps are written); at most a
    /// few megabytes (it lists layers).
    fn read_request(input: OwnedFd) -> Result<Request> {
        use std::io::Read as _;
        let mut line = Vec::new();
        BufReader::new(std::fs::File::from(input).take(4 * 1024 * 1024))
            .read_until(b'\n', &mut line)
            .context("reading the storage request")?;
        serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(&line))
            .context("decoding the storage request")
    }

    /// Does what's asked in the store, as root in the namespace.
    fn work(store: &OwnedFd, request: Request) -> Result<LayerReport> {
        let maps = shipyard_runtime::process::current_id_maps()?;
        // Reached through the descriptor: the path names nothing but it.
        let store = Store::open(format!("/proc/self/fd/{}", store.as_raw_fd()))
            .context("opening the image store")?;
        match request {
            Request::Extract(layers) => {
                let options = UnpackOptions {
                    ownership: Ownership::Apply {
                        uids: ranges(&maps.uids),
                        gids: ranges(&maps.gids),
                        shift: None,
                    },
                    limits: Limits::default(),
                    // `trusted.*` attributes need real root.
                    trusted_xattrs: false,
                };
                Ok(shipyard_image::extract_layers(&store, &layers, &options)?.report)
            }
            Request::RemoveLayers(names) => {
                shipyard_image::remove_layers(&store, &names)?;
                Ok(LayerReport::default())
            }
            Request::RemoveAreas(names) => {
                shipyard_image::remove_areas(&store, &names)?;
                Ok(LayerReport::default())
            }
            Request::RemoveVolumes(names) => {
                crate::volumes::remove_trash(store.root(), &names)?;
                Ok(LayerReport::default())
            }
        }
    }

    /// The IDs a map lets this namespace give files.
    pub(crate) fn ranges(maps: &[IdMap]) -> IdRanges {
        IdRanges(maps.iter().map(|map| (map.container, map.size)).collect())
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "tests")]
    mod tests {
        use super::*;

        /// A helper that answers, then ends, between a look at its pipe that
        /// saw nothing and the check that it's still there: its answer is
        /// read all the same (it's in the pipe), not taken for none.
        #[test]
        fn an_answer_a_gone_helper_left_in_its_pipe_is_read() {
            let (from, to) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
            let mut helper = Command::new("sh")
                .args(["-c", r#"printf '"Ready"\n'"#])
                .stdout(Stdio::from(to))
                .spawn()
                .unwrap();
            // Answered and gone before the first look.
            helper.wait().unwrap();
            let mut first = true;
            let look = |fds: &mut [PollFd<'_>]| {
                if std::mem::take(&mut first) {
                    return Ok(0);
                }
                poll(fds, PollTimeout::ZERO)
            };
            let ready: Ready = Lines::default()
                .read(&from, &mut helper, None, look)
                .unwrap();
            assert!(matches!(ready, Ready::Ready));
        }

        /// A helper that ends without a word is told as one, not waited for.
        #[test]
        fn a_helper_that_ends_without_answering_is_an_error() {
            let (from, to) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
            let mut helper = Command::new("true")
                .stdout(Stdio::from(to))
                .spawn()
                .unwrap();
            helper.wait().unwrap();
            let error = Lines::default()
                .read::<Ready>(&from, &mut helper, None, |fds: &mut [PollFd<'_>]| {
                    poll(fds, PollTimeout::ZERO)
                })
                .unwrap_err();
            assert!(
                error.to_string().contains("ended without answering"),
                "{error}"
            );
        }
    }
}
