// Shared async-action state for the busy/err UI shape repeated across the
// unlocked shell's modals and forms: a boolean "in flight" flag, a last-error
// message, cleared on every new attempt, reset in a `finally`. Only wire this
// up where busy really is ONE boolean for the whole action — a per-row/
// per-key busy (WorkspaceModal's member-row mutations, RelayModal's per-url
// forget, RequestsModal's per-request approve/deny) needs its own keyed
// state instead and must not be forced into this shape.
import { useState } from "react";
import { errMsg } from "./format";

export function useAsync(): {
  busy: boolean;
  err: string | null;
  setErr: (err: string | null) => void;
  run: (fn: () => Promise<void>) => Promise<void>;
} {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  async function run(fn: () => Promise<void>): Promise<void> {
    setBusy(true);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setBusy(false);
    }
  }

  return { busy, err, setErr, run };
}
