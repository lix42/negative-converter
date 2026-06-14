#!/usr/bin/env bash
# PostToolUse(Edit|Write|MultiEdit): format the just-edited Rust file with
# rustfmt so the strict `cargo fmt --all --check` CI gate can never fail on
# formatting alone. Best-effort: a mid-edit parse error is left for the build
# to surface, not reported as noise here.
set -uo pipefail

file=$(jq -r '.tool_input.file_path // empty')
case "$file" in
  *.rs)
    [ -f "$file" ] && rustfmt --edition 2024 "$file" 2>/dev/null || true
    ;;
esac
exit 0
