// Typed client over the Rust core's three invoke commands + two event
// channels. This file is where the session wire contract
// (crates/moyu-cli/src/session/protocol.rs) is mirrored into TS — the Rust
// bridge stays generic on purpose.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Lock state as reported by hello frames / supervisor lifecycle. */
export type SessionState =
  | "starting"
  | "no-account"
  | "locked"
  | "unlocked"
  | "down"
  | "restarting";

export interface StatusPayload {
  state: SessionState;
  account?: { present: boolean; label?: string; npub?: string };
  capabilities?: string[];
  fatal?: unknown;
}

export interface AttachmentRef {
  file_name: string;
  media_type: string;
  ciphertext_sha256: string;
}

/**
 * One display row — identical shape for live `message`-family events and
 * history rows (`ops::record_to_event` guarantees it); history rows
 * additionally carry `direction`.
 */
export interface MessageRow {
  event: "message" | "reaction" | "agent_op" | "agent_activity";
  group: string;
  channel?: string;
  /** live `message` events only: the group is a 1:1 DM (sidecar-verified) */
  dm?: boolean;
  sender: string;
  sender_name?: string | null;
  kind?: number;
  ts: number;
  message_id: string;
  body?: string;
  reply_to?: string;
  attachments?: AttachmentRef[];
  direction?: "sent" | "received";
  /** reaction rows */
  emoji?: string;
  target?: string;
  /** agent_op/agent_activity rows */
  status?: string;
  text?: string;
  event_type?: string;
  name?: string;
  ok?: boolean;
  duration_ms?: number;
  run_id?: string;
  /** client-side only: optimistic-send lifecycle */
  pending?: boolean;
  failed?: boolean;
}

/** One async session event (`type:"event"`), discriminated by `event`. */
export interface SessionEvent extends Partial<Omit<MessageRow, "event">> {
  v: number;
  type: "event";
  event:
    | "message"
    | "joined"
    | "reaction"
    | "agent_op"
    | "agent_activity"
    | "join_request"
    | "governance"
    | "snapshot_rebroadcast"
    | "approve"
    | "sync_error";
  [key: string]: unknown;
}

export interface ConvChannel {
  slug: string;
  name: string;
  private: boolean;
  archived: boolean;
  /** private channel's own group id (null for public channels) */
  group: string | null;
}

export interface Conversations {
  dms: { group: string; npub?: string; label?: string; nip05?: string }[];
  workspaces: { group: string; name: string; channels: ConvChannel[] }[];
  contacts: { npub: string; label: string; nip05?: string }[];
}

export interface HistoryPage {
  group: string;
  messages: MessageRow[];
  next_cursor: string | null;
}

/** `Err` strings from the bridge are `"<stable_code>: <human text>"`. */
export class SessionError extends Error {
  readonly code: string;
  constructor(raw: string) {
    const idx = raw.indexOf(": ");
    super(idx > 0 ? raw.slice(idx + 2) : raw);
    this.code = idx > 0 ? raw.slice(0, idx) : "engine";
  }
}

async function request<T>(cmd: string, args: unknown = null): Promise<T> {
  try {
    return (await invoke<T>("session_request", { cmd, args })) as T;
  } catch (e) {
    throw new SessionError(String(e));
  }
}

