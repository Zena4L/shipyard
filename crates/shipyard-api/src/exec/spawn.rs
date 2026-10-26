//! An exec's child: `shipyard exec` of this very binary, its command line and
//! its standard streams (a pseudo-terminal of its own, or pipes), and the file
//! its command's PID lands in.

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use shipyard_runtime::terminal::{Size, host_pty};

use super::{ExecConfig, Instance, KEPT, PID_FILE_MAX};

/// The child `shipyard exec`'s argv (after the program) and its
/// `SHIPYARD_CALLER_ENV`: each variable's key on the argv (`--env=KEY`),
/// its value only in the environment. An entry without `=` names no value
/// (Docker's client resolves those itself), so it's left out. The command's
/// PID goes to `pid_file` once it runs. The container and the command come
/// after `--`, so neither is read as an option.
pub(crate) fn child_command(
    container: &str,
    config: &ExecConfig,
    pid_file: Option<&std::path::Path>,
) -> (Vec<OsString>, String) {
    let mut args: Vec<OsString> = vec!["exec".into()];
    if config.stdin {
        args.push("--interactive".into());
    }
    if config.tty {
        args.push("--tty".into());
    }
    if let Some(user) = &config.user {
        args.push(format!("--user={user}").into());
    }
    if let Some(workdir) = &config.workdir {
        args.push(format!("--workdir={workdir}").into());
    }
    let mut values = Vec::new();
    for (key, value) in config.env.iter().filter_map(|entry| entry.split_once('=')) {
        if key.is_empty() {
            continue;
        }
        args.push(format!("--env={key}").into());
        values.push((key.to_owned(), value.to_owned()));
    }
    if let Some(file) = pid_file {
        let mut arg = OsString::from("--pid-file=");
        arg.push(file);
        args.push(arg);
    }
    args.push("--".into());
    args.push(container.into());
    args.extend(config.cmd.iter().map(OsString::from));
    let caller_env = shipyard_runtime::attach::forward::caller_env_value(&values);
    (args, caller_env)
}

/// How an exec's streams are made, decided before it's spawned.
#[derive(Debug, Clone, Copy)]
pub(super) enum Mode {
    /// A terminal of its own, this big.
    Terminal(Size),
    /// Pipes.
    Pipes,
    /// None: detached.
    Detached,
}

/// A running exec's streams.
#[derive(Debug)]
pub(super) enum Io {
    /// A terminal: its master, both ways.
    Terminal(Arc<OwnedFd>),
    /// Pipes: its input (if attached), stdout and stderr.
    Pipes {
        stdin: Option<std::process::ChildStdin>,
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
    },
    /// None: detached.
    Detached,
}

/// Spawns the exec's child, its streams made as `mode` says: it and its
/// streams. Linux only: the child is this binary, as `/proc/self/exe`.
pub(super) fn spawn(instance: &Instance, mode: Mode) -> std::io::Result<(Child, Io)> {
    use std::os::unix::process::CommandExt as _;
    if cfg!(not(target_os = "linux")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "containers need Linux",
        ));
    }
    let (args, caller_env) = child_command(
        &instance.container,
        &instance.config,
        instance.pid_file.as_deref(),
    );
    let mut command = Command::new("/proc/self/exe");
    command
        .args(&args)
        .env(shipyard_runtime::attach::forward::CALLER_ENV, caller_env);
    let mut master = None;
    match mode {
        Mode::Terminal(size) => {
            let (primary, slave) = host_pty(size).map_err(std::io::Error::from)?;
            // A transport, not a user's terminal: raw, so the bytes pass as
            // they are (no `\r\n` for `\n`, no echo, no ^C as a signal); the
            // child sets the container's own terminal as the command asks.
            let mut settings = nix::sys::termios::tcgetattr(&slave)?;
            nix::sys::termios::cfmakeraw(&mut settings);
            nix::sys::termios::tcsetattr(&slave, nix::sys::termios::SetArg::TCSANOW, &settings)?;
            command
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            // SAFETY: between the fork and the exec, two system calls only
            // (`setsid`, `ioctl`): async-signal-safe, no allocation, no lock.
            unsafe {
                command.pre_exec(shipyard_runtime::terminal::take_controlling_terminal);
            }
            master = Some(Arc::new(primary));
        }
        Mode::Pipes => {
            let stdin = if instance.config.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            command
                .stdin(stdin)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
        }
        Mode::Detached => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
        }
    }
    let mut child = command.spawn()?;
    let io = match (mode, &master) {
        (Mode::Terminal(_), Some(master)) => Io::Terminal(Arc::clone(master)),
        (Mode::Pipes, _) => {
            let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
                // (Made piped above: never.) Its child ended here, never
                // left running when its start is given back.
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::ErrorKind::BrokenPipe.into());
            };
            Io::Pipes {
                stdin: child.stdin.take(),
                stdout,
                stderr,
            }
        }
        _ => Io::Detached,
    };
    let mut state = instance.state();
    state.running = true;
    state.master = master;
    Ok((child, io))
}

/// Removes the PID files execs left in the runtime's state root (`dir`),
/// and their writes' temporaries, older than [`KEPT`]: one written after its
/// service had gone (its exec's instance with it), or cut short. A live
/// one is read within a look: another service's (root's tests share the
/// root) is never taken.
pub(crate) fn sweep_pid_files(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(".exec-") || name.starts_with("..exec-");
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= KEPT));
        if ours && old && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The command's PID, from its PID file once the child has written it (a
/// file whole or not at all): kept, and the file removed.
pub(super) fn read_pid(instance: &Instance) {
    let Some(file) = &instance.pid_file else {
        return;
    };
    if instance.state().pid.is_some() {
        return;
    }
    let pid = shipyard_sys::fs::read_bounded(file, PID_FILE_MAX)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        instance.state().pid = Some(pid);
        let _ = std::fs::remove_file(file);
    }
}
