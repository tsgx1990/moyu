//! Print-free command cores ("ops") shared by the one-shot CLI handlers in
//! `main.rs` and, next, the long-lived `moyu session` dispatcher (the GUI-shell
//! track): each op takes already-resolved inputs plus already-open handles,
//! performs exactly what its `cmd_*` wrapper used to inline, and returns a
//! typed receipt (or streams typed [`SessionEvent`]s) for the caller to
//! render — human text, one-shot `--json`, or session-framed JSONL. Keeping
//! this layer print-free is what will guarantee `moyu session`'s stdout stays
//! pure JSONL (a single stray `println!` would corrupt the GUI's parser).
//! Diagnostics/warnings remain `eprintln!` (stderr) exactly as before.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use moyu_core::engine::{
    AgentOperationEventRequest, AppClient, AppMessageRecord, GroupId, MediaAttachmentReference,
    MoyuEngine, SelfMembership,
};
use moyu_core::governance::{self, LeavePlan};
use moyu_core::identity::resolve_peer;
use moyu_core::invite;
use moyu_core::keypackage_rotation;
use moyu_core::store::{self, Contact, MoyuStore};
use moyu_core::workspace::{self, ControlEvent};
use std::net::SocketAddr;

use crate::output::{self, SCHEMA_VERSION};

/// `whoami`: the active account's label and (when the label is a pubkey hex,
/// which `moyu init` guarantees) its npub. Deliberately needs no
/// engine/passphrase — it only reads the locally persisted label.
pub(crate) struct Whoami {
    pub label: String,
    pub npub: Option<String>,
}

pub(crate) fn whoami(data_dir: &Path) -> anyhow::Result<Whoami> {
    let label = crate::domain::active_label(data_dir)?;
    let npub = moyu_core::identity::npub_from_hex(&label).ok();
    Ok(Whoami { label, npub })
}

/// What a `send` should carry: plain text, or a file attachment with an
/// optional caption. The arg/stdin resolution (`read_message_arg`) stays at
/// the call site — a session command supplies the body directly.
pub(crate) enum SendBody {
    Text(String),
    File {
        path: PathBuf,
        caption: Option<String>,
    },
}

/// The attachment half of a [`SendReceipt`]: the structured rows for a JSON
/// receipt plus the comma-joined file names for the human one.
pub(crate) struct AttachmentParts {
    pub json: Vec<serde_json::Value>,
    pub names: String,
}

/// Shared `send --file`/`post --file` upload step:
/// `upload_one_file` → `attachment_receipt_parts` → `attachment_names`, the
/// three-call sequence both attachment paths need. Each caller wraps the
/// result in its own receipt type (`SendReceipt` carries a `peer`,
/// `PostReceipt` a `group`/`channel`/`private`) -- only this shared middle is
/// identical.
async fn upload_attachment(
    client: &mut AppClient,
    group_id: &GroupId,
    path: &Path,
    caption: Option<String>,
    blossom: Option<String>,
) -> anyhow::Result<(usize, Vec<String>, AttachmentParts)> {
    let result = crate::domain::upload_one_file(client, group_id, path, caption, blossom).await?;
    let (published, message_ids, json) = crate::domain::attachment_receipt_parts(&result);
    let names = crate::domain::attachment_names(&result);
    Ok((published, message_ids, AttachmentParts { json, names }))
}

/// Typed receipt of a 1:1 `send` — everything both renderers (human line,
/// `--json` object) need.
pub(crate) struct SendReceipt {
    pub peer: String,
    pub group: String,
    pub published: usize,
    pub message_ids: Vec<String>,
    /// `Some` only on the attachment path.
    pub attachments: Option<AttachmentParts>,
}

impl SendReceipt {
    /// The shared `send` receipt body — identical in the one-shot `--json`
    /// object (which additionally wraps `v`/`ok`) and the session `send`
    /// receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "peer": self.peer,
            "group": self.group,
            "kind": crate::domain::CHAT_MESSAGE_KIND,
            "published": self.published,
            "message_ids": self.message_ids,
        });
        if let Some(parts) = &self.attachments {
            v["attachments"] = serde_json::Value::Array(parts.json.clone());
        }
        v
    }
}

/// Non-interactive 1:1 send core: `find_or_create_dm` then a single
/// `AppClient::send` (text) or `upload_media(send: true)` (attachment).
pub(crate) async fn send(
    client: &mut AppClient,
    moyu_store: &mut MoyuStore,
    peer: &str,
    body: SendBody,
    socks5: Option<SocketAddr>,
    blossom: Option<String>,
) -> anyhow::Result<SendReceipt> {
    let (peer_npub, group_id) =
        crate::domain::find_or_create_dm(client, moyu_store, peer, socks5).await?;

    match body {
        SendBody::File { path, caption } => {
            // A DM has a single channel, so the caption is the raw text (no
            // channel envelope). `upload_media(send:true)` encrypts, uploads,
            // and sends the kind-9 imeta message in one call.
            let (published, message_ids, attachments) =
                upload_attachment(client, &group_id, &path, caption, blossom).await?;
            Ok(SendReceipt {
                peer: peer_npub,
                group: group_id.to_string(),
                published,
                message_ids,
                attachments: Some(attachments),
            })
        }
        SendBody::Text(message) => {
            let summary = client.send(&group_id, message.as_bytes()).await?;
            Ok(SendReceipt {
                peer: peer_npub,
                group: group_id.to_string(),
                published: summary.published,
                message_ids: summary.message_ids,
                attachments: None,
            })
        }
    }
}

/// One rendered-agnostic inbound event out of a sync tick — exactly the eight
/// shapes `recv` has always emitted, now carried as data so each front end
/// picks its rendering: `recv --json` a bare `{"v":1,"event":...}` object
/// ([`Self::to_json`]), human `recv` a text line ([`Self::to_human`]), and the
/// upcoming `moyu session` the same JSON wrapped in a `"type":"event"` frame.
pub(crate) enum SessionEvent {
    /// Accepted a pending group invite (Welcome).
    Joined { group: String, name: String },
    /// A kind-7 reaction, keyed to the message it targets (`e` tag).
    Reaction {
        group: String,
        sender: String,
        sender_name: Option<String>,
        emoji: String,
        target: String,
        ts: u64,
        message_id: String,
    },
    /// A kind-1202 agent operation (`is_op`) or kind-1201 agent activity —
    /// `content` is the event's decoded JSON payload, kept whole because the
    /// JSON rendering surfaces a dynamic subset of its fields.
    Bot {
        is_op: bool,
        group: String,
        channel: String,
        sender: String,
        sender_name: Option<String>,
        kind: u64,
        content: serde_json::Value,
        ts: u64,
        message_id: String,
    },
    /// A `{"moyu":{"type":"join-request",...}}` envelope received as a DM.
    /// `ws_name` is the LOCALLY-verified display name (never the envelope's
    /// attacker-controlled claim — see `join_request_trust`).
    JoinRequest {
        npub: String,
        sender: String,
        ws_name: String,
        ws_gid: String,
        trusted: bool,
        ts: u64,
    },
    /// A real chat line (kind-9), channel-decoded, with any attachments and
    /// the reply target (`q` tag) it carries. `dm` marks a 1:1 DM group
    /// (MLS-protected profile name, [`crate::domain::dm_group_hexes`]) —
    /// without it a consumer can't tell a DM from a workspace `#general`
    /// post, since an unenveloped DM body also decodes to the default slug
    /// (the desktop shell titles its OS notifications off this). Additive
    /// field: the JSON contract is stable, existing consumers ignore it.
    Message {
        group: String,
        channel: String,
        dm: bool,
        sender: String,
        sender_name: Option<String>,
        kind: u64,
        ts: u64,
        message_id: String,
        body: String,
        attachments: Vec<MediaAttachmentReference>,
        reply_to: Option<String>,
    },
    /// A join request auto-approved by the tick's self-healing scan (the
    /// historical wire shape carries `"ok":true` unlike other events — kept).
    Approved {
        npub: String,
        ws_name: String,
        auto: bool,
    },
    /// One governance notice line derived from a roster diff (already
    /// formatted — the JSON contract carries the same text).
    Governance { text: String },
    /// §5.2 fallback re-broadcast catch-up snapshots for newly-joined members.
    SnapshotRebroadcast { count: usize },
}

impl SessionEvent {
    /// The bare `{"v":1,"event":...}` object `recv --json` emits — field set
    /// and construction unchanged from the pre-extraction inline code (the
    /// JSON contract is stable; bots depend on it).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        match self {
            SessionEvent::Joined { group, name } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "event": "joined",
                "group": group,
                "name": name,
            }),
            SessionEvent::Reaction {
                group,
                sender,
                sender_name,
                emoji,
                target,
                ts,
                message_id,
            } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "event": "reaction",
                "group": group,
                "sender": sender,
                "sender_name": sender_name,
                "emoji": emoji,
                "target": target,
                "ts": ts,
                "message_id": message_id,
            }),
            SessionEvent::Bot {
                is_op,
                group,
                channel,
                sender,
                sender_name,
                kind,
                content,
                ts,
                message_id,
            } => {
                let mut event = serde_json::json!({
                    "v": SCHEMA_VERSION,
                    "event": if *is_op { "agent_op" } else { "agent_activity" },
                    "group": group,
                    "channel": channel,
                    "sender": sender,
                    "sender_name": sender_name,
                    "kind": kind,
                    "status": content.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                    "text": content.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    "ts": ts,
                    "message_id": message_id,
                });
                if *is_op && let Some(obj) = event.as_object_mut() {
                    for key in ["event_type", "name", "run_id", "preview"] {
                        if let Some(s) = content.get(key).and_then(|v| v.as_str()) {
                            obj.insert(key.to_owned(), serde_json::json!(s));
                        }
                    }
                    if let Some(b) = content.get("ok").and_then(|v| v.as_bool()) {
                        obj.insert("ok".to_owned(), serde_json::json!(b));
                    }
                    if let Some(n) = content.get("duration_ms").and_then(|v| v.as_u64()) {
                        obj.insert("duration_ms".to_owned(), serde_json::json!(n));
                    }
                }
                // Surface the sender's own `details` (op) / `extra` (activity)
                // object, minus the internal `moyu` routing key, so a
                // downstream `recv --json | jq` sees the fields a bot attached
                // (e.g. a CI run_url). Omitted when nothing but routing remains.
                let payload_key = if *is_op { "details" } else { "extra" };
                if let Some(payload) = content.get(payload_key).and_then(|v| v.as_object()) {
                    let user_fields: serde_json::Map<String, serde_json::Value> = payload
                        .iter()
                        .filter(|(k, _)| k.as_str() != "moyu")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if !user_fields.is_empty()
                        && let Some(obj) = event.as_object_mut()
                    {
                        obj.insert(
                            payload_key.to_owned(),
                            serde_json::Value::Object(user_fields),
                        );
                    }
                }
                event
            }
            SessionEvent::JoinRequest {
                npub,
                sender,
                ws_name,
                ws_gid,
                trusted,
                ts,
            } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "event": "join_request",
                "npub": npub,
                "sender": sender,
                "ws_name": ws_name,
                "ws_gid": ws_gid,
                "trusted": trusted,
                "ts": ts,
            }),
            SessionEvent::Message {
                group,
                channel,
                dm,
                sender,
                sender_name,
                kind,
                ts,
                message_id,
                body,
                attachments,
                reply_to,
            } => {
                let mut event = serde_json::json!({
                    "v": SCHEMA_VERSION,
                    "event": "message",
                    "group": group,
                    "channel": channel,
                    "dm": dm,
                    "sender": sender,
                    "sender_name": sender_name,
                    "kind": kind,
                    "ts": ts,
                    "message_id": message_id,
                    "body": body,
                });
                if !attachments.is_empty() {
                    event["attachments"] = serde_json::Value::Array(
                        attachments
                            .iter()
                            .map(|a| {
                                serde_json::json!({
                                    "file_name": a.file_name,
                                    "media_type": a.media_type,
                                    "ciphertext_sha256": a.ciphertext_sha256,
                                })
                            })
                            .collect(),
                    );
                }
                if let Some(parent) = reply_to {
                    event["reply_to"] = serde_json::Value::String(parent.clone());
                }
                event
            }
            SessionEvent::Approved {
                npub,
                ws_name,
                auto,
            } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "ok": true,
                "event": "approve",
                "npub": npub,
                "ws_name": ws_name,
                "auto": auto,
            }),
            SessionEvent::Governance { text } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "event": "governance",
                "text": text,
            }),
            SessionEvent::SnapshotRebroadcast { count } => serde_json::json!({
                "v": SCHEMA_VERSION,
                "event": "snapshot_rebroadcast",
                "count": count,
            }),
        }
    }

    /// The human `recv` line — text unchanged from the pre-extraction inline
    /// `println!` branches.
    pub(crate) fn to_human(&self) -> String {
        /// `sender_name` falling back to the sender hex — the `who` of every
        /// human line.
        fn who(sender: &str, sender_name: &Option<String>) -> String {
            sender_name.clone().unwrap_or_else(|| sender.to_owned())
        }
        fn short(s: &str, n: usize) -> String {
            s.chars().take(n).collect()
        }
        match self {
            SessionEvent::Joined { group, name } => {
                format!("Accepted invite -- joined group {group} (\"{name}\").")
            }
            SessionEvent::Reaction {
                group,
                sender,
                sender_name,
                emoji,
                target,
                ..
            } => {
                format!(
                    "[{}] {} reacted {emoji} to {}…",
                    short(group, 8),
                    who(sender, sender_name),
                    short(target, 8)
                )
            }
            SessionEvent::Bot {
                is_op,
                group,
                channel,
                sender,
                sender_name,
                content,
                ..
            } => {
                let bot_body = crate::domain::format_bot_event_body(content, *is_op);
                format!(
                    "[{}] 🤖 {} #{channel} {}",
                    short(group, 8),
                    who(sender, sender_name),
                    output::indent_continuation(&bot_body)
                )
            }
            SessionEvent::JoinRequest {
                npub,
                ws_name,
                trusted,
                ..
            } => {
                format!(
                    "🔑 {npub} 想加入 #{ws_name}  [{}]  → moyu approve {npub}",
                    crate::domain::trust_badge(*trusted)
                )
            }
            SessionEvent::Message {
                group,
                channel,
                sender,
                sender_name,
                body,
                attachments,
                reply_to,
                ..
            } => {
                let att = if attachments.is_empty() {
                    String::new()
                } else {
                    let names: Vec<String> = attachments
                        .iter()
                        .map(|a| format!("📎 {} ({})", a.file_name, a.media_type))
                        .collect();
                    format!("[{}] ", names.join(", "))
                };
                let reply_marker = match reply_to {
                    Some(parent) => format!("↩{}… ", short(parent, 8)),
                    None => String::new(),
                };
                format!(
                    "[{}] {} [#{channel}]: {reply_marker}{att}{}",
                    short(group, 8),
                    who(sender, sender_name),
                    output::indent_continuation(body)
                )
            }
            SessionEvent::Approved { npub, ws_name, .. } => {
                format!("✅ approved {npub} into #{ws_name}")
            }
            SessionEvent::Governance { text } => text.clone(),
            SessionEvent::SnapshotRebroadcast { count } => {
                format!("[re-broadcast {count} workspace snapshot(s) for newly-joined member(s)]")
            }
        }
    }
}

