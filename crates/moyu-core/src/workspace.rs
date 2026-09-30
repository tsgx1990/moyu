//! moyu's multi-channel workspace control-plane (increment ②).
//!
//! A workspace is one MLS group; channels are logical partitions. Shared state
//! (channel list + workspace name) is a *projection* of an ordered log of
//! control messages, which travel as MDK group-system events (kind 1210) via
//! `AppClient::send_group_system_event`
//! Everything here is pure (no I/O) so each rule is unit-tested.
//!
//! ## Convergence (the load-bearing property)
//!
//! Two members who have received the same set of control messages MUST converge
//! to identical state. Ordering therefore uses only fields that are **identical
//! on every client** for a given event: the primary key is `recorded_at` (the
//! source-event timestamp, 1-second granularity, same value everywhere) and the
//! tie-break is `message_id_hex` (a content-derived event id, same everywhere).
//! We deliberately do **not** use `AppMessageRecord.insert_order` — MDK documents
//! it as a *local* SQLite rowid ("not part of the cross-client display order"),
//! so using it as a tie-break would let two devices diverge on equal-`ts` events.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::engine::AppMessageRecord;

/// MDK's `MARMOT_APP_EVENT_KIND_GROUP_SYSTEM` — every moyu control message
/// rides on this kind (never rendered as chat).
pub const KIND_GROUP_SYSTEM: u64 = crate::kinds::MARMOT_APP_EVENT_KIND_GROUP_SYSTEM;
/// Namespace prefix for moyu's own `system_type`s, so they never collide with
/// MDK's built-in `GROUP_SYSTEM_TYPE_*` (member_added, …).
pub const CONTROL_PREFIX: &str = "moyu.";

/// The always-present default channel every workspace has.
pub const DEFAULT_CHANNEL_SLUG: &str = "general";

/// The most channels one workspace can have (the default channel included).
/// See [`WorkspaceProjection::apply`]'s room check.
pub const MAX_CHANNELS: usize = 256;

/// Turn a user-typed channel name into a stable slug: lowercase, only
/// `[a-z0-9-]`, other runs collapse to a single `-`, trimmed; empty result
/// falls back to `general`.
pub fn normalize_slug(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        DEFAULT_CHANNEL_SLUG.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Wrap a chat message body so the receiver can route it to a channel:
/// `{"moyu":1,"ch":"<slug>","body":"<text>"}` (`send` gives no
/// custom-tag hook). The `"moyu":1` marker is what distinguishes our envelope
/// from a user who simply typed some JSON into chat (moyu's audience is
/// developers, who paste JSON constantly).
pub fn encode_channel_body(slug: &str, body: &str) -> String {
    serde_json::json!({ "moyu": 1, "ch": slug, "body": body }).to_string()
}

/// Inverse of [`encode_channel_body`]. Only a genuine moyu envelope (a JSON
/// object carrying `"moyu":1`) is unwrapped; anything else — legacy M0
/// plaintext, or arbitrary JSON a user typed — routes to `general` with the
/// original text preserved verbatim as the body (never lost, never misrouted).
pub fn decode_channel_body(plaintext: &str) -> (String, String) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(plaintext)
        && v.get("moyu").and_then(|m| m.as_u64()) == Some(1)
    {
        let ch = v.get("ch").and_then(|c| c.as_str()).unwrap_or("");
        let body = v
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_owned();
        return (normalize_slug(ch), body);
    }
    (DEFAULT_CHANNEL_SLUG.to_owned(), plaintext.to_owned())
}

/// One channel entry inside a `workspace.snapshot` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChannel {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub ts: u64,
}

/// A workspace/channel metadata change, carried as a `moyu.*` group-system event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    ChannelCreate {
        slug: String,
        name: String,
    },
    ChannelRename {
        slug: String,
        name: String,
        ts: u64,
    },
    ChannelArchive {
        slug: String,
        ts: u64,
    },
    WorkspaceRename {
        name: String,
        ts: u64,
    },
    WorkspaceSnapshot {
        name: String,
        name_ts: u64,
        channels: Vec<SnapshotChannel>,
    },
}

impl ControlEvent {
    /// The `(system_type, data)` pair to hand to
    /// `AppClient::send_group_system_event`.
    pub fn to_wire(&self) -> (String, serde_json::Value) {
        use serde_json::json;
        match self {
            ControlEvent::ChannelCreate { slug, name } => (
                "moyu.channel.create".to_owned(),
                json!({ "slug": slug, "name": name }),
            ),
            ControlEvent::ChannelRename { slug, name, ts } => (
                "moyu.channel.rename".to_owned(),
                json!({ "slug": slug, "name": name, "ts": ts }),
            ),
            ControlEvent::ChannelArchive { slug, ts } => (
                "moyu.channel.archive".to_owned(),
                json!({ "slug": slug, "ts": ts }),
            ),
            ControlEvent::WorkspaceRename { name, ts } => (
                "moyu.workspace.rename".to_owned(),
                json!({ "name": name, "ts": ts }),
            ),
            ControlEvent::WorkspaceSnapshot {
                name,
                name_ts,
                channels,
            } => (
                "moyu.workspace.snapshot".to_owned(),
                json!({ "name": name, "name_ts": name_ts, "channels": channels }),
            ),
        }
    }
}

// Internal wire payload shapes for decoding `data`.
#[derive(Deserialize)]
struct WireChannelCreate {
    slug: String,
    name: String,
}
#[derive(Deserialize)]
struct WireChannelRename {
    slug: String,
    name: String,
    #[serde(default)]
    ts: u64,
}
#[derive(Deserialize)]
struct WireChannelArchive {
    slug: String,
    #[serde(default)]
    ts: u64,
}
#[derive(Deserialize)]
struct WireWorkspaceRename {
    name: String,
    #[serde(default)]
    ts: u64,
}
#[derive(Deserialize)]
struct WireWorkspaceSnapshot {
    name: String,
    #[serde(default)]
    name_ts: u64,
    #[serde(default)]
    channels: Vec<SnapshotChannel>,
}

/// Decode a stored/received `AppMessageRecord` into a moyu control event, or
/// `None` if it is not one (wrong kind, not `moyu.*`, or malformed — malformed
/// payloads are skipped, never fatal, so one bad message can't break a workspace).
pub fn control_event_from_record(rec: &AppMessageRecord) -> Option<ControlEvent> {
    if rec.kind != KIND_GROUP_SYSTEM {
        return None;
    }
    let content: serde_json::Value = serde_json::from_str(&rec.plaintext).ok()?;
    let system_type = content.get("system_type")?.as_str()?;
    if !system_type.starts_with(CONTROL_PREFIX) {
        return None;
    }
    let data = content
        .get("data")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ev = match system_type {
        "moyu.channel.create" => {
            let d: WireChannelCreate = serde_json::from_value(data).ok()?;
            ControlEvent::ChannelCreate {
                slug: d.slug,
                name: d.name,
            }
        }
        "moyu.channel.rename" => {
            let d: WireChannelRename = serde_json::from_value(data).ok()?;
            ControlEvent::ChannelRename {
                slug: d.slug,
                name: d.name,
                ts: d.ts,
            }
        }
        "moyu.channel.archive" => {
            let d: WireChannelArchive = serde_json::from_value(data).ok()?;
            ControlEvent::ChannelArchive {
                slug: d.slug,
                ts: d.ts,
            }
        }
        "moyu.workspace.rename" => {
            let d: WireWorkspaceRename = serde_json::from_value(data).ok()?;
            ControlEvent::WorkspaceRename {
                name: d.name,
                ts: d.ts,
            }
        }
        "moyu.workspace.snapshot" => {
            let d: WireWorkspaceSnapshot = serde_json::from_value(data).ok()?;
            ControlEvent::WorkspaceSnapshot {
                name: d.name,
                name_ts: d.name_ts,
                channels: d.channels,
            }
        }
        _ => return None,
    };
    Some(ev)
}

