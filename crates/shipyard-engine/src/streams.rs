//! What a run's command gets as its standard input, output and error:
//! Docker's `-i` and `-t`, and what Shipyard decides for a container on a
//! terminal (stage 8, steps 8.2a and 8.2b).
//!
//! **A container never holds the user's terminal (D16).** A process that
//! holds a terminal descriptor can type into it as if the user had
//! (`TIOCSTI`, where the kernel still allows it), or leave it raw. So where
//! an isolated command's standard descriptor would be a terminal, it gets a
//! pipe that Shipyard relays instead. Files and pipes are passed on as they
//! are: they can't be used that way.
//!
//! **Input only when asked for (D17).** Without `-i`, the command's stdin
//! is `/dev/null`, as Docker's is: it can't take keys meant for the shell.
//!
//! **Without isolation** the command is the user's own process, not a
//! container: it keeps Shipyard's descriptors, terminal and all.

use std::io::IsTerminal;

use shipyard_runtime::process::{Pty, Stdio, Stream};

use crate::StreamsError;

/// How the run's command runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Runs {
    /// On the host, as the user's own process (`--no-isolation`; macOS).
    #[default]
    Unisolated,
    /// In a container.
    Isolated,
    /// In a container from an OCI bundle, which asks for a terminal in its
    /// own `config.json`.
    Bundle,
}

/// What the run asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Request {
    /// `-t`: a pseudo-terminal of the container's own.
    pub tty: bool,
    /// `-i`: keep stdin open, and pass it on.
    pub interactive: bool,
    /// Where the command runs.
    pub runs: Runs,
}

/// Which of Shipyard's own standard descriptors are terminals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Terminals {
    /// Descriptor 0.
    pub stdin: bool,
    /// Descriptor 1.
    pub stdout: bool,
    /// Descriptor 2.
    pub stderr: bool,
}

impl Terminals {
    /// This process's own.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
            stderr: std::io::stderr().is_terminal(),
        }
    }
}