/// Fallback cadence, in ticks, for `sync_tick`'s auto-approve scan when a
/// tick carries no new message to trigger it directly (see the scan's own
/// comment below for the full reasoning). At the 2s `RECV_POLL_INTERVAL`
/// `recv --follow`/`moyu session` poll on, 15 ticks is a ≤30s worst-case
/// retry latency.
const AUTO_APPROVE_FALLBACK_TICKS: u64 = 15;

/// `sync_tick` call counter driving the auto-approve scan's cadence.
/// Process-global (not per-session state) since `moyu session` holds exactly
/// one account per process anyway; starts at 0 so `fetch_add`'s first return
/// is 0, and `0.is_multiple_of(AUTO_APPROVE_FALLBACK_TICKS)` -- the process's
/// very first tick always scans, so any join requests that piled up while
/// the process was down get handled immediately rather than waiting out the
/// fallback cadence.
static SYNC_TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// One bounded receive pass — the body `recv`'s poll loop (and, next, the
/// `moyu session` event pump) runs every tick: refresh the KeyPackage, drain
/// `sync()`, accept Welcomes, classify inbound messages, apply buffered
/// convergence commits, diff rosters into governance notices, re-broadcast
/// catch-up snapshots, and run the self-healing auto-approve scan. Events are
/// delivered through `on_event` AS they are produced (not batched), so a
/// mid-tick error loses nothing already surfaced — the exact semantics the
/// inline loop had. Returns whether the tick made progress (drives the
/// one-shot `recv` idle-exit heuristic).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sync_tick(
    engine: &MoyuEngine,
    client: &mut AppClient,
    moyu_store: &mut MoyuStore,
    account_label: &str,
    allow_loopback: bool,
    prev_rosters: &mut Option<std::collections::HashMap<String, crate::domain::WsRoster>>,
    on_event: &mut dyn FnMut(&SessionEvent),
) -> anyhow::Result<bool> {
    // Re-check each tick so a long-lived `--follow`/session keeps its
    // KeyPackage fresh mid-run, not just at startup (cheap local timestamp
    // check; actually publishes at most ~once every 60 days).
    // This handle lives as long as the session. Pick up what another process
    // wrote since the last tick (an invite issued or revoked in another
    // terminal) before anything below reads it. Best-effort: on a read
    // error keep the copy we have.
    let _ = moyu_store.reload();
    crate::domain::refresh_keypackage(client, moyu_store).await;
    let summary = client.sync().await?;
    let mut progressed = !summary.joined_groups.is_empty() || !summary.messages.is_empty();

    for group_id in &summary.joined_groups {
        match client.accept_group_invite(group_id) {
            Ok(record) => {
                on_event(&SessionEvent::Joined {
                    group: group_id.to_string(),
                    name: record.profile.name.clone(),
                });
                // Only a genuine 1:1 DM should become a Contact bound to its
                // group. A workspace or private-channel group has a non-"dm"
                // profile name; binding a peer contact to it would misroute a
                // later `moyu send <peer>` into that group instead of a DM
                // (the misclassification hazard, guarded here the same way the TUI guards
                // `ensure_chat_for_group`). `profile.name` is MLS-protected and
                // arrives with the Welcome, so this is knowable at join time.
                if record.profile.name == workspace::DM_GROUP_NAME
                    && let Err(e) =
                        crate::domain::save_peer_contact_for_group(client, moyu_store, group_id)
                {
                    heprintln!("[warning: could not remember contact for group {group_id}: {e}]");
                }
            }
            Err(e) => heprintln!("[failed to accept invite for group {group_id}: {e}]"),
        }
    }

    // Which groups are 1:1 DMs, snapshotted once per tick (fail-closed, same
    // MLS-protected profile-name rule the TUI routes by and the Joined
    // contact-binding guard above uses). Taken AFTER the accept-invite loop
    // so a DM joined this very tick already classifies correctly.
    let dm_groups = if summary.messages.is_empty() {
        std::collections::HashSet::new()
    } else {
        crate::domain::dm_group_hexes(engine, account_label)
    };

    for msg in &summary.messages {
        // Reactions (kind-7): surface as their own event/line, keyed to the
        // message they target (the `e` tag) so a bot can watch for 👍/✅
        // acknowledgements. The emoji is the reaction's plaintext content.
        if msg.kind == crate::domain::REACTION_KIND {
            let target = crate::domain::first_tag_value(&msg.tags, crate::domain::EVENT_REF_TAG)
                .unwrap_or("");
            on_event(&SessionEvent::Reaction {
                group: msg.group_id.to_string(),
                sender: msg.sender.clone(),
                sender_name: msg.sender_display_name.clone(),
                emoji: msg.plaintext.trim().to_owned(),
                target: target.to_owned(),
                ts: msg.recorded_at,
                message_id: msg.message_id_hex.clone(),
            });
            continue;
        }
        // Bot/agent events (kind-1201 activity, kind-1202 operation):
        // surface with a 🤖 marker rather than folding them away, so a CI/git/
        // monitoring integration shows up in `recv`. The content is a JSON
        // object (`{v,status,text,...}`); the event records its channel slug
        // under `details.moyu.ch` (op) / `extra.moyu.ch` (activity) — for
        // private channels too — which `bot_event_channel` pulls back out.
        if msg.kind == crate::domain::AGENT_OPERATION_KIND
            || msg.kind == crate::domain::AGENT_ACTIVITY_KIND
        {
            let is_op = msg.kind == crate::domain::AGENT_OPERATION_KIND;
            let content: serde_json::Value =
                serde_json::from_str(&msg.plaintext).unwrap_or_else(|_| serde_json::json!({}));
            let slug =
                crate::domain::bot_event_channel(&content).unwrap_or_else(|| "general".to_owned());
            on_event(&SessionEvent::Bot {
                is_op,
                group: msg.group_id.to_string(),
                channel: slug,
                sender: msg.sender.clone(),
                sender_name: msg.sender_display_name.clone(),
                kind: msg.kind,
                content,
                ts: msg.recorded_at,
                message_id: msg.message_id_hex.clone(),
            });
            continue;
        }
        // Join-request: a plain kind-9 DM whose content is the
        // `{"moyu":{"type":"join-request",...}}` envelope `moyu join` sends.
        // Must be special-cased BEFORE the generic chat rendering below (and
        // before `workspace::decode_channel_body`) or it would print as raw
        // JSON chat text instead of the 🔑 line / `join_request` event a
        // human or scripted `approve` flow expects.
        if msg.kind == crate::domain::CHAT_MESSAGE_KIND
            && let Some(jr) = invite::parse_join_request(&msg.plaintext)
        {
            let npub = moyu_core::identity::npub_from_hex(&msg.sender)
                .unwrap_or_else(|_| msg.sender.clone());
            // Resolve the name + trust badge from the envelope's *gid*, never
            // its attacker-controlled `ws_name` (a human drives `approve` off this very line).
            let (ws_display, trusted) = crate::domain::join_request_trust(
                engine,
                client,
                moyu_store,
                account_label,
                &jr,
                keypackage_rotation::now_unix_secs(),
            );
            on_event(&SessionEvent::JoinRequest {
                npub,
                sender: msg.sender.clone(),
                ws_name: ws_display,
                ws_gid: jr.ws_gid_hex.clone(),
                trusted,
                ts: msg.recorded_at,
            });
            continue;
        }
        // Skip edits/deletes/group-system (moyu.* control events, MDK's own
        // member_added, ...) -- only real chat (kind 9) is a display line;
        // control events are folded into the workspace projection via
        // `engine.messages()` instead, matching tui.rs's live-sync filter.
        if msg.kind != crate::domain::CHAT_MESSAGE_KIND {
            continue;
        }
        // A kind-9 carrying a `q` quote tag is a threaded reply; the value
        // is the parent's `message_id`. Plain chat has no `e`/`q` tag.
        let reply_to = crate::domain::first_tag_value(&msg.tags, crate::domain::QUOTE_REF_TAG);
        // Decode the `{"moyu":1,"ch","body"}` channel envelope (workspace
        // posts) -- legacy/unenveloped 1:1 plaintext decodes to `#general`
        // with the original text preserved verbatim, so this is safe for
        // both DM and workspace messages.
        let (slug, body) = workspace::decode_channel_body(&msg.plaintext);
        // Attachments: a media message is kind-9 with one imeta
        // tag per attachment (the ref) plus the caption in plaintext. Parse
        // the imeta tags into refs for display / --json; a tag we can't
        // parse (e.g. a loopback URL without --dev-allow-loopback) is simply
        // dropped from the rendered list, never fatal.
        let attachments: Vec<MediaAttachmentReference> = msg
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
            .filter_map(|tag| {
                moyu_core::engine::media_attachment_from_imeta_tag(
                    tag,
                    Some(msg.source_epoch),
                    allow_loopback,
                )
                .ok()
            })
            .collect();
        on_event(&SessionEvent::Message {
            group: msg.group_id.to_string(),
            channel: slug,
            dm: dm_groups.contains(&msg.group_id.to_string()),
            sender: msg.sender.clone(),
            sender_name: msg.sender_display_name.clone(),
            kind: msg.kind,
            ts: msg.recorded_at,
            message_id: msg.message_id_hex.clone(),
            body,
            attachments,
            reply_to: reply_to.map(str::to_owned),
        });
    }

    // Apply any peer handshake commits (kick / promote / add) that a prior
    // sync only *buffered* into MDK's convergence subsystem -- see
    // `drive_convergence`. Counts as progress so a one-shot `recv` keeps
    // polling until a buffered commit settles rather than exiting early.
    if crate::domain::drive_convergence(engine, client, account_label).await {
        progressed = true;
    }

    // Governance notices: diff the roster/admin snapshot across this pass and
    // surface a line for each kick / leave / promote / demote / peer join
    // (and a "you were removed" if I'm the one who got kicked). These arrive
    // ONLY via convergence, whose `SendSummary` has no events, so a roster
    // diff is the only signal (see `governance` module docs / `drive_convergence`).
    // Only diff when BOTH the baseline and this snapshot read cleanly (`Some`);
    // a degraded snapshot must not be diffed (it would fabricate departures /
    // demotions) nor overwrite the good baseline -- skip and retry next poll.
    let after_rosters = crate::domain::snapshot_rosters(engine, client, account_label);
    let mut peer_joined = false;
    if let (Some(before), Some(after)) = (prev_rosters.as_ref(), after_rosters.as_ref()) {
        let gov_lines = crate::domain::governance_diff(before, after);
        for line in &gov_lines {
            on_event(&SessionEvent::Governance {
                text: crate::domain::format_gov_line(line),
            });
        }
        // A governance change counts as progress so a one-shot `recv` keeps
        // polling until any follow-on settles rather than exiting on this tick.
        if !gov_lines.is_empty() {
            progressed = true;
        }
        peer_joined = crate::domain::any_peer_joined(&gov_lines);
    }
    if after_rosters.is_some() {
        *prev_rosters = after_rosters;
    }

    // §5.2 fallback: re-broadcast catch-up snapshots for any workspace whose
    // join isn't yet covered when a member was added. Over the wire an
    // existing member observes the join as a roster-diff `Joined`
    // (`peer_joined`), NOT as a `SyncSummary.events` MemberAdded
    // (`saw_member_added`, which only fires on the local same-tick apply) --
    // trigger on either. Gated so the common chat/idle poll pays nothing; our
    // own snapshot is `moyu.workspace.snapshot`, not a member-add, so this
    // never feeds back on itself.
    if crate::domain::saw_member_added(&summary) || peer_joined {
        let sent = crate::domain::broadcast_catch_up_snapshots(engine, client, account_label).await;
        if sent > 0 {
            on_event(&SessionEvent::SnapshotRebroadcast { count: sent });
        }
    }

    // Self-healing auto-approve: scan DM history for pending join
    // requests and auto-approve any whose secret matches a LOCAL issued
    // invite recorded with `auto_approve == true`, so the inviter never has
    // to run `moyu approve` by hand. Re-opens `MoyuStore` fresh each scan
    // (rather than reusing the `moyu_store` opened once by the caller) so a
    // `moyu invite --auto-approve` issued in another terminal mid-session is
    // picked up without a restart. Not tied to `saw_member_added` /
    // `peer_joined` above -- scanning the whole history directly makes this
    // naturally idempotent (`approve_one` no-ops on an existing member) and
    // retry-safe across polls (a KeyPackage-propagation failure this tick just
    // gets retried next poll, never lost).
    //
    // Gated (not run every tick, unlike the rest of this function): the scan
    // is `scan_join_requests` -> `engine.messages(label)`, an O(whole-account-
    // history) SQLCipher decrypt. At the 2s tick a long-lived `moyu session`
    // runs indefinitely, doing that on every idle tick forever is wasteful
    // for a scan that's overwhelmingly a no-op. So: a tick that actually
    // decrypted a new message runs the scan immediately (a fresh join
    // request needs zero added latency) -- everything else is a pure retry
    // path (self-healing a stalled KeyPackage propagation), which tolerates
    // a periodic fallback sweep instead of a per-tick one. `SYNC_TICK_COUNT`
    // starts at 0, so the process's first tick always scans too (handles any
    // requests that piled up while the process was down).
    let tick_n = SYNC_TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    let should_scan_join_requests =
        !summary.messages.is_empty() || tick_n.is_multiple_of(AUTO_APPROVE_FALLBACK_TICKS);
    if should_scan_join_requests {
        // Only requests that are still open AND may be approved without a
        // human: a valid, unexpired `--auto-approve` invite for that exact
        // workspace, from someone who was never removed from it. A request
        // that was already answered is not open any more, so a member who is
        // kicked (or leaves) is not put back by their old request.
        // (`moyu_store` was re-read at the top of this tick, so an invite
        // issued or revoked in another terminal is already visible.)
        let open = crate::domain::scan_join_requests(engine, moyu_store, account_label)
            .unwrap_or_default();
        for pj in open.into_iter().filter(|pj| pj.auto_ok) {
            match crate::domain::approve_one(engine, client, moyu_store, account_label, &pj, true)
                .await
            {
                Ok(ApproveOutcome::Approved {
                    npub,
                    ws_name,
                    auto,
                }) => on_event(&SessionEvent::Approved {
                    npub,
                    ws_name,
                    auto,
                }),
                // Idempotent no-op: the request was already fulfilled (by this
                // loop on an earlier tick, or another admin). The pre-refactor
                // inline loop printed a human "already in" line here EVERY
                // tick for every stale auto-approve request -- tick-spam,
                // deliberately dropped when `approve_one` became print-free.
                Ok(ApproveOutcome::AlreadyMember { .. }) => {}
                Err(e) => heprintln!("[auto-approve for #{} deferred: {e}]", pj.ws_name),
            }
        }
    }

    Ok(progressed)
}

