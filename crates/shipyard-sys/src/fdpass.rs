//! Descriptors passed between processes over a Unix socket (`SCM_RIGHTS`,
//! TLPI §61.13.3): the kernel installs a new descriptor in the receiver,
//! onto the same open file description as the sender's (its offset, its
//! locks).

#[cfg(not(target_os = "linux"))]
use std::os::fd::FromRawFd as _;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
#[cfg(not(target_os = "linux"))]
use nix::sys::socket::{ControlMessageOwned, recvmsg};

/// Sends `bytes` with the descriptors `fds` on `socket`, in one message:
/// how many of the bytes went (the descriptors go with the first). Tried
/// again if a signal interrupts it. With no descriptors, a plain message
/// (no control message at all).
///
/// # Errors
///
/// The `sendmsg(2)` errno.
pub fn send(
    socket: BorrowedFd<'_>,
    fds: &[BorrowedFd<'_>],
    bytes: &[u8],
    flags: MsgFlags,
) -> nix::Result<usize> {
    let fds: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let rights = [ControlMessage::ScmRights(&fds)];
    let control: &[ControlMessage<'_>] = if fds.is_empty() { &[] } else { &rights };
    let iov = [std::io::IoSlice::new(bytes)];
    loop {
        match sendmsg::<()>(socket.as_raw_fd(), &iov, control, flags, None) {
            Ok(sent) => return Ok(sent),
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(errno),
        }
    }
}

/// Room for the descriptors one [`receive`] can take: made once, used for
/// every message.
#[derive(Debug)]
pub struct Room {
    #[cfg(target_os = "linux")]
    space: Vec<std::mem::MaybeUninit<u8>>,
    #[cfg(not(target_os = "linux"))]
    space: Vec<u8>,
}

impl Room {
    /// Room for at least `fds` descriptors in one message (rustix's
    /// sizing adds room for its alignment).
    #[must_use]
    pub fn new(fds: usize) -> Self {
        #[cfg(target_os = "linux")]
        let space = vec![std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(fds))];
        // (nix reads the room as the `Vec`'s length, as its `cmsg_space!`
        // makes it.)
        #[cfg(not(target_os = "linux"))]
        let space = {
            let length = u32::try_from(fds * size_of::<RawFd>()).unwrap_or(u32::MAX);
            // SAFETY: `CMSG_SPACE` only computes a size.
            vec![0; unsafe { libc::CMSG_SPACE(length) } as usize]
        };
        Self { space }
    }
}

/// One message [`receive`]d.
#[derive(Debug)]
pub struct Received {
    /// How many bytes were read into the buffer; 0 at end-of-file.
    pub bytes: usize,
    /// The descriptors passed with it, owned now.
    pub fds: Vec<OwnedFd>,
    /// Not all that was passed came (`MSG_CTRUNC`: more than its [`Room`],
    /// or the receiver at its descriptor limit). What the kernel did
    /// install is in `fds` on Linux (F54), and goes when they do; the rest
    /// it closed. (On macOS, nix gives none of a truncated message's.)
    pub truncated: bool,
}

/// Receives one message from `socket` into `buffer`, with `room` for the
/// descriptors passed with it. Close-on-exec as they're installed if
/// `flags` asks (`MSG_CMSG_CLOEXEC`, Linux).
///
/// On Linux, read with rustix's `recvmsg`, whose walk of the control
/// messages yields every descriptor the kernel installed, a truncated
/// message's too: nix's gives none once `MSG_CTRUNC` is said, and those
/// would stay open, owned by nothing (F54).
///
/// # Errors
///
/// The `recvmsg(2)` errno (`EAGAIN` with `MSG_DONTWAIT` and nothing there,
/// `EINTR`: the caller says what they mean).
#[cfg(target_os = "linux")]
pub fn receive(
    socket: BorrowedFd<'_>,
    buffer: &mut [u8],
    room: &mut Room,
    flags: MsgFlags,
) -> nix::Result<Received> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags};
    let flags = RecvFlags::from_bits_retain(flags.bits().cast_unsigned());
    let mut control = RecvAncillaryBuffer::new(&mut room.space);
    let mut iov = [std::io::IoSliceMut::new(buffer)];
    let message = rustix::net::recvmsg(socket, &mut iov, &mut control, flags)
        .map_err(|errno| Errno::from_raw(errno.raw_os_error()))?;
    let mut fds = Vec::new();
    for cmsg in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = cmsg {
            fds.extend(received);
        }
    }
    Ok(Received {
        bytes: message.bytes,
        fds,
        truncated: message.flags.contains(ReturnFlags::CTRUNC),
    })
}

/// Receives one message from `socket` into `buffer`, with `room` for the
/// descriptors passed with it.
///
/// # Errors
///
/// The `recvmsg(2)` errno (`EAGAIN` with `MSG_DONTWAIT` and nothing there,
/// `EINTR`: the caller says what they mean).
#[cfg(not(target_os = "linux"))]
pub fn receive(
    socket: BorrowedFd<'_>,
    buffer: &mut [u8],
    room: &mut Room,
    flags: MsgFlags,
) -> nix::Result<Received> {
    let mut iov = [std::io::IoSliceMut::new(buffer)];
    let message = recvmsg::<()>(socket.as_raw_fd(), &mut iov, Some(&mut room.space), flags)?;
    let truncated = message.flags.contains(MsgFlags::MSG_CTRUNC);
    let mut fds = Vec::new();
    // (nix gives none to read once a message was truncated.)
    for cmsg in message.cmsgs().into_iter().flatten() {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            // SAFETY: the kernel just installed these in this process, and
            // nothing else here knows them: each is ours alone.
            fds.extend(
                received
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    Ok(Received {
        bytes: message.bytes,
        fds,
        truncated,
    })
}

#[cfg(test)]
mod tests;
