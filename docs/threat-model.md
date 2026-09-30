# moyu threat model

What moyu protects, against whom, and — just as important — what it does not.
This is the reference the [security policy](../SECURITY.md) scopes reports
against. It describes moyu 0.2.x; the cryptographic core is
[MDK](https://github.com/marmot-protocol/mdk) (Marmot protocol = MLS
RFC 9420 over Nostr), and this document does not restate MLS's own analysis.

## Assets

| Asset | Where it lives | Protection |
|---|---|---|
| Long-term identity key (Nostr `nsec`) | `<data-dir>/…` encrypted at rest | Argon2id-derived key, XChaCha20-Poly1305; the passphrase is prompted, never on argv (the optional `--import-nsec` value is — see below); `Zeroizing` on every in-memory path |
| MLS group state (ratchet trees, epoch secrets) | SQLCipher database managed by MDK | Encrypted with a key MDK derives (HKDF-SHA256) from the identity key and a per-database random salt — so it is exactly as protected as the encrypted identity key above |
| Message plaintext | Only inside a member's process and its local encrypted store | End-to-end encrypted on the wire (MLS application messages, kind 445) |
| Group membership, channel names, workspace names | Inside MLS application data / group context | Encrypted; a relay sees only ciphertext |
| Contacts, issued invites, the record of handled join requests | `moyu-state.json` (plain JSON, private file mode) | Not encrypted at rest — see "Not protected" |
| Attachments | Encrypted blobs on a Blossom server; plaintext cached locally after download | Encrypted with a per-file key carried inside the MLS message |

## Adversaries

1. **Relay operator** (any relay in your set — public or the one you self-host).
2. **Network observer** between you and the relay (ISP, censor, coffee-shop Wi-Fi).
3. **Another group member**, current or former.
4. **Someone with your device** — stolen laptop, shared machine.
5. **Malicious message content** rendered by the desktop app (XSS-style).

## Guarantees

