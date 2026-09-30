//! The Marmot inner-event kind/tag constants (aliased to `moyu_core::kinds`,
//! the canonical upstream `cgka_traits::app_event` values -- never hand-copied
//! magic numbers) and the message/attachment encode-decode helpers built on
//! them: bot-event channel routing and rendering, local search, MIME
//! guessing, and the upload/download attachment plumbing.

use std::path::PathBuf;

use anyhow::Context;
use moyu_core::engine::{
    AppClient, GroupId, MediaAttachmentReference, MediaUploadAttachmentRequest, MediaUploadRequest,
    MediaUploadResult, MoyuEngine, media_attachment_from_imeta_tag,
};
use moyu_core::workspace;

/// `message_ids` (or any id list) as display strings, for `--json` receipts.
fn id_strings<T: ToString>(ids: &[T]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// How long to wait between drain polls, and how the "once" (default) mode
/// decides it has waited long enough: relays are unordered and delivery is
/// not instant (a Welcome and the first chat message can even arrive out of
/// order), so a single `sync()` is not reliable -- keep polling until a
/// couple of polls in a row make no progress, up to a hard cap either way.
pub(crate) const RECV_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const RECV_MAX_IDLE_POLLS: usize = 4;
pub(crate) const RECV_MAX_TOTAL_POLLS: usize = 12;

/// Nostr `kind` of a Marmot chat message (vs 7 reaction, 1210 group-system --
/// matches `tui.rs`'s `CHAT_MESSAGE_KIND`). Now that workspace commands send
/// `moyu.*` control events as kind-1210 group-system messages
/// (`send_group_system_event`), `sync()`'s `SyncSummary.messages` surfaces
/// those alongside real chat -- without this filter `recv` would print each
/// one's raw control-message JSON as if it were a chat line. Aliased to the
/// canonical upstream constant (`crates/traits/src/app_event.rs`) via
/// `moyu_core::kinds`, never a hand-copied literal.
pub(crate) const CHAT_MESSAGE_KIND: u64 = moyu_core::kinds::MARMOT_APP_EVENT_KIND_CHAT;

/// Nostr `kind` of a Marmot reaction (kind-7). `recv` surfaces these as their
/// own `reaction` event/line rather than folding them away, so a bot can watch
/// for acknowledgements. Aliased to MDK's `MARMOT_APP_EVENT_KIND_REACTION`
/// (crates/traits/src/app_event.rs) via `moyu_core::kinds`.
pub(crate) const REACTION_KIND: u64 = moyu_core::kinds::MARMOT_APP_EVENT_KIND_REACTION;

/// Nostr `kind`s of Marmot bot/agent events. An AGENT_OPERATION
/// (kind-1202) is a structured CI/deploy/git/monitoring event; an AGENT_ACTIVITY
/// (kind-1201) is a lighter "bot said/did X" line. Both are protocol-level bot
/// markers (a peer tells them apart from a human kind-9) and both surface in
/// `recv`/TUI with a 🤖 marker rather than being folded away. Aliased to MDK's
/// `MARMOT_APP_EVENT_KIND_AGENT_{OPERATION,ACTIVITY}` (crates/traits/src/app_event.rs)
/// via `moyu_core::kinds`.
pub(crate) const AGENT_OPERATION_KIND: u64 =
    moyu_core::kinds::MARMOT_APP_EVENT_KIND_AGENT_OPERATION;
pub(crate) const AGENT_ACTIVITY_KIND: u64 = moyu_core::kinds::MARMOT_APP_EVENT_KIND_AGENT_ACTIVITY;

/// Tag names MDK stamps on reactions/replies (crates/traits/src/app_event.rs).
/// A reaction/reply references its target via `["e", <message_id>]`; a reply
/// additionally carries `["q", <message_id>]` (the quote), which is what tells a
/// threaded reply apart from a plain kind-9 chat line (chat carries only `p`
/// mention tags, never `e`/`q`). Aliased to `moyu_core::kinds`.
pub(crate) const EVENT_REF_TAG: &str = moyu_core::kinds::EVENT_REF_TAG;
pub(crate) const QUOTE_REF_TAG: &str = moyu_core::kinds::QUOTE_REF_TAG;

/// First value of the first tag named `name` (i.e. `tag[0] == name`), if any.
/// Used to pull the `e`/`q` target off a reaction or reply. A tag that is just
/// its name with no value (`["q"]`) is treated as absent.
pub(crate) fn first_tag_value<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|t| t.first().map(String::as_str) == Some(name))
        .and_then(|t| t.get(1))
        .map(String::as_str)
}

