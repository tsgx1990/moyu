//! Engine/session construction and the small id/wire helpers everything else
//! in `domain` builds on: `build_engine` (an already-prompted passphrase in,
//! a `MoyuEngine` out), the loopback-relay guard, `send_control` (every
//! `moyu.*` control-event send goes through here), and `active_label`/
//! `group_id_from_hex`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moyu_core::engine::{AppClient, GroupId, MoyuEngine};
use moyu_core::identity::Argon2idFileSecretStore;
use moyu_core::store::{self, MoyuStore};
use moyu_core::workspace::ControlEvent;
use moyu_core::{keypackage_rotation, transport};

/// How often a long-running `tui`/`chat` session re-checks KeyPackage
/// freshness on its own poll tick, on top of the check every command already
/// does once at startup (`refresh_keypackage`, below). A session left open
/// for weeks would otherwise never re-check: 6h is far inside the ~84-day
/// proactive-rotation threshold `keypackage_rotation` enforces, so this is
/// just insurance against a very long-lived process, not the primary trigger.
pub(crate) const KEYPACKAGE_RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

/// Pure timing check for the periodic re-check, factored out so the interval
/// logic is unit-testable without an actual 6h wait: due once `now` is at
/// least `KEYPACKAGE_RECHECK` past `last`.
pub(crate) fn keypackage_recheck_due(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= KEYPACKAGE_RECHECK
}

pub(crate) fn active_label(data_dir: &Path) -> anyhow::Result<String> {
    store::read_active_account_label(data_dir)?
        .ok_or_else(|| anyhow::anyhow!("no active account -- run `moyu init` first"))
}

/// Opportunistically rotate this account's KeyPackage if it has aged past its
/// proactive-rotation threshold, so peers can always start new chats with us
/// (a KeyPackage's MLS `Lifetime` expires -- Marmot caps it at ~84 days; see
/// `moyu_core::keypackage_rotation`). Called on the normal usage commands so
/// the user never has to run `moyu keypackage rotate` by hand.
///
/// Best-effort and cheap: it's a local timestamp check that only performs a
/// relay publish about once every couple of months; a failure (e.g. relay
/// unreachable) is warned about, not fatal, so it never blocks chatting.
pub(crate) async fn refresh_keypackage(client: &mut AppClient, store: &mut MoyuStore) {
    match keypackage_rotation::ensure_fresh(client, store).await {
        Ok(true) => heprintln!("Rotated an aging KeyPackage and published a fresh one."),
        Ok(false) => {}
        Err(e) => heprintln!("[KeyPackage freshness check failed (non-fatal): {e}]"),
    }
}

/// `GroupId::new(hex::decode(hex_id)?)` -- the one-liner `find_or_create_dm`
/// already inlines for its own group; workspace/channel commands need it
/// repeatedly (resolve a workspace, then act on its group), so it earns a
/// helper here.
pub(crate) fn group_id_from_hex(hex_id: &str) -> anyhow::Result<GroupId> {
    Ok(GroupId::new(hex::decode(hex_id)?))
}

/// Construct the engine from an already-prompted passphrase. The prompt text
/// and empty-passphrase policy differ per command (`init`/`join` choose a
/// passphrase and reject empty ones), so prompting stays at the call site;
/// everything after the prompt is identical everywhere and lives here.
pub(crate) fn build_engine(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    passphrase: String,
) -> MoyuEngine {
    let secret_store = Arc::new(Argon2idFileSecretStore::new(
        data_dir.to_path_buf(),
        passphrase.into_bytes(),
    ));
    MoyuEngine::open(
        data_dir.to_path_buf(),
        relays.to_vec(),
        allow_loopback,
        socks5,
        secret_store,
    )
}

/// Send one `ControlEvent` as a `moyu.*` group-system event (kind 1210).
/// Every control-event send in the workspace/channel commands goes through
/// this helper so the wire encoding lives in exactly one place.
pub(crate) async fn send_control(
    client: &mut AppClient,
    gid: &GroupId,
    ev: ControlEvent,
) -> anyhow::Result<()> {
    let (system_type, data) = ev.to_wire();
    client
        .send_group_system_event(gid, system_type, String::new(), Some(data))
        .await?;
    Ok(())
}

/// The real entry point: everything runs inside `run` so one place can render a
/// failure as either a human `Error:` line or a `{"ok":false,..}` JSON object
/// (per `--json`) and set the exit code -- mirroring anyhow's default
/// `Termination` for the human path, plus the JSON path bots parse.
/// Refuse loopback relays unless `--dev-allow-loopback` was passed. Loopback
/// relays are only ever a local dev/testing target; failing up front (naming
/// the offending relay(s)) beats MDK's own deep, vaguer refusal. One function
/// so the process-wide guard and init's selected-set guard can't drift.
pub(crate) fn reject_loopback_relays(
    relays: &[String],
    allow_loopback: bool,
) -> anyhow::Result<()> {
    let loopback = transport::loopback_relays_in(relays);
    if !loopback.is_empty() && !allow_loopback {
        anyhow::bail!(
            "refusing to use loopback relay(s) [{}] by default -- loopback relays are for \
             local development/testing only. Re-run with --dev-allow-loopback to allow them \
             (never point moyu at a loopback relay in production).",
            loopback.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypackage_recheck_not_due_before_the_interval_elapses() {
        let last = Instant::now();
        let now = last + KEYPACKAGE_RECHECK - Duration::from_secs(1);
        assert!(!keypackage_recheck_due(last, now));
    }

    #[test]
    fn keypackage_recheck_due_once_the_interval_elapses() {
        let last = Instant::now();
        let now = last + KEYPACKAGE_RECHECK;
        assert!(keypackage_recheck_due(last, now));
    }

    #[test]
    fn keypackage_recheck_due_well_past_the_interval() {
        let last = Instant::now();
        let now = last + KEYPACKAGE_RECHECK * 10;
        assert!(keypackage_recheck_due(last, now));
    }

    #[test]
    fn keypackage_recheck_not_due_at_zero_elapsed() {
        let now = Instant::now();
        assert!(!keypackage_recheck_due(now, now));
    }
}
