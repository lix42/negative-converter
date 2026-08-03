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

# `--density-curve sigmoid` deliberately: it is the curve under investigation.
#
# `--output-preset ultra-hdr-v1` is also deliberate, and it corrects an earlier mistake.
# Previews used to come from the *legacy* path (reconstruct -> finish_print ->
# color::to_output), but the acceptance bounds are measured on `pipeline::sdr::render` —
# different code, with a Hermite shoulder and radial gamut mapping instead of legacy's
# linear-space soft clip. Reviewing one renderer while measuring another is not a fair
# test. This preset's JPEG *base* IS the `pipeline::sdr` rendition, so the page now shows
# what gets measured. It also fixes a wrong finding: the legacy path clipped 7-26% at
# +EV, while this path reports clipped_high 0 at contrast 2.0 / EV +1.5.
#
# The gain map is dropped by the `sips` downscale, which is fine — these thumbnails are
# for SDR review. The Display P3 profile survives (verified: red matrix column 0.51512).
# Full-size HDR review is deferred until every candidate config is renderable, so the
# comparison can be made across all of them at once rather than on this one.
#
# White balance is left NEUTRAL (as the frozen recipe has it) even though these frames
# carry a visible blue cast. Auto-WB would be a *frame-local* operation, and the point of
# the exposure comparison below is to read per-frame differences — injecting a per-frame
# correction would confound exactly what is being measured.
echo "rendering previews -> $OUT"
while IFS='|' read -r mark roll stem frame; do
  [ -n "$mark" ] || continue
  if "$NC" convert "$A/rolls/$roll/$frame" -o "$TMP/$mark.src.jpg" \
        --params "$REC/$stem.json" --density-curve sigmoid \
        --output-preset ultra-hdr-v1 >/dev/null 2>&1 \
     && sips -Z 3000 -s format jpeg -s formatOptions 90 "$TMP/$mark.src.jpg" \
        --out "$OUT/$mark.jpg" >/dev/null 2>&1; then
    echo "  $mark  $frame"
  else
    echo "  $mark  $frame  FAILED" >&2
  fi
  rm -f "$TMP/$mark.src.jpg" "$TMP/$mark.src.jpg.json"
done <<<"$FRAMES"

# Exposure variants, for judging under/over.
#
# Why real renders rather than a CSS `filter: brightness()`: CSS filters operate on
# *encoded* sRGB values, so `brightness(2)` is not "+1 stop" — it is a non-photometric
# curve, and a variant chosen that way would not map back to any pipeline number. These
# use the real `--print-exposure` knob (a true 2^EV linear gain), so whichever variant
# reads as correctly exposed converts directly into an EV offset.
#
# Rendered at `--sigmoid-contrast 2.0`, not the shipped 1.0, because the raised black
# floor is *why* exposure is hard to judge — with no black reference a frame reads as
# neither under nor over. 2.0 is the datasheet-derived value (0.745 / Δ 0.36 ≈ 2.07), so
# the row doubles as a preview of the leading remedy. The full-frame image above each row
# stays at the shipped contrast 1.0, giving a direct contrast comparison.
#
# The sweep is TWO-SIDED. A downward-only range was a measurement error: every frame
# picked EV 0, the boundary, which means the optimum sat at or beyond it. Upward variants
# clip 7-26% of highlights at contrast 2.0 -- itself a finding, since it shows exposure is
# the wrong knob for a raised floor. Override the set with EVS="<ev>:<token> ...".
EVS=${EVS:-"-2:m20 -1.5:m15 -1:m10 -0.5:m05 0:p00 0.5:p05x 1:p10x 1.5:p15x"}
echo "rendering exposure variants (contrast 2.0)"
while IFS='|' read -r mark roll stem frame; do
  [ -n "$mark" ] || continue
  for pair in $EVS; do
    ev=${pair%%:*}; tok=${pair##*:}
    "$NC" convert "$A/rolls/$roll/$frame" -o "$TMP/v.jpg" \
       --params "$REC/$stem.json" --density-curve sigmoid \
       --sigmoid-contrast 2.0 --print-exposure="$ev" \
       --output-preset ultra-hdr-v1 >/dev/null 2>&1 \
    && sips -Z 1600 -s format jpeg -s formatOptions 85 "$TMP/v.jpg" \
       --out "$OUT/$mark-$tok.jpg" >/dev/null 2>&1 \
    || echo "  $mark EV $ev FAILED" >&2
    rm -f "$TMP/v.jpg" "$TMP/v.jpg.json"
  done
  echo "  $mark  $(echo $EVS | wc -w | tr -d ' ') variants"
done <<<"$FRAMES"

EVS="$EVS" python3 "$HERE/build_patch_review.py" "$RAW" "$OUT/index.html"
echo
echo "open: file://$(cd "$OUT" && pwd -P)/index.html"
