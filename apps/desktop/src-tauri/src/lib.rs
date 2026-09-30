//! moyu desktop — the Tauri shell over the `moyu session` sidecar.
//!
//! The webview never touches the sidecar (zero shell permissions, see
//! capabilities/default.json): it invokes the commands below and listens on
//! `moyu://event` / `moyu://session-status` / `moyu://file-drop` /
//! `moyu://file-drag` — everything else is `session::SessionHandle`'s (or,
//! for attachments, `attachments::Attachments`'s) job.

mod attachments;
mod netcfg;
mod session;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use zeroize::Zeroizing;

/// Whether session.rs's `maybe_notify` should fire an OS notification for an
/// inbound message. Defaults to enabled; toggled by the webview's in-app
/// switch (Modals.tsx's identity modal), which persists the choice in its
/// OWN localStorage and re-pushes it here once at every startup — Rust
/// itself keeps no record of the choice across a restart. `pub(crate)` so
/// session.rs's event-forwarding path can read it.
pub(crate) struct NotifyEnabled(pub AtomicBool);

/// The webview's only notification-related command — flips the in-memory
/// toggle `maybe_notify` reads. Deliberately NOT part of the generic
/// `session_request` bridge: it has nothing to do with the sidecar session
/// and must work in every session state, including locked/starting.
#[tauri::command]
fn set_notify(state: tauri::State<'_, NotifyEnabled>, enabled: bool) {
    state.0.store(enabled, Ordering::Relaxed);
}

/// Read the persisted network settings (SOCKS5 proxy / loopback allowance) for
/// the settings panel. Like [`set_notify`], this is deliberately NOT part of
/// the sidecar bridge — it must work in every session state, including before
/// the sidecar has even come up.
#[tauri::command]
fn net_settings_get(app: tauri::AppHandle) -> netcfg::NetSettings {
    netcfg::load(&app)
}

/// Validate, persist, and apply new network settings. Applying = restarting the
/// sidecar so the new `--socks5` / `--dev-allow-loopback` flags take effect
/// (they are spawn-time globals). Returns the normalized settings the UI should
/// now show. An invalid proxy address is rejected here and never reaches a
/// spawn (which it would otherwise crash-loop).
#[tauri::command]
async fn net_settings_set(
    app: tauri::AppHandle,
    handle: tauri::State<'_, session::SessionHandle>,
    socks5: Option<String>,
    allow_loopback: bool,
) -> Result<netcfg::NetSettings, String> {
    let settings = netcfg::NetSettings {
        socks5,
        allow_loopback,
        ..netcfg::NetSettings::default()
    }
    .validated()?;
    netcfg::save(&app, &settings)?;
    handle.restart().await;
    Ok(settings)
}

/// Session-command args that hand the sidecar a local filesystem path or a raw
/// secret key. The GUI never legitimately uses any of them directly —
/// `send`/`post` attach files through a picker- or drop-granted path (see
/// [`pick_attachment`], the drag-drop handler in [`run`], and
/// [`send_attachment`], which resolves the registered path server-side and
/// calls `state.request` itself, bypassing this generic bridge entirely);
/// `download` chooses its own out-dir ([`download_attachment`]); import goes
/// through the dedicated passphrase-safe command. Refusing them on the
/// generic bridge closes an XSS-containment gap: `capabilities/default.json`
/// drops `shell:*` so rendered chat content can't spawn a process, but the
/// sidecar IS a filesystem-capable process — an unfiltered `file`/`out`
/// would let a hypothetical webview compromise read (`send file:/…/id_rsa`
/// → E2E-exfil) or overwrite (`download out:/…/.zshrc`) any local file.
const FORBIDDEN_ARGS: &[&str] = &["file", "out", "import_nsec"];

