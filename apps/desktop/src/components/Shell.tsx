// The unlocked main shell: warm-paper sidebar (DMs + workspace channel
// trees), message pane with day dividers / markdown / bot cards / reaction
// pills, notice toasts, and a composer (DM send / channel post, text +
// attachments).
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  lock,
  onFileDrag,
  onFileDrop,
  pickAttachment,
  type MessageRow,
  type StagedFile,
} from "../lib/session";
import { errMsg, shortHex } from "../lib/format";
import { useAsync } from "../lib/hooks";
import { rowInConv, useStore, type Conversation } from "../store";
import Modals, { type ModalRequest } from "./Modals";
import {
  AttachmentChips,
  BotCard,
  MessageBody,
  ReactionPills,
  fmtDay,
  fmtTime,
  initials,
  isSelf,
  senderName,
} from "./Message";

function Sidebar({ onOpen }: { onOpen: (r: ModalRequest) => void }) {
  const convs = useStore((s) => s.convs);
  const activeKey = useStore((s) => s.activeKey);
  const select = useStore((s) => s.select);
  const identity = useStore((s) => s.identity);
  const requestCount = useStore((s) => s.joinRequests.length);
  // Client-side sidebar filter — matches DM/channel titles and workspace
  // names; no backend search command exists (or is needed) for this.
  const [filter, setFilter] = useState("");
  const q = filter.trim().toLowerCase();

  const allDms = convs.filter((c) => c.kind === "dm");
  const dms = q ? allDms.filter((c) => c.title.toLowerCase().includes(q)) : allDms;
  const wsGroups = new Map<string, { name: string; items: Conversation[] }>();
  for (const c of convs) {
    if (c.kind === "dm" || !c.wsGroup) continue;
    const g = wsGroups.get(c.wsGroup) ?? { name: c.wsName ?? "", items: [] };
    g.items.push(c);
    wsGroups.set(c.wsGroup, g);
  }
  // A workspace-name match keeps every channel in it; otherwise filter each
  // channel's own title and drop the group entirely if nothing matches.
  const visibleWsGroups = [...wsGroups.entries()]
    .map(([g, ws]): [string, { name: string; items: Conversation[] }] => {
      if (!q || ws.name.toLowerCase().includes(q)) return [g, ws];
      return [g, { ...ws, items: ws.items.filter((c) => c.title.toLowerCase().includes(q)) }];
    })
    .filter(([, ws]) => ws.items.length > 0);
  const noResults = q.length > 0 && dms.length === 0 && visibleWsGroups.length === 0;

  const item = (c: Conversation, label: string, avatar?: string) => (
    <a
      key={c.key}
      href="#"
      className={c.key === activeKey ? "on" : ""}
      onClick={(e) => {
        e.preventDefault();
        void select(c.key);
      }}
    >
      {avatar !== undefined ? <span className="av">{avatar}</span> : null}
      <span className="lbl">{label}</span>
      {c.unread > 0 && <span className="n">{c.unread}</span>}
    </a>
  );

  return (
    <nav className="side">
      <div className="sidetools">
        <button onClick={() => onOpen({ kind: "contact" })} title="添加联系人">
          ＋ 联系人
        </button>
        <button onClick={() => onOpen({ kind: "workspace_new" })} title="新建工作区">
          ＋ 工作区
        </button>
        <button onClick={() => onOpen({ kind: "join" })} title="用邀请码加入">
          加入
        </button>
        <button onClick={() => onOpen({ kind: "invite" })} title="生成邀请码">
          邀请
        </button>
        <button
          className="reqbtn"
          onClick={() => onOpen({ kind: "requests" })}
          title="加入请求"
        >
          请求
          {requestCount > 0 && <span className="tbadge">{requestCount}</span>}
        </button>
        <button onClick={() => onOpen({ kind: "relay" })} title="Relay 设置">
          Relay
        </button>
        <button
          onClick={() => onOpen({ kind: "network" })}
          title="网络 / 代理设置（SOCKS5、本地 relay）"
        >
          网络
        </button>
      </div>
      <div className="sidesearch">
        <input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="🔍 按名称过滤会话…"
        />
      </div>
      {dms.length > 0 && <div className="sec">私聊</div>}
      {dms.map((c) => item(c, c.title, initials(c.title)))}
      {visibleWsGroups.map(([g, ws]) => (
        <div key={g}>
          <div className="sec wsrow">
            <span>{ws.name.toUpperCase()}</span>
            <button
              className="wsgear"
              onClick={() => onOpen({ kind: "workspace", wsGid: g, wsName: ws.name })}
              title={`${ws.name} · 工作区设置`}
            >
              ⚙
            </button>
          </div>
          {ws.items.map((c) =>
            item(c, `${c.kind === "private" ? "🔒 " : "＃ "}${c.title}`),
          )}
        </div>
      ))}
      {noResults && <p className="mdim tiny sidenone">没有匹配的会话。</p>}
      <div className="me">
        {/* Just the npub here — the lock state ("已解锁") is already shown in
            the top statusbar, and the 236px column has no room to spare beside
            the 身份/锁定 buttons. Full npub is on the title tooltip. */}
        <span title={identity?.npub}>
          {identity?.npub ? `${shortHex(identity.npub, 10)}…` : ""}
        </span>
        <button
          className="lockbtn"
          onClick={() => onOpen({ kind: "identity" })}
          title="身份设置"
        >
          身份
        </button>
        <button
          className="lockbtn"
          onClick={() => {
            void lock();
          }}
          title="锁定会话"
        >
          锁定
        </button>
      </div>
    </nav>
  );
}

