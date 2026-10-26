//! `import --change` (step 12.2b, D12.2-7): the Dockerfile instructions
//! Docker's `import` takes into an image's config: `CMD`, `ENTRYPOINT`,
//! `ENV`, `EXPOSE`, `LABEL`, `USER`, `VOLUME`, `WORKDIR`, `STOPSIGNAL`.
//! Anyone who reaches the API's socket can send them, so the parse is
//! bounded and only ever makes values for the config: nothing is run.
//!
//! The forms, as Docker's: an exec form (a JSON array) or a shell form (the
//! rest of the line, run as `["/bin/sh", "-c", …]`) for `CMD` and
//! `ENTRYPOINT`, a JSON array or words for `VOLUME`; `KEY=VALUE` pairs, or
//! the older `KEY VALUE`, for `ENV` and `LABEL` (a value quoted with `"` or
//! `'`, or with `\` escapes, as Docker's word rules).

use std::collections::{BTreeMap, BTreeSet};

/// The most changes one import takes.
pub const MAX_CHANGES: usize = 64;

/// The longest one change, in bytes.
pub const MAX_CHANGE: usize = 64 * 1024;

/// The most ports one `EXPOSE` range may name.
pub const MAX_RANGE: u32 = 1024;

/// Why a change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChangeError {
    /// An instruction `import` doesn't take (or one that isn't one).
    #[error(
        "{0:?} isn't an instruction import takes (CMD, ENTRYPOINT, ENV, EXPOSE, LABEL, USER, \
         VOLUME, WORKDIR, STOPSIGNAL)"
    )]
    Unknown(String),
    /// An instruction's value that can't be read.
    #[error("{instruction}: {why}")]
    Malformed {
        /// The instruction.
        instruction: &'static str,
        /// Why.
        why: String,
    },
    /// Past a bound: more changes, or a longer one, than an import takes.
    #[error("{0}")]
    TooLarge(String),
}

/// What the changes make of an image's config: each set field replaces the
/// config's (an import's starts empty); `env`, `labels`, `exposed` and
/// `volumes` gather, a later `KEY` replacing an earlier one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// `Cmd`.
    pub cmd: Option<Vec<String>>,
    /// `Entrypoint`.
    pub entrypoint: Option<Vec<String>>,
    /// `Env`, as `KEY=VALUE`, in order (a later one of a key replaces it).
    pub env: Vec<String>,
    /// `ExposedPorts`' keys (`80/tcp`).
    pub exposed: BTreeSet<String>,
    /// `Labels`.
    pub labels: BTreeMap<String, String>,
    /// `User`.
    pub user: Option<String>,
    /// `Volumes`' keys.
    pub volumes: BTreeSet<String>,
    /// `WorkingDir`.
    pub workdir: Option<String>,
    /// `StopSignal`.
    pub stop_signal: Option<String>,
}

impl Changes {
    /// Reads `changes`, each one instruction (`CMD ["sh"]`).
    ///
    /// # Errors
    ///
    /// More than [`MAX_CHANGES`], one past [`MAX_CHANGE`] bytes
    /// ([`ChangeError::TooLarge`]); an instruction `import` doesn't take
    /// ([`ChangeError::Unknown`]); a value that can't be read
    /// ([`ChangeError::Malformed`]).
    pub fn parse(changes: &[String]) -> Result<Self, ChangeError> {
        if changes.len() > MAX_CHANGES {
            return Err(ChangeError::TooLarge(format!(
                "more than {MAX_CHANGES} changes"
            )));
        }
        let mut made = Self::default();
        for change in changes {
            if change.len() > MAX_CHANGE {
                return Err(ChangeError::TooLarge(format!(
                    "a change of more than {MAX_CHANGE} bytes"
                )));
            }
            made.apply(change.trim())?;
        }
        Ok(made)
    }

    /// Applies one change.
    fn apply(&mut self, change: &str) -> Result<(), ChangeError> {
        let (word, rest) = change
            .split_once(char::is_whitespace)
            .map_or((change, ""), |(word, rest)| (word, rest.trim()));
        match word.to_ascii_uppercase().as_str() {
            "CMD" => self.cmd = Some(command("CMD", rest)?),
            "ENTRYPOINT" => self.entrypoint = Some(command("ENTRYPOINT", rest)?),
            "ENV" => {
                for (key, value) in pairs("ENV", rest)? {
                    let line = format!("{key}={value}");
                    let prefix = format!("{key}=");
                    self.env.retain(|had| !had.starts_with(&prefix));
                    self.env.push(line);
                }
            }
            "LABEL" => self.labels.extend(pairs("LABEL", rest)?),
            "EXPOSE" => {
                for port in words("EXPOSE", rest)? {
                    self.exposed.extend(exposed(&port)?);
                }
            }
            "USER" => self.user = Some(one("USER", rest)?),
            "WORKDIR" => self.workdir = Some(one("WORKDIR", rest)?),
            "STOPSIGNAL" => self.stop_signal = Some(one("STOPSIGNAL", rest)?),
            "VOLUME" => {
                let volumes = if rest.starts_with('[') {
                    json_array("VOLUME", rest)?
                } else {
                    words("VOLUME", rest)?
                };
                if volumes.is_empty() {
                    return Err(malformed("VOLUME", "no path given"));
                }
                self.volumes.extend(volumes);
            }
            _ => return Err(ChangeError::Unknown(word.to_owned())),
        }
        Ok(())
    }
}

