//! The invite/join-request pipeline: finding the join requests that are
//! still open in local message history, the bearer-secret trust check
//! (scoped to the exact workspace an invite was issued for, and to the
//! invite's lifetime), and [`approve_one`], the admin-gated core shared by
//! one-shot `approve` and `recv`'s self-healing auto-approve.
//!
//! A join request is acted on at most once. "Is the requester a member right
//! now?" is NOT the test for whether a request is still open: a member who
//! was removed is a non-member again, and their old request would look open
//! for ever. A request stays open only until something happens to its sender
//! in that workspace (see [`open_join_requests`]).

use std::collections::HashMap;

use moyu_core::engine::{AppClient, AppMessageRecord, MoyuEngine};
use moyu_core::store::{self, MoyuStore};
use moyu_core::workspace::{MembershipChange, membership_change};
use moyu_core::{governance, invite};

use super::content::CHAT_MESSAGE_KIND;
use super::engine::send_control;
use super::membership::workspace_gov_by_gid;
use super::resolve::resolve_workspace_by_gid;

/// One `moyu join` request against a workspace we (may) administer: the
/// join-request envelope's payload plus who sent it and when, after dedup
/// (see [`pending_from_records`]).
#[derive(Debug, Clone)]
pub(crate) struct PendingJoin {
    pub sender_hex: String,
    pub ws_gid_hex: String,
    pub ws_name: String,
    pub secret_hex: String,
    /// The requester's own send time (`recorded_at`), for display only. The
    /// requester chooses it, so nothing is decided by it.
    pub ts: u64,
    /// When THIS device stored the request (local clock). Decides whether the
    /// invite had expired.
    pub received_at: u64,
    /// Where the request row sits in this device's message store (its row
    /// id). This, not a timestamp, is what "earlier" and "later" mean below:
    /// the store assigns it once, in arrival order, and never rewrites it.
    pub insert_order: i64,
    /// The request's secret matches an invite this account issued for exactly
    /// this workspace, and that invite was still valid when the request
    /// arrived here. Filled in by [`open_join_requests`].
    pub trusted: bool,
    /// The request may be approved without a human: `trusted`, the invite was
    /// issued with `--auto-approve`, and the requester has never been removed
    /// from this workspace. Filled in by [`open_join_requests`].
    pub auto_ok: bool,
}

/// Pure filter/dedup over an account's whole message history (any group,
/// typically the 1:1 DM the joiner sent it into): keep inbound kind-9
/// messages that decode as a [`invite::JoinRequest`] envelope
/// (`invite::parse_join_request`), then collapse repeats from the same
/// sender FOR THE SAME WORKSPACE down to the single LATEST one (the one this
/// device stored last) -- a stale re-join (e.g. after the invite code's secret
/// rotated) must not leave a ghost entry alongside the fresh request.
/// `trusted`/`auto_ok` are left `false`; [`open_join_requests`] decides them.
///
/// `direction == "received"` (NOT `"inbound"`) is `AppMessageRecord`'s actual
/// inbound marker -- confirmed against `marmot-app`'s `client/sync.rs`
/// (`direction: "received".to_owned()` for a synced app message) and
/// `client/projection.rs` (`"sent"` for our own outgoing messages); `tui.rs`
/// keys its own "is this mine" check off `r.direction == "sent"` the same
/// way. A join-request is always something WE received (the other party's
/// `send`), so this excludes our own sent copy of anything that happens to
/// parse the same way.
pub(crate) fn pending_from_records(records: &[AppMessageRecord]) -> Vec<PendingJoin> {
    let mut latest: HashMap<(String, String), PendingJoin> = HashMap::new();
    for r in records {
        if r.kind != CHAT_MESSAGE_KIND {
            continue;
        }
        if r.direction != "received" {
            continue;
        }
        let Some(jr) = invite::parse_join_request(&r.plaintext) else {
            continue;
        };
        let key = (
            r.sender.to_ascii_lowercase(),
            jr.ws_gid_hex.to_ascii_lowercase(),
        );
        let pj = PendingJoin {
            sender_hex: r.sender.clone(),
            ws_gid_hex: jr.ws_gid_hex,
            ws_name: jr.ws_name,
            secret_hex: jr.secret_hex,
            ts: r.recorded_at,
            received_at: r.received_at,
            insert_order: r.insert_order,
            trusted: false,
            auto_ok: false,
        };
        latest
            .entry(key)
            .and_modify(|e| {
                if pj.insert_order > e.insert_order {
                    *e = pj.clone();
                }
            })
            .or_insert(pj);
    }
    let mut out: Vec<PendingJoin> = latest.into_values().collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.insert_order));
    out
}

