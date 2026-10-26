//! Printing for people: tables, text from outside made safe for a
//! terminal, prompts, byte counts, and one action over many names.

use std::borrow::Cow;
use std::io::{BufRead, Write};
use std::process::ExitCode;

use anyhow::Result;

use crate::broken_pipe;
use crate::reporter::{EXIT_FAILURE, report_error};

/// Writes `rows` under `header`, in columns as wide as their widest cell,
/// three spaces apart (the last one unpadded).
pub(crate) fn table(
    out: &mut impl Write,
    header: &[&str],
    rows: &[Vec<String>],
) -> std::io::Result<()> {
    let rows: Vec<Vec<Cow<'_, str>>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| printable(cell)).collect())
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |out: &mut dyn Write, cells: &mut dyn Iterator<Item = &str>| {
        use std::fmt::Write as _;
        let mut text = String::new();
        for (index, (cell, width)) in cells.zip(&widths).enumerate() {
            if index + 1 < widths.len() {
                let _ = write!(text, "{cell:<width$}   ");
            } else {
                text.push_str(cell);
            }
        }
        writeln!(out, "{}", text.trim_end())
    };
    line(out, &mut header.iter().copied())?;
    for row in &rows {
        line(out, &mut row.iter().map(AsRef::as_ref))?;
    }
    Ok(())
}

/// `text` with what a terminal would act on escaped (`\u{1b}`), so a name,
/// a command or a label is printed, never sent: control characters (C0 and
/// C1), and the bidirectional and format ones `char::is_control` misses,
/// which reorder what's shown (U+202E writes the rest right to left).
pub(crate) fn printable(text: &str) -> Cow<'_, str> {
    escaped_but(text, &[])
}

/// `text` as [`printable`] has it, but for a message on stderr: its line
/// breaks and tabs kept (a setup's failure quotes its output's lines), so
/// it can only print text, never act on the terminal.
pub(crate) fn printable_message(text: &str) -> Cow<'_, str> {
    escaped_but(text, &['\n', '\t'])
}

/// Whether a terminal would act on `c` rather than show it: a control
/// character (C0 and C1), or a bidirectional or format one.
pub(crate) fn acts_on_terminal(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// `text` with what a terminal acts on escaped, except the characters in
/// `kept`.
fn escaped_but<'a>(text: &'a str, kept: &[char]) -> Cow<'a, str> {
    let escaped = |c: char| acts_on_terminal(c) && !kept.contains(&c);
    if !text.chars().any(escaped) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if escaped(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Does `act` for each of `names`, reporting each failure and going on;
/// exits 1 if any failed. Its output's reader gone stops it, quietly, as
/// `fail` does: the rest are left as they are, and that isn't a failure.
pub(crate) fn each(names: &[String], mut act: impl FnMut(&str) -> Result<()>) -> ExitCode {
    let mut code = ExitCode::SUCCESS;
    for name in names {
        if let Err(err) = act(name) {
            if broken_pipe(&err) {
                break;
            }
            code = report_error(&err, EXIT_FAILURE);
        }
    }
    code
}

/// Asks `question` on `output`, `[y/N]`, and reads the answer from `input`:
/// whether it's yes (no, unless it says so).
pub(crate) fn confirm(
    question: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    write!(output, "{question} [y/N] ")?;
    output.flush()?;
    // Bytes, not a `String`: an answer that isn't UTF-8 is a no, not an
    // error.
    let mut answer = Vec::new();
    input.read_until(b'\n', &mut answer)?;
    Ok(matches!(
        answer.trim_ascii().to_ascii_lowercase().as_slice(),
        b"y" | b"yes"
    ))
}

/// Each of `references` as `view` shows it, as one JSON array (the ones
/// found: each that isn't is said); exits 1 if any wasn't. (`inspect`'s
/// and `image inspect`'s, as Docker's.)
pub(crate) fn shown(
    references: &[String],
    format: Option<&str>,
    mut view: impl FnMut(&str) -> Result<serde_json::Value>,
) -> Result<ExitCode> {
    let mut found = Vec::new();
    let code = each(references, |reference| {
        found.push(view(reference)?);
        Ok(())
    });
    crate::format::objects(&mut std::io::stdout().lock(), format, &found)?;
    Ok(code)
}

/// The refusal of a tar archive written to a terminal (`cp … -`, `save`,
/// `export`): `hint`, the command's own other way (`, or give -o FILE`).
pub(crate) fn tar_to_terminal_refused(hint: &str) -> anyhow::Error {
    anyhow::anyhow!("refusing to write a tar archive to a terminal: redirect it (> file.tar){hint}")
}

/// The hints of the terminal refusals, by what each command takes instead
/// (the same on Linux and from a Mac).
pub(crate) const GIVE_OUTPUT: &str = ", or give -o FILE";
/// `load`'s.
pub(crate) const GIVE_INPUT: &str = ", or give -i FILE";
/// `import`'s.
pub(crate) const GIVE_FILE: &str = ", or give a FILE";
/// `build -o type=tar`'s.
pub(crate) const GIVE_TAR_DEST: &str = ", or give -o type=tar,dest=FILE";

/// The refusal of a tar archive read from a terminal (`load`, `import -`):
/// `hint` as [`tar_to_terminal_refused`]'s.
pub(crate) fn tar_from_terminal_refused(hint: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "refusing to read a tar archive from a terminal: redirect one in (< file.tar){hint}"
    )
}

