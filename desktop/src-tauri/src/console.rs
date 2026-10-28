//! The console's commands (step 14.5): the selection's logs, in batches over
//! a channel, and one shell, its terminal's bytes over a channel as the
//! page takes them, its input and size by command. The shell's command is
//! the client's (`sh`), never the page's; a session is the core's, by the
//! number it handed out. Both go when the window lets go (`release`).
//!
//! Neither lets a container flood the window: a channel's `send` never
//! waits (Tauri hands each message to the web view at once), so logs are
//! sent ten times a second at most, the newest [`LOG_KEEP`] lines of each
//! batch, and a shell's output waits for the page to say it wrote what it
//! had ([`shell_ack`]) once [`UNACKED_MOST`] bytes are out; the API's own
//! backpressure then holds the container's terminal, as a real one does.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use serde::Serialize;
use shipyard_client::console::Shell;
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::{Mutex, Notify, mpsc};

use crate::{Core, Said, Selected, Task, said};

/// The most log lines a batch carries (the page keeps as many).
const LOG_KEEP: usize = 2000;

/// How often logs are sent.
const LOG_TICK: Duration = Duration::from_millis(100);

/// How many bytes of a shell's output may be out before the page says it
/// wrote them.
const UNACKED_MOST: usize = 1024 * 1024;

/// How many inputs and resizes may wait for the shell (beyond, `shell_input`
/// waits, holding nothing else).
const QUEUED_MOST: usize = 256;

/// The console's state: its logs follower, its one shell.
#[derive(Debug, Default)]
pub(crate) struct Console {
    logs: Mutex<Selected>,
    shell: Mutex<Option<Session>>,
    sessions: AtomicU64,
}

/// A shell open: its number, its queue to the writer, the bytes the page
/// hasn't said it wrote, and its tasks (the writer, the pump).
#[derive(Debug)]
struct Session {
    number: u64,
    queue: mpsc::Sender<ToShell>,
    unacked: Arc<Unacked>,
    tasks: [Task; 2],
}

/// What the writer hands its shell.
#[derive(Debug)]
enum ToShell {
    Input(Vec<u8>),
    Resize(u16, u16),
}

/// A shell's output out and not yet written by the page, and the pump's
/// wake when the page says it was.
#[derive(Debug, Default)]
struct Unacked {
    bytes: AtomicUsize,
    acked: Notify,
}

/// One message of the logs: lines, and, at a follower's end, why.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LogBatch {
    lines: Vec<String>,
    ended: Option<String>,
}

impl Console {
    /// Its logs follower and its shell let go (the window hidden a while).
    pub(crate) async fn release(&self) {
        self.logs.lock().await.reset();
        close(&mut *self.shell.lock().await);
    }
}

/// The open shell closed: its writer and pump stopped, its connection
/// dropped (the API hangs the shell up). Never waits on a write.
fn close(session: &mut Option<Session>) {
    if let Some(open) = session.take() {
        for task in open.tasks {
            task.abort();
        }
    }
}

/// Follows container `id`'s logs for the page's `selection` (its last
/// lines, then, with `follow`, each new one), sent on `lines` in batches;
/// the follower before it ends. A follower's failure is its last batch's
/// `ended`.
#[tauri::command]
pub(crate) async fn logs_follow(
    core: State<'_, Core>,
    id: String,
    follow: bool,
    selection: u64,
    lines: Channel<LogBatch>,
) -> Said<()> {
    shipyard_client::api::container_ref(&id).map_err(said)?;
    // Held across the connection, as `watch_stats`'s: a stop or a release
    // asked meanwhile comes after the follower is made, and ends it.
    let mut follower = core.console.logs.lock().await;
    if !follower.turn_to(selection) {
        return Ok(());
    }
    let client = core.client().await?;
    follower.task = Some(tauri::async_runtime::spawn(async move {
        let pending = std::sync::Mutex::new(VecDeque::new());
        let read = shipyard_client::console::logs(&client, &id, follow, |line| {
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() == LOG_KEEP {
                pending.pop_front();
            }
            pending.push_back(line);
        });
        tokio::pin!(read);
        let mut tick = tokio::time::interval(LOG_TICK);
        let ended = loop {
            tokio::select! {
                ended = &mut read => break ended,
                _ = tick.tick() => send_lines(&pending, &lines, None),
            }
        };
        send_lines(&pending, &lines, ended.err().map(|error| error.to_string()));
    }));
    Ok(())
}

/// What `pending` holds sent on `lines` as one batch, with `ended` if it's
/// the last; nothing sent for nothing.
fn send_lines(
    pending: &std::sync::Mutex<VecDeque<String>>,
    lines: &Channel<LogBatch>,
    ended: Option<String>,
) {
    let batch: Vec<String> = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect();
    if !batch.is_empty() || ended.is_some() {
        let _ = lines.send(LogBatch {
            lines: batch,
            ended,
        });
    }
}

/// Stops following logs, for the page's `selection`.
#[tauri::command]
pub(crate) async fn logs_stop(core: State<'_, Core>, selection: u64) -> Said<()> {
    core.console.logs.lock().await.turn_to(selection);
    Ok(())
}

