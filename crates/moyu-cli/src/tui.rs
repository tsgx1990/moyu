//! `moyu tui`: a ratatui full-screen chat UI over the same headless
//! `moyu_core` engine the CLI commands use.
//!
//! # Design
//!
//! Same single-owner-`AppClient` + `select!`-over-input-and-a-poll-tick shape
//! as `crate::repl` (see that module's doc for why `sync()` polling — not the
//! unbounded `next_event()` — is the deadlock-safe choice): `AppClient` is
//! owned by this one task, crossterm key events are read on a blocking OS
//! thread (crossterm's `read()` is sync) and forwarded over an mpsc, and a
//! periodic tick drains the bounded `AppClient::sync()`. Incoming messages are
//! routed into a **per-chat** buffer keyed by `GroupId`, so — unlike the M0
//! `recv`, which only printed the current group — every chat stays visible and
//! live at once (closes the old L-2 multi-group-visibility gap).
//!
//! Scope (M1 v1): one screen — a chat list, the selected chat's messages, an
//! input line, and a status bar. Starting a brand-new DM works (first send
//! founds the 1:1 group); persisted history is backfilled on open (see
//! [`Model::load_history`]) so a chat opens showing its conversation, then
//! `sync()` appends new messages on top. Group management / search / settings
//! are deliberately out of scope for this first cut.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Stdout;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use moyu_core::engine::{AppClient, AppMessageRecord, GroupId, MoyuEngine, ReceivedMessage};
use moyu_core::identity::npub_from_hex;
use moyu_core::store::{Contact, MoyuStore};
use moyu_core::workspace::{self, DEFAULT_CHANNEL_SLUG, DM_GROUP_NAME, KIND_GROUP_SYSTEM};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Nostr `kind` of a Marmot chat message (vs 7 reaction, 1210 group-system).
const CHAT_MESSAGE_KIND: u64 = 9;
/// Nostr `kind` of a reaction (kind-7) and a delete (kind-5). A kind-7 attaches
/// an emoji to the message its `e` tag points at; an `unreact` is a kind-5
/// delete of the *reaction* event, so retracting a reaction means finding the
/// kind-7 that kind-5 deletes. (Mirrors MDK's app_event.rs kind constants.)
const REACTION_KIND: u64 = 7;
const DELETE_KIND: u64 = 5;
/// Nostr `kind`s of bot/agent events: a structured operation
/// (kind-1202, CI/deploy/git/monitor) and a lightweight activity line
/// (kind-1201). Both render as a distinct 🤖 line, status-colored, rather than
/// as a plain chat line. (Mirror `crate::domain::AGENT_{OPERATION,ACTIVITY}_KIND`.)
const AGENT_OPERATION_KIND: u64 = 1202;
const AGENT_ACTIVITY_KIND: u64 = 1201;

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Extra state a bot/agent line (kind-1201/1202) carries beyond a chat line:
/// the `ok` tri-state drives the status color (green ✓ / red ✗ / magenta none).
struct BotMeta {
    ok: Option<bool>,
}

struct Msg {
    /// This message's own `message_id_hex` — the id a reaction/reply targets.
    /// Empty only for a locally-echoed send whose id `SendSummary` didn't
    /// return (then a reaction to it just won't attach — a cosmetic v1 gap).
    id: String,
    who: String,
    text: String,
    mine: bool,
    /// Parent `message_id` when this is a threaded reply (kind-9 with a `q`
    /// quote tag); rendered as a dim quote line above the reply.
    reply_to: Option<String>,
    /// Aggregated reaction counts on THIS message (emoji -> count), rendered as
    /// dim pills beneath it. `BTreeMap` for a stable emoji order across frames.
    reactions: BTreeMap<String, u32>,
    /// `Some` iff this line is a bot/agent event (kind-1201/1202): `text` is the
    /// pre-formatted body (`crate::domain::format_bot_event_body`) and this carries its
    /// status color. `None` for an ordinary chat line.
    bot: Option<BotMeta>,
}

impl Msg {
    /// A chat line (kind-9). Reactions start empty and accrue as kind-7 events
    /// arrive; `reply_to` is `Some` only for a threaded reply.
    ///
    /// `who`/`text` are sanitized here -- the one place every chat line
    /// enters the model, whether from live sync, backfilled history, or a
    /// join-request line built from an (attacker-controlled) invite envelope
    /// -- so ratatui's own control-character filtering (`Buffer::set_stringn`)
    /// doesn't have to be the only thing standing between a hostile Unicode
    /// bidi override and the rendered line.
    fn chat(id: String, who: String, text: String, mine: bool, reply_to: Option<String>) -> Self {
        Msg {
            id,
            who: crate::output::term_safe(&who).into_owned(),
            text: crate::output::term_safe(&text).into_owned(),
            mine,
            reply_to,
            reactions: BTreeMap::new(),
            bot: None,
        }
    }

    /// A bot/agent line (kind-1201/1202): `text` is the already-
    /// formatted body, `ok` the status tri-state for coloring. Never a reply
    /// target for reactions in v1 (bots don't get reacted to), so `reply_to` and
    /// `reactions` stay empty. `who`/`text` sanitized -- see [`Msg::chat`].
    fn bot(id: String, who: String, text: String, mine: bool, ok: Option<bool>) -> Self {
        Msg {
            id,
            who: crate::output::term_safe(&who).into_owned(),
            text: crate::output::term_safe(&text).into_owned(),
            mine,
            reply_to: None,
            reactions: BTreeMap::new(),
            bot: Some(BotMeta { ok }),
        }
    }
}

struct Chat {
    label: String,
    npub: String,
    /// The 1:1 MLS group, once one exists. `None` = no chat founded yet; the
    /// first send founds it.
    group: Option<GroupId>,
    messages: Vec<Msg>,
    unread: usize,
    /// Message ids already in `messages`, to dedup backfilled history against
    /// re-ingested sync events (MDK's `seen_events` cap can re-surface old
    /// messages past ~16k events, and a message lives in both history and a
    /// fresh sync).
    seen_ids: HashSet<String>,
}

impl Chat {
    fn group_hex(&self) -> Option<String> {
        self.group.as_ref().map(|g| g.to_string())
    }
}

/// One channel's message buffer inside a workspace. Buffers are keyed by
/// `(group, slug)` rather than just `group` -- a `message_id_hex` is only
/// unique *within* the channel its envelope decodes to, so `seen_ids` (and
/// `unread`) live per-channel, not per-workspace-group.
struct ChannelBuf {
    slug: String,
    archived: bool,
    /// `None` for a public channel (a logical tag inside the workspace's own MLS
    /// group -- routes to [`WorkspaceEntry::group`]); `Some(g)` for a PRIVATE
    /// channel, which is a *separate* MLS group `g` with its own epoch key.
    /// `is_private()` derives from this.
    group: Option<GroupId>,
    messages: Vec<Msg>,
    seen_ids: HashSet<String>,
    unread: usize,
}

impl ChannelBuf {
    /// A private channel (🔒) is its own MLS group; a public channel is a tag in
    /// the workspace group.
    fn is_private(&self) -> bool {
        self.group.is_some()
    }
}

/// One workspace: its MLS group plus the channels it currently has, in
/// display order (mirrors `WorkspaceProjection::ordered_channels`).
struct WorkspaceEntry {
    group: GroupId,
    name: String,
    channels: Vec<ChannelBuf>,
}

/// A flattened left-rail row: `model.selected` indexes into
/// `Model::rows()`, not `chats`/`workspaces` directly, so DMs and
/// workspace/channel entries share one selectable list.
#[derive(Debug)]
enum Row {
    Dm(usize),
    WorkspaceHeader(usize),
    Channel(usize, usize),
}

/// The whole UI state. Built once from the local store before entering the
/// alt-screen, then mutated by key events and sync ticks.
pub struct Model {
    account_npub_short: String,
    relay_summary: String,
    socks5_on: bool,
    /// The account's own label (its pubkey hex) -- needed to call
    /// `crate::domain::all_workspaces` when (re)building the workspace tree.
    account_label: String,
    chats: Vec<Chat>,
    workspaces: Vec<WorkspaceEntry>,
    /// `Some(rows)` while the `Ctrl-R` roster overlay is open -- each row is a
    /// final display string (shortened npub, admins first + " (admin)" badge).
    roster: Option<Vec<String>>,
    /// Last-tick roster+admin snapshot per workspace `group_id_hex`, refreshed by
    /// `sync_tick`. Two jobs: it is the "before" side of the governance diff
    /// (`crate::domain::governance_diff`), and it is where `open_roster` reads a group's
    /// admin set to badge the roster (the `Ctrl-R` handler has no `engine`, and
    /// `AppClient` alone can't surface `admin_policy` -- see `snapshot_rosters`).
    prev_rosters: HashMap<String, crate::domain::WsRoster>,
    /// Reaction bookkeeping so a later `unreact` (a kind-5 delete of the
    /// reaction event) can be undone: `reaction_event_id -> (group_hex,
    /// target_message_id, emoji)`. Also dedups a re-ingested kind-7 (a key
    /// already present was already counted).
    reaction_index: HashMap<String, (String, String, String)>,
    selected: usize,
    input: String,
    status: String,
}

impl Model {
    pub fn from_store(
        account_label: &str,
        store: &MoyuStore,
        relays: &[String],
        socks5_on: bool,
    ) -> Self {
        let account_npub_short = moyu_core::identity::npub_from_hex(account_label)
            .map(|n| shorten(&n))
            .unwrap_or_else(|_| account_label.chars().take(12).collect());

        let chats = store
            .contacts()
            .iter()
            .map(|c| Chat {
                // A contact's label can be a peer-chosen display name (set
                // while accepting their invite) -- sanitize on the way into
                // the model, same as `Msg::chat`/`Msg::bot`.
                label: crate::output::term_safe(&c.label).into_owned(),
                npub: c.npub.clone(),
                group: c.group_id_hex.as_ref().and_then(|h| match hex::decode(h) {
                    Ok(bytes) => Some(GroupId::new(bytes)),
                    Err(_) => {
                        // Corrupt on-disk hex -> treat as "no group yet" but say
                        // so (this runs before the alt-screen, so a log is fine);
                        // otherwise the next send would re-found a duplicate.
                        tracing::warn!(
                            "ignoring a contact's corrupt group_id_hex in the local store"
                        );
                        None
                    }
                }),
                messages: Vec::new(),
                unread: 0,
                seen_ids: HashSet::new(),
            })
            .collect();

        let relay_summary = if relays.is_empty() {
            "(none)".to_string()
        } else {
            relays
                .iter()
                .map(|r| shorten_relay(r))
                .collect::<Vec<_>>()
                .join(",")
        };

        Self {
            account_npub_short,
            relay_summary,
            socks5_on,
            account_label: account_label.to_string(),
            chats,
            workspaces: Vec::new(),
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".to_string(),
        }
    }

