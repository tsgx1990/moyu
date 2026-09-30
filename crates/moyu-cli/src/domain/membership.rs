//! The membership/governance projection: one `groups()`/`messages()`/
//! `members()` scan classified into workspaces (with their control-message
//! projection) and private channels ([`membership_view`]/[`membership_scan`]),
//! roster snapshotting and before/after diffing into human governance notices
//! ([`snapshot_rosters`]/[`governance_diff`]), and the admin-gated command
//! prologue ([`workspace_gov`]/[`workspace_gov_by_gid`]).

use std::collections::HashMap;

use moyu_core::engine::{
    AppClient, AppGroupMemberRecord, AppMessageRecord, GroupEvent, GroupId, GroupStateChange,
    MoyuEngine, SelfMembership, SyncSummary,
};
use moyu_core::governance;
use moyu_core::workspace::{self, WorkspaceProjection};

use super::engine::{group_id_from_hex, send_control};
use super::resolve::{resolve_workspace, resolve_workspace_by_gid};

/// One classification pass over every group this account is *still a member of*,
/// split into workspaces (with projection) and private channels — a single
/// `engine.messages()` + `engine.groups()` scan (so `workspace list` / `channel
/// list` / `resolve_workspace` cost O(history) total, not O(groups x history)).
/// Two membership filters run before classifying:
/// - a group whose `self_membership` is not `Member` (`Left` after `workspace
///   leave`, or `Removed`) is dropped -- otherwise a workspace we left would keep
///   listing forever and could make `resolve_workspace` permanently "ambiguous".
///   `self_membership` alone is NOT enough: MDK only flips it to `Removed` on the
///   sync *delivery* path (`sync.rs:485`), never when a peer's removal commit is
///   applied via the *convergence* path (`sync.rs:100`). So a member kicked by an
///   admin keeps `self_membership == Member` locally; the MLS roster is
///   authoritative, so we ALSO drop a group whose current `members()` no longer
///   contains the local account -- fail-open if the roster read errors, to never
///   hide a live group.
///
/// Classification is race-free (keyed on the MLS-protected `profile.name`, which
/// arrives with the Welcome): a private-channel-encoded name is peeled off as a
/// [`workspace::PrivateChannelRef`] (surfaced under its parent by `channel list`,
/// never as a top-level workspace); otherwise [`workspace::classify_group`] —
/// the canonical rule — splits a workspace from a 1:1 DM. A workspace with no
/// projected name yet falls back to its profile name so it never shows blank.
/// [`membership_view`]'s split: `(workspaces, private_channels)`, each keyed by
/// `group_id_hex` (workspaces carry their projection).
pub(crate) type MembershipView = (
    Vec<(String, WorkspaceProjection)>,
    Vec<workspace::PrivateChannelRef>,
);

