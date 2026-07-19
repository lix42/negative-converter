# Dependency & Module Hygiene

## Goal

Remove dead weight surfaced by the dependency/module-hygiene review (see
`docs/progress.md`): three declared-but-unused crates and a duplicate algorithm
selector enum. Pure cleanup — **no behavior change, byte-identical output.**

> Split out as its own task (not folded into the current review-driven doc-only
> changes) so a code change doesn't land in a working tree that currently holds
> only documentation edits.

## Scope — two independent, verified items

### 1. Drop three unused dependencies (`Cargo.toml:8,9,12`)

`image`, `kamadak-exif`, and `palette` are declared but unused. **Verified:** no
source references (the `image::` hits in the tree are `.new_image::` /
`.write_image::` methods on the **`tiff`** crate's encoder, not the `image`
crate; zero hits for `exif`/`kamadak`/`palette`), and `cargo build --all-targets`
(lib + bins + tests) succeeds with all three removed.

- Remove the three lines from `Cargo.toml` and update `Cargo.lock` (committed —
  binary crate). `image` in particular pulls a large codec tree (PNG/JPEG/…,
  `zune-*`), so this trims build time and surface.
- Note: `cargo` does **not** warn on unused *dependencies* by default (only unused
  code), which is why CI didn't catch these. Consider a follow-up note about
  `cargo-machete` / `cargo-udeps` in CI, but that's optional and out of scope here.

### 2. Unify the two `Algorithm` enums (`src/algo/mod.rs:75`)

Two selectors exist:
- `types::Algorithm` — the **wired** one: drives `cfg.algorithm`, used throughout
  `cli.rs`, and matched in `pipeline/stages.rs` to build `AlgoParams`.
- `algo::mod::Algorithm` — the **dead copy** (`#[allow(dead_code)]`, mod.rs:82).
  Its only consumer is `AlgoParams::algorithm()` (mod.rs:139-141), itself
  documented as not consumed by the pipeline directly and "exercised by tests"
  (mod.rs:135).

Collapse onto `types::Algorithm`:
- Delete `algo::Algorithm`, its `FromStr` impl, and the `#[allow(dead_code)]` at
  mod.rs:82 (removing an `allow` aligns with the CLAUDE.md goal of keeping those
  minimal).
- Repoint `AlgoParams::algorithm()` to return `types::Algorithm`, or drop the
  method entirely if it proves to be pure test scaffolding with no non-test caller.
- Update the local `algo::mod` tests that reference the removed enum.
- Confirm `pipeline/stages.rs` (which already matches on `types::Algorithm` via
  `cfg.algorithm`) is unaffected.

## Constraints (must hold)

- **No behavior change.** Output stays byte-identical; this removes unused
  declarations and a dead type, nothing on the conversion path.
- **CI-clean.** Match the gate before done: `cargo fmt --all --check` →
  `cargo clippy --all-targets -- -D warnings` → `cargo build` → `cargo test`.
- **Keep the remaining `allow`s justified.** Only the dead-enum `allow` goes; the
  documented item-level allows in `algo/mod.rs` / `pipeline/color.rs` that cover
  real API surface stay (don't remove one that still has a caller-less-but-intended
  item without a replacement comment).

## How to Verify

- `cargo build --all-targets` and full `cargo test` pass with the three deps
  removed and the enums unified.
- `grep` confirms zero remaining references to `image` / `kamadak-exif` / `palette`
  crate paths and to the deleted `algo::Algorithm`.
- `Cargo.lock` reflects the dropped crates (the `image` codec subtree is gone).
- No new `#[allow(dead_code)]` introduced; the dead-enum one is removed.
- A `nc convert` on a sample scan produces output identical to pre-cleanup
  (throwaway `#[ignore]` test / manual check; derived numbers only — never read
  sample pixels into context).

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md) — integrated the wired
  `types::Algorithm` that the cleanup standardizes on; the dep removal itself is
  standalone (no ordering dependency).
