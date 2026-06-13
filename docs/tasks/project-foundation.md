# Project Foundation and Core Types

## Goal

Stand up the Rust project for `nc` — Cargo manifest, dependency declarations,
module skeleton — and define the core types that every pipeline stage shares.
This is the root task; everything else builds on the types it establishes.

## Design

Single binary crate `nc` (working name). Module layout per the design spec §10:

```
src/
  main.rs        # thin: calls into cli
  cli.rs         # stub for now (filled by cli-framework)
  io/{decode,encode}.rs   # stubs
  pipeline/{film_base,color,stages}.rs  # stubs
  algo/{mod,simple,density}.rs          # stubs
  types.rs       # THIS task: the shared types
```

Core types in `types.rs` (pure data; no I/O):

```rust
/// Linear scanner image in f32, planar or interleaved RGB plus optional IR.
pub struct LinearImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,        // len = width*height*3, range ~[0,1]
    pub ir: Option<Vec<f32>>, // len = width*height when present
}

/// Per-channel unexposed-film base transmission (the Dmin anchor).
pub struct FilmBase { pub r: f32, pub g: f32, pub b: f32 }

/// Output bit depth selector.
pub enum OutDepth { U16, F32 }

/// Top-level error type for the whole tool.
pub enum NcError {
    Usage(String),       // exit 2
    Decode(String),      // exit 3
    Unsupported(String), // exit 4
    Write(String),       // exit 5
    Other(String),       // exit 1
}
```

Also declare the param structs that the recipe/CLI will populate (one struct per
stage: `FilmBaseParams`, `DensityParams`, `SimpleParams`, `PrintParams`,
`OutputParams`) so downstream tasks have a stable shape to fill in. Keep them
`#[derive(Serialize, Deserialize, Clone, Debug)]` with sensible `Default`s.

Declare dependencies in `Cargo.toml`: `clap`, `tiff`, `image`, `palette`,
`lcms2`, `serde`, `serde_json`, `rayon`, `kamadak-exif` (versions resolved via
Context7 / crates.io at implementation time).

## Implementation Suggestion

- Map `NcError` variants to exit codes in one place (a `fn exit_code(&self) -> i32`)
  so `cli-framework` and `pipeline-orchestration` reuse it.
- Keep `types.rs` free of any crate-specific image types — it's the neutral
  contract between stages. Conversions to/from `image`/`tiff` live in the io tasks.
- Stub modules can be empty `pub` modules or contain `todo!()`-returning fns just
  so the tree compiles.

## How to Verify

- `cargo build` succeeds with the full module tree and all declared deps resolving.
- `cargo test` runs (even if only a trivial type test exists).
- `LinearImage`, `FilmBase`, `OutDepth`, `NcError`, and the param structs are
  public and `serde`-(de)serializable where noted (a round-trip test on
  `DensityParams` passes).

## Dependencies

None — this is the root task.
