//! What the helpers a container's monitor runs (`__cleanup`, `__health`)
//! share: their log (what they couldn't do, a line each, in a file beside
//! the container's record) and their arguments (paths, as text).

#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::{Arc, Mutex};

use crate::report::{Message, Reporter};

/// The largest log kept: past it, it's moved to `NAME.1` (the one before
/// dropped) when it's opened.
const LIMIT: u64 = 64 * 1024;

/// `name` beside container `id`'s record in `containers` (`cleanup.log`,
/// `health.log`). Opened only if the record's directory is there (it's
/// never made here: an empty directory named like an ID would trouble the
/// store's readers); with none, nothing is said.
#[derive(Clone)]
pub(in crate::run) struct Log {
    file: Option<Arc<Mutex<std::fs::File>>>,
}

impl Log {
    /// The log `name` of container `id`, opened to append.
    pub(in crate::run) fn open(containers: &std::path::Path, id: &str, name: &str) -> Self {
        let dir = containers.join(id);
        let path = dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.len() > LIMIT) {
            let _ = std::fs::rename(&path, dir.join(format!("{name}.1")));
        }
        let file = dir.is_dir().then(|| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .ok()
        });
        Self {
            file: file.flatten().map(|file| Arc::new(Mutex::new(file))),
        }
    }

    /// `text`, as a line.
    pub(in crate::run) fn line(&self, text: &str) {
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{text}");
        }
    }
}

impl Reporter for Log {
    fn report(&self, message: Message) {
        if let Message::Warning(warning) = message {
            self.line(&format!("warning: {}", warning.message));
        }
    }
}

/// `path` as an argument a monitor keeps (in JSON, so UTF-8), or why it
/// can't be one (`for_what`: what needs it).
///
/// # Errors
///
/// [`ErrorKind::Environment`](crate::ErrorKind::Environment): a path that
/// isn't UTF-8.
pub(in crate::run) fn arg_text(path: &std::path::Path, for_what: &str) -> crate::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        crate::Error::new(
            crate::ErrorKind::Environment,
            format!("{} isn't UTF-8, as {for_what} needs", path.display()),
        )
    })
}
