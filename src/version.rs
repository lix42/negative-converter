//! Conversion identity: what produced an output.
//!
//! Three independent layers, all stamped into the JSON **report** (and mirrored
//! into the sidecar's `meta` envelope — never as recipe keys; see
//! `io::encode::write_sidecar` and `cli::load_recipe_for`):
//!
//! 1. **Build identity** — crate semver ([`NC_VERSION`]), the git commit the
//!    binary was built from ([`git_commit`] / [`git_dirty`], captured by
//!    `build.rs`), and the compile target ([`TARGET`]). Answers "which binary".
//! 2. **Behavioral pipeline version** — [`PIPELINE_VERSION`], an integer that is
//!    **independent of semver** and bumps *only* when the **default** conversion
//!    behavior changes. Answers "would this build render my frame differently".
//! 3. **Params hash** — [`stable_hash`] over the canonical resolved-recipe JSON.
//!    Those are the exact bytes `--dump-params` writes; the sidecar's `params` body
//!    is the same *document* but not the same bytes, because nesting it under
//!    `params` re-indents every line by two spaces (`cli::canonical_params_json`).
//!    Answers "was this the same configuration".
//!
//! All of it is **operational metadata**, in the same class as `--report` and the
//! telemetry flags (CLAUDE.md): it is not a conversion knob, has no CLI flag or
//! recipe key, and must never perturb a single output pixel.

use std::sync::OnceLock;

use serde::Serialize;

/// Crate semver — the *release* identity, which moves for any change (docs, a
/// refactor, a new flag), not just behavioral ones. That is exactly why
/// [`PIPELINE_VERSION`] exists beside it.
pub const NC_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Compile target triple (from `build.rs`). Part of identity because nc's
/// determinism contract is byte-identity **per build/architecture** (design-spec
/// §8): transcendental libm results and the lcms2 transform differ by target, so a
/// cross-target comparison must be read as such rather than as a behavior change.
pub const TARGET: &str = env!("NC_TARGET");

/// Raw `build.rs` capture: the short commit hash, or `"unknown"` when the build
/// tree had no usable git (source tarball, no `git` on `PATH`, or a repository that
/// is not this package's — see `build.rs`).
const GIT_COMMIT_RAW: &str = env!("NC_GIT_COMMIT");

/// Raw `build.rs` capture: `"true"` / `"false"` / `"unknown"`.
const GIT_DIRTY_RAW: &str = env!("NC_GIT_DIRTY");

