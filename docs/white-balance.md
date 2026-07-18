# White balance & roll consistency

Guidance on what the white-balance correction is, and how to get a **consistent
look across a roll** (frames shot on the same stock, developed together, scanned
in one session). Companion to the [`auto-neutral-wb`](tasks/auto-neutral-wb.md)
task and design-spec §8/§9.

## There are two corrections, at two stages

A frame's color is set by two independent corrections in the pipeline, and they
behave very differently across a roll:

| | What it corrects | Depends on | Roll-constant? |
|---|---|---|---|
| **Film base / Dmin** (stage 2) | The orange mask / stock cast | Stock + development + scanner | **Yes** — same for every frame |
| **White balance** (stage 4, `print.white_balance`) | Residual color cast after inversion | *Which mode you pick* | Only if you pin it |

Keeping these separate is the point. Most of "getting neutral color" is the
**film base**, which is genuinely a property of the roll, not the picture. White
balance is a smaller stage-4 gain on top.

## Does the same roll get the same correction?

**Film base — yes, and you should make it so.** The base (Dmin) depends only on
the stock, development, and scanner, so it is identical for every frame on the
roll. Measure it once from an unexposed rebate/leader and pin it with
`--film-base` (or `--base-region`) across the whole roll. `--auto-base`
re-estimates per frame and drifts slightly; for consistency, pin it.

**Auto white balance (`--auto-wb gray-world` / `percentile`) — no, by
design.** The auto modes estimate the gains *per frame* from that frame's own
pixels (`GrayWorld` equalizes per-channel trimmed means; `Percentile` equalizes
matched near-white percentiles — see `src/algo/density.rs`). Two frames on the
same roll under the same light get **different** gains whenever their content
differs — a snow scene and a sunset are pushed toward neutral differently.

Auto-WB works with both the `density` and `sigmoid` algorithms (each has a
stage-4 print white-balance slot); only `simple` lacks one. Note `GrayWorld`
equalizes the per-channel trimmed means over the **full frame** — including any
dark holder / rebate margin left in the crop — so on real scans it can be biased
toward whatever fills that margin; `Percentile`, anchored on the near-white
pixels, is the more robust choice there.

The estimate is deterministic (same frame + same mode ⇒ identical gains every
run); it is *across frames* that it varies.

## The subtlety worth naming

Content-based auto-WB **conflates two things**: the roll-constant cast (imperfect
Dmin, scanner illuminant) that you want held fixed, and the genuine per-scene
color that you don't want flattened. So if your goal is a uniform look across a
roll shot under one light, per-frame auto-WB is the wrong tool — it will actively
*introduce* inconsistency by "correcting" real scene-to-scene differences (the
classic gray-world failure on a frame dominated by a single color).

## Practical guidance

**Want roll consistency (the common case):**

1. Pin the base once — `--film-base <r,g,b>` measured from an unexposed reference.
2. Set white balance **once**, then reuse it as fixed gains for every frame:
   - either pass explicit gains directly (`--white-balance <r,g,b>`), or
   - run an auto mode on **one** representative/neutral frame, read the resolved
     gains from the convert JSON report, and freeze them into the roll recipe as
     `Explicit` gains.

   Identical correction across the roll; genuine scene-to-scene color preserved.

**Reach for per-frame auto-WB only** when frames on the roll were genuinely shot
under *different* illuminants and you want each neutralized independently.

**Choosing between the auto modes:** `Percentile` (near-white anchor) is more
robust than `GrayWorld` when a frame has a dominant color — exactly the
roll-mixed-content case — but neither is a substitute for pinning when you want a
uniform roll.

## See also

- [`auto-neutral-wb`](tasks/auto-neutral-wb.md) — the feature spec (modes,
  report round-trip, four-coupled-spots wiring).
- design-spec §8 (film base) and §9 (`--white-balance` / `--auto-wb` flags +
  recipe key).
