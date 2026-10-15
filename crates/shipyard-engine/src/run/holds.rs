//! What a detached container's monitor holds for it: the descriptors whose
//! `flock`s (or bound sockets) keep its resources alive once the process
//! that made it has gone, and which the monitor hands on to `__cleanup`
//! (in this order, from 3 up) after the container ends.
//!
//! Each is described by a kind, so `__cleanup` knows what the descriptor at
//! each place is: its form (`KIND` or `KIND:NAME`) is the hold's label, which
//! the monitor tells `__cleanup` (`SHIPYARD_HOLD_LABELS`, stage 9, step
//! 9.7b1), with those held since `create` (a network connected later); a
//! monitor from before 9.7b1 doesn't, and the layout travels in `__cleanup`'s
//! arguments, one `--hold KIND` per descriptor. They're made and read by the same
//! binary (the exit command is `/proc/self/exe`), so the form is this
//! module's alone, never kept on disk.

#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by Linux's detached containers only")
)]

use std::os::fd::BorrowedFd;

/// What a held descriptor is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldKind {
    /// A network lease's file, locked: the container's address on the
    /// network `network`.
    Lease {
        /// The network.
        network: String,
    },
    /// A bound socket holding a published host port.
    Port,
    /// The run directory's lock file, locked.
    RunDirLock,
    /// The run directory itself, locked.
    RunDir,
    /// A named volume's lock file, locked (shared).
    Volume,
    /// The image area's directory, locked: the area named `name`.
    Area {
        /// Its name, in the image store's `containers/`.
        name: String,
    },
    /// A container's own writable area's directory (`ctr-<id>`, 9.4a),
    /// locked: closed at the end, never removed, unless the container goes
    /// with its end (`--rm`).
    OwnedArea {
        /// Its name, in the image store's `containers/`.
        name: String,
    },
    /// The container's `running` lock file, locked (9.4a): free only once
    /// everything of a start is let go, so closed last.
    Running,
}

impl HoldKind {
    /// Its argument form: `lease:NAME`, `port`, `run-dir-lock`, `run-dir`,
    /// `volume`, `area:NAME`, `owned-area:NAME`, `running`.
    pub(crate) fn to_arg(&self) -> String {
        match self {
            Self::Lease { network } => format!("lease:{network}"),
            Self::Port => "port".to_owned(),
            Self::RunDirLock => "run-dir-lock".to_owned(),
            Self::RunDir => "run-dir".to_owned(),
            Self::Volume => "volume".to_owned(),
            Self::Area { name } => format!("area:{name}"),
            Self::OwnedArea { name } => format!("owned-area:{name}"),
            Self::Running => "running".to_owned(),
        }
    }

    /// Read back from [`to_arg`](Self::to_arg)'s form.
    pub(crate) fn from_arg(arg: &str) -> Option<Self> {
        let (kind, name) = arg
            .split_once(':')
            .map_or((arg, None), |(kind, name)| (kind, Some(name.to_owned())));
        match (kind, name) {
            ("lease", Some(network)) if !network.is_empty() => Some(Self::Lease { network }),
            ("port", None) => Some(Self::Port),
            ("run-dir-lock", None) => Some(Self::RunDirLock),
            ("run-dir", None) => Some(Self::RunDir),
            ("volume", None) => Some(Self::Volume),
            ("area", Some(name)) if !name.is_empty() => Some(Self::Area { name }),
            ("owned-area", Some(name)) if !name.is_empty() => Some(Self::OwnedArea { name }),
            ("running", None) => Some(Self::Running),
            _ => None,
        }
    }

    /// Whether a descriptor of file type `mode` (`st_mode & S_IFMT`) can be
    /// one of these: a file for a lock file, a directory for a directory,
    /// a socket for a port.
    #[cfg(target_os = "linux")]
    pub(crate) fn fits(&self, mode: libc::mode_t) -> bool {
        let wanted = match self {
            Self::Lease { .. } | Self::RunDirLock | Self::Volume | Self::Running => libc::S_IFREG,
            Self::RunDir | Self::Area { .. } | Self::OwnedArea { .. } => libc::S_IFDIR,
            Self::Port => libc::S_IFSOCK,
        };
        mode & libc::S_IFMT == wanted
    }
}

