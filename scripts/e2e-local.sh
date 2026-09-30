#!/usr/bin/env bash
# End-to-end local test: two throwaway identities (alice, bob), a local
# nostr-rs-relay, and moyu's non-interactive `send`/`recv` commands, proving a
# 1:1 MLS-over-Nostr message round-trips and decrypts on both sides. Also
# exercises the M1 multi-channel workspace CLI: alice creates a workspace +
# channel, invites bob (who catches up via a `workspace.snapshot` control
# event), and a channel post round-trips both ways. Finally it exercises the
# M2 workspace-governance CLI with a third identity (carol): admin add/remove/
# list + member badges, kick (carol removed), and the headline creator-leave —
# alice, the sole admin, hands admin to bob via `leave --transfer-to` and leaves
# (the M1 "creator cannot leave" boundary is now closed). Also exercises the
# frictionless-onboarding CLI (`invite`/`join`/`requests`/`approve`) with a
# fresh set of throwaway identities: a workspace invite code trusted-badged
# and approved, a 1:1 contact invite's hello, a `--auto-approve` code that
# self-heals a joiner in with no explicit `approve`, and a non-admin's
# `--auto-approve` invite being refused up front.
#
# Usage:
#   ./scripts/e2e-local.sh
#   RELAY=ws://127.0.0.1:7777 ./scripts/e2e-local.sh   # override the relay
#   MOYU_BIN=/path/to/moyu ./scripts/e2e-local.sh      # use a pre-built binary
#
# Safe to re-run: every run gets fresh `mktemp -d` data directories (never
# inside the repo -- see CLAUDE.md's "运行时数据...绝不提交").

set -uo pipefail

RELAY="${RELAY:-ws://127.0.0.1:7777}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Portable temp paths: GNU mktemp requires a template ending in XXXXXX while
# BSD/macOS mktemp accepts a bare `-t prefix` -- so pass an explicit template
# that both accept. (CI's first run died here: `mktemp -d -t moyu-e2e-alice`
# printed "too few X's" on ubuntu and every data dir came back empty.)
tmpdir() { mktemp -d "${TMPDIR:-/tmp}/moyu-e2e-$1.XXXXXX"; }
tmpfile() { mktemp "${TMPDIR:-/tmp}/moyu-e2e-$1.XXXXXX"; }
PASSPHRASE="testpass"

# Optional SOCKS5 proxy for the whole run, e.g.
#   SOCKS5=127.0.0.1:1080 ./scripts/e2e-local.sh
# (a local Tor or `ssh -D` SOCKS port). Empty = dial relays directly.
SOCKS5_ARGS=()
[ -n "${SOCKS5:-}" ] && SOCKS5_ARGS=(--socks5 "$SOCKS5")

ALICE_MSG="hello from alice over MLS"
BOB_MSG="hello back from bob over MLS"

pass=1
fail() {
    echo "FAIL: $*" >&2
    pass=0
}

echo "== moyu E2E: 1:1 MLS message round-trip over $RELAY =="

# --- 0. relay reachability ---------------------------------------------------
if ! curl -s -m 5 -H "Accept: application/nostr+json" "${RELAY/ws:/http:}" >/dev/null 2>&1 \
    && ! curl -s -m 5 -H "Accept: application/nostr+json" "${RELAY/wss:/https:}" >/dev/null 2>&1; then
    echo "WARN: could not reach relay info endpoint for $RELAY (continuing anyway; the relay may still accept websocket connections)" >&2
fi

# --- 1. build (unless caller supplied a binary) ------------------------------
if [ -z "${MOYU_BIN:-}" ]; then
    echo "-- building moyu-cli (debug) --"
    (cd "$REPO_ROOT" && cargo build -p moyu-cli) || { fail "cargo build failed"; exit 1; }
    MOYU_BIN="$REPO_ROOT/target/debug/moyu"
fi
echo "using binary: $MOYU_BIN"

# --- 2. fresh temp data dirs (never under the repo) --------------------------
ALICE_DIR="$(tmpdir alice)"
BOB_DIR="$(tmpdir bob)"
CAROL_DIR="$(tmpdir carol)"
echo "alice data dir: $ALICE_DIR"
echo "bob   data dir: $BOB_DIR"
echo "carol data dir: $CAROL_DIR"

cleanup() {
    rm -rf "$ALICE_DIR" "$BOB_DIR" "$CAROL_DIR" \
        ${BLOSSOM_STORE:+"$BLOSSOM_STORE"} ${ATTACH_DIR:+"$ATTACH_DIR"} ${DL_DIR:+"$DL_DIR"} \
        ${DAN_DIR:+"$DAN_DIR"} ${ERIN_DIR:+"$ERIN_DIR"} ${FAY_DIR:+"$FAY_DIR"} \
        ${GUS_DIR:+"$GUS_DIR"} ${HAL_DIR:+"$HAL_DIR"}
    # Kill the local Blossom mock if it was started.
    [ -n "${BLOSSOM_PID:-}" ] && kill "$BLOSSOM_PID" 2>/dev/null
    return 0
}
trap cleanup EXIT

