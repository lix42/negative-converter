# Telemetry event schema v2

## Goal

Evolve the successful-conversion-only local record into typed success and failure
events, then produce a separate privacy-minimized upload projection. This task
establishes the stable contract used independently by the uploader, ingestion
service, and panic hook.

The approved field manifest and decisions live in
[`docs/telemetry-strategy.md`](../telemetry-strategy.md).

## Design

Keep local and remote privacy contracts distinct:

- Local schema v2 adds a random per-event ID, an event discriminator,
  `outcome.status: success|failure`, typed failure/exit category, failed/active
  stage, elapsed time, and optional completed-stage/image context.
- Upload schema v1 is a separate typed `UploadEvent` produced only through a pure
  `to_upload_event` projection. It contains the exact allowlist from the strategy
  note and carries `source_schema_version`.
- Local diagnostic fields such as `params_hash`, precise timestamp, exact
  dimensions/sizes, messages, paths, recipes, and parameter values have no type
  slot in `UploadEvent`.
- A stable random 128-bit `event_id` is injected into every projection. It supports
  retry deduplication only, serializes as exactly 32 lowercase hexadecimal
  characters, and is never reused as an installation/session ID.
- Legacy local-v1 records can be parsed and deterministically projected when the
  caller supplies the stable ID. The pure schema/projection layer performs no
  durability or ID persistence; the uploader owns assignment and durable spool
  storage.

Refactor the orchestrator so a small telemetry attempt/context tracks the active
stage and completed timing/facts without affecting stage inputs. Emit success
only after artifacts and strict checks succeed. Emit typed failure events for
recoverable command failures, including strict-warning promotion, without ever
passing an error display string into telemetry.

V1 telemetry supports `convert` only. Include recoverable `convert` parse/usage
errors when the parser can safely classify the intended command and fixed error
kind. `roll`, `inspect`, `estimate`, `params`, and unknown-subcommand parse
failures are out of scope; they must not enter the conversion denominator.

Automatic collection is conditional on persistent consent (implemented by
`telemetry-upload`); explicit `--telemetry` remains local per-run collection.
`--telemetry-file` remains a one-off sink and does not itself imply upload.

Any wire-shape change bumps the appropriate local or upload schema version. Keep
builders and privacy projection pure by injecting time, ID generation, platform,
and other ambient values at the orchestration boundary.

Check in a machine-readable upload-v1 JSON Schema and canonical valid/invalid
JSON request/response corpus. It fixes the complete POST envelope, 128-bit ID
encoding, SemVer and string limits, numeric ranges, all enum members, and the
sole normalized-frame grammar/string exception from the strategy. Rust tests
consume this same corpus that the Worker task consumes; additions cannot land in
only one language.

The shared schema/corpus also owns the canonical byte-for-byte local-v2
`panic-ready` fixture. It expresses with `if`/`then`/`oneOf` that success is
`exit_code: 0` + `error_kind: none`; failure is a non-`none` error with mapping
`usage` → `2`, `decode` → `3`, `unsupported` → `4`, `write` → `5`, and
`strict`/`other` → `1`; source schema v1 is conversion-success-only, while v2
allows conversion success/failure and panic. `image.bit_depth` is bits per sample
and accepts integer `16` or string `unknown`, never total 48/64-bit layout.

## Implementation Suggestion

- Replace the success-only top-level record with a tagged local event envelope,
  while keeping shared image/timing/conversion structs where their semantics hold.
- Introduce fixed `CommandKind`, `StageKind`, `OutcomeStatus`, and `ErrorKind`
  enums; map `NcError`/strict failures without storing messages.
- Represent unknown/not-yet-completed fields with `Option`, not zero/false.
- Bucket/round only in the upload projection so the local record stays useful for
  user inspection.
- Keep the projection in the Rust client and mirror its allowlist independently
  in the ingestion service; one is defense against mistakes in the other.

## How to Verify

`cargo test` passes with:

- full/minimal local-v2 and upload-v1 snapshot tests;
- success, decode failure, unsupported input, write failure, usage failure, and
  strict-promotion end-to-end events with correct stage/exit category;
- a failure before decode has no invented image/timing fields;
- deterministic legacy-v1 projection when supplied a stable event ID, with no
  filesystem/durability behavior in the pure projection;
- a known-16-bit legacy-v1 fixture projects `image.bit_depth: 16`;
- hostile local values containing paths, filenames, usernames, newlines, error
  messages, recipe values, exact dimensions/sizes/timestamps, and `params_hash`
  never occur in serialized `UploadEvent`;
- no persistent ID or cross-event correlation field exists;
- the checked-in schema/corpus accepts every canonical valid example and rejects
  every invalid boundary/enum/string/envelope example, all invalid
  status/error/exit and source-version/event pairings, and 48/64 bit-depth totals
  in Rust;
- `convert` recoverable parse failures are included, while other commands and
  unknown-subcommand parse failures emit no v1 event;
- telemetry on/off still produces byte-identical output and sidecar, and
  collection failure cannot change the command exit code.

## Dependencies

- [Telemetry strategy spike](telemetry-strategy.md) — fixes the event manifest,
  privacy boundary, and success/failure semantics.
