//! Turning a user-typed `<ws>`/`<channel>` reference (or an untrusted
//! envelope's exact group id) into a `GroupId`, plus the 1:1 DM
//! find-or-create and the peer-contact bookkeeping that follows accepting an
//! invite.

use std::net::SocketAddr;

use moyu_core::engine::{AppClient, GroupId, MoyuEngine};
use moyu_core::identity::resolve_peer;
use moyu_core::store::{Contact, MoyuStore};
use moyu_core::workspace::{self, WorkspaceProjection};

use super::engine::group_id_from_hex;
use super::membership::{all_workspaces, membership_view};

/// Resolve a user-supplied `<ws>` (group-id hex short-prefix OR
/// workspace-name prefix, case-insensitive) to a single `(group_id_hex,
/// WorkspaceProjection)`, considering only workspaces the account still
/// belongs to (see [`all_workspaces`]). Ambiguous or absent matches error out
/// listing the candidates so the user can disambiguate.
pub(crate) fn resolve_workspace(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws_ref: &str,
) -> anyhow::Result<(String, WorkspaceProjection)> {
    if ws_ref.is_empty() {
        // An empty ref prefix-matches every workspace (vacuously) -- reject it
        // up front rather than reporting a spurious "ambiguous".
        anyhow::bail!("workspace reference is empty");
    }
    let ws_lower = ws_ref.to_lowercase();
    let mut hits: Vec<(String, WorkspaceProjection)> = all_workspaces(engine, client, label)?
        .into_iter()
        .filter(|(id, proj)| {
            let name = proj.name.as_deref().unwrap_or_default();
            id.starts_with(ws_ref) || name.to_lowercase().starts_with(&ws_lower)
        })
        .collect();
    match hits.len() {
        0 => anyhow::bail!("no workspace matches '{ws_ref}'"),
        1 => Ok(hits.pop().unwrap()),
        _ => {
            let names: Vec<String> = hits
                .iter()
                .map(|(id, proj)| {
                    format!(
                        "{} ({})",
                        proj.name.as_deref().unwrap_or_default(),
                        &id[..8.min(id.len())]
                    )
                })
                .collect();
            anyhow::bail!("'{ws_ref}' is ambiguous: {}", names.join(", "))
        }
    }
}

/// Resolve a workspace by its EXACT group-id hex. Unlike [`resolve_workspace`],
/// which prefix/name-matches a user-typed `<ws>` ref, this is for a group id
/// that arrived from an UNTRUSTED place -- a join-request envelope's `ws_gid` --
/// where fuzzy matching would be a category error: a crafted short gid could
/// prefix-resolve to an unintended-but-real workspace. Matches nothing but the
/// exact id (case-insensitive).
pub(crate) fn resolve_workspace_by_gid(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    gid_hex: &str,
) -> anyhow::Result<(String, WorkspaceProjection)> {
    all_workspaces(engine, client, label)?
        .into_iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(gid_hex))
        .ok_or_else(|| anyhow::anyhow!("no workspace matches group id {gid_hex}"))
}

/// Resolve `peer` (an existing contact's label/npub, or a fresh
/// `npub1...`/hex/NIP-05 reference) to their pubkey and the 1:1 MLS
/// `GroupId` to talk to them in, creating a brand-new group if no chat with
/// them exists yet. Shared by `chat` and `send` so "find or found the DM"
/// lives in exactly one place instead of being copy-pasted per command.
pub(crate) async fn find_or_create_dm(
    client: &mut AppClient,
    moyu_store: &mut MoyuStore,
    peer: &str,
    socks5: Option<SocketAddr>,
) -> anyhow::Result<(String, GroupId)> {
    let (peer_npub, existing_group_id_hex, contact_existed) = match moyu_store.find_contact(peer) {
        Some(c) => (c.npub.clone(), c.group_id_hex.clone(), true),
        None => (resolve_peer(peer, socks5).await?.npub, None, false),
    };

    let group_id = match existing_group_id_hex {
        Some(hex_id) => {
            let bytes = hex::decode(&hex_id)?;
            GroupId::new(bytes)
        }
        None => {
            // Progress/diagnostic -> stderr, never stdout: `moyu --json send`
            // (first contact with a new peer reaches here) must keep stdout a
            // clean JSONL stream. An un-gated stdout line here
            // injected a non-JSON line ahead of the send receipt.
            heprintln!(
                "No existing chat with {peer_npub}; creating a 1:1 group (this talks to relays)..."
            );
            // `AppClient::create_group(&mut self, name: &str, member_refs:
            // &[&str]) -> Result<GroupId, AppError>`
            // (`crates/marmot-app/src/client/mod.rs:219-277`) is the
            // "founding creation with initial invitees" step: it resolves
            // `peer_npub` to a KeyPackage itself (local cache -> directory
            // cache -> live relay fetch of kind-30443 events) and the
            // Welcome is published automatically inside this same call, no
            // separate publish step needed. Confirmed in
            // ../../docs/mdk-api-map.md.
            let group_id = client
                .create_group(workspace::DM_GROUP_NAME, &[peer_npub.as_str()])
                .await?;
            if contact_existed {
                // Preserve the existing contact's label / nip05; only bind the
                // freshly-created group to it (M2: do not clobber with a fresh
                // Contact built from the raw CLI arg).
                moyu_store.set_contact_group_id(&peer_npub, group_id.to_string())?;
            } else {
                moyu_store.upsert_contact(Contact {
                    npub: peer_npub.clone(),
                    nip05: None,
                    label: peer.to_string(),
                    group_id_hex: Some(group_id.to_string()),
                })?;
            }
            group_id
        }
    };

    Ok((peer_npub, group_id))
}

