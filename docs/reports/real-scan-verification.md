# Real-scan core verification

Task: [`real-scan-verification`](../tasks/real-scan-verification.md). Executed
2026-07-22 against the user's full-size real scans. **No sample pixels were read
into an agent context** — every number below is derived (JSON reports, exiftool
IFD structure, `/usr/bin/time` RSS). Rerunnable via
[`scripts/real-scan-verify/harness.sh`](../../scripts/real-scan-verify/harness.sh)
(see its `README.md` for usage).

## Verdict

The current TIFF pipeline converts all five of the user's rolls correctly and
deterministically. Estimation, IR carry, `--strict`, and determinism all behave
per spec. Three items warrant follow-up (none is a hard defect): default 16-bit
highlight clipping (4.8–10.3 %), the Harman Phoenix dense-base stock tripping the
`Dmax` plausibility floor, and a measured peak RSS (~50 MiB/MP) that runs ~1.5×
the design's sizing model. The memory measurement feeds `streaming-tiled-io`
STEP 0 (below): on the 8 GB-class target, the user's assumed 4× worst case sits
close enough to the budget that the `memory-preflight` gate is genuinely required.

## Environment

| | |
|---|---|
| Binary | `nc 0.1.0`, `--release`, worktree `real-scan-verification` |
| Build machine | Mac16,7, 48 GB (this session) |
| **Target machine** | **M3 MacBook Air (Early 2024), base 8 GB unified memory** — the lowest Air released ~2 yr ago; ~4–5 GB realistically usable after OS/GPU |
| Scanner (all assets) | Plustek OpticFilm 8300i, SilverFast 9.2.9 |
| Assets | `../nc-assets/{Ektar,phoenix,Portra160,Portra400,Portra400-leica-flaw}` |
| Converted output | `../nc-assets/converted/2026-07-22/<roll>/` (34 TIFFs, 5.3 GB) |

**All assets are HDRi**, not plain HDR. `nc inspect` reports `format: hdri`,
`ir_present: true` on every frame: the full-resolution "transparency-mask" IFD
(exiftool label) is the SilverFast IR plane (`NewSubfileType=4`,
`Photometric=BlackIsZero`, 1-sample 16-bit), exactly as design-spec §4 describes.
Standard frame is **5184×3599 ≈ 18.66 MP**; a few reference/older frames are
smaller (phoenix `933` 4666×3423; leica `1033` 3120×3305; 64bit-full fixtures
3456×2396).

## Frame classification

Auto-detected per roll from `nc estimate --grid`: the **fully-exposed leader** is
the one very-dark frame (center luma < 0.08); the **unexposed reference** is the
one spatially uniform frame (grid `agreement: true`); everything else is a real
photo. Matches the known references (`phoenix/933`+`1010`, `Ektar/1009`).

| Roll | Unexposed → Dmin | Leader → Dmax | Real frames converted |
|---|---|---|---|
| Ektar | 963 | 1009 | 971, 989, 991 |
| phoenix | 933 | 1010 | 936, 956, 958 |
| Portra160 | 1059 | 1058 | 1061, 1065, 1076, 1089 |
| Portra400 | 994 | 1032 | 999, 1011, 1029 |
| Portra400-leica-flaw | 1034 | 1033 | 1037, 1043, 1049, 1056 |

## Resolved roll calibration (frozen recipes)

