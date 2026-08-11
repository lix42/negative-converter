# Value-domain terminology & Dmin/Dmax clarity

## Goal

Make nc's value-domain terminology — especially `Dmin`/`Dmax` — easy to
understand, use, and maintain for **both people and agents**. This is a clarity /
representation task: it improves understandability, operability, and
maintainability **without changing the data flow** (the pipeline stages and the
transformations they perform). It may well change code — e.g. splitting two values
into four, new value types, or CLI/recipe surface — it just does not re-route the
pipeline.

## Why

The design-spec §4 "Terminology & value domains" content is correct but buried in
the spec and awkward to reference. And `Dmin`/`Dmax` are a recurring source of
**human** confusion (agents cope better): they are named as a pair but live in two
different measurement systems — `Dmin` is a per-channel *transmission*, `Dmax` is a
scalar *density*. This has had to be re-explained repeatedly.

## Scope (kept deliberately high-level — brainstorm specifics when executing)

1. **Extract the terminology.** Pull "Terminology & value domains" out of
   `design-spec.md` into its own standalone doc so people and agents can track and
   reference it directly (design-spec links to it). Add an **agent skill** so agents
   consistently use the correct terms in naming and docs.
2. **Clearer Dmin/Dmax definition.** Give them a human-friendly definition that, at
   minimum, stops pairing two different measurement systems under similar names.
   Possibly introduce explicit named values/terms (e.g. `TransClear` /
   `TransBlocking` alongside `Dmin` / `Dmax`) — to be decided at execution.

3. **The CLI/recipe surface is inconsistent on the same axis.** `--film-base`
   names the *thing*; `--d-max` names the *quantity* — and `--base-region` /
   `--d-max-region` inherits it. One concrete candidate from the 2026-08-11
   Dmin/Dmax discussion: **`--film-base` + `--reference-density`** (with
   `--base-region` / `--reference-region`). The argument for it is that the
   codebase already moved — the sigmoid anchor split made `curve.dmax` the roll's
   *reference density* and `curve.anchor` the placement, so source comments, the
   baseline report and `docs/using-nc.md` all say "reference density" while the
   flag still says `d-max` and its help text still calls it a "display-white
   anchor", which has been wrong since that split.
   **Avoid a symmetric pair** (`--d-min`/`--d-max`, or base/ceiling): `Dmin` is a
   per-channel *transmission* and `Dmax` a scalar *density*, so symmetric naming
   would encode into the CLI exactly the conflation this task exists to remove.
   A rename spans flags, recipe keys, report keys, `DmaxSource`, `NOMINAL_DMAX`
   and the docs together; nc is unreleased, so it is cheapest now.

## Constraints

- **Preserve the data flow.** The pipeline stages and the transformations they
  perform must not change. Code changes are expected (new value types/terms,
  CLI/recipe surface, mechanical renames) — this is about how values are
  *represented and named*, not how data moves through the pipeline.
- Terminology and definitions must stay **stock-general** — nc supports extreme
  stocks (e.g. Harman Phoenix) even though it does not optimize for them.

> Intentionally light on detail: the concrete term set and doc/skill structure are
> to be brainstormed when the task is picked up.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