/// The **behavioral** pipeline version: bumped *only* when the **default**
/// conversion behavior changes (default reconstruction/params, the density curve,
/// the anchor placement, the film-base source and its detector, auto white balance,
/// the working-space mapping) — never for a refactor that preserves default
/// pixels, and never for a new opt-in knob.
///
/// History (the label a comparison is keyed on):
///
/// | version | default render |
/// |---|---|
/// | 0 | the Step-1 MVP baseline recorded in `docs/reports/v0-baseline.md`: per-frame `auto` `Dmax` (99.5th-percentile density), exponential curve, no auto WB |
/// | 1 | every default change since that baseline, collapsed into one label: `film-base/dmax-reference` replaced the per-frame anchor with the roll-fixed nominal `Dmax = 2.0` **density**, `film-base/auto-base-redesign` replaced the auto film-base detector with the inward-scan rebate detector, and `core/input-semantics` added the stage-1b transfer/meaning resolution. The tagged-`reconstruction` split was proven bit-identical and is *not* part of the change. |
/// | 3 | the output-preset default migration (2026-08-09, `output/presets`): the default `output.preset` became **`gain-map-hdr`**, a dual-dialect gain-map JPEG, where it was `legacy` (16-bit TIFF). This is a **container** change as much as a render one — `hanten convert -o out.tif` with no preset is now a usage error — and the pixels differ because the default path crosses the ACEScg boundary into the SDR/HDR display renderers instead of running `finish_print` before the ICC transform. `legacy` is unchanged and still reachable by name. The row's `render`/`base` fingerprints are **unmoved**: they measure `reconstruct_and_print` and `film_base::estimate`, neither of which the preset selects — which is exactly the coverage limit `PipelineFingerprint` documents, so this row's evidence is the report in `docs/reports/render-defaults-v3.md`, not the gate. |
/// | 4 | the per-channel density gain `density.scale` `[1, 1, 1]` → **`[1, 0.90, 0.86]`** (2026-09-09, `algo/film-stock-profiles`). The scalar reconstruction path leaves `contrast · (D'_c − D'_R)`, so a channel whose density rises faster than red drifts against it across the tone scale; measured over 21 real frames, green ran +0.79 and blue +1.26 stops per unit density. This gain cancels both (green +0.02, blue +0.12). Blue's `0.860` is the manufacturers' published per-channel structure, which reproduces at 98%; green's `0.900` is calibrated from scans because the published `0.977` measured only 49% of the real drift. Every default pixel moves, and colour more than tone. Evidence: `algo::curve_probe::sigmoid_scale` and `docs/progress/algo.md`. |
/// | 5 | the same gain again, `[1, 0.90, 0.86]` → **`[1, 0.84, 0.73]`** (2026-09-16, `io/scanner-density-calibration`). Calibrated from **31 hand-marked neutral patches** over five rolls instead of from the tone-scale slope: each roll's median nulling scale, averaged with equal weight per roll, gives green 0.837 and blue 0.733. Blue is the half that holds — every roll wants 0.68–0.78, so v4's `0.860`, taken from the manufacturers' published per-channel structure, overcorrects on this scanner. Green **splits by scan date** (July rolls 0.86–0.90, September ~0.77, consistent with a change of developer), so `0.84` is a deliberate compromise fitting neither group exactly. Shipped on a visual verdict over five rolls with an NLP reference beside them, where it beat v4 on every frame but one — `2026-07-15-Ektar100/991` reads green-yellow, which is the overshoot the July patches predict. Every default pixel moves, and colour more than tone. Evidence: `docs/progress/algo.md` (2026-09-16). |
/// | 6 | the default density curve sigmoid → **exponential at the fixed decode's configuration** (2026-09-23, `nf-retire/sigmoid-and-simple`): contrast 2.0, mid-grey pinned 0.62 density above the film base (`mid-at-base-offset`), the same `[1, 0.84, 0.73]` gain. It is `algo::fixed`'s decode on the current chain, bit-identically (`the_fixed_decode_matches_the_equivalent_legacy_configuration`), so the two chains now render the same default reconstruction. The sigmoid's toe and shoulder were a rendering fused into the decode; the default anchor no longer reads the roll's reference density. Every default pixel moves, highlights most (nothing is compressed at white any more). `simple` retired in the same change, which moves no default pixel. |
/// | 7 | **current** — the default display tone `shoulder` → **extended Reinhard at 6 stops of headroom** (2026-09-24, `nf-retire/display-tones`), fit range's operator. `shoulder` and `none` retired with `highlight_compress`; the headroom moved to `fit_range.headroom_stops`. Every default display pixel moves — the whole curve is compressed rather than only the top, with mid-grey held at 0.18 — while `render` and `base` are unmoved, since the gate stops before the display stages. Evidence: `docs/progress/nf-retire.md`. |
/// **Contested, and deliberately left at 3 — read this before assuming it settled.**
/// `film-base/ir-usability-detection` (2026-09-04) turned the IR holder-mask
/// detector from opt-in behind `--film-type chromogenic` into the default for every
/// HDRi `auto` run. That is not covered by the "never for a new opt-in knob"
/// exemption above, and "the film-base source **and its detector**" is listed as a
/// trigger — so the written rule points at a bump. The reasons it did not get one,
/// and the case against them:
///
/// - *For 3:* bumping is not free. `pipeline_version_warning` fires on **any**
///   mismatch with the text "the output will not match the original", so replaying
///   an archived sidecar that states an explicit `--film-base` — which renders
///   bit-identically — would exit 1 under `--strict` on a false claim. And on the
///   assets to hand, `--auto-base` refuses on every real frame tried (11 frames, 6
///   rolls), so no frame has been *demonstrated* to change output.
/// - *Against:* "not demonstrated" is not "cannot". Where auto does resolve a base
///   on an HDRi scan and the mask changes which candidate wins, two files both
///   labelled `3` differ — exactly what this label exists to prevent. The
///   `ir-holder-detection` precedent cited for staying is **not parallel**: it
///   shipped behind a flag, which this doc exempts. The `estimation` precedent
///   (v1 staying v1) is not parallel either — there, no pixel moved at all.
///
/// **Raised in review and reaffirmed (2026-09-05, PR #104):** an automated
/// reviewer filed this as a P1 to bump, arguing that explicit-base recipes
/// replaying unchanged is the normal tradeoff of a *pipeline-wide* version. The
/// owner's decision was to stay at 3, on the narrower ground above: `--auto-base`
/// is best-effort and refuses on every real frame tried (11 frames, 6 rolls), the
/// supported workflow is measure-once-and-reuse with an explicit base, and a v4
/// row would carry render/base/recipe hashes **identical** to v3's — a new label
/// with nothing the gate can point at. Whoever next changes the film-base detector
/// inherits a live question, not a settled one: if a frame is ever found where
/// auto resolves a base and the mask changes which candidate wins, that is the
/// evidence this decision was missing, and the bump follows.
///
/// **Note (2026-09-10):** the version has since moved to 4 for an unrelated reason —
/// the `density.scale` default above — so "left at 3" records the *IR decision*, not the
/// current label. That decision stands on its own ground and is unaffected: the IR
/// detector still has no demonstrated pixel change, and v4's row carries hashes that
/// differ from v3's, which is what v4-for-IR would have lacked. The live question above
/// is still live.
/// | 2 | three render defaults moved together (2026-08-08, `algo/negative-reconstruction-density-curves`): the nominal `Fixed` anchor `Dmax = 2.0` → **1.3**, the default density curve exponential → **sigmoid** (mid-grey anchored), and `ExponentialParams::gamma` 1.0 → **2.0** for anyone still selecting that curve explicitly. Measured in `docs/reports/render-defaults-v2.md`. Film-base estimation is untouched, which is why the row's `base` fingerprint is unchanged. |
///
/// **The v1 row is a collapse, not a single step.** `docs/reports/v0-baseline.md`
/// measured its numbers with an **explicit** `--film-base`, so those numbers stay
/// comparable across the film-base redesign; the *default* render, however, crossed
/// three boundaries between v0 and v1, and only one label was available to record
/// them. That is a known limitation of retrofitting the constant, not a claim that
/// nothing else moved.
///
/// **v1 stayed v1 when `film_base.source` lost its default**, because no default
/// pixel moved: a recipe that states a base renders bit-identically, and the v1
/// row's `render`/`base` fingerprints are unchanged. What changed is that a recipe
/// stating *nothing* is now refused (exit 2) instead of quietly detecting — a
/// contract change, not a render change. Bumping for it would have been actively
/// harmful: `pipeline_version_warning` fires on any mismatch, telling a user
/// re-running an archived v1 sidecar that "the output will not match the original"
/// when it matches exactly. The row's `recipe` hash was refreshed in place (the one
/// field this table sanctions editing) and `PIPELINE_BEHAVIOR` was amended to drop
/// its film-base clause — see both for why that amendment is permitted here.
///
/// Version 0 predates this constant, so no fingerprint of it can be computed from
/// the current tree; `docs/reports/v0-baseline.md` is its record. From 1 onward the
/// pairing is machine-enforced by the drift gate below (`PIPELINE_FINGERPRINTS` +
/// `mod drift_gate`): change a default *within the fingerprinted stages* and that
/// test fails until the fingerprints **and** this constant are updated together.
/// Read `PipelineFingerprint` for exactly which stages those are — the gate is not
/// whole-pipeline coverage and must not be described as if it were.
pub const PIPELINE_VERSION: u32 = 7;

