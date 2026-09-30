//! The session-protocol client: spawns the bundled `moyu session` sidecar,
//! owns its stdin/stdout, correlates receipts to in-flight requests, and
//! re-emits async events into the webview as Tauri events.
//!
//! Lives ENTIRELY in the Rust core — the webview holds zero shell
//! permissions and only ever sees our `#[tauri::command]`s plus two Tauri
//! event channels:
//!
//! - `moyu://event`          — every async session event, verbatim
//!   (`{"v":1,"type":"event","event":"message",...}`)
//! - `moyu://session-status` — lifecycle: `{state}` from hello frames
//!   (`no-account`/`locked`/`unlocked`), plus `down` when the child dies
//!   and `restarting` while the supervisor respawns it.
//!
//! Wire contract: crates/moyu-cli/src/session/protocol.rs. Receipts carry
//! the echoed `id`; events never do. The supervisor restarts a dead child
//! with capped exponential backoff — a fresh child starts LOCKED, so the UI
//! returns to its unlock screen (`down` → `hello locked`); the passphrase is
//! deliberately never cached for auto-reunlock.
//!
//! Shutdown needs no kill hook: when this process exits, the child's stdin
//! pipe closes and `moyu session` exits itself on EOF (its documented
//! orderly shutdown path).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

/// How long a single request may wait for its receipt. Generous: an `init`
/// or a first `send` to a fresh peer does real relay round-trips.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>;

/// The handle the rest of the app (the `#[tauri::command]`s) talks through.
/// Managed as Tauri state.
pub struct SessionHandle {
    to_child: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicU64,
    status_rx: watch::Receiver<String>,
    restart_tx: mpsc::Sender<()>,
}

impl SessionHandle {
    /// Send one command and await its receipt. `Ok(data)` for `ok:true`
    /// receipts; `Err("<code>: <error>")` for `ok:false` — the TS layer
    /// splits the stable code back off the front.
    pub async fn request(
        &self,
        cmd: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line =
            serde_json::json!({ "id": format!("r{id}"), "cmd": cmd, "args": args }).to_string();
        if self.to_child.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err("session_down: session process is not running".into());
        }

        let receipt = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(_)) => return Err("session_down: session process exited".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err("timeout: no receipt from the session".into());
            }
        };
        if receipt.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(receipt
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        } else {
            let code = receipt
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("engine");
            let error = receipt
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(format!("{code}: {error}"))
        }
    }

    pub fn status(&self) -> String {
        self.status_rx.borrow().clone()
    }

    /// Ask the supervisor to kill the current sidecar and respawn it — the way
    /// changed network settings (proxy / loopback) take effect, since those are
    /// spawn-time global flags. The fresh child re-reads `net-settings.json`,
    /// so this is fire-and-forget: a dropped signal (channel full) is harmless
    /// because a restart is already pending. Restarting always drops the child
    /// back to LOCKED / NO-ACCOUNT, so the UI returns to its unlock/onboarding
    /// screen — the passphrase is never cached for auto-reunlock.
    pub async fn restart(&self) {
        let _ = self.restart_tx.try_send(());
    }
}

/// Spawn the supervisor and hand back the app-facing handle. Called once
/// from `setup()`.
pub fn start(app: AppHandle) -> SessionHandle {
    let (to_child, from_app) = mpsc::channel::<String>(64);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (status_tx, status_rx) = watch::channel("starting".to_string());
    // Capacity 1: a restart already queued makes a second request redundant —
    // `try_send` drops it, coalescing bursts (e.g. save-twice) into one respawn.
    let (restart_tx, restart_rx) = mpsc::channel::<()>(1);

    tauri::async_runtime::spawn(supervise(
        app,
        from_app,
        pending.clone(),
        status_tx,
        restart_rx,
    ));

    SessionHandle {
        to_child,
        pending,
        next_id: AtomicU64::new(1),
        status_rx,
        restart_tx,
    }
}

/// Why the inner pump loop ended — governs whether we respawn and how long we
/// wait first.
enum LoopExit {
    /// All `SessionHandle` senders dropped: the app is shutting down. Stop.
    AppShutdown,
    /// The child crashed / exited on its own. Respawn after backoff.
    ChildDied,
    /// A restart was requested (settings changed). Respawn promptly with the
    /// backoff reset — this is deliberate, not a failure.
    Restart,
}