// ---------------------------------------------------------------------------
// A-2: the remaining one-shot command cores (workspace/channel messaging,
// admin/setup mutations). Same shape as `send`/`sync_tick` above: already-
// resolved inputs + already-open handles in, a typed receipt out, never a
// `println!`. Argument resolution that reads stdin/tty (message bodies,
// passphrases) stays in the `cmd_*` wrapper in `main.rs`.
//
// A handful of multi-step mutations (`init`, `workspace_leave`) need to
// surface PARTIAL progress even when a later step in the same sequence fails
// (the whole point of "reports how far it got" -- see `workspace_leave`'s doc
// comment); [`progress`] is the one place that decides where such a line
// goes: stdout in human mode (unchanged), stderr under `--json` so stdout
// stays exactly one JSON object even on eventual failure.
// ---------------------------------------------------------------------------

fn progress(line: &str) {
    if output::json_mode() {
        heprintln!("{line}");
    } else {
        hprintln!("{line}");
    }
}

/// What a `post` should carry -- the workspace-channel counterpart of
/// [`SendBody`].
pub(crate) enum PostBody {
    Text(String),
    File {
        path: PathBuf,
        caption: Option<String>,
    },
}

/// Typed receipt of a `post`.
pub(crate) struct PostReceipt {
    pub group: String,
    pub channel: String,
    pub private: bool,
    pub published: usize,
    pub message_ids: Vec<String>,
    /// `Some` only on the attachment path.
    pub attachments: Option<AttachmentParts>,
}

impl PostReceipt {
    /// The shared `post` receipt body — identical in the one-shot `--json`
    /// object and the session `post` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "group": self.group,
            "channel": self.channel,
            "private": self.private,
            "kind": crate::domain::CHAT_MESSAGE_KIND,
            "published": self.published,
            "message_ids": self.message_ids,
        });
        if let Some(parts) = &self.attachments {
            v["attachments"] = serde_json::Value::Array(parts.json.clone());
        }
        v
    }
}

/// `post <ws> <channel> <message>`'s core: resolve the channel target (private
/// channel -> its own group; public channel -> the workspace group after
/// confirming it exists), then send the `{ch,body}`-enveloped chat message or
/// attachment.
pub(crate) async fn post(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    channel: &str,
    body: PostBody,
    blossom: Option<String>,
) -> anyhow::Result<PostReceipt> {
    let (target_gid, group_hex, is_private, slug) =
        crate::domain::resolve_channel_target(engine, client, label, ws, channel)?;

    match body {
        PostBody::File { path, caption } => {
            // The caption carries the `{"moyu":1,..}` channel envelope so a
            // workspace-channel attachment keeps its channel routing on the
            // receiver, exactly like a text post does.
            let caption_env =
                workspace::encode_channel_body(&slug, caption.as_deref().unwrap_or(""));
            let (published, message_ids, attachments) =
                upload_attachment(client, &target_gid, &path, Some(caption_env), blossom).await?;
            Ok(PostReceipt {
                group: group_hex,
                channel: slug,
                private: is_private,
                published,
                message_ids,
                attachments: Some(attachments),
            })
        }
        PostBody::Text(text) => {
            let summary = client
                .send(
                    &target_gid,
                    workspace::encode_channel_body(&slug, &text).as_bytes(),
                )
                .await?;
            Ok(PostReceipt {
                group: group_hex,
                channel: slug,
                private: is_private,
                published: summary.published,
                message_ids: summary.message_ids,
                attachments: None,
            })
        }
    }
}

/// Typed receipt of a `reply`.
pub(crate) struct ReplyReceipt {
    pub group: String,
    pub target: String,
    pub published: usize,
    pub message_ids: Vec<String>,
}

impl ReplyReceipt {
    /// The shared `reply` receipt body — identical in the one-shot `--json`
    /// object and the session `reply` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "target": self.target,
            "kind": crate::domain::CHAT_MESSAGE_KIND,
            "published": self.published,
            "message_ids": self.message_ids,
        })
    }
}

/// `reply <group> <message_id> [text]`'s core: send a first-class threaded
/// reply (kind-9 carrying the parent's `e`+`q` tags), preserving the target's
/// channel envelope (if any) so the reply stays in-channel rather than
/// #general.
pub(crate) async fn reply(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    group: &str,
    message_id: &str,
    text: String,
) -> anyhow::Result<ReplyReceipt> {
    let group_id = crate::domain::resolve_group_id(engine, client, label, group)?;
    let target = message_id.trim();
    let body =
        match crate::domain::target_channel_slug(engine, label, &group_id.to_string(), target) {
            Some(slug) => workspace::encode_channel_body(&slug, &text),
            None => text,
        };
    let summary = client.reply_to_message(&group_id, target, &body).await?;
    Ok(ReplyReceipt {
        group: group_id.to_string(),
        target: target.to_owned(),
        published: summary.published,
        message_ids: summary.message_ids,
    })
}

/// Typed receipt of a `react`/`unreact`.
pub(crate) struct ReactReceipt {
    pub group: String,
    pub target: String,
    /// The emoji reacted with; `None` on the `--remove`/retract path. Kept on
    /// the receipt (rather than left for each caller to echo its own input
    /// back) so both renderers read it from one place.
    pub emoji: Option<String>,
    /// `"react"` or `"unreact"`.
    pub action: &'static str,
    pub published: usize,
    pub message_ids: Vec<String>,
}

impl ReactReceipt {
    /// The shared `react`/`unreact` receipt body — identical in the one-shot
    /// `--json` object and the session `react` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "target": self.target,
            "emoji": self.emoji,
            "action": self.action,
            "published": self.published,
            "message_ids": self.message_ids,
        })
    }
}

/// `react <group> <message_id> [emoji]` / `... --remove`'s core: send a kind-7
/// reaction (`emoji` given) or retract it (`emoji` is `None`, the `--remove`
/// path). Emoji validation (required unless `--remove`) stays at the call site.
pub(crate) async fn react(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    group: &str,
    message_id: &str,
    emoji: Option<&str>,
) -> anyhow::Result<ReactReceipt> {
    let group_id = crate::domain::resolve_group_id(engine, client, label, group)?;
    let target = message_id.trim();
    let (summary, action) = match emoji {
        Some(e) => (
            client.react_to_message(&group_id, target, e).await?,
            "react",
        ),
        None => (
            client.unreact_from_message(&group_id, target).await?,
            "unreact",
        ),
    };
    Ok(ReactReceipt {
        group: group_id.to_string(),
        target: target.to_owned(),
        emoji: emoji.map(str::to_owned),
        action,
        published: summary.published,
        message_ids: summary.message_ids,
    })
}

/// Typed receipt of a `download`.
pub(crate) struct DownloadReceipt {
    pub group: String,
    pub file_name: String,
    pub media_type: String,
    pub size: u64,
    pub path: PathBuf,
}

impl DownloadReceipt {
    /// The shared `download` receipt body — identical in the one-shot
    /// `--json` object and the session `download` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "file_name": self.file_name,
            "media_type": self.media_type,
            "size": self.size,
            "path": self.path.display().to_string(),
        })
    }
}

/// `download <group> <hash> [--out]`'s core: find the attachment by content
/// hash in a group's synced messages, `download_media` (fetch from Blossom,
/// decrypt, verify both hashes), and write the plaintext to disk -- refusing to
/// clobber a SENDER-named path that already exists (a malicious
/// group member could name an attachment `.bashrc`/`.env` and silently
/// overwrite it; an explicit `--out <file>` is the user's own choice and
/// overwrites, like `curl -o`).
pub(crate) async fn download(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    target: &str,
    hash: &str,
    allow_loopback: bool,
    out: Option<PathBuf>,
) -> anyhow::Result<DownloadReceipt> {
    let group_id = crate::domain::resolve_group_id(engine, client, label, target)?;
    let group_hex = group_id.to_string();
    let reference =
        crate::domain::find_attachment_ref(engine, label, &group_hex, hash, allow_loopback)?;
    let download = client.download_media(&group_id, reference).await?;
    let sender_named = out.as_ref().map(|p| p.is_dir()).unwrap_or(true);
    let out_path = crate::domain::resolve_download_path(out, &download.file_name);
    if sender_named && out_path.symlink_metadata().is_ok() {
        anyhow::bail!(
            "refusing to overwrite existing {} (the attachment's name is sender-controlled) \
             -- pass --out <path> to choose a destination",
            out_path.display()
        );
    }
    std::fs::write(&out_path, &download.plaintext)
        .with_context(|| format!("writing downloaded file to {}", out_path.display()))?;
    Ok(DownloadReceipt {
        group: group_hex,
        file_name: download.file_name,
        media_type: download.media_type,
        size: download.size_bytes,
        path: out_path,
    })
}

/// One local-history search hit.
pub(crate) struct SearchHit {
    pub group: String,
    pub channel: String,
    pub sender: String,
    pub direction: String,
    pub kind: u64,
    pub ts: u64,
    pub message_id: String,
    pub body: String,
    pub reply_to: Option<String>,
}

impl SearchHit {
    /// The shared `search` hit body (no `"v"` — the one-shot renderer stamps
    /// one onto each row of its bare top-level array; the session renderer's
    /// `data` is already inside a `"v"`-carrying receipt frame, so its rows
    /// don't need one).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "group": self.group,
            "channel": self.channel,
            "sender": self.sender,
            "direction": self.direction,
            "kind": self.kind,
            "ts": self.ts,
            "message_id": self.message_id,
            "body": self.body,
        });
        if let Some(parent) = &self.reply_to {
            v["reply_to"] = serde_json::Value::String(parent.clone());
        }
        v
    }
}

/// `search <query> [--group] [--limit]`'s core: a pure offline scan of the
/// account's already-decrypted local history (never opens a client -- no relay
/// traffic). `needle` must already be trimmed/lowercased/non-empty and
/// `group_filter` trimmed/lowercased. Chat lines only (kind-9), most-recent
/// first, capped at `limit`.
pub(crate) fn search(
    engine: &MoyuEngine,
    label: &str,
    needle: &str,
    group_filter: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<SearchHit>> {
    let mut records = engine.messages(label)?;
    // Most-recent first (send time), stable on message id across ties.
    records.sort_by(|a, b| {
        b.recorded_at
            .cmp(&a.recorded_at)
            .then_with(|| a.message_id_hex.cmp(&b.message_id_hex))
    });

    let mut hits = Vec::new();
    for r in &records {
        if r.kind != crate::domain::CHAT_MESSAGE_KIND {
            continue;
        }
        if let Some(pref) = group_filter
            && !r.group_id_hex.to_lowercase().starts_with(pref)
        {
            continue;
        }
        let Some((slug, body)) = crate::domain::search_match(&r.plaintext, needle) else {
            continue;
        };
        if hits.len() >= limit {
            break;
        }
        let reply_to = crate::domain::first_tag_value(&r.tags, crate::domain::QUOTE_REF_TAG)
            .map(str::to_owned);
        hits.push(SearchHit {
            group: r.group_id_hex.clone(),
            channel: slug,
            sender: r.sender.clone(),
            direction: r.direction.clone(),
            kind: r.kind,
            ts: r.recorded_at,
            message_id: r.message_id_hex.clone(),
            body,
            reply_to,
        });
    }
    Ok(hits)
}

/// Typed receipt of a `deny`.
pub(crate) struct DenyReceipt {
    pub who: String,
}

impl DenyReceipt {
    /// The shared `deny` receipt body. The one-shot `--json` object
    /// additionally stamps an `"event":"deny"` marker the session receipt
    /// does not carry (the session frame's own `"type":"receipt"` already
    /// says as much) -- that one extra key is layered on by `cmd_deny`
    /// itself, not part of this shared body.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "who": self.who })
    }
}

/// `deny <who>`: v1 is a purely local dismissal -- no event is sent, nothing is
/// persisted (see `main.rs`'s `cmd_deny` doc comment for the full rationale).
pub(crate) fn deny(who: String) -> DenyReceipt {
    DenyReceipt { who }
}

/// Typed receipt of an `op` (kind-1202 agent operation event).
pub(crate) struct OpReceipt {
    pub group: String,
    pub channel: String,
    pub private: bool,
    pub event_type: String,
    pub status: String,
    pub published: usize,
    pub message_ids: Vec<String>,
}

impl OpReceipt {
    /// The shared `op` receipt body — identical in the one-shot `--json`
    /// object and the session `op` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "channel": self.channel,
            "private": self.private,
            "kind": crate::domain::AGENT_OPERATION_KIND,
            "event_type": self.event_type,
            "status": self.status,
            "published": self.published,
            "message_ids": self.message_ids,
        })
    }
}

/// `op <ws> <channel>`'s core: resolve the channel target (same routing as
/// `post`), stamp the channel slug into `details.moyu.ch` (a kind-1202 event
/// carries no kind-9 envelope), and send via `AppClient::send_agent_operation_event`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn op(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    channel: &str,
    text: String,
    event_type: String,
    status: String,
    name: Option<String>,
    run_id: Option<String>,
    ok_flag: Option<bool>,
    duration_ms: Option<u64>,
    preview: Option<String>,
    details: Option<serde_json::Value>,
) -> anyhow::Result<OpReceipt> {
    let (target_gid, group_hex, is_private, slug) =
        crate::domain::resolve_channel_target(engine, client, label, ws, channel)?;
    // A kind-1202 event has structured content, not the `{moyu,ch,body}` kind-9
    // channel envelope, so it records its channel slug in `details.moyu.ch` for
    // the receiver to label it by — for a PRIVATE channel too (mirrors `post`,
    // which envelopes the slug even into a private channel), so `recv`/TUI show
    // the real channel rather than defaulting to #general. Slug is already known
    // to the channel's members, so this is no extra exposure. (D1, M5 spec.)
    let details_val = crate::domain::agent_op_details_with_channel(details, &slug)?;

    let request = AgentOperationEventRequest {
        event_type: event_type.clone(),
        status: status.clone(),
        operation_id: None,
        run_id,
        turn_id: None,
        name,
        text,
        preview,
        details: details_val,
        sequence: None,
        ok: ok_flag,
        duration_ms,
        reply_to_message_id: None,
    };
    let summary = client
        .send_agent_operation_event(&target_gid, request)
        .await?;
    Ok(OpReceipt {
        group: group_hex,
        channel: slug,
        private: is_private,
        event_type,
        status,
        published: summary.published,
        message_ids: summary.message_ids,
    })
}

/// Typed receipt of an `activity` (kind-1201 agent activity line).
pub(crate) struct ActivityReceipt {
    pub group: String,
    pub channel: String,
    pub private: bool,
    pub status: String,
    pub published: usize,
    pub message_ids: Vec<String>,
}

