//! The console's two halves (step 14.5): a container's logs, read a line at
//! a time as text (its control sequences stripped: a log is shown, never fed
//! to a terminal, D14.5-2), and a shell in it, `sh` on a terminal of its own
//! (`shipyard exec -it NAME sh`, D14.5-1), whose bytes go to the window's
//! terminal as they come.

use std::pin::Pin;

use bollard::container::LogOutput;
use futures_util::{Stream, StreamExt as _};
use hyper::body::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use crate::Result;
use crate::api::{Client, bounded, container_ref};

/// How many of a container's last lines its logs start with.
pub const TAIL: usize = 500;

/// The most of one log line kept, bytes (the rest of it dropped).
pub const MAX_LINE: usize = 16 * 1024;

/// The command a shell runs: the core's, never the page's.
pub const SHELL: &str = "sh";

/// The shell's detach keys: a sequence no one types. Given none, the API
/// takes Docker's, Ctrl-P Ctrl-Q, which shells, emacs and vi users press:
/// it would end the session and leave its `sh` running, holding the
/// service (and the Mac's VM) awake.
pub const DETACH_NEVER: &str = "ctrl-^,ctrl-_,ctrl-^,ctrl-_,ctrl-^,ctrl-_,ctrl-^,ctrl-_";

/// Follows container `id`'s logs (its last [`TAIL`] lines first; with
/// `follow`, then each new one until it stops), each line, stripped and
/// bounded, handed to `on_line`.
///
/// # Errors
///
/// [`crate::ClientError::BadReference`] for an ID of another shape; the
/// API's.
pub async fn logs(
    client: &Client,
    id: &str,
    follow: bool,
    mut on_line: impl FnMut(String),
) -> Result<()> {
    let id = container_ref(id)?;
    let options = bollard::query_parameters::LogsOptionsBuilder::default()
        .follow(follow)
        .stdout(true)
        .stderr(true)
        .tail(&TAIL.to_string())
        .build();
    let mut chunks = client.docker().logs(id, Some(options));
    let mut lines = Lines::default();
    while let Some(chunk) = chunks.next().await {
        for line in lines.push(&message_of(chunk?)) {
            on_line(line);
        }
    }
    if let Some(line) = lines.rest() {
        on_line(line);
    }
    Ok(())
}

/// The bytes of a stream's chunk, whichever stream it came on.
fn message_of(chunk: LogOutput) -> Bytes {
    match chunk {
        LogOutput::StdErr { message }
        | LogOutput::StdOut { message }
        | LogOutput::StdIn { message }
        | LogOutput::Console { message } => message,
    }
}

/// Chunks made lines: a line split across chunks put back together, each
/// one bounded at [`MAX_LINE`] bytes as it grows, then stripped.
#[derive(Debug, Default)]
struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    /// The lines `chunk` ends.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut done = Vec::new();
        for piece in chunk.split_inclusive(|&byte| byte == b'\n') {
            let room = MAX_LINE.saturating_sub(self.partial.len());
            let (body, ended) = match piece.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (piece, false),
            };
            self.partial
                .extend_from_slice(&body[..body.len().min(room)]);
            if ended {
                done.push(strip_controls(&String::from_utf8_lossy(&self.partial)));
                self.partial.clear();
            }
        }
        done
    }

    /// What's left without its newline, if anything.
    fn rest(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| {
            let line = strip_controls(&String::from_utf8_lossy(&self.partial));
            self.partial.clear();
            line
        })
    }
}

/// `text` with its control sequences gone: CSI (`ESC [`, parameters and
/// intermediates, a final byte; anything else ends it), OSC and the other
/// control strings (to BEL, `ESC \` or ST), other escapes (their
/// intermediates and final byte: `ESC ( B`), C1 controls, every C0 control
/// but a tab, and the marks that reorder a line's display (bidi). What's
/// left is text alone.
#[must_use]
pub fn strip_controls(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    skip_csi(&mut chars);
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    chars.next();
                    skip_string(&mut chars);
                }
                _ => skip_escape(&mut chars),
            },
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{9f}' | '\u{9e}' | '\u{98}' => skip_string(&mut chars),
            '\t' => kept.push(c),
            c if c.is_control() || is_bidi(c) => {}
            c => kept.push(c),
        }
    }
    kept
}

/// Past a CSI: its parameters (`0x30..=0x3F`) and intermediates
/// (`0x20..=0x2F`), then its final byte; anything else ends it there,
/// left to be read as text.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if ('\u{20}'..='\u{3f}').contains(&c) {
            chars.next();
        } else {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                chars.next();
            }
            return;
        }
    }
}

/// Past another escape: its intermediates (`0x20..=0x2F`), then its final
/// byte (`0x30..=0x7E`).
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars
        .next_if(|c| ('\u{20}'..='\u{2f}').contains(c))
        .is_some()
    {}
    chars.next_if(|c| ('\u{30}'..='\u{7e}').contains(c));
}

/// Whether `c` reorders a line's display (Unicode's bidi controls).
fn is_bidi(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Past a control string's end: BEL, `ESC \` or ST.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' | '\u{9c}' => return,
            '\u{1b}' if chars.peek() == Some(&'\\') => {
                chars.next();
                return;
            }
            _ => {}
        }
    }
}