    /// The flattened left rail: every DM first, then per workspace a header
    /// row followed by its channels. `selected` indexes into this, not
    /// `chats`/`workspaces` directly, so the two kinds of entry share one
    /// list.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = (0..self.chats.len()).map(Row::Dm).collect();
        for (w, ws) in self.workspaces.iter().enumerate() {
            rows.push(Row::WorkspaceHeader(w));
            rows.extend((0..ws.channels.len()).map(move |c| Row::Channel(w, c)));
        }
        rows
    }

    /// The `Row` currently under the cursor, if any (`rows()` can be empty).
    fn selected_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.selected)
    }

    /// A stable identity for the currently-selected entity, independent of its
    /// positional index in `rows()`. Captured before any structural change
    /// (rebuild, a new DM chat pushed, an on-the-fly channel appended) and
    /// replayed via [`Model::reselect`] afterwards, so `selected` keeps
    /// pointing at the SAME chat/channel rather than at whatever entity slid
    /// into that index. A DM is keyed by its contact `npub` (stable even
    /// before a group exists), a workspace/channel by `(group_hex, slug)`.
    fn selected_identity(&self) -> Option<RowId> {
        match self.selected_row()? {
            Row::Dm(i) => Some(RowId::Dm(self.chats[i].npub.clone())),
            Row::WorkspaceHeader(w) => Some(RowId::Header(self.workspaces[w].group.to_string())),
            // Key by the channel's ROUTING group (a private channel's own group,
            // else the workspace group) so a private #foo and a public #foo under
            // the same workspace stay distinct identities across a rebuild.
            Row::Channel(w, c) => Some(RowId::Channel(
                self.channel_group(w, c).to_string(),
                self.workspaces[w].channels[c].slug.clone(),
            )),
        }
    }

    /// Restore `selected` to the row matching `prev`'s identity in the current
    /// `rows()`. If that entity vanished (e.g. its channel was archived away),
    /// clamp `selected` to a valid index instead of leaving it dangling. A
    /// no-op when nothing was selected. See [`Model::selected_identity`].
    fn reselect(&mut self, prev: Option<RowId>) {
        let Some(prev) = prev else {
            return;
        };
        let rows = self.rows();
        let found = rows.iter().position(|row| match (row, &prev) {
            (Row::Dm(i), RowId::Dm(npub)) => self.chats[*i].npub == *npub,
            (Row::WorkspaceHeader(w), RowId::Header(g)) => {
                self.workspaces[*w].group.to_string() == *g
            }
            (Row::Channel(w, c), RowId::Channel(g, s)) => {
                self.channel_group(*w, *c).to_string() == *g
                    && self.workspaces[*w].channels[*c].slug == *s
            }
            _ => false,
        });
        match found {
            Some(idx) => self.selected = idx,
            None => {
                let n = rows.len();
                self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
            }
        }
    }

    /// The local display name of the workspace whose MLS group id is `gid_hex`,
    /// if this account belongs to it. Lets a join-request render against the REAL
    /// workspace name instead of the envelope's attacker-controlled claim;
    /// `None` when the gid names no workspace we're in, so the
    /// caller shows the short gid rather than the unverified claim.
    fn workspace_name_by_gid(&self, gid_hex: &str) -> Option<String> {
        self.workspaces
            .iter()
            .find(|ws| ws.group.to_string().eq_ignore_ascii_case(gid_hex))
            .map(|ws| ws.name.clone())
    }

    /// Resolve an incoming message's `group_id_hex` (plus its
    /// envelope-decoded channel `slug`, irrelevant for DMs) to the buffer it
    /// belongs in. DMs are tried first (matches `chats`), then workspaces
    /// (matches `workspaces[*].group`) -- in practice the two never overlap
    /// (a group is either a 1:1 the store bound via `Contact::group_id_hex`,
    /// or a named workspace `all_workspaces` projected), so the order is
    /// paranoia, not load-bearing. A workspace group whose slug has no
    /// `ChannelBuf` yet (a message racing ahead of its
    /// `channel.create`/snapshot control event) gets one created on the fly,
    /// empty, so the message is never silently dropped. Returns `None` only
    /// when `group_id_hex` matches neither a chat nor a workspace.
    fn resolve_route(&mut self, group_id_hex: &str, slug: &str) -> Option<Target> {
        if let Some(i) = self
            .chats
            .iter()
            .position(|c| c.group_hex().as_deref() == Some(group_id_hex))
        {
            return Some(Target::Dm(i));
        }
        // A PRIVATE channel is its own MLS group, so an inbound message carries
        // the private channel's group id (not its parent workspace's). Match it by
        // that own group -- these buffers are built by `rebuild_workspaces` from
        // `membership_view`, never on the fly, so a miss here just falls through.
        for (w, ws) in self.workspaces.iter().enumerate() {
            if let Some(c) = ws.channels.iter().position(|c| {
                c.group.as_ref().map(|g| g.to_string()).as_deref() == Some(group_id_hex)
            }) {
                return Some(Target::Channel(w, c));
            }
        }
        // Otherwise a PUBLIC channel: the message's group is the workspace group,
        // routed by envelope slug (an unknown slug gets an on-the-fly buffer).
        let w = self
            .workspaces
            .iter()
            .position(|ws| ws.group.to_string() == group_id_hex)?;
        let c = match self.workspaces[w]
            .channels
            .iter()
            .position(|c| c.slug == slug && !c.is_private())
        {
            Some(c) => c,
            None => {
                self.workspaces[w].channels.push(ChannelBuf {
                    slug: slug.to_string(),
                    archived: false,
                    group: None,
                    messages: Vec::new(),
                    seen_ids: HashSet::new(),
                    unread: 0,
                });
                self.workspaces[w].channels.len() - 1
            }
        };
        Some(Target::Channel(w, c))
    }

    /// The MLS group a `(workspace, channel)` routes to: a private channel's own
    /// group, else the workspace's group (public channels are tags in it).
    fn channel_group(&self, w: usize, c: usize) -> GroupId {
        self.workspaces[w].channels[c]
            .group
            .clone()
            .unwrap_or_else(|| self.workspaces[w].group.clone())
    }

    /// Find the buffered message with id `id` in group `group_hex`, wherever it
    /// lives (a DM, or any channel of the workspace whose routing group is
    /// `group_hex`). A reaction only carries its target's id + group, never a
    /// channel slug, so locating the target is a scan rather than a route.
    fn find_msg_mut(&mut self, group_hex: &str, id: &str) -> Option<&mut Msg> {
        for chat in &mut self.chats {
            if chat.group_hex().as_deref() == Some(group_hex)
                && let Some(m) = chat.messages.iter_mut().find(|m| m.id == id)
            {
                return Some(m);
            }
        }
        for ws in &mut self.workspaces {
            let ws_hex = ws.group.to_string();
            for ch in &mut ws.channels {
                // Public channel routes to the workspace group; a private one is
                // its own MLS group (see `channel_group`).
                let ch_group_hex = match &ch.group {
                    Some(g) => g.to_string(),
                    None => ws_hex.clone(),
                };
                if ch_group_hex == group_hex
                    && let Some(m) = ch.messages.iter_mut().find(|m| m.id == id)
                {
                    return Some(m);
                }
            }
        }
        None
    }

    /// Attach a reaction to its target message and remember it (by the
    /// reaction's own event id) so a later `unreact` can undo it. A reaction
    /// whose target isn't buffered yet (arrived before its message, or before
    /// that group's history loaded) is dropped — a cosmetic v1 gap. `rid`
    /// already present means this kind-7 was counted before (re-ingest): skip.
    fn apply_reaction(&mut self, group_hex: &str, target_id: &str, emoji: &str, rid: String) {
        let emoji = emoji.trim();
        if emoji.is_empty() || target_id.is_empty() || self.reaction_index.contains_key(&rid) {
            return;
        }
        let applied = if let Some(m) = self.find_msg_mut(group_hex, target_id) {
            *m.reactions.entry(emoji.to_string()).or_insert(0) += 1;
            true
        } else {
            false
        };
        if applied {
            self.reaction_index.insert(
                rid,
                (
                    group_hex.to_string(),
                    target_id.to_string(),
                    emoji.to_string(),
                ),
            );
        }
    }

    /// Undo the reaction whose event id is `rid` (the id a kind-5 `unreact`
    /// deletes). A no-op for a kind-5 that deletes something other than a
    /// reaction we counted (e.g. a message deletion — not modelled in v1).
    fn retract_reaction(&mut self, rid: &str) {
        let Some((group_hex, target_id, emoji)) = self.reaction_index.remove(rid) else {
            return;
        };
        if let Some(m) = self.find_msg_mut(&group_hex, &target_id)
            && let Some(count) = m.reactions.get_mut(&emoji)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                m.reactions.remove(&emoji);
            }
        }
    }

    /// Route a bot/agent event (kind-1201/1202) into its channel buffer
    /// as a 🤖 line, mirroring the chat routing in
    /// [`load_history`](Self::load_history)/[`route_incoming`]. The channel slug
    /// comes from the event's own routing (`crate::domain::bot_event_channel`, absent for
    /// a private channel), defaulting to #general; the body is pre-formatted by
    /// `crate::domain::format_bot_event_body`; dedup + placement follow the chat path.
    /// Returns the `Target` it landed in (so a live caller can bump `unread`), or
    /// `None` if the group isn't tracked yet or the id was already seen.
    fn push_bot_event(
        &mut self,
        ghex: &str,
        mine: bool,
        sender_hex: &str,
        id: String,
        plaintext: &str,
        is_op: bool,
    ) -> Option<Target> {
        let content: serde_json::Value =
            serde_json::from_str(plaintext).unwrap_or_else(|_| serde_json::json!({}));
        let slug =
            crate::domain::bot_event_channel(&content).unwrap_or_else(|| "general".to_owned());
        let body = crate::domain::format_bot_event_body(&content, is_op);
        let ok = content.get("ok").and_then(|v| v.as_bool());
        match self.resolve_route(ghex, &slug)? {
            Target::Dm(i) => {
                if !self.chats[i].seen_ids.insert(id.clone()) {
                    return None;
                }
                let who = if mine {
                    "me".to_string()
                } else {
                    self.chats[i].label.clone()
                };
                self.chats[i]
                    .messages
                    .push(Msg::bot(id, who, body, mine, ok));
                Some(Target::Dm(i))
            }
            Target::Channel(w, c) => {
                if !self.workspaces[w].channels[c].seen_ids.insert(id.clone()) {
                    return None;
                }
                let who = if mine {
                    "me".to_string()
                } else {
                    npub_from_hex(sender_hex)
                        .map(|n| shorten(&n))
                        .unwrap_or_else(|_| sender_hex.to_owned())
                };
                self.workspaces[w].channels[c]
                    .messages
                    .push(Msg::bot(id, who, body, mine, ok));
                Some(Target::Channel(w, c))
            }
        }
    }

    /// Backfill each chat/channel buffer with persisted history (chat
    /// messages only, `kind == 9`), in chronological order. Records whose
    /// group matches neither a chat nor a (already-built, see
    /// `rebuild_workspaces`) workspace are skipped (v1) -- a live message
    /// will create the row later. Runs once before the event loop; `sync()`
    /// then only appends genuinely new / undelivered messages
    /// (already-delivered history is not re-surfaced by sync, so there is no
    /// duplication).
    pub fn load_history(&mut self, mut records: Vec<AppMessageRecord>) {
        // Sort by SEND time (`recorded_at`), not local ingest time
        // (`received_at`): MDK stamps received_at when the row is written
        // locally, so after an offline gap / cross-relay jitter a
        // late-delivered message would otherwise sort after ones actually said
        // later. received_at + message_id are stable tiebreaks (matches MDK's
        // own wn-tui ordering).
        records.sort_by(|a, b| {
            (a.recorded_at, a.received_at, a.message_id_hex.as_str()).cmp(&(
                b.recorded_at,
                b.received_at,
                b.message_id_hex.as_str(),
            ))
        });
        for r in records {
            match r.kind {
                CHAT_MESSAGE_KIND => {
                    let mine = r.direction == "sent";
                    // A join-request is a normal kind-9 DM whose content is the
                    // `invite::JoinRequest` envelope (content-based, not a
                    // separate kind) -- render a clean 🔑 line instead of the
                    // raw JSON as a chat bubble. Interactive approve-in-TUI is
                    // out of scope (v1); `moyu approve` stays the only path.
                    // Checked before `decode_channel_body` since a join-request
                    // is never itself workspace-enveloped.
                    if let Some(jr) = crate::invite::parse_join_request(&r.plaintext) {
                        if let Some(Target::Dm(i)) = self.resolve_route(&r.group_id_hex, "general")
                            && self.chats[i].seen_ids.insert(r.message_id_hex.clone())
                        {
                            // Name from the envelope's *gid*, never its
                            // attacker-controlled `ws_name`;
                            // unknown gid -> short gid, not the unverified claim.
                            let ws_label = self.workspace_name_by_gid(&jr.ws_gid_hex);
                            let chat = &mut self.chats[i];
                            let who = if mine {
                                "me".to_string()
                            } else {
                                chat.label.clone()
                            };
                            let line = match ws_label {
                                Some(name) => {
                                    format!("🔑 {who} 想加入 #{name} — CLI: moyu approve {who}")
                                }
                                None => {
                                    let gid8: String = jr.ws_gid_hex.chars().take(8).collect();
                                    format!(
                                        "🔑 {who} 想加入 群 {gid8}(未验证)— CLI: moyu approve {who}"
                                    )
                                }
                            };
                            chat.messages.push(Msg::chat(
                                r.message_id_hex.clone(),
                                who,
                                line,
                                mine,
                                None,
                            ));
                        }
                        continue;
                    }
                    // A `q` quote tag marks a threaded reply; its value is the
                    // parent's message id (rendered as a dim quote line).
                    let reply_to =
                        crate::domain::first_tag_value(&r.tags, crate::domain::QUOTE_REF_TAG)
                            .map(str::to_owned);
                    // Every kind-9 body goes through the channel envelope decode:
                    // a DM's plaintext was never enveloped (`send_current` sends
                    // DMs raw), so this is a no-op fallthrough to `(general,
                    // plaintext unchanged)` for them; a workspace post decodes to
                    // its real `(slug, body)`.
                    let (slug, body) = workspace::decode_channel_body(&r.plaintext);
                    let id = r.message_id_hex.clone();
                    match self.resolve_route(&r.group_id_hex, &slug) {
                        Some(Target::Dm(i)) => {
                            let chat = &mut self.chats[i];
                            if !chat.seen_ids.insert(r.message_id_hex) {
                                continue; // already have this message
                            }
                            let who = if mine {
                                "me".to_string()
                            } else {
                                chat.label.clone()
                            };
                            chat.messages.push(Msg::chat(id, who, body, mine, reply_to));
                        }
                        Some(Target::Channel(w, c)) => {
                            let buf = &mut self.workspaces[w].channels[c];
                            if !buf.seen_ids.insert(r.message_id_hex) {
                                continue; // already have this message
                            }
                            // Unlike a DM (one fixed peer -> the chat's own
                            // label), a workspace channel can carry messages from
                            // any member, so the sender is resolved per-message.
                            let who = if mine {
                                "me".to_string()
                            } else {
                                npub_from_hex(&r.sender)
                                    .map(|n| shorten(&n))
                                    .unwrap_or(r.sender)
                            };
                            buf.messages.push(Msg::chat(id, who, body, mine, reply_to));
                        }
                        None => {} // group not tracked yet -- dropped (v1 skip)
                    }
                }
                // Reactions/retractions fold onto the message they target rather
                // than becoming their own line (see `apply_reaction`).
                REACTION_KIND => {
                    let target =
                        crate::domain::first_tag_value(&r.tags, crate::domain::EVENT_REF_TAG)
                            .unwrap_or("");
                    self.apply_reaction(&r.group_id_hex, target, &r.plaintext, r.message_id_hex);
                }
                DELETE_KIND => {
                    if let Some(deleted) =
                        crate::domain::first_tag_value(&r.tags, crate::domain::EVENT_REF_TAG)
                    {
                        self.retract_reaction(deleted);
                    }
                }
                // Bot/agent events (kind-1201/1202) render as their own 🤖 line,
                // routed into the channel their payload names.
                AGENT_OPERATION_KIND | AGENT_ACTIVITY_KIND => {
                    let mine = r.direction == "sent";
                    self.push_bot_event(
                        &r.group_id_hex,
                        mine,
                        &r.sender,
                        r.message_id_hex.clone(),
                        &r.plaintext,
                        r.kind == AGENT_OPERATION_KIND,
                    );
                }
                _ => {} // edits / group-system: folded into the projection elsewhere
            }
        }
    }
}