impl ActivityReceipt {
    /// The shared `activity` receipt body — identical in the one-shot
    /// `--json` object and the session `activity` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "channel": self.channel,
            "private": self.private,
            "kind": crate::domain::AGENT_ACTIVITY_KIND,
            "status": self.status,
            "published": self.published,
            "message_ids": self.message_ids,
        })
    }
}

/// `activity <ws> <channel>`'s core: the lightweight companion to [`op`] --
/// same channel routing and `moyu.ch` stamping (under `extra` rather than
/// `details`), via `AppClient::send_agent_activity`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn activity(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    channel: &str,
    text: String,
    status: String,
    extra: Option<serde_json::Value>,
) -> anyhow::Result<ActivityReceipt> {
    let (target_gid, group_hex, is_private, slug) =
        crate::domain::resolve_channel_target(engine, client, label, ws, channel)?;
    // Stamp the slug into `extra.moyu.ch` (kind-1201 carries no kind-9 envelope
    // either), for a private channel too, so `recv`/TUI label it correctly
    // rather than defaulting to #general — same convention as `op`'s `details`.
    let extra_val = crate::domain::agent_op_details_with_channel(extra, &slug)?;

    let summary = client
        .send_agent_activity(&target_gid, status.clone(), text, None, extra_val)
        .await?;
    Ok(ActivityReceipt {
        group: group_hex,
        channel: slug,
        private: is_private,
        status,
        published: summary.published,
        message_ids: summary.message_ids,
    })
}

/// Which invite `ops::invite` is minting -- a plain contact invite needs no
/// engine/client (pure token construction); a workspace invite needs an
/// already-open engine/client/store to check admin status and persist the
/// issued-invite record. Kept as an enum (rather than `Option` tuples) so the
/// call site's `match &workspace` shape in `cmd_invite` maps onto it directly.
pub(crate) enum InviteScope<'a> {
    Contact,
    Workspace {
        engine: &'a MoyuEngine,
        client: &'a AppClient,
        store: &'a mut MoyuStore,
        slug: &'a str,
    },
}

/// Typed receipt of an `invite`.
pub(crate) struct InviteReceipt {
    pub token: String,
    /// `"workspace"` or `"contact"`.
    pub kind: &'static str,
    pub ws_name: Option<String>,
}

impl InviteReceipt {
    /// The shared `invite` receipt body — identical in the one-shot `--json`
    /// object and the session `invite` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "token": self.token,
            "kind": self.kind,
            "ws_name": self.ws_name,
        })
    }
}

/// Typed receipt of `invite <workspace> --revoke`.
pub(crate) struct InviteRevokeReceipt {
    pub group: String,
    pub workspace: String,
    pub revoked: usize,
}

impl InviteRevokeReceipt {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "workspace": self.workspace,
            "revoked": self.revoked,
        })
    }
}

/// `invite <workspace> --revoke`'s core: forget every invite this account
/// issued for that workspace, so codes already handed out are no longer
/// trusted or auto-approved. Purely local (the codes only ever meant anything
/// to the account that issued them); members who already joined stay.
pub(crate) fn invite_revoke(
    engine: &MoyuEngine,
    client: &AppClient,
    store: &mut MoyuStore,
    account_label: &str,
    slug: &str,
) -> anyhow::Result<InviteRevokeReceipt> {
    let (gid, proj) = crate::domain::resolve_workspace(engine, client, account_label, slug)?;
    let revoked = store.revoke_issued_invites(&gid)?;
    Ok(InviteRevokeReceipt {
        group: gid,
        workspace: proj.name.unwrap_or_else(|| slug.to_owned()),
        revoked,
    })
}

/// `invite [<workspace>] [--auto-approve]`'s core: mint an invite
/// token. A contact invite is pure token construction; a workspace invite
/// additionally checks the local account is (ideally) an admin of `slug`,
/// mints a bearer secret, and persists an [`store::IssuedInvite`] record (for
/// `requests`' ✓ badge and `recv`'s self-healing auto-approve).
pub(crate) fn invite(
    account_label: &str,
    relays: &[String],
    scope: InviteScope<'_>,
    auto_approve: bool,
) -> anyhow::Result<InviteReceipt> {
    let inviter: [u8; 32] = hex::decode(account_label)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("account label is not a 32-byte pubkey hex"))?;

    let (kind, ws_gid, ws_name, secret) = match scope {
        InviteScope::Contact => {
            if auto_approve {
                anyhow::bail!("--auto-approve only applies to a workspace invite");
            }
            (invite::INVITE_KIND_CONTACT, None, None, None)
        }
        InviteScope::Workspace {
            engine,
            client,
            store,
            slug,
        } => {
            let (gid, me, admins, proj) =
                crate::domain::workspace_gov(engine, client, account_label, slug)?;
            let am_admin = governance::is_admin(&me, &admins);
            if auto_approve && !am_admin {
                anyhow::bail!(
                    "--auto-approve refused: you are not an admin of workspace {}, so approve would fail",
                    slug
                );
            }
            if !am_admin {
                heprintln!(
                    "warning: you are not an admin of {slug}; you won't be able to `approve` join requests for this code"
                );
            }
            let name = proj.name.clone().unwrap_or_else(|| slug.to_owned());
            let secret_hex = invite::new_secret_hex();
            // `GroupId` is an opaque byte string (MDK's `byte_id!` wraps a
            // `Vec<u8>`; OpenMLS's default group id is 16 random bytes, not a
            // 32-byte Nostr-pubkey-shaped value), so this is NOT a fixed-32
            // decode -- `invite::InviteToken::ws_gid` carries it length-prefixed.
            let gid_bytes: Vec<u8> = hex::decode(gid.to_string())?;
            let secret_bytes: [u8; 16] = hex::decode(&secret_hex)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("bad secret len"))?;
            // 持久化签发记录(供 requests ✓ 徽标 + auto-approve)
            let created_at = keypackage_rotation::now_unix_secs();
            store.record_issued_invite(store::IssuedInvite {
                secret_hex,
                ws_gid_hex: gid.to_string(),
                ws_name: name.clone(),
                auto_approve,
                created_at,
                expires_at: Some(created_at.saturating_add(store::INVITE_TTL_SECS)),
            })?;
            (
                invite::INVITE_KIND_WORKSPACE,
                Some(gid_bytes),
                Some(name),
                Some(secret_bytes),
            )
        }
    };

    let label_opt = moyu_core::identity::npub_from_hex(account_label).ok();
    let token = invite::encode_token(&invite::InviteToken {
        v: 1,
        kind,
        inviter,
        relays: relays.to_vec(),
        label: label_opt,
        ws_gid,
        ws_name: ws_name.clone(),
        secret,
    })?;
    let kind_str = if kind == invite::INVITE_KIND_WORKSPACE {
        "workspace"
    } else {
        "contact"
    };
    Ok(InviteReceipt {
        token,
        kind: kind_str,
        ws_name,
    })
}

/// Typed receipt of a `join`.
pub(crate) struct JoinReceipt {
    pub peer_npub: String,
    /// `"contact"` or `"workspace"`.
    pub kind: &'static str,
    pub ws_name: Option<String>,
    pub published: usize,
}

impl JoinReceipt {
    /// The body shared by all THREE `join` call sites: the one-shot `--json`
    /// object (which additionally wraps `v`/`ok`/`event":"join"`), the
    /// session `join` receipt from the no-account state (which additionally
    /// merges in `state`/`label`/`npub`, the unlock-equivalent fields), and
    /// the session `join` receipt from the unlocked state (which uses this
    /// body verbatim, no extras). All three fields sets are supersets of this
    /// one, so a single body suffices — no shape actually diverges here.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "inviter": self.peer_npub,
            "ws_name": self.ws_name,
            "published": self.published,
        })
    }
}

/// `join <code>`'s core (post-passphrase-prompt, post-engine-open): resolve or
/// inline-create the local account, refuse a self-invite, ensure our
/// KeyPackage is published, then branch on the token kind -- a contact code
/// DMs the inviter a hello; a workspace code DMs a join-request envelope for
/// them to `moyu approve`. Progress lines stay `eprintln!` (stderr) exactly as
/// before, mirroring `find_or_create_dm`'s stderr-routed diagnostics.
pub(crate) async fn join(
    engine: &MoyuEngine,
    data_dir: &Path,
    effective_relays: &[String],
    socks5: Option<SocketAddr>,
    token: invite::InviteToken,
) -> anyhow::Result<JoinReceipt> {
    // No identity yet in this data-dir? `join` is often someone's FIRST moyu
    // command, so inline-init one rather than making them run `moyu init`
    // first. Already have one? Reuse it (repeat/second `join` in the same
    // data-dir).
    let account_label = match store::read_active_account_label(data_dir)? {
        Some(l) => l,
        None => {
            heprintln!("No identity yet; creating one and publishing your KeyPackage...");
            let result = engine
                .create_or_import_account(
                    None,
                    effective_relays.to_vec(),
                    effective_relays.to_vec(),
                    true,
                )
                .await?;
            store::write_active_account_label(data_dir, &result.account.label)?;
            result.account.label
        }
    };

    // self-invite guard: joining your own code is a no-op at best, confusing
    // at worst (you'd DM yourself).
    if hex::encode(token.inviter) == account_label {
        anyhow::bail!("that invite code is your own -- share it with someone else to join you");
    }

    // Make sure our KeyPackage is published to the (now-merged) relay set --
    // idempotent, so the inviter's `approve`/reply-DM can always fetch it,
    // even if we already had an identity that never got around to it.
    let _ = engine.publish_key_package(&account_label).await;

    let mut moyu_store = MoyuStore::open(data_dir, &account_label)?;
    let mut client = engine.client(&account_label).await?;
    let inviter_npub = moyu_core::identity::npub_from_hex(&hex::encode(token.inviter))?;

    match token.kind {
        invite::INVITE_KIND_CONTACT => {
            let (peer_npub, gid) = crate::domain::find_or_create_dm(
                &mut client,
                &mut moyu_store,
                &inviter_npub,
                socks5,
            )
            .await?;
            let summary = client
                .send(&gid, "\u{1F44B} joined via invite".as_bytes())
                .await?;
            heprintln!("Said hi to {peer_npub}. They'll see you after they run `moyu recv`.");
            Ok(JoinReceipt {
                peer_npub,
                kind: "contact",
                ws_name: None,
                published: summary.published,
            })
        }
        invite::INVITE_KIND_WORKSPACE => {
            let ws_name = token.ws_name.clone().unwrap_or_default();
            let ws_gid_hex = token
                .ws_gid
                .map(hex::encode)
                .ok_or_else(|| anyhow::anyhow!("workspace code missing group id"))?;
            let secret_hex = token
                .secret
                .map(hex::encode)
                .ok_or_else(|| anyhow::anyhow!("workspace code missing secret"))?;
            let (peer_npub, gid) = crate::domain::find_or_create_dm(
                &mut client,
                &mut moyu_store,
                &inviter_npub,
                socks5,
            )
            .await?;
            let content = invite::build_join_request_content(&invite::JoinRequest {
                ws_gid_hex,
                ws_name: ws_name.clone(),
                secret_hex,
            });
            let summary = client.send(&gid, content.as_bytes()).await?;
            heprintln!(
                "Requested to join #{ws_name}. Waiting for {peer_npub} to `moyu approve` you."
            );
            Ok(JoinReceipt {
                peer_npub,
                kind: "workspace",
                ws_name: Some(ws_name),
                published: summary.published,
            })
        }
        k => anyhow::bail!("unknown invite kind {k}"),
    }
}

/// One row of `requests`: a pending join request, resolved to its display
/// name and trust badge.
pub(crate) struct RequestRow {
    pub npub: String,
    pub ws_name: String,
    pub ws_gid: String,
    pub trusted: bool,
    pub ts: u64,
}

impl RequestRow {
    /// The shared `requests` row body (no `"v"` — see [`SearchHit::to_json`]
    /// for why the one-shot bare-array renderer stamps one on per row and
    /// the session renderer does not).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "npub": self.npub,
            "ws_name": self.ws_name,
            "ws_gid": self.ws_gid,
            "trusted": self.trusted,
            "ts": self.ts,
        })
    }
}

/// `requests`'s core: every pending join request, skipping anyone already a
/// member of the target workspace (their request has already been fulfilled).
/// Each is resolved to the REAL local workspace name (never the envelope's
/// attacker-controlled `ws_name`) and a trust flag (its
/// secret matches an invite this account actually issued for that exact
/// workspace).
pub(crate) fn requests(
    engine: &MoyuEngine,
    client: &AppClient,
    store: &MoyuStore,
    account_label: &str,
) -> anyhow::Result<Vec<RequestRow>> {
    let mut rows = Vec::new();
    for pj in crate::domain::scan_join_requests(engine, store, account_label)? {
        // Already a member (this request has already been fulfilled) -> skip.
        if let Ok(gid) = crate::domain::group_id_from_hex(&pj.ws_gid_hex)
            && let Ok(members) = client.members(&gid)
            && members
                .iter()
                .any(|m| m.member_id_hex.eq_ignore_ascii_case(&pj.sender_hex))
        {
            continue;
        }
        let trusted = pj.trusted;
        // Exact-gid resolution: `pj.ws_gid_hex` is a full group
        // id from an untrusted envelope, so it must not be fuzzy prefix/name
        // matched. Real local name, never the envelope's claimed `ws_name`.
        let display_ws =
            crate::domain::resolve_workspace_by_gid(engine, client, account_label, &pj.ws_gid_hex)
                .ok()
                .and_then(|(_, proj)| proj.name)
                .unwrap_or_else(|| pj.ws_name.clone());
        let npub = moyu_core::identity::npub_from_hex(&pj.sender_hex)
            .unwrap_or_else(|_| pj.sender_hex.clone());
        rows.push(RequestRow {
            npub,
            ws_name: display_ws,
            ws_gid: pj.ws_gid_hex,
            trusted,
            ts: pj.ts,
        });
    }
    Ok(rows)
}

/// `approve <npub|hex|all>`'s core: add the sender(s) of matching pending join
/// request(s) into their target workspace. `who == "all"` approves every
/// pending request (across every workspace); otherwise only requests from that
/// one npub/hex. Each match is handled by `approve_one` (unchanged, in
/// `main.rs` -- it is shared with `sync_tick`'s self-healing auto-approve and
/// already renders its own receipt/println, gated on `output::json_mode()`,
/// exactly as before this extraction).
/// What one `approve_one` actually did -- the caller renders (one-shot
/// `cmd_approve` prints; the session/tick paths emit a
/// [`SessionEvent::Approved`] or stay silent on the idempotent no-op).
pub(crate) enum ApproveOutcome {
    /// The sender was already a member of the target workspace (a re-run
    /// after a partial failure, another admin won the race, or an earlier
    /// tick already approved) -- an idempotent success, not an error.
    AlreadyMember { npub: String, ws_name: String },
    Approved {
        npub: String,
        ws_name: String,
        auto: bool,
    },
}

