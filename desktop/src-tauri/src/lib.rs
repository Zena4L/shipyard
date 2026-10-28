//! Shipyard's desktop app (stage 14): the window around `shipyard-client`
//! (step 14.2). The page asks the core for what it may, by name (`build.rs`'s
//! manifest, `capabilities/default.json`), and listens to six events:
//! `engine-event` (the API's), `vm-line` (the VM's start), `connection-lost`,
//! `stats` and `stats-ended` (the selected container's), `run-line` (a
//! run's pull); the console's logs and shell come over channels of their
//! own (`console.rs`).
//! The window never leaves the app's own pages (`on_navigation`).

use std::path::PathBuf;

use serde::Serialize;
use shipyard_client::api::Client;
use shipyard_client::{ClientError, ErrorKind, place, settings, vm};
use tauri::{AppHandle, Emitter as _, Manager as _, State};
use tokio::sync::Mutex;

use console::{
    logs_follow, logs_stop, shell_ack, shell_close, shell_input, shell_open, shell_resize,
};

mod console;

/// The core's state, for the app's life.
#[derive(Debug)]
pub(crate) struct Core {
    /// Where the settings live, if anywhere (no home: defaults alone).
    settings_path: Option<PathBuf>,
    settings: Mutex<settings::Loaded>,
    /// The API, once connected.
    client: Mutex<Option<Client>>,
    /// Held by the one start under way: a later ask waits for it.
    starting: Mutex<()>,
    /// The event follower, while it runs.
    follower: Mutex<Option<Task>>,
    /// The selected container's stats follower (one at a time: the
    /// inspector's).
    stats: Mutex<Selected>,
    /// The console's logs follower and shell (the selection's).
    console: console::Console,
}

pub(crate) type Task = tauri::async_runtime::JoinHandle<()>;

/// Aborts `task`, if one runs.
pub(crate) fn stop(task: &mut Option<Task>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

/// A task for the page's selection (its stats, its logs), and the
/// selection it's for: the page numbers each selection, and an ask about an
/// older one than the latest is ignored, so one that lands late never
/// replaces a newer selection's.
#[derive(Debug, Default)]
pub(crate) struct Selected {
    selection: u64,
    pub(crate) task: Option<Task>,
}

impl Selected {
    /// Its task stopped and its numbering begun again: a page loaded anew
    /// numbers its selections from 1 (a reload, a web content process that
    /// crashed), and an older page's number would otherwise outrank it.
    pub(crate) fn reset(&mut self) {
        stop(&mut self.task);
        self.selection = 0;
    }

    /// Takes `selection` as the latest and stops what ran, unless a newer
    /// one was asked for already (false: leave it be).
    pub(crate) fn turn_to(&mut self, selection: u64) -> bool {
        if selection < self.selection {
            return false;
        }
        self.selection = selection;
        stop(&mut self.task);
        true
    }
}

/// What the page is told before anything is loaded.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    platform: &'static str,
    vm: Option<vm::VmState>,
    start_vm_on_open: bool,
    shipyard: ShipyardPlace,
    settings_said: Option<String>,
}

/// The `shipyard` the core runs, or why there's none.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ShipyardPlace {
    Path(String),
    Missing(String),
}

/// Everything the page loads at once: the containers and `info` as Docker's
/// JSON (`bollard`'s, passed on), the links answer, the rail's counts and
/// the networks (the map's key and colours).
#[derive(Debug, Serialize)]
struct Snapshot {
    containers: Vec<shipyard_client::api::ContainerSummary>,
    links: serde_json::Value,
    info: shipyard_client::api::SystemInfo,
    counts: shipyard_client::api::Counts,
    networks: Vec<shipyard_client::api::Network>,
}

/// A command's failure, as the page shows it: its kind (what the page
/// heads it with) and its words.
#[derive(Debug, Serialize)]
struct Failure {
    kind: ErrorKind,
    message: String,
}