/// Where a decoded `(group, slug)` pair routes: a DM `chats` index, or a
/// `(workspace, channel)` index pair into `workspaces`. Shared between
/// `Model::load_history` and `sync_tick` so history backfill and live sync
/// agree on exactly one routing rule.
enum Target {
    Dm(usize),
    Channel(usize, usize),
}

/// A position-independent identity of a selected row, used to keep `selected`
/// anchored to the same entity across a `rows()`-changing mutation. See
/// [`Model::selected_identity`] / [`Model::reselect`].
enum RowId {
    /// A DM, keyed by contact npub (stable even before its group is founded).
    Dm(String),
    /// A workspace header, keyed by the workspace's group-id hex.
    Header(String),
    /// A channel, keyed by `(workspace group-id hex, channel slug)`.
    Channel(String, String),
}

/// (Re)build `model.workspaces` from the live `crate::domain::all_workspaces`
/// projection (a group counts as a workspace once its control-message log
/// has named it). Called once at TUI startup (see `cmd_tui`) and again
/// whenever `sync_tick` sees a joined group or a `moyu.*` control message, so
/// the rail stays live without a restart.
///
/// **Preserves in-memory buffers**: every existing channel's `messages` /
/// `seen_ids` / `unread` is re-matched by `(group_id_hex, slug)` against the
/// outgoing tree before the new one is built. A channel the projection knows
/// is carried over; a **non-empty on-the-fly buffer** whose slug the
/// projection hasn't heard of yet (a message that raced ahead of its
/// `channel.create`/snapshot -- see `resolve_route`) is re-attached to its
/// workspace so its already-displayed lines survive until the create arrives. So a refresh never drops already-received
/// messages. Only two things are intentionally dropped: an empty on-the-fly
/// placeholder, and a channel the projection reports **archived** (no
/// archived-channel display is supported yet) -- archived slugs stay in the
/// projection's slug set, so they are never resurrected as on-the-fly buffers.
///
/// Selection is anchored across the rebuild by identity, not index.
pub fn rebuild_workspaces(
    model: &mut Model,
    client: &AppClient,
    engine: &MoyuEngine,
) -> anyhow::Result<()> {
    let prev = model.selected_identity();
    let (projected, private_channels) =
        crate::domain::membership_view(engine, client, &model.account_label)?;

    // Index the outgoing buffers so they survive the rebuild. Key each by its
    // ROUTING group hex -- a private channel by its OWN group, a public channel by
    // the workspace group -- so private buffers are preserved and never collide
    // with a public channel of the same slug.
    let mut old: HashMap<(String, String), ChannelBuf> = HashMap::new();
    for ws in model.workspaces.drain(..) {
        let ws_ghex = ws.group.to_string();
        for buf in ws.channels {
            let ghex = buf
                .group
                .as_ref()
                .map(|g| g.to_string())
                .unwrap_or_else(|| ws_ghex.clone());
            old.insert((ghex, buf.slug.clone()), buf);
        }
    }

    // Per still-present workspace group: its index in the new `workspaces`, and
    // the full set of slugs the projection is aware of (archived included) so
    // an intentionally-archived channel is never re-attached as "on-the-fly".
    let mut idx_of: HashMap<String, usize> = HashMap::new();
    let mut known_slugs: HashMap<String, HashSet<String>> = HashMap::new();

    let mut workspaces = Vec::with_capacity(projected.len());
    for (group_id_hex, proj) in &projected {
        let Ok(group) = crate::domain::group_id_from_hex(group_id_hex) else {
            // Corrupt on-disk hex -- skip this workspace rather than panic;
            // it will simply be missing from the rail until the store heals.
            continue;
        };
        // Workspace names come off the governance state, set by any admin --
        // sanitize on the way into the model, see `Model::from_store`.
        let name = crate::output::term_safe(proj.name.as_deref().unwrap_or_default()).into_owned();
        let slugs: HashSet<String> = proj.channels.keys().cloned().collect();
        let channels = proj
            .ordered_channels()
            .into_iter()
            .filter(|c| !c.archived)
            .map(|c| {
                let key = (group_id_hex.clone(), c.slug.clone());
                match old.remove(&key) {
                    Some(mut buf) => {
                        buf.archived = c.archived;
                        buf
                    }
                    None => ChannelBuf {
                        slug: c.slug.clone(),
                        archived: c.archived,
                        group: None,
                        messages: Vec::new(),
                        seen_ids: HashSet::new(),
                        unread: 0,
                    },
                }
            })
            .collect();
        idx_of.insert(group_id_hex.clone(), workspaces.len());
        known_slugs.insert(group_id_hex.clone(), slugs);
        workspaces.push(WorkspaceEntry {
            group,
            name,
            channels,
        });
    }

    // Nest private channels (🔒) under their parent workspace. Each is a SEPARATE
    // MLS group (its own epoch key), so its buffer carries `group: Some(..)` and
    // routes to itself (see `resolve_route` / `channel_group`). Its buffer is
    // preserved across the rebuild via the (own-group, slug) key. Orphans -- a
    // private channel whose parent workspace we are NOT a member of (rare D5
    // drift) -- are intentionally not shown in the tree for now (a documented M3
    // follow-up; they are equally unreachable via the CLI `post <ws>` path).
    let ws_hexes: BTreeSet<String> = idx_of.keys().cloned().collect();
    let nested = workspace::nest_private_channels(private_channels, &ws_hexes);
    for (parent_hex, pcs) in nested.by_parent {
        let Some(&wi) = idx_of.get(&parent_hex) else {
            continue;
        };
        for pc in pcs {
            let Ok(group) = crate::domain::group_id_from_hex(&pc.group_id_hex) else {
                continue; // corrupt hex -- skip rather than panic
            };
            let key = (pc.group_id_hex.clone(), pc.slug.clone());
            let buf = match old.remove(&key) {
                Some(mut buf) => {
                    buf.group = Some(group);
                    buf
                }
                None => ChannelBuf {
                    slug: pc.slug.clone(),
                    archived: false,
                    group: Some(group),
                    messages: Vec::new(),
                    seen_ids: HashSet::new(),
                    unread: 0,
                },
            };
            workspaces[wi].channels.push(buf);
        }
    }

    // Re-attach surviving on-the-fly buffers: a buffer carrying messages
    // whose slug the projection has not yet heard of, belonging to a
    // still-present workspace, is appended so its lines aren't lost until the
    // channel.create/snapshot folds it into the ordered list on a later
    // rebuild. Empty placeholders and archived (i.e. projection-known) slugs
    // are left to drop.
    for ((ghex, slug), buf) in old {
        if buf.messages.is_empty() {
            continue;
        }
        let Some(&wi) = idx_of.get(&ghex) else {
            continue; // its workspace is gone (left/removed)
        };
        if known_slugs
            .get(&ghex)
            .is_some_and(|slugs| slugs.contains(&slug))
        {
            continue; // a known (e.g. archived) channel, not on-the-fly
        }
        workspaces[wi].channels.push(buf);
    }

    model.workspaces = workspaces;
    model.reselect(prev);
    Ok(())
}

/// Run the TUI to completion. Owns `client`/`store` for the session.
///
/// The event loop is split into [`event_loop`] so the terminal is *always*
/// restored (raw mode + alt-screen off) however the loop ends — a clean quit,
/// or an error propagated out of a `terminal.draw(...)?`. The panic hook covers
/// the panic path; this covers the `Err` path.
pub async fn run(
    client: AppClient,
    store: MoyuStore,
    model: Model,
    engine: MoyuEngine,
) -> anyhow::Result<()> {
    let mut terminal = init_terminal()?;
    install_panic_hook();
    let result = event_loop(&mut terminal, client, store, model, engine).await;
    let restored = restore_terminal();
    // The loop's error (if any) is the interesting one; otherwise surface a
    // restore failure.
    result.and(restored)
}

async fn event_loop(
    terminal: &mut Term,
    mut client: AppClient,
    mut store: MoyuStore,
    mut model: Model,
    mut engine: MoyuEngine,
) -> anyhow::Result<()> {
    // crossterm read() is blocking; read on an OS thread and forward events.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Catch up on anything already waiting before the first render.
    if let Err(e) = sync_tick(&mut client, &mut store, &mut model, &mut engine).await {
        model.status = format!("sync error: {e}");
    }
    terminal.draw(|f| render(f, &model))?;

    // `sync_tick` never checks KeyPackage freshness, and a `tui` session can
    // be left open for weeks -- only the one-shot check `open_session` does
    // before entering this loop would otherwise ever run. Re-check on the
    // poll tick, throttled to `KEYPACKAGE_RECHECK` (raw mode is active here,
    // so the outcome goes into `model.status`, never stderr).
    let mut last_keypackage_check = Instant::now();

    loop {
        tokio::select! {
            maybe_ev = rx.recv() => match maybe_ev {
                Some(ev) => match handle_event(ev, &mut client, &mut store, &mut model).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(e) => model.status = format!("error: {e}"),
                },
                None => return Ok(()),
            },
            _ = poll.tick() => {
                if let Err(e) = sync_tick(&mut client, &mut store, &mut model, &mut engine).await {
                    model.status = format!("sync error: {e}");
                }
                let now = Instant::now();
                if crate::domain::keypackage_recheck_due(last_keypackage_check, now) {
                    last_keypackage_check = now;
                    match moyu_core::keypackage_rotation::ensure_fresh(&mut client, &mut store).await {
                        Ok(true) => model.status = "rotated an aging KeyPackage".into(),
                        Ok(false) => {}
                        Err(e) => model.status = format!("KeyPackage freshness check failed: {e}"),
                    }
                }
            }
        }
        terminal.draw(|f| render(f, &model))?;
    }
}

/// Returns `Ok(true)` to quit.
async fn handle_event(
    ev: Event,
    client: &mut AppClient,
    store: &mut MoyuStore,
    model: &mut Model,
) -> anyhow::Result<bool> {
    let Event::Key(key) = ev else {
        return Ok(false);
    };
    // crossterm delivers Press+Release on some platforms; act on Press only.
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    // Ctrl-C always quits, regardless of input focus or an open overlay.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    // While the roster overlay is open it is MODAL: Enter or Esc dismiss
    // it; every other key is swallowed so it can't send a message or move the
    // selection behind the popup. (Ctrl-C above still quits.)
    if model.roster.is_some() {
        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            model.roster = None;
        }
        return Ok(false);
    }

    // Ctrl-R opens the roster for the selected row's group -- modifier-gated
    // like Ctrl-C, since a bare 'r' must still be captured as input text.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
        if let Err(e) = open_roster(client, model) {
            model.status = format!("roster failed: {e}");
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Up => move_selection(model, -1),
        KeyCode::Down => move_selection(model, 1),
        KeyCode::Enter => send_current(client, store, model).await?,
        KeyCode::Backspace => {
            model.input.pop();
        }
        // No overlay is open here (the modal branch above returned early), so
        // Esc clears the input line.
        KeyCode::Esc => model.input.clear(),
        KeyCode::Char(c) => model.input.push(c),
        _ => {}
    }
    Ok(false)
}

/// Resolve the selected row's `GroupId`, fetch its members over MDK, and open
/// the roster overlay with their npubs (the local account is excluded,
/// matching `ensure_chat_for_group`'s "the peer" framing -- a roster is about
/// who else is in the room).
fn open_roster(client: &AppClient, model: &mut Model) -> anyhow::Result<()> {
    let Some(row) = model.selected_row() else {
        model.status = "no chat selected".into();
        return Ok(());
    };
    let gid = match row {
        Row::Dm(i) => match model.chats.get(i).and_then(|c| c.group.clone()) {
            Some(g) => g,
            None => {
                model.status = "no group yet -- send a message first".into();
                return Ok(());
            }
        },
        // A private channel's roster is its OWN group's members (a subset of the
        // workspace); a public channel / header shows the workspace roster.
        Row::Channel(w, c) => model.channel_group(w, c),
        Row::WorkspaceHeader(w) => model.workspaces[w].group.clone(),
    };
    // Badge admins using the admin set snapshotted by the last `sync_tick`
    // (`AppClient` alone can't surface `admin_policy`; a workspace not yet in the
    // snapshot -- or a DM, which never is -- simply shows no badges). Order
    // admins first, then by id (`governance::member_roles`).
    let admins: Vec<String> = model
        .prev_rosters
        .get(&gid.to_string())
        .map(|w| w.snap.admins.iter().cloned().collect())
        .unwrap_or_default();
    let member_hexes: Vec<String> = client
        .members(&gid)?
        .into_iter()
        .filter(|m| !m.local)
        .map(|m| m.member_id_hex)
        .collect();
    let rows = moyu_core::governance::member_roles(&member_hexes, &admins)
        .into_iter()
        .map(|r| {
            let npub = npub_from_hex(&r.member_id_hex)
                .map(|n| shorten(&n))
                .unwrap_or_else(|_| shorten(&r.member_id_hex));
            if r.is_admin {
                format!("{npub} (admin)")
            } else {
                npub
            }
        })
        .collect::<Vec<_>>();
    model.roster = Some(rows);
    model.status = "roster".into();
    Ok(())
}

