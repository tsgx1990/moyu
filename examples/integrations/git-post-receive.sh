#!/usr/bin/env bash
# git server-side `post-receive` hook: announce each pushed branch into an E2EE
# moyu channel as a 🤖 git-push operation (kind-1202).
#
# Install: on the server hosting the bare repo, save this as
#   <repo>.git/hooks/post-receive
# and `chmod +x` it. git feeds it "<old-sha> <new-sha> <ref>" lines on stdin.
#
# Configure the bot (see examples/integrations/README.md for the one-time setup):
export MOYU_DIR="${MOYU_DIR:-/srv/moyu-bot}"       # the bot's data dir
export MOYU_RELAY="${MOYU_RELAY:-wss://relay.your-org.example}"
export MOYU_WS="${MOYU_WS:-eng}"                   # target workspace
export MOYU_CHANNEL="${MOYU_CHANNEL:-git}"         # target channel
export MOYU_PASSPHRASE="${MOYU_PASSPHRASE:?set MOYU_PASSPHRASE for the bot identity}"

set -euo pipefail

while read -r oldrev newrev ref; do
  branch="${ref#refs/heads/}"
  [ "$branch" = "$ref" ] && continue               # skip tags / non-branch refs

  if [ "$oldrev" = "0000000000000000000000000000000000000000" ]; then
    status="created"; count="new branch"
  else
    status="pushed"
    count="$(git rev-list --count "$oldrev..$newrev") commit(s)"
  fi
  subject="$(git log -1 --format='%s' "$newrev" 2>/dev/null || echo '')"
  author="$(git log -1 --format='%an' "$newrev" 2>/dev/null || echo '')"

  moyu --data-dir "$MOYU_DIR" --relay "$MOYU_RELAY" \
    op "$MOYU_WS" "$MOYU_CHANNEL" \
      --type git-push \
      --status "$status" \
      --name "$branch" \
      --ok \
      --details "$(jq -n --arg b "$branch" --arg a "$author" --arg n "$newrev" \
                    '{branch:$b, author:$a, new:$n}')" \
      "$author $status $count to $branch — $subject"
done