/// The command's standard descriptors for `request`, given which of
/// Shipyard's own are terminals.
///
/// # Errors
///
/// [`StreamsError::InputNotATerminal`] for `-it` when stdin isn't a
/// terminal (Docker's rule: there'd be nothing to type with), and
/// [`StreamsError::TerminalFromBundle`] for `-t` with a bundle.
pub fn plan(request: Request, host: Terminals) -> Result<Stdio, StreamsError> {
    let passed_on = |terminal: bool| {
        if terminal {
            Stream::Relay
        } else {
            Stream::Inherit
        }
    };
    if request.tty {
        if request.interactive && !host.stdin {
            return Err(StreamsError::InputNotATerminal);
        }
        // `-t`: a terminal of the command's own takes its 0, 1 and 2; the
        // user's terminal goes raw with `-i` (a terminal, as just checked),
        // so every key reaches it.
        let terminal = Some(Pty {
            input: request.interactive,
            raw: request.interactive,
        });
        return Ok(match request.runs {
            Runs::Unisolated => Stdio {
                terminal,
                ..Stdio::default()
            },
            // Before the command takes its terminal, the container's setup
            // (its init's messages) still never holds the user's: D16.
            Runs::Isolated => Stdio {
                stdin: Stream::Null,
                stdout: passed_on(host.stdout),
                stderr: passed_on(host.stderr),
                terminal,
            },
            Runs::Bundle => return Err(StreamsError::TerminalFromBundle),
        });
    }
    if request.runs == Runs::Unisolated {
        return Ok(Stdio::default());
    }
    Ok(Stdio {
        stdin: if request.interactive {
            passed_on(host.stdin)
        } else {
            Stream::Null
        },
        stdout: passed_on(host.stdout),
        stderr: passed_on(host.stderr),
        terminal: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL: Terminals = Terminals {
        stdin: true,
        stdout: true,
        stderr: true,
    };
    const PIPES: Terminals = Terminals {
        stdin: false,
        stdout: false,
        stderr: false,
    };

    fn isolated(interactive: bool) -> Request {
        Request {
            interactive,
            runs: Runs::Isolated,
            ..Request::default()
        }
    }

    #[test]
    fn without_isolation_everything_is_passed_on() {
        for host in [TERMINAL, PIPES] {
            for interactive in [false, true] {
                let request = Request {
                    interactive,
                    ..Request::default()
                };
                assert_eq!(plan(request, host), Ok(Stdio::default()));
            }
        }
    }

    #[test]
    fn a_container_on_a_terminal_gets_pipes_and_no_input_without_i() {
        assert_eq!(
            plan(isolated(false), TERMINAL),
            Ok(Stdio {
                stdin: Stream::Null,
                stdout: Stream::Relay,
                stderr: Stream::Relay,
                terminal: None,
            })
        );
    }

    #[test]
    fn with_i_a_terminal_stdin_is_relayed() {
        assert_eq!(
            plan(isolated(true), TERMINAL),
            Ok(Stdio {
                stdin: Stream::Relay,
                stdout: Stream::Relay,
                stderr: Stream::Relay,
                terminal: None,
            })
        );
    }

    #[test]
    fn files_and_pipes_are_passed_on_as_they_are() {
        assert_eq!(
            plan(isolated(true), PIPES),
            Ok(Stdio {
                stdin: Stream::Inherit,
                stdout: Stream::Inherit,
                stderr: Stream::Inherit,
                terminal: None,
            })
        );
        assert_eq!(
            plan(isolated(false), PIPES),
            Ok(Stdio {
                stdin: Stream::Null,
                stdout: Stream::Inherit,
                stderr: Stream::Inherit,
                terminal: None,
            })
        );
        // Each descriptor on its own: output to a file, errors to the
        // terminal.
        let mixed = Terminals {
            stdin: true,
            stdout: false,
            stderr: true,
        };
        assert_eq!(
            plan(isolated(true), mixed),
            Ok(Stdio {
                stdin: Stream::Relay,
                stdout: Stream::Inherit,
                stderr: Stream::Relay,
                terminal: None,
            })
        );
    }

    #[test]
    fn it_needs_a_terminal_to_type_on() {
        let request = Request {
            tty: true,
            ..isolated(true)
        };
        assert_eq!(plan(request, PIPES), Err(StreamsError::InputNotATerminal));
        // `-t` alone with piped input is Docker's to allow.
        let request = Request {
            tty: true,
            ..isolated(false)
        };
        assert!(plan(request, PIPES).is_ok());
    }

    #[test]
    fn a_bundle_asks_for_its_terminal_itself() {
        let request = Request {
            tty: true,
            runs: Runs::Bundle,
            ..isolated(false)
        };
        assert_eq!(
            plan(request, TERMINAL),
            Err(StreamsError::TerminalFromBundle)
        );
    }

    #[test]
    fn t_gives_the_command_a_terminal_of_its_own() {
        let it = Request {
            tty: true,
            ..isolated(true)
        };
        assert_eq!(
            plan(it, TERMINAL),
            Ok(Stdio {
                stdin: Stream::Null,
                stdout: Stream::Relay,
                stderr: Stream::Relay,
                terminal: Some(Pty {
                    input: true,
                    raw: true,
                }),
            })
        );
        // `-t` alone, output to a pipe: a terminal, no input, nothing raw.
        let t = Request {
            tty: true,
            ..isolated(false)
        };
        assert_eq!(
            plan(t, PIPES),
            Ok(Stdio {
                stdin: Stream::Null,
                stdout: Stream::Inherit,
                stderr: Stream::Inherit,
                terminal: Some(Pty::default()),
            })
        );
        // Without isolation: the user's own process keeps their streams
        // until it takes its terminal.
        let unisolated = Request {
            tty: true,
            interactive: true,
            runs: Runs::Unisolated,
        };
        assert_eq!(
            plan(unisolated, TERMINAL),
            Ok(Stdio {
                terminal: Some(Pty {
                    input: true,
                    raw: true,
                }),
                ..Stdio::default()
            })
        );
    }
}