/// The channel slug a bot event recorded so `recv`/TUI can label it.
/// A kind-1202 op nests it under `details.moyu.ch`, a kind-1201 activity under
/// `extra.moyu.ch` (MDK stores the whole `details`/`extra` object verbatim inside
/// the event content). `None` only for a payload without routing (e.g. an event
/// from a non-moyu agent), in which case the caller falls back to #general.
pub(crate) fn bot_event_channel(content: &serde_json::Value) -> Option<String> {
    let slug = content
        .get("details")
        .or_else(|| content.get("extra"))
        .and_then(|v| v.get("moyu"))
        .and_then(|v| v.get("ch"))
        .and_then(|v| v.as_str())?;
    (!slug.is_empty()).then(|| slug.to_owned())
}

/// Human-readable body of a bot event line — the part after
/// `[group] 🤖 who #slug`. An op renders `[type·status] name: text ✓ (4.2s)`
/// (✓/✗ from `ok`, elapsed from `duration_ms`); an activity renders
/// `[status] text`. Missing fields degrade gracefully to empty.
pub(crate) fn format_bot_event_body(content: &serde_json::Value, is_op: bool) -> String {
    let status = content.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let text = content.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if !is_op {
        return format!("[{status}] {text}");
    }
    let event_type = content
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("op");
    let name = content.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let mark = match content.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => " ✓",
        Some(false) => " ✗",
        None => "",
    };
    let dur = content
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .map(|ms| format!(" ({:.1}s)", ms as f64 / 1000.0))
        .unwrap_or_default();
    let name_part = if name.is_empty() {
        String::new()
    } else {
        format!(" {name}:")
    };
    format!("[{event_type}·{status}]{name_part} {text}{mark}{dur}")
}

/// Record a bot event's channel `slug` under a `moyu.ch` key so the receiver can
/// label the (envelope-less) event with the right channel — used for both `op`'s
/// `details` (kind-1202) and `activity`'s `extra` (kind-1201), for public and
/// private channels alike (a private channel's slug is already known to its
/// members). `None` becomes `{"moyu":{"ch":slug}}`; an existing JSON object gets
/// the key merged in; a non-object payload is a user error (we can't attach
/// routing to a bare scalar/array).
pub(crate) fn agent_op_details_with_channel(
    details: Option<serde_json::Value>,
    slug: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    match details {
        None => Ok(Some(serde_json::json!({ "moyu": { "ch": slug } }))),
        Some(mut v) => {
            let obj = v.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!(
                    "--details must be a JSON object when posting to public channel #{slug}"
                )
            })?;
            obj.insert("moyu".to_owned(), serde_json::json!({ "ch": slug }));
            Ok(Some(v))
        }
    }
}

/// A chat record matches a search when its decoded human body contains
/// `needle` (which must already be lowercased). Returns the decoded
/// `(slug, body)` on a hit so the caller renders the body a bot would see, not
/// the raw `{"moyu":1,...}` channel envelope. Pure + owned-return, so it is
/// unit-tested directly.
pub(crate) fn search_match(plaintext: &str, needle: &str) -> Option<(String, String)> {
    let (slug, body) = workspace::decode_channel_body(plaintext);
    if body.to_lowercase().contains(needle) {
        Some((slug, body))
    } else {
        None
    }
}

/// Best-effort MIME type from a file's extension, for an attachment's
/// `media_type`. A small built-in map (no extra dependency); an
/// unknown extension falls back to `application/octet-stream`, which every
/// client can still store and download.
pub(crate) fn guess_media_type(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// The file name to advertise for an attachment (the path's final component),
/// erroring on a path with no usable name (e.g. `/` or a `..` tail).
pub(crate) fn media_file_name(path: &std::path::Path) -> anyhow::Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("--file path has no file name: {}", path.display()))
}