pub(crate) async fn approve(
    engine: &MoyuEngine,
    client: &mut AppClient,
    store: &mut MoyuStore,
    account_label: &str,
    who: String,
    workspace: Option<&str>,
    auto: bool,
) -> anyhow::Result<(Vec<ApproveOutcome>, Vec<String>)> {
    // A session's store handle is long-lived; see what is on disk now.
    let _ = store.reload();
    let mut pending = crate::domain::scan_join_requests(engine, store, account_label)?;
    if let Some(ws) = workspace {
        let (gid, _) = crate::domain::resolve_workspace(engine, client, account_label, ws)?;
        pending.retain(|p| p.ws_gid_hex.eq_ignore_ascii_case(&gid));
    }
    let targets: Vec<crate::domain::PendingJoin> = if who == "all" {
        pending
    } else {
        let want = moyu_core::identity::parse_npub_or_hex(&who)
            .map(|pk| pk.to_hex())
            .unwrap_or_else(|_| who.clone());
        let theirs: Vec<crate::domain::PendingJoin> = pending
            .into_iter()
            .filter(|p| p.sender_hex.eq_ignore_ascii_case(&want))
            .collect();
        // One person may have asked to join several workspaces -- and a join
        // request names its own target, so the extra one may be a workspace
        // nobody invited them to. `approve <npub>` must not grant more than
        // the one the admin is looking at.
        if theirs.len() > 1 {
            let names: Vec<String> = theirs
                .iter()
                .map(|p| {
                    crate::domain::resolve_workspace_by_gid(
                        engine,
                        client,
                        account_label,
                        &p.ws_gid_hex,
                    )
                    .ok()
                    .and_then(|(_, proj)| proj.name)
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| {
                        format!("group {}", p.ws_gid_hex.chars().take(8).collect::<String>())
                    })
                })
                .collect();
            anyhow::bail!(
                "{who} has open join requests for more than one workspace ({}); \
                 say which one with --workspace <name>",
                names.join(", ")
            );
        }
        theirs
    };
    if targets.is_empty() {
        if who == "all" {
            anyhow::bail!("no pending join requests");
        }
        anyhow::bail!("no pending join request from {who}");
    }

    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    for pj in &targets {
        match crate::domain::approve_one(engine, client, store, account_label, pj, auto).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => {
                let who = moyu_core::identity::npub_from_hex(&pj.sender_hex)
                    .unwrap_or_else(|_| pj.sender_hex.clone());
                heprintln!("approve failed for {who}: {e}");
                failures.push(who);
            }
        }
    }
    // Partial failure is NOT an early error here: the successes really did
    // add members on the wire and every caller must still report them (the
    // pre-extraction loop printed each success inline before bailing).
    // The caller decides how to surface `failures`
    // (one-shot `approve` renders outcomes then exits non-zero; the session
    // returns both in the receipt).
    Ok((outcomes, failures))
}

/// `relay list`: the relays persisted in `config.json` (pure local file read,
/// no account/relay connection).
pub(crate) fn relay_list(data_dir: &Path) -> Vec<String> {
    moyu_core::config::persisted_relays(data_dir)
}

/// `relay add <url>`: persist one more relay into `config.json` -- the same
/// `config::merge_relays` that `join` uses to adopt an inviter's relay, so a
/// self-hosted relay can be attached to an existing identity without
/// re-running `init`. Idempotent. The scheme is checked up front because a
/// malformed URL persisted here would poison every later command.
pub(crate) fn relay_add(data_dir: &Path, url: &str) -> anyhow::Result<Vec<String>> {
    let url = url.trim();
    let host = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"));
    match host {
        Some(h) if !h.is_empty() => {}
        _ => anyhow::bail!("relay URL must look like wss://host[:port] (got {url:?})"),
    }
    Ok(moyu_core::config::merge_relays(
        data_dir,
        std::slice::from_ref(&url.to_string()),
    )?)
}

/// `relay forget <url>`: prune one relay from the persisted set, returning
/// those that remain.
pub(crate) fn relay_forget(data_dir: &Path, url: &str) -> anyhow::Result<Vec<String>> {
    Ok(moyu_core::config::forget_relay(data_dir, url)?)
}

/// One row of `workspace list`.
pub(crate) struct WorkspaceListRow {
    pub group: String,
    pub name: String,
    pub channels: usize,
    /// `None` when the per-group `members()` read failed (a per-row
    /// failure must not abort the whole listing).
    pub members: Option<usize>,
}

impl WorkspaceListRow {
    /// The `workspace list` row body (no `"v"` — see [`SearchHit::to_json`]).
    /// One-shot only today (no session `workspace_list` command exists yet);
    /// still given a `to_json()` so a future session counterpart reuses this
    /// shape instead of forking it.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "name": self.name,
            "channels": self.channels,
            "members": self.members,
        })
    }
}

/// `workspace list`'s core: every workspace the account still belongs to (see
/// `all_workspaces` -- left/removed groups excluded, DMs never match), with
/// its channel/member counts.
pub(crate) fn workspace_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
) -> anyhow::Result<Vec<WorkspaceListRow>> {
    let workspaces = crate::domain::all_workspaces(engine, client, label)?;
    Ok(workspaces
        .into_iter()
        .map(|(id, proj)| {
            let members = crate::domain::group_id_from_hex(&id)
                .ok()
                .and_then(|gid| client.members(&gid).ok())
                .map(|m| m.len());
            WorkspaceListRow {
                channels: proj.ordered_channels().len(),
                name: proj.name.unwrap_or_default(),
                members,
                group: id,
            }
        })
        .collect())
}

/// One row of `channel list`.
pub(crate) struct ChannelListRow {
    pub slug: String,
    pub name: String,
    pub private: bool,
    pub archived: bool,
    /// `Some` only for a private channel (its own group id).
    pub group: Option<String>,
    /// `Some` only for a private channel (its parent workspace's group id).
    pub parent: Option<String>,
}

impl ChannelListRow {
    /// The `channel list` row body (no `"v"` — see [`SearchHit::to_json`]).
    /// One-shot only today (no session `channel_list` command exists yet);
    /// still given a `to_json()` so a future session counterpart reuses this
    /// shape instead of forking it. A private row carries `group`/`parent`
    /// and a hardcoded `archived:false` (a private channel has no archive
    /// state yet); a public row omits `group`/`parent` entirely rather than
    /// nulling them — preserved exactly as the pre-extraction inline code
    /// shaped it.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        if self.private {
            serde_json::json!({
                "slug": self.slug,
                "name": self.name,
                "private": true,
                "archived": false,
                "group": self.group,
                "parent": self.parent,
            })
        } else {
            serde_json::json!({
                "slug": self.slug,
                "name": self.name,
                "private": false,
                "archived": self.archived,
            })
        }
    }
}

/// `channel list <ws>`'s core: the workspace's channels in the projection's
/// stable display order (`#general` first, then by creation time), followed by
/// 🔒 private channels this account is in.
pub(crate) fn channel_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<Vec<ChannelListRow>> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    // Private channels of this workspace that I'm in (🔒). A non-member simply
    // never receives these groups' Welcomes, so they never appear here for them.
    // Nested via the core helper for a deterministic, cross-client order.
    let (workspaces, private_channels) = crate::domain::membership_view(engine, client, label)?;
    let ws_hexes: std::collections::BTreeSet<String> =
        workspaces.into_iter().map(|(id, _)| id).collect();
    let nested = workspace::nest_private_channels(private_channels, &ws_hexes);

    let mut rows: Vec<ChannelListRow> = proj
        .ordered_channels()
        .iter()
        .map(|c| ChannelListRow {
            slug: c.slug.clone(),
            name: c.name.clone(),
            private: false,
            archived: c.archived,
            group: None,
            parent: None,
        })
        .collect();
    if let Some(list) = nested.by_parent.get(&id) {
        for pc in list {
            rows.push(ChannelListRow {
                slug: pc.slug.clone(),
                name: pc.display_name().to_owned(),
                private: true,
                archived: false,
                group: Some(pc.group_id_hex.clone()),
                parent: Some(pc.parent_group_id_hex.clone()),
            });
        }
    }
    Ok(rows)
}

/// Typed receipt of `init`.
pub(crate) struct InitReceipt {
    pub label: String,
    pub npub: Option<String>,
    /// `Some` (with `npub: None`) only if bech32-encoding the account id
    /// failed -- see `whoami`'s doc comment for why this is a soft failure.
    pub npub_encode_err: Option<String>,
    pub key_package_bytes: Option<usize>,
}

impl InitReceipt {
    /// The one-shot `init --json` shape ONLY -- `label`/`npub` come off the
    /// receipt, but `relays`/`imported` are the CALLER's own context (the
    /// resolved relay list and whether `--import-nsec` was used), never part
    /// of this receipt, hence the parameters. `npub_encode_err` and
    /// `key_package_bytes` are deliberately not surfaced in `--json` either
    /// (unchanged from before this extraction -- only the human branch prints
    /// them). The session `do_init` shape is SMALLER (`state`/`label`/`npub`,
    /// no `relays`/`imported`) and must stay that way -- it builds its own
    /// object straight from `receipt.label`/`receipt.npub` rather than
    /// calling this method; see its call site for why.
    pub(crate) fn to_json(&self, relays: &[String], imported: bool) -> serde_json::Value {
        serde_json::json!({
            "label": self.label,
            "npub": self.npub,
            "relays": relays,
            "imported": imported,
        })
    }
}

/// `init`'s core (post-passphrase-prompt): create/import the account, persist
/// it as the active one, merge+persist the relay set, and mark the initial
/// KeyPackage published if one was. Prompting (the passphrase itself) stays in
/// `cmd_init`.
pub(crate) async fn init(
    data_dir: &Path,
    relays: &[String],
    allow_loopback: bool,
    socks5: Option<SocketAddr>,
    passphrase: String,
    import_nsec: Option<zeroize::Zeroizing<String>>,
) -> anyhow::Result<InitReceipt> {
    // MDK >= 0.9.16 only imports a *secret* here; a public key would be
    // rejected deep inside account setup with a terse "expected private key".
    // Catch the common slip (pasting an npub) with an actionable message.
    if let Some(v) = import_nsec.as_deref()
        && v.trim().starts_with("npub1")
    {
        anyhow::bail!(
            "--import-nsec needs the SECRET key (nsec1... or 64 hex chars), not an npub -- moyu \
             must be able to sign as this identity"
        );
    }
    let engine = crate::domain::build_engine(data_dir, relays, allow_loopback, socks5, passphrase);

    progress(
        "Creating account and publishing relay lists + initial KeyPackage (this talks to relays)...",
    );

    // The nsec (if any) is wiped as soon as MDK has consumed it.
    let result = engine
        .create_or_import_account(
            import_nsec,
            relays.to_vec(),
            relays.to_vec(),
            /* publish_initial_key_package */ true,
        )
        .await?;

    store::write_active_account_label(data_dir, &result.account.label)?;

    // 持久化 relay,后续命令不必再敲 --relay(杀脚枪)。
    let _ = moyu_core::config::merge_relays(data_dir, relays);

    let mut moyu_store = MoyuStore::open(data_dir, &result.account.label)?;
    if result.key_package_bytes.is_some() {
        moyu_store.mark_keypackage_published(keypackage_rotation::now_unix_secs())?;
    }

    let (npub, npub_encode_err) =
        match moyu_core::identity::npub_from_hex(&result.account.account_id_hex) {
            Ok(npub) => (Some(npub), None),
            Err(e) => (None, Some(e.to_string())),
        };

    Ok(InitReceipt {
        label: result.account.label,
        npub,
        npub_encode_err,
        key_package_bytes: result.key_package_bytes,
    })
}

/// Typed receipt of `add`.
pub(crate) struct AddReceipt {
    pub npub: String,
    pub label: String,
    pub nip05: Option<String>,
}

impl AddReceipt {
    /// The shared `add` receipt body — identical in the one-shot `--json`
    /// object and the session `add` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "npub": self.npub, "label": self.label, "nip05": self.nip05 })
    }
}

/// `add <npub|nip05>`'s core: resolve the peer reference and upsert it as a
/// contact. Takes the CALLER's store handle: `MoyuStore` is a whole-file
/// JSON cache whose every `save()` rewrites the file from memory, so a
/// long-lived session must funnel all writes through its single owned handle
/// — a second store opened here would make the new contact invisible to the
/// session and get clobbered by the session's next save.
pub(crate) async fn add(
    moyu_store: &mut MoyuStore,
    socks5: Option<SocketAddr>,
    npub_or_nip05: String,
    label: Option<String>,
) -> anyhow::Result<AddReceipt> {
    let npub = resolve_peer(&npub_or_nip05, socks5).await?.npub;
    let contact_label = label.unwrap_or_else(|| crate::domain::default_contact_label(&npub));
    let nip05 = npub_or_nip05.contains('@').then_some(npub_or_nip05.clone());

    moyu_store.upsert_contact(Contact {
        npub: npub.clone(),
        nip05: nip05.clone(),
        label: contact_label.clone(),
        group_id_hex: None,
    })?;

    Ok(AddReceipt {
        npub,
        label: contact_label,
        nip05,
    })
}

/// Typed receipt of `keypackage publish`/`keypackage rotate`.
pub(crate) struct KeypackageReceipt {
    /// `"publish"` or `"rotate"`.
    pub action: &'static str,
    pub bytes: usize,
    pub key_package_ref_hex: String,
}

impl KeypackageReceipt {
    /// The shared `keypackage publish`/`keypackage rotate` receipt body —
    /// identical in the one-shot `--json` object and the session
    /// `keypackage` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "action": self.action,
            "bytes": self.bytes,
            "ref": self.key_package_ref_hex,
        })
    }
}

/// `keypackage publish`/`keypackage rotate`'s core.
pub(crate) async fn keypackage(
    engine: &MoyuEngine,
    moyu_store: &mut MoyuStore,
    label: &str,
    action: super::KeypackageAction,
) -> anyhow::Result<KeypackageReceipt> {
    let (action_str, published) = match action {
        super::KeypackageAction::Publish => ("publish", engine.publish_key_package(label).await?),
        super::KeypackageAction::Rotate => ("rotate", engine.rotate_key_package(label).await?),
    };
    moyu_store.mark_keypackage_published(keypackage_rotation::now_unix_secs())?;
    Ok(KeypackageReceipt {
        action: action_str,
        bytes: published.bytes,
        key_package_ref_hex: published.key_package_ref_hex,
    })
}

/// Typed receipt of `workspace new`.
pub(crate) struct WorkspaceNewReceipt {
    pub group: String,
    pub name: String,
}

impl WorkspaceNewReceipt {
    /// The shared `workspace new` receipt body — identical in the one-shot
    /// `--json` object and the session `workspace_new` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "name": self.name })
    }
}

