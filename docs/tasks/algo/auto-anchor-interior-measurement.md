# Auto Anchor: Measure the Interior, Not the Holder

## Goal

Make the content-driven anchor (`DmaxSource::Auto`) measure the **picture area** rather than
the whole scan, so it stops being dominated by the opaque film holder. Today it is unusable
on a real full-frame scan, which silently disqualifies every content-driven rendering mode.

## Design

**The defect, measured** (`algo/reference-anchored-sigmoid`, 2026-08-03): `Auto` takes the
99.5th percentile of corrected densities over the whole frame. A real scan is laid out
`dark holder → thin inset rebate → picture`, and the holder is nearly opaque — so its
transmission is at the `SCAN_EPSILON` floor and its corrected density is enormous. The
holder therefore *owns* the top percentile. On the three fixture rolls `Auto` resolved to
**2.23–2.37** against roll Dmax values of **1.28–1.38**, and every frame rendered to 0/255.

**The fix is a sampling region, not a new statistic.** `algo::density::auto_dmax` currently
strides the entire buffer. It needs the picture rectangle instead. `pipeline::film_base`
already locates the rebate by marching 1-px strips inward
(`film-base/auto-base-redesign`), so the knowledge exists in the codebase — the work is
plumbing a resolved interior region into the reconstruction stage without breaking the
"stages are pure functions" rule (the orchestrator resolves it, like the film base).

**Decide explicitly** whether the region is: (a) derived from the same detector film-base
uses, (b) a fixed conservative inset, or (c) a required explicit `--auto-region`. (b) is the
cheapest and needs no cross-epic coupling; (a) is the most correct; (c) is the most honest if
detection is unreliable. Whatever is chosen, an `Auto` anchor that lands above the roll's
plausible density range should **fail loudly** rather than render a black frame.

## Implementation Suggestion

- Reuse the harness in `pipeline::shadow_metrics` to confirm the fix: `Auto` should resolve
  near the frame's own bright content, not above Dmax.
- `auto_dmax` already strides for cost; a region restricts the walk rather than adding one.
- Watch the interaction with `film-base/ir-holder-detection`: on an IR scan the holder mask
  already exists and is a better region source than any geometric inset.

## How to Verify

- On each fixture roll, `Auto` resolves **below** the roll's reference Dmax and within the
  frame's bright content, not at 2.2+.
- A synthetic committed fixture with a deliberately opaque border proves the border is
  excluded, so the regression is caught with no external assets.
- An `Auto` result above the plausible range is a loud error, not a black image.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
  `cargo test` pass.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
- [Robust auto film-base detection](../film-base/auto-base-redesign.md)