/// Opens a shell in container `id` on a terminal of `cols` × `rows` (the
/// one before it closed), its output sent on `output` as bytes, as the
/// page acknowledges them; an empty message is its end (after its last
/// bytes, on the same channel). Returns its number.
#[tauri::command]
pub(crate) async fn shell_open(
    core: State<'_, Core>,
    id: String,
    cols: u16,
    rows: u16,
    output: Channel<InvokeResponseBody>,
) -> Said<u64> {
    let mut session = core.console.shell.lock().await;
    close(&mut session);
    let client = core.client().await?;
    let (mut shell, mut bytes) = Shell::open(&client, &id, cols, rows).await.map_err(said)?;
    let number = core.console.sessions.fetch_add(1, Ordering::Relaxed) + 1;
    let (queue, mut queued) = mpsc::channel(QUEUED_MOST);
    let writer = tauri::async_runtime::spawn(async move {
        while let Some(next) = queued.recv().await {
            let done = match next {
                ToShell::Input(data) => shell.write(&data).await,
                ToShell::Resize(cols, rows) => shell.resize(cols, rows).await,
            };
            if done.is_err() {
                break;
            }
        }
    });
    let unacked = Arc::new(Unacked::default());
    let held = Arc::clone(&unacked);
    let pump = tauri::async_runtime::spawn(async move {
        while let Some(Ok(chunk)) = bytes.next().await {
            wait_for_room(&held).await;
            held.bytes.fetch_add(chunk.len(), Ordering::AcqRel);
            if output
                .send(InvokeResponseBody::Raw(chunk.to_vec()))
                .is_err()
            {
                return;
            }
        }
        let _ = output.send(InvokeResponseBody::Raw(Vec::new()));
    });
    *session = Some(Session {
        number,
        queue,
        unacked,
        tasks: [writer, pump],
    });
    Ok(number)
}

/// Once the page has written enough of what's out: no more than
/// [`UNACKED_MOST`] bytes unacknowledged.
async fn wait_for_room(unacked: &Unacked) {
    loop {
        let acked = unacked.acked.notified();
        if unacked.bytes.load(Ordering::Acquire) <= UNACKED_MOST {
            return;
        }
        acked.await;
    }
}

/// Shell `session`'s queue, if it's still the one open (the lock let go
/// before anything waits on it).
async fn queue_of(core: &Core, session: u64) -> Option<mpsc::Sender<ToShell>> {
    core.console
        .shell
        .lock()
        .await
        .as_ref()
        .filter(|open| open.number == session)
        .map(|open| open.queue.clone())
}

/// Types `data` into shell `session`, if it's still the one open.
#[tauri::command]
pub(crate) async fn shell_input(core: State<'_, Core>, session: u64, data: String) -> Said<()> {
    if let Some(queue) = queue_of(&core, session).await {
        let _ = queue.send(ToShell::Input(data.into_bytes())).await;
    }
    Ok(())
}

/// Resizes shell `session`'s terminal, if it's still the one open.
#[tauri::command]
pub(crate) async fn shell_resize(
    core: State<'_, Core>,
    session: u64,
    cols: u16,
    rows: u16,
) -> Said<()> {
    if let Some(queue) = queue_of(&core, session).await {
        let _ = queue.send(ToShell::Resize(cols, rows)).await;
    }
    Ok(())
}

/// The page wrote `bytes` more of shell `session`'s output.
#[tauri::command]
pub(crate) async fn shell_ack(core: State<'_, Core>, session: u64, bytes: usize) -> Said<()> {
    if let Some(open) = core.console.shell.lock().await.as_ref()
        && open.number == session
    {
        // A compare-exchange loop (`fetch_update`'s name moves on nightly:
        // AGENTS.md §10).
        let out = &open.unacked.bytes;
        let mut now = out.load(Ordering::Acquire);
        while let Err(seen) = out.compare_exchange_weak(
            now,
            now.saturating_sub(bytes),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            now = seen;
        }
        open.unacked.acked.notify_waiters();
    }
    Ok(())
}

/// Closes shell `session`, if it's still the one open.
#[tauri::command]
pub(crate) async fn shell_close(core: State<'_, Core>, session: u64) -> Said<()> {
    let mut open = core.console.shell.lock().await;
    if open.as_ref().is_some_and(|open| open.number == session) {
        close(&mut open);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pump waits once too much is out, and goes on once the page
    /// says it wrote enough.
    #[tokio::test]
    async fn output_waits_for_the_page() {
        let unacked = Arc::new(Unacked::default());
        unacked.bytes.store(UNACKED_MOST, Ordering::Release);
        // At the bound: room.
        tokio::time::timeout(Duration::from_secs(1), wait_for_room(&unacked))
            .await
            .unwrap();
        unacked.bytes.store(UNACKED_MOST + 1, Ordering::Release);
        let waiting = tokio::spawn({
            let unacked = Arc::clone(&unacked);
            async move { wait_for_room(&unacked).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "it went on with too much out");
        unacked.bytes.store(10, Ordering::Release);
        unacked.acked.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
    }
}