/// `workspace new <name>`'s core: found a brand-new MLS group (no initial
/// invitees) then immediately send its name as a `WorkspaceRename` control
/// event, which is what makes `resolve_workspace`/`workspace list` recognize
/// it as a workspace at all.
pub(crate) async fn workspace_new(
    client: &mut AppClient,
    name: &str,
) -> anyhow::Result<WorkspaceNewReceipt> {
    // The name becomes the group's MLS `profile.name`. Reject a name reserved by
    // moyu's private-channel encoding (would decode as a private channel and
    // vanish from the creator's own list) and one past MDK's profile-name limit
    // (MDK would reject it deep inside `create_group` with an opaque error).
    if workspace::is_reserved_group_name(name) {
        anyhow::bail!("that workspace name is reserved by moyu; choose another");
    }
    if name.len() > workspace::MAX_GROUP_NAME_BYTES {
        anyhow::bail!(
            "workspace name too long ({} bytes; max {})",
            name.len(),
            workspace::MAX_GROUP_NAME_BYTES
        );
    }
    let gid = client.create_group(name, &[]).await?;
    let ts = keypackage_rotation::now_unix_secs();
    crate::domain::send_control(
        client,
        &gid,
        ControlEvent::WorkspaceRename {
            name: name.to_owned(),
            ts,
        },
    )
    .await?;
    Ok(WorkspaceNewReceipt {
        group: gid.to_string(),
        name: name.to_owned(),
    })
}

/// Typed receipt of `workspace add`.
pub(crate) struct WorkspaceAddReceipt {
    pub group: String,
    pub workspace: String,
    pub npub: String,
    /// Whether the catch-up `WorkspaceSnapshot` broadcast succeeded. A `false`
    /// here does NOT mean the invite failed (it always stands); the new
    /// member's channel list simply fills in on the next control event.
    pub snapshot_sent: bool,
}

impl WorkspaceAddReceipt {
    /// The shared `workspace add` receipt body — identical in the one-shot
    /// `--json` object and the session `workspace_add` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "workspace": self.workspace,
            "npub": self.npub,
            "snapshot_sent": self.snapshot_sent,
        })
    }
}

/// `workspace add <ws> <npub|nip05>`'s core: invite the peer's MLS KeyPackage
/// into the group, then broadcast a `WorkspaceSnapshot` control event so the
/// new member can catch up on the current name/channel list without needing
/// to replay every historical control message (which MLS forward secrecy
/// means they can't see anyway).
pub(crate) async fn workspace_add(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    peer: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<WorkspaceAddReceipt> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let gid = crate::domain::group_id_from_hex(&id)?;
    let npub = resolve_peer(peer, socks5).await?.npub;
    client.invite_members(&gid, &[npub.as_str()]).await?;

    // The invite has landed (the peer is genuinely in the MLS group now).
    // Confirm it FIRST, then send the catch-up snapshot best-effort -- a
    // snapshot failure must never hide a successful invite.
    let name = proj.name.as_deref().unwrap_or_default().to_owned();

    // Broadcast the current name + channel list so the new member converges
    // without replaying pre-join control history (MLS forward secrecy hides it
    // from them anyway). If this send fails the invite still stands and the new
    // member's channel list fills in on the next control event, so warn rather
    // than propagate with `?`.
    let snapshot_sent =
        match crate::domain::send_control(client, &gid, proj.snapshot_event("")).await {
            Ok(()) => true,
            Err(e) => {
                heprintln!(
                    "warning: invited {npub}, but the channel-list snapshot failed to send ({e}); \
                 re-run `workspace add` or the new member's channel list will fill in on the next \
                 workspace change."
                );
                false
            }
        };

    Ok(WorkspaceAddReceipt {
        group: id,
        workspace: name,
        npub,
        snapshot_sent,
    })
}

/// One row of `workspace members`.
pub(crate) struct WorkspaceMemberRow {
    pub npub: String,
    pub admin: bool,
    pub you: bool,
}

impl WorkspaceMemberRow {
    /// The shared `workspace members` row body (no `"v"` — see
    /// [`SearchHit::to_json`]).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "npub": self.npub, "admin": self.admin, "you": self.you })
    }
}

/// `workspace members <ws>`'s core: every member's npub, badging admins and
/// the local account. Admins are listed first (see `governance::member_roles`).
pub(crate) fn workspace_members(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<Vec<WorkspaceMemberRow>> {
    let (id, _proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let gid = crate::domain::group_id_from_hex(&id)?;
    let members = client.members(&gid)?;
    let admins = crate::domain::workspace_admins(engine, label, &id)?;
    let local_hex = members
        .iter()
        .find(|m| m.local)
        .map(|m| m.member_id_hex.clone());
    let member_hexes: Vec<String> = members.iter().map(|m| m.member_id_hex.clone()).collect();
    let mut rows = Vec::new();
    for role in governance::member_roles(&member_hexes, &admins) {
        let npub = moyu_core::identity::npub_from_hex(&role.member_id_hex)?;
        let you = local_hex.as_deref() == Some(role.member_id_hex.as_str());
        rows.push(WorkspaceMemberRow {
            npub,
            admin: role.is_admin,
            you,
        });
    }
    Ok(rows)
}

/// Typed receipt of `workspace rename`.
pub(crate) struct WorkspaceRenameReceipt {
    pub group: String,
    pub name: String,
}

impl WorkspaceRenameReceipt {
    /// The shared `workspace rename` receipt body — identical in the
    /// one-shot `--json` object and the session `workspace_rename` receipt's
    /// `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "name": self.name })
    }
}

/// `workspace rename <ws> <name>`'s core: send a `WorkspaceRename` control
/// event; the projection's `(ts, message_id_hex)` LWW decides the winner if
/// two members race.
pub(crate) async fn workspace_rename(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
) -> anyhow::Result<WorkspaceRenameReceipt> {
    let (id, _proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let gid = crate::domain::group_id_from_hex(&id)?;
    let ts = keypackage_rotation::now_unix_secs();
    crate::domain::send_control(
        client,
        &gid,
        ControlEvent::WorkspaceRename {
            name: name.to_owned(),
            ts,
        },
    )
    .await?;
    Ok(WorkspaceRenameReceipt {
        group: id,
        name: name.to_owned(),
    })
}

/// Typed receipt of `workspace kick`.
pub(crate) struct WorkspaceKickReceipt {
    pub group: String,
    pub workspace: String,
    pub npub: String,
}

impl WorkspaceKickReceipt {
    /// The shared `workspace kick` receipt body — identical in the one-shot
    /// `--json` object and the session `workspace_kick` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "workspace": self.workspace, "npub": self.npub })
    }
}

/// `workspace kick <ws> <member>`'s core: remove a member from the group
/// (admin only). Pre-checks that you are an admin (nicer than MDK's raw
/// error) and that you are not kicking yourself; MDK is still the
/// authoritative enforcer.
pub(crate) async fn workspace_kick(
    engine: &MoyuEngine,
    client: &mut AppClient,
    store: &mut MoyuStore,
    label: &str,
    ws: &str,
    member: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<WorkspaceKickReceipt> {
    let (gid, me, admins, proj) = crate::domain::workspace_gov(engine, client, label, ws)?;
    let name = proj.name.unwrap_or_default();
    if !governance::is_admin(&me, &admins) {
        anyhow::bail!("only an admin can remove members from workspace {name}");
    }
    let peer = resolve_peer(member, socks5).await?;
    let member_hex = peer.hex;
    if member_hex == me {
        anyhow::bail!("can't kick yourself -- use `moyu workspace leave {ws}` instead");
    }
    let member_npub = peer.npub;
    client
        .remove_members(&gid, &[member_hex.as_str()])
        .await
        .with_context(|| format!("failed to remove {member_npub} from workspace {name}"))?;
    // A removed member may still hold an `--auto-approve` invite code. Record
    // the removal so no such code lets them straight back in; re-admitting
    // them takes an explicit `moyu approve`. The member is already out, so a
    // failed write is a warning, not an error.
    if let Err(e) = store.block_auto_join(&gid.to_string(), &member_hex) {
        heprintln!("warning: could not record the removal locally: {e}");
    }
    Ok(WorkspaceKickReceipt {
        group: gid.to_string(),
        workspace: name,
        npub: member_npub,
    })
}

/// Typed receipt of `workspace leave`.
pub(crate) struct WorkspaceLeaveReceipt {
    pub group: String,
    pub workspace: String,
    /// `Some` only on the sole-admin path (`--transfer-to` was used).
    pub transferred_to: Option<String>,
    pub stepped_down: bool,
}

impl WorkspaceLeaveReceipt {
    /// The shared `workspace leave` receipt body — identical in the one-shot
    /// `--json` object and the session `workspace_leave` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "workspace": self.workspace,
            "transferred_to": self.transferred_to,
            "stepped_down": self.stepped_down,
        })
    }
}

/// `workspace leave <ws> [--transfer-to <member>]`'s core: leave the
/// underlying MLS group. MDK forbids an admin from self-removing while still
/// in the admin set, and forbids emptying the admin set -- so the exact steps
/// depend on whether you are an admin and whether a co-admin exists
/// (`governance::leave_plan`):
/// - not an admin -> leave directly;
/// - an admin with a co-admin -> step down (`self_demote_admin`) then leave;
/// - the SOLE admin -> must first promote `--transfer-to <member>`, then step
///   down, then leave (else the group would be left admin-less).
///
/// The steps are separate MLS commits (MDK has no atomic transfer). If one
/// fails mid-way, [`progress`] has already surfaced how far it got (stdout in
/// human mode, stderr under `--json`) before the error propagates; re-running
/// is safe -- `leave_plan` recomputes from the now-current admin set.
pub(crate) async fn workspace_leave(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    transfer_to: Option<&str>,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<WorkspaceLeaveReceipt> {
    let (gid, me, admins, proj) = crate::domain::workspace_gov(engine, client, label, ws)?;
    let name = proj.name.unwrap_or_default();
    let mut transferred_to = None;
    let mut stepped_down = false;
    match governance::leave_plan(&me, &admins) {
        LeavePlan::LeaveNow => {}
        LeavePlan::SelfDemoteThenLeave => {
            client.self_demote_admin(&gid).await.with_context(|| {
                format!("failed to step down as admin of {name} before leaving")
            })?;
            stepped_down = true;
            progress(&format!("stepped down as admin of {name}"));
        }
        LeavePlan::TransferThenLeave => {
            let target = transfer_to.ok_or_else(|| {
                anyhow::anyhow!(
                    "you are the only admin of workspace {name}; pass `--transfer-to <npub>` to \
                     hand admin to another member before leaving (or add a co-admin first with \
                     `moyu workspace admin add {ws} <npub>`)"
                )
            })?;
            let peer = resolve_peer(target, socks5).await?;
            let target_hex = peer.hex;
            if target_hex == me {
                anyhow::bail!("--transfer-to must name another member, not yourself");
            }
            let target_npub = peer.npub;
            client
                .promote_admin(&gid, target_hex.as_str())
                .await
                .with_context(|| format!("failed to promote {target_npub} to admin of {name}"))?;
            progress(&format!("promoted {target_npub} to admin of {name}"));
            transferred_to = Some(target_npub.clone());
            client.self_demote_admin(&gid).await.with_context(|| {
                format!(
                    "promoted {target_npub} to admin, but failed to step down yourself; \
                     re-run `moyu workspace leave {ws}` to finish leaving"
                )
            })?;
            stepped_down = true;
            progress(&format!("stepped down as admin of {name}"));
        }
    }
    client
        .leave_group(&gid)
        .await
        .with_context(|| format!("failed to leave workspace {name}"))?;
    Ok(WorkspaceLeaveReceipt {
        group: gid.to_string(),
        workspace: name,
        transferred_to,
        stepped_down,
    })
}

/// One row of `workspace admin list`.
pub(crate) struct AdminRow {
    pub npub: String,
    pub you: bool,
}

impl AdminRow {
    /// The shared `workspace admin list` row body (no `"v"` — see
    /// [`SearchHit::to_json`]).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "npub": self.npub, "you": self.you })
    }
}

/// `workspace admin list <ws>`'s core result: the workspace's display name
/// (needed for the human "no admin set" message) plus its admin rows.
pub(crate) struct AdminListing {
    pub workspace: String,
    pub admins: Vec<AdminRow>,
}

/// `workspace admin list <ws>`'s core: the workspace's admins (npub, `you`
/// flag for the local account).
pub(crate) fn admin_list(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
) -> anyhow::Result<AdminListing> {
    let (_gid, me, admins, proj) = crate::domain::workspace_gov(engine, client, label, ws)?;
    let mut rows = Vec::new();
    for admin_hex in &admins {
        let npub = moyu_core::identity::npub_from_hex(admin_hex)?;
        rows.push(AdminRow {
            npub,
            you: admin_hex.eq_ignore_ascii_case(&me),
        });
    }
    Ok(AdminListing {
        workspace: proj.name.unwrap_or_default(),
        admins: rows,
    })
}

/// Typed receipt of `workspace admin add`/`workspace admin remove`.
pub(crate) struct AdminChangeReceipt {
    pub group: String,
    pub workspace: String,
    pub npub: String,
    /// `"promote"` or `"demote"`.
    pub action: &'static str,
}

impl AdminChangeReceipt {
    /// The shared `workspace admin add`/`workspace admin remove` receipt
    /// body — identical in the one-shot `--json` object and the session
    /// `admin_add`/`admin_remove` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "workspace": self.workspace,
            "npub": self.npub,
            "action": self.action,
        })
    }
}

/// `workspace admin add|remove <ws> <member>`'s core: promote/demote a member
/// in the admin set (admin only). Pre-checks you are an admin; MDK enforces
/// the rest (e.g. it rejects demoting the last admin, keeping the group
/// governable).
pub(crate) async fn admin_change(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    member: &str,
    socks5: Option<SocketAddr>,
    change: crate::domain::AdminChange,
) -> anyhow::Result<AdminChangeReceipt> {
    let (gid, me, admins, proj) = crate::domain::workspace_gov(engine, client, label, ws)?;
    let name = proj.name.unwrap_or_default();
    if !governance::is_admin(&me, &admins) {
        anyhow::bail!("only an admin can change the admin set of workspace {name}");
    }
    let peer = resolve_peer(member, socks5).await?;
    let member_hex = peer.hex;
    let member_npub = peer.npub;
    let action = match change {
        crate::domain::AdminChange::Promote => {
            client
                .promote_admin(&gid, member_hex.as_str())
                .await
                .with_context(|| format!("failed to promote {member_npub} to admin of {name}"))?;
            "promote"
        }
        crate::domain::AdminChange::Demote => {
            client
                .demote_admin(&gid, member_hex.as_str())
                .await
                .with_context(|| format!("failed to demote {member_npub} from admin of {name}"))?;
            "demote"
        }
    };
    Ok(AdminChangeReceipt {
        group: gid.to_string(),
        workspace: name,
        npub: member_npub,
        action,
    })
}

