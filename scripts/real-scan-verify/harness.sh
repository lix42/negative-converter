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
set -euo pipefail

# Paths resolve relative to this script so the harness is portable across
# worktrees; every one is env-overridable. `nc-assets` is a sibling of the repo
# (see CLAUDE.md `../nc-assets`).
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"        # repo root (worktree)
NC=${NC:-$ROOT/target/release/nc}
A=${A:-$ROOT/../nc-assets}
OUTDIR=${OUTDIR:-$A/converted/nc/2026-07-22}
REC=${REC:-$HERE/recipes}
ART=${ART:-/private/tmp/rsv-artifacts}   # per-run report JSON (not committed)
mkdir -p "$REC" "$ART" "$OUTDIR"

# roll | unexposed(Dmin) | fully-exposed(Dmax) | real frames (space sep)
#
# These calibration triples now come from the asset manifest via `nctool`, not a
# hard-coded list — one source of truth (docs/tasks/analysis/asset-manifest.md). `nctool`
# emits a triple only for rolls with a complete unexposed+leader pair, so the
# NLP-source roll (all `real`) is skipped and the original five-roll set is
# reproduced. Because frozen recipes depend only on the Dmin/Dmax frames, the
# `freeze`-stage `recipes/` are byte-identical to the former hard-coded array;
# the convert/ir/determinism stages instead run over every `real` frame the
# manifest lists (a larger/different set than the old curated list). A `while
# read` loop (not `readarray`) keeps this working on macOS's stock bash 3.2.
#
# Capture the `roles` output to a temp file and check its exit status explicitly:
# process substitution (`< <(...)`) discards the subprocess exit code, so a
# mid-stream `roles` crash (partial stdout + traceback + exit 1) would otherwise
# leave a truncated ROLLS and the harness would proceed silently on fewer rolls.
_roles_out="$ART/manifest-roles.txt"
if PYTHONPATH="$ROOT/scripts/analysis" python3 -m nctool manifest roles \
    --asset-root "$A" >"$_roles_out"; then
  _roles_rc=0
else
  _roles_rc=$?
  echo "error: 'nctool manifest roles' exited $_roles_rc (partial/failed read of $A/manifest.json)." >&2
  echo "       regenerate it first: PYTHONPATH=$ROOT/scripts/analysis \\" >&2
  echo "         python3 -m nctool manifest generate --asset-root $A" >&2
  exit 2
fi
ROLLS=()
while IFS= read -r _row; do ROLLS+=("$_row"); done < "$_roles_out"
if [ ${#ROLLS[@]} -eq 0 ]; then
  echo "error: no roll calibration triples from $A/manifest.json." >&2
  echo "       generate it first: PYTHONPATH=$ROOT/scripts/analysis \\" >&2
  echo "         python3 -m nctool manifest generate --asset-root $A" >&2
  exit 2
fi

center_region() { # file -> "X,Y,W,H" for a holder-free center 40% box
  read w h <<<"$($NC inspect "$1" 2>/dev/null | jq -r '"\(.decode.width) \(.decode.height)"')"
  python3 -c "w,h=$w,$h; print(f'{round(0.3*w)},{round(0.3*h)},{round(0.4*w)},{round(0.4*h)}')"
}

require_file() { # path description
  if [ ! -f "$1" ]; then
    echo "error: $2 was not produced: $1" >&2
    return 1
  fi
}

require_tiff() { # path description
  local magic
  require_file "$1" "$2"
  magic=$(od -An -tx1 -N4 "$1" | tr -d ' \n')
  case "$magic" in
    49492a00|4d4d002a|49492b00|4d4d002b) ;;
    *)
      echo "error: $2 is not a TIFF container (bad magic $magic): $1" >&2
      return 1
      ;;
  esac
}

require_json_object() { # path description
  require_file "$1" "$2"
  if ! jq -e 'type == "object"' "$1" >/dev/null; then
    echo "error: $2 is not a valid JSON object: $1" >&2
    return 1
  fi
}

require_sidecar() { # path description
  require_file "$1" "$2"
  if ! jq -e '
      type == "object" and
      (.meta | type == "object") and
      (.params | type == "object")
    ' "$1" >/dev/null; then
    echo "error: $2 is not a valid sidecar envelope with object-valued meta and params: $1" >&2
    return 1
  fi
}

