# Telemetry strategy spike

**Done:** 2026-07-23 — decisions are recorded in
[`docs/telemetry-strategy.md`](../telemetry-strategy.md); implementation is split
into the schema, ingestion, uploader, and panic-hook children below.

## Goal

A single **investigation / spike** that decides the strategy for growing `nc`'s
telemetry beyond the shipped local record (`perf-telemetry`): the **infrastructure**
(where records go and how), the **expanded data set** (what else to collect), and
the **privacy/consent** model that governs both. The deliverable is a **design
note** plus a set of concretely-scoped child tasks — **not** an implementation.

This exists because the questions below are coupled (the data you collect drives
the backend you need and the consent you must obtain), and answering them
piecemeal risks a half-built pipeline. One spike settles the shape; the children
build it.

## Scope — the three questions to answer

1. **Infrastructure / backend.**
   - Where do records land: a hosted collection service (owned) vs a managed
     observability backend? What is the ingestion contract (keyed off the record's
     `schema_version`)?
   - **OpenTelemetry (OTel) export vs custom ingestion.** Evaluate emitting the
     record as OTel (logs/metrics/traces — per-stage timings map naturally to
     spans/metrics) shipped via an OTel collector, **versus** a bespoke JSON POST
     endpoint. Weigh vendor-neutrality and off-the-shelf tooling against the added
     dependency/footprint and the fact that today's record is a purpose-built flat
     JSON object. Recommend one, with the tradeoff stated.
   - **How the local JSONL queue drains to it.** `perf-telemetry` already writes an
     append-only JSONL queue (default `$XDG_DATA_HOME/nc/telemetry.jsonl`, override
     `NC_TELEMETRY_LOG`). Decide the drain trigger and crash-safety model (this is
     the province of the `telemetry-upload` child).

2. **Expanded data.**
   - **Error / failure events.** Today a record is emitted only on a *successful*
     conversion — a record's existence implies success, and there is no
     `outcome.success` field (a constant `true` was dropped in the `perf-telemetry`
     round-1 review). Decide whether/how to emit records for failed runs
     (decode/encode/usage errors, mapped to exit codes), and how that reshapes the
     schema — likely reintroducing the signal as an `outcome.status` enum that
     actually varies, rather than a bare bool.
   - **Crash / panic hooks.** A panic hook writing a crash record (version,
     backtrace, params *shape*) — coordinate with the design-spec §12 crash-report
     idea. Decide whether this is part of the telemetry record stream or a separate
     artifact.
   - **Coarse usage / interaction events.** Which flags / algorithms / output modes
     were used, at a coarse, non-identifying granularity — enough to see feature
     adoption without reconstructing a user's images or workflow.

3. **Privacy / consent (governs 1 and 2).**
   - **Opt-in model.** Today telemetry is per-run opt-in via `--telemetry` /
     `--telemetry-file`. Decide the model for *upload* (a persistent consent /
     `NC_TELEMETRY=0`-style off switch per design-spec §12) — collection and
     transmission are distinct consent questions.
   - **PII / path scrubbing.** The existing invariant is that records carry **no
     pixels and no file paths** (only derived facts — dims, bytes, timings,
     params hash). Any expanded field (especially error events, which are tempting
     to enrich with paths/messages) must uphold this; define the scrubbing rules
     explicitly and how they are enforced/tested.
   - Document a concrete event/field list a user can inspect.

## Deliverable

- A **design note** ([`docs/telemetry-strategy.md`](../telemetry-strategy.md))
  recording the decisions and their rationale.
- **Concretely-scoped child tasks** filed in `TASKS.md` + `docs/tasks/`. At minimum
  this refines the existing [`telemetry-upload`](telemetry-upload.md) (which is
  already wired as a child of this spike); add others only where they genuinely
  clarify the graph (e.g. a schema-v2 error/outcome task, a crash-hook task, a
  usage-events task) rather than pre-fragmenting.

## How to Verify

A spike is done when the design note answers all three scope questions with a
recommendation (not just options), the privacy rules are concrete and testable,
and the child tasks are filed with clear deps so an implementer can start without
re-litigating the strategy.

## Dependencies

- [Embedded performance + context telemetry](perf-telemetry.md) — the shipped
  local record + JSONL queue this spike plans to grow and ship.
