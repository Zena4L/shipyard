//! `__guest-doctor` (stage 10's close): the guest's own checks, the
//! engine's as on a Linux host (its probe helper included: this same
//! binary), as a [`GuestDoctor`] on stdout, for the Mac's `doctor` to show
//! beside the VM's.
//!
//! [`GuestDoctor`]: crate::doctor::GuestDoctor

use std::io::Write as _;
use std::sync::Arc;

/// Prints the checks; its exit code (1: they couldn't be written, said on
/// stderr). A failed check is in what's printed, not in the exit code.
pub(super) fn print() -> i32 {
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(crate::report::Quiet));
    let report = crate::doctor::checks(&engine);
    let mut checked = crate::doctor::GuestDoctor::of(&report);
    let marker = std::path::Path::new(super::init::DATA).join(super::disk::REPAIRED);
    let marker = shipyard_sys::fs::read_bounded(&marker, super::disk::REPAIRED_MAX)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    // Counted only after a repair (the marker's there): what it left.
    let lost = marker.as_ref().and_then(|_| {
        std::fs::read_dir(std::path::Path::new(super::init::DATA).join("lost+found"))
            .map(Iterator::count)
            .ok()
    });
    checked.checks.extend(repaired(
        marker.as_deref().map(super::disk::marker_says),
        lost,
    ));
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    checked.checks.extend(rosetta(
        super::init::rosetta_given(&cmdline),
        crate::image::platforms_here().len() > 1,
    ));
    checked.checks.push(agent_port(handed_on()));
    if let Some(home) = super::home_of(&cmdline) {
        let folders: Vec<std::path::PathBuf> = shipyard_runtime::attach::forward::GUARDED_FOLDERS
            .iter()
            .map(|(folder, _)| std::path::Path::new(home).join(folder))
            .filter(|folder| folder.is_dir())
            .collect();
        checked.checks.extend(privacy(&try_reading(&folders)));
    }
    let written = checked
        .encode()
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())
        });
    match written {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "shipyard: the guest's checks: {why}");
            1
        }
    }
}

/// The data disk's line while it carries a repair's marker (`marker`:
/// what was done, D10.4-2), a warning: it was repaired at a recent boot,
/// and what `e2fsck` couldn't place (images', containers' or volumes'
/// files) is in `lost+found` on it, `lost` entries (F113: nothing on the Mac
/// reaches them yet).
fn repaired(marker: Option<&str>, lost: Option<usize>) -> Option<crate::doctor::GuestCheck> {
    let held = match lost {
        Some(0) => "lost+found on the VM's data disk is empty".to_owned(),
        Some(count) => format!(
            "what e2fsck couldn't place (images', containers' or volumes' files) is in \
             lost+found on the VM's data disk: {count} entr{}",
            if count == 1 { "y" } else { "ies" }
        ),
        None => "what e2fsck couldn't place (images', containers' or volumes' files) is in \
                 lost+found on the VM's data disk"
            .to_owned(),
    };
    marker.map(|how| crate::doctor::GuestCheck {
        level: crate::doctor::Level::Warn,
        name: "data disk".to_owned(),
        text: format!("repaired at a recent boot ({how}): {held}"),
    })
}

/// Rosetta's line, when the Mac gives its share (`given`): registered (amd64
/// programs run through it) or not (the guest's console says why). Without
/// the share, nothing: the Mac's own `rosetta` line says so.
fn rosetta(given: bool, registered: bool) -> Option<crate::doctor::GuestCheck> {
    given.then(|| crate::doctor::GuestCheck {
        level: if registered {
            crate::doctor::Level::Ok
        } else {
            crate::doctor::Level::Fail
        },
        name: "rosetta".to_owned(),
        text: if registered {
            "registered: amd64 images run through it".to_owned()
        } else {
            "given, but not registered (the VM's console log says why): amd64 images won't run"
                .to_owned()
        },
    })
}