reject_directory_target() { # path description
  # `-d` follows symlinks, so this rejects both directories and directory
  # symlinks before `mv` can silently publish the source *inside* one.
  if [ -d "$1" ]; then
    echo "error: $2 publication target is a directory: $1" >&2
    return 1
  fi
}

normalize_roll_report() { # report staging-prefix final-dir hdr-suffix(bool)
  local report=$1 staging=$2 final=$3 hdr=$4 normalized
  normalized=$(mktemp "$ART/.roll-report.XXXXXX")
  if [ "$hdr" = true ]; then
    jq --arg staging "$staging/" --arg final "$final/" '
      (.frames[] | select(.output | startswith($staging)) | .output) |=
        ($final + (split("/")[-1] | sub("_positive\\.tiff$"; "_positive_hdr.tiff")))
    ' "$report" > "$normalized"
  else
    jq --arg staging "$staging/" --arg final "$final/" '
      (.frames[] | select(.output | startswith($staging)) | .output) |=
        ($final + (split("/")[-1]))
    ' "$report" > "$normalized"
  fi
  mv "$normalized" "$report"
}

require_report_output() { # report path description
  if ! jq -e --arg output "$2" \
      '[.frames[] | select(.status == "ok" and .output == $output)] | length == 1' \
      "$1" >/dev/null; then
    echo "error: $3 is not represented exactly once as a successful report frame: $2" >&2
    return 1
  fi
}

