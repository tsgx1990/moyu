// Shared string-formatting helpers used across the desktop UI: normalizing
// caught errors, and truncating hex/npub identifiers for display. Centralized
// so every call site formats the same class of value identically instead of
// re-deriving the same one-liner.

/** Normalize a caught value into a human-readable message — the
 * `catch (e) { ... e instanceof Error ? e.message : String(e) ... }` shape
 * repeated across every async action in the unlocked shell. */
export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** First `n` characters of a hex-ish identifier (message id, sha256, npub),
 * no ellipsis appended — callers that want a truncation marker append "…"
 * themselves, since some show the head only and others head+tail. */
export function shortHex(s: string, n = 8): string {
  return s.slice(0, n);
}

/** Head+tail truncation for a bech32 npub (`npub1abc…wxyz`), used for
 * sidebar/label display where a head-only prefix risks visually colliding
 * similar-looking identifiers. */
export function shortNpub(npub?: string): string {
  return npub ? `${npub.slice(0, 10)}…${npub.slice(-4)}` : "unknown";
}