/// Read a file, guess its MIME type, and `upload_media(send: true)` it to a
/// group in one call (encrypt → Blossom upload → send the kind-9 `imeta`
/// message). Shared by `send --file` and `post --file`. `caption`
/// is the message body carried alongside the attachment -- for a workspace
/// channel the caller has already channel-enveloped it.
pub(crate) async fn upload_one_file(
    client: &mut AppClient,
    group_id: &GroupId,
    path: &std::path::Path,
    caption: Option<String>,
    blossom: Option<String>,
) -> anyhow::Result<MediaUploadResult> {
    let plaintext =
        std::fs::read(path).with_context(|| format!("reading --file {}", path.display()))?;
    let file_name = media_file_name(path)?;
    let media_type = guess_media_type(path).to_owned();
    let request = MediaUploadRequest {
        attachments: vec![MediaUploadAttachmentRequest {
            file_name,
            media_type,
            plaintext,
            dim: None,
            thumbhash: None,
        }],
        // An empty caption -> None so we don't post a blank body; the imeta
        // attachment still travels in the message tags. A workspace channel's
        // caption is the `{"moyu":1,..}` envelope, which is never empty, so
        // channel routing is preserved.
        caption: caption.filter(|c| !c.is_empty()),
        send: true,
        blossom_server: blossom,
    };
    Ok(client.upload_media(group_id, request).await?)
}

/// The `(published, message_ids, attachments)` JSON pieces of an upload receipt,
/// shared by the `send`/`post` `--file` receipts. `published` mirrors the text
/// receipts' field so the JSON contract stays symmetric.
pub(crate) fn attachment_receipt_parts(
    result: &MediaUploadResult,
) -> (usize, Vec<String>, Vec<serde_json::Value>) {
    let published = result.sent.as_ref().map(|s| s.published).unwrap_or(0);
    let ids = result
        .sent
        .as_ref()
        .map(|s| id_strings(&s.message_ids))
        .unwrap_or_default();
    let attachments = result
        .attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "file_name": a.reference.file_name,
                "media_type": a.reference.media_type,
                "ciphertext_sha256": a.reference.ciphertext_sha256,
                "encrypted_size": a.encrypted_size_bytes,
            })
        })
        .collect();
    (published, ids, attachments)
}

