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
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(Quiet));
    let report = crate::doctor::checks(&engine);
    let mut checked = crate::doctor::GuestDoctor::of(&report);
    let marker = std::path::Path::new(super::init::DATA).join(super::disk::REPAIRED);
    let marker = shipyard_sys::fs::read_bounded(&marker, super::disk::REPAIRED_MAX)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    checked
        .checks
        .extend(repaired(marker.as_deref().map(super::disk::marker_says)));
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
/// files) is in `lost+found` on it.
fn repaired(marker: Option<&str>) -> Option<crate::doctor::GuestCheck> {
    marker.map(|how| crate::doctor::GuestCheck {
        level: crate::doctor::Level::Warn,
        name: "data disk".to_owned(),
        text: format!(
            "repaired at a recent boot ({how}): what e2fsck couldn't place (images', \
             containers' or volumes' files) is in lost+found on the VM's data disk"
        ),
    })
}

/// A reporter that says nothing: what the checks find is in them.
struct Quiet;

impl crate::report::Reporter for Quiet {
    fn report(&self, _message: crate::report::Message) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repaired disk is said as a warning while its marker is there,
    /// with what was done; without one, nothing.
    #[test]
    fn a_repaired_disk_is_said() {
        let said = repaired(Some("e2fsck -p exit 4, then -y exit 1")).unwrap();
        assert_eq!(said.level, crate::doctor::Level::Warn);
        assert!(
            said.text.contains("(e2fsck -p exit 4, then -y exit 1)"),
            "{}",
            said.text
        );
        assert!(said.text.contains("lost+found"));
        assert!(repaired(None).is_none());
    }
}