/// A command's result.
pub(crate) type Said<T> = Result<T, Failure>;

/// The client's error, said.
#[expect(
    clippy::needless_pass_by_value,
    reason = "used as `map_err(said)`, which hands it over"
)]
pub(crate) fn said(error: ClientError) -> Failure {
    Failure {
        kind: error.kind(),
        message: error.to_string(),
    }
}

/// A failure of the core's own, of `kind`.
fn failure(kind: ErrorKind, message: impl Into<String>) -> Failure {
    Failure {
        kind,
        message: message.into(),
    }
}

impl Core {
    /// The settings now.
    async fn settings(&self) -> settings::Settings {
        self.settings.lock().await.settings.clone()
    }

    /// The VM's state (the Mac's; none on Linux), asked of its host on a
    /// blocking thread (a local socket, bounded by `shipyard-vm`).
    async fn vm_state(&self) -> Said<Option<vm::VmState>> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        let settings = self.settings().await;
        tauri::async_runtime::spawn_blocking(move || vm::state(&settings))
            .await
            .map_err(|error| failure(ErrorKind::Other, error.to_string()))?
            .map(Some)
            .map_err(said)
    }

    /// The API's client, only while the VM serves (asked each time, a
    /// connected client too: a request to launchd's `docker.sock` after a
    /// stop would start the VM again), connected now if it isn't.
    pub(crate) async fn client(&self) -> Said<Client> {
        if let Some(vm::VmState::Stopped | vm::VmState::Running { serving: false, .. }) =
            self.vm_state().await?
        {
            *self.client.lock().await = None;
            return Err(failure(
                ErrorKind::Unreachable,
                "Shipyard's VM isn't serving",
            ));
        }
        let mut client = self.client.lock().await;
        if let Some(client) = client.as_ref() {
            return Ok(client.clone());
        }
        let settings = self.settings().await;
        let endpoint = place::api_endpoint(&settings).map_err(said)?;
        let connected = match Client::connect(&endpoint).await {
            // Linux: nothing answers on the user's socket: the service
            // started by `shipyard` (the CLI's own rule, F190), once, under
            // this lock, then asked again.
            Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::Unreachable => {
                let shipyard = place::shipyard(&settings).map_err(said)?;
                shipyard_client::service::ensure(&shipyard, endpoint.path())
                    .await
                    .map_err(said)?;
                Client::connect(&endpoint).await.map_err(said)?
            }
            connected => connected.map_err(said)?,
        };
        *client = Some(connected.clone());
        Ok(connected)
    }
}

/// Where things stand before anything is loaded: the platform, the VM,
/// the settings, the `shipyard` found.
#[tauri::command]
async fn status(core: State<'_, Core>) -> Said<Status> {
    let loaded = core.settings.lock().await.clone();
    let shipyard = match place::shipyard(&loaded.settings) {
        Ok(path) => ShipyardPlace::Path(path.display().to_string()),
        Err(error) => ShipyardPlace::Missing(error.to_string()),
    };
    Ok(Status {
        platform: std::env::consts::OS,
        vm: core.vm_state().await?,
        start_vm_on_open: loaded.settings.start_vm_on_open,
        shipyard,
        settings_said: loaded.said,
    })
}

/// Starts the VM (one start at a time), its lines sent as `vm-line`; returns
/// once its API serves.
#[tauri::command]
async fn start_vm(core: State<'_, Core>, app: AppHandle) -> Said<()> {
    // One start at a time: a later ask waits, then finds the VM up.
    let _one = core.starting.lock().await;
    if let Some(vm::VmState::Running { serving: true, .. }) = core.vm_state().await? {
        return Ok(());
    }
    let settings = core.settings().await;
    let shipyard = place::shipyard(&settings).map_err(said)?;
    vm::check_signed(&shipyard).await.map_err(said)?;
    vm::start(&shipyard, &settings, |line| {
        let _ = app.emit("vm-line", line);
    })
    .await
    .map_err(said)?;
    // `vm start` returns as the guest is ready, a moment before its host
    // says it serves: the page's load comes after that.
    vm::wait_serving(&settings, vm::SERVING_WAIT)
        .await
        .map_err(said)
}

