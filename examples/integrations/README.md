# moyu integrations — pipe CI, git & alerts into an E2EE channel

This is what makes moyu different from an "encrypted Slack": it's a **first-class
Unix citizen**. Any tool that can run a command can push into an
**end-to-end-encrypted** channel — and it arrives marked as a **bot**, not
impersonating a human.

```
$ ci-summary | moyu op eng ci \
    --type ci --status failed --name "build+test" \
    --run-id 1287 --fail --duration-ms 48200 "3 tests failed on main"

🤖 op [ci·failed] → #ci (published 1 event(s)).
```

A teammate running `moyu recv --follow` (or the `moyu tui`) sees, decrypted on
their own device:

```
[a1b2c3d4] 🤖 ci-bot #ci [ci·failed] build+test: 3 tests failed on main ✗ (48.2s)
```

In the TUI that line is a **red 🤖 card** (green ✓ on success, magenta for a
plain status). Nobody's relay ever saw the plaintext — it's MLS the whole way.

## Why a bot, not just `moyu post`

`moyu op` / `moyu activity` emit Marmot **agent events** (kind-1202 / kind-1201),
so a reader — human or another bot — can tell automation apart from a person at
the protocol level. `op` is a **structured operation** (CI, deploy, git push, a
monitoring alert) with `--type`, `--status`, `--ok/--fail`, `--duration-ms`,
`--run-id`, `--details`; `activity` is a lighter "bot said X" line.

Everything is scriptable and pipeable:

- read the body from **stdin** (`ci-summary | moyu op …`) or as an argument,
- `--json` on `op`/`activity` for a machine-readable receipt,
- `moyu recv --json` streams **every** event (chat, reaction, `agent_op`,
  `agent_activity`) as one JSON object per line, so a downstream bot can react.

```
$ moyu recv --json | jq -c 'select(.event=="agent_op" and .ok==false)'
{"v":1,"event":"agent_op","channel":"ci","kind":1202,"event_type":"ci",
 "status":"failed","name":"build+test","run_id":"1287","ok":false,
 "duration_ms":48200,"text":"3 tests failed on main","message_id":"…"}
```

## One-time setup: give the bot an identity

A bot is just a moyu account that happens to be a member of your workspace.

```bash
# 1. Create the bot's identity (its own keypair, encrypted at rest).
MOYU_PASSPHRASE=… moyu --data-dir /srv/moyu-bot init

# 2. From a human account that admins the workspace, add the bot:
moyu workspace add eng <the-bot's-npub>

# 3. The bot accepts the invite once, then it can post forever:
MOYU_PASSPHRASE=… moyu --data-dir /srv/moyu-bot recv
```

Then wherever the bot runs (CI, a git server, a cron box), give it:

- `MOYU_PASSPHRASE` — the bot identity's passphrase (a CI/deploy **secret**),
- `--data-dir` — the bot's own moyu data directory,
- `--relay wss://<your-relay>` — the relay the workspace uses.

> `MOYU_PASSPHRASE` is a convenience for automation; env vars are readable via
> `ps -E`/`/proc`, so scope the secret to the job and rotate it like any other.

## The recipes

| File | Turns this… | …into a 🤖 event |
|---|---|---|
| [`github-actions.yml`](./github-actions.yml) | a GitHub Actions job result | `--type ci --status <job.status>` |
| [`git-post-receive.sh`](./git-post-receive.sh) | a `git push` to a server repo | `--type git-push --name <branch>` |
| [`webhook-bridge.sh`](./webhook-bridge.sh) | any JSON webhook / alert (stdin) | `--type <source> --status <state>` |

All three are ~15 lines of shell — copy, set three env vars, done. That's the
pitch: **the integration surface is your shell**, not a plugin marketplace.

## Notes / limits (v1)

- The bot posts as itself (an MLS member); there's no separate "app token" model
  yet — the identity IS the account. Keep the bot's nsec/passphrase as secrets.
- A bot event addressed to a **public** channel records its channel slug inside
  the event (`details.moyu.ch`); a **private** channel is its own MLS group, so
  just target it by slug and only its members can read it.
- Inbound webhooks need something to *run* `moyu` (CI step, git hook, a small
  cron/`socat` bridge). moyu is the sink, not an HTTP server — by design.
