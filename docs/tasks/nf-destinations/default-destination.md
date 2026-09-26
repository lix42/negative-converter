# Which destination the default resolves

## Goal

Decide and execute what a bare `hanten convert` writes under the new flow. The product
shape is decided; this task owns the execution and the migration cost.

## Design

This supersedes [`output/display-p3-default`](../output/display-p3-default.md).
What survives from it:

- **The decided product shape** (user, 2026-08-09, reaffirmed 2026-09-13): SDR
  lossless is the default; HDR lossless stays supported; SDR JPEG is to be
  supported; HDR JPEG is good to have and stays opt-in. The wide-gamut-fidelity
  argument beat sRGB's "surprise nobody".
- **The argument for one `pipeline_version` bump rather than two.** That task
  collided with `algo/split-default-migration` because both owed a bump and a
  before/after report, and every recipe silent on the preset changes container at
  the second. Under the new flow the collision is larger, not smaller, so the
  container move should ride the same bump as the chain move.

What does not survive: the specific preset name ([preset-set](preset-set.md)
decides the new set) and the ordering question against the split migration, which
the migration as a whole subsumes.

Open:

- **Does the default move once or twice?** The flag's written expiry flips the
  default when the minimal end-to-end render lands — a *chain* flip, which may
  precede the destination being the intended one. If so, say so plainly.
- **What the before/after report measures** now that the legacy-measuring coverage
  is deliberately not preserved — the reference rendition comes from the tagged
  binary, not a surviving branch. `tests/pipeline.rs` states each test's preset
  explicitly since `nf-retire/legacy-custom`, so moving the default does not silently
  retarget them.

- **2026-09-25 (`preset-set`):** a destination is a set of axis values, not a preset
  name, so the default is the axes' defaults (today `sdr`, `native`, `display-p3`,
  `tiff`, resolved through `preset-set`'s table), and moving it means moving one or
  more of those.

## How to Verify

- A bare `hanten convert` resolves the intended destination with no output-selection
  flags, and `hanten roll` derives its names to match.
- A `pipeline_version` bump with its own recorded fingerprint row — never a
  historical row edited in place — and a before/after report on real frames.
- `docs/using-nc.md` updated by running the binary, not by reading the diff.

## Dependencies

- [The destination set](preset-set.md)
- [The direct destination for external editing](direct-preset.md) — the default is
  chosen against it, since "minimal" and "what most users should get" are different
  answers and the contrast is what makes the choice legible