fn move_selection(model: &mut Model, delta: i32) {
    let rows = model.rows();
    if rows.is_empty() {
        return;
    }
    let n = rows.len() as i32;
    let next = (model.selected as i32 + delta).rem_euclid(n) as usize;
    model.selected = next;
    // Opening a row clears its own unread count; a workspace header is an
    // inert section label with no buffer of its own, so it's a no-op.
    match rows[next] {
        Row::Dm(i) => model.chats[i].unread = 0,
        Row::Channel(w, c) => model.workspaces[w].channels[c].unread = 0,
        Row::WorkspaceHeader(_) => {}
    }
}

/// Send the current input to the selected row. A `Row::Dm` sends raw
/// plaintext, founding the 1:1 group on first send if it does not exist yet
/// (unchanged from v1); a `Row::Channel` (or a `Row::WorkspaceHeader`, which
/// targets that workspace's `#general`) channel-envelopes it and sends it
/// into the workspace's one MLS group.
async fn send_current(
    client: &mut AppClient,
    store: &mut MoyuStore,
    model: &mut Model,
) -> anyhow::Result<()> {
    let text = model.input.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }
    let Some(row) = model.selected_row() else {
        model.status = "no chat selected".into();
        return Ok(());
    };

    match row {
        Row::Dm(idx) => {
            let group = match model.chats[idx].group.clone() {
                Some(g) => g,
                None => {
                    let npub = model.chats[idx].npub.clone();
                    model.status = "founding 1:1 group…".into();
                    let g = client.create_group(DM_GROUP_NAME, &[npub.as_str()]).await?;
                    // Record the founded group in memory FIRST: a real MLS
                    // group now exists (its Welcome is already sent to the
                    // peer). If the store write below fails, the model still
                    // knows the group, so the next send retries into it
                    // instead of founding a second, orphaned one.
                    model.chats[idx].group = Some(g.clone());
                    store.set_contact_group_id(&npub, g.to_string())?;
                    g
                }
            };
            let summary = client.send(&group, text.as_bytes()).await?;
            let id = summary.message_ids.first().cloned().unwrap_or_default();
            model.chats[idx]
                .messages
                .push(Msg::chat(id, "me".into(), text, true, None));
        }
        Row::Channel(w, c) => send_to_channel(client, model, w, c, text).await?,
        Row::WorkspaceHeader(w) => {
            // Posting from the header itself targets that workspace's
            // `#general` matching where the pane already
            // displays for a selected header (`selected_pane_content`).
            let Some(c) = model.workspaces[w]
                .channels
                .iter()
                .position(|c| c.slug == DEFAULT_CHANNEL_SLUG)
            else {
                model.status = "workspace has no #general channel".into();
                return Ok(());
            };
            send_to_channel(client, model, w, c, text).await?;
        }
    }
    model.input.clear();
    model.status = "sent".into();
    Ok(())
}

/// Send `text` into channel `c` of workspace `w` and echo it into that
/// channel's buffer. A private channel routes to its own MLS group; a public
/// one to the workspace group (see `channel_group`).
async fn send_to_channel(
    client: &mut AppClient,
    model: &mut Model,
    w: usize,
    c: usize,
    text: String,
) -> anyhow::Result<()> {
    let group = model.channel_group(w, c);
    let slug = model.workspaces[w].channels[c].slug.clone();
    let payload = workspace::encode_channel_body(&slug, &text);
    let summary = client.send(&group, payload.as_bytes()).await?;
    let id = summary.message_ids.first().cloned().unwrap_or_default();
    model.workspaces[w].channels[c]
        .messages
        .push(Msg::chat(id, "me".into(), text, true, None));
    Ok(())
}

/// Drain the bounded `sync()`: refresh the workspace tree if this batch
/// changed its shape, accept any new invites, and route inbound messages
/// into their chat/channel buffer.
async fn sync_tick(
    client: &mut AppClient,
    store: &mut MoyuStore,
    model: &mut Model,
    engine: &mut MoyuEngine,
) -> anyhow::Result<()> {
    let summary = client.sync().await?;

    // Apply peer handshake commits (member add/remove, admin promote/demote)
    // that `sync()` only *buffers* into MDK's convergence subsystem -- see
    // `crate::domain::drive_convergence`. The roster overlay (`Ctrl-R`) reads live
    // membership, so this alone surfaces governance changes there; any published
    // follow-on also forces the workspace-tree refresh below.
    let converged = crate::domain::drive_convergence(engine, client, &model.account_label).await;

    // Governance diff: compare the roster/admin snapshot against last tick's and
    // surface any kick / leave / promote / demote / peer-join in the status line
    // (the `Ctrl-R` overlay re-reads live members on open, so it needs no push).
    // These reach an observer ONLY via convergence, whose `SendSummary` has no
    // events, so a snapshot diff is the only signal -- same rationale, and the
    // same shared helpers, as the CLI `recv` daemon. A failed snapshot read
    // (`None`) is skipped whole: don't diff a degraded map (it would fabricate
    // departures/demotions) and keep the good `prev_rosters` baseline.
    let mut peer_joined = false;
    if let Some(after_rosters) =
        crate::domain::snapshot_rosters(engine, client, &model.account_label)
    {
        let gov_lines = crate::domain::governance_diff(&model.prev_rosters, &after_rosters);
        peer_joined = crate::domain::any_peer_joined(&gov_lines);
        if !gov_lines.is_empty() {
            model.status = gov_status(&gov_lines);
        }
        model.prev_rosters = after_rosters;
    }

    // Anchor the selection to its entity across every structural change this
    // tick makes (rebuild, a new DM chat pushed, an on-the-fly channel
    // appended) -- restored at the end so `selected` never silently slides to
    // a different chat/channel and mis-target the next Enter.
    let prev_selected = model.selected_identity();

    // A newly joined group or a `moyu.*` control event (channel
    // create/rename/archive, workspace rename/snapshot) can change the
    // workspace/channel tree's shape. `client.sync()` above already
    // persisted both to the local store by the time it returned, so
    // rebuilding *now* -- before accepting invites or routing messages --
    // picks up this very tick's changes, and does so before the
    // `ensure_chat_for_group` fallback below has a chance to misfile a
    // workspace peer as a bogus 1:1 DM contact. Skipped on the common tick
    // that carries neither (a full history rescan isn't free).
    let should_refresh_workspaces = converged
        || !summary.joined_groups.is_empty()
        || summary.messages.iter().any(|m| m.kind == KIND_GROUP_SYSTEM);
    if should_refresh_workspaces && let Err(e) = rebuild_workspaces(model, client, engine) {
        model.status = format!("workspace refresh failed: {e}");
    }

    // Group hexes whose MDK profile name is the DM sentinel -- the ONLY groups
    // that may be adopted as a 1:1 DM (belt-and-suspenders). Read from the
    // same local projection the rebuild used; reflects this tick's joins since
    // `sync()` persisted them.
    let dm_groups = crate::domain::dm_group_hexes(engine, &model.account_label);

    for group in &summary.joined_groups {
        if let Err(e) = client.accept_group_invite(group) {
            model.status = format!("accept invite failed: {e}");
            continue;
        }
        // Only adopt a joined group as a DM chat row when it is NOT already a
        // known workspace AND its profile name is the DM sentinel. Either
        // guard alone closes the hole; both together are defense in depth against a
        // workspace invite (`workspace add`) whose name control message hasn't
        // synced yet being persisted as a bogus 1:1 contact.
        let ghex = group.to_string();
        let is_workspace = model
            .workspaces
            .iter()
            .any(|ws| ws.group.to_string() == ghex);
        if is_workspace || !dm_groups.contains(&ghex) {
            continue;
        }
        if let Err(e) = ensure_chat_for_group(client, store, model, group) {
            model.status = format!("track group failed: {e}");
        }
    }

    for msg in &summary.messages {
        route_incoming(client, store, model, msg, &dm_groups);
    }

    // §5.2 fallback (same driver + gate the CLI recv daemon uses): re-broadcast
    // catch-up snapshots for any workspace whose join isn't yet covered when a
    // member was added. Over the wire an existing member observes the join as a
    // roster-diff `Joined` (`peer_joined`), NOT as a `SyncSummary.events`
    // MemberAdded (`saw_member_added`, which only fires on the local same-tick
    // apply) -- trigger on either. Gated so ordinary ticks pay nothing; our own
    // snapshot is `moyu.workspace.snapshot`, not a member-add, so it never feeds
    // back.
    if crate::domain::saw_member_added(&summary) || peer_joined {
        let sent =
            crate::domain::broadcast_catch_up_snapshots(engine, client, &model.account_label).await;
        if sent > 0 {
            model.status = format!("re-broadcast {sent} workspace snapshot(s) for new member(s)");
        }
    }

    model.reselect(prev_selected);
    Ok(())
}

/// Route one already-decrypted `ReceivedMessage` from a `sync()` batch into
/// its chat/channel buffer. Chat messages only (`kind == 9`), matching the
/// history backfill -- reactions/edits/deletes/group-system (1210) are
/// skipped (the workspace tree refresh in `sync_tick` is what a 1210 control
/// event actually triggers).
fn route_incoming(
    client: &AppClient,
    store: &mut MoyuStore,
    model: &mut Model,
    msg: &ReceivedMessage,
    dm_groups: &HashSet<String>,
) {
    let ghex = msg.group_id.to_string();
    // Reactions/retractions fold onto the message they target (kind-7/kind-5),
    // not into a chat line; other non-chat kinds are ignored here.
    match msg.kind {
        REACTION_KIND => {
            let target = crate::domain::first_tag_value(&msg.tags, crate::domain::EVENT_REF_TAG)
                .unwrap_or("");
            model.apply_reaction(&ghex, target, &msg.plaintext, msg.message_id_hex.clone());
            return;
        }
        DELETE_KIND => {
            if let Some(deleted) =
                crate::domain::first_tag_value(&msg.tags, crate::domain::EVENT_REF_TAG)
            {
                model.retract_reaction(deleted);
            }
            return;
        }
        CHAT_MESSAGE_KIND => {}
        // Live bot/agent event (kind-1201/1202): place it as a 🤖 line and bump
        // unread on a channel that isn't currently selected.
        AGENT_OPERATION_KIND | AGENT_ACTIVITY_KIND => {
            match model.push_bot_event(
                &ghex,
                false,
                &msg.sender,
                msg.message_id_hex.clone(),
                &msg.plaintext,
                msg.kind == AGENT_OPERATION_KIND,
            ) {
                Some(Target::Dm(i)) if i != model.selected => model.chats[i].unread += 1,
                Some(Target::Channel(w, c)) => bump_channel_unread_if_unselected(model, w, c),
                _ => {}
            }
            return;
        }
        _ => return,
    }
    // join-request DM (content-based, mirrors `Model::load_history`): render a
    // clean 🔑 line rather than the raw JSON as a chat bubble. Interactive
    // approve-in-TUI is out of scope (v1); `moyu approve` stays the only path.
    // Checked before `decode_channel_body` since a join-request is never
    // itself workspace-enveloped.
    if let Some(jr) = crate::invite::parse_join_request(&msg.plaintext) {
        let mut target = model.resolve_route(&ghex, "general");
        if target.is_none() && dm_groups.contains(&ghex) {
            if let Err(e) = ensure_chat_for_group(client, store, model, &msg.group_id) {
                model.status = format!("track group failed: {e}");
            }
            target = model.resolve_route(&ghex, "general");
        }
        if let Some(Target::Dm(i)) = target
            && model.chats[i].seen_ids.insert(msg.message_id_hex.clone())
        {
            // Name from the envelope's *gid*, never its attacker-controlled
            // `ws_name`; unknown gid -> short gid, not the claim.
            let ws_label = model.workspace_name_by_gid(&jr.ws_gid_hex);
            let who = model.chats[i].label.clone();
            let line = match ws_label {
                Some(name) => format!("🔑 {who} 想加入 #{name} — CLI: moyu approve {who}"),
                None => {
                    let gid8: String = jr.ws_gid_hex.chars().take(8).collect();
                    format!("🔑 {who} 想加入 群 {gid8}(未验证)— CLI: moyu approve {who}")
                }
            };
            model.chats[i].messages.push(Msg::chat(
                msg.message_id_hex.clone(),
                who,
                line,
                false,
                None,
            ));
            if i != model.selected {
                model.chats[i].unread += 1;
            }
        }
        return;
    }
    // A `q` quote tag marks a threaded reply; its value is the parent's id.
    let reply_to =
        crate::domain::first_tag_value(&msg.tags, crate::domain::QUOTE_REF_TAG).map(str::to_owned);
    // Every kind-9 body goes through the channel envelope decode (see
    // `Model::load_history`'s matching comment) -- a DM's raw plaintext falls
    // through to `(general, plaintext unchanged)`.
    let (slug, body) = workspace::decode_channel_body(&msg.plaintext);

    let mut target = model.resolve_route(&ghex, &slug);
    if target.is_none() && dm_groups.contains(&ghex) {
        // A message for an untracked group whose profile name IS the DM
        // sentinel -- adopt it as a DM (a message racing ahead of the
        // `joined_groups` handling in `sync_tick`). A non-"dm" (workspace)
        // group is never fabricated into a DM here; if its tree isn't
        // built yet the message falls through to the drop below and the next
        // refresh tick will place it.
        if let Err(e) = ensure_chat_for_group(client, store, model, &msg.group_id) {
            model.status = format!("track group failed: {e}");
        }
        target = model.resolve_route(&ghex, &slug);
    }
    match target {
        Some(Target::Dm(i)) => {
            // Dedup by message id so a message already shown from history
            // (or re-ingested past MDK's seen-event cap) is not appended
            // twice.
            if !model.chats[i].seen_ids.insert(msg.message_id_hex.clone()) {
                return;
            }
            // 1:1 chat: the peer is the chat itself, so label received lines
            // with the chat's label -- reads the same as backfilled history.
            // (Assumes the 1:1 has exactly its two members; a future
            // multi-member group would misattribute -- acceptable for v1.)
            let who = model.chats[i].label.clone();
            model.chats[i].messages.push(Msg::chat(
                msg.message_id_hex.clone(),
                who,
                body,
                false,
                reply_to,
            ));
            if i != model.selected {
                model.chats[i].unread += 1;
            }
        }
        Some(Target::Channel(w, c)) => {
            if !model.workspaces[w].channels[c]
                .seen_ids
                .insert(msg.message_id_hex.clone())
            {
                return;
            }
            // A workspace channel has no single fixed peer, so (unlike a DM)
            // the sender must be resolved per-message.
            let who = npub_from_hex(&msg.sender)
                .map(|n| shorten(&n))
                .unwrap_or_else(|_| msg.sender.clone());
            model.workspaces[w].channels[c].messages.push(Msg::chat(
                msg.message_id_hex.clone(),
                who,
                body,
                false,
                reply_to,
            ));
            bump_channel_unread_if_unselected(model, w, c);
        }
        // Couldn't place the message (unknown group with no reachable peer,
        // or ensure failed just above). Surface it rather than dropping it
        // invisibly.
        None => model.status = format!("dropped a message for group {}…", shorten(&ghex)),
    }
}

