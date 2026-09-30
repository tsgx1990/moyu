// Social modals for the unlocked shell — add contact, join by code,
// generate an invite, review pending join requests — plus the relay-settings
// modal (also reachable while locked, since relay_list/relay_forget need no
// engine). None of these carry a secret, so contacts/invites/requests ride the
// generic session bridge; join goes through the dedicated `session_join`
// command (no passphrase in the unlocked state).
//
// Adds workspace governance (members/rename/leave/kick/admin),
// channel management (new/new-private/invite/rename/archive), and a
// keypackage-rotate action in the identity modal — same generic bridge,
// same style.
import { useEffect, useState } from "react";
import {
  adminAdd,
  adminRemove,
  channelArchive,
  channelInvite,
  channelNew,
  channelNewPrivate,
  channelRename,
  createInvite,
  joinByCode,
  keypackageRotate,
  netSettingsGet,
  netSettingsSet,
  relayForget,
  relayList,
  workspaceAdd,
  workspaceKick,
  workspaceLeave,
  workspaceMembers,
  workspaceNew,
  workspaceRename,
  type ApproveResult,
  type InviteReceipt,
  type JoinReceipt,
  type WorkspaceMemberRow,
} from "../lib/session";
import { getNotifyPref, setNotifyPref } from "../lib/notify";
import { errMsg, shortHex } from "../lib/format";
import { useAsync } from "../lib/hooks";
import { useStore, type Conversation } from "../store";

export type ModalRequest =
  | { kind: "contact" }
  | { kind: "join" }
  | { kind: "invite" }
  | { kind: "requests" }
  | { kind: "relay" }
  | { kind: "network" }
  | { kind: "identity" }
  | { kind: "workspace_new" }
  | { kind: "workspace"; wsGid: string; wsName: string };

