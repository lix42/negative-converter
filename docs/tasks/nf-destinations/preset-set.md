# The destination set

## Goal

Settle which destinations a render can go to under the new flow, how they are
selected alongside `--new-flow`, and how each one's file suffix is decided. This is
the list and the selection rules; the individual destinations are separate tasks.

## Design

What is known:

- **Ten preset names ship today**: `legacy` and `custom` retired with the print path
  (`nf-retire/legacy-custom`, 2026-09-23), taking `to_output`, ProPhoto, arbitrary ICC
  and the `--out-depth` / `--output-profile` / `--bigtiff` selectors with them.
  `film-master` is not a rendering destination at all — it is the reconstruction
  output, and runs no rendering stage.
- **Two abilities move across from legacy**: Adobe RGB output is a must-have (it
  was reachable only through an ICC path on legacy), and a rendered float TIFF is
  good to have (`hdr-linear-tiff` is the current one). Retiring a path is a list of abilities to re-add, not a loss.
- **The Adobe RGB render** ([`output/adobe-rgb-gamut`](../output/adobe-rgb-gamut.md),
  2026-09-24): `DestinationGamut::AdobeRgb` through fit gamut, and
  `color::encode_display_linear` applies its `563/256` curve and embeds a named
  profile. `--gamut adobe-rgb` (recipe `output.display.gamut`) selects it — the
  destination's gamut reaches fit gamut through its `DisplayTarget`.
- **`--new-flow` is scaffolding, not a feature** (`docs/nf-migration.md`): CLI-only,
  never a recipe key, and it dies when the default flips — so destination selection
  must not be built on it.
- **Suffix derivation is carried, not redesigned.**
  [`output/output-path-suffix`](../output/output-path-suffix.md) shipped it
  (2026-09-22); this task states each destination's accepted spellings. The seam it
  left is `cli::container_for` — the **only** preset-shaped step, with the accepted
  set and the supplied spelling both hanging off `Container`. So the open question
  below (name vs product of selectors) changes that one function under either
  answer, and nothing about suffixes needs re-deciding. Keep it an exhaustive match
  so a moved `OutputPreset` fails to compile.
- **`nctool` needs one row per destination in two lookup tables** — the only
  tooling change the migration owes.
- **A new-flow gain-map destination consumes `chain::render_pair` and
  `pipeline::gain_ratio`** (`nf-display-stages/branch-contract`, 2026-09-24): the
  pair gives the SDR base and the HDR alternate from one look, and `gain_ratio`
  gives the per-channel gains. Their rules: the gain is ratioed against the base
  **as stored** (clamped to `[0, 1]`), never the unclamped rendition; and a flat map
  (`GainRange::flat`) is reported rather than shipped silently.
- **The gain-map destination must clamp HDR to its peak and count what it clamps**,
  as the single-rendition encoders do. Fit range sets no hard ceiling (decided
  2026-09-24) because the encoder clamps and counts above-peak samples, but
  `gain_ratio::between` clamps the HDR rendition only to `>= 0`: a sample above the
  peak (measured up to about `2 P` at headroom 2) would otherwise become a larger
  gain, uncounted.

Decisions (user, 2026-09-25):

- **A destination is a small product of separate knobs, not a name per
  combination** — e.g. range × gamut × container, and perhaps the gain-map dialect.
  A bundle needs a specific reason. Combinations the code cannot produce are refused
  at the CLI. Orthogonal selectors are the default shape.
- **New flow only.** The selectors are new, exist only under `--new-flow`, and have
  their own `output` section in the new chain's recipe (`crate::recipe`); whether their
  names match the old presets does not matter. `--output-preset` stays on the legacy
  chain until `nf-core/default-flip`, and under `--new-flow` it is refused, naming the
  new flags.
- **HDR destinations stay available before the default flips**, if possible: the gain
  map through `chain::render_pair` and `pipeline::gain_ratio` (ratioed against the base
  as stored, HDR clamped to the peak and counted, a flat map reported), and the PQ,
  HLG and linear-float paths.

The shape (user, 2026-09-25, after the risks below):

- **Four axes** — `--range sdr|hdr`, `--transfer native|linear|pq|hlg` (`native` is the
  gamut's own curve), `--gamut display-p3|adobe-rgb|bt2020`, `--container
  tiff|jpeg|avif` — in the recipe's `output` section, each optional. One table of
  rows drives resolution, refusals, help and the container.
- **An axis left unset is derived from the table**, in a fixed order: its default if a
  row consistent with what is stated has it, else the one value left, else a refusal
  listing the choices. The report records every resolved axis.
- **`film-master` is its own selection**, not a value on the axes: it runs no
  rendering, so range, gamut and transfer do not apply to it.
- **The gain-map JPEG is a separate task**
  ([`gain-map-destination`](gain-map-destination.md)): it needs a multichannel ISO
  21496-1 container the tree does not have. Its row is refused as "not yet".

Risks to settle in the design, raised with the decisions:

- Most combinations are invalid (a gain map needs a JPEG; PQ/HLG need BT.2020; Adobe
  RGB is SDR-only; AVIF is HDR-only today). The refusal rules, the help text, the parse
  diagnostics and `cli::container_for`'s suffixes come from **one** table, on the
  `OutputPreset::ALL` precedent, so they cannot drift.
- An axis default that depends on another axis (gamut defaults to Display P3, but HDR
  needs BT.2020) reopens the presence-vs-value asymmetry `--out-depth` needed. Record the
  *resolved* values in the report so a replay is exact, and diagnose the most specific
  fault first.
- `film-master` runs no rendering, so it is likely a selection of its own rather than a
  value on these axes — the one case with a reason to be a single name.
- `nctool` keys its two lookup tables on a destination name; with knobs, the row key
  has to be decided.

## How to Verify

- Every destination in the list resolves end to end, states its suffixes, and is
  covered by a parse diagnostic generated from the same list (the `OutputPreset::ALL`
  precedent, so the name list and the help text cannot desynchronize).
- `hanten roll` derives a name for each; `docs/using-nc.md` updated by running the binary.
- `film-master` runs no rendering stage, so it refuses a request for any stage it does
  not run — scene correction, the look, fit range — naming the stage rather than a knob:
  **one** rule per stage, never one per knob (`nf-look/stage`), each sparing its default
  and its identity. For the look (`nf-look/path-to-white`):
  Key it on `LookSection::asks_for_a_look` — neither the default nor empty. Not "any
  look at all": highlight desaturation is on by default, so that would refuse every
  default recipe. Not "not the default" either: an empty look
  (`--contrast 1 --highlight-desaturation 0`) renders exactly what `film-master` does, and refusing that identity kills the
  flags-win reset.
- One recipe drives the same look through every destination that runs one, and the
  report names the look stage on each, empty or not.

## Dependencies

- [The SDR/HDR branch contract](../nf-display-stages/branch-contract.md)
- [Derive the output suffix from the resolved preset](../output/output-path-suffix.md)