require_entry_count() { # directory expected-count description
  local actual
  actual=$(find "$1" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')
  if [ "$actual" -ne "$2" ]; then
    echo "error: $3 produced $actual staging entries; expected exactly $2" >&2
    find "$1" -mindepth 1 -maxdepth 1 -print >&2
    return 1
  fi
}

stage_classify() {
  printf "%-24s %-22s %8s %6s  %s\n" FRAME ROLL cLuma agree CLASS
  for row in "${ROLLS[@]}"; do IFS='|' read -r roll uf ff reals <<<"$row"
    # Match both .tif and .tiff (list_imgs / the manifest accept either); the
    # `-e` guard is nullglob-safe on bash 3.2 (an unmatched glob stays literal, so
    # skip it rather than passing a bogus path to `nc estimate`).
    for f in "$A/rolls/$roll"/*.tif "$A/rolls/$roll"/*.tiff; do
      [ -e "$f" ] || continue
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
    U="$A/rolls/$roll/$uf"; F="$A/rolls/$roll/$ff"
    ureg=$(center_region "$U"); freg=$(center_region "$F")
    jmin=$($NC estimate --base-region "$ureg" "$U" 2>"$ART/$roll.dmin.warn")
    dmin=$(echo "$jmin" | jq -c '.film_base'); dflag=$(echo "$jmin" | jq -r '.film_base_flag' | sed 's/--film-base //')
    jmax=$($NC estimate --film-base "$dflag" --d-max-region "$freg" "$F" 2>"$ART/$roll.dmax.warn")
    dmax=$(echo "$jmax" | jq -r '.dmax')
    # `output.preset` is stated, not defaulted: the product default is `gain-map-hdr`
    # (a JPEG) since pipeline_version 3, and this harness converts to TIFFs
    # throughout. `output.depth` replaced the removed `output.hdr` bool, and is
    # consulted only by the non-atomic presets — hence `legacy` on both.
    jq -n --argjson b "$dmin" --argjson d "$dmax" \
      '{film_base:{source:{explicit:[$b.r,$b.g,$b.b]}},reconstruction:{type:"density",curve:{type:"exponential",dmax:{explicit:$d}}},output:{preset:"legacy"}}' > "$REC/$roll.json"
    jq -n --argjson b "$dmin" --argjson d "$dmax" \
      '{film_base:{source:{explicit:[$b.r,$b.g,$b.b]}},reconstruction:{type:"density",curve:{type:"exponential",dmax:{explicit:$d}}},output:{preset:"legacy",depth:"f32"}}' > "$REC/$roll.hdr.json"
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
    ins=(); for fr in $reals; do ins+=("$A/rolls/$roll/$fr"); done
    od="$OUTDIR/$roll"; mkdir -p "$od"
    # Both modes render into a fresh staging tree. Persistent outputs from an
    # earlier run therefore cannot hide a missing/wrong-suffix result, and no
    # output is published until the complete expected set has been verified.
    run_tmp=$(mktemp -d "$od/.rsv-$roll.XXXXXX")
    u16tmp="$run_tmp/u16"; htmp="$run_tmp/hdr"
    mkdir -p "$u16tmp" "$htmp"

    $NC roll --params "$REC/$roll.json" --out-dir "$u16tmp" "${ins[@]}" \
      --report json > "$ART/$roll.roll16.json" 2>"$ART/$roll.roll16.err"
    $NC roll --params "$REC/$roll.hdr.json" --out-dir "$htmp" "${ins[@]}" \
      --report json > "$ART/$roll.rollhdr.json" 2>"$ART/$roll.rollhdr.err"

    require_json_object "$ART/$roll.roll16.json" "$roll 16-bit roll report"
    require_json_object "$ART/$roll.rollhdr.json" "$roll float roll report"

    frame_count=0
    for fr in $reals; do
      name=$(basename "$fr"); stem=${name%.*}; frame_count=$((frame_count + 1))
      require_tiff "$u16tmp/${stem}_positive.tiff" "16-bit TIFF for $roll/$fr"
      require_sidecar "$u16tmp/${stem}_positive.tiff.json" "16-bit sidecar for $roll/$fr"
      require_tiff "$htmp/${stem}_positive.tiff" "float TIFF for $roll/$fr"
      require_sidecar "$htmp/${stem}_positive.tiff.json" "float sidecar for $roll/$fr"
    done
    expected_entries=$((frame_count * 2))
    require_entry_count "$u16tmp" "$expected_entries" "$roll 16-bit conversion"
    require_entry_count "$htmp" "$expected_entries" "$roll float conversion"

    # Validate raw report provenance before publishing anything. If nc changes
    # the roll-report schema or omits a successful frame, keep all images staged.
    for fr in $reals; do
      name=$(basename "$fr"); stem=${name%.*}
      require_report_output "$ART/$roll.roll16.json" "$u16tmp/${stem}_positive.tiff" \
        "staged 16-bit TIFF for $roll/$fr"
      require_report_output "$ART/$roll.rollhdr.json" "$htmp/${stem}_positive.tiff" \
        "staged float TIFF for $roll/$fr"
    done

    # Check every final name before moving anything. `mv src existing-dir` would
    # otherwise succeed by nesting the source and leave a false-looking result.
    for fr in $reals; do
      name=$(basename "$fr"); stem=${name%.*}
      reject_directory_target "$od/${stem}_positive.tiff" "16-bit TIFF for $roll/$fr"
      reject_directory_target "$od/${stem}_positive.tiff.json" "16-bit sidecar for $roll/$fr"
      reject_directory_target "$od/${stem}_positive_hdr.tiff" "float TIFF for $roll/$fr"
      reject_directory_target "$od/${stem}_positive_hdr.tiff.json" "float sidecar for $roll/$fr"
    done

    for fr in $reals; do
      name=$(basename "$fr"); stem=${name%.*}
      mv "$u16tmp/${stem}_positive.tiff" "$od/${stem}_positive.tiff"
      mv "$u16tmp/${stem}_positive.tiff.json" "$od/${stem}_positive.tiff.json"
      mv "$htmp/${stem}_positive.tiff" "$od/${stem}_positive_hdr.tiff"
      mv "$htmp/${stem}_positive.tiff.json" "$od/${stem}_positive_hdr.tiff.json"
    done

    # Reports are durable artifacts too: replace their now-deleted staging paths
    # with the published paths, then revalidate both publication and provenance.
    normalize_roll_report "$ART/$roll.roll16.json" "$u16tmp" "$od" false
    normalize_roll_report "$ART/$roll.rollhdr.json" "$htmp" "$od" true
    for fr in $reals; do
      name=$(basename "$fr"); stem=${name%.*}
      require_tiff "$od/${stem}_positive.tiff" "published 16-bit TIFF for $roll/$fr"
      require_sidecar "$od/${stem}_positive.tiff.json" "published 16-bit sidecar for $roll/$fr"
      require_tiff "$od/${stem}_positive_hdr.tiff" "published float TIFF for $roll/$fr"
      require_sidecar "$od/${stem}_positive_hdr.tiff.json" "published float sidecar for $roll/$fr"
      require_report_output "$ART/$roll.roll16.json" "$od/${stem}_positive.tiff" \
        "published 16-bit TIFF for $roll/$fr"
      require_report_output "$ART/$roll.rollhdr.json" "$od/${stem}_positive_hdr.tiff" \
        "published float TIFF for $roll/$fr"
    done
    rmdir "$u16tmp" "$htmp" "$run_tmp"
    echo "converted $roll: $frame_count frames x2 modes"
  done
}

stage_ir() {
  # one representative real frame per matrix; export IR + --strict behaviour
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  $NC convert --params "$REC/$roll.json" --export-ir "$ART/ir-$roll.tiff" \
     -o "$ART/ir-pos-$roll.tiff" "$A/rolls/$roll/$fr" --report json > "$ART/ir.json" 2>"$ART/ir.err"
  echo "IR export ($roll/$fr):"; exiftool -s -s -s -ImageWidth -ImageHeight -BitsPerSample "$ART/ir-$roll.tiff" 2>/dev/null
  echo "--strict on same frame (expect IR-ignored warning -> hard error):"
  if $NC convert --params "$REC/$roll.json" -o "$ART/strict.tiff" \
      "$A/rolls/$roll/$fr" --strict >/dev/null 2>"$ART/strict.err"; then
    echo "error: --strict unexpectedly succeeded; expected the IR-ignored warning to fail" >&2
    return 1
  else
    strict_rc=$?
  fi
  if [ "$strict_rc" -ne 1 ]; then
    echo "error: --strict exited $strict_rc; expected warning-promotion exit 1" >&2
    cat "$ART/strict.err" >&2
    return 1
  fi
  if ! grep -Fq 'input carries an IR plane; it is preserved but not used' "$ART/strict.err" ||
      ! grep -Fq 'error: --strict:' "$ART/strict.err"; then
    echo "error: --strict exit 1 lacked the expected IR-ignored/strict diagnostic" >&2
    cat "$ART/strict.err" >&2
    return 1
  fi
  echo "  exit=$strict_rc"; cat "$ART/strict.err"
}

stage_determinism() {
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  $NC convert --params "$REC/$roll.json" -o "$ART/det-a.tiff" "$A/rolls/$roll/$fr" --report none 2>/dev/null
  $NC convert --params "$REC/$roll.json" -o "$ART/det-b.tiff" "$A/rolls/$roll/$fr" --report none 2>/dev/null
  if cmp -s "$ART/det-a.tiff" "$ART/det-b.tiff"; then
    echo "determinism: re-run BYTE-IDENTICAL"
  else
    echo "error: determinism re-run differs" >&2
    return 1
  fi
  # dump-params reload
  $NC convert --params "$REC/$roll.json" --dump-params "$ART/resolved.json" -o "$ART/det-c.tiff" "$A/rolls/$roll/$fr" --report none 2>/dev/null
  $NC convert --params "$ART/resolved.json" -o "$ART/det-d.tiff" "$A/rolls/$roll/$fr" --report none 2>/dev/null
  if cmp -s "$ART/det-a.tiff" "$ART/det-d.tiff"; then
    echo "determinism: dump-params reload BYTE-IDENTICAL"
  else
    echo "error: dump-params reload differs" >&2
    return 1
  fi
}

stage_resource() {
  # Benchmark the WORST CASE: samples/largest.tif (74.6 MP), the largest scan
  # available — this is what feeds memory-preflight / streaming-tiled-io. Fall
  # back to the first Ektar real frame (~18.7 MP) only if largest.tif is absent.
  IFS='|' read -r roll uf ff reals <<<"${ROLLS[0]}"; fr=$(echo $reals|awk '{print $1}')
  local big="$A/samples/largest.tif"; local label="samples/largest.tif (74.6 MP)"
  if [ ! -e "$big" ]; then big="$A/rolls/$roll/$fr"; label="$roll/$fr (~18.7 MP; largest.tif missing)"; fi
  echo "resource on $label (16-bit):"
  /usr/bin/time -l "$NC" convert --params "$REC/$roll.json" -o "$ART/res16.tiff" "$big" --report none 2>&1 | grep -E 'real|maximum resident'
  echo "resource on $label (float HDR):"
  /usr/bin/time -l "$NC" convert --params "$REC/$roll.hdr.json" -o "$ART/reshdr.tiff" "$big" --report none 2>&1 | grep -E 'real|maximum resident'
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
