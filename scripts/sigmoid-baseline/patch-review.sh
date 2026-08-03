#!/usr/bin/env bash
# Build the Phase-1 patch-review page for `algo/reference-anchored-sigmoid`.
#
# Renders each fixture roll's `real` frames to a viewable positive, then lays the
# candidate patch rectangles over them with magnified crops and per-frame questions.
# The page exists so a human can confirm the *semantics* of each patch — the harness
# picks candidates by a deterministic rule, but only a person can say whether a box is
# actually a diffuse reflector or a mid-grey-equivalent surface.
#
# Output goes to a throwaway directory (default `../temp/patch-review`), never into
# `../nc-assets` (a Google Drive symlink) and never into the repo: these are rendered
# personal photographs, and nothing here is a committed artifact.
#
# Usage:
#   bash scripts/sigmoid-baseline/patch-review.sh <propose-output.txt> [outdir]
#
# where <propose-output.txt> is the captured stdout of
#   cargo test --release --quiet shadow_metrics::propose -- --ignored --nocapture --test-threads=1
set -uo pipefail

RAW=${1:?usage: patch-review.sh <propose-output.txt> [outdir]}
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT=${2:-$ROOT/../temp/patch-review}
NC=${NC:-$ROOT/target/release/nc}
A=${A:-$ROOT/../nc-assets}
REC="$ROOT/scripts/real-scan-verify/recipes"

for tool in sips python3; do
  command -v "$tool" >/dev/null || { echo "error: $tool not on PATH" >&2; exit 2; }
done
[ -x "$NC" ] || { echo "error: no $NC — run 'cargo build --release'" >&2; exit 2; }
[ -f "$RAW" ] || { echo "error: no such propose output: $RAW" >&2; exit 2; }

mkdir -p "$OUT"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# mark | roll | recipe stem | frame
FRAMES=$(cat <<'ROWS'
G1|2026-07-24-Gold200|2026-07-24-Gold200|20260724-leica-1137.tif
G2|2026-07-24-Gold200|2026-07-24-Gold200|20260724-leica-1144.tif
G3|2026-07-24-Gold200|2026-07-24-Gold200|20260724-leica-1151.tif
E1|Ektar|Ektar|20260713-nikon-971.tif
E2|Ektar|Ektar|20260714-nikon-989.tif
E3|Ektar|Ektar|20260714-nikon-991.tif
P1|Portra160-2026-07-22|Portra160-2026-07-22|20260722-nikon-1102.tif
P2|Portra160-2026-07-22|Portra160-2026-07-22|20260723-nikon-1111.tif
P3|Portra160-2026-07-22|Portra160-2026-07-22|20260723-nikon-1121.tif
P4|Portra160-2026-07-22|Portra160-2026-07-22|20260723-nikon-1127.tif
ROWS
)

# `--density-curve sigmoid` deliberately: it is the curve under investigation, and its
# shoulder avoids the ~10% highlight clipping the frozen exponential recipe produces —
# clipped highlights would defeat the "is this a diffuse white?" judgement the page asks.
echo "rendering previews -> $OUT"
while IFS='|' read -r mark roll stem frame; do
  [ -n "$mark" ] || continue
  if "$NC" convert "$A/rolls/$roll/$frame" -o "$TMP/$mark.tif" \
        --params "$REC/$stem.json" --density-curve sigmoid >/dev/null 2>&1 \
     && sips -Z 3000 -s format jpeg -s formatOptions 90 "$TMP/$mark.tif" \
        --out "$OUT/$mark.jpg" >/dev/null 2>&1; then
    echo "  $mark  $frame"
  else
    echo "  $mark  $frame  FAILED" >&2
  fi
  rm -f "$TMP/$mark.tif" "$TMP/$mark.tif.json"
done <<<"$FRAMES"

python3 "$HERE/build_patch_review.py" "$RAW" "$OUT/index.html"
echo
echo "open: file://$(cd "$OUT" && pwd -P)/index.html"