/// Ensure there is a chat row bound to `group`, deriving the peer from the
/// group's non-local member and persisting the contact<->group link.
fn ensure_chat_for_group(
    client: &AppClient,
    store: &mut MoyuStore,
    model: &mut Model,
    group: &GroupId,
) -> anyhow::Result<()> {
    let ghex = group.to_string();
    if model
        .chats
        .iter()
        .any(|c| c.group_hex().as_deref() == Some(ghex.as_str()))
    {
        return Ok(());
    }
    // 1:1 group: a single remote member.
    let Some(member) = client.members(group)?.into_iter().find(|m| !m.local) else {
        // No non-local member to bind a chat to -- unusual for a 1:1. Note it
        // instead of silently doing nothing.
        model.status = "joined a group with no reachable peer".into();
        return Ok(());
    };
    let npub = moyu_core::identity::npub_from_hex(&member.member_id_hex)?;
    let label = store
        .contacts()
        .iter()
        .find(|c| c.npub == npub)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| crate::domain::default_contact_label(&npub));

    match store.find_contact(&npub) {
        Some(_) => store.set_contact_group_id(&npub, ghex.clone())?,
        None => store.upsert_contact(Contact {
            npub: npub.clone(),
            nip05: None,
            label: label.clone(),
            group_id_hex: Some(ghex),
        })?,
    }
    model.chats.push(Chat {
        // `label` may be an existing contact's stored label (itself possibly
        // peer-chosen) -- sanitize on the way into the model, see
        // `Model::from_store`.
        label: crate::output::term_safe(&label).into_owned(),
        npub,
        group: Some(group.clone()),
        messages: Vec::new(),
        unread: 0,
        seen_ids: HashSet::new(),
    });
    Ok(())
}

/// Bump a channel's unread count unless it is the currently selected row.
fn bump_channel_unread_if_unselected(model: &mut Model, w: usize, c: usize) {
    let is_selected = matches!(
        model.selected_row(),
        Some(Row::Channel(sw, sc)) if sw == w && sc == c
    );
    if !is_selected {
        model.workspaces[w].channels[c].unread += 1;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, model: &Model) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    let body = root[0];
    let status = root[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(1)])
        .split(body);

    render_chat_list(f, model, cols[0]);
    render_chat_pane(f, model, cols[1]);
    render_status(f, model, status);

    if let Some(npubs) = &model.roster {
        render_roster(f, npubs, f.area());
    }
}