/// The join requests that are still OPEN, most recent first, each with its
/// `trusted` / `auto_ok` verdict. Pure over the account's message history and
/// the local store, so every rule below is unit-tested without an engine.
///
/// A request is closed -- dropped here -- once either holds:
///
/// - the local ledger says requests from this sender for this workspace up to
///   that send time were already dealt with (`approve_one` records that); or
/// - the workspace's own history shows a membership change about the sender
///   (added, removed, or left) that this device stored AFTER it stored the
///   request. The request was answered, or overtaken. Those rows are written
///   by MDK from the MLS commits themselves, so this also covers a change
///   made by a different admin.
///
/// Both tests order by the row id this device's message store assigned, not
/// by any timestamp. The requester picks the request's timestamp, a few
/// minutes of clock skew between two people must not make a fresh request
/// look old (or an old one fresh), and MDK rewrites a row's `received_at`
/// when it re-records it (crash replay, relay redelivery) while the row id
/// stays put.
///
/// `auto_ok` is refused to anyone who was EVER removed from the workspace
/// (ledger flag, or a `member_removed` row for them anywhere in its history):
/// an auto-approve invite is a bearer code, and someone who was kicked still
/// holds it. They can ask again and an admin can `approve` them by hand.
/// This is as good as this device's own records: an admin who joined the
/// workspace after the removal has no row for it (MLS history starts at
/// join) and no ledger entry, so a code THAT admin issues does not know.
///
/// "Already a member" is deliberately not checked here (it needs the live
/// group state): `requests` filters on it and `approve_one` no-ops on it.
pub(crate) fn open_join_requests(
    records: &[AppMessageRecord],
    store: &MoyuStore,
) -> Vec<PendingJoin> {
    let mut changes: HashMap<(String, String), Vec<(MembershipChange, &AppMessageRecord)>> =
        HashMap::new();
    for r in records {
        if let Some((change, subject_hex)) = membership_change(r) {
            changes
                .entry((r.group_id_hex.to_ascii_lowercase(), subject_hex))
                .or_default()
                .push((change, r));
        }
    }
    pending_from_records(records)
        .into_iter()
        .filter_map(|mut pj| {
            let key = (
                pj.ws_gid_hex.to_ascii_lowercase(),
                pj.sender_hex.to_ascii_lowercase(),
            );
            let history = changes.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            let ledger = store.join_entry(&pj.ws_gid_hex, &pj.sender_hex);

            let handled = ledger.is_some_and(|e| e.handled_order >= pj.insert_order);
            let overtaken = history
                .iter()
                .any(|(_, r)| r.insert_order > pj.insert_order);
            if handled || overtaken {
                return None;
            }

            let removed_before = ledger.is_some_and(|e| e.auto_blocked)
                || history
                    .iter()
                    .any(|(change, _)| *change == MembershipChange::Removed);
            let issued = scoped_issued(store, &pj.secret_hex, &pj.ws_gid_hex, pj.received_at);
            pj.trusted = issued.is_some();
            pj.auto_ok = issued.is_some_and(|inv| inv.auto_approve) && !removed_before;
            Some(pj)
        })
        .collect()
}

/// Thin wrapper over [`open_join_requests`] for the real DB-backed path.
pub(crate) fn scan_join_requests(
    engine: &MoyuEngine,
    store: &MoyuStore,
    label: &str,
) -> anyhow::Result<Vec<PendingJoin>> {
    Ok(open_join_requests(&engine.messages(label)?, store))
}

/// The issued invite that authorizes a join request — matched on the bearer
/// secret, the `ws_gid` it was minted for, AND its lifetime. A secret that
/// matches a known invite but names a DIFFERENT `ws_gid` (a leaked bearer
/// secret replayed against another workspace) returns `None`, and so does one
/// whose invite had already expired when the request reached this device
/// (`seen_at`, a local clock reading -- see `IssuedInvite::is_valid_at`). In
/// both cases the request is neither badged trusted nor auto-approved.
pub(crate) fn scoped_issued<'a>(
    store: &'a MoyuStore,
    secret_hex: &str,
    ws_gid_hex: &str,
    seen_at: u64,
) -> Option<&'a store::IssuedInvite> {
    store
        .find_issued_by_secret(secret_hex)
        .filter(|inv| inv.ws_gid_hex.eq_ignore_ascii_case(ws_gid_hex))
        .filter(|inv| inv.is_valid_at(seen_at))
}

