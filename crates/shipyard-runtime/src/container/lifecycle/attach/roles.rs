//! What a control client is let in for: a waiter, a stopper's one stop,
//! a holder's one hold or release.

use std::os::fd::AsFd as _;

use crate::attach::{CONTROL_VERSION, Frame, HOLD_VERSION, HoldRefusal, REASON_VERSION, negotiate};

use super::connection::{Leaving, Role};
use super::{Attachments, MAX_CONTROL, StopRequest};

impl Attachments {
    /// How many clients of `role` are let in.
    pub(super) fn welcomed(&self, role: Role) -> usize {
        self.clients
            .iter()
            .filter(|client| client.welcomed && client.role == role)
            .count()
    }

    /// Client `at` said [`Frame::HelloControl`] or [`Frame::HelloHolder`]
    /// with `version`: let in as a control client of `role`, unless the
    /// container has ended, [`MAX_CONTROL`] of its kind are in already, or
    /// its version has none of that kind.
    pub(super) fn welcome_control(&mut self, at: usize, version: u32, role: Role) {
        let first = if role == Role::Holder {
            HOLD_VERSION
        } else {
            CONTROL_VERSION
        };
        let controlling = self.welcomed(role);
        let (terminal, takes_stdin, ended) = (self.terminal, self.takes_stdin, self.ended);
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let refused = if ended {
            Some("the container has ended".to_owned())
        } else if controlling >= MAX_CONTROL {
            Some(format!(
                "{MAX_CONTROL} control clients are connected already"
            ))
        } else {
            None
        };
        match (
            refused,
            negotiate(version).filter(|agreed| *agreed >= first),
        ) {
            (None, Some(agreed)) => {
                client.role = role;
                client.welcomed = true;
                client.version = agreed;
                client.send(&Frame::Welcome {
                    version: agreed,
                    terminal,
                    stdin: takes_stdin,
                });
            }
            (Some(reason), _) => client.finish_with(&Frame::Refused(reason)),
            (None, None) => client.finish_with(&Frame::Refused(format!(
                "control protocol version {version}"
            ))),
        }
    }

    /// A frame from control client `at`, welcomed: a stopper's stop, kept
    /// for the monitor (then the stopper goes), or a stop that can't be
    /// (refused); a holder's hold or release, done or not, and answered.
    /// Anything else (a waiter's stop too) isn't the protocol.
    pub(super) fn handle_control(&mut self, at: usize, frame: Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        match client.role {
            Role::Holder => self.handle_holder(at, frame),
            Role::Stopper => self.handle_stop(at, &frame),
            Role::Attach | Role::Waiter => client.leaving = Leaving::Now,
        }
    }

    /// Holder `at`'s one request: a hold (of the descriptor that came with
    /// it, if it's a file, a directory or a socket) or a release (with none
    /// passed), done or not, and answered.
    fn handle_holder(&mut self, at: usize, frame: Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let done = match (frame, client.sent.take()) {
            (Frame::Hold { .. }, None) => Err((
                HoldRefusal::NoDescriptor,
                "no descriptor came with the hold".to_owned(),
            )),
            // Refused, `fd` is dropped: closed.
            (Frame::Hold { label }, Some(fd)) => {
                super::super::holds::holdable(fd.as_fd()).and_then(|()| self.holds.add(fd, label))
            }
            (Frame::Release { label }, None) => self.holds.release(&label),
            (Frame::Release { .. }, Some(_)) => Err((
                HoldRefusal::DescriptorWithRelease,
                "a descriptor came with the release: nothing let go of".to_owned(),
            )),
            _ => {
                client.leaving = Leaving::Now;
                return;
            }
        };
        client.finish_with(&match done {
            Ok(()) => Frame::Done {
                ok: true,
                why: String::new(),
            },
            // A version 3 holder knows only the words.
            Err((reason, why)) if client.version >= REASON_VERSION => {
                Frame::NotDone { reason, why }
            }
            Err((_, why)) => Frame::Done { ok: false, why },
        });
    }

    /// Stopper `at`'s one stop, kept for the monitor (then the stopper
    /// goes), or one that can't be (refused); anything else isn't the
    /// protocol.
    fn handle_stop(&mut self, at: usize, frame: &Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let &Frame::Stop {
            signal,
            timeout_secs,
        } = frame
        else {
            client.leaving = Leaving::Now;
            return;
        };
        match StopRequest::of(signal, timeout_secs) {
            Some(request) => {
                self.stops.push(request);
                client.leaving = Leaving::Now;
            }
            None => client.finish_with(&Frame::Refused(format!(
                "signal {signal} can't stop a container"
            ))),
        }
    }
}
