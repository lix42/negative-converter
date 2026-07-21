# Input Color Management

## Goal

Make `input.color` mean what design-spec §9 promises: convert the decoded input
into nc's linear working space through an ICC profile, instead of always assuming
raw-linear Rec.709/D65. This turns the three `InputColor` variants into real,
distinct behaviors and lifts the loud rejection of `--input-profile`.

Concretely, apply a scanner IT8 (or other supplied/embedded) ICC profile to the
decoded pixels *before* film-base estimation and density conversion, so the
pipeline starts from colorimetrically-defined values rather than uninterpreted
scanner integers.

> **Context.** The color-profile review (see `docs/progress.md`) found `Auto`
> silently equals `Linear` today: decode normalizes integers and every stage
> assumes linear Rec.709/D65 (`io/decode.rs`, `pipeline/color.rs`), and
> `run_convert` only rejects `Profile`. Survey of all `../nc-assets` scans:
> **none** carry an embedded ICC profile or colorimetry tags — they are raw
> `Gamma=1` Plustek OpticFilm 8300i / SilverFast HDR scans. So this is a
> forward-looking color-fidelity feature (enabled once the user makes an IT8
> profile for the scanner), **not** a fix for current sample output. The cheaper
> "honest default / fail-loud on embedded profile" option was deliberately
> skipped because nothing is released yet.

> **Global (cross-roll) tier.** A scanner IT8 profile characterizes the *device*,
> so it is constant across every roll — the canonical **global** config, one level
> above the per-roll recipe. The layering is: global defaults → roll recipe
> (`roll-conversion`) → per-frame override. This task delivers *consuming* an input
> profile per scan; wiring a device profile as a shared global layer above roll
> recipes is a roll-workflow concern (`roll-conversion` / `base-acquisition-planner`),
> and *creating* a profile from an IT8 target (scanner profiling) stays a non-goal
> (design-spec §2 out-of-scope / §12). See design-spec §12 item 18.

## Design