/// The channel slug the target message belongs to, IF it was a channel-enveloped
/// post (a public-channel `{"moyu":1,"ch":...}` payload). Looks the target up in
/// the local history by `(group_hex, message_id)`. `None` for a DM / unenveloped
/// message (or one not synced locally) — the reply then sends its text raw.
pub(crate) fn target_channel_slug(
    engine: &MoyuEngine,
    label: &str,
    group_hex: &str,
    target_id: &str,
) -> Option<String> {
    for r in engine.messages(label).ok()? {
        if r.group_id_hex == group_hex && r.message_id_hex == target_id {
            return channel_envelope_slug(&r.plaintext);
        }
    }
    None
}

/// The `ch` slug of a moyu channel envelope (`{"moyu":1,"ch":"<slug>",...}`), or
/// `None` if `plaintext` isn't such an envelope (a DM / raw line). An empty slug
/// is treated as absent (it round-trips to #general anyway).
pub(crate) fn channel_envelope_slug(plaintext: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(plaintext).ok()?;
    if v.get("moyu").and_then(|m| m.as_u64()) != Some(1) {
        return None;
    }
    v.get("ch")
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Offline group resolution for `history`: a group-id hex (or unique prefix)
/// matching one local group. Deliberately hex-only: workspace-NAME resolution
/// (`resolve_group_id`) needs an open client for membership checks, and
/// `history` keeps `search`'s offline contract (engine reads only, no client,
/// no relay traffic).
pub(crate) fn resolve_group_hex_offline(
    engine: &MoyuEngine,
    label: &str,
    target: &str,
) -> anyhow::Result<String> {
    let t = target.trim().to_lowercase();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!(
            "history takes a group-id hex (or unique prefix) -- find it via \
             `moyu conversations`, `workspace list`, or `recv --json`'s `group`"
        );
    }
    let hits: Vec<String> = engine
        .groups(label)?
        .into_iter()
        .map(|g| g.group_id_hex)
        .filter(|h| h.to_lowercase().starts_with(&t))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no local group matches {target:?}"),
        _ => anyhow::bail!(
            "ambiguous group prefix {target:?} (matches {} groups) -- use a longer prefix",
            hits.len()
        ),
    }
}

/// The persisted fallback `Contact.label` when nothing better is known: the
/// npub's first 16 chars. Used at every site that upserts a `Contact` without
/// an explicit label (`save_peer_contact_for_group`, `ops::add`, the TUI's
/// join-time contact binding) -- the three call sites must agree, since a
/// contact created via any of them can be looked up by the others.
pub(crate) fn default_contact_label(npub: &str) -> String {
    npub.chars().take(16).collect()
}

/// After accepting an invite into `group_id`, remember the (single, for a
/// 1:1 group) remote member as a `Contact` bound to this group -- so a
/// subsequent `moyu send <their-npub> ...` replies into the same group
/// instead of trying to found a second one. Does not clobber an
/// already-known contact's label/nip05, matching `find_or_create_dm`'s M2
/// no-clobber rule.
pub(crate) fn save_peer_contact_for_group(
    client: &AppClient,
    moyu_store: &mut MoyuStore,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    for member in client.members(group_id)? {
        if member.local {
            continue;
        }
        let npub = moyu_core::identity::npub_from_hex(&member.member_id_hex)?;
        match moyu_store.find_contact(&npub) {
            Some(_) => moyu_store.set_contact_group_id(&npub, group_id.to_string())?,
            None => moyu_store.upsert_contact(Contact {
                npub: npub.clone(),
                nip05: None,
                label: default_contact_label(&npub),
                group_id_hex: Some(group_id.to_string()),
            })?,
        }
    }
    Ok(())
}

