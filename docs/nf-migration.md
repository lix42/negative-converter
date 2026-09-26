# The new-flow migration — epics

The plan for moving Hanten to the design in [design-update.md](design-update.md). This
file is the **high-level** record: the strategy, the epic list, and the ordering
constraints. Tasks, dependencies and status live in `docs/TASKS.md` as usual; each
epic below is a `docs/tasks/nf-*/` directory with a `docs/progress/nf-*.md` log.

**Status:** filed 2026-09-19 — ten epics, 53 tasks, and the triage of the existing
plan (44 tasks carry over unchanged, 9 superseded, 7 retired; the superseded and
retired files keep a header and leave the active checklist). `docs/TASKS.md` is
authoritative for status and dependencies.

## Strategy

**Strangler, with the old behaviour preserved by git rather than by code.**

- A **git tag** (`pre-new-flow`) freezes the pre-migration binary; the reference is
  the `reserve` branch that starts there and may take fixes, named by commit. A
  reference rendition is produced by building it in a worktree and pointing the review
  harness at it (`scripts/reference-snapshot/README.md`), and `nctool compare` compares two
  builds the same way. Nothing in the tree has to stay alive to remain comparable —
  which is what lets `nf-retire` run early instead of last.
- **`--new-flow`** is a presence flag that selects the new chain. It is
  **scaffolding, not a feature**: CLI-only — never a recipe key — with coarse
  availability rules, one generic "this knob has no meaning under the new flow"
  rejection rather than a per-knob matrix in two places. CLI-only for a *different*
  reason than `--report` / `--telemetry` / `--max-memory`, though: those can never
  change a pixel, while this one selects which chain — and so which knobs — exist,
  which is precisely why it must stay out of the recipe rather than merely out of the
  image. Do not describe it with the operational trio's "never affects the output"
  wording.
- **The chain's defaults move piecewise, with the retirements.** Removing the
  sigmoid, the `shoulder` tone or the `Dmax` anchor *is* a default change: each
  `nf-retire` task flips the default it removes, in the same change, and pays its own
  `pipeline_version` bump. That is cheaper than one large flip and leaves no window
  where the tree carries two defaults. What makes it safe is the tag — the old
  behaviour lives there, so main's default may be a half-built chain for a while.
- **The flag's life is short.** `--new-flow` ships with `nf-core/new-flow-flag` (on
  `convert` and `roll`) and lasts until the retirements have removed the old chain;
  after that there is nothing to select. `nf-core/default-flip` is the last of those
  changes plus the flag's removal. `nf-core/stage-skeleton` built the chain it
  selects and `nf-core/minimal-end-to-end` connected it: the fixed decode feeds it and
  it wrote one destination, a Display P3 16-bit TIFF, until `nf-destinations/preset-set`
  made the destination four knobs (`--range`, `--transfer`, `--gamut`, `--container`,
  or `--film-master`). Scene correction has since
  gained white balance and exposure (`nf-scene-correction/stage`) and fit range its
  reinhard operator (`nf-display-stages/fit-range`), and the look highlight
  desaturation (`nf-look/path-to-white`, on by default). The availability refusals went live before the render did.
- **The container default is untouched by all of this**, so it moves once, when
  `nf-destinations/default-destination` says so.
- **New stages are written fresh.** See CLAUDE.md's migration rule: structure for
  the long term beats reusing the old seams, and retiring an old path is a list of
  abilities the new code may need, not a capability loss.

## The epics

| Epic | Purpose |
|---|---|
| `nf-core` | The new flow exists and can be selected |
| `nf-reconstruction` | The fixed, stock-agnostic decode |
| `nf-scene-correction` | White balance, exposure, flare — as a named stage |
| `nf-look` | The creative stage, which does not exist today |
| `nf-display-stages` | Fit range and fit gamut as real stages |
| `nf-destinations` | Where a render can go |
| `nf-calibration` | The numbers, not the machinery |
| `nf-verification` | Gates that describe the new chain |
| `nf-retire` | Remove the old paths |
| `nf-docs` | Fold the design into the spec and the guide |

### `nf-core`
`--new-flow`, the new module tree, per-knob availability rules, and a minimal
end-to-end render (reconstruction → one destination, enough to produce a file).
Ends by removing its own flag when the default flips. Depends on nothing.