export async function unlock(passphrase: string): Promise<{
  state: "unlocked";
  label: string;
  npub?: string;
}> {
  try {
    return await invoke("unlock", { passphrase });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

export const whoami = () =>
  request<{ present: boolean; label?: string; npub?: string }>("whoami");

export const lock = () => request<{ state: string }>("lock");

/** One synchronous relay sync pass. The sidecar runs a 2 s event pump while
 * unlocked but no MDK auto-reconnect: after a dropped socket (sleep, network
 * change) the pump keeps failing quietly until something re-drives it. The
 * shell calls this on OS `online` / window-foreground signals. */
export const catchup = () => request<{ progressed: boolean }>("catchup");

export const conversations = () => request<Conversations>("conversations");

export const history = (group: string, before?: string, limit = 50) =>
  request<HistoryPage>("history", { group, before, limit });

/** 1:1 DM text send. */
export const sendDm = (peer: string, message: string) =>
  request<{ group: string; message_ids: string[] }>("send", { peer, message });

/** Workspace-channel text post (`ws` = workspace group hex). */
export const post = (ws: string, channel: string, message: string) =>
  request<{ group: string; channel: string; message_ids: string[] }>("post", {
    ws,
    channel,
    message,
  });

/** Threaded reply to a message in a group. */
export const reply = (group: string, messageId: string, message: string) =>
  request<{ group: string; target: string; message_ids: string[] }>("reply", {
    group,
    message_id: messageId,
    message,
  });

/** Send (or retract) a kind-7 reaction on a message. */
export const react = (
  group: string,
  messageId: string,
  emoji: string,
  remove = false,
) =>
  request<{ group: string; target: string; action: string }>("react", {
    group,
    message_id: messageId,
    emoji,
    remove,
  });

export const sessionStatus = () => invoke<string>("session_status");

export const onSessionStatus = (
  cb: (payload: StatusPayload) => void,
): Promise<UnlistenFn> =>
  listen<StatusPayload>("moyu://session-status", (e) => cb(e.payload));

export const onSessionEvent = (
  cb: (ev: SessionEvent) => void,
): Promise<UnlistenFn> =>
  listen<SessionEvent>("moyu://event", (e) => cb(e.payload));

// ------------------------------------------------- network settings (spawn-time)
//
// The two connectivity flags the CLI supports (`--socks5`, `--dev-allow-loopback`)
// but the GUI couldn't set. They're spawn-time globals, so `net_settings_set`
// persists them and restarts the sidecar; both commands are dedicated Rust
// commands (NOT the session bridge) that work in every state, including before
// the sidecar is up. See src-tauri/src/netcfg.rs.

export interface NetSettings {
  /** SOCKS5 proxy `IP:PORT`, or null for a direct connection. */
  socks5: string | null;
  /** pass `--dev-allow-loopback` (use a `ws://127.0.0.1` relay for testing). */
  allow_loopback: boolean;
}

export async function netSettingsGet(): Promise<NetSettings> {
  try {
    return await invoke<NetSettings>("net_settings_get");
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** Validate + persist + apply (restarts the sidecar so the new flags take
 * effect). Returns the normalized settings the UI should now show; rejects on
 * an unparseable proxy address (never spawns a crash-looping sidecar). */
export async function netSettingsSet(
  socks5: string | null,
  allowLoopback: boolean,
): Promise<NetSettings> {
  try {
    return await invoke<NetSettings>("net_settings_set", {
      socks5,
      allowLoopback,
    });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

// ----------------------------------------------------------- Attachments
//
// The webview NEVER supplies a raw filesystem path (see src-tauri's
// FORBIDDEN_ARGS). `pickAttachment` and the drag-drop listeners below only
// ever hand back an opaque `token` — the real path lives Rust-side in
// `Attachments` (src-tauri/src/attachments.rs) and is resolved there by
// `send_attachment`.

/** `pick_attachment`'s success payload — an opaque token + display metadata. */
export interface StagedFile {
  token: number;
  file_name: string;
  size: number;
}

/** `send_attachment`'s receipt — same shape as `sendDm`/`post`'s, plus the
 * attachment refs the sidecar actually published. */
export interface SendAttachmentReceipt {
  peer?: string;
  group: string;
  channel?: string;
  published: number;
  message_ids: string[];
  attachments?: AttachmentRef[];
}

/** `download_attachment`'s receipt. `media_type` is `null` on a cache hit
 * (the sidecar isn't consulted, so there's no fresh MIME guess). */
export interface DownloadReceipt {
  group: string;
  file_name: string;
  media_type: string | null;
  size: number;
  path: string;
  cached?: boolean;
}

/** Native open-file dialog. `null` if the user cancelled. */
export async function pickAttachment(): Promise<StagedFile | null> {
  try {
    return await invoke<StagedFile | null>("pick_attachment");
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** Send a staged attachment to a DM peer or a channel post. `token` is
 * consumed server-side the instant this call is made (success or failure) —
 * it can never be reused for a retry. */
export async function sendAttachment(
  target: { peer: string } | { ws: string; channel: string },
  token: number,
  caption?: string,
): Promise<SendAttachmentReceipt> {
  try {
    return await invoke<SendAttachmentReceipt>("send_attachment", {
      target,
      token,
      caption,
    });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** Fetch (or serve from the local cache) the plaintext bytes behind one
 * `ciphertext_sha256` reference. */
export async function downloadAttachment(
  group: string,
  hash: string,
): Promise<DownloadReceipt> {
  try {
    return await invoke<DownloadReceipt>("download_attachment", {
      group,
      hash,
    });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** Reveal a previously downloaded attachment in the OS file manager. */
export async function revealAttachment(
  group: string,
  hash: string,
): Promise<void> {
  try {
    await invoke<void>("reveal_attachment", { group, hash });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

export const onFileDrop = (
  cb: (files: StagedFile[]) => void,
): Promise<UnlistenFn> =>
  listen<StagedFile[]>("moyu://file-drop", (e) => cb(e.payload));

export const onFileDrag = (
  cb: (payload: { hovering: boolean }) => void,
): Promise<UnlistenFn> =>
  listen<{ hovering: boolean }>("moyu://file-drag", (e) => cb(e.payload));

// ------------------------------------------- Onboarding & social
//
// Passphrase- and nsec-bearing commands (`init`, `join`) NEVER transit the
// generic `session_request` bridge — they have dedicated Rust commands
// (`session_init` / `session_join`) that wrap the secret in `Zeroizing`
// (src-tauri/src/lib.rs). Everything else here (contacts / invites / join
// requests / relay config) carries no secret and rides the generic `request`
// helper above.

/** Onboarding relay choice. `default` omits the arg entirely (the sidecar
 * falls back to its resolved default — the public built-ins on a fresh
 * machine); `custom` sends an explicit URL list (e.g. a self-hosted relay,
 * see docs/self-host-relay.md). moyu operates no relay of its own, so there
 * is deliberately no hosted preset here. */
export type RelayChoice =
  | { preset: "default" }
  | { preset: "custom"; urls: string[] };

export function relaysFor(choice: RelayChoice): string[] | undefined {
  if (choice.preset === "default") return undefined;
  return choice.urls;
}

/** The unlocked-state shape returned by both `init` and no-account `join`
 * (mirrors `unlock`'s success shape). */
export interface UnlockedReceipt {
  state: "unlocked";
  label: string;
  npub?: string;
}

/** Create (or, with `importNsec`, import) an identity. Both secrets go through
 * the dedicated `session_init` command; the caller MUST clear its own inputs
 * the instant this resolves or rejects. */
export async function initAccount(
  passphrase: string,
  opts: { importNsec?: string; relays?: RelayChoice } = {},
): Promise<UnlockedReceipt> {
  try {
    return await invoke<UnlockedReceipt>("session_init", {
      passphrase,
      importNsec: opts.importNsec,
      relays: opts.relays ? relaysFor(opts.relays) : undefined,
    });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** One `join` receipt. In the no-account state it also carries the
 * unlock-equivalent `state`/`label`/`npub`; in the unlocked state it is the
 * bare join body. */
export interface JoinReceipt {
  kind: "contact" | "workspace";
  inviter: string;
  ws_name: string | null;
  published: number;
  state?: "unlocked";
  label?: string;
  npub?: string;
}

/** Redeem an invite code. `passphrase` is required only in the no-account
 * state (it creates the account); omit it when already unlocked. Goes through
 * the dedicated `session_join` command. */
export async function joinByCode(
  code: string,
  passphrase?: string,
): Promise<JoinReceipt> {
  try {
    return await invoke<JoinReceipt>("session_join", { code, passphrase });
  } catch (e) {
    throw new SessionError(String(e));
  }
}

/** One pending join request (`requests` command row). */
export interface RequestRow {
  npub: string;
  ws_name: string;
  ws_gid: string;
  /** the request's bearer secret matches an invite THIS account issued for
   * this exact workspace — a "known code" badge, not identity proof. */
  trusted: boolean;
  ts: number;
}

export const listRequests = () =>
  request<{ requests: RequestRow[] }>("requests");

/** One approved/idempotent outcome from an `approve` batch. */
export interface ApproveOutcome {
  status: "approved" | "already_member";
  npub: string;
  ws_name: string;
}

/** `approve`'s aggregated receipt. `outcomes` really happened on the wire even
 * when `failed` is non-empty (partial success is still `ok:true`). */
export interface ApproveResult {
  outcomes: ApproveOutcome[];
  /** human-readable per-target error strings */
  failed: string[];
}

/** Approve a specific requester (`npub`) or `"all"` pending requests. */
export const approve = (who: string, auto = false) =>
  request<ApproveResult>("approve", { who, auto });

/** Locally dismiss a join request (v1 `deny` sends nothing on the wire). */
export const deny = (who: string) => request<{ who: string }>("deny", { who });

/** `add` receipt — the resolved contact. */
export interface AddReceipt {
  npub: string;
  label: string;
  nip05: string | null;
}

/** Add a contact by npub or nip-05. `peer` is the sidecar's arg name. */
export const addContact = (peer: string, label?: string) =>
  request<AddReceipt>("add", { peer, label });

/** `invite` receipt — a mintable invite token (`moyuinv1…`). The bare token
 * is the canonical, always-working form; `join` also accepts any link that
 * carries the token after a `#`. */
export interface InviteReceipt {
  token: string;
  kind: "workspace" | "contact";
  ws_name: string | null;
}

/** Mint an invite code. Omit `workspace` for a 1:1 contact invite; pass a
 * workspace slug for a workspace invite. */
export const createInvite = (workspace?: string, autoApprove = false) =>
  request<InviteReceipt>("invite", {
    workspace,
    auto_approve: autoApprove,
  });

/** The persisted relay set (`config.json`). Works in every session state. */
export const relayList = () => request<{ relays: string[] }>("relay_list");

/** Drop one relay from the persisted set by exact URL. */
export const relayForget = (url: string) =>
  request<{ forgot: string; relays: string[] }>("relay_forget", { url });

// ---------------------------------------------- Governance & channels
//
// Workspace/channel admin commands, plus keypackage rotation. All ride the
// generic `session_request` bridge (none carries a path or a secret), and
// each receipt mirrors the corresponding `ops::*Receipt::to_json()` shape
// one-for-one (crates/moyu-cli/src/ops.rs) — see the session dispatcher's
// `do_workspace_*`/`do_channel_*`/`do_admin_*`/`do_keypackage` handlers
// (crates/moyu-cli/src/session/mod.rs) for the exact arg names mirrored here.

export interface WorkspaceNewReceipt {
  group: string;
  name: string;
}

/** Found a brand-new workspace (its own MLS group, no initial invitees). The
 * name becomes the group's `profile.name`; the sidecar auto-seeds a
 * `#general` channel, so the sidebar's `conversations()` refresh right after
 * this always has somewhere to land the caller. */
export const workspaceNew = (name: string) =>
  request<WorkspaceNewReceipt>("workspace_new", { name });

export interface WorkspaceAddReceipt {
  group: string;
  workspace: string;
  npub: string;
  /** whether the catch-up channel-list snapshot broadcast succeeded — `false`
   * does NOT mean the invite failed (it always stands); the new member's
   * channel list just fills in on the next control event. */
  snapshot_sent: boolean;
}

/** Invite an existing contact's npub (or nip-05) straight into a workspace —
 * distinct from `channelInvite`, which targets one PRIVATE channel's own
 * member set. No admin gate server-side: any current member can pull
 * another peer in, same as the CLI's `workspace add`. */
export const workspaceAdd = (ws: string, peer: string) =>
  request<WorkspaceAddReceipt>("workspace_add", { ws, peer });

/** One row of `workspace_members`. */
export interface WorkspaceMemberRow {
  npub: string;
  admin: boolean;
  /** true for the local account's own row. */
  you: boolean;
}

export const workspaceMembers = (ws: string) =>
  request<{ members: WorkspaceMemberRow[] }>("workspace_members", { ws });

export interface WorkspaceRenameReceipt {
  group: string;
  name: string;
}

export const workspaceRename = (ws: string, name: string) =>
  request<WorkspaceRenameReceipt>("workspace_rename", { ws, name });

export interface WorkspaceKickReceipt {
  group: string;
  workspace: string;
  npub: string;
}

export const workspaceKick = (ws: string, peer: string) =>
  request<WorkspaceKickReceipt>("workspace_kick", { ws, peer });

export interface WorkspaceLeaveReceipt {
  group: string;
  workspace: string;
  /** set only on the sole-admin path, when `transferTo` was passed. */
  transferred_to: string | null;
  stepped_down: boolean;
}

/** Leave a workspace. `transferTo` is required only when the local account
 * is the workspace's sole admin (the sidecar reports that case as an
 * `engine` error naming the constraint; the caller surfaces it as-is). */
export const workspaceLeave = (ws: string, transferTo?: string) =>
  request<WorkspaceLeaveReceipt>("workspace_leave", {
    ws,
    transfer_to: transferTo,
  });

export interface AdminChangeReceipt {
  group: string;
  workspace: string;
  npub: string;
  action: "promote" | "demote";
}

export const adminAdd = (ws: string, peer: string) =>
  request<AdminChangeReceipt>("admin_add", { ws, peer });

export const adminRemove = (ws: string, peer: string) =>
  request<AdminChangeReceipt>("admin_remove", { ws, peer });

export interface ChannelNewReceipt {
  group: string;
  channel: string;
  name: string;
}

export const channelNew = (ws: string, name: string) =>
  request<ChannelNewReceipt>("channel_new", { ws, name });

export interface ChannelNewPrivateReceipt {
  /** the new private channel's own MLS group id. */
  group: string;
  /** the parent workspace's group id. */
  parent: string;
  channel: string;
  workspace: string;
  invited: string[];
}

/** Create a private channel (its own MLS group nested under the workspace).
 * Every entry in `invite` must already be a workspace member — the sidecar
 * enforces that and reports a plain error otherwise. */
export const channelNewPrivate = (ws: string, name: string, invite: string[]) =>
  request<ChannelNewPrivateReceipt>("channel_new_private", { ws, name, invite });

export interface ChannelInviteReceipt {
  group: string;
  channel: string;
  npub: string;
}

/** Add a member to an existing PRIVATE channel (gated on that channel's own
 * admin set server-side — a non-admin's attempt surfaces as an `engine`
 * error). */
export const channelInvite = (ws: string, channel: string, peer: string) =>
  request<ChannelInviteReceipt>("channel_invite", { ws, channel, peer });

export interface ChannelRenameReceipt {
  group: string;
  channel: string;
  name: string;
}

export const channelRename = (ws: string, channel: string, name: string) =>
  request<ChannelRenameReceipt>("channel_rename", { ws, channel, name });

export interface ChannelArchiveReceipt {
  group: string;
  channel: string;
}

/** Archival is monotonic — there is no un-archive command in M1. */
export const channelArchive = (ws: string, channel: string) =>
  request<ChannelArchiveReceipt>("channel_archive", { ws, channel });

export interface KeypackageReceipt {
  action: "publish" | "rotate";
  bytes: number;
  ref: string;
}

/** Rotate the account's published MLS KeyPackage. Existing sessions are
 * unaffected (PCS doesn't need this); future invites/adds use the fresh one. */
export const keypackageRotate = () =>
  request<KeypackageReceipt>("keypackage", { action: "rotate" });