/// Spawn → pump → (on death/restart) fail pending + backoff → respawn, forever.
/// Each spawn re-reads `net-settings.json`, so a settings change followed by a
/// [`SessionHandle::restart`] brings the new proxy / loopback flags into effect.
async fn supervise(
    app: AppHandle,
    mut from_app: mpsc::Receiver<String>,
    pending: Pending,
    status_tx: watch::Sender<String>,
    mut restart_rx: mpsc::Receiver<()>,
) {
    let mut backoff_ms: u64 = 250;
    loop {
        // Global connectivity flags (`--socks5`, `--dev-allow-loopback`)
        // resolved fresh each spawn — the whole point of the restart path.
        let argv = crate::netcfg::load(&app).session_args();
        let spawned = app
            .shell()
            .sidecar("moyu")
            .map(|c| c.args(argv))
            .and_then(|c| c.spawn());
        let (mut child_rx, mut child) = match spawned {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!("sidecar spawn failed: {e}");
                set_status(&app, &status_tx, "down");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(4000);
                continue;
            }
        };
        set_status(&app, &status_tx, "starting");

        // stdout may arrive in arbitrary chunks — accumulate and split on
        // newlines ourselves; one stray partial line must never be parsed.
        let mut buf: Vec<u8> = Vec::new();
        let exit = loop {
            tokio::select! {
                _ = restart_rx.recv() => {
                    // Settings changed: end the pump so the child is killed
                    // (below) and the loop respawns with fresh flags.
                    break LoopExit::Restart;
                }
                maybe_line = from_app.recv() => {
                    match maybe_line {
                        Some(line) => {
                            let mut framed = line.into_bytes();
                            framed.push(b'\n');
                            if child.write(&framed).is_err() {
                                break LoopExit::ChildDied;
                            }
                        }
                        // All SessionHandle senders dropped: the app is
                        // shutting down. Dropping `child` closes its stdin,
                        // and `moyu session` exits on EOF.
                        None => break LoopExit::AppShutdown,
                    }
                }
                ev = child_rx.recv() => {
                    match ev {
                        Some(CommandEvent::Stdout(bytes)) => {
                            buf.extend_from_slice(&bytes);
                            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=pos).collect();
                                handle_frame(&app, &pending, &status_tx, &line[..line.len()-1]).await;
                            }
                        }
                        Some(CommandEvent::Stderr(bytes)) => {
                            tracing::info!(target: "moyu_session", "{}", String::from_utf8_lossy(&bytes).trim_end());
                        }
                        Some(CommandEvent::Terminated(_)) | None => break LoopExit::ChildDied,
                        _ => {}
                    }
                }
            }
        };
        if let LoopExit::AppShutdown = exit {
            // Drop `child` → its stdin closes → `moyu session` exits on EOF
            // (the documented orderly shutdown), no kill needed.
            return;
        }
        // Crash or intentional restart: make sure the process is gone before we
        // respawn. On a crash it has already exited (kill → harmless Err); on a
        // restart this is what actually terminates it. SIGKILL is safe —
        // SQLCipher recovers from an uncommitted journal on next open and the
        // OS releases the DB lock as the process dies.
        let _ = child.kill();

        // Child gone (crash or intentional restart): every in-flight request
        // fails now (its receipt can never arrive); the UI drops to the
        // unlock/onboarding screen on reconnect.
        for (_, tx) in pending.lock().await.drain() {
            let _ = tx.send(serde_json::json!({
                "ok": false, "code": "session_down", "error": "session process exited",
            }));
        }
        set_status(&app, &status_tx, "down");
        match exit {
            // Deliberate restart: reset backoff and settle just long enough for
            // the killed child's DB lock/pipes to be released before respawn.
            LoopExit::Restart => {
                backoff_ms = 250;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            // Crash: exponential backoff so a persistently-broken sidecar (e.g.
            // a bad binary) doesn't spin the CPU.
            _ => {
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(4000);
            }
        }
        set_status(&app, &status_tx, "restarting");
    }
}

fn set_status(app: &AppHandle, status_tx: &watch::Sender<String>, state: &str) {
    let _ = status_tx.send(state.to_owned());
    let _ = app.emit(
        "moyu://session-status",
        serde_json::json!({ "state": state }),
    );
}