### `nf-reconstruction`
Exponential as the default; `mid-at-base-offset` as the only anchor rule; a runtime
constant for `d` with its justification (today the per-stock figures exist only in a
test table, and render paths may not read the datasheet `d_min`); the calibration
half of `gamma`; `scale` as one global value; `Dmax` off the path. **Not** which
values — that is `nf-calibration`.

### `nf-scene-correction`
White balance, exposure and flare/fog removal on scene-referred values; the black
point split in two (flare here, display black in fit range); a home and a name for
what `linear_range` does today.

### `nf-look`
The stage itself — where it sits, its recipe section, its report fields — plus the
per-channel grade with a mid-grey pivot, highlight desaturation (path to white)
anchored at diffuse white, and the print-contrast half of `gamma`. Print emulation
and per-stock normalization are later additions to this stage.

**Open: an opt-in scene-range mapping.** Measuring a frame's own range and mapping it
to the output is what NLP does, and what makes it collapse on a frame filled by one
surface — but the failure is the *unbounded* stretch, not the idea. A per-frame
opt-in with a ceiling on the gain is still worth having, and it would set exposure
(scene correction) and contrast (look) from a measurement. It must never be the
default: roll consistency is the product's central promise.

### `nf-display-stages`
Fit range as one function both branches use, with reinhard as the baseline setting
and a parametric operator with a toe; one gamut-mapping implementation rather than
three; the SDR/HDR branch point and what each branch may differ in.

The toe lives here, not in reconstruction. A content-aware toe was rejected as a
*reconstruction* curve mode; shaping the approach to black at the display stage is a
different question and is open.

### `nf-destinations`
Destination presets; the Adobe RGB preset (its colorimetry, gamut and encode landed
with `output/adobe-rgb-gamut`);
the "direct" combination for external editing; encode and package wiring; memory
profiles; file-suffix rules; the two `nctool` lookup tables.

### `nf-calibration`
An early `scale` ladder against today's binary, then the `scale` and `gamma` tuning
loop against a held-fixed rendering; whether `offset` earns a value; the measurement
plumbing and what a user-facing calibration procedure would be. Carries the **release
gate**: the default cannot move until neutrality is checked against known-neutral
frames. The ladder is the without-colorchecker pass and the gate is the with-one pass;
neither replaces the other.

### `nf-verification`
The reference snapshot (the tag, the worktree, and how to drive that binary in a
review set); fingerprints rebased on the new chain; goldens for the new stages; a
benchmark set to replace the legacy-pinned one.

### `nf-retire`
`legacy` and `custom` (and with them `to_output`, ProPhoto and arbitrary ICC); the
`shoulder` and `none` display tones; the sigmoid's knees; `simple`; the `Dmax`
machinery nothing reads any more; the `print.*` rename. **Early, not last** — every
later epic is smaller once it lands.

What retires is the **reconstruction anchor** and the roll-fixed reference measured
from a leader or reference frame. Measuring a frame's own density range is a separate
capability that may return at the display stage as an opt-in (see `nf-look`) — written
fresh there if it does, not preserved here.

### `nf-docs`
`design-spec.md` (principle 2, the NC film RGB v1 contract, the curves section),
`using-nc.md` verified by running the binary, and CLAUDE.md's architecture map and
HDR framing. Runs alongside the others rather than at the end.

## Ordering constraints

- `nf-core` first; everything else depends on it.
- The stage chain builds in order: reconstruction → scene correction → look →
  display stages → destinations.
- `nf-verification`'s **reference snapshot lands before `nf-retire`**.
- `nf-retire` needs `nf-core` plus one working destination.
- `nf-calibration` mostly needs renders to judge, so it follows `nf-destinations`,
  and its release gate additionally waits on the calibration frames (an existing task
  in the `analysis` epic). **The one exception runs first:** `scale-ladder` judges the
  decode's per-channel gain against today's binary, because the new chain would
  otherwise inherit a sigmoid-era value unexamined, and the question outranks the
  chain it would be measured in.

## What carries over untouched

Colorimetry (Adobe RGB's two artifacts landed with `output/adobe-rgb-gamut`), `io::decode`, input semantics,
telemetry (a schema bump only when an enum member is removed), the memory model's
arithmetic, `tools/review-app`, and `nctool` (one row per new preset in two tables).
