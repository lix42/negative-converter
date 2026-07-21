# Transactional Output Writes

## Goal

Ensure a failed or interrupted `nc convert` never leaves a **partial or
inconsistent artifact set** on disk. Today every artifact is written straight to
its final path with `File::create`, sequentially, so a mid-write failure leaves a
truncated file at the real filename, and a later-stage failure orphans the
already-written earlier ones.

> **Context.** From the output-atomicity review (see `docs/progress.md`).
> Reproduced: `encode` succeeds → `write_sidecar` fails → exit 5, leaving a
> **complete primary TIFF with no sidecar**. Likewise a crash / disk-full during
> the TIFF write leaves a truncated `.tiff` at its final path (`flush_buf` flushes
> the BufWriter but never fsyncs). The pipeline already has *partial* mitigation —
> IR is exported before the primary, and `export_ir` checks the IR plane exists
> before `create` — but those are ordering tricks, not atomicity.

## Scope note — honest guarantees (not a true multi-file transaction)

POSIX `rename` is atomic **per file**; several files cannot be flipped as one
atomic unit. So this task does **not** promise all-or-nothing across the set. It
delivers two achievable guarantees:

1. **No truncated/partial file ever appears at a final path.** Each artifact is
   written to a same-directory temp file, flushed + fsynced, then renamed into
   place — so a failure mid-write only ever leaves a discarded temp.
2. **Minimal inconsistency window.** All fallible work (encode, serialize, fsync
   of *every* temp) happens first; the only remaining operations are the renames,
   which are fast and rarely fail. A crash between renames can still leave some
   final paths updated and others not — this is inherent and must be documented,
   not papered over.

Write the design and docs to this framing. Do not claim literal transactional
all-or-nothing semantics.

## Design

The artifact set for one `nc convert`: primary TIFF, optional IR export, sidecar
JSON, and (optionally) the `--report-file`. Restructure the write phase in
`run_convert` (`cli.rs`) plus the `io/encode.rs` writers so:

1. **Write to same-directory temps.** For each target `<dir>/<name>`, create
   `<dir>/<name>.<pid-or-rand>.tmp` (same directory ⇒ same filesystem ⇒ rename is
   atomic; a temp on another filesystem **cannot be renamed into place at all** —
   `rename` fails with `EXDEV`, it does *not* silently fall back to a copy — so a
   same-directory temp is what keeps finalization a real rename).
   A shared helper (e.g. `io::encode::atomic_write(path, |w| …)` /
   `finalize(temp, final)`) keeps the temp+fsync+rename pattern in one place so
   `encode`, `export_ir`, `write_sidecar`, and the report all use it.
2. **Flush + fsync each temp** before any rename, so the bytes are durable and no
   rename promotes half-written data. (Directory fsync for full power-loss
   durability — decide below; likely out of scope.)
3. **Rename all temps into place last**, after every temp exists and is synced —
   shrinking the failure window to just the renames.
4. **Cleanup on failure.** Any error path must unlink the temp files it created
   (RAII guard / `Drop`, or explicit cleanup), so failed runs don't litter
   `*.tmp`. A successful rename consumes its temp.

Keep the existing **up-front write-target collision checks** (the `targets`
guard in `run_convert`) — they still validate the *final* paths before anything
is decoded. Temp names must not collide with those or with each other.

### Decisions to pin down (record in `progress.md`)

- **fsync depth.** fsync each temp before rename: **required**. fsync the parent
  directory after the renames (guards the rename itself against power loss):
  likely **out of scope** for a conversion CLI — decide and document, don't leave
  implicit.
- **Overwrite semantics — the contract is atomic replace.** A rename over an
  existing final path replaces it, matching today's `File::create`
  truncate-in-place overwrite (so `nc` keeps overwriting its output, not refusing).
  This is a decided contract, **not** an open choice — the How-to-Verify section
  depends on it. What remains to pin down is the **cross-platform finalization
  strategy**: `std::fs::rename` replace-existing semantics differ by platform
  (on Windows a rename can fail if the destination is open/locked, and
  atomic-replace is not guaranteed the same way as POSIX), so specify how
  finalization achieves replace on each supported platform and gate the atomic-
  replace test on that. Ensure it composes with the up-front collision checks.
- **Report inclusion.** The stdout report can't be transactional. A
  `--report-file` artifact *can* join the set — decide whether it does, or stays
  best-effort (like telemetry). Note the report is currently emitted *after* the
  primary/sidecar and before the `--strict` gate; preserve that the report still
  lands even when `--strict` then fails the run.
- **Ordering vs telemetry.** Telemetry stays after the finalized output and is
  best-effort (never fails the run) — unchanged; it is not part of the set.

## Constraints (must hold)

- **Determinism unchanged.** Same input + recipe ⇒ identical output bytes; this
  is purely *how/where* bytes are written, never *what* is written.
- **Fail loudly, mapped exit codes.** A temp write / fsync / rename failure is a
  hard error with the documented exit code (design-spec §11) and a clear message
  naming the artifact — never a silent partial success.
- **No new silent fallback.** If a rename fails, do not fall back to an in-place
  write; surface it.

## How to Verify

- **Failure injection** (the core of this task): simulate a sidecar write failure
  after a successful primary encode and assert **no** primary TIFF exists at its
  final path (only the discarded temp, then cleaned up) — the exact scenario the
  review reproduced. Repeat for a failure during the primary encode (no truncated
  final TIFF) and during IR export.
- **Temp cleanup:** after any injected failure, no `*.tmp` remains in the target
  directory.
- **Success path:** all final artifacts present, byte-identical to the current
  encoder's output (regression guard), no temps left behind.
- **Overwrite:** converting over an existing output replaces it atomically; an
  interrupted overwrite leaves the *old* file intact, not a truncated new one.
- **Collision checks** still reject an output/sidecar/IR/report path that
  collides with the input or with each other, before any write.
- Exercise end-to-end on a real scan (throwaway `#[ignore]` test; derived numbers
  only — never read sample pixels into context).

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md) — owns `run_convert` and the
  write-target guards; this restructures its write phase and the `io/encode.rs`
  writers it calls.