fn render_chat_list(f: &mut Frame, model: &Model, area: Rect) {
    let rows = model.rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            Row::Dm(i) => {
                let c = &model.chats[*i];
                let mut spans = vec![Span::raw(c.label.clone())];
                if c.unread > 0 {
                    spans.push(Span::styled(
                        format!("  ({})", c.unread),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
            Row::WorkspaceHeader(w) => {
                let ws = &model.workspaces[*w];
                ListItem::new(Line::from(Span::styled(
                    ws.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )))
            }
            Row::Channel(w, c) => {
                let ch = &model.workspaces[*w].channels[*c];
                // 🔒 marks a private channel (its own MLS group); public channels
                // are plain `#slug`.
                let label = if ch.is_private() {
                    format!("  🔒 #{}", ch.slug)
                } else {
                    format!("  #{}", ch.slug)
                };
                let mut spans = vec![Span::raw(label)];
                if ch.unread > 0 {
                    spans.push(Span::styled(
                        format!("  ({})", ch.unread),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Chats "))
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(model.selected.min(rows.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// Resolve the selected `Row` to what `render_chat_pane` needs: the header
/// line's spans, and the message buffer to render underneath it. `None` when
/// nothing is selected (an empty rail -- no chats and no workspaces yet).
/// `Row::WorkspaceHeader` (an inert section label -- see [`Row`]) falls
/// through to that workspace's `#general` channel, matching where `Enter`
/// actually posts (see `send_current`) so the pane never contradicts send.
fn selected_pane_content(model: &Model) -> Option<(Vec<Span<'static>>, &[Msg])> {
    match model.selected_row()? {
        Row::Dm(i) => {
            let chat = model.chats.get(i)?;
            let spans = vec![
                Span::styled(
                    chat.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(shorten(&chat.npub), Style::default().fg(Color::DarkGray)),
                Span::raw(if chat.group.is_none() {
                    "  (no chat yet — press Enter to start)"
                } else {
                    ""
                }),
            ];
            Some((spans, chat.messages.as_slice()))
        }
        Row::Channel(w, c) => {
            let ws = model.workspaces.get(w)?;
            let ch = ws.channels.get(c)?;
            Some((channel_header(&ws.name, &ch.slug), ch.messages.as_slice()))
        }
        Row::WorkspaceHeader(w) => {
            let ws = model.workspaces.get(w)?;
            match ws.channels.iter().find(|c| c.slug == DEFAULT_CHANNEL_SLUG) {
                Some(ch) => Some((channel_header(&ws.name, &ch.slug), ch.messages.as_slice())),
                // #general archived/absent: show the workspace name + a note
                // rather than returning None, which would fall through to the
                // global "No chats yet" hint even though a workspace IS
                // selected. The empty slice renders an empty message box.
                None => {
                    let spans = vec![
                        Span::styled(
                            ws.name.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            "(no channels — all archived)".to_string(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];
                    Some((spans, &[]))
                }
            }
        }
    }
}

fn channel_header(workspace_name: &str, slug: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            workspace_name.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("#{slug}"), Style::default().fg(Color::DarkGray)),
    ]
}

/// Left indent that aligns a continuation line (reply quote, reaction pills)
/// under the message body, past the `{:>8} ` sender column (8 + 1 space).
const MSG_BODY_INDENT: &str = "         ";

/// The one-to-three display lines for a message: an optional dim reply-quote
/// line above it, the message line itself, then an optional dim reaction-pill
/// line below it. `all` is the same buffer, used to resolve a reply's parent to
/// a short quote (falling back to the parent id when it isn't buffered).
fn message_lines(m: &Msg, all: &[Msg]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);

    // A bot/agent line (kind-1201/1202) renders distinctly: a 🤖 gutter,
    // the sender, and a status-colored body (green ✓ / red ✗ / magenta when the
    // status carries no ok). No reply quote or reaction pills in v1.
    if let Some(bot) = &m.bot {
        let color = match bot.ok {
            Some(true) => Color::Green,
            Some(false) => Color::Red,
            None => Color::Magenta,
        };
        let head = Style::default().fg(color).add_modifier(Modifier::BOLD);
        let lines: Vec<&str> = if m.text.is_empty() {
            vec![""]
        } else {
            m.text.lines().collect()
        };
        for (i, line) in lines.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if i == 0 {
                spans.push(Span::styled(format!("{:>8} ", "🤖"), head));
                spans.push(Span::styled(format!("{} ", m.who.trim()), head));
            } else {
                spans.push(Span::raw(MSG_BODY_INDENT));
            }
            spans.push(Span::styled((*line).to_owned(), Style::default().fg(color)));
            out.push(Line::from(spans));
        }
        return out;
    }

    if let Some(pid) = &m.reply_to {
        let quote = match all.iter().find(|x| &x.id == pid) {
            Some(p) => format!("↩ {}: {}", p.who.trim(), quote_snippet(&p.text)),
            None => format!("↩ {}…", pid.chars().take(8).collect::<String>()),
        };
        out.push(Line::from(Span::styled(
            format!("{MSG_BODY_INDENT}{quote}"),
            dim,
        )));
    }

    let who_style = if m.mine {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    // The body renders as one visual line per source line (so embedded newlines
    // and fenced ```code``` blocks lay out correctly): the sender label leads
    // the first line, continuations are indented, and lines inside a code fence
    // get a gutter + lightweight syntax highlighting.
    let who_span = Span::styled(format!("{:>8} ", m.who), who_style);
    for (i, mut spans) in body_segments(&m.text).into_iter().enumerate() {
        let mut line_spans = Vec::with_capacity(spans.len() + 1);
        line_spans.push(if i == 0 {
            who_span.clone()
        } else {
            Span::raw(MSG_BODY_INDENT)
        });
        line_spans.append(&mut spans);
        out.push(Line::from(line_spans));
    }

    if !m.reactions.is_empty() {
        let pills = m
            .reactions
            .iter()
            .map(|(emoji, count)| format!("{emoji} {count}"))
            .collect::<Vec<_>>()
            .join("  ");
        out.push(Line::from(Span::styled(
            format!("{MSG_BODY_INDENT}{pills}"),
            dim,
        )));
    }
    out
}

/// A one-line, length-bounded preview of a message, for a reply quote. Uses the
/// first line only, truncated to 48 chars, with an ellipsis when it was cut.
fn quote_snippet(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("");
    let s: String = first_line.chars().take(48).collect();
    if first_line.chars().count() > 48 || text.lines().nth(1).is_some() {
        format!("{s}…")
    } else {
        s
    }
}

/// Common keywords highlighted inside code blocks — a deliberately broad,
/// language-agnostic set (C-like, Rust, Python, Go, JS, …). A few are keywords
/// in one language and plain identifiers in another; over-coloring an
/// identifier is a cosmetic miss, not a correctness issue — the right trade for
/// a dependency-free highlighter (vs. pulling in a full syntax-definition
/// engine like syntect for a terminal chat).
const CODE_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "begin",
    "bool",
    "break",
    "case",
    "catch",
    "chan",
    "char",
    "class",
    "const",
    "continue",
    "def",
    "default",
    "defer",
    "do",
    "dyn",
    "elif",
    "else",
    "end",
    "enum",
    "except",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "fn",
    "for",
    "from",
    "func",
    "function",
    "go",
    "if",
    "impl",
    "implements",
    "import",
    "in",
    "int",
    "interface",
    "lambda",
    "let",
    "loop",
    "map",
    "match",
    "mod",
    "module",
    "move",
    "mut",
    "new",
    "nil",
    "none",
    "null",
    "package",
    "pass",
    "priv",
    "private",
    "protected",
    "pub",
    "public",
    "raise",
    "range",
    "ref",
    "require",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "switch",
    "then",
    "this",
    "throw",
    "throws",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "use",
    "var",
    "void",
    "where",
    "while",
    "with",
    "yield",
];

/// The line-comment marker for a fenced block's language: `#` for
/// Python/shell/Ruby/YAML/TOML, `--` for SQL/Lua/Haskell, `;` for Lisps, `//`
/// otherwise (Rust/C/Go/JS/…). Language-aware so Rust's `#[attr]` isn't
/// mistaken for a comment.
fn comment_prefix(lang: &str) -> &'static str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "py" | "python" | "sh" | "bash" | "shell" | "zsh" | "rb" | "ruby" | "yaml" | "yml"
        | "toml" | "ini" | "conf" | "r" | "perl" | "pl" | "makefile" | "make" | "dockerfile" => "#",
        "sql" | "lua" | "hs" | "haskell" | "ada" | "elm" => "--",
        "lisp" | "clojure" | "clj" | "scheme" | "el" | "elisp" => ";",
        _ => "//",
    }
}

/// True if the chars starting at `i` spell out `marker`.
fn starts_with_at(chars: &[char], i: usize, marker: &str) -> bool {
    marker
        .chars()
        .enumerate()
        .all(|(k, mc)| chars.get(i + k) == Some(&mc))
}

/// Flush any pending unstyled text into `spans` as a plain span.
fn flush_plain(buf: &mut String, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::raw(std::mem::take(buf)));
    }
}

/// Lightweight, dependency-free syntax highlighting for one code line: line
/// comments dim, string literals yellow, numbers cyan, common keywords magenta,
/// everything else plain. A single-pass char scanner — good enough to make code
/// readable in the TUI without a full syntax-definition engine.
fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let string_style = Style::default().fg(Color::Yellow);
    let number_style = Style::default().fg(Color::Cyan);
    let keyword_style = Style::default().fg(Color::Magenta);
    let comment = comment_prefix(lang);

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        // A line comment runs to end-of-line.
        if starts_with_at(&chars, i, comment) {
            flush_plain(&mut buf, &mut spans);
            spans.push(Span::styled(chars[i..].iter().collect::<String>(), dim));
            return spans;
        }
        let c = chars[i];
        // String literal: up to the matching (unescaped) quote.
        if c == '"' || c == '\'' || c == '`' {
            flush_plain(&mut buf, &mut spans);
            let mut s = String::new();
            s.push(c);
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                s.push(d);
                i += 1;
                if d == '\\' && i < chars.len() {
                    s.push(chars[i]); // keep the escaped char, don't let it close
                    i += 1;
                } else if d == c {
                    break;
                }
            }
            spans.push(Span::styled(s, string_style));
            continue;
        }
        // Identifier / keyword.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if CODE_KEYWORDS.contains(&word.as_str()) {
                flush_plain(&mut buf, &mut spans);
                spans.push(Span::styled(word, keyword_style));
            } else {
                buf.push_str(&word);
            }
            continue;
        }
        // Number literal (a token starting with a digit).
        if c.is_ascii_digit() {
            flush_plain(&mut buf, &mut spans);
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            spans.push(Span::styled(
                chars[start..i].iter().collect::<String>(),
                number_style,
            ));
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush_plain(&mut buf, &mut spans);
    spans
}

/// Split a message body into per-line span groups (no sender prefix), detecting
/// fenced ```code``` blocks: the fence lines become a dim separator (with the
/// language label on the opener), lines inside get a `│` gutter + syntax
/// highlighting, and prose lines render plain. An unclosed fence keeps
/// highlighting to the end (reasonable for a partial paste).
fn body_segments(text: &str) -> Vec<Vec<Span<'static>>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut segs: Vec<Vec<Span<'static>>> = Vec::new();
    let mut in_code = false;
    let mut lang = String::new();
    for line in text.split('\n') {
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            if in_code {
                in_code = false;
                lang.clear();
                segs.push(vec![Span::styled("└─────────".to_string(), dim)]);
            } else {
                in_code = true;
                lang = rest.trim().to_string();
                let label = if lang.is_empty() {
                    "┌─ code".to_string()
                } else {
                    format!("┌─ {lang}")
                };
                segs.push(vec![Span::styled(label, dim)]);
            }
            continue;
        }
        if in_code {
            let mut spans = vec![Span::styled("│ ".to_string(), dim)];
            spans.extend(highlight_code_line(line, &lang));
            segs.push(spans);
        } else {
            segs.push(vec![Span::raw(line.to_string())]);
        }
    }
    segs
}

fn render_chat_pane(f: &mut Frame, model: &Model, area: Rect) {
    let Some((header_spans, messages)) = selected_pane_content(model) else {
        let hint = Paragraph::new(
            "No chats yet.\n\nAdd a contact:  moyu add <npub|hex|name@domain>\nThen reopen `moyu tui` and press Enter to send.",
        )
        .block(Block::default().borders(Borders::ALL).title(" moyu "))
        .wrap(Wrap { trim: false });
        f.render_widget(hint, area);
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);

    // Header: who (DM) or which workspace/channel we're looking at.
    let header = Paragraph::new(Line::from(header_spans));
    f.render_widget(header, rows[0]);

    // Messages: each is one line, optionally preceded by a dim reply-quote line
    // and followed by a dim reaction-pill line. Scrolled to the bottom. (Wrapped
    // long lines can make the scroll approximate; good enough for v1.)
    let lines: Vec<Line> = messages
        .iter()
        .flat_map(|m| message_lines(m, messages))
        .collect();
    let visible = rows[1].height.saturating_sub(2) as usize; // minus block borders
    let scroll = lines.len().saturating_sub(visible) as u16;
    let messages = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(messages, rows[1]);

    // Input line. The box is one row tall with no wrap, so show the tail of a
    // long input (by char, not byte) to keep the cursor visible.
    let inner_w = rows[2].width.saturating_sub(5) as usize; // borders + "› " + cursor
    let shown: String = {
        let chars: Vec<char> = model.input.chars().collect();
        if chars.len() > inner_w {
            chars[chars.len() - inner_w..].iter().collect()
        } else {
            model.input.clone()
        }
    };
    let input = Paragraph::new(Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Green)),
        Span::raw(shown),
        Span::styled("▏", Style::default().fg(Color::Gray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Message (Enter to send) "),
    );
    f.render_widget(input, rows[2]);
}

fn render_status(f: &mut Frame, model: &Model, area: Rect) {
    let socks = if model.socks5_on { "on" } else { "off" };
    let line = Line::from(vec![
        Span::styled(
            format!(" me {} ", model.account_npub_short),
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw(format!(
            " relay {} · socks5 {} ",
            model.relay_summary, socks
        )),
        Span::styled(
            "· ↑↓ chats · ⏎ send · Ctrl-R roster · Ctrl-C quit ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("· {}", model.status),
            Style::default().fg(Color::Yellow),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// The `Ctrl-R` roster overlay: a centered `Clear` + `List` of the selected
/// row's group members, admins first and badged " (admin)", drawn on top of
/// everything else. Rows are pre-formatted display strings (see `open_roster`),
/// so this renders them verbatim. Only shown while `model.roster.is_some()`.
fn render_roster(f: &mut Frame, rows: &[String], area: Rect) {
    let popup = centered_rect(60, 60, area);
    f.render_widget(Clear, popup);
    let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(r.clone())).collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Members (Esc to close) "),
    );
    f.render_widget(list, popup);
}

/// A `width_pct` x `height_pct` rectangle centered inside `area` -- the usual
/// ratatui two-axis `Layout::Percentage` popup-centering trick.
fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// Terminal harness (adapted from wn-tui's MIT `tui.rs` pattern)
// ---------------------------------------------------------------------------

fn init_terminal() -> anyhow::Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    // Past this point any failure must undo raw mode before returning -- run()
    // only starts restoring once init_terminal has returned Ok, so a failure
    // strictly between enabling raw mode and here would otherwise wedge the
    // shell with no cleanup path.
    let build = || -> anyhow::Result<Term> {
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
        Ok(Terminal::new(CrosstermBackend::new(stdout))?)
    };
    let result = build();
    if result.is_err() {
        let _ = restore_terminal();
    }
    result
}

fn restore_terminal() -> anyhow::Result<()> {
    // Run BOTH steps unconditionally: a failure leaving the alt-screen must not
    // skip disabling raw mode (which would wedge the shell). Surface the first
    // error only after both have run.
    let leave = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let raw = crossterm::terminal::disable_raw_mode();
    leave?;
    raw?;
    Ok(())
}

/// Restore the terminal before a panic unwinds through the raw-mode/alt-screen
/// so the user's shell is not left wedged.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original(info);
    }));
}

// ---------------------------------------------------------------------------
// Small display helpers
// ---------------------------------------------------------------------------

/// `npub1abcdef…` — enough to recognize, short enough for a header/status bar.
fn shorten(s: &str) -> String {
    if s.chars().count() <= 14 {
        return s.to_string();
    }
    let head: String = s.chars().take(13).collect();
    format!("{head}…")
}

/// A compact one-line summary of a governance diff for the status bar, e.g.
/// `demo: npub1abcde…xyz gone · demo: npub1def…uvw +admin`. Honest about
/// ambiguity: a departure shows as "gone" (a roster diff can't tell a kick from
/// a voluntary leave).
fn gov_status(lines: &[crate::domain::GovLine]) -> String {
    use moyu_core::governance::GovChange::{Demoted, Departed, Joined, Promoted};
    let parts: Vec<String> = lines
        .iter()
        .map(|l| match l {
            crate::domain::GovLine::Change { ws_name, change } => {
                let (hex, verb) = match change {
                    Joined { member_id_hex } => (member_id_hex, "joined"),
                    Departed { member_id_hex } => (member_id_hex, "gone"),
                    Promoted { member_id_hex } => (member_id_hex, "+admin"),
                    Demoted { member_id_hex } => (member_id_hex, "-admin"),
                };
                let who = npub_from_hex(hex)
                    .map(|n| shorten(&n))
                    .unwrap_or_else(|_| shorten(hex));
                format!("{ws_name}: {who} {verb}")
            }
            crate::domain::GovLine::SelfRemoved { ws_name } => format!("removed from {ws_name}"),
        })
        .collect();
    parts.join(" · ")
}