/// Generic bridge for every session command EXCEPT the passphrase-bearing
/// ones — those must go through [`unlock`] so passphrases never transit the
/// generic (and generically loggable) path. Typed wrappers live in the TS
/// layer (src/lib/session.ts), which is where the wire contract's types are
/// mirrored.
#[tauri::command]
async fn session_request(
    state: tauri::State<'_, session::SessionHandle>,
    cmd: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if matches!(cmd.as_str(), "unlock" | "init" | "join") {
        return Err(format!(
            "invalid_args: {cmd:?} carries a passphrase -- call the dedicated unlock command"
        ));
    }
    if let Some(obj) = args.as_object() {
        if let Some(bad) = FORBIDDEN_ARGS.iter().find(|k| obj.contains_key(**k)) {
            return Err(format!(
                "invalid_args: arg {bad:?} is not permitted from the GUI bridge"
            ));
        }
    }
    state.request(&cmd, args).await
}

/// Dedicated unlock path: the passphrase is wrapped in `Zeroizing` at the
/// IPC boundary (wiped on drop), never logged, and never enters any store.
/// Serializing the command line necessarily makes transient un-wiped heap
/// copies (the json Value + the stdin line) — the same accepted residual as
/// the CLI side's documented residual: process-local freed heap, no
/// worse than the long-standing MOYU_PASSPHRASE env path. (`init`/`join`
/// get their own dedicated commands in the onboarding flow.)
#[tauri::command]
async fn unlock(
    state: tauri::State<'_, session::SessionHandle>,
    passphrase: String,
) -> Result<serde_json::Value, String> {
    let passphrase = Zeroizing::new(passphrase);
    state
        .request("unlock", serde_json::json!({ "passphrase": &*passphrase }))
        .await
}

/// Dedicated onboarding path for `init` (create OR import an identity). Both
/// secrets — the account passphrase AND an imported `nsec` — are wrapped in
/// `Zeroizing` at the IPC boundary, never logged, and never routed through the
/// generic `session_request` bridge (which refuses the `import_nsec` arg
/// wholesale, see [`FORBIDDEN_ARGS`]). The sidecar args are assembled here in
/// Rust, so nothing a webview could smuggle reaches the sidecar unfiltered.
/// The React caller clears its own input state the instant it invokes this,
/// so neither secret ever lands in a store / localStorage. Same accepted
/// residual as [`unlock`]: transient freed heap copies of the serialized line.
///
/// `relays` is the onboarding relay choice: an explicit URL list (custom /
/// cloud preset), or `None` to let the sidecar fall back to its resolved
/// default (the public built-ins for a fresh machine). Not a secret — it is
/// deliberately assembled alongside the secrets here only so the whole `init`
/// argument object is built in one Rust-owned place.
#[tauri::command]
async fn session_init(
    state: tauri::State<'_, session::SessionHandle>,
    passphrase: String,
    import_nsec: Option<String>,
    relays: Option<Vec<String>>,
) -> Result<serde_json::Value, String> {
    let passphrase = Zeroizing::new(passphrase);
    let import_nsec = import_nsec.map(Zeroizing::new);
    let mut args = serde_json::Map::new();
    args.insert("passphrase".into(), serde_json::json!(&*passphrase));
    if let Some(nsec) = &import_nsec {
        args.insert("import_nsec".into(), serde_json::json!(&**nsec));
    }
    if let Some(relays) = relays {
        args.insert("relays".into(), serde_json::json!(relays));
    }
    state.request("init", serde_json::Value::Object(args)).await
}

/// Dedicated path for `join` — split off the generic bridge (which refuses
/// the `join` cmd outright) because in the no-account state it carries a
/// passphrase, wrapped in `Zeroizing` here exactly like [`session_init`]. The
/// same command also serves the unlocked state (join a workspace by code with
/// the live engine), where `passphrase` is `None` and simply omitted — the
/// session's state machine picks the right branch. The invite `code` is not a
/// secret.
#[tauri::command]
async fn session_join(
    state: tauri::State<'_, session::SessionHandle>,
    code: String,
    passphrase: Option<String>,
) -> Result<serde_json::Value, String> {
    let passphrase = passphrase.map(Zeroizing::new);
    let mut args = serde_json::Map::new();
    args.insert("code".into(), serde_json::json!(code));
    if let Some(pp) = &passphrase {
        args.insert("passphrase".into(), serde_json::json!(&**pp));
    }
    state.request("join", serde_json::Value::Object(args)).await
}

