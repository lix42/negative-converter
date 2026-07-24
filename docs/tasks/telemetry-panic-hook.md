# Sanitized panic telemetry

## Goal

Record explicitly consented Rust panics as minimal, privacy-safe telemetry events
without changing normal panic output/termination or claiming general native crash
coverage.

## Design

Install a best-effort panic hook only when invocation-start persistent managed
consent is active. Explicit per-run `--telemetry` and `--telemetry-file` never
install it. Chain to the previously installed/default hook after capture.

The hook writes the common panic event shape defined in
[`docs/telemetry-strategy.md`](../telemetry-strategy.md) without a shared append
stream. For each panic, derive the active JSONL's private sibling spool and create
a unique `.panic-<128-bit-lowercase-hex>.tmp` regular file there without
following symlinks, write at most 16 KiB, best-effort flush/sync it, then
atomically rename it in that spool to immutable `panic-ready-<id>.json`. Never
overwrite on collision. It must not take the
ordinary telemetry queue lock: a panic may have occurred while that lock was
held. The invocation holds its shared collection lease and immutable
generation/active/spool snapshot through process end, so disable or retarget
cannot invalidate the hook's destination mid-process. The normal uploader
reconciles stale temp/ready files and later consumes the ready event.

Capture only:

- common event/version/coarse-platform context;
- the v1 `convert` command and fixed active-stage enum;
- at most 32 normalized function/module symbols belonging to `nc`, each ASCII,
  2–192 bytes, matching
  `^nc(?:::[A-Za-z_][A-Za-z0-9_]*){0,15}$`.

Discard panic payload/message text, raw backtrace text, addresses, dependency
frames, source filenames/directories, line/column numbers, crate hash suffixes,
parameters, recipes, and paths. If parsing/sanitizing cannot prove a frame safe,
drop it; if capture fails, an empty frame list is valid. No hook failure may
panic recursively or replace the original panic behavior.

Documentation must call this **panic reporting**. Rust panic hooks do not promise
coverage for access violations/signals, non-panic aborts, OOM kills, or forced
termination.

## How to Verify

`cargo test` and a subprocess integration test cover:

- consent disabled means no hook-owned panic event;
- persistent managed-consent invocation writes one parseable panic event;
- explicit `--telemetry`/`--telemetry-file` without persistent consent installs
  no hook and creates no panic spool;
- disable does not wait for an already-running consented process; its later panic
  may publish one local ready event, which remains unsent while inactive;
- simultaneous panics in multiple processes publish distinct ready files whose
  bytes never interleave;
- a crash/short write before publication leaves only a temp that uploader
  startup safely removes or quarantines, never uploads;
- hostile panic messages and synthetic backtraces containing absolute paths,
  usernames, line numbers, addresses, dependency frames, and newlines yield only
  capped normalized `nc` symbols;
- sanitizer ambiguity yields no frame rather than leaked text;
- a failing/unwritable panic spool does not recurse or change the process's
  normal panic stderr/exit behavior;
- a panic while the ordinary queue lock is held does not deadlock;
- inactive-only purge waits for the process's shared collection lease, so it
  cannot remove the spool while the hook may still publish;
- the previous/default hook still runs.

The canonical local-v2 `panic-ready` byte fixture is owned by
`telemetry-schema-v2`'s shared corpus. Panic-hook tests must emit a byte-compatible
ready file. Uploader tests independently consume that shared fixture through
projection and delivery; neither task produces an artifact for the other, so the
fixture does not add a separate direct schema dependency. Schema remains
transitive through the uploader runtime dependency below.

Document manual expectations for uncaptured native termination classes.

## Dependencies

- [Background telemetry upload](telemetry-upload.md) — supplies the managed
  consent generation, invocation collection lease, private spool lifecycle,
  uploader reconciliation, and purge synchronization required by the hook.
  Its schema-v2 dependency transitively supplies the common envelope, enums,
  privacy projection, shared fixture, and panic upload shape.