# `${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"}` (not the plainer `"${SOCKS5_ARGS[@]}"`)
# is required for `set -u` compatibility with bash 3.2 (macOS's system
# /bin/bash, still the default `env bash` on an unmodified Mac): that bash
# treats indexing a zero-element array with `[@]` as an unset-variable error
# under `nounset`, even though the array itself is declared. This idiom tests
# "is the array set" first, so it works for both the empty (no --socks5) and
# populated case, on both bash 3.2 and modern bash.
alice() { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$ALICE_DIR" --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
bob()   { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$BOB_DIR"   --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
carol() { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$CAROL_DIR" --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }

# `drain <member-fn>`: sync a member a few times so it applies any backlog of
# MLS commits (a real client syncs repeatedly via `recv --follow`; a single
# one-shot `recv` can race relay propagation of a chain of governance commits).
# Governance commits are applied incrementally, so we also keep each member's
# backlog small (well under MDK's ~5-commit rewind window) by draining between
# groups of changes rather than only at the very end.
drain() {
    local who="$1"
    for _ in 1 2 3 4; do "$who" recv >/dev/null 2>&1 || true; sleep 1; done
}

# `drain_capture <member-fn>`: like `drain`, but prints the concatenated recv
# output of every poll so a caller can assert a `[governance]` notice fired on
# some poll (the notice is printed exactly once, on whichever poll applies the
# convergence commit -- capturing all polls makes the assertion race-free).
drain_capture() {
    local who="$1" acc=""
    for _ in 1 2 3 4; do
        acc+="$("$who" recv 2>&1 || true)"$'\n'
        sleep 1
    done
    printf '%s' "$acc"
}

# --- 3. init both identities --------------------------------------------------
echo "-- alice: init --"
alice_init_out="$(alice init 2>&1)"
echo "$alice_init_out"
alice_npub="$(printf '%s\n' "$alice_init_out" | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$alice_npub" ] || { fail "could not parse alice's npub from init output"; exit 1; }

echo "-- bob: init --"
bob_init_out="$(bob init 2>&1)"
echo "$bob_init_out"
bob_npub="$(printf '%s\n' "$bob_init_out" | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$bob_npub" ] || { fail "could not parse bob's npub from init output"; exit 1; }

echo "-- carol: init --"
carol_init_out="$(carol init 2>&1)"
echo "$carol_init_out"
carol_npub="$(printf '%s\n' "$carol_init_out" | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$carol_npub" ] || { fail "could not parse carol's npub from init output"; exit 1; }

echo "alice npub: $alice_npub"
echo "bob   npub: $bob_npub"
echo "carol npub: $carol_npub"

# KeyPackages were just published to the relay; give the relay a moment to
# make them fetchable before alice tries to resolve bob's.
sleep 2

# --- 4. alice -> bob ----------------------------------------------------------
echo "-- alice: send to bob --"
alice_send_out="$(alice send "$bob_npub" "$ALICE_MSG" 2>&1)"
echo "$alice_send_out"
printf '%s\n' "$alice_send_out" | grep -q "^Sent to " || fail "alice's send did not report success"

echo "-- bob: recv (accept invite + sync) --"
bob_recv_out="$(bob recv 2>&1)"
echo "$bob_recv_out"
if printf '%s\n' "$bob_recv_out" | grep -qF "$ALICE_MSG"; then
    echo "PASS: bob received and decrypted alice's message"
else
    fail "bob did not receive/decrypt alice's message"
fi

# --- 5. bob -> alice (reply, proves the group is genuinely bidirectional) ----
echo "-- bob: send reply to alice --"
bob_send_out="$(bob send "$alice_npub" "$BOB_MSG" 2>&1)"
echo "$bob_send_out"
printf '%s\n' "$bob_send_out" | grep -q "^Sent to " || fail "bob's send did not report success"

echo "-- alice: recv (sync) --"
alice_recv_out="$(alice recv 2>&1)"
echo "$alice_recv_out"
if printf '%s\n' "$alice_recv_out" | grep -qF "$BOB_MSG"; then
    echo "PASS: alice received and decrypted bob's reply"
else
    fail "alice did not receive/decrypt bob's reply"
fi

# --- 6. workspace round-trip (multi-channel workspace CLI + snapshot catch-up) ---
echo "-- alice: workspace new demo --"
alice workspace new demo

echo "-- alice: channel new demo backend --"
alice channel new demo backend

out=$(alice workspace list 2>&1); echo "$out"
echo "$out" | grep -q "demo" || fail "alice workspace list shows demo"

echo "-- alice: workspace add demo bob --"
alice workspace add demo "$bob_npub"
sleep 2

echo "-- bob: recv (accept invite + sync; ingests the workspace + snapshot) --"
bob_ws_recv_out="$(bob recv 2>&1)"
echo "$bob_ws_recv_out"

out=$(bob channel list demo 2>&1); echo "$out"
echo "$out" | grep -q "backend" || fail "bob sees #backend via snapshot"

echo "-- bob: post demo backend --"
bob post demo backend "hi from bob"
sleep 2

echo "-- alice: recv (sync) --"
out=$(alice recv 2>&1); echo "$out"
echo "$out" | grep -qF "hi from bob" || fail "alice receives bob's #backend post"

# --- 6b. invite carol too, so M2 governance has a third member ---------------
echo "-- alice: workspace add demo carol --"
alice workspace add demo "$carol_npub"
sleep 2
echo "-- carol: recv (join demo) --"
drain carol
out=$(carol workspace list 2>&1); echo "$out"
echo "$out" | grep -q "demo" || fail "carol joined workspace demo"

# --- 6c. private channels: a channel visible only to a subset -----------
# alice creates a PRIVATE channel `secret` inside demo, inviting ONLY bob. It is
# a separate MLS group nested under demo; carol is a full workspace member but
# was not invited, so she must never see it exists nor be able to address it.
# The headline M3 privacy property.
echo "-- alice: channel new-private demo secret (invite bob only) --"
alice channel new-private demo secret "$bob_npub"
sleep 2
echo "-- bob: recv (join the private channel via its Welcome) --"
drain bob
echo "-- bob: channel list demo (expect a 🔒 secret private channel) --"
out=$(bob channel list demo 2>&1); echo "$out"
echo "$out" | grep -q "secret" || fail "private channel: bob (a member) sees #secret"
echo "-- carol: channel list demo (must NOT reveal the private channel) --"
out=$(carol channel list demo 2>&1); echo "$out"
echo "$out" | grep -q "secret" \
    && fail "private channel: carol (a non-member) must NOT see #secret" || true