/// A shell in a container, on a terminal: its input, and its exec's ID for
/// a resize. Dropped, its connection goes, and the API hangs the shell up.
pub struct Shell {
    exec: String,
    input: Pin<Box<dyn AsyncWrite + Send>>,
    client: Client,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("exec", &self.exec)
            .finish_non_exhaustive()
    }
}

/// A shell's output, its terminal's bytes as they come.
pub type ShellOutput = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// The exec a shell is: [`SHELL`] on a terminal of `cols` × `rows`, its
/// input attached.
#[must_use]
pub fn shell_config(cols: u16, rows: u16) -> bollard::models::ExecConfig {
    bollard::models::ExecConfig {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(true),
        console_size: Some(vec![usize::from(rows), usize::from(cols)]),
        env: Some(vec!["TERM=xterm-256color".to_owned()]),
        cmd: Some(vec![SHELL.to_owned()]),
        detach_keys: Some(DETACH_NEVER.to_owned()),
        ..bollard::models::ExecConfig::default()
    }
}

impl Shell {
    /// A shell in container `id`, on a terminal of `cols` × `rows`, and
    /// its output.
    ///
    /// # Errors
    ///
    /// [`crate::ClientError::BadReference`] for an ID of another shape; the
    /// API's (the container not running is its 409).
    pub async fn open(
        client: &Client,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(Self, ShellOutput)> {
        let id = container_ref(id)?;
        let docker = client.docker();
        let exec = bounded(
            "opening a shell",
            docker.create_exec(id, shell_config(cols, rows)),
        )
        .await??
        .id;
        let options = bollard::exec::StartExecOptions {
            detach: false,
            tty: true,
            output_capacity: None,
        };
        let started = bounded("opening a shell", docker.start_exec(&exec, Some(options))).await??;
        let bollard::exec::StartExecResults::Attached { output, input } = started else {
            return Err(crate::ClientError::Http(
                "the shell started detached".to_owned(),
            ));
        };
        let output: ShellOutput = Box::pin(output.map(|chunk| Ok(message_of(chunk?))));
        let shell = Self {
            exec,
            input,
            client: client.clone(),
        };
        Ok((shell, output))
    }

    /// Its exec's ID (its inspect says whether it still runs).
    #[must_use]
    pub fn exec_id(&self) -> &str {
        &self.exec
    }

    /// Types `bytes` into it.
    ///
    /// # Errors
    ///
    /// Its connection's.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.input.write_all(bytes).await?;
        self.input.flush().await?;
        Ok(())
    }

    /// Its terminal made `cols` × `rows` (the shell gets its `SIGWINCH`).
    ///
    /// # Errors
    ///
    /// The API's.
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let options = bollard::query_parameters::ResizeExecOptionsBuilder::default()
            .w(i32::from(cols))
            .h(i32::from(rows))
            .build();
        bounded(
            "resizing a shell",
            self.client.docker().resize_exec(&self.exec, options),
        )
        .await??;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A log's colours, links, titles and other commands go; its text stays.
    #[test]
    fn a_log_is_text_alone() {
        let hostile = "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;https://x.test\u{7}link\u{1b}]8;;\u{7} \
                       \u{1b}]0;title\u{1b}\\done\u{1b}c\u{9b}2Jx\u{7}\ttab<img src=x>";
        assert_eq!(strip_controls(hostile), "red link donex\ttab<img src=x>");
        // A character set's escape, a CSI cut by a letter it can't hold,
        // and the marks that reorder a line.
        assert_eq!(strip_controls("\u{1b}(Bx\u{1b}#8y\u{1b}%Gz"), "xyz");
        assert_eq!(strip_controls("a\u{1b}[1;é rest"), "aé rest");
        assert_eq!(strip_controls("tab\u{202e}gnp.exe\u{2066}x"), "tabgnp.exex");
        // An escape cut at the end leaves nothing behind.
        assert_eq!(strip_controls("a\u{1b}[12"), "a");
        assert_eq!(strip_controls("a\u{1b}]52;c;aGk="), "a");
    }

    /// Lines come whole across chunks, each bounded, the last without its
    /// newline too.
    #[test]
    fn lines_are_whole_and_bounded() {
        let mut lines = Lines::default();
        assert!(lines.push(b"one ").is_empty());
        assert_eq!(lines.push(b"line\ntwo\nthr"), ["one line", "two"]);
        assert_eq!(lines.rest().as_deref(), Some("thr"));
        let long = vec![b'x'; MAX_LINE * 2 + 3];
        lines.push(&long[..=MAX_LINE]);
        let ended = lines.push(&[&long[MAX_LINE + 1..], b"\nok\n"].concat());
        assert_eq!(ended.len(), 2);
        assert_eq!(ended[0].len(), MAX_LINE);
        assert_eq!(ended[1], "ok");
    }

    /// The shell is the core's `sh` on a terminal, sized as asked.
    #[test]
    fn a_shell_is_sh_on_a_terminal() {
        let config = shell_config(120, 40);
        assert_eq!(config.cmd, Some(vec!["sh".to_owned()]));
        assert_eq!(config.tty, Some(true));
        assert_eq!(config.attach_stdin, Some(true));
        assert_eq!(config.console_size, Some(vec![40, 120]));
        // Never Docker's Ctrl-P Ctrl-Q: a shell would detach, its `sh` left.
        assert_eq!(config.detach_keys.as_deref(), Some(DETACH_NEVER));
        assert!(!DETACH_NEVER.contains("ctrl-p"));
    }
}
