// The no-account welcome screen. Three ways in — create a fresh
// identity, import an existing nsec, or redeem an invite code — all of which
// transition the session straight to unlocked.
//
// SECURITY: the passphrase, the imported nsec, and the confirm field live ONLY
// in this component's local state. They go through the dedicated
// `initAccount` / `joinByCode` wrappers (Rust-side `session_init` /
// `session_join`, which wrap the secret in `Zeroizing`), NEVER through the
// generic bridge or the zustand store. On success the fields are wiped (the
// component also unmounts as the session flips to unlocked); on FAILURE they
// are deliberately retained so a flaky-relay retry doesn't force the user to
// re-type the whole passphrase — the same secret is already sitting in the
// on-screen input either way, so keeping it across a failed attempt adds no
// new exposure.
import { useState } from "react";
import { initAccount, joinByCode, type RelayChoice } from "../lib/session";
import { useAsync } from "../lib/hooks";
import { useStore } from "../store";
import { NetworkModal } from "./Modals";

type Tab = "create" | "import" | "join";

type Preset = "default" | "custom";

function RelayPicker({
  preset,
  setPreset,
  customUrls,
  setCustomUrls,
  disabled,
}: {
  preset: Preset;
  setPreset: (p: Preset) => void;
  customUrls: string;
  setCustomUrls: (s: string) => void;
  disabled: boolean;
}) {
  return (
    <div className="relaypick">
      <span className="fl">Relay</span>
      <div className="segs">
        {(
          [
            ["default", "默认公共"],
            ["custom", "自定义 / 自建"],
          ] as [Preset, string][]
        ).map(([p, label]) => (
          <button
            key={p}
            type="button"
            className={preset === p ? "seg on" : "seg"}
            onClick={() => setPreset(p)}
            disabled={disabled}
          >
            {label}
          </button>
        ))}
      </div>
      {preset === "custom" && (
        <textarea
          className="urls"
          placeholder="wss:// relay URL，每行或空格分隔一个"
          value={customUrls}
          onChange={(e) => setCustomUrls(e.target.value)}
          disabled={disabled}
          rows={2}
        />
      )}
    </div>
  );
}

/** Split a free-text relay box into a URL list (whitespace/newline separated). */
function parseUrls(raw: string): string[] {
  return raw
    .split(/\s+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

export default function Onboarding() {
  const setIdentity = useStore((s) => s.setIdentity);
  const setSessionState = useStore((s) => s.setSessionState);
  const [tab, setTab] = useState<Tab>("create");
  const { busy, err: error, setErr: setError, run } = useAsync();

  // Secrets — local only, wiped on every submit.
  const [passphrase, setPassphrase] = useState("");
  const [confirm, setConfirm] = useState("");
  const [nsec, setNsec] = useState("");
  const [code, setCode] = useState("");

  // Non-secret relay choice.
  const [preset, setPreset] = useState<Preset>("default");
  const [customUrls, setCustomUrls] = useState("");

  // Network/proxy settings panel (SOCKS5 + loopback) — the escape hatch when
  // the default public relays are unreachable from this network.
  const [netOpen, setNetOpen] = useState(false);

  function wipeSecrets() {
    setPassphrase("");
    setConfirm("");
    setNsec("");
  }

  function relayChoice(): RelayChoice {
    if (preset === "custom") return { preset: "custom", urls: parseUrls(customUrls) };
    return { preset };
  }

  async function submit(e: { preventDefault(): void }) {
    e.preventDefault();
    if (busy) return;
    setError(null);

    // Client-side guards before we touch any secret-bearing command. All
    // three paths create/unlock an account, so a passphrase is always
    // required (no-account `join` rejects an empty one backend-side too).
    if (passphrase.length === 0) return setError("请设置口令");
    if (passphrase !== confirm) return setError("两次输入的口令不一致");
    if (tab === "import" && nsec.trim().length === 0)
      return setError("请粘贴要导入的 nsec");
    if (tab === "join" && code.trim().length === 0)
      return setError("请粘贴邀请码");
    if (preset === "custom" && parseUrls(customUrls).length === 0)
      return setError("自定义 relay 需要至少一个 wss:// URL");

    // Snapshot the secrets into locals so the awaited call below never reads
    // component state. The on-screen fields are wiped only once the call
    // SUCCEEDS (see below) — a failed attempt keeps them so the user can
    // retry (e.g. after switching to a reachable relay) without re-typing.
    const pp = passphrase;
    const importNsec = nsec.trim();
    const joinCode = code.trim();
    await run(async () => {
      let res: { state: "unlocked"; label?: string; npub?: string };
      if (tab === "join") {
        res = { state: "unlocked", ...(await joinByCode(joinCode, pp)) };
      } else {
        res = await initAccount(pp, {
          importNsec: tab === "import" ? importNsec : undefined,
          relays: relayChoice(),
        });
      }
      // Success: safe to clear every on-screen secret now.
      wipeSecrets();
      setCode("");
      setIdentity({ label: res.label, npub: res.npub });
      setSessionState("unlocked");
    });
  }

  const TABS: [Tab, string][] = [
    ["create", "新建身份"],
    ["import", "导入 nsec"],
    ["join", "邀请码加入"],
  ];

  return (
    <div className="ul">
      <div className="ulcard wide">
        <div className="mark">摸</div>
        <h1>moyu</h1>
        <p className="tag">端到端加密 · 给程序员的聊天</p>

        <div className="tabs">
          {TABS.map(([t, label]) => (
            <button
              key={t}
              type="button"
              className={tab === t ? "tab on" : "tab"}
              onClick={() => {
                setTab(t);
                setError(null);
              }}
              disabled={busy}
            >
              {label}
            </button>
          ))}
        </div>

        <form onSubmit={submit}>
          {tab === "import" && (
            <input
              type="password"
              placeholder="nsec1…（要导入的私钥）"
              value={nsec}
              onChange={(e) => setNsec(e.target.value)}
              autoComplete="off"
              disabled={busy}
            />
          )}
          {tab === "join" && (
            <input
              type="text"
              placeholder="邀请码 moyuinv1…（或含邀请码的链接）"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              autoComplete="off"
              disabled={busy}
              spellCheck={false}
            />
          )}

          <input
            type="password"
            placeholder={tab === "join" ? "设置本机口令" : "口令"}
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            autoComplete="new-password"
            disabled={busy}
          />
          <input
            type="password"
            placeholder="确认口令"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            autoComplete="new-password"
            disabled={busy}
          />

          {tab !== "join" && (
            <RelayPicker
              preset={preset}
              setPreset={setPreset}
              customUrls={customUrls}
              setCustomUrls={setCustomUrls}
              disabled={busy}
            />
          )}

          <button type="submit" disabled={busy}>
            {busy
              ? "处理中…"
              : tab === "create"
                ? "创建身份"
                : tab === "import"
                  ? "导入并解锁"
                  : "加入并解锁"}
          </button>
        </form>

        {error && <p className="error">{error}</p>}
        <p className="hint">
          {tab === "join"
            ? "凭码会新建本机身份并请求加入 · 口令加密存放本机"
            : "私钥以 Argon2id 加密存放本机 · 忘记口令无法恢复"}
        </p>
        <button
          type="button"
          className="linkbtn"
          onClick={() => setNetOpen(true)}
          disabled={busy}
        >
          网络 / 代理设置
        </button>
      </div>
      {netOpen && <NetworkModal onClose={() => setNetOpen(false)} />}
    </div>
  );
}