echo "-- carol: post to the private channel must fail (she cannot address it) --"
carol_post="$(carol post demo secret "sneaky" 2>&1 || true)"; echo "$carol_post"
echo "$carol_post" | grep -q "no channel #secret" \
    || fail "private channel: carol cannot post to a channel she's not in"
echo "-- bob: post to the private channel; alice (also a member) reads it --"
bob post demo secret "hello inside the private channel"
sleep 2
alice_secret="$(drain_capture alice)"; echo "$alice_secret"
echo "$alice_secret" | grep -q "hello inside the private channel" \
    || fail "private channel: alice (a member) reads bob's private post"
echo "PASS: private channel — visible to its members (alice/bob), hidden from the non-member (carol), separate-group round-trip"

# --- 6d. programmable surface (M4 ①): --json output + stdin piping -----------
# The differentiating bot/CI use case: machine-readable output a script can
# parse, and a message body piped in over stdin (`echo ... | moyu post`). Runs
# while alice/bob are still members of demo, before section 7 tears membership
# down.
echo "-- alice: whoami --json (machine-readable identity) --"
who_json="$(alice whoami --json 2>&1)"; echo "$who_json"
echo "$who_json" | grep -q '"npub":"npub1' || fail "json: whoami --json emits an npub field"

echo "-- alice: channel list demo --json (structured channel array) --"
cl_json="$(alice channel list demo --json 2>&1)"; echo "$cl_json"
echo "$cl_json" | grep -q '"slug":"backend"' || fail "json: channel list --json includes #backend"

echo "-- alice: post to #backend with the body piped over stdin --"
echo "piped over stdin" | alice post demo backend
sleep 2
echo "-- bob: recv --json (expect a JSONL message event carrying the piped body) --"
recv_json="$(bob recv --json 2>&1)"; echo "$recv_json"
echo "$recv_json" | grep -q '"event":"message"' || fail "json: recv --json emits a message event"
echo "$recv_json" | grep -q '"body":"piped over stdin"' \
    || fail "json: recv --json carries the stdin-piped body verbatim"

echo "-- alice: post to a nonexistent channel --json (structured error + nonzero exit) --"
err_json="$(alice post demo no-such-channel "x" --json 2>&1)"; err_code=$?
echo "$err_json"
[ "$err_code" -ne 0 ] || fail "json: a failed post exits non-zero"
echo "$err_json" | grep -q '"ok":false' || fail "json: a failed post emits an ok:false error object"
echo "PASS: programmable surface — --json identity/list, stdin-piped post, JSONL recv stream, structured error + nonzero exit"

# --- 6e. encrypted attachments --------------------------------------
# A Blossom blob server is HTTP storage, SEPARATE from the relay -- start a
# minimal local one (scripts/blossom-mock.py), have alice post a FILE to
# #backend, and prove bob sees it in `recv --json` and downloads byte-identical
# bytes. `--dev-allow-loopback` (already in the wrappers) also opts into MDK's
# blob-endpoint loopback gate. NEVER points at a public Blossom server.
BLOSSOM_PORT=18939
BLOSSOM_URL="http://127.0.0.1:$BLOSSOM_PORT"
BLOSSOM_STORE="$(mktemp -d)"
python3 "$REPO_ROOT/scripts/blossom-mock.py" "$BLOSSOM_PORT" "$BLOSSOM_STORE" >/dev/null 2>&1 &
BLOSSOM_PID=$!
# Wait until it accepts connections (any 404 response means it's up).
for _ in $(seq 1 30); do
    curl -s -o /dev/null "$BLOSSOM_URL/ping" && break
    sleep 0.2
done

ATTACH_DIR="$(mktemp -d)"
ATTACH="$ATTACH_DIR/report.txt"
printf 'moyu encrypted attachment payload — %s\n' "$(date +%s)" >"$ATTACH"
ORIG_SHA="$(shasum -a 256 "$ATTACH" | cut -d' ' -f1)"

echo "-- alice: post demo backend --file (encrypt + upload to local Blossom + send) --"
alice post demo backend "here is the report" --file "$ATTACH" --blossom "$BLOSSOM_URL"
sleep 2
echo "-- bob: recv --json (expect a message carrying an attachments[] array) --"
att_json="$(bob recv --json 2>&1)"
echo "$att_json"
echo "$att_json" | grep -q '"attachments"' || fail "attachment: bob's recv --json shows an attachments array"
echo "$att_json" | grep -q '"file_name":"report.txt"' || fail "attachment: the file name is surfaced"
CT_SHA="$(printf '%s' "$att_json" | grep -oE '"ciphertext_sha256":"[0-9a-f]+"' | head -1 | grep -oE '[0-9a-f]{64}')"
[ -n "$CT_SHA" ] || fail "attachment: could not read ciphertext_sha256 from recv --json"

echo "-- bob: download the attachment by its content hash --"
DL_DIR="$(mktemp -d)"
bob download demo "$CT_SHA" --out "$DL_DIR"
DL_FILE="$DL_DIR/report.txt"
[ -f "$DL_FILE" ] || fail "attachment: the decrypted file was written"
DL_SHA="$(shasum -a 256 "$DL_FILE" 2>/dev/null | cut -d' ' -f1)"
[ "$DL_SHA" = "$ORIG_SHA" ] || fail "attachment: decrypted download matches the original ($DL_SHA vs $ORIG_SHA)"
kill "$BLOSSOM_PID" 2>/dev/null || true
echo "PASS: encrypted attachment — Blossom upload, recv --json surfaces it, download round-trips byte-identical"