/// The advisory trust badge for a join request: ✓ when its secret matches an
/// invite we issued for that exact workspace, ⚠ otherwise. Purely advisory --
/// `approve` works either way (see [`cmd_requests`]).
pub(crate) fn trust_badge(trusted: bool) -> &'static str {
    if trusted {
        "✓ 持码可信"
    } else {
        "⚠ 无凭证"
    }
}

/// The LOCALLY-verified `(display_name, trusted)` for a join request, keyed off
/// the envelope's `ws_gid` -- NEVER its attacker-controlled `ws_name`. The name
/// comes from the real local workspace projection, falling back to the unverified
/// claim only when the gid names no workspace we belong to (in which case the
/// request also isn't approvable). This is the display-layer twin of the
/// bearer-secret authorization fix: `recv` and `requests` both route through it so the surface
/// a human drives `approve` off can never show one workspace while the grant
/// lands in another.
pub(crate) fn join_request_trust(
    engine: &MoyuEngine,
    client: &AppClient,
    store: &MoyuStore,
    label: &str,
    request: &invite::JoinRequest,
    seen_at: u64,
) -> (String, bool) {
    let trusted = scoped_issued(store, &request.secret_hex, &request.ws_gid_hex, seen_at).is_some();
    let name = resolve_workspace_by_gid(engine, client, label, &request.ws_gid_hex)
        .ok()
        .and_then(|(_, proj)| proj.name)
        .unwrap_or_else(|| request.ws_name.clone());
    (name, trusted)
}

/// Approve one pending join request: gate on the local account being an
/// admin of the target workspace, no-op success if the sender is already a
/// member (idempotent -- a re-run after a partial failure, or two admins
/// racing, must not error), then `invite_members` (retried with backoff --
/// the sender's KeyPackage may not have finished propagating to the relay
/// set yet right after their `join`) and broadcast a catch-up
/// `WorkspaceSnapshot` (mirrors `cmd_workspace_add`'s pattern) so they
/// converge on the current name/channel list without replaying pre-join
/// control history.
///
/// Either success outcome closes the request: it is recorded in the local
/// ledger so neither this path nor the auto-approve sweep acts on it again
/// (see [`open_join_requests`]).
pub(crate) async fn approve_one(
    engine: &MoyuEngine,
    client: &mut AppClient,
    store: &mut MoyuStore,
    label: &str,
    pj: &PendingJoin,
    auto: bool,
) -> anyhow::Result<crate::ops::ApproveOutcome> {
    let outcome = add_requester(engine, client, label, pj, auto).await?;
    // The member is in (or already was). A failed ledger write must not turn
    // that into an error -- but say so, because the request may then be
    // offered again.
    if let Err(e) = store.mark_join_handled(&pj.ws_gid_hex, &pj.sender_hex, pj.insert_order) {
        heprintln!("warning: could not record this join request as handled: {e}");
    }
    Ok(outcome)
}

