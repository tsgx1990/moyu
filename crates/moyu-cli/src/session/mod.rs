//! `moyu session` — the long-lived stdio command/event loop a GUI shell
//! drives (JSON commands in on stdin, JSONL receipts + events out on stdout;
//! see `protocol` for the framing and `writer` for the stdout discipline).
//!
//! One session = one process = the single owner of the account's engine /
//! `AppClient` / SQLCipher store, exactly like `chat`/`tui` — which is the
//! point: a GUI spawning one session avoids the cross-process concurrent-DB
//! question entirely.
//!
//! # State machine
//!
//! - **no-account** (`active-account.txt` absent): `init` / `join` (they
//!   choose the passphrase, so they transition straight to unlocked) and
//!   `whoami` are valid; everything else ⇒ `code:no_account`.
//! - **locked** (account present, no engine held): `unlock`, `whoami`, and
//!   the engine-less locals; engine-touching commands ⇒ `code:locked`. A
//!   wrong passphrase is a per-command failure (`bad_passphrase`) — the
//!   session stays alive and the GUI retries; consecutive failures back off
//!   linearly (Argon2id is deliberately expensive; don't let a stdin flood
//!   burn CPU).
//! - **unlocked**: everything is valid. `lock` drops the engine (zeroizing
//!   the derived secret with it) and returns to locked.
//!
//! # Event pump
//!
//! While unlocked, a 2s `ops::sync_tick` runs under `tokio::select!` against
//! command dispatch (single owner, never concurrent — the same shape as
//! `repl.rs`/`tui.rs`), streaming inbound traffic as framed `event` objects.
//! A tick failure must NOT kill the session (unlike one-shot `recv`, where
//! `?` is correct): it surfaces as an `event:"sync_error"` diagnostic — moyu
//! runs no MDK auto-reconnect runtime, so a long-lived session's relay
//! sockets can silently drop; the GUI reacts with `catchup` (e.g. on an OS
//! network-change signal) or by re-locking. The roster baseline for
//! governance diffs is (re)snapshotted on every unlock/init/join transition
//! and cleared on `lock`, so a re-unlock never diffs against stale state.
//!
//! # stdout purity
//!
//! The session leans on the print-free `ops` layer and sets the process-wide
//! JSON mode ON at startup: the only `ops` writer gated on it
//! (`ops::progress`) then routes to stderr. `session::run` itself never
//! returns `Err` (it renders its own `fatal` frame and exits), so `main()`'s
//! bare-object `--json` error path can never splice into the framed stream.

mod protocol;
mod writer;

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;

use zeroize::{Zeroize, Zeroizing};

use moyu_core::engine::{AppClient, MoyuEngine};
use moyu_core::store::MoyuStore;

use crate::{ops, output};
use protocol::{ErrCode, Incoming};
use writer::SessionWriter;

/// The globals the session inherits from the CLI invocation (`--data-dir`,
/// resolved relays, `--dev-allow-loopback`, `--socks5`, `--blossom`).
pub(crate) struct SessionOpts {
    pub data_dir: PathBuf,
    pub relays: Vec<String>,
    pub allow_loopback: bool,
    pub socks5: Option<SocketAddr>,
    pub blossom: Option<String>,
}

/// The exact command set `dispatch` implements — `hello.capabilities` is
/// generated from this, so a GUI can feature-gate truthfully. Extend in
/// lockstep with `dispatch`.
const CAPABILITIES: &[&str] = &[
    // any state
    "whoami",
    "unlock",
    "lock",
    "catchup",
    "relay_list",
    "relay_add",
    "relay_forget",
    "deny",
    // no-account state only (init/join transition straight to unlocked)
    "init",
    "join",
    // unlocked only
    "send",
    "history",
    "conversations",
    "post",
    "reply",
    "react",
    "download",
    "search",
    "op",
    "activity",
    "invite",
    "requests",
    "approve",
    "add",
    "keypackage",
    "workspace_new",
    "workspace_add",
    "workspace_members",
    "workspace_rename",
    "workspace_kick",
    "workspace_leave",
    "admin_list",
    "admin_add",
    "admin_remove",
    "channel_new",
    "channel_new_private",
    "channel_invite",
    "channel_rename",
    "channel_archive",
];

/// Everything an unlocked session owns. Boxed inside [`State`] (clippy
/// large_enum_variant: the engine/client/store dwarf the other variants).
struct Unlocked {
    label: String,
    engine: MoyuEngine,
    client: AppClient,
    store: MoyuStore,
}

enum State {
    NoAccount,
    Locked { label: String },
    Unlocked(Box<Unlocked>),
}

/// The governance-diff baseline the pump diffs each tick against (see
/// `snapshot_rosters`): `None` until the first post-unlock snapshot, reset by
/// `lock`.
type Rosters = Option<std::collections::HashMap<String, crate::domain::WsRoster>>;

/// (Re)snapshot the roster baseline after a transition INTO unlocked, so the
/// pump's first diff is against the on-disk state at unlock time — never an
/// empty map (which would announce every existing member as freshly joined)
/// and never a stale pre-lock baseline.
fn reset_roster_baseline(state: &State, prev_rosters: &mut Rosters) {
    *prev_rosters = match state {
        State::Unlocked(u) => crate::domain::snapshot_rosters(&u.engine, &u.client, &u.label),
        _ => None,
    };
}