# --- 6f. reactions / replies / local search ------------------------
# bob reacts to and replies to a message alice posted, and alice's recv --json
# surfaces both -- proving the target `message_id` (the canonical event id) is
# stable across parties (bob targets the id HE saw; alice's copy matches). Then
# alice greps her own local history offline. Runs while both are still members
# of demo (before section 7 tears membership down).
echo "-- alice: post a uniquely-tagged line to #backend (to react/reply to) --"
REACT_TAG="react-target-$(date +%s)"
alice post demo backend "$REACT_TAG please review"
sleep 2
echo "-- bob: recv --json and extract that message's id --"
r3_json="$(bob recv --json 2>&1)"; echo "$r3_json"
TARGET_MID="$(printf '%s\n' "$r3_json" | grep "$REACT_TAG" | grep -oE '"message_id":"[0-9a-f]+"' | head -1 | grep -oE '[0-9a-f]{64}')"
[ -n "$TARGET_MID" ] || fail "react: could not read the target message_id from bob's recv --json"

echo "-- bob: react 👍 to alice's message (--json receipt) --"
react_json="$(bob react demo "$TARGET_MID" 👍 --json 2>&1)"; echo "$react_json"
echo "$react_json" | grep -q '"ok":true' || fail "react: react --json emits an ok:true receipt"
echo "$react_json" | grep -q '"action":"react"' || fail "react: the receipt records the react action"
sleep 2
echo "-- alice: recv --json (expect a reaction event targeting her message) --"
ar_json="$(alice recv --json 2>&1)"; echo "$ar_json"
echo "$ar_json" | grep -q '"event":"reaction"' || fail "react: alice's recv --json surfaces a reaction event"
echo "$ar_json" | grep -q "\"target\":\"$TARGET_MID\"" || fail "react: the reaction targets alice's exact message id (cross-party id stable)"

echo "-- bob: reply to alice's message (body piped over stdin) --"
echo "on it — replying in thread" | bob reply demo "$TARGET_MID"
sleep 2
echo "-- alice: recv --json (expect a message carrying reply_to = her message id) --"
rp_json="$(alice recv --json 2>&1)"; echo "$rp_json"
echo "$rp_json" | grep -q "\"reply_to\":\"$TARGET_MID\"" || fail "reply: the reply event carries reply_to = the parent id"
echo "$rp_json" | grep -q '"body":"on it — replying in thread"' || fail "reply: the reply body round-trips"
# A reply to a #backend post must stay in #backend (not fall through to
# #general): the same event line carries both reply_to and channel=backend.
printf '%s\n' "$rp_json" | grep "\"reply_to\":\"$TARGET_MID\"" | grep -q '"channel":"backend"' \
    || fail "reply: a public-channel reply stays in-channel (#backend), not #general"

echo "-- alice: search her local history for the tag (offline grep, --json) --"
srch_json="$(alice search "$REACT_TAG" --json 2>&1)"; echo "$srch_json"
echo "$srch_json" | grep -q "$REACT_TAG" || fail "search: alice finds her tagged message in local history"
echo "$srch_json" | grep -q '"channel":"backend"' || fail "search: the match reports its channel"
echo "PASS: reactions/replies/search — react receipt + alice sees the reaction event, threaded reply carries reply_to, offline search finds local history"

# --- 6g. bot/agent events: CI/git/monitor -> E2EE channel ----------
# alice emits a bot OPERATION (kind-1202) into #backend; bob's recv --json must
# surface it as an `agent_op` carrying the structured fields (event_type/status/
# ok/run_id) AND the channel slug -- proving public-channel bot routing (the slug
# rides in details.moyu.ch, since a kind-1202 event has no kind-9 envelope) and,
# critically, that a non-chat kind round-trips through a real relay and the
# peer's sync surfaces it. Then a bot ACTIVITY (kind-1201) proves the 🤖 human
# render.
OP_RUN="run-$(date +%s)"
echo "-- alice: emit a failed CI op into #backend (kind-1202, --json receipt) --"
op_receipt="$(alice op demo backend --json --type ci --status failed --name build --fail --duration-ms 4200 --run-id "$OP_RUN" "3 tests failed on main" 2>&1)"; echo "$op_receipt"
echo "$op_receipt" | grep -q '"kind":1202' || fail "op: receipt records kind-1202"
echo "$op_receipt" | grep -q '"event_type":"ci"' || fail "op: receipt records event_type=ci"
sleep 2
echo "-- bob: recv --json (expect an agent_op event with the structured fields) --"
op_json="$(bob recv --json 2>&1)"; echo "$op_json"
op_line="$(printf '%s\n' "$op_json" | grep '"event":"agent_op"')"
[ -n "$op_line" ] || fail "op: bob's recv --json surfaces an agent_op event (kind-1202 round-trips)"
printf '%s\n' "$op_line" | grep -q '"event_type":"ci"' || fail "op: agent_op carries event_type=ci"
printf '%s\n' "$op_line" | grep -q '"status":"failed"' || fail "op: agent_op carries status=failed"
printf '%s\n' "$op_line" | grep -q '"ok":false' || fail "op: agent_op carries ok=false"
printf '%s\n' "$op_line" | grep -q "\"run_id\":\"$OP_RUN\"" || fail "op: agent_op carries the run_id"
printf '%s\n' "$op_line" | grep -q '"channel":"backend"' || fail "op: public-channel bot routing lands in #backend (details.moyu.ch)"