/// A malformed `instruction`'s error.
fn malformed(instruction: &'static str, why: impl Into<String>) -> ChangeError {
    ChangeError::Malformed {
        instruction,
        why: why.into(),
    }
}

/// `CMD`'s or `ENTRYPOINT`'s value: a JSON array as it is, else the line run
/// by `/bin/sh -c` (Docker's shell form).
fn command(instruction: &'static str, rest: &str) -> Result<Vec<String>, ChangeError> {
    if rest.is_empty() {
        return Err(malformed(instruction, "no command given"));
    }
    if rest.starts_with('[') {
        // A line that only looks like JSON is the shell's, as Docker's.
        if let Ok(array) = json_array(instruction, rest) {
            return Ok(array);
        }
    }
    Ok(vec!["/bin/sh".to_owned(), "-c".to_owned(), rest.to_owned()])
}

/// A JSON array of strings.
fn json_array(instruction: &'static str, text: &str) -> Result<Vec<String>, ChangeError> {
    serde_json::from_str::<Vec<String>>(text)
        .map_err(|error| malformed(instruction, format!("not a JSON array of strings: {error}")))
}

/// One word (`USER app`, `WORKDIR /srv`): the rest of the line, unquoted.
fn one(instruction: &'static str, rest: &str) -> Result<String, ChangeError> {
    let mut words = words(instruction, rest)?;
    match (words.pop(), words.is_empty()) {
        (Some(word), true) => Ok(word),
        (None, _) => Err(malformed(instruction, "no value given")),
        (Some(_), false) => Err(malformed(instruction, "one value only")),
    }
}

/// `KEY=VALUE` pairs, or the older `KEY VALUE` (the rest of the line, one
/// pair).
fn pairs(instruction: &'static str, rest: &str) -> Result<Vec<(String, String)>, ChangeError> {
    let words = words(instruction, rest)?;
    let Some(first) = words.first() else {
        return Err(malformed(instruction, "no KEY=VALUE given"));
    };
    if !first.contains('=') {
        // `ENV KEY VALUE`: the key, and the rest of the line as it is (a
        // key quoted there would be taken with its quotes: refused).
        if rest.starts_with(['"', '\'']) {
            return Err(malformed(instruction, "a quoted key without =VALUE"));
        }
        let (key, value) = rest
            .split_once(char::is_whitespace)
            .map(|(key, value)| (key, value.trim()))
            .filter(|(_, value)| !value.is_empty())
            .ok_or_else(|| malformed(instruction, format!("{first} has no value")))?;
        return Ok(vec![(key.to_owned(), value.to_owned())]);
    }
    words
        .into_iter()
        .map(|word| match word.split_once('=') {
            Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
            _ => Err(malformed(instruction, format!("{word:?} isn't KEY=VALUE"))),
        })
        .collect()
}

/// The words of `text`, as Docker's word rules split them: whitespace
/// between words; `"…"` and `'…'` quote (a `\` escapes in `"…"` and
/// outside quotes).
fn words(instruction: &'static str, text: &str) -> Result<Vec<String>, ChangeError> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let escaped = chars
                    .next()
                    .ok_or_else(|| malformed(instruction, "a \\ at the end"))?;
                word.push(escaped);
                in_word = true;
            }
            '"' | '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some(end) if end == c => break,
                        Some('\\') if c == '"' => word.push(
                            chars
                                .next()
                                .ok_or_else(|| malformed(instruction, "a \\ at the end"))?,
                        ),
                        Some(inside) => word.push(inside),
                        None => return Err(malformed(instruction, format!("a {c} not closed"))),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// An `EXPOSE` word's ports: `80`, `53/udp`, `8000-8010/tcp` (at most
