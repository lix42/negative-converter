# Mask the holder, then estimate from a single population

## Goal

Make `Dmin` and `Dmax` measurement sample only film: mask the holder per edge when
IR permits, fall back to a fixed fraction when it does not — and once the sampled
region is a single population, estimate its **centre** rather than reaching for an
extreme percentile.

**Pixel change.** The base is the divisor of the whole conversion, so this owes a
`pipeline_version` bump. Masking and the estimator ship together for that reason:
split, they cost two bumps and two baselines for one conceptual change.

## Why the estimator must change with the mask

p97 exists to perform **population selection**: on a rebate strip the region is a
*mixture*, unexposed film is the sub-population with the largest transmission, and
an extreme percentile reaches past the contaminants to land in it.

Masking removes that premise. A masked unexposed frame is a *single* population,
and an extreme percentile then just lands in its noise tail — biased by
construction, roughly +2σ in transmission and therefore an *understated* density,
since `D = −log10(t/t_base)` is strictly decreasing.

Measured on the Gold 200 leader (2026-08-11), p97 sits 0.046 density from p50 —
0.16 stops once `dA/dR = f = 0.5` carries it into the sigmoid anchor, systematic
across the roll and in the "pale" direction the sigmoid work exists to fix.

`reference_dmax` already samples at p = 0.5 for exactly this reason. This task
brings `Dmin` onto the same rule, keeping p97 for the paths that are still
mixtures (`--auto-base` rebate strips, an untrusted user rectangle).

## What is known

- **Holder depth is small and asymmetric.** On the unexposed HP5 frame IR clears
  at ~2% of the short edge on the right, ~3% top and bottom, ~5% left. A single
  rectangular crop must take the worst edge; per-edge masking need not, and
  `EdgeHolderMask` already expresses per-edge segments.
- **The fallback is a first-class path, not a rare one.** For silver stock IR can
  never separate the holder on a *leader*, so every silver `Dmax` measurement
  takes it. It deserves a reported, deliberate value rather than a safety net.
- **Existing fractions to align with, not multiply:** `REBATE_SCAN_FRAC = 0.10`
  and `IR_HOLDER_PROBE_FRAC = 0.005`.
- **The spread within a leader is grain and scanner noise**, not defects — smooth,
  symmetric, no discontinuity — and the median of ~40k samples is reproducible to
  1.4e-4 density on split halves. The wide distribution does not threaten the
  estimate; only a non-central statistic does.
- Provenance is **per-run**: record whether the holder was masked, and how, in the
  report and the existing output sidecar. No persisted pre-processed input.

## Open questions

1. **Which central estimator** — median, or a trimmed mean, and trimmed where? On
   a distribution this symmetric they agree to a few thousandths, so pick for
   robustness against the asymmetric case rather than for the symmetric one.
2. **The fallback fraction.** Measured need is 2–5%; `REBATE_SCAN_FRAC` is 10%.
   Reuse it, or introduce a measurement-specific value and justify it.
3. **Memory.** Materialising the whole masked region is ~900 MB of `Vec<f32>` on a
   75 MP frame. A 16-bit histogram per channel gives exact percentiles and a
   trimmed mean in O(1) — expected to be the shape here, which means
   `pipeline::memory` gets *new* numbers rather than the current `12·s` term.
4. **Does `--auto-base` change at all?** Its strips stay mixtures, so its estimator
   should not — but confirm it still shares the masking.

## How to Verify

- A masked unexposed frame and the same frame with the holder manually cropped
  away produce the same base, to within the estimator's reproducibility.
- The holder contributes nothing: a synthetic frame with a deliberately extreme
  holder value yields the same base masked as it does cropped.
- `--auto-base` rebate-strip results are unchanged — the mixture path keeps p97.
- Per-edge asymmetry is exercised: a fixture whose holder is deeper on one edge is
  masked per edge, not to the worst edge everywhere.
- Silver-leader `Dmax` takes the fallback and **says so** in the report.
- The `pipeline_version` bump has its fingerprint row, and the report/sidecar
  record the masking provenance.

## Dependencies

- [Decide IR usability by measurement](ir-usability-detection.md)
- [Conversion versioning and baseline comparison](../core/conversion-versioning.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md) — this task
  changes `reference_dmax` sampling, which that task introduced
