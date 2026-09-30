# moyu

An end-to-end-encrypted CLI chat tool for programmers, built on **MLS
(RFC 9420) over Nostr** — the Marmot protocol — using
[MDK (Marmot Development Kit)](https://github.com/marmot-protocol/mdk) (MIT)
as its cryptographic + transport engine. Forward secrecy and post-compromise
security from day one.

中文说明:[`README.zh-CN.md`](README.zh-CN.md) · 使用指南:[`docs/user-guide.zh-CN.md`](docs/user-guide.zh-CN.md)

The threat model is [`docs/threat-model.md`](docs/threat-model.md); a Chinese
user guide is [`docs/user-guide.zh-CN.md`](docs/user-guide.zh-CN.md).

## Status

**Maintenance mode.** moyu gets security fixes and MDK upgrades; no new
features are planned. Upstream MDK ships its own CLI and TUI (`wn`) that
covers the same chat core and is where new protocol work lands first. What
moyu adds on top of MDK is narrower: SOCKS5 proxying for relays, NIP-05
lookups and attachments (through a small [MDK fork](https://github.com/tsgx1990/mdk)),
a workspace / channel / invite-code / approval model, a write-policy plugin
for a self-hosted strfry relay, a desktop GUI, and Chinese documentation. It
is a one-person project with no third-party security audit — read
[Known limitations](#known-limitations-honest-about-whats-not-there-yet)
before relying on it.

**0.2.0 — production-readiness release.** Feature-wise moyu has been complete
since 2026-07: 1:1 and multi-member workspace E2EE chat, private channels, a
programmable surface (`--json` / stdin pipes / streaming `recv`), encrypted
attachments, reactions / replies / search, bot & CI integration events,
one-command `invite` / `join`, and a cross-platform **desktop app** (Tauri 2
GUI driving the same CLI via a `moyu session` sidecar) — see
[Desktop app (GUI)](#desktop-app-gui). 0.2.0 is about being safe to run:

- dependency tree clean of RustSec advisories, gated by `cargo deny` on every
  change (0.1.0 shipped 20 of them);
- MDK v0.9.20 (2026-09-08) on Rust 1.97.1;
- the real MLS-over-Nostr round-trip suite runs in CI on every PR, the desktop
  shell has unit tests;
- no hosted relay and no placeholder domain anywhere — you pick public relays
  or [run your own](docs/self-host-relay.md);
- a [security policy](SECURITY.md) and a written
  [threat model](docs/threat-model.md).

[`CHANGELOG.md`](CHANGELOG.md) has the details.

## Install

moyu is **not on crates.io**. `cargo install moyu` or `cargo install
moyu-cli` would install an unrelated crate (or whatever someone registers
under that name later) — use one of the channels below, or build from this
repository.

Prebuilt binaries — no Rust toolchain needed. Platforms: macOS (Apple
Silicon + Intel), Linux (x86_64 + arm64, glibc) and Windows (x64/MSVC, since
`cli/v0.2.0`). The installed binary is `moyu` (the package/formula is named
`moyu-cli`).

**Homebrew**

```sh
brew install tsgx1990/moyu/moyu-cli
```

**Shell (curl | sh)**

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/tsgx1990/homebrew-moyu/releases/latest/download/moyu-cli-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/tsgx1990/homebrew-moyu/releases/latest/download/moyu-cli-installer.ps1 | iex"
```

**Manual** — download `moyu-cli-<target>.tar.xz` (`.zip` on Windows) for your platform from
[Releases](https://github.com/tsgx1990/homebrew-moyu/releases), unpack, and
`chmod +x moyu`. If you downloaded it with a **browser** on macOS, clear the
quarantine flag first (Homebrew and curl|sh installs don't need this):

```sh
xattr -dr com.apple.quarantine ./moyu
```

**桌面版(GUI)** — Tauri 2 壳,内嵌上面这个 CLI 作为 sidecar,与独立安装的
CLI 共享同一身份/数据目录。官方安装包从 **`app-v0.2.0`** 起发布在同一个
[Releases](https://github.com/tsgx1990/homebrew-moyu/releases) 页(macOS `.dmg`
×2 / Linux `.deb`&`.AppImage` / Windows NSIS,tag 前缀 `app-v*`);目前**未签名**,
首次打开的绕行步骤见 [`docs/desktop-install.md`](docs/desktop-install.md)。
想自己出包或跑开发模式:

```sh
node scripts/package-desktop.mjs        # 免签名打包当前平台(或 apps/desktop 下 pnpm package)
# 开发模式:cd apps/desktop && pnpm install && pnpm tauri dev
```

v1 不做代码签名(macOS 产物带免账号的 ad-hoc 签名),从别处下载的安装包会被
系统拦一下(Gatekeeper / SmartScreen)——绕行步骤、本地打包细节和后续签名计划
见 [`docs/desktop-install.md`](docs/desktop-install.md)。

## Build from source (contributors)

Regular users should install a prebuilt binary (see **Install** above). Building
from source needs the Rust 1.97 toolchain and network access to fetch the pinned
MDK git dependency:

```sh
# Rust 1.97.1 is pinned via rust-toolchain.toml (matches MDK's toolchain).
cargo build --workspace
cargo test  --workspace
```

## Commands

```
# identity & keys
moyu init [--import-nsec nsec1…] [--relay-preset public]
                                              create/import identity; publish relay list + first KeyPackage
moyu whoami                                   print the active account's label + npub
moyu keypackage publish | rotate              (re)publish / force-rotate the KeyPackage (kind 30443)

# contacts & 1:1 chat
moyu add <npub|hex|name@domain> [--label L]   remember a contact
moyu chat <peer>                              open (creating if needed) a 1:1 chat and enter the REPL
moyu send <peer> <text> [--file PATH]         one-shot send; --file attaches an E2EE file (text = caption)
moyu recv [--follow]                          accept pending invites, sync, print inbound; --follow keeps polling
moyu tui                                      full-screen chat UI (ratatui)

# workspaces & channels
moyu workspace new|list|add|members|rename|kick|admin|leave
moyu channel   new|new-private|invite|list|rename|archive
moyu post <ws> <channel> [text] [--file PATH] post into a channel (omit text / pass `-` to read stdin)

# invites & joining (see Quickstart)
moyu invite [<workspace>] [--auto-approve | --revoke]
moyu join <code>
moyu requests | approve <npub|all> [--workspace <ws>] | deny <npub>

# reading & reacting
moyu conversations                            every DM + workspace/channel tree in one list
moyu history <group> [--before CURSOR] [--limit N]
                                              one group's local history, newest page first (offline)
moyu search <substring>                       offline grep over all already-decrypted history
moyu react <group> <message_id> [emoji] [--remove]
moyu reply <group> <message_id> [text]
moyu download <group> <hash> [--out PATH]     fetch + decrypt an attachment by its ciphertext_sha256

# bots & automation
moyu op <ws> <ch> [text] --type T --status S  🤖 structured bot OPERATION (kind-1202): CI/deploy/git/monitor
moyu activity <ws> <ch> <text>                🤖 lightweight bot ACTIVITY line (kind-1201)
moyu session                                  long-lived stdio JSONL loop (what the desktop GUI drives)

# relays
moyu relay list | add <url> | forget <url>    audit / extend / prune the persisted relay set
```

Every one-shot command also takes `--json` (see **Scripts & bots** below);
`moyu <cmd> --help` has the full story for each.

Global flags: `--data-dir <path>`, `--relay <url>` (repeatable; defaults to a
few public relays), and `--dev-allow-loopback` (**dev/testing only** — required
to point `--relay` at a `ws://127.0.0.1:…` local relay; a loopback relay is
refused up front without it). `MOYU_PASSPHRASE` supplies the at-rest passphrase
non-interactively for scripts/CI; otherwise it is read with a hidden (no-echo)
prompt.

`--relay` no longer needs repeating on every command: `init` and `join` now
persist the relay set to `<data-dir>/config.json`, and later commands fall
back to it when `--relay` is omitted (an explicit `--relay` still overrides
for that one call, it just doesn't get written back).

## Quickstart(和朋友 30 秒试)

Two commands, two people:

```sh
# A: has a workspace "eng" and is its admin
$ moyu invite eng
moyuinv1qqs…                     # <- send this string to B (any channel: chat, email, carrier pigeon)

# B: may not even have a moyu identity yet
$ moyu join "moyuinv1qqs…"
# join sets up relays from the code, inits an identity + publishes a
# KeyPackage if B doesn't have one yet, then sends A a join request.

# A: approve it
$ moyu requests                  # lists who's waiting: npub, workspace, ✓ trusted / ⚠ uncredentialed
$ moyu approve all               # (or `moyu approve <npub>` for just one)
```

B's next `moyu recv` picks them up inside `eng`, ready for `moyu chat`/`moyu tui`.
Add `--auto-approve` to `invite eng` to skip the manual `approve` step (A must
still be `eng`'s admin — a non-admin's `--auto-approve` is refused up front,
not silently dropped).

A workspace code is a bearer token: anyone who holds it can ask to join, and
with `--auto-approve` they are let in. It stops working 7 days after it is
issued, `moyu invite eng --revoke` cancels every code issued for `eng`, and
someone you removed from the workspace is never let back in by a code — only
by your explicit `moyu approve`.

No workspace, just a 1:1 contact? Drop the slug:

```sh
$ moyu invite                     # A: prints a plain contact invite code
$ moyu join "<code>"              # B: joins, DMs A a hello
$ moyu recv                       # A: sees B's hello, auto-added as a contact
```

The bare `moyuinv1…` string is the canonical form. If a chat client turned it
into a link, `moyu join` also accepts any link that carries the code after a
`#` (the fragment never leaves your machine in an HTTP request).

**Still needs a relay.** `invite`/`join` kill the *manual* relay bookkeeping
(the code carries the relay set and `join` persists it, see above), but
reachability still depends on which relay set you're on — see below.

### Relays: public by default, self-host when it matters

moyu operates no relay of its own. `moyu init` lets you pick where your
traffic goes:

- **Public relays** (`--relay-preset public`, the built-in default) —
  decentralized (damus / nos.lol / primal). Free to drop the Marmot event kinds
  moyu depends on, so a message can occasionally go missing.
- **Custom / self-host** (`--relay wss://...`) — point at your own relay.
  [docs/self-host-relay.md](docs/self-host-relay.md) stands one up in a few
  minutes (strfry + moyu's Marmot-only write policy) and explains exactly what
  a relay operator can and cannot see.

Non-interactive `init` (no TTY, or any relay flag) never prompts. An invite
code carries the inviter's relay, so `moyu join <code>` puts both parties on the
same relay automatically.

## Usage guide

### First run

```sh
moyu init                      # interactive: pick a relay set, choose a passphrase
moyu init --import-nsec        # or bring an existing Nostr identity (hidden prompt)
```

`init` generates (or imports) your Nostr key, encrypts it at rest
(Argon2id + XChaCha20-Poly1305) behind the passphrase you choose, publishes
your relay list and first KeyPackage, and persists the relay set so you don't
repeat `--relay` on every command. Scripts/CI can pass the passphrase via
`MOYU_PASSPHRASE` instead of the hidden prompt. `--import-nsec` with no value
prompts for the key without echo, or reads one line from stdin when piped
(`pass show nostr | moyu init --import-nsec`). Writing the key inline
(`--import-nsec nsec1…`) still works but puts it in your shell history and
process list — avoid it.

Everything lives in one per-OS data directory (identity, encrypted MLS state,
local message store) — **backing it up is backing up your identity; it never
belongs in a repo**:

| OS | default `--data-dir` |
|---|---|
| macOS | `~/Library/Application Support/chat.moyu.moyu` |
| Linux | `~/.local/share/moyu` |
| Windows | `%APPDATA%\moyu\moyu\data` |

Separate identities = separate `--data-dir`s (the e2e script does exactly this).

### Daily chat

```sh
moyu add alice@nostr.example --label alice   # or an npub1… / hex pubkey
moyu chat alice                              # REPL; or `moyu tui` for the full-screen UI
moyu send alice "review my PR?"              # one-shot, script-friendly
moyu send alice "the diff" --file ./pr.patch # E2EE attachment (text becomes the caption)
moyu recv --follow                           # live-tail everything inbound
```

Attachments are encrypted client-side and uploaded to a Blossom blob server
(`--blossom <URL>` to override; the server only ever sees ciphertext). The
recipient downloads by content hash:
`moyu download <group> <ciphertext_sha256> --out ./`.

### Workspaces (persistent groups with channels)

```sh
moyu workspace new eng                       # a fresh MLS group with just you
moyu channel new eng backend                 # public channel
moyu channel new-private eng sec alice bob   # private channel = its own nested MLS group,
                                             #   invisible to non-invited workspace members
moyu post eng backend "deploy at 3?"         # or: ci-log | moyu post eng backend -
moyu invite eng --auto-approve               # onboard someone (see Quickstart)
moyu workspace members eng                   # who's in, admins badged
moyu workspace admin add eng alice           # governance is admin-gated
moyu workspace leave eng --transfer-to alice # sole admin must hand off first
```

Kicks and admin changes are MLS group state: other members' clients reject
one an admin did not make. Renames and archives are group messages every
member's client folds into the same view; the CLI only lets an admin send
them, but that part is a client-side rule, not a cryptographic one (see the
[threat model](docs/threat-model.md)). Every client shows a governance notice
for each change.

### Reading, searching, replying

```sh
moyu conversations                    # sidebar-in-a-command: DMs + workspace/channel tree
moyu history eng --limit 20           # newest page; page back with --before <next_cursor>
moyu search "deploy"                  # offline substring grep over all decrypted history
moyu reply eng 4b37… "yes, 3pm"       # threaded reply (quotes the parent)
moyu react eng 4b37… 👍               # kind-7 reaction; --remove retracts yours
```

`<group>` anywhere above is a workspace name/prefix **or** a group-id
hex/prefix — exactly what `recv --json` / `conversations --json` print.

### Scripts & bots

Every command takes `--json` and prints exactly one machine-readable object
(or JSONL stream for `recv`), each carrying a schema version `"v"`; failures
are `{"ok":false,"error":…}` + non-zero exit. For long-lived automation,
`moyu session` turns the CLI into a framed stdio command/event loop (JSON
commands in, JSONL receipts + async events out — the desktop app is literally
a customer of this protocol). Bot-flavored output belongs to `op`/`activity`
(kind-1201/1202), which render with a 🤖 marker instead of impersonating a
human — see **Integrations** below.

### Network & keys

- **Relays**: persisted at `init`/`join` into `<data-dir>/config.json`;
  `moyu relay list` audits, `moyu relay add <url>` extends (e.g. your own
  relay), `moyu relay forget <url>` prunes. Presets: see
  [Relays](#relays-public-by-default-self-host-when-it-matters) above.
- **Behind a proxy**: `--socks5 127.0.0.1:1080` routes all relay traffic,
  NIP-05 lookups and attachment transfers through a SOCKS5 proxy (a local
  Tor, `ssh -D`, or any other SOCKS5 endpoint).
- **KeyPackage hygiene**: auto-rotated proactively (~every 60 days, under the
  spec's 84-day cap) whenever you run normal commands; `moyu keypackage
  rotate` forces it (e.g. after restoring a backup).

## Desktop app (GUI)

`apps/desktop` is a Tauri 2 shell (React + TypeScript) over the **same** CLI:
it spawns the bundled `moyu` binary as a `moyu session` sidecar and drives it
over framed stdio JSONL — the GUI has no crypto code of its own, the webview
has zero shell permissions, and file paths/credentials never cross the
webview boundary. It shares the OS data directory with a separately-installed
CLI, so both front ends see the same identity and history (unlock with the
same passphrase).

What it covers today (parity with the CLI): onboarding (create / import /
join-by-invite-code), DM + workspace/channel chat with markdown (sanitized —
message bodies are untrusted E2EE content), optimistic send/retry, threaded
replies, reactions, encrypted attachments (picker + drag-and-drop, inline
image preview, reveal-in-file-manager), join-request approval, full workspace
/ channel governance, KeyPackage rotation, client-side search (sidebar filter
+ in-conversation Cmd/Ctrl+F), unread badges, and focus-gated OS
notifications.

Run it:

```sh
node scripts/package-desktop.mjs   # one-click unsigned package for this platform
cd apps/desktop && pnpm install && pnpm tauri dev   # or: dev mode
```

Official installers ship from the `app-v*` releases (first: `app-v0.2.0`);
they are unsigned — Gatekeeper / SmartScreen notes live in
[`docs/desktop-install.md`](docs/desktop-install.md).

## Integrations — pipe CI, git & alerts into an E2EE channel

moyu is a first-class Unix citizen: any tool that can run a command can push
into an **end-to-end-encrypted** channel, arriving marked as a **bot**
(kind-1201/1202 agent events) rather than impersonating a human.

```
$ ci-summary | moyu op eng ci --type ci --status failed --name build --fail "3 tests failed"
# a teammate's `moyu recv --follow` (decrypted on their device):
[a1b2c3d4] 🤖 ci-bot #ci [ci·failed] build: 3 tests failed ✗ (48.2s)
```

Copy-paste recipes (GitHub Actions, a git `post-receive` hook, a generic
webhook bridge) live in **[`examples/integrations/`](./examples/integrations/)**.

## Try it locally

```sh
# 1. run any nostr relay locally, e.g. nostr-rs-relay on ws://127.0.0.1:7777
# 2. drive a full two-party round-trip (uses --dev-allow-loopback under the hood):
bash scripts/e2e-local.sh            # RELAY=… / MOYU_BIN=… to override
```

Manual two-account flow: `moyu --data-dir A init` and `moyu --data-dir B init`,
then `moyu --data-dir A send <B-npub> "hi"` followed by
`moyu --data-dir B recv`.

## Security

- **Report vulnerabilities privately** — see [`SECURITY.md`](SECURITY.md)
  (GitHub private vulnerability reporting on this repository; 7-day
  acknowledgement, disclosure within 14 days of the fix).
- **What is and isn't protected** is written down in
  [`docs/threat-model.md`](docs/threat-model.md): MLS gives confidentiality,
  forward secrecy and post-compromise security for message content; a relay
  operator still sees connection metadata, which is why self-hosting and
  `--socks5` exist.
- **Supply chain**: `deny.toml` + the `security` workflow run `cargo deny`
  (advisories / SPDX license allowlist / dependency sources / bans) on every
  PR and weekly, over both the CLI workspace and the desktop shell.

## Known limitations (honest about what's not there yet)

- **Proxy is explicit; no embedded Tor.** Pass `--socks5 <IP:PORT>` (global, e.g.
  `--socks5 127.0.0.1:1080` pointing at a local Tor or `ssh -D`) and moyu
  routes **all relay connections, NIP-05 lookups *and* attachment transfers**
  through that SOCKS5 proxy (host names are resolved by the proxy, so no DNS
  leak), so it works on networks where relays are not directly reachable.
  Two things it deliberately does
  *not* do: auto-honor `HTTP(S)_PROXY` / `ALL_PROXY` env vars (you pass
  `--socks5` explicitly), and bundle its own Tor (point `--socks5` at an
  external one). Without `--socks5` everything connects directly.
- **Loopback relays need `--dev-allow-loopback`.** MDK refuses to dial a
  non-public relay host by default (an anti-SSRF guard against relay lists
  other people publish). The flag stays available in release builds on
  purpose: a self-hosted relay reached through an `ssh -L` tunnel *is* a
  loopback address. It is a global, explicit opt-in and is never inferred.
- **Single device.** No multi-device yet — MDK's multi-device is unimplemented
  upstream (MIP-06 pending). A new device cannot read history from before it
  joined (an inherent MLS forward-secrecy cost, surfaced honestly to users).
- **MDK is young and unaudited.** No third-party security audit of MDK or
  moyu; the wire format and fork-convergence subsystem still change across
  releases (MDK's DB schema migrations are forward-only — back up your data
  directory before upgrading). The rev is pinned deliberately — bump it only
  against MDK's CHANGELOG, never blindly.
- **`moyu chat` shows one conversation.** The 1:1 REPL prints only its own
  DM live; traffic for other groups that arrives meanwhile is still stored and
  shows up in `history`, `recv` and the `tui` (which keeps every chat visible
  at once). Two peers who both `send` before either has processed the other's
  Welcome still found two separate groups (invite glare, no rendezvous);
  MDK's convergence recovers superseded invitations since 0.9.20 but moyu has
  no test pinning that path yet.

## Directory structure

```
moyu/
├── Cargo.toml                    workspace root; MDK deps pinned to a git rev (see its comment)
├── rust-toolchain.toml           pins Rust 1.97.1 (matches MDK)
├── deny.toml                     cargo-deny policy: advisories, license allowlist, sources, bans
├── .cargo/config.toml            Windows /STACK:8MiB link-arg — moyu.exe is unrunnable without it
├── .github/workflows/            ci (fmt/clippy/test ×2 OS, desktop, e2e), security, releases
├── SECURITY.md                   how to report a vulnerability, what is in scope
├── CONTRIBUTING.md               build gates, testing rules, what must never be committed
├── CHANGELOG.md                  Keep-a-Changelog release notes
├── scripts/
│   ├── e2e-local.sh              two-identity MLS round-trip over a local relay
│   ├── privacy-check.sh          secret + personal-data scan (pre-commit hook and CI)
│   ├── build-sidecar.mjs         builds moyu-cli and stages it as the Tauri sidecar
│   └── package-desktop.mjs       one-click unsigned desktop package (current platform)
├── docs/
│   ├── desktop-install.md        desktop install & unsigned-build notes (Gatekeeper/SmartScreen)
│   ├── self-host-relay.md        run your own Marmot-only relay (strfry + write policy)
│   ├── threat-model.md           assets, adversaries, guarantees, and what is NOT protected
│   ├── release.md                release runbook (Chinese)
│   └── mdk-api-map.md            exact MDK call sites for every core-loop step
├── apps/
│   └── desktop/                  Tauri 2 GUI shell (React 19 + TS + Vite; its own cargo workspace)
│       ├── src/                  front end: store, Shell / Onboarding / Modals / Message
│       └── src-tauri/            Rust bridge: sidecar supervisor + session client,
│                                 dedicated credential commands, attachment token broker
└── crates/
    ├── moyu-core/                headless engine library: MoyuEngine over MDK, identity
    │                             (Argon2id nsec-at-rest), invites, workspace/channel
    │                             projection, governance, transport/loopback guard, local store
    ├── moyu-cli/                 the `moyu` binary: clap commands (main.rs), print-free
    │                             command cores shared by CLI & session (ops.rs), shared
    │                             domain layer (domain/), `moyu session` stdio loop
    │                             (session/), ratatui TUI (tui.rs), chat REPL (repl.rs)
    └── moyu-relay-policy/        strfry write-policy plugin for a self-hosted Marmot relay
```

## What's real vs. what's a placeholder, at a glance

| Area | Status |
|---|---|
| Nostr identity generation/import, npub/nsec encoding | Working |
| Argon2id + XChaCha20-Poly1305 nsec-at-rest encryption | Real primitives (`sqlcipher_kdf.rs`) behind a real `AccountSecretStore` (`identity.rs`) |
| SQLCipher key derivation for MLS/session state | Automatic inside `marmot-app` — not moyu's problem |
| Account creation + initial KeyPackage publish | Working (`MarmotAppRuntime::create_or_import_account`) |
| KeyPackage publish / force-rotation | Working (`AppClient::publish_key_package` / `rotate_key_package`) |
| KeyPackage 84-day lifecycle | Working — auto-rotated proactively (~60d, under the spec's 84d + 1h max) on normal commands; `keypackage publish\|rotate` remain manual |
| 1:1 group creation (founding creation with invitees) | Working — one `create_group` call does KeyPackage fetch + create + Welcome publish |
| Sending / receiving / decrypting messages | Working, proven end-to-end both directions (local + public relay) |
| Accepting an invite as the invitee | Working (`moyu recv` → `AppClient::accept_group_invite`) |
| ratatui TUI | Working (`moyu tui`) — chat list incl. workspace channels with every chat live at once, history backfilled on open, member roster (Ctrl-R), bot and join-request lines; group management / search / settings are CLI- and desktop-only |
| `moyu session` stdio protocol | Working — the full command surface over framed JSONL (receipts + async events), hermetic integration tests; the desktop app is its first customer |
| Desktop app (Tauri GUI) | Working at CLI parity (onboarding → chat → attachments → governance → search → notifications); official installers since `app-v0.2.0` (unsigned, see `docs/desktop-install.md`), or build locally via `scripts/package-desktop.mjs` |
| Windows | CLI + desktop compile and pass the full fmt/clippy/test gate in CI (windows-2022); prebuilt CLI (`.zip` + PowerShell installer) since `cli/v0.2.0`, desktop NSIS installer since `app-v0.2.0` |
| SOCKS5 proxy (`--socks5`: relays + NIP-05 + attachments) | Working — host names resolved by the proxy (no DNS leak); no `HTTP_PROXY` env autodetect and no embedded Tor (point `--socks5` at an external one) |
| Hosted relay | None, by design — public relays by default, or [self-host](docs/self-host-relay.md); no placeholder domain anywhere in the binary |
| Supply chain | `cargo deny` gate (advisories / licenses / sources / bans) on every PR + weekly; tree clean of RustSec advisories as of 0.2.0 |
| Outbox routing, multi-device | Not started — multi-device is blocked upstream in MDK (see above); outbox routing unscheduled |

## License / dependency discipline

moyu is **MIT** (see [`LICENSE`](LICENSE)), matching MDK's own licensing;
third-party notices are in [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
Nothing from `whitenoise-rs` (AGPLv3) was used for implementation — only MDK
(MIT) and the `nostr` crate (MIT) — so the whole tree stays MIT. `cargo deny`
enforces an SPDX license allowlist over every dependency and rejects the GPL
family. MDK has no third-party security audit yet and its fork-convergence
subsystem is still actively being fixed; the git-rev pin exists precisely so
upgrades are deliberate.