printf '%s\n' "$op_line" | grep -q '"duration_ms":4200' || fail "op: agent_op carries duration_ms"
# The public op above carried no user --details, so recv --json must NOT emit a
# "details" key (nothing survives once the internal moyu routing key is stripped).
printf '%s\n' "$op_line" | grep -q '"details"' && fail "op: recv --json must omit an empty details (routing-only)" || true

# A PRIVATE-channel op must label as its real channel (#secret),
# not #general, AND recv --json must surface the user's own --details fields with
# the internal moyu routing key stripped.
echo "-- alice: emit an op into the PRIVATE channel #secret (with --details) --"
alice op demo secret --type deploy --status ok --ok --details '{"env":"prod","ver":"v2"}' "shipped to prod"
sleep 2
echo "-- bob: recv --json (private-channel op labels #secret, details surfaced sans moyu) --"
sec_json="$(bob recv --json 2>&1)"; echo "$sec_json"
sec_line="$(printf '%s\n' "$sec_json" | grep '"event":"agent_op"' | grep '"channel":"secret"')"
[ -n "$sec_line" ] || fail "op: a private-channel op labels as #secret, not #general"
printf '%s\n' "$sec_line" | grep -q '"env":"prod"' || fail "op: the user's --details fields reach recv --json"
printf '%s\n' "$sec_line" | grep -q '"moyu"' && fail "op: recv --json must strip the internal moyu routing key from details" || true

echo "-- alice: emit a bot activity line into #backend (kind-1201) --"
alice activity demo backend "deploying v2 to prod" --status working
sleep 2
echo "-- bob: recv (human render, expect a 🤖 marker + the body) --"
act_human="$(bob recv 2>&1)"; echo "$act_human"
printf '%s\n' "$act_human" | grep -q '🤖' || fail "activity: bob's recv shows a 🤖 bot marker"
printf '%s\n' "$act_human" | grep -q 'deploying v2 to prod' || fail "activity: the activity body round-trips"
echo "PASS: bot events — op (kind-1202) round-trips (type/status/ok/run_id/duration) + public routing to #backend + private routing labels #secret + --details surfaced sans moyu; activity (kind-1201) renders a 🤖 line"

# --- 6h. onboarding: invite / join / requests / approve ------
# Fresh identities (never touch alice/bob/carol's demo-workspace state above):
# dan creates workspace #eng and is its sole admin; erin joins it via a
# workspace invite code and is approved (a request badged ✓ trusted, since it
# carries a secret dan actually issued); fay/gus prove the 1:1 contact-invite
# "hello"; dan then reuses #eng with a --auto-approve code so a fifth
# identity, hal, is let in with no explicit `approve`; finally erin -- a
# non-admin member of #eng from the first round -- has her own
# `invite eng --auto-approve` refused up front (only an admin may mint one).
DAN_DIR="$(tmpdir dan)"
ERIN_DIR="$(tmpdir erin)"
FAY_DIR="$(tmpdir fay)"
GUS_DIR="$(tmpdir gus)"
HAL_DIR="$(tmpdir hal)"
echo "dan  data dir: $DAN_DIR"
echo "erin data dir: $ERIN_DIR"
echo "fay  data dir: $FAY_DIR"
echo "gus  data dir: $GUS_DIR"
echo "hal  data dir: $HAL_DIR"

