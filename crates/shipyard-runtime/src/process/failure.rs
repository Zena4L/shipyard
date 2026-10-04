//! What can go wrong between `fork` and `exec`, and how the parent explains it.
//!
//! The child can only report a step number, an errno and one number of detail
//! (it can't allocate after fork). The parent turns that into a message, using
//! what it knows about the spec and, for exec failures, a look at the files.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;

use super::rlimit::Rlimit;
use super::status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND};

/// The child's setup steps, in order. The numbers travel over the report pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum ChildStep {
    ResetSignals = 1,
    CloseFds = 2,
    Rlimit = 3,
    Chdir = 4,
    SetGroups = 5,
    SetGid = 6,
    SetUid = 7,
    VerifyCredentials = 8,
    ParentDeathSignal = 9,
    Exec = 10,
}

impl ChildStep {
    const ALL: [Self; 10] = [
        Self::ResetSignals,
        Self::CloseFds,
        Self::Rlimit,
        Self::Chdir,
        Self::SetGroups,
        Self::SetGid,
        Self::SetUid,
        Self::VerifyCredentials,
        Self::ParentDeathSignal,
        Self::Exec,
    ];

    pub(crate) fn from_raw(raw: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|step| *step as u16 == raw)
    }
}

/// A failure reported by the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildFailure {
    pub(crate) step: ChildStep,
    pub(crate) errno: Errno,
    /// Step-specific: which rlimit, or which exec candidate.
    pub(crate) detail: u64,
}

/// What the parent knows about the spawn, for explaining a failure.
#[derive(Debug, Clone)]
pub(crate) struct FailureContext {
    pub(crate) program: PathBuf,
    pub(crate) candidates: Vec<PathBuf>,
    pub(crate) cwd: PathBuf,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) rlimits: Vec<Rlimit>,
}

/// Why a command never got to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnErrorKind {
    /// The command, or its `#!` interpreter, wasn't found (exit 127).
    NotFound,
    /// The command was found but couldn't be executed (exit 126).
    CannotExecute,
    /// Setting up the process failed before exec (exit 125).
    Setup,
}

/// A command that was started but never ran, with a message saying why.
///
/// The message is complete on its own (it already explains the errno in
/// words), so the raw errno isn't chained as a source; [`SpawnError::errno`]
/// still gives it to programs that want it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SpawnError {
    kind: SpawnErrorKind,
    message: String,
    errno: Errno,
}

impl SpawnError {
    fn new(kind: SpawnErrorKind, message: impl Into<String>, errno: Errno) -> Self {
        Self {
            kind,
            message: message.into(),
            errno,
        }
    }

    /// Why the command didn't run.
    #[must_use]
    pub fn kind(&self) -> SpawnErrorKind {
        self.kind
    }

    /// The error number the kernel returned.
    #[must_use]
    pub fn errno(&self) -> Errno {
        self.errno
    }

    /// 127 for not found, 126 for can't execute, 125 for setup failures.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            SpawnErrorKind::NotFound => EXIT_NOT_FOUND,
            SpawnErrorKind::CannotExecute => EXIT_CANNOT_EXECUTE,
            SpawnErrorKind::Setup => EXIT_ENGINE_ERROR,
        }
    }
}

/// Turns the child's report into an explained error.
pub(crate) fn describe(failure: ChildFailure, ctx: &FailureContext) -> SpawnError {
    use SpawnErrorKind::Setup;
    let ChildFailure {
        step,
        errno,
        detail,
    } = failure;
    // Setup messages say what was being done; the errno says why.
    let setup = |doing: String| SpawnError::new(Setup, format!("{doing}: {}", errno.desc()), errno);
    match step {
        ChildStep::Exec => describe_exec(errno, detail, ctx),
        ChildStep::ResetSignals => setup("resetting signal handlers".into()),
        ChildStep::CloseFds => setup("closing inherited file descriptors".into()),
        ChildStep::Rlimit => SpawnError::new(Setup, describe_rlimit(errno, detail, ctx), errno),
        ChildStep::Chdir => setup(format!("changing directory to {}", ctx.cwd.display())),
        ChildStep::SetGroups => setup("setting supplementary groups".into()),
        ChildStep::SetGid => setup(format!("switching to group {}", ctx.gid)),
        ChildStep::SetUid => setup(format!("switching to user {}", ctx.uid)),
        ChildStep::VerifyCredentials => setup(
            "the user and groups did not change as expected; \
             refusing to run with leftover privileges"
                .into(),
        ),
        ChildStep::ParentDeathSignal => setup("setting the parent-death signal".into()),
    }
}

