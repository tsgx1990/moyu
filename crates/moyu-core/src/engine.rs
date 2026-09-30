//! Thin wrapper around `marmot_app::MarmotApp` — moyu-core's "engine" layer.
//!
//! # Architecture decision (confirmed against MDK's own reference CLI)
//!
//! MDK's `wn` CLI can run either standalone (in-process, one `MarmotApp` per
//! invocation) or against a long-lived `wnd` daemon (`MarmotAppRuntime` +
//! `AccountManager`, one background worker task per account, a
//! `broadcast::Sender<MarmotAppEvent>` event bus, auto-reconnect). Both paths
//! converge on the *same* underlying calls
//! (`commands::*` → `MarmotApp`/`MarmotAppRuntime` → `marmot_account::
//! AccountDeviceRuntime` → `cgka_session::AccountDeviceSession` →
//! `cgka_engine::Engine`) — the daemon/actor machinery exists only to support
//! live multi-subscriber event delivery and multi-process access, neither of
//! which a single-process CLI needs.
//!
//! moyu skips the daemon/actor layer entirely, per the research this crate
//! was written from. `MoyuEngine` uses:
//! - `MarmotApp::runtime()` (a cheap, stateless `MarmotAppRuntime` handle)
//!   only for account setup (`create_or_import_account`) and KeyPackage
//!   publish/rotate, which do not need a live session held open;
//! - `MarmotApp::client(label)` to obtain a live `AppClient` for everything
//!   group/message-related. Callers (moyu-cli) hold that `AppClient` for the
//!   lifetime of a chat session and drive `create_group` / `invite_members` /
//!   `send` / `sync` / `next_event` on it directly — these are plain public
//!   `async fn`s with no actor/daemon dependency, so there is nothing left
//!   for moyu-core to usefully wrap without adding indirection for its own
//!   sake.
//!
//! `AppClient`, `GroupId`, `SendSummary`, `SyncSummary`, `ReceivedMessage`,
//! and `AccountSummary` are re-exported from here so moyu-cli never needs
//! `marmot-app`/`marmot-account`/`cgka-traits` as direct dependencies —
//! keeping the "无头核心 + 前端分离" boundary real,
//! not just aspirational.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeroize::Zeroizing;

use marmot_account::{AccountHome, AccountSecretStore};