Both anchors measured from a **center 40 % region** of the reference frame, which
excludes the dark film holder at the edges (the user's explicit concern). Frozen
as scalars into per-roll recipes under
[`scripts/real-scan-verify/recipes/`](../../scripts/real-scan-verify/recipes) (`<roll>.json`
= 16-bit, `<roll>.hdr.json` = float; `<roll>.provenance.json` records frame,
region, and any estimator warnings).

| Roll | Dmin (R,G,B transmission) | Dmax (scalar density) | Notes |
|---|---|---|---|
| Ektar | 0.517, 0.277, 0.190 | 1.293 | clean |
| phoenix | 0.363, 0.263, 0.425 | **0.898** | ⚠ base non-uniform (spread 0.15); ⚠ Dmax < 1.0 floor |
| Portra160 | 0.534, 0.263, 0.157 | 1.335 | clean |
| Portra400 | 0.541, 0.260, 0.157 | 1.738 | clean |
| Portra400-leica-flaw | 0.542, 0.247, 0.150 | 1.443 | clean |

Ektar/Portra bases are classic orange-mask (R > G > B). **Phoenix is atypical**
(B > R > G, dense base) — see Finding 2.

## Verification matrix

| # | Row | Result |
|---|---|---|
| 1 | **inspect** | ✅ exit 0 on all; format/dims/`ir_present`/make/model/software correct and internally consistent. |
| 2 | **estimate** | ✅ `--base-region`/`--d-max-region` yield finite, plausible anchors. `--auto-base` **fails loudly on every frame** (exit 1: "no uniform unexposed rebate band … on any edge") — correct for the holder→rebate→picture layout; Dmin must come from the unexposed reference. `--grid` cleanly separates reference frames from real ones. |
| 3 | **convert (16-bit + float)** | ✅ default u16 and `--output-hdr` float both exit 0, preserve 5184×3599 dims. Float is **byte-lossless (0 clipped / 0 non-finite on every frame)**; u16 clips **4.8–10.3 %** of samples high (Finding 1). White balance resolves to identity `[1,1,1]` (base-relative density conversion) — final neutrality is left to the user's visual review of the converted images, not a measured gray patch. |
| 4 | **IR path (HDRi)** | ✅ `--export-ir` writes a matching-dimension 5184×3599 16-bit IR TIFF; IR carried, not consumed. `--strict` promotes the IR-ignored + clipping warnings to a hard error (exit 1) per §11. |
| 5 | **Determinism** | ✅ two identical convert runs are **byte-identical**; `--dump-params` → reload via `--params` reproduces byte-identical output. |
| 6 | **Resource** | ✅ 18.66 MP frame: **~930 MiB peak RSS**, ~1.6–1.7 s wall (16-bit and float within noise). No quadratic blowup; see STEP 0. |

Per-frame clip % (16-bit): min 4.75 % (Portra400/1029), max 10.33 % (Ektar/971),
median ≈ 6.7 %. Float path: 0 on all 17 frames.

## Memory & `streaming-tiled-io` STEP 0 input

The number STEP 0 needs, measured on the real worst case:

- **Measured peak: ~930 MiB at 18.66 MP ⇒ ~50 MiB/MP** (16-bit and float alike).
  Peak scales ~linearly with pixel count (decoded RGB **+ carried IR**, algorithm
  `positive`, `to_output` clone, u16 quantize buffer all live near peak).
- This is **~1.5× the design's ~600 MB @ 18 MP model** (`memory-preflight`) — the
  model under-counts because it omits the carried IR plane and the pre-fix
  `to_output` clone. Feed this back into `memory-preflight`'s sizing model.
- **User has no oversized asset yet; assume the max is 4× a regular scan.** Two
  readings of "4×":
  - 4× **megapixels** (~75 MP) ⇒ ~**3.7 GiB** peak.
  - 4× **per-side** (~16× area, ~300 MP) ⇒ ~**15 GiB** peak.

**Go/no-go on the 8 GB Air (~4–5 GB usable):** the 4×-megapixel case (~3.7 GiB)
lands within a worrying margin of the budget; the 4×-per-side case (~15 GiB)
exceeds it outright. Either way the **`memory-preflight` gate is required now**,
and **streaming becomes a conditional GO** — justified once the user's true input
envelope is confirmed toward the upper reading, or a real >~5 GiB peak is
measured. Recommendation: ship `memory-preflight` (preflight reject + in-place
transform to shed the clone), re-measure post-fix, then re-evaluate this gate. Do
**not** build streaming purely on today's regular-scan numbers.

## Findings / follow-ups

1. **Default 16-bit clips 4.8–10.3 % of samples as highlights** (0 in float).
   With roll-fixed `Dmax` from the leader, real-frame speculars/bright regions
   land above display white and the u16 encode clips them. Likely *expected*
   (HDR retains them; SDR is a display choice), but the magnitude is well beyond
   "spot highlights." Belongs to the display-output roadmap
   (`output-presets` / `sdr-display-rendering` / `--highlight-compress`) rather
   than a new task — flag for that gate, and verify a highlight roll-off tames it.
2. **Harman Phoenix breaks two estimator heuristics** (new, untracked): its dense
   bluish base gives Dmax 0.898 (< the ≳1.0 plausibility floor) and a
   borderline-non-uniform base warning. The floor is calibrated for C41 orange
   stock. Candidate follow-up: **per-stock / dense-base `Dmax` plausibility
   handling** (relax or per-stock-calibrate the floor; possibly a Phoenix stock
   constant). Convert output for phoenix still looks sane; the warnings are the
   deliverable.
3. **Peak RSS ~1.5× the sizing model** — not a bug, but `memory-preflight` must
   widen its model to count the IR plane + clone or its preflight will approve
   runs that still approach OOM. Cross-check listed above.

No hard defects (no crashes, no silently-wrong images, no determinism breaks).

## Reusable artifacts (for downstream tasks)

- **Frozen per-roll recipes** — `scripts/real-scan-verify/recipes/<roll>{,.hdr}.json`
  + `.provenance.json`. Replayable directly via `nc roll --params`. Consumed by
  `display-output-acceptance` ("resolved Dmin/Dmax inputs").
- **Harness** — `scripts/real-scan-verify/harness.sh` + `README.md` (stages:
  classify, freeze, convert, ir, determinism, resource). The reusable harness both
  downstream tasks extend.
- **Converted images** — `../nc-assets/converted/2026-07-22/<roll>/`
  (`_positive.tiff` 16-bit + `_positive_hdr.tiff` float, each with a resolved-recipe
  `.json` sidecar). Not committed (large); regenerate with `harness.sh convert`.
