# negative-converter (`nc`)

A command-line tool that converts film **negative** scans into **positive**
images.

It reads high-bit-depth scanner files (SilverFast HDR/HDRi first), runs a
deterministic negative→positive pipeline in a 32-bit float linear working space,
and writes a TIFF (16-bit or 32-bit float).

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

- [`docs/design-spec.md`](docs/design-spec.md) — full design (architecture,
  pipeline, CLI surface, parameters).
- [`docs/TASKS.md`](docs/TASKS.md) — the build plan and dependency graph.
- [`docs/negative-convertor-research-report.md`](docs/negative-convertor-research-report.md)
  — background research.

## Usage (current CLI)

```sh
# Convert a negative scan to a positive 16-bit TIFF.
nc convert in.tiff -o out.tiff --algorithm density

# Full HDR float output with explicit controls.
nc convert in.tiff -o out.tiff --algorithm density --output-hdr \
  --film-base 0.92,0.55,0.42 --density-gamma 1.8 --print-exposure 0.0

# Inspect a scan and emit machine-readable JSON.
nc inspect in.tiff --report json
```

See the design spec for the complete command and parameter reference.

## License

TBD.