/// `wss://relay.damus.io` -> `relay.damus.io`; `ws://127.0.0.1:7777` -> that.
fn shorten_relay(url: &str) -> String {
    url.strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn model_with(chats: Vec<(&str, bool)>) -> Model {
        Model {
            account_npub_short: "npub1abc…".into(),
            relay_summary: "nos.lol".into(),
            socks5_on: true,
            account_label: "me".into(),
            chats: chats
                .into_iter()
                .map(|(label, has_group)| Chat {
                    label: label.into(),
                    npub: "npub1peer".into(),
                    group: has_group.then(|| GroupId::new(vec![0u8; 16])),
                    messages: Vec::new(),
                    unread: 0,
                    seen_ids: HashSet::new(),
                })
                .collect(),
            workspaces: Vec::new(),
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".into(),
        }
    }

    /// Render to an in-memory backend and assert the layout's anchor strings
    /// show up — a headless smoke test that the widget tree builds and draws
    /// without panicking (a full interactive TUI can't be driven in a test).
    /// Also exercises the threaded-reply render path: a threaded reply (dim quote
    /// line) and a reaction pill (👍 2) below a message.
    #[test]
    fn renders_layout_without_panicking() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut model = model_with(vec![("alice", true), ("bob", false)]);
        model.chats[0].messages.push(Msg::chat(
            "mid_alice".into(),
            "alice".into(),
            "hi over MLS".into(),
            false,
            None,
        ));
        // My line replies to alice's and carries a 👍×2 reaction.
        let mut mine = Msg::chat(
            "mid_me".into(),
            "me".into(),
            "hey".into(),
            true,
            Some("mid_alice".into()),
        );
        mine.reactions.insert("👍".into(), 2);
        model.chats[0].messages.push(mine);
        model.input = "typing".into();
        // One workspace ("demo") with #general + #backend, the latter with
        // one message -- exercises the two-level rail alongside the DMs.
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xDDu8; 16]),
            name: "demo".into(),
            channels: vec![
                ChannelBuf {
                    slug: "general".into(),
                    archived: false,
                    group: None,
                    messages: Vec::new(),
                    seen_ids: HashSet::new(),
                    unread: 0,
                },
                ChannelBuf {
                    slug: "backend".into(),
                    archived: false,
                    group: None,
                    messages: vec![Msg::chat(
                        "mid_carol".into(),
                        "carol".into(),
                        "shipping the tui".into(),
                        false,
                        None,
                    )],
                    seen_ids: HashSet::new(),
                    unread: 0,
                },
            ],
        });

        // selected == 0 (alice's DM) -- the rail shows every row regardless
        // of selection, so the workspace/channel entries are visible here too
        // even though the message pane on the right is still alice's.
        term.draw(|f| render(f, &model)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Chats"), "chat list pane present");
        assert!(dump.contains("alice"), "DM label shown in the rail");
        assert!(dump.contains("hi over MLS"), "incoming DM message shown");
        assert!(dump.contains("typing"), "input buffer shown");
        assert!(dump.contains("socks5 on"), "status bar shows proxy state");
        assert!(dump.contains("demo"), "workspace name shown in the rail");
        assert!(dump.contains("#backend"), "channel slug shown in the rail");

        // Move selection onto the #backend channel row (Dm, Dm, header,
        // #general, #backend -> index 4) and confirm the pane switches to
        // showing that channel's own message.
        model.selected = 4;
        term.draw(|f| render(f, &model)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("demo"), "workspace name shown in the header");
        assert!(dump.contains("#backend"), "channel header shown");
        assert!(
            dump.contains("shipping the tui"),
            "selected channel's message body shown"
        );
    }

    /// M3: a private channel is a SEPARATE MLS group nested under its parent
    /// workspace. Inbound messages on its own group route to it; posts/roster use
    /// its own group; and the rail marks it 🔒 (a public channel of the same slug
    /// is never confused for it).
    #[test]
    fn private_channel_routes_to_its_own_group_and_renders_locked() {
        let g_ws = GroupId::new(vec![0xDDu8; 16]);
        let g_priv = GroupId::new(vec![0xAAu8; 16]);
        let mut model = model_with(vec![]);
        model.workspaces.push(WorkspaceEntry {
            group: g_ws.clone(),
            name: "demo".into(),
            channels: vec![
                empty_channel("general", 0), // public: group None -> workspace group
                ChannelBuf {
                    slug: "secret".into(),
                    archived: false,
                    group: Some(g_priv.clone()), // private: its own MLS group
                    messages: Vec::new(),
                    seen_ids: HashSet::new(),
                    unread: 0,
                },
            ],
        });

        // A message on the private channel's OWN group routes to that channel
        // (index 1), even though its group != the workspace group.
        assert!(matches!(
            model.resolve_route(&g_priv.to_string(), "secret"),
            Some(Target::Channel(0, 1))
        ));
        // A message on the workspace group routes to the PUBLIC channel by slug,
        // never the private one.
        assert!(matches!(
            model.resolve_route(&g_ws.to_string(), "general"),
            Some(Target::Channel(0, 0))
        ));
        // Routing group: private -> own group; public -> workspace group.
        assert_eq!(model.channel_group(0, 1), g_priv);
        assert_eq!(model.channel_group(0, 0), g_ws);
        assert!(model.workspaces[0].channels[1].is_private());
        assert!(!model.workspaces[0].channels[0].is_private());

        // The rail marks the private channel with a lock.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| render(f, &model)).unwrap();
        let dump: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            dump.contains("#secret"),
            "private channel slug shown in the rail"
        );
        assert!(dump.contains('🔒'), "private channel marked with a lock");
    }

    /// A workspace whose `#general` was archived away has no
    /// `#general` channel; selecting its header must still render a workspace
    /// pane (name + a note), NOT fall through to the global "No chats yet"
    /// hint as if nothing were selected.
    #[test]
    fn workspace_header_without_general_renders_a_pane_not_the_global_hint() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut model = model_with(vec![("alice", true)]);
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xEEu8; 16]),
            name: "demo".into(),
            // No #general (archived away); no other channels either.
            channels: vec![],
        });
        // rows: [Dm(0)=alice, Header(0)] -> select the workspace header.
        model.selected = 1;
        assert!(matches!(
            model.selected_row(),
            Some(Row::WorkspaceHeader(0))
        ));

        term.draw(|f| render(f, &model)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            dump.contains("demo"),
            "workspace name shown in the header pane"
        );
        assert!(
            !dump.contains("No chats yet"),
            "must not fall through to the global no-chats hint when a workspace is selected"
        );
    }

    #[test]
    fn selection_wraps_and_clears_unread() {
        let mut model = model_with(vec![("a", true), ("b", true), ("c", true)]);
        model.chats[1].unread = 3;
        move_selection(&mut model, 1); // -> b
        assert_eq!(model.selected, 1);
        assert_eq!(model.chats[1].unread, 0, "opening a chat clears its unread");
        move_selection(&mut model, -1); // -> a
        assert_eq!(model.selected, 0);
        move_selection(&mut model, -1); // wrap -> c
        assert_eq!(model.selected, 2);
    }

    fn empty_channel(slug: &str, unread: usize) -> ChannelBuf {
        ChannelBuf {
            slug: slug.into(),
            archived: false,
            group: None,
            messages: Vec::new(),
            seen_ids: HashSet::new(),
            unread,
        }
    }

    #[test]
    fn rows_flattens_dms_then_per_workspace_header_and_channels() {
        let mut model = model_with(vec![("alice", true), ("bob", false)]);
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xEEu8; 16]),
            name: "demo".into(),
            channels: vec![empty_channel("general", 0), empty_channel("backend", 0)],
        });
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xFFu8; 16]),
            name: "second".into(),
            channels: vec![empty_channel("general", 0)],
        });

        let rows = model.rows();
        assert!(matches!(&rows[0], Row::Dm(0)));
        assert!(matches!(&rows[1], Row::Dm(1)));
        assert!(matches!(&rows[2], Row::WorkspaceHeader(0)));
        assert!(matches!(&rows[3], Row::Channel(0, 0)));
        assert!(matches!(&rows[4], Row::Channel(0, 1)));
        assert!(matches!(&rows[5], Row::WorkspaceHeader(1)));
        assert!(matches!(&rows[6], Row::Channel(1, 0)));
        assert_eq!(
            rows.len(),
            7,
            "2 DMs + (header+2 channels) + (header+1 channel)"
        );
    }

    #[test]
    fn move_selection_clears_unread_across_dms_and_channels() {
        let mut model = model_with(vec![("alice", true)]);
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xEEu8; 16]),
            name: "demo".into(),
            channels: vec![empty_channel("backend", 5)],
        });
        // rows: [Dm(0), WorkspaceHeader(0), Channel(0, 0)]
        move_selection(&mut model, 1); // -> WorkspaceHeader(0), an inert landing
        assert_eq!(model.selected, 1);
        move_selection(&mut model, 1); // -> Channel(0, 0)
        assert_eq!(model.selected, 2);
        assert_eq!(
            model.workspaces[0].channels[0].unread, 0,
            "landing on a channel clears its own unread, same as a DM"
        );
        move_selection(&mut model, 1); // wraps back to Dm(0)
        assert_eq!(model.selected, 0);
    }

    /// `selected` must stay anchored to the SAME entity across a
    /// `rows()`-shifting change (here a background sync adding a DM, which
    /// pushes every workspace/channel row down one). Positional `selected`
    /// would then point at a different channel and mis-target the next Enter.
    #[test]
    fn selection_is_anchored_by_identity_across_a_rows_shift() {
        let mut model = model_with(vec![("alice", true)]);
        model.workspaces.push(WorkspaceEntry {
            group: GroupId::new(vec![0xEEu8; 16]),
            name: "demo".into(),
            channels: vec![empty_channel("general", 0), empty_channel("backend", 0)],
        });
        // rows: [Dm(0)=alice, Header(0), Channel(0,0)=general, Channel(0,1)=backend]
        model.selected = 3; // #backend
        assert!(matches!(model.selected_row(), Some(Row::Channel(0, 1))));

        // A background sync adds a DM (like ensure_chat_for_group), shifting
        // every workspace row down by one. Anchor across it by identity.
        let prev = model.selected_identity();
        model.chats.push(Chat {
            label: "bob".into(),
            npub: "npub1bob".into(),
            group: Some(GroupId::new(vec![0xBBu8; 16])),
            messages: Vec::new(),
            unread: 0,
            seen_ids: HashSet::new(),
        });
        model.reselect(prev);

        // rows now: [Dm(0)=alice, Dm(1)=bob, Header(0), general, backend]
        assert_eq!(
            model.selected, 4,
            "reselect followed #backend to its new index"
        );
        match model.selected_row() {
            Some(Row::Channel(w, c)) => {
                assert_eq!(model.workspaces[w].channels[c].slug, "backend");
            }
            other => panic!("expected #backend channel selected, got {other:?}"),
        }

        // If the anchored entity disappears (e.g. #backend archived away),
        // reselect clamps to a valid index instead of dangling past the end.
        let prev = model.selected_identity(); // #backend
        model.workspaces[0].channels.pop(); // drop #backend
        model.reselect(prev);
        let n = model.rows().len();
        assert!(
            model.selected < n,
            "selection clamped into range after its row vanished"
        );
    }

    #[test]
    fn shorten_helpers() {
        assert_eq!(shorten("short"), "short");
        assert_eq!(shorten("npub1abcdefghijklmnop"), "npub1abcdefgh…");
        assert_eq!(shorten_relay("wss://relay.damus.io/"), "relay.damus.io");
        assert_eq!(shorten_relay("ws://127.0.0.1:7777"), "127.0.0.1:7777");
    }

    /// A kind-7 reaction folds onto the message its `e` tag targets
    /// (aggregated per emoji), a re-ingested reaction is not double-counted, a
    /// reaction whose target isn't buffered is silently dropped, and a kind-5
    /// `unreact` (keyed by the reaction event id) decrements / removes it.
    #[test]
    fn reactions_apply_and_retract_onto_target_message() {
        let mut model = model_with(vec![("alice", true)]);
        let ghex = model.chats[0].group_hex().unwrap();
        model.chats[0].messages.push(Msg::chat(
            "target1".into(),
            "alice".into(),
            "ship it".into(),
            false,
            None,
        ));
        // Two reactors 👍, one ✅; a re-ingest of r1 must not double-count.
        model.apply_reaction(&ghex, "target1", "👍", "r1".into());
        model.apply_reaction(&ghex, "target1", "👍", "r2".into());
        model.apply_reaction(&ghex, "target1", "✅", "r3".into());
        model.apply_reaction(&ghex, "target1", "👍", "r1".into());
        // A reaction to an unbuffered target is dropped (no panic, no entry).
        model.apply_reaction(&ghex, "not-here", "🎉", "r4".into());
        {
            let rx = &model.chats[0].messages[0].reactions;
            assert_eq!(rx.get("👍"), Some(&2));
            assert_eq!(rx.get("✅"), Some(&1));
        }
        // Retract one 👍 -> count 1; retract the ✅ -> key removed entirely.
        model.retract_reaction("r1");
        model.retract_reaction("r3");
        // Retracting an unknown reaction id is a harmless no-op.
        model.retract_reaction("nope");
        let rx = &model.chats[0].messages[0].reactions;
        assert_eq!(rx.get("👍"), Some(&1));
        assert_eq!(rx.get("✅"), None);
    }

    /// The code highlighter colours keywords (magenta), strings
    /// (yellow), numbers (cyan) and line comments (dim), and is language-aware
    /// about comment markers so Rust's `#[attr]` is NOT dimmed as a comment
    /// (only `//` is), while Python's `#` is.
    #[test]
    fn highlight_code_line_colors_tokens_language_aware() {
        use ratatui::style::Color;
        let fg = |spans: &[Span], needle: &str| -> Option<Color> {
            spans
                .iter()
                .find(|s| s.content.as_ref() == needle)
                .and_then(|s| s.style.fg)
        };
        let rust = super::highlight_code_line("let x = \"hi\"; // note", "rust");
        assert_eq!(fg(&rust, "let"), Some(Color::Magenta));
        assert_eq!(fg(&rust, "\"hi\""), Some(Color::Yellow));
        assert_eq!(fg(&rust, "// note"), Some(Color::DarkGray));
        // In Rust `#` opens an attribute, not a comment -> nothing is dimmed.
        let attr = super::highlight_code_line("#[derive(Debug)]", "rust");
        assert!(attr.iter().all(|s| s.style.fg != Some(Color::DarkGray)));
        // In Python `#` IS a comment; the digit is a number.
        let py = super::highlight_code_line("x = 1  # c", "py");
        assert_eq!(fg(&py, "# c"), Some(Color::DarkGray));
        assert_eq!(fg(&py, "1"), Some(Color::Cyan));
    }

    /// Fenced ```lang blocks are detected — the opener becomes a
    /// dim `┌─ lang` label, inner lines get a `│ ` gutter and are highlighted,
    /// and prose outside the fence stays a single plain span.
    #[test]
    fn body_segments_detects_fenced_code_blocks() {
        use ratatui::style::Color;
        let segs = super::body_segments("see:\n```rust\nfn main() {}\n```\ndone");
        // prose, opener, code, closer, prose = 5 visual lines.
        assert_eq!(segs.len(), 5);
        assert!(segs[1][0].content.contains("rust")); // opener carries the label
        assert_eq!(segs[2][0].content.as_ref(), "│ "); // code line gutter
        assert!(
            segs[2]
                .iter()
                .any(|s| s.content.as_ref() == "fn" && s.style.fg == Some(Color::Magenta))
        );
        assert_eq!(segs[0].len(), 1); // prose = one plain span
        assert_eq!(segs[0][0].content.as_ref(), "see:");
    }

    #[test]
    fn load_history_routes_filters_orders() {
        let g_a = GroupId::new(vec![0xAAu8; 16]);
        let g_b = GroupId::new(vec![0xBBu8; 16]);
        let g_ws = GroupId::new(vec![0xCCu8; 16]);
        let (ha, hb, hws) = (g_a.to_string(), g_b.to_string(), g_ws.to_string());
        let mut model = Model {
            account_npub_short: "me".into(),
            relay_summary: "r".into(),
            socks5_on: false,
            account_label: "me".into(),
            chats: vec![
                Chat {
                    label: "alice".into(),
                    npub: "npuba".into(),
                    group: Some(g_a),
                    messages: vec![],
                    unread: 0,
                    seen_ids: HashSet::new(),
                },
                Chat {
                    label: "bob".into(),
                    npub: "npubb".into(),
                    group: Some(g_b),
                    messages: vec![],
                    unread: 0,
                    seen_ids: HashSet::new(),
                },
            ],
            workspaces: vec![WorkspaceEntry {
                group: g_ws,
                name: "demo".into(),
                // Only #general exists up front -- #backend must be created
                // on the fly by routing.
                channels: vec![ChannelBuf {
                    slug: "general".into(),
                    archived: false,
                    group: None,
                    messages: vec![],
                    seen_ids: HashSet::new(),
                    unread: 0,
                }],
            }],
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".into(),
        };
        let rec =
            |gid: &str, dir: &str, kind: u64, text: &str, at: u64, ord: i64| AppMessageRecord {
                message_id_hex: format!("m{ord}"),
                direction: dir.into(),
                group_id_hex: gid.into(),
                sender: "npuba".into(),
                plaintext: text.into(),
                kind,
                tags: vec![],
                source_epoch: None,
                recorded_at: at,
                received_at: at,
                insert_order: ord,
                retention: None,
                invalidated: false,
                moderation_grant: false,
            };
        model.load_history(vec![
            rec(&ha, "received", 9, "second", 20, 2), // out of order on purpose
            rec(&ha, "sent", 9, "first", 10, 1),
            rec(&ha, "received", 7, "a-reaction", 15, 3), // kind 7 -> filtered
            rec(&hb, "received", 9, "bob msg", 5, 4),
            rec("cccc", "received", 9, "orphan", 30, 5), // no chat row -> skipped
            rec(&ha, "received", 9, "dup-of-first", 10, 1), // same id (m1) -> deduped
            rec(
                &hws,
                "received",
                9,
                r#"{"moyu":1,"ch":"backend","body":"hi"}"#,
                40,
                6,
            ), // workspace envelope -> routes to #backend, created on the fly
            rec(
                &hws,
                "received",
                KIND_GROUP_SYSTEM,
                r#"{"v":1,"system_type":"moyu.channel.create","text":"","data":{}}"#,
                41,
                7,
            ), // kind 1210 -> filtered, never reaches routing/display
        ]);

        // alice: chronological, reaction filtered, dup dropped -> exactly 2
        let a = &model.chats[0].messages;
        assert_eq!(a.len(), 2, "reaction filtered + duplicate id deduped");
        assert_eq!(a[0].text, "first");
        assert!(a[0].mine);
        assert_eq!(a[0].who, "me");
        assert_eq!(a[1].text, "second");
        assert!(!a[1].mine);
        assert_eq!(a[1].who, "alice"); // 1:1 -> chat label, not raw sender
        // bob got its one message; the orphan-group record was dropped
        assert_eq!(model.chats[1].messages.len(), 1);
        assert_eq!(model.chats[1].messages[0].text, "bob msg");

        // workspace: #general untouched, #backend created on the fly with
        // exactly the one (decoded, kind-1210-excluded) message.
        let ws = &model.workspaces[0];
        assert_eq!(ws.channels.len(), 2, "backend created alongside general");
        assert!(ws.channels[0].messages.is_empty(), "general untouched");
        let backend = ws
            .channels
            .iter()
            .find(|c| c.slug == "backend")
            .expect("#backend created on the fly");
        assert_eq!(backend.messages.len(), 1, "kind-1210 record never routed");
        assert_eq!(backend.messages[0].text, "hi");
        assert!(!backend.messages[0].mine);
    }

    /// Bot/agent events (kind-1201/1202) load as distinct 🤖 lines:
    /// routed into the channel their payload names (or #general), body
    /// pre-formatted, `ok` captured for coloring, and rendered with a 🤖 marker.
    #[test]
    fn load_history_places_bot_events_as_bot_lines() {
        let g_ws = GroupId::new(vec![0xCCu8; 16]);
        let hws = g_ws.to_string();
        let mut model = Model {
            account_npub_short: "me".into(),
            relay_summary: "r".into(),
            socks5_on: false,
            account_label: "me".into(),
            chats: vec![],
            workspaces: vec![WorkspaceEntry {
                group: g_ws,
                name: "demo".into(),
                channels: vec![ChannelBuf {
                    slug: "general".into(),
                    archived: false,
                    group: None,
                    messages: vec![],
                    seen_ids: HashSet::new(),
                    unread: 0,
                }],
            }],
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".into(),
        };
        let rec = |kind: u64, text: &str, ord: i64| AppMessageRecord {
            message_id_hex: format!("b{ord}"),
            direction: "received".into(),
            group_id_hex: hws.clone(),
            sender: "npuba".into(),
            plaintext: text.into(),
            kind,
            tags: vec![],
            source_epoch: None,
            recorded_at: 10 * ord as u64,
            received_at: 10 * ord as u64,
            insert_order: ord,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        };
        model.load_history(vec![
            // A failed CI op, explicitly routed to #general via details.moyu.ch.
            rec(
                1202,
                r#"{"v":1,"status":"failed","text":"3 tests failed","event_type":"ci","name":"build","ok":false,"duration_ms":4200,"details":{"moyu":{"ch":"general"}}}"#,
                1,
            ),
            // A plain activity with no routing -> defaults to #general.
            rec(1201, r#"{"v":1,"status":"active","text":"deploying"}"#, 2),
        ]);

        let general = &model.workspaces[0].channels[0];
        assert_eq!(
            general.messages.len(),
            2,
            "both bot events placed in #general"
        );
        let op = &general.messages[0];
        assert!(op.bot.is_some(), "kind-1202 is a bot line");
        assert_eq!(op.bot.as_ref().unwrap().ok, Some(false));
        assert_eq!(op.text, "[ci·failed] build: 3 tests failed ✗ (4.2s)");
        let act = &general.messages[1];
        assert!(act.bot.is_some(), "kind-1201 is a bot line");
        assert_eq!(act.bot.as_ref().unwrap().ok, None);
        assert_eq!(act.text, "[active] deploying");

        // The rendered first line leads with a 🤖 marker and the op body.
        let lines = message_lines(op, &[]);
        assert!(!lines.is_empty());
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            first.contains("🤖"),
            "bot line renders a 🤖 marker: {first:?}"
        );
        assert!(
            first.contains("[ci·failed]"),
            "renders the op body: {first:?}"
        );
    }

    /// A join-request is a normal kind-9 DM whose plaintext is the
    /// `invite::JoinRequest` envelope (content-based, not a separate kind) --
    /// `load_history` must render it as a clean 🔑 line, never the raw JSON, and
    /// never the envelope's attacker-controlled `ws_name`: the
    /// workspace name is resolved from the envelope's *gid* against the local
    /// workspaces, falling back to the short gid marked "未验证" when unknown.
    #[test]
    fn load_history_renders_join_request_by_gid_not_claim() {
        let ha = GroupId::new(vec![0xAAu8; 16]).to_string();
        let base_model = || Model {
            account_npub_short: "me".into(),
            relay_summary: "r".into(),
            socks5_on: false,
            account_label: "me".into(),
            chats: vec![Chat {
                label: "alice".into(),
                npub: "npuba".into(),
                group: Some(GroupId::new(vec![0xAAu8; 16])),
                messages: vec![],
                unread: 0,
                seen_ids: HashSet::new(),
            }],
            workspaces: vec![],
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".into(),
        };
        let rec = |plaintext: String, id: &str| AppMessageRecord {
            message_id_hex: id.into(),
            direction: "received".into(),
            group_id_hex: ha.clone(),
            sender: "npuba".into(),
            plaintext,
            kind: 9,
            tags: vec![],
            source_epoch: None,
            recorded_at: 10,
            received_at: 10,
            insert_order: 1,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        };
        let texts = |m: &Model| -> Vec<String> {
            m.chats[0].messages.iter().map(|x| x.text.clone()).collect()
        };

        // Unknown gid: NEVER echo the attacker's claimed "eng"; show 未验证 + gid.
        let jr_unknown =
            moyu_core::invite::build_join_request_content(&moyu_core::invite::JoinRequest {
                ws_gid_hex: "ab".repeat(32),
                ws_name: "eng".into(),
                secret_hex: "cd".repeat(16),
            });
        let mut model = base_model();
        model.load_history(vec![rec(jr_unknown, "m1")]);
        let rendered = texts(&model);
        assert!(
            rendered
                .iter()
                .any(|t| t.contains("想加入") && t.contains("未验证")),
            "unknown gid rendered as unverified: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|t| t.contains("eng")),
            "must NOT echo the envelope's attacker-controlled ws_name: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|t| t.contains("\"join-request\"")),
            "never renders the raw JSON: {rendered:?}"
        );

        // Known gid: resolve the REAL local workspace name, ignore the claim.
        let g_ws = GroupId::new(vec![0xEEu8; 16]);
        let jr_known =
            moyu_core::invite::build_join_request_content(&moyu_core::invite::JoinRequest {
                ws_gid_hex: g_ws.to_string(),
                ws_name: "attacker-claim".into(),
                secret_hex: "cd".repeat(16),
            });
        let mut model = base_model();
        model.workspaces.push(WorkspaceEntry {
            group: g_ws,
            name: "real-eng".into(),
            channels: vec![],
        });
        model.load_history(vec![rec(jr_known, "m2")]);
        let rendered = texts(&model);
        assert!(
            rendered
                .iter()
                .any(|t| t.contains("想加入") && t.contains("real-eng")),
            "known gid resolves to the real workspace name: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|t| t.contains("attacker-claim")),
            "must NOT echo the claimed name when the gid is known: {rendered:?}"
        );
    }

    /// A malicious message body or sender label can't smuggle a terminal
    /// escape or a Unicode bidi override into the model: `load_history` ->
    /// `Msg::chat` sanitizes both on the way in (see `Msg::chat`'s doc
    /// comment), independent of -- and before -- ratatui's own
    /// control-character filtering in `Buffer::set_stringn`.
    #[test]
    fn load_history_sanitizes_control_and_bidi_chars_in_body_and_label() {
        let g = GroupId::new(vec![0xBBu8; 16]);
        let mut model = Model {
            account_npub_short: "me".into(),
            relay_summary: "r".into(),
            socks5_on: false,
            account_label: "me".into(),
            chats: vec![Chat {
                label: "mallory\u{202e}evil".into(),
                npub: "npubm".into(),
                group: Some(g.clone()),
                messages: vec![],
                unread: 0,
                seen_ids: HashSet::new(),
            }],
            workspaces: vec![],
            roster: None,
            prev_rosters: HashMap::new(),
            reaction_index: HashMap::new(),
            selected: 0,
            input: String::new(),
            status: "ready".into(),
        };
        let rec = AppMessageRecord {
            message_id_hex: "m1".into(),
            direction: "received".into(),
            group_id_hex: g.to_string(),
            sender: "npubm".into(),
            plaintext: "hi \x1b[31mred\x1b[0m".into(),
            kind: 9,
            tags: vec![],
            source_epoch: None,
            recorded_at: 10,
            received_at: 10,
            insert_order: 1,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        };
        model.load_history(vec![rec]);

        let msg = &model.chats[0].messages[0];
        assert!(
            !msg.text.contains('\x1b'),
            "ESC must not reach the buffer: {:?}",
            msg.text
        );
        assert!(msg.text.contains('\u{fffd}'));
        assert!(
            !msg.who.contains('\u{202e}'),
            "RLO must not reach the buffer: {:?}",
            msg.who
        );
        assert!(msg.who.contains('\u{fffd}'));
    }

    /// The `Ctrl-R` roster overlay renders its pre-formatted rows verbatim, so an
    /// admin row's " (admin)" badge reaches the buffer. Drives `render` with a
    /// set overlay (badging itself is `open_roster`'s job and needs a client).
    #[test]
    fn roster_overlay_renders_admin_badge() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut model = model_with(vec![("alice", true)]);
        model.roster = Some(vec!["npub1admin… (admin)".into(), "npub1plain…".into()]);
        term.draw(|f| render(f, &model)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Members"), "roster overlay title shown");
        assert!(
            dump.contains("(admin)"),
            "admin badge rendered in the roster"
        );
    }

    /// `gov_status` folds a governance diff into one compact status line, honest
    /// about a departure being indistinguishable from a kick ("gone").
    #[test]
    fn gov_status_summarizes_a_diff() {
        let hx = |b: u8| hex::encode([b; 32]);
        let lines = vec![
            crate::domain::GovLine::Change {
                ws_name: "demo".into(),
                change: moyu_core::governance::GovChange::Departed {
                    member_id_hex: hx(0xaa),
                },
            },
            crate::domain::GovLine::Change {
                ws_name: "demo".into(),
                change: moyu_core::governance::GovChange::Promoted {
                    member_id_hex: hx(0xbb),
                },
            },
            crate::domain::GovLine::SelfRemoved {
                ws_name: "second".into(),
            },
        ];
        let s = gov_status(&lines);
        assert!(s.contains("demo:"), "workspace name shown");
        assert!(
            s.contains("gone"),
            "departure shown as 'gone' (kick or leave)"
        );
        assert!(s.contains("+admin"), "promotion shown");
        assert!(s.contains("removed from second"), "self-removal shown");
        assert!(s.contains(" · "), "multiple changes joined");
    }
}
