//! moyu's own local bookkeeping store — contacts and KeyPackage-rotation
//! schedule.
//!
//! This is *not* the MLS/message-history store. All group/message/epoch
//! state lives inside MDK's `storage-sqlite` (one SQLCipher database per
//! account, wired up automatically by `marmot-app`/`marmot-account` — see
//! `docs/mdk-api-map.md`). This module only tracks
//! the handful of things that are entirely moyu's own responsibility and
//! that MDK deliberately does not track for you:
//!
//! - contacts (npub/nip05 -> a friendly local label, and which `GroupId` the
//!   1:1 chat with them lives in, so `moyu chat <peer>` does not need to
//!   re-fetch a KeyPackage / re-create a group every time),
//! - the invite codes this account issued, and a per-(workspace, requester)
//!   ledger of join requests already acted on (see [`JoinLedgerEntry`]),
//! - when this account's KeyPackage was last (re)published, so
//!   `crate::keypackage_rotation` can enforce the ~84-day lifecycle MDK's
//!   engine does not enforce for you (confirmed: `cgka_engine::key_package::
//!   KeyPackageMetadata` carries only `key_package_ref_hex` /
//!   `credential_identity_hex`, no timestamp —
//!   `crates/cgka-engine/src/key_package.rs:24-27`).
//!
//! Stored as one plaintext JSON file per account
//! (`<root>/accounts/<label>/moyu-state.json`) via `fs_private`, matching the
//! `0600` posture `fs-private` gives every other file in the account
//! directory. Nothing in here is secret key material, so this file does not
//! need the Argon2id encryption `crate::identity::Argon2idSecretStore` uses.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{MoyuError, MoyuResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    /// bech32 `npub1...`.
    pub npub: String,
    /// Original nip-05 identifier used to add this contact, if any
    /// (`alice@example.com`) — kept only for display; resolution always
    /// re-happens at add-time, never cached as a trust anchor.
    pub nip05: Option<String>,
    /// Local friendly label the user picked (defaults to the npub prefix).
    pub label: String,
    /// hex-encoded `cgka_traits::GroupId` of the 1:1 chat with this contact,
    /// once one has been created. `GroupId`'s `Display` impl is already hex
    /// (`crates/traits/src/types.rs:43-47`, the `byte_id!` macro), so this is
    /// just `group_id.to_string()` / `hex::decode(..)` +
    /// `GroupId::new(bytes)` to round-trip.
    pub group_id_hex: Option<String>,
}

/// A workspace invite code this account has issued (via `moyu invite`),
/// recorded so a later `requests`/`approve` flow can look up who a given
/// invite secret belongs to and whether it should be auto-approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedInvite {
    /// Hex-encoded invite secret handed out to the invitee.
    pub secret_hex: String,
    /// hex-encoded `cgka_traits::GroupId` of the workspace this invite grants
    /// access to.
    pub ws_gid_hex: String,
    /// Local friendly name of the workspace, for display.
    pub ws_name: String,
    /// Whether a join request presenting this secret should be
    /// auto-approved rather than requiring manual approval.
    pub auto_approve: bool,
    /// Unix seconds this invite was issued.
    pub created_at: u64,
    /// Unix seconds after which this invite authorizes nothing (no trust
    /// badge, no auto-approve). `None` only in files written before expiry
    /// existed; those expire [`INVITE_TTL_SECS`] after `created_at`, so an
    /// old never-expiring code does not stay live forever. Read it through
    /// [`IssuedInvite::expiry`].
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// How long an invite code stays valid after it is issued. An invite is a
/// bearer token: anyone who sees the code can present it, so it must not live
/// forever.
pub const INVITE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

impl IssuedInvite {
    /// The instant (unix seconds) this invite stops being valid.
    pub fn expiry(&self) -> u64 {
        self.expires_at
            .unwrap_or_else(|| self.created_at.saturating_add(INVITE_TTL_SECS))
    }

    /// Whether a join request first seen locally at `seen_at` may use this
    /// invite. `seen_at` must be a LOCAL clock reading (when this device
    /// received the request), never the requester's own timestamp -- a
    /// requester could backdate that.
    pub fn is_valid_at(&self, seen_at: u64) -> bool {
        seen_at <= self.expiry()
    }
}

