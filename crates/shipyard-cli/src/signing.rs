//! `shipyard push`, `login` and `logout` (step 12.4): Docker's flags and
//! words, the engine's work.
//!
//! `login`'s password is never echoed: on a terminal it's read with the
//! terminal's echo off, and its signal keys off too (`ISIG`), so a Ctrl-C
//! or Ctrl-Z comes as a byte that cancels the prompt (the terminal put back
//! first), never a signal that would leave it without echo.

use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use shipyard_engine::image::{Credential, GivenCredential, Kept};

use crate::reporter::{CliReporter, engine_with};

/// The most `--password-stdin` reads.
const MAX_PASSWORD: u64 = 4096;

/// The most a handed credential is read as.
const MAX_HANDED: u64 = 16 * 1024;

/// The credential this command was handed (a Mac's, for a forwarded
/// `push` or `pull`: D12.4-2), if it was: read whole from its descriptor,
/// which is closed, and the variable naming it removed, before anything
/// else runs.
///
/// # Safety
///
/// Called while this process has one thread (first thing in `main`, after
/// the entry points: the variable is removed), and before anything has
/// opened a descriptor that could be the one it takes.
pub(crate) unsafe fn take_handed() -> Result<Option<GivenCredential>> {
    use shipyard_runtime::attach::forward::{REGISTRY_AUTH_FD, REGISTRY_AUTH_FD_ENV};
    let Some(named) = std::env::var_os(REGISTRY_AUTH_FD_ENV) else {
        return Ok(None);
    };
    // SAFETY: one thread (the caller's promise): nothing reads the
    // environment meanwhile.
    unsafe { std::env::remove_var(REGISTRY_AUTH_FD_ENV) };
    if named.to_str() != Some(&REGISTRY_AUTH_FD.to_string()) {
        bail!("{REGISTRY_AUTH_FD_ENV} names descriptor {REGISTRY_AUTH_FD} or nothing");
    }
    // SAFETY: left for this process to take (the caller's promise), taken
    // once.
    let handed = unsafe {
        shipyard_sys::fd::take_inherited(REGISTRY_AUTH_FD, shipyard_sys::fd::OnExec::Close)
    }
    .context("taking the credential handed to this command")?;
    let mut text = String::new();
    std::fs::File::from(handed)
        .take(MAX_HANDED)
        .read_to_string(&mut text)
        .context("reading the credential handed to this command")?;
    GivenCredential::from_docker_auth(&text).context("the credential handed to this command")
}

/// `push`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct PushArgs {
    /// Push every tag of the repository the store has.
    #[arg(short, long = "all-tags")]
    pub(crate) all_tags: bool,
    /// Print only each name pushed (no progress).
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// The image: `NAME[:TAG]` (`:latest` if none), as `tag` named it for
    /// the registry (`ghcr.io/owner/app:1`).
    #[arg(value_name = "NAME[:TAG]")]
    pub(crate) name: String,
}

/// `login`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct LoginArgs {
    /// The username (asked on the terminal if not given).
    #[arg(short, long)]
    pub(crate) username: Option<String>,
    /// The password: seen by anyone who can list processes, and kept in
    /// the shell's history (use --password-stdin).
    #[arg(short, long, conflicts_with = "password_stdin")]
    pub(crate) password: Option<String>,
    /// Read the password (or token) from standard input.
    #[arg(long)]
    pub(crate) password_stdin: bool,
    /// The registry (Docker Hub if none): `ghcr.io`, `registry.example:5000`.
    #[arg(value_name = "SERVER")]
    pub(crate) server: Option<String>,
}

