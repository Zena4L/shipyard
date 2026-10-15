//! A fixture registry: an in-process, std-only HTTP server that serves an
//! image built from the host's own programs (`RootFs`), so image tests need
//! no network and no real registry. Used by `tests/image.rs` and the timing
//! budgets (`tests/budgets.rs`).

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;

use crate::rootfs::RootFs;

/// sha256 of `bytes`, as `sha256:<hex>` (an OCI digest).
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(71);
    hex.push_str("sha256:");
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Builds one image layer from `root`'s tree: the gzip-compressed tar, and
/// the digest of the tar itself (its `diff_id`).
#[must_use]
pub fn layer_tar(root: &std::path::Path) -> (Vec<u8>, String) {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all(".", root).unwrap();
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    (encoder.finish().unwrap(), sha256(&tar))
}

/// A minimal in-process registry serving a fixed set of GET routes; stopped
/// when dropped.
#[derive(Debug)]
pub struct Serving {
    /// `127.0.0.1:PORT`.
    pub base: String,
    stop: Arc<AtomicBool>,
    /// While set, blobs are held back: a pull stays in the middle of its
    /// downloads.
    pub hold: Arc<AtomicBool>,
    /// How many blob requests have been held back.
    pub held: Arc<AtomicU32>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Serves `routes` (path, content type, body) on a free loopback port.
#[must_use]
pub fn serve(routes: Vec<(String, &'static str, Vec<u8>)>) -> Serving {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let hold = Arc::new(AtomicBool::new(false));
    let hold_thread = hold.clone();
    let held = Arc::new(AtomicU32::new(0));
    let held_thread = held.clone();
    let routes = Arc::new(routes);
    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let routes = routes.clone();
                    let hold = (hold_thread.clone(), held_thread.clone());
                    thread::spawn(move || handle_conn(stream, &routes, &hold.0, &hold.1));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    });
    Serving {
        base,
        stop,
        hold,
        held,
        handle: Some(handle),
    }
}

