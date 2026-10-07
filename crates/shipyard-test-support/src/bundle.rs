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

    /// Hooks for every phase that say what they see, a line each, appended
    /// to `log` (on the host): `PHASE id=ID status=STATUS pid=PID`, the
    /// state they were given, and
    ///
    /// - for `createRuntime`, `mounted` if the container's `/proc` and
    ///   `/marker` are there at its root filesystem's host path, seen
    ///   through `/proc/PID/root`: its mounts are made, its root not yet
    ///   switched;
    /// - for `createContainer`, `uts=` its UTS namespace (the container's,
    ///   if it runs in it) and `umask=` its umask (as `sh` prints it).
    ///
    /// `startContainer` runs inside the container: its line, `startContainer
    /// inside caps=CAPS STATE` (`inside` if `/marker` is at `/`; `CAPS` its
    /// effective capabilities, in hex), goes to the container's `/tmp/hooks`.
    #[must_use]
    pub fn recording_hooks(&self, log: &Path) -> Value {
        let root = self.root.path().display();
        let log = log.display();
        let host = format!(
            r#"state=$(cat)
field() {{ echo "$state" | sed -n "s/.*\"$1\":\"\{{0,1\}}\([^\",]*\).*/\1/p"; }}
pid=$(field pid)
extra=
case $0 in
createRuntime) [ -d "/proc/$pid/root{root}/proc/1" ] && [ -f "/proc/$pid/root{root}/marker" ] && extra=mounted ;;
createContainer) extra="uts=$(readlink /proc/self/ns/uts) umask=$(umask)" ;;
esac
echo "$0 id=$(field id) status=$(field status) pid=$pid $extra" >> {log}"#
        );
        let inside = r#"state=$(cat); where=; [ -f /marker ] && where=inside
caps=; while read -r key value; do [ "$key" = CapEff: ] && caps=$value; done < /proc/self/status
echo "$0 $where caps=$caps $state" >> /tmp/hooks"#;
        let hook = |script: &str, phase: &str| {
            json!([{
                "path": "/bin/sh",
                "args": ["sh", "-c", script, phase],
                "env": ["PATH=/usr/bin:/bin"]
            }])
        };
        json!({
            "prestart": hook(&host, "prestart"),
            "createRuntime": hook(&host, "createRuntime"),
            "createContainer": hook(&host, "createContainer"),
            "startContainer": hook(inside, "startContainer"),
            "poststart": hook(&host, "poststart"),
            "poststop": hook(&host, "poststop"),
        })
    }

    /// Checks the `startContainer` line [`Bundle::recording_hooks`] wrote
    /// (`out`, starting with it): the hook ran inside the root filesystem,
    /// with no capability but `CAP_KILL` (it's the image's program: never with
    /// the init's power). Returns the state that followed.
    ///
    /// # Panics
    ///
    /// If it's not so.
    #[must_use]
    pub fn checked_start_container(out: &str) -> &str {
        /// `CAP_KILL`'s bit (capability 5).
        const CAP_KILL: u64 = 1 << 5;
        let rest = out
            .strip_prefix("startContainer inside caps=")
            .unwrap_or_else(|| panic!("not a startContainer line inside the root: {out}"));
        let (caps, state) = rest.split_once(' ').unwrap();
        let caps = u64::from_str_radix(caps, 16).unwrap();
        assert_eq!(
            caps & !CAP_KILL,
            0,
            "startContainer's capabilities: {caps:#x}"
        );
        state
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
