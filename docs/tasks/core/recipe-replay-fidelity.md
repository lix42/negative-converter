# Recipe replay fidelity for non-default behavior changes

## Goal

Decide and implement what `nc` owes a **frozen recipe whose render changed because a
non-default path's defaults moved**. Today `pipeline_version` answers this for the
*default* path only, so a recipe that opts into a non-default curve can be replayed under
a new build, carry the same `pipeline_version`, and produce different pixels.

## Background

`core/conversion-versioning` shipped the identity stamp, the `pipeline_version` label, and
the `PIPELINE_FINGERPRINTS` drift gate. Its contract is deliberately narrow — the constant
"bumps *only* when the **default** conversion behavior changes" and the gate hashes the
default recipe plus the curated golden vectors. That was the right scope for the label; it
leaves a real hole one level down.

`algo/reference-anchored-sigmoid` (2026-08-03) is the first instance and made the hole
concrete. It changed three sigmoid defaults — `contrast` 1.0 → ≈2.0687, `shoulder` 0.2 →
0.6, and a new `curve.anchor` defaulting to mid-grey placement where the previous behavior
pinned display white. The default curve is `exponential`, so:

- `PIPELINE_VERSION` correctly did **not** move, by its own documented contract;
- the drift gate correctly did **not** fail, because it stops at the default recipe;
- yet any recipe selecting `sigmoid` renders differently, even with `contrast`, `toe`,
  `shoulder` and `dmax` all pinned — because omitted keys take the *new* defaults.

That task shipped a hand-written, `--strict`-promotable warning
(`cli::sigmoid_anchor_default_warning`) for the `anchor` case, modelled on the existing
`pipeline_version_warning`. It closes that one instance and does not generalize: it names
one knob and one date in prose, and it says nothing about the `contrast` and `shoulder`
moves that have the identical property.

**Two remedies were considered and rejected there, for reasons that should not be
re-derived:**

- **Bumping `reconstruction.schema_version`** — that constant versions the schema *shape*
  and the reader checks it for **exact equality**, so bumping rejects every recipe that
  emitted the old version, including the majority selecting `exponential` that such a
  change does not touch. Trading silent reinterpretation of some recipes for hard rejection
  of all of them is not an improvement.
- **Per-schema-version default tables** (decode v1 as `white-at-dmax`) — a defensible
  design, but it cannot stop at one knob, and committing the project to maintaining
  historical defaults per version is a *policy* decision that belongs here rather than
  inside an algo task.

## Design

**This task's first deliverable is the decision, not code.** Pick one policy and write it
down where the next default change will find it; the implementation follows from the pick.
Sketched options, with what each costs:

1. **Widen the label.** `pipeline_version` bumps for any behavior change reachable from a
   recipe, not only the default path. Cheapest to state, and the gate already knows how to
   enforce a label. Cost: the version stops meaning "the default render moved", so
   comparing two default renders needs a second signal, and the gate's coverage must grow
   to the non-default paths it currently skips (the sigmoid *is* pinned by
   `pipeline::stages::golden`, but no label is keyed to it).
2. **A second label** for opt-in path behavior, leaving `pipeline_version` alone.
   Preserves the existing meaning; costs a new concept in every report and comparison.
3. **Generalize the warning.** Keep both labels as they are and make "this defaulted key's
   default moved in build X" a declared, tested table rather than a hand-written string —
   so the *next* default move cannot ship without an entry. Cheapest to live with, weakest
   guarantee: a warning is not reproducibility.
4. **Historical defaults**, keyed on schema version. Strongest fidelity — an archived
   recipe reproduces exactly — and the most expensive to maintain, since every future
   default change adds a row that must stay correct forever.

Whatever is chosen must state its answer to the questions the sigmoid instance raised:

- Does replaying an archived recipe **reproduce** its original render, or merely **say
  loudly** that it cannot? (1–3 choose the latter; 4 the former.)
- Which defaults are in scope — every `#[serde(default)]` recipe key, or only those on a
  behavior-selecting path?
- What does the **gate** cover? `PIPELINE_FINGERPRINTS` stops before lcms2 and before
  `io::{decode,encode}` and hashes only the default recipe; a policy that promises more
  than the gate enforces is a promise no CI failure will keep.

**Retrofit the known instance.** Whatever the policy, the three sigmoid defaults that moved
on 2026-08-03 are its first rows / labels, and `sigmoid_anchor_default_warning` either
becomes an instance of the general mechanism or is deleted in favour of it. Leaving a
bespoke warning beside a general mechanism is the outcome to avoid.

**Boundaries.** This is about *behavior* drift under a stable recipe, not schema evolution
(`reconstruction.schema_version` keeps versioning shape) and not the comparison metric set
(`core/conversion-versioning`'s boundary note still applies). `output/presets` will flip
the default curve to sigmoid and owns the `pipeline_version` bump *for that default
change*; this task decides what is owed to recipes that opted in **before** it became the
default.

## How to Verify

- The chosen policy is written down in `docs/design-spec.md` (§8, beside the existing
  identity/version contract) with its answers to the three questions above, and
  `PIPELINE_VERSION`'s doc comment says explicitly what it does *not* cover.
- A test proves the mechanism fires for the 2026-08-03 sigmoid defaults: a recipe frozen
  with the old values either reproduces its original render (option 4) or fails/warns
  loudly and specifically (options 1–3). `--strict` promotes any warning.
- Under the chosen mechanism, moving a defaulted non-default-path value **without** the
  corresponding label/row/entry fails a test — the same "cannot silently drift" property
  `pipeline_version` already has for the default path.
- `cli::sigmoid_anchor_default_warning` is either gone or reimplemented on top of the
  general mechanism; no bespoke per-knob warning remains beside it.
- Full CI gate green: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo build`, `cargo test`.

## Dependencies

- [Conversion versioning & baseline comparison](conversion-versioning.md) — owns the label
  and the gate this extends. Completed; this task changes what its contract *promises*, so
  it is filed as a sibling rather than reopening it (reopening would also make
  `output/presets` non-executable).
- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)
  — the instance that exposed the gap and shipped the stopgap warning.
