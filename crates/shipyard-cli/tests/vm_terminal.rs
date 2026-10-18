//! End-to-end tests of a terminal through the macOS VM (step 10.3b2): the
//! Mac's `shipyard`, on a pseudo-terminal of the fake shell's
//! (`shipyard_test_support::pty`), running `run -it` in the guest, where
//! the agent gives it a pseudo-terminal of its own (D10.3-7).
//!
//! They run only where asked (`SHIPYARD_TEST_VM=required`, the guest built
//! by `scripts/guest/build.sh`), as `tests/vm.rs`'s do. Like
//! `tests/terminal.rs`, **this binary must stay single-threaded** (the fake
//! shell is forked from it and runs Rust): `main` runs the tests one by
//! one, and never starts a thread.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test binary, a panic is a test failure with a clear message"
)]

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    mac::main()
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    println!(
        "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
    );
    ExitCode::SUCCESS
}

#[cfg(target_os = "macos")]
mod vm_support;

#[cfg(target_os = "macos")]
mod mac {
    use std::io::Write as _;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use shipyard_test_support::pty::{Mode, Session, Shell, Status};

    use super::vm_support::{SHIPYARD, Vm};

    const IMAGE: &str = "alpine:3.20";
    const CTRL_C: &[u8] = b"\x03";

