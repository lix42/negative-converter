# Background telemetry upload

## Goal

Ship the local telemetry records produced by `perf-telemetry` to a collection
server in the background, strictly opt-in, without blocking or perturbing any
`nc convert` run. `perf-telemetry` already writes an append-only JSONL queue
(default `$XDG_DATA_HOME/nc/telemetry.jsonl`, override `NC_TELEMETRY_LOG`); this
task drains that queue and uploads it.

> **Scoped by the spike.** The infra/protocol shape (OTel export vs custom
> ingestion, server contract, drain trigger) and the privacy/consent model for
> this task are **decided by** [`telemetry-strategy`](telemetry-strategy.md). The
> sketch below is the pre-spike starting point; the spike's design note supersedes
> it where they differ.

## Design (sketch — not yet implemented)

- **Separate from the conversion path.** Upload never runs inside `nc convert`'s
  critical path; a conversion must never wait on or fail because of the network.
  Candidate shapes: a detached background flush after a successful convert, an
  explicit `nc telemetry flush` maintenance subcommand, and/or a periodic
  external uploader — to be decided here.
- **Drain semantics.** Read the JSONL queue line by line, POST batches to the
  documented server endpoint, and truncate/advance only the successfully-shipped
  lines (crash-safe: an interrupted upload re-sends, never drops). Handle
  concurrent `nc` runs appending while draining.
- **Strictly opt-in + privacy.** Honor an off switch (design-spec §12:
  `NC_TELEMETRY=0`-style), a documented event/field list, and the existing rule
  that records carry no pixels and no file *paths* (only derived facts). No
  stdout pollution, no effect on exit codes or output bytes.
- **Server contract.** Endpoint URL, auth, batching, retry/backoff, and the
  ingestion schema (keyed off the record's `schema_version`) are defined here.

## How to Verify

To be detailed when the task is picked up: a fake/local endpoint receives the
drained records; an interrupted upload re-sends without loss; the off switch
suppresses all network activity; a conversion's timing/exit/output is unchanged
whether or not an upload is in flight.

## Dependencies

- [Telemetry strategy spike](telemetry-strategy.md) — this task's infra/protocol
  shape and privacy/consent model are decided by the spike; pick it up once that
  lands. (Transitively depends on
  [Embedded performance + context telemetry](perf-telemetry.md), which produces
  the JSONL queue this drains.)