/// Group hexes whose MDK group-profile name is the DM sentinel
/// ([`workspace::DM_GROUP_NAME`]) -- the only groups that may be treated as a
/// 1:1 DM. A workspace's profile name is its workspace name, so this cleanly
/// excludes workspaces even before their moyu control-plane name has synced
/// (the DM/workspace misclassification race). A read failure yields an empty set (fail closed: no group
/// is treated as a DM), which is the safe direction. Shared by the TUI's
/// incoming-message routing and `sync_tick`'s `dm` event flag.
pub(crate) fn dm_group_hexes(
    engine: &MoyuEngine,
    label: &str,
) -> std::collections::HashSet<String> {
    engine
        .groups(label)
        .map(|groups| {
            groups
                .into_iter()
                .filter(|g| g.profile.name == workspace::DM_GROUP_NAME)
                .map(|g| g.group_id_hex)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a workspace name + channel slug to the MLS group to send into, shared
/// by `post`/`op`/`activity` so their channel routing stays identical. A PRIVATE
/// channel routes to its OWN group (its own epoch key); a PUBLIC channel routes
/// to the workspace group after confirming the channel exists (no silent
/// misroute into a channel nobody created). Checked private-first so a private
/// slug never leaks to the public path. Returns `(group id, its hex, is_private,
/// normalized slug)`.
pub(crate) fn resolve_channel_target(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    ws: &str,
    channel: &str,
) -> anyhow::Result<(GroupId, String, bool, String)> {
    let (id, proj) = resolve_workspace(engine, client, label, ws)?;
    let slug = workspace::normalize_slug(channel);
    let (_, private_channels) = membership_view(engine, client, label)?;
    match private_channels
        .iter()
        .find(|pc| pc.parent_group_id_hex == id && pc.slug == slug)
    {
        Some(pc) => Ok((
            group_id_from_hex(&pc.group_id_hex)?,
            pc.group_id_hex.clone(),
            true,
            slug,
        )),
        None => {
            if proj.channel(&slug).is_none() {
                anyhow::bail!(
                    "no channel #{slug} in workspace {}",
                    proj.name.as_deref().unwrap_or_default()
                );
            }
            Ok((group_id_from_hex(&id)?, id.clone(), false, slug))
        }
    }
}

/// Resolve a `download` target to a `GroupId`: a group-id hex / unique hex
/// prefix (what `recv --json` emits as `group`, works for DMs/private channels
/// too) wins; otherwise a workspace name/prefix via [`resolve_workspace`].
pub(crate) fn resolve_group_id(
    engine: &MoyuEngine,
    client: &AppClient,
    label: &str,
    target: &str,
) -> anyhow::Result<GroupId> {
    let t = target.trim().to_lowercase();
    // Only treat it as a hex handle if it actually looks like hex, so a
    // workspace name never accidentally prefix-collides with a group id.
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let groups = engine.groups(label)?;
        let hits: Vec<String> = groups
            .into_iter()
            .map(|g| g.group_id_hex)
            .filter(|h| h.to_lowercase().starts_with(&t))
            .collect();
        match hits.as_slice() {
            [one] => return group_id_from_hex(one),
            [] => {} // fall through to workspace-name resolution
            _ => anyhow::bail!(
                "ambiguous group prefix {target:?} (matches {} groups) -- use a longer prefix",
                hits.len()
            ),
        }
    }
    let (id, _) = resolve_workspace(engine, client, label, target)?;
    group_id_from_hex(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply to a public-channel post is wrapped in that channel's
    /// envelope (so it stays in-channel, not #general). `channel_envelope_slug`
    /// pulls the slug only from a real moyu envelope, ignoring DM/raw lines.
    #[test]
    fn channel_envelope_slug_extracts_public_channel_only() {
        assert_eq!(
            channel_envelope_slug(r#"{"moyu":1,"ch":"backend","body":"x"}"#).as_deref(),
            Some("backend")
        );
        // Raw DM text is not an envelope.
        assert_eq!(channel_envelope_slug("just a dm"), None);
        // An envelope with an empty ch routes to #general anyway -> absent.
        assert_eq!(
            channel_envelope_slug(r#"{"moyu":1,"ch":"","body":"x"}"#),
            None
        );
        // A different version marker is not a v1 envelope.
        assert_eq!(channel_envelope_slug(r#"{"moyu":2,"ch":"x"}"#), None);
    }
}