/// Writes an archive to `path` with `write` (`save -o`, `export -o`): a
/// regular file (or a new one) is written beside it and renamed at the end
/// (as Docker's CLI does), so a failure leaves no half archive under the
/// name asked for; anything else there (a device, a FIFO, a link:
/// `/dev/null`, `/dev/stdout`) is written into as it is, never replaced (a
/// rename over `/dev/null` as root would replace the host's). The file it
/// writes is the caller's alone (0600, as Docker's `os.CreateTemp`): an
/// image or a container's files may hold secrets.
pub(crate) fn write_archive_to(
    path: &std::path::Path,
    write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let writing = || format!("writing {}", path.display());
    let there = match std::fs::symlink_metadata(path) {
        Ok(there) => Some(there),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(writing),
    };
    if there.is_some_and(|there| !there.file_type().is_file()) {
        // A link to a regular file: that file, written from its start (its
        // old bytes past the archive's end cut off).
        let to_a_file = std::fs::metadata(path).is_ok_and(|target| target.is_file());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(to_a_file)
            .open(path)
            .with_context(writing)?;
        let mut out = std::io::BufWriter::new(file);
        write(&mut out)?;
        return out.flush().with_context(writing);
    }
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?;
    let mut temporary = name.to_owned();
    temporary.push(format!(".shipyard-{}", std::process::id()));
    let temporary = path.with_file_name(temporary);
    // One left by a write killed before its end, under this PID: no live
    // process but this one has it.
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(writing)?;
        let mut out = std::io::BufWriter::new(&file);
        write(&mut out)?;
        out.flush()
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&temporary, path))
            .with_context(writing)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_written_to_a_file_is_the_caller_s_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("shipyard-archive-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // A new file, then one there before that anyone could read.
        let new = dir.join("new.tar");
        write_archive_to(&new, |out| Ok(out.write_all(b"archive")?)).unwrap();
        let had = dir.join("had.tar");
        std::fs::write(&had, b"old").unwrap();
        std::fs::set_permissions(&had, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_archive_to(&had, |out| Ok(out.write_all(b"archive")?)).unwrap();
        for path in [&new, &had] {
            assert_eq!(std::fs::read(path).unwrap(), b"archive");
            assert_eq!(mode(path), 0o600, "{}", path.display());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_prompt_means_no_unless_it_says_yes() {
        let ask = |answer: &str| {
            let mut output = Vec::new();
            let yes = confirm("Sure?", &mut answer.as_bytes(), &mut output).unwrap();
            assert_eq!(output, b"Sure? [y/N] ");
            yes
        };
        assert!(ask("y\n") && ask("YES\n") && ask(" yes "));
        assert!(!ask("\n") && !ask("n\n") && !ask("") && !ask("yep\n"));
    }

    #[test]
    fn columns_are_as_wide_as_their_widest_cell() {
        let mut out = Vec::new();
        let rows = vec![
            vec!["a-much-longer-name".to_owned(), "1".to_owned()],
            vec!["b".to_owned(), "22".to_owned()],
        ];
        table(&mut out, &["NAME", "N"], &rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "NAME                 N\na-much-longer-name   1\nb                    22\n"
        );
    }

    /// What a terminal acts on is printed, never sent (stage 9, step 9.9c):
    /// a control character, an OSC sequence, a bidirectional override; plain
    /// text is left as it is, not copied.
    #[test]
    fn printable_escapes_what_a_terminal_acts_on() {
        assert!(matches!(
            printable("web-1 ünïcode"),
            Cow::Borrowed("web-1 ünïcode")
        ));
        assert_eq!(printable("a\nb\u{7f}\u{9b}"), "a\\u{a}b\\u{7f}\\u{9b}");
        assert_eq!(
            printable("\x1b]0;owned\x07name"),
            "\\u{1b}]0;owned\\u{7}name"
        );
        assert_eq!(printable("evil\u{202e}txt.exe"), "evil\\u{202e}txt.exe");
        for c in [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{2066}', '\u{2069}',
        ] {
            assert_eq!(printable(&c.to_string()), c.escape_unicode().to_string());
        }
    }

    /// A message on stderr keeps its lines and tabs, but what a terminal
    /// acts on is escaped as in a table (a hostile registry's error, a
    /// layer's file name).
    #[test]
    fn a_message_keeps_its_lines_but_never_acts() {
        assert!(matches!(
            printable_message("setup failed:\n\tline"),
            Cow::Borrowed("setup failed:\n\tline")
        ));
        assert_eq!(
            printable_message("pull: \x1b]52;c;owned\x07\nnext\u{202e}"),
            "pull: \\u{1b}]52;c;owned\\u{7}\nnext\\u{202e}"
        );
        assert_eq!(printable("a\tb"), "a\\u{9}b");
    }

    /// A table's cells are printed, never sent, and its columns are as wide
    /// as what's shown.
    #[test]
    fn table_cells_are_printable() {
        let mut out = Vec::new();
        let rows = vec![
            vec!["\x1b[2J".to_owned(), "1".to_owned()],
            vec!["b".to_owned(), "2".to_owned()],
        ];
        table(&mut out, &["NAME", "N"], &rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "NAME        N\n\\u{1b}[2J   1\nb           2\n"
        );
    }
}