fn handle_conn(
    stream: TcpStream,
    routes: &[(String, &'static str, Vec<u8>)],
    hold: &AtomicBool,
    held: &AtomicU32,
) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
    // Drain headers.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
    }
    if path.contains("/blobs/") && hold.load(Ordering::Relaxed) {
        held.fetch_add(1, Ordering::Relaxed);
    }
    while path.contains("/blobs/") && hold.load(Ordering::Relaxed) {
        thread::sleep(std::time::Duration::from_millis(10));
    }
    let response = routes.iter().find(|(p, _, _)| *p == path);
    let mut stream = stream;
    match response {
        Some((_, content_type, body)) => {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
        None => {
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    }
    let _ = stream.flush();
}

/// A scratch directory, removed on drop, whose `store` the image store is
/// made in (Shipyard makes it private; one made here first would be
/// refused).
#[derive(Debug)]
pub struct Scratch(pub PathBuf);

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Scratch {
    /// A new, empty one.
    #[must_use]
    pub fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        // Overlay refuses an upperdir that is itself on overlay: the system
        // temp dir must be a disk or tmpfs (`test-linux.sh` mounts a tmpfs
        // there in its container); where it isn't, the run skips.
        let dir = std::env::temp_dir().join(format!("shipyard-img-e2e-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Where the image store goes.
    #[must_use]
    pub fn store(&self) -> PathBuf {
        self.0.join("store")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fixture registry serving a one-layer image built from the host's
/// programs, and the image's reference.
#[must_use]
pub fn fixture() -> (Serving, String) {
    fixture_with(0)
}

/// The fixture image with `extra` layers on top of its base, each adding
/// one file, `/layer-N` (holding `N`), and writing `/top` (`N` too: the
/// top layer's wins).
#[must_use]
pub fn fixture_with(extra: usize) -> (Serving, String) {
    fixture_of(&RootFs::minimal(), extra)
}

/// As [`fixture_with`], the base layer made from `root`.
#[must_use]
pub fn fixture_of(root: &RootFs, extra: usize) -> (Serving, String) {
    let mut layers = vec![layer_tar(root.path())];
    for index in 0..extra {
        let dir = Scratch::new();
        std::fs::write(dir.0.join(format!("layer-{index}")), index.to_string()).unwrap();
        std::fs::write(dir.0.join("top"), index.to_string()).unwrap();
        layers.push(layer_tar(&dir.0));
    }
    fixture_layers(layers)
}

/// A layer of one file, `path`, holding `content`, with no entries for the
/// directories it's in: it implies them.
#[must_use]
pub fn implying_layer(path: &str, content: &[u8]) -> (Vec<u8>, String) {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    builder.append_data(&mut header, path, content).unwrap();
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    (encoder.finish().unwrap(), sha256(&tar))
}

/// A fixture registry serving an image of `layers` (base first).
#[must_use]
pub fn fixture_layers(layers: Vec<(Vec<u8>, String)>) -> (Serving, String) {
    fixture_configured(layers, r#"{"Cmd":["/bin/cat","/marker"]}"#)
}

/// The fixture image (one layer, the host's programs) with `runtime` as
/// its config's `config` object (JSON: `{"Cmd": […], "Healthcheck": {…}}`).
#[must_use]
pub fn fixture_with_config(runtime: &str) -> (Serving, String) {
    fixture_configured(vec![layer_tar(RootFs::minimal().path())], runtime)
}

/// A fixture registry serving an image of `layers` (base first), its
/// config's `config` object `runtime` (JSON).
fn fixture_configured(layers: Vec<(Vec<u8>, String)>, runtime: &str) -> (Serving, String) {
    let diff_ids: Vec<String> = layers.iter().map(|(_, id)| format!("\"{id}\"")).collect();
    let config = format!(
        r#"{{"architecture":"{arch}","os":"linux","rootfs":{{"type":"layers","diff_ids":[{}]}},"config":{runtime}}}"#,
        diff_ids.join(","),
        arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" },
    )
    .into_bytes();
    let config_digest = sha256(&config);
    let descriptors: Vec<String> = layers
        .iter()
        .map(|(layer, _)| {
            format!(
                r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":{}}}"#,
                sha256(layer),
                layer.len()
            )
        })
        .collect();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{}]}}"#,
        config.len(),
        descriptors.join(","),
    )
    .into_bytes();

    let repo = "library/testimg";
    let mut routes = vec![
        (
            format!("/v2/{repo}/manifests/latest"),
            "application/vnd.oci.image.manifest.v1+json",
            manifest,
        ),
        (
            format!("/v2/{repo}/blobs/{config_digest}"),
            "application/octet-stream",
            config,
        ),
    ];
    for (layer, _) in layers {
        routes.push((
            format!("/v2/{repo}/blobs/{}", sha256(&layer)),
            "application/octet-stream",
            layer,
        ));
    }
    let serving = serve(routes);
    let image = format!("{}/{repo}", serving.base);
    (serving, image)
}

/// Whether an image run could happen here at all. Root always can: any
/// failure is the test's to report. A normal user needs rootless containers
/// (`rootless`, which fails rather than skips where they're required) and a
/// kernel that lets them mount overlay; only that refusal is a skip, and
/// not where rootless runs are required either.
#[must_use]
pub fn image_runs_here(output: &std::process::Output) -> bool {
    use crate::{Rootless, is_root, rootless};
    if output.status.success() || is_root() {
        return true;
    }
    if !rootless(Rootless::Full) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("mounting the image's overlay as the root filesystem") {
        return true;
    }
    assert!(
        !crate::rootless_required(),
        "rootless image runs are required here (SHIPYARD_TEST_ROOTLESS), but:\n{stderr}"
    );
    eprintln!("skipping: this kernel won't mount overlay for a normal user:\n{stderr}");
    false
}