async fn add_requester(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    pj: &PendingJoin,
    auto: bool,
) -> anyhow::Result<crate::ops::ApproveOutcome> {
    // Exact-gid resolution: the id is from an untrusted
    // join-request envelope, so `approve` must not prefix/name-match it.
    let (gid, me, admins, proj) = workspace_gov_by_gid(engine, client, label, &pj.ws_gid_hex)?;
    // The REAL local name of the group we're actually acting on
    // (`pj.ws_gid_hex`) -- never the envelope's attacker-controlled
    // `pj.ws_name`, so what's displayed always matches what's granted.
    let ws_display = proj.name.as_deref().unwrap_or(&pj.ws_name);
    if !governance::is_admin(&me, &admins) {
        anyhow::bail!("only an admin of #{} can approve members", ws_display);
    }
    let npub = moyu_core::identity::npub_from_hex(&pj.sender_hex)?;
    // Already a member -> no-op success (idempotent).
    if let Ok(members) = client.members(&gid)
        && members
            .iter()
            .any(|m| m.member_id_hex.eq_ignore_ascii_case(&pj.sender_hex))
    {
        return Ok(crate::ops::ApproveOutcome::AlreadyMember {
            npub,
            ws_name: ws_display.to_owned(),
        });
    }
    // Bounded retry: the sender's KeyPackage may not have propagated to the
    // relay set yet (they may have just run `join` seconds ago).
    let mut last_err = None;
    for attempt in 0..4u32 {
        match client.invite_members(&gid, &[npub.as_str()]).await {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        400 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(anyhow::anyhow!(
            "could not add {npub} (their KeyPackage may not have propagated yet): {e}"
        ));
    }

    // Broadcast the current name + channel list (mirrors `cmd_workspace_add`)
    // so the new member converges without replaying pre-join control history
    // (MLS forward secrecy hides it from them anyway). Best-effort: a
    // snapshot failure must never hide a successful invite.
    let _ = send_control(client, &gid, proj.snapshot_event("")).await;

    Ok(crate::ops::ApproveOutcome::Approved {
        npub,
        ws_name: ws_display.to_owned(),
        auto,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "abababababababababababababababab";

    fn rec(
        sender: &str,
        dir: &str,
        group: &str,
        kind: u64,
        plaintext: String,
        ts: u64,
    ) -> AppMessageRecord {
        AppMessageRecord {
            message_id_hex: format!("{sender}{ts}{kind}"),
            direction: dir.into(),
            group_id_hex: group.into(),
            sender: sender.into(),
            plaintext,
            kind,
            tags: vec![],
            source_epoch: None,
            recorded_at: ts,
            received_at: ts,
            // stored in the order of `ts` unless a test says otherwise
            insert_order: ts as i64,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        }
    }

    /// A join request from `sender` for [`WS`], sent and received at `ts`.
    fn request(sender: &str, secret: &str, ts: u64) -> AppMessageRecord {
        let body = invite::build_join_request_content(&invite::JoinRequest {
            ws_gid_hex: WS.into(),
            ws_name: "eng".into(),
            secret_hex: secret.into(),
        });
        rec(sender, "received", "dm", 9, body, ts)
    }

    /// The row MDK writes into a group's own history for a membership change
    /// (kind 1210, the subject in `data.subject`), stored locally at `ts`.
    fn change(system_type: &str, subject: &str, ts: u64) -> AppMessageRecord {
        let body = serde_json::json!({
            "v": 1,
            "system_type": system_type,
            "text": "",
            "data": { "actor": "admin", "subject": subject },
        })
        .to_string();
        rec("admin", "system", WS, 1210, body, ts)
    }

    fn temp_store(tag: &str) -> (std::path::PathBuf, MoyuStore) {
        let root = std::env::temp_dir().join(format!(
            "moyu-join-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MoyuStore::open(&root, "lbl").unwrap();
        (root, store)
    }

    /// An auto-approve invite for [`WS`] with secret `"s"`, issued at 0 and
    /// valid through `expires_at`.
    fn issue(store: &mut MoyuStore, auto_approve: bool, expires_at: u64) {
        store
            .record_issued_invite(store::IssuedInvite {
                secret_hex: "s".into(),
                ws_gid_hex: WS.into(),
                ws_name: "eng".into(),
                auto_approve,
                created_at: 0,
                expires_at: Some(expires_at),
            })
            .unwrap();
    }

    /// `pending_from_records`: filters history down to RECEIVED (not our own
    /// sent) kind-9 join-request envelopes, ignoring plain chat, and dedupes
    /// by sender keeping only the LATEST (highest `recorded_at`) request --
    /// so a repeated `moyu join` after a stale/rotated invite doesn't leave a
    /// ghost stale-secret entry alongside the fresh one. Also pins that
    /// `direction` is checked against the literal `"received"`
    /// (`AppMessageRecord`'s real inbound marker) -- an earlier draft of this
    /// filter checked for a nonexistent `"inbound"` value and silently
    /// matched nothing, a bug only a live-relay smoke test caught; the
    /// `"dave"` row below regression-guards that exact mistake.
    #[test]
    fn scan_dedups_and_filters_join_requests() {
        let msgs = vec![
            rec("alice", "received", "dm", 9, "just a chat".into(), 1), // 非 join-request → 忽略
            request("bob", "s1", 2),                                    // bob 早
            request("bob", "s2", 5),                                    // bob 晚 → 取这条
            request("carol", "s3", 3),
            // REGRESSION GUARD: a `"sent"` row (our own outgoing copy, e.g. an
            // echo/relay quirk) that otherwise decodes as a perfectly valid
            // join-request must NOT surface as pending -- only "received"
            // counts as someone actually asking to join.
            {
                let mut r = request("dave", "s4", 9);
                r.direction = "sent".into();
                r
            },
        ];
        let pending = pending_from_records(&msgs);
        assert_eq!(pending.len(), 2); // bob(去重)+ carol -- NOT dave
        let bob = pending.iter().find(|p| p.sender_hex == "bob").unwrap();
        assert_eq!(bob.secret_hex, "s2"); // 最新
        assert!(
            pending.iter().all(|p| p.sender_hex != "dave"),
            "a \"sent\" (outbound) row must never surface as a pending join request"
        );
    }

    /// One sender asking to join two different workspaces has two open
    /// requests, not one.
    #[test]
    fn requests_for_different_workspaces_are_kept_apart() {
        let other = {
            let body = invite::build_join_request_content(&invite::JoinRequest {
                ws_gid_hex: "cd".repeat(16),
                ws_name: "ops".into(),
                secret_hex: "x".into(),
            });
            rec("bob", "received", "dm", 9, body, 4)
        };
        let pending = pending_from_records(&[request("bob", "s", 2), other]);
        assert_eq!(pending.len(), 2);
    }

    /// A bearer invite `secret` alone must not be enough to badge ✓trusted /
    /// auto-approve -- it must ALSO match the `ws_gid` the invite was
    /// actually issued for. A holder of a leaked secret who replays it in a
    /// join-request naming a DIFFERENT workspace's `ws_gid` must get `None`.
    #[test]
    fn scoped_issued_requires_ws_gid_match() {
        let (root, mut store) = temp_store("scoped");
        issue(&mut store, true, 100);
        // same secret + same ws_gid -> authorized
        assert!(scoped_issued(&store, "s", WS, 1).is_some());
        // case-insensitive on the gid
        assert!(scoped_issued(&store, "s", &WS.to_ascii_uppercase(), 1).is_some());
        // same secret, DIFFERENT ws_gid (replay against another group) -> None
        assert!(scoped_issued(&store, "s", &"22".repeat(16), 1).is_none());
        // unknown secret -> None
        assert!(scoped_issued(&store, "nope", WS, 1).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scoped_issued_refuses_an_expired_invite() {
        let (root, mut store) = temp_store("expired");
        issue(&mut store, true, 100);
        assert!(scoped_issued(&store, "s", WS, 100).is_some());
        assert!(scoped_issued(&store, "s", WS, 101).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_request_with_a_valid_auto_invite_is_open_trusted_and_auto() {
        let (root, mut store) = temp_store("fresh");
        issue(&mut store, true, 100);
        let open = open_join_requests(&[request("bob", "s", 10)], &store);
        assert_eq!(open.len(), 1);
        assert!(open[0].trusted);
        assert!(open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manual_invite_is_trusted_but_never_auto() {
        let (root, mut store) = temp_store("manual");
        issue(&mut store, false, 100);
        let open = open_join_requests(&[request("bob", "s", 10)], &store);
        assert!(open[0].trusted);
        assert!(!open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The requester's own timestamp says "within the lifetime", but the
    /// request reached this device after the invite expired: not trusted, not
    /// auto. (A requester can backdate `recorded_at`; they cannot backdate
    /// when we received it.)
    #[test]
    fn a_request_arriving_after_expiry_is_open_but_untrusted() {
        let (root, mut store) = temp_store("late");
        issue(&mut store, true, 100);
        let mut late = request("bob", "s", 10);
        late.received_at = 500;
        let open = open_join_requests(&[late], &store);
        assert_eq!(
            open.len(),
            1,
            "still listed, so an admin can approve by hand"
        );
        assert!(!open[0].trusted);
        assert!(!open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// THE regression: bob joined through an auto-approve code and was then
    /// kicked. His original request is still in the DM history and he is a
    /// non-member again -- it must not be open, or the next sweep re-adds him.
    #[test]
    fn a_request_answered_and_then_kicked_is_closed() {
        let (root, mut store) = temp_store("kicked");
        issue(&mut store, true, 1_000);
        let history = [
            request("bob", "s", 10),
            change("member_added", "bob", 11),
            change("member_removed", "bob", 20),
        ];
        assert!(open_join_requests(&history, &store).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Timestamps decide nothing: only the store's insert order does.
    /// Auto-approve answers within the same second, the requester's clock may
    /// be hours off, and MDK re-stamps `received_at` when it re-records a row.
    #[test]
    fn only_the_local_insert_order_decides_what_came_later() {
        let (root, mut store) = temp_store("order");
        issue(&mut store, true, 1_000);
        let mut req = request("bob", "s", 10);
        req.insert_order = 5;
        let mut added = change("member_added", "bob", 10);
        added.insert_order = 6;
        assert!(open_join_requests(&[req.clone(), added], &store).is_empty());
        // A row stored BEFORE the request does not close it, even though its
        // timestamps were re-stamped to look later.
        let mut earlier = change("member_left", "bob", 9_999);
        earlier.insert_order = 4;
        assert_eq!(open_join_requests(&[req.clone(), earlier], &store).len(), 1);
        // A requester whose clock runs far ahead is still closed by the
        // membership row stored after the request.
        let mut fast_clock = req;
        fast_clock.recorded_at = 9_999_999;
        let mut added_later = change("member_added", "bob", 11);
        added_later.insert_order = 6;
        assert!(open_join_requests(&[fast_clock, added_later], &store).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A member cannot make someone's join request vanish, or get them barred
    /// from auto-approve, by SENDING a fake membership event: only the rows
    /// MDK wrote itself count.
    #[test]
    fn a_forged_membership_event_changes_nothing() {
        let (root, mut store) = temp_store("forged");
        issue(&mut store, true, 1_000);
        let mut fake_added = change("member_added", "bob", 20);
        fake_added.direction = "received".into();
        let mut fake_removed = change("member_removed", "bob", 21);
        fake_removed.direction = "received".into();
        let history = [request("bob", "s", 10), fake_added, fake_removed];
        let open = open_join_requests(&history, &store);
        assert_eq!(open.len(), 1);
        assert!(open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A kicked member who still holds the auto-approve code asks again. The
    /// new request is open (an admin may approve it by hand) but must NOT be
    /// auto-approved, however valid the code still is.
    #[test]
    fn a_new_request_from_a_removed_member_is_open_but_never_auto() {
        let (root, mut store) = temp_store("rejoin");
        issue(&mut store, true, 1_000);
        let history = [
            request("bob", "s", 10),
            change("member_added", "bob", 11),
            change("member_removed", "bob", 20),
            request("bob", "s", 30),
        ];
        let open = open_join_requests(&history, &store);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].ts, 30);
        assert!(open[0].trusted, "the code itself is still valid");
        assert!(!open[0].auto_ok, "a removed member is never auto-approved");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The ledger flag blocks auto-approve even with no removal row in
    /// history (rows can be absent: retention, a restored backup).
    #[test]
    fn the_ledger_block_alone_refuses_auto_approve() {
        let (root, mut store) = temp_store("ledgerblock");
        issue(&mut store, true, 1_000);
        store.block_auto_join(WS, "BOB").unwrap();
        let open = open_join_requests(&[request("bob", "s", 30)], &store);
        assert_eq!(open.len(), 1);
        assert!(!open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Someone who left on their own may come back through a valid code --
    /// but only with a NEW request, not by their old one being replayed.
    #[test]
    fn a_member_who_left_is_not_re_added_by_the_old_request_but_may_rejoin() {
        let (root, mut store) = temp_store("left");
        issue(&mut store, true, 1_000);
        let mut history = vec![
            request("bob", "s", 10),
            change("member_added", "bob", 11),
            change("member_left", "bob", 20),
        ];
        assert!(open_join_requests(&history, &store).is_empty());
        history.push(request("bob", "s", 30));
        let open = open_join_requests(&history, &store);
        assert_eq!(open.len(), 1);
        assert!(open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A request recorded as handled stays closed even if the workspace
    /// history carries no membership row for it at all.
    #[test]
    fn a_request_marked_handled_in_the_ledger_is_closed() {
        let (root, mut store) = temp_store("handled");
        issue(&mut store, true, 1_000);
        store.mark_join_handled(WS, "bob", 10).unwrap();
        assert!(open_join_requests(&[request("bob", "s", 10)], &store).is_empty());
        // a later request from the same person is a new request
        assert_eq!(
            open_join_requests(&[request("bob", "s", 11)], &store).len(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A membership change about SOMEONE ELSE, or in ANOTHER group, says
    /// nothing about this request.
    #[test]
    fn unrelated_membership_changes_do_not_close_a_request() {
        let (root, mut store) = temp_store("unrelated");
        issue(&mut store, true, 1_000);
        let mut elsewhere = change("member_removed", "bob", 20);
        elsewhere.group_id_hex = "ee".repeat(16);
        let history = [
            request("bob", "s", 10),
            change("member_removed", "carol", 20),
            elsewhere,
        ];
        let open = open_join_requests(&history, &store);
        assert_eq!(open.len(), 1);
        assert!(open[0].auto_ok);
        let _ = std::fs::remove_dir_all(&root);
    }
}