/// Everything the page shows, read at once.
#[tauri::command]
async fn snapshot(core: State<'_, Core>) -> Said<Snapshot> {
    let client = core.client().await?;
    let containers = client.containers().await.map_err(said)?;
    let links = client.links().await.map_err(said)?;
    let info = client.info().await.map_err(said)?;
    let images = info
        .images
        .and_then(|images| usize::try_from(images).ok())
        .unwrap_or(0);
    let networks = client.networks().await.map_err(said)?;
    let counts = shipyard_client::api::Counts {
        images,
        volumes: client.volume_count().await.map_err(said)?,
        // The bridges alone: the Networks section's (the built-ins aren't).
        networks: networks
            .iter()
            .filter(|network| network.driver == "bridge")
            .count(),
    };
    Ok(Snapshot {
        containers,
        links,
        info,
        counts,
        networks,
    })
}

/// The links answer alone: the map's poll (its live connections).
#[tauri::command]
async fn links(core: State<'_, Core>) -> Said<serde_json::Value> {
    core.client().await?.links().await.map_err(said)
}

/// What the host, the running containers and the monitors use now: the
/// page's poll of its live numbers (it divides two reads).
#[tauri::command]
async fn usage(core: State<'_, Core>) -> Said<shipyard_client::usage::Usage> {
    let client = core.client().await?;
    shipyard_client::usage::usage(&client).await.map_err(said)
}

/// Starts a container (the map's fix for a stopped dependency, the
/// inspector's Start), by its ID
/// or name: never anything else (the client checks its shape).
#[tauri::command]
async fn start_container(core: State<'_, Core>, id: String) -> Said<()> {
    core.client().await?.start(&id).await.map_err(said)
}

/// Stops a container (the inspector's Stop), by its ID or name.
#[tauri::command]
async fn stop_container(core: State<'_, Core>, id: String) -> Said<()> {
    core.client().await?.stop(&id).await.map_err(said)
}

/// Restarts a container.
#[tauri::command]
async fn restart_container(core: State<'_, Core>, id: String) -> Said<()> {
    core.client().await?.restart(&id).await.map_err(said)
}

/// Pauses a container.
#[tauri::command]
async fn pause_container(core: State<'_, Core>, id: String) -> Said<()> {
    core.client().await?.pause(&id).await.map_err(said)
}

/// Resumes a paused container.
#[tauri::command]
async fn unpause_container(core: State<'_, Core>, id: String) -> Said<()> {
    core.client().await?.unpause(&id).await.map_err(said)
}

/// Removes a container once its confirm said so (`force`: a running one
/// stopped first, D14.4b-1).
#[tauri::command]
async fn remove_container(core: State<'_, Core>, id: String, force: bool) -> Said<()> {
    core.client().await?.remove(&id, force).await.map_err(said)
}

/// Runs an image detached from the Run sheet's spec (the body is the
/// client's to build: the page names nothing else), each line of a pull
/// as a `run-line` event; the new container's ID.
#[tauri::command]
async fn run_image(
    core: State<'_, Core>,
    app: AppHandle,
    spec: shipyard_client::run::RunSpec,
) -> Said<String> {
    let client = core.client().await?;
    shipyard_client::run::run(&client, &spec, |line| {
        let _ = app.emit("run-line", line);
    })
    .await
    .map_err(said)
}

/// Every image with its layers (the Images section, the map's overlay).
#[tauri::command]
async fn images(core: State<'_, Core>) -> Said<Vec<shipyard_client::images::ImageLayers>> {
    let client = core.client().await?;
    shipyard_client::images::images(&client).await.map_err(said)
}

