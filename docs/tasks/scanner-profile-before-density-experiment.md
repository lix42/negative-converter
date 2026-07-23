# Scanner ICC Before-Density Experiment

## Goal

Determine empirically whether applying the same conventional scanner ICC
transform to both image pixels and Dmin before component-wise density conversion
improves negative reconstruction. This is a deferred alternative-workflow
experiment and is independent of post-reconstruction color characterization.

## Design

Use a controlled negative set containing an unexposed Dmin reference, a
photographed color target with usable reference values, gamma-1 HDR/HDRi scans,
and the scanner profile under test. Compare identical source pixels through:

```text
A. scanner RGB → Dmin/log density → negative reconstruction
B. scanner RGB → scanner ICC → defined linear RGB/XYZ
   → transformed Dmin/log density → negative reconstruction
```

Pipeline B must use linear coordinates; never divide or take logarithms of PCS
Lab values. Both variants then receive the same permitted exposure, white
balance, NC film RGB working-space mapping, and output transform so the only
experimental variable is the pre-density scanner ICC.

Measure target-patch error with a documented Delta E metric and report median
and tail error, neutral behavior, clipping, and stability across frames. A
positive IT8 scanner profile may improve reproduction of the film-as-object
without improving reconstruction of the photographed scene; report those goals
separately.

The result decides only whether nc should offer an experimental/supported
pre-density input-profile path. It does not choose or block the normal NC film
RGB mapping or optional correction-profile work.

## Implementation Suggestion

- Build an isolated harness or ignored integration test so the transform cannot
  enter the default pipeline accidentally.
- Preserve profile identity and intermediate linear values in the report.
- Defer rather than infer a winner visually when target/reference data is
  inadequate.

## How to Verify

- Both variants consume identical source pixels and Dmin samples.
- Re-running with identical inputs is deterministic.
- The report contains per-patch and aggregate error, clipping counts, profile
  identity, and representative outputs.
- A written conclusion either selects a validated pre-density path or records
  that the evidence is inconclusive; normal behavior changes only in a separate
  reviewed implementation.

## Dependencies

- [Input data semantics and validation](input-data-semantics.md)
- [Color management](color-management.md)