/// The descriptors a container's monitor is to hold, in order, each with
/// its kind, and its kind's label (`to_arg`'s form).
#[derive(Debug, Default)]
pub(crate) struct Holds<'a> {
    pub(crate) held: Vec<(HoldKind, String, BorrowedFd<'a>)>,
}

impl<'a> Holds<'a> {
    /// Adds `fd`, of `kind`.
    pub(crate) fn add(&mut self, kind: HoldKind, fd: BorrowedFd<'a>) {
        let label = kind.to_arg();
        self.held.push((kind, label, fd));
    }

    /// The descriptors in order, each with its label, for the runtime's
    /// `create`.
    pub(crate) fn for_runtime(&self) -> Vec<shipyard_runtime::container::Hold<'_>> {
        self.held
            .iter()
            .map(|(_, label, fd)| shipyard_runtime::container::Hold {
                fd: *fd,
                label: label.as_str(),
            })
            .collect()
    }

    /// `__cleanup`'s arguments saying what each is: `--hold KIND` each.
    pub(crate) fn args(&self) -> Vec<String> {
        self.held
            .iter()
            .flat_map(|(_, label, _)| ["--hold".to_owned(), label.clone()])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label the engine gives is one the runtime takes, the longest
    /// too (a network's name at its longest, a container's own area): else
    /// every detached start would fail.
    #[test]
    fn every_kind_is_a_label() {
        let longest_network = "N".repeat(64);
        let area = format!("run-{}-{:x}-{:x}", u32::MAX, u128::MAX, u64::MAX);
        let owned = format!("ctr-{}", "f".repeat(64));
        for kind in [
            HoldKind::Lease {
                network: longest_network,
            },
            HoldKind::Port,
            HoldKind::RunDirLock,
            HoldKind::RunDir,
            HoldKind::Volume,
            HoldKind::Area { name: area },
            HoldKind::OwnedArea { name: owned },
            HoldKind::Running,
        ] {
            let label = kind.to_arg();
            assert!(shipyard_runtime::attach::is_label(&label), "{label}");
            assert_eq!(HoldKind::from_arg(&label), Some(kind));
        }
    }

    /// A kind fits only the file type it's held as: a lock file a regular
    /// file, a directory a directory, a port a socket.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kind_fits_only_its_file_type() {
        let file = libc::S_IFREG | 0o600;
        let dir = libc::S_IFDIR | 0o700;
        let socket = libc::S_IFSOCK | 0o600;
        let lease = HoldKind::Lease {
            network: "n".to_owned(),
        };
        let area = HoldKind::Area {
            name: "a".to_owned(),
        };
        let owned = HoldKind::OwnedArea {
            name: "ctr-a".to_owned(),
        };
        for (kind, fits) in [
            (&lease, file),
            (&HoldKind::RunDirLock, file),
            (&HoldKind::Volume, file),
            (&HoldKind::Running, file),
            (&HoldKind::RunDir, dir),
            (&area, dir),
            (&owned, dir),
            (&HoldKind::Port, socket),
        ] {
            assert!(kind.fits(fits), "{kind:?}");
            for other in [file, dir, socket].into_iter().filter(|mode| *mode != fits) {
                assert!(!kind.fits(other), "{kind:?} isn't {other:o}");
            }
        }
    }

    /// Every kind reads back as written; what isn't one isn't read.
    #[test]
    fn kinds_read_back_as_written() {
        let kinds = [
            HoldKind::Lease {
                network: "web".to_owned(),
            },
            HoldKind::Port,
            HoldKind::RunDirLock,
            HoldKind::RunDir,
            HoldKind::Volume,
            HoldKind::Area {
                name: "run-1-2-3".to_owned(),
            },
            HoldKind::OwnedArea {
                name: "ctr-0".to_owned(),
            },
            HoldKind::Running,
        ];
        for kind in kinds {
            assert_eq!(HoldKind::from_arg(&kind.to_arg()), Some(kind));
        }
        for wrong in [
            "",
            "lease",
            "lease:",
            "area:",
            "port:x",
            "socket",
            "volume:v",
            "owned-area:",
            "running:x",
        ] {
            assert_eq!(HoldKind::from_arg(wrong), None, "{wrong}");
        }
    }
}