pub(crate) fn membership_view(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> anyhow::Result<MembershipView> {
    let (rows, private_channels) = membership_scan(engine, client, label)?;
    Ok((
        rows.into_iter().map(|r| (r.ghex, r.proj)).collect(),
        private_channels,
    ))
}

/// One kept workspace from a [`membership_scan`] pass, carrying the raw
/// per-group data the scan already read so per-tick callers
/// ([`snapshot_rosters`]) don't re-read the store for it: the group's admin
/// set (from the same `groups()` read) and its live roster member hexes (from
/// the same `members()` read; `None` when that read failed — or the group id
/// hex was corrupt — and the group was kept fail-open).
pub(crate) struct WorkspaceRow {
    pub ghex: String,
    pub proj: WorkspaceProjection,
    pub admins: Vec<String>,
    pub roster: Option<Vec<String>>,
}

/// The single classification pass behind [`membership_view`] /
/// [`snapshot_rosters`]: one `groups()` + one `messages()` + one per-group
/// `members()` read.
pub(crate) fn membership_scan(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> anyhow::Result<(Vec<WorkspaceRow>, Vec<workspace::PrivateChannelRef>)> {
    let groups = engine.groups(label)?;
    let msgs = engine.messages(label)?;
    let mut by_group: HashMap<String, Vec<AppMessageRecord>> = HashMap::new();
    for m in msgs {
        by_group.entry(m.group_id_hex.clone()).or_default().push(m);
    }
    let mut workspaces = Vec::new();
    let mut private_channels = Vec::new();
    for g in groups {
        if g.self_membership != SelfMembership::Member {
            continue; // left/removed -- no longer ours to list or resolve
        }
        // Authoritative "am I still in this group?" check -- catches an admin
        // kick that left `self_membership` stale. Only drops when the roster read
        // SUCCEEDS and I'm absent; a read error keeps the group (fail-open).
        let roster = group_id_from_hex(&g.group_id_hex)
            .ok()
            .and_then(|gid| client.members(&gid).ok());
        if let Some(members) = &roster
            && !members.iter().any(|m| m.local)
        {
            continue;
        }
        // Peel a private channel off first: it is a separate MLS group nested
        // under a parent workspace, never a top-level workspace.
        if let Some(link) = workspace::decode_private_channel_name(&g.profile.name) {
            private_channels.push(workspace::PrivateChannelRef::from_link(
                g.group_id_hex.clone(),
                link,
            ));
            continue;
        }
        let recs = by_group
            .get(&g.group_id_hex)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut proj = workspace::project_workspace(recs);
        // Private channels were peeled off above, so anything that is not a
        // workspace here is a 1:1 DM (profile name "dm", no moyu name).
        if workspace::classify_group(&g.profile.name, proj.name.as_deref())
            != workspace::GroupKind::Workspace
        {
            continue;
        }
        // Fall back to the MDK profile name so a just-joined workspace shows a
        // real name before its workspace.rename/snapshot control event lands.
        if proj.name.is_none() {
            proj.name = Some(g.profile.name.clone());
        }
        workspaces.push(WorkspaceRow {
            ghex: g.group_id_hex,
            proj,
            admins: g.admin_policy.admins,
            roster: roster.map(|ms| ms.into_iter().map(|m| m.member_id_hex).collect()),
        });
    }
    Ok((workspaces, private_channels))
}

/// Every workspace this account still belongs to (thin wrapper over
/// [`membership_view`]; private channels and DMs excluded). Its own name because
/// `resolve_workspace` / `workspace list` / `snapshot_rosters` want only the
/// workspace half.
pub(crate) fn all_workspaces(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> anyhow::Result<Vec<(String, WorkspaceProjection)>> {
    Ok(membership_view(engine, client, label)?.0)
}

/// Catch-up gate: did this sync tick observe a member being **added** to a
/// group? MDK synthesizes `member_added` locally as a `GroupStateChanged`
/// engine event (Approach A — no kind-1210 message is sent on the wire), so it
/// surfaces in [`SyncSummary::events`], **never** `SyncSummary::messages`
/// (which is wire-received app messages only). An existing member observing
/// another's join — the actor the §5.2 fallback is built around, including one
/// applying the commit for the first time on reconnect — sees exactly this.
///
/// The local account's *own* join surfaces as `joined_groups`/`GroupJoined`
/// instead, and is deliberately NOT a trigger: a fresh joinee's projection is
/// [`workspace::WorkspaceProjection::is_default`], so `catch_up_snapshot`
/// returns `None` for it anyway (the thin-snapshot guard).
pub(crate) fn saw_member_added(summary: &SyncSummary) -> bool {
    summary.events.iter().any(|e| {
        matches!(
            e,
            GroupEvent::GroupStateChanged {
                change: GroupStateChange::MemberAdded { .. },
                ..
            }
        )
    })
}

/// Catch-up fallback driver: for every workspace this account still belongs
/// to, re-broadcast a catch-up snapshot when a member's join is not yet covered
/// by one (see [`workspace::catch_up_snapshot`]). One `engine.messages()` scan
/// bucketed by group, mirroring [`all_workspaces`]. Best-effort: a send failure
/// is logged, never propagated. Returns how many snapshots were sent.
///
/// `catch_up_snapshot` returns `None` for DMs and untouched workspaces
/// (`is_default`), so no separate DM filter is needed here.
pub(crate) async fn broadcast_catch_up_snapshots(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
) -> usize {
    let (groups, msgs) = match (engine.groups(label), engine.messages(label)) {
        (Ok(g), Ok(m)) => (g, m),
        _ => return 0,
    };
    let mut by_group: HashMap<String, Vec<AppMessageRecord>> = HashMap::new();
    for m in msgs {
        by_group.entry(m.group_id_hex.clone()).or_default().push(m);
    }
    let mut sent = 0usize;
    for g in &groups {
        if g.self_membership != SelfMembership::Member {
            continue;
        }
        let recs = by_group
            .get(&g.group_id_hex)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let Some(ev) = workspace::catch_up_snapshot(recs, &g.profile.name) else {
            continue;
        };
        let Ok(gid) = group_id_from_hex(&g.group_id_hex) else {
            continue;
        };
        match send_control(client, &gid, ev).await {
            Ok(()) => sent += 1,
            Err(e) => heprintln!(
                "[warning: catch-up snapshot for group {} failed: {e}]",
                g.group_id_hex.chars().take(8).collect::<String>()
            ),
        }
    }
    sent
}

/// Drive MDK's distributed-convergence pass for every group so this account
/// APPLIES peer handshake commits (member add/remove, admin promote/demote) that
/// `sync()` merely *buffers*. MDK ingests a peer's epoch-advancing commit into a
/// convergence buffer and applies it only on a later pass, ~1s after the commit
/// arrives (a quiescence window, `settlement_quiescence_ms = 1000`); that pass is
/// normally driven by MDK's account-worker background loop, which moyu — being
/// deliberately actor-less (see `engine.rs`) — does not run. Without this, a
/// peer's kick/promote/add is fetched but never applied, so `members()` /
/// `admin_policy` never change on the receiving side (confirmed empirically over
/// a live relay). See `AppClient::retry_group_convergence`
/// (`marmot-app/src/client/mod.rs:1661`) — the same public primitive that worker
/// calls; sweeping it once per `sync()` applies anything buffered on a *previous*
/// tick (our poll interval, 2s, is safely past the 1s window), and is a cheap
/// no-op when nothing is buffered. Best-effort: a per-group error is swallowed so
/// one wedged group can't stall the receive loop. Returns whether any group
/// reported published follow-on effects (lets a one-shot `recv` keep polling
/// until convergence settles instead of exiting on an "idle" buffered tick).
pub(crate) async fn drive_convergence(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
) -> bool {
    let Ok(groups) = engine.groups(label) else {
        return false;
    };
    let mut progressed = false;
    for g in &groups {
        if let Ok(gid) = group_id_from_hex(&g.group_id_hex)
            && let Ok(summary) = client.retry_group_convergence(&gid).await
        {
            progressed |= summary.published > 0;
        }
    }
    progressed
}

/// A workspace's display name + roster/admin snapshot, keyed by `group_id_hex`,
/// for a governance before/after diff. Governance changes (kick / promote /
/// demote / a peer's join) reach an observer only via convergence, whose
/// `SendSummary` carries no `GroupStateChange` (see `governance`'s module docs),
/// so a snapshot diff is the only way to surface them.
pub(crate) struct WsRoster {
    pub name: String,
    pub snap: governance::RosterSnapshot,
}

/// Snapshot every workspace this account still belongs to: its display name and
/// `(members, admins)` sets, keyed by `group_id_hex`. DMs are excluded
/// (governance is a workspace concept -- `all_workspaces` already drops them and
/// their profile-name sentinel).
///
/// Returns `None` if ANY backing read fails (the `membership_scan` projection
/// reads, or any per-group `members()` read). This is deliberate and
/// load-bearing: a degraded/partial snapshot must NEVER be diffed. If a
/// transient DB blip made this map merely *miss* a group or its admins, the
/// caller would diff it against a healthy baseline and FABRICATE governance
/// notices -- "you are no longer a member of X" for every workspace, or "no
/// longer an admin" for every admin. A `None` tick is instead skipped whole: the
/// caller keeps its good baseline and re-snapshots next poll (self-healing). So
/// a read error can only ever SUPPRESS a notice for one tick, never fabricate
/// one. (A corrupt on-disk `group_id_hex`, which MDK never produces, is the one
/// per-group skip -- that group is unusable regardless.)
pub(crate) fn snapshot_rosters(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> Option<HashMap<String, WsRoster>> {
    // One membership_scan pass carries everything this needs — the projection,
    // the admin set (same `groups()` read) and the live roster (same
    // `members()` read) — so this runs no store reads of its own. That matters:
    // it is called every poll tick (TUI `sync_tick` / `recv --follow`).
    let (rows, _) = membership_scan(engine, client, label).ok()?;
    let mut out = HashMap::new();
    for row in rows {
        let Some(member_hexes) = row.roster else {
            // The scan kept this group fail-open without a roster. Corrupt
            // group-id hex (never from MDK) -> skip just this group; a
            // transient roster read failure must instead fail the WHOLE
            // snapshot (-> None), not silently drop the group (which would
            // read as "I left it").
            if group_id_from_hex(&row.ghex).is_err() {
                continue;
            }
            return None;
        };
        out.insert(
            row.ghex,
            WsRoster {
                name: row.proj.name.unwrap_or_default(),
                snap: governance::RosterSnapshot::new(&member_hexes, &row.admins),
            },
        );
    }
    Some(out)
}

/// One human-facing governance line derived from a roster diff.
#[derive(Debug)]
pub(crate) enum GovLine {
    /// A membership/admin change in a workspace this account is STILL in.
    Change {
        ws_name: String,
        change: governance::GovChange,
    },
    /// This account itself is no longer a member of a workspace it was in
    /// (kicked, or it left) -- the whole group vanished from `snapshot_rosters`
    /// (which drops groups where the live MLS roster no longer lists me).
    SelfRemoved { ws_name: String },
}

/// Diff before/after roster snapshots into governance lines: per-workspace
/// membership/admin changes for groups still present (sorted by group id for a
/// deterministic order), plus a `SelfRemoved` for any workspace that vanished (I
/// was kicked / left). Brand-new workspaces (present only in `after`) are
/// intentionally NOT diffed -- a fresh join is announced by the invite-accept
/// path, not replayed as a flood of "member joined".
pub(crate) fn governance_diff(
    before: &HashMap<String, WsRoster>,
    after: &HashMap<String, WsRoster>,
) -> Vec<GovLine> {
    let mut ghexes: Vec<&String> = before.keys().collect();
    ghexes.sort();
    let mut lines = Vec::new();
    for ghex in ghexes {
        let prev = &before[ghex];
        match after.get(ghex) {
            Some(now) => {
                for change in governance::diff_roster(&prev.snap, &now.snap) {
                    lines.push(GovLine::Change {
                        ws_name: now.name.clone(),
                        change,
                    });
                }
            }
            None => lines.push(GovLine::SelfRemoved {
                ws_name: prev.name.clone(),
            }),
        }
    }
    lines
}

/// True iff any line reports a PEER joining a workspace -- the re-arm trigger for
/// the §5.2 catch-up fallback over the wire (an existing member observes the
/// join via convergence, not `SyncSummary.events`; see `saw_member_added`).
pub(crate) fn any_peer_joined(lines: &[GovLine]) -> bool {
    lines.iter().any(|l| {
        matches!(
            l,
            GovLine::Change {
                change: governance::GovChange::Joined { .. },
                ..
            }
        )
    })
}

/// A compact, human-readable member id for one-line governance notices:
/// `npub1abcde…uvwxyz`, or the hex prefix if the id won't bech32-encode.
pub(crate) fn short_member(member_id_hex: &str) -> String {
    match moyu_core::identity::npub_from_hex(member_id_hex) {
        Ok(npub) if npub.len() > 20 => {
            format!("{}…{}", &npub[..10], &npub[npub.len() - 6..])
        }
        Ok(npub) => npub,
        Err(_) => member_id_hex.chars().take(12).collect(),
    }
}

/// Render a [`GovLine`] as a `recv` notice line. Phrasing is honest about what a
/// roster diff can and cannot know: a departure may be a kick OR a voluntary
/// leave, so it says neither.
pub(crate) fn format_gov_line(line: &GovLine) -> String {
    match line {
        GovLine::Change { ws_name, change } => {
            use governance::GovChange::*;
            let (who, verb) = match change {
                Joined { member_id_hex } => (member_id_hex, "joined workspace"),
                Departed { member_id_hex } => (member_id_hex, "is no longer in workspace"),
                Promoted { member_id_hex } => (member_id_hex, "is now an admin of workspace"),
                Demoted { member_id_hex } => (member_id_hex, "is no longer an admin of workspace"),
            };
            format!("[governance] {} {} {}", short_member(who), verb, ws_name)
        }
        GovLine::SelfRemoved { ws_name } => {
            format!("[governance] you are no longer a member of workspace {ws_name}")
        }
    }
}

/// The admin pubkey-hex set of the workspace group `group_id_hex`, read from
/// the account's local group projection (`AppGroupRecord.admin_policy.admins`).
/// Empty if the group carries no admin-policy component. These entries are
/// `hex::encode` of each admin's 32-byte account pubkey — the same encoding as a
/// member's `member_id_hex`, which is what makes `governance::is_admin` a plain
/// string containment check (see that module for the verified join key).
pub(crate) fn workspace_admins(
    engine: &MoyuEngine,
    label: &str,
    group_id_hex: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(engine
        .groups(label)?
        .into_iter()
        .find(|g| g.group_id_hex == group_id_hex)
        .map(|g| g.admin_policy.admins)
        .unwrap_or_default())
}

/// The local account's `member_id_hex` within a group's `members()` list. moyu
/// is single-account-per-process (the M0/M1 single-device decision), so exactly
/// one member is `local`; that member is "me", the join key for
/// `governance::is_admin`/`leave_plan`. (Identifying self this way — rather than
/// by the account label — is correct even for an imported account whose label
/// is not its pubkey hex.)
pub(crate) fn local_member_hex(members: &[AppGroupMemberRecord]) -> anyhow::Result<String> {
    members
        .iter()
        .find(|m| m.local)
        .map(|m| m.member_id_hex.clone())
        .ok_or_else(|| anyhow::anyhow!("you are not a member of this group"))
}

/// Resolve a `<ws>` ref and load everything an admin-gated command needs:
/// the `GroupId`, my `member_id_hex`, the group's admin set, and the projection
/// (for its name). One place so `kick` / `admin` / `leave` share the setup.
pub(crate) fn workspace_gov(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<(GroupId, String, Vec<String>, WorkspaceProjection)> {
    let (id, proj) = resolve_workspace(engine, client, label, ws)?;
    gov_tuple(engine, client, label, id, proj)
}

/// [`workspace_gov`] keyed on an EXACT group-id hex rather than a fuzzy `<ws>`
/// ref -- used by `approve`, where the id comes from an untrusted join-request
/// envelope and must not be prefix/name-matched.
pub(crate) fn workspace_gov_by_gid(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    gid_hex: &str,
) -> anyhow::Result<(GroupId, String, Vec<String>, WorkspaceProjection)> {
    let (id, proj) = resolve_workspace_by_gid(engine, client, label, gid_hex)?;
    gov_tuple(engine, client, label, id, proj)
}

/// The shared assembly tail of [`workspace_gov`] / [`workspace_gov_by_gid`]
/// for an already-resolved workspace. Only the RESOLUTION differs between the
/// two (fuzzy by-ref vs exact by-gid — a deliberate security distinction);
/// everything after it must stay identical, so it lives
/// here once.
pub(crate) fn gov_tuple(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    id: String,
    proj: WorkspaceProjection,
) -> anyhow::Result<(GroupId, String, Vec<String>, WorkspaceProjection)> {
    let gid = group_id_from_hex(&id)?;
    let me = local_member_hex(&client.members(&gid)?)?;
    let admins = workspace_admins(engine, label, &id)?;
    Ok((gid, me, admins, proj))
}

pub(crate) enum AdminChange {
    Promote,
    Demote,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §5.2 gate MUST key off `SyncSummary.events` (where MDK synthesizes
    /// `member_added` as a `GroupStateChanged`), NOT `SyncSummary.messages`
    /// (wire-received app messages, which never carry the locally-synthesized
    /// member_added). This pins an exact regression: the
    /// original gate scanned `.messages`, so it never fired for the existing
    /// member the fallback is built around.
    #[test]
    fn saw_member_added_triggers_only_on_a_member_added_engine_event() {
        use moyu_core::engine::{
            EpochId, GroupEvent, GroupId, GroupStateChange, MemberId, ReceivedMessage, SyncSummary,
        };

        let member_added = || GroupEvent::GroupStateChanged {
            group_id: GroupId::new(vec![1]),
            epoch: EpochId(1),
            actor: None,
            change: GroupStateChange::MemberAdded {
                member: MemberId::new(vec![2]),
            },
            origin_commit_id: None,
        };

        // An observed member-add fires the gate.
        let added = SyncSummary {
            events: vec![member_added()],
            ..Default::default()
        };
        assert!(saw_member_added(&added), "MemberAdded event must fire");

        // A non-add state change (someone leaving) must NOT fire it.
        let left = SyncSummary {
            events: vec![GroupEvent::GroupStateChanged {
                group_id: GroupId::new(vec![1]),
                epoch: EpochId(2),
                actor: None,
                change: GroupStateChange::MemberLeft {
                    member: MemberId::new(vec![2]),
                },
                origin_commit_id: None,
            }],
            ..Default::default()
        };
        assert!(
            !saw_member_added(&left),
            "MemberLeft is not a join -- no catch-up trigger"
        );

        // REGRESSION GUARD: a member_added-shaped row sitting only in `.messages`
        // (the collection the buggy gate scanned) must NOT fire -- the real
        // signal lives in `.events`, and `.messages` never carries it.
        let only_in_messages = SyncSummary {
            messages: vec![ReceivedMessage {
                message_id_hex: "m".into(),
                source_message_id_hex: "s".into(),
                sender: "npub".into(),
                sender_display_name: None,
                group_id: GroupId::new(vec![1]),
                source_epoch: 1,
                plaintext:
                    r#"{"v":1,"system_type":"member_added","text":"Member added","data":{}}"#.into(),
                kind: moyu_core::workspace::KIND_GROUP_SYSTEM,
                tags: vec![vec!["system".into(), "member_added".into()]],
                recorded_at: 100,
                received_at: 100,
                retention: None,
            }],
            ..Default::default()
        };
        assert!(
            !saw_member_added(&only_in_messages),
            "the gate must read .events, not .messages -- else it never fires (the reviewed bug)"
        );

        // Nothing at all -> no trigger.
        assert!(!saw_member_added(&SyncSummary::default()));
    }

    #[test]
    fn governance_diff_reports_changes_and_self_removal() {
        use moyu_core::governance::{GovChange, RosterSnapshot};
        use std::collections::HashMap;

        let id = |b: u8| hex::encode([b; 32]);
        let ws = |name: &str, members: &[String], admins: &[String]| WsRoster {
            name: name.into(),
            snap: RosterSnapshot::new(members, admins),
        };
        let before: HashMap<String, WsRoster> = HashMap::from([
            (
                "aaaa".into(),
                ws("demo", &[id(0x01), id(0x02)], &[id(0x01)]),
            ),
            ("bbbb".into(), ws("second", &[id(0x01)], &[id(0x01)])),
        ]);
        // demo: bb promoted to admin; `second` vanished (I left / was kicked).
        let after: HashMap<String, WsRoster> = HashMap::from([(
            "aaaa".into(),
            ws("demo", &[id(0x01), id(0x02)], &[id(0x01), id(0x02)]),
        )]);

        let lines = governance_diff(&before, &after);
        // Deterministic order: sorted by group_id key ("aaaa" demo before "bbbb").
        assert_eq!(lines.len(), 2);
        match &lines[0] {
            GovLine::Change {
                ws_name,
                change: GovChange::Promoted { member_id_hex },
            } => {
                assert_eq!(ws_name.as_str(), "demo");
                assert_eq!(member_id_hex, &id(0x02));
            }
            other => panic!("expected demo promotion, got {other:?}"),
        }
        assert!(
            matches!(&lines[1], GovLine::SelfRemoved { ws_name } if ws_name.as_str() == "second")
        );
        assert!(!any_peer_joined(&lines), "a promotion is not a join");
    }

    /// Pins the hazard the `snapshot_rosters -> Option` contract guards against:
    /// diffing a healthy baseline against a DEGRADED (empty) snapshot self-removes
    /// EVERY workspace. `snapshot_rosters` returns `None` (never an empty map) on a
    /// read error and the callers skip the diff, so this fabrication never reaches
    /// the user -- but the pure diff itself must still behave this way.
    #[test]
    fn governance_diff_empty_after_self_removes_all() {
        use moyu_core::governance::RosterSnapshot;
        use std::collections::HashMap;

        let id = |b: u8| hex::encode([b; 32]);
        let ws = |name: &str| WsRoster {
            name: name.into(),
            snap: RosterSnapshot::new(&[id(0x01)], &[id(0x01)]),
        };
        let before: HashMap<String, WsRoster> =
            HashMap::from([("aaaa".into(), ws("demo")), ("bbbb".into(), ws("second"))]);
        let after: HashMap<String, WsRoster> = HashMap::new();

        let lines = governance_diff(&before, &after);
        assert_eq!(
            lines.len(),
            2,
            "an empty snapshot self-removes every workspace"
        );
        assert!(
            lines
                .iter()
                .all(|l| matches!(l, GovLine::SelfRemoved { .. }))
        );
    }
}
