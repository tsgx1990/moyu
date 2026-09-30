# Security policy

moyu is an end-to-end-encrypted chat client (MLS over Nostr, via the Marmot
protocol / MDK). This document says which versions get fixes, how to report a
vulnerability, what happens next, and what is out of scope. The threat model —
what moyu does and does not protect against — lives in
[`docs/threat-model.md`](docs/threat-model.md).

## Supported versions

| Version | Supported |
|---|---|
| 0.2.x (current) | yes |
| 0.1.x | no — upgrade; 0.1.0 shipped with 20 known RustSec advisories in its dependency tree |

Only the latest minor release receives fixes.

## Reporting a vulnerability

Please **do not** open a public issue for anything security-sensitive.

Use GitHub's private vulnerability reporting on this repository:
<https://github.com/tsgx1990/moyu/security/advisories/new>. It reaches the
maintainer directly and keeps the report private until a fix is out.

Include what you can of: affected version (`moyu --version`), platform, a
minimal reproduction, and your assessment of impact. Reports about
dependencies (MDK, OpenMLS, rust-nostr) are welcome too — if the fix belongs
upstream we will coordinate with them.

## What to expect

- **Acknowledgement within 7 days.** You will hear back with an initial
  severity assessment and whether the issue is confirmed.
- **Fix and release** as fast as severity warrants; critical issues in the
  encryption or key-handling path take priority over everything else.
- **Public disclosure no later than 14 days after the fixed release**, in the
  CHANGELOG and the GitHub advisory, with credit to the reporter unless they
  prefer otherwise.
- If a report turns out to be a duplicate or out of scope, you will be told
  why.

## Scope

In scope — anything that breaks a guarantee the threat model makes:

- plaintext or key material reaching a relay, disk, log, clipboard, or another
  process without the user's intent;
- a member who was removed from a group still being able to read or send;
- forged or replayed messages/invites being accepted as genuine;
- the passphrase / `nsec` import paths, on-disk encryption
  (Argon2id + XChaCha20-Poly1305, SQLCipher), or the session sidecar bridge
  leaking secrets;
- the desktop webview gaining a capability it is not granted (shell, file
  system, arbitrary navigation), including via rendered message content.

Out of scope — known limitations, not vulnerabilities:

- **Metadata visible to a relay operator** (who connects, when, from which
  IP, which event kinds). This is inherent to the Nostr transport; the threat
  model documents it and `docs/self-host-relay.md` is the mitigation.
- A current workspace member renaming or archiving channels, or renaming the
  workspace, without being an admin. Channel metadata is not admin-enforced;
  the threat model says why ("Channel metadata is not admin-enforced").
  Anything that lets a non-admin change *membership* or *admin rights* is in
  scope.
- A device that is already compromised (root/admin malware, physical access
  to an unlocked session).
- Denial of service against public relays, or a relay dropping events.
- Vulnerabilities in the operating system, terminal emulator, or webview
  runtime.
- Findings from automated scanners without a demonstrated impact.

## Known unfixed items

Tracked in `docs/threat-model.md` under "Known unfixed advisories"; today the
dependency tree is clean of RustSec vulnerabilities (`cargo deny check` runs on
every change and weekly).
