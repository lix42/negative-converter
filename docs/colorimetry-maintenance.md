# Colorimetry maintenance

How to add or update a colour space in NC without leaving stale derived
coefficients behind, and how to tell a representation-only refactor from a pixel
change.

The code lives in `src/pipeline/colorimetry/`:

| File | Holds | Edit it when |
| --- | --- | --- |
| `definitions.rs` | **Standard definitions** — primaries, white points, cone-response matrices, transfer constants, normatively tabulated vectors, each with its standard and edition | A standard revises a value, or you are adding a colour space |
| `pinned.rs` | **Derived artifacts** — the reviewed `f32`/`f64` matrices and luma vectors the runtime multiplies by | Only as a deliberate, reviewed pixel change (see step 6) |
| `derive.rs` | The canonical binary64 derivation. `#[cfg(test)]`, so the runtime can never derive | You are changing *how* something is derived, not what from |
| `audit.rs` | The check/regeneration command | You are adding an artifact to the catalog |
| `tests.rs` | **Verification** — tolerances, invariants, independent reference vectors | Always: a new artifact needs an independent anchor |
| `derived-artifacts.txt` | Generated audit record | Never by hand — regenerate it |

Product policy (reference white, peak luminance, shoulder, gamut policy,
gain-map offsets) deliberately stays with the stage that owns it. Those
constants should *refer* to a named colour space, not restate its colorimetry.

## The command

```sh
cargo test colorimetry::audit                          # check mode
NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit   # regeneration mode
```

Check mode runs inside the ordinary `cargo test` gate, so CI exercises it on
every PR — there is no separate step to forget, and no Python in the loop.

Regeneration rewrites **only** `derived-artifacts.txt`. It never rewrites
`pinned.rs`. That asymmetry is the safety property: the generator can only
produce a diff for you to review, never silently move a coefficient the renderer
uses. Because the audit file records the shipped values too, editing a literal in
`pinned.rs` without regenerating also fails the check — staleness is caught in
both directions.

The derivation uses only IEEE-754 binary64 `+ - * /`, so the generated text is
identical on macOS/aarch64 and Linux/x86_64. Unlike a whole-frame checksum (see
CLAUDE.md), this artifact is safe as a cross-platform CI gate.

## The workflow

### 1. Identify the exact standard revision

Record which standard and edition changed, and what the old and new source
values are. If the citation currently in `definitions.rs` stops at the edition
without a clause number, this is the moment to confirm it against the actual
standard text and tighten it.

### 2. Edit the named source definition — never a matrix literal

Change the chromaticity, white point, or cone-response matrix in
`definitions.rs`. Do not "fix" a number in `pinned.rs` to match something you
computed elsewhere; that inverts the dependency this module exists to establish.

Adding a colour space means: a `ColorSpace` in `definitions.rs`, an entry in
`audit.rs`'s `catalog()`, a pinned artifact if the runtime needs a matrix for
it, and an independent anchor in `tests.rs`.

### 3. Run the command in check mode

```sh
cargo test colorimetry::audit
```

It will fail — that is the point. The failure names the first line that moved.

### 4. Regenerate, then read the diff

```sh
NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit
git diff src/pipeline/colorimetry/derived-artifacts.txt
```

Read the `ulps=` column. It reports `derived − shipped` on a monotonic ordering
of the `f32` line, so a **positive** value means the derivation sits *above* the
shipped literal (for a negative entry, that is the smaller magnitude) and a
negative value means it sits below. This is a review step, not a formality:

- **`ulps` still 0 or ±1 everywhere** — the source change was below the
  precision the pinned coefficients can express. Nothing in `pinned.rs` needs to
  move. Commit the regenerated artifact. **But read the warning below before
  calling it representation-only.**
- **`ulps` moved by more than that** — the source change is real and visible at
  `f32`. Continue to step 6; you are making a pixel change.

> ### ⚠ `pinned.rs` is not the only runtime consumer of a definition
>
> `pipeline::color` feeds `definitions::{REC709, DISPLAY_P3, ACESCG, PROPHOTO}`
> **directly** into Little CMS profile construction. A change to one of those
> four therefore alters embedded ICC bytes and every lcms2-transformed pixel
> *even when every `ulps` column stays at 0* and `pinned.rs` never moves.
>
> Nothing automated will catch that. `version::PIPELINE_FINGERPRINTS` stops
> before lcms2 by design (see CLAUDE.md), and the audit only compares pinned
> artifacts against the derivation — neither looks at a profile.
>
> **So: if you touched `REC709`, `DISPLAY_P3`, `ACESCG`, or `PROPHOTO`, treat it
> as a pixel change and go to step 6 regardless of the ulp column.** Run the
> before/after output comparison in step 5 including `--output-profile` for every
> affected space. The luma vectors, cone-response matrices, and transfer
> constants have no such second path; for those the ulp column is the whole
> story.

