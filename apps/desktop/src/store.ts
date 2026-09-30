// App state: one zustand store, populated by hydrate() after unlock and by
// the single moyu://event listener (wired in App.tsx). Conversations are a
// UI concept — a DM group, a public channel (workspace group + slug filter),
// or a private channel (its own group) — while rows are cached per MLS
// group, exactly how the protocol serves them.
import { create } from "zustand";
import {
  addContact,
  approve,
  conversations,
  deny,
  downloadAttachment,
  history,
  listRequests,
  pickAttachment,
  post,
  react,
  reply,
  sendAttachment,
  sendDm,
  type AddReceipt,
  type ApproveResult,
  type Conversations,
  type MessageRow,
  type RequestRow,
  type SessionEvent,
  type SessionState,
  type StagedFile,
} from "./lib/session";
import { errMsg, shortNpub } from "./lib/format";

export interface Conversation {
  key: string;
  kind: "dm" | "channel" | "private";
  /** sidebar text: contact label / npub short for DMs, slug for channels */
  title: string;
  /** the MLS group whose history backs this conversation */
  group: string;
  /** slug filter for PUBLIC channels (a workspace group is multi-channel) */
  channel?: string;
  wsGroup?: string;
  wsName?: string;
  peerNpub?: string;
  unread: number;
}

export interface Notice {
  id: number;
  kind: "governance" | "sync_error" | "approve" | "join_request" | "info";
  text: string;
}

/** One `ciphertext_sha256`'s local-fetch lifecycle. */
export interface DownloadStatus {
  status: "downloading" | "done" | "failed";
  path?: string;
}

interface MoyuStore {
  sessionState: SessionState;
  identity: { label?: string; npub?: string } | null;
  convs: Conversation[];
  activeKey: string | null;
  rowsByGroup: Record<string, MessageRow[]>;
  cursorByGroup: Record<string, string | null>;
  loading: Record<string, boolean>;
  errorByGroup: Record<string, string | undefined>;
  notices: Notice[];
  hydrated: boolean;
  /** keyed by `ciphertext_sha256` — global, not per-conversation, since the
   * same hash can appear in more than one row (e.g. history + a live echo). */
  downloadsByHash: Record<string, DownloadStatus>;
  /** pending workspace join requests awaiting this account's approval;
   * refreshed on unlock and on every live `join_request`/`approve` event so a
   * sidebar badge stays live. */
  joinRequests: RequestRow[];

  setSessionState: (s: SessionState) => void;
  setIdentity: (i: { label?: string; npub?: string } | null) => void;
  hydrate: () => Promise<void>;
  select: (key: string) => Promise<void>;
  loadOlder: (group: string) => Promise<void>;
  applyEvent: (ev: SessionEvent) => void;
  sendText: (text: string, replyTo?: string) => Promise<void>;
  retrySend: (tempId: string) => Promise<void>;
  sendAttachmentMsg: (staged: StagedFile, caption: string) => Promise<void>;
  retryAttachment: (tempId: string) => Promise<void>;
  fetchAttachment: (group: string, hash: string) => Promise<void>;
  toggleReaction: (targetId: string, emoji: string) => Promise<void>;
  dismissNotice: (id: number) => void;
  loadJoinRequests: () => Promise<void>;
  approveJoin: (npub: string) => Promise<ApproveResult>;
  denyJoin: (npub: string) => Promise<void>;
  addNewContact: (peer: string, label?: string) => Promise<AddReceipt>;
}

export function buildConvs(view: Conversations): Conversation[] {
  const dms: Conversation[] = view.dms.map((d) => ({
    key: `dm:${d.group}`,
    kind: "dm",
    title: d.label ?? shortNpub(d.npub),
    group: d.group,
    peerNpub: d.npub,
    unread: 0,
  }));
  const channels: Conversation[] = view.workspaces.flatMap((w) =>
    w.channels
      .filter((c) => !c.archived)
      .map((c): Conversation => {
        if (c.private && c.group) {
          return {
            key: `pc:${c.group}`,
            kind: "private",
            title: c.slug,
            group: c.group,
            wsGroup: w.group,
            wsName: w.name,
            unread: 0,
          };
        }
        return {
          key: `ch:${w.group}:${c.slug}`,
          kind: "channel",
          title: c.slug,
          group: w.group,
          channel: c.slug,
          wsGroup: w.group,
          wsName: w.name,
          unread: 0,
        };
      }),
  );
  return [...dms, ...channels];
}