/// Removes an image's tag, or an untagged image by its ID (the page never
/// forces: one a container uses is the engine's refusal).
#[tauri::command]
async fn remove_image(core: State<'_, Core>, reference: String, force: bool) -> Said<()> {
    let client = core.client().await?;
    shipyard_client::images::remove(&client, &reference, force)
        .await
        .map_err(said)
}

/// Every volume's size, files and place (the Volumes section).
#[tauri::command]
async fn volumes(core: State<'_, Core>) -> Said<serde_json::Value> {
    let client = core.client().await?;
    shipyard_client::volumes::volumes(&client)
        .await
        .map_err(said)
}

/// Deletes a volume by name, never forced (one a container names is the
/// engine's refusal).
#[tauri::command]
async fn remove_volume(core: State<'_, Core>, name: String) -> Said<()> {
    let client = core.client().await?;
    shipyard_client::volumes::remove(&client, &name)
        .await
        .map_err(said)
}

/// Shipyard's own view of a bridge network (its bridge and veths), `null`
/// where bridges aren't served (rootless).
#[tauri::command]
async fn network_view(
    core: State<'_, Core>,
    name: String,
) -> Said<Option<shipyard_client::networks::NetworkView>> {
    let client = core.client().await?;
    shipyard_client::networks::view(&client, &name)
        .await
        .map_err(said)
}

/// A running container's isolation (`/shipyard/containers/{id}/isolation`).
#[tauri::command]
async fn isolation(core: State<'_, Core>, id: String) -> Said<serde_json::Value> {
    core.client().await?.isolation(&id).await.map_err(said)
}

/// A container's Docker inspect (its state, an out-of-memory kill).
#[tauri::command]
async fn inspect(core: State<'_, Core>, id: String) -> Said<serde_json::Value> {
    core.client().await?.inspect(&id).await.map_err(said)
}

/// One of a `stats` event: the container it's of, and its sample.
#[derive(Debug, Clone, Serialize)]
struct StatsEvent {
    id: String,
    #[serde(flatten)]
    sample: shipyard_client::stats::Sample,
}

/// A stats stream's end on its own: its container, and why.
#[derive(Debug, Clone, Serialize)]
struct StatsEnded {
    id: String,
    why: String,
}

/// Follows container `id`'s stats as `stats` events, for the page's
/// `selection` (the one followed before ended): the inspector's live numbers.
#[tauri::command]
async fn watch_stats(
    core: State<'_, Core>,
    app: AppHandle,
    id: String,
    selection: u64,
) -> Said<()> {
    shipyard_client::api::container_ref(&id).map_err(said)?;
    // Held across the connection, so an unwatch or a release asked
    // meanwhile comes after the follower is made, and ends it.
    let mut follower = core.stats.lock().await;
    if !follower.turn_to(selection) {
        return Ok(());
    }
    let client = core.client().await?;
    follower.task = Some(tauri::async_runtime::spawn(async move {
        let ended = shipyard_client::stats::follow(&client, &id, |sample| {
            let _ = app.emit(
                "stats",
                StatsEvent {
                    id: id.clone(),
                    sample,
                },
            );
        })
        .await;
        // Its own end (an abort never gets here): the page says it.
        let why = ended
            .err()
            .map_or_else(String::new, |error| error.to_string());
        let _ = app.emit("stats-ended", StatsEnded { id, why });
    }));
    Ok(())
}

/// Stops following stats, for the page's `selection`.
#[tauri::command]
async fn unwatch_stats(core: State<'_, Core>, selection: u64) -> Said<()> {
    core.stats.lock().await.turn_to(selection);
    Ok(())
}