/// Initial hydration for the UI (later transitions arrive as
/// `moyu://session-status` events).
#[tauri::command]
fn session_status(state: tauri::State<'_, session::SessionHandle>) -> String {
    state.status()
}

/// `pick_attachment`'s success payload — an opaque token standing in for the
/// real path, plus the display metadata the composer needs (name, size).
#[derive(serde::Serialize)]
struct StagedFile {
    token: u64,
    file_name: String,
    size: u64,
}

/// The per-hash cache dir a downloaded attachment lives in:
/// `<app_data_dir>/attachments/<full 64-hex ciphertext sha256>/`.
/// The FULL hash (not a prefix) is deliberate: `cached_file_in` serves
/// whatever file sits in the bucket WITHOUT re-verification, so the bucket
/// name must be collision-free. A 16-hex prefix would let a peer grind
/// ~2^32 encryptions into two references sharing a bucket — the second
/// would then be served the first's bytes with `cached:true`, silently
/// bypassing the sidecar's sha256 check (merge review, Medium).
fn attachment_dir(app: &tauri::AppHandle, hash: &str) -> Result<PathBuf, String> {
    // The hash reaches us from the webview, which got it from a PEER's imeta
    // tag — attacker-controlled bytes. Joined into a path unvalidated, a
    // "hash" like `../../../../Users/me` would walk `create_dir_all` (and the
    // sidecar's subsequent `out`-dir write) right out of app-data: a remote
    // path-traversal file-write primitive. A real ciphertext sha256 is
    // exactly 64 hex chars; refuse everything else before any path math.
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid_args: malformed attachment hash".to_string());
    }
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("engine: cannot resolve app data dir: {e}"))?;
    Ok(base.join("attachments").join(hash))
}

/// If `dir` already holds a downloaded file (from a previous run or an
/// earlier tick this session), return it. The sidecar's `download` command
/// refuses to overwrite an existing file when `out` is a directory, so
/// callers MUST short-circuit before invoking it again.
fn cached_file_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
}

/// Display name for a resolved path, defaulting to `"file"` for the
/// (practically unreachable — every path here came from a native picker,
/// drop, or our own cache dir) case of a missing or non-UTF-8 file name.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string()
}

/// Native open-file dialog (Rust-side `tauri-plugin-dialog` API only — see
/// the Cargo.toml comment: no `dialog:*` capability is granted to the
/// webview). Mints a token for the picked path and hands back ONLY that
/// token plus display metadata — the webview never sees the path itself.
#[tauri::command]
async fn pick_attachment(
    app: tauri::AppHandle,
    attachments: tauri::State<'_, attachments::Attachments>,
) -> Result<Option<StagedFile>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    app.dialog().file().pick_file(move |picked| {
        if let Some(tx) = tx.take() {
            let _ = tx.send(picked);
        }
    });
    let picked = rx
        .await
        .map_err(|_| "engine: file dialog closed unexpectedly".to_string())?;
    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path.into_path().map_err(|e| format!("engine: {e}"))?;
    let metadata =
        std::fs::metadata(&path).map_err(|e| format!("engine: cannot read picked file: {e}"))?;
    let file_name = file_name_of(&path);
    let size = metadata.len();
    let token = attachments.stage(path);
    Ok(Some(StagedFile {
        token,
        file_name,
        size,
    }))
}