/// Typed receipt of `channel new`.
pub(crate) struct ChannelNewReceipt {
    pub group: String,
    pub channel: String,
    pub name: String,
}

impl ChannelNewReceipt {
    /// The shared `channel new` receipt body — identical in the one-shot
    /// `--json` object and the session `channel_new` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "channel": self.channel, "name": self.name })
    }
}

/// `channel new <ws> <name>`'s core: send a `ChannelCreate` control event.
/// Dedup (two members creating the same slug concurrently) is the
/// projection's concern (`WorkspaceProjection::apply` keeps the first-seen
/// create), so it is always fine to send this even if the channel might
/// already exist.
pub(crate) async fn channel_new(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
) -> anyhow::Result<ChannelNewReceipt> {
    let (id, _proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let gid = crate::domain::group_id_from_hex(&id)?;
    let slug = workspace::normalize_slug(name);
    crate::domain::send_control(
        client,
        &gid,
        ControlEvent::ChannelCreate {
            slug: slug.clone(),
            name: name.to_owned(),
        },
    )
    .await?;
    Ok(ChannelNewReceipt {
        group: id,
        channel: slug,
        name: name.to_owned(),
    })
}

/// Typed receipt of `channel new-private`.
pub(crate) struct ChannelNewPrivateReceipt {
    /// The new private channel's own group id.
    pub group: String,
    /// The parent workspace's group id.
    pub parent: String,
    pub channel: String,
    pub workspace: String,
    pub invited: Vec<String>,
}

impl ChannelNewPrivateReceipt {
    /// The shared `channel new-private` receipt body — identical in the
    /// one-shot `--json` object and the session `channel_new_private`
    /// receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "parent": self.parent,
            "channel": self.channel,
            "workspace": self.workspace,
            "invited": self.invited,
        })
    }
}

/// The parent workspace's member id-hex set -- needed by both
/// `channel_new_private` (checking every invited member) and `channel_invite`
/// (checking the one invitee) to enforce D4: private-channel members ⊆
/// workspace members.
fn parent_member_hexes(client: &AppClient, parent_gid: &GroupId) -> anyhow::Result<Vec<String>> {
    Ok(client
        .members(parent_gid)?
        .iter()
        .map(|m| m.member_id_hex.clone())
        .collect())
}

/// `channel new-private <ws> <name> [members...]`'s core: create a private
/// channel as a SEPARATE MLS group nested under the workspace. Its
/// `profile.name` encodes the parent workspace id + slug
/// (`workspace::encode_private_channel_name`), so members recognize it the
/// instant they join (no control-message round-trip) and non-members never
/// learn it exists. Every invited member must already belong to the parent
/// workspace (D4: private-channel members ⊆ workspace members; an app-level
/// check, since MLS can't enforce cross-group membership). The creator is its
/// sole initial admin.
pub(crate) async fn channel_new_private(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    name: &str,
    members: &[String],
    socks5: Option<SocketAddr>,
) -> anyhow::Result<ChannelNewPrivateReceipt> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let parent_gid = crate::domain::group_id_from_hex(&id)?;
    let slug = workspace::normalize_slug(name);
    let encoded = workspace::encode_private_channel_name(&id, &slug, name);
    if encoded.len() > workspace::MAX_GROUP_NAME_BYTES {
        anyhow::bail!(
            "private channel name too long (encodes to {} bytes; max {})",
            encoded.len(),
            workspace::MAX_GROUP_NAME_BYTES
        );
    }
    // Resolve each requested member and require they already belong to the parent
    // workspace before creating the group.
    let ws_member_hexes = parent_member_hexes(client, &parent_gid)?;
    let mut npubs = Vec::new();
    let mut req_hexes = Vec::new();
    for m in members {
        let peer = resolve_peer(m, socks5).await?;
        req_hexes.push(peer.hex);
        npubs.push(peer.npub);
    }
    let outside = workspace::members_outside_workspace(&req_hexes, &ws_member_hexes);
    if !outside.is_empty() {
        let who: Vec<String> = outside
            .iter()
            .map(|h| moyu_core::identity::npub_from_hex(h).unwrap_or_else(|_| h.clone()))
            .collect();
        anyhow::bail!(
            "these members are not in workspace {} -- add them to the workspace first: {}",
            proj.name.as_deref().unwrap_or_default(),
            who.join(", ")
        );
    }
    let refs: Vec<&str> = npubs.iter().map(|s| s.as_str()).collect();
    let pgid = client.create_group(&encoded, &refs).await?;
    Ok(ChannelNewPrivateReceipt {
        group: pgid.to_string(),
        parent: id,
        channel: slug,
        workspace: proj.name.unwrap_or_default(),
        invited: npubs,
    })
}

/// Typed receipt of `channel invite`.
pub(crate) struct ChannelInviteReceipt {
    /// The private channel's own group id.
    pub group: String,
    pub channel: String,
    pub npub: String,
}

impl ChannelInviteReceipt {
    /// The shared `channel invite` receipt body — identical in the one-shot
    /// `--json` object and the session `channel_invite` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "channel": self.channel, "npub": self.npub })
    }
}

/// `channel invite <ws> <slug> <member>`'s core: add a member to an existing
/// private channel. `invite_members` is admin-gated on the private channel's
/// own group, so only that channel's admins can add; the invitee must already
/// be a member of the parent workspace (D4).
pub(crate) async fn channel_invite(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
    member: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<ChannelInviteReceipt> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let parent_gid = crate::domain::group_id_from_hex(&id)?;
    let slug = workspace::normalize_slug(slug);
    let (_, private_channels) = crate::domain::membership_view(engine, client, label)?;
    let pc_hex = private_channels
        .iter()
        .find(|pc| pc.parent_group_id_hex == id && pc.slug == slug)
        .map(|pc| pc.group_id_hex.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no private channel #{slug} in workspace {}",
                proj.name.as_deref().unwrap_or_default()
            )
        })?;
    let peer = resolve_peer(member, socks5).await?;
    let ws_member_hexes = parent_member_hexes(client, &parent_gid)?;
    if !workspace::members_outside_workspace(std::slice::from_ref(&peer.hex), &ws_member_hexes)
        .is_empty()
    {
        anyhow::bail!(
            "{} is not a member of workspace {} -- add them to the workspace first",
            peer.npub,
            proj.name.as_deref().unwrap_or_default()
        );
    }
    let npub = peer.npub;
    let pgid = crate::domain::group_id_from_hex(&pc_hex)?;
    client.invite_members(&pgid, &[npub.as_str()]).await?;
    Ok(ChannelInviteReceipt {
        group: pc_hex,
        channel: slug,
        npub,
    })
}

/// Typed receipt of `channel rename`.
pub(crate) struct ChannelRenameReceipt {
    pub group: String,
    pub channel: String,
    pub name: String,
}

impl ChannelRenameReceipt {
    /// The shared `channel rename` receipt body — identical in the one-shot
    /// `--json` object and the session `channel_rename` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "channel": self.channel, "name": self.name })
    }
}

/// `channel rename <ws> <slug> <name>`'s core: pre-checks the channel exists
/// (a rename of a nonexistent slug would otherwise be a silent no-op once
/// applied -- `WorkspaceProjection::apply` drops a rename that targets an
/// unknown slug) before sending the control event.
pub(crate) async fn channel_rename(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
    name: &str,
) -> anyhow::Result<ChannelRenameReceipt> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let slug = workspace::normalize_slug(slug);
    if proj.channel(&slug).is_none() {
        anyhow::bail!("no channel #{slug}");
    }
    let gid = crate::domain::group_id_from_hex(&id)?;
    let ts = keypackage_rotation::now_unix_secs();
    crate::domain::send_control(
        client,
        &gid,
        ControlEvent::ChannelRename {
            slug: slug.clone(),
            name: name.to_owned(),
            ts,
        },
    )
    .await?;
    Ok(ChannelRenameReceipt {
        group: id,
        channel: slug,
        name: name.to_owned(),
    })
}

/// Typed receipt of `channel archive`.
pub(crate) struct ChannelArchiveReceipt {
    pub group: String,
    pub channel: String,
}

impl ChannelArchiveReceipt {
    /// The shared `channel archive` receipt body — identical in the one-shot
    /// `--json` object and the session `channel_archive` receipt's `data`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "group": self.group, "channel": self.channel })
    }
}

/// `channel archive <ws> <slug>`'s core: pre-checks the channel exists (same
/// silent no-op hazard as rename) before sending the control event. Archival
/// is monotonic -- there is no un-archive in M1.
pub(crate) async fn channel_archive(
    engine: &MoyuEngine,
    client: &mut AppClient,
    label: &str,
    ws: &str,
    slug: &str,
) -> anyhow::Result<ChannelArchiveReceipt> {
    let (id, proj) = crate::domain::resolve_workspace(engine, client, label, ws)?;
    let slug = workspace::normalize_slug(slug);
    if proj.channel(&slug).is_none() {
        anyhow::bail!("no channel #{slug}");
    }
    let gid = crate::domain::group_id_from_hex(&id)?;
    let ts = keypackage_rotation::now_unix_secs();
    crate::domain::send_control(
        client,
        &gid,
        ControlEvent::ChannelArchive {
            slug: slug.clone(),
            ts,
        },
    )
    .await?;
    Ok(ChannelArchiveReceipt {
        group: id,
        channel: slug,
    })
}

// ---------------------------------------------------------------------------
// A-3: per-group history pagination + the conversations view. Same contract as
// everything above: print-free cores shared by the one-shot CLI commands
// (`moyu history` / `moyu conversations`) and the upcoming session dispatcher
// (a GUI opens a chat -> `history`; hydrates its sidebar -> `conversations`).
// ---------------------------------------------------------------------------

/// Parse the opaque `--before` cursor: `"<recorded_at>:<message_id_hex>"`.
/// Callers must treat cursors as tokens from a previous page's `next_cursor`;
/// the format is an implementation detail. It is exactly the history sort key
/// ([`history`]'s `(recorded_at, message_id_hex)`), so pagination is a pure
/// keyset filter -- a page boundary can never skip or duplicate a row even as
/// new messages land.
fn parse_cursor(s: &str) -> anyhow::Result<(u64, String)> {
    let parsed = s.split_once(':').and_then(|(ts, id)| {
        let ts = ts.parse::<u64>().ok()?;
        (!id.is_empty()).then(|| (ts, id.to_owned()))
    });
    parsed.ok_or_else(|| {
        anyhow::anyhow!("invalid --before cursor (pass a next_cursor value from a previous page)")
    })
}

fn cursor_of(ts: u64, id: &str) -> String {
    format!("{ts}:{id}")
}

/// Classify ONE persisted record into the same [`SessionEvent`] shape the live
/// `sync_tick` emits -- minus `sender_name`, which `AppMessageRecord` does not
/// persist (a front end overlays display names from the roster) -- so history
/// pages and live events share a single renderer. `None` = not a display row:
/// control events (kind-1210 etc.) and join-request envelopes (surfaced via
/// `requests`, not chat history), mirroring `sync_tick`'s filters. `dm` is the
/// caller's per-GROUP classification (history reads one group, so it's one
/// lookup per page, not per record).
pub(crate) fn record_to_event(
    r: &AppMessageRecord,
    allow_loopback: bool,
    dm: bool,
) -> Option<SessionEvent> {
    if r.kind == crate::domain::REACTION_KIND {
        let target =
            crate::domain::first_tag_value(&r.tags, crate::domain::EVENT_REF_TAG).unwrap_or("");
        return Some(SessionEvent::Reaction {
            group: r.group_id_hex.clone(),
            sender: r.sender.clone(),
            sender_name: None,
            emoji: r.plaintext.trim().to_owned(),
            target: target.to_owned(),
            ts: r.recorded_at,
            message_id: r.message_id_hex.clone(),
        });
    }
    if r.kind == crate::domain::AGENT_OPERATION_KIND || r.kind == crate::domain::AGENT_ACTIVITY_KIND
    {
        let is_op = r.kind == crate::domain::AGENT_OPERATION_KIND;
        let content: serde_json::Value =
            serde_json::from_str(&r.plaintext).unwrap_or_else(|_| serde_json::json!({}));
        let slug =
            crate::domain::bot_event_channel(&content).unwrap_or_else(|| "general".to_owned());
        return Some(SessionEvent::Bot {
            is_op,
            group: r.group_id_hex.clone(),
            channel: slug,
            sender: r.sender.clone(),
            sender_name: None,
            kind: r.kind,
            content,
            ts: r.recorded_at,
            message_id: r.message_id_hex.clone(),
        });
    }
    if r.kind != crate::domain::CHAT_MESSAGE_KIND {
        return None;
    }
    if invite::parse_join_request(&r.plaintext).is_some() {
        return None;
    }
    let reply_to =
        crate::domain::first_tag_value(&r.tags, crate::domain::QUOTE_REF_TAG).map(str::to_owned);
    let (slug, body) = workspace::decode_channel_body(&r.plaintext);
    let attachments: Vec<MediaAttachmentReference> = r
        .tags
        .iter()
        .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
        .filter_map(|tag| {
            moyu_core::engine::media_attachment_from_imeta_tag(tag, r.source_epoch, allow_loopback)
                .ok()
        })
        .collect();
    Some(SessionEvent::Message {
        group: r.group_id_hex.clone(),
        channel: slug,
        dm,
        sender: r.sender.clone(),
        sender_name: None,
        kind: r.kind,
        ts: r.recorded_at,
        message_id: r.message_id_hex.clone(),
        body,
        attachments,
        reply_to,
    })
}

/// One display row of a history page: a [`SessionEvent`] (the same shape live
/// `sync_tick` events use, so one renderer serves both paths) plus the
/// persisted `direction` (`"sent"`/`"received"`) live events don't carry.
pub(crate) struct HistoryRow {
    pub direction: String,
    pub event: SessionEvent,
}

impl HistoryRow {
    /// The row's JSON form: exactly the live event object plus `"direction"`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut v = self.event.to_json();
        v["direction"] = serde_json::Value::String(self.direction.clone());
        v
    }
}

/// One page of a group's local history.
pub(crate) struct HistoryPage {
    pub group: String,
    /// Ascending (oldest -> newest) display rows.
    pub rows: Vec<HistoryRow>,
    /// Pass back as `--before` to fetch the next OLDER page; `None` when this
    /// page already reaches the start of local history.
    pub next_cursor: Option<String>,
}

