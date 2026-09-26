# A memory profile per destination

## Goal

Give every destination in the new set its own `RunProfile` arm, calibrated against
a measured run. The memory preflight is a gate that fails closed at exit 6, so a
destination with no profile either cannot ship or ships un-gated.

## Design

What is known:

- **The model's arithmetic carries over untouched** (`docs/nf-migration.md`): what
  changes is which buffers are simultaneously live, not how they are counted.
- **Sharing an existing arm must be measured, not assumed.** `SdrTiff` shares the
  coded arm's arithmetic and `gain-map-hdr` shares `UltraHdrV1`'s — both recorded in
  `src/pipeline/memory.rs` as measured rather than inherited. Two destinations of
  the same apparent shape are not evidence.
- **Which phase peaks is per profile**, and a sentence claiming otherwise has been
  wrong twice; `memory`'s `which_phase_peaks_is_per_profile_and_measured_not_assumed`
  is where that is read off, not a rule of thumb.
- **Calibrate across two frame sizes, not one** — that is what separated the AVIF
  slope from its fixed cost — and leave `accounted` slightly under measured, since
  the allowance covers allocator overhead. **Nothing tests the model against the
  code**, so a new full-frame buffer under-approves until someone updates the arm.

Open:

- **Does the new chain change the live set at all?** Staged pure functions may hold
  more intermediates than the current fused per-pixel bodies, or fewer if a stage
  consumes and returns its input (the `color::to_output` precedent). Measure before
  assuming the old numbers transfer.
- **Whether a skipped branch changes the profile** — the branch-contract question
  about single-rendition destinations is the difference between two display buffers
  and four. **Answered** (2026-09-24, `nf-display-stages/branch-contract`): a
  single-rendition destination renders one branch (`chain::render`) and holds no second
  buffer; a gain-map pair (`chain::render_pair`) copies the graded image once and
  renders each branch in place, so it holds two full-frame working buffers where one
  destination holds one. The copy includes the IR plane when the scan has one, so a
  profile calibrated on an IR-free fixture under-counts an HDRi scan. The encoder's
  own buffers come on top, per destination.
- Whether the arms are per destination or per shape; fewer is better only if each
  sharing is measured.

- **2026-09-25 (`preset-set`):** destinations are axis combinations, so the arms key on
  the shape a row renders and encodes, not on names — three arms: the u16 TIFFs (the
  SDR TIFF and the coded HDR TIFF share `NewFlowU16Tiff`), the f32 TIFF, and the AVIF.
  `preset-set` gives each a provisional arm by counting its buffers; measuring them is
  this task's.

## How to Verify

- Every destination resolves a profile; a destination with none fails loudly at
  resolution rather than at encode.
- Each arm's estimate is compared against a measured peak on two frame sizes, with
  the numbers recorded, and the estimate sits under measured.
- A frame over budget fails with exit 6 before decode, and on `roll` follows roll's
  ordinary per-frame handling (recorded, siblings written, roll exits 1).

## Dependencies

- [The destination set](preset-set.md) — the list of arms to calibrate