/// Send (DM) or post (channel) a previously staged attachment. `target` is
/// `{"peer": npub}` for a DM or `{"ws": ..., "channel": ...}` for a channel
/// post — deliberately re-validated field-by-field here (not blindly
/// forwarded) so nothing an attacker-controlled `target` payload might smuggle
/// (e.g. a spoofed `"file"` key) ever reaches the sidecar unfiltered: this
/// function builds the sidecar args itself, and the real path (resolved from
/// `token`, never from JS) always wins over anything `target` might contain.
#[tauri::command]
async fn send_attachment(
    state: tauri::State<'_, session::SessionHandle>,
    attachments: tauri::State<'_, attachments::Attachments>,
    target: serde_json::Value,
    token: u64,
    caption: Option<String>,
) -> Result<serde_json::Value, String> {
    let path = attachments
        .take_staged(token)
        .ok_or_else(|| "invalid_args: unknown attachment token".to_string())?;
    let path_str = path.to_string_lossy().into_owned();

    let mut args = serde_json::Map::new();
    let cmd = if let Some(peer) = target.get("peer").and_then(|v| v.as_str()) {
        args.insert("peer".into(), serde_json::Value::String(peer.to_string()));
        "send"
    } else if let (Some(ws), Some(channel)) = (
        target.get("ws").and_then(|v| v.as_str()),
        target.get("channel").and_then(|v| v.as_str()),
    ) {
        args.insert("ws".into(), serde_json::Value::String(ws.to_string()));
        args.insert(
            "channel".into(),
            serde_json::Value::String(channel.to_string()),
        );
        "post"
    } else {
        return Err("invalid_args: target must be {peer} or {ws, channel}".into());
    };
    args.insert("file".into(), serde_json::Value::String(path_str));
    if let Some(cap) = caption.filter(|c| !c.is_empty()) {
        args.insert("message".into(), serde_json::Value::String(cap));
    }
    state.request(cmd, serde_json::Value::Object(args)).await
}

/// Fetch (or serve from the local cache dir) the plaintext bytes behind one
/// `ciphertext_sha256` reference. `group` doubles as the sidecar's `ws` arg —
/// a bare group hex resolves fine there.
#[tauri::command]
async fn download_attachment(
    app: tauri::AppHandle,
    state: tauri::State<'_, session::SessionHandle>,
    attachments: tauri::State<'_, attachments::Attachments>,
    group: String,
    hash: String,
) -> Result<serde_json::Value, String> {
    let dir = attachment_dir(&app, &hash)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("engine: cannot create cache dir: {e}"))?;

    if let Some(path) = cached_file_in(&dir) {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file_name = file_name_of(&path);
        attachments.record_download(hash.clone(), path.clone());
        return Ok(serde_json::json!({
            "group": group,
            "file_name": file_name,
            "media_type": serde_json::Value::Null,
            "size": size,
            "path": path.display().to_string(),
            "cached": true,
        }));
    }

    let receipt = state
        .request(
            "download",
            serde_json::json!({ "ws": group, "hash": hash, "out": dir.display().to_string() }),
        )
        .await?;
    if let Some(path) = receipt.get("path").and_then(|v| v.as_str()) {
        attachments.record_download(hash.clone(), PathBuf::from(path));
    }
    Ok(receipt)
}

/// Reveal a previously downloaded attachment in the OS file manager. Looks
/// up the in-memory `downloads` cache first, falling back to a fresh scan of
/// the deterministic cache dir (covers a process restart between download
/// and reveal, where the in-memory map is empty but the file is still on
/// disk). NEVER accepts a path from JS — only `hash` (`group` is accepted for
/// wire compatibility with the existing frontend call, see attachments.rs's
/// module doc for why the lookup itself no longer needs it).
#[tauri::command]
fn reveal_attachment(
    app: tauri::AppHandle,
    attachments: tauri::State<'_, attachments::Attachments>,
    #[allow(unused_variables)] group: String,
    hash: String,
) -> Result<(), String> {
    let path = match attachments.download_path(&hash) {
        Some(p) => p,
        None => {
            let dir = attachment_dir(&app, &hash)?;
            let found = cached_file_in(&dir)
                .ok_or_else(|| "not_found: attachment not downloaded yet".to_string())?;
            attachments.record_download(hash, found.clone());
            found
        }
    };
    opener::reveal(&path).map_err(|e| format!("engine: {e}"))
}