/// The recorded ⟨`pipeline_version`, fingerprints, behavior⟩ rows — the
/// machine-enforced half of "the behavioral version cannot silently drift" (see
/// [`PipelineFingerprint`] for what each fingerprint covers, what it does **not**,
/// and why each is safe cross-platform).
///
/// The drift gate looks [`PIPELINE_VERSION`] up here and compares the fingerprints
/// it computes from the live code against the row:
///
/// - **Fingerprint changed, version didn't** → the row mismatches → CI fails. This
///   is the case the task exists to catch.
/// - **Version bumped, no row** → nothing to compare against → CI fails, telling
///   you to record the row (the message prints the computed values, so it is a
///   copy-paste). A bump without a recorded fingerprint would leave the *new*
///   version undefended, which is why the bump is not free.
///
/// **A recorded row is history, not a scratchpad.** The only edit-in-place this
/// table sanctions is refreshing `recipe` for a new opt-in knob with a neutral
/// default (see [`PipelineFingerprint`]). Editing an existing row's `render` or
/// `base` would make one version label two different behaviors — silently
/// destroying the very attribution the table exists to provide. Add a row instead.
///
/// Test-only: nothing at runtime reads it, and gating it on `cfg(test)` keeps it
/// out of the shipped binary without needing a `dead_code` allow.
#[cfg(test)]
pub const PIPELINE_FINGERPRINTS: &[PipelineFingerprint] = &[
    PipelineFingerprint {
        pipeline_version: 1,
        render: "1fce7367c4bfec58",
        base: "01c5acccc36a3388",
        // Refreshed in place when `film_base.source` lost its default: the default
        // document now carries `"source": null`, so the recipe hash moved. `render`
        // and `base` are byte-identical to what this row has always carried — no
        // pixel moved, and a recipe that states a base renders exactly as it did —
        // which is precisely the "new value in the default document, no default
        // pixel change" case this field sanctions editing for.
        recipe: "5bd3e903db3a02b5",
        // `behavior` is a **string literal** here, not `PIPELINE_BEHAVIOR`. It used to
        // be the constant, back when v1 was current; the v2 bump below took the
        // constant over, so v1's description had to be frozen as the text it carried
        // — that is the whole point of recording the description per row. The literal
        // is v1's final text: it had been *amended* once (dropping an opening "auto
        // rebate film base" clause when `film_base.source` lost its default), which
        // was permitted only because v1's `render`/`base` did not move, so the render
        // v1 labels was unchanged and the removed clause described a resolution step
        // no longer part of any default render. `render` and `base` remain never-edit.
        behavior: "roll-fixed nominal Dmax 2.0 density, exponential density \
     curve, no auto white balance",
    },
    // v2 — the default *render* changed, which is what this table exists for.
    // Three defaults moved together (2026-08-08):
    //   * `NOMINAL_DMAX` 2.0 → 1.3, because every roll measured in this repo
    //     lands 0.90–1.74 and an anchor above all of them darkened the frame by
    //     the difference (5.09x linear on the Ektar reference);
    //   * the default curve exponential → sigmoid, which pins mid-grey instead of
    //     white and so can place the black floor and the midtones at once;
    //   * the exponential's own `gamma` 1.0 → 2.0, taking its black floor
    //     72 → 12/255 for anyone who still selects it explicitly.
    // `base` is unchanged: none of these touch film-base estimation.
    PipelineFingerprint {
        pipeline_version: 2,
        // Frozen literal, not `PIPELINE_BEHAVIOR`: the v3 bump took the constant over
        // (see v1's row for the same handover).
        behavior: "roll-fixed nominal Dmax 1.3 density, mid-grey-anchored sigmoid curve, \
                   no auto white balance",
        render: "9beca8b24eb785b0",
        base: "01c5acccc36a3388",
        // **Left at the value a v2 build actually emitted** (`"hdr": false`), not
        // refreshed for the `output.hdr` → `output.depth` rename. It was briefly
        // refreshed to `662384df7a2255dd` while `PIPELINE_VERSION` was still 2, which
        // was legitimate at that instant — but the same change went on to bump to v3,
        // and nothing ever shipped a v2 build emitting the new key. Leaving the
        // refreshed hash would have made this row an unverifiable claim about a
        // document shape v2 never wrote, and `drift_gate` cannot catch it: it only
        // recomputes the row matching the *current* `PIPELINE_VERSION`.
        //
        // The rule the field's doc states is "refresh the **current** version's
        // recipe hash for a neutral-default change without bumping". Once a bump
        // lands in the same change, the prior row is history again.
        recipe: "3d37b13ecb7a5095",
    },
    PipelineFingerprint {
        pipeline_version: 3,
        // **Unchanged from v2, and that is the point to read before trusting this
        // row.** The two fingerprints cover `reconstruct_and_print` and
        // `film_base::estimate`; the output preset selects neither, so the gate
        // cannot witness the default's move from a TIFF through `finish_print` to a
        // gain-map JPEG through the display renderers. Only `recipe` moved. The
        // before/after evidence for this version is
        // `docs/reports/render-defaults-v3.md`.
        render: "9beca8b24eb785b0",
        base: "01c5acccc36a3388",
        // Refreshed in place when `print.display_tone` was added (`output/linear-render`):
        // a new opt-in knob whose default (`shoulder`) is the behaviour v3 already had,
        // so the default document gained a key while no default pixel moved — `render`
        // and `base` are byte-identical. That is the one case this field sanctions
        // editing for; see `docs/progress/core.md`.
        recipe: "a26e8ec6434e8ebc",
        // Frozen literal, not `PIPELINE_BEHAVIOR`: the v4 bump took the constant over
        // (see v1's and v2's rows for the same handover).
        behavior: "gain-map-hdr default output (dual-dialect gain-map JPEG), roll-fixed \
                   nominal Dmax 1.3 density, mid-grey-anchored sigmoid curve, no auto \
                   white balance",
    },
    // v4 — the default *render* changed: `density.scale` `[1, 1, 1]` -> `[1, 0.90, 0.86]`
    // (2026-09-09). `base` is unchanged; the per-channel gain is applied in
    // `algo::density::to_density`, downstream of film-base estimation.
    PipelineFingerprint {
        pipeline_version: 4,
        render: "323499bad6c71237",
        base: "01c5acccc36a3388",
        recipe: "72e424ee6a15d53b",
        // Frozen literal, not `PIPELINE_BEHAVIOR`: the v5 bump took the constant over
        // (see v1's, v2's and v3's rows for the same handover).
        behavior: "gain-map-hdr default output (dual-dialect gain-map JPEG), roll-fixed \
                   nominal Dmax 1.3 density, mid-grey-anchored sigmoid curve, calibrated \
                   per-channel density gain, no auto white balance",
    },
    // v5 — the default *render* changed again: `density.scale` `[1, 0.90, 0.86]` ->
    // `[1, 0.84, 0.73]` (2026-09-16), calibrated from neutral patches rather than from the
    // tone-scale slope. `base` is unchanged for the same reason as v4: the per-channel gain
    // is applied in `algo::density::to_density`, downstream of film-base estimation.
    PipelineFingerprint {
        pipeline_version: 5,
        render: "9c97b6954612c356",
        base: "01c5acccc36a3388",
        // Refreshed in place when the `measure` recipe section arrived
        // (`film-base/holder-depth-mask`): the default document gained
        // `"measure": {"inset": 0.05}`, so the recipe hash moved while `render` and
        // `base` are byte-identical. The default anchor is `Fixed`, so no default
        // render moved — the "new value in the default document, no default pixel
        // change" case this field sanctions editing for. **v4's row is deliberately
        // untouched**: v4's default document never carried `measure`, so editing it
        // would make a historical row describe a document that version never had.
        //
        // **Not bumping `PIPELINE_VERSION` is a recorded decision, not a
        // consequence of the above.** The v1/v3 precedents for an in-place refresh
        // were *new* knobs, where no pre-existing recipe could change meaning.
        // Here a frozen recipe carrying `reconstruction.curve.dmax: "auto"` renders
        // differently before and after this change under the same
        // `pipeline_version` and the same `behavior` string — and on a path the
        // fingerprints cannot witness at all, since `golden`'s vectors resolve
        // `Fixed`. That is a version-*identity* question, not merely the
        // verification gap it looks like. It is accepted because nc is unshipped,
        // `Auto` is opt-in via `--auto-d-max`, and the prior behaviour rendered
        // every real frame black (it measured the opaque holder). If it is
        // revisited, `core/conversion-versioning` is its home.
        //
        // So the `--auto-d-max` change that *did* happen is verified by
        // same-machine before/after, not here.
        //
        // **Refreshed in place a second time** by `core/calibration-recipe-section`,
        // which moved `film_base.source` and `reconstruction.curve.dmax` into one
        // top-level `calibration` section. The default document changed *shape* while
        // every default **value** stayed put (`film_base` still `null`, the reference
        // still `"fixed"`), so this hash moved and `render`/`base` did not — the gate
        // asserts those two first, and they held. Sanctioned in the task file, which
        // named this case in advance. It is not the kind of change the paragraph above
        // worries about: a recipe written against the old shape does not render
        // differently, it is *rejected* with a migration error naming the new path.
        //
        // **Refreshed in place a third time** by `nf-retire/legacy-custom`, which removed
        // the `output.depth` / `output.output_profile` / `output.bigtiff` selectors with
        // the only presets that read them. Every remaining preset resolves those itself,
        // so the default document lost three keys and no default value moved; `render`
        // and `base` held. The same shape as the second refresh: a recipe still naming a
        // removed key is rejected with a migration error, not rendered differently.
        recipe: "9ca8dcca192e605a",
        // Frozen literal, not `PIPELINE_BEHAVIOR`: the v6 bump took the constant over
        // (see v1's to v4's rows for the same handover).
        behavior: "gain-map-hdr default output (dual-dialect gain-map JPEG), roll-fixed \
                   nominal Dmax 1.3 density, mid-grey-anchored sigmoid curve, \
                   neutral-patch-calibrated per-channel density gain, no auto white balance",
    },
    // v6 — the default *render* changed: the sigmoid retired and the default curve became
    // the exponential at the fixed decode's configuration (2026-09-23). `base` is
    // unchanged: film-base estimation is upstream of the curve.
    PipelineFingerprint {
        pipeline_version: 6,
        render: "752e701021a41307",
        base: "01c5acccc36a3388",
        recipe: "dbac245a916032f2",
        // Frozen literal, not `PIPELINE_BEHAVIOR`: the v7 bump took the constant over.
        behavior: "gain-map-hdr default output (dual-dialect gain-map JPEG), exponential \
                   density curve at the fixed decode's configuration (mid-grey 0.62 density \
                   above the film base, contrast 2.0), neutral-patch-calibrated per-channel \
                   density gain, no auto white balance",
    },
    // v7 — the default *display tone* changed: `shoulder` retired and the default became
    // extended Reinhard at 6 stops (2026-09-24). `render` and `base` are unchanged — the
    // gate stops at `algo::reconstruct`, before any display stage — so only `recipe`
    // moved, and this row's evidence is the progress log, not the gate.
    PipelineFingerprint {
        pipeline_version: 7,
        render: "752e701021a41307",
        base: "01c5acccc36a3388",
        // Refreshed in place by `nf-retire/dmax-machinery`, which removed
        // `calibration.dmax` from the default document. The default placement never read
        // it, so `render` and `base` held — the gate asserts them first — and a recipe
        // still stating the old default `"fixed"` is dropped on load and replays. Refreshed
        // again by `nf-retire/regional-balance`, which removed the three regional-balance
        // keys from `reconstruction.density`: their neutral default skipped the pass
        // bit-exactly, so `render` and `base` held, and the neutral keys are dropped on load.
        // Refreshed a third time by `nf-retire/characteristic`, which collapsed the curve to
        // its one variant and stopped writing `reconstruction.curve.type`: the default
        // curve was already the exponential, so `render` and `base` held, and the old
        // `"type": "exponential"` is dropped on load.
        recipe: "53af9f2172093cac",
        behavior: PIPELINE_BEHAVIOR,
    },
];