// Every name below is re-exported at its MDK crate root (each crate's
// `lib.rs` is a thin facade, per that crate's AGENTS.md). The per-item
// comments cite where the item is defined upstream, as of the first MDK
// snapshot (see docs/mdk-api-map.md). The build is the proof: an item that
// moves or is renamed upstream fails `cargo build` here first.
pub use cgka_traits::engine::{GroupEvent, GroupStateChange}; // crates/traits/src/engine.rs:284,335
// Engine-synthesized group-state events (kind-1210 sources). `member_added` etc.
// surface in `SyncSummary.events` as `GroupEvent::GroupStateChanged`, NOT in
// `SyncSummary.messages` (wire-received app messages only) — the §5.2 catch-up
// gate keys off these. `EpochId`/`MemberId` are the id types those events carry.
pub use cgka_traits::{EpochId, GroupId, MemberId}; // crates/traits/src/types.rs:74 (root re-exports)
pub use marmot_account::AccountSummary; // marmot-account/src/home.rs:69-80
pub use marmot_app::{
    AccountSetupRequest, // marmot-app/src/runtime/mod.rs:675-689
    AccountSetupResult,  // marmot-app/src/runtime/mod.rs:675-689
    // --- bot / agent-operation events — kind-1202 CI/git/deploy events ---
    AgentOperationEventRequest, // marmot-app/src/lib.rs:420 — { event_type, status, name, run_id, ok, duration_ms, text, details, ... }
    AppClient,                  // marmot-app/src/client/mod.rs:57-74
    AppError,
    AppGroupAdminPolicyComponent, // marmot-app/src/groups.rs:233-238 — admin set (hex pubkeys)
    AppGroupMemberRecord, // marmot-app/src/groups.rs:65-70 — { member_id_hex, account, local }
    AppGroupRecord,       // marmot-app/src/groups.rs:34-63
    AppMessageQuery,      // marmot-app/src/lib.rs:655-658 — { group_id_hex, limit }
    AppMessageRecord,     // marmot-app/src/lib.rs:631 — persisted message (history backfill)
    // --- encrypted media / attachments — all marmot-app root re-exports ---
    DEFAULT_BLOSSOM_SERVER_URL, // media/mod.rs:29 — "https://blossom.primal.net"
    MarmotApp,
    MarmotAppConfig,                 // marmot-app/src/config.rs:14-38
    MediaAttachmentReference,        // media/mod.rs:39-50 — the imeta-tag attachment ref
    MediaDownloadResult,             // media/mod.rs — decrypted + hash-verified blob bytes
    MediaLocator,                    // media/mod.rs:33-36 — { kind, value }
    MediaUploadAttachmentRequest, // media/mod.rs:174-181 — { file_name, media_type, plaintext, dim, thumbhash }
    MediaUploadAttachmentResult,  // media/mod.rs:193-203 — { reference, encrypted_size_bytes }
    MediaUploadRequest, // media/mod.rs:183-191 — { attachments, caption, send, blossom_server }
    MediaUploadResult,  // media/mod.rs:193-203 — { attachments, sent }
    ReceivedMessage,    // marmot-app/src/lib.rs:580-599
    RelayConnectionMode, // marmot-app/src/config.rs — SOCKS5 relay proxy (fork add)
    SelfMembership,     // storage-sqlite/src/account_projection.rs:21-26 — Member|Left|Removed
    SendSummary,        // marmot-app/src/lib.rs:661-664
    SyncSummary,        // marmot-app/src/lib.rs:560-578
    media_attachment_from_imeta_tag, // media/mod.rs:504 — rebuild a ref from an incoming imeta tag
}; // crates/traits/src/types.rs:51-55

use crate::error::MoyuResult;
use crate::transport::to_transport_endpoints;

/// Diagnostic info about a just-published/rotated KeyPackage — the ref hash
/// is useful to log/print so a user can correlate "which KeyPackage event
/// did my peer actually fetch" without ever touching key material. Built via
/// `cgka_engine::key_package::key_package_metadata(&KeyPackage) ->
/// Result<KeyPackageMetadata, EngineError>` (confirmed at
/// `crates/cgka-engine/src/key_package.rs:24-31` — `KeyPackageMetadata {
/// key_package_ref_hex, credential_identity_hex }`, no timestamp field, which
/// is exactly why `crate::keypackage_rotation` exists).
pub struct PublishedKeyPackage {
    pub bytes: usize,
    pub key_package_ref_hex: String,
    pub credential_identity_hex: String,
}

/// Owns one `MarmotApp` bound to a single on-disk `root` directory
/// (`<root>/accounts/<label>/...`, per `AccountHome`'s layout). One
/// `MoyuEngine` per moyu-cli process invocation is the M0 shape — matches
/// the single-device design decision (no multi-account,
/// no multi-device).
pub struct MoyuEngine {
    app: MarmotApp,
    root: PathBuf,
}

