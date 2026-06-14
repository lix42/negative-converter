#!/usr/bin/env bash
# Stop hook: before Claude finishes a turn, run the SAME clippy gate CI enforces
# (`-D warnings`) so red lint is caught here, not on a PR. Scoped to only run
# when Rust sources are actually dirty/new, so chat-only or clean turns are free.
set -uo pipefail

input=$(cat)

# Loop breaker: if we already blocked once this turn, let Claude stop.
if [ "$(printf '%s' "$input" | jq -r '.stop_hook_active // false')" = "true" ]; then
  exit 0
fi

# Only gate when there are changed or untracked .rs files in the tree.
if git status --porcelain --untracked-files=all 2>/dev/null | grep -q '\.rs$'; then
  if ! out=$(cargo clippy --all-targets --quiet -- -D warnings 2>&1); then
    {
      echo "clippy gate failed — CI runs \`cargo clippy --all-targets -- -D warnings\`."
      echo "Fix these before finishing:"
      echo
      printf '%s\n' "$out"
    } >&2
    exit 2
  fi
fi
exit 0
