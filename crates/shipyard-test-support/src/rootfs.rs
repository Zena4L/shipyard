//! A tiny root filesystem for the tests, made from the host's own programs
//! and the libraries they load (no downloads, no network).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The programs every test root filesystem has, in `/bin`.
pub const PROGRAMS: [&str; 15] = [
    "sh", "cat", "ls", "stat", "touch", "head", "sleep", "readlink", "mkdir", "wc", "stty", "tty",
    "mknod", "chown", "chmod",
];

/// What `/marker` holds: proof the command sees this root filesystem.
pub const MARKER: &str = "shipyard test root filesystem\n";

/// A root filesystem in a temporary directory, removed on drop.
///
/// It holds [`PROGRAMS`] with their libraries and dynamic loader (copied to
/// the paths `ldd` gives, so they load without a loader cache), `/marker`,
/// an empty `/work`, and a world-writable `/tmp`. No `/proc`, `/sys` or
/// `/dev`: shipyard makes them.
#[derive(Debug)]
pub struct RootFs {
    dir: PathBuf,
}

impl RootFs {
    /// A new one under the system's temporary directory.
    #[must_use]
    pub fn minimal() -> Self {
        Self::minimal_in(&std::env::temp_dir())
    }

    /// A new one under `parent` (say, a mount with particular options).
    #[must_use]
    pub fn minimal_in(parent: &Path) -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = parent.join(format!(
            "shipyard-rootfs-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = Self { dir };
        for program in PROGRAMS {
            root.add_program(&host_program(program));
        }
        std::fs::write(root.path().join("marker"), MARKER).unwrap();
        std::fs::create_dir_all(root.path().join("work")).unwrap();
        let tmp = root.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o1777)).unwrap();
        root
    }

    /// The directory (absolute).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The directory as a string, for command lines.
    #[must_use]
    pub fn arg(&self) -> &str {
        self.dir.to_str().unwrap()
    }

    /// The host path of `inside` (an absolute path inside the root).
    #[must_use]
    pub fn host_path(&self, inside: &str) -> PathBuf {
        self.dir.join(inside.trim_start_matches('/'))
    }

    /// Adds the host's `program` (found in the usual directories) to
    /// `/bin`, with the libraries it loads.
    pub fn add_host_program(&self, program: &str) {
        self.add_program(&host_program(program));
    }

    /// Copies the host program at `host` into `/bin`, with what it loads.
    fn add_program(&self, host: &Path) {
        let name = host.file_name().unwrap();
        self.copy_file(host, &Path::new("/bin").join(name));
        for library in libraries(host) {
            self.copy_file(&library, &library);
        }
    }

    /// Copies the host file `from` to `inside` (following symlinks).
    fn copy_file(&self, from: &Path, inside: &Path) {
        let to = self.host_path(inside.to_str().unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        if !to.exists() {
            std::fs::copy(from, &to).unwrap_or_else(|e| panic!("copying {}: {e}", from.display()));
        }
    }

    /// Gives everything in it to host user `uid` and group `gid`, as a user
    /// namespace that maps root inside to them needs (`chown -R`, without
    /// following symlinks).
    pub fn chown_all(&self, uid: u32, gid: u32) {
        fn walk(path: &Path, uid: u32, gid: u32) {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid)).unwrap();
            if path.is_dir() && !path.is_symlink() {
                for entry in std::fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), uid, gid);
                }
            }
        }
        walk(&self.dir, uid, gid);
    }

    /// Removes the dynamic loader of `/bin/sh`: programs are still there,
    /// but can't start (ENOENT at exec).
    pub fn remove_loader(&self) {
        let loader = libraries(&host_program("sh"))
            .into_iter()
            .find(|path| path.to_string_lossy().contains("ld-linux"))
            .expect("sh has a dynamic loader");
        std::fs::remove_file(self.host_path(loader.to_str().unwrap())).unwrap();
    }
}

impl Drop for RootFs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where the host has `program`, from the usual directories.
fn host_program(program: &str) -> PathBuf {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{program} isn't on this host"))
}

/// The libraries and loader `program` loads, as `ldd` lists them (absolute
/// paths only: `linux-vdso.so.1` is the kernel's, with no file).
fn libraries(program: &Path) -> Vec<PathBuf> {
    let output = Command::new("ldd")
        .arg(program)
        .output()
        .expect("running ldd");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|word| word.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}
