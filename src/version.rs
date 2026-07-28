//! Conversion identity: what produced an output.
//!
//! Three independent layers, all stamped into the JSON **report** (and mirrored
//! into the sidecar's `meta` envelope — never as recipe keys; see
//! `io::encode::write_sidecar` and `cli::load_recipe`):
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
/// the `Dmax` policy, the film-base source and its detector, auto white balance,
/// the working-space mapping) — never for a refactor that preserves default
/// pixels, and never for a new opt-in knob.
///
/// History (the label a comparison is keyed on):
///
/// | version | default render |
/// |---|---|
/// | 0 | the Step-1 MVP baseline recorded in `docs/reports/v0-baseline.md`: per-frame `auto` `Dmax` (99.5th-percentile density), exponential curve, no auto WB |
/// | 1 | **current** — every default change since that baseline, collapsed into one label: `film-base/dmax-reference` replaced the per-frame anchor with the roll-fixed nominal `Dmax = 2.0` **density**, `film-base/auto-base-redesign` replaced the auto film-base detector with the inward-scan rebate detector, and `core/input-semantics` added the stage-1b transfer/meaning resolution. The tagged-`reconstruction` split was proven bit-identical and is *not* part of the change. |
///
/// **The v1 row is a collapse, not a single step.** `docs/reports/v0-baseline.md`
/// measured its numbers with an **explicit** `--film-base`, so those numbers stay
/// comparable across the film-base redesign; the *default* render, however, crossed
/// three boundaries between v0 and v1, and only one label was available to record
/// them. That is a known limitation of retrofitting the constant, not a claim that
/// nothing else moved.
///
/// Version 0 predates this constant, so no fingerprint of it can be computed from
/// the current tree; `docs/reports/v0-baseline.md` is its record. From 1 onward the
/// pairing is machine-enforced by the drift gate below (`PIPELINE_FINGERPRINTS` +
/// `mod drift_gate`): change a default *within the fingerprinted stages* and that
/// test fails until the fingerprints **and** this constant are updated together.
/// Read `PipelineFingerprint` for exactly which stages those are — the gate is not
/// whole-pipeline coverage and must not be described as if it were.
pub const PIPELINE_VERSION: u32 = 1;

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
pub const PIPELINE_FINGERPRINTS: &[PipelineFingerprint] = &[PipelineFingerprint {
    pipeline_version: 1,
    render: "1fce7367c4bfec58",
    base: "01c5acccc36a3388",
    recipe: "8a5b874faa30d391",
    behavior: PIPELINE_BEHAVIOR,
}];

/// One recorded row of [`PIPELINE_FINGERPRINTS`]: a `pipeline_version`, the
/// fingerprints of the default conversion behavior it labels, and the
/// [`PIPELINE_BEHAVIOR`] string that describes it.
///
/// **Why three fingerprints.** They answer different questions and fail for
/// different reasons:
///
/// - `render` — [`stable_hash`] over the default-path result of the curated
///   per-pixel vectors in `pipeline::stages::golden` (`Reconstruction::default()` +
///   `PrintParams::default()` over `golden::pixels()` / `golden::base()`): every
///   output pixel's `f32` bit pattern plus the resolved `Dmax` / white-balance /
///   balance-range diagnostics. This is the *arithmetic* of stages 3–4.
/// - `base` — [`stable_hash`] over `film_base::estimate`'s result (the resolved
///   base's `f32` bit patterns plus its warnings) for `FilmBaseParams::default()`
///   over the frozen synthetic scan in `pipeline::film_base::golden`. This is
///   **stage 2**, which `render` structurally cannot see: `render` is handed a
///   hardcoded base, while the default `film_base.source` is `auto` and estimates
///   one from pixels on every real run. Retuning the rebate detector or its
///   percentile changes every default conversion and nothing else here would move.
/// - `recipe` — [`stable_hash`] over the canonical JSON of
///   `cli::ResolvedConfig::default()`. This is the default *configuration*: it
///   covers default **values** the other two cannot see (`output.hdr`,
///   `output.output_profile`, `film_base.source`, the `input` defaults). Note it
///   covers the *values*, never the code implementing them — `film_base.source`
///   appears in it only as the string `"auto"`, which is why `base` exists.
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
///   only shows up on real rebate geometry.
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
/// - `render` hashes **exactly** the per-pixel values that
///   `golden_density_exponential_default_is_bit_identical` already pins as literal
///   bit patterns — vectors chosen because they agree across libm implementations,
///   and proven so by that test being green on both targets today. Hashing them
///   adds a version label; it does not widen the numeric surface by one value.
/// - It stops at `reconstruct_and_print`, i.e. **before** the lcms2 output color
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
pub const PIPELINE_BEHAVIOR: &str = "auto rebate film base, roll-fixed nominal Dmax 2.0 density, \
     exponential density curve, no auto white balance";

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
    /// keyed on.
    pub pipeline_version: u32,
    /// Compile target triple ([`TARGET`]).
    pub target: &'static str,
    /// Hash of the canonical resolved-recipe JSON, when the command resolved a
    /// full recipe (`convert`, `roll`). Omitted for `inspect` / `estimate`, which
    /// run no conversion and therefore have no effective recipe to identify —
    /// [`Identity::new`] is the constructor for exactly that state, and both of
    /// those commands use it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params_hash: Option<String>,
}

impl Identity {
    /// Build identity for a command with no effective conversion recipe
    /// (`inspect` / `estimate`).
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
    use crate::pipeline::stages::{golden, reconstruct_and_print};
    use crate::types::{
        DensityCurve, DensityParams, ExponentialParams, FilmBaseParams, FilmType, PrintParams,
        Reconstruction,
    };

