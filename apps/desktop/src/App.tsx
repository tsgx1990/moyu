// Root: session status routing. Locked/down states show the unlock card;
// unlocked hydrates the store and renders the shell. One status listener +
// one event listener feed the zustand store (single dispatch point).
import { useEffect, useRef, useState } from "react";
import {
  catchup,
  onSessionEvent,
  onSessionStatus,
  sessionStatus,
  unlock,
  whoami,
  type SessionState,
} from "./lib/session";
import { makeCatchupTrigger } from "./lib/catchup";
import { syncNotifyPrefOnStartup } from "./lib/notify";
import { useAsync } from "./lib/hooks";
import { useStore } from "./store";
import Shell from "./components/Shell";
import Onboarding from "./components/Onboarding";
import { RelayModal, NetworkModal } from "./components/Modals";
import "./theme.css";

const STATE_LABEL: Record<SessionState, string> = {
  starting: "启动中",
  "no-account": "无账号",
  locked: "已锁定",
  unlocked: "已解锁",
  down: "会话中断",
  restarting: "重连中",
};

function UnlockCard({ state }: { state: SessionState }) {
  const setIdentity = useStore((s) => s.setIdentity);
  const setSessionState = useStore((s) => s.setSessionState);
  const [passphrase, setPassphrase] = useState("");
  const { busy, err: error, run } = useAsync();
  const [relayOpen, setRelayOpen] = useState(false);
  const [netOpen, setNetOpen] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  async function handleUnlock(e: { preventDefault(): void }) {
    e.preventDefault();
    await run(async () => {
      try {
        const res = await unlock(passphrase);
        setPassphrase(""); // never keep the passphrase in JS state post-submit
        setIdentity({ label: res.label, npub: res.npub });
        setSessionState("unlocked");
      } catch (err) {
        setPassphrase("");
        inputRef.current?.focus();
        throw err;
      }
    });
  }

  return (
    <div className="ul">
      <div className="ulcard">
        <div className="mark">摸</div>
        <h1>moyu</h1>
        <p className="tag">端到端加密 · 给程序员的聊天</p>
        <form onSubmit={handleUnlock}>
          <input
            ref={inputRef}
            type="password"
            placeholder="口令"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            autoComplete="off"
            autoFocus
            disabled={busy || state === "down" || state === "restarting"}
          />
          <button type="submit" disabled={busy || passphrase.length === 0}>
            {busy ? "解锁中…" : "解锁"}
          </button>
        </form>
        {error && <p className="error">{error}</p>}
        <p className="hint">
          私钥以 Argon2id 加密存放本机 · 忘记口令无法恢复
        </p>
        <div className="ulinks">
          <button
            type="button"
            className="linkbtn"
            onClick={() => setRelayOpen(true)}
          >
            Relay 设置
          </button>
          <button
            type="button"
            className="linkbtn"
            onClick={() => setNetOpen(true)}
          >
            网络 / 代理设置
          </button>
        </div>
      </div>
      {relayOpen && <RelayModal onClose={() => setRelayOpen(false)} />}
      {netOpen && <NetworkModal onClose={() => setNetOpen(false)} />}
    </div>
  );
}

export default function App() {
  const state = useStore((s) => s.sessionState);
  const identity = useStore((s) => s.identity);
  const setSessionState = useStore((s) => s.setSessionState);
  const setIdentity = useStore((s) => s.setIdentity);
  // Latched "does an account exist" bit. Lets us keep the right surface
  // mounted across a self-induced restart's transient states (down/restarting/
  // starting), which carry no account info — see the routing note below.
  // Driven off the CONCRETE session states rather than hello frames: hello
  // fires only once at startup, but `init`/`join` flip no-account→unlocked
  // client-side with no fresh hello, so a hello-only latch would stay stale
  // `false` for the whole session an identity was just created in.
  const [hasAccount, setHasAccount] = useState<boolean | null>(null);
  useEffect(() => {
    if (state === "unlocked" || state === "locked") setHasAccount(true);
    else if (state === "no-account") setHasAccount(false);
    // transient states (starting/down/restarting) carry no signal → keep latch
  }, [state]);

  // Rust boots the notification toggle to "on" and has no localStorage of
  // its own — push the persisted preference across once per app launch,
  // independent of session state (locked/starting is fine; this command
  // doesn't touch the sidecar).
  useEffect(() => {
    syncNotifyPrefOnStartup();
  }, []);

  useEffect(() => {
    // Keep the unlisten PROMISES: StrictMode's mount→unmount→mount runs the
    // cleanup before an awaited value would be assigned, which leaks the
    // first listener pair (doubled events in dev). Unsubscribing through the
    // promise is correct in every ordering.
    const statusP = onSessionStatus((p) => {
      useStore.getState().setSessionState(p.state);
    });
    const eventP = onSessionEvent((ev) => {
      useStore.getState().applyEvent(ev);
    });
    void (async () => {
      const fetched = (await sessionStatus()) as SessionState;
      // The hello status event may already have delivered a concrete state;
      // never clobber it with a stale snapshot.
      if (useStore.getState().sessionState === "starting") {
        setSessionState(fetched);
      }
    })();
    return () => {
      void statusP.then((u) => u());
      void eventP.then((u) => u());
    };
  }, [setSessionState]);

  // Post-unlock hydration: identity (if the unlock receipt was missed, e.g.
  // a hot-reload) and the conversation list.
  useEffect(() => {
    if (state !== "unlocked") return;
    void (async () => {
      if (!identity) {
        const me = await whoami();
        setIdentity({ label: me.label, npub: me.npub });
      }
      await useStore.getState().hydrate();
    })();
  }, [state, identity, setIdentity]);

  // Relay sockets can drop silently while unlocked (sleep, Wi-Fi change):
  // the sidecar runs a 2 s pump but no MDK auto-reconnect, and its docs say
  // the GUI re-drives it with `catchup`. Do that when the OS reports
  // connectivity back or the window returns to the foreground, at most once
  // per 10 s. Failures are already surfaced as sync_error events by the pump.
  useEffect(() => {
    if (state !== "unlocked") return;
    const trigger = makeCatchupTrigger(() => {
      void catchup().catch((e: unknown) => console.warn("catchup failed", e));
    }, 10_000);
    const onOnline = () => {
      trigger();
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") trigger();
    };
    // WKWebView (macOS) does not always flip visibilityState on app switch
    // and rarely fires `online`; window focus is the signal that does fire.
    const onFocus = () => {
      trigger();
    };
    window.addEventListener("online", onOnline);
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.removeEventListener("online", onOnline);
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [state]);

  // Onboarding renders for the no-account state — but a self-induced sidecar
  // restart (e.g. saving network settings from the onboarding-hosted panel)
  // briefly passes through down/restarting/starting, which carry no account
  // info. Without this, App would swap in <UnlockCard/>, unmounting Onboarding
  // and wiping the in-progress passphrase/nsec the retain-on-failure fix is
  // meant to preserve. Keep Onboarding mounted across that window whenever the
  // latched account bit says none exists; the statusbar already shows "重连中".
  const transient =
    state === "starting" || state === "down" || state === "restarting";
  const showOnboarding =
    state === "no-account" || (transient && hasAccount === false);

  return (
    <main className="root">
      <header className="statusbar" data-state={state}>
        <span className="dot" />
        moyu · {STATE_LABEL[state]}
      </header>
      {state === "unlocked" ? (
        <Shell />
      ) : showOnboarding ? (
        <Onboarding />
      ) : (
        <UnlockCard state={state} />
      )}
    </main>
  );
}
