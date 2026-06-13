# Algorithm Interface

## Goal

Define the pluggable conversion interface so negative→positive algorithms can be
added without touching the rest of the pipeline, plus the selection mechanism
that maps an `--algorithm` name to an implementation.

## Design

`algo/mod.rs`:

```rust
/// A negative->positive conversion algorithm. Pure: no I/O, no hidden state.
pub trait Converter {
    /// Convert a linear scanner image to a linear positive image.
    fn convert(&self, img: &LinearImage, base: &FilmBase) -> Result<LinearImage, NcError>;
}

/// All shipped algorithms.
pub enum Algorithm { Simple, Density }

/// Build a boxed converter from the selected algorithm + its params.
pub fn build(algo: Algorithm, params: &AlgoParams) -> Box<dyn Converter>;
```

- `AlgoParams` is an enum/struct holding the per-algorithm parameter sets
  (`SimpleParams`, `DensityParams`) defined in `project-foundation`.
- `convert` takes the already-estimated `FilmBase` as input — algorithms consume
  it, they do not estimate it (that's `film-base-estimation`, joined at
  orchestration). This is why the algorithms depend only on this task.
- Selection from a string (`"simple"`/`"density"`) lives here too so the CLI can
  parse `--algorithm` into `Algorithm` and report unknown names cleanly.

## Implementation Suggestion

- Keep the trait minimal; push print/tone controls into each algorithm's params
  rather than widening the trait signature.
- Provide `Algorithm::from_str` returning `NcError::Usage` for unknown names.
- The two algorithm tasks implement `Converter`; this task can ship a no-op or
  `todo!()` placeholder impl behind a feature/test so it compiles standalone.

## How to Verify

- `cargo build` with the trait, `Algorithm` enum, `build()`, and `from_str`.
- `Algorithm::from_str("simple"|"density")` works; an unknown name returns
  `NcError::Usage`.
- A trivial test double implementing `Converter` can be built and called,
  confirming the trait is object-safe (`Box<dyn Converter>` works).

## Dependencies

- [Project foundation and core types](project-foundation.md)
