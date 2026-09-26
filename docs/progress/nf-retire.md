# Hanten — nf-retire Progress Log

Execution log for the `nf-retire` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status; this file is the
narrative beside it.

One `##` section per task in this epic, named by the bare task name. Read this
whole file before starting a task in this epic, and read other epics' `Epic
summary` sections when you depend on them. Append entries — don't rewrite earlier
ones.

## Epic summary

Remove the old paths once the reference build exists: `legacy`/`custom`, the bounded display tones, the sigmoid and `simple`, the `Dmax` anchor machinery, the regional balance, and the `print.*` prefix.

Created on 2026-09-19 as part of the new-flow migration plan (`docs/nf-migration.md`).
Landed so far: **`legacy-custom`** (2026-09-23), **`sigmoid-and-simple`** (2026-09-23),
**`display-tones`** (2026-09-24), **`dmax-machinery`** (2026-09-24),
**`regional-balance`** (2026-09-25), **`characteristic`** (2026-09-26).

**What `legacy-custom` means for the rest of the epic.**

- **One implementation of the print controls is left** — `render_split`'s, past the
  ACEScg boundary. `highlight_compress` now means only the display knee, which is the
  block `display-tones` was waiting on, and `print-prefix-rename` can rename without
  renaming twice.
- **Every preset is atomic.** `output.depth` / `output_profile` / `bigtiff` are gone;
  depth comes from `OutputParams::depth()` (a function of the preset) and BigTIFF is
  always `auto`. A future destination that wants an f32 or ICC choice adds its own knob.
- **The drift gate now hashes `algo::reconstruct`** (`stages::golden::reconstructed`)
  plus the default white balance's resolved gains, which reproduced the v5 `render`
  hash exactly — the default print was a bit-exact identity. `nf-verification/fingerprints`
  still owns deciding where the new `render` hash stops.
- **Tests state their preset.** `tests/pipeline.rs` no longer injects one; a TIFF
  test names `display-p3` (u16) or `film-master` (f32), and a test that needs
  clipping uses `--display-tone reinhard`, the one tone that overshoots white.

**What `sigmoid-and-simple` means for the rest of the epic.**

- **The current chain's default is the fixed decode's configuration** — the exponential
  at contrast 2.0 and `mid-at-base-offset(0.62)`, read from `algo::fixed`'s constants
  (`pipeline_version` 6). Both chains render the same default reconstruction, so
  `default-flip` no longer moves the decode, only the stages after it.
- **The default reads no `Dmax`.** A stated reference is carried and warned about
  (`unconsumed_dmax_warning`, keyed on `render_reads_the_reference`). What still reads one
  is `--anchor-white-at-reference` / `--anchor-mid-fraction` — `dmax-machinery`'s whole
  remaining surface. The report's `reconstruction_result.curve.dmax` still resolves
  `fixed` / 1.3 under the default placement even though nothing read it; that is left
  for `dmax-machinery` to remove rather than re-shaped here.
- **No reconstruction is bounded at white any more**, so `--display-tone none` refuses
  ordinary content on SDR; tests pull the fixture down with `--print-exposure`. That
  sharpens `display-tones`' case rather than blocking it. The default gain map is live.
- **`Reconstruction` is a struct** (`density` + tagged `curve`); `characteristic` is the
  only other curve, so `DensityCurve` has two members and `ConversionPreset` three — both
  `characteristic`'s to finish.

**What `display-tones` means for the rest of the epic.**

- **The display tone's one knob already lives at its final key**, `fit_range.headroom_stops`,
  on both chains (`ResolvedConfig::fit_range` is `recipe::FitRange`). `print-prefix-rename`
  has no display-tone key left to move — `print` is now exposure, black point, white
  balance and `linear_range` only.
- **`ConversionPreset` sets three knobs** (curve, `density.scale`, `print_exposure`), not
  the tone; the presets' old reinhard-at-6 is simply the default.
- **The current chain's SDR tone is fit range's operator bit-for-bit; its HDR form is not**
  (asymptotic base, strictly under the peak). `default-flip` retires `pipeline::sdr`/`hdr`
  and with them that difference. Zero headroom reports `"identity"` on both chains.
- **Old sidecars carry `"display_tone": "shoulder"` and are refused**, the old default
  included (it replays differently) — so a pre-v7 sidecar needs that key deleted before it
  replays. `highlight_compress: 0` is stripped.

**What `dmax-machinery` means for the rest of the epic.**

- **No reference density exists on either chain.** `AnchorPlacement` has one variant,
  `mid-at-base-offset`, still a tagged enum. `nf-calibration/anchor-comparison` placed the roll's white
  through `look.contrast` instead (2026-09-25), so no content-referenced variant is
  planned. `calibration` holds only
  `film_base`; `CalibrationParams` and `recipe::Calibration` now have the same shape.
- **Retired recipe keys follow one rule, `strip_retired_keys_at_old_defaults`:** the value every
  earlier build wrote by default is dropped on load, anything else is refused by
  `reject_legacy_recipe_keys`. A later retirement adds its key there.
- **The drift gate's `dmax=` line is a frozen literal** (`FROZEN_DMAX_LINE`); the text
  must stay byte-identical to what the recorded rows hashed.
- **The effective area has no `convert` consumer.** An empty region always warns, and
  `holder_applied` no longer suppresses the IR note; a future consumer must decide both.

**What `regional-balance` means for the rest of the epic.**

- **The current chain has no tone-dependent per-channel control** until `default-flip`;
  the grade (`look.channel_grade`) exists only under `--new-flow`. Every migration
  message says so rather than naming a flag the current chain refuses.
- **`reconstruction.density` is `scale` and `offset` only.** The three keys join
  `strip_retired_keys_at_old_defaults`, which now parses a triple as `f32`
  (`balance_triple`), as the old deserializer did — a later retirement of a numeric key
  should compare the same way, or a value the old build read as its default is refused.
- **`film-master` has no frame-local rule left**, only the downstream-control sweep, and
  no report or telemetry field carries a per-frame reconstruction measurement.
- **The drift gate's `render` text has two frozen lines now** (`dmax=`,
  `balance_range=`); both must stay byte-identical.

**What `characteristic` means for the rest of the epic.**

- **One curve, no selector.** `Reconstruction.curve` is an `ExponentialParams`; no recipe
  section is internally tagged any more (`merge_json`'s tagged-switch rule went). The
  old `"type": "exponential"` is dropped on load — a later retirement of a tag should do
  the same, since every sidecar carries it.
- **`--preset` is gone**, not just its names, and `nf-look/look-presets` closed without
  rebuilding it: precedence is `defaults < params < flags`, and no flag is a CLI-only
  expansion any more.
- **`density.scale` has one default** (`fixed::DENSITY_SCALE`); roll no longer
  re-resolves anything per curve by hand.
- **Telemetry is schema 7** and the report's curve block is `anchor` / `anchor_value`.
- **`film_stock` is `#[cfg(test)]`**; the scalar-path asset probes (`curve_probe`) are in
  git only.