/** Does a message-family row belong to this conversation? */
export function rowInConv(row: MessageRow, conv: Conversation): boolean {
  if (row.group !== conv.group) return false;
  if (conv.kind === "channel" && row.event !== "reaction") {
    return (row.channel ?? "general") === conv.channel;
  }
  return true;
}

export function dedupeAppend(rows: MessageRow[], row: MessageRow): MessageRow[] {
  if (rows.some((r) => r.message_id === row.message_id)) return rows;
  return [...rows, row];
}

/** Mint a locally-unique optimistic-row id: a `prefix-<ms>-<rand>` token, not
 * a real message_id — reconciliation (or the remove-on-failure path) always
 * matches on this exact string, so two mints in the same millisecond must
 * not collide within one prefix's namespace. */
function genTempId(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
}

/** Patch one row (by `message_id`) in a group's row cache — the
 * reconciliation step shared by `sendText` and `sendAttachmentMsg`: success
 * merges the real receipt fields in, failure just flips pending/failed. */
function patchRow(
  set: (fn: (s: MoyuStore) => Partial<MoyuStore>) => void,
  group: string,
  id: string,
  patch: Partial<MessageRow>,
): void {
  set((s) => ({
    rowsByGroup: {
      ...s.rowsByGroup,
      [group]: (s.rowsByGroup[group] ?? []).map((r) =>
        r.message_id === id ? { ...r, ...patch } : r,
      ),
    },
  }));
}

/** Shared by `retrySend`/`retryAttachment`: find the failed optimistic row by
 * tempId and drop it from the cache (the retry re-appends a fresh optimistic
 * row via `sendText`/`sendAttachmentMsg`), returning the row that was removed
 * so the caller can re-issue the send with its `body`/`reply_to`. `undefined`
 * if the group or the failed row no longer exists. */
function popFailedRow(
  get: () => MoyuStore,
  set: (fn: (s: MoyuStore) => Partial<MoyuStore>) => void,
  group: string,
  tempId: string,
): MessageRow | undefined {
  const row = (get().rowsByGroup[group] ?? []).find(
    (r) => r.message_id === tempId && r.failed,
  );
  if (!row) return undefined;
  set((s) => ({
    rowsByGroup: {
      ...s.rowsByGroup,
      [group]: (s.rowsByGroup[group] ?? []).filter((r) => r.message_id !== tempId),
    },
  }));
  return row;
}

let noticeSeq = 1;
const MAX_NOTICES = 6;

// One reaction toggle in flight per (group, target, emoji): a rapid
// double-click reads `mine` twice before either optimistic row lands, which
// would publish two `add`s and strand a duplicate local pill (merge review).
const reactInFlight = new Set<string>();

/** Push a notice, uniformly bounded to the most-recent MAX_NOTICES and
 * coalescing a repeated sync_error (a dropped relay socket emits one every 2s
 * pump tick, and a remote peer can spam join_request DMs — both would grow the
 * toast column without bound otherwise). */
function pushNotice(
  set: (fn: (s: MoyuStore) => Partial<MoyuStore>) => void,
  kind: Notice["kind"],
  text: string,
): void {
  set((s) => {
    let notices = s.notices;
    // Collapse a run of identical sync_errors into the existing one.
    if (
      kind === "sync_error" &&
      notices.length > 0 &&
      notices[notices.length - 1].kind === "sync_error"
    ) {
      notices = notices.slice(0, -1);
    }
    return {
      notices: [...notices, { id: noticeSeq++, kind, text }].slice(-MAX_NOTICES),
    };
  });
}

const MESSAGE_EVENTS = new Set([
  "message",
  "reaction",
  "agent_op",
  "agent_activity",
]);

