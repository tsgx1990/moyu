//! KeyPackage lifecycle policy — entirely moyu's own responsibility.
//!
//! The Marmot spec wants a
//! published KeyPackage's lifetime capped at roughly 84 days plus rotation
//! after a Welcome consumes one, but **the engine does not enforce this**.
//! Confirmed by reading the code: `cgka_engine::key_package::KeyPackageMetadata`
//! (`crates/cgka-engine/src/key_package.rs:24-27`) carries only
//! `key_package_ref_hex` and `credential_identity_hex` — no creation
//! timestamp, no expiry field, nothing an engine-level policy could hook.
//! `AppClient::publish_key_package`/`rotate_key_package`
//! (`crate::engine::MoyuEngine`) will happily keep serving/re-publishing the
//! same or a fresh KeyPackage forever; nothing calls them on a schedule.
//!
//! So moyu tracks "when did I last publish" itself
//! (`crate::store::MoyuStore::keypackage_published_at`/
//! `mark_keypackage_published`) and this module is just the policy that
//! compares that timestamp against a lifetime budget and calls the live
//! `AppClient`'s `rotate_key_package` when it has elapsed.
//!
//! `ensure_fresh` below is the policy function; moyu-cli's `refresh_keypackage`
//! helper calls it at the startup of `chat`/`send`/`tui` and on every poll of
//! the long-lived `recv --follow` loop, so an aging KeyPackage is rotated
//! automatically and the user never has to run `moyu keypackage rotate` by hand
//! (`moyu keypackage publish`/`rotate` remain as explicit manual escape
//! hatches). Rotation only happens while moyu is running, which is why the
//! threshold ([`KEY_PACKAGE_ROTATE_AFTER_SECS`]) is deliberately proactive —
//! well under the spec's 84-day maximum — so a fresh KeyPackage is up before the
//! old one can expire between sessions. (One residual gap: a single
//! *interactive* `chat`/`tui` session left running continuously past the
//! threshold re-checks only at its startup, not mid-session; the far more
//! common short sessions and the `recv --follow` daemon are covered.)

use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::AppClient;
use crate::error::MoyuResult;
use crate::store::MoyuStore;

/// Marmot caps a KeyPackage's declared MLS `Lifetime` at
/// `not_after - not_before <= 7_261_200` seconds — 84 days plus a one-hour
/// clock-skew margin (`marmot/foundation/key-packages.md` line 74, confirmed
/// against the spec; a `last_resort` KeyPackage does not relax it).
/// This is the MAXIMUM validity a published KeyPackage can declare. moyu can't
/// read the exact `not_after` MDK actually stamped
/// (`cgka_engine::KeyPackageMetadata` carries no timestamp), so it tracks its
/// own publish time and rotates *before* this bound rather than at it.
pub const KEY_PACKAGE_MAX_LIFETIME_SECS: u64 = 7_261_200;

/// Rotate once the published KeyPackage is older than this. Deliberately well
/// under [`KEY_PACKAGE_MAX_LIFETIME_SECS`] so a fresh KeyPackage is always up
/// before the old one can expire, leaving headroom for the user not opening
/// moyu for a couple of weeks and for clock skew (~60 days → ~24 days margin).
pub const KEY_PACKAGE_ROTATE_AFTER_SECS: u64 = 60 * 24 * 60 * 60;

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `true` if the currently tracked KeyPackage (if any) is missing or older
/// than [`KEY_PACKAGE_ROTATE_AFTER_SECS`] (the proactive-rotation window, well
/// under the spec's 84-day maximum).
pub fn is_stale(store: &MoyuStore, now: u64) -> bool {
    match store.keypackage_published_at() {
        None => true,
        Some(published_at) => now.saturating_sub(published_at) > KEY_PACKAGE_ROTATE_AFTER_SECS,
    }
}

/// Rotate-and-republish the KeyPackage on the caller's already-open `client` if
/// it is stale, recording the new publish time in `store`. Returns `true` if a
/// rotation happened. Takes the live `AppClient` (not a `MoyuEngine`) so it
/// reuses the open session instead of spinning up a second one just to publish.
pub async fn ensure_fresh(client: &mut AppClient, store: &mut MoyuStore) -> MoyuResult<bool> {
    let now = now_unix_secs();
    if !is_stale(store, now) {
        return Ok(false);
    }
    client.rotate_key_package().await?;
    store.mark_keypackage_published(now)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MoyuStore;

    #[test]
    fn is_stale_tracks_publish_age_and_stays_under_the_spec_max() {
        // Both operands are `const`, so clippy sees this as always-true and
        // flags it as dead code (`assertions_on_constants`). Left as a runtime
        // `assert!` rather than hoisted into a `const _: () = assert!(...)`
        // compile-time check: this is a key-lifecycle safety invariant, and
        // widening where/how it's enforced is a behavioral call this
        // clippy-cleanup task is scoped to avoid making.
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(
                KEY_PACKAGE_ROTATE_AFTER_SECS < KEY_PACKAGE_MAX_LIFETIME_SECS,
                "must rotate before a KeyPackage can expire"
            );
        }

        let dir = std::env::temp_dir().join(format!(
            "moyu-kp-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        let mut store = MoyuStore::open(&dir, "acct").expect("open temp store");
        let now = 1_700_000_000u64;

        // Never published -> must (re)publish.
        assert!(is_stale(&store, now));

        // Just published -> fresh, and still fresh right at the threshold.
        store.mark_keypackage_published(now).unwrap();
        assert!(!is_stale(&store, now));
        assert!(!is_stale(&store, now + KEY_PACKAGE_ROTATE_AFTER_SECS));

        // One second past the proactive window -> rotate.
        assert!(is_stale(&store, now + KEY_PACKAGE_ROTATE_AFTER_SECS + 1));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