/// [`MAX_RANGE`] ports), as `ExposedPorts`' keys.
fn exposed(word: &str) -> Result<Vec<String>, ChangeError> {
    let (ports, protocol) = word.split_once('/').unwrap_or((word, "tcp"));
    let protocol = protocol.to_ascii_lowercase();
    if !matches!(protocol.as_str(), "tcp" | "udp" | "sctp") {
        return Err(malformed(
            "EXPOSE",
            format!("{word:?}: no protocol {protocol:?}"),
        ));
    }
    let port = |text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| malformed("EXPOSE", format!("{word:?} isn't a port")))
    };
    let (first, last) = match ports.split_once('-') {
        Some((first, last)) => (port(first)?, port(last)?),
        None => (port(ports)?, port(ports)?),
    };
    if last < first {
        return Err(malformed(
            "EXPOSE",
            format!("{word:?}: a range that ends before it starts"),
        ));
    }
    if u32::from(last - first) >= MAX_RANGE {
        return Err(malformed(
            "EXPOSE",
            format!("{word:?}: more than {MAX_RANGE} ports in one range"),
        ));
    }
    Ok((first..=last)
        .map(|port| format!("{port}/{protocol}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(changes: &[&str]) -> Result<Changes, ChangeError> {
        Changes::parse(&changes.iter().map(|&c| c.to_owned()).collect::<Vec<_>>())
    }

    /// Each instruction into its field, as Docker's import makes them: the
    /// exec and shell forms, pairs and the older `KEY VALUE`, quotes.
    #[test]
    fn each_instruction_is_docker_s() {
        let made = parsed(&[
            r#"CMD ["/bin/app", "--serve"]"#,
            "ENTRYPOINT echo hi && sleep 1",
            r#"ENV A=1 B="two words" C=it\'s"#,
            "ENV LEGACY a whole line",
            "env A=again",
            r#"LABEL org.example="x y" z=1"#,
            "EXPOSE 80 53/udp 8000-8002/TCP",
            "USER app:app",
            "WORKDIR /srv",
            r#"VOLUME ["/data", "/logs"]"#,
            "VOLUME /cache",
            "STOPSIGNAL SIGINT",
        ])
        .unwrap();
        assert_eq!(made.cmd.unwrap(), ["/bin/app", "--serve"]);
        assert_eq!(
            made.entrypoint.unwrap(),
            ["/bin/sh", "-c", "echo hi && sleep 1"]
        );
        assert_eq!(
            made.env,
            ["B=two words", "C=it's", "LEGACY=a whole line", "A=again"]
        );
        assert_eq!(made.labels["org.example"], "x y");
        assert_eq!(made.labels["z"], "1");
        assert_eq!(
            made.exposed.into_iter().collect::<Vec<_>>(),
            ["53/udp", "80/tcp", "8000/tcp", "8001/tcp", "8002/tcp"]
        );
        assert_eq!(made.user.as_deref(), Some("app:app"));
        assert_eq!(made.workdir.as_deref(), Some("/srv"));
        assert_eq!(
            made.volumes.into_iter().collect::<Vec<_>>(),
            ["/cache", "/data", "/logs"]
        );
        assert_eq!(made.stop_signal.as_deref(), Some("SIGINT"));
    }

    /// What `import` doesn't take is refused by kind: another instruction
    /// (`RUN`, `ONBUILD`, `HEALTHCHECK`), a value that can't be read, a
    /// range too wide, too many or too long. The controls: each bound's
    /// edge is taken.
    #[test]
    fn what_import_doesnt_take_is_refused() {
        for change in ["RUN make", "ONBUILD RUN x", "HEALTHCHECK NONE", "FROM x"] {
            assert!(
                matches!(parsed(&[change]), Err(ChangeError::Unknown(_))),
                "{change}"
            );
        }
        for change in [
            "CMD",
            "ENV KEY",
            r#"ENV "A B" c"#,
            "ENV =x",
            "LABEL",
            "USER a b",
            "WORKDIR",
            "VOLUME",
            "VOLUME [1]",
            "EXPOSE 0",
            "EXPOSE 70000",
            "EXPOSE 80/http",
            "EXPOSE 90-80",
            "EXPOSE 1-1025",
            r#"ENV A="open"#,
            r"ENV A=x\",
        ] {
            assert!(
                matches!(parsed(&[change]), Err(ChangeError::Malformed { .. })),
                "{change}"
            );
        }
        assert!(parsed(&["EXPOSE 1-1024"]).is_ok(), "the range's bound");
        let many: Vec<&str> = std::iter::repeat_n("USER a", MAX_CHANGES).collect();
        assert!(parsed(&many).is_ok(), "the count's bound");
        let more: Vec<&str> = std::iter::repeat_n("USER a", MAX_CHANGES + 1).collect();
        assert!(matches!(parsed(&more), Err(ChangeError::TooLarge(_))));
        let long = format!("USER {}", "a".repeat(MAX_CHANGE));
        assert!(matches!(
            parsed(&[long.as_str()]),
            Err(ChangeError::TooLarge(_))
        ));
        // A shell form that only looks like JSON is the shell's.
        assert_eq!(
            parsed(&["CMD [not json"]).unwrap().cmd.unwrap(),
            ["/bin/sh", "-c", "[not json"]
        );
    }
}