export const useStore = create<MoyuStore>((set, get) => ({
  sessionState: "starting",
  identity: null,
  convs: [],
  activeKey: null,
  rowsByGroup: {},
  cursorByGroup: {},
  loading: {},
  errorByGroup: {},
  notices: [],
  hydrated: false,
  downloadsByHash: {},
  joinRequests: [],

  setSessionState: (s) =>
    set(() => ({
      sessionState: s,
      // A dead/relocked session invalidates the unlocked view.
      ...(s !== "unlocked"
        ? {
            identity: null,
            hydrated: false,
            convs: [],
            activeKey: null,
            joinRequests: [],
          }
        : {}),
    })),

  setIdentity: (identity) => set({ identity }),

  hydrate: async () => {
    // Re-hydration (e.g. a `joined` event) must not reset unread badges:
    // carry existing counts over by key; only genuinely-new convs start at 0.
    const prev = new Map(get().convs.map((c) => [c.key, c.unread]));
    const view = await conversations();
    const convs = buildConvs(view).map((c) => ({
      ...c,
      unread: prev.get(c.key) ?? 0,
    }));
    set({ convs, hydrated: true });
    void get().loadJoinRequests();
    const { activeKey, select } = get();
    if (!activeKey && convs.length > 0) await select(convs[0].key);
  },

  select: async (key) => {
    const conv = get().convs.find((c) => c.key === key);
    if (!conv) return;
    set((st) => ({
      activeKey: key,
      convs: st.convs.map((c) => (c.key === key ? { ...c, unread: 0 } : c)),
    }));
    if (get().rowsByGroup[conv.group] === undefined) {
      set((st) => ({
        loading: { ...st.loading, [conv.group]: true },
        errorByGroup: { ...st.errorByGroup, [conv.group]: undefined },
      }));
      try {
        const page = await history(conv.group);
        set((st) => ({
          rowsByGroup: { ...st.rowsByGroup, [conv.group]: page.messages },
          cursorByGroup: { ...st.cursorByGroup, [conv.group]: page.next_cursor },
        }));
      } catch (e) {
        // A failed load must surface in the pane, never a silent blank.
        set((st) => ({
          errorByGroup: {
            ...st.errorByGroup,
            [conv.group]: errMsg(e),
          },
        }));
      } finally {
        set((st) => ({ loading: { ...st.loading, [conv.group]: false } }));
      }
    }
  },

  loadOlder: async (group) => {
    const cursor = get().cursorByGroup[group];
    if (!cursor || get().loading[group]) return;
    set((st) => ({ loading: { ...st.loading, [group]: true } }));
    try {
      const page = await history(group, cursor);
      set((st) => ({
        rowsByGroup: {
          ...st.rowsByGroup,
          [group]: [...page.messages, ...(st.rowsByGroup[group] ?? [])],
        },
        cursorByGroup: { ...st.cursorByGroup, [group]: page.next_cursor },
      }));
    } catch (e) {
      set((st) => ({
        errorByGroup: {
          ...st.errorByGroup,
          [group]: errMsg(e),
        },
      }));
    } finally {
      set((st) => ({ loading: { ...st.loading, [group]: false } }));
    }
  },

  applyEvent: (ev) => {
    const st = get();
    if (MESSAGE_EVENTS.has(ev.event)) {
      const row = ev as unknown as MessageRow;
      if (!row.group || !row.message_id) return;
      // Only append into already-loaded caches; an unloaded group hydrates
      // fresh from history on first open (which includes this row).
      if (st.rowsByGroup[row.group] !== undefined) {
        set((s) => ({
          rowsByGroup: {
            ...s.rowsByGroup,
            [row.group]: dedupeAppend(s.rowsByGroup[row.group], row),
          },
        }));
      }
      if (row.event === "message" || row.event === "agent_op" || row.event === "agent_activity") {
        const active = st.convs.find((c) => c.key === st.activeKey);
        const next = st.convs.map((c) =>
          rowInConv(row, c) && (!active || c.key !== active.key)
            ? { ...c, unread: c.unread + 1 }
            : c,
        );
        // Only every row event reaches here (message/agent_op/agent_activity
        // events fire far more often than the active pane changes), so most
        // calls touch no conv's unread count at all — skip the `convs`
        // reference churn (and the full-sidebar re-render it triggers) when
        // nothing actually changed.
        if (next.some((c, i) => c !== st.convs[i])) set({ convs: next });
      }
      return;
    }
    switch (ev.event) {
      case "joined":
        // New group (invite accepted mid-session): refresh the sidebar.
        void get().hydrate();
        break;
      case "governance":
        pushNotice(set, "governance", String(ev.text ?? ""));
        break;
      case "join_request":
        pushNotice(
          set,
          "join_request",
          `🔑 ${String(ev.npub ?? "")} 请求加入 #${String(ev.ws_name ?? "")}${ev.trusted ? "（✓ 持码可信）" : "（⚠ 无凭证）"}`,
        );
        // Refresh the authoritative pending list so the requests panel + its
        // sidebar badge reflect this arrival (the event alone is a toast).
        void get().loadJoinRequests();
        break;
      case "approve":
        pushNotice(
          set,
          "approve",
          `✅ 已批准 ${String(ev.npub ?? "")} 加入 #${String(ev.ws_name ?? "")}`,
        );
        void get().loadJoinRequests();
        break;
      case "sync_error":
        pushNotice(set, "sync_error", `同步失败：${String(ev.error ?? "")}`);
        break;
      default:
        break;
    }
  },

  sendText: async (text, replyTo) => {
    const st = get();
    const conv = st.convs.find((c) => c.key === st.activeKey);
    if (!conv || !text.trim()) return;
    // Optimistic: the row appears immediately as pending, then reconciles
    // with the receipt's real message_id (own sends never echo via sync()).
    const tempId = genTempId("tmp");
    const row: MessageRow = {
      event: "message",
      group: conv.group,
      channel: conv.kind === "dm" ? "general" : conv.title,
      sender: st.identity?.label ?? "",
      sender_name: null,
      ts: Math.floor(Date.now() / 1000),
      message_id: tempId,
      body: text,
      direction: "sent",
      reply_to: replyTo,
      pending: true,
    };
    set((s) => ({
      rowsByGroup: {
        ...s.rowsByGroup,
        [conv.group]: [...(s.rowsByGroup[conv.group] ?? []), row],
      },
    }));
    const patch = (updates: Partial<MessageRow>) => patchRow(set, conv.group, tempId, updates);
    try {
      let messageId: string | undefined;
      if (replyTo) {
        const r = await reply(conv.group, replyTo, text);
        messageId = r.message_ids[0];
      } else if (conv.kind === "dm") {
        if (!conv.peerNpub) throw new Error("此 DM 缺少对端 npub");
        const r = await sendDm(conv.peerNpub, text);
        messageId = r.message_ids[0];
      } else {
        const r = await post(conv.wsGroup ?? conv.group, conv.title, text);
        messageId = r.message_ids[0];
      }
      patch({ message_id: messageId ?? tempId, pending: false });
    } catch (e) {
      patch({ pending: false, failed: true });
      throw e;
    }
  },

  retrySend: async (tempId) => {
    const conv = get().convs.find((c) => c.key === get().activeKey);
    if (!conv) return;
    const row = popFailedRow(get, set, conv.group, tempId);
    if (!row) return;
    await get().sendText(row.body ?? "", row.reply_to);
  },

  sendAttachmentMsg: async (staged, caption) => {
    const st = get();
    const conv = st.convs.find((c) => c.key === st.activeKey);
    if (!conv) return;
    // Same optimistic-row discipline as sendText: appears immediately as
    // pending, reconciles with the receipt's real message_id + attachment
    // refs. The placeholder `ciphertext_sha256` (`tmp-<token>`) marks the
    // row as not-yet-downloadable — AttachmentChips renders it inert until
    // reconciliation replaces it with the real hash.
    const tempId = genTempId("tmp");
    const row: MessageRow = {
      event: "message",
      group: conv.group,
      channel: conv.kind === "dm" ? "general" : conv.title,
      sender: st.identity?.label ?? "",
      sender_name: null,
      ts: Math.floor(Date.now() / 1000),
      message_id: tempId,
      body: caption,
      direction: "sent",
      attachments: [
        {
          file_name: staged.file_name,
          media_type: "",
          ciphertext_sha256: `tmp-${staged.token}`,
        },
      ],
      pending: true,
    };
    set((s) => ({
      rowsByGroup: {
        ...s.rowsByGroup,
        [conv.group]: [...(s.rowsByGroup[conv.group] ?? []), row],
      },
    }));
    const patch = (updates: Partial<MessageRow>) => patchRow(set, conv.group, tempId, updates);
    try {
      const target =
        conv.kind === "dm"
          ? (() => {
              if (!conv.peerNpub) throw new Error("此 DM 缺少对端 npub");
              return { peer: conv.peerNpub };
            })()
          : { ws: conv.wsGroup ?? conv.group, channel: conv.title };
      const r = await sendAttachment(target, staged.token, caption || undefined);
      patch({
        message_id: r.message_ids[0] ?? tempId,
        pending: false,
        attachments: r.attachments ?? row.attachments,
      });
    } catch (e) {
      patch({ pending: false, failed: true });
      throw e;
    }
  },

  /** The staged token is burned the instant `sendAttachmentMsg` calls into
   * Rust (success or failure — see attachments.rs's `take_staged`), so a
   * failed attachment send can never be retried with the same token. The
   * only recovery is to re-pick the file and re-send with the SAME caption
   * the failed row carried. */
  retryAttachment: async (tempId) => {
    const conv = get().convs.find((c) => c.key === get().activeKey);
    if (!conv) return;
    const row = (get().rowsByGroup[conv.group] ?? []).find(
      (r) => r.message_id === tempId && r.failed,
    );
    if (!row) return;
    const staged = await pickAttachment();
    if (!staged) return; // cancelled — leave the failed row for another try
    popFailedRow(get, set, conv.group, tempId);
    await get().sendAttachmentMsg(staged, row.body ?? "");
  },

  fetchAttachment: async (group, hash) => {
    const cur = get().downloadsByHash[hash];
    // Idempotent: a click while already downloading/done is a no-op; a
    // click on a `failed` entry is the retry affordance, so it re-runs.
    if (cur?.status === "downloading" || cur?.status === "done") return;
    set((s) => ({
      downloadsByHash: { ...s.downloadsByHash, [hash]: { status: "downloading" } },
    }));
    try {
      const r = await downloadAttachment(group, hash);
      set((s) => ({
        downloadsByHash: {
          ...s.downloadsByHash,
          [hash]: { status: "done", path: r.path },
        },
      }));
    } catch {
      set((s) => ({
        downloadsByHash: { ...s.downloadsByHash, [hash]: { status: "failed" } },
      }));
    }
  },

  toggleReaction: async (targetId, emoji) => {
    const st = get();
    const conv = st.convs.find((c) => c.key === st.activeKey);
    if (!conv) return;
    const flightKey = `${conv.group}:${targetId}:${emoji}`;
    if (reactInFlight.has(flightKey)) return;
    reactInFlight.add(flightKey);
    try {
      const selfLabel = st.identity?.label ?? "";
      const mine = (st.rowsByGroup[conv.group] ?? []).find(
        (r) =>
          r.event === "reaction" &&
          r.target === targetId &&
          r.emoji === emoji &&
          (r.direction === "sent" || r.sender === selfLabel),
      );
      if (mine) {
        await react(conv.group, targetId, emoji, true);
        set((s) => ({
          rowsByGroup: {
            ...s.rowsByGroup,
            [conv.group]: (s.rowsByGroup[conv.group] ?? []).filter(
              (r) => r.message_id !== mine.message_id,
            ),
          },
        }));
      } else {
        const r = await react(conv.group, targetId, emoji, false);
        const row: MessageRow = {
          event: "reaction",
          group: conv.group,
          sender: selfLabel,
          sender_name: null,
          ts: Math.floor(Date.now() / 1000),
          // Same uniqueness discipline as sendText's tempId: two reactions
          // minted in the same millisecond must not share a message_id, or
          // the remove-path filter above deletes both.
          message_id: genTempId("tmpr"),
          emoji,
          target: r.target ?? targetId,
          direction: "sent",
        };
        set((s) => ({
          rowsByGroup: {
            ...s.rowsByGroup,
            [conv.group]: [...(s.rowsByGroup[conv.group] ?? []), row],
          },
        }));
      }
    } finally {
      reactInFlight.delete(flightKey);
    }
  },

  dismissNotice: (id) =>
    set((s) => ({ notices: s.notices.filter((n) => n.id !== id) })),

  loadJoinRequests: async () => {
    try {
      const { requests } = await listRequests();
      set({ joinRequests: requests });
    } catch {
      // A transient scan failure must not blank a good list or crash the
      // pump-driven refresh; the next event/hydrate retries.
    }
  },

  approveJoin: async (npub) => {
    const res = await approve(npub);
    // The approve may have partially failed; reload the authoritative pending
    // list either way (approved requesters drop off once they're members).
    await get().loadJoinRequests();
    return res;
  },

  denyJoin: async (npub) => {
    // v1 deny is a local dismissal (nothing on the wire). Drop it from the
    // list optimistically; a later scan would resurface it if still pending,
    // which is the honest v1 behavior.
    await deny(npub);
    set((s) => ({
      joinRequests: s.joinRequests.filter((r) => r.npub !== npub),
    }));
  },

  addNewContact: async (peer, label) => {
    const receipt = await addContact(peer, label);
    // A new contact becomes a DM conversation — refresh the sidebar.
    await get().hydrate();
    return receipt;
  },
}));