    /// Format an `f32` as its raw bit pattern in hex — no decimal formatting, so
    /// nothing is rounded on its way into a fingerprint.
    fn hex(v: f32) -> String {
        format!("{:08x}", v.to_bits())
    }

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
    fn render_fingerprint_text(recon: &Reconstruction, print: &PrintParams) -> String {
        let (out, report) = reconstruct_and_print(&golden::pixels(), &golden::base(), recon, print)
            .expect("the render must succeed on the curated vectors");
        let rgb: Vec<String> = out.rgb.iter().copied().map(hex).collect();
        let opt = |v: Option<f32>| v.map_or_else(|| "-".to_string(), hex);
        let triple = |v: Option<[f32; 3]>| {
            v.map_or_else(
                || "-".to_string(),
                |a| a.iter().copied().map(hex).collect::<Vec<_>>().join(","),
            )
        };
        let pair = |v: Option<[f32; 2]>| {
            v.map_or_else(
                || "-".to_string(),
                |a| a.iter().copied().map(hex).collect::<Vec<_>>().join(","),
            )
        };
        format!(
            "rgb={}\ndmax={}\nwhite_balance={}\nbalance_range={}\n",
            rgb.join(","),
            opt(report.dmax),
            triple(report.white_balance),
            pair(report.balance_range)
        )
    }

    /// The default **stage 2** fingerprint input: what `film_base::estimate`
    /// resolves for `FilmBaseParams::default()` (i.e. `source = "auto"`, the default
    /// every real `nc convert` takes) over the frozen synthetic scan, plus any
    /// warnings it raised. Parameterized for the same reason as the render text.
    fn base_fingerprint_text(params: &FilmBaseParams) -> String {
        let est = film_base::estimate(&film_base::golden::scan(), params, FilmType::default())
            .expect("the default film-base estimate must succeed on the frozen scan");
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
    /// run (`nc params` prints the same text with a trailing newline).
    fn recipe_fingerprint_text() -> String {
        serde_json::to_string_pretty(&ResolvedConfig::default())
            .expect("the default recipe must serialize")
    }

    /// The recorded row for [`PIPELINE_VERSION`], or a panic naming exactly what to
    /// add. Shared by the gate and the tests that reason about the table.
    fn recorded_row() -> &'static PipelineFingerprint {
        let render = stable_hash(&render_fingerprint_text(
            &Reconstruction::default(),
            &PrintParams::default(),
        ));
        let base = stable_hash(&base_fingerprint_text(&FilmBaseParams::default()));
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
            &PrintParams::default(),
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
             `pipeline::stages::golden::golden_density_exponential_default_is_bit_identical` \
             names the pixel that moved.\n\nfingerprint input was:\n{}",
            render_fingerprint_text(&Reconstruction::default(), &PrintParams::default())
        );

        let base = stable_hash(&base_fingerprint_text(&FilmBaseParams::default()));
        assert_eq!(
            base,
            row.base,
            "the DEFAULT FILM-BASE ESTIMATE changed but PIPELINE_VERSION is still \
             {PIPELINE_VERSION}.\n\n\
             `film_base.source` defaults to `auto`, so this stage runs on every default \
             conversion and a change here moves every default output: raise PIPELINE_VERSION, \
             update PIPELINE_BEHAVIOR, add a history-table row, and ADD a new row with base: \
             \"{base}\" (never edit an existing row's `base`).\n\n\
             If you changed `film_base::golden::scan` instead of the detector, revert it — that \
             fixture is frozen precisely so this hash means \"the algorithm moved\".\n\n\
             fingerprint input was:\n{}",
            base_fingerprint_text(&FilmBaseParams::default())
        );

        let recipe = stable_hash(&recipe_fingerprint_text());
        assert_eq!(
            recipe,
            row.recipe,
            "the DEFAULT RECIPE changed but PIPELINE_VERSION is still {PIPELINE_VERSION}.\n\n\
             If a *default value* changed (output depth/profile, film-base source, an input \
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
             render than PIPELINE_BEHAVIOR does. `nc --version` prints PIPELINE_BEHAVIOR, so \
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

        // (a) the print side.
        let perturbed_print = PrintParams {
            print_exposure: PrintParams::default().print_exposure + f32::EPSILON,
            ..PrintParams::default()
        };
        assert_ne!(
            stable_hash(&render_fingerprint_text(&default_recon, &perturbed_print)),
            row.render,
            "a perturbed default print knob must move the render fingerprint"
        );

        // (b) the reconstruction side — a print-only perturbation leaves the density
        // curve, the part `render` mostly exists to pin, unproven.
        let perturbed_recon = Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Exponential(ExponentialParams {
                gamma: 1.05,
                ..ExponentialParams::default()
            }),
        };
        assert_ne!(
            stable_hash(&render_fingerprint_text(
                &perturbed_recon,
                &PrintParams::default()
            )),
            row.render,
            "a perturbed density curve must move the render fingerprint"
        );

        // (c) the film-base side: a different source resolves a different base, so
        // the stage-2 fingerprint must move too.
        let perturbed_base = FilmBaseParams {
            source: crate::types::FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
        };
        assert_ne!(
            stable_hash(&base_fingerprint_text(&perturbed_base)),
            row.base,
            "a different film-base source must move the base fingerprint"
        );

        // And the unperturbed defaults DO match the row — so the assertions above
        // failed for the perturbation, not because the formatter never matches.
        assert_eq!(
            stable_hash(&render_fingerprint_text(
                &default_recon,
                &PrintParams::default()
            )),
            row.render
        );
        assert_eq!(
            stable_hash(&base_fingerprint_text(&FilmBaseParams::default())),
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
