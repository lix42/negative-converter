# iso-decoder-oracle

The external decoder oracle for
[`iso-gain-map-metadata`](../../docs/tasks/output/iso-gain-map-metadata.md) and
[`mp-container-conformance`](../../docs/tasks/output/mp-container-conformance.md):
a small Swift program that reads nc's gain-map JPEGs with **Apple ImageIO**, an
independent implementation of both ISO 21496-1 and the legacy Ultra HDR dialect.
What it reports about nc's bytes is evidence nc's own reader can never supply —
it found the placement defect fixed on 2026-08-06, which the entire Rust suite
had passed over.

**macOS-only, and deliberately not part of CI.** It needs the system ImageIO
framework and a Swift toolchain, and it is a manual gate run by hand when the
container or the ISO serializer changes. Nothing in `cargo test` invokes it.

Results write-up:
[`docs/progress/output.md`](../../docs/progress/output.md), under
`## iso-gain-map-metadata (decoder oracle — a real defect)`.

## Contents

- `oracle.swift` — the reader. Prints, per file: how many images the container
  holds, whether each gain-map dialect is present, the ISO metadata fields
  ImageIO parsed, and the SDR and HDR decode headrooms.

## Prerequisites

- macOS 15.0 or newer (`kCGImageAuxiliaryDataTypeISOGainMap` was added there;
  the recorded results are from macOS 26.5) and Xcode command-line tools.
- Nothing else — no assets, no `nc` binary at run time.

## Usage

Every command below runs from the **repo root**.

```bash
# 1. build the reader
(cd scripts/iso-decoder-oracle && swiftc -O oracle.swift -o oracle)

# 2. generate the three-file set
mkdir -p /tmp/iso-oracle
NC_ISO_SAMPLE_DIR=/tmp/iso-oracle \
NC_ISO_SAMPLE_INPUT=../nc-assets/rolls/<roll>/<frame>.tif \
NC_ISO_SAMPLE_BASE=<r,g,b> NC_ISO_SAMPLE_DMAX=<dmax> NC_ISO_SAMPLE_EV=3.0 \
  cargo test --bin nc iso_oracle_samples -- --ignored --nocapture

# 3. read them back
./scripts/iso-decoder-oracle/oracle /tmp/iso-oracle/oracle-*.jpg
```

`iso_oracle_samples` (`src/io/ultra_hdr.rs`, `#[ignore]`) writes
`oracle-legacy-only.jpg`, `oracle-dual-dialect.jpg` and `oracle-conflicting.jpg`
from **one** render through **one** container path, so any difference the oracle
reports is attributable to the metadata alone. There is no CLI path to a
dual-dialect file — `ultra-hdr-v1` is contractually ISO-free — so this test is
the only way to produce one.

## Inputs (env vars on the sample writer)

| Var | Default | Meaning |
|---|---|---|
| `NC_ISO_SAMPLE_DIR` | the system temp dir | where the three files are written (must exist) |
| `NC_ISO_SAMPLE_INPUT` | unset → the toy in-test fixture | a real scan to render instead |
| `NC_ISO_SAMPLE_BASE` | — | film base `r,g,b`; **required** with `_INPUT` |
| `NC_ISO_SAMPLE_DMAX` | — | explicit `Dmax` for the exponential curve; **required** with `_INPUT` |
| `NC_ISO_SAMPLE_EV` | `0.0` | print exposure |

**The EV is not optional in practice.** At defaults the gain map is flat —
measured `GainMapMax` 0.0039 log2 = 1.003x on both the toy fixture and a real
Ektar frame — because the exponential curve anchors display white at `Dmax` and
ordinary content lands far below the SDR shoulder knee. A flat gain map cannot
discriminate an HDR reconstruction from an SDR one, so the oracle would report
"present and correct" no matter what the reconstruction did. `+3 EV` pushes
content over the knee (`GainMapMax` 1.095 log2 = 2.14x) and makes the check
meaningful. (That the *default* render produces no HDR is a separate, recorded
finding owned by `output/presets`.)

Measure `_BASE` and `_DMAX` once per roll the usual way — `nc estimate`, or the
frozen `scripts/real-scan-verify/recipes/<roll>.json`.

## Reading the output

For the dual-dialect file, the gate wants:

```
  ISO 21496-1 gain map (kCGImageAuxiliaryDataTypeISOGainMap): PRESENT
      meta: HDRToneMap:AlternateHeadroom = 2.300448
      meta: HDRToneMap:ChannelMetadata = [ … GainMapMax = 1.095282 … ]
  ...
  HDR decode: WxH, headroom 4.9261084
```

- **The pass condition is `PRESENT` *plus* a `GainMapMax` materially above 0**
  — about `1.095` (log2, = 2.14x) on a real frame at `_EV=3.0`. `ABSENT` is a
  failure, and has meant a *placement* problem before rather than a
  serialization one. `PRESENT` with `GainMapMax ≈ 0.0039` means the metadata
  parsed but the gain map is inert: the file is structurally fine and
  photographically a no-op, which is the defaults case §Inputs warns about.
- **Do not read the headroom figure as a measurement — it is the trap here.**
  `HDR decode: headroom 4.9261084` is just `2^AlternateHeadroom`, i.e. nc's own
  declared `1000/203` policy constant parsed out of the metadata and echoed
  back. It reads **4.9261084 even on a completely flat gain map**, so it can
  confirm that ImageIO parsed the headroom field, and nothing more. "Headroom
  1.0 with `PRESENT`" is a state nc's files cannot produce; treating it as the
  failure mode makes the gate unfalsifiable.
- The `meta:` lines are ImageIO's own parse of each ISO field, and *this* is the
  substantive evidence: compare each against what nc wrote (the test prints the
  legacy metadata, and `exiftool -a -G1` shows the segments).

  | ImageIO prints | nc's `IsoGainMapFields` |
  |---|---|
  | `HDRToneMap:ChannelMetadata[i]` `GainMapMin` / `GainMapMax` | `gain_map_min_log2[i]` / `gain_map_max_log2[i]` |
  | `…[i]` `Gamma` / `BaseOffset` / `AlternateOffset` | `gain_map_gamma[i]` / `base_offset[i]` / `alternate_offset[i]` |
  | `BaseHeadroom` / `AlternateHeadroom` / `BaseColorIsWorkingColor` | `base_hdr_headroom_log2` / `alternate_hdr_headroom_log2` / `use_base_colour_space` |

  Three `ChannelMetadata` entries rather than one is `is_multichannel = true`
  read back; that is deliberate and must not be "fixed" (C.2.3 lets the metadata
  channel count differ from the map's).
- `oracle-legacy-only.jpg` is expected to be **ABSENT for both dialects** with
  headroom 1.0: Apple ignores Google's Ultra HDR v1 XMP entirely. This is the
  one place the headroom figure is informative — with no ISO metadata to read,
  there is no declared constant to echo.
- `oracle-conflicting.jpg` carries legacy and ISO metadata that disagree by
  exactly one stop, which is how the observed dual-aware precedence was
  established. It is *observed Apple behaviour* only — ISO 21496-1 says nothing
  about coexistence, so it must never be stated as a conformance property.

## Notes

- **Validate the oracle before trusting a negative.** An `ABSENT` result is only
  evidence about nc if a known-good file reads as `PRESENT` on the same machine.
  The control was `ultrahdr_app` v1.4.0 (homebrew) encoding a synthetic
  rgba1010102 gradient — pinned here because homebrew will move off 1.4.0 and
  the exact invocation is what makes the negative trustworthy:

  ```bash
  # <raw> = 256x256 rgba1010102, e.g. a horizontal luminance ramp
  ultrahdr_app -m 0 -p <raw> -w 256 -h 256 -a 5 -t 1 -C 2   # writes out.jpeg
  ./oracle out.jpeg    # must report the ISO gain map PRESENT
  ```

  Its ISO payload is 61 bytes (1 metadata channel) against nc's 141 (3
  channels); both fit `4 + 1 + 16 + 40·channels`, which is independent evidence
  nc's C.2.2 field order is right.
- The compiled `oracle` binary and any generated JPEG are build products; the
  directory's `.gitignore` keeps them out of the repo. It ignores them
  **silently** — a `git add` of a sample or control image here looks like it
  worked and stages nothing, which is the intended outcome. Don't `-f` past it:
  no sample, control, or PDF belongs in the repo.
- The complementary in-repo check is libultrahdr, which reads the **legacy**
  dialect only — it was never an ISO oracle, which is exactly why this exists.