function Modal({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="mback" onClick={onClose}>
      <div className="mdl" onClick={(e) => e.stopPropagation()}>
        <div className="mdlhd">
          <b>{title}</b>
          <button onClick={onClose} aria-label="关闭">
            ✕
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

function ContactModal({ onClose }: { onClose: () => void }) {
  const addNewContact = useStore((s) => s.addNewContact);
  const [peer, setPeer] = useState("");
  const [label, setLabel] = useState("");
  const { busy, err, run } = useAsync();
  const [ok, setOk] = useState<string | null>(null);

  async function submit(e: { preventDefault(): void }) {
    e.preventDefault();
    if (busy || !peer.trim()) return;
    await run(async () => {
      const r = await addNewContact(peer.trim(), label.trim() || undefined);
      setOk(`已添加 ${r.label}`);
      setPeer("");
      setLabel("");
    });
  }

  return (
    <Modal title="添加联系人" onClose={onClose}>
      <form className="mform" onSubmit={submit}>
        <label>npub 或 NIP-05</label>
        <input
          value={peer}
          onChange={(e) => setPeer(e.target.value)}
          placeholder="npub1… 或 name@domain"
          autoFocus
          disabled={busy}
          spellCheck={false}
        />
        <label>备注名（可选）</label>
        <input
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder="显示名"
          disabled={busy}
        />
        {err && <p className="merr">{err}</p>}
        {ok && <p className="mok">{ok}</p>}
        <button type="submit" disabled={busy || !peer.trim()}>
          {busy ? "解析中…" : "添加"}
        </button>
      </form>
    </Modal>
  );
}

function JoinModal({ onClose }: { onClose: () => void }) {
  const [code, setCode] = useState("");
  const { busy, err, run } = useAsync();
  const [ok, setOk] = useState<JoinReceipt | null>(null);

  async function submit(e: { preventDefault(): void }) {
    e.preventDefault();
    if (busy || !code.trim()) return;
    await run(async () => {
      // Already unlocked: no passphrase. The `joined` event refreshes the
      // sidebar; here we just confirm the outcome.
      const r = await joinByCode(code.trim());
      setOk(r);
      setCode("");
    });
  }

  return (
    <Modal title="用邀请码加入" onClose={onClose}>
      <form className="mform" onSubmit={submit}>
        <label>邀请码</label>
        <input
          value={code}
          onChange={(e) => setCode(e.target.value)}
          placeholder="moyuinv1…（或含邀请码的链接）"
          autoFocus
          disabled={busy}
          spellCheck={false}
        />
        {err && <p className="merr">{err}</p>}
        {ok && (
          <p className="mok">
            {ok.kind === "workspace"
              ? `已请求加入 #${ok.ws_name ?? ""}，等待管理员批准`
              : `已向 ${shortHex(ok.inviter, 12)}… 发送联系请求`}
          </p>
        )}
        <button type="submit" disabled={busy || !code.trim()}>
          {busy ? "加入中…" : "加入"}
        </button>
      </form>
    </Modal>
  );
}

function InviteModal({ onClose }: { onClose: () => void }) {
  const convs = useStore((s) => s.convs);
  // Unique workspaces from the sidebar model: {gid → name}.
  const workspaces = new Map<string, string>();
  for (const c of convs) {
    if (c.wsGroup) workspaces.set(c.wsGroup, c.wsName ?? c.wsGroup.slice(0, 8));
  }
  const wsList = [...workspaces.entries()];

  // "" = contact invite (no workspace); otherwise a workspace group hex.
  const [wsGid, setWsGid] = useState("");
  const [autoApprove, setAutoApprove] = useState(false);
  const { busy, err, run } = useAsync();
  const [result, setResult] = useState<InviteReceipt | null>(null);
  const [copied, setCopied] = useState(false);

  async function generate() {
    if (busy) return;
    await run(async () => {
      setResult(null);
      const r = await createInvite(wsGid || undefined, autoApprove);
      setResult(r);
    });
  }

  async function copy() {
    if (!result) return;
    try {
      await navigator.clipboard.writeText(result.token);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard denied — the field is selectable as a fallback.
    }
  }

  return (
    <Modal title="生成邀请码" onClose={onClose}>
      <div className="mform">
        <label>范围</label>
        <select
          value={wsGid}
          onChange={(e) => setWsGid(e.target.value)}
          disabled={busy}
        >
          <option value="">联系人邀请（1:1）</option>
          {wsList.map(([gid, name]) => (
            <option key={gid} value={gid}>
              Workspace · {name}
            </option>
          ))}
        </select>
        {wsGid !== "" && (
          <label className="chk">
            <input
              type="checkbox"
              checked={autoApprove}
              onChange={(e) => setAutoApprove(e.target.checked)}
              disabled={busy}
            />
            持此码者自动批准加入
          </label>
        )}
        {err && <p className="merr">{err}</p>}
        {result && (
          <div className="invout">
            <label>邀请码</label>
            <textarea readOnly value={result.token} rows={3} onFocus={(e) => e.target.select()} />
            <button type="button" className="ghost" onClick={() => void copy()}>
              {copied ? "已复制 ✓" : "复制邀请码"}
            </button>
          </div>
        )}
        <button type="button" onClick={() => void generate()} disabled={busy}>
          {busy ? "生成中…" : result ? "重新生成" : "生成"}
        </button>
      </div>
    </Modal>
  );
}

/** Found a brand-new workspace. Selects its auto-seeded `#general` channel
 * on success so the caller lands somewhere, not on a blank sidebar entry. */
function WorkspaceNewModal({ onClose }: { onClose: () => void }) {
  const hydrate = useStore((s) => s.hydrate);
  const select = useStore((s) => s.select);
  const [name, setName] = useState("");
  const { busy, err, run } = useAsync();

  async function submit(e: { preventDefault(): void }) {
    e.preventDefault();
    const trimmed = name.trim();
    if (busy || !trimmed) return;
    await run(async () => {
      const r = await workspaceNew(trimmed);
      await hydrate();
      await select(`ch:${r.group}:general`);
      onClose();
    });
  }

  return (
    <Modal title="新建工作区" onClose={onClose}>
      <form className="mform" onSubmit={submit}>
        <label>工作区名称</label>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="例如 backend-team"
          autoFocus
          disabled={busy}
        />
        {err && <p className="merr">{err}</p>}
        <button type="submit" disabled={busy || !name.trim()}>
          {busy ? "创建中…" : "创建"}
        </button>
      </form>
    </Modal>
  );
}

function RequestsModal({ onClose }: { onClose: () => void }) {
  const requests = useStore((s) => s.joinRequests);
  const loadJoinRequests = useStore((s) => s.loadJoinRequests);
  const approveJoin = useStore((s) => s.approveJoin);
  const denyJoin = useStore((s) => s.denyJoin);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  useEffect(() => {
    void loadJoinRequests();
  }, [loadJoinRequests]);

  async function onApprove(npub: string) {
    setBusy(npub);
    setMsg(null);
    try {
      const res: ApproveResult = await approveJoin(npub);
      const okCount = res.outcomes.length;
      setMsg(
        res.failed.length > 0
          ? `已批准 ${okCount} 项，${res.failed.length} 项失败：${res.failed.join("；")}`
          : `已批准 ${okCount} 项`,
      );
    } catch (e) {
      setMsg(errMsg(e));
    } finally {
      setBusy(null);
    }
  }

  async function onDeny(npub: string) {
    setBusy(npub);
    setMsg(null);
    try {
      await denyJoin(npub);
    } catch (e) {
      setMsg(errMsg(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <Modal title="加入请求" onClose={onClose}>
      <div className="mform">
        {requests.length === 0 && <p className="mdim">暂无待批准的加入请求。</p>}
        {requests.map((r) => (
          <div key={`${r.ws_gid}:${r.npub}`} className="reqrow">
            <div className="reqmeta">
              <span className="reqnpub" title={r.npub}>
                {shortHex(r.npub, 14)}…
              </span>
              <span className="reqws">
                #{r.ws_name}{" "}
                {r.trusted ? (
                  <em className="trust ok">✓ 持码可信</em>
                ) : (
                  <em className="trust warn">⚠ 无凭证</em>
                )}
              </span>
            </div>
            <div className="reqacts">
              <button
                className="ghost"
                onClick={() => void onDeny(r.npub)}
                disabled={busy === r.npub}
              >
                拒绝
              </button>
              <button
                onClick={() => void onApprove(r.npub)}
                disabled={busy === r.npub}
              >
                {busy === r.npub ? "…" : "批准"}
              </button>
            </div>
          </div>
        ))}
        {msg && <p className="mok">{msg}</p>}
        <p className="mdim tiny">
          拒绝仅在本机隐藏该请求（v1 不发送任何网络事件）。
        </p>
      </div>
    </Modal>
  );
}

/** Relay settings — works while locked OR unlocked. Standalone export so the
 * locked unlock card can open it too. */
export function RelayModal({ onClose }: { onClose: () => void }) {
  const [relays, setRelays] = useState<string[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  async function refresh() {
    try {
      const { relays } = await relayList();
      setRelays(relays);
    } catch (e) {
      setErr(errMsg(e));
    }
  }
  useEffect(() => {
    void refresh();
  }, []);

  async function forget(url: string) {
    setBusy(url);
    setErr(null);
    try {
      const { relays } = await relayForget(url);
      setRelays(relays);
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <Modal title="Relay 设置" onClose={onClose}>
      <div className="mform">
        {relays === null && !err && <p className="mdim">加载中…</p>}
        {relays?.length === 0 && (
          <p className="mdim">配置中没有持久化的 relay（使用内置默认）。</p>
        )}
        {relays?.map((url) => (
          <div key={url} className="relayrow">
            <span className="relayurl" title={url}>
              {url}
            </span>
            <button
              className="ghost"
              onClick={() => void forget(url)}
              disabled={busy === url}
            >
              {busy === url ? "…" : "移除"}
            </button>
          </div>
        ))}
        {err && <p className="merr">{err}</p>}
        <p className="mdim tiny">
          新增 relay 目前只能在新建身份（init）或凭邀请码加入（join
          会并入邀请方的 relay）时设置。
        </p>
      </div>
    </Modal>
  );
}

/** Network settings — the SOCKS5 proxy + local-relay (loopback) toggle.
 * Exported (like `RelayModal`) so it mounts from the onboarding / unlock
 * screens too, where a blocked default relay is exactly the problem it solves.
 * Saving restarts the sidecar so the new spawn-time flags take effect. */
export function NetworkModal({ onClose }: { onClose: () => void }) {
  const [socks5, setSocks5] = useState("");
  const [loopback, setLoopback] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [ok, setOk] = useState<string | null>(null);
  const { busy, err, setErr, run } = useAsync();

  useEffect(() => {
    void (async () => {
      try {
        const s = await netSettingsGet();
        setSocks5(s.socks5 ?? "");
        setLoopback(s.allow_loopback);
      } catch (e) {
        setErr(errMsg(e));
      } finally {
        setLoaded(true);
      }
    })();
  }, [setErr]);

  async function save() {
    setOk(null);
    await run(async () => {
      // Empty proxy → null (direct). The Rust side re-validates and rejects a
      // malformed address before it can reach a spawn.
      const s = await netSettingsSet(socks5.trim() || null, loopback);
      setSocks5(s.socks5 ?? "");
      setLoopback(s.allow_loopback);
      setOk("已保存 · 正在按新设置重连后台会话…");
    });
  }

  return (
    <Modal title="网络 / 代理设置" onClose={onClose}>
      <div className="mform">
        <label>SOCKS5 代理</label>
        <input
          type="text"
          placeholder="127.0.0.1:1080（留空 = 直连）"
          value={socks5}
          onChange={(e) => setSocks5(e.target.value)}
          spellCheck={false}
          autoComplete="off"
          disabled={busy || !loaded}
        />
        <p className="mdim tiny">
          如果所在网络无法直连公共 relay，填入一个 SOCKS5 代理（例如本机 Tor 或
          ssh -D 的端口），所有 relay 连接、NIP-05 查询与附件传输都会走它。需
          IP:PORT（用 IP，不是域名）。
        </p>

        <hr />
        <label>本地 relay（测试）</label>
        <label className="chk">
          <input
            type="checkbox"
            checked={loopback}
            onChange={(e) => setLoopback(e.target.checked)}
            disabled={busy || !loaded}
          />
          允许 loopback relay（ws://127.0.0.1，仅本地测试）
        </label>
        <p className="mdim tiny">
          勾选后可在"新建身份"里用"自定义"填 ws://127.0.0.1:PORT 指向本机 relay
          做端到端自测。生产环境请勿指向 loopback relay。
        </p>

        <hr />
        {err && <p className="merr">{err}</p>}
        {ok && <p className="mok">{ok}</p>}
        <p className="mdim tiny">
          保存会重启后台会话（短暂回到解锁/引导屏），口令不会被缓存。
        </p>
        <button type="button" onClick={() => void save()} disabled={busy || !loaded}>
          {busy ? "保存中…" : "保存并重连"}
        </button>
      </div>
    </Modal>
  );
}

/** Identity settings — currently just the keypackage-rotate action. Only
 * mounted while unlocked (Shell's sidebar is the sole entry point), unlike
 * `RelayModal` which also works locked. */
function IdentityModal({ onClose }: { onClose: () => void }) {
  const identity = useStore((s) => s.identity);
  const { busy, err, run } = useAsync();
  const [msg, setMsg] = useState<string | null>(null);
  const [notify, setNotify] = useState(() => getNotifyPref());

  function toggleNotify(enabled: boolean) {
    setNotify(enabled);
    setNotifyPref(enabled);
  }

  async function rotate() {
    if (busy) return;
    await run(async () => {
      setMsg(null);
      const r = await keypackageRotate();
      setMsg(`已轮换 KeyPackage · ${r.bytes} 字节 · ref ${shortHex(r.ref, 12)}…`);
    });
  }

  return (
    <Modal title="身份设置" onClose={onClose}>
      <div className="mform">
        <label>账号</label>
        <p className="mdim">{identity?.label ?? "—"}</p>
        {identity?.npub && (
          <p className="reqnpub" title={identity.npub}>
            {identity.npub}
          </p>
        )}

        <hr />
        <label>通知</label>
        <label className="chk">
          <input
            type="checkbox"
            checked={notify}
            onChange={(e) => toggleNotify(e.target.checked)}
          />
          桌面通知（收到新消息且窗口未聚焦时，弹出系统通知）
        </label>

        <hr />
        <label>KeyPackage 轮换</label>
        <p className="mdim tiny">
          轮换会发布一个新的 KeyPackage 并使旧的失效——只影响之后收到的邀请/被拉入的会话，
          已加入的会话不受影响（MLS 的前向保密/后妥协自愈已经覆盖它们）。怀疑本机密钥曾泄露时可主动轮换。
        </p>
        {err && <p className="merr">{err}</p>}
        {msg && <p className="mok">{msg}</p>}
        <button type="button" onClick={() => void rotate()} disabled={busy}>
          {busy ? "轮换中…" : "轮换 KeyPackage"}
        </button>
      </div>
    </Modal>
  );
}

/** One `key`'s transient inline-editor state for a channel row — mutually
 * exclusive rename/invite/confirm-archive, so opening one closes any other
 * row's in-progress edit. */
type ChannelRowMode =
  | { key: string; mode: "rename" | "invite"; value: string }
  | { key: string; mode: "confirm-archive" }
  | null;

/** Workspace governance + channel management panel for ONE workspace.
 * Members/admin state is fetched locally (not stored in zustand — nothing
 * else in the shell needs it); every mutation that changes the sidebar's
 * shape (rename/kick/leave/channel create/rename/archive) re-runs `hydrate`
 * immediately afterward so the conversation list never waits on the 2s pump. */
function WorkspaceModal({
  wsGid,
  wsName,
  onClose,
}: {
  wsGid: string;
  wsName: string;
  onClose: () => void;
}) {
  const hydrate = useStore((s) => s.hydrate);
  const convs = useStore((s) => s.convs);
  const [members, setMembers] = useState<WorkspaceMemberRow[] | null>(null);
  const [membersErr, setMembersErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [name, setName] = useState(wsName);
  // Tracks the just-renamed name locally: `hydrate()` refreshes the STORE's
  // copy, but this modal's own `wsName` prop is fixed at mount (Shell.tsx
  // doesn't re-open it with fresh props), so the title/compare-against value
  // would otherwise stay stale until the modal is closed and reopened.
  const [liveWsName, setLiveWsName] = useState(wsName);
  const [transferTo, setTransferTo] = useState("");
  const [rowMode, setRowMode] = useState<ChannelRowMode>(null);
  const [newChName, setNewChName] = useState("");
  const [newChPrivate, setNewChPrivate] = useState(false);
  const [newChInvite, setNewChInvite] = useState("");
  const [addPeer, setAddPeer] = useState("");

  async function loadMembers() {
    try {
      const { members } = await workspaceMembers(wsGid);
      setMembers(members);
      setMembersErr(null);
    } catch (e) {
      setMembersErr(errMsg(e));
    }
  }
  useEffect(() => {
    void loadMembers();
    // wsGid is the only thing this modal is scoped to; loadMembers is stable
    // per render but re-created, intentionally not in the dep list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [wsGid]);

  const amIAdmin = members?.find((m) => m.you)?.admin ?? false;
  const labelFor = (npub: string) =>
    convs.find((c) => c.kind === "dm" && c.peerNpub === npub)?.title;

  /** Shared run wrapper: busy-key + error/success surface for every mutation
   * below (kick/rename/admin/leave/channel ops all follow the same shape). */
  async function run(key: string, fn: () => Promise<void>) {
    setBusy(key);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setBusy(null);
    }
  }

  async function doRename(e: { preventDefault(): void }) {
    e.preventDefault();
    const trimmed = name.trim();
    if (!trimmed || trimmed === liveWsName) return;
    await run("rename", async () => {
      await workspaceRename(wsGid, trimmed);
      await hydrate();
      setLiveWsName(trimmed);
      setMsg("已重命名工作区");
    });
  }

  async function doKick(npub: string) {
    await run(`kick:${npub}`, async () => {
      await workspaceKick(wsGid, npub);
      await Promise.all([loadMembers(), hydrate()]);
      setConfirm(null);
      setMsg("已移出成员");
    });
  }

  async function doAdmin(npub: string, promote: boolean) {
    await run(`admin:${npub}`, async () => {
      if (promote) await adminAdd(wsGid, npub);
      else await adminRemove(wsGid, npub);
      await loadMembers();
      setMsg(promote ? "已设为管理员" : "已撤销管理员");
    });
  }

  /** Pull an existing contact straight into the workspace by npub/nip-05 —
   * no admin gate server-side (same as the CLI's `workspace add`), so this
   * is offered to every member, not just admins. */
  async function doAddMember(e: { preventDefault(): void }) {
    e.preventDefault();
    const trimmed = addPeer.trim();
    if (!trimmed) return;
    await run("addmember", async () => {
      await workspaceAdd(wsGid, trimmed);
      await Promise.all([loadMembers(), hydrate()]);
      setAddPeer("");
      setMsg("已拉入工作区");
    });
  }

  async function doLeave() {
    await run("leave", async () => {
      await workspaceLeave(wsGid, transferTo || undefined);
      await hydrate();
      onClose(); // the workspace this panel is scoped to is gone for us now
    });
  }

  const channels = convs.filter(
    (c): c is Conversation & { wsGroup: string } =>
      c.wsGroup === wsGid && (c.kind === "channel" || c.kind === "private"),
  );

  async function doChannelCreate() {
    const trimmed = newChName.trim();
    if (!trimmed) return;
    await run("channew", async () => {
      if (newChPrivate) {
        const invite = newChInvite
          .split(/[,\n]/)
          .map((s) => s.trim())
          .filter(Boolean);
        await channelNewPrivate(wsGid, trimmed, invite);
      } else {
        await channelNew(wsGid, trimmed);
      }
      await hydrate();
      setNewChName("");
      setNewChPrivate(false);
      setNewChInvite("");
      setMsg("已创建频道");
    });
  }

  async function doChannelRename(c: Conversation, newName: string) {
    if (!newName || newName === c.title) {
      setRowMode(null);
      return;
    }
    await run(`chrename:${c.key}`, async () => {
      await channelRename(wsGid, c.title, newName);
      await hydrate();
      setRowMode(null);
      setMsg("已重命名频道");
    });
  }

  async function doChannelInvite(c: Conversation, peer: string) {
    if (!peer) return;
    await run(`chinvite:${c.key}`, async () => {
      await channelInvite(wsGid, c.title, peer);
      setRowMode(null);
      setMsg(`已邀请加入 #${c.title}`);
    });
  }

  async function doChannelArchive(c: Conversation) {
    await run(`charchive:${c.key}`, async () => {
      await channelArchive(wsGid, c.title);
      await hydrate();
      setRowMode(null);
      setMsg("已归档频道");
    });
  }

  const others = members?.filter((m) => !m.you) ?? [];

  return (
    <Modal title={`工作区设置 · ${liveWsName}`} onClose={onClose}>
      <div className="mform">
        <label>工作区名称</label>
        <form onSubmit={doRename}>
          <div className="reqrow">
            <div className="reqmeta">
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                disabled={busy === "rename"}
              />
            </div>
            <div className="reqacts">
              <button
                type="submit"
                disabled={busy === "rename" || !name.trim() || name.trim() === liveWsName}
              >
                {busy === "rename" ? "…" : "重命名"}
              </button>
            </div>
          </div>
        </form>

        <hr />
        <label>成员{amIAdmin ? "" : "（只读 · 你不是管理员）"}</label>
        {membersErr && <p className="merr">{membersErr}</p>}
        {members === null && !membersErr && <p className="mdim">加载中…</p>}
        {members?.map((m) => (
          <div key={m.npub} className="reqrow">
            <div className="reqmeta">
              <span className="reqnpub" title={m.npub}>
                {labelFor(m.npub) ?? `${shortHex(m.npub, 14)}…`}
              </span>
              <span className="reqws">
                {m.admin && <em className="badge admin">管理员</em>}
                {m.admin && m.you && " "}
                {m.you && <em className="badge you">你</em>}
                {!m.admin && !m.you && "成员"}
              </span>
            </div>
            {!m.you && amIAdmin && (
              <div className="reqacts">
                {confirm === `kick:${m.npub}` ? (
                  <>
                    <span className="confirmtext">移出该成员？</span>
                    <button className="ghost" onClick={() => setConfirm(null)}>
                      取消
                    </button>
                    <button
                      className="danger"
                      disabled={busy === `kick:${m.npub}`}
                      onClick={() => void doKick(m.npub)}
                    >
                      {busy === `kick:${m.npub}` ? "…" : "确认移出"}
                    </button>
                  </>
                ) : (
                  <>
                    <button
                      className="ghost"
                      disabled={busy === `admin:${m.npub}`}
                      onClick={() => void doAdmin(m.npub, !m.admin)}
                    >
                      {busy === `admin:${m.npub}`
                        ? "…"
                        : m.admin
                          ? "撤销管理员"
                          : "设为管理员"}
                    </button>
                    <button className="danger" onClick={() => setConfirm(`kick:${m.npub}`)}>
                      移出
                    </button>
                  </>
                )}
              </div>
            )}
          </div>
        ))}
        <form onSubmit={doAddMember} className="reqrow">
          <div className="reqmeta">
            <input
              placeholder="npub 或 nip-05 · 直接拉入工作区（无需管理员权限）"
              value={addPeer}
              onChange={(e) => setAddPeer(e.target.value)}
              disabled={busy === "addmember"}
            />
          </div>
          <div className="reqacts">
            <button type="submit" disabled={busy === "addmember" || !addPeer.trim()}>
              {busy === "addmember" ? "…" : "拉入"}
            </button>
          </div>
        </form>

        <hr />
        <label>频道</label>
        {channels.length === 0 && <p className="mdim">暂无频道。</p>}
        {channels.map((c) => {
          const mode = rowMode?.key === c.key ? rowMode : null;
          return (
            <div key={c.key} className="reqrow">
              <div className="reqmeta">
                {mode?.mode === "rename" || mode?.mode === "invite" ? (
                  <input
                    autoFocus
                    value={mode.value}
                    placeholder={mode.mode === "invite" ? "npub 或 nip-05" : undefined}
                    onChange={(e) => setRowMode({ key: c.key, mode: mode.mode, value: e.target.value })}
                  />
                ) : (
                  <span className="reqnpub">
                    {c.kind === "private" ? "🔒 " : "＃ "}
                    {c.title}
                  </span>
                )}
                <span className="reqws">{c.kind === "private" ? "私有频道" : "公开频道"}</span>
              </div>
              <div className="reqacts">
                {mode?.mode === "rename" ? (
                  <>
                    <button className="ghost" onClick={() => setRowMode(null)}>
                      取消
                    </button>
                    <button
                      disabled={busy === `chrename:${c.key}` || !mode.value.trim()}
                      onClick={() => void doChannelRename(c, mode.value.trim())}
                    >
                      {busy === `chrename:${c.key}` ? "…" : "保存"}
                    </button>
                  </>
                ) : mode?.mode === "invite" ? (
                  <>
                    <button className="ghost" onClick={() => setRowMode(null)}>
                      取消
                    </button>
                    <button
                      disabled={busy === `chinvite:${c.key}` || !mode.value.trim()}
                      onClick={() => void doChannelInvite(c, mode.value.trim())}
                    >
                      {busy === `chinvite:${c.key}` ? "…" : "邀请"}
                    </button>
                  </>
                ) : mode?.mode === "confirm-archive" ? (
                  <>
                    <span className="confirmtext">归档后不可撤销</span>
                    <button className="ghost" onClick={() => setRowMode(null)}>
                      取消
                    </button>
                    <button
                      className="danger"
                      disabled={busy === `charchive:${c.key}`}
                      onClick={() => void doChannelArchive(c)}
                    >
                      {busy === `charchive:${c.key}` ? "…" : "确认归档"}
                    </button>
                  </>
                ) : (
                  <>
                    {c.kind === "private" && (
                      <button
                        className="ghost"
                        onClick={() => setRowMode({ key: c.key, mode: "invite", value: "" })}
                      >
                        拉人
                      </button>
                    )}
                    {/* rename/archive are workspace-projection ops — a private
                        channel lives in its own MLS group outside the projection,
                        so the sidecar rejects both (`no channel #<slug>`). Public
                        channels only. */}
                    {c.kind === "channel" && (
                      <>
                        <button
                          className="ghost"
                          onClick={() => setRowMode({ key: c.key, mode: "rename", value: c.title })}
                        >
                          重命名
                        </button>
                        <button
                          className="danger"
                          onClick={() => setRowMode({ key: c.key, mode: "confirm-archive" })}
                        >
                          归档
                        </button>
                      </>
                    )}
                  </>
                )}
              </div>
            </div>
          );
        })}
        <div className="reqrow">
          <div className="reqmeta">
            <input
              placeholder="新频道名称"
              value={newChName}
              onChange={(e) => setNewChName(e.target.value)}
              disabled={busy === "channew"}
            />
            <label className="chk">
              <input
                type="checkbox"
                checked={newChPrivate}
                onChange={(e) => setNewChPrivate(e.target.checked)}
                disabled={busy === "channew"}
              />
              私有频道
            </label>
            {newChPrivate && (
              <input
                placeholder="邀请成员 npub（逗号或换行分隔，可留空）"
                value={newChInvite}
                onChange={(e) => setNewChInvite(e.target.value)}
                disabled={busy === "channew"}
              />
            )}
          </div>
          <div className="reqacts">
            <button
              disabled={busy === "channew" || !newChName.trim()}
              onClick={() => void doChannelCreate()}
            >
              {busy === "channew" ? "…" : "新建"}
            </button>
          </div>
        </div>

        {err && <p className="merr">{err}</p>}
        {msg && <p className="mok">{msg}</p>}

        <hr />
        <label>离开工作区</label>
        {others.length > 0 && (
          <select
            value={transferTo}
            onChange={(e) => setTransferTo(e.target.value)}
            disabled={busy === "leave"}
          >
            <option value="">如果你是唯一管理员，先选择转让对象（否则留空）</option>
            {others.map((m) => (
              <option key={m.npub} value={m.npub}>
                {labelFor(m.npub) ?? `${shortHex(m.npub, 14)}…`}
              </option>
            ))}
          </select>
        )}
        {confirm === "leave" ? (
          <div className="reqacts">
            <span className="confirmtext">确定要离开该工作区？</span>
            <button className="ghost" onClick={() => setConfirm(null)}>
              取消
            </button>
            <button className="danger" disabled={busy === "leave"} onClick={() => void doLeave()}>
              {busy === "leave" ? "…" : "确认离开"}
            </button>
          </div>
        ) : (
          <button type="button" className="ghost" onClick={() => setConfirm("leave")}>
            离开工作区
          </button>
        )}
      </div>
    </Modal>
  );
}

/** Dispatch by request. `relay` is handled by the standalone `RelayModal`
 * export above so it can also mount from the locked screen. */
export default function Modals({
  request,
  onClose,
}: {
  request: ModalRequest;
  onClose: () => void;
}) {
  switch (request.kind) {
    case "contact":
      return <ContactModal onClose={onClose} />;
    case "join":
      return <JoinModal onClose={onClose} />;
    case "invite":
      return <InviteModal onClose={onClose} />;
    case "requests":
      return <RequestsModal onClose={onClose} />;
    case "relay":
      return <RelayModal onClose={onClose} />;
    case "network":
      return <NetworkModal onClose={onClose} />;
    case "identity":
      return <IdentityModal onClose={onClose} />;
    case "workspace_new":
      return <WorkspaceNewModal onClose={onClose} />;
    case "workspace":
      return (
        <WorkspaceModal wsGid={request.wsGid} wsName={request.wsName} onClose={onClose} />
      );
  }
}
