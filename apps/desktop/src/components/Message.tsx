// Row renderers for the message pane. One component per row family:
// chat (markdown), bot events (banded card), plus the attachment chip and
// aggregated reaction pills.
import { useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import { revealAttachment, type AttachmentRef, type MessageRow } from "../lib/session";
import { shortHex } from "../lib/format";
import { useStore } from "../store";

// Sanitize FIRST (message bodies are untrusted E2E content — the XSS red
// line), then highlight adds its own spans to the now-clean tree. The
// schema only additionally lets `language-*` classes through so the
// highlighter knows the fence's language.
const schema = {
  ...defaultSchema,
  attributes: {
    ...defaultSchema.attributes,
    code: [
      ...(defaultSchema.attributes?.code ?? []),
      ["className", /^language-./] as [string, RegExp],
    ],
  },
};
const rehypePlugins = [[rehypeSanitize, schema], rehypeHighlight] as never[];

/** The reserved label for the local user — also our authenticated-self marker,
 * so a peer's self-declared display name must never be allowed to equal it. */
const SELF_LABEL = "你";

/** Authenticated self-check: `direction` is a local fact and `sender` is
 * MLS-authenticated, so this decision NEVER touches the peer-controlled
 * `sender_display_name`. All self-styling keys off this boolean, closing the
 * "peer sets display name to 你 and renders as the local user" impersonation. */
export function isSelf(row: MessageRow, selfLabel?: string): boolean {
  return row.direction === "sent" || row.sender === selfLabel;
}

export function senderName(row: MessageRow, self: boolean): string {
  if (self) return SELF_LABEL;
  // `sender_name` is peer-controlled; never let it wear the reserved self label.
  const claimed =
    row.sender_name && row.sender_name !== SELF_LABEL ? row.sender_name : null;
  return claimed ?? shortHex(row.sender);
}

export function initials(name: string, self = false): string {
  return self ? SELF_LABEL : name.slice(0, 1).toUpperCase();
}

export function MessageBody({ body }: { body: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={rehypePlugins}>
        {body}
      </ReactMarkdown>
    </div>
  );
}

const IMAGE_EXT = /\.(png|jpe?g|gif|webp|bmp|svg)$/i;

/** One attachment chip. Split out from `AttachmentChips` so each attachment
 * gets its own `downloadsByHash` subscription and local reveal-error state —
 * a `.map()` callback can't call hooks per iteration. */
function AttachmentChip({ row, att }: { row: MessageRow; att: AttachmentRef }) {
  const dl = useStore((s) => s.downloadsByHash[att.ciphertext_sha256]);
  const fetchAttachment = useStore((s) => s.fetchAttachment);
  const [revealErr, setRevealErr] = useState(false);

  // Our own not-yet-reconciled optimistic send (see store.ts's
  // sendAttachmentMsg) — no real hash exists yet, so there's nothing to
  // download; the row's own pending/failed markers already carry the status.
  if (att.ciphertext_sha256.startsWith("tmp-")) {
    return (
      <span className="att">
        📎 {att.file_name}
      </span>
    );
  }

  async function reveal() {
    setRevealErr(false);
    try {
      await revealAttachment(row.group, att.ciphertext_sha256);
    } catch {
      setRevealErr(true);
    }
  }

  const isImage =
    (!!att.media_type && att.media_type.startsWith("image/")) ||
    IMAGE_EXT.test(att.file_name);

  if (dl?.status === "done" && dl.path) {
    if (isImage) {
      return (
        <span className="att attimg">
          <img
            src={convertFileSrc(dl.path)}
            alt={att.file_name}
            onClick={() => void reveal()}
          />
          {revealErr && <i className="atterr">打开失败</i>}
        </span>
      );
    }
    return (
      <span className="att">
        📎 {att.file_name}
        <button type="button" onClick={() => void reveal()}>
          打开所在文件夹
        </button>
        {revealErr && <i className="atterr">打开失败</i>}
      </span>
    );
  }

  if (dl?.status === "downloading") {
    return (
      <span className="att">
        📎 {att.file_name} <i>下载中…</i>
      </span>
    );
  }

  return (
    <span className="att">
      📎 {att.file_name}
      <button
        type="button"
        onClick={() => void fetchAttachment(row.group, att.ciphertext_sha256)}
      >
        {dl?.status === "failed" ? "下载失败 · 重试" : "下载"}
      </button>
    </span>
  );
}

export function AttachmentChips({ row }: { row: MessageRow }) {
  if (!row.attachments?.length) return null;
  return (
    <p className="atts">
      {row.attachments.map((a) => (
        <AttachmentChip key={a.ciphertext_sha256} row={row} att={a} />
      ))}
    </p>
  );
}

export function ReactionPills({
  pills,
  onToggle,
}: {
  pills: Map<string, number>;
  onToggle?: (emoji: string) => void;
}) {
  if (pills.size === 0) return null;
  return (
    <div className="reacts">
      {[...pills.entries()].map(([emoji, n]) => (
        <button type="button" key={emoji} onClick={() => onToggle?.(emoji)}>
          {emoji} {n}
        </button>
      ))}
    </div>
  );
}

export function BotCard({ row }: { row: MessageRow }) {
  const isOp = row.event === "agent_op";
  const failed = row.ok === false || row.status === "failed";
  const dur =
    row.duration_ms !== undefined
      ? ` · ${(row.duration_ms / 1000).toFixed(1)}s`
      : "";
  const head = isOp
    ? `🤖 ${row.name || "bot"} · ${row.event_type ?? "op"} ${failed ? "失败" : row.status ?? ""}${dur}`
    : `🤖 bot · ${row.status ?? "active"}`;
  return (
    <div className={`bot ${failed ? "" : "ok"}`}>
      <div className="h">{head}</div>
      <div className="b">
        <p>
          {row.text || row.body || ""}
          {isOp && (row.ok === true ? " ✓" : failed ? " ✗" : "")}
        </p>
        {(row.run_id || row.channel) && (
          <p className="meta">
            {row.run_id ? `run ${row.run_id} · ` : ""}#{row.channel ?? "general"} ·{" "}
            {fmtTime(row.ts)}
          </p>
        )}
      </div>
    </div>
  );
}

export function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function fmtDay(ts: number): string {
  const d = new Date(ts * 1000);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  if (sameDay) return "今天";
  return d.toLocaleDateString("zh-CN", { month: "long", day: "numeric" });
}
