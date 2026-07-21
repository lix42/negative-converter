# Streaming / Tiled I/O

## Goal

Bound peak memory to a small working set (a few strips/tiles) instead of several
whole-image buffers, by moving decode and encode toward **strip/tile streaming**:
strip/tile decode, quantize-and-write encode strip-by-strip, avoiding
materializing the full u16 and f32 images at once.

This is the **expensive, speculative** half of the memory-safety review (see
`docs/progress.md`). Its cheaper counterpart —
[memory-preflight](memory-preflight.md) (peak estimate + in-place transform) — is
the committed deliverable and ships first.

## STEP 0 (gate): evaluate whether this task is needed — do this first

**Do not implement anything until this evaluation says so.** Streaming/tiled I/O
is a large architectural change with real risk (tiled *decode* fights the
reverse-engineered SilverFast layout — no public spec, "verify against real
sample files" — and the `tiff` crate's whole-image `read_image` API). It is only
justified if real workloads actually exceed the memory budget. For ~18 MP scans
peak is ~600 MB, which is fine — so the default expectation is **not needed yet**.

Decision procedure:

1. **Gather the data.** From [memory-preflight](memory-preflight.md)'s estimate,
   [real-scan-verification](real-scan-verification.md)'s **measured peak** on the
   user's full-size real scans, and `perf-telemetry` field records, establish:
   the largest real input dimensions, the measured/estimated peak for them, and
   the target machines' available RAM.
2. **If there is not enough data to decide, collect it first** — do **not**
   proceed on assumption. Run the preflight/verification measurements (or extend
   telemetry to capture peak RSS) on the largest available real scans and record
   the numbers in `docs/progress.md`. Re-evaluate with data in hand.
3. **Decide with a written criterion.** Proceed **only if** measured/estimated
   peak on realistic inputs exceeds (or comes within a documented margin of) the
   memory budget on target hardware — i.e. the [memory-preflight](memory-preflight.md)
   gate would actually reject real work. Record the go/no-go, the numbers behind
   it, and the date in `docs/progress.md`.
4. **If no-go,** leave this task `[ ]` with the evaluation recorded as the
   rationale; the preflight already fails such inputs loudly and safely, which is
   an acceptable end state. Revisit only if the input envelope grows (e.g. much
   larger scanners, multi-frame roll buffers).

Only if STEP 0 says **go** does the design below apply.

## Design (only if STEP 0 = go)

Stage it smallest-risk-first; each sub-step is independently shippable and should
re-check the STEP 0 criterion still holds:

- **Streaming encode first (lower risk).** Quantize and write the u16 output
  strip-by-strip so the full quantized buffer never coexists with the f32 source.
  The `tiff` crate supports strip-based writing; validate output stays
  byte-identical to the whole-image encoder.
- **Strip/tile decode (higher risk).** Read the SilverFast RGB (and IR) planes in
  strips rather than one `read_image`, normalizing each strip to f32 as it lands.
  This is the risky part against the reverse-engineered layout — prototype behind
  a `#[ignore]` test on real scans (derived numbers only; never read sample pixels
  into context) and degrade gracefully / fall back to whole-image decode on any
  unrecognized layout.
- **Interaction with the pipeline.** Film-base estimation, the density algorithm,
  and color transform currently assume a whole `LinearImage`. Streaming decode
  only reduces peak if these don't immediately re-materialize the full image —
  scope honestly: partial streaming (bounded decode/encode around a still-whole
  working image) may be the realistic target, full end-to-end streaming a larger
  effort. State which is being delivered.

## Constraints (must hold)

- **Determinism & byte-identical output.** Output must be identical to the
  whole-image path — this changes *how* bytes are produced, never *what*.
  Regression tests against the current encoder/decoder are mandatory.
- **Graceful degradation.** Unrecognized/edge layouts fall back to whole-image
  decode with a warning, never a wrong or truncated image (verify against real
  sample files).
- **Fail loudly** on real errors; map to documented exit codes.
- **IR preserved**, carried through untouched.

## How to Verify

- STEP 0 evaluation recorded in `docs/progress.md` with the numbers and go/no-go.
- (If go) measured peak RSS on the largest real scan drops to a small multiple of
  the strip/tile working set, not the whole image.
- Streaming encode and decode each produce **byte-identical** output/pixels vs the
  whole-image path (regression guards).
- Fallback path exercised: a layout that isn't strip-decodable still converts via
  the whole-image path with a warning.
- End-to-end on the user's full-size real scans (throwaway `#[ignore]` test,
  derived numbers only).

## Dependencies

- [Memory preflight & in-place transform](memory-preflight.md) — provides the peak
  sizing model and the budget gate that defines "needed"; ships the cheap wins
  first.
- [Real-scan verification](real-scan-verification.md) — supplies the **measured
  peak on real full-size scans** that STEP 0 evaluates; without it there is not
  enough data to justify this task.
