# Changelog

All notable changes to moyu are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
SemVer while the project is pre-1.0 (minor bumps may break).

## [Unreleased]

### Security

- **A member who was removed from a workspace could be put back
  automatically.** `recv`, `tui` and the desktop app sweep the message
  history for join requests and auto-approve the ones made with an
  `--auto-approve` invite. The only test for "is this request still open?"
  was "is the sender a member right now?" — so after an admin kicked someone
  who had joined that way, their original request looked open again and the
  next sweep re-added them, with no action on their part. The same happened
  to someone who left on their own. A join request is now acted on at most
  once: it is closed as soon as its sender is added, removed or leaves
  (judged from the membership records MDK itself writes from the MLS commits
  — a membership event merely *sent* by a member is ignored — plus a local
  record of what this account already approved; "earlier" and "later" are
  the local message store's own order, never a timestamp a sender chose),
  and anyone who was ever removed from a workspace is never re-admitted by an
  auto-approve code — only by an explicit `moyu approve`. `workspace kick`
  records the removal.
- **`moyu approve <npub>` no longer grants more than you looked at.** A join
  request names its own target workspace, so someone could ask to join the
  workspace they were invited to *and* another one you administer;
  `approve <npub>` acted on the request it happened to find. It now refuses
  when one person has open requests for more than one workspace and asks for
  `--workspace <name>`.
- **Workspace invite codes expire, and can be revoked.** A code was valid for
  ever: anyone who ever saw an `--auto-approve` code could join at any later
  time. Codes now stop being trusted or auto-approved 7 days after they are
  issued, measured by when the request reaches the inviter's device (a
  requester cannot backdate that), and `moyu invite <workspace> --revoke`
  cancels every code issued for a workspace. **Behavior change:** codes
  issued by 0.2.0 carry no expiry and are treated as expiring 7 days after
  they were issued, so an old code may already be dead after upgrading.
- **`moyu-state.json` no longer loses writes between two running moyu
  processes.** Every write re-reads the file first and replaces it
  atomically, so a long-running `session` / `tui` / `recv --follow` no longer
  overwrites an invite or a contact that a one-shot command recorded in the
  meantime (previously the invite silently stopped being trusted), and it
  re-reads the file each sync so a code revoked in another terminal stops
  being honored at once.
- **`moyu join` says which relays an invite code added** to your persisted
  relay set, and how to remove them (`moyu relay forget`). Those relays see
  your pubkey, IP and timing from then on.
- **Relay write-policy plugin (`moyu-relay-policy`):** identity events
  (KeyPackages, relay lists) were rate-limited per pubkey only, and a pubkey
  costs nothing to mint — one client could publish without limit. They are
  now also limited per client network (IPv6 grouped by /56), every event
  counts against a per-address limit (IPv6 by /64), and each of the limiter's
  tables has a hard size limit instead of growing without bound; a full
  pubkey table lets events through untracked rather than locking new
  identities out.
- **The channel list of a workspace is capped at 256.** Channel events are
  ordinary group messages any member can send; a member can no longer make
  every other member's client hold an unbounded list. What a non-admin member
  can still do to channel names is documented in `docs/threat-model.md`
  ("Channel metadata is not admin-enforced").
- **`examples/integrations/github-actions.yml` installed moyu with
  `cargo install moyu-cli`.** moyu is not on crates.io and that name is free
  for anyone to register, so the example would have run somebody else's code
  with the bot's key in its environment. It now installs a pinned release
  archive verified against a SHA-256 in the file, and no longer pastes
  `${{ github.* }}` values into the shell script (a branch name could inject
  commands).
- Release workflows: every action is pinned to a commit SHA, release builds
  do not restore a build cache, and `ci.yml` runs with a read-only token.
- Piped `--import-nsec`: the read buffer is pre-sized (a growing buffer left
  un-wiped copies of a key that arrived in several reads), and surrounding
  whitespace is trimmed in place.
- Test fixtures no longer contain real people's Nostr public keys.
- **Attachments now go through `--socks5` too.** The MDK fork routes every
  Blossom media transfer (attachment upload/download, group and profile
  images) through the configured SOCKS5 proxy, handing the host name to the
  proxy (`socks5h`) so no local DNS lookup reveals the media host. 0.2.0
  documented this as a known gap (upstream builds all media clients
  `no_proxy`; its own test pins that). Verified with a SOCKS5 handshake stub
  in the fork's test suite and by running the local E2E suite (including the
  attachment round-trip) through a logging SOCKS5 proxy.
