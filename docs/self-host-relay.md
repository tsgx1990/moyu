# Self-host a relay for moyu

moyu does **not** operate a relay. Out of the box it talks to a few public
Nostr relays (`wss://relay.damus.io`, `wss://nos.lol`, `wss://relay.primal.net`),
which is the decentralized default — but public relays are free to drop the
event kinds Marmot needs (KeyPackages, group messages, gift-wraps), and when
they do, "invite a friend" silently fails. Running your own relay is the
first-class fix: it takes one small VPS, and every moyu client you invite from
it lands on the same relay automatically (invite codes carry the relay set).

This guide stands up [strfry](https://github.com/hoytech/strfry) with moyu's
write-policy plugin so the relay carries *only* Marmot traffic.

## What the relay operator can and cannot see

A relay is untrusted infrastructure in moyu's threat model
(`docs/threat-model.md`); running one yourself just means *you* are the party
that sees the metadata.

- **Can see:** which pubkey connects, when, from which IP, and which Nostr
  event *kind* is published (KeyPackage / group message / gift-wrap / relay
  list). Group-message (445) and gift-wrap (1059) events are signed with
  per-event ephemeral keys, so they do not carry the sender's long-term
  identity.
- **Cannot see:** message content, group membership, display names, or any
  plaintext — everything is MLS end-to-end encrypted and decryptable only by
  group members.

## 1. Install strfry

Follow strfry's `docs/DEPLOYMENT.md` (build deps → `make` → binary at
`/usr/local/bin/strfry`). Copy this repo's `infra/relay/strfry.conf` to
`/etc/strfry.conf` and `mkdir -p /var/lib/strfry/db`.

## 2. Build and install the write-policy plugin

`moyu-relay-policy` is a strfry `writePolicy` plugin (line-delimited JSON over
stdin/stdout). It accepts only the five Marmot kinds — `30443` KeyPackage,
`445` group message, `1059` gift-wrap, `10002` NIP-65 relay list, `10050`
inbox relay list — and rate-limits them: identity kinds per pubkey *and* per
client network (a pubkey costs nothing to mint; an IPv6 client is grouped by
its /56 here), and every event per client address (IPv6 by /64). Each of the
limiter's three tables has a hard size limit, so memory cannot be grown
without bound. A full per-address table turns away addresses it does not
track yet until entries expire; a full pubkey table lets events through
without per-pubkey tracking, so minting keys cannot lock new identities out.

On a box with the pinned Rust toolchain (`rust-toolchain.toml` in this repo):

```sh
cargo build --release -p moyu-relay-policy
install -m 0755 target/release/moyu-relay-policy /usr/local/bin/moyu-relay-policy
```

`infra/relay/strfry.conf` already points `relay.writePolicy.plugin` at that
path.

## 3. TLS via Caddy

Install Caddy, copy `infra/relay/Caddyfile` to `/etc/caddy/Caddyfile`, replace
`relay.example.com` with your hostname, `systemctl reload caddy`. Caddy obtains
a Let's Encrypt certificate and proxies `wss://` to `127.0.0.1:7777`.

Caddy MUST forward the real client IP via `X-Forwarded-For` (it does by
default): `strfry.conf` sets `realIpHeader = "X-Forwarded-For"` so the plugin's
per-IP rate limiting sees real clients instead of `127.0.0.1`. strfry binds
loopback only, so the header cannot be spoofed from outside.

## 4. Run strfry

Run `strfry relay` under systemd (strfry's docs include a unit file). Confirm:

```sh
curl -H 'Accept: application/nostr+json' https://relay.example.com   # NIP-11 info
```

## 5. Retention

Kinds 445 and 1059 are the only stored kinds that grow. Keep a 30–90 day
window (45 days is a sane start) with a daily cron:

```sh
strfry delete --age 3888000 --filter '{"kinds":[445,1059]}'   # 45 days
```

Addressable kinds (30443 / 10002 / 10050) are replaceable — strfry keeps only
the newest per `(pubkey, kind[, d-tag])` automatically.

## 6. Before you invite anyone

- Fill `info.pubkey` and `info.contact` in `strfry.conf` (NIP-11; blank by
  default).
- Verify strfry's `maxWebsocketPayloadSize` ≥ `events.maxEventSize` (131072)
  so near-maximum events are not dropped at the websocket frame layer.
- Alert at 80% of the data volume; if it fires, lower the retention age or
  resize.

## 7. Point moyu at it

```sh
moyu init --relay wss://relay.example.com          # fresh identity on your relay
moyu invite eng                                     # the code carries the relay
```

Anyone who runs `moyu join <code>` gets your relay merged into their set. To
add it to an existing identity: `moyu relay add wss://relay.example.com`; to
drop it again: `moyu relay forget wss://relay.example.com`. If the relay is
only reachable through a proxy, use the global `--socks5 IP:PORT` flag — it
applies to relay traffic, NIP-05 lookups and attachment transfers alike.