/// One recorded row of [`PIPELINE_FINGERPRINTS`]: a `pipeline_version`, the
/// fingerprints of the default conversion behavior it labels, and the
/// [`PIPELINE_BEHAVIOR`] string that describes it.
///
/// **Why three fingerprints.** They answer different questions and fail for
/// different reasons:
///
/// - `render` — [`stable_hash`] over the default reconstruction of the curated
///   per-pixel vectors in `pipeline::stages::golden` (`Reconstruction::default()`
///   over `golden::pixels()` / `golden::base()`): every output pixel's `f32` bit
///   pattern, and three lines kept only for byte-identity with the recorded rows:
///   `white_balance` (it echoes the default's explicit gains), `dmax` (a frozen literal
///   since the reference density retired) and `balance_range` (a frozen literal since
///   the regional balance retired). This is the *arithmetic* of stage 3 and
///   nothing after it — no print control is covered. (Until `nf-retire/legacy-custom` it hashed
///   `reconstruct_and_print`, whose print half was a bit-exact identity under
///   default settings — so the text, and every recorded row, is unchanged.)
/// - `base` — [`stable_hash`] over `film_base::estimate`'s result (the resolved
///   base's `f32` bit patterns plus its warnings) for [`FilmBaseSource::Auto`]
///   over the frozen synthetic scan in `pipeline::film_base::golden`. This is
///   **stage 2**, which `render` structurally cannot see: `render` is handed a
///   hardcoded base, while `auto` estimates one from pixels on every run that
///   selects it. Retuning the rebate detector or its
///   percentile changes every default conversion and nothing else here would move.
/// - `recipe` — [`stable_hash`] over the canonical JSON of
///   `cli::ResolvedConfig::default()`. This is the default *configuration*: it
///   covers default **values** the other two cannot see (`output.preset`,
///   `calibration.film_base`, the `input` defaults). It is also the *only* fingerprint that moves when the default
///   **preset** changes: `render` and `base` measure the reconstruction and
///   `film_base::estimate`, which the preset does not select — the v3 row is exactly
///   that case, and its evidence is `docs/reports/render-defaults-v3.md`. Note it
///   covers the *values*, never the code implementing them — `calibration.film_base`
///   appears in it only as `null` (it has no default and must be chosen), which
///   is why `base` exists.
///
/// **What the gate does NOT cover.** Being explicit matters more than sounding
/// comprehensive; a claim of whole-pipeline coverage would be worse than no claim,
/// because it stops people looking:
///
/// - stage 1 `io::decode` (container parsing, sample scaling, IR-plane handling);
/// - stage 1b `pipeline::input_semantics::resolve` (transfer/meaning resolution);
/// - the lcms2 **output color transform** and the embedded ICC bytes — excluded
///   *deliberately*, since both differ by target and no cross-platform hash of them
///   is possible (design-spec §8);
/// - `io::encode` (u16 quantization, clip accounting, BigTIFF promotion);
/// - the `Region` / `Explicit` film-base sources and `estimate_grid`, none of which
///   are the default;
/// - the auto detector's behavior on **real** scans — `base` pins it on one frozen
///   synthetic layout, which catches a retuned constant but not a regression that
///   only shows up on real rebate geometry;
/// - the IR holder mask (`film_base::ir_separability` / `ir_holder_mask`): `base`'s
///   frozen scan carries no IR plane, so a change there moves the film base of every
///   HDRi auto-base run and none of the three hashes.
///
/// A change confined to those areas can move default output with every test green.
/// The `scripts/real-scan-verify/` harness and `nctool compare` are the tools for
/// that; this gate is the automatic part, not the whole answer.
///
/// A deliberate trade-off on `recipe`: adding a **new opt-in knob** with a neutral
/// default changes the default recipe JSON and therefore trips this gate, even
/// though no default pixel moved. That is a false positive, and the correct
/// response is to update the `recipe` fingerprint **without** bumping
/// [`PIPELINE_VERSION`] (and say so in the progress log). The alternative — an
/// allowlist of "behavior-bearing" keys — silently stops covering whatever key
/// nobody remembered to add, which is the failure mode this gate exists to prevent.
/// A gate that makes you look is worth more than one that quietly stops looking.
///
/// **Why hashing these particular values is safe on both macOS/aarch64 and x86_64
/// Linux** (CLAUDE.md's cross-platform determinism rule, design-spec §8):
///
/// - `render` hashes **exactly** the per-pixel values a `stages::golden` test
///   already pins as literal bit patterns, so hashing adds a version label without
///   widening the numeric surface by one value. Which test depends on the row: the
///   **v6** render is the exponential at the fixed decode's configuration, pinned by
///   `golden_new_default_is_bit_identical`; the **v1** render is the exponential
///   straight line at gamma 1.0 / anchor 2.0, still pinned by
///   `golden_density_exponential_reference_is_bit_identical`. The v2–v5 renders were
///   the retired sigmoid; their rows are history, and the golden that pinned them
///   left with the curve.
///
///   **The two claims are not equally strong, and saying so matters more than
///   sounding confident.** The v1 vectors have been green on macOS/aarch64 *and* CI's
///   x86_64 Linux runner for as long as this gate has existed — observed agreement,
///   not an argument. The v6 vector was captured on this host on 2026-09-23; it
///   evaluates the same one `log10` and one `10^` per sample as v1, but at a different
///   slope and anchor, so its cross-target agreement is a **prediction that CI's Linux
///   runner validates**. If that runner ever reds on
///   `golden_new_default_is_bit_identical`, the failure is the vector's — pick sample
///   values that do agree, per CLAUDE.md's rule — not the gate's, and not a real
///   behavior change. **And that is not a remote possibility:**
///   `algo/characteristic-curve-coverage` established that x86_64 and macOS return
///   different `f32` results from `log10f` on two samples of this very vector under
///   the (since retired) characteristic curve.
///   Settle it by enumerating what a conforming libm can return (the retired
///   characteristic golden's `reachable_window`, in git history, is the worked
///   example) — but note a fingerprint has no tolerance window to absorb the answer
///   the way a golden does.
/// - It stops at the reconstruction, i.e. **before** the lcms2 output color
///   transform. No post-lcms2 pixel and no embedded ICC byte — both of which differ
///   by target — enters any of the hashes.
/// - `base` hashes the output of a code path with **no transcendental at all** (no
///   `powf` / `10^` / `log10` / `exp` / `sqrt` anywhere in `film_base`): integer
///   indexing, IEEE `+ - * /`, comparisons, and a nearest-rank order statistic
///   whose value is independent of tie order. The ~1-ULP libm divergence that rules
///   out a whole-frame reconstruct hash has nothing to act on here. See
///   `film_base::golden` for the full argument.
/// - None of the three is a whole-frame or whole-file checksum. There is no encoded
///   TIFF, no full-frame reconstruct output, and no accumulation over many pixels
///   where a 1-ULP libm difference could land. (`base` *does* reduce 30 000 pixels
///   to a percentile — but by **selection**, not summation: the result is one of the
///   input values, bit-for-bit.)
/// - `recipe` hashes serde-generated **text**, and [`stable_hash`] is pure integer
///   arithmetic over bytes. Neither has a floating-point or platform dependency.
#[cfg(test)]
pub struct PipelineFingerprint {
    pub pipeline_version: u32,
    pub render: &'static str,
    pub base: &'static str,
    pub recipe: &'static str,
    /// The [`PIPELINE_BEHAVIOR`] string for this version. Recorded here so the
    /// human-readable description is **paired to the version by a gate**: without
    /// it, you could bump the version, record fingerprints, forget to rewrite
    /// [`PIPELINE_BEHAVIOR`], and ship a `nc --version` that describes the previous
    /// render with every test green.
    pub behavior: &'static str,
}

