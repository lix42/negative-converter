# Dmax Anchor Reliability

## Goal

Establish whether the roll-fixed `Dmax` anchor measures the quantity it is supposed to, and
fix it if not. `film-base/dmax-reference` built exactly what was specified — a scalar measured
from a fully-exposed leader — but evidence from `algo/reference-anchored-sigmoid` indicates the
*specified* quantity is not reliably a film property. This is a follow-up on a completed
task's contract, which is why it is a new task rather than an edit.

## Design

**Three independent findings, all from committed data (2026-08-02/03):**

1. **Same-stock rolls disagree by a full stop while their bases agree.** `Portra400` reads
   Dmax 1.7383 and `Portra400-leica-flaw` 1.4435 — a 0.295 density gap — while their **red
   base agrees to 0.0005**. Both quantities cannot be film properties: the base proves ±0.03
   reproducibility is achievable in this workflow, and the leader misses it by an order of
   magnitude. (The Portra 160 pair differs by only 0.046, so the leader is not *reliably*
   wrong — it is **uncontrolled**, which is worse for an anchor, because a single measurement
   cannot tell you which case you have.)
2. **Real content exceeds the anchor.** G3 measures `D′` 1.3265 against its roll Dmax of
   1.2758, and P3 1.5062 against 1.3816. The anchor does not even bound the frame.
3. **Leaders are uniform, so it is not a fogging gradient.** Interior `D′` range across the
   leader is 0.024 / 0.039 / 0.067 with L−R and T−B gradients ≤0.024. The leader is a uniform
   field at an *uncontrolled level*, and grain sensitivity means "fully exposed" is arguably
   ill-posed: insensitive grains keep responding well past where the curve looks flat, so
   reaching true Dmax can take many stops more than a leader receives.

**And the fallback is separately wrong.** `NOMINAL_DMAX = 2.0` is the shipped default when no
roll reference exists, while measured rolls span 0.90–1.74 (median ≈1.34, ≈1.36 excluding the
poor-quality Harman Phoenix). Worst-case error against 2.0 is 1.10 density — several stops —
and it makes switching between `Fixed` and `Explicit` a large unexplained rendering change.
~1.35 is a better provisional value, but n=7 rolls is too small to fix a shipped constant, and
Portra400's own 1.7383 is one of the suspect measurements.

**Do not settle the fallback number yet** (user, 2026-08-03) — more rolls are coming, and they
will also show whether Portra400's 1.7383 is a mistake or real. What *is* settled is the
**method**: when the constant is finally computed, **exclude extreme cases** rather than taking a
plain median over everything. Harman Phoenix (0.8976) is the worked example — a poor-quality
stock with a dense, non-orange base, already known to false-alarm the plausibility floor
(`film-base/dense-base-dmax-plausibility`). A fallback dragged down by an outlier stock is worse
than one computed from the mainstream population it will actually be applied to. Record the
exclusion criterion explicitly, so the choice is auditable rather than a silent judgement.

**Directions to weigh, not a predetermined fix:**

- **Measure something else.** A diffuse-white-referenced anchor is what
  `algo/reference-anchored-sigmoid` found renders correctly, and it is Dmax-free.
- **Measure the leader better** — require a *demonstrably* saturated reference, and fail
  loudly when the estimate is implausible rather than freezing it.
- **Roll-wide content measurement** as the fallback, which
  [base-acquisition planner](../core/base-acquisition-planner.md) already owns the cascade for.
- **Retire the leader anchor from the default path** and keep it as an explicit override.

Whatever is chosen, `algo` candidates 2 and 3 are *contingent* on this: candidate 3 halves a
Dmax error (`dA/dDmax = 0.5`, so 0.046 → 0.15 stop but 0.295 → 0.98 stop) and candidate 2
passes it through in full.

## How to Verify

- Two rolls of one stock, developed together, produce anchors agreeing to within the ±0.03
  the *base* already demonstrates — or the anchor is explicitly documented as not a film
  property and the default stops depending on it.
- The no-reference fallback is justified against measured rolls rather than asserted, and
  switching between fallback and measured no longer produces a multi-stop jump.
- An implausible measurement fails loudly instead of being frozen into a recipe.
- Full CI gate passes.

## Dependencies

- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)