impl MoyuEngine {
    /// `root` must be a private (0700-ish) directory this process owns
    /// outright — both `AccountHome` and `MarmotApp` create/expect files
    /// directly under it (`accounts/<label>/...`, plus `MarmotApp`'s own
    /// per-account SQLCipher session/cache databases, all HKDF-keyed
    /// automatically off the account's Nostr secret key — see
    /// `crate::sqlcipher_kdf` for why moyu does not derive those itself).
    ///
    /// `secret_store` is moyu's `crate::identity::Argon2idFileSecretStore`
    /// in production; tests could substitute
    /// `marmot_account::LocalFileSecretStore` to skip passphrase prompts.
    ///
    /// Construction is *not* async — confirmed:
    /// `AccountHome::open_with_secret_store` (home.rs:118-127) and
    /// `MarmotApp::with_relays_and_account_home` (lib.rs:951-962) are both
    /// plain (non-async) `fn`s; no I/O happens until an account is actually
    /// created/loaded.
    /// `dev_allow_loopback` opts into MDK's relay-safety escape hatch AND its
    /// separate blob-endpoint loopback gate (one dev switch, wired from
    /// `--dev-allow-loopback`). MDK's relay chokepoint
    /// (`crates/marmot-app/src/relay_plane/safety.rs`) otherwise
    /// refuses to open a socket to a non-public relay host (loopback included)
    /// -- `moyu init --relay ws://127.0.0.1:7777` failed with "relay endpoint
    /// host is not a public address" before this gate was wired. This is a
    /// pure honest boolean: moyu-core takes no view on when loopback is
    /// appropriate. The *policy* -- gate it behind an explicit
    /// `--dev-allow-loopback` CLI flag, refuse a loopback `--relay` without it
    /// -- lives entirely in moyu-cli (`main`), so this decision is never made
    /// implicitly by inspecting the relay list. Pass `false` for any real
    /// (public-relay) use.
    pub fn open(
        root: impl Into<PathBuf>,
        relays: Vec<String>,
        dev_allow_loopback: bool,
        socks5_proxy: Option<SocketAddr>,
        secret_store: Arc<dyn AccountSecretStore>,
    ) -> Self {
        let root = root.into();
        let account_home = AccountHome::open_with_secret_store(&root, secret_store);
        // `socks5_proxy` routes ALL relay traffic (and the directory fetcher)
        // through a SOCKS5 proxy -- e.g. a local Tor daemon's or `ssh -D`'s
        // SOCKS port -- which is what lets moyu reach relays on a network
        // where a direct WebSocket does not get through. `None` dials relays
        // directly.
        // (Relies on `MarmotAppConfig::relay_connection`, added in moyu's MDK
        // fork; see the workspace `Cargo.toml` MDK block.)
        let relay_connection = match socks5_proxy {
            Some(addr) => RelayConnectionMode::Socks5(addr),
            None => RelayConnectionMode::Direct,
        };
        let config = MarmotAppConfig::default()
            .with_allow_loopback_relay_endpoints(dev_allow_loopback)
            // `--dev-allow-loopback` is a single dev/test switch: it also opts
            // into MDK's SEPARATE blob-endpoint loopback gate (`config.rs:45`,
            // distinct from the relay one) so the attachment E2E can use a
            // loopback Blossom server. Off for any real
            // (public-Blossom) use, exactly like the relay gate above.
            .with_allow_loopback_blob_endpoints(dev_allow_loopback)
            .with_relay_connection(relay_connection);
        let app =
            MarmotApp::with_relays_and_account_home_and_config(&root, relays, account_home, config);
        Self { app, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create a brand-new identity (`import_nsec: None`) or import an existing
    /// one from its secret key (`import_nsec: Some(nsec1…|hex)`), publish its
    /// NIP-65/inbox relay lists, and (if `publish_initial_key_package`)
    /// publish its first KeyPackage (kind 30443) — moyu's M0 step 1+2 in one
    /// call.
    ///
    /// MDK ≥ 0.9.16 splits the request: `identity` is for a PUBLIC key only
    /// (an nsec there is refused with `UnexpectedPrivateKey`), private-key
    /// import goes through the `Zeroizing` `import_nsec` field, and
    /// `discovery_relays` are extra directory indexers consulted (bounded,
    /// advisory) when importing a pre-existing identity so its published
    /// profile/relay list can be found. moyu passes none on purpose: MDK then
    /// only consults the setup relays themselves (no unsolicited connections
    /// to public indexers such as purplepag.es — relevant for self-hosted and
    /// proxied setups); the only cost is that an imported identity's public
    /// display name is not looked up.
    ///
    /// `MarmotApp::runtime(&self) -> MarmotAppRuntime` (lib.rs:994-996) is a
    /// cheap handle, safe to create per call.
    /// `MarmotAppRuntime::create_or_import_account` (confirmed at
    /// `runtime/mod.rs:2421-2426`, shared impl at `runtime/mod.rs:2902-2993`)
    /// does the rest, with all-or-nothing rollback on failure
    /// (`rollback_account_after_setup_failure`, `runtime/mod.rs:3145-3161`).
    pub async fn create_or_import_account(
        &self,
        import_nsec: Option<Zeroizing<String>>,
        default_relays: Vec<String>,
        bootstrap_relays: Vec<String>,
        publish_initial_key_package: bool,
    ) -> MoyuResult<AccountSetupResult> {
        let request = AccountSetupRequest {
            identity: None,
            import_nsec,
            default_relays: to_transport_endpoints(&default_relays),
            bootstrap_relays: to_transport_endpoints(&bootstrap_relays),
            discovery_relays: Vec::new(),
            publish_missing_relay_lists: true,
            publish_initial_key_package,
        };
        // Account setup runs on MDK's runtime model: it spawns a managed
        // account *worker* for the new account (upstream's `wn` then drives
        // everything through that worker) and returns while the worker is
        // still alive, holding the account's single session slot (MDK >=
        // 0.9.11). moyu drives accounts through direct `AppClient`s instead.
        // Dropping this temporary runtime handle closes the worker's command
        // and shutdown channels, so the worker exits -- but asynchronously,
        // and MDK exposes no way to wait for just that (`runtime.shutdown()`
        // would also tear down the relay plane the app shares with it, and
        // `deactivate_account` writes a durable signed-out marker). The wait
        // therefore lives in [`Self::client`], which retries a busy session
        // for a bounded window. See SESSION_HANDOFF_WAIT.
        let runtime = self.app.runtime();
        let result = runtime.create_or_import_account(request).await?;
        drop(runtime);
        Ok(result)
    }

    /// Open a live per-account session. Confirmed:
    /// `MarmotApp::client(&self, label: &str) -> Result<AppClient, AppError>`
    /// (lib.rs:1006-1012) — internally opens the account's SQLCipher-keyed
    /// `AccountDeviceSession`, activates the transport adapter, and syncs
    /// transport group subscriptions. Callers should hold onto the returned
    /// `AppClient` for as long as they need it (e.g. the whole `moyu chat`
    /// REPL loop) rather than re-opening it per operation.
    pub async fn client(&self, label: &str) -> MoyuResult<AppClient> {
        // Normal opens succeed first try. The retry arm exists for exactly one
        // hand-off: right after `create_or_import_account`, MDK's setup worker
        // is still releasing the account's session slot (see the comment
        // there). Upstream's documented rule is "drop the owning client, then
        // retry"; the owner here is guaranteed to be exiting, so a bounded
        // wait is correct, not a guess. `join` from a fresh data dir hit this
        // race on a slow CI runner while never failing locally.
        let deadline = std::time::Instant::now() + SESSION_HANDOFF_WAIT;
        let mut delay = std::time::Duration::from_millis(50);
        loop {
            match self.app.client(label).await {
                Err(marmot_app::AppError::AccountSessionBusy)
                    if std::time::Instant::now() < deadline =>
                {
                    tracing::debug!(
                        target: "moyu_core::engine",
                        "account session still held by MDK's setup worker; retrying in {delay:?}"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_millis(500));
                }
                other => return Ok(other?),
            }
        }
    }

    /// Load this account's persisted message history (all groups) from the
    /// local SQLCipher projection. The TUI uses it to backfill a chat's past
    /// messages on open, since `AppClient::sync()` only surfaces new /
    /// undelivered messages, not already-delivered history. Synchronous local
    /// DB read (no relay traffic); `MarmotApp::messages` confirmed at
    /// `crates/marmot-app/src/lib.rs:1315`, returning `Vec<AppMessageRecord>`.
    /// A new device sees only post-join history (an MLS forward-secrecy
    /// property, not a bug).
    pub fn messages(&self, label: &str) -> MoyuResult<Vec<AppMessageRecord>> {
        Ok(drop_invalidated(self.app.messages(label)?))
    }

    /// Persisted messages of ONE group, ascending. Synchronous local DB read
    /// (no relay traffic); `MarmotApp::messages_with_query` confirmed at
    /// `crates/marmot-app/src/lib.rs:1319` — its SQL takes a newest-first
    /// `LIMIT` window re-sorted ascending when `limit` is set, but exposes no
    /// offset/cursor, so callers wanting keyset pagination (`moyu history`)
    /// read the group's full (retention-bounded) history and window in
    /// memory. Push the keyset filter down HERE (still not into MDK) if a
    /// group's history ever grows past what a full read tolerates.
    pub fn messages_for_group(
        &self,
        label: &str,
        group_id_hex: &str,
    ) -> MoyuResult<Vec<AppMessageRecord>> {
        Ok(drop_invalidated(self.app.messages_with_query(
            label,
            AppMessageQuery {
                group_id_hex: Some(group_id_hex.to_owned()),
                kinds: None,
                limit: None,
            },
        )?))
    }

    /// All groups (DMs + workspaces) for this account. Workspace-vs-DM is a
    /// projection concern (a group is a workspace iff its control-message
    /// projection has a name), not stored here. Synchronous local read (no
    /// relay traffic); `MarmotApp::groups` confirmed at
    /// `crates/marmot-app/src/lib.rs:1896`, returning `Vec<AppGroupRecord>`.
    pub fn groups(&self, label: &str) -> MoyuResult<Vec<AppGroupRecord>> {
        Ok(self.app.groups(label)?)
    }

    /// Publish this account's KeyPackage (kind 30443), minting a fresh one
    /// if none is cached. Confirmed:
    /// `AppClient::publish_key_package(&mut self) -> Result<KeyPackage, AppError>`
    /// (`client/mod.rs:190-208`). Returns the published KeyPackage's
    /// serialized byte length (`KeyPackage::bytes(&self) -> &[u8]`, used
    /// internally by `cgka_engine::key_package::key_package_metadata` per
    /// research — same accessor, just for a byte count here rather than
    /// parsing).
    pub async fn publish_key_package(&self, label: &str) -> MoyuResult<PublishedKeyPackage> {
        let mut client = self.client(label).await?;
        let kp = client.publish_key_package().await?;
        describe_key_package(kp)
    }

    /// Force-mint and publish a brand-new KeyPackage regardless of whether a
    /// cached one exists — used by `crate::keypackage_rotation` once the
    /// ~84-day lifetime budget (moyu's own tracking; MDK enforces no
    /// lifecycle policy itself, see that module) has elapsed. Confirmed:
    /// `AppClient::rotate_key_package(&mut self) -> Result<KeyPackage, AppError>`
    /// (`client/mod.rs:210-217`).
    ///
    /// MDK mints exactly one kind of KeyPackage: `do_fresh_key_package`
    /// (`cgka-engine/src/key_package.rs`) unconditionally marks every one
    /// `last_resort` and stamps OpenMLS's default ~84-day `Lifetime`, all under
    /// one replaceable Nostr event (kind 30443). So this refreshes the *sole*
    /// (last_resort) KeyPackage — there is no separate non-last-resort artifact
    /// left behind to silently expire, so moyu's time-based rotation alone keeps
    /// it valid.
    pub async fn rotate_key_package(&self, label: &str) -> MoyuResult<PublishedKeyPackage> {
        let mut client = self.client(label).await?;
        let kp = client.rotate_key_package().await?;
        describe_key_package(kp)
    }
}

/// `KeyPackage::bytes(&self) -> &[u8]` — confirmed indirectly: `cgka_engine::
/// key_package::key_package_metadata` (`crates/cgka-engine/src/key_package.rs:31-33`)
/// calls `MlsMessageIn::tls_deserialize_exact(kp.bytes())` internally, so
/// `.bytes()` is the accessor MDK's own code uses for exactly this purpose.
/// How long [`MoyuEngine::client`] keeps retrying `AccountSessionBusy` while
/// MDK's account-setup worker releases the session slot it held. Generous on
/// purpose (the wait normally ends within milliseconds); if it ever elapses
/// the real `AccountSessionBusy` error surfaces unchanged.
const SESSION_HANDOFF_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Drop rows MDK (>= 0.9.16) retains only as *invalidated* tombstones: after
/// fork recovery, messages from the losing branch stay in the store with
/// `invalidated = true` so replay/dedup remain stable, but every other Marmot
/// client hides them. Filtering at moyu's single read boundary keeps
/// `recv`/`history`/TUI/session and the workspace + membership projections
/// (all of which read through `messages`/`messages_for_group`) consistent
/// with the group's agreed history. Live `ReceivedMessage`s carry no such
/// flag: convergence only ever invalidates already-stored rows.
fn drop_invalidated(records: Vec<AppMessageRecord>) -> Vec<AppMessageRecord> {
    records.into_iter().filter(|r| !r.invalidated).collect()
}

fn describe_key_package(kp: cgka_traits::engine::KeyPackage) -> MoyuResult<PublishedKeyPackage> {
    let bytes = kp.bytes().len();
    let meta = cgka_engine::key_package::key_package_metadata(&kp)?;
    Ok(PublishedKeyPackage {
        bytes,
        key_package_ref_hex: meta.key_package_ref_hex,
        credential_identity_hex: meta.credential_identity_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, invalidated: bool) -> AppMessageRecord {
        AppMessageRecord {
            message_id_hex: id.to_owned(),
            direction: "received".to_owned(),
            group_id_hex: "aa".repeat(16),
            sender: "bb".repeat(32),
            plaintext: "hi".to_owned(),
            kind: 9,
            tags: Vec::new(),
            source_epoch: None,
            retention: None,
            recorded_at: 1,
            received_at: 1,
            insert_order: 0,
            invalidated,
            moderation_grant: false,
        }
    }

    #[test]
    fn drop_invalidated_hides_tombstones() {
        let out = drop_invalidated(vec![
            rec("live", false),
            rec("dead", true),
            rec("ok", false),
        ]);
        let ids: Vec<&str> = out.iter().map(|r| r.message_id_hex.as_str()).collect();
        assert_eq!(ids, vec!["live", "ok"]);
    }

    /// MDK 0.9.20 added opt-in usage diagnostics (stock Aptabase + OTLP
    /// relay-telemetry exporters) behind one persisted consent. moyu never
    /// grants it, so a freshly opened engine must report every exporter
    /// disabled and the decision still pending. Pins that default so an MDK
    /// bump that flips it fails here before it can ship.
    #[test]
    fn telemetry_exporters_are_off_by_default() {
        // 现有约定:std::env::temp_dir() + 唯一名 + remove_dir_all 清理(config.rs:120)。
        let root = std::env::temp_dir().join(format!("moyu-telemetry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store: Arc<dyn AccountSecretStore> =
            Arc::new(marmot_account::LocalFileSecretStore::new(&root));
        let engine = MoyuEngine::open(&root, Vec::new(), false, None, store);

        let relay = engine
            .app
            .relay_telemetry_settings()
            .expect("relay telemetry settings readable on a fresh root");
        assert!(
            !relay.export_enabled,
            "relay telemetry export must default off"
        );
        let usage = engine
            .app
            .usage_diagnostics_settings()
            .expect("usage diagnostics settings readable on a fresh root");
        assert_eq!(
            usage.decision,
            marmot_app::UsageDiagnosticsDecision::AcceptanceRequired,
            "no consent may be implied without an explicit grant"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