/// One-line description of what [`PIPELINE_VERSION`]'s default render does,
/// printed by `nc --version` so an operator can tell two builds apart without
/// looking up the number.
///
/// Kept a plain `const` rather than a lookup into `PIPELINE_FINGERPRINTS` because
/// that table is `cfg(test)`: making it the runtime source would put the
/// fingerprint hashes into the shipped binary, where nothing reads them (a
/// `dead_code` allow with no consumer, which CLAUDE.md forbids). The pairing is
/// enforced instead by two assertions in `mod drift_gate` — the current version's
/// row must carry *this* string, and no two rows may share a behavior — which fails
/// for both ways of forgetting to update it.
///
/// **Rewritten at each bump, not amended.** This text describes whatever
/// [`PIPELINE_VERSION`] currently is, so a version bump replaces it outright: the
/// v2 string below was written fresh for the 2026-08-08 default render, and v1's
/// final text moved into its row in `PIPELINE_FINGERPRINTS` as a frozen literal.
/// That is the normal path.
///
/// The *abnormal* path is amending a string while its version stays put, and it has
/// happened exactly once — v1's text used to open with "auto rebate film base", and
/// that clause was dropped when `film_base.source` lost its default. It was allowed
/// only because the v1 row's `render` and `base` fingerprints did not move (the
/// render v1 labels was unchanged, and the removed clause described a resolution
/// step that is no longer part of any *default* render, there being no default).
/// The v1 row records the outcome; read it before amending anything here.
pub const PIPELINE_BEHAVIOR: &str = "gain-map-hdr default output (dual-dialect gain-map \
     JPEG), exponential density curve at the fixed decode's configuration (mid-grey 0.62 \
     density above the film base, contrast 2.0), neutral-patch-calibrated per-channel \
     density gain, no auto white balance, extended-Reinhard display tone at 6 stops of \
     headroom";

/// The short git commit hash, or `None` when the build could not determine it
/// (source tarball / no `git` / not this package's repository). `None` is reported
/// as an **absent** field rather than the string `"unknown"`, so a consumer never
/// mistakes a placeholder for a hash.
pub fn git_commit() -> Option<&'static str> {
    (GIT_COMMIT_RAW != "unknown").then_some(GIT_COMMIT_RAW)
}

