//! `shipyard doctor`: the engine's checks of this host
//! (`shipyard_engine::doctor`), shown one a line: the level, the check's
//! name, what was found. The exit code is 1 if a check failed, so a script
//! can ask.

use std::process::ExitCode;

use anyhow::Result;
use shipyard_engine::doctor::{Report, verdict};

/// A report line's columns: the level, the check's name; a text's next
/// lines start under its first.
const LEVEL_WIDTH: usize = 5;
const NAME_WIDTH: usize = 16;
const INDENT: usize = LEVEL_WIDTH + 1 + NAME_WIDTH + 1;

/// `shipyard doctor`.
pub(crate) fn run() -> Result<ExitCode> {
    #[cfg_attr(
        not(target_os = "macos"),
        allow(unused_mut, reason = "a Mac's VM adds its checks")
    )]
    let mut report = shipyard_engine::doctor::checks(&crate::reporter::engine());
    #[cfg_attr(
        not(target_os = "macos"),
        allow(unused_mut, reason = "a Mac's VM has its guest's")
    )]
    let mut guest = None;
    #[cfg(target_os = "macos")]
    if !crate::forward::vm_off(std::env::var_os("SHIPYARD_VM").as_deref()) {
        mac::with_the_vm(&mut report);
        guest = mac::guest();
    }
    // `writeln!` instead of `println!`: a closed stdout becomes an error,
    // not a panic.
    let mut out = std::io::stdout().lock();
    let mut failed = show(&report, &mut out)?;
    if let Some(guest) = &guest {
        failed |= show_guest(guest, &mut out)?;
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Writes `report` to `out`: whether a check failed.
fn show(report: &Report, out: &mut impl std::io::Write) -> std::io::Result<bool> {
    let kernel = report
        .kernel
        .as_ref()
        .map(|release| format!(" (kernel {release})"))
        .unwrap_or_default();
    writeln!(out, "host:      {} / {}{kernel}", report.os, report.arch)?;
    writeln!(out, "backend:   {}", report.backend)?;
    if let Some(user) = report.user {
        writeln!(
            out,
            "user:      {} ({})",
            user.uid,
            if user.rootless { "rootless" } else { "root" }
        )?;
    }
    if let Some(isolation) = report.isolation {
        writeln!(out, "isolation: {isolation}")?;
    }
    if report.checks.is_empty() {
        return Ok(false);
    }
    writeln!(out)?;
    for check in &report.checks {
        write_check(out, check.level.label(), check.name, &check.text)?;
    }
    let verdict = verdict(&report.checks);
    writeln!(out, "\n{}", verdict.text)?;
    Ok(verdict.failed)
}

/// One check's line: its level and name in their columns, a text's next
/// lines under its first.
fn write_check(
    out: &mut impl std::io::Write,
    level: &str,
    name: &str,
    text: &str,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{level:<LEVEL_WIDTH$} {name:<NAME_WIDTH$} {}",
        text.replace('\n', &format!("\n{:INDENT$}", ""))
    )
}

/// What a Mac's `doctor` says of its VM's guest: the guest's own checks
/// (the engine's, as on a Linux host), when it runs.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "a Mac's; tested everywhere")
)]
enum Guest {
    /// The VM isn't running: not started to be checked.
    NotRunning,
    /// Its checks.
    Checked(shipyard_engine::doctor::GuestDoctor),
    /// Its checks can't be had (an older guest, a guest gone): why.
    Unanswered(String),
}

/// Writes the guest's section to `out`: whether it failed (one of its
/// checks, or a running guest's checks that can't be had).
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "a Mac's; tested everywhere")
)]
fn show_guest(guest: &Guest, out: &mut impl std::io::Write) -> std::io::Result<bool> {
    use crate::output::printable;
    writeln!(out)?;
    let checked = match guest {
        Guest::NotRunning => {
            writeln!(
                out,
                "guest:     the VM isn't running: `shipyard vm start`, then `shipyard doctor`, \
                 checks its Linux too"
            )?;
            return Ok(false);
        }
        Guest::Unanswered(why) => {
            writeln!(
                out,
                "guest:     its checks can't be had: {}",
                printable(why)
            )?;
            // A VM that runs but can't say how its Linux is: a failure, for
            // a script that asks (F112).
            return Ok(true);
        }
        Guest::Checked(checked) => checked,
    };
    let kernel = checked.kernel.as_deref().unwrap_or("its kernel unknown");
    writeln!(out, "guest:     Linux {}\n", printable(kernel))?;
    for check in &checked.checks {
        write_check(
            out,
            check.level.label(),
            &printable(&check.name),
            &printable(&check.text),
        )?;
    }
    let failed = checked.failed();
    if failed {
        writeln!(
            out,
            "\nthe VM's Linux can't run containers as it is: see its failed checks"
        )?;
    }
    Ok(failed)
}

/// A Mac's report, its containers running in Shipyard's VM (step 10.3c2):
/// the VM's checks (`shipyard_vm::doctor`) as its lines.
#[cfg(target_os = "macos")]
mod mac {
    use shipyard_engine::doctor::{Check, Level, Report};
    use shipyard_vm::doctor::{self, Finding};

    /// `report` as a Mac with its VM has it: the VM as the backend and
    /// the isolation, its checks as the lines (a place for the VM that
    /// can't be had is one of them, failed).
    pub(super) fn with_the_vm(report: &mut Report) {
        report.backend = "linux-vm (Virtualization.framework)";
        report.isolation =
            Some("in Shipyard's Linux VM: containers are isolated there as on Linux");
        report.checks = doctor::checks(shipyard_vm::VmPaths::from_env().as_ref(), crate::REVISION)
            .into_iter()
            .map(|finding| line(&finding))
            .collect();
    }