/// Whether this process (a command the agent runs) holds the agent's
/// listening socket (a vsock socket on [`forward::AGENT_PORT`]): it never
/// should, else a command left running (a detached container's monitor)
/// could take the Mac's next connection.
///
/// [`forward::AGENT_PORT`]: shipyard_runtime::attach::forward::AGENT_PORT
fn handed_on() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .any(super::agent::is_agent_socket)
}

/// The agent's port's line (F108): held by the guest's init alone, or
/// handed on to a command (`leaked`).
fn agent_port(leaked: bool) -> crate::doctor::GuestCheck {
    if leaked {
        crate::doctor::GuestCheck {
            level: crate::doctor::Level::Fail,
            name: "agent port".to_owned(),
            text: "a command holds the agent's socket: one left running could take the Mac's \
                   connections"
                .to_owned(),
        }
    } else {
        crate::doctor::GuestCheck {
            level: crate::doctor::Level::Ok,
            name: "agent port".to_owned(),
            text: "not handed to the commands the agent runs".to_owned(),
        }
    }
}

/// How long a folder macOS guards is given to be read: past it, macOS is
/// likely asking the user.
const READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What reading a folder macOS guards came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tried {
    /// Read.
    Read,
    /// Refused (the permission isn't given).
    Refused,
    /// No answer within [`READ_LIMIT`] (macOS is asking), or the try
    /// couldn't be made.
    Unanswered,
}