- **`--import-nsec` no longer needs the secret on the command line.** With no
  value the flag prompts without echo, or reads one line from stdin when
  piped; the inline form is still accepted for scripts but discouraged. The
  value is wrapped in `Zeroizing` as soon as it is read (the desktop's
  session `init` path now does the same).
- **Multi-line message bodies can no longer spoof a following line** in the
  human `recv` / `history` / `search` / `chat` output: continuation lines of a
  body are indented.
- **CLI/TUI human output is now sanitized against terminal escape and
  Unicode bidi injection from other group members.** Message bodies, sender
  / workspace / channel names, invite/relay strings, and MDK error text all
  printed straight to the terminal with no filtering, so a malicious group
  member could plant an ANSI/OSC escape (retitle the window, an OSC 52
  clipboard write, a fake OSC 8 hyperlink, cursor tricks to hide text) or a
  Unicode bidi override (visually reordering a rendered line) in anything
  `moyu-cli` rendered. Every human print now routes through
  `output::term_safe` (via new `hprintln!`/`heprintln!`/`heprint!` macros),
  which drops `\r`, replaces every other control character except `\n`/`\t`
  with U+FFFD, and replaces Unicode bidi controls and the tag/annotation
  ranges with U+FFFD, while preserving CJK, emoji (incl. ZWJ sequences), and
  Persian/Indic text (ZWNJ). A crate-wide
  `#![deny(clippy::print_stdout, clippy::print_stderr)]` fails the build on
  any future raw `println!`/`eprintln!`/`print!`/`eprint!`. `--json` output
  and the `session` JSONL stream the desktop sidecar reads are unaffected
  and stay byte-lossless (they never went through a raw print). The `tui`
  front end additionally sanitizes remote text at the points it enters the
  model (message/sender text, contact labels, workspace names), on top of
  ratatui's existing control-character filtering, which does not cover bidi
  overrides.

### Changed

- **The source repository is public** at <https://github.com/tsgx1990/moyu>.
  Report vulnerabilities through its private advisory form (see
  `SECURITY.md`). A `scripts/privacy-check.sh` gate (gitleaks plus
  repository-specific rules) runs as a pre-commit hook and in CI.
- **Releases move to the source repository, and the Homebrew tap is
  discontinued.** From this version on, the CLI archives, the installer
  scripts and the desktop installers are published in
  <https://github.com/tsgx1990/moyu/releases>, and the `curl | sh` and
  PowerShell one-liners download from there. The old distribution
  repository `tsgx1990/homebrew-moyu`, which held 0.2.0 and earlier and the
  Homebrew formula, will be removed. If you installed with Homebrew, run
  `brew uninstall moyu-cli && brew untap tsgx1990/moyu`, then use the
  `curl | sh` installer; your identity and data directory are not touched.
- The desktop release builds with a read-only token and creates the draft
  release in a separate job that runs no project code, so a compromised npm
  dependency can no longer reach a token that can write to a repository.
- moyu is in maintenance mode: security fixes and MDK upgrades, no new
  features planned (README, "Status").
- **MDK upgraded to v0.9.20** (from v0.9.16; upstream 2026-09-08). The fork
  branch is now `tsgx1990/mdk` `moyu/relay-proxy-0.9.20` (v0.9.20 plus the
  unchanged 3-commit SOCKS5 relay-proxy patch, applied without conflicts);
  Rust 1.97.1, `nostr` 0.44.8 and `rusqlite` 0.40.1 are unchanged. **Upgrade
  note:** MDK account databases advance through migrations 57–67 and the
  shared store to schema 3; migrations are forward-only, so back up the data
  directory before running the new binary and do not downgrade afterwards.
  0.9.20 also introduces opt-in usage diagnostics (Aptabase / OTLP relay
  telemetry) behind an explicit consent; moyu never grants it, and a
  moyu-core test now pins every exporter disabled on a fresh data directory.
- Dependabot runs monthly and no longer auto-rebases open PRs on every push
  to `main` (each rebase re-ran the full CI matrix).
- A `tui` or `chat` session left open re-checks KeyPackage freshness every
  6 hours (previously only at command start, so a weeks-long session could
  let the published KeyPackage age past the rotation threshold).
- MDK fork: `cargo test -p marmot-app` compiles again (two upstream tests
  added after the SOCKS5 patch called its widened `full_history_with_loopback`
  with the old arity).
- CI skips docs-only changes (`**.md`, `docs/**`, `LICENSE*`), and the
  `cargo deny` gate runs on PR / main pushes only when a `Cargo.toml`,
  `Cargo.lock` or `deny.toml` changed (the weekly scan is unchanged).