/// Whether the build tree had uncommitted changes; `None` when unknown (see
/// [`git_commit`]). A `true` here means the commit hash alone does **not** identify
/// the source — treat such an output as unattributable.
///
/// The relationship to [`git_commit`] is **one-directional**: `build.rs` reports
/// cleanliness `unknown` whenever the commit is unknown, but a readable `HEAD` with
/// an unreadable index legitimately yields `Some(commit)` with `None` here.
pub fn git_dirty() -> Option<bool> {
    match GIT_DIRTY_RAW {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The identity block stamped into every JSON report and into the sidecar's `meta`
/// envelope. Serialize-only: nothing deserializes it back into a run (a sidecar's
/// `meta` is provenance about the run that produced it, never parameters to
/// re-apply), which is what keeps it out of the recipe schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// Crate semver ([`NC_VERSION`]).
    pub nc_version: &'static str,
    /// Short git commit hash; omitted when the build could not determine it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<&'static str>,
    /// Whether the build tree was dirty; omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    /// The behavioral [`PIPELINE_VERSION`] — the axis a version comparison is
    /// keyed on. A property of the **build's default render**, not of the run: a
    /// non-default configuration — `--new-flow` included — carries it unchanged, the
    /// way a legacy run with a non-default curve does.
    pub pipeline_version: u32,
    /// Compile target triple ([`TARGET`]).
    pub target: &'static str,
    /// Hash of the canonical resolved-recipe JSON, when the command resolved a
    /// full recipe (`convert`, `roll`). Omitted for `inspect` / `estimate`, which
    /// run no conversion and therefore have no effective recipe to identify —
    /// [`Identity::new`] is the constructor for exactly that state — and under
    /// `--new-flow`, where what a run's identity hashes is still
    /// `nf-core/report-contract`'s to decide, although the run has its own recipe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params_hash: Option<String>,
}

impl Identity {
    /// Build identity for a run with no effective conversion recipe to hash:
    /// `inspect` / `estimate`, and a `--new-flow` conversion (see `params_hash`).
    pub fn new() -> Self {
        Self {
            nc_version: NC_VERSION,
            git_commit: git_commit(),
            git_dirty: git_dirty(),
            pipeline_version: PIPELINE_VERSION,
            target: TARGET,
            params_hash: None,
        }
    }

    /// Full identity for a conversion: build identity plus the hash of the
    /// canonical resolved-recipe JSON (see [`stable_hash`]).
    pub fn with_params_hash(hash: String) -> Self {
        Self {
            params_hash: Some(hash),
            ..Self::new()
        }
    }
}

/// `nc --version` text: semver plus everything needed to attribute an output —
/// the behavioral pipeline version (with its one-line description), the commit
/// (marked `-dirty` when the tree wasn't clean, or `(dirty unknown)` when
/// cleanliness could not be read), and the target triple.
///
/// Interned in a `OnceLock` because clap wants a `&'static str` and the string is
/// assembled from runtime-formatted parts.
pub fn version_string() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let commit = match (git_commit(), git_dirty()) {
            (Some(c), Some(true)) => format!("{c}-dirty"),
            (Some(c), Some(false)) => c.to_string(),
            // A known commit with unknown cleanliness (`build.rs` read `HEAD` but
            // not the index). Printing a bare hash here would be indistinguishable
            // from a clean tree, which is the one thing this field must not imply.
            (Some(c), None) => format!("{c} (dirty unknown)"),
            (None, _) => "unknown".to_string(),
        };
        format!(
            "{NC_VERSION}\npipeline_version: {PIPELINE_VERSION} ({PIPELINE_BEHAVIOR})\n\
             commit: {commit}\ntarget: {TARGET}"
        )
    })
}

