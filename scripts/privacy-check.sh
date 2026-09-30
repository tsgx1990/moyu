#!/usr/bin/env bash
# Secret and personal-data gate for this (public) repository.
#
#   scripts/privacy-check.sh            check every tracked file as it is on disk
#   scripts/privacy-check.sh --staged   check what is about to be committed
#   scripts/privacy-check.sh --history  check every commit on every ref
#
# It does two things:
#   1. refuses files that must never be tracked here (maintainer ledgers,
#      AI-assistant state, runtime data);
#   2. runs gitleaks with .gitleaks.toml (stock secret rules + this repo's
#      privacy rules).
#
# Enable the pre-commit hook once per clone:
#   git config core.hooksPath .githooks
# gitleaks: https://github.com/gitleaks/gitleaks#installing (`brew install gitleaks`).
set -euo pipefail

mode="${1:-tree}"
root="$(git rev-parse --show-toplevel)"
cd "$root"

# --- 1. paths that must never be tracked ------------------------------------
forbidden='^(PROGRESS\.md|HANDOFF\.md|CLAUDE\.local\.md|\.remember/|\.claude/|docs/superpowers/|docs/design/|moyu-data/)|(^|/)(moyu-state\.json|active-account\.txt|net-settings\.json|\.env(\..*)?)$|\.(moyu\.enc\.json|sqlite|sqlite-wal|sqlite-shm|pem|p12)$'
case "$mode" in
    --staged)  names="$(git diff --cached --name-only --diff-filter=ACMR)" ;;
    --history) names="$(git log --all --name-only --format= --diff-filter=ACMR | sort -u)" ;;
    tree)      names="$(git ls-files)" ;;
    *) echo "usage: $0 [--staged|--history]" >&2; exit 2 ;;
esac
bad="$(printf '%s\n' "$names" | grep -E "$forbidden" || true)"
if [ -n "$bad" ]; then
    echo "privacy-check: these paths must not be committed to this repository:" >&2
    printf '  %s\n' $bad >&2
    exit 1
fi

# --- 2. content --------------------------------------------------------------
if ! command -v gitleaks >/dev/null 2>&1; then
    echo "privacy-check: gitleaks is not installed, so nothing was scanned." >&2
    echo "  Install it (https://github.com/gitleaks/gitleaks#installing) and re-run." >&2
    exit 1
fi
config="$root/.gitleaks.toml"
case "$mode" in
    --staged)
        gitleaks git --pre-commit --staged --no-banner --redact --config "$config" .
        ;;
    --history)
        gitleaks git --no-banner --redact --config "$config" --log-opts="--all" .
        ;;
    tree)
        # Only tracked files, as they are on disk now (a plain directory scan
        # would also walk build output and ignored local files).
        tmp="$(mktemp -d "${TMPDIR:-/tmp}/moyu-privacy.XXXXXX")"
        trap 'rm -rf "$tmp"' EXIT
        git ls-files -z | while IFS= read -r -d '' f; do
            [ -f "$f" ] || continue
            mkdir -p "$tmp/$(dirname "$f")"
            cp "$f" "$tmp/$f"
        done
        (cd "$tmp" && gitleaks dir --no-banner --redact --config "$config" .)
        ;;
esac
echo "privacy-check: ok ($mode)"
