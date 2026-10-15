//! What a container's monitor holds for its caller (stage 9, step 9.7b1):
//! descriptors whose `flock`s, or bound sockets, keep the caller's resources
//! alive once the caller has gone, each with its label. Given at `create`
//! (`--hold-fd N=LABEL`), added while the container runs (a holder's
//! [`Frame::Hold`](crate::attach::Frame::Hold)), let go of by label
//! ([`Frame::Release`](crate::attach::Frame::Release)); what's left at the
//! end is the exit command's.

use std::os::fd::OwnedFd;

/// The most descriptors a monitor holds ([`MOST_HOLDS`](super::super::MOST_HOLDS)).
pub(in crate::container) const MOST: usize = super::super::MOST_HOLDS;

/// The most bytes of labels it holds, all told
/// ([`MOST_HOLD_LABEL_BYTES`](super::super::MOST_HOLD_LABEL_BYTES)).
pub(in crate::container) const MOST_LABEL_BYTES: usize = super::super::MOST_HOLD_LABEL_BYTES;

/// Whether `labels` (with their separators) fit in [`MOST_LABEL_BYTES`].
pub(in crate::container) fn labels_fit<'a>(labels: impl IntoIterator<Item = &'a str>) -> bool {
    labels
        .into_iter()
        .map(|label| label.len().saturating_add(1))
        .fold(0_usize, usize::saturating_add)
        <= MOST_LABEL_BYTES
}

/// The descriptors a monitor holds, in order, each with its label. Its
/// `Default` holds none and takes none (what's left once they're handed
/// on).
#[derive(Debug, Default)]
pub(super) struct Holds {
    held: Vec<(OwnedFd, String)>,
    /// Whether more are taken: while the container runs (none before the
    /// monitor has its holds, none once it begins to end: a hold then
    /// could miss the exit command).
    taking: bool,
}

impl Holds {
    /// The holds given at `create`, in order (labels checked by `create`:
    /// two may share one, as two ports do).
    pub(super) fn new(held: Vec<(OwnedFd, String)>) -> Self {
        Self { held, taking: true }
    }

    /// No more are taken (the container is ending); a release still lets
    /// go.
    pub(super) fn stop_taking(&mut self) {
        self.taking = false;
    }

    /// Holds `fd` as `label` too, unless none are taken now, something is
    /// held as `label` already, or [`MOST`] are held: then `fd` is closed,
    /// and the answer says why.
    pub(super) fn add(&mut self, fd: OwnedFd, label: String) -> Result<(), String> {
        if !self.taking {
            return Err("the container is starting or ending: nothing more is held".to_owned());
        }
        if self.held.iter().any(|(_, held)| *held == label) {
            return Err(format!("something is held as {label} already"));
        }
        if self.held.len() >= MOST {
            return Err(format!("{MOST} {}", crate::container::HOLDS_FULL));
        }
        if !labels_fit(
            self.held
                .iter()
                .map(|(_, held)| held.as_str())
                .chain([label.as_str()]),
        ) {
            return Err(format!(
                "the labels held already come to almost {MOST_LABEL_BYTES} bytes"
            ));
        }
        self.held.push((fd, label));
        Ok(())
    }

    /// Lets go of (closes) the one held as `label`, unless there's none,
    /// or more than one (two ports, say: which isn't said).
    pub(super) fn release(&mut self, label: &str) -> Result<(), String> {
        let mut found = self
            .held
            .iter()
            .enumerate()
            .filter(|(_, (_, held))| held == label)
            .map(|(at, _)| at);
        match (found.next(), found.next()) {
            (Some(at), None) => {
                drop(self.held.remove(at));
                Ok(())
            }
            (None, _) => Err(format!("nothing is held as {label}")),
            (Some(_), Some(_)) => Err(format!("more than one descriptor is held as {label}")),
        }
    }

    /// The descriptors, in order.
    pub(super) fn fds(&self) -> impl Iterator<Item = &OwnedFd> {
        self.held.iter().map(|(fd, _)| fd)
    }

    /// How many are held.
    pub(super) fn len(&self) -> usize {
        self.held.len()
    }

    /// Their labels, in order, comma-separated (what the exit command is
    /// told: labels have no commas).
    pub(super) fn labels(&self) -> String {
        self.held
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn fd() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    /// Holds come in order, with their labels; a label held already, or a
    /// 65th, is refused; a release closes the one of its label, and refuses
    /// none, or two.
    #[test]
    fn holds_are_added_and_let_go_of_by_label() {
        let mut holds = Holds::new(vec![
            (fd(), "port".to_owned()),
            (fd(), "port".to_owned()),
            (fd(), "lease:app".to_owned()),
        ]);
        let refused = |result: Result<(), String>, why: &str| {
            let error = result.unwrap_err();
            assert!(error.contains(why), "{error}");
        };
        assert_eq!(holds.labels(), "port,port,lease:app");
        refused(
            holds.add(fd(), "lease:app".to_owned()),
            "held as lease:app already",
        );
        holds.add(fd(), "lease:db".to_owned()).unwrap();
        assert_eq!(holds.labels(), "port,port,lease:app,lease:db");
        refused(holds.release("port"), "more than one");
        refused(holds.release("lease:none"), "nothing is held as lease:none");
        holds.release("lease:app").unwrap();
        assert_eq!(holds.labels(), "port,port,lease:db");
        assert_eq!(holds.len(), 3);
        // Let go of, it can be held again.
        holds.add(fd(), "lease:app".to_owned()).unwrap();
        // As many descriptors as a monitor holds: more than a test's soft
        // limit may allow.
        let (_, hard) =
            nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE).unwrap();
        let room = u64::try_from(MOST + 64).unwrap().min(hard);
        nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE, room, hard)
            .unwrap();
        if room < u64::try_from(MOST + 64).unwrap() {
            return;
        }
        for at in holds.len()..MOST {
            holds.add(fd(), format!("fd-{at}")).unwrap();
        }
        assert_eq!(holds.len(), MOST);
        refused(
            holds.add(fd(), "one-more".to_owned()),
            &format!("{MOST} descriptors are held"),
        );
        // The words a caller tells it by (the engine's `network connect`),
        // as older ones match them too.
        assert_eq!(crate::container::HOLDS_FULL, "descriptors are held already");
        // Ending: none taken, a release still done; none before the
        // monitor has its holds either.
        holds.release("lease:app").unwrap();
        holds.stop_taking();
        refused(
            holds.add(fd(), "lease:app".to_owned()),
            "starting or ending",
        );
        holds.release("lease:db").unwrap();
        refused(
            Holds::default().add(fd(), "a".to_owned()),
            "starting or ending",
        );
    }
}
