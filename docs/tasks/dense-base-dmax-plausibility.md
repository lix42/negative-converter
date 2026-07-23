# Stock-aware Dmax plausibility (dense-base stocks)

## Goal

Stop `nc estimate` from emitting spurious plausibility warnings on legitimately
dense- / atypical-base film stocks. The `--d-max-region` reference-Dmax
plausibility floor (and, secondarily, the unexposed-base uniformity check) are
calibrated for standard C41 orange-mask negatives; a correctly-calibrated
Harman Phoenix roll trips them even though the measured anchors are right.

## Background

Found during [`real-scan-verification`](real-scan-verification.md) (2026-07-23,
see `docs/reports/real-scan-verification.md`). Four of five rolls (Ektar, two
Portra 160/400 rolls, Portra 400 "leica-flaw") froze `Dmin`/`Dmax` cleanly.
**Harman Phoenix** tripped two warnings from a correct calibration:

- Reference `Dmax = 0.898` measured from the fully-exposed leader (`1010`) is
  flagged *"implausibly low for a fully-exposed leader (expected ≳ 1.0 density)"*.
  Phoenix's base is unusually dense **and** non-orange (measured base transmission
  `B > R > G`, ≈ 0.363/0.263/0.425, versus C41's `R > G > B` ≈ 0.54/0.26/0.16), so
  the leader-minus-base density is genuinely compressed below the C41-tuned floor.
- The unexposed frame's `--base-region` reports a borderline non-uniformity
  warning (worst per-channel spread 0.15 at the 0.15 tolerance) — Phoenix's base
  is simply less flat than C41.

Both warnings are false positives here: the leader is a real fully-exposed frame
and the base is a real unexposed frame. The `≳ 1.0` floor and the uniformity
tolerance encode a C41 assumption. Converted Phoenix output is otherwise sane.

## Design

- Make the reference-`Dmax` plausibility floor **stock-relative rather than an
  absolute `≳ 1.0`**: judge the leader-to-base density span against the measured
  base density (or a per-stock constant), so a dense base isn't penalised. A
  genuinely bad region (e.g. sampling image content, not the leader) must still
  warn — keep a loud failure mode, just anchored correctly.
- Keep it a **warning, not a hard error** (unless `--strict`), preserving the
  current fail-soft contract; the goal is to remove false alarms, not to silence
  real ones.
- Consider surfacing the resolved stock assumption / thresholds in the report so
  the decision is auditable (which floor was used and why).
- Secondarily, review whether the unexposed-base uniformity tolerance should be
  stock-aware or simply widened slightly; a borderline-at-threshold trip on a real
  unexposed frame is low-value noise.
- No change to the frozen scalar semantics or determinism — this only affects
  *warning* emission and any resolved-threshold reporting.

## How to Verify

- Phoenix `933` (unexposed) + `1010` (leader): `estimate --d-max-region` no longer
  warns on the correct region, and the frozen `Dmax` is unchanged.
- A deliberately wrong `--d-max-region` on Phoenix (image content, not the leader)
  **still** warns — the floor stays protective, just stock-relative.
- C41 stocks (Ektar / Portra) are unaffected: same anchors, no new warnings.
- Determinism and the frozen recipe values are byte-for-byte unchanged.

## Dependencies

- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md) — owns
  the plausibility floor this task makes stock-aware.