fn describe_rlimit(errno: Errno, detail: u64, ctx: &FailureContext) -> String {
    let limit = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.rlimits.get(index));
    let mut message = match limit {
        Some(limit) => format!("setting resource limit {limit}: {}", errno.desc()),
        None => format!("setting a resource limit: {}", errno.desc()),
    };
    if errno == Errno::EPERM {
        message.push_str(" (raising a hard limit needs root");
        if cfg!(target_os = "linux") {
            message.push_str("; Linux also caps nofile at fs.nr_open");
        }
        message.push(')');
    }
    message
}

fn describe_exec(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    use SpawnErrorKind::{CannotExecute, NotFound, Setup};
    let program = ctx.program.display();
    let candidate = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.candidates.get(index))
        .unwrap_or(&ctx.program);
    let path = candidate.display();

    match errno {
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::ENAMETOOLONG => {
            // The file can exist and still give ENOENT: then it's the `#!`
            // interpreter or the dynamic loader that's missing.
            if let Some(found) = ctx.candidates.iter().find(|c| c.is_file()) {
                let message = format!(
                    "{}: found, but its interpreter or loader is missing \
                     (check the #! line)",
                    found.display()
                );
                return SpawnError::new(NotFound, message, errno);
            }
            let message = if has_slash(&ctx.program) {
                format!("{program}: no such file")
            } else {
                let mut message = format!("{program}: command not found");
                if let Some(dirs) = searched_dirs(ctx) {
                    let _ = write!(message, " (searched {dirs})");
                }
                message
            };
            SpawnError::new(NotFound, message, errno)
        }
        Errno::EACCES => {
            let message = if candidate.is_dir() {
                format!("{path}: is a directory")
            } else {
                format!("{path}: permission denied (is it executable?)")
            };
            SpawnError::new(CannotExecute, message, errno)
        }
        Errno::ENOEXEC => SpawnError::new(
            CannotExecute,
            format!("{path}: not a valid program (scripts need a #! line)"),
            errno,
        ),
        Errno::ETXTBSY => SpawnError::new(
            CannotExecute,
            format!("{path}: file is busy (it is being written)"),
            errno,
        ),
        Errno::EAGAIN => SpawnError::new(
            CannotExecute,
            format!(
                "{path}: too many processes for user {} (RLIMIT_NPROC)",
                ctx.uid
            ),
            errno,
        ),
        Errno::E2BIG => SpawnError::new(
            Setup,
            "the argument list and environment are too large",
            errno,
        ),
        _ => SpawnError::new(
            CannotExecute,
            format!("{path}: cannot execute: {}", errno.desc()),
            errno,
        ),
    }
}

fn has_slash(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().contains(&b'/')
}