## [0.2.0] - 2026-09-02

The "production-readiness" release: no behavior a user would call a feature,
but everything that separated a working demo from something safe to run.

### Security

- **Removed every reference to `moyu.chat`.** The domain is registered to an
  unrelated third party; 0.1.0 shipped it as the opt-in "moyu Cloud" relay
  preset and as the base of `invite --url` links, which would have handed
  connection metadata to whoever controls that host. moyu operates no relay.
- **Dependency tree: 20 RustSec advisories → 0.** Upgrading MDK (see below)
  brought the resolved `nostr` from 0.44.4 to 0.44.8 (eight advisories: NIP-44/NIP-04 remote
  DoS, NIP-46/NIP-60 credentials in `Debug` output, …), `nostr-relay-pool`
  0.44.1 → 0.44.3 (unverified relay events processed, signature-verification
  cache poisoning, auth-challenge memory exhaustion) and dropped
  `libcrux-aesgcm` (two advisories with no fixed version) from the graph.
- New supply-chain gate: `cargo deny` (advisories, SPDX license allowlist,
  dependency sources, bans) runs on every PR/push and weekly, over both the
  CLI workspace and the desktop shell.

### Changed

- **MDK upgraded to v0.9.16** (from a 2026-07-07 snapshot) on Rust 1.97.1.
  The SOCKS5 relay-proxy patch moyu depends on remains a fork
  (`tsgx1990/mdk`, branch `moyu/relay-proxy-0.9.16`); upstream closed the
  corresponding PR without merging.
- `moyu init` no longer offers a hosted preset. The interactive prompt is
  "1) public relays 2) custom / self-host"; `--relay-preset` accepts only
  `public`. `init` now prints where the persisted relay set came from.
- The desktop invite dialog shares the bare `moyuinv1…` code instead of a
  link.

### Fixed

- `moyu join` from a fresh data directory (which creates the identity
  inline) could fail with "marmot account session is already in use" on a
  slow machine: MDK's account setup leaves a runtime worker holding the
  account session for a moment after it returns, which raced moyu's own
  client. moyu now waits (bounded) for that hand-off instead of failing.
- Messages that MDK marks as *invalidated* after fork recovery (the losing
  branch's rows, kept only as tombstones) are no longer rendered as normal
  messages; moyu now hides them at its single read boundary, matching other
  Marmot clients.
- Desktop: relay sync is re-driven when the OS comes back online or the
  window regains focus (the session protocol always expected this).
- Desktop: a panic while holding the attachments lock no longer cascades into
  every later attachment call.

### Added

- `moyu relay add <url>` (and the session command `relay_add`): attach a
  relay — typically your own — to an existing identity without re-running
  `init`. Same loopback guard as every other command.
- `docs/self-host-relay.md`: run a Marmot-only relay (strfry + moyu's write
  policy + Caddy) in a few minutes, including what an operator can and
  cannot see.
- `SECURITY.md` and `docs/threat-model.md`.
- The real-protocol e2e suite (`scripts/e2e-local.sh`) runs in CI on every
  PR against a `nostr-rs-relay` service container; the desktop shell gained
  its first unit tests (vitest); Dependabot watches cargo, npm, and Actions.

### Documented limitations (pre-existing, now written down)

- With `--socks5`, attachment uploads/downloads still connect to the Blossom
  server directly — MDK's media client is built with `no_proxy()`. Relay
  traffic and NIP-05 lookups are proxied as before.
- `--import-nsec` passes the secret on the command line (visible to `ps` and
  shell history).

### Removed

- `invite --url` and the `url` field of the invite receipt. The bare code is
  canonical; `join` still accepts any link that carries a code after `#`.
- `docs/moyu-cloud-privacy.md` (superseded by the self-host guide).

### Upgrade notes

- **Back up your data directory before upgrading** (`moyu --data-dir` /
  the default under your OS config dir). MDK's database schema migrations are
  forward-only: a 0.2.0 binary upgrades a 0.1.0 store in place and 0.1.0
  cannot read it afterwards.
- If your saved relay set contains `wss://relay.moyu.chat`, remove it:
  `moyu relay forget wss://relay.moyu.chat`.
- Building from source now needs Rust 1.97.1 (`rust-toolchain.toml`).

## [0.1.0] - 2026-07-10

First prebuilt release: 1:1 and workspace E2EE chat, private channels,
`--json` / stdin / streaming `recv`, encrypted attachments, reactions /
replies / search, bot events, invite / join onboarding, Homebrew + `curl | sh`
installers.