/// What this account already did about join requests from one requester for
/// one workspace. Exists so a join request is acted on at most once: without
/// it, the only "is this request still open?" test was "is the requester a
/// member right now?", which made every old request look open again the
/// moment its sender was removed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinLedgerEntry {
    /// hex `GroupId` of the workspace.
    pub ws_gid_hex: String,
    /// hex pubkey of the requester.
    pub sender_hex: String,
    /// Requests from this sender for this workspace that this device stored
    /// at or before this point are done with (approved, or found already a
    /// member). It is the request row's local insert order in the message
    /// store -- a number only this device assigns. The request's own
    /// timestamp is not used: its sender chooses that.
    #[serde(default)]
    pub handled_order: i64,
    /// Set once this requester has been removed from the workspace: an
    /// auto-approve invite must never put them back. A manual `approve` still
    /// can -- that is an admin's explicit decision.
    #[serde(default)]
    pub auto_blocked: bool,
}

/// Current on-disk schema version of `moyu-state.json`. Bump when a field
/// changes meaning; additive optional fields do not need a bump.
pub const SCHEMA_VERSION: u32 = 1;

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreState {
    /// Schema version. Files written before this field existed (0.1.x) have
    /// none and read as 1; a file from a NEWER moyu is refused rather than
    /// half-parsed (see [`MoyuStore::open`]).
    #[serde(default = "schema_version")]
    v: u32,
    contacts: Vec<Contact>,
    /// Unix seconds this account's KeyPackage was last published/rotated.
    keypackage_published_at: Option<u64>,
    #[serde(default)]
    issued_invites: Vec<IssuedInvite>,
    #[serde(default)]
    join_ledger: Vec<JoinLedgerEntry>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            v: SCHEMA_VERSION,
            contacts: Vec::new(),
            keypackage_published_at: None,
            issued_invites: Vec::new(),
            join_ledger: Vec::new(),
        }
    }
}

pub struct MoyuStore {
    path: PathBuf,
    state: StoreState,
}

impl MoyuStore {
    /// `<root>/accounts/<label>/moyu-state.json`. `root` must be the same
    /// directory passed to `AccountHome::open_with_secret_store` /
    /// `MarmotApp::with_relays_and_account_home` (see `crate::engine`).
    pub fn open(root: &std::path::Path, label: &str) -> MoyuResult<Self> {
        let dir = root.join("accounts").join(label);
        fs_private::create_dir_all_private(&dir)?;
        let path = dir.join("moyu-state.json");

        let state = Self::read_state(&path)?.unwrap_or_default();
        Ok(Self { path, state })
    }

    /// The state file's contents, `None` if it does not exist yet. A file
    /// from a NEWER moyu is refused rather than half-parsed.
    fn read_state(path: &std::path::Path) -> MoyuResult<Option<StoreState>> {
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        let state: StoreState = serde_json::from_slice(&bytes)?;
        if state.v > SCHEMA_VERSION {
            return Err(MoyuError::Other(format!(
                "{} is schema v{} but this moyu understands v{} -- upgrade moyu (a newer \
                 version wrote this file)",
                path.display(),
                state.v,
                SCHEMA_VERSION
            )));
        }
        Ok(Some(state))
    }