/// True for the app's own origin — the only place top-level navigation is
/// allowed to land. In dev the frontend is served from `localhost:<port>`;
/// in a bundle it loads from `tauri(.)localhost` / the custom scheme. The SPA
/// itself never navigates (React routes in-memory), so in practice the only
/// navigations that reach [`run`]'s guard are user clicks on rendered links.
fn is_internal_origin(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "tauri" | "ipc" | "asset")
        || matches!(url.host_str(), Some("localhost") | Some("tauri.localhost"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Diagnostics for the shell process itself (the sidecar has its own
    // RUST_LOG). `RUST_LOG` selects the level, default `info`, stderr only.
    // Never logs message content or secrets — only lifecycle and framing.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            app.manage(NotifyEnabled(AtomicBool::new(true)));

            // Best-effort permission priming (macOS/Windows may prompt the
            // user the first time; Linux desktop notifications typically
            // need none). A denied/still-prompt state just means
            // `maybe_notify`'s `.show()` calls silently no-op later — never
            // fatal to startup.
            {
                use tauri::plugin::PermissionState;
                use tauri_plugin_notification::NotificationExt;
                if app
                    .notification()
                    .permission_state()
                    .unwrap_or(PermissionState::Prompt)
                    == PermissionState::Prompt
                {
                    let _ = app.notification().request_permission();
                }
            }

            // Build the main window in Rust (not tauri.conf.json) so it can
            // carry the on_navigation guard (builder-time only). A rendered
            // message link to an attacker origin must NEVER navigate the
            // webview — that would let a pixel-perfect fake "re-enter your
            // passphrase" page capture it. External links are handed to the
            // system browser instead.
            // (drag-drop interception is enabled by default on the builder,
            // so HTML5 `drop` never reaches the DOM — the on_window_event
            // handler registered below is the only drop path.)
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("moyu")
                    .inner_size(1100.0, 720.0)
                    .min_inner_size(720.0, 480.0)
                    .on_navigation(|url| {
                        if is_internal_origin(url) {
                            return true;
                        }
                        // External link click: open in the system browser, never
                        // in-app. `opener` is a Rust-side crate — no webview
                        // capability is granted for this.
                        let _ = opener::open_browser(url.as_str());
                        false
                    })
                    .build()?;

            app.manage(attachments::Attachments::default());

            // Drag-drop: the other channel (besides the native pick dialog in
            // `pick_attachment`) through which a filesystem path enters the
            // app. Mint a token per dropped path here and hand the webview
            // nothing but that token + display metadata — same contract as
            // the picker (see attachments.rs's module doc).
            let drag_app = app.handle().clone();
            window.on_window_event(move |event| {
                let tauri::WindowEvent::DragDrop(drag_event) = event else {
                    return;
                };
                match drag_event {
                    tauri::DragDropEvent::Enter { .. } | tauri::DragDropEvent::Over { .. } => {
                        let _ = drag_app
                            .emit("moyu://file-drag", serde_json::json!({ "hovering": true }));
                    }
                    tauri::DragDropEvent::Drop { paths, .. } => {
                        let _ = drag_app
                            .emit("moyu://file-drag", serde_json::json!({ "hovering": false }));
                        let attachments = drag_app.state::<attachments::Attachments>();
                        let files: Vec<serde_json::Value> = paths
                            .iter()
                            .filter_map(|p| {
                                let metadata = std::fs::metadata(p).ok()?;
                                if !metadata.is_file() {
                                    return None;
                                }
                                let file_name = p.file_name()?.to_str()?.to_string();
                                let size = metadata.len();
                                let token = attachments.stage(p.clone());
                                Some(serde_json::json!({
                                    "token": token,
                                    "file_name": file_name,
                                    "size": size,
                                }))
                            })
                            .collect();
                        let _ = drag_app.emit("moyu://file-drop", files);
                    }
                    tauri::DragDropEvent::Leave => {
                        let _ = drag_app
                            .emit("moyu://file-drag", serde_json::json!({ "hovering": false }));
                    }
                    _ => {}
                }
            });

            let handle = session::start(app.handle().clone());
            app.manage(handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            session_request,
            unlock,
            session_init,
            session_join,
            session_status,
            set_notify,
            net_settings_get,
            net_settings_set,
            pick_attachment,
            send_attachment,
            download_attachment,
            reveal_attachment,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