- **Confidentiality and integrity of messages** against adversaries 1–2 and
  against former members: MLS gives forward secrecy (a compromised key does
  not reveal past messages) and post-compromise security (the group heals
  after a member's key is rotated). A removed member cannot read messages
  from epochs after their removal.
- **Sender identity is not on the wire.** Group messages (kind 445) and
  Welcome gift-wraps (kind 1059) are signed with per-event ephemeral keys; a
  relay cannot link a group message to a long-term identity.
- **Invites cannot be replayed across workspaces, and do not live for
  ever.** A workspace invite carries a bearer secret bound to that workspace;
  `approve` checks it, and a code issued by a non-admin is refused up front.
  A code stops being trusted or auto-approved 7 days after it is issued
  (judged by when the request reaches the inviter's device, not by the
  requester's own timestamp), and `moyu invite <workspace> --revoke` cancels
  every code issued for a workspace at once.
- **A join request is acted on once, and removal sticks.** A request is closed
  as soon as its sender is added, removed or leaves, so an old request cannot
  put someone back after they were kicked or left. Someone who was removed is
  never re-admitted by an `--auto-approve` code, even a still-valid one —
  that takes an admin's explicit `approve`. Anyone else holding a valid
  auto-approve code can join until it expires or is revoked: treat the code
  like a password. Two limits: this is decided from the inviter's own
  device's records, so an admin who joined the workspace *after* someone was
  removed does not know about that removal, and a code that admin issues
  would let the removed person back in; and `moyu approve <npub>` refuses to
  guess when one person has open requests for more than one workspace
  (`--workspace` picks one).
- **At rest, the identity and MLS state are unreadable without the
  passphrase.** Argon2id parameters are chosen so a wrong-passphrase attempt
  costs noticeable CPU; the session sidecar backs off on repeated failures.
- **The desktop webview has no shell, file-system, or navigation
  capability.** Message bodies go through sanitize → highlight before
  rendering; external links open in the system browser; paths and secrets
  never cross the generic JS bridge (dedicated Rust commands wrap the
  passphrase in `Zeroizing`).
- **CLI/TUI human output can't be turned into a terminal exploit by another
  member.** Message bodies, sender/workspace/channel names, invite/relay
  strings, and MDK error text all originate from adversary 3 (or a relay) and
  are treated as untrusted before they reach a human terminal: every control
  character except `\n`/`\t` (C0 incl. ESC/BS, C1 incl. CSI/OSC), Unicode bidi
  controls (RLO/LRO, the LRI/RLI/FSI/PDI isolates, …), and the interlinear/tag
  format-character ranges are replaced with U+FFFD (`moyu-cli::output::term_safe`,
  reached through the `hprintln!`/`heprintln!` macros) — this is what stops an
  OSC 52 clipboard write, a fake OSC 8 hyperlink, a cursor-hiding escape, or a
  bidi override that visually reorders a line. `--json` output
  (`output::emit*`) and the `session` JSONL stream the desktop sidecar reads
  are exempt and stay byte-lossless — they are machine contracts, never
  rendered by a terminal. The `tui` front end applies the same bidi/tag
  filtering at the points remote text enters its model, on top of (not
  instead of) ratatui's own control-character filtering.
- **Relay traffic, NIP-05 lookups and Blossom attachment transfers honor
  `--socks5 IP:PORT`** with no direct-dial fallback. On the proxied path the
  Blossom host *name* is handed to the proxy (`socks5h`), so no local DNS
  lookup reveals the media host either. This is a moyu-fork change to MDK
  (upstream builds every media client `no_proxy`); the trade-off is that the
  origin-address vetting MDK performs for direct media connections is
  delegated to the proxy you configured — URL-level validation still runs.
- **`--import-nsec` reads the key out-of-band.** With no value the flag
  prompts without echo (or reads one line from stdin when piped), so the
  secret never sits in argv, `ps` or shell history; the in-process copy is
  zeroized. The inline `--import-nsec <nsec>` form is still accepted for
  scripts but discouraged (see "Not protected").
- **A multi-line message body cannot impersonate a following line.** Human
  renderers indent every continuation line of a body, so a body containing
  `\n[group] alice…: …` still reads as one message.

## Not protected (by design or not yet)

- **Metadata at the relay.** A relay operator (adversary 1) sees which pubkey
  connects, when, from which IP, and which event *kinds* it publishes or
  subscribes to. KeyPackages (kind 30443) and relay lists (10002/10050) are
  signed by your long-term key by design — that is how others find you. The
  mitigation is choosing your relay (`docs/self-host-relay.md`) and, on a
  hostile network, a SOCKS5/Tor proxy for the IP.
- **Channel metadata is not admin-enforced.** Who is in a workspace, who is
  an admin, and who is in a private channel are MLS group state: other
  members' clients reject a change an admin did not make. Channel names, the
  workspace's display name and the "archived" flag are different — they are
  ordinary group messages that every member's client folds into the same
  view. The CLI and desktop app only let an admin send them, but a modified
  client used by a current member (adversary 3) can send them too: rename the
  workspace or a channel, archive a channel (archiving cannot be undone), or
  pin a name with a timestamp no honest rename can beat. Such a member cannot
  read a private channel they are not in, add or remove anyone, or make
  themselves an admin. The number of channels is capped at 256 (so the list
  cannot be grown without bound), which also means such a member can use the
  cap up. The remedy today is removing that member. Enforcing admin-only metadata needs an authenticated
  per-epoch admin history that moyu does not keep yet.
- **An invite code adds relays to your configuration.** `moyu join <code>`
  merges the code's relay list into your persisted relay set, so that relay
  sees your pubkey, IP and timing from then on (the same metadata any relay
  in your set sees). The CLI's `join` prints which relays it added; `moyu relay list` /
  `moyu relay forget <url>` audit and remove them. Only join with a code from
  someone you are willing to share a relay with.
- **`--import-nsec <nsec>` with an inline value** still puts the secret on the
  command line (`ps`, shell history). Use the bare `--import-nsec` (prompt) or
  pipe it on stdin instead; the inline form exists only for scripts that
  already hold the key in a variable.
- **Other MDK HTTP clients moyu does not use** (profile-image fetch, the
  directory "open ranking" lookup, audit-log upload) are outside the SOCKS5
  path; moyu never triggers them (no profile images, no user search, no audit
  endpoint configured), so nothing leaves the host through them.
- **Traffic analysis** by adversary 2: timing and sizes of encrypted events.
- **Availability.** Public relays may drop Marmot event kinds or rate-limit
  you; moyu retries but cannot force delivery. Self-hosting is the answer.
- **A compromised or unlocked device** (adversary 4 with the session open,
  or malware with your privileges) sees what you see. Locking the session
  (`lock` in the desktop app, exiting the CLI) drops the engine and zeroizes
  the derived secret; the encrypted files remain and need the passphrase.
- **`moyu-state.json` is not encrypted.** It holds contact labels, npubs,
  issued-invite records (workspace ids and bearer secrets) and the record of
  which join requests were handled and who was removed. It is written with
  private file permissions only. Encrypting it is tracked as future work.
- **Downloaded attachments are cached in plaintext** under the app data
  directory until you delete them.
- **Multi-device** is not supported (the Marmot MIP-06 draft is not adopted
  yet). A new device cannot read history from before it joined — an inherent
  MLS forward-secrecy cost.
- **Third-party audit:** neither moyu nor MDK has had one. MDK's wire format
  and fork-convergence logic still change between releases; moyu pins an
  exact revision and upgrades deliberately.

## Trust boundaries

```
 you ──passphrase──▶ moyu (CLI or desktop sidecar) ──MLS ciphertext──▶ relay(s) ──▶ other members
        │                    │                                          ▲
        │                    └─ encrypted store (Argon2id/XChaCha20 + SQLCipher)   metadata visible here
        └─ desktop webview ◀── framed JSON over stdin/stdout ── (no secrets, no paths)
```

- Between the desktop webview and the Rust shell: only session commands and
  events; passphrase / `nsec` / file paths use dedicated Rust commands.
- Between moyu and MDK: MDK owns the MLS state and the relay client; moyu
  owns identity encryption, the local store, and all UI.
- Between moyu and the relay: everything that leaves the process is either
  an MLS ciphertext, a gift-wrap, or a signed public record (KeyPackage /
  relay list).

## Dependency and supply-chain posture

- `cargo deny` (RustSec advisories, SPDX license allowlist, dependency
  sources, bans) gates every change and runs weekly; the tree is currently
  free of known vulnerabilities.
- MDK is pinned to an exact git revision of a moyu-maintained fork
  (`tsgx1990/mdk`, upstream v0.9.20 plus a SOCKS5 relay-proxy patch);
  OpenMLS is pinned by MDK to an `erskingardner/openmls` revision pending an
  upstream fix. Both sources are on the `deny.toml` allowlist; nothing else
  may be a git dependency.
- Release binaries are built from a tagged commit by GitHub Actions
  (cargo-dist / tauri-action) on GitHub-hosted runners, for every platform,
  and published with SHA-256 sums. The repository has no self-hosted
  runners. Every action in the release workflows is pinned to a commit SHA,
  and release builds do not restore a build cache. Builds are not
  reproducible yet, so a binary cannot be checked against the source by
  rebuilding it. Desktop builds are currently **unsigned** (macOS Gatekeeper
  / Windows SmartScreen will warn; see `docs/desktop-install.md`).
- Releases are published in this repository with the workflow's own
  short-lived `GITHUB_TOKEN` (0.2.0 and earlier were published in
  `tsgx1990/homebrew-moyu` and remain there). The only long-lived token is
  `HOMEBREW_TAP_TOKEN`, a personal access token that can write to the
  Homebrew tap repository; only the job that commits the formula holds it,
  and that job runs no project code. Pull requests never reach a job that
  can write anywhere (fork pull requests get no secrets at all, and the
  release workflows only run on tags).
- The desktop release builds with a read-only token and creates the draft
  release in a separate job that runs no project code. The CLI release,
  generated by cargo-dist, does not separate them yet: its build jobs run
  with a token that can write to this repository in their environment while
  they compile every dependency's build script, so a malicious crate that
  made it into the lockfile could use that token during a tagged release.

### Known unfixed advisories

None with a vulnerability classification as of 2026-09-30. Transitive crates
flagged *unmaintained* by RustSec (`instant`, `paste`, `proc-macro-error2`,
`nostr-relay-pool` 0.44 — all pulled in by MDK / hax / nostr-sdk) are
reported as warnings; replacing them is upstream's call and tracked in
`marmot-protocol/mdk#1116`.