    /// Write to a temp file and rename it over the real one, so another
    /// handle re-reading the file never sees it half-written (writers now
    /// re-read before every change -- see [`Self::mutate`]).
    fn save(&self) -> MoyuResult<()> {
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        let tmp = self.path.with_extension("json.tmp");
        fs_private::write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Replace this handle's copy with what is on disk now. A long-lived
    /// handle calls this before it READS something another process may have
    /// changed (an invite issued or revoked in another terminal).
    pub fn reload(&mut self) -> MoyuResult<()> {
        if let Some(on_disk) = Self::read_state(&self.path)? {
            self.state = on_disk;
        }
        Ok(())
    }

    /// Apply one change and persist it. The file is re-read first, so the
    /// change lands on top of whatever is on disk NOW rather than on this
    /// handle's possibly stale copy: more than one handle is routinely open
    /// on the same file (a long-lived `session`/`tui`/`recv --follow` next to
    /// a one-shot `moyu invite` in another terminal), and writing a stale
    /// whole-file copy back would silently drop the other writer's change.
    /// An unreadable file is an error -- it is never overwritten.
    fn mutate<R>(&mut self, change: impl FnOnce(&mut StoreState) -> R) -> MoyuResult<R> {
        if let Some(on_disk) = Self::read_state(&self.path)? {
            self.state = on_disk;
        }
        let out = change(&mut self.state);
        self.save()?;
        Ok(out)
    }

    pub fn contacts(&self) -> &[Contact] {
        &self.state.contacts
    }

    pub fn find_contact(&self, npub_or_label: &str) -> Option<&Contact> {
        self.state
            .contacts
            .iter()
            .find(|c| c.npub == npub_or_label || c.label == npub_or_label)
    }

    pub fn upsert_contact(&mut self, contact: Contact) -> MoyuResult<()> {
        self.mutate(|state| {
            if let Some(existing) = state.contacts.iter_mut().find(|c| c.npub == contact.npub) {
                *existing = contact;
            } else {
                state.contacts.push(contact);
            }
        })
    }

    pub fn set_contact_group_id(&mut self, npub: &str, group_id_hex: String) -> MoyuResult<()> {
        self.mutate(|state| {
            if let Some(c) = state.contacts.iter_mut().find(|c| c.npub == npub) {
                c.group_id_hex = Some(group_id_hex);
            }
        })
    }

    pub fn keypackage_published_at(&self) -> Option<u64> {
        self.state.keypackage_published_at
    }

    pub fn mark_keypackage_published(&mut self, now_unix_secs: u64) -> MoyuResult<()> {
        self.mutate(|state| state.keypackage_published_at = Some(now_unix_secs))
    }

    pub fn issued_invites(&self) -> &[IssuedInvite] {
        &self.state.issued_invites
    }

    pub fn record_issued_invite(&mut self, inv: IssuedInvite) -> MoyuResult<()> {
        self.mutate(|state| state.issued_invites.push(inv))
    }

    pub fn find_issued_by_secret(&self, secret_hex: &str) -> Option<&IssuedInvite> {
        self.state
            .issued_invites
            .iter()
            .find(|i| i.secret_hex == secret_hex)
    }

    /// Forget every invite issued for one workspace, so codes already handed
    /// out stop working (no trust badge, no auto-approve). Returns how many
    /// were dropped. Does not touch anyone who already joined.
    pub fn revoke_issued_invites(&mut self, ws_gid_hex: &str) -> MoyuResult<usize> {
        self.mutate(|state| {
            let before = state.issued_invites.len();
            state
                .issued_invites
                .retain(|i| !i.ws_gid_hex.eq_ignore_ascii_case(ws_gid_hex));
            before - state.issued_invites.len()
        })
    }

    /// The ledger entry for one (workspace, requester), if anything was ever
    /// recorded about them.
    pub fn join_entry(&self, ws_gid_hex: &str, sender_hex: &str) -> Option<&JoinLedgerEntry> {
        self.state.join_ledger.iter().find(|e| {
            e.ws_gid_hex.eq_ignore_ascii_case(ws_gid_hex)
                && e.sender_hex.eq_ignore_ascii_case(sender_hex)
        })
    }

    /// Record that `sender`'s join requests for this workspace stored at or
    /// before `request_order` (the request row's local insert order) are
    /// done with. Never moves backwards.
    pub fn mark_join_handled(
        &mut self,
        ws_gid_hex: &str,
        sender_hex: &str,
        request_order: i64,
    ) -> MoyuResult<()> {
        self.mutate(|state| {
            let e = ledger_entry_mut(state, ws_gid_hex, sender_hex);
            e.handled_order = e.handled_order.max(request_order);
        })
    }

    /// Record that `sender` was removed from this workspace, so no
    /// auto-approve invite re-admits them.
    pub fn block_auto_join(&mut self, ws_gid_hex: &str, sender_hex: &str) -> MoyuResult<()> {
        self.mutate(|state| ledger_entry_mut(state, ws_gid_hex, sender_hex).auto_blocked = true)
    }
}

fn ledger_entry_mut<'a>(
    state: &'a mut StoreState,
    ws_gid_hex: &str,
    sender_hex: &str,
) -> &'a mut JoinLedgerEntry {
    let at = state.join_ledger.iter().position(|e| {
        e.ws_gid_hex.eq_ignore_ascii_case(ws_gid_hex)
            && e.sender_hex.eq_ignore_ascii_case(sender_hex)
    });
    let at = at.unwrap_or_else(|| {
        state.join_ledger.push(JoinLedgerEntry {
            ws_gid_hex: ws_gid_hex.to_ascii_lowercase(),
            sender_hex: sender_hex.to_ascii_lowercase(),
            ..JoinLedgerEntry::default()
        });
        state.join_ledger.len() - 1
    });
    &mut state.join_ledger[at]
}