/// One pump pass: run the shared `ops::sync_tick` with this session's
/// handles, streaming each inbound event as a framed `event` object.
async fn pump_tick(
    u: &mut Unlocked,
    opts: &SessionOpts,
    prev_rosters: &mut Rosters,
    writer: &SessionWriter,
) -> anyhow::Result<bool> {
    ops::sync_tick(
        &u.engine,
        &mut u.client,
        &mut u.store,
        &u.label,
        opts.allow_loopback,
        prev_rosters,
        &mut |ev| writer.event(ev.to_json()),
    )
    .await
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::NoAccount => "no-account",
            State::Locked { .. } => "locked",
            State::Unlocked(_) => "unlocked",
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            State::NoAccount => None,
            State::Locked { label } => Some(label),
            State::Unlocked(u) => Some(&u.label),
        }
    }

    fn account_json(&self) -> serde_json::Value {
        match self.label() {
            None => serde_json::json!({ "present": false }),
            Some(label) => {
                let npub = moyu_core::identity::npub_from_hex(label).ok();
                serde_json::json!({ "present": true, "label": label, "npub": npub })
            }
        }
    }
}

pub(crate) async fn run(opts: SessionOpts) -> anyhow::Result<()> {
    // Route `ops::progress` lines to stderr (see module doc) — the session's
    // stdout carries ONLY framed protocol objects.
    output::set_json_mode(true);

    let writer = SessionWriter;
    let mut state = match moyu_core::store::read_active_account_label(&opts.data_dir) {
        Ok(Some(label)) => State::Locked { label },
        Ok(None) => State::NoAccount,
        Err(e) => {
            writer.fatal("store", &format!("cannot read account state: {e:#}"));
            std::process::exit(1);
        }
    };
    writer.hello(state.name(), state.account_json(), CAPABILITIES);

    // Blocking stdin reader on its own OS thread → mpsc, the same shape
    // repl.rs/tui.rs use for terminal input; the A-5 pump will `select!`
    // this channel against the 2s sync tick.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut unlock_failures: u32 = 0;
    let mut prev_rosters: Rosters = None;
    // The pump interval exists from the start but its select! branch is
    // gated on the unlocked state; tokio's first tick resolves immediately,
    // so the moment an unlock lands, an initial catch-up sync fires.
    let mut poll = tokio::time::interval(crate::domain::RECV_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            maybe_line = rx.recv() => {
                let Some(mut line) = maybe_line else { break };
                if line.trim().is_empty() {
                    line.zeroize();
                    continue;
                }
                match serde_json::from_str::<Incoming>(&line) {
                    Ok(incoming) => {
                        dispatch(
                            incoming,
                            &mut state,
                            &writer,
                            &opts,
                            &mut unlock_failures,
                            &mut prev_rosters,
                        )
                        .await;
                    }
                    Err(e) => {
                        writer.receipt_err(
                            None,
                            ErrCode::InvalidArgs,
                            &format!("invalid command JSON: {e}"),
                        );
                    }
                }
                // The raw line may have carried an unlock passphrase — wipe
                // every line uniformly rather than special-casing.
                //
                // Documented residual: serde's parse makes
                // one more owned String copy of args.passphrase inside the
                // `Incoming.args` Value, dropped un-wiped at end of dispatch.
                // Wiping it would need a custom Deserialize; the exposure is
                // process-local freed heap, no worse than the long-standing
                // MOYU_PASSPHRASE env path — accepted for v1.
                line.zeroize();
            }
            _ = poll.tick(), if matches!(state, State::Unlocked(_)) => {
                let State::Unlocked(u) = &mut state else {
                    unreachable!("branch precondition");
                };
                if let Err(e) = pump_tick(u, &opts, &mut prev_rosters, &writer).await {
                    // See module doc: a tick failure is a diagnostic event,
                    // never session death.
                    writer.event(serde_json::json!({
                        "v": output::SCHEMA_VERSION,
                        "event": "sync_error",
                        "error": format!("{e:#}"),
                    }));
                }
            }
        }
    }

    // stdin EOF = the parent closed us down: the orderly exit (0), still
    // announced as a final frame so a log tail shows why the stream ended.
    writer.fatal("stdin_closed", "input stream closed");
    Ok(())
}

/// Pull a required string field out of a command's `args`.
fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("args.{key} (string) is required"))
}

// ---------------------------------------------------------------------------
// A-4b: small shared plumbing for the per-command helpers below. Every
// unlocked-state command (`post`, `reply`, ... `channel_archive`) is a
// `do_*` function that parses its own `args`, calls straight into the
// print-free `ops` layer, and returns a [`CmdResult`] — `Ok(data)` becomes
// the receipt's `data`, `Err((code, msg))` becomes its `code`/`error`. This
// keeps `dispatch`'s match arms one-to-three lines each (parse the enclosing
// state, call the helper, `respond`) instead of duplicating the
// `receipt_ok`/`receipt_err` plumbing ~30 times.
// ---------------------------------------------------------------------------

/// What every per-command helper below returns: the receipt `data` on
/// success, or the `(code, message)` pair `respond` turns into a
/// `receipt_err` on failure.
type CmdResult = Result<serde_json::Value, (ErrCode, String)>;

/// Turn a [`CmdResult`] into the one receipt it always was: `Ok` → success,
/// `Err` → failure. The single place `dispatch`'s new arms funnel through.
fn respond(writer: &SessionWriter, id: Option<&str>, result: CmdResult) {
    match result {
        Ok(data) => writer.receipt_ok(id, data),
        Err((code, msg)) => writer.receipt_err(id, code, &msg),
    }
}

fn invalid_args(msg: impl Into<String>) -> (ErrCode, String) {
    (ErrCode::InvalidArgs, msg.into())
}