/// Route one stdout line: receipts resolve their pending request; hello and
/// events go to the webview. NEVER log frame contents wholesale — a receipt
/// is fine, but the unlock command's echo... never appears on stdout at all
/// (commands go TO the child), so frames here are safe to inspect, and we
/// still only log parse failures' length, not bytes.
async fn handle_frame(
    app: &AppHandle,
    pending: &Pending,
    status_tx: &watch::Sender<String>,
    line: &[u8],
) {
    if line.is_empty() {
        return;
    }
    let value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("unparseable session frame ({} bytes): {e}", line.len());
            return;
        }
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("receipt") => {
            let key = value
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.strip_prefix('r'))
                .and_then(|s| s.parse::<u64>().ok());
            let waiter = match key {
                Some(k) => pending.lock().await.remove(&k),
                None => None,
            };
            match waiter {
                Some(tx) => {
                    let _ = tx.send(value);
                }
                // An uncorrelated receipt (e.g. the session's own reply to a
                // malformed line with id:null) — nothing waits on it.
                None => tracing::debug!("receipt with no pending request"),
            }
        }
        Some("hello") => {
            if let Some(state) = value.get("state").and_then(|v| v.as_str()) {
                let _ = status_tx.send(state.to_owned());
            }
            let _ = app.emit("moyu://session-status", value);
        }
        Some("event") => {
            maybe_notify(app, &value);
            let _ = app.emit("moyu://event", value);
        }
        Some("fatal") => {
            let _ = app.emit(
                "moyu://session-status",
                serde_json::json!({ "state": "down", "fatal": value }),
            );
        }
        _ => tracing::warn!("frame with unknown type"),
    }
}

/// Fire an OS notification for an inbound `message` event when the
/// main window isn't focused — the "already looking at it" signal is focus,
/// not visibility (a partially-obscured-but-focused window still counts as
/// seen). Deliberately narrower than the frontend's `MESSAGE_EVENTS` set
/// (message/reaction/agent_op/agent_activity, store.ts): reactions and bot
/// events are usually noise at the OS-popup level, so only a real chat
/// message triggers one. Own sends never round-trip as a live `message`
/// event (store.ts's optimistic-row comment), so this can never self-notify.
fn maybe_notify(app: &AppHandle, value: &serde_json::Value) {
    if value.get("event").and_then(|v| v.as_str()) != Some("message") {
        return;
    }
    if !app
        .state::<crate::NotifyEnabled>()
        .0
        .load(Ordering::Relaxed)
    {
        return;
    }
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_focused().unwrap_or(true) {
        return;
    }

    // Reconnect catch-up drains a whole offline backlog as ordinary live
    // `message` events in one burst — without a floor between popups that's
    // one OS notification per missed message. One per 1.5s is enough to say
    // "something arrived"; the unread badges carry the exact count.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_NOTIFY_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 1_500 {
        return;
    }
    LAST_NOTIFY_MS.store(now_ms, Ordering::Relaxed);

    // `sender_name` is peer-controlled (Message.tsx's `senderName` doc): a
    // stranger could set it to the reserved self label to make a notification
    // read as if it came from "you" — same guard the frontend applies before
    // ever displaying it.
    const SELF_LABEL: &str = "你";
    let sender_name = value
        .get("sender_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != SELF_LABEL);
    // Every message event carries a `channel` (a DM's unenveloped body
    // decodes to the default `general` slug), so channel presence can't
    // distinguish a DM — the sidecar's explicit `dm` flag does (absent on an
    // older sidecar → false, i.e. the channel title, never a wrong sender).
    let dm = value.get("dm").and_then(|v| v.as_bool()).unwrap_or(false);
    let channel = value.get("channel").and_then(|v| v.as_str());
    let title = match (dm, sender_name, channel) {
        (true, Some(name), _) => name.to_string(),
        (false, _, Some(ch)) => format!("#{ch}"),
        _ => value
            .get("sender")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(10).collect::<String>())
            .unwrap_or_else(|| "moyu".to_string()),
    };
    let body = truncate_chars(
        value.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        120,
    );

    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Epoch-millis of the last OS notification shown — the coalescing floor
/// `maybe_notify` applies. Plain relaxed atomics: worst case a racing pair of
/// events yields one extra popup, which is exactly the failure mode the
/// floor exists to bound.
static LAST_NOTIFY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Char-safe truncation (never split a multi-byte UTF-8 char, which
/// `body[..n]` byte-slicing would risk on non-ASCII text like Chinese).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}
