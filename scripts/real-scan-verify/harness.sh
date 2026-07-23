#!/usr/bin/env bash
# Real-scan core verification harness (task: real-scan-verification).
#
# Drives the compiled `nc` binary over the user's full-size real scans and records
# derived numbers only — it NEVER reads sample pixels into an agent context.
# Rerunnable when assets or defaults change. Downstream tasks (display-output-
# acceptance, streaming-tiled-io) reuse the frozen recipes + measured peak here.
#
# Stages (pass a stage name to run one; default runs B..E):
#   classify  - grid-classify every frame per roll (unexposed / full-exp / real)
#   freeze    - measure per-roll Dmin (unexposed) + Dmax (leader), freeze recipes
#   convert   - roll-convert every real frame, 16-bit + float HDR
#   ir        - export IR plane, check --strict behaviour
#   determinism - byte-identical re-run + --params reload
#   resource  - /usr/bin/time -l peak RSS + wall-clock on the largest scan
set -uo pipefail

# Paths resolve relative to this script so the harness is portable across
# worktrees; every one is env-overridable. `nc-assets` is a sibling of the repo
# (see CLAUDE.md `../nc-assets`).
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"        # repo root (worktree)
NC=${NC:-$ROOT/target/release/nc}
A=${A:-$ROOT/../nc-assets}
OUTDIR=${OUTDIR:-$A/converted/2026-07-22}
REC="$HERE/recipes"
ART=${ART:-/private/tmp/rsv-artifacts}   # per-run report JSON (not committed)
mkdir -p "$REC" "$ART" "$OUTDIR"

# roll | unexposed(Dmin) | fully-exposed(Dmax) | real frames (space sep)
ROLLS=(
"Ektar|20260713-nikon-963.tif|20260715-nikon-1009.tif|20260713-nikon-971.tif 20260714-nikon-989.tif 20260714-nikon-991.tif"
"phoenix|20260712-nikon-933.tif|20260715-nikon-1010.tif|20260712-nikon-936.tif 20260713-nikon-956.tif 20260713-nikon-958.tif"
"Portra160|20260720-nikon-1059.tif|20260720-nikon-1058.tif|20260720-nikon-1061.tif 20260720-nikon-1065.tif 20260720-nikon-1076.tif 20260721-nikon-1089.tif"
"Portra400|20260714-nikon-994.tif|20260717-nikon-1032.tif|20260715-nikon-999.tif 20260715-nikon-1011.tif 20260716-nikon-1029.tif"
"Portra400-leica-flaw|20260719-nikon-1034.tif|20260719-nikon-1033.tif|20260719-nikon-1037.tif 20260719-nikon-1043.tif 20260720-nikon-1049.tif 20260720-nikon-1056.tif"
)

center_region() { # file -> "X,Y,W,H" for a holder-free center 40% box
  read w h <<<"$($NC inspect "$1" 2>/dev/null | jq -r '"\(.decode.width) \(.decode.height)"')"
  python3 -c "w,h=$w,$h; print(f'{round(0.3*w)},{round(0.3*h)},{round(0.4*w)},{round(0.4*h)}')"
}

