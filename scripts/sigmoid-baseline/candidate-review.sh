#!/usr/bin/env bash
# Render every candidate anchoring form for every fixture frame and build a comparison page.
#
# Companion to patch-review.sh. Where that page asks "is this patch a diffuse white?", this
# one asks "which anchoring form renders acceptably?" — the question the reduced scope
# (filter forms, do not tune parameters) actually needs answered.
#
# Every form reduces to one number: the sigmoid anchor (`--d-max`) plus a contrast. The
# anchor rules live in build_candidate_review.py so the renders and the page cannot disagree.
#
# The frozen recipe supplies the roll's Dmin via `--params`; the curve, contrast and anchor
# are then overridden by flags (flags win over the recipe, per the documented merge).
#
# Renders through `--output-preset ultra-hdr-v1`, whose JPEG base is the `pipeline::sdr`
# rendition — the same renderer the metrics measure. Output is throwaway: it never goes into
# ../nc-assets or the repo.
#
# Usage: bash scripts/sigmoid-baseline/candidate-review.sh [outdir]
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT=${1:-$ROOT/../temp/candidate-review}
NC=${NC:-$ROOT/target/release/nc}
A=${A:-$ROOT/../nc-assets}
FX="$ROOT/scripts/sigmoid-baseline/fixtures.json"
# STUDY=--shoulder renders the two GO forms at three shoulder widths instead of all 8 forms.
STUDY=${STUDY:-}

for t in sips python3; do command -v "$t" >/dev/null || { echo "error: $t missing" >&2; exit 2; }; done
[ -x "$NC" ] || { echo "error: no $NC — run 'cargo build --release'" >&2; exit 2; }
mkdir -p "$OUT"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

PLAN="$TMP/plan.txt"
python3 "$HERE/build_candidate_review.py" --plan "$FX" $STUDY > "$PLAN" || exit 2
echo "rendering $(wc -l < "$PLAN" | tr -d ' ') candidate images -> $OUT"

fail=0
while IFS='|' read -r mark cid anchor contrast shoulder roll recipe file; do
  [ -n "$mark" ] || continue
  # `auto` selects the shipped content-driven measurement; otherwise pin the anchor.
  if [ "$anchor" = "auto" ]; then dm=(--auto-d-max); else dm=(--d-max "$anchor"); fi
  if "$NC" convert "$A/rolls/$roll/$file" -o "$TMP/v.jpg" \
       --params "$ROOT/$recipe" --density-curve sigmoid \
       --sigmoid-contrast "$contrast" --sigmoid-shoulder "$shoulder" "${dm[@]}" \
       --output-preset ultra-hdr-v1 >/dev/null 2>&1 \
     && sips -Z 1200 -s format jpeg -s formatOptions 82 "$TMP/v.jpg" \
       --out "$OUT/$mark-$cid.jpg" >/dev/null 2>&1; then
    printf '.'
  else
    echo; echo "  $mark/$cid FAILED" >&2; fail=$((fail+1))
  fi
  rm -f "$TMP/v.jpg" "$TMP/v.jpg.json"
done < "$PLAN"
echo; [ "$fail" -eq 0 ] || echo "$fail render(s) failed" >&2

python3 "$HERE/build_candidate_review.py" --page "$FX" "$OUT/index.html" $STUDY || exit 2
echo "open: file://$(cd "$OUT" && pwd -P)/index.html"