/// `history <group>`'s core: a pure offline keyset-paginated read of ONE
/// group's already-decrypted local history (never opens a client -- no relay
/// traffic, same offline contract as [`search`]). Pages walk BACKWARDS in
/// time -- the first call returns the newest `limit` rows, `next_cursor`
/// fetches older -- because that is how a chat front end scrolls.
pub(crate) fn history(
    engine: &MoyuEngine,
    label: &str,
    group_hex: &str,
    before: Option<&str>,
    limit: usize,
    allow_loopback: bool,
) -> anyhow::Result<HistoryPage> {
    let before_key = before.map(parse_cursor).transpose()?;
    // Whether THIS group is a 1:1 DM — one profile-name scan per page, so
    // history rows carry the same `dm` flag live `message` events do (the
    // two shapes are one contract; see `record_to_event`'s doc).
    let dm = crate::domain::dm_group_hexes(engine, label).contains(group_hex);
    let mut records = engine.messages_for_group(label, group_hex)?;
    // The canonical history order: send time (`recorded_at` -- see
    // `tui::load_history` for why not local ingest time), message id as the
    // unique total-order tiebreak. The cursor is exactly this sort key.
    records.sort_by(|a, b| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then_with(|| a.message_id_hex.cmp(&b.message_id_hex))
    });

    // (sort key, display row) pairs; the key stays alongside so the page
    // boundary's cursor is read straight off the sorted data.
    let mut display: Vec<(u64, String, HistoryRow)> = Vec::new();
    for r in &records {
        if let Some((bts, bid)) = &before_key {
            let older = r.recorded_at < *bts
                || (r.recorded_at == *bts && r.message_id_hex.as_str() < bid.as_str());
            if !older {
                continue;
            }
        }
        if let Some(event) = record_to_event(r, allow_loopback, dm) {
            display.push((
                r.recorded_at,
                r.message_id_hex.clone(),
                HistoryRow {
                    direction: r.direction.clone(),
                    event,
                },
            ));
        }
    }
    let start = display.len().saturating_sub(limit.max(1));
    let next_cursor = (start > 0)
        .then(|| display.get(start).map(|(ts, id, _)| cursor_of(*ts, id)))
        .flatten();
    let rows = display.drain(start..).map(|(_, _, row)| row).collect();
    Ok(HistoryPage {
        group: group_hex.to_owned(),
        rows,
        next_cursor,
    })
}

/// One DM conversation (an existing 1:1 group).
pub(crate) struct DmRow {
    pub group: String,
    /// The peer's npub -- `None` only if the roster read failed or the hex
    /// would not bech32-encode (degenerate; the group is still listed).
    pub npub: Option<String>,
    /// Contact-book overlay, when the peer is a saved contact.
    pub label: Option<String>,
    pub nip05: Option<String>,
}

impl DmRow {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group, "npub": self.npub, "label": self.label, "nip05": self.nip05,
        })
    }
}

/// One channel row under a workspace in the conversations view.
pub(crate) struct ConvChannel {
    pub slug: String,
    pub name: String,
    pub private: bool,
    pub archived: bool,
    /// `Some` only for a private channel (its own group id).
    pub group: Option<String>,
}

impl ConvChannel {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "slug": self.slug,
            "name": self.name,
            "private": self.private,
            "archived": self.archived,
            "group": self.group,
        })
    }
}

/// One workspace (with its channel tree) in the conversations view.
pub(crate) struct ConvWorkspace {
    pub group: String,
    pub name: String,
    pub channels: Vec<ConvChannel>,
}

impl ConvWorkspace {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "group": self.group,
            "name": self.name,
            "channels": self.channels.iter().map(ConvChannel::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A saved contact with no DM group yet -- a "start a chat" target.
pub(crate) struct ContactRow {
    pub npub: String,
    pub label: String,
    pub nip05: Option<String>,
}

impl ContactRow {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "npub": self.npub, "label": self.label, "nip05": self.nip05 })
    }
}

/// The whole sidebar in one read: every conversation this account can open.
pub(crate) struct Conversations {
    pub dms: Vec<DmRow>,
    pub workspaces: Vec<ConvWorkspace>,
    pub contacts: Vec<ContactRow>,
}

impl Conversations {
    /// The shared `conversations` receipt body (no top-level `"v"` — the
    /// one-shot renderer stamps one on the whole wrapping object; the
    /// session renderer's `data` is already inside a `"v"`-carrying receipt
    /// frame) — identical field set/nesting in both today's one-shot
    /// `--json` output and the session `conversations` receipt's `data`
    /// (this was already true before this extraction; see the former
    /// `session::conversations_json` this method replaces).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "dms": self.dms.iter().map(DmRow::to_json).collect::<Vec<_>>(),
            "workspaces": self.workspaces.iter().map(ConvWorkspace::to_json).collect::<Vec<_>>(),
            "contacts": self.contacts.iter().map(ContactRow::to_json).collect::<Vec<_>>(),
        })
    }
}

/// `conversations`' core: enumerate DMs + workspaces (with nested channels,
/// 🔒 private ones included) + not-yet-chatted contacts in one pass. This is
/// the one command that lists DM groups at all (nothing else did) -- a GUI
/// hydrates its sidebar from it. Same fail-open roster semantics as
/// `membership_scan` (a members() read error keeps the group; a SUCCESSFUL
/// read without me drops it -- a stale-kick guard), but kept as its own scan
/// because `membership_scan` deliberately drops DMs and this is a display
/// path, not the per-tick hot path.
pub(crate) fn conversations(
    engine: &MoyuEngine,
    client: &AppClient,
    moyu_store: &MoyuStore,
    label: &str,
) -> anyhow::Result<Conversations> {
    let groups = engine.groups(label)?;
    let msgs = engine.messages(label)?;
    let mut by_group: std::collections::HashMap<String, Vec<AppMessageRecord>> =
        std::collections::HashMap::new();
    for m in msgs {
        by_group.entry(m.group_id_hex.clone()).or_default().push(m);
    }

    let mut dms = Vec::new();
    let mut workspaces: Vec<(String, workspace::WorkspaceProjection)> = Vec::new();
    let mut private_channels = Vec::new();
    for g in groups {
        if g.self_membership != SelfMembership::Member {
            continue;
        }
        let members = crate::domain::group_id_from_hex(&g.group_id_hex)
            .ok()
            .and_then(|gid| client.members(&gid).ok());
        if let Some(ms) = &members
            && !ms.iter().any(|m| m.local)
        {
            continue;
        }
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
        if workspace::classify_group(&g.profile.name, proj.name.as_deref())
            == workspace::GroupKind::Workspace
        {
            if proj.name.is_none() {
                proj.name = Some(g.profile.name.clone());
            }
            workspaces.push((g.group_id_hex, proj));
            continue;
        }
        // A 1:1 DM: the sole non-local roster member is the peer.
        let npub = members
            .as_ref()
            .and_then(|ms| ms.iter().find(|m| !m.local))
            .and_then(|m| moyu_core::identity::npub_from_hex(&m.member_id_hex).ok());
        let contact = npub.as_deref().and_then(|n| moyu_store.find_contact(n));
        dms.push(DmRow {
            group: g.group_id_hex,
            label: contact.map(|c| c.label.clone()),
            nip05: contact.and_then(|c| c.nip05.clone()),
            npub,
        });
    }

    let ws_hexes: std::collections::BTreeSet<String> =
        workspaces.iter().map(|(id, _)| id.clone()).collect();
    let nested = workspace::nest_private_channels(private_channels, &ws_hexes);
    let workspaces = workspaces
        .into_iter()
        .map(|(id, proj)| {
            let mut channels: Vec<ConvChannel> = proj
                .ordered_channels()
                .iter()
                .map(|c| ConvChannel {
                    slug: c.slug.clone(),
                    name: c.name.clone(),
                    private: false,
                    archived: c.archived,
                    group: None,
                })
                .collect();
            if let Some(list) = nested.by_parent.get(&id) {
                for pc in list {
                    channels.push(ConvChannel {
                        slug: pc.slug.clone(),
                        name: pc.display_name().to_owned(),
                        private: true,
                        archived: false,
                        group: Some(pc.group_id_hex.clone()),
                    });
                }
            }
            ConvWorkspace {
                name: proj.name.unwrap_or_default(),
                group: id,
                channels,
            }
        })
        .collect();

    // Contacts with no DM row above -- "start a chat" targets.
    let dm_npubs: std::collections::BTreeSet<&str> =
        dms.iter().filter_map(|d| d.npub.as_deref()).collect();
    let contacts = moyu_store
        .contacts()
        .iter()
        .filter(|c| !dm_npubs.contains(c.npub.as_str()))
        .map(|c| ContactRow {
            npub: c.npub.clone(),
            label: c.label.clone(),
            nip05: c.nip05.clone(),
        })
        .collect();

    Ok(Conversations {
        dms,
        workspaces,
        contacts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u64, ts: u64, id: &str, plaintext: &str) -> AppMessageRecord {
        AppMessageRecord {
            message_id_hex: id.to_owned(),
            direction: "received".to_owned(),
            group_id_hex: "aa".repeat(16),
            sender: "bb".repeat(32),
            plaintext: plaintext.to_owned(),
            kind,
            tags: Vec::new(),
            source_epoch: None,
            recorded_at: ts,
            received_at: ts,
            insert_order: 0,
            retention: None,
            invalidated: false,
            moderation_grant: false,
        }
    }

    /// `SessionEvent::to_human` itself does not sanitize -- that happens once,
    /// at the actual print boundary (`output::human_line`, reached via the
    /// `hprintln!`/`heprintln!` macros every `recv`/`history` call site uses).
    /// This test exercises that same real path: build the human line from a
    /// malicious message body, then sanitize it exactly as `hprintln!` would,
    /// and confirm the escape is gone and no raw ESC byte survives.
    #[test]
    fn recv_human_line_neutralizes_a_terminal_escape_in_the_message_body() {
        let ev = SessionEvent::Message {
            group: "aa".repeat(16),
            channel: "general".to_owned(),
            dm: false,
            sender: "bb".repeat(32),
            sender_name: Some("mallory".to_owned()),
            kind: 9,
            ts: 0,
            message_id: "cc".repeat(16),
            body: "hi \x1b[31mred\x1b[0m".to_owned(),
            attachments: Vec::new(),
            reply_to: None,
        };
        let human = ev.to_human();
        assert!(
            human.contains('\x1b'),
            "fixture sanity: body carries a raw ESC"
        );

        let rendered = crate::output::term_safe(&human);
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.contains('\u{fffd}'));
        assert!(rendered.contains("hi \u{fffd}[31mred\u{fffd}[0m"));
    }

    /// A multi-line message body must not be able to spoof a following,
    /// independently-attributed line: `to_human` indents every body line
    /// after the first via `output::indent_continuation`, so an embedded
    /// fake "[id] sender [#chan]:" prefix stays visibly indented instead of
    /// rendering flush-left as a second message.
    #[test]
    fn to_human_indents_a_multiline_message_body() {
        let ev = SessionEvent::Message {
            group: "aa".repeat(16),
            channel: "general".to_owned(),
            dm: false,
            sender: "bb".repeat(32),
            sender_name: Some("alice".to_owned()),
            kind: 9,
            ts: 0,
            message_id: "cc".repeat(16),
            body: "hi\n[abcd1234] mallory [#general]: fake".to_owned(),
            attachments: Vec::new(),
            reply_to: None,
        };
        let human = ev.to_human();
        assert!(human.contains("hi\n    [abcd1234] mallory [#general]: fake"));
        assert!(!human.contains("hi\n[abcd1234]"));
    }

    #[test]
    fn relay_add_validates_scheme_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "moyu-relay-add-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for bad in [
            "",
            "relay.example.com",
            "https://relay.example.com",
            "wss://",
            "ws://",
        ] {
            assert!(relay_add(&dir, bad).is_err(), "{bad:?} must be rejected");
        }
        let once = relay_add(&dir, " wss://relay.example.com ").unwrap();
        assert_eq!(once, vec!["wss://relay.example.com".to_string()]);
        let twice = relay_add(&dir, "wss://relay.example.com").unwrap();
        assert_eq!(
            twice, once,
            "adding the same relay again must not duplicate it"
        );
        assert_eq!(
            relay_list(&dir),
            once,
            "persisted set is what `relay list` reports"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_roundtrips_and_rejects_garbage() {
        let (ts, id) = parse_cursor(&cursor_of(1720000000, "deadbeef")).unwrap();
        assert_eq!((ts, id.as_str()), (1720000000, "deadbeef"));
        for bad in ["", "nocolon", ":idonly", "12:", "x9:aa"] {
            assert!(parse_cursor(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn record_classification_mirrors_sync_tick() {
        // Plain chat -> Message with the channel envelope decoded.
        match record_to_event(
            &record(9, 1, "m1", r#"{"moyu":1,"ch":"backend","body":"hi"}"#),
            false,
            false,
        ) {
            Some(SessionEvent::Message { channel, body, .. }) => {
                assert_eq!((channel.as_str(), body.as_str()), ("backend", "hi"));
            }
            other => panic!("expected Message, got {:?}", other.is_some()),
        }
        // Reaction -> Reaction keyed to the `e` tag target.
        let mut r7 = record(7, 2, "m2", " 👍 ");
        r7.tags = vec![vec!["e".into(), "target-id".into()]];
        match record_to_event(&r7, false, false) {
            Some(SessionEvent::Reaction { emoji, target, .. }) => {
                assert_eq!((emoji.as_str(), target.as_str()), ("👍", "target-id"));
            }
            other => panic!("expected Reaction, got {:?}", other.is_some()),
        }
        // Bot events -> Bot, op vs activity by kind.
        match record_to_event(
            &record(1202, 3, "m3", r#"{"status":"ok","text":"t"}"#),
            false,
            false,
        ) {
            Some(SessionEvent::Bot { is_op, .. }) => assert!(is_op),
            other => panic!("expected Bot, got {:?}", other.is_some()),
        }
        match record_to_event(&record(1201, 4, "m4", r#"{"status":"ok"}"#), false, false) {
            Some(SessionEvent::Bot { is_op, .. }) => assert!(!is_op),
            other => panic!("expected Bot, got {:?}", other.is_some()),
        }
        // Control events (kind-1210) and join-request envelopes -> not display rows.
        assert!(record_to_event(&record(1210, 5, "m5", "{}"), false, false).is_none());
        let jr = r#"{"moyu":{"type":"join-request","ws_gid":"aa","ws_name":"w","secret":"s"}}"#;
        assert!(record_to_event(&record(9, 6, "m6", jr), false, false).is_none());
    }

    /// `init` rejects a pasted npub (the common slip) with an actionable
    /// error before it ever touches the network -- this check runs first, so
    /// a bogus data_dir/relay set here still exercises the real message.
    #[tokio::test]
    async fn init_rejects_npub_as_import_nsec() {
        let result = init(
            Path::new("/nonexistent-moyu-test-data-dir"),
            &[],
            false,
            None,
            "passphrase".to_owned(),
            Some(zeroize::Zeroizing::new(
                "npub1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq".to_owned(),
            )),
        )
        .await;
        let err = match result {
            Ok(_) => panic!("expected init() to reject an npub passed as --import-nsec"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("SECRET key"), "unexpected error: {msg}");
        assert!(msg.contains("npub"), "unexpected error: {msg}");
    }
}
