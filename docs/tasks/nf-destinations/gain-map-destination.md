# The gain-map destination

## Goal

Ship the new chain's HDR JPEG: an SDR base with a per-channel ISO 21496-1 gain map,
built from `chain::render_pair` and `pipeline::gain_ratio`. In the destination set
([preset-set](preset-set.md)) it is the `--range hdr --container jpeg` row, which is
refused as "not yet" until this lands.

## Design

What is known:

- **Split out of `preset-set` (user, 2026-09-25)** because it needs a container the
  tree does not have. `io::ultra_hdr` packages a *single-channel luminance* map, since
  the legacy Ultra HDR v1 XMP cannot signal a multichannel one, and its ISO fields are
  projected from that map. `gain_ratio` gives **per-channel** gains, and the colour they
  carry where the SDR cube binds is the reason the branch contract keeps them
  (`nf-display-stages/branch-contract`). Collapsing them to luminance to reuse the old
  writer was rejected.
- **No dialect knob**: the legacy XMP dialect cannot carry a per-channel map, and Apple
  platforms read only the ISO one (`output` epic summary). An ISO-only file is the
  destination.
- **The rules `gain_ratio` leaves to its caller**: the HDR rendition is clamped to the
  destination's peak (`1000/203`) and what that clamps is counted into the report, since
  `gain_ratio::between` clamps the alternate only to `>= 0`; a flat map
  (`GainRange::flat`) is reported rather than shipped silently. The gain is ratioed
  against the base as stored, never the unclamped rendition.
- **Verification needs `scripts/iso-decoder-oracle/`** (macOS, manual): exiftool and
  libultrahdr both accept files no decoder parses.
- Needs its own `RunProfile` (the pair holds two working buffers; the copy includes the
  IR plane).

Open:

- Downsampling and quantization of a three-channel map, and which ISO fields a
  multichannel map states (`output/iso-gain-map-metadata` records that
  `is_multichannel` describes the metadata's channel count).
- Whether the default (`--range hdr` with nothing else) should resolve here once it
  lands — the table's derivation already takes it there.

## How to Verify

- `--new-flow --range hdr --container jpeg` writes a JPEG the ISO oracle reads as
  `PRESENT` with a `GainMapMax` above 0 on a frame with highlights, and reports a flat
  map on one without.
- The HDR rendition's above-peak samples are counted in the report.
- `docs/using-nc.md` updated by running the binary.

## Dependencies

- [The destination set](preset-set.md)