/// True iff a `(kind, content-plaintext)` pair is MDK's own `member_added`
/// group-system event (kind 1210, `system_type == "member_added"`). MDK
/// synthesizes this row (including in the adder's own timeline) on every MLS
/// Add. It carries no `moyu.` prefix, so
/// [`control_event_from_record`] ignores it; this is the separate detector the
/// catch-up fallback uses to notice "someone joined". Type-agnostic so it
/// works on both a stored [`AppMessageRecord`] and a live `sync()`
/// `ReceivedMessage` (both expose `kind` + `plaintext`).
pub fn is_member_added(kind: u64, plaintext: &str) -> bool {
    kind == KIND_GROUP_SYSTEM
        && serde_json::from_str::<serde_json::Value>(plaintext)
            .ok()
            .and_then(|c| {
                c.get("system_type")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "member_added")
            })
            .unwrap_or(false)
}

/// The cross-client `recorded_at` of a stored record iff it is MDK's own
/// `member_added` row (see [`is_member_added`]); else `None`.
pub fn member_added_at(rec: &AppMessageRecord) -> Option<u64> {
    is_member_added(rec.kind, &rec.plaintext).then_some(rec.recorded_at)
}

/// A change to who is in a group, as MDK records it in that group's own
/// history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    Added,
    Removed,
    Left,
}

/// `(what happened, to whom)` iff `rec` is one of MDK's own membership rows
/// (kind 1210 with `system_type` `member_added` / `member_removed` /
/// `member_left`); the subject is the affected member's id, lower-case hex.
/// MDK writes these rows itself from the MLS commits it processes, in every
/// member's history, whoever made the change.
///
/// Only rows MDK wrote locally count: it stores them with
/// `direction == "system"`. Any member can SEND a kind-1210 event carrying
/// one of these type strings, but a message that arrived over the network is
/// stored as `"received"` (or `"sent"` for our own), so a forged row is
/// ignored here rather than being able to hide someone's join request or
/// block them from being auto-approved.
pub fn membership_change(rec: &AppMessageRecord) -> Option<(MembershipChange, String)> {
    if rec.kind != KIND_GROUP_SYSTEM || rec.direction != "system" {
        return None;
    }
    let content: serde_json::Value = serde_json::from_str(&rec.plaintext).ok()?;
    let change = match content.get("system_type")?.as_str()? {
        "member_added" => MembershipChange::Added,
        "member_removed" => MembershipChange::Removed,
        "member_left" => MembershipChange::Left,
        _ => return None,
    };
    let subject = content.get("data")?.get("subject")?.as_str()?;
    (!subject.is_empty()).then(|| (change, subject.to_ascii_lowercase()))
}

/// Materialized state of one channel (a projection, not persisted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelState {
    pub slug: String,
    pub name: String,
    /// Monotonic: once archived (by an archive event or a snapshot), stays
    /// archived — M1 has no un-archive, so this can never be lost by a
    /// later rename or regressed by a snapshot.
    pub archived: bool,
    /// `recorded_at` of the create / first sight — display ordering key.
    pub created_at: u64,
    /// LWW cursor for the `name` field only: the `(ts, message_id_hex)` of the
    /// last rename/create/snapshot that set the name. Both components are
    /// cross-client-identical, so all members resolve rename races the same way.
    pub name_ts: u64,
    pub name_msgid: String,
}

/// Materialized workspace state, recomputed from the control-message log.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceProjection {
    pub name: Option<String>,
    /// LWW cursor for the workspace `name`: `(name_ts, name_msgid)`.
    pub name_ts: u64,
    pub name_msgid: String,
    pub channels: BTreeMap<String, ChannelState>,
}

impl WorkspaceProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when this projection carries no control-plane state a fresh member
    /// couldn't reconstruct locally: no workspace rename and only the seeded
    /// `#general`, untouched. Such a workspace (and every 1:1 DM) needs no §5.2
    /// catch-up broadcast — a joiner re-seeds `#general` and reads the workspace
    /// name from the MDK group profile (which travels in the Welcome). This is
    /// also what keeps a brand-new joinee (whose projection is exactly this
    /// default when the adder's snapshot never arrived) from re-broadcasting a
    /// wrong, thin snapshot.
    pub fn is_default(&self) -> bool {
        self.name.is_none()
            && self.channels.len() == 1
            && self
                .channels
                .get(DEFAULT_CHANNEL_SLUG)
                .is_some_and(|c| !c.archived && c.name == DEFAULT_CHANNEL_SLUG && c.name_ts == 0)
    }

    pub fn channel(&self, slug: &str) -> Option<&ChannelState> {
        self.channels.get(slug)
    }

    /// Channels in stable display order: `general` first, then by
    /// `(created_at, slug)`. `slug` is a unique, cross-client-identical
    /// tiebreak, so the order is deterministic on every member.
    pub fn ordered_channels(&self) -> Vec<&ChannelState> {
        let mut v: Vec<&ChannelState> = self.channels.values().collect();
        v.sort_by(|a, b| {
            let a_default = a.slug == DEFAULT_CHANNEL_SLUG;
            let b_default = b.slug == DEFAULT_CHANNEL_SLUG;
            b_default
                .cmp(&a_default)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.slug.cmp(&b.slug))
        });
        v
    }

    /// The catch-up `WorkspaceSnapshot` control event for this projection's
    /// current state (full ordered channel set + name/`name_ts`) — the one
    /// shape broadcast by `workspace add`, `approve`, and the §5.2 fallback
    /// scanner. `fallback_name` fills `name` when the workspace was never
    /// explicitly renamed (the CLI broadcast paths pass `""`;
    /// [`catch_up_snapshot`] passes the MDK group-profile name).
    pub fn snapshot_event(&self, fallback_name: &str) -> ControlEvent {
        let channels: Vec<SnapshotChannel> = self
            .ordered_channels()
            .iter()
            .map(|c| SnapshotChannel {
                slug: c.slug.clone(),
                name: c.name.clone(),
                archived: c.archived,
                ts: c.name_ts,
            })
            .collect();
        ControlEvent::WorkspaceSnapshot {
            name: self
                .name
                .clone()
                .unwrap_or_else(|| fallback_name.to_owned()),
            name_ts: self.name_ts,
            channels,
        }
    }

    /// Fold one control event into the projection. `recorded_at` and `msg_id`
    /// are the carrying record's cross-client-identical ordering coordinates
    /// (`AppMessageRecord::recorded_at` / `message_id_hex`).
    /// Whether `slug` may be (or already is) a channel of this workspace.
    /// Control events are ordinary group messages: any member can send one,
    /// so without a ceiling a single member could make every other member's
    /// client hold and render an unbounded channel list. Events are folded in
    /// one deterministic order, so every member drops the same ones.
    fn has_room_for(&self, slug: &str) -> bool {
        self.channels.contains_key(slug) || self.channels.len() < MAX_CHANNELS
    }

    pub fn apply(&mut self, ev: &ControlEvent, recorded_at: u64, msg_id: &str) {
        match ev {
            ControlEvent::ChannelCreate { slug, name } => {
                if !self.has_room_for(slug) {
                    return;
                }
                self.channels
                    .entry(slug.clone())
                    .or_insert_with(|| ChannelState {
                        slug: slug.clone(),
                        name: name.clone(),
                        archived: false,
                        created_at: recorded_at,
                        name_ts: 0,
                        name_msgid: msg_id.to_owned(),
                    });
            }
            ControlEvent::ChannelRename { slug, name, ts } => {
                if let Some(c) = self.channels.get_mut(slug)
                    && (*ts, msg_id) > (c.name_ts, c.name_msgid.as_str())
                {
                    c.name = name.clone();
                    c.name_ts = *ts;
                    c.name_msgid = msg_id.to_owned();
                }
            }
            ControlEvent::ChannelArchive { slug, .. } => {
                // `archived` is monotonic (M1 has no un-archive), so no ts/LWW is
                // needed. Accepted boundary: if clock skew sorts
                // an archive *before* its channel's create, the channel isn't
                // present yet and the archive is dropped — convergent (all members
                // agree) but lost. `#general` is immune (seeded before folding).
                if let Some(c) = self.channels.get_mut(slug) {
                    c.archived = true;
                }
            }
            ControlEvent::WorkspaceRename { name, ts } => {
                if (*ts, msg_id) > (self.name_ts, self.name_msgid.as_str()) {
                    self.name = Some(name.clone());
                    self.name_ts = *ts;
                    self.name_msgid = msg_id.to_owned();
                }
            }
            ControlEvent::WorkspaceSnapshot {
                name,
                name_ts,
                channels,
            } => {
                // Accepted boundary: a re-broadcast snapshot
                // re-stamps the name with the *snapshot's* msg_id, so among
                // concurrent equal-`ts` name-sets the content-id tie-break can
                // re-pick a different (still deterministic, all-members-agree)
                // winner — a name may flip when a member joins with nobody
                // renaming. Strict `>` still never regresses a strictly-newer ts.
                if (*name_ts, msg_id) > (self.name_ts, self.name_msgid.as_str()) {
                    self.name = Some(name.clone());
                    self.name_ts = *name_ts;
                    self.name_msgid = msg_id.to_owned();
                }
                for sc in channels {
                    if !self.has_room_for(&sc.slug) {
                        continue;
                    }
                    match self.channels.get_mut(&sc.slug) {
                        None => {
                            self.channels.insert(
                                sc.slug.clone(),
                                ChannelState {
                                    slug: sc.slug.clone(),
                                    name: sc.name.clone(),
                                    archived: sc.archived,
                                    created_at: recorded_at,
                                    name_ts: sc.ts,
                                    name_msgid: msg_id.to_owned(),
                                },
                            );
                        }
                        Some(c) => {
                            if (sc.ts, msg_id) > (c.name_ts, c.name_msgid.as_str()) {
                                c.name = sc.name.clone();
                                c.name_ts = sc.ts;
                                c.name_msgid = msg_id.to_owned();
                            }
                            // `archived` is monotonic: a snapshot can only ever
                            // confirm an archive, never undo one.
                            c.archived |= sc.archived;
                        }
                    }
                }
            }
        }
    }
}

