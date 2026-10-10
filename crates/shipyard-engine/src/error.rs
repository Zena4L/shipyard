//! The engine's errors (H6: tests match the variant, not the wording).

/// Why a run's standard input and output can't be set up as asked (see
/// [`streams::plan`](crate::streams::plan)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StreamsError {
    /// `-it` asks to type into a terminal, and stdin isn't one. Docker's
    /// words, which scripts already look for.
    #[error(
        "the input device is not a TTY (-it needs a terminal to type on; use -i alone to pipe input in)"
    )]
    InputNotATerminal,
    /// `-t` with an OCI bundle: the bundle says whether it wants a terminal
    /// (`process.terminal` in its `config.json`).
    #[error("-t: a bundle says whether it wants a terminal, in its config.json (process.terminal)")]
    TerminalFromBundle,
}