/// A `resolve_workspace`/`resolve_group_id`/`resolve_channel_target` failure
/// -- always "this name doesn't match anything locally", never a relay/store
/// fault, so it always maps to [`ErrCode::NotFound`]. Generic over `Display`
/// since the resolvers return `anyhow::Error` while a couple of local-only
/// helpers (`MoyuStore::open`, `MoyuEngine::client`) return `MoyuError`.
fn not_found(e: impl std::fmt::Display) -> (ErrCode, String) {
    (ErrCode::NotFound, format!("{e:#}"))
}

/// Every other `ops`/engine/relay/store failure (see [`not_found`] for why
/// this is generic too).
fn engine_err(e: impl std::fmt::Display) -> (ErrCode, String) {
    (ErrCode::Engine, format!("{e:#}"))
}

/// [`arg_str`] with its error already shaped as a [`CmdResult`] failure, so
/// a `do_*` helper can chain it with `?`.
fn arg_str_e(args: &serde_json::Value, key: &str) -> Result<String, (ErrCode, String)> {
    arg_str(args, key).map_err(invalid_args)
}

async fn dispatch(
    incoming: Incoming,
    state: &mut State,
    writer: &SessionWriter,
    opts: &SessionOpts,
    unlock_failures: &mut u32,
    prev_rosters: &mut Rosters,
) {
    let id = incoming.id.as_deref();
    let args = &incoming.args;

    match incoming.cmd.as_str() {
        // ------------------------------------------------------------- any state
        "whoami" => {
            writer.receipt_ok(id, state.account_json());
        }

        "lock" => {
            if let State::Unlocked(u) = state {
                let label = u.label.clone();
                // Dropping the engine drops the Argon2id-derived secret
                // (Zeroizing) with it.
                *state = State::Locked { label };
            }
            *prev_rosters = None;
            writer.receipt_ok(id, serde_json::json!({ "state": state.name() }));
        }

        // Force an immediate pump pass (the GUI calls this on an OS
        // network-change signal, or after a `sync_error`, instead of waiting
        // out the 2s interval). Events stream out before the receipt.
        "catchup" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            match pump_tick(u, opts, prev_rosters, writer).await {
                Ok(progressed) => {
                    writer.receipt_ok(id, serde_json::json!({ "progressed": progressed }));
                }
                Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
            }
        }

        "unlock" => {
            match state {
                State::NoAccount => {
                    writer.receipt_err(
                        id,
                        ErrCode::NoAccount,
                        "no account yet -- run init or join first",
                    );
                    return;
                }
                State::Unlocked(_) => {
                    // Idempotent: already open.
                    writer.receipt_ok(id, serde_json::json!({ "state": "unlocked" }));
                    return;
                }
                State::Locked { .. } => {}
            }
            let passphrase = match arg_str(args, "passphrase") {
                Ok(p) => Zeroizing::new(p),
                Err(e) => {
                    writer.receipt_err(id, ErrCode::InvalidArgs, &e);
                    return;
                }
            };
            // Linear backoff on consecutive failures: one in-flight unlock at
            // a time (dispatch is serial), each retry after a failure waits
            // `failures * 500ms` before burning another Argon2id derivation.
            if *unlock_failures > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * (*unlock_failures as u64).min(8),
                ))
                .await;
            }
            let State::Locked { label } = state else {
                unreachable!("checked above");
            };
            let label = label.clone();
            // One transient String copy, consumed by `build_engine` →
            // `into_bytes` → `Zeroizing<Vec<u8>>` inside the secret store;
            // the `Zeroizing` original wipes on drop at the end of this arm.
            let engine = crate::domain::build_engine(
                &opts.data_dir,
                &opts.relays,
                opts.allow_loopback,
                opts.socks5,
                passphrase.to_string(),
            );
            // The first decrypt happens inside `client()`: a wrong passphrase
            // surfaces here. Any failure drops the engine (zeroizing the
            // derived key) and reports bad_passphrase with the underlying
            // error text -- the GUI shows it and retries.
            match engine.client(&label).await {
                Ok(client) => match MoyuStore::open(&opts.data_dir, &label) {
                    Ok(mut store) => {
                        *unlock_failures = 0;
                        // Same opportunistic freshness check `open_session`
                        // runs; the A-5 pump repeats it every tick.
                        let mut client = client;
                        crate::domain::refresh_keypackage(&mut client, &mut store).await;
                        let npub = moyu_core::identity::npub_from_hex(&label).ok();
                        *state = State::Unlocked(Box::new(Unlocked {
                            label: label.clone(),
                            engine,
                            client,
                            store,
                        }));
                        reset_roster_baseline(state, prev_rosters);
                        writer.receipt_ok(
                            id,
                            serde_json::json!({
                                "state": "unlocked", "label": label, "npub": npub,
                            }),
                        );
                    }
                    Err(e) => {
                        writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}"));
                    }
                },
                Err(e) => {
                    *unlock_failures += 1;
                    writer.receipt_err(id, ErrCode::BadPassphrase, &format!("{e:#}"));
                }
            }
        }

        // Pure local `config.json` reads/writes -- no account/passphrase/relay
        // connection needed, so (like `whoami`) these work in every state.
        "relay_list" => {
            writer.receipt_ok(
                id,
                serde_json::json!({ "relays": ops::relay_list(&opts.data_dir) }),
            );
        }

        "relay_add" => {
            let url = match arg_str(args, "url") {
                Ok(u) => u,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            // Same loopback guard as the one-shot CLI (`cmd_relay`): a loopback
            // relay persisted here would break every later sidecar spawn.
            if let Err(e) = crate::domain::reject_loopback_relays(
                std::slice::from_ref(&url),
                opts.allow_loopback,
            ) {
                return writer.receipt_err(id, ErrCode::InvalidArgs, &format!("{e:#}"));
            }
            match ops::relay_add(&opts.data_dir, &url) {
                Ok(relays) => {
                    writer.receipt_ok(id, serde_json::json!({ "added": url, "relays": relays }))
                }
                Err(e) => writer.receipt_err(id, ErrCode::InvalidArgs, &format!("{e:#}")),
            }
        }

        "relay_forget" => {
            let url = match arg_str(args, "url") {
                Ok(u) => u,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            match ops::relay_forget(&opts.data_dir, &url) {
                Ok(relays) => {
                    writer.receipt_ok(id, serde_json::json!({ "forgot": url, "relays": relays }))
                }
                Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
            }
        }

        // `deny`'s v1 is a purely local dismissal (see `ops::deny`'s doc
        // comment) -- it touches no engine/store, so it needs no account
        // state at all, exactly like `whoami`.
        "deny" => {
            let who = match arg_str(args, "who") {
                Ok(w) => w,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            let receipt = ops::deny(who);
            writer.receipt_ok(id, receipt.to_json());
        }

        // ------------------------------------------------------- no-account state
        "init" => {
            if !matches!(state, State::NoAccount) {
                writer.receipt_err(id, ErrCode::InvalidArgs, "account already exists");
                return;
            }
            match do_init(opts, args).await {
                Ok((unlocked, data)) => {
                    *state = State::Unlocked(Box::new(unlocked));
                    reset_roster_baseline(state, prev_rosters);
                    writer.receipt_ok(id, data);
                }
                Err((code, msg)) => writer.receipt_err(id, code, &msg),
            }
        }

        // `join` spans states (unlike every other command here): in
        // no-account state it behaves like `init` (needs a passphrase,
        // transitions to unlocked); in unlocked state it reuses the live
        // engine/client; locked is simply refused. Kept inline (not a
        // `do_*` helper) because the state fan-out IS its logic, mirroring
        // how `unlock` above is written inline rather than extracted.
        "join" => {
            let code = match arg_str(args, "code") {
                Ok(c) => c,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            // Accept both a bare `moyuinv1...` code and its
            // link-with-`#<token>` form, exactly like `cmd_join`.
            let raw = code
                .rsplit_once('#')
                .map(|(_, t)| t)
                .unwrap_or(&code)
                .trim()
                .to_owned();
            let token = match moyu_core::invite::decode_token(&raw) {
                Ok(t) => t,
                Err(e) => {
                    return writer.receipt_err(
                        id,
                        ErrCode::InvalidArgs,
                        &format!("invalid invite code: {e}"),
                    );
                }
            };
            match state {
                State::Locked { .. } => {
                    writer.receipt_err(id, ErrCode::Locked, "session is locked; unlock first");
                }
                State::NoAccount => {
                    let passphrase = match arg_str(args, "passphrase") {
                        Ok(p) => Zeroizing::new(p),
                        Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
                    };
                    if passphrase.is_empty() {
                        return writer.receipt_err(
                            id,
                            ErrCode::InvalidArgs,
                            "passphrase must not be empty",
                        );
                    }
                    // C1: effective relays = this process's resolved relays ∪
                    // the code's relays, unioned into (and persisted to)
                    // config.json -- mirrors `cmd_join` exactly, or a code
                    // pointing at a relay we don't already know about would
                    // silently fail to reach the inviter.
                    let mut effective: Vec<String> = opts.relays.clone();
                    for r in &token.relays {
                        if !effective.iter().any(|x| x == r) {
                            effective.push(r.clone());
                        }
                    }
                    let effective =
                        match moyu_core::config::merge_relays(&opts.data_dir, &effective) {
                            Ok(r) => r,
                            Err(e) => {
                                return writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}"));
                            }
                        };
                    let engine = crate::domain::build_engine(
                        &opts.data_dir,
                        &effective,
                        opts.allow_loopback,
                        opts.socks5,
                        passphrase.to_string(),
                    );
                    match ops::join(&engine, &opts.data_dir, &effective, opts.socks5, token).await {
                        Ok(receipt) => {
                            let label =
                                match moyu_core::store::read_active_account_label(&opts.data_dir) {
                                    Ok(Some(l)) => l,
                                    Ok(None) => {
                                        return writer.receipt_err(
                                            id,
                                            ErrCode::Engine,
                                            "join succeeded but no active account was persisted",
                                        );
                                    }
                                    Err(e) => {
                                        return writer.receipt_err(
                                            id,
                                            ErrCode::Engine,
                                            &format!("{e:#}"),
                                        );
                                    }
                                };
                            match engine.client(&label).await {
                                Ok(mut client) => match MoyuStore::open(&opts.data_dir, &label) {
                                    Ok(mut store) => {
                                        crate::domain::refresh_keypackage(&mut client, &mut store)
                                            .await;
                                        let npub = moyu_core::identity::npub_from_hex(&label).ok();
                                        *state = State::Unlocked(Box::new(Unlocked {
                                            label: label.clone(),
                                            engine,
                                            client,
                                            store,
                                        }));
                                        reset_roster_baseline(state, prev_rosters);
                                        // The unlock-equivalent state fields
                                        // merged on top of the shared `join`
                                        // body -- the unlocked-branch arm
                                        // below uses `receipt.to_json()` bare,
                                        // with none of these.
                                        let mut data = receipt.to_json();
                                        data["state"] = serde_json::json!("unlocked");
                                        data["label"] = serde_json::json!(label);
                                        data["npub"] = serde_json::json!(npub);
                                        writer.receipt_ok(id, data);
                                    }
                                    Err(e) => {
                                        writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}"))
                                    }
                                },
                                Err(e) => {
                                    writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}"))
                                }
                            }
                        }
                        Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
                    }
                }
                State::Unlocked(u) => {
                    // Best-effort persist the C1 relay union for future
                    // sessions -- the ALREADY-open engine keeps this
                    // process's original relay set (a new relay from the
                    // code takes effect on the next unlock/session, the same
                    // way a one-shot `moyu join` from a second terminal
                    // wouldn't retroactively reconfigure this one).
                    let mut merged = opts.relays.clone();
                    for r in &token.relays {
                        if !merged.iter().any(|x| x == r) {
                            merged.push(r.clone());
                        }
                    }
                    let _ = moyu_core::config::merge_relays(&opts.data_dir, &merged);
                    match ops::join(&u.engine, &opts.data_dir, &opts.relays, opts.socks5, token)
                        .await
                    {
                        Ok(receipt) => writer.receipt_ok(id, receipt.to_json()),
                        Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
                    }
                }
            }
        }

        // ---------------------------------------------------------- unlocked only
        "send" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            let peer = match arg_str(args, "peer") {
                Ok(p) => p,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            let body = match (
                args.get("message").and_then(|v| v.as_str()),
                args.get("file").and_then(|v| v.as_str()),
            ) {
                (_, Some(file)) => ops::SendBody::File {
                    path: PathBuf::from(file),
                    caption: args
                        .get("message")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                },
                (Some(msg), None) if !msg.is_empty() => ops::SendBody::Text(msg.to_owned()),
                _ => {
                    return writer.receipt_err(
                        id,
                        ErrCode::InvalidArgs,
                        "args.message (string) or args.file (path) is required",
                    );
                }
            };
            match ops::send(
                &mut u.client,
                &mut u.store,
                &peer,
                body,
                opts.socks5,
                opts.blossom.clone(),
            )
            .await
            {
                Ok(receipt) => writer.receipt_ok(id, receipt.to_json()),
                Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
            }
        }

        "history" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            let group = match arg_str(args, "group") {
                Ok(g) => g,
                Err(e) => return writer.receipt_err(id, ErrCode::InvalidArgs, &e),
            };
            let before = args.get("before").and_then(|v| v.as_str());
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50);
            let group_hex =
                match crate::domain::resolve_group_hex_offline(&u.engine, &u.label, &group) {
                    Ok(h) => h,
                    Err(e) => return writer.receipt_err(id, ErrCode::NotFound, &format!("{e:#}")),
                };
            match ops::history(
                &u.engine,
                &u.label,
                &group_hex,
                before,
                limit,
                opts.allow_loopback,
            ) {
                Ok(page) => {
                    let rows: Vec<serde_json::Value> =
                        page.rows.iter().map(|r| r.to_json()).collect();
                    writer.receipt_ok(
                        id,
                        serde_json::json!({
                            "group": page.group,
                            "messages": rows,
                            "next_cursor": page.next_cursor,
                        }),
                    );
                }
                Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
            }
        }

        "conversations" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            match ops::conversations(&u.engine, &u.client, &u.store, &u.label) {
                Ok(view) => writer.receipt_ok(id, view.to_json()),
                Err(e) => writer.receipt_err(id, ErrCode::Engine, &format!("{e:#}")),
            }
        }

        "post" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_post(u, opts, args).await);
        }

        "reply" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_reply(u, args).await);
        }

        "react" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_react(u, args).await);
        }

        "download" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_download(u, opts, args).await);
        }

        "search" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_search(u, args));
        }

        "op" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_op(u, args).await);
        }

        "activity" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_activity(u, args).await);
        }

        "invite" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_invite(u, opts, args));
        }

        "requests" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_requests(u));
        }

        "approve" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_approve(u, args).await);
        }

        "add" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_add(u, opts, args).await);
        }

        "keypackage" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_keypackage(u, args).await);
        }

        "workspace_new" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_new(u, args).await);
        }

        "workspace_add" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_add(u, opts, args).await);
        }

        "workspace_members" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_members(u, args));
        }

        "workspace_rename" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_rename(u, args).await);
        }

        "workspace_kick" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_kick(u, opts, args).await);
        }

        "workspace_leave" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_workspace_leave(u, opts, args).await);
        }

        "admin_list" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_admin_list(u, args));
        }

        "admin_add" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(
                writer,
                id,
                do_admin_change(u, opts, args, crate::domain::AdminChange::Promote).await,
            );
        }

        "admin_remove" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(
                writer,
                id,
                do_admin_change(u, opts, args, crate::domain::AdminChange::Demote).await,
            );
        }

        "channel_new" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_channel_new(u, args).await);
        }

        "channel_new_private" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_channel_new_private(u, opts, args).await);
        }

        "channel_invite" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_channel_invite(u, opts, args).await);
        }

        "channel_rename" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_channel_rename(u, args).await);
        }

        "channel_archive" => {
            let Some(u) = require_unlocked(state, writer, id) else {
                return;
            };
            respond(writer, id, do_channel_archive(u, args).await);
        }

        other => {
            writer.receipt_err(
                id,
                ErrCode::UnknownCmd,
                &format!("unknown command {other:?} (see hello.capabilities)"),
            );
        }
    }
}