    /// The guest's own checks, if the VM runs (never started for them);
    /// `None` if the VM's places can't be had (its line says why).
    pub(super) fn guest() -> Option<super::Guest> {
        let paths = shipyard_vm::VmPaths::from_env().ok()?;
        let asked = shipyard_vm::client::ask(
            &paths,
            &[shipyard_runtime::attach::forward::DOCTOR_REQUEST],
            shipyard_runtime::attach::forward::MAX_DOCTOR_ANSWER,
        );
        Some(match asked {
            Ok(None) => super::Guest::NotRunning,
            Ok(Some(bytes)) => match shipyard_engine::doctor::GuestDoctor::decode(&bytes) {
                Ok(checked) => super::Guest::Checked(checked),
                // A guest of another build, kept up by its containers.
                Err(error) => super::Guest::Unanswered(format!(
                    "{error} (a guest of another build: `shipyard vm stop`, then try again)"
                )),
            },
            Err(error) => super::Guest::Unanswered(error.to_string()),
        })
    }

    /// A VM finding as a report's line: its text (a guest's words, a path,
    /// an error's) made printable.
    fn line(finding: &Finding) -> Check {
        let level = match finding.level {
            doctor::Level::Ok => Level::Ok,
            doctor::Level::Warn => Level::Warn,
            doctor::Level::Fail => Level::Fail,
        };
        Check::new(level, finding.name, crate::output::printable(&finding.text))
    }
}

#[cfg(test)]
mod tests {
    use shipyard_engine::doctor::{Check, Level, User};

    use super::*;

    /// The guest's section: its kernel, its checks as the host's lines,
    /// a failed one failing `doctor`; a VM not running said, not failed.
    #[test]
    fn the_guests_checks_are_shown_and_counted() {
        use shipyard_engine::doctor::{GuestCheck, GuestDoctor};
        let check = |level: Level, name: &str| GuestCheck {
            level,
            name: name.to_owned(),
            text: "found".to_owned(),
        };
        let fine = GuestDoctor {
            version: 1,
            kernel: Some("6.18.57-0-virt".to_owned()),
            checks: vec![check(Level::Ok, "cgroups")],
        };
        let mut out = Vec::new();
        assert!(!show_guest(&Guest::Checked(fine.clone()), &mut out).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\nguest:     Linux 6.18.57-0-virt\n\nok    cgroups          found\n"
        );
        let mut broken = fine;
        broken.checks.push(check(Level::Fail, "network"));
        let mut out = Vec::new();
        assert!(show_guest(&Guest::Checked(broken), &mut out).unwrap());
        assert!(String::from_utf8(out).unwrap().contains("fail  network"));
        let mut out = Vec::new();
        let unanswered = Guest::Unanswered("a guest of another build".to_owned());
        assert!(
            show_guest(&unanswered, &mut out).unwrap(),
            "can't be had: failed"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("its checks can't be had: a guest of another build")
        );
        let mut out = Vec::new();
        assert!(!show_guest(&Guest::NotRunning, &mut out).unwrap());
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("the VM isn't running")
        );
    }

    /// The layout, byte for byte (the differential test folds runs of
    /// spaces: it can't see the columns): each level, a name as wide as
    /// its column, and a check of several lines.
    #[test]
    fn a_report_is_laid_out_in_its_columns() {
        let report = Report {
            os: "linux",
            arch: "aarch64",
            kernel: Some("6.8.0".to_owned()),
            backend: "native (host kernel)",
            user: Some(User {
                uid: 1000,
                rootless: true,
            }),
            isolation: None,
            checks: vec![
                Check::new(Level::Ok, "user namespaces", "allowed (probed)"),
                Check::new(
                    Level::Warn,
                    "firewall",
                    "one in the way\nanother in the way",
                ),
                Check::new(Level::Fail, "network", "no pasta"),
            ],
        };
        let mut out = Vec::new();
        assert!(show(&report, &mut out).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "host:      linux / aarch64 (kernel 6.8.0)\n\
             backend:   native (host kernel)\n\
             user:      1000 (rootless)\n\
             \n\
             ok    user namespaces  allowed (probed)\n\
             warn  firewall         one in the way\n                       another in the way\n\
             fail  network          no pasta\n\
             \n\
             containers can't run here as they are: 1 problem to fix\n"
        );
    }

    /// Only warnings, as root, with no kernel said: passes, and says so.
    #[test]
    fn only_warnings_pass_and_root_is_said() {
        let report = Report {
            os: "linux",
            arch: "x86_64",
            kernel: None,
            backend: "native (host kernel)",
            user: Some(User {
                uid: 0,
                rootless: false,
            }),
            isolation: None,
            checks: vec![Check::new(Level::Warn, "cgroups", "limited")],
        };
        let mut out = Vec::new();
        assert!(!show(&report, &mut out).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "host:      linux / x86_64\n\
             backend:   native (host kernel)\n\
             user:      0 (root)\n\
             \n\
             warn  cgroups          limited\n\
             \n\
             containers run here; 1 thing limited\n"
        );
    }

    /// Off Linux: the isolation line, and nothing checked (no verdict).
    #[test]
    fn nothing_checked_says_why_and_passes() {
        let report = Report {
            os: "macos",
            arch: "aarch64",
            kernel: None,
            backend: "native (host kernel)",
            user: None,
            isolation: Some("none here"),
            checks: Vec::new(),
        };
        let mut out = Vec::new();
        assert!(!show(&report, &mut out).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "host:      macos / aarch64\nbackend:   native (host kernel)\nisolation: none here\n"
        );
    }
}