/// Human-readable comma-joined attachment file names, for the non-JSON receipt.
pub(crate) fn attachment_names(result: &MediaUploadResult) -> String {
    result
        .attachments
        .iter()
        .map(|a| a.reference.file_name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Scan a group's persisted messages for the attachment whose `ciphertext_sha256`
/// (or `plaintext_sha256`) matches `hash`, rebuilding each `imeta` tag into a
/// [`MediaAttachmentReference`] via the public MDK helper. This is how
/// `download` addresses an attachment by content hash (as shown in `recv
/// --json`).
pub(crate) fn find_attachment_ref(
    engine: &MoyuEngine,
    label: &str,
    group_hex: &str,
    hash: &str,
    allow_loopback: bool,
) -> anyhow::Result<MediaAttachmentReference> {
    let hash = hash.trim().to_lowercase();
    for record in engine.messages(label)? {
        if record.group_id_hex != group_hex {
            continue;
        }
        for tag in &record.tags {
            if tag.first().map(String::as_str) != Some("imeta") {
                continue;
            }
            if let Ok(reference) =
                media_attachment_from_imeta_tag(tag, record.source_epoch, allow_loopback)
                && (reference.ciphertext_sha256.eq_ignore_ascii_case(&hash)
                    || reference.plaintext_sha256.eq_ignore_ascii_case(&hash))
            {
                return Ok(reference);
            }
        }
    }
    anyhow::bail!(
        "no attachment with hash {hash} in group {} (has it synced? try `moyu recv` first)",
        &group_hex[..8.min(group_hex.len())]
    )
}

/// Decide where a downloaded file lands. A `--out` directory (or the default
/// cwd) writes `<dir>/<file_name>` with the sender-controlled `file_name`
/// sanitized to its final component (so a crafted `"../../x"` can't escape the
/// target directory); an explicit `--out` file path is used verbatim (the user
/// chose it).
pub(crate) fn resolve_download_path(out: Option<PathBuf>, file_name: &str) -> PathBuf {
    let safe = std::path::Path::new(file_name)
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("download.bin"));
    match out {
        Some(p) if p.is_dir() => p.join(safe),
        Some(p) => p,
        None => safe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Attachment MIME guessing: extension-mapped, case-insensitive,
    /// unknown -> the universal `application/octet-stream`.
    #[test]
    fn guess_media_type_maps_extensions_case_insensitively() {
        use std::path::Path;
        assert_eq!(guess_media_type(Path::new("a.png")), "image/png");
        assert_eq!(guess_media_type(Path::new("A.PNG")), "image/png");
        assert_eq!(guess_media_type(Path::new("photo.jpeg")), "image/jpeg");
        assert_eq!(guess_media_type(Path::new("doc.pdf")), "application/pdf");
        assert_eq!(
            guess_media_type(Path::new("noext")),
            "application/octet-stream"
        );
        assert_eq!(
            guess_media_type(Path::new("weird.xyz")),
            "application/octet-stream"
        );
    }

    /// Download path safety: a sender-controlled `file_name` cannot
    /// escape the target directory (path traversal), while an explicit `--out`
    /// file path is honored verbatim.
    #[test]
    fn resolve_download_path_sanitizes_sender_file_name() {
        use std::path::PathBuf;
        // Default (cwd) target: only the final component of a crafted name is
        // used, so `../../etc/passwd` becomes `passwd`.
        assert_eq!(
            resolve_download_path(None, "../../etc/passwd"),
            PathBuf::from("passwd")
        );
        assert_eq!(
            resolve_download_path(None, "report.txt"),
            PathBuf::from("report.txt")
        );
        // Into an existing directory: joins the sanitized final component only.
        let dir = std::env::temp_dir();
        assert_eq!(
            resolve_download_path(Some(dir.clone()), "../../evil.sh"),
            dir.join("evil.sh")
        );
        // An explicit (non-directory) file path is used as given.
        let explicit = PathBuf::from("/tmp/moyu-dl-explicit-9f3a.bin");
        assert_eq!(
            resolve_download_path(Some(explicit.clone()), "sender-name.txt"),
            explicit
        );
    }

    /// Reaction/reply tag parsing: `first_tag_value` pulls the
    /// `e`/`q` target off a reaction or reply, tells a threaded reply (`q`)
    /// apart from a plain chat line (only `p` mentions), and treats a bare
    /// value-less tag as absent — this is what drives `recv`'s `reply_to` /
    /// reaction surfacing.
    #[test]
    fn first_tag_value_finds_reply_quote_and_ignores_plain_chat() {
        let reply = vec![
            vec!["e".to_owned(), "abc123".to_owned()],
            vec!["q".to_owned(), "abc123".to_owned()],
        ];
        assert_eq!(first_tag_value(&reply, QUOTE_REF_TAG), Some("abc123"));
        assert_eq!(first_tag_value(&reply, EVENT_REF_TAG), Some("abc123"));

        // A plain chat line carries only `p` mention tags -> not a reply.
        let chat = vec![vec!["p".to_owned(), "deadbeef".to_owned()]];
        assert_eq!(first_tag_value(&chat, QUOTE_REF_TAG), None);
        assert_eq!(first_tag_value(&chat, EVENT_REF_TAG), None);

        // A tag that is just its name with no value is treated as absent.
        let bare = vec![vec!["q".to_owned()]];
        assert_eq!(first_tag_value(&bare, QUOTE_REF_TAG), None);
    }

    /// A public-channel bot op carries no kind-9 channel envelope, so
    /// its channel slug is stamped into `details.moyu.ch`: absent details become
    /// `{"moyu":{"ch":slug}}`, an existing object gets the key merged in
    /// (preserving the caller's own fields), and a non-object `--details` is a
    /// hard error rather than silently dropping the channel routing.
    #[test]
    fn agent_op_details_stamps_public_channel_slug() {
        // None -> a fresh {moyu:{ch}} object.
        let made = agent_op_details_with_channel(None, "ci").unwrap().unwrap();
        assert_eq!(made, serde_json::json!({ "moyu": { "ch": "ci" } }));
        // Existing object -> slug merged in, caller's fields preserved.
        let merged =
            agent_op_details_with_channel(Some(serde_json::json!({ "sha": "abc" })), "backend")
                .unwrap()
                .unwrap();
        assert_eq!(
            merged,
            serde_json::json!({ "sha": "abc", "moyu": { "ch": "backend" } })
        );
        // A non-object payload can't carry routing -> user error.
        assert!(agent_op_details_with_channel(Some(serde_json::json!("scalar")), "ci").is_err());
        assert!(agent_op_details_with_channel(Some(serde_json::json!([1, 2])), "ci").is_err());
    }

    /// `recv` recovers a public-channel bot event's slug from where it was
    /// stamped: an op nests it under `details.moyu.ch`, an activity under
    /// `extra.moyu.ch`. A private-channel event (its group IS the channel) or a
    /// routing-less payload yields `None` (the caller falls back to #general).
    #[test]
    fn bot_event_channel_reads_details_or_extra_moyu_ch() {
        // op: details.moyu.ch
        let op = serde_json::json!({ "status": "failed", "details": { "moyu": { "ch": "ci" } } });
        assert_eq!(bot_event_channel(&op).as_deref(), Some("ci"));
        // activity: extra.moyu.ch
        let act = serde_json::json!({ "status": "active", "extra": { "moyu": { "ch": "ops" } } });
        assert_eq!(bot_event_channel(&act).as_deref(), Some("ops"));
        // no routing (private channel / bare event) -> None
        assert_eq!(
            bot_event_channel(&serde_json::json!({ "status": "ok" })),
            None
        );
        // empty slug -> None (never routes to "")
        let empty = serde_json::json!({ "details": { "moyu": { "ch": "" } } });
        assert_eq!(bot_event_channel(&empty), None);
    }

    /// The bot event line body: an op renders type·status, an optional
    /// `name:`, the text, a ✓/✗ from `ok`, and an elapsed `(N.Ns)` from
    /// `duration_ms`; an activity renders just `[status] text`. Absent optional
    /// fields drop out cleanly.
    #[test]
    fn format_bot_event_body_renders_op_and_activity() {
        // Full op: failed CI with a name + duration.
        let op = serde_json::json!({
            "event_type": "ci", "status": "failed", "name": "build",
            "text": "3 tests failed", "ok": false, "duration_ms": 4200
        });
        assert_eq!(
            format_bot_event_body(&op, true),
            "[ci·failed] build: 3 tests failed ✗ (4.2s)"
        );
        // Op without ok/name/duration: no marker, no name prefix, no elapsed.
        let bare =
            serde_json::json!({ "event_type": "deploy", "status": "running", "text": "starting" });
        assert_eq!(
            format_bot_event_body(&bare, true),
            "[deploy·running] starting"
        );
        // ok:true -> ✓.
        let good = serde_json::json!({ "event_type": "ci", "status": "success", "text": "green", "ok": true });
        assert_eq!(format_bot_event_body(&good, true), "[ci·success] green ✓");
        // Activity ignores op-only fields.
        let act = serde_json::json!({ "status": "active", "text": "on it" });
        assert_eq!(format_bot_event_body(&act, false), "[active] on it");
    }

    /// Local search: the match runs case-insensitively on the decoded
    /// human body, so a channel-enveloped workspace post matches on its text
    /// (not the `{"moyu":1,...}` wrapper) and reports the channel slug, while a
    /// plain DM line decodes to `#general` with its text preserved. A miss is
    /// `None`.
    #[test]
    fn search_match_runs_on_decoded_body_case_insensitively() {
        // Workspace post envelope: matches on body, returns the real slug.
        let enveloped = r#"{"moyu":1,"ch":"ci","body":"Deploy SUCCEEDED ✅"}"#;
        let hit = search_match(enveloped, "deploy").expect("body contains 'deploy'");
        assert_eq!(hit.0, "ci");
        assert_eq!(hit.1, "Deploy SUCCEEDED ✅");
        // The envelope keys themselves must not be searchable content.
        assert_eq!(search_match(enveloped, "moyu"), None);
        // Plain DM text decodes to the default channel, text preserved.
        let dm = search_match("Let's ship it", "ship").expect("dm body contains 'ship'");
        assert_eq!(dm.0, "general");
        assert_eq!(dm.1, "Let's ship it");
        // A miss is None.
        assert_eq!(search_match("nothing here", "absent"), None);
    }
}