/// The directories a PATH search went through, for "command not found".
fn searched_dirs(ctx: &FailureContext) -> Option<String> {
    let dirs: Vec<String> = ctx
        .candidates
        .iter()
        .filter_map(|c| c.parent())
        .map(|dir| dir.display().to_string())
        .collect();
    (!dirs.is_empty()).then(|| dirs.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(program: &str, candidates: &[&str]) -> FailureContext {
        FailureContext {
            program: program.into(),
            candidates: candidates.iter().map(PathBuf::from).collect(),
            cwd: "/srv/app".into(),
            uid: 1000,
            gid: 1000,
            rlimits: vec!["nofile=10:20".parse().unwrap()],
        }
    }

    fn exec(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
        describe(
            ChildFailure {
                step: ChildStep::Exec,
                errno,
                detail,
            },
            ctx,
        )
    }

    #[test]
    fn steps_round_trip_through_their_numbers() {
        for step in ChildStep::ALL {
            assert_eq!(ChildStep::from_raw(step as u16), Some(step));
        }
        assert_eq!(ChildStep::from_raw(0), None);
        assert_eq!(ChildStep::from_raw(999), None);
    }

    #[test]
    fn command_not_found() {
        let ctx = ctx("nope", &["/no/such/dir/nope", "/also/missing/nope"]);
        let err = exec(Errno::ENOENT, 1, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert!(
            err.to_string().starts_with("nope: command not found"),
            "{err}"
        );
        assert!(
            err.to_string().contains("/no/such/dir:/also/missing"),
            "{err}"
        );
    }

    #[test]
    fn missing_file_given_by_path() {
        let ctx = ctx("/no/such/thing", &["/no/such/thing"]);
        let err = exec(Errno::ENOENT, 0, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert_eq!(err.to_string(), "/no/such/thing: no such file");
    }

    #[test]
    fn existing_file_with_enoent_blames_the_interpreter() {
        // /etc/hosts exists, so ENOENT can't mean "not found".
        let ctx = ctx("hosts", &["/no/such/dir/hosts", "/etc/hosts"]);
        let err = exec(Errno::ENOENT, 0, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert!(err.to_string().contains("interpreter or loader"), "{err}");
    }

    #[test]
    fn permission_problems() {
        let dir = exec(Errno::EACCES, 0, &ctx("/tmp", &["/tmp"]));
        assert_eq!(dir.exit_code(), 126);
        assert!(dir.to_string().ends_with("is a directory"), "{dir}");

        let file = exec(Errno::EACCES, 0, &ctx("/etc/hosts", &["/etc/hosts"]));
        assert_eq!(file.exit_code(), 126);
        assert!(file.to_string().contains("permission denied"), "{file}");
    }

    #[test]
    fn other_exec_errors() {
        let ctx = ctx("/x", &["/x"]);
        assert!(
            exec(Errno::ENOEXEC, 0, &ctx)
                .to_string()
                .contains("not a valid program")
        );
        assert!(exec(Errno::ETXTBSY, 0, &ctx).to_string().contains("busy"));
        assert!(
            exec(Errno::EAGAIN, 0, &ctx)
                .to_string()
                .contains("RLIMIT_NPROC")
        );
        let too_big = exec(Errno::E2BIG, 0, &ctx);
        assert_eq!(too_big.exit_code(), 125);
        assert_eq!(exec(Errno::ENOMEM, 0, &ctx).exit_code(), 126);
    }

    #[test]
    fn out_of_range_detail_falls_back_to_the_program() {
        let err = exec(Errno::ENOEXEC, u64::MAX, &ctx("/x", &["/x"]));
        assert!(err.to_string().starts_with("/x:"), "{err}");
    }

    #[test]
    fn setup_failures_name_the_step_and_value() {
        let ctx = ctx("/x", &["/x"]);
        let fail = |step, errno, detail| {
            describe(
                ChildFailure {
                    step,
                    errno,
                    detail,
                },
                &ctx,
            )
        };

        let chdir = fail(ChildStep::Chdir, Errno::ENOENT, 0);
        assert_eq!(chdir.exit_code(), 125);
        assert_eq!(
            chdir.to_string(),
            "changing directory to /srv/app: No such file or directory"
        );

        let limit = fail(ChildStep::Rlimit, Errno::EPERM, 0);
        assert!(
            limit
                .to_string()
                .starts_with("setting resource limit nofile=10:20"),
            "{limit}"
        );
        assert!(limit.to_string().contains("needs root"), "{limit}");

        assert_eq!(
            fail(ChildStep::SetUid, Errno::EPERM, 0).to_string(),
            "switching to user 1000: Operation not permitted"
        );
    }
}