/// Follows the API's events as `engine-event` (the previous follower
/// ended), returning once subscribed; its end is `connection-lost`.
#[tauri::command]
async fn follow(core: State<'_, Core>, app: AppHandle) -> Said<()> {
    let client = core.client().await?;
    let mut follower = core.follower.lock().await;
    stop(&mut follower);
    // Subscribed before this returns, so the page's list after it misses
    // nothing. No `since`: the service filters live events by its own clock,
    // and the Mac's, ahead of a guest's that lags after a sleep, would drop
    // the events between them.
    let subscribed = match shipyard_client::events::subscribe(&client, None).await {
        Ok(subscribed) => subscribed,
        // Its service gone since the client was made: forgotten, so "Try
        // again" connects anew (and on Linux starts the service), never
        // asks the dead one again.
        Err(error) => {
            if error.kind() == ErrorKind::Unreachable {
                *core.client.lock().await = None;
            }
            return Err(said(error));
        }
    };
    let events = app.clone();
    *follower = Some(tauri::async_runtime::spawn(async move {
        let ended = subscribed
            .run(|event| {
                let _ = events.emit("engine-event", event);
            })
            .await;
        // Gone: the next ask connects again, once the VM serves.
        if let Some(core) = events.try_state::<Core>() {
            *core.client.lock().await = None;
        }
        let why = ended
            .err()
            .map_or_else(String::new, |error| error.to_string());
        let _ = events.emit("connection-lost", why);
    }));
    Ok(())
}

/// Lets go of the event stream, the selection's stats, logs and shell, and
/// the connection, the selections' numbering begun again: a hidden window
/// (the VM's idle stop may come), and every page as it loads (a reload or a
/// crashed web content process leaves the last page's behind).
#[tauri::command]
async fn release(core: State<'_, Core>) -> Said<()> {
    stop(&mut *core.follower.lock().await);
    core.stats.lock().await.reset();
    core.console.release().await;
    *core.client.lock().await = None;
    Ok(())
}

/// Saves whether the VM starts when the app opens.
#[tauri::command]
async fn set_start_vm_on_open(core: State<'_, Core>, start: bool) -> Said<()> {
    change_settings(&core, |settings| settings.start_vm_on_open = start).await
}

/// Lets the user pick the `shipyard` program in the core's own file dialog
/// (never a path from the page), and saves it.
#[tauri::command]
async fn choose_shipyard(core: State<'_, Core>, app: AppHandle) -> Said<()> {
    use tauri_plugin_dialog::DialogExt as _;
    let (chosen, picked) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose the shipyard program")
        .pick_file(move |file| {
            let _ = chosen.send(file.and_then(|file| file.into_path().ok()));
        });
    let Some(path) = picked
        .await
        .map_err(|_| failure(ErrorKind::Other, "the file dialog closed"))?
    else {
        return Ok(());
    };
    change_settings(&core, move |settings| settings.shipyard = Some(path)).await
}

/// The settings changed by `change` and saved: never over a newer app's.
async fn change_settings(core: &Core, change: impl FnOnce(&mut settings::Settings)) -> Said<()> {
    let mut loaded = core.settings.lock().await;
    if !loaded.writable {
        return Err(failure(
            ErrorKind::Settings,
            loaded
                .said
                .clone()
                .unwrap_or_else(|| "the settings are a newer app's".to_owned()),
        ));
    }
    let path = core.settings_path.as_ref().ok_or_else(|| {
        failure(
            ErrorKind::Settings,
            "no place for the settings (no home directory)",
        )
    })?;
    let mut settings = loaded.settings.clone();
    change(&mut settings);
    settings::save(path, &settings).map_err(said)?;
    loaded.settings = settings;
    loaded.said = None;
    Ok(())
}

/// Whether the window may go to `url`: the app's own pages alone (its
/// bundle's `tauri://`, or in a debug build the dev server's port).
fn own_page(url: &tauri::Url) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" if cfg!(debug_assertions) => {
            url.host_str() == Some("127.0.0.1") && url.port() == Some(1420)
        }
        _ => false,
    }
}

