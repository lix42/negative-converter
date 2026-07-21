# Density Safety Bounds

## Goal

Close the gap where a validation-passing density recipe can silently produce a
degenerate (e.g. finite all-black) image. Two complementary mechanisms:

1. **Bounded parameter ranges** for `density_scale` / `density_offset` /
   `density_gamma` — physically-meaningful upper/magnitude bounds enforced at the
   CLI `validate` boundary, the direct analogue of the sigmoid degenerate-value
   bounds that density currently lacks. *(committed core)*
2. **Degenerate-output warning** — a post-render histogram / dynamic-range-collapse
   check that raises a report warning (`--strict` promotes) when the output has
   collapsed, catching degeneracy from *any* cause including the finite-black
   underflow the loss counters miss. *(higher value, needs a false-positive guard)*

> **Context — the confirmed gap.** From the density-safety review (see
> `docs/progress.md`). `validate` checks only finiteness/positivity for density
> params (`cli.rs:781-783`): `density_gamma`/`density_scale` are `positive` (no
> upper bound) and `density_offset` is `finite` only (no range). Sigmoid, by
> contrast, enforces `SIGMOID_CONTRAST_MAX` / `SIGMOID_KNEE_MAX` with documented
> photographic rationale (`cli.rs:837-855`). The render maps density through
> `10^(γ·(d−anchor))` (the stage-3 tone map at `density.rs:408`): a hugely
> *positive* density → `±inf` (caught by the encoder's non-finite counter), but a
> hugely *negative* one underflows to a finite `+0.0` — a quietly black pixel
> **no counter flags**. That tone map has no finiteness/collapse guard.
> `pixel_tone`'s non-finite skip is a *different* defense: it covers both the
> regional-balance measure **and** apply (`density.rs:350`) — see its doc at
> `density.rs:221-226` — but it does nothing for the tone-map underflow, so don't
> mistake it for the guard this task needs.

## Design

### 1. Parameter bounds (committed core)

Add upper/magnitude bounds in `validate` (`cli.rs`), mirroring the sigmoid
pattern. Define each as a named constant with a documented, generous, physically-
grounded rationale so it rejects **only** degenerate values, never legitimate
photographic ones:

- `density_gamma` — currently `> 0`; add an upper bound (a near-vertical curve
  beyond real film/print gammas is degenerate). Cite the realistic range like the
  sigmoid constants do.
- `density_scale` — per-channel density multiplier; add an upper bound (and keep
  `> 0`). A scale far beyond realistic pushes corrected density into the
  underflow/overflow regime.
- `density_offset` (field `cli.rs:239`, validated finite-only at `cli.rs:783`) —
  **must stay able to go negative**: a below-zero offset is legal (`cli.rs:253`
  notes densities can shift below zero) and is how orange-mask compensation works.
  Bound it as a **generous magnitude cap** (|offset| ≤ some multiple of a scan's
  full density range), not a positivity rule.

Bounds are physical constants, not conversion knobs — they are fixed validation
limits, not new recipe keys, so no four-spot knob wiring is needed. Failure is a
loud usage error (exit 2) naming the parameter, its value, and the bound — exactly
like the sigmoid checks.

> Consider whether the sigmoid path (which reuses `density_scale`/`density_offset`)
> should share the same bounds — keep them consistent so a value legal for one
> algorithm isn't degenerate in the other.

### 2. Degenerate-output warning (higher value; build with a false-positive guard)

After the render, before encode, inspect the output for **collapse** and, if
detected, push a report warning (`push_warning`, so `--strict` promotes it and the
JSON report records it). This is **cause-agnostic** — it catches degeneracy from
param interactions (scale × offset × gamma × film-base × dmax can collapse output
while each param is individually in range), bad film base, or bad dmax, not just
out-of-bound params — and specifically covers the finite-all-black underflow the
encoder's clamp/non-finite counters cannot see (a `0.0` is a legal in-range
sample).

Detection candidates (pick and justify against real data):
- near-zero **dynamic range** / spread of the output histogram, and/or
- an implausible fraction of samples pinned at pure black `0.0` (or pure white).

**The hard part is the false-positive guard.** A legitimately very dark (low-key)
or very bright (high-key) scan must **not** trip this. Tune the threshold against
the real `../nc-assets` scans (which include legitimately dark frames) so normal
conversions stay silent. This is a **warning, never a hard failure** on its own
(a user may genuinely want a dark result) — `--strict` is the opt-in that
promotes it. Document the threshold and its rationale in `progress.md`.

This fills a documented hole; it does **not** duplicate the encoder's clip /
non-finite counters (those count out-of-range and NaN/inf; this catches
*in-range finite collapse*).

## Constraints (must hold)

- **Determinism.** Bounds and the collapse check depend only on params / output
  values, not on wall-clock or ordering — same input + recipe ⇒ same decision.
- **Fail loudly, right severity.** Out-of-bound params ⇒ usage error (exit 2);
  degenerate output ⇒ report *warning* (not a hard error), `--strict` promotes.
  Never a silently-wrong (quietly black) image with no signal.
- **No legitimate use rejected.** Param bounds generous and cited; the output
  warning validated against real scans for zero false positives on normal frames.
- **Keep existing counters intact.** The clip / non-finite `EncodeReport` counting
  stays; this adds the finite-collapse case they miss.

## How to Verify

- A recipe with an out-of-bound `density_gamma` / `density_scale` / `density_offset`
  is rejected at `validate` with exit 2 and a message naming value + bound; a
  realistic in-range recipe passes unchanged.
- The specific review scenario — a pathological (previously validation-passing)
  `density_offset`/`density_scale` that renders a finite all-black image — is now
  either rejected by the bound **or** flagged by the degenerate-output warning
  (ideally both): construct it as a test and assert the signal fires.
- **False-positive guard:** every real `../nc-assets` scan (including the darkest)
  converts with default params and raises **no** degenerate-output warning
  (throwaway `#[ignore]` test; derived numbers only, never read sample pixels into
  context).
- The warning is present in the JSON report and promoted by `--strict`.
- Regression: normal conversions produce byte-identical output (this adds
  validation/warnings, not output changes).

## Dependencies

- [Density-domain algorithm](algo-density.md) — owns `DensityParams` and the
  acknowledged underflow gap (`algo/density.rs`); the bounds and collapse check
  attach to its parameters and output.
- [Pipeline orchestration](pipeline-orchestration.md) — owns `validate`, the
  render→report path, and `push_warning` where the degenerate-output warning is
  raised.