stage_classify() {
  printf "%-24s %-22s %8s %6s  %s\n" FRAME ROLL cLuma agree CLASS
  for row in "${ROLLS[@]}"; do IFS='|' read -r roll uf ff reals <<<"$row"
    for f in "$A/$roll"/*.tif; do
      j=$($NC estimate --grid "$f" 2>/dev/null)
      read cr cg cb ag <<<"$(echo "$j" | jq -r '.grid.cells[4].base as $c|"\($c.r) \($c.g) \($c.b) \(.grid.agreement)"')"
      lum=$(python3 -c "print(f'{0.2126*$cr+0.7152*$cg+0.0722*$cb:.4f}')")
      cls=$(python3 -c "l=$lum;print('full-exp' if l<0.08 else ('unexposed' if '$ag'=='true' else 'real'))")
      printf "%-24s %-22s %8s %6s  %s\n" "$(basename "$f")" "$roll" "$lum" "$ag" "$cls"
    done
  done
}

stage_freeze() {
  for row in "${ROLLS[@]}"; do IFS='|' read -r roll uf ff reals <<<"$row"
    U="$A/$roll/$uf"; F="$A/$roll/$ff"
    ureg=$(center_region "$U"); freg=$(center_region "$F")
    jmin=$($NC estimate --base-region "$ureg" "$U" 2>"$ART/$roll.dmin.warn")
    dmin=$(echo "$jmin" | jq -c '.film_base'); dflag=$(echo "$jmin" | jq -r '.film_base_flag' | sed 's/--film-base //')
    jmax=$($NC estimate --film-base "$dflag" --d-max-region "$freg" "$F" 2>"$ART/$roll.dmax.warn")
    dmax=$(echo "$jmax" | jq -r '.dmax')
    jq -n --argjson b "$dmin" --argjson d "$dmax" \
      '{film_base:{source:{explicit:[$b.r,$b.g,$b.b]}},density:{dmax:{explicit:$d}}}' > "$REC/$roll.json"
    jq -n --argjson b "$dmin" --argjson d "$dmax" \
      '{film_base:{source:{explicit:[$b.r,$b.g,$b.b]}},density:{dmax:{explicit:$d}},output:{hdr:true}}' > "$REC/$roll.hdr.json"
    jq -n --arg roll "$roll" --arg uf "$uf" --arg ureg "$ureg" --arg ff "$ff" --arg freg "$freg" \
      --argjson b "$dmin" --argjson d "$dmax" \
      --arg mw "$(tr '\n' ' ' <"$ART/$roll.dmin.warn")" --arg xw "$(tr '\n' ' ' <"$ART/$roll.dmax.warn")" '{
        roll:$roll, dmin:{frame:$uf,region:$ureg,base:$b,warnings:$mw},
        dmax:{frame:$ff,region:$freg,scalar:$d,warnings:$xw},
        note:"center 40% region excludes film holder; scalars frozen for deterministic apply"
      }' > "$REC/$roll.provenance.json"
    echo "froze $roll: Dmin=$dmin Dmax=$dmax"
  done
}

stage_convert() {
  for row in "${ROLLS[@]}"; do IFS='|' read -r roll uf ff reals <<<"$row"
    ins=(); for fr in $reals; do ins+=("$A/$roll/$fr"); done
    od="$OUTDIR/$roll"; mkdir -p "$od"
    # 16-bit -> <stem>_positive.tiff (matrix default)
    $NC roll --params "$REC/$roll.json"     --out-dir "$od" "${ins[@]}" --report json > "$ART/$roll.roll16.json"  2>"$ART/$roll.roll16.err"
    # float HDR -> a temp dir, then rename to <stem>_positive_hdr.tiff so both modes coexist
    htmp="$od/.hdrtmp"; mkdir -p "$htmp"
    $NC roll --params "$REC/$roll.hdr.json" --out-dir "$htmp" "${ins[@]}" --report json > "$ART/$roll.rollhdr.json" 2>"$ART/$roll.rollhdr.err"
    for g in "$htmp"/*_positive.tiff; do [ -e "$g" ] || continue
      b=$(basename "$g" _positive.tiff); mv "$g" "$od/${b}_positive_hdr.tiff"
      [ -e "$g.json" ] && mv "$g.json" "$od/${b}_positive_hdr.tiff.json"
    done
    rmdir "$htmp" 2>/dev/null
    echo "converted $roll: $(echo $reals | wc -w | tr -d ' ') frames x2 modes"
  done
}

stage_ir() {
  # one representative real frame per matrix; export IR + --strict behaviour
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  $NC convert --params "$REC/$roll.json" --export-ir "$ART/ir-$roll.tiff" \
     -o "$ART/ir-pos-$roll.tiff" "$A/$roll/$fr" --report json > "$ART/ir.json" 2>"$ART/ir.err"
  echo "IR export ($roll/$fr):"; exiftool -s -s -s -ImageWidth -ImageHeight -BitsPerSample "$ART/ir-$roll.tiff" 2>/dev/null
  echo "--strict on same frame (expect IR-ignored warning -> hard error):"
  $NC convert --params "$REC/$roll.json" -o "$ART/strict.tiff" "$A/$roll/$fr" --strict >/dev/null 2>"$ART/strict.err"; echo "  exit=$?"; cat "$ART/strict.err"
}

stage_determinism() {
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  $NC convert --params "$REC/$roll.json" -o "$ART/det-a.tiff" "$A/$roll/$fr" --report none 2>/dev/null
  $NC convert --params "$REC/$roll.json" -o "$ART/det-b.tiff" "$A/$roll/$fr" --report none 2>/dev/null
  cmp -s "$ART/det-a.tiff" "$ART/det-b.tiff" && echo "determinism: re-run BYTE-IDENTICAL" || echo "determinism: DIFFER"
  # dump-params reload
  $NC convert --params "$REC/$roll.json" --dump-params "$ART/resolved.json" -o "$ART/det-c.tiff" "$A/$roll/$fr" --report none 2>/dev/null
  $NC convert --params "$ART/resolved.json" -o "$ART/det-d.tiff" "$A/$roll/$fr" --report none 2>/dev/null
  cmp -s "$ART/det-a.tiff" "$ART/det-d.tiff" && echo "determinism: dump-params reload BYTE-IDENTICAL" || echo "determinism: reload DIFFERS"
}

stage_resource() {
  # largest scan = a full 5184x3599 frame
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  echo "resource on $roll/$fr (16-bit):"
  /usr/bin/time -l "$NC" convert --params "$REC/$roll.json" -o "$ART/res16.tiff" "$A/$roll/$fr" --report none 2>&1 | grep -E 'real|maximum resident'
  echo "resource on $roll/$fr (float HDR):"
  /usr/bin/time -l "$NC" convert --params "$REC/$roll.hdr.json" -o "$ART/reshdr.tiff" "$A/$roll/$fr" --report none 2>&1 | grep -E 'real|maximum resident'
}

case "${1:-all}" in
  classify) stage_classify;;
  freeze) stage_freeze;;
  convert) stage_convert;;
  ir) stage_ir;;
  determinism) stage_determinism;;
  resource) stage_resource;;
  all) stage_freeze; stage_convert; stage_ir; stage_determinism; stage_resource;;
  *) echo "unknown stage: $1"; exit 2;;
esac
