// The desktop-notification on/off switch. The preference itself lives
// ONLY in localStorage (a plain UI setting, not session state — it must
// survive lock/unlock and even a supervisor restart, neither of which touch
// the webview). Rust actually gates the OS notification (see src-tauri's
// `NotifyEnabled` + session.rs's `maybe_notify`), so every change here also
// pushes the value across via the dedicated `set_notify` command — a plain
// Tauri command, NOT the generic `session_request` bridge, since it has
// nothing to do with the sidecar session and must work in every session
// state (including locked/starting).
import { invoke } from "@tauri-apps/api/core";

const KEY = "moyu.notify.enabled";

/** Read the persisted preference; defaults to enabled (unset key). */
export function getNotifyPref(): boolean {
  try {
    const raw = localStorage.getItem(KEY);
    return raw === null ? true : raw === "1";
  } catch {
    // Storage disabled/unavailable — fall back to the default without
    // crashing the settings UI over it.
    return true;
  }
}

/** Persist the preference and push it to the Rust side immediately. */
export function setNotifyPref(enabled: boolean): void {
  try {
    localStorage.setItem(KEY, enabled ? "1" : "0");
  } catch {
    // Best-effort persistence; the Rust-side push below still applies for
    // the running session even if it can't survive a restart.
  }
  void invoke("set_notify", { enabled }).catch(() => {
    // A failed toggle just leaves Rust on its previous value; nothing in the
    // UI depends on this succeeding synchronously.
  });
}

/** Rust always boots with notifications enabled and has no localStorage of
 * its own — call this once at startup to re-apply a previously saved "off". */
export function syncNotifyPrefOnStartup(): void {
  void invoke("set_notify", { enabled: getNotifyPref() }).catch(() => {});
}