/// The unlocked-only guard every engine-touching branch above starts with:
/// not `State::Unlocked` ⇒ emit the state-appropriate refusal receipt (see
/// [`locked_code`]/[`locked_msg`] for how the code/message vary between
/// `locked` and `no-account`) and hand back `None` so the caller can
/// `return`; `Unlocked` ⇒ `Some` of the mutable engine/client/store bundle.
/// Not used by `unlock`/`init`/`join`, whose state fan-out IS their logic
/// (see the module doc and their inline comments).
fn require_unlocked<'s>(
    state: &'s mut State,
    writer: &SessionWriter,
    id: Option<&str>,
) -> Option<&'s mut Unlocked> {
    let State::Unlocked(u) = state else {
        writer.receipt_err(id, locked_code(state), locked_msg(state));
        return None;
    };
    Some(u)
}

/// The right refusal for an engine-touching command in a non-unlocked state.
fn locked_code(state: &State) -> ErrCode {
    match state {
        State::NoAccount => ErrCode::NoAccount,
        _ => ErrCode::Locked,
    }
}

fn locked_msg(state: &State) -> &'static str {
    match state {
        State::NoAccount => "no account yet -- run init or join first",
        _ => "session is locked; unlock first",
    }
}

// ---------------------------------------------------------------------------
// A-4b: per-command helpers for the unlocked-only commands, in `CAPABILITIES`
// order. Each mirrors its `cmd_*` one-shot counterpart in `main.rs`: parse
// `args`, call the same `ops::*` core, shape a receipt `data` with the same
// field names as that command's `--json` output (minus `"v"`/`"ok"`, which
// the receipt frame already carries). A `ws`/`group`/`channel` argument is
// pre-resolved via the same private `crate::domain::resolve_*` helper the `ops` call
// uses internally, purely so a bad name reports `not_found` instead of the
// generic `engine` code (both reads are local/offline, so resolving twice
// costs no relay round trip); a `peer`/`member` argument is NOT
// pre-resolved (peer resolution can touch the network for NIP-05), so its
// failure surfaces as `engine`, bundled with any other failure inside the
// `ops` call.
// ---------------------------------------------------------------------------

