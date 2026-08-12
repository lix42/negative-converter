# negative-converter (`nc`)

A command-line tool that converts film **negative** scans into **positive**
images.

It reads high-bit-depth scanner files (SilverFast HDR/HDRi first), runs a
deterministic negative→positive pipeline in a 32-bit float linear working space,
and writes a gain-map HDR JPEG by default. An output preset selects the rest: SDR
TIFF in Display P3 or sRGB, the legacy Ultra HDR v1 gain-map JPEG, a 10-bit HDR
AVIF (PQ or HLG), an HDR TIFF, or the transitional 16-bit/float TIFF path.

## Design goal: built for agents

Film conversion has many knobs — film-base estimation, density, white balance,
tone, gamma, color management. The core idea here is that **every parameter is a
CLI flag** and the tool is deterministic and scriptable (JSON recipes in, JSON
reports out), so an automated agent — or a human — can drive the whole conversion
reproducibly.

This is *not* about using AI/ML to process images. The pipeline is a
physics-based deterministic core; any future ML assistance stays optional and
around the edges.

## Status

The Step-1 TIFF converter is implemented, with post-MVP pipeline, display-output,
and hardening work tracked in the task roadmap.

- [`docs/using-nc.md`](docs/using-nc.md) — **how to use `nc`**: the
  measure → freeze a recipe → apply workflow, recipes, presets, exit codes.
- [`docs/design-spec.md`](docs/design-spec.md) — full design (architecture,
  pipeline, CLI surface, parameters).
- [`docs/TASKS.md`](docs/TASKS.md) — the build plan and dependency graph.
- [`docs/negative-convertor-research-report.md`](docs/negative-convertor-research-report.md)
  — background research.

## Usage (current CLI)

```sh
# Measure the film base (Dmin) once per roll from an unexposed border.
nc estimate reference.tiff --base-region 0,0,120,40

# Convert a negative scan to a positive 16-bit TIFF. Every conversion must state
# where the film base comes from — there is no default, because Dmin sets both the
# black point and the colour balance. Use the measured value, or --auto-base to
# detect the rebate band (best-effort; it fails loudly when it can't).
nc convert in.tiff -o out.tiff --reconstruction density \
  --film-base 0.92,0.55,0.42

# Full HDR float output with explicit controls. `--density-gamma` is the
# exponential curve's knob, so that curve is selected explicitly — the default
# is the sigmoid, whose slope is `--sigmoid-contrast`.
nc convert in.tiff -o out.tiff --reconstruction density --output-hdr \
  --film-base 0.92,0.55,0.42 \
  --density-curve exponential --density-gamma 1.8 --print-exposure 0.0

# Inspect a scan and emit machine-readable JSON.
nc inspect in.tiff --report json

# Backward-compatible SDR JPEG with legacy Ultra HDR v1 gain-map metadata.
nc convert in.tiff -o out.jpg --output-preset ultra-hdr-v1 \
  --film-base 0.92,0.55,0.42
```

See the design spec for the complete command and parameter reference.

## Building

The Rust build also compiles the pinned libultrahdr and libjpeg-turbo sources
statically, plus libaom (the AV1 encoder behind the `hdr-pq` / `hdr-hlg` AVIF
outputs) from the `libaom-sys` crate's vendored source. A fresh build machine needs
CMake, C and C++ compilers, and libclang for bindgen (plus NASM for libjpeg-turbo
and libaom SIMD on supported targets). No network access is needed for any of the
native builds. For example:

```sh
# Debian/Ubuntu
sudo apt-get install build-essential cmake clang libclang-dev nasm

# macOS with Homebrew (Xcode Command Line Tools are also required)
brew install cmake llvm nasm
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
```

Runtime deployment does not require a separate libultrahdr, libjpeg, libaom or
libavif installation — nc writes the AVIF container itself and statically links
every codec. The complete IJG attribution, Modified BSD terms, Adobe Gain Map
notice, and the libaom / Alliance for Open Media patent-license summary are in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Binary release packaging must
also include the exact libultrahdr, image_io, modp_b64, libjpeg-turbo, Adobe, and
libaom license files listed in that notice.

## License

TBD.

Third-party notices and license terms are collected in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
