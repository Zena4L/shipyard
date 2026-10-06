//! An OCI bundle for the tests: a [`RootFs`] and a `config.json`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

use crate::rootfs::RootFs;

/// A bundle directory holding `config.json`, with its root filesystem
/// elsewhere (`root.path` is absolute). Removed on drop.
#[derive(Debug)]
pub struct Bundle {
    dir: PathBuf,
    /// The root filesystem `config.json` names.
    pub root: RootFs,
}

impl Bundle {
    /// A bundle whose `config.json` is [`Bundle::config`], unchanged.
    #[must_use]
    pub fn new() -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-bundle-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bundle = Self {
            dir,
            root: RootFs::minimal(),
        };
        bundle.write(&bundle.config());
        bundle
    }

    /// The directory, for `--bundle`.
    #[must_use]
    pub fn arg(&self) -> &str {
        self.dir.to_str().unwrap()
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// A config that runs `sh -c 'echo ready'` as root, in new PID, mount,
    /// UTS and IPC namespaces, on [`Bundle::root`], with the capabilities
    /// and `noNewPrivileges` `runc spec` writes. Change it and
    /// [`Bundle::write`] it.
    #[must_use]
    pub fn config(&self) -> Value {
        const RUNC_SPEC_CAPABILITIES: [&str; 3] =
            ["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"];
        json!({
            "ociVersion": "1.2.0",
            "root": {"path": self.root.path()},
            "process": {
                "user": {"uid": 0, "gid": 0},
                "args": ["sh", "-c", "echo ready"],
                "env": ["PATH=/bin"],
                "cwd": "/",
                // What `runc spec` writes: engines always fill these in.
                "capabilities": {
                    "bounding": RUNC_SPEC_CAPABILITIES,
                    "effective": RUNC_SPEC_CAPABILITIES,
                    "permitted": RUNC_SPEC_CAPABILITIES
                },
                "noNewPrivileges": true
            },
            "linux": {
                "namespaces": [
                    {"type": "pid"}, {"type": "mount"}, {"type": "uts"}, {"type": "ipc"}
                ]
            }
        })
    }

    /// Writes `config` as the bundle's `config.json`.
    pub fn write(&self, config: &Value) {
        std::fs::write(self.dir.join("config.json"), config.to_string()).unwrap();
    }
}

impl Default for Bundle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