/// `shipyard push`: each name pushed, its progress on stderr (unless
/// `-q`), Docker's last line for each (`TAG: digest: … size: …`).
pub(crate) fn push(args: &PushArgs, given: Option<GivenCredential>) -> Result<ExitCode> {
    let engine = engine_with(CliReporter {
        progress: !args.quiet,
    });
    let pushed = shipyard_engine::image::push(
        &engine,
        &shipyard_engine::image::PushRequest {
            name: args.name.clone(),
            all_tags: args.all_tags,
            given,
        },
    )?;
    let mut out = std::io::stdout().lock();
    for name in pushed {
        if args.quiet {
            writeln!(out, "{}", crate::output::printable(&name.name))?;
        } else {
            writeln!(
                out,
                "{}: digest: {} size: {}",
                crate::output::printable(&name.tag),
                name.digest,
                name.size
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard login`: the credential checked against the registry, then
/// kept where Docker keeps it; "Login Succeeded", as Docker's.
pub(crate) fn login(args: &LoginArgs) -> Result<ExitCode> {
    let server = args
        .server
        .clone()
        .filter(|server| !server.trim().is_empty());
    let username = match &args.username {
        Some(username) => username.clone(),
        None if args.password_stdin => bail!("--password-stdin needs --username"),
        None => ask("Username: ", true)?,
    };
    if username.is_empty() {
        bail!("a username is needed");
    }
    let password = if let Some(password) = &args.password {
        crate::reporter::warn(
            "a password on the command line is seen by anyone listing processes, and kept in \
             the shell's history: use --password-stdin"
                .to_owned(),
        )?;
        password.clone()
    } else if args.password_stdin {
        password_from_stdin()?
    } else {
        ask("Password: ", false)?
    };
    if password.is_empty() {
        bail!("a password (or token) is needed");
    }
    let credential = Credential::Basic { username, password };
    let kept = shipyard_engine::image::login_and_keep(GivenCredential {
        credential,
        registry: server,
    })?;
    match kept {
        shipyard_engine::image::Signed::Refused(why) => {
            let mut err = std::io::stderr().lock();
            writeln!(err, "shipyard: {}", crate::output::printable_message(&why))?;
            Ok(ExitCode::from(crate::reporter::EXIT_FAILURE))
        }
        shipyard_engine::image::Signed::Kept(kept) => {
            if let Kept::File(path) = kept {
                crate::reporter::warn(format!(
                    "your credentials are stored unencrypted in '{}': a credential helper keeps \
                     them safer (credsStore in that file)",
                    path.display()
                ))?;
            }
            writeln!(std::io::stdout().lock(), "Login Succeeded")?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `shipyard logout [SERVER]`: the sign-in removed, as Docker's says it.
pub(crate) fn logout(server: Option<&str>) -> Result<ExitCode> {
    let shown = server.unwrap_or(shipyard_engine::image::DOCKER_HUB_NAME);
    let forgotten = shipyard_engine::image::logout(server)?;
    if let Some(why) = &forgotten.helper_failed {
        crate::reporter::warn(format!("couldn't erase the credentials for {shown}: {why}"))?;
    }
    let mut out = std::io::stdout().lock();
    if forgotten.removed {
        writeln!(
            out,
            "Removing login credentials for {}",
            crate::output::printable(shown)
        )?;
    } else {
        writeln!(out, "Not logged in to {}", crate::output::printable(shown))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// A password from standard input: at most [`MAX_PASSWORD`] bytes, its
/// trailing line end dropped.
fn password_from_stdin() -> Result<String> {
    let mut read = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_PASSWORD + 1)
        .read_to_end(&mut read)
        .context("reading the password from standard input")?;
    if read.len() as u64 > MAX_PASSWORD {
        bail!("the password on standard input is over {MAX_PASSWORD} bytes");
    }
    let text = String::from_utf8(read).context("the password on standard input isn't UTF-8")?;
    Ok(text.trim_end_matches(['\n', '\r']).to_owned())
}

/// Asks `prompt` on the terminal (standard input and error): a line,
/// echoed (the username: the terminal as it is, a Ctrl-C ending this as any
/// program) or not (the password: [`hidden_line`]). Refused where standard
/// input isn't a terminal.
fn ask(prompt: &str, echo: bool) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("there's no terminal to ask on: give --username and --password-stdin");
    }
    let mut err = std::io::stderr().lock();
    write!(err, "{prompt}")?;
    err.flush()?;
    if echo {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_owned());
    }
    let line = {
        let _quiet = Quiet::on(&stdin)?;
        hidden_line(&mut stdin.lock())
    };
    writeln!(err)?;
    line
}

/// A line read a byte at a time from a terminal in [`Quiet`]'s mode: Enter
/// ends it, Backspace and Ctrl-U edit it; Ctrl-C, Ctrl-Z, Ctrl-\ (signals
/// off: bytes) or Ctrl-D on an empty line cancel it at once.
fn hidden_line(input: &mut impl std::io::Read) -> Result<String> {
    const CANCEL: [u8; 3] = [0x03, 0x1a, 0x1c];
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            break;
        }
        match byte[0] {
            b'\n' | b'\r' => break,
            key if CANCEL.contains(&key) => bail!("cancelled"),
            0x04 if line.is_empty() => bail!("cancelled"),
            // Backspace: the last character, whole (UTF-8's continuation
            // bytes with it).
            0x7f | 0x08 => while line.pop().is_some_and(|gone| gone & 0xc0 == 0x80) {},
            0x15 => line.clear(),
            other => {
                if line.len() as u64 >= MAX_PASSWORD {
                    bail!("the password is over {MAX_PASSWORD} bytes");
                }
                line.push(other);
            }
        }
    }
    String::from_utf8(line).context("the password isn't UTF-8")
}

/// The terminal's settings while the password is read: no echo, no line
/// editing of its own (bytes as they're typed, one at a time), its signal
/// keys read as bytes; put back when dropped.
struct Quiet<'a> {
    fd: std::os::fd::BorrowedFd<'a>,
    before: nix::sys::termios::Termios,
}

impl<'a> Quiet<'a> {
    fn on(stdin: &'a std::io::Stdin) -> Result<Self> {
        use nix::sys::termios::{
            LocalFlags, SetArg, SpecialCharacterIndices, tcgetattr, tcsetattr,
        };
        use std::os::fd::AsFd as _;
        let fd = stdin.as_fd();
        let before = tcgetattr(fd).context("reading the terminal's settings")?;
        let mut quiet = before.clone();
        quiet
            .local_flags
            .remove(LocalFlags::ISIG | LocalFlags::ECHO | LocalFlags::ICANON);
        quiet.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        quiet.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        tcsetattr(fd, SetArg::TCSAFLUSH, &quiet).context("setting the terminal's echo")?;
        Ok(Self { fd, before })
    }
}

impl Drop for Quiet<'_> {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};
        let _ = tcsetattr(self.fd, SetArg::TCSAFLUSH, &self.before);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The password's keys: typed bytes kept, Backspace takes a whole
    /// character, Ctrl-U the line, Enter ends it; Ctrl-C, Ctrl-Z, Ctrl-\
    /// and Ctrl-D on an empty line cancel it.
    #[test]
    fn a_hidden_line_s_keys() {
        let read = |bytes: &[u8]| hidden_line(&mut &bytes[..]);
        assert_eq!(read(b"pw\r").unwrap(), "pw");
        assert_eq!(read(b"pq\x7fw\n").unwrap(), "pw");
        assert_eq!(read("p\u{e9}\x7fw\n".as_bytes()).unwrap(), "pw");
        assert_eq!(read(b"wrong\x15pw\n").unwrap(), "pw");
        assert_eq!(read(b"pw").unwrap(), "pw", "its input's end");
        for cancel in [&b"p\x03w\n"[..], b"\x1a", b"\x1c", b"\x04"] {
            assert!(read(cancel).is_err(), "{cancel:?}");
        }
        assert_eq!(
            read(b"p\x04w\n").unwrap(),
            "p\u{4}w",
            "Ctrl-D mid-line is a byte"
        );
    }
}