/// The always-present `#general` channel, seeded before any event is folded so
/// that a rename/archive targeting it (which may arrive before any explicit
/// `channel.create general` / snapshot) is never dropped.
fn seed_general() -> ChannelState {
    ChannelState {
        slug: DEFAULT_CHANNEL_SLUG.to_owned(),
        name: DEFAULT_CHANNEL_SLUG.to_owned(),
        archived: false,
        created_at: 0,
        name_ts: 0,
        name_msgid: String::new(),
    }
}

/// Recompute a workspace's materialized state from a group's message records
/// (as returned by `crate::engine::MoyuEngine::messages`). Non-control records
/// are ignored; control records are applied in `(recorded_at, message_id_hex)`
/// order — both cross-client-identical — so every member converges; `general`
/// is always present (seeded before folding so events can target it).
pub fn project_workspace(records: &[AppMessageRecord]) -> WorkspaceProjection {
    let mut events: Vec<(&AppMessageRecord, ControlEvent)> = records
        .iter()
        .filter_map(|r| control_event_from_record(r).map(|ev| (r, ev)))
        .collect();
    events.sort_by(|(a, _), (b, _)| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then_with(|| a.message_id_hex.cmp(&b.message_id_hex))
    });

    let mut proj = WorkspaceProjection::new();
    proj.channels
        .insert(DEFAULT_CHANNEL_SLUG.to_owned(), seed_general());
    for (rec, ev) in events {
        proj.apply(&ev, rec.recorded_at, &rec.message_id_hex);
    }
    proj
}

/// Join catch-up **fallback**. Given ONE group's message
/// records and the group's display name (the MDK profile name — the same
/// fallback `workspace add` uses when the workspace was never renamed), decide
/// whether *this* member should re-broadcast a catch-up snapshot:
///
/// - Returns `None` if the projection is [`WorkspaceProjection::is_default`]
///   (nothing a joiner can't rebuild locally — includes DMs and never-touched
///   workspaces), OR if every observed `member_added` already has a
///   `WorkspaceSnapshot` at/after it (the adder's primary snapshot, or an
///   earlier fallback, already covered the join).
/// - Otherwise returns the `WorkspaceSnapshot` to broadcast, built exactly like
///   `workspace add`'s primary snapshot (full ordered channel set + current
///   name / `name_ts`).
///
/// Idempotent and no-regress: the returned snapshot's own `recorded_at` will be
/// `>=` the join, so once broadcast this returns `None` on every re-scan
/// (self-terminating, restart-safe). In the rare genuine-fallback case several
/// online members may each fire; every such snapshot is harmless per §4.3.
pub fn catch_up_snapshot(
    group_records: &[AppMessageRecord],
    display_name: &str,
) -> Option<ControlEvent> {
    let proj = project_workspace(group_records);
    if proj.is_default() {
        return None;
    }
    let snapshot_times: Vec<u64> = group_records
        .iter()
        .filter(|r| {
            matches!(
                control_event_from_record(r),
                Some(ControlEvent::WorkspaceSnapshot { .. })
            )
        })
        .map(|r| r.recorded_at)
        .collect();
    let has_uncovered_join = group_records
        .iter()
        .filter_map(member_added_at)
        .any(|join_ts| !snapshot_times.iter().any(|&s| s >= join_ts));
    if !has_uncovered_join {
        return None;
    }
    Some(proj.snapshot_event(display_name))
}

// ===== M3: private channels (separate MLS groups, nested under a workspace) =====
//
// A private channel is its own MLS group whose membership is a subset of the
// parent workspace's members. MLS gives every group member the epoch key, so a
// "channel only some members can read" is necessarily a *distinct* group — MDK
// has no sub-group / selective-visibility primitive. The link
// back to the parent workspace (plus the channel's slug + display name) rides in
// the group's MLS-protected `profile.name`, set once at `create_group` and thus
// present the instant a member joins via Welcome — the same race-free classifier
// slot moyu already uses for the DM sentinel and workspace names. Nothing is
// announced in the parent workspace group, so a private channel is invisible to
// workspace members who are not in it (fully hidden).

/// The MDK group-profile name every 1:1 DM is founded with. A group whose profile
/// name is exactly this — and which carries no moyu workspace name — is a DM.
///
/// The profile name is an MLS-protected group component that travels in the
/// Welcome, so a peer knows it the instant they join — *before* any moyu
/// `workspace.rename`/`workspace.snapshot` control message has synced. A group
/// whose profile name is anything other than this sentinel is therefore a
/// workspace, knowable at join time. That closes the DM/workspace
/// misclassification race (a workspace's Welcome and its name control message
/// can arrive in different `sync()` ticks; without this, the join tick would
/// persist a bogus 1:1 contact bound to the workspace group, permanently).
/// [`classify_group`] is the canonical rule built on it.
pub const DM_GROUP_NAME: &str = "dm";

