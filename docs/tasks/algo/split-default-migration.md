# Activate the reconstruction / render split as the default

## Goal

Make the split the shipped default: reconstruction stops shaping tone, the display
operator carries the character. That is a `pipeline_version` bump with a
before/after report — the part `algo/reconstruction-render-curve-split`
deliberately left out of its own scope.

## Why

The split is decided, not speculative. `algo/reconstruction-render-curve-split`
reached a positive verdict on seven real frames at matched lightness, over a user
visual verdict from `output/display-tone-mapping`. Everything needed to render it
already ships and is reachable from the CLI today; what has not happened is making
it what `nc convert` does with no flags.

It is a separate task because activation inherits a problem the split itself does
not own — see the blocker below — and because a default migration is its own kind
of work: a version bump, a drift-gate row, a measured report, and a guide update.

## Open questions

- **How much of the shape moves?** The measured answer is "both knees off", but the
  migration has to decide whether the default anchor placement moves with it. Keeping
  `MidAtDmaxFraction(0.5)` costs a measured ~0.21–0.28 EV of exposure bias against the
  lightness-matched anchor — inside what `print.print_exposure` corrects, so plausibly
  fine, and it avoids shipping the uncalibrated 0.626 offset as a constant.
- **Is the default display tone the same one?** `--display-tone reinhard` at the
  6-stop default is what was reviewed. Its **1.000-stop cost at diffuse white** is a
  rendering-intent call that was accepted for one rendition and explicitly not
  endorsed as a general default. A migration has to make that call, and no
  measurement can decide it.
- **What does the report have to say?** The split changes what a stage *does*, which
  is CLAUDE.md's "fifth spot" — check the prose claims, not just the values.
- **Does anything downstream assume the old bound?** Reconstruction currently holds
  `lin ≤ 1.0` under a positive shoulder; without it the master and both display
  sources go over-range by design.

## Known vs unknown

**Known:** the rendition is reachable today (`--sigmoid-shoulder 0 --display-tone
reinhard`); `film-master` accepts it at exit 0 and needs no change; the gain map goes
live and its plateau share improves 10–25x; `PIPELINE_FINGERPRINTS` needs a new row
and a historical row must never be edited in place.

**Unknown:** whether the per-channel neutrality fix lands close enough in shape to
what this assumes, and whether the diffuse-white cost survives review as a default
rather than as an option.

## The blocker, and why it is a real one

**`film-base/dmax-per-channel-reduction` must land first.** The sigmoid's shoulder
was *hiding* a model error: measured on the uniformly-exposed leader — a target with
no scene content, so every deviation is model error — Gold reads B/G **1.826**, Portra
R/G **1.676**, Ektar B/G 1.170, i.e. 17–83% off neutral on a grey target. The shoulder
washes highlights toward white and drains the cast along with the detail; shoulder-less,
it survives into the highlights. That task's own analysis calls the per-channel term
"redundant under the exponential, not under the sigmoid, **which is the intended
default**" — a premise this migration overturns. Shipping the split as the default
before it lands means shipping a visible cast on Gold and Portra.

## How to Verify

- A `pipeline_version` bump with its own `PIPELINE_FINGERPRINTS` row, and a
  before/after report under `docs/reports/`.
- Neutrality checked on the leader for each stock, since that is the thing the old
  default was hiding.
- `docs/using-nc.md` updated by running the binary, not by reading the diff.

## Dependencies

- [Reconstruction / render curve split](reconstruction-render-curve-split.md)
- [Per-channel Dmax and the gray-mean reduction](../film-base/dmax-per-channel-reduction.md)