dan()  { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$DAN_DIR"  --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
erin() { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$ERIN_DIR" --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
fay()  { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$FAY_DIR"  --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
gus()  { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$GUS_DIR"  --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }
hal()  { MOYU_PASSPHRASE="$PASSPHRASE" "$MOYU_BIN" --data-dir "$HAL_DIR"  --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} "$@"; }

# Only the two admins/inviters need an explicit `init` up front -- everyone
# else (erin/gus/hal) is inline-inited by their own `join`, exactly the
# frictionless-onboarding path this section exists to prove.
echo "-- dan: init --"
dan_init_out="$(dan init 2>&1)"; echo "$dan_init_out"
dan_npub="$(printf '%s\n' "$dan_init_out" | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$dan_npub" ] || fail "onboarding: could not parse dan's npub from init output"

echo "-- fay: init --"
fay_init_out="$(fay init 2>&1)"; echo "$fay_init_out"

# Give dan's and fay's freshly-published KeyPackages a moment to land on the
# relay before anyone tries to resolve them (mirrors the section-4 sleep).
sleep 2

echo "-- dan: workspace new eng --"
dan workspace new eng

echo "-- dan: invite eng (stdout is ONLY the token; diagnostics on stderr) --"
CODE="$(dan invite eng 2>/dev/null)"
echo "invite code: $CODE"
printf '%s\n' "$CODE" | grep -q "moyuinv1" || fail "onboarding: invite eng prints a moyuinv1 code on stdout"

echo "-- erin: join \"\$CODE\" (fresh data dir -- inline-inits + sends dan a join request) --"
erin_join_out="$(erin join "$CODE" 2>&1)"; echo "$erin_join_out"
erin_npub="$(erin whoami 2>&1 | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$erin_npub" ] || fail "onboarding: could not resolve erin's npub after join"
sleep 2

echo "-- dan: recv (sync erin's join request; the 🔑 line now carries a trust badge, F1) --"
dan_jr_recv="$(drain_capture dan)"; echo "$dan_jr_recv"
# The recv 🔑 line resolves the workspace from the envelope's gid and badges
# it -- erin's secret is one dan issued for #eng, so it reads "✓ 持码可信".
printf '%s\n' "$dan_jr_recv" | grep -q "持码可信" \
    || fail "onboarding: dan's recv 🔑 line badges erin's request trusted (F1)"

echo "-- dan: requests --json (expect erin's request, badged trusted -- her secret is dan's own) --"
REQ="$(dan requests --json 2>/dev/null)"; echo "$REQ"
printf '%s\n' "$REQ" | grep -qF "\"npub\":\"$erin_npub\"" || fail "onboarding: dan's requests lists erin's pending join"
printf '%s\n' "$REQ" | grep -q '"trusted":true' || fail "onboarding: dan's requests badges erin's request trusted"

echo "-- dan: approve all --"
dan approve all
sleep 2

echo "-- erin: recv (accept the invite_members Welcome + catch-up snapshot) --"
drain erin

echo "-- dan: workspace members eng (expect erin now a member) --"
out=$(dan workspace members eng 2>&1); echo "$out"
{ [ -n "$erin_npub" ] && printf '%s\n' "$out" | grep -qF "$erin_npub"; } || fail "onboarding: dan's workspace members eng lists erin after approve"

echo "-- erin: workspace list (expect eng) --"
out=$(erin workspace list 2>&1); echo "$out"
printf '%s\n' "$out" | grep -q "eng" || fail "onboarding: erin's workspace list shows eng after being approved"
echo "PASS: onboarding — workspace invite/join/requests/approve round-trip, request badged trusted"

# --- 6h-ii. 1:1 contact invite: fay invites (no workspace slug), gus joins ---
echo "-- fay: invite (no workspace => a 1:1 contact invite) --"
CCODE="$(fay invite 2>/dev/null)"
echo "contact invite code: $CCODE"
printf '%s\n' "$CCODE" | grep -q "moyuinv1" || fail "onboarding: contact invite prints a moyuinv1 code"

echo "-- gus: join \"\$CCODE\" (fresh data dir -- inline-inits + DMs fay a hello) --"
gus_join_out="$(gus join "$CCODE" 2>&1)"; echo "$gus_join_out"
sleep 2

echo "-- fay: recv (expect gus's 👋 joined via invite hello) --"
fay_recv_out="$(drain_capture fay)"; echo "$fay_recv_out"
printf '%s\n' "$fay_recv_out" | grep -q "joined via invite" || fail "onboarding: fay's recv shows gus's joined-via-invite hello"
echo "PASS: onboarding — 1:1 contact invite round-trip (fay sees gus's hello)"

# --- 6h-iii. auto-approve: dan's --auto-approve code lets hal in with no `approve` ---
echo "-- dan: invite eng --auto-approve --"
CODE2="$(dan invite eng --auto-approve 2>/dev/null)"
echo "auto-approve invite code: $CODE2"
printf '%s\n' "$CODE2" | grep -q "moyuinv1" || fail "onboarding: --auto-approve invite still prints a moyuinv1 code"

echo "-- hal: join \"\$CODE2\" (fresh data dir) --"
hal_join_out="$(hal join "$CODE2" 2>&1)"; echo "$hal_join_out"
hal_npub="$(hal whoami 2>&1 | sed -n 's/^ *npub: *//p' | head -n1)"
[ -n "$hal_npub" ] || fail "onboarding: could not resolve hal's npub after join"
sleep 2

echo "-- dan: recv (self-healing auto-approve fallback should let hal in with no explicit approve) --"
drain dan

echo "-- dan: workspace members eng (expect hal, auto-approved) --"
out=$(dan workspace members eng 2>&1); echo "$out"
if ! printf '%s\n' "$out" | grep -qF "$hal_npub"; then
    # KeyPackage propagation can occasionally outlast one drain (4 polls); one
    # more recv round mirrors how the rest of this script retries async state.
    drain dan
    out=$(dan workspace members eng 2>&1); echo "$out"
fi
{ [ -n "$hal_npub" ] && printf '%s\n' "$out" | grep -qF "$hal_npub"; } || fail "onboarding: auto-approve landed hal in eng without an explicit approve"
echo "PASS: onboarding — --auto-approve invite self-heals hal into #eng with no explicit approve"

# --- 6h-iii-b. a removed member is not put back by the auto-approve code ----
# hal's original join request stays in dan's DM history for ever, and hal
# still holds CODE2. Neither may let him back in once dan has kicked him:
# re-admitting a removed member takes an explicit `approve`.
echo "-- hal: recv (accept the Welcome, so he is a full member before the kick) --"
drain hal

echo "-- dan: workspace kick eng hal --"
dan workspace kick eng "$hal_npub" || fail "rejoin: dan could not kick hal"
sleep 2

echo "-- dan: recv repeatedly (each run starts with the join-request sweep) --"
drain dan
drain dan
out=$(dan workspace members eng 2>&1); echo "$out"
if printf '%s\n' "$out" | grep -qF "$hal_npub"; then
    fail "rejoin: hal was kicked but his old join request put him back in eng"
fi
echo "PASS: rejoin — a kicked member's old join request does not re-add him"

echo "-- hal: recv (learn he was removed), then join with the same code again --"
drain hal
hal join "$CODE2" 2>&1 || true
sleep 2

echo "-- dan: recv (hal's new request must NOT be auto-approved) --"
drain dan
drain dan
out=$(dan workspace members eng 2>&1); echo "$out"
if printf '%s\n' "$out" | grep -qF "$hal_npub"; then
    fail "rejoin: a removed member rejoined through the auto-approve code"
fi

echo "-- dan: requests --json (hal's new request is open, waiting for a manual approve) --"
out=$(dan requests --json 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$hal_npub" \
    || fail "rejoin: hal's new request is not listed for a manual approve"
echo "PASS: rejoin — a removed member holding the auto-approve code is not auto-approved, only listed"

echo "-- dan: invite eng --revoke (cancel every code issued for eng) --"
out=$(dan invite eng --revoke 2>&1); echo "$out"
printf '%s\n' "$out" | grep -q "revoked 2 invite code(s) for #eng" \
    || fail "revoke: expected both eng codes (manual + auto-approve) to be revoked"
out=$(dan requests --json 2>&1); echo "$out"
printf '%s\n' "$out" | grep -F "$hal_npub" | grep -q '"trusted":false' \
    || fail "revoke: a request made with a revoked code is still badged trusted"
echo "PASS: revoke — revoked codes are no longer trusted"

# --- 6h-iv. a non-admin's --auto-approve invite is refused up front ---------
echo "-- erin (non-admin member of eng): invite eng --auto-approve must be refused --"
erin_na_out="$(erin invite eng --auto-approve 2>&1)"; erin_na_code=$?
echo "$erin_na_out"
[ "$erin_na_code" -ne 0 ] || fail "onboarding: a non-admin's --auto-approve invite must exit non-zero"
printf '%s\n' "$erin_na_out" | grep -q "not an admin" || fail "onboarding: a non-admin's --auto-approve invite reports she is not an admin"
echo "PASS: onboarding — a non-admin's --auto-approve invite is refused (not an admin)"

# --- 6h-v. relay list / forget: audit + prune the persisted relay set ---
# `join` unions an inviter's relays into config.json; `relay list` audits that
# set and `relay forget` undoes it (the mitigation for relay-set drift). `relay`
# reads config.json, so the per-command `--relay` override is irrelevant here.
echo "-- erin: relay list (erin's join persisted the effective relay set to config.json) --"
relays_out="$(erin relay list 2>/dev/null)"; echo "$relays_out"
printf '%s\n' "$relays_out" | grep -qF "$RELAY" || fail "onboarding: relay list shows the relay join persisted"
echo "-- erin: relay forget \"\$RELAY\" then relay list (must be gone) --"
erin relay forget "$RELAY" >/dev/null 2>&1 || fail "onboarding: relay forget exits 0"
relays_after="$(erin relay list 2>/dev/null)"; echo "relays after forget: [$relays_after]"
if printf '%s\n' "$relays_after" | grep -qF "$RELAY"; then
    fail "onboarding: relay forget removed the relay from config.json"
fi
echo "PASS: onboarding — relay list/forget audits and prunes the persisted relay set"

# --- 7. M2 governance: admin set, member badges, kick, creator transfer-leave -
# alice created demo, so she is its sole MLS admin; bob and carol are regular
# members. (This section also covers the leave-vs-removed property both ways: a member
# who LEAVES (alice, below) and one who is REMOVED (carol, kicked) must both
# stop listing the workspace afterwards.)
echo "-- alice: workspace members demo (alice badged admin, >=3 members) --"
out=$(alice workspace members demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -F "$alice_npub" | grep -q "admin" \
    || fail "members: creator alice is badged admin"
[ "$(printf '%s\n' "$out" | grep -c "npub1")" -ge 3 ] || fail "members: demo lists >=3 members"

echo "-- alice: workspace admin list demo (only alice) --"
out=$(alice workspace admin list demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$alice_npub" || fail "admin list: alice is admin"
printf '%s\n' "$out" | grep -qF "$bob_npub" && fail "admin list: bob is not an admin yet" || true

# admin add / remove round-trip (alice-local commits; no cross-sync needed for
# alice's own reads).
echo "-- alice: workspace admin add demo carol --"
alice workspace admin add demo "$carol_npub"
out=$(alice workspace admin list demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$carol_npub" || fail "admin add: carol is now admin"
echo "-- alice: workspace admin remove demo carol --"
alice workspace admin remove demo "$carol_npub"
out=$(alice workspace admin list demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$carol_npub" && fail "admin remove: carol no longer admin" || true

# kick: alice removes carol from the group entirely.
echo "-- alice: workspace kick demo carol --"
kick_out="$(alice workspace kick demo "$carol_npub" 2>&1)"; echo "$kick_out"
printf '%s\n' "$kick_out" | grep -q "removed" || fail "kick: alice reports removing carol"
out=$(alice workspace members demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$carol_npub" && fail "kick: carol still a member after kick" || true
sleep 2
echo "-- carol: recv (sync the removal; expect a self-removal governance notice) --"
carol_drain="$(drain_capture carol)"; echo "$carol_drain"
printf '%s\n' "$carol_drain" | grep -q "no longer a member of workspace demo" \
    || fail "governance notice: carol's recv announces her own removal"
out=$(carol workspace list 2>&1); echo "$out"
printf '%s\n' "$out" | grep -q "demo" && fail "kick: carol's removed workspace still lists" || true

# bob has not synced since section 6; drain his commit backlog (invite/promote/
# demote/remove carol) so he is current before the transfer round.
echo "-- bob: drain backlog --"
drain bob

# creator transfer-leave: alice is the sole admin; --transfer-to hands admin to
# bob (promote -> self-demote -> leave), closing the M1 "creator cannot leave".
echo "-- alice: workspace leave demo --transfer-to bob --"
leave_out="$(alice workspace leave demo --transfer-to "$bob_npub" 2>&1)"; echo "$leave_out"
printf '%s\n' "$leave_out" | grep -q "left workspace demo" || fail "transfer-leave: alice left demo"
out=$(alice workspace list 2>&1); echo "$out"
printf '%s\n' "$out" | grep -q "demo" && fail "transfer-leave: alice's left workspace still lists" || true
sleep 2
echo "-- bob: recv (sync promote + alice's leave; expect observer governance notices) --"
bob_drain="$(drain_capture bob)"; echo "$bob_drain"
# Observer-side proof: bob learns of his own promotion via a convergence-driven
# roster diff (there is NO sync event for it -- see `governance` module docs).
printf '%s\n' "$bob_drain" | grep -q "is now an admin of workspace demo" \
    || fail "governance notice: bob observes his promotion during the transfer"
out=$(bob workspace admin list demo 2>&1); echo "$out"
printf '%s\n' "$out" | grep -qF "$bob_npub" || fail "transfer-leave: bob took over as admin"
printf '%s\n' "$out" | grep -qF "$alice_npub" && fail "transfer-leave: alice no longer admin" || true

# --- 10. moyu session (GUI-shell workflow A): framed stdio protocol + pump ---
echo "-- bob: long-lived session receives alice's send as a framed live event --"
SESSION_OUT="$(tmpfile session-out)"
SESSION_MSG="live-session-ping-$$"
{
    printf '%s\n' '{"id":"u1","cmd":"unlock","args":{"passphrase":"'"$PASSPHRASE"'"}}'
    sleep 8
    printf '%s\n' '{"id":"c1","cmd":"catchup"}'
    sleep 2
} | "$MOYU_BIN" --data-dir "$BOB_DIR" --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} session >"$SESSION_OUT" 2>/dev/null &
SESSION_PID=$!
sleep 3
alice send "$bob_npub" "$SESSION_MSG" >/dev/null 2>&1 || fail "session: alice's one-shot send failed"
wait "$SESSION_PID" || fail "session: bob's session exited non-zero"
echo "-- bob session stream (tail) --"; tail -n 5 "$SESSION_OUT"
grep -q '"type":"hello"' "$SESSION_OUT" || fail "session: missing hello frame"
grep -q '"id":"u1"' "$SESSION_OUT" || fail "session: missing unlock receipt"
grep -q '"type":"event"' "$SESSION_OUT" || fail "session: no live event frames from the pump"
grep -q "$SESSION_MSG" "$SESSION_OUT" || fail "session: alice's message did not surface as a live event"
grep -q '"code":"stdin_closed"' "$SESSION_OUT" || fail "session: missing clean-EOF fatal frame"
# Framing discipline: every stdout line is a JSON object carrying "type"
# (one stray line would corrupt a GUI's JSONL parser).
grep -v '"type":' "$SESSION_OUT" | grep -q . && fail "session: unframed line on stdout" || true
rm -f "$SESSION_OUT"

# Regression: an in-session `add` must write through the
# session's ONE store handle — a later store flush (keypackage publish) must
# not clobber the contact, and conversations must see it immediately.
echo "-- bob session: add-contact survives a store flush (H1 regression) --"
SESSION_OUT2="$(tmpfile session-out2)"
# A throwaway public key minted for this test (nobody holds its secret key).
CAROL_NPUB="npub1w2gutm26her56272vdam9h03e8eru6r3jrhqvp8zxla5vr2zmz8st83mj3"
{
    printf '%s\n' '{"id":"u1","cmd":"unlock","args":{"passphrase":"'"$PASSPHRASE"'"}}'
    printf '%s\n' '{"id":"a1","cmd":"add","args":{"peer":"'"$CAROL_NPUB"'","label":"carol"}}'
    printf '%s\n' '{"id":"k1","cmd":"keypackage","args":{"action":"publish"}}'
    printf '%s\n' '{"id":"v1","cmd":"conversations"}'
    sleep 2
} | "$MOYU_BIN" --data-dir "$BOB_DIR" --relay "$RELAY" --dev-allow-loopback ${SOCKS5_ARGS[@]+"${SOCKS5_ARGS[@]}"} session >"$SESSION_OUT2" 2>/dev/null \
    || fail "session: add/flush session exited non-zero"
grep '"id":"a1"' "$SESSION_OUT2" | grep -q '"ok":true' || fail "session: add receipt failed"
grep '"id":"v1"' "$SESSION_OUT2" | grep -q '"label":"carol"' \
    || fail "session: added contact invisible to conversations (stale session store)"
# And the contact survived the keypackage-publish store flush ON DISK:
grep -q "carol" "$BOB_DIR"/accounts/*/moyu-state.json \
    || fail "session: added contact clobbered on disk by the store flush (H1)"
rm -f "$SESSION_OUT2"
echo "PASS: session — framed hello/receipt/fatal + live message event via the 2s pump + add-through-owned-store (H1)"

echo "=================================="
if [ "$pass" -eq 1 ]; then
    echo "PASS: moyu E2E — 1:1 MLS round-trip + workspace create/invite/channel/post + M3 private channels (subset-only visibility, hidden from non-members, separate-group round-trip) + M4 ① programmable surface (--json output, stdin-piped post, JSONL recv stream, structured error+exit) + M4 ② encrypted attachments (Blossom upload, recv --json, byte-identical download) + M4 ③ reactions/replies/search (react receipt, reaction event, threaded reply_to, offline search) + M5 ① bot/agent events (kind-1202 op round-trip with event_type/status/ok/run_id + public-channel routing, kind-1201 activity 🤖 render) + ⓑ-1 onboarding (invite/join/requests/approve: workspace invite trusted-badged + approved, 1:1 contact hello, --auto-approve self-heal, non-admin --auto-approve refused) + M2 governance (admin add/remove/list, kick, creator transfer-leave) + convergence-driven governance notices (self-removal + observer-side promotion) + moyu session (framed stdio protocol: hello/receipt/event/fatal, unlock state machine, live message via the 2s event pump) succeeded"
    exit 0
else
    echo "FAIL: moyu E2E round-trip did not fully succeed (see above)"
    exit 1
fi