/// Sentinel beginning the encoded `profile.name` of a private-channel group. The
/// leading `\u{1}` (SOH) is a control char no sane user types as a workspace or
/// channel name; the create path (`workspace new` / `channel new-private` /
/// rename, in the CLI-wiring increment) rejects any name beginning with it, so a
/// genuine workspace name can never decode as a private channel. Decode here is
/// defensive regardless. A crafted sentinel name is an **app-level spoofing
/// surface** (the D1 linkage is app-level, not cryptographic): an attacker who
/// knows a workspace W's group id and a victim's KeyPackage can create such a
/// group, set `parent` = W, and invite the victim — whose `channel list W` then
/// shows a fabricated 🔒 row nested under W (the mislabel lands in the *invitee's*
/// view, not the crafter's). It leaks **no key/content** (MLS still isolates every
/// group) and can only nest under a workspace the victim already belongs to (accepted
/// limitation). Followed by US-separated fields:
/// parent-group-id-hex, slug, display-name.
const PRIVATE_CHANNEL_TAG: &str = "\u{1}moyu.pch";
/// Field separator inside an encoded private-channel `profile.name` (US, 0x1F) —
/// a control char, so it never appears in a slug or a sane display name.
const PRIVATE_CHANNEL_SEP: char = '\u{1f}';

/// The parent link + identity a private-channel group carries in its `profile.name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateChannelLink {
    /// `group_id` hex of the parent workspace this private channel belongs under.
    pub parent_group_id_hex: String,
    /// Normalized channel slug (unique within the parent workspace, by convention).
    pub slug: String,
    /// Human display name (may be empty; callers fall back to `slug`).
    pub name: String,
}

/// Display-name fallback shared by [`PrivateChannelLink`] and
/// [`PrivateChannelRef`]: the explicit name, or the slug when none was set.
fn name_or_slug<'a>(name: &'a str, slug: &'a str) -> &'a str {
    if name.is_empty() { slug } else { name }
}

impl PrivateChannelLink {
    /// Display name, falling back to the slug when no explicit name was set.
    pub fn display_name(&self) -> &str {
        name_or_slug(&self.name, &self.slug)
    }
}

/// Encode a private channel's parent link + identity into an MLS group
/// `profile.name` (set once at `create_group`). The slug is normalized; the
/// display name is sanitized so it can never contain the sentinel/separator (they
/// collapse to a space), keeping the encoding unambiguous. Result stays well under
/// MDK's 256-byte profile-name limit for a 64-hex parent + short slug/name.
pub fn encode_private_channel_name(parent_group_id_hex: &str, slug: &str, name: &str) -> String {
    let slug = normalize_slug(slug);
    let safe_name = name
        .replace([PRIVATE_CHANNEL_SEP, '\u{1}'], " ")
        .trim()
        .to_owned();
    format!(
        "{PRIVATE_CHANNEL_TAG}{PRIVATE_CHANNEL_SEP}{}{PRIVATE_CHANNEL_SEP}{slug}{PRIVATE_CHANNEL_SEP}{safe_name}",
        parent_group_id_hex.to_ascii_lowercase()
    )
}

/// The MDK profile-name length limit (`cgka-engine` rejects a longer name at
/// `create_group` rather than truncating). moyu checks it up front so an
/// over-long private-channel name yields a clean CLI error, not an opaque MDK one.
pub const MAX_GROUP_NAME_BYTES: usize = 256;

/// True if `name` is reserved by moyu's group-name encoding — it begins with the
/// private-channel sentinel [`PRIVATE_CHANNEL_TAG`]. `workspace new` must reject
/// such a name: otherwise the resulting group's `profile.name` would decode as a
/// private channel and vanish from the creator's own workspace list. This is the
/// enforcement the `PRIVATE_CHANNEL_TAG` doc relies on (a user can't type the SOH
/// sentinel by accident, so this only ever fires on deliberate input).
pub fn is_reserved_group_name(name: &str) -> bool {
    name.starts_with(PRIVATE_CHANNEL_TAG)
}

/// Inverse of [`encode_private_channel_name`]. Returns `Some` only for a genuine
/// moyu private-channel descriptor (correct sentinel, non-empty *hex* parent +
/// non-empty slug); anything else — the DM sentinel, a workspace name, arbitrary
/// text — returns `None` and is therefore classified as a workspace/DM, never a
/// private channel.
pub fn decode_private_channel_name(profile_name: &str) -> Option<PrivateChannelLink> {
    let rest = profile_name
        .strip_prefix(PRIVATE_CHANNEL_TAG)?
        .strip_prefix(PRIVATE_CHANNEL_SEP)?;
    let mut fields = rest.splitn(3, PRIVATE_CHANNEL_SEP);
    let parent = fields.next()?;
    let slug = fields.next()?;
    let name = fields.next().unwrap_or("");
    if parent.is_empty() || slug.is_empty() {
        return None;
    }
    // Parent must be plausible group-id hex; guards against a garbled name that
    // carries the sentinel yet decodes into a bogus parent.
    if !parent.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(PrivateChannelLink {
        parent_group_id_hex: parent.to_ascii_lowercase(),
        slug: normalize_slug(slug),
        name: name.to_owned(),
    })
}

/// The three kinds of MLS group moyu tracks, distinguished by the MLS-protected
/// `profile.name` (+ any moyu workspace name projected from control messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKind {
    /// A 1:1 direct-message group (profile name is the DM sentinel, no moyu name).
    Dm,
    /// A workspace: a public multi-channel group (profile name is the workspace name).
    Workspace,
    /// A private channel: a separate group nested under a parent workspace.
    PrivateChannel(PrivateChannelLink),
}

/// Classify a group from its MDK `profile.name` and any projected moyu workspace
/// name. Checks the most specific shape first (private channel), then applies the
/// existing workspace-vs-DM rule (workspace iff it carries a moyu name OR its
/// profile name is not the DM sentinel). Race-free: `profile.name` is delivered
/// with the Welcome, so a just-joined group is classified correctly on arrival.
pub fn classify_group(profile_name: &str, projected_name: Option<&str>) -> GroupKind {
    if let Some(link) = decode_private_channel_name(profile_name) {
        return GroupKind::PrivateChannel(link);
    }
    if projected_name.is_some() || profile_name != DM_GROUP_NAME {
        GroupKind::Workspace
    } else {
        GroupKind::Dm
    }
}

/// The members in `candidates` that are NOT in `workspace_members` (case-
/// insensitive hex compare). moyu requires a private channel's members ⊆ the
/// parent workspace's members and calls this at invite time to flag any outsider.
/// Empty result = every candidate is a workspace member. App-level check only —
/// MLS does not enforce cross-group membership.
pub fn members_outside_workspace(
    candidates: &[String],
    workspace_members: &[String],
) -> Vec<String> {
    let inside: BTreeSet<String> = workspace_members
        .iter()
        .map(|m| m.to_ascii_lowercase())
        .collect();
    candidates
        .iter()
        .filter(|c| !inside.contains(&c.to_ascii_lowercase()))
        .cloned()
        .collect()
}

/// One private-channel group resolved for display under a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateChannelRef {
    pub group_id_hex: String,
    pub parent_group_id_hex: String,
    pub slug: String,
    pub name: String,
}

impl PrivateChannelRef {
    /// Build from a group's `group_id` hex and its decoded [`PrivateChannelLink`].
    pub fn from_link(group_id_hex: impl Into<String>, link: PrivateChannelLink) -> Self {
        Self {
            group_id_hex: group_id_hex.into(),
            parent_group_id_hex: link.parent_group_id_hex,
            slug: link.slug,
            name: link.name,
        }
    }

    /// Display name, falling back to the slug when no explicit name was set.
    pub fn display_name(&self) -> &str {
        name_or_slug(&self.name, &self.slug)
    }
}

/// The result of nesting private-channel groups under their parent workspaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NestedPrivateChannels {
    /// parent-workspace `group_id` hex -> its private channels, sorted by slug.
    pub by_parent: BTreeMap<String, Vec<PrivateChannelRef>>,
    /// Private channels whose parent workspace is not among the known workspace
    /// groups — you are in the channel but not its parent workspace (the
    /// orphan case). Sorted by (parent, slug).
    pub orphans: Vec<PrivateChannelRef>,
}