/// Runs the app.
///
/// # Panics
///
/// If Tauri can't start (its context, the window).
pub fn run() {
    let settings_path = settings::path().ok();
    let loaded = settings_path.as_deref().map_or_else(
        || settings::Loaded {
            settings: settings::Settings::default(),
            writable: false,
            said: Some("no home directory: the defaults used".to_owned()),
        },
        settings::load,
    );
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(Core {
            settings_path,
            settings: Mutex::new(loaded),
            client: Mutex::new(None),
            starting: Mutex::new(()),
            follower: Mutex::new(None),
            stats: Mutex::new(Selected::default()),
            console: console::Console::default(),
        })
        .invoke_handler(tauri::generate_handler![
            status,
            start_vm,
            snapshot,
            links,
            usage,
            start_container,
            stop_container,
            restart_container,
            pause_container,
            unpause_container,
            remove_container,
            run_image,
            images,
            remove_image,
            volumes,
            remove_volume,
            network_view,
            isolation,
            inspect,
            watch_stats,
            unwatch_stats,
            logs_follow,
            logs_stop,
            shell_open,
            shell_input,
            shell_resize,
            shell_ack,
            shell_close,
            follow,
            release,
            set_start_vm_on_open,
            choose_shipyard
        ])
        .setup(|app| {
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Shipyard")
            .inner_size(1360.0, 860.0)
            .min_inner_size(960.0, 600.0)
            .on_navigation(own_page);
            // The page draws the design's title bar; the Mac's own buttons
            // sit where the mock paints its lights (14 px in, centred in
            // its 51 px bar: a 14 px button's corner at 13, 19).
            #[cfg(target_os = "macos")]
            let window = window
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true)
                .traffic_light_position(tauri::LogicalPosition::new(13.0, 19.0));
            window.build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| panic!("Shipyard's window: {error}"));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The window stays on the app's own pages: never a remote one, a
    /// file, or another port of the host's.
    #[test]
    fn the_window_never_leaves_the_app() {
        let url = |text: &str| tauri::Url::parse(text).unwrap();
        assert!(own_page(&url("tauri://localhost/index.html")));
        assert!(!own_page(&url("https://example.com/")));
        assert!(!own_page(&url("file:///etc/passwd")));
        assert!(!own_page(&url("http://127.0.0.1:8080/")));
        // The dev server's own port, in a debug build alone.
        assert_eq!(
            own_page(&url("http://127.0.0.1:1420/")),
            cfg!(debug_assertions)
        );
    }

    /// A watch or unwatch for an older selection, landing after a newer
    /// one's, changes nothing; the newer one's and the same one's do.
    #[test]
    fn a_late_ask_never_replaces_a_newer_selection() {
        let mut stats = Selected::default();
        assert!(stats.turn_to(2));
        assert!(!stats.turn_to(1), "an older selection's ask");
        assert_eq!(stats.selection, 2);
        assert!(stats.turn_to(2), "the same selection's unwatch, then watch");
        assert!(stats.turn_to(3));
        // A page loaded anew numbers from 1 again: after its release, its
        // first selection is taken.
        stats.reset();
        assert!(stats.turn_to(1), "a new page's first selection");
    }

    include!("commands.rs");

    /// The capability allows exactly the app's commands (`commands.rs`,
    /// which `build.rs` declares), and of Tauri's own, listening to events
    /// and dragging the window by its title bar alone (no `core:default`: no
    /// page `emit`, no paths; no plugin's); the handler registers exactly
    /// those commands.
    #[test]
    fn the_page_is_allowed_only_its_commands() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let allowed: Vec<&str> = capability["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|permission| permission.as_str().unwrap())
            .collect();
        let commands: Vec<String> = COMMANDS
            .iter()
            .map(|name| format!("allow-{}", name.replace('_', "-")))
            .collect();
        let mut expected: Vec<&str> = vec![
            "core:event:allow-listen",
            "core:event:allow-unlisten",
            "core:window:allow-start-dragging",
        ];
        expected.extend(commands.iter().map(String::as_str));
        assert_eq!(allowed, expected);
        let source = include_str!("lib.rs");
        let handled: Vec<&str> = source
            .split("generate_handler![")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        assert_eq!(handled, COMMANDS);
    }
}