function Notices() {
  const notices = useStore((s) => s.notices);
  const dismiss = useStore((s) => s.dismissNotice);
  if (notices.length === 0) return null;
  return (
    <div className="notices">
      {notices.map((n) => (
        <div key={n.id} className={`notice ${n.kind}`}>
          <span>{n.text}</span>
          <button onClick={() => dismiss(n.id)} aria-label="dismiss">
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}

const QUICK_EMOJI = ["👍", "✅", "🚀", "😂"];

/** Splits `text` on case-insensitive occurrences of `query`, wrapping matches
 * in `<mark>` — plain text nodes only, built by slicing the raw string, never
 * markdown/HTML. This is the search-results preview list ONLY; it never
 * touches the live-rendered message body (Message.tsx's sanitize→highlight
 * pipeline), so there is no `dangerouslySetInnerHTML` path here to reopen
 * the XSS boundary that pipeline exists to close. */
function highlightMatches(text: string, query: string): ReactNode {
  if (!query) return text;
  const lower = text.toLowerCase();
  const needle = query.toLowerCase();
  const parts: ReactNode[] = [];
  let i = 0;
  let idx = lower.indexOf(needle, i);
  let key = 0;
  while (idx !== -1) {
    if (idx > i) parts.push(text.slice(i, idx));
    parts.push(<mark key={key++}>{text.slice(idx, idx + needle.length)}</mark>);
    i = idx + needle.length;
    idx = lower.indexOf(needle, i);
  }
  if (i < text.length) parts.push(text.slice(i));
  return parts;
}

/** A short plain-text window centered on the first match, so a long message
 * doesn't blow out the results list. */
function previewSnippet(text: string, query: string, radius = 50): string {
  const idx = text.toLowerCase().indexOf(query.toLowerCase());
  if (idx === -1 || text.length <= radius * 2) return text.slice(0, 160);
  const start = Math.max(0, idx - radius);
  const end = Math.min(text.length, idx + query.length + radius);
  return `${start > 0 ? "…" : ""}${text.slice(start, end)}${end < text.length ? "…" : ""}`;
}

/** One matched message: enough to render a results-list row and to scroll
 * the real pane to it. */
interface SearchMatch {
  id: string;
  who: string;
  text: string;
}

function Pane({
  conv,
  onReply,
  searchOpen,
  onCloseSearch,
}: {
  conv: Conversation;
  onReply: (t: ReplyTarget) => void;
  searchOpen: boolean;
  onCloseSearch: () => void;
}) {
  const rows = useStore((s) => s.rowsByGroup[conv.group]);
  const cursor = useStore((s) => s.cursorByGroup[conv.group]);
  const loading = useStore((s) => s.loading[conv.group]);
  const loadError = useStore((s) => s.errorByGroup[conv.group]);
  const loadOlder = useStore((s) => s.loadOlder);
  const select = useStore((s) => s.select);
  const identity = useStore((s) => s.identity);
  const toggleReaction = useStore((s) => s.toggleReaction);
  const retrySend = useStore((s) => s.retrySend);
  const retryAttachment = useStore((s) => s.retryAttachment);
  const endRef = useRef<HTMLDivElement>(null);

  // Rows for THIS conversation + reactions folded onto their targets.
  const { visible, reactions } = useMemo(() => {
    const inConv = (rows ?? []).filter((r) => rowInConv(r, conv));
    const reactions = new Map<string, Map<string, number>>();
    const visible: MessageRow[] = [];
    for (const r of inConv) {
      if (r.event === "reaction") {
        if (!r.target || !r.emoji) continue;
        const m = reactions.get(r.target) ?? new Map<string, number>();
        m.set(r.emoji, (m.get(r.emoji) ?? 0) + 1);
        reactions.set(r.target, m);
      } else {
        visible.push(r);
      }
    }
    return { visible, reactions };
  }, [rows, conv]);

  const containerRef = useRef<HTMLDivElement>(null);
  // Whether the user is (near) the bottom of the pane right now — only then
  // does a newly-arrived message auto-scroll; scrolled-up-reading-history
  // must never get yanked back down by a live event.
  const stickToBottomRef = useRef(true);
  // Set right before `loadOlder` prepends older rows; consumed by the
  // scroll-anchor effect below to cancel out the resulting jump.
  const prevScrollHeightRef = useRef<number | null>(null);
  const messageRefs = useRef(new Map<string, HTMLDivElement>());
  const [flashId, setFlashId] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [activeIdx, setActiveIdx] = useState(0);

  const lastId = visible[visible.length - 1]?.message_id;
  const firstId = visible[0]?.message_id;

  // Switching conversations always lands at the bottom, unconditionally —
  // matches opening a channel/DM fresh, regardless of where the previous
  // conversation had scrolled to.
  useEffect(() => {
    stickToBottomRef.current = true;
    setQuery("");
    setActiveIdx(0);
    const el = containerRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [conv.key]);

  // New content appended (own send, live event, or the initial history
  // load): scroll to it only if the user was already stuck to the bottom.
  useEffect(() => {
    if (!stickToBottomRef.current) return;
    endRef.current?.scrollIntoView({ block: "end" });
  }, [lastId]);

  // Older messages were just prepended by `loadOlder`: the browser leaves
  // `scrollTop` fixed in pixels, which visually shoves whatever the user was
  // reading further down (out of view, in the common case). Restore the same
  // visual anchor by adding back exactly the height the prepend inserted.
  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el || prevScrollHeightRef.current === null) return;
    el.scrollTop += el.scrollHeight - prevScrollHeightRef.current;
    prevScrollHeightRef.current = null;
  }, [firstId]);

  function handleScroll() {
    const el = containerRef.current;
    if (!el) return;
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }

  /** Shared by the pagination button and the search panel's "load more"
   * affordance — both prepend older rows into the SAME cache, so both need
   * the same anchor capture. */
  function requestOlder() {
    prevScrollHeightRef.current = containerRef.current?.scrollHeight ?? null;
    void loadOlder(conv.group);
  }

  // ---------------------------------------------------- in-conversation search
  // Client-side only, over already-loaded rows — no backend search command.
  const matches: SearchMatch[] = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return [];
    return visible
      .filter(
        (r) =>
          r.event === "message" || r.event === "agent_op" || r.event === "agent_activity",
      )
      .map((r) => ({
        id: r.message_id,
        who: senderName(r, isSelf(r, identity?.label)),
        text: r.text || r.body || "",
      }))
      .filter((m) => m.text.toLowerCase().includes(needle));
  }, [visible, query, identity]);

  useEffect(() => {
    setActiveIdx(0);
  }, [query]);

  function jumpTo(id: string) {
    messageRefs.current.get(id)?.scrollIntoView({ block: "center", behavior: "smooth" });
    setFlashId(id);
    window.setTimeout(() => setFlashId((cur) => (cur === id ? null : cur)), 1200);
  }

  // Live-jump to the current match as the query changes or the user steps
  // prev/next — the effect (not the event handlers) owns the actual scroll
  // so every path that changes `activeIdx` behaves identically.
  useEffect(() => {
    if (!searchOpen || matches.length === 0) return;
    const m = matches[Math.min(activeIdx, matches.length - 1)];
    if (m) jumpTo(m.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeIdx, matches, searchOpen]);

  function stepMatch(delta: 1 | -1) {
    if (matches.length === 0) return;
    setActiveIdx((i) => (i + delta + matches.length) % matches.length);
  }

  let lastDay = "";
  return (
    <div className="msgs" ref={containerRef} onScroll={handleScroll}>
      {searchOpen && (
        <div className="searchbar">
          <div className="searchrow">
            <input
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  onCloseSearch();
                  return;
                }
                if (e.key === "Enter") {
                  e.preventDefault();
                  stepMatch(e.shiftKey ? -1 : 1);
                }
              }}
              placeholder="搜索本会话已加载的消息…"
            />
            <span className="searchcount">
              {query ? (matches.length > 0 ? `${activeIdx + 1}/${matches.length}` : "无匹配") : ""}
            </span>
            <button
              type="button"
              className="ghost"
              onClick={() => stepMatch(-1)}
              disabled={matches.length === 0}
              title="上一条 (Shift+Enter)"
            >
              ↑
            </button>
            <button
              type="button"
              className="ghost"
              onClick={() => stepMatch(1)}
              disabled={matches.length === 0}
              title="下一条 (Enter)"
            >
              ↓
            </button>
            <button type="button" className="ghost" onClick={onCloseSearch} aria-label="关闭搜索">
              ✕
            </button>
          </div>
          {query && matches.length === 0 && cursor && (
            <button
              type="button"
              className="ghost searchmore"
              disabled={!!loading}
              onClick={requestOlder}
            >
              {loading ? "加载中…" : "已加载消息中未找到 — 加载更早继续搜"}
            </button>
          )}
          {matches.length > 0 && (
            <div className="searchresults">
              {matches.map((m, i) => (
                <button
                  type="button"
                  key={m.id}
                  className={i === activeIdx ? "on" : ""}
                  onClick={() => setActiveIdx(i)}
                >
                  <b>{m.who}</b>
                  {/* match detection trims the query — snippet/highlight must
                      search the same needle or a padded query highlights nothing */}
                  <span>
                    {highlightMatches(previewSnippet(m.text, query.trim()), query.trim())}
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      {cursor && (
        <button className="older" disabled={!!loading} onClick={requestOlder}>
          {loading ? "加载中…" : "加载更早的消息"}
        </button>
      )}
      {loadError && (
        <p className="empty">
          历史加载失败：{loadError}{" "}
          <button className="older" onClick={() => void select(conv.key)}>
            重试
          </button>
        </p>
      )}
      {rows !== undefined && visible.length === 0 && (
        <p className="empty">还没有消息 — 说点什么吧。</p>
      )}
      {visible.map((r) => {
        const day = fmtDay(r.ts);
        const divider = day !== lastDay;
        lastDay = day;
        const self = isSelf(r, identity?.label);
        const who = senderName(r, self);
        return (
          <div
            key={r.message_id}
            ref={(el) => {
              if (el) messageRefs.current.set(r.message_id, el);
              else messageRefs.current.delete(r.message_id);
            }}
            data-searchhit={flashId === r.message_id || undefined}
          >
            {divider && <div className="day">{day}</div>}
            {r.event === "agent_op" || r.event === "agent_activity" ? (
              <BotCard row={r} />
            ) : (
              <div className="m" data-pending={r.pending || undefined}>
                <span
                  className="av"
                  data-self={self || undefined}
                  title={r.sender}
                >
                  {initials(who, self)}
                </span>
                <div className="mbody">
                  <div className="hd">
                    <b>{who}</b>
                    <span>{fmtTime(r.ts)}</span>
                    {r.reply_to && (
                      <span className="rref">↩ {shortHex(r.reply_to)}…</span>
                    )}
                    {r.pending && <span className="sending">发送中…</span>}
                    {!r.pending && !r.failed && (
                      <span className="acts">
                        {QUICK_EMOJI.map((e) => (
                          <button
                            key={e}
                            onClick={() => void toggleReaction(r.message_id, e)}
                            title={`${e} 回应`}
                          >
                            {e}
                          </button>
                        ))}
                        <button
                          onClick={() =>
                            onReply({
                              id: r.message_id,
                              preview: `${who}: ${(r.body ?? "").slice(0, 40)}`,
                            })
                          }
                        >
                          ↩ 回复
                        </button>
                      </span>
                    )}
                  </div>
                  <MessageBody body={r.body ?? ""} />
                  {r.failed && (
                    <p className="sendfail">
                      发送失败
                      {r.attachments?.length ? (
                        // The staged token was already burned by the failed
                        // call (store.ts's sendAttachmentMsg) — there is no
                        // path to retry with, only to re-pick a file.
                        <button onClick={() => void retryAttachment(r.message_id)}>
                          重新选择文件
                        </button>
                      ) : (
                        <button onClick={() => void retrySend(r.message_id)}>
                          重试
                        </button>
                      )}
                    </p>
                  )}
                  <AttachmentChips row={r} />
                  <ReactionPills
                    pills={reactions.get(r.message_id) ?? new Map()}
                    onToggle={(e) => void toggleReaction(r.message_id, e)}
                  />
                </div>
              </div>
            )}
          </div>
        );
      })}
      <div ref={endRef} />
    </div>
  );
}

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function Composer({
  conv,
  replyTo,
  onCancelReply,
}: {
  conv: Conversation;
  replyTo: ReplyTarget | null;
  onCancelReply: () => void;
}) {
  const sendText = useStore((s) => s.sendText);
  const sendAttachmentMsg = useStore((s) => s.sendAttachmentMsg);
  const [text, setText] = useState("");
  const { busy, err, setErr, run } = useAsync();
  // Staging is LOCAL component state, not store state: the `key={conv.key}`
  // remount on conv switch (Shell.tsx) wipes it, same as the draft-leak
  // fix for `text` — a file staged for A must never follow a switch to B.
  const [staged, setStaged] = useState<StagedFile | null>(null);
  const [dragHover, setDragHover] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);

  // Auto-grow the textarea to fit its content on every keystroke AND on the
  // programmatic clear after a send — `max-height`/`overflow-y:auto` in
  // theme.css cap it and switch to an internal scrollbar past that point, so
  // this never needs to compute the cap itself.
  useEffect(() => {
    const el = taRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, [text]);

  // Read via refs inside the event listeners below so the subscription
  // effect can run exactly once per mount (no resubscribe churn / missed-
  // event window on every keystroke) while still seeing current
  // replyTo/busy at drop time — mirrors App.tsx's keep-the-promise pattern.
  const replyToRef = useRef(replyTo);
  const busyRef = useRef(busy);
  useEffect(() => {
    replyToRef.current = replyTo;
    // No reply-with-file in the protocol: a file staged BEFORE the reply
    // started would otherwise win the submit branch and silently ship as a
    // plain attachment, discarding the reply context (merge review, Low).
    // Entering reply mode visibly drops the staging instead.
    if (replyTo) setStaged(null);
  }, [replyTo]);
  useEffect(() => {
    busyRef.current = busy;
  }, [busy]);

  useEffect(() => {
    const dropP = onFileDrop((files) => {
      // No reply-with-file in the protocol, and a send already in flight
      // must not have its staged file swapped out from under it.
      if (replyToRef.current || busyRef.current || files.length === 0) return;
      setStaged(files[0]); // first file only if multiple were dropped
      setDragHover(false);
    });
    const dragP = onFileDrag(({ hovering }) => {
      if (replyToRef.current || busyRef.current) {
        setDragHover(false);
        return;
      }
      setDragHover(hovering);
    });
    return () => {
      void dropP.then((u) => u());
      void dragP.then((u) => u());
    };
  }, []);

  async function handlePick() {
    try {
      const picked = await pickAttachment();
      if (picked) setStaged(picked);
    } catch (error) {
      // Shares the busy/err pair with `submit` below but never toggles
      // `busy` itself — the file dialog isn't a send, so it must not disable
      // the composer while it's open.
      setErr(errMsg(error));
    }
  }

  async function submit(e: { preventDefault(): void }) {
    e.preventDefault();
    const body = text.trim();
    if (busy || (!body && !staged)) return;
    // The staged token is burned server-side the instant sendAttachmentMsg
    // calls in (success or failure — see attachments.rs) — never re-offer
    // it, so clear local staging unconditionally before awaiting.
    const s = staged;
    setStaged(null);
    await run(async () => {
      // `s && replyTo` is unreachable (entering reply mode clears staging,
      // and drops/pick are hidden while replying) — the !replyTo guard is
      // belt-and-suspenders so a reply can never silently lose its target.
      if (s && !replyTo) {
        await sendAttachmentMsg(s, body);
      } else {
        await sendText(body, replyTo?.id);
      }
      setText("");
      onCancelReply();
    });
  }

  const target = conv.kind === "dm" ? conv.title : `#${conv.title}`;
  return (
    <form className="compose" onSubmit={submit}>
      {!replyTo && dragHover && <div className="dropzone">拖放以添加附件</div>}
      {replyTo && (
        <div className="rb">
          <span>↩ 回复 {replyTo.preview}…</span>
          <button type="button" onClick={onCancelReply} aria-label="取消回复">
            ✕
          </button>
        </div>
      )}
      {err && <div className="cerr">{err}</div>}
      {staged && (
        <div className="stagedbar">
          <span>
            📎 {staged.file_name} · {fmtSize(staged.size)}
          </span>
          <button type="button" onClick={() => setStaged(null)} aria-label="移除附件">
            ✕
          </button>
        </div>
      )}
      <div className="in">
        {/* protocol has no reply-with-file — hide while replying */}
        {!replyTo && (
          <button
            type="button"
            className="attachbtn"
            onClick={() => void handlePick()}
            disabled={busy}
            title="添加附件"
          >
            📎
          </button>
        )}
        <textarea
          ref={taRef}
          rows={1}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            // Shift+Enter (or any IME composition Enter) inserts a newline —
            // only a bare Enter submits. `isComposing` covers the CJK-input
            // case where Enter confirms the candidate, not the message.
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              void submit(e);
            }
          }}
          placeholder={`给 ${target} 发消息 · 支持 markdown · Enter 发送 / Shift+Enter 换行`}
          disabled={busy}
        />
        <button type="submit" disabled={busy || (!text.trim() && !staged)}>
          {busy ? "…" : "发送 ➤"}
        </button>
      </div>
    </form>
  );
}

export interface ReplyTarget {
  id: string;
  preview: string;
}

export default function Shell() {
  const convs = useStore((s) => s.convs);
  const activeKey = useStore((s) => s.activeKey);
  const hydrated = useStore((s) => s.hydrated);
  const conv = convs.find((c) => c.key === activeKey);
  const [replyTo, setReplyTo] = useState<ReplyTarget | null>(null);
  const [modal, setModal] = useState<ModalRequest | null>(null);
  const [searchOpen, setSearchOpen] = useState(false);
  useEffect(() => setReplyTo(null), [activeKey]);
  useEffect(() => setSearchOpen(false), [activeKey]);

  // Cmd+F / Ctrl+F opens the in-conversation search bar instead of the
  // browser's native find (there is no page to find-in-page across — the
  // webview has no `shell:*`/find-bar capability anyway); Escape closing it
  // is handled inside Pane's own search input.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (!conv) return;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
        e.preventDefault();
        setSearchOpen(true);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [conv]);

  // Stable-typed handle for the chat-header "workspace settings" entry —
  // only shown for workspace/private-channel conversations, never DMs.
  const wsGid = conv && conv.kind !== "dm" ? conv.wsGroup : undefined;
  const wsNameForModal = conv?.wsName ?? wsGid;

  return (
    <div className="app">
      <Sidebar onOpen={setModal} />
      <div className="main">
        {conv ? (
          <>
            <div className="chead">
              <b>
                {conv.kind === "dm"
                  ? conv.title
                  : `${conv.kind === "private" ? "🔒 " : "#"}${conv.title}`}
              </b>
              <span>
                {conv.kind === "dm" ? "私聊" : conv.wsName}
              </span>
              {wsGid && (
                <button
                  className="gear"
                  onClick={() =>
                    setModal({ kind: "workspace", wsGid, wsName: wsNameForModal ?? wsGid })
                  }
                  title="工作区设置"
                >
                  ⚙ 管理
                </button>
              )}
              <button
                className="gear"
                onClick={() => setSearchOpen((v) => !v)}
                title="搜索本会话消息 (⌘F)"
              >
                🔍 搜索
              </button>
              <span className="e2e">🔐 端到端加密</span>
            </div>
            <Pane
              conv={conv}
              onReply={setReplyTo}
              searchOpen={searchOpen}
              onCloseSearch={() => setSearchOpen(false)}
            />
            {/* key resets the composer's draft/busy/err state per conversation
                — without it React reuses one instance and a half-typed message
                to A can be sent into B after a switch (wrong-recipient send). */}
            <Composer
              key={conv.key}
              conv={conv}
              replyTo={replyTo}
              onCancelReply={() => setReplyTo(null)}
            />
          </>
        ) : (
          <div className="blank">
            {hydrated
              ? "还没有会话 — 用 moyu add / moyu join 添加联系人或加入 workspace。"
              : "载入会话中…"}
          </div>
        )}
      </div>
      <Notices />
      {modal && <Modals request={modal} onClose={() => setModal(null)} />}
    </div>
  );
}