/// Bucket private-channel groups under their parent workspace `group_id`. A
/// channel whose `parent_group_id_hex` is in `workspace_group_hexes` nests under
/// that workspace; otherwise it is an orphan. Deterministic ordering
/// (by slug within a parent; by (parent, slug) among orphans) so every client
/// renders the same tree.
pub fn nest_private_channels(
    channels: Vec<PrivateChannelRef>,
    workspace_group_hexes: &BTreeSet<String>,
) -> NestedPrivateChannels {
    let mut out = NestedPrivateChannels::default();
    for ch in channels {
        if workspace_group_hexes.contains(&ch.parent_group_id_hex) {
            out.by_parent
                .entry(ch.parent_group_id_hex.clone())
                .or_default()
                .push(ch);
        } else {
            out.orphans.push(ch);
        }
    }
    // Total order (final tiebreak on the cross-client-identical group_id_hex) so
    // two channels that legitimately share a (parent, slug) — slug uniqueness is
    // only a convention, not enforced — still render in the same order everywhere.
    for list in out.by_parent.values_mut() {
        list.sort_by(|a, b| {
            a.slug
                .cmp(&b.slug)
                .then_with(|| a.group_id_hex.cmp(&b.group_id_hex))
        });
    }
    out.orphans.sort_by(|a, b| {
        a.parent_group_id_hex
            .cmp(&b.parent_group_id_hex)
            .then_with(|| a.slug.cmp(&b.slug))
            .then_with(|| a.group_id_hex.cmp(&b.group_id_hex))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AppMessageRecord;

    // ----- M3 private channels -----

    const PARENT_HEX: &str = "94c037b93664532b6e0dab2d817c5ae9";

    #[test]
    fn private_channel_name_round_trips() {
        let encoded = encode_private_channel_name(PARENT_HEX, "backend", "Secret Backend");
        let link = decode_private_channel_name(&encoded).expect("decodes");
        assert_eq!(link.parent_group_id_hex, PARENT_HEX);
        assert_eq!(link.slug, "backend");
        assert_eq!(link.name, "Secret Backend");
        assert_eq!(link.display_name(), "Secret Backend");
        // Comfortably under MDK's 256-byte profile-name limit.
        assert!(encoded.len() < 256);
    }

    #[test]
    fn encode_normalizes_slug_lowercases_parent_and_sanitizes_name() {
        // Uppercase parent hex is lowercased; a spaced/upper slug is normalized;
        // a name containing the separator/sentinel has them collapsed to a space.
        let encoded = encode_private_channel_name("94C037B9", "Back End", "ev\u{1f}il\u{1}name");
        let link = decode_private_channel_name(&encoded).expect("decodes");
        assert_eq!(link.parent_group_id_hex, "94c037b9");
        assert_eq!(link.slug, "back-end");
        assert!(!link.name.contains(PRIVATE_CHANNEL_SEP));
        assert!(!link.name.contains('\u{1}'));
    }

    #[test]
    fn decode_rejects_non_private_names() {
        // DM sentinel, a workspace name, arbitrary text, empty — none decode.
        for name in [DM_GROUP_NAME, "Acme", "just some text", "", "moyu.pch"] {
            assert!(
                decode_private_channel_name(name).is_none(),
                "{name:?} must not decode as a private channel"
            );
        }
    }

    #[test]
    fn decode_rejects_malformed_private_names() {
        let tag = PRIVATE_CHANNEL_TAG;
        let sep = PRIVATE_CHANNEL_SEP;
        // Sentinel but too few fields, empty parent, empty slug, non-hex parent.
        let bad = [
            format!("{tag}{sep}{PARENT_HEX}"), // missing slug field entirely
            format!("{tag}{sep}{sep}backend{sep}Name"), // empty parent
            format!("{tag}{sep}{PARENT_HEX}{sep}{sep}Name"), // empty slug
            format!("{tag}{sep}nothex!{sep}backend{sep}Name"), // parent not hex
        ];
        for s in bad {
            assert!(
                decode_private_channel_name(&s).is_none(),
                "malformed {s:?} must not decode"
            );
        }
    }

    #[test]
    fn classify_group_distinguishes_all_three_kinds() {
        // DM: exactly the sentinel, no projected name.
        assert_eq!(classify_group(DM_GROUP_NAME, None), GroupKind::Dm);
        // Workspace: a plain name; or belt-and-suspenders a "dm"-profiled group
        // that already carries a projected moyu name.
        assert_eq!(classify_group("Acme", None), GroupKind::Workspace);
        assert_eq!(
            classify_group(DM_GROUP_NAME, Some("Acme")),
            GroupKind::Workspace
        );
        // Private channel: an encoded profile name wins over the workspace arm.
        let encoded = encode_private_channel_name(PARENT_HEX, "backend", "BE");
        match classify_group(&encoded, None) {
            GroupKind::PrivateChannel(link) => {
                assert_eq!(link.parent_group_id_hex, PARENT_HEX);
                assert_eq!(link.slug, "backend");
            }
            other => panic!("expected PrivateChannel, got {other:?}"),
        }
    }

    #[test]
    fn members_outside_workspace_is_case_insensitive() {
        let ws = vec!["AABB".to_owned(), "ccdd".to_owned()];
        // All inside (mixed case) -> empty.
        assert!(members_outside_workspace(&["aabb".to_owned(), "CCDD".to_owned()], &ws).is_empty());
        // One outsider is reported verbatim.
        let outside = members_outside_workspace(&["aabb".to_owned(), "eeff".to_owned()], &ws);
        assert_eq!(outside, vec!["eeff".to_owned()]);
    }

    #[test]
    fn nest_private_channels_buckets_under_parent_and_collects_orphans() {
        let known: BTreeSet<String> = [PARENT_HEX.to_owned()].into_iter().collect();
        let channels = vec![
            PrivateChannelRef {
                group_id_hex: "g2".into(),
                parent_group_id_hex: PARENT_HEX.into(),
                slug: "zeta".into(),
                name: "Z".into(),
            },
            PrivateChannelRef {
                group_id_hex: "g1".into(),
                parent_group_id_hex: PARENT_HEX.into(),
                slug: "alpha".into(),
                name: "A".into(),
            },
            PrivateChannelRef {
                group_id_hex: "g3".into(),
                parent_group_id_hex: "deadbeef".into(), // parent workspace not known
                slug: "orphan".into(),
                name: String::new(),
            },
        ];
        let nested = nest_private_channels(channels, &known);
        // Two channels nest under the known parent, sorted by slug.
        let under = &nested.by_parent[PARENT_HEX];
        assert_eq!(
            under.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        // The unknown-parent channel is an orphan; empty name displays as slug.
        assert_eq!(nested.orphans.len(), 1);
        assert_eq!(nested.orphans[0].slug, "orphan");
        assert_eq!(nested.orphans[0].display_name(), "orphan");
    }

    #[test]
    fn nest_private_channels_is_deterministic_on_duplicate_slug() {
        // Two DISTINCT groups legitimately share (parent, slug); the tree order
        // must be identical regardless of the local enumeration order.
        let known: BTreeSet<String> = [PARENT_HEX.to_owned()].into_iter().collect();
        let mk = |gid: &str| PrivateChannelRef {
            group_id_hex: gid.to_owned(),
            parent_group_id_hex: PARENT_HEX.into(),
            slug: "backend".into(),
            name: "BE".into(),
        };
        let forward = nest_private_channels(vec![mk("aaaa"), mk("bbbb")], &known);
        let reversed = nest_private_channels(vec![mk("bbbb"), mk("aaaa")], &known);
        let order = |n: &NestedPrivateChannels| {
            n.by_parent[PARENT_HEX]
                .iter()
                .map(|c| c.group_id_hex.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&forward), vec!["aaaa".to_owned(), "bbbb".to_owned()]);
        assert_eq!(order(&forward), order(&reversed)); // input order irrelevant
    }

    #[test]
    fn reserved_group_name_flags_only_the_sentinel_prefix() {
        // A real encoded private-channel name is reserved; ordinary names are not.
        assert!(is_reserved_group_name(&encode_private_channel_name(
            PARENT_HEX, "x", "X"
        )));
        assert!(is_reserved_group_name("\u{1}moyu.pch anything"));
        assert!(!is_reserved_group_name("Acme"));
        assert!(!is_reserved_group_name(DM_GROUP_NAME));
        assert!(!is_reserved_group_name("moyu.pch")); // no SOH -> not reserved
        assert!(!is_reserved_group_name(""));
    }

    /// Build an `AppMessageRecord` shaped exactly like the local/received
    /// projection of `send_group_system_event(_, system_type, text, data)`:
    /// content JSON `{"v":1,"system_type":..,"text":..,"data":..}`, kind 1210,
    /// tag `["system", system_type]`. `msg_id` is the cross-client event id.
    fn sys_record(
        system_type: &str,
        data: serde_json::Value,
        recorded_at: u64,
        msg_id: &str,
    ) -> AppMessageRecord {
        let content = serde_json::json!({
            "v": 1,
            "system_type": system_type,
            "text": "",
            "data": data,
        });
        AppMessageRecord {
            message_id_hex: msg_id.to_owned(),
            direction: "received".to_owned(),
            group_id_hex: "grp".to_owned(),
            sender: "npub1sender".to_owned(),
            plaintext: content.to_string(),
            kind: KIND_GROUP_SYSTEM,
            tags: vec![vec!["system".to_owned(), system_type.to_owned()]],
            source_epoch: None,
            recorded_at,
            received_at: recorded_at,
            insert_order: 0,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        }
    }

    #[test]
    fn control_events_round_trip_through_the_wire_shape() {
        let cases = vec![
            ControlEvent::ChannelCreate {
                slug: "backend".into(),
                name: "Backend".into(),
            },
            ControlEvent::ChannelRename {
                slug: "backend".into(),
                name: "BE".into(),
                ts: 5,
            },
            ControlEvent::ChannelArchive {
                slug: "backend".into(),
                ts: 6,
            },
            ControlEvent::WorkspaceRename {
                name: "Acme".into(),
                ts: 7,
            },
            ControlEvent::WorkspaceSnapshot {
                name: "Acme".into(),
                name_ts: 7,
                channels: vec![SnapshotChannel {
                    slug: "general".into(),
                    name: "general".into(),
                    archived: false,
                    ts: 0,
                }],
            },
        ];
        for ev in cases {
            let (system_type, data) = ev.to_wire();
            assert!(
                system_type.starts_with(CONTROL_PREFIX),
                "moyu control events must be namespaced: {system_type}"
            );
            let rec = sys_record(&system_type, data, 1, "m1");
            let decoded = control_event_from_record(&rec).expect("decodes back");
            assert_eq!(decoded, ev);
        }
    }

    #[test]
    fn non_moyu_and_non_1210_records_are_ignored() {
        // MDK's own member_added: kind 1210 but system_type is not `moyu.*`.
        let mdk_member_added = {
            let mut r = sys_record("member_added", serde_json::json!({}), 1, "m1");
            r.direction = "system".to_owned();
            r
        };
        assert!(control_event_from_record(&mdk_member_added).is_none());

        // A plain chat message (kind 9) is never a control event.
        let mut chat = sys_record("moyu.channel.create", serde_json::json!({}), 1, "m1");
        chat.kind = 9;
        assert!(control_event_from_record(&chat).is_none());

        // Malformed control payload is skipped, not fatal.
        let mut bad = sys_record("moyu.channel.create", serde_json::json!({}), 1, "m1");
        bad.plaintext = "{not json".to_owned();
        assert!(control_event_from_record(&bad).is_none());
    }

    #[test]
    fn normalize_slug_makes_safe_stable_slugs() {
        assert_eq!(normalize_slug("Backend"), "backend");
        assert_eq!(normalize_slug("Back End Team"), "back-end-team");
        assert_eq!(normalize_slug("  #core_decisions!! "), "core-decisions");
        assert_eq!(normalize_slug("a---b"), "a-b");
        assert_eq!(normalize_slug(""), DEFAULT_CHANNEL_SLUG);
        assert_eq!(normalize_slug("!!!"), DEFAULT_CHANNEL_SLUG);
    }

    #[test]
    fn channel_body_round_trips_marker_guards_against_false_positives() {
        let wire = encode_channel_body("backend", "hi there");
        let (slug, body) = decode_channel_body(&wire);
        assert_eq!((slug.as_str(), body.as_str()), ("backend", "hi there"));

        // Legacy M0 plaintext (not our JSON envelope) -> general, verbatim body.
        let (slug, body) = decode_channel_body("plain old message");
        assert_eq!(
            (slug.as_str(), body.as_str()),
            (DEFAULT_CHANNEL_SLUG, "plain old message"),
            "legacy/unenveloped text routes to #general untouched"
        );

        // A developer literally typing JSON with a `ch` field but NO moyu marker
        // must NOT be hijacked/lost — it stays in #general, shown verbatim.
        let typed = r#"{"ch":"test"}"#;
        let (slug, body) = decode_channel_body(typed);
        assert_eq!(
            (slug.as_str(), body.as_str()),
            (DEFAULT_CHANNEL_SLUG, typed),
            "user-typed JSON without the moyu marker is not treated as an envelope"
        );
    }

    fn ch<'a>(p: &'a WorkspaceProjection, slug: &str) -> &'a ChannelState {
        p.channel(slug).expect("channel present")
    }

    #[test]
    fn apply_creates_dedups_renames_and_archives() {
        let mut p = WorkspaceProjection::new();

        p.apply(
            &ControlEvent::ChannelCreate {
                slug: "backend".into(),
                name: "Backend".into(),
            },
            100,
            "a",
        );
        assert_eq!(ch(&p, "backend").name, "Backend");
        assert!(!ch(&p, "backend").archived);

        // duplicate create is ignored (dedup by slug)
        p.apply(
            &ControlEvent::ChannelCreate {
                slug: "backend".into(),
                name: "OTHER".into(),
            },
            101,
            "b",
        );
        assert_eq!(
            ch(&p, "backend").name,
            "Backend",
            "duplicate create ignored"
        );

        // rename wins (ts 5 > 0)
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "backend".into(),
                name: "BE".into(),
                ts: 5,
            },
            105,
            "c",
        );
        assert_eq!(ch(&p, "backend").name, "BE");

        // stale rename (ts 4 < 5) is dropped by LWW
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "backend".into(),
                name: "STALE".into(),
                ts: 4,
            },
            106,
            "d",
        );
        assert_eq!(ch(&p, "backend").name, "BE", "older rename dropped by LWW");

        // archive sets archived (monotonic; no ts needed)
        p.apply(
            &ControlEvent::ChannelArchive {
                slug: "backend".into(),
                ts: 9,
            },
            109,
            "e",
        );
        assert!(ch(&p, "backend").archived);

        // C4: a newer rename AFTER an archive must NOT clear the archived flag
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "backend".into(),
                name: "Prod".into(),
                ts: 100,
            },
            110,
            "f",
        );
        assert_eq!(ch(&p, "backend").name, "Prod");
        assert!(
            ch(&p, "backend").archived,
            "archive is monotonic — a later rename cannot un-archive"
        );

        // rename to a channel that doesn't exist is ignored
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "ghost".into(),
                name: "x".into(),
                ts: 1,
            },
            111,
            "g",
        );
        assert!(p.channel("ghost").is_none());
    }

    #[test]
    fn workspace_rename_is_strict_lww_and_snapshot_never_regresses() {
        let mut p = WorkspaceProjection::new();
        p.apply(
            &ControlEvent::WorkspaceRename {
                name: "Acme".into(),
                ts: 5,
            },
            100,
            "a",
        );
        assert_eq!(p.name.as_deref(), Some("Acme"));
        // older workspace rename (ts 4 < 5) dropped
        p.apply(
            &ControlEvent::WorkspaceRename {
                name: "OLD".into(),
                ts: 4,
            },
            101,
            "b",
        );
        assert_eq!(p.name.as_deref(), Some("Acme"), "older rename dropped");

        // local: backend renamed to "BE" at ts 8
        p.apply(
            &ControlEvent::ChannelCreate {
                slug: "backend".into(),
                name: "Backend".into(),
            },
            90,
            "c",
        );
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "backend".into(),
                name: "BE".into(),
                ts: 8,
            },
            108,
            "d",
        );

        // snapshot carries backend with an OLDER ts 3 -> must not clobber "BE";
        // and adds #random (new). snapshot name_ts 5 == current 5 but msgid "z"
        // > "a": a *strictly greater* key, so it may set the name deterministically
        // — the point is every member computes the same winner.
        let snap = ControlEvent::WorkspaceSnapshot {
            name: "Acme".into(),
            name_ts: 5,
            channels: vec![
                SnapshotChannel {
                    slug: "backend".into(),
                    name: "SNAP-OLD".into(),
                    archived: false,
                    ts: 3,
                },
                SnapshotChannel {
                    slug: "random".into(),
                    name: "random".into(),
                    archived: false,
                    ts: 2,
                },
            ],
        };
        p.apply(&snap, 120, "z");
        assert_eq!(
            ch(&p, "backend").name,
            "BE",
            "snapshot must not regress a newer local rename"
        );
        assert_eq!(
            ch(&p, "random").name,
            "random",
            "snapshot adds missing channels"
        );
    }

    #[test]
    fn ordered_channels_are_creation_ordered_general_first() {
        let recs = vec![
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "zeta", "name": "Zeta" }),
                100,
                "a",
            ),
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "alpha", "name": "Alpha" }),
                101,
                "b",
            ),
        ];
        let p = project_workspace(&recs);
        let order: Vec<&str> = p
            .ordered_channels()
            .iter()
            .map(|c| c.slug.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["general", "zeta", "alpha"],
            "general first, then by creation time"
        );
    }

    #[test]
    fn project_workspace_ignores_noise_orders_by_time_and_guarantees_general() {
        let mut recs = vec![
            // out-of-order in the vec: rename (recorded_at 200) before create (100)
            sys_record(
                "moyu.channel.rename",
                serde_json::json!({ "slug": "backend", "name": "BE", "ts": 9 }),
                200,
                "r",
            ),
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "backend", "name": "Backend" }),
                100,
                "c",
            ),
            // noise the projection must ignore:
            {
                // a real chat message (kind 9), carrying a channel envelope
                let mut r = sys_record("moyu.channel.create", serde_json::json!({}), 150, "chat");
                r.kind = 9;
                r.plaintext = encode_channel_body("backend", "hello");
                r
            },
            {
                // MDK's own member_added system row
                let mut r = sys_record("member_added", serde_json::json!({}), 160, "sys");
                r.direction = "system".to_owned();
                r
            },
            sys_record(
                "moyu.workspace.rename",
                serde_json::json!({ "name": "Acme", "ts": 3 }),
                120,
                "w",
            ),
        ];
        let p = project_workspace(&recs);

        assert_eq!(p.name.as_deref(), Some("Acme"));
        assert_eq!(
            p.channel("backend").expect("backend exists").name,
            "BE",
            "create(100) then rename(200) applied in recorded_at order"
        );
        assert!(
            p.channel(DEFAULT_CHANNEL_SLUG).is_some(),
            "general always present"
        );

        // empty input still yields a workspace with just #general
        recs.clear();
        let empty = project_workspace(&recs);
        assert_eq!(empty.ordered_channels().len(), 1);
        assert_eq!(empty.ordered_channels()[0].slug, DEFAULT_CHANNEL_SLUG);
    }

    /// C3: a rename/archive targeting `general` that arrives before any explicit
    /// create/snapshot for general must still apply (general is seeded first).
    #[test]
    fn general_can_be_renamed_and_archived_without_an_explicit_create() {
        let recs = vec![
            sys_record(
                "moyu.channel.rename",
                serde_json::json!({ "slug": "general", "name": "General Chat", "ts": 5 }),
                100,
                "a",
            ),
            sys_record(
                "moyu.channel.archive",
                serde_json::json!({ "slug": "general", "ts": 6 }),
                101,
                "b",
            ),
        ];
        let p = project_workspace(&recs);
        assert_eq!(
            ch(&p, "general").name,
            "General Chat",
            "rename applies to seeded general"
        );
        assert!(
            ch(&p, "general").archived,
            "archive applies to seeded general"
        );
    }

    /// C1: the core convergence property — the SAME control messages in ANY
    /// received order project to the SAME state (equal `recorded_at`/`ts` events
    /// are broken deterministically by `message_id_hex`, never local order).
    #[test]
    fn projection_is_order_independent_convergent() {
        // Two concurrent renames of `backend`: same second (recorded_at 100),
        // same logical ts 10, different content ids "y"/"x".
        let create = sys_record(
            "moyu.channel.create",
            serde_json::json!({ "slug": "backend", "name": "Backend" }),
            50,
            "c",
        );
        let rename_x = sys_record(
            "moyu.channel.rename",
            serde_json::json!({ "slug": "backend", "name": "X-name", "ts": 10 }),
            100,
            "x",
        );
        let rename_y = sys_record(
            "moyu.channel.rename",
            serde_json::json!({ "slug": "backend", "name": "Y-name", "ts": 10 }),
            100,
            "y",
        );

        // "Alice" stores create, x, y; "Bob" stores them in a different local order.
        let alice = project_workspace(&[create.clone(), rename_x.clone(), rename_y.clone()]);
        let bob = project_workspace(&[rename_y, rename_x, create]);

        // Deterministic winner is the higher message_id_hex at equal ts => "y".
        assert_eq!(alice.channel("backend").unwrap().name, "Y-name");
        assert_eq!(
            alice.channel("backend").unwrap().name,
            bob.channel("backend").unwrap().name,
            "two members with the same messages must converge regardless of local order"
        );
    }

    // ----- §5.2 join catch-up fallback ---------------------------------------

    /// MDK's own `member_added` row: kind 1210, `system_type == "member_added"`
    /// (no `moyu.` prefix), `data.{actor,subject}` present.
    fn member_added_record(recorded_at: u64, msg_id: &str) -> AppMessageRecord {
        let mut r = sys_record(
            "member_added",
            serde_json::json!({ "actor": "aa", "subject": "bb" }),
            recorded_at,
            msg_id,
        );
        r.direction = "system".to_owned();
        r
    }

    #[test]
    fn is_default_only_for_untouched_seed() {
        assert!(
            project_workspace(&[]).is_default(),
            "empty log -> just seeded #general -> default"
        );
        let extra_channel = vec![sys_record(
            "moyu.channel.create",
            serde_json::json!({ "slug": "backend", "name": "Backend" }),
            100,
            "c",
        )];
        assert!(
            !project_workspace(&extra_channel).is_default(),
            "an extra channel makes it non-default"
        );
        let renamed_ws = vec![sys_record(
            "moyu.workspace.rename",
            serde_json::json!({ "name": "Acme", "ts": 5 }),
            100,
            "w",
        )];
        assert!(
            !project_workspace(&renamed_ws).is_default(),
            "a workspace rename makes it non-default"
        );
        let renamed_general = vec![sys_record(
            "moyu.channel.rename",
            serde_json::json!({ "slug": "general", "name": "Lobby", "ts": 5 }),
            100,
            "r",
        )];
        assert!(
            !project_workspace(&renamed_general).is_default(),
            "renaming #general itself makes it non-default"
        );
    }

    /// A member flooding `channel_create` events (or one giant snapshot)
    /// cannot grow the channel list without bound; existing channels keep
    /// working at the ceiling.
    #[test]
    fn the_channel_list_is_capped() {
        let mut p = WorkspaceProjection::default();
        for i in 0..(MAX_CHANNELS + 50) {
            p.apply(
                &ControlEvent::ChannelCreate {
                    slug: format!("c{i}"),
                    name: format!("c{i}"),
                },
                i as u64,
                &format!("m{i}"),
            );
        }
        assert_eq!(p.channels.len(), MAX_CHANNELS);
        // a snapshot carrying yet more new channels adds none...
        let extra: Vec<SnapshotChannel> = (0..10)
            .map(|i| SnapshotChannel {
                slug: format!("x{i}"),
                name: format!("x{i}"),
                archived: false,
                ts: 1,
            })
            .collect();
        p.apply(
            &ControlEvent::WorkspaceSnapshot {
                name: "ws".into(),
                name_ts: 1,
                channels: extra,
            },
            9_999,
            "snap",
        );
        assert_eq!(p.channels.len(), MAX_CHANNELS);
        // ...but a rename of a channel that already exists still applies.
        p.apply(
            &ControlEvent::ChannelRename {
                slug: "c0".into(),
                name: "renamed".into(),
                ts: 5,
            },
            10_000,
            "ren",
        );
        assert_eq!(p.channels["c0"].name, "renamed");
    }

    #[test]
    fn membership_change_reads_mdks_membership_rows_only() {
        // MDK stores the rows it synthesizes itself with direction "system".
        let row = |ty: &str, data: serde_json::Value| {
            let mut r = sys_record(ty, data, 1, "m");
            r.direction = "system".to_owned();
            r
        };
        let subject = serde_json::json!({ "actor": "aa", "subject": "BB11" });
        assert_eq!(
            membership_change(&row("member_added", subject.clone())),
            Some((MembershipChange::Added, "bb11".to_owned()))
        );
        assert_eq!(
            membership_change(&row("member_removed", subject.clone())),
            Some((MembershipChange::Removed, "bb11".to_owned()))
        );
        assert_eq!(
            membership_change(&row("member_left", subject.clone())),
            Some((MembershipChange::Left, "bb11".to_owned()))
        );
        // other system types, a missing or empty subject, a non-system kind
        assert_eq!(
            membership_change(&row("admin_added", subject.clone())),
            None
        );
        assert_eq!(
            membership_change(&row("moyu.workspace.rename", subject.clone())),
            None
        );
        assert_eq!(
            membership_change(&row("member_removed", serde_json::json!({}))),
            None
        );
        assert_eq!(
            membership_change(&row("member_removed", serde_json::json!({ "subject": "" }))),
            None
        );
        let mut chat = row("member_removed", subject.clone());
        chat.kind = 9;
        assert_eq!(membership_change(&chat), None);
        // The same content SENT by a member (arrives as "received", or is our
        // own "sent" copy) is a forgery, not a membership change.
        for direction in ["received", "sent"] {
            let mut forged = row("member_removed", subject.clone());
            forged.direction = direction.to_owned();
            assert_eq!(membership_change(&forged), None, "{direction}");
        }
    }

    #[test]
    fn member_added_at_detects_only_mdk_member_added() {
        assert_eq!(member_added_at(&member_added_record(42, "m")), Some(42));
        let ours = sys_record(
            "moyu.channel.create",
            serde_json::json!({ "slug": "x", "name": "X" }),
            1,
            "a",
        );
        assert_eq!(
            member_added_at(&ours),
            None,
            "a moyu.* control event is not a member_added"
        );
        let mut chat = member_added_record(1, "c");
        chat.kind = 9;
        assert_eq!(
            member_added_at(&chat),
            None,
            "kind-9 chat is never a member_added"
        );
    }

    #[test]
    fn catch_up_none_when_default_even_with_join() {
        // A DM / trivial workspace: a join happened but there is nothing to teach.
        let recs = vec![member_added_record(100, "j")];
        assert_eq!(catch_up_snapshot(&recs, "dm"), None);
    }

    #[test]
    fn catch_up_none_when_snapshot_covers_join() {
        // Non-default (extra channel) + a join + a snapshot at/after it (the
        // adder's primary path) => no fallback needed.
        let recs = vec![
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "backend", "name": "Backend" }),
                50,
                "c",
            ),
            member_added_record(100, "j"),
            sys_record(
                "moyu.workspace.snapshot",
                serde_json::json!({ "name": "Acme", "name_ts": 0, "channels": [] }),
                100, // recorded_at >= join ts 100
                "s",
            ),
        ];
        assert_eq!(
            catch_up_snapshot(&recs, "Acme"),
            None,
            "adder's snapshot at/after the join covers it"
        );
    }

    #[test]
    fn catch_up_broadcasts_full_snapshot_when_join_uncovered() {
        // Non-default (extra channel) + a join with NO snapshot at/after => fire.
        let recs = vec![
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "backend", "name": "Backend" }),
                50,
                "c",
            ),
            member_added_record(100, "j"),
        ];
        match catch_up_snapshot(&recs, "Acme-profile").expect("uncovered join -> broadcast") {
            ControlEvent::WorkspaceSnapshot { name, channels, .. } => {
                assert_eq!(
                    name, "Acme-profile",
                    "name falls back to the MDK profile name when never renamed"
                );
                let slugs: Vec<&str> = channels.iter().map(|c| c.slug.as_str()).collect();
                assert_eq!(
                    slugs,
                    vec!["general", "backend"],
                    "snapshot carries the full current channel set, general first"
                );
            }
            other => panic!("expected WorkspaceSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn catch_up_is_self_terminating() {
        // Once the fallback's own snapshot lands (recorded_at >= join), a re-scan
        // returns None -- so the fallback never loops.
        let mut recs = vec![
            sys_record(
                "moyu.channel.create",
                serde_json::json!({ "slug": "backend", "name": "Backend" }),
                50,
                "c",
            ),
            member_added_record(100, "j"),
        ];
        assert!(catch_up_snapshot(&recs, "Acme").is_some());
        recs.push(sys_record(
            "moyu.workspace.snapshot",
            serde_json::json!({
                "name": "Acme",
                "name_ts": 0,
                "channels": [
                    { "slug": "general", "name": "general", "archived": false, "ts": 0 },
                    { "slug": "backend", "name": "Backend", "archived": false, "ts": 0 }
                ]
            }),
            101, // the broadcast's own recorded_at ("now") is >= the join
            "s",
        ));
        assert_eq!(
            catch_up_snapshot(&recs, "Acme"),
            None,
            "self-terminating once the catch-up snapshot is recorded"
        );
    }

    #[test]
    fn catch_up_uses_projection_name_when_renamed() {
        let recs = vec![
            sys_record(
                "moyu.workspace.rename",
                serde_json::json!({ "name": "Renamed", "ts": 5 }),
                40,
                "w",
            ),
            member_added_record(100, "j"),
        ];
        match catch_up_snapshot(&recs, "profile-fallback").expect("uncovered join") {
            ControlEvent::WorkspaceSnapshot { name, .. } => assert_eq!(
                name, "Renamed",
                "a renamed workspace uses its projection name, not the profile fallback"
            ),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }
}