/// Reads each of `folders`' entries by `ls`, each in a process of its own,
/// all at once: a read macOS holds while it asks the user waits in the
/// kernel, where nothing can end it, so one still waiting at
/// [`READ_LIMIT`] is left to end by itself (PID 1 reaps it). An `ls` that
/// fails is taken for macOS's refusal (it doesn't say its `errno`; the
/// folder was there a moment before).
fn try_reading(folders: &[std::path::PathBuf]) -> Vec<(String, Tried)> {
    use std::process::{Command, Stdio};
    let mut children: Vec<(String, Option<std::process::Child>)> = folders
        .iter()
        .map(|folder| {
            let child = Command::new("/bin/ls")
                .arg("-A")
                .arg(folder)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            (folder.display().to_string(), child)
        })
        .collect();
    let mut tried: Vec<Option<Tried>> = vec![None; children.len()];
    let deadline = std::time::Instant::now() + READ_LIMIT;
    while tried.iter().any(Option::is_none) && std::time::Instant::now() < deadline {
        for ((_, child), seen) in children.iter_mut().zip(tried.iter_mut()) {
            if seen.is_some() {
                continue;
            }
            *seen = match child.as_mut().map(std::process::Child::try_wait) {
                None | Some(Err(_)) => Some(Tried::Unanswered),
                Some(Ok(Some(status))) if status.success() => Some(Tried::Read),
                Some(Ok(Some(_))) => Some(Tried::Refused),
                Some(Ok(None)) => None,
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    children
        .into_iter()
        .zip(tried)
        .map(|((folder, _), seen)| (folder, seen.unwrap_or(Tried::Unanswered)))
        .collect()
}

/// The privacy line (F105, F140): whether the VM may read the folders macOS
/// guards (`tried`: each folder and what reading it came to). Refused ones
/// name the permission; unanswered ones, the prompt. None tried: nothing.
fn privacy(tried: &[(String, Tried)]) -> Option<crate::doctor::GuestCheck> {
    let of = |wanted: Tried| -> Vec<&str> {
        tried
            .iter()
            .filter(|(_, read)| *read == wanted)
            .map(|(folder, _)| folder.as_str())
            .collect()
    };
    if tried.is_empty() {
        return None;
    }
    let (refused, unanswered) = (of(Tried::Refused), of(Tried::Unanswered));
    let mut said = Vec::new();
    if !refused.is_empty() {
        said.push(super::shares::privacy_message(&refused.join(", ")));
    }
    if !unanswered.is_empty() {
        said.push(format!(
            "{} gave no answer within {} s: macOS may be asking whether `shipyard` may read it",
            unanswered.join(", "),
            READ_LIMIT.as_secs()
        ));
    }
    Some(if said.is_empty() {
        crate::doctor::GuestCheck {
            level: crate::doctor::Level::Ok,
            name: "privacy".to_owned(),
            text: format!(
                "the VM may read the folders macOS guards ({})",
                shipyard_runtime::attach::forward::GUARDED_FOLDERS
                    .map(|(_, name)| name)
                    .join(", ")
            ),
        }
    } else {
        crate::doctor::GuestCheck {
            level: crate::doctor::Level::Warn,
            name: "privacy".to_owned(),
            text: said.join("; "),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repaired disk is said as a warning while its marker is there,
    /// with what was done; without one, nothing.
    #[test]
    fn a_repaired_disk_is_said() {
        let said = repaired(Some("e2fsck -p exit 4, then -y exit 1"), Some(3)).unwrap();
        assert_eq!(said.level, crate::doctor::Level::Warn);
        assert!(
            said.text.contains("(e2fsck -p exit 4, then -y exit 1)"),
            "{}",
            said.text
        );
        assert!(
            said.text
                .ends_with("lost+found on the VM's data disk: 3 entries"),
            "{}",
            said.text
        );
        let one = repaired(Some("e2fsck -p exit 1"), Some(1)).unwrap();
        assert!(one.text.ends_with(": 1 entry"), "{}", one.text);
        let none = repaired(Some("e2fsck -p exit 1"), Some(0)).unwrap();
        assert!(
            none.text
                .ends_with("lost+found on the VM's data disk is empty"),
            "{}",
            none.text
        );
        assert!(repaired(None, Some(3)).is_none());
    }

    /// The agent's port's line: ok when held by init alone, a failure when
    /// a command holds it (F108).
    #[test]
    fn the_agents_port_is_said_held_or_handed_on() {
        assert_eq!(agent_port(false).level, crate::doctor::Level::Ok);
        let leaked = agent_port(true);
        assert_eq!(leaked.level, crate::doctor::Level::Fail);
        assert!(leaked.text.contains("holds the agent's socket"));
        // This test's own process holds no vsock socket.
        assert!(!handed_on());
    }

    /// The privacy line: ok when every folder was read; a warning naming
    /// each refused (with the permission) and each unanswered (the prompt),
    /// never one that was read; nothing when none was tried.
    #[test]
    fn the_privacy_line_names_what_macos_holds_back() {
        assert_eq!(privacy(&[]), None);
        let read = privacy(&[("/Users/me/Desktop".to_owned(), Tried::Read)]).unwrap();
        assert_eq!(read.level, crate::doctor::Level::Ok);
        let held = privacy(&[
            ("/Users/me/Desktop".to_owned(), Tried::Refused),
            ("/Users/me/Documents".to_owned(), Tried::Read),
            ("/Users/me/Downloads".to_owned(), Tried::Unanswered),
        ])
        .unwrap();
        assert_eq!(held.level, crate::doctor::Level::Warn);
        assert!(
            held.text
                .contains("hasn't let the VM read /Users/me/Desktop: allow `shipyard`"),
            "{}",
            held.text
        );
        assert!(held.text.contains("/Users/me/Downloads gave no answer"));
        assert!(!held.text.contains("Documents"), "{}", held.text);
    }

    /// Rosetta's line only where the Mac gives its share: ok if registered,
    /// a failure if not.
    #[test]
    fn rosetta_is_said_where_it_is_given() {
        assert!(rosetta(false, false).is_none());
        assert_eq!(rosetta(true, true).unwrap().level, crate::doctor::Level::Ok);
        assert_eq!(
            rosetta(true, false).unwrap().level,
            crate::doctor::Level::Fail
        );
    }
}