Calibration for judging "how big is big": the chromaticities are specified to
three decimals, and perturbing one primary by its own ±5e-4 rounding moves
matrix entries ~3,500 ulps. A handful of ulps is noise in the derivation route;
thousands means the colour space genuinely changed.

### 5. Run the verification suite and the quality gates

```sh
cargo test colorimetry
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
```

`colorimetry::tests` is what catches a mistyped primary — the audit alone cannot,
because it compares your derivation against your own definitions. If you added a
space, make sure you added an anchor that does not share a source with it.

Do a same-machine before/after comparison on real pixels for any change that
reaches `pinned.rs` **or that touches one of the four colour spaces Little CMS
consumes** (see the warning in step 4). Build the binary before and after — a
`git worktree add --detach <tmp> <base>` gives you a clean "before" without
disturbing your tree — convert the same fixture through `legacy`, `film-master`,
`ultra-hdr-v1`, and an explicit `--output-profile` for each affected space, and
compare output checksums.

### 6. Decide: representation-only, or a pixel change?

**Representation-only** (moved the definition, `pinned.rs` untouched, outputs
bit-identical): commit. No version bump.

**Pixel change** (a runtime literal moves): this needs the full treatment —

- a `pipeline_version` decision with `core/conversion-versioning`;
- a new row in `version::PIPELINE_FINGERPRINTS` — **never** edit a historical
  row's `render` hash in place, which would make one version label two
  behaviours;
- a baseline/report refresh under `docs/reports/`;
- a design-spec update if the change is user-visible;
- for the NC film RGB v1 mapping specifically: a **new identifier**. `v1` is
  frozen. It must never be silently altered.

### 7. Record the decision

Append a dated entry to `docs/progress/color.md` under
`## colorimetry-source-of-truth` (the log is append-only) stating the standard
revision, the observed ulp movement, and which branch of step 6 you took and
why.

## Two things that will surprise you

**There are two Bradford conventions in the tree, on purpose.**
`BRADFORD` uses the exact `f64` inverse of the cone-response matrix and is
canonical for new artifacts. `BRADFORD_PUBLISHED_INVERSE` pairs it with
Lindbloom's printed 7-decimal inverse, and exists solely because
`NC_FILM_RGB_V1_TO_ACESCG` was pinned with it: re-deriving v1 with the canonical
one shifts it by 9.1e-8, which is a pixel change to a frozen identifier. Do not
"tidy up" by collapsing them — a test fails loudly if you try.

**The two luma vectors are different kinds of number.** `BT2020_LUMA` is
transcribed from BT.2020's table and deliberately does *not* match a derivation
from the BT.2020 primaries (they differ by ~2e-6, about 17 ulps). The standard
rounds and encoders are expected to use the rounded values. `DISPLAY_P3_LUMA`
has no tabulated form and *is* derived. Their verification rules differ
accordingly, and a test pins the gap so nobody "corrects" the tabulated one.

## Known deviation

Three of the 36 shipped matrix entries sit exactly **+1 `f32` ulp** from the
canonical derivation: `ACESCG_TO_SRGB[2][1]`, `ACESCG_TO_DISPLAY_P3[2][0]`, and
`BT2020_TO_DISPLAY_P3[0][2]`. All three are negative values, so the derivation
being one ulp *above* the shipped literal means it has the smaller magnitude. Reaching those values needs a ~3e-9 relative shift
— far too large for `f64` accumulation noise (a sweep over inverse algorithms,
association orders, and summation orders moves the result ~1e-17) and far too
small for a different primaries or white-point choice. They are consistent with
the originals having been composed from intermediate matrices rounded to ~9–10
significant digits; no derivation script was committed with them, so the exact
route is unrecoverable.

This is recorded, not fixed. Both values are three orders of magnitude inside
the precision the standards themselves define, neither is more correct, and
re-pinning them would be an unreviewed pixel change. The check tolerance is
±1 ulp for exactly this reason.