async fn do_post(u: &mut Unlocked, opts: &SessionOpts, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    let body = match (
        args.get("message").and_then(|v| v.as_str()),
        args.get("file").and_then(|v| v.as_str()),
    ) {
        (_, Some(file)) => ops::PostBody::File {
            path: PathBuf::from(file),
            caption: args
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        },
        (Some(msg), None) if !msg.is_empty() => ops::PostBody::Text(msg.to_owned()),
        _ => {
            return Err(invalid_args(
                "args.message (string) or args.file (path) is required",
            ));
        }
    };
    crate::domain::resolve_channel_target(&u.engine, &u.client, &u.label, &ws, &channel)
        .map_err(not_found)?;
    let receipt = ops::post(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &channel,
        body,
        opts.blossom.clone(),
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_reply(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let group = arg_str_e(args, "group")?;
    let message_id = arg_str_e(args, "message_id")?;
    let text = arg_str_e(args, "message")?;
    crate::domain::resolve_group_id(&u.engine, &u.client, &u.label, &group).map_err(not_found)?;
    let receipt = ops::reply(
        &u.engine,
        &mut u.client,
        &u.label,
        &group,
        &message_id,
        text,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_react(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let group = arg_str_e(args, "group")?;
    let message_id = arg_str_e(args, "message_id")?;
    let remove = args
        .get("remove")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let emoji = if remove {
        None
    } else {
        let e = args
            .get("emoji")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match e {
            Some(e) => Some(e.to_owned()),
            None => {
                return Err(invalid_args(
                    "args.emoji (string) is required unless args.remove is true",
                ));
            }
        }
    };
    crate::domain::resolve_group_id(&u.engine, &u.client, &u.label, &group).map_err(not_found)?;
    let receipt = ops::react(
        &u.engine,
        &mut u.client,
        &u.label,
        &group,
        &message_id,
        emoji.as_deref(),
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_download(u: &mut Unlocked, opts: &SessionOpts, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let hash = arg_str_e(args, "hash")?;
    let out = args.get("out").and_then(|v| v.as_str()).map(PathBuf::from);
    crate::domain::resolve_group_id(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::download(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &hash,
        opts.allow_loopback,
        out,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

/// Sync (offline, like `ops::search` itself): a pure local-history scan,
/// never a relay round trip.
fn do_search(u: &Unlocked, args: &serde_json::Value) -> CmdResult {
    let query = arg_str_e(args, "query")?;
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Err(invalid_args("args.query must not be empty"));
    }
    let group_filter = args
        .get("group")
        .and_then(|v| v.as_str())
        .map(|g| g.trim().to_lowercase())
        .filter(|g| !g.is_empty());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(50);
    let hits = ops::search(&u.engine, &u.label, &needle, group_filter.as_deref(), limit)
        .map_err(engine_err)?;
    let rows: Vec<serde_json::Value> = hits.iter().map(ops::SearchHit::to_json).collect();
    Ok(serde_json::json!({ "hits": rows }))
}

async fn do_op(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    // The free-text summary is optional (the structured fields carry the
    // meaning) -- mirrors `read_optional_message_arg`'s "absent = empty".
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let event_type = arg_str_e(args, "type")?;
    let status = arg_str_e(args, "status")?;
    let name = args.get("name").and_then(|v| v.as_str()).map(str::to_owned);
    let run_id = args
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let ok_flag = args.get("ok").and_then(|v| v.as_bool());
    let duration_ms = args.get("duration_ms").and_then(|v| v.as_u64());
    let preview = args
        .get("preview")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    // Passed straight through as a JSON object (not the one-shot CLI's raw
    // `--details` string) -- a session command is already structured JSON.
    let details = args.get("details").filter(|v| !v.is_null()).cloned();
    crate::domain::resolve_channel_target(&u.engine, &u.client, &u.label, &ws, &channel)
        .map_err(not_found)?;
    let receipt = ops::op(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &channel,
        text,
        event_type,
        status,
        name,
        run_id,
        ok_flag,
        duration_ms,
        preview,
        details,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_activity(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    // Unlike `op`, an activity IS a message -- its body is required (matches
    // `read_message_arg`'s requirement in `cmd_activity`).
    let text = arg_str_e(args, "text")?;
    let status = args
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("active")
        .to_owned();
    let extra = args.get("extra").filter(|v| !v.is_null()).cloned();
    crate::domain::resolve_channel_target(&u.engine, &u.client, &u.label, &ws, &channel)
        .map_err(not_found)?;
    let receipt = ops::activity(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &channel,
        text,
        status,
        extra,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

/// Sync (`ops::invite` itself is sync): a contact invite is pure local token
/// construction; a workspace invite additionally reads (and persists to) the
/// already-open `u.store` -- no relay round trip either way.
fn do_invite(u: &mut Unlocked, opts: &SessionOpts, args: &serde_json::Value) -> CmdResult {
    let workspace = args
        .get("workspace")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let auto_approve = args
        .get("auto_approve")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let receipt = if let Some(slug) = &workspace {
        crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, slug)
            .map_err(not_found)?;
        ops::invite(
            &u.label,
            &opts.relays,
            ops::InviteScope::Workspace {
                engine: &u.engine,
                client: &u.client,
                store: &mut u.store,
                slug,
            },
            auto_approve,
        )
        .map_err(engine_err)?
    } else {
        ops::invite(
            &u.label,
            &opts.relays,
            ops::InviteScope::Contact,
            auto_approve,
        )
        .map_err(engine_err)?
    };
    Ok(receipt.to_json())
}

/// Sync (`ops::requests` itself is sync): a local-history scan plus a
/// `client.members()` read per pending request -- both local/offline.
fn do_requests(u: &mut Unlocked) -> CmdResult {
    // The store handle lives as long as the session; an invite issued or
    // revoked in another terminal must show in the trust badge.
    let _ = u.store.reload();
    let rows = ops::requests(&u.engine, &u.client, &u.store, &u.label).map_err(engine_err)?;
    let json_rows: Vec<serde_json::Value> = rows.iter().map(ops::RequestRow::to_json).collect();
    Ok(serde_json::json!({ "requests": json_rows }))
}

async fn do_approve(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let who = arg_str_e(args, "who")?;
    let auto = args.get("auto").and_then(|v| v.as_bool()).unwrap_or(false);
    // Optional: which workspace, when the same person asked to join several.
    let ws = args.get("ws").and_then(|v| v.as_str()).map(str::to_owned);
    let (outcomes, failed) = ops::approve(
        &u.engine,
        &mut u.client,
        &mut u.store,
        &u.label,
        who,
        ws.as_deref(),
        auto,
    )
    .await
    .map_err(engine_err)?;
    // NOT unified with `SessionEvent::Approved::to_json()` / the one-shot
    // `approve`'s per-outcome streamed object (see `main.rs::cmd_approve`,
    // which DOES reuse that method): this is a genuinely different, already
    // AGGREGATED shape (`{"status":...,"npub","ws_name"}` rows, batched under
    // `outcomes`/`failed`) rather than one framed event per outcome. Existing
    // wire shape, not touched by this pass.
    let rows: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| match o {
            ops::ApproveOutcome::Approved { npub, ws_name, .. } => {
                serde_json::json!({ "status": "approved", "npub": npub, "ws_name": ws_name })
            }
            ops::ApproveOutcome::AlreadyMember { npub, ws_name } => {
                serde_json::json!({ "status": "already_member", "npub": npub, "ws_name": ws_name })
            }
        })
        .collect();
    // Partial failure stays an ok:true receipt carrying BOTH lists: the
    // approvals in `outcomes` really happened on the wire, so the GUI must
    // see them even when some targets failed; it renders
    // `failed` as per-target errors.
    Ok(serde_json::json!({ "outcomes": rows, "failed": failed }))
}

async fn do_add(u: &mut Unlocked, opts: &SessionOpts, args: &serde_json::Value) -> CmdResult {
    let peer = arg_str_e(args, "peer")?;
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    // MUST write through the session's one owned store handle: a second `MoyuStore::open` would be invisible to this session and
    // clobbered by its next whole-file save.
    let receipt = ops::add(&mut u.store, opts.socks5, peer, label)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_keypackage(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let action_str = arg_str_e(args, "action")?;
    let action = match action_str.as_str() {
        "publish" => super::KeypackageAction::Publish,
        "rotate" => super::KeypackageAction::Rotate,
        other => {
            return Err(invalid_args(format!(
                "args.action must be \"publish\" or \"rotate\", got {other:?}"
            )));
        }
    };
    let receipt = ops::keypackage(&u.engine, &mut u.store, &u.label, action)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_workspace_new(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let name = arg_str_e(args, "name")?;
    let receipt = ops::workspace_new(&mut u.client, &name)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_workspace_add(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let peer = arg_str_e(args, "peer")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::workspace_add(&u.engine, &mut u.client, &u.label, &ws, &peer, opts.socks5)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

/// Sync (`ops::workspace_members` itself is sync).
fn do_workspace_members(u: &Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let rows = ops::workspace_members(&u.engine, &u.client, &u.label, &ws).map_err(engine_err)?;
    let json_rows: Vec<serde_json::Value> =
        rows.iter().map(ops::WorkspaceMemberRow::to_json).collect();
    Ok(serde_json::json!({ "members": json_rows }))
}

async fn do_workspace_rename(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let name = arg_str_e(args, "name")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::workspace_rename(&u.engine, &mut u.client, &u.label, &ws, &name)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_workspace_kick(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let peer = arg_str_e(args, "peer")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::workspace_kick(
        &u.engine,
        &mut u.client,
        &mut u.store,
        &u.label,
        &ws,
        &peer,
        opts.socks5,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_workspace_leave(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let transfer_to = args
        .get("transfer_to")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::workspace_leave(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        transfer_to.as_deref(),
        opts.socks5,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

/// Sync (`ops::admin_list` itself is sync).
fn do_admin_list(u: &Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let listing = ops::admin_list(&u.engine, &u.client, &u.label, &ws).map_err(engine_err)?;
    let rows: Vec<serde_json::Value> = listing.admins.iter().map(ops::AdminRow::to_json).collect();
    Ok(serde_json::json!({ "admins": rows }))
}

/// Shared by `admin_add`/`admin_remove` -- only the [`crate::domain::AdminChange`]
/// direction differs, mirroring `cmd_admin_change`.
async fn do_admin_change(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
    change: crate::domain::AdminChange,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let peer = arg_str_e(args, "peer")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::admin_change(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &peer,
        opts.socks5,
        change,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_channel_new(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let name = arg_str_e(args, "name")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::channel_new(&u.engine, &mut u.client, &u.label, &ws, &name)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_channel_new_private(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let name = arg_str_e(args, "name")?;
    let members: Vec<String> = args
        .get("invite")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::channel_new_private(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &name,
        &members,
        opts.socks5,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_channel_invite(
    u: &mut Unlocked,
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    let peer = arg_str_e(args, "peer")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::channel_invite(
        &u.engine,
        &mut u.client,
        &u.label,
        &ws,
        &channel,
        &peer,
        opts.socks5,
    )
    .await
    .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_channel_rename(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    let name = arg_str_e(args, "name")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::channel_rename(&u.engine, &mut u.client, &u.label, &ws, &channel, &name)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

async fn do_channel_archive(u: &mut Unlocked, args: &serde_json::Value) -> CmdResult {
    let ws = arg_str_e(args, "ws")?;
    let channel = arg_str_e(args, "channel")?;
    crate::domain::resolve_workspace(&u.engine, &u.client, &u.label, &ws).map_err(not_found)?;
    let receipt = ops::channel_archive(&u.engine, &mut u.client, &u.label, &ws, &channel)
        .await
        .map_err(engine_err)?;
    Ok(receipt.to_json())
}

/// `init`'s core: same as `cmd_init`'s post-prompt call into `ops::init`,
/// except the passphrase comes from `args` (never a tty/env prompt -- a
/// session never reads the tty) and, on success, a fresh engine/client/store
/// are opened under that same passphrase so `init` hands back a
/// ready-to-use unlocked session in one round trip (mirrors the `unlock`
/// success arm) instead of making the GUI immediately follow up with
/// `unlock`.
async fn do_init(
    opts: &SessionOpts,
    args: &serde_json::Value,
) -> Result<(Unlocked, serde_json::Value), (ErrCode, String)> {
    let passphrase = Zeroizing::new(arg_str_e(args, "passphrase")?);
    if passphrase.is_empty() {
        return Err(invalid_args("passphrase must not be empty"));
    }
    let import_nsec = args
        .get("import_nsec")
        .and_then(|v| v.as_str())
        .map(|s| Zeroizing::new(s.to_owned()));
    let relays: Vec<String> = match args.get("relays").and_then(|v| v.as_array()) {
        Some(arr) => {
            let relays: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            crate::domain::reject_loopback_relays(&relays, opts.allow_loopback)
                .map_err(|e| invalid_args(format!("{e:#}")))?;
            relays
        }
        None => opts.relays.clone(),
    };

    let receipt = ops::init(
        &opts.data_dir,
        &relays,
        opts.allow_loopback,
        opts.socks5,
        passphrase.to_string(),
        import_nsec,
    )
    .await
    .map_err(engine_err)?;

    let engine = crate::domain::build_engine(
        &opts.data_dir,
        &relays,
        opts.allow_loopback,
        opts.socks5,
        passphrase.to_string(),
    );
    let mut client = engine.client(&receipt.label).await.map_err(engine_err)?;
    let mut store = MoyuStore::open(&opts.data_dir, &receipt.label).map_err(engine_err)?;
    crate::domain::refresh_keypackage(&mut client, &mut store).await;

    // Deliberately NOT `receipt.to_json(...)` -- that's the one-shot `init
    // --json` shape (`label`/`npub`/`relays`/`imported`); this unlocked-state
    // shape is smaller and must stay that way (mirrors `unlock`'s success
    // shape instead). See `ops::InitReceipt::to_json`'s doc comment.
    let data = serde_json::json!({
        "state": "unlocked", "label": receipt.label, "npub": receipt.npub,
    });
    Ok((
        Unlocked {
            label: receipt.label,
            engine,
            client,
            store,
        },
        data,
    ))
}