/// Stable 64-bit FNV-1a hash of `text`, hex-formatted (16 lowercase digits).
///
/// Hand-rolled rather than `std::hash::DefaultHasher`, whose output is explicitly
/// **not** guaranteed stable across toolchains — an identity hash that changes
/// when the compiler changes would be worthless for cross-version comparison. Every
/// consumer depends on that stability *and* on it being platform-independent, which
/// this is: pure integer arithmetic over bytes, identical on every target.
///
/// **This is the crate's only params-hash implementation.** It originated in
/// `telemetry` and was moved here — a neutral home both consumers can reach —
/// rather than copied: the report is core output and must not depend on the
/// opt-in telemetry module, and two hashes of the same recipe that could disagree
/// would defeat the point of publishing one. [`crate::telemetry::params_hash`] is
/// now a delegation to this function, so a telemetry record's `params_hash` and a
/// report's `identity.params_hash` are the same function over the same bytes.
/// The consumers are:
///
/// - `identity.params_hash` in the report and the sidecar's `meta` (via
///   `cli::canonical_params_json`) and the telemetry record's `conversion.params_hash`;
/// - the `pipeline_version` drift-gate fingerprints (`PIPELINE_FINGERPRINTS`).
pub fn stable_hash(text: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in text.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

/// The `pipeline_version` drift gate: proves the behavioral label in
/// [`PIPELINE_VERSION`] still describes what the code actually does.
///
/// Read [`PipelineFingerprint`] first — it documents what each fingerprint covers,
/// **what it does not**, and why hashing these particular values is safe on both
/// macOS/aarch64 and x86_64 Linux while a whole-file or whole-frame checksum would
/// not be.
#[cfg(test)]
mod drift_gate {
    use super::*;
    use crate::cli::ResolvedConfig;
    use crate::pipeline::film_base;
    use crate::pipeline::stages::golden;
    use crate::pipeline::white_balance::resolve_print_gains;
    use crate::types::{
        DensityParams, ExponentialParams, FilmBaseSource, PrintParams, Reconstruction, WbSource,
    };

    /// Format an `f32` as its raw bit pattern in hex — no decimal formatting, so
    /// nothing is rounded on its way into a fingerprint.
    fn hex(v: f32) -> String {
        format!("{:08x}", v.to_bits())
    }

    /// The `dmax=` value every recorded row hashed: `hex(1.3)`, the retired nominal
    /// reference. See [`render_fingerprint_text`].
    const FROZEN_DMAX_LINE: &str = "3fa66666";

    /// The `balance_range=` value every recorded row hashed: `-`, the neutral default's
    /// "no range consulted". See [`render_fingerprint_text`].
    const FROZEN_BALANCE_RANGE_LINE: &str = "-";

    /// A render's fingerprint input, as canonical text.
    ///
    /// **Parameterized on purpose.** The gate hashes it with the real defaults, and
    /// the "this gate can actually fail" test hashes it with a *perturbed* config —
    /// the same formatter both times, so the comparison is like-shaped text against
    /// the recorded row rather than two differently-shaped strings that could never
    /// match whatever the inputs were.
    ///
    /// Kept human-readable (rather than hashing raw bytes) so a gate failure can
    /// print it and a developer can *see* which pixel or diagnostic moved instead of
    /// only that a hash differs.
    fn render_fingerprint_text(recon: &Reconstruction, white_balance: WbSource) -> String {
        let (out, _) = golden::reconstructed(recon);
        // The `white_balance` line **covers no arithmetic**: the default is explicit
        // gains, so this only echoes a recipe value the `recipe` fingerprint already
        // hashes. It is kept for one reason — the text must stay byte-identical to what
        // the recorded rows hashed, when the retired legacy print stage reported the
        // same resolution. Nothing in this gate exercises the shared print controls'
        // arithmetic; where the new `render` hash stops is
        // `nf-verification/fingerprints`' call.
        let white_balance = Some(
            resolve_print_gains(&out.rgb, white_balance)
                .expect("the default white balance must resolve"),
        );
        let rgb: Vec<String> = out.rgb.iter().copied().map(hex).collect();
        let triple = |v: Option<[f32; 3]>| {
            v.map_or_else(
                || "-".to_string(),
                |a| a.iter().copied().map(hex).collect::<Vec<_>>().join(","),
            )
        };
        // The `dmax` and `balance_range` lines are **frozen literals**: the reference
        // density `dmax` echoed (the nominal 1.3, which the base-derived default never
        // read) retired in `nf-retire/dmax-machinery`, and the regional balance whose
        // range `balance_range` echoed (none, at the neutral default) in
        // `nf-retire/regional-balance`. The text must stay byte-identical to what the
        // recorded rows hashed. Neither covers arithmetic, like `white_balance`.
        format!(
            "rgb={}\ndmax={}\nwhite_balance={}\nbalance_range={}\n",
            rgb.join(","),
            FROZEN_DMAX_LINE,
            triple(white_balance),
            FROZEN_BALANCE_RANGE_LINE
        )
    }

    /// The **stage 2** fingerprint input: what `film_base::estimate` resolves for
    /// [`FilmBaseSource::Auto`] over the frozen synthetic scan, plus any warnings
    /// it raised. Parameterized for the same reason as the render text.
    ///
    /// It pins `Auto` **explicitly** rather than "whatever the default is",
    /// because this fingerprint exists to catch drift in the *detector*. Since
    /// `calibration.film_base` lost its default, tying the gate to the default would
    /// have meant the estimator's fingerprint moving for a reason that has
    /// nothing to do with the estimator. (`Auto` was that default, so the
    /// recorded hash is unchanged by the switch.)
    fn base_fingerprint_text(source: &FilmBaseSource) -> String {
        let est = film_base::estimate(&film_base::golden::scan(), source)
            .expect("the `auto` film-base estimate must succeed on the frozen scan");
        let rgb: Vec<String> = <[f32; 3]>::from(est.base)
            .iter()
            .copied()
            .map(hex)
            .collect();
        format!(
            "base={}\nwarnings={}\n",
            rgb.join(","),
            est.warnings.join("|")
        )
    }

    /// The default *configuration*'s fingerprint input: the canonical resolved-recipe
    /// document — exactly the bytes `--dump-params` writes for an untouched default
    /// run (`hanten params` prints the same text with a trailing newline).
    fn recipe_fingerprint_text() -> String {
        serde_json::to_string_pretty(&ResolvedConfig::default())
            .expect("the default recipe must serialize")
    }

    /// The recorded row for [`PIPELINE_VERSION`], or a panic naming exactly what to
    /// add. Shared by the gate and the tests that reason about the table.
    fn recorded_row() -> &'static PipelineFingerprint {
        let render = stable_hash(&render_fingerprint_text(
            &Reconstruction::default(),
            PrintParams::default().white_balance,
        ));
        let base = stable_hash(&base_fingerprint_text(&FilmBaseSource::Auto));
        let recipe = stable_hash(&recipe_fingerprint_text());
        PIPELINE_FINGERPRINTS
            .iter()
            .find(|r| r.pipeline_version == PIPELINE_VERSION)
            .unwrap_or_else(|| {
                panic!(
                    "PIPELINE_VERSION {PIPELINE_VERSION} has no recorded fingerprint. ADD a row \
                     to `PIPELINE_FINGERPRINTS` (never edit an existing one):\n\n    \
                     PipelineFingerprint {{ pipeline_version: {PIPELINE_VERSION}, render: \
                     \"{render}\", base: \"{base}\", recipe: \"{recipe}\", behavior: \
                     PIPELINE_BEHAVIOR }},\n\n\
                     and give the new version a line in the PIPELINE_VERSION history table plus a \
                     fresh PIPELINE_BEHAVIOR string. A bump with no recorded fingerprint would \
                     leave the new version's default behavior undefended against the next \
                     silent change."
                )
            })
    }

    #[test]
    fn default_conversion_behavior_matches_the_recorded_pipeline_version() {
        let row = recorded_row();

        let render = stable_hash(&render_fingerprint_text(
            &Reconstruction::default(),
            PrintParams::default().white_balance,
        ));
        assert_eq!(
            render,
            row.render,
            "the DEFAULT RENDER changed but PIPELINE_VERSION is still {PIPELINE_VERSION}.\n\n\
             Default pixels are the behavioral contract, so this is a `pipeline_version` bump: \
             raise PIPELINE_VERSION, update PIPELINE_BEHAVIOR, add a history-table row, and \
             ADD a new PIPELINE_FINGERPRINTS row with render: \"{render}\".\n\n\
             NEVER edit an existing row's `render` in place. That row is the recorded history of \
             a shipped version; overwriting it makes one `pipeline_version` label two different \
             behaviors, and every output already stamped with it becomes unattributable.\n\n\
             If instead you believe the default render is unchanged, the bit patterns say \
             otherwise — the sibling test \
             `pipeline::stages::golden::golden_new_default_is_bit_identical` \
             names the pixel that moved.\n\nfingerprint input was:\n{}",
            render_fingerprint_text(
                &Reconstruction::default(),
                PrintParams::default().white_balance
            )
        );

        let base = stable_hash(&base_fingerprint_text(&FilmBaseSource::Auto));
        assert_eq!(
            base,
            row.base,
            "the DEFAULT FILM-BASE ESTIMATE changed but PIPELINE_VERSION is still \
             {PIPELINE_VERSION}.\n\n\
             `calibration.film_base` has NO default, so this stage runs only on runs that asked for \
             `auto` — but for those it resolves the divisor of the density conversion, and a \
             detector change moves every one of their outputs: raise PIPELINE_VERSION, update \
             PIPELINE_BEHAVIOR, add a history-table row, and ADD a new row with base: \
             \"{base}\" (never edit an existing row's `base`).\n\n\
             If you changed `film_base::golden::scan` instead of the detector, revert it — that \
             fixture is frozen precisely so this hash means \"the algorithm moved\".\n\n\
             fingerprint input was:\n{}",
            base_fingerprint_text(&FilmBaseSource::Auto)
        );

        let recipe = stable_hash(&recipe_fingerprint_text());
        assert_eq!(
            recipe,
            row.recipe,
            "the DEFAULT RECIPE changed but PIPELINE_VERSION is still {PIPELINE_VERSION}.\n\n\
             If a *default value* changed (the output preset, film-base source, an input \
             default), default output changed with it — bump PIPELINE_VERSION and ADD a new \
             row.\n\n\
             If you only ADDED an opt-in knob whose default is neutral, no default pixel \
             moved: update this row's `recipe` to \"{recipe}\" WITHOUT bumping \
             PIPELINE_VERSION, and note in `docs/progress/core.md` why the default render is \
             unaffected. `recipe` is the ONE field this table sanctions editing in place; \
             `render` and `base` are history.\n\nfingerprint input was:\n{}",
            recipe_fingerprint_text()
        );

        assert_eq!(
            row.behavior, PIPELINE_BEHAVIOR,
            "PIPELINE_VERSION {PIPELINE_VERSION}'s recorded row describes a different default \
             render than PIPELINE_BEHAVIOR does. `hanten --version` prints PIPELINE_BEHAVIOR, so \
             leaving these out of step ships a build that describes the wrong behavior. Set the \
             row's `behavior` to PIPELINE_BEHAVIOR and make sure PIPELINE_BEHAVIOR itself \
             describes THIS version."
        );
    }

    #[test]
    fn the_fingerprint_gate_actually_detects_a_changed_default() {
        // The gate is only worth having if a perturbed default really does move a
        // fingerprint. Prove it against the RECORDED ROW (not a re-computation), and
        // with the same formatter the gate uses — otherwise the assertion could pass
        // on a shape difference for any input at all, proving nothing.
        let row = recorded_row();
        let default_recon = Reconstruction::default();

        let default_wb = PrintParams::default().white_balance;

        // (a) the white-balance line is wired to the default and not to a constant.
        // This is **not** print-control coverage — the line only echoes explicit gains
        // (see `render_fingerprint_text`) — it guards against the line going stale.
        let WbSource::Explicit([r, g, b]) = default_wb else {
            panic!("the default white balance is explicit gains")
        };
        let perturbed_wb = WbSource::Explicit([r, g, b + f32::EPSILON]);
        assert_ne!(
            stable_hash(&render_fingerprint_text(&default_recon, perturbed_wb)),
            row.render,
            "the white_balance line must follow the default it echoes"
        );

        // (b) the reconstruction side — the density curve, the part `render` mostly
        // exists to pin.
        let perturbed_recon = Reconstruction {
            density: DensityParams::default(),
            curve: ExponentialParams {
                gamma: 1.05,
                ..ExponentialParams::default()
            },
        };
        assert_ne!(
            stable_hash(&render_fingerprint_text(&perturbed_recon, default_wb)),
            row.render,
            "a perturbed density curve must move the render fingerprint"
        );

        // (c) the film-base side: a different source resolves a different base, so
        // the stage-2 fingerprint must move too.
        let perturbed_base = FilmBaseSource::Explicit([0.9, 0.55, 0.42]);
        assert_ne!(
            stable_hash(&base_fingerprint_text(&perturbed_base)),
            row.base,
            "a different film-base source must move the base fingerprint"
        );

        // And the unperturbed defaults DO match the row — so the assertions above
        // failed for the perturbation, not because the formatter never matches.
        assert_eq!(
            stable_hash(&render_fingerprint_text(&default_recon, default_wb)),
            row.render
        );
        assert_eq!(
            stable_hash(&base_fingerprint_text(&FilmBaseSource::Auto)),
            row.base
        );
    }

    #[test]
    fn the_table_records_exactly_the_shipped_versions() {
        // Every version this build claims to have shipped needs a row (so it is
        // defended), and no row may claim a version that does not exist yet (which
        // would silently pre-approve a future default). Version 0 predates the
        // constant and is deliberately unrecorded — see the history table.
        let versions: Vec<u32> = PIPELINE_FINGERPRINTS
            .iter()
            .map(|r| r.pipeline_version)
            .collect();
        for v in 1..=PIPELINE_VERSION {
            assert!(
                versions.contains(&v),
                "pipeline_version {v} has no PIPELINE_FINGERPRINTS row; versions recorded: \
                 {versions:?}"
            );
        }
        assert!(
            versions.iter().all(|v| *v >= 1 && *v <= PIPELINE_VERSION),
            "PIPELINE_FINGERPRINTS records a version outside 1..={PIPELINE_VERSION}: {versions:?}"
        );

        // Two rows claiming the same version would make the lookup order-dependent
        // and silently defend only one of them.
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            versions.len(),
            "duplicate pipeline_version in the table: {versions:?}"
        );

        // Two versions describing themselves identically means one of them was
        // recorded without refreshing PIPELINE_BEHAVIOR — the exact slip the
        // `behavior` field exists to catch.
        let mut behaviors: Vec<&str> = PIPELINE_FINGERPRINTS.iter().map(|r| r.behavior).collect();
        let n = behaviors.len();
        behaviors.sort_unstable();
        behaviors.dedup();
        assert_eq!(
            behaviors.len(),
            n,
            "two pipeline_versions share a PIPELINE_BEHAVIOR description: {behaviors:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_serializes_the_wire_shape_and_omits_unknown_git_facts() {
        // The wire shape is the contract (reports, sidecar `meta`, telemetry), so
        // assert on the serialized JSON rather than on the fields we just set.
        let json = serde_json::to_value(Identity::new()).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["nc_version"], NC_VERSION);
        assert_eq!(obj["pipeline_version"], PIPELINE_VERSION);
        assert_eq!(obj["target"], TARGET);
        // `inspect`/`estimate` identity carries no recipe hash, and absence is an
        // OMITTED key — never a `null` a consumer could read as a value.
        assert!(!obj.contains_key("params_hash"), "{json}");

        // A build with no usable git omits both git facts entirely: the object is
        // exactly the three always-present keys.
        let none_git = Identity {
            git_commit: None,
            git_dirty: None,
            ..Identity::new()
        };
        let none_git_json = serde_json::to_value(&none_git).unwrap();
        let keys: Vec<&str> = none_git_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["nc_version", "pipeline_version", "target"],
            "{keys:?}"
        );
    }

    #[test]
    fn a_known_commit_may_have_unknown_cleanliness_but_not_the_reverse() {
        // `build.rs` reports cleanliness `unknown` whenever the commit is unknown,
        // but deliberately allows a known commit with an unreadable index. The
        // invariant is therefore ONE-directional; asserting the biconditional would
        // fail CI on a machine building exactly as designed.
        let id = Identity::new();
        assert!(
            id.git_dirty.is_none() || id.git_commit.is_some(),
            "a dirty flag without a commit would be meaningless: {id:?}"
        );
    }

    #[test]
    fn params_hash_rides_in_the_identity_block() {
        let json = serde_json::to_value(Identity::with_params_hash(stable_hash("recipe"))).unwrap();
        assert_eq!(json["params_hash"], stable_hash("recipe"));
    }

    #[test]
    fn stable_hash_is_pinned_deterministic_and_input_sensitive() {
        // Pinned vectors: this hash is a wire value (report / sidecar / telemetry
        // record), so its output must never drift with a refactor.
        assert_eq!(stable_hash(""), "cbf29ce484222325");
        assert_eq!(stable_hash("a"), "af63dc4c8601ec8c");
        assert_ne!(stable_hash("{}"), stable_hash("{} "));
        assert_eq!(stable_hash("x").len(), 16);
    }

    #[test]
    fn version_string_names_every_identity_axis() {
        let v = version_string();
        assert!(v.contains(NC_VERSION), "{v}");
        assert!(
            v.contains(&format!("pipeline_version: {PIPELINE_VERSION}")),
            "{v}"
        );
        assert!(v.contains(PIPELINE_BEHAVIOR), "{v}");
        assert!(v.contains("commit:"), "{v}");
        assert!(v.contains(TARGET), "{v}");
        // A dirty tree must be marked: a bare hash next to uncommitted changes is
        // the one thing this line must never imply.
        if git_dirty() == Some(true) {
            assert!(v.contains("-dirty"), "{v}");
        }
    }
}
