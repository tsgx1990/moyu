#!/usr/bin/env bash
# Bridge any JSON webhook / monitoring alert into an E2EE moyu channel as a 🤖
# operation (kind-1202). Reads one JSON payload on stdin, extracts a few fields
# with jq, and forwards them.
#
# Wire it up however your source runs a command, e.g.:
#   * an Alertmanager `command` receiver,
#   * a `socat`/`nc` one-liner that pipes an HTTP body into this script,
#   * a cron job polling an API and emitting JSON.
#
# Example — turn a Prometheus Alertmanager alert into a moyu op:
#   cat alert.json | ./webhook-bridge.sh monitor
#
# Configure the bot (see examples/integrations/README.md for one-time setup):
export MOYU_DIR="${MOYU_DIR:-/srv/moyu-bot}"
export MOYU_RELAY="${MOYU_RELAY:-wss://relay.your-org.example}"
export MOYU_WS="${MOYU_WS:-eng}"
export MOYU_CHANNEL="${MOYU_CHANNEL:-alerts}"
export MOYU_PASSPHRASE="${MOYU_PASSPHRASE:?set MOYU_PASSPHRASE for the bot identity}"

set -euo pipefail

SOURCE="${1:-webhook}"                 # the --type (e.g. monitor, sentry, deploy)
payload="$(cat)"                       # the raw JSON body on stdin

# Pull common fields; tweak the jq paths to match your source's schema.
status="$(jq -r '.status // .state // "firing"' <<<"$payload")"
name="$(jq -r '.alertname // .labels.alertname // .title // "alert"' <<<"$payload")"
summary="$(jq -r '.summary // .annotations.summary // .message // .text // "(no summary)"' <<<"$payload")"

# Map the source's status to moyu's --ok/--fail (resolved states are "ok").
case "$status" in
  resolved|ok|success|healthy) OKFLAG=--ok ;;
  *)                           OKFLAG=--fail ;;
esac

# Forward the full original payload as `details` so nothing is lost, and pass the
# human summary on stdin (so shell quoting can't mangle it).
printf '%s' "$summary" | \
  moyu --data-dir "$MOYU_DIR" --relay "$MOYU_RELAY" \
    op "$MOYU_WS" "$MOYU_CHANNEL" \
      --type "$SOURCE" \
      --status "$status" \
      --name "$name" \
      "$OKFLAG" \
      --details "$payload" \
      -                                 # read the summary from stdin