    /// A test, by name.
    type Test = (&'static str, fn(&Vm));

    /// A fake shell whose jobs reach `vm` (the harness clears their
    /// environment: the home the VM shares is given back too).
    fn session(vm: &Vm, script: fn(&mut Shell)) -> Session {
        let mut env = vm.env();
        env.push(("HOME".to_owned(), std::env::var_os("HOME")));
        // SAFETY: this binary is single-threaded (see the top of the file).
        unsafe { Session::start_with_env(SHIPYARD, env, script) }
    }

    /// `shipyard ARGS` off the terminal, succeeding.
    fn ok(vm: &Vm, args: &[&str]) {
        let output = vm.shipyard(args);
        assert!(output.status.success(), "shipyard {args:?}: {output:?}");
    }

    /// Waits for container `name` to be gone from the guest (its `--rm`
    /// done): its command ended, not left running there.
    fn gone(vm: &Vm, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while vm.shipyard(&["inspect", name]).status.success() {
            assert!(Instant::now() < deadline, "{name} still runs in the guest");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// `run -it`'s shell in the guest: on a terminal (`tty`), raw on the
    /// Mac (the guest's terminal echoes and edits), its size this window's
    /// and following it, Ctrl-C reaching its job only (the shell lives on),
    /// and its exit code the Mac's.
    fn run_it_is_a_terminal_in_the_guest(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(Mode::Foreground, &["run", "--rm", "-it", IMAGE, "sh"]);
            shell.wait(job);
        });
        session.job();
        session.type_keys(b"tty; stty size\r");
        session.expect("/dev/pts/");
        session.expect("24 80");
        session.resize(30, 100);
        session.type_keys(b"stty size\r");
        session.expect("30 100");
        // The mark printed by the job itself (split by quotes: the echo of
        // what's typed never matches): the sleep is the terminal's
        // foreground job when it's seen.
        session.type_keys(b"sh -c 'echo sl\"\"eeping; exec sleep 60'\r");
        session.expect("sleeping");
        session.type_keys(CTRL_C);
        session.type_keys(b"echo after-$?\r");
        session.expect("after-130");
        session.type_keys(b"exit 7\r");
        assert_eq!(
            session.status(),
            Status::Exited(7),
            "{}",
            session.transcript()
        );
        session.finish();
    }

    /// Without input, this terminal stays as it is (not raw), so a Ctrl-C
    /// is a SIGINT to this `shipyard`: passed on as a frame, to the guest
    /// terminal's foreground group, the guest's `shipyard`, and from it to
    /// the container, which ends by it.
    fn a_ctrl_c_without_input_is_passed_on_as_a_signal(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(
                Mode::Foreground,
                &[
                    "run",
                    "--rm",
                    IMAGE,
                    "sh",
                    "-c",
                    "echo re''ady; exec sleep 60",
                ],
            );
            shell.wait(job);
        });
        session.job();
        session.expect("ready");
        session.type_keys(CTRL_C);
        assert_eq!(
            session.status(),
            Status::Exited(130),
            "{}",
            session.transcript()
        );
        session.finish();
    }

    /// A third SIGINT (the command passing the first two by) makes this
    /// `shipyard` let go (130), saying so; the command in the guest is then
    /// hung up on, and ends.
    fn a_third_interrupt_lets_go_and_the_guest_hangs_up(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(
                Mode::Foreground,
                &[
                    "run",
                    "--rm",
                    "--name",
                    "pty-int",
                    IMAGE,
                    "sh",
                    "-c",
                    "trap 'echo g\"\"ot' INT; echo re''ady; while :; do sleep 1; done",
                ],
            );
            shell.wait(job);
        });
        session.job();
        session.expect("ready");
        // Each one seen before the next: two at once would be one signal.
        session.type_keys(CTRL_C);
        session.expect("got");
        session.type_keys(CTRL_C);
        session.expect("got");
        session.type_keys(CTRL_C);
        session.expect("interrupted 3 times: letting go of the command");
        assert_eq!(
            session.status(),
            Status::Exited(130),
            "{}",
            session.transcript()
        );
        session.finish();
        gone(vm, "pty-int");
    }

    /// Closing the Mac's terminal hangs the guest's up: this `shipyard`
    /// ends as a hung-up one does (129), and so does the command in the
    /// guest (its `--rm` container then removed), not left running there.
    fn a_hangup_on_the_mac_ends_the_command_in_the_guest(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(
                Mode::Foreground,
                &[
                    "run",
                    "--rm",
                    "-it",
                    "--name",
                    "pty-hup",
                    IMAGE,
                    "sh",
                    "-c",
                    "echo re\"\"ady; exec sleep 100",
                ],
            );
            shell.wait(job);
        });
        session.job();
        session.expect("ready");
        session.hang_up();
        assert_eq!(
            session.status(),
            Status::Exited(129),
            "{}",
            session.transcript()
        );
        session.finish();
        gone(vm, "pty-hup");
    }

    /// The detach keys go through to the guest's `run -it`, which detaches
    /// there: this `shipyard` ends as it does (0), the container runs on.
    fn the_detach_keys_detach_in_the_guest(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(
                Mode::Foreground,
                &["run", "-it", "--name", "pty-detach", IMAGE, "sh"],
            );
            shell.wait(job);
        });
        session.job();
        session.type_keys(b"echo re''ady\r");
        session.expect("ready");
        session.type_keys(b"\x10\x11");
        assert_eq!(
            session.status(),
            Status::Exited(0),
            "{}",
            session.transcript()
        );
        session.finish();
        // (`ps` lists the running ones only.)
        let running = vm.shipyard(&["ps"]);
        let _ = vm.shipyard(&["rm", "-f", "pty-detach"]);
        assert!(
            String::from_utf8_lossy(&running.stdout).contains("pty-detach"),
            "{running:?}"
        );
    }

    /// Without `-t`, the guest's command still gets a terminal (both of
    /// the Mac's ends are one): `test -t` there says so, and it ends.
    fn a_plain_command_on_a_terminal_gets_one(vm: &Vm) {
        let mut session = session(vm, |shell| {
            let job = shell.spawn(
                Mode::Foreground,
                &[
                    "run",
                    "--rm",
                    "-t",
                    IMAGE,
                    "sh",
                    "-c",
                    "test -t 1 && echo on-a-tty",
                ],
            );
            shell.wait(job);
        });
        session.job();
        session.expect("on-a-tty");
        assert_eq!(
            session.status(),
            Status::Exited(0),
            "{}",
            session.transcript()
        );
        session.finish();
    }

    pub(super) fn main() -> ExitCode {
        let tests: &[Test] = &[
            (
                "run_it_is_a_terminal_in_the_guest",
                run_it_is_a_terminal_in_the_guest,
            ),
            (
                "a_ctrl_c_without_input_is_passed_on_as_a_signal",
                a_ctrl_c_without_input_is_passed_on_as_a_signal,
            ),
            (
                "a_third_interrupt_lets_go_and_the_guest_hangs_up",
                a_third_interrupt_lets_go_and_the_guest_hangs_up,
            ),
            (
                "a_hangup_on_the_mac_ends_the_command_in_the_guest",
                a_hangup_on_the_mac_ends_the_command_in_the_guest,
            ),
            (
                "the_detach_keys_detach_in_the_guest",
                the_detach_keys_detach_in_the_guest,
            ),
            (
                "a_plain_command_on_a_terminal_gets_one",
                a_plain_command_on_a_terminal_gets_one,
            ),
        ];
        if std::env::args().any(|arg| arg == "--list") {
            for (name, _) in tests {
                println!("{name}: test");
            }
            return ExitCode::SUCCESS;
        }
        let Some(vm) = Vm::new("pty") else {
            println!(
                "\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; {} filtered out\n",
                tests.len()
            );
            return ExitCode::SUCCESS;
        };
        shipyard_test_support::pty::kill_everything_on_alarm();
        // Booted and the image pulled before any terminal's wait starts.
        ok(&vm, &["vm", "start"]);
        ok(&vm, &["pull", IMAGE]);
        println!("\nrunning {} tests", tests.len());
        let mut failed = 0;
        for (name, test) in tests {
            print!("test {name} ... ");
            let _ = std::io::stdout().flush();
            // SAFETY: `alarm` has no preconditions.
            unsafe { libc::alarm(120) };
            let passed = std::panic::catch_unwind(|| test(&vm)).is_ok();
            println!("{}", if passed { "ok" } else { "FAILED" });
            failed += usize::from(!passed);
        }
        let result = if failed == 0 { "ok" } else { "FAILED" };
        println!(
            "\ntest result: {result}. {} passed; {failed} failed; 0 ignored; 0 measured; 0 filtered out\n",
            tests.len() - failed
        );
        if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}
