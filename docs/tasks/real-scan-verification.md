# Real-Scan Core Verification

## Goal

Verify the current TIFF pipeline against the user's full-size real scans —
the 50–160 MB SilverFast HDR/HDRi assets, not just the small committed fixtures
that `pipeline-orchestration` was validated on. The deliverable is a completed
verification matrix (results recorded in `progress.md`) plus a follow-up task for
every defect found. This task intentionally does not wait for future display/HDR
outputs; the user prepares the assets.

## Design

Not a code task — a structured verification pass driving the compiled `nc`
binary. Asset locations (from CLAUDE.md): the `nc-assets` Google Drive folder
(https://drive.google.com/drive/folders/1qXE2jF3MuVnQ2sW0pGTp3URwBJuf_LV6), synced
locally to `../nc-assets/{48,64}bit-full/` (and `~/Pictures/scan/`). **Never read
image files into context**: run the binary,
inspect only exit codes, JSON reports, and derived numbers (`tiffinfo` for
structure).

Verification matrix — for each asset class (HDR 48-bit full, HDRi 64-bit full,
plus a sample of `~/Pictures/scan/`):

1. **inspect** — exit 0; `decode` block matches the file (format, dimensions,
   `ir_present`, make/model/software).
2. **estimate** — *Dmin*: `--base-region` over the film rebate / unexposed
   reference frame yields a finite, plausible base; auto-base behavior on the
   dark-holder layout fails loudly or warns per spec (record which). *Dmax*:
   acquire the roll-fixed scalar from a **fully-exposed leader** via
   `--d-max-region` (with the resolved `--film-base`), holder-excluded and
   symmetric to Dmin — record the scalar and any plausibility warning. Detecting
   *which* frame is the unexposed / fully-exposed reference belongs to
   `base-acquisition-planner`, not here; supply the frames.
3. **convert, current TIFF paths** (density, resolved Dmax) — default 16-bit TIFF
   and explicit `--output-hdr` rendered float TIFF both exit 0; dimensions,
   profile, and report are internally consistent, grays are plausibly neutral,
   and the float path preserves unclamped values reported by the current
   pipeline. Do not call this transitional print-rendered output the future
   `scene-master`.
4. **IR path (HDRi)** — `--export-ir` writes a matching-dimension IR TIFF;
   `--strict` behavior per spec.
5. **Determinism** — two identical TIFF runs are byte-identical; sidecar reloaded
   via `--params` reproduces the resolved output.
6. **Resource sanity** — wall-clock and peak memory on the largest scan are
   recorded and unsurprising (no accidental quadratic blowup; rayon scaling
   sane).

Every row gets pass/fail + the observed numbers in `progress.md`. A failure
becomes a new tracked task (via the task-update flow) rather than an ad-hoc fix
inside this one — this task's output is *knowledge*, not patches.

## Implementation Suggestion

- Script the matrix as a throwaway shell/`#[ignore]` harness so it's rerunnable
  when assets or defaults change; don't commit large outputs.
- Measure Dmin once from an unexposed reference frame per roll
  (`--base-region`), then reuse via `--film-base` — per design-spec §8; expect
  `--auto-base` to be best-effort on the holder→rebate→picture layout.
- `/usr/bin/time -l` (macOS) for the resource row.
- If the assets aren't present yet, this task is blocked on the user — say so
  rather than substituting the small fixtures (they're already covered).

## How to Verify

The matrix above is this task's definition of done: every row executed against
every asset class with results recorded in `progress.md`, and a filed follow-up
task (or explicit "none") for defects. No code changes expected; if any prove
necessary they go through their own tasks.

## Status / deliverables (executed 2026-07-23)

Matrix run against the five real rolls (all HDRi, 5184×3599). Results, resolved
per-roll Dmin/Dmax, and the memory/streaming STEP 0 number in
[`docs/reports/real-scan-verification.md`](../reports/real-scan-verification.md).
Durable artifacts for downstream tasks under
[`scripts/real-scan-verify/`](../../scripts/real-scan-verify/) (`harness.sh` +
`README.md` + frozen per-roll recipes with provenance), plus (uncommitted, large)
converted images in `../nc-assets/converted/2026-07-22/`. Follow-ups filed:
[`dense-base-dmax-plausibility`](dense-base-dmax-plausibility.md) (Phoenix);
[`conversion-analysis-tooling`](conversion-analysis-tooling.md) (harness
enhancement / asset manifest / NLP comparison); default-SDR paleness routes to the
display-output roadmap (`sdr-display-rendering`); peak-RSS model note feeds
`memory-preflight`.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
- [Display-range white anchor (Dmax)](dmax-white-anchor.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