// ---------------------------------------------------------------------------
// Active-account bookkeeping.
//
// `marmot_account::AccountHome::create_nostr_account` picks the account
// *label* for you (it is the account's pubkey hex, not a name the user
// chooses — confirmed `home.rs:142-145`), so after `moyu init` succeeds,
// something has to remember "which label is the one to use by default" for
// every later command (`whoami`, `keypackage publish`, `add`, `chat`) so the
// user is never made to type a 64-hex-char label. This is a process-wide,
// not per-account, fact, so it lives directly under `root` rather than under
// `root/accounts/<label>/` like `MoyuStore`/`Argon2idFileSecretStore` do.
// ---------------------------------------------------------------------------

fn active_account_path(root: &std::path::Path) -> PathBuf {
    root.join("active-account.txt")
}

pub fn read_active_account_label(root: &std::path::Path) -> MoyuResult<Option<String>> {
    let path = active_account_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)?;
    let label = contents.trim();
    if label.is_empty() {
        Ok(None)
    } else {
        Ok(Some(label.to_string()))
    }
}

pub fn write_active_account_label(root: &std::path::Path, label: &str) -> MoyuResult<()> {
    fs_private::create_dir_all_private(root)?;
    fs_private::write_private(&active_account_path(root), label.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_invites_persist_and_lookup() {
        // 现有约定:std::env::temp_dir() + 唯一名(keypackage_rotation.rs:98)。
        let root = std::env::temp_dir().join(format!("moyu-store-issued-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root = root.as_path();
        let mut s = MoyuStore::open(root, "lbl").unwrap();
        assert!(s.issued_invites().is_empty());
        s.record_issued_invite(IssuedInvite {
            secret_hex: "aa".repeat(16),
            ws_gid_hex: "bb".repeat(32),
            ws_name: "eng".into(),
            auto_approve: true,
            created_at: 42,
            expires_at: None,
        })
        .unwrap();
        // 重开验证落盘
        let s2 = MoyuStore::open(root, "lbl").unwrap();
        assert_eq!(s2.issued_invites().len(), 1);
        assert_eq!(
            s2.find_issued_by_secret(&"aa".repeat(16)).unwrap().ws_name,
            "eng"
        );
        assert!(s2.find_issued_by_secret("nope").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    fn schema_temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "moyu-store-schema-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("accounts").join("acct")).unwrap();
        dir
    }

    #[test]
    fn pre_versioned_state_file_reads_as_schema_1() {
        let root = schema_temp_root("old");
        std::fs::write(
            root.join("accounts/acct/moyu-state.json"),
            br#"{"contacts":[],"keypackage_published_at":null}"#,
        )
        .unwrap();
        let store = MoyuStore::open(&root, "acct").expect("0.1.x file loads");
        assert_eq!(store.state.v, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn newer_schema_is_refused_with_a_clear_error() {
        let root = schema_temp_root("newer");
        std::fs::write(
            root.join("accounts/acct/moyu-state.json"),
            br#"{"v":99,"contacts":[],"keypackage_published_at":null}"#,
        )
        .unwrap();
        let msg = match MoyuStore::open(&root, "acct") {
            Ok(_) => panic!("a v99 state file must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("schema v99"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn invite(secret: &str, ws: &str, created_at: u64, expires_at: Option<u64>) -> IssuedInvite {
        IssuedInvite {
            secret_hex: secret.into(),
            ws_gid_hex: ws.into(),
            ws_name: "eng".into(),
            auto_approve: true,
            created_at,
            expires_at,
        }
    }

    #[test]
    fn invite_expiry_is_explicit_or_created_plus_ttl() {
        // An explicit expiry wins.
        let inv = invite("s", "ws", 1_000, Some(1_500));
        assert_eq!(inv.expiry(), 1_500);
        assert!(inv.is_valid_at(1_500));
        assert!(!inv.is_valid_at(1_501));
        // A record written before expiry existed must NOT be immortal.
        let legacy = invite("s", "ws", 1_000, None);
        assert_eq!(legacy.expiry(), 1_000 + INVITE_TTL_SECS);
        assert!(!legacy.is_valid_at(1_000 + INVITE_TTL_SECS + 1));
        // No overflow panic on an absurd created_at.
        assert_eq!(invite("s", "ws", u64::MAX, None).expiry(), u64::MAX);
    }

    #[test]
    fn a_pre_expiry_state_file_still_loads() {
        let root = schema_temp_root("noexp");
        std::fs::write(
            root.join("accounts/acct/moyu-state.json"),
            br#"{"v":1,"contacts":[],"keypackage_published_at":null,"issued_invites":[
                {"secret_hex":"aa","ws_gid_hex":"bb","ws_name":"eng","auto_approve":true,"created_at":5}]}"#,
        )
        .unwrap();
        let store = MoyuStore::open(&root, "acct").expect("0.2.0 file loads");
        let inv = store.find_issued_by_secret("aa").unwrap();
        assert_eq!(inv.expires_at, None);
        assert_eq!(inv.expiry(), 5 + INVITE_TTL_SECS);
        assert!(store.join_entry("bb", "anyone").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn revoke_drops_only_that_workspaces_invites() {
        let root = schema_temp_root("revoke");
        let mut s = MoyuStore::open(&root, "acct").unwrap();
        s.record_issued_invite(invite("a1", "AA11", 1, None))
            .unwrap();
        s.record_issued_invite(invite("a2", "aa11", 2, None))
            .unwrap();
        s.record_issued_invite(invite("b1", "bb22", 3, None))
            .unwrap();
        assert_eq!(s.revoke_issued_invites("aa11").unwrap(), 2);
        assert!(s.find_issued_by_secret("a1").is_none());
        assert!(s.find_issued_by_secret("a2").is_none());
        assert!(s.find_issued_by_secret("b1").is_some());
        assert_eq!(s.revoke_issued_invites("aa11").unwrap(), 0);
        let reopened = MoyuStore::open(&root, "acct").unwrap();
        assert_eq!(reopened.issued_invites().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn join_ledger_marks_persist_and_never_move_backwards() {
        let root = schema_temp_root("ledger");
        let mut s = MoyuStore::open(&root, "acct").unwrap();
        assert!(s.join_entry("ws", "bob").is_none());
        s.mark_join_handled("WS", "Bob", 100).unwrap();
        s.mark_join_handled("ws", "bob", 40).unwrap(); // older: ignored
        s.block_auto_join("ws", "BOB").unwrap();
        s.mark_join_handled("ws", "carol", 7).unwrap();
        let reopened = MoyuStore::open(&root, "acct").unwrap();
        let bob = reopened.join_entry("ws", "bob").unwrap();
        assert_eq!(bob.handled_order, 100);
        assert!(bob.auto_blocked);
        let carol = reopened.join_entry("ws", "carol").unwrap();
        assert_eq!(carol.handled_order, 7);
        assert!(!carol.auto_blocked);
        // a different workspace is a different entry
        assert!(reopened.join_entry("other", "bob").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two handles on one file (a long-lived session next to a one-shot
    /// command) must not erase each other's writes.
    #[test]
    fn a_stale_handle_does_not_clobber_another_writers_change() {
        let root = schema_temp_root("twohandles");
        let mut long_lived = MoyuStore::open(&root, "acct").unwrap();
        long_lived.mark_keypackage_published(1).unwrap();

        let mut one_shot = MoyuStore::open(&root, "acct").unwrap();
        one_shot
            .record_issued_invite(invite("s1", "ws", 9, None))
            .unwrap();
        one_shot.block_auto_join("ws", "bob").unwrap();

        // The long-lived handle still holds the pre-invite state in memory.
        long_lived.mark_keypackage_published(2).unwrap();

        let reopened = MoyuStore::open(&root, "acct").unwrap();
        assert_eq!(reopened.keypackage_published_at(), Some(2));
        assert!(
            reopened.find_issued_by_secret("s1").is_some(),
            "invite lost"
        );
        assert!(
            reopened.join_entry("ws", "bob").unwrap().auto_blocked,
            "ledger lost"
        );
        // ...and the long-lived handle now sees them too.
        assert!(long_lived.find_issued_by_secret("s1").is_some());
        // `reload` picks up a change without having to write anything.
        one_shot.revoke_issued_invites("ws").unwrap();
        assert!(
            long_lived.find_issued_by_secret("s1").is_some(),
            "still the old copy"
        );
        long_lived.reload().unwrap();
        assert!(long_lived.find_issued_by_secret("s1").is_none());
        // no temp file is left behind
        assert!(!root.join("accounts/acct/moyu-state.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_store_persists_the_schema_version() {
        let root = schema_temp_root("fresh");
        let mut store = MoyuStore::open(&root, "acct").unwrap();
        store.mark_keypackage_published(1).unwrap();
        let bytes = std::fs::read(root.join("accounts/acct/moyu-state.json")).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["v"], serde_json::json!(SCHEMA_VERSION));
        let _ = std::fs::remove_dir_all(&root);
    }
}
