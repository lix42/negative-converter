# Display-Output Acceptance

## Goal

Verify the shipped display-output policies against the user's full-size real
scans after the gain-map encoder and output presets land. This is the final
product-quality and interoperability gate; it does not block earlier core
pipeline resource measurements.

## Design

Reuse the asset classes and resolved Dmin/Dmax inputs established by
`real-scan-verification`. For representative color and HDR frames, execute this
matrix:

1. **Default `gain-map-hdr`** — the HDR rendition uses declared headroom; the SDR
   Display P3 base is independently valid; aware and fallback readers produce
   plausible, consistent color.
2. **Explicit presets** — `display-p3` and `compatibility` render correctly;
   `scene-master` preserves unclamped linear ACEScg and cross-frame exposure under
   fixed/roll-calibrated Dmax; `hdr-pq` and `hdr-hlg` carry
   their declared color signaling and metadata.
3. **Container/profile metadata** — independent inspection confirms the resolved
   container, ICC/CICP signaling, gain-map metadata, reference white, and
   headroom. Output suffixes agree with their containers.
4. **Interoperability** — test the default on target macOS/iPhone software and at
   least one non-Apple aware reader plus one SDR-only fallback reader.
5. **Determinism** — repeated runs meet each encoder's documented contract:
   byte-identical where promised, otherwise identical decoded pixels and semantic
   metadata.
6. **Characterization state** — a compatible calibrated artifact is identified,
   its reconstruction-domain contract matches, and it is exercised for the
   color-accuracy rows. The assumed linear Rec.709/D65 → ACEScg/D60 fallback is
   separately checked as internally valid but visibly provisional. Untagged
   identity device RGB is available only through its explicit custom diagnostic
   path and is rejected by every named color-defined preset.
7. **Simple boundary and migration** — a simple conversion characterizes raw
   unclamped `1 - scan/Dmin`; named display output applies resolved
   `print.white_balance`/`print.linear_range` afterward, while scene master
   rejects non-default values and legacy aliases follow their warned migration
   contract.

Record each row, reader/version, and observed result in `progress.md`. File a
follow-up task for every defect instead of fixing it ad hoc inside acceptance.

## Implementation Suggestion

- Extend the reusable harness from `real-scan-verification`; do not duplicate or
  re-read large assets into agent context.
- Preserve small metadata dumps and derived measurements, not large generated
  image outputs.
- Pin viewer/OS versions because HDR and gain-map behavior can change outside nc.

## How to Verify

Every matrix row passes for each applicable asset class, results are recorded,
and every failure has a tracked follow-up (or the log explicitly records none).

## Dependencies

- [Output presets and guidance](output-presets.md)
- [Real-scan core verification](real-scan-verification.md)
- [Color-characterization calibration](color-characterization-calibration.md)
