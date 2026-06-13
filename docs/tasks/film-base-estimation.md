# Film-Base / Dmin Estimation

## Goal

Estimate the unexposed-film base transmission (`FilmBase`, the `Dmin` anchor)
from a scan — automatically from the film border, or from an explicit region —
and allow a full CLI override. This value anchors the density conversion.

## Design

`pipeline/film_base.rs`:

```rust
pub struct FilmBaseParams {
    pub explicit: Option<FilmBase>,     // overrides everything
    pub region: Option<[u32; 4]>,       // x,y,w,h to sample
    pub auto: bool,                     // default: estimate from border
}

pub fn estimate(img: &LinearImage, p: &FilmBaseParams) -> Result<FilmBase, NcError>;
```

Resolution order:
1. If `explicit` is set, return it.
2. Else if `region` is set, sample that rectangle.
3. Else (`auto`), detect the unexposed border and sample it.

Estimation within a region: take a robust per-channel statistic (e.g. high
percentile / median of the brightest, most-saturated unexposed area, since
unexposed negative is the densest/brightest-transmission base) and return it as
`FilmBase`.

Auto border detection (Step 1, heuristic): find the near-uniform brightest
margin region around the frame edge. Keep it simple and deterministic; if it
can't find a confident border, return a clear `NcError` (or a warning + fallback)
so the user can pass `--film-base` / `--base-region` instead.

## Implementation Suggestion

- Use a high percentile (e.g. 95th–99th) per channel rather than the raw max to
  resist hot pixels.
- Emit the estimated `FilmBase` and the region used into the JSON report so
  `estimate`/`inspect` can show it and an agent can reuse it as `--film-base`.
- Don't over-engineer auto-detection in Step 1 — a margin-sampling heuristic plus
  good override flags is enough; smarter detection can come later.

## How to Verify

- Explicit params short-circuit to the given `FilmBase`.
- A synthetic image with a known bright uniform border yields a `FilmBase` close
  to that border's values (within tolerance).
- An explicit `region` samples the right rectangle.
- Auto mode on a real negative produces a plausible base; failure path returns a
  clear, actionable error rather than a wrong silent value.

## Dependencies

- [Project foundation and core types](project-foundation.md)
