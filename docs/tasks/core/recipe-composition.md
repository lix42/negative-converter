# Layered recipe composition

## Goal

Let configuration be assembled from parts instead of authored as one document:
make `--params` repeatable (file or `-` for stdin), give `roll` the same per-knob
override flags `convert` has, and define one precedence chain that every command
follows.

This is what makes the pipeline-profile / roll-calibration split usable — see the
target subsection in design-spec §8, which this task implements.

## What is known

- **No schema change is needed.** Both halves are already valid recipes today,
  verified 2026-08-11: a recipe carrying only `reconstruction`/`print`/`output`
  works when the base comes from a flag, and a recipe carrying only `film_base`
  and `dmax` works with everything else defaulted. The only missing mechanic is
  that `--params` cannot be repeated.
- The precedence rule already exists in one direction — flags beat the recipe,
  **by source rather than value** (an explicit `--white-balance 1,1,1` over a
  recipe's auto mode means neutral gains, not re-estimation). Layering extends
  the same chain: `defaults < params A < params B < … < flags`.
- `roll` is currently recipe-only — `--frames`, `--out-dir`, `--params`,
  `--strict`, `--max-memory` and reporting. There is no `--film-base` on it,
  which is precisely what forces file authoring for a one-off.

## Open questions

1. **How does a later layer override a *tagged* value?** Replacing
   `film_base.source` wholesale is obvious. Less obvious is a layer that sets
   `reconstruction.curve.contrast` when an earlier layer chose a different
   `curve.type` — a deep merge would produce a curve that no layer asked for.
   Whole-object replacement per tagged node is the safer default; say so
   explicitly either way.
2. **Does an empty or all-defaults layer differ from an absent one?** It should
   not, but `film_base.source` has no default, so "absent" and "explicitly null"
   are distinguishable and probably must stay so.
3. **Should `--params -` be allowed more than once?** Reading stdin twice cannot
   work; refuse the second rather than silently reusing the buffer.
4. **Which of `convert`'s overrides make sense roll-wide?** Most do. The
   frame-local measurements (`--auto-d-max`, `--auto-balance-range`) are
   accepted today but are exactly what breaks roll consistency — see
   `core/unfrozen-auto-mode-warning`.

## How to Verify

- Two layers compose: a profile with no `calibration` plus a calibration with
  nothing else converts identically to the single merged recipe.
- Order matters and later wins, including when both layers set the same key.
- A flag still beats every layer, and still beats by *source* — the existing
  white-balance precedence test extends to the layered case.
- `--params -` reads stdin, and the piped form in design-spec §8 works end to end.
- `roll` runs with no recipe file at all, configured entirely by flags.
- The resolved config is unchanged for every existing single-`--params`
  invocation — this task adds composition, it must not move any pixel.

## Dependencies

- [CLI framework](cli-framework.md)
- [Roll conversion](roll-conversion.md)
