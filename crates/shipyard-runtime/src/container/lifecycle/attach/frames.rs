//! Letting connections in, and reading each client's frames: its hello,
//! then what an attached client types, its sizes and the end of its input
//! (a control client's frames go to [`roles`](super::roles)).

use std::collections::VecDeque;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;

use nix::errno::Errno;
use shipyard_sys::fdpass::{Received, Room};

use crate::attach::{Decoder, DetachKeys, Frame, negotiate};

use super::connection::{Client, Leaving, Role};
use super::{ACCEPT_REST, Attachments, MAX_CLIENTS, READS_PER_PASS, STDIN_QUEUE, Stdin};

impl Attachments {
    pub(super) fn accept(&mut self) {
        if self.resting.is_some_and(|until| Instant::now() < until) {
            return;
        }
        self.resting = None;
        let Some(listener) = &self.listener else {
            return;
        };
        loop {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
                // Gone before it was taken: the next.
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                // EMFILE, ENFILE, ENOMEM: the connection stays queued, and
                // the listener ready; resting, rather than trying again at
                // once, forever.
                Err(_) => {
                    self.resting = Some(Instant::now() + ACCEPT_REST);
                    return;
                }
            };
            // Who connected, as the kernel says: this user, or root.
            let allowed =
                nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                    .is_ok_and(|peer| {
                        let me = nix::unistd::geteuid().as_raw();
                        peer.uid() == me || peer.uid() == 0
                    });
            if !allowed || stream.set_nonblocking(true).is_err() {
                continue;
            }
            // Those still to say hello: as many as may be let in, no more.
            let waiting = self
                .clients
                .iter()
                .filter(|client| !client.welcomed && client.leaving == Leaving::No)
                .count();
            if waiting >= MAX_CLIENTS {
                // Said before its hello is read: the client reads it even
                // if its hello found the socket closed.
                let mut refused = Vec::new();
                Frame::Refused(format!(
                    "{MAX_CLIENTS} connections are waiting to say hello already"
                ))
                .encode(&mut refused);
                let _ = (&stream).write(&refused);
                continue;
            }
            self.clients.push(Client {
                stream,
                decoder: Decoder::new(),
                matcher: None,
                welcomed: false,
                connected: Instant::now(),
                frames: VecDeque::new(),
                offset: 0,
                queued: 0,
                leaving: Leaving::No,
                typist: false,
                role: Role::Attach,
                held: false,
                sent: None,
                version: 0,
            });
        }
    }

    pub(super) fn read_client(&mut self, at: usize) {
        self.read_frames(at);
        // The last size it asked for, once, whatever made the reading stop
        // (it was sent before; a size is harmless).
        if let Some(size) = self.asked.take() {
            self.set_size(size);
        }
    }

    /// The terminal's size, now (or once the master comes).
    fn set_size(&mut self, size: crate::process::terminal::Size) {
        #[cfg(test)]
        {
            self.sizes_set += 1;
        }
        match &self.master {
            Some(master) => {
                let _ = crate::process::terminal::set_window_size(master.as_raw_fd(), size);
            }
            None => self.size = Some(size),
        }
    }

    /// What client `at` sent, frame by frame ([`Self::read_client`]).
    fn read_frames(&mut self, at: usize) {
        for _ in 0..READS_PER_PASS {
            let stdin_full = self.stdin_full();
            let Some(client) = self.clients.get_mut(at) else {
                return;
            };
            if client.leaving != Leaving::No || client.input_waits(stdin_full) {
                return;
            }
            match receive(&client.stream, &mut self.read, &mut self.control) {
                Ok(Received { bytes: 0, .. }) => {
                    client.leaving = Leaving::Now;
                    return;
                }
                Ok(Received {
                    bytes: read,
                    fds,
                    truncated,
                }) => {
                    // A descriptor only from a holder, one at a time, and
                    // none cut off: else it's closed, and its client goes.
                    if !fds.is_empty() || truncated {
                        let one = fds.len() == 1 && !truncated;
                        let fits = one && client.role == Role::Holder && client.sent.is_none();
                        if !fits {
                            client.leaving = Leaving::Now;
                            return;
                        }
                        client.sent = fds.into_iter().next();
                    }
                    client
                        .decoder
                        .push(self.read.get(..read).unwrap_or_default());
                }
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => return,
                Err(_) => {
                    client.leaving = Leaving::Now;
                    return;
                }
            }
            loop {
                let Some(client) = self.clients.get_mut(at) else {
                    return;
                };
                // Detached (or dropped): what it sent after isn't taken.
                if client.leaving != Leaving::No {
                    return;
                }
                match client.decoder.next_frame() {
                    Ok(Some(frame)) => self.handle(at, frame),
                    Ok(None) => break,
                    // A client that isn't the protocol: dropped, alone.
                    Err(_) => {
                        client.leaving = Leaving::Now;
                        return;
                    }
                }
            }
        }
    }

    fn handle(&mut self, at: usize, frame: Frame) {
        // A size asked for before this typing, in the same read, is set
        // before the typing is written (F68): the container never reads
        // keys typed at a size before it has that size. The rest of a
        // read's sizes still come to one change (F40).
        if matches!(frame, Frame::Stdin(_))
            && let Some(size) = self.asked.take()
        {
            self.set_size(size);
        }
        let attached = self.welcomed(Role::Attach);
        let (terminal, takes_stdin, ended) = (self.terminal, self.takes_stdin, self.ended);
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        if client.role != Role::Attach {
            self.handle_control(at, frame);
            return;
        }
        match (client.welcomed, frame) {
            (false, Frame::HelloControl { version, stopper }) => {
                let role = if stopper { Role::Stopper } else { Role::Waiter };
                self.welcome_control(at, version, role);
            }
            (false, Frame::HelloHolder { version }) => {
                self.welcome_control(at, version, Role::Holder);
            }
            (false, hello @ (Frame::Hello { .. } | Frame::HelloHeld { .. })) => {
                let held = matches!(hello, Frame::HelloHeld { .. });
                let (Frame::Hello {
                    version,
                    detach_keys,
                }
                | Frame::HelloHeld {
                    version,
                    detach_keys,
                }) = hello
                else {
                    return;
                };
                let refused = if ended {
                    Some("the container has ended".to_owned())
                } else if attached >= MAX_CLIENTS {
                    Some(format!("{MAX_CLIENTS} clients are attached already"))
                } else if negotiate(version).is_none() {
                    Some(format!("attach protocol version {version}"))
                } else {
                    None
                };
                if let Some(reason) = refused {
                    client.finish_with(&Frame::Refused(reason));
                    return;
                }
                client.matcher = DetachKeys::from_bytes(&detach_keys).map(|keys| keys.matcher());
                client.welcomed = true;
                client.held = held;
                client.send(&Frame::Welcome {
                    version: negotiate(version).unwrap_or(1),
                    terminal,
                    stdin: takes_stdin,
                });
            }
            (true, Frame::Stdin(typed)) => {
                client.typist |= Stdin::first_typist(self.stdin.as_mut());
                let mut passed = Vec::new();
                let detached = client
                    .matcher
                    .as_mut()
                    .is_some_and(|matcher| matcher.feed(&typed, &mut passed));
                if detached {
                    client.finish_with(&Frame::Detached);
                }
                if let Some(stdin) = self.stdin.as_mut().filter(|stdin| !stdin.closing) {
                    // Beyond the queue (a container not reading): dropped;
                    // a held client's all kept (it isn't read while the
                    // queue is full: at most a read's worth beyond it).
                    let room = if client.held {
                        passed.len()
                    } else {
                        STDIN_QUEUE.saturating_sub(stdin.queue.len())
                    };
                    stdin.queue.extend_from_slice(
                        passed.get(..room.min(passed.len())).unwrap_or_default(),
                    );
                }
                // Into the pipe at once, as far as it takes: the queue is
                // for what it doesn't.
                #[cfg(test)]
                {
                    self.typed_after_sizes = self.sizes_set;
                }
                self.write_stdin();
            }
            (true, Frame::StdinEnd) => {
                client.typist |= Stdin::first_typist(self.stdin.as_mut());
                if client.typist {
                    self.typist_done();
                }
            }
            // Set once the read is done ([`Self::read_client`]).
            (true, Frame::Resize { rows, cols }) if terminal => {
                self.asked = Some(crate::process::terminal::Size::new(rows, cols));
            }
            (true, Frame::Resize { .. }) => {}
            // Anything else, or a hello twice: not the protocol.
            _ => client.leaving = Leaving::Now,
        }
    }
}

/// Reads what `stream` has into `buffer`, with any descriptors passed with
/// it (`control`: room for [`MOST_PASSED`](super::MOST_PASSED)),
/// close-on-exec as they're received (`MSG_CMSG_CLOEXEC`, TLPI §61.13.3):
/// never inherited by what the monitor starts. A holder passes one, with its
/// hold.
fn receive(stream: &UnixStream, buffer: &mut [u8], control: &mut Room) -> Result<Received, Errno> {
    use std::os::fd::AsFd as _;
    let flags = nix::sys::socket::MsgFlags::MSG_CMSG_CLOEXEC;
    shipyard_sys::fdpass::receive(stream.as_fd(), buffer, control, flags)
}