## legacy-custom

**Status:** done
**Updated:** 2026-09-23

- 2026-09-19: created with the new-flow plan. Goal: retire `legacy` and `custom`.
- 2026-09-23: **plan.** Decisions taken with the user before starting: (1) retire the
  `--out-depth` / `--output-profile` / `--bigtiff` selectors now — once every preset is
  atomic they accept only their defaults; (2) `tests/pipeline.rs` states each preset at
  the call site rather than flipping the injection; (3) `benchmark.json` moves to
  `display-p3` / `film-master` as a holding set until `nf-verification/benchmark-set`;
  (4) keep v5 in the drift gate by hashing the reconstruction, no `pipeline_version` bump.
- 2026-09-23: **done.** Removed `OutputPreset::{Legacy, Custom}`, `is_atomic`,
  `stages::{render, render_legacy, reconstruct_and_print}` (film-master is now
  `stages::render_film_master`, whose signature carries no print parameters),
  `algo::finish_print`, `density::{render_print, soft_clip}`, `color::to_output` with
  `resolve_output_space`, `OutputSpace::{parse, ProPhoto, Custom}`, and the
  `validate_output_preset` atomicity and legacy-branch rules (the two left are
  renumbered: 1 film-master, 2 reinhard). All five names are removed-value errors (exit
  2) from flag and recipe alike; the selectors share one table,
  `cli::REMOVED_OUTPUT_SELECTORS`, so the two provenances say the same thing.
  - **Drift gate:** default print was a bit-exact identity on the golden vectors, so
    hashing `algo::reconstruct` with the `white_balance` line from
    `white_balance::resolve_print_gains` reproduced v5's `render` hash (`9c97b6954612c356`)
    and `base`. Only `recipe` moved (three output keys left the default document):
    refreshed in place to `9ca8dcca192e605a`, no bump — no default pixel moved.
  - **Goldens:** moved onto `algo::reconstruct`. The two customized ones carried a
    non-default print, so they were recaptured without it; re-applying the retired
    print arithmetic to the new bits reproduces the old captures to f32 rounding (worst
    relative error 2e-6). The three auto-WB goldens pinned the estimate on pre-matrix
    film RGB, a placement no chain has, and were deleted with ~15 print-stage unit tests.
  - **`tests/pipeline.rs`:** the injection is gone and `run_exact` merged into `run`.
    A temporary guard that panicked on any preset-less `.tif` convert found every call
    site that had relied on it (88 tests); each now names `display-p3` (or
    `film-master` / `hdr-linear-tiff` where it wanted f32). Four tests about legacy
    itself were deleted. Three clipping tests switched to `--display-tone reinhard`,
    since the shoulder never clips; the memory test's peak moved to the render phase
    (the SDR TIFF profile's measured peak).
  - **Scripts:** `real-scan-verify` recipes and `harness.sh` write `display-p3` and
    `hdr-linear-tiff`; `benchmark.json` is a `display-p3` / `film-master` holding set.
    nctool keeps *reading* `legacy`/`custom` and their encodings, because the
    reference build still writes them.
  - **Colorimetry:** `PROPHOTO` lost its one runtime consumer and stays for the
    `ADOBE_RGB` reason (nctool reads it); the lcms2-consumed set is four spaces now.
- 2026-09-23: **review round** (`/code-review high`). The one real regression: every
  sidecar and `--dump-params` recipe the previous build wrote carried the three
  retired output keys at their defaults, and the first cut refused any recipe that
  merely *contained* them — so no old recipe replayed, and the message blamed a flag
  nobody typed with a remedy that did not work. Now a key at its old default is
  dropped on load (`strip_old_default_output_selectors`, on `convert` and roll's
  per-frame override), and a non-default one is refused with "Remove the key". The
  selector table became `RemovedOutputSelector` rows looked up by key, so nothing
  depends on row order. Also: `convert_is_deterministic` runs `display-p3` again (the
  one TIFF path through the parallel lcms2 transform), and the drift gate's
  `white_balance` line is documented as echo-only — it covers no print arithmetic.
- 2026-09-23: **ship review** (diff-reviewer + Codex; Codex found nothing). The
  retired-flag errors told a `--new-flow` user to pick a preset, which that chain
  refuses; the remedy is now flow-aware ("drop it"), and the new-flow test asserts the
  preset advice is absent. The golden module's "two of twelve are not reference
  captures" became four of ten.

## display-tones

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: retire the `shoulder` and `none` tones.
- 2026-09-24: **plan.** Decisions taken with the user before starting:
  1. **`pipeline_version` 7.** The default tone moves from `shoulder` to reinhard at 6
     stops, which moves every default display render; the drift gate stops before the
     display stages, so the `render` hash is unchanged and only `recipe` moves.
  2. **No selector survives.** `fit-range` settled reinhard as the one operator, so
     `--display-tone` is a removed flag at every value (`reinhard` included — it is
     always applied), and `print.display_tone` is a migration error at every value
     (`shoulder` is the old default but replays differently; carrying a `reinhard`
     headroom across would be an alias). The headroom moves to `fit_range.headroom_stops`
     in `ResolvedConfig` — the new recipe's key, so neither `print-prefix-rename` nor
     `default-flip` renames it again. `--highlight-compress` is removed; the recipe key
     is stripped at its old default `0` and refused otherwise. `KneeWidth` goes;
     `Headroom` stays (it is the white point). Legacy HDR keeps its asymptotic-base
     `highlight_lifted_reinhard` until the flip — only SDR is fit range's operator
     bit-for-bit.
  3. **The over-range refusal survives on zero headroom.** `--display-tone-headroom 0` is
     the identity, and it now refuses an over-range sample on SDR as `none` did (HDR
     already did). A non-zero headroom keeps counting the SDR overshoot at the encode.
  4. **Report fields** `shoulder_start` / `highlight_compress` and `NO_TONE_CURVE` go.
- 2026-09-24: **implemented; all gates green, not yet reviewed.**
  - `DisplayToneCurve`, `KneeWidth`, the Hermite shoulders, `tone_curve_id` and the
    `bounds_*` predicates are gone; the renderers take a `Headroom`, and
    `Headroom::is_identity(crossover)` keys the zero-headroom refusal on both branches.
    `accepts_reinhard_tone` folded into `applies_display_tone`, and
    `validate_output_preset`'s rule 2 went with it.
  - **No presence rule for the headroom.** `film-master`'s value sweep catches a
    non-default headroom from either provenance, and a presence rule would have refused
    the flags-win reset (`--display-tone-headroom 6` over a recipe's value).
  - Conversion presets no longer own the tone (their expansion was reinhard at the
    default, which is now simply the default), so a recipe's headroom survives `--preset`.
  - The roll overlay guard `names_the_same_externally_tagged_variant` existed only for the
    reinhard struct variant and is deleted, as is the tone-switch headroom warning.
  - **A latent gain-map defect, found because this made reinhard the default.**
    `encode_legacy_gain_map` ratioed its luminance gain against the *rendered* SDR, not the
    stored `min(sdr, 1)` that `gain_pixel` uses. One far-over-white sample (a scan value of
    0 in `ultra_hdr_v1_native_reconstruction_covers_odd_dimensions_and_hdr_vectors`)
    drove `GainMapMin` to log2 −42 and the libultrahdr decode to garbage (white → PQ 0)
    at exit 0. Reproduced on `main` with `--display-tone reinhard`; fixed and pinned by
    `the_legacy_luminance_gain_ratios_against_the_stored_base` (mutation-checked).
  - Default `GainMapMax` on `hdr-48bit.tif` (`--film-base 0.9,0.55,0.42`): 1.88 → 0.93
    log2. **`--strict` at defaults can now fail** where it passed: the shoulder never
    clipped, while reinhard's SDR overshoot beyond the headroom is counted and warned
    (neither fixture clips at defaults; documented in `using-nc.md` §7). The task file's
    "`Headroom` leaves with `highlight_compress`" was wrong — `Headroom` is the white
    point and stays.
- 2026-09-24: **code review** (`/code-review`, 10 findings, all taken). Zero headroom now
  reports `"identity"` as the operator (report and both renderers' `tone_curve`), the new
  chain's rule via `fit_range::IDENTITY`; `Headroom` resolves the white point and gain once
  per frame instead of per pixel, and `convert_frame` resolves it once; the two chains'
  headroom refusals share `types::headroom_fault_message`; the zero-headroom errors name
  `fit_range.headroom_stops` beside the flag, since `roll` takes no flags;
  `Recipe::to_config` carries `fit_range`; three stale comments fixed.
- 2026-09-24: **ship review** (`ship:diff-reviewer` + Codex; Codex found nothing). Taken:
  CLAUDE.md's default `GainMapMax` (1.88 → 0.93); the `reinhard` recipe migration no
  longer claims a lost render (moving the headroom renders identically — only `shoulder`
  and `none` point at the reference build); the `none` remedy is scoped to display
  presets; `design-update.md`'s "Today" column and several stale sentences. **Declined:**
  stripping `"shoulder"` from `film-master` sidecars (it replays identically there). The
  recipe is loaded before the final preset is known — a flag can change it — so the strip
  would have to be preset-aware at load; the refusal is loud and its remedy is one key.
- 2026-09-24: **done.** Verified: all CI gates green (fmt, clippy `-D warnings`, build,
  801 unit + 236 integration tests after rebasing onto #155/#156, 392 `nctool` tests),
  `cargo doc` with no warnings, and `docs/using-nc.md` re-verified against the binary. The task's three checks
  hold: removed names get removed-value errors on flag and recipe, on both chains
  (`the_retired_display_tone_flags_are_refused_with_a_migration_error`, unit and binary);
  no rule, field or message names a retired tone except as history; zero headroom refuses
  over-range on SDR and renders it on HDR
  (`the_zero_headroom_ceiling_is_per_branch_not_one_reference_white`).

## sigmoid-and-simple

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: retire the sigmoid and `simple`.
- 2026-09-23: **plan.** Decisions taken with the user before starting: (1) the current
  chain's default moves off the sigmoid to the exponential at the fixed decode's
  configuration (`gamma` 2.0, `mid-at-base-offset(0.62)`, scale `[1, 0.84, 0.73]`), and
  `ExponentialParams::default` moves with it — a `pipeline_version` 6 bump; (2)
  `Reconstruction` collapses into a struct now, the wire's `"type": "density"` accepted at
  its old default and `"simple"` refused; (3) `sigmoid-knees` / `sigmoid-flat` become
  plain removed-value errors, and `scripts/sigmoid-baseline/` is deleted. Tests that used
  `simple` as a cheap fixture move to `FilmRgbImage::fixture`; sigmoid goldens and probes
  are deleted, not re-pointed.
- 2026-09-23: **done.**
  - **Default (v6).** `ExponentialParams::default` reads `algo::fixed::{CONTRAST,
    MID_ABOVE_BASE}` and `default_scale_for(Exponential)` returns `fixed::DENSITY_SCALE`;
    `fixed`'s `the_current_chains_default_is_this_decode` pins `Reconstruction::default()`
    to its equivalent configuration. New `PIPELINE_FINGERPRINTS` row (render
    `752e701021a41307`, recipe `dbac245a916032f2`, base unchanged); v5's behaviour frozen
    as a literal. `golden_new_default` recaptured. Telemetry schema 5 (`reconstruction`
    dropped, `curve` always present).
  - **Removed:** `algo/{sigmoid,simple}.rs`, `SigmoidParams`, `REFERENCE_SHOULDER`,
    `ReconstructionType`, `AnchorPlacement`'s `Default`, the sigmoid validate rules, the
    `simple` merge/validate arms (`--auto-wb` under `simple`, preset-over-`simple`),
    `active_density_domain_flag`, the `sigmoid-knees` rules
    (`brightness_is_in_the_anchor`), and `UnpinnedCurve::AnchorOnly`. Migration errors:
    `REMOVED_SIGMOID_CURVE` (recipe + a custom `--density-curve` parser),
    `REMOVED_SIMPLE_RECONSTRUCTION` (recipe + hidden `--reconstruction`), hidden
    `--sigmoid-*` flags naming `--density-gamma` / `--display-tone` / `--anchor-*`, and the
    two presets by name. `reconstruction.type` is accepted only at `"density"`.
  - **Tests:** the eleven module fixtures use `FilmRgbImage::fixture`; `all_configs`
    loops cover exponential + characteristic. Deleted: the two sigmoid goldens, the
    `simple` golden, `the_linear_rendered_sigmoid_takes_its_brightness_from_the_anchor`,
    `each_candidate_look_needs_its_own_print_exposure` (without the sigmoid looks the
    remaining three start within 0.125 stop of each other, under its 0.25 bar — the
    per-look exposures now rest on 1.59–1.91 across the characteristic presets), and ten
    `shadow_metrics` probes that rendered a sigmoid (`tone_map_review.html` with them).
    `curve_probe::sigmoid_scale` stays: it measures density slopes and renders no curve.
    In `tests/pipeline.rs`, `simple` fixtures were rebuilt on a stated exponential and
    `--display-tone none` tests pull the fixture down 2.2–5 stops.
  - **Found on the way:** with an unread default reference, the HDR SDR-range warning and
    both over-range errors advised levers that no longer worked (`--d-max`, "bound the
    reconstruction"); they now name `--print-exposure` / `--anchor-mid-offset`.
    `explicit_dmax_domain_warning` would have fired on every default `--d-max` run; it and
    `unconsumed_dmax_warning` now share `render_reads_the_reference`, so exactly one fires.
  - **Scripts:** `scripts/sigmoid-baseline/` deleted except `fixtures.json`, moved to
    `scripts/analysis/fixtures.json` (nctool's default and three docs read it).
    `scripts/hdr-tone-review/` deleted too — beyond the agreed plan: its page is the
    sigmoid study's measured findings, which a config swap would have left wrong.
    Preset matrix, `benchmark.json` and nctool's roll freeze (default curve
    `exponential`) updated.
  - **Asset probes:** the `#[ignore]`d `curve_probe` / `shadow_metrics` set panics before
    measuring on today's `../nc-assets`, whose rolls were renamed (`Ektar` →
    `2026-07-15-Ektar100`); unrelated to this change, and not fixed here.
- 2026-09-24: **review round** (`/code-review`). All ten findings taken. The ones that
  change behaviour: the legacy-recipe migration error no longer advises the refused
  `reconstruction.type = "simple"`; the HDR SDR-range warning names only
  `--print-exposure` (the `--anchor-*` family is refused under `characteristic`); and
  **`nctool roll convert` makes Dmax opt-in** (`--measure-dmax`, user decision) — it
  froze a leader Dmax the default placement never reads, so every frame warned and
  `--strict-roll` failed the roll. `--d-max` still freezes a stated value; the leader
  frame is required only when measuring. Also: `REMOVED_SIGMOID_CURVE` no longer claims
  the default sits "at the same anchor" (it names `--anchor-mid-fraction 0.5` as the old
  placement); the film-master negative-sample guarantee is back, as
  `io::encode`'s `the_film_master_branch_writes_a_negative_sample_unclamped` (fixture →
  mapper → split → f32 bytes); every "does the render read `Dmax`" question now goes
  through `render_reads_the_reference`; and stale `simple`/sigmoid prose went from
  CLAUDE.md, `stages.rs`, `gain_map.rs`, `recipe.rs` and the design-spec example.
- 2026-09-24: pre-ship review (Codex + local reviewer). The removed-sigmoid-flag
  remedies fire on both chains (`reject_removed_flags` runs before the flow table), so
  each now also names what works under `--new-flow` (`--anchor-mid-offset`; display
  tone "not yet available"), with a test that the named new-flow flag is accepted.
  `nctool roll convert` refuses `--dmax-region` without `--measure-dmax`, and drops a
  partial recipe's retired `reconstruction.type = "density"` before merging — `hanten
  params` no longer writes the tag, so the merge read it as a variant switch and
  replaced the whole default reconstruction. Stale prose fixed: `--no-d-max` is unity
  placement only under `--anchor-white-at-reference`; `--auto-d-max` *is* warned about
  under the base-derived anchors; telemetry heading is schema 5; comments citing
  deleted tests went. All gates green.

## dmax-machinery

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: retire the `dmax` anchor machinery.
- 2026-09-24: **plan.** Decisions taken with the user before starting: (1) the three
  placements other than `mid-at-base-offset` retire with the reference
  (`anchor-rule`'s outcome), so `AnchorPlacement` keeps one variant; (2) an old recipe's
  `calibration.dmax` is dropped at its old default `"fixed"` and refused otherwise, and
  `reconstruction.curve.anchor` likewise accepts only the surviving placement; (3)
  `nctool roll convert` loses `--measure-dmax` / `--d-max` / `--dmax-region` outright;
  (4) the report's `Dmax` fields and telemetry's `conversion.dmax` go (telemetry schema
  6); (5) no `pipeline_version` bump — the default reads no reference, so the drift
  gate's `render` hash must reproduce and only `recipe` moves.
- 2026-09-24: **done.**
  - **Removed:** `DmaxSource`, `DmaxInput`, `CalibrationParams::dmax`; the placements
    `WhiteAtDmax` / `MidAtDmaxFraction` / `BlackAtBase` and `reads_reference`;
    `DensityCurve[Type]::consumes_reference`; `algo::density::{resolve_dmax, auto_dmax*,
    reference_dmax, ReferenceDmax, NOMINAL_DMAX, MIN_PLAUSIBLE_REFERENCE_DMAX}` and
    `ReconstructionReport::dmax`; in `cli`, `DmaxOverrides`, `DmaxReuseReady`,
    `DmaxResolution` / `DmaxPolicy` / `DmaxProvenance` / `DmaxSetting`, `MasterAnchor`,
    `measures_over_region`, `region_reaches_a_rendered_pixel`,
    `render_reads_the_reference`, `unconsumed_dmax_warning`,
    `explicit_dmax_domain_warning`, the reference plausibility warning, film-master's
    auto-Dmax rule, roll's "Dmax NOT frozen" and per-frame `calibration.dmax` warnings,
    `estimate`'s Dmax half, report `dmax` / `dmax_region` / `d_max_flag` /
    `reconstruction_result.curve.dmax`, telemetry `conversion.dmax` (schema 6),
    `SamplePlan::with_rect`, and `flow.rs`'s seven now-unreachable rows.
  - **Migration:** `RemovedDmaxFlags` (hidden) → `removed_dmax_message`, shared by
    `convert` and `estimate --d-max-region`, runs in `reject_removed_flags` on both
    chains and names `--anchor-mid-offset`. Recipes: `REMOVED_ANCHOR_PLACEMENTS` in the
    curve deserializer; `calibration.dmax` stripped at `"fixed"`, refused otherwise.
    The sigmoid-flag remedies that named the retired placements now name
    `--anchor-mid-offset`.
  - **Goldens / drift gate:** the frozen reference captures (white pinned at 2.0 and at
    1.8) are reached through `mid-at-base-offset` at an offset that hits the captured
    anchor **bit-exactly** (`golden::offset_reaching`, asserted), so their bits did not
    move; the `none` and `auto` goldens pinned retired placements and were deleted.
    `render` (`752e701021a41307`) and `base` reproduced with the `dmax=` line frozen as
    `3fa66666`; `recipe` refreshed in place (after rebasing onto `display-tones`, on the v7 row: `b518f436e7f4317a`).
  - **Tests:** 768 unit, 231 integration, 387 nctool, all green. `tests/pipeline.rs`
    lost ten Dmax-only tests, gained
    `the_reference_density_and_retired_placements_are_migration_errors` (both chains,
    `estimate`, recipe replay at `"fixed"`, roll per-frame override).
  - **Memory, re-measured rather than assumed** (release builds of `95ee921` and this
    change, 18.66 MP HDRi frame — no 74.65 MP scan was on hand): peak RSS identical
    within 0.1% for `display-p3` (0.907 GB, model 1.079), `film-master` (0.683 / 0.821),
    `hdr-pq` (1.523 / 1.765), a full-frame `--base-region` convert (1.056 / 1.336) and a
    full-frame `estimate --base-region` (0.607 / 0.735); `gain-map-hdr` 1.69 GB on both
    (model 1.851; one base run read 1.46, i.e. run-to-run noise). All four outputs
    byte-identical across the two builds. The only rectangle removed,
    `--d-max-region`, was estimate-only, so no profile constant moved.
  - **Scripts / docs:** `nctool roll convert` lost `--measure-dmax` / `--d-max` /
    `--dmax-region` (a reference build's `"dmax": "fixed"` is dropped when freezing);
    `real-scan-verify` no longer measures the leader and its recipes dropped `dmax`
    (the `.provenance.json` records keep theirs — they record what was measured);
    `render-defaults-v2` now says it needs the reference build. The `--measure-inset`
    over-maximum error still named `--auto-d-max`; fixed. design-spec moved the anchor
    text from §7.3 (now sigmoid-only) to §7.2, and six `src/` citations followed.
- 2026-09-24: **review round** (`/code-review`). All nine findings taken. Behaviour:
  the removed-flag and sigmoid-placement remedies named `--anchor-mid-offset` /
  `--density-gamma` unconditionally, which the characteristic curve refuses — they now
  say "drop the flag" and scope those flags to the exponential; under `--new-flow` a
  recipe's `calibration.dmax` was told the current chain still reads it (moved to
  `recipe::RETIRED_KEYS`, "not a key of either chain"); a roll per-frame override is
  echoed as written, so one that restated `"dmax": "fixed"` no longer reports
  `{"calibration": {}}`; `nctool manifest roles` no longer requires a leader (the field
  stays, empty when absent), so the harness covers every roll with an unexposed frame.
  The `#[ignore]`d `shadow_metrics` probes read the leader `Dmax` from the recipes'
  `.provenance.json` records, which keep it, instead of the recipes. Also: a rustdoc
  left on the wrong function by the predicate deletion, stale test comments, and the
  oracle README's `_DMAX` step.
- 2026-09-24: **second review round** (`/code-review`). Taken: the `#[ignore]`d
  `shadow_metrics` probes drop their `% of Dmax` column (reading it from provenance
  records would break on the next harness freeze, which no longer writes it); the
  hidden removed flags take any value or none (`num_args = 0..=1`,
  `allow_hyphen_values`), so `--d-max` alone or `--d-max -1.5` reach the migration
  message rather than clap's; `nctool roll convert` checksums only the frames it reads
  (a missing leader no longer fails it); `film-master`'s auto-balance-range error and
  five doc comments stopped citing the retired auto `Dmax`; the residual-inset floor
  in `effective_area` is re-justified against `measure-roll`'s pooled p99 (a 1–2%
  holder ring is the size of that tail) rather than dropped; `sample_region_at` is
  private again. Declined: turning `SamplePlan` into an enum — its doc already says why
  it stays a struct, and reshaping the memory model is not this task's.

## regional-balance

**Status:** done
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: retire the regional balance.
- 2026-09-25: **plan.** Decisions taken with the user before starting: (1) the four
  flags are hidden migration errors on both chains at every value, `0,0,0` and
  `--auto-balance-range` included (the `dmax` precedent); since `--channel-grade` is
  new-flow only, the current chain's remedy is "drop the flag; under `--new-flow` the
  grade replaces it"; (2) a recipe's `shadow_balance`/`highlight_balance` at
  `[0,0,0]` and `balance_range` at `"auto"` are stripped on load, anything else
  refused — an equal non-zero pair is exactly a uniform offset, and its message says so
  (`reconstruction.density.offset` replays it identically); (3) the report's
  `balance_range` (convert and roll) goes, no constant stand-in; (4) no
  `pipeline_version` bump — the neutral default short-circuited bit-exactly, so the
  drift gate's `balance_range=` line becomes a frozen literal and only `recipe` moves;
  (5) the customized golden is recaptured with neutral balances **from the pre-removal
  build**, the auto-range golden is deleted; (6) the difference is documented on a
  synthetic crossover wedge (reference balance vs `--new-flow --channel-grade`), not
  on a real frame — none was ever rendered with a balance.
- 2026-09-25: **implemented; all gates green, not yet reviewed.**
  - **Removed:** `DensityParams::{shadow_balance, highlight_balance, balance_range}`,
    `BalanceRange`, `algo::density::{regional_balance, measure_balance_range,
    consults_balance_range, pixel_tone, smoothstep}` and the percentile constants;
    `ReconstructionReport` / `ConvertReport` / `Report` / roll `FrameStatus::Ok`
    `balance_range`; the balance `merge` arms and `validate` rules; `film-master`'s
    frame-local-range rule (its one remaining rule is the downstream-control sweep);
    `flow.rs`'s three balance rows and `BALANCE_REPLACED_BECAUSE`.
  - **Migration:** `RemovedBalanceFlags` (hidden, `num_args = 0..=1`,
    `allow_hyphen_values`) checked in `reject_removed_flags` on both chains, sharing
    `REGIONAL_BALANCE_RETIRED` with the recipe message. Recipes: the three keys stripped
    at their neutral defaults in `strip_retired_keys_at_old_defaults` (convert and roll
    per-frame), anything else refused by `removed_balance_recipe_message`. **The
    equal-pair remedy is exact only over a zero offset**: checked against the
    pre-removal build, an equal pair re-expressed as `--density-offset` is byte-identical
    at offset 0 and differs by f32 rounding otherwise (`(x + o) + s` against
    `x + (o + s)`), so the message says so.
  - **Drift gate / goldens:** `render` and `base` reproduced; the `balance_range=` line
    is a frozen literal (`FROZEN_BALANCE_RANGE_LINE = "-"`); `recipe` refreshed in place
    on the v7 row (`8d1fc292f2e6c723`). The customized golden's bits were captured from
    `b36ca64` (pre-removal) with its balances zeroed, then pinned against the new code
    unchanged. The auto-range golden went. Five `..DensityParams::default()` became
    needless updates (clippy) and were dropped.
  - **Tests:** 802 unit, 229 integration, 390 nctool. `tests/pipeline.rs` lost three
    balance tests and the film-master range case, gained
    `the_regional_balance_is_a_migration_error` (every flag and spelling on both chains,
    both remedies accepted, neutral sidecar replays byte-identically to its absence,
    non-neutral and explicit-range keys refused, equal-pair remedy, roll per-frame, no
    key in report / sidecar / `hanten params`); mutation-checked by disabling the strip
    and the flag check. Two new-flow tests that used `--shadow-balance` as their example
    now use `--film-stock`.
  - **The difference, measured (not asserted).** A synthetic wedge (generated in scratch, not
    committed): 26 neutral bands at `D′` 0.05–1.30 with a pure crossover about
    `D′` 0.62 — red density contrast ×1.08, blue ×0.94 — base `0.9,0.55,0.42`, read
    per band with `nctool metrics image --space display-p3`. Uncorrected, both chains
    render it alike (C\* within 0.03): 2.9 in the deepest band, 0.2 at mid, 13.9 at the
    top. **The balance**, hand-matched to the crossover at its measured `auto` range
    `[0.046, 1.304]` (`--shadow-balance=0.0456,0,-0.0342 --highlight-balance=-0.0544,0,0.0408`),
    leaves up to **C\* 2.16** (around `D′` 1.05) and ~1.0 across the lower mids: it is
    exact only at the range ends, because its smoothstep ramp cannot follow a linear
    crossover. **The grade** at a hand-matched `0.95,1.05` leaves at most **C\* 0.71**.
    Its "analytic" value `1/k` (`0.926,1.064`) over-corrects to C\* 7.2 at the top —
    the grade acts on ACEScg channels after the 3×3 and inside the look's contrast, so
    a density-exponent error does not translate one-to-one; match it by eye. **The
    fold**, on `film-master` with red balances `±1` over `[0.05, 1.3]`: red runs
    +1.86 → +2.43 → +0.27 → +3.15 stops across the wedge where the unbalanced red rises
    monotonically −3.95 → +4.74 — two densities onto one, as the task file said. The
    grade's exponent-spread bound rules that out.
  - **Docs:** `using-nc.md` (re-verified against the binary: `hanten params`, the
    new-flow refusals, film-master, the recipe migrations; header pin now `b36ca64`),
    design-spec §4 table, §6 film-master, §7.2 (model and a retirement note), §9 CLI
    list and examples, design-update's four mentions.
- 2026-09-25: **review round** (`/review-fix-loop`: Codex clean; `nc-reviewer` four
  findings, all taken, plus diff-caused doc staleness). The one that changes behaviour:
  an explicit `balance_range` beside **equal** balances (neutral included, an absent key
  counting as `[0,0,0]`) was never consulted — the old pass returned before reading it —
  so it replays unchanged; its message said "reproducible only from the reference
  build" and now says remove the key, the render is unchanged. Still refused, not
  stripped (only the old default strips). The equal-pair test compares as f32, as the
  old short-circuit did. **A regression of my own, caught here:** the §7.2 rewrite in
  design-spec had cut through to `### 7.3`, deleting the auto-neutral-WB paragraph, the
  fidelity-rule restatement and the `--highlight-compress` note; restored verbatim.
  Also: `RemovedBalanceFlags`' doc now says a bare stub followed by a valued flag hits
  clap's error (the `RemovedDmaxFlags` precedent); `render_split`'s "frame-local
  measurement" claim went; three open task files stopped citing deleted code
  (`consults_balance_range`, `pixel_tone`, `--auto-balance-range`). Pre-existing
  staleness left for the user: `TASKS.md` and `unfrozen-auto-mode-warning.md` still
  treat `dmax: "auto"` as live, `recipe-composition.md` lists `--auto-d-max`,
  `density-safety-bounds.md` cites the retired `render_print`.
- 2026-09-25: **second review round** (`/code-review`, seven findings, all taken). The
  strip and the message parsed "neutral" differently — f64 against the old
  deserializer's f32 — so `[1e-50, 0, 0]`, which the old build read as zero, was
  refused with an advice to add `[0, 0, 0]` to the offset; both now share
  `balance_triple` (f32), and a test pins the replay. The equal-pair remedies said
  "add it to the offset", but a roll per-frame override's arrays **replace** the shared
  recipe's and `--density-offset` **sets** rather than adds, so both now say to set the
  offset to the resolved one plus the pair. Two open task files lost their remaining
  claims that `--auto-d-max` / `dmax: "auto"` are live (lines this change had already
  edited); a doc-comment agreement fix in `stages.rs`. Gates green: 802 unit, 229
  integration, 390 nctool.
- 2026-09-25: **ship review** (`ship:diff-reviewer` + Codex; Codex found nothing). Taken:
  the flag message's offset remedy now carries the recipe message's qualifier (exact over
  a zero offset, otherwise to float rounding) and no longer says "on both chains" — the
  new chain never had the balance; `using-nc.md` §5 says "set", as the message does.
- 2026-09-25: **done.** The regional balance is gone from the current chain: four flags
  and three recipe keys are migration errors on both chains, their neutral defaults strip
  on load, and the report carries no balance range. Verified: all CI gates green (802
  unit, 229 integration, 390 nctool), `render`/`base` reproduced with `recipe` refreshed,
  the customized golden recaptured from the pre-removal build, `docs/using-nc.md`
  re-verified against the binary, and the difference documented on a synthetic crossover
  rather than asserted. The task's three checks hold: removed names refuse naming the
  grade (`the_regional_balance_is_a_migration_error`); no report, sidecar or
  `hanten params` output carries a balance; the crossover renders through the grade with
  the measured difference above.

## print-prefix-rename

**Status:** not started
**Updated:** 2026-09-19

- 2026-09-19: created with the new-flow plan. Goal: rename the `print.*` prefix.

## characteristic

**Status:** done
**Updated:** 2026-09-26

- 2026-09-19: filed after the plan review. Goal: retire the `characteristic` curve path.
- 2026-09-25: **plan.** Decisions taken with the user before starting: (1) **`--preset`
  itself retires**, not only its three names: with them gone `ConversionPreset` is empty,
  and a look-only bundle would hold one or two independent knobs (`look.contrast` is the
  roll's and a preset may not set it; `look.channel_grade` corrects the roll's crossover),
  which layered `--params` already names without code. The flag becomes a hidden
  migration error at every value, the expansion layer and the report's
  `conversion_preset` go, precedence becomes `defaults < params < flags`, and
  `nf-look/look-presets` closes as retired, not rebuilt; (2) **`DensityCurve` collapses to
  a struct** (the `Reconstruction` precedent): the wire's `"type": "exponential"` is
  stripped on load, `"characteristic"` refused; (3) **one-value surfaces go**:
  `--density-curve` is a hidden migration error at every value (the `display-tones`
  precedent), telemetry drops `conversion.curve` (schema bump), the report drops the
  curve type; (4) `density.scale` keeps one plain default, `fixed::DENSITY_SCALE`, and the
  three per-curve resolution sites go; (5) no `pipeline_version` bump — `render` and
  `base` must reproduce, `recipe` refreshes in place; (6) `scripts/preset-review/` is
  deleted whole, its only matrix being the three presets.
- 2026-09-25: **implemented; all gates green, not yet reviewed.**
  - **Removed:** `algo/characteristic.rs` and `algo/curve_probe.rs` whole;
    `DensityCurve`, `DensityCurveType`, `CharacteristicParams`,
    `DensityParams::default_scale_for`, `ReconstructionReport::out_of_table` (and
    `curve_anchor` is no longer an `Option`); in `cli`, `ConversionPreset`,
    `PresetExpansion`, `ConversionPresetResult` and the report's `conversion_preset`,
    `StockResult`, `OUT_OF_TABLE_WARN_FRACTION` and its warning, the curve-switch
    warnings and `curve_type_spelling`, `sets_curve_stock` / `sets_density_scale`,
    roll's per-frame gain reset, `merge_json`'s `internally_tagged_switch` (no recipe
    section is internally tagged any more), `parse_density_curve`, the `validate` branch
    and `merge` refusals that told the curves apart; `flow.rs`'s three rows (now
    unreachable); `OutputPreset::applies_display_tone`, `REFERENCE_CONTRAST`,
    `FilmStock`'s serde and `parse` (dead). `film_stock` is `#[cfg(test)]`.
  - **Migration:** `RemovedCharacteristicFlags` (hidden, any value or none) in
    `reject_removed_flags`, both chains; `ExponentialParams`' deserializer drops
    `"type": "exponential"`, refuses `"characteristic"` / `stock` with
    `REMOVED_CHARACTERISTIC_CURVE` (which names the `[1, 1, 1]` `density.scale` those
    sidecars carry — dropping only the curve would replay the wrong gain), and refuses any
    other tag.
  - **Found on the way:** the unpinned-curve probe keyed on `type == "exponential"`, so a
    curve without the tag (every recipe this build writes) would never have warned; it
    now keys on `gamma`/`anchor` alone, and the warning no longer says the recipe "pins
    `curve.type`". `fixed::DENSITY_SCALE`'s doc still claimed to be independent of the
    legacy default, which had read it since `pipeline_version` 6.
  - **Drift gate:** `render`/`base` reproduced; `recipe` refreshed in place
    (`53af9f2172093cac`). Telemetry schema 7.
  - **Tests:** 762 unit, 213 integration, 388 nctool. Gone: the preset, curve-switch and
    characteristic tests and the characteristic golden with its libm-window harness
    (`reachable_window`, cited in CLAUDE.md and `version.rs` as in git). Added:
    `the_characteristic_flags_are_migration_errors` (unit) and
    `the_characteristic_curve_is_a_migration_error` (binary: both chains before the
    missing-base rule, tagged-sidecar byte-identical replay, the recipe refusal and its
    remedy rendering, no retired field in report or sidecar). The "producer-agnostic"
    tests now pair `reconstruct` with `fixed::decode`; the midtone probes pin mid-grey
    through the curve's own anchor instead of an inverted datasheet patch.
  - **Docs:** `using-nc.md` §5–§6, §11, §12–13 and header (examples re-run against the
    binary), design-spec §2/§4/§5/§7/§8/§9 and the telemetry shape, design-update, CLAUDE.md,
    the `render-review-set` and `perf-telemetry` skills, three READMEs, and the open tasks
    that named the removed surface (`profile-authoring`, `recipe-composition`,
    `subcommands`, `reference-sweep`, `scanner-density-calibration`).
- 2026-09-26: **ship review** (`ship:diff-reviewer` + Codex). Taken: `benchmark.json`'s
  `hdri-exponential` case still passed `--density-curve` and so exited 2 on every
  `nctool compare run` — deleted (it was `hdri-default` once the flag went); stale
  prose in `using-nc.md` §4 (a `curve.stock` roll warning, per-frame curve-switch
  re-resolution) and design-spec's roll invariants; the whole-curve warning's remedy
  still said to write a "tagged" curve (a user following it got the next warning);
  stale `--density-curve` mentions in `recipe.rs`, `types.rs` and two tests; a merge test
  still passing the removed flag; `nctool`'s `_drop_retired_curve_tag` crashed on a
  non-object `reconstruction` (Codex; now left for `_freeze_recipe`'s message), and
  `_freeze_recipe` no longer writes back the tag this build dropped. The generated
  `curves.rs` and its emitter lost their last `--film-stock` mention.
- 2026-09-26: **done.** Verified: all CI gates green (762 unit, 213 integration, 388
  nctool), `render`/`base` reproduced with `recipe` refreshed, `docs/using-nc.md`
  re-verified against the binary (minimal recipe, §3 recipe and sidecar replay
  byte-identical; every quoted refusal re-run). The task's checks hold: a recipe naming
  `characteristic` and each preset name refuse naming the replacement; nothing resolves a
  per-curve `density.scale`; no message or help text recommends a removed flag. For
  dependents: `print-prefix-rename` has one curve and no `type` tag to carry;
  `nf-core/default-flip` inherits no curve selector; `io/scanner-density-calibration`'s
  probes live in git (`9b34848:src/algo/curve_probe.rs`).