A new pure input→working transform stage, orchestrated by the CLI, sitting
**between decode and film-base estimation** — exactly where `pipeline/color.rs`
already reserves it ("any input→working conversion happens upstream of this
stage"). It uses `lcms2`, mirroring the existing output-side `to_output`.

```
decode → [input color → working space] → film-base → algorithm → output color → encode
```

Resolution of the source profile from `InputColor` (recipe key `input.color`):

- `Profile(path)` — load the user's ICC (e.g. the IT8 scanner profile) via
  `lcms2::Profile::new_file`. This is the primary intended path.
- `Auto` (default) — use the file's **embedded** ICC profile if present; else
  fall back to the documented linear Rec.709/D65 default (no transform). Because
  `Auto` and `Linear` then differ only when an embedded profile exists, and no
  current scan carries one, `Auto` ≡ `Linear` in practice today — but the
  promised semantics become real.
- `Linear` — no transform; pixels are already the working space.

The transform destination is nc's **working space** — linear Rec.709 primaries /
D65 white / linear TRC — *not* the output space. That working-space profile must
be defined once and shared with `pipeline/color.rs` (which currently only
*assumes* this space on its source side); factor it into a single
`working_space_profile()` so input and output agree by construction.

```rust
// pipeline/color.rs (or a sibling), lcms2:
/// The fixed working-space profile: linear Rec.709 primaries, D65, linear TRC.
fn working_space_profile() -> Result<Profile, NcError>;

/// Resolve the source profile bytes/handle for the input, if any transform is needed.
pub enum InputProfile { None, Embedded(Vec<u8>), File(PathBuf) }
pub fn resolve_input_profile(color: &InputColor, embedded: Option<Vec<u8>>) -> Result<InputProfile, NcError>;

/// Apply source→working transform in place over the RGB f32 buffer. No-op for `None`.
pub fn to_working(img: &mut LinearImage, src: &InputProfile) -> Result<(), NcError>;
```

### Constraints (must hold)

- **Range policy — the transform output must be a valid transmission domain.**
  An lcms2 transform of saturated / out-of-gamut input can yield channels
  **outside `[0, 1]`, including negative**. The density stage treats input as
  transmission and floors it with `SCAN_EPSILON` (`-(s.max(SCAN_EPSILON)/base).log10()`,
  `density.rs:170`) — a floor documented (`density.rs:152`) for the dead-pixel
  case, **not** for out-of-gamut color. Left unhandled, an ordinary out-of-gamut
  pixel becomes an extreme (near-black) density silently. So this task must
  **define and test** how `to_working`'s output is constrained to a valid
  transmission domain before film-base estimation / density — e.g. clamp to
  `[0, 1]` (and count/report clipping like the encoder does), or an explicit
  documented policy. Do not hand unbounded transform output to the density stage.
- **IR untouched.** Color-manage the 3 RGB channels only; carry the IR plane
  through unchanged (Step-1 rule: IR preserved, not consumed).
- **lcms2 global-error gotcha.** `Transform::transform_in_place` is infallible;
  Little CMS reports runtime faults only via the global `cmsSetLogErrorHandler`
  that `cli` already installs (`AtomicBool` + stderr). The input transform must
  clear the flag before and check it after, turning a CMS fault into a loud error
  (documented exit code) instead of a silently unconverted image — same pattern
  `run_convert` uses around the output transform.
- **Determinism.** lcms2 transforms are deterministic for fixed profiles +
  intent; keep it so. Same input + recipe ⇒ identical output.
- **Fail loudly.** A `Profile` path that doesn't exist / isn't a valid ICC, or an
  embedded profile that fails to parse under `Auto`, is a hard error with the
  documented exit code — never a silent linear fallback.
- **Rendering intent.** Add an intent knob (default relative colorimetric) as a
  proper CLI flag *and* recipe key — see the four-spot checklist below — or record
  in `progress.md` why a fixed intent is acceptable for Step 1.
- **Negative caveat (document, don't fix).** An IT8 scanner profile characterizes
  the scanner for the *film-as-object* (dyes + orange mask), not the scene. The
  stock-specific negative inversion remains the density stage's job; this
  transform only makes the starting colorimetry well-defined. Note this in the
  stage docs so it isn't mistaken for stock correction.

### Open design risk (resolve early)

Does the `tiff` crate surface the embedded-ICC tag (TIFF tag 34675,
`ICCProfile`) on decode? If not, `Auto`'s embedded path needs a raw-tag read in
`io/decode.rs`. Spike this before committing to the `Auto` embedded branch — if
it's costly, `Auto` may ship as "linear fallback only" initially with `Profile`
as the working path, and embedded detection tracked as a sub-item.

## Wiring a knob (the four coupled spots)

`input.color` already exists end-to-end. New knobs (rendering intent, and any
embedded-detection toggle) each need: a field in the CLI `*Overrides` struct
(`cli.rs`), the recipe `*Params` struct (`types.rs`), a `merge` arm, and usually
a `validate` check — plus a **merge test** (a forgotten merge arm makes the flag
a silent no-op). Recipe keys live under `input.*` per design-spec §9.

## How to Verify

- With a known ICC profile (synthetic or a real IT8), a neutral/known input patch
  transforms to the expected working-space value within tolerance; `Linear`
  leaves it unchanged.
- `Profile(<bad path>)` and a corrupt ICC fail loudly with the documented exit
  code; a CMS runtime fault (forced) is caught via the global handler, not
  silently passed through.
- `Auto` with no embedded profile == `Linear` (byte-identical output on the
  current `../nc-assets` scans — a regression guard that this feature doesn't
  perturb existing raw-linear conversions).
- IR plane is bit-identical before/after the input transform.
- `run_convert` no longer rejects `--input-profile`; design-spec §9 updated to
  drop the "not yet applied / rejected loudly" wording.
- Merge test for any new knob (intent) proves flags win over the recipe.
- Exercise end-to-end on a real scan with a real IT8 profile once the user
  produces one (throwaway `#[ignore]` test that prints derived numbers only;
  never read sample pixels into context).

## Dependencies

- [Color management](color-management.md) — defines the working space and the
  lcms2 output-side patterns this mirrors; the shared `working_space_profile()`
  is factored out here.
- [Pipeline orchestration](pipeline-orchestration.md) — owns `run_convert`, where
  `InputColor` is resolved and where the new stage is invoked (and where the
  `Profile` rejection is lifted).
