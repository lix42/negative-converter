//! The new chain's recipe (`nf-core/recipe-schema`).
//!
//! One document, versioned **as a document**: a recipe for the new chain states
//! `"recipe_version": 2` at top level, and that marker is what makes the recipe
//! self-describing. `--new-flow` still selects the chain, but a recipe can no longer
//! mean one thing under the flag and another without it — each side refuses the
//! other's recipe by name ([`check_body`]). That closes the gap `deny_unknown_fields`
//! cannot: it rejects an *unknown* key, and is blind to a **known but meaningless**
//! one, such as a whole `print` section loaded under a chain that has no print stage.
//!
//! The sections follow the chain, one per stage, and an identity stage still has one:
//!
//! ```text
//! input · calibration · measure      shared with the current chain (decode, film base)
//! reconstruction                     the fixed decode — algo::fixed::DecodeParams
//! scene_correction · look ·          one per rendering stage with its knobs; fit
//! fit_range · fit_gamut                gamut has none (its ceiling and target are not
//!                                      the recipe's), so its section stays empty
//! ```
//!
//! **No per-section `schema_version`.** The current chain's tagged `reconstruction`
//! carries one because it once had to tell several shapes apart inside a single
//! object; here the document version does that for every section at once.
//!
//! **No `output` section yet.** The new chain writes one fixed destination, so there is
//! no output policy to choose, and a section nothing reads is the defect this module
//! exists to prevent. `nf-destinations/preset-set` adds it with the destination set. Each key lands with the task that ships its knob — design-spec
//! §9 states the shape, not keys written ahead of the code.
//!
//! Named for what it will be rather than for the migration: after
//! `nf-core/default-flip` this is *the* recipe, and the current chain's
//! `cli::ResolvedConfig` is what gets deleted.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::algo::fixed::{AnchorRule, DecodeFault, DecodeParams, LINEARIZATION};
use crate::cli::ResolvedConfig;
use crate::pipeline::chain::{ChainParams, DisplayTarget, SharedParams};
use crate::pipeline::fit_gamut::DestinationGamut;
use crate::pipeline::fit_range::DisplayPeak;
use crate::pipeline::look::{
    ChannelGradeFault, ContrastFault, DesaturationFault, LookParams, LookSection, MAX_START_STOPS,
};
use crate::pipeline::scene_correction::{SceneCorrectionParams, SceneFault, WhiteBalance};
use crate::types::{
    CalibrationParams, FilmBaseSource, InputParams, MeasureParams, NcError, Result,
};

/// The only document version this build reads.
pub const RECIPE_VERSION: u32 = 2;

/// The top-level key carrying [`RECIPE_VERSION`].
///
/// Reserved beside `params` (the sidecar envelope's key): the current chain's recipe
/// must never gain a field of this name, or a v2 document would stop being
/// distinguishable from a v1 one. A test pins it absent from `ResolvedConfig`.
pub const VERSION_KEY: &str = "recipe_version";

/// A recipe for the new chain.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// Required on load, so a document that does not say which chain it describes
    /// is never read as this one. [`check_body`] refuses its absence with a
    /// migration message before serde would report a bare "missing field".
    pub recipe_version: RecipeVersion,
    #[serde(default)]
    pub input: InputParams,
    #[serde(default)]
    pub calibration: Calibration,
    #[serde(default)]
    pub measure: MeasureParams,
    #[serde(default)]
    pub reconstruction: DecodeParams,
    #[serde(default)]
    pub scene_correction: SceneCorrectionParams,
    #[serde(default)]
    pub look: LookSection,
    #[serde(default)]
    pub fit_range: FitRange,
    #[serde(default)]
    pub fit_gamut: FitGamut,
}

/// The document version — a type with exactly one value, so a recipe cannot
/// deserialize into a version this build does not read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecipeVersion;

impl Serialize for RecipeVersion {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u32(RECIPE_VERSION)
    }
}

impl<'de> Deserialize<'de> for RecipeVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = u64::deserialize(d)?;
        if v == u64::from(RECIPE_VERSION) {
            Ok(RecipeVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "`{VERSION_KEY}` is {v}; this build reads only {RECIPE_VERSION}"
            )))
        }
    }
}

/// Fit range's recipe section: how much scene range above diffuse white it
/// compresses.
///
/// Its own type rather than [`FitRangeParams`], for the reason [`FitGamut`] is: the
/// stage's other parameter, the display's peak, is the **destination's** to state, and
/// [`Recipe::chain_params`] adds it. The headroom is shared by every rendition of a
/// frame, the peak is not (`pipeline::chain`'s branch contract).
///
/// [`FitRangeParams`]: crate::pipeline::fit_range::FitRangeParams
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FitRange {
    /// In stops above diffuse white: reinhard's white point is `2^headroom_stops`, and
    /// `0` is the identity.
    pub headroom_stops: f32,
}

impl Default for FitRange {
    fn default() -> Self {
        Self {
            headroom_stops: crate::types::DEFAULT_HEADROOM_STOPS,
        }
    }
}

/// Fit gamut's recipe section: empty, and refuses any key. The map has no knob — its
/// ceiling is fit range's output and its target the destination's — and no off switch
/// (decided 2026-09-23, `nf-display-stages/gamut-map-share`).
///
/// Its own type rather than [`FitGamutParams`], because the stage's one parameter
/// today — the target gamut — is the **destination's** to state, not the recipe's:
/// [`Recipe::chain_params`] adds it. A recipe key for it would be a second way to
/// choose primaries the destination already fixes.
///
/// [`FitGamutParams`]: crate::pipeline::fit_gamut::FitGamutParams
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitGamut {}

/// The new chain's roll calibration: the film base alone.
///
/// Its own type rather than the current chain's [`CalibrationParams`], which retires
/// with that chain (`nf-core/default-flip`). The two had different shapes until the
/// reference density `dmax` retired (`nf-retire/dmax-machinery`). The section stays
/// open, as design-spec §8 describes it; a later measurement joins it with its own
/// task.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Calibration {
    /// Required at `convert`/`roll` time with no default, exactly as on the current
    /// chain — `cli::validate` refuses an unstated one on the projection.
    pub film_base: Option<FilmBaseSource>,
}

/// The current chain's sections this recipe does not have, and where each one's
/// knobs go. `reconstruction` is not here: the name survives with a different
/// shape, and [`check_body`] diagnoses its old keys one by one.
const SECTIONS_WITH_NO_COUNTERPART: &[(&str, &str)] = &[
    (
        "print",
        "white balance and exposure are `scene_correction.white_balance` and \
         `scene_correction.exposure`; the display tone is fit range, whose one \
         operator is reinhard and whose headroom is `fit_range.headroom_stops`; the \
         black point splits between scene correction and fit range \
         (`nf-scene-correction/flare-removal`), and `linear_range` has no home yet \
         (`nf-scene-correction/levels-knob`) — neither of those two has a key yet",
    ),
    (
        "output",
        "the new chain writes one fixed destination, so there is no output policy to \
         choose; its output section arrives with the destination set \
         (`nf-destinations/preset-set`)",
    ),
];

/// The current chain's `reconstruction` keys, and each one's fate here.
const OLD_RECONSTRUCTION_KEYS: &[(&str, &str)] = &[
    (
        "schema_version",
        "the document's `recipe_version` versions every section at once",
    ),
    (
        "type",
        "there is one fixed decode, so no reconstruction type to choose",
    ),
    (
        "curve",
        "there is one curve: its slope is `reconstruction.linearization` (print contrast \
         is `look.contrast`) and its placement \
         `reconstruction.anchor` (`{\"mid-at-base-offset\": <d>}`)",
    ),
    (
        "density",
        "`density.scale` and `density.offset` are `reconstruction.scale` and \
         `reconstruction.offset`; the regional balances are replaced by the look's \
         per-channel grade, `look.channel_grade`",
    ),
];

/// Keys neither chain reads any more — each already a migration error on the
/// current chain (`cli::reject_legacy_recipe_keys`), whose remedies point at that
/// chain's homes. These point at the new chain's, so the diagnosis survives the flag.
/// A path is top-level (`["density"]`) or one level down (`["input", "color"]`).
const RETIRED_KEYS: &[(&[&str], &str)] = &[
    (
        &["film_base"],
        "the film base is `calibration.film_base` (`\"auto\"`, `{\"region\": [x, y, w, h]}` \
         or `{\"explicit\": [r, g, b]}`)",
    ),
    (
        &["algorithm"],
        "there is one fixed decode, so no algorithm to choose",
    ),
    (
        &["density"],
        "`density.scale` and `density.offset` are `reconstruction.scale` and \
         `reconstruction.offset`",
    ),
    (
        &["sigmoid"],
        "there is one curve: its slope is `reconstruction.linearization` (print contrast \
         is `look.contrast`) and its placement \
         `reconstruction.anchor`",
    ),
    (
        &["simple"],
        "there is one fixed decode, so no algorithm to choose",
    ),
    (
        &["calibration", "dmax"],
        "the roll reference density retired with the placements that read it; the fixed \
         decode's anchor rule is reference-free",
    ),
    (
        &["input", "color"],
        "it conflated transfer encoding with measurement meaning; use the independent \
         keys `input.transfer` (auto|linear) and `input.meaning` \
         (auto|scanner-device|colorimetric)",
    ),
];

/// Refuse a recipe body written for the other chain, before serde sees it.
///
/// Run on the raw JSON, because the failure is about presence: a missing marker, or
/// a key that parses on one chain and means nothing on the other. `whole` is `true`
/// for a whole recipe and `false` for a `roll` per-frame overlay, which is a partial
/// document merged onto an already-versioned one and so need not restate the
/// version (it may, and must then state the right one).
///
/// One diagnosis per call, the first found: a user fixing a recipe removes keys one
/// at a time, and a list reads as though all of them had to go before anything else
/// could be checked.
pub fn check_body(body: &serde_json::Value, whole: bool, context: &str) -> Result<()> {
    let usage = |m: String| Err(NcError::Usage(format!("{context}: {m}")));
    match body.get(VERSION_KEY) {
        None if whole => {
            return usage(format!(
                "under `--new-flow` a recipe must state `\"{VERSION_KEY}\": {RECIPE_VERSION}` — \
                 without it the document describes the current chain, whose sections this \
                 chain does not read. `hanten params --new-flow` writes the new layout; or run \
                 without `--new-flow`, where a recipe with no `{VERSION_KEY}` is read"
            ));
        }
        Some(v) if v.as_u64() != Some(u64::from(RECIPE_VERSION)) => {
            return usage(format!(
                "`{VERSION_KEY}` is {v}; this build reads only {RECIPE_VERSION}"
            ));
        }
        _ => {}
    }
    for (path, why) in RETIRED_KEYS {
        let found = match path {
            [key] => body.get(key),
            [section, key] => body.get(section).and_then(|s| s.get(key)),
            _ => unreachable!("a retired key is one or two levels deep"),
        };
        if found.is_some() {
            return usage(format!(
                "`{}` is not a key of either chain's recipe any more: {why}",
                path.join(".")
            ));
        }
    }
    for (section, why) in SECTIONS_WITH_NO_COUNTERPART {
        if body.get(section).is_some() {
            return usage(format!(
                "`{section}` is a section of the current chain's recipe, not the new one's: \
                 {why}. Drop it — the current chain reads it only in a recipe with no \
                 `{VERSION_KEY}`"
            ));
        }
    }
    let old_key = |section: &str, table: &[(&'static str, &'static str)]| {
        let fields = body.get(section)?.as_object()?;
        table
            .iter()
            .find(|(key, _)| fields.contains_key(*key))
            .map(|(key, why)| (*key, *why))
    };
    // Retired by `nf-scene-correction/roll-white-balance`. Named here rather than left
    // to serde, whose "unknown variant" says nothing about where the mode went.
    if let Some(mode) = body
        .get("scene_correction")
        .and_then(|s| s.get("white_balance"))
        .and_then(|w| w.as_str())
        .filter(|m| ["gray-world", "percentile"].contains(m))
    {
        return usage(format!(
            "`scene_correction.white_balance` \"{mode}\" was a per-frame estimate, and the \
             new chain has none: it read a sunset as the cast and removed it. Drop it, \
             then state the gains `hanten measure-roll` reports for the roll, as \
             `{{\"explicit\": [r, g, b]}}`"
        ));
    }
    // Retired by `nf-reconstruction/gamma-split`, which split the one slope in two.
    // Refused at every value, the old default included: no single new key replays it.
    if let Some(v) = body.get("reconstruction").and_then(|r| r.get("contrast")) {
        let remedy = match v.as_f64() {
            // In f32, as the recipe holds it, so the stated value prints as written.
            // Only a look value validation accepts: a stated slope so small or so large
            // that the quotient leaves the normal f32 range falls to the generic remedy.
            Some(gamma) if (gamma as f32 / LINEARIZATION).is_normal() && gamma > 0.0 => {
                format!(
                    "to keep a stated {} as the whole contrast, write `look.contrast`: {} \
                     and leave `reconstruction.linearization` at its default \
                     {LINEARIZATION}",
                    gamma as f32,
                    gamma as f32 / LINEARIZATION,
                )
            }
            _ => format!(
                "state `look.contrast` for the print contrast, and leave \
                 `reconstruction.linearization` at its default {LINEARIZATION}"
            ),
        };
        return usage(format!(
            "`reconstruction.contrast` split in two: the decode's slope is now \
             `reconstruction.linearization`, the film's linearization, and how contrasty \
             the picture is is the look's `look.contrast`. Drop the key; {remedy}"
        ));
    }
    if let Some((key, why)) = old_key("reconstruction", OLD_RECONSTRUCTION_KEYS) {
        return usage(format!(
            "`reconstruction.{key}` belongs to the current chain's recipe, not the new \
             one's: {why}. Drop it — the current chain reads it only in a recipe with no \
             `{VERSION_KEY}`"
        ));
    }
    Ok(())
}

/// Refuse a new-chain recipe loaded **without** `--new-flow`.
///
/// The other half of the marker's contract. Without it the current chain's
/// `deny_unknown_fields` would still refuse the document, but with an opaque
/// "unknown field" message that never says the recipe was fine for the other chain.
pub fn check_body_without_flag(body: &serde_json::Value, context: &str) -> Result<()> {
    // The remedy says only what the value supports: "pass `--new-flow`" is advice
    // only for the version the new chain reads, or the user is sent to a second
    // refusal (`recipe_version` 1 is the likely case: meant as "the current schema").
    match body.get(VERSION_KEY) {
        None => Ok(()),
        Some(v) if v.as_u64() == Some(u64::from(RECIPE_VERSION)) => Err(NcError::Usage(format!(
            "{context}: states `{VERSION_KEY}`, so it describes the new rendering chain, \
                 which only `--new-flow` reads. The current chain's recipe carries no version"
        ))),
        Some(v) => Err(NcError::Usage(format!(
            "{context}: states `{VERSION_KEY}` {v}, which no chain reads — the current \
             chain's recipe carries no `{VERSION_KEY}` at all (remove it), and the new \
             chain (`--new-flow`) reads only {RECIPE_VERSION}"
        ))),
    }
}

/// Apply the command-line flags the new chain reads, flags winning over the recipe.
///
/// Every other conversion flag is refused by presence before this runs — by
/// `flow::reject_unavailable_flags`, or on both chains by `cli`'s removed-flag check
/// (`--reconstruction`, `--density-curve`, `--preset`, …) — so it has nothing to set. `flow`'s `every_kept_flag_reaches_the_recipe` holds that each kept flag
/// has an arm here.
pub fn merge(mut r: Recipe, args: &crate::cli::ConvertArgs) -> Recipe {
    crate::cli::merge_shared_sections(
        &mut r.input,
        &mut r.calibration.film_base,
        &mut r.measure,
        args,
    );
    // The decode's own knobs. `--density-gamma` is the decode's linearization — the
    // calibrated half of `gamma`; print contrast is `--contrast`, the look's
    // (`nf-reconstruction/gamma-split`). The flag keeps the current chain's spelling,
    // where it is still the whole bundled slope.
    if let Some(v) = args.density.density_scale {
        r.reconstruction.scale = v;
    }
    if let Some(v) = args.density.density_offset {
        r.reconstruction.offset = v;
    }
    if let Some(v) = args.density.density_gamma {
        r.reconstruction.linearization = v;
    }
    if let Some(d) = args.anchor.anchor_mid_offset {
        r.reconstruction.anchor = AnchorRule::MidAboveBase(d);
    }
    // Scene correction. `--auto-wb` never reaches here: `flow` refuses it by
    // presence, since this chain has no per-frame estimate.
    if let Some(gains) = args.print.white_balance {
        r.scene_correction.white_balance = WhiteBalance::Explicit(gains);
    }
    if let Some(stops) = args.scene.exposure {
        r.scene_correction.exposure = stops;
    }
    if let Some(v) = args.look.contrast {
        r.look.contrast = v;
    }
    if let Some(v) = args.look.channel_grade {
        r.look.channel_grade = v;
    }
    let desat = &mut r.look.highlight_desaturation;
    if let Some(v) = args.look.highlight_desaturation {
        desat.strength = v;
    }
    if let Some(v) = args.look.highlight_desaturation_start {
        desat.start_stops = v;
    }
    if let Some(v) = args.look.highlight_desaturation_band {
        desat.band = v;
    }
    if let Some(stops) = args.print.display_tone_headroom {
        r.fit_range.headroom_stops = stops;
    }
    r
}

/// How a validation message names a knob: by the flag and the recipe key on
/// `convert`, which accepts both, and by the key alone on `roll`, which accepts no
/// conversion flags — naming a flag there hands the user a remedy they cannot type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnobNames {
    FlagAndKey,
    KeyOnly,
}

/// A knob as a validation message names it — one spelling rule for every section.
fn knob_name(names: KnobNames, section: &str, flag: &str, key: &str) -> String {
    match names {
        KnobNames::FlagAndKey => format!("{flag} (recipe `{section}.{key}`)"),
        KnobNames::KeyOnly => format!("`{section}.{key}`"),
    }
}

/// The value rules this recipe's own sections carry: the decode's, which live in
/// [`DecodeParams::check`] and are rendered here as a usage error. The shared
/// sections are checked on the projection, by the same `cli::validate` the current
/// chain uses.
pub fn validate(r: &Recipe, names: KnobNames) -> Result<()> {
    let name = |flag: &str, key: &str| knob_name(names, "reconstruction", flag, key);
    let d = &r.reconstruction;
    let message = match d.check() {
        Ok(_) => {
            validate_scene_correction(&r.scene_correction, names)?;
            validate_look(&r.look, names)?;
            validate_whole_contrast(d.linearization, &r.look, names)?;
            return validate_fit_range(&r.fit_range, names);
        }
        Err(DecodeFault::Offset { channel, value }) => format!(
            "{} must be finite on every channel, got {value} on channel {channel}",
            name("--density-offset", "offset")
        ),
        Err(DecodeFault::Scale { channel, value }) => format!(
            "{} must be finite and positive on every channel, got {value} on channel {channel}",
            name("--density-scale", "scale")
        ),
        Err(DecodeFault::Linearization(v)) => format!(
            "{} must be finite and positive, got {v}",
            name("--density-gamma", "linearization")
        ),
        Err(DecodeFault::MidAboveBase(v)) => format!(
            "{} must be finite and positive, got {v}",
            name("--anchor-mid-offset", "anchor")
        ),
        Err(DecodeFault::Anchor {
            anchor,
            linearization,
        }) => {
            let AnchorRule::MidAboveBase(mid) = d.anchor;
            // Two routes, and opposite remedies: a tiny linearization overflows the
            // anchor itself (`0.745 / linearization`), while a huge offset overflows only
            // the exponent `linearization · anchor`, where a larger one makes it worse.
            let remedy = if anchor.is_finite() {
                format!("Use a smaller {}", name("--anchor-mid-offset", "anchor"))
            } else {
                format!("Use a larger {}", name("--density-gamma", "linearization"))
            };
            format!(
                "the decode's anchor is not usable at {} {linearization:e} and {} {mid:e}: \
                 it derives an anchor of {anchor:e}, whose exponent overflows f32 and would \
                 render every sample as exactly 0.0. {remedy}",
                name("--density-gamma", "linearization"),
                name("--anchor-mid-offset", "anchor"),
            )
        }
    };
    Err(NcError::Usage(message))
}

/// The look's value rules ([`LookSection::check_contrast`],
/// [`LookSection::check_channel_grade`], [`HighlightDesaturation::check`]), rendered as
/// a usage error naming the knob the way `names` says the command spells it.
///
/// [`HighlightDesaturation::check`]: crate::pipeline::look::HighlightDesaturation::check
fn validate_look(p: &LookSection, names: KnobNames) -> Result<()> {
    if let Err(ContrastFault(v)) = p.check_contrast() {
        return Err(NcError::Usage(format!(
            "{} must be finite and positive (1 is the identity), got {v}",
            knob_name(names, "look", "--contrast", "contrast")
        )));
    }
    if let Err(ChannelGradeFault([r, b])) = p.check_channel_grade() {
        return Err(NcError::Usage(format!(
            "{} must be two finite, positive exponents whose spread with green's 1 is \
             under 1 (1,1 is the identity; a wider spread can fold the tone scale), got \
             [{r}, {b}]",
            knob_name(names, "look", "--channel-grade", "channel_grade")
        )));
    }
    let name = |flag: &str, key: &str| {
        knob_name(
            names,
            "look",
            flag,
            &format!("highlight_desaturation.{key}"),
        )
    };
    let message = match p.highlight_desaturation.check() {
        Ok(()) => return Ok(()),
        Err(DesaturationFault::Strength(v)) => format!(
            "{} must be within [0, 1] (0 is off), got {v}",
            name("--highlight-desaturation", "strength")
        ),
        Err(DesaturationFault::Start(v)) => format!(
            "{} must be negative and at most {MAX_START_STOPS} stops below diffuse white — \
             it is a highlight operator, and midtone cast is the grade's — got {v}",
            name("--highlight-desaturation-start", "start_stops")
        ),
        Err(DesaturationFault::Band([s0, s1])) => format!(
            "{} must be finite with 0 <= s0 < s1, got [{s0}, {s1}]",
            name("--highlight-desaturation-band", "band")
        ),
    };
    Err(NcError::Usage(message))
}

/// The whole contrast, `linearization · look.contrast` — the divisor highlight
/// desaturation normalises its saturation measure by — must be a normal positive f32.
/// Each factor can pass its own rule while the product overflows to infinity or
/// underflows to zero or a subnormal, which the stage cannot use. Keyed on the product
/// whether or not desaturation is on: a whole contrast outside f32 describes no usable
/// picture, and one rule is easier to state than a conditional one. Runs after both
/// factors' own rules, so each is already finite and positive.
fn validate_whole_contrast(linearization: f32, look: &LookSection, names: KnobNames) -> Result<()> {
    let total = linearization * look.contrast;
    if total.is_normal() {
        return Ok(());
    }
    let gamma = knob_name(names, "reconstruction", "--density-gamma", "linearization");
    let contrast = knob_name(names, "look", "--contrast", "contrast");
    let (what, remedy) = if total.is_infinite() {
        ("overflows", "smaller")
    } else {
        ("underflows", "larger")
    };
    Err(NcError::Usage(format!(
        "the whole contrast, {gamma} {linearization:e} times {contrast} {:e}, {what} \
         f32 (highlight desaturation divides by it). Use a {remedy} value for either",
        look.contrast
    )))
}

/// Scene correction's value rules ([`SceneCorrectionParams::check`]), rendered as a
/// usage error naming the knob the way `names` says the command spells it.
fn validate_scene_correction(p: &SceneCorrectionParams, names: KnobNames) -> Result<()> {
    let name = |flag: &str, key: &str| knob_name(names, "scene_correction", flag, key);
    let message = match p.check() {
        Ok(()) => return Ok(()),
        Err(SceneFault::WhiteBalance { channel, value }) => format!(
            "{} must be finite and positive on every channel, got {value} on channel \
             {channel}",
            name("--white-balance", "white_balance")
        ),
        Err(SceneFault::Exposure(stops)) => format!(
            "{} must be finite, with a gain 2^EV that is a normal f32 (roughly -126 to \
             +127 stops), got {stops}",
            name("--exposure", "exposure")
        ),
        Err(SceneFault::Combined { channel, gain }) => format!(
            "{} times the exposure gain from {} is {gain:e} on channel {channel}, which \
             is not a normal f32 — every sample of that channel would render as 0 or \
             inf. Move the white balance or the exposure toward neutral",
            name("--white-balance", "white_balance"),
            name("--exposure", "exposure"),
        ),
    };
    Err(NcError::Usage(message))
}

/// Fit range's value rule — the headroom's, [`crate::types::headroom_fault`], shared
/// with the current chain's knob — rendered as a usage error for this recipe's key.
fn validate_fit_range(p: &FitRange, names: KnobNames) -> Result<()> {
    let Some(fault) = crate::types::headroom_fault(p.headroom_stops) else {
        return Ok(());
    };
    let name = knob_name(
        names,
        "fit_range",
        "--display-tone-headroom",
        "headroom_stops",
    );
    Err(NcError::Usage(crate::types::headroom_fault_message(
        fault, &name,
    )))
}

impl Recipe {
    /// One rendition's parameters for `pipeline::chain::render` — the recipe's shared
    /// half ([`Recipe::shared_params`]) plus the destination's peak and gamut, which
    /// only the destination states.
    pub fn chain_params(&self, peak: DisplayPeak, gamut: DestinationGamut) -> ChainParams {
        ChainParams {
            shared: self.shared_params(),
            target: DisplayTarget { peak, gamut },
        }
    }

    /// Everything every rendition of a frame shares — the stages above the SDR/HDR
    /// branch point and fit range's headroom (`pipeline::chain`'s branch contract).
    pub fn shared_params(&self) -> SharedParams {
        SharedParams {
            scene_correction: self.scene_correction.clone(),
            look: LookParams {
                section: self.look,
                linearization: self.reconstruction.linearization,
            },
            headroom_stops: self.fit_range.headroom_stops,
        }
    }

    /// The current chain's config carrying this recipe's **shared** sections, for the
    /// stages both chains run: decode, the film base and the measurement region — plus
    /// `fit_range`, the one section both recipes spell identically.
    ///
    /// Scaffolding, deleted with `ResolvedConfig` by `nf-core/default-flip`. Every
    /// other section is left at its default, which is safe only because nothing past
    /// the film base reads them on the new flow: the decode reads
    /// [`Recipe::reconstruction`] and the chain [`Recipe::chain_params`], never this
    /// projection, and the destination is fixed rather than resolved from `output`.
    pub fn to_config(&self) -> ResolvedConfig {
        ResolvedConfig {
            input: self.input.clone(),
            calibration: CalibrationParams {
                film_base: self.calibration.film_base.clone(),
            },
            measure: self.measure.clone(),
            // The same section on both chains, so the projection states the user's value
            // rather than a default the run does not use.
            fit_range: self.fit_range.clone(),
            ..ResolvedConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> std::result::Result<Recipe, serde_json::Error> {
        serde_json::from_str(json)
    }

    fn check(json: &str, whole: bool) -> std::result::Result<(), String> {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        check_body(&v, whole, "recipe r.json").map_err(|e| e.message().to_string())
    }

    /// Top-level keys of a serialized document, in the order they are written.
    /// (A `serde_json::Value` map sorts its keys, so it cannot answer this.)
    fn written_order(text: &str) -> Vec<String> {
        let mut depth = 0usize;
        let mut keys = Vec::new();
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                '"' => {
                    let end = text[i + 1..].find('"').unwrap() + i + 1;
                    if depth == 1 && text[end + 1..].trim_start().starts_with(':') {
                        keys.push(text[i + 1..end].to_string());
                    }
                    while chars.peek().is_some_and(|(j, _)| *j <= end) {
                        chars.next();
                    }
                }
                _ => {}
            }
        }
        keys
    }

    #[test]
    fn the_default_document_names_every_stage_in_chain_order_and_round_trips() {
        let text = serde_json::to_string_pretty(&Recipe::default()).unwrap();
        assert_eq!(
            written_order(&text),
            [
                VERSION_KEY,
                "input",
                "calibration",
                "measure",
                "reconstruction",
                "scene_correction",
                "look",
                "fit_range",
                "fit_gamut",
            ]
        );
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        // A stage with no knob is present as an empty object, not absent or `null`;
        // one with knobs writes each of them at its default — the identity for scene
        // correction, contrast 2.0/1.8, the identity grade and highlight desaturation at
        // 0.8 for the look (in the order the stage applies them), and reinhard at six
        // stops for fit range. (Fit gamut's map runs at every setting; it simply has
        // nothing for a recipe to set.)
        assert_eq!(json["fit_gamut"], serde_json::json!({}));
        assert_eq!(
            serde_json::to_string(&Recipe::default().look).unwrap(),
            r#"{"contrast":1.1111112,"channel_grade":[1.0,1.0],"highlight_desaturation":{"strength":0.8,"start_stops":-1.0,"band":[0.015,0.025]}}"#
        );
        assert_eq!(
            json["scene_correction"],
            serde_json::json!({"white_balance": {"explicit": [1.0, 1.0, 1.0]}, "exposure": 0.0})
        );
        assert_eq!(
            json["fit_range"],
            serde_json::json!({"headroom_stops": 6.0})
        );
        assert_eq!(json[VERSION_KEY], RECIPE_VERSION);
        let back: Recipe = serde_json::from_str(&text).unwrap();
        assert_eq!(back, Recipe::default());
    }

    #[test]
    fn the_decode_section_spells_the_decode_parameters() {
        // Compared as text: a `Value` holds the f32 defaults widened to f64
        // (`0.8399999737739563`), while the written document spells `0.84`.
        assert_eq!(
            serde_json::to_string(&Recipe::default().reconstruction).unwrap(),
            r#"{"scale":[1.0,0.84,0.73],"offset":[0.0,0.0,0.0],"linearization":1.8,"anchor":{"mid-at-base-offset":0.62}}"#
        );
        // No reference density in the new calibration section.
        let json = serde_json::to_value(Recipe::default()).unwrap();
        assert_eq!(json["calibration"], serde_json::json!({"film_base": null}));
    }

    #[test]
    fn the_look_is_handed_the_decodes_linearization() {
        // Highlight desaturation's measure is normalised by the whole contrast that
        // shaped its input — the decode's half of it the look section cannot state, so
        // `chain_params` must.
        let r = parse(
            r#"{"recipe_version": 2, "reconstruction": {"linearization": 3.1},
                "look": {"contrast": 1.2, "highlight_desaturation": {"strength": 0.5}}}"#,
        )
        .unwrap();
        let look = r
            .chain_params(DisplayPeak::SDR, DestinationGamut::DisplayP3)
            .shared
            .look;
        assert_eq!(look.linearization, 3.1);
        assert_eq!(look.section.contrast, 1.2);
        assert_eq!(look.section.highlight_desaturation.strength, 0.5);
        assert_eq!(
            look.section.highlight_desaturation.band,
            LookSection::default().highlight_desaturation.band,
            "an omitted key takes its default"
        );
    }

    #[test]
    fn the_look_contrast_leaves_the_decode_untouched() {
        // The split's falsifiable point: how contrasty the picture is must not reach the
        // decode. The decode reads `reconstruction` alone, so its output — what
        // `film-master` will carry — is bit-identical across look contrasts, while the
        // linearization, stated beside it, does move it.
        use crate::algo::fixed;
        use crate::types::{FilmBase, LinearImage};
        let scan = LinearImage::new(2, 1, vec![0.5, 0.3, 0.2, 0.05, 0.04, 0.03], None).unwrap();
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let decoded = |json: &str| {
            let r = parse(json).unwrap();
            let (film, _) = fixed::decode(&scan, &base, &r.reconstruction).unwrap();
            film.rgb().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        };
        let plain = decoded(r#"{"recipe_version": 2}"#);
        assert_eq!(
            plain,
            decoded(r#"{"recipe_version": 2, "look": {"contrast": 1.6}}"#)
        );
        assert_ne!(
            plain,
            decoded(r#"{"recipe_version": 2, "reconstruction": {"linearization": 2.0}}"#)
        );
    }

    #[test]
    fn a_partial_recipe_takes_the_defaults_it_omits() {
        let r =
            parse(r#"{"recipe_version": 2, "reconstruction": {"linearization": 1.7}}"#).unwrap();
        assert_eq!(r.reconstruction.linearization, 1.7);
        assert_eq!(r.reconstruction.scale, DecodeParams::default().scale);
        assert_eq!(r.look, LookSection::default());
    }

    #[test]
    fn the_version_is_required_and_exact() {
        assert!(
            parse("{}")
                .unwrap_err()
                .to_string()
                .contains("recipe_version")
        );
        let err = parse(r#"{"recipe_version": 3}"#).unwrap_err().to_string();
        assert!(err.contains("reads only 2"), "{err}");
    }

    #[test]
    fn every_section_rejects_an_unknown_key() {
        for section in [
            "input",
            "calibration",
            "measure",
            "reconstruction",
            "scene_correction",
            "look",
            "fit_range",
            "fit_gamut",
        ] {
            let json = format!(r#"{{"recipe_version": 2, "{section}": {{"nonsense": 1}}}}"#);
            let err = parse(&json).unwrap_err().to_string();
            assert!(err.contains("nonsense"), "{section}: {err}");
        }
        let err = parse(r#"{"recipe_version": 2, "nonsense": {}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonsense"), "{err}");
    }

    #[test]
    fn a_retired_per_frame_white_balance_names_the_roll_measurement() {
        for mode in ["gray-world", "percentile"] {
            let body = format!(r#"{{"scene_correction": {{"white_balance": "{mode}"}}}}"#);
            let err = check(&body, false).unwrap_err();
            assert!(
                err.contains(mode) && err.contains("measure-roll") && err.contains("explicit"),
                "{err}"
            );
        }
        // Only the two retired names: any other string is a typo for serde to report,
        // not "a per-frame estimate".
        check(
            r#"{"scene_correction": {"white_balance": "neutral"}}"#,
            false,
        )
        .unwrap();
        // Stated gains are the one form, and pass the schema check.
        check(
            r#"{"scene_correction": {"white_balance": {"explicit": [1.2, 1, 1.1]}}}"#,
            false,
        )
        .unwrap();
    }

    #[test]
    fn a_body_without_the_marker_is_refused_as_the_current_chains() {
        let err = check(r#"{"calibration": {"film_base": "auto"}}"#, true).unwrap_err();
        assert!(err.contains("\"recipe_version\": 2"), "{err}");
        assert!(err.contains("hanten params --new-flow"), "{err}");
        // A per-frame overlay is partial and may omit it…
        check(r#"{"calibration": {"film_base": "auto"}}"#, false).unwrap();
        // …but may not state a wrong one.
        let err = check(r#"{"recipe_version": 1}"#, false).unwrap_err();
        assert!(err.contains("reads only 2"), "{err}");
    }

    #[test]
    fn an_old_section_is_refused_by_name_with_where_it_went() {
        let err = check(r#"{"recipe_version": 2, "print": {}}"#, true).unwrap_err();
        assert!(
            err.contains("`print`") && err.contains("scene_correction"),
            "{err}"
        );
        let err = check(r#"{"recipe_version": 2, "output": {}}"#, true).unwrap_err();
        assert!(
            err.contains("`output`") && err.contains("preset-set"),
            "{err}"
        );
        let err = check(
            r#"{"recipe_version": 2, "reconstruction": {"density": {"scale": [1, 1, 1]}}}"#,
            true,
        )
        .unwrap_err();
        assert!(err.contains("reconstruction.density") && err.contains("reconstruction.scale"));
        let err = check(
            r#"{"recipe_version": 2, "reconstruction": {"curve": {"type": "exponential"}}}"#,
            true,
        )
        .unwrap_err();
        assert!(err.contains("reconstruction.linearization"), "{err}");
        let err = check(
            r#"{"recipe_version": 2, "calibration": {"dmax": "fixed"}}"#,
            true,
        )
        .unwrap_err();
        assert!(err.contains("calibration.dmax") && err.contains("reference-free"));
        // Overlays get the same diagnosis.
        let err = check(r#"{"print": {"print_exposure": 1}}"#, false).unwrap_err();
        assert!(err.contains("`print`"), "{err}");
        // The remedy must not send the user to the current chain as-is: this document
        // states `recipe_version`, which that chain refuses outright.
        assert!(!err.contains("run without `--new-flow`"), "{err}");
    }

    #[test]
    fn a_new_chain_recipe_is_refused_without_the_flag() {
        let v = serde_json::json!({"recipe_version": 2});
        let err = check_body_without_flag(&v, "recipe r.json").unwrap_err();
        assert!(err.message().contains("only `--new-flow` reads"), "{err}");
        check_body_without_flag(&serde_json::json!({}), "recipe r.json").unwrap();
        // Any other value reads on neither chain, so the flag is not the remedy: under
        // it, `1` would be refused again for the version.
        let v = serde_json::json!({"recipe_version": 1});
        let err = check_body_without_flag(&v, "recipe r.json").unwrap_err();
        assert!(!err.message().contains("pass `--new-flow`"), "{err}");
        assert!(err.message().contains("remove it"), "{err}");
    }

    /// The marker distinguishes the two documents only while the current chain's
    /// recipe cannot carry it — the same reserved-key rule as the envelope's `params`.
    #[test]
    fn the_version_key_is_reserved_and_no_section_is_named_params() {
        let old = serde_json::to_value(ResolvedConfig::default()).unwrap();
        assert!(old.get(VERSION_KEY).is_none());
        let new = serde_json::to_value(Recipe::default()).unwrap();
        assert!(new.get("params").is_none());
    }

    /// The recipe half of the availability inventory: every section and key the
    /// current chain's recipe can carry is either shared with this one or refused by
    /// name. A key added to the current chain's schema and classified nowhere would
    /// reach the new flow as an opaque "unknown field" — or, in a shared section, as
    /// a key the new flow parses and never reads.
    #[test]
    fn every_key_of_the_current_chains_recipe_is_shared_or_diagnosed() {
        let old = serde_json::to_value(ResolvedConfig::default()).unwrap();
        let new = serde_json::to_value(Recipe::default()).unwrap();
        for (section, fields) in old.as_object().unwrap() {
            if SECTIONS_WITH_NO_COUNTERPART
                .iter()
                .any(|(s, _)| s == section)
            {
                assert!(new.get(section).is_none(), "{section}");
                continue;
            }
            let shared = new
                .get(section)
                .unwrap_or_else(|| panic!("`{section}` is neither shared nor diagnosed"));
            let table: &[(&str, &str)] = match section.as_str() {
                "reconstruction" => OLD_RECONSTRUCTION_KEYS,
                _ => &[],
            };
            for key in fields.as_object().unwrap().keys() {
                let diagnosed = table.iter().any(|(k, _)| k == key);
                assert!(
                    shared.get(key).is_some() != diagnosed,
                    "`{section}.{key}` must be exactly one of shared or diagnosed"
                );
            }
        }
    }

    #[test]
    fn validate_refuses_each_unusable_decode_value() {
        let with = |f: fn(&mut DecodeParams)| {
            let mut r = Recipe::default();
            f(&mut r.reconstruction);
            validate(&r, KnobNames::FlagAndKey).map_err(|e| e.message().to_string())
        };
        validate(&Recipe::default(), KnobNames::FlagAndKey).unwrap();
        assert!(
            with(|d| d.scale[1] = 0.0)
                .unwrap_err()
                .contains("reconstruction.scale")
        );
        assert!(
            with(|d| d.offset[2] = f32::NAN)
                .unwrap_err()
                .contains("reconstruction.offset")
        );
        assert!(
            with(|d| d.linearization = -1.0)
                .unwrap_err()
                .contains("reconstruction.linearization")
        );
        assert!(
            with(|d| d.anchor = AnchorRule::MidAboveBase(0.0))
                .unwrap_err()
                .contains("reconstruction.anchor")
        );
        // A linearization small enough that `0.745 / linearization` overflows.
        // The remedy follows the route: here the anchor itself overflows, so a
        // larger linearization is the fix…
        let err = with(|d| d.linearization = 1e-39).unwrap_err();
        assert!(err.contains("anchor is not usable"), "{err}");
        assert!(err.contains("Use a larger --density-gamma"), "{err}");
        // A finite anchor whose exponent still overflows.
        // …and here only the exponent does, where a larger linearization makes it worse.
        let err = with(|d| d.anchor = AnchorRule::MidAboveBase(2e38)).unwrap_err();
        assert!(err.contains("anchor is not usable"), "{err}");
        assert!(err.contains("Use a smaller --anchor-mid-offset"), "{err}");
        assert!(!err.contains("larger"), "{err}");
    }

    #[test]
    fn roll_names_the_key_alone() {
        // `roll` accepts no conversion flags, so naming one there is a remedy the
        // user cannot type.
        let mut r = Recipe::default();
        r.reconstruction.linearization = 0.0;
        let msg = validate(&r, KnobNames::KeyOnly).unwrap_err();
        let msg = msg.message();
        assert!(msg.contains("`reconstruction.linearization`"), "{msg}");
        assert!(!msg.contains("--density-gamma"), "{msg}");
        let mut r = Recipe::default();
        r.look.contrast = 0.0;
        let msg = validate(&r, KnobNames::KeyOnly).unwrap_err();
        let msg = msg.message();
        assert!(msg.contains("`look.contrast`"), "{msg}");
        assert!(!msg.contains("--contrast"), "{msg}");
    }

    #[test]
    fn validate_refuses_an_unusable_look_contrast() {
        for bad in [0.0, -1.1, f32::NAN, f32::INFINITY] {
            let mut r = Recipe::default();
            r.look.contrast = bad;
            let msg = validate(&r, KnobNames::FlagAndKey).unwrap_err();
            assert!(
                msg.message()
                    .contains("--contrast (recipe `look.contrast`)"),
                "{bad}: {}",
                msg.message()
            );
        }
        let mut r = Recipe::default();
        r.look.contrast = 1.0;
        validate(&r, KnobNames::FlagAndKey).unwrap();
    }

    #[test]
    fn validate_refuses_an_unusable_channel_grade() {
        for bad in [
            [0.0, 1.0],
            [1.0, -0.2],
            [f32::NAN, 1.0],
            [2.0, 1.0],
            [1.5, 0.5],
        ] {
            let mut r = Recipe::default();
            r.look.channel_grade = bad;
            let msg = validate(&r, KnobNames::FlagAndKey).unwrap_err();
            let msg = msg.message();
            assert!(
                msg.contains("--channel-grade (recipe `look.channel_grade`)"),
                "{bad:?}: {msg}"
            );
            // The most specific rule speaks, not a neighbour's.
            assert!(!msg.contains("--contrast"), "{msg}");
            let msg = validate(&r, KnobNames::KeyOnly).unwrap_err();
            assert!(
                msg.message().contains("`look.channel_grade`")
                    && !msg.message().contains("--channel-grade"),
                "{}",
                msg.message()
            );
        }
        let mut r = Recipe::default();
        r.look.channel_grade = [1.4, 0.6];
        validate(&r, KnobNames::FlagAndKey).unwrap();
    }

    #[test]
    fn the_retired_contrast_key_is_refused_with_its_split() {
        // Refused at every value, the old default included: the key was both halves at
        // once, and no single new key replays it. The remedy states the look value
        // that keeps the stated slope as the whole contrast.
        for (json, look) in [
            (
                r#"{"recipe_version": 2, "reconstruction": {"contrast": 2.0}}"#,
                "1.11",
            ),
            (
                r#"{"recipe_version": 2, "reconstruction": {"contrast": 3.6}}"#,
                "2",
            ),
        ] {
            let err = check(json, true).unwrap_err();
            assert!(
                err.contains("reconstruction.linearization") && err.contains("look.contrast"),
                "{err}"
            );
            assert!(err.contains(&format!("`look.contrast`: {look}")), "{err}");
        }
        // A value whose quotient validation would refuse (zero, subnormal or infinite
        // in f32) gets the generic remedy, never a `look.contrast` it would then refuse.
        for json in [
            r#"{"recipe_version": 2, "reconstruction": {"contrast": "steep"}}"#,
            r#"{"recipe_version": 2, "reconstruction": {"contrast": 1e-50}}"#,
            r#"{"recipe_version": 2, "reconstruction": {"contrast": 1e-39}}"#,
            r#"{"recipe_version": 2, "reconstruction": {"contrast": 1e39}}"#,
            r#"{"recipe_version": 2, "reconstruction": {"contrast": -2.0}}"#,
        ] {
            let err = check(json, true).unwrap_err();
            assert!(err.contains("state `look.contrast`"), "{json}: {err}");
            assert!(!err.contains("write `look.contrast`"), "{json}: {err}");
        }
    }

    #[test]
    fn validate_refuses_a_whole_contrast_outside_f32() {
        // Each factor passes its own rule; the product, which highlight desaturation
        // divides by, does not. The remedy follows the direction.
        let with = |linearization: f32, contrast: f32, names| {
            let mut r = Recipe::default();
            r.reconstruction.linearization = linearization;
            r.look.contrast = contrast;
            validate(&r, names).map_err(|e| (e.exit_code(), e.message().to_string()))
        };
        for (linearization, contrast, remedy) in [
            (1e30, 1e10, "smaller"),
            (1e-30, 1e-20, "larger"),
            (1e-30, 1e-9, "larger"), // subnormal, not zero
        ] {
            let (code, msg) = with(linearization, contrast, KnobNames::FlagAndKey).unwrap_err();
            assert_eq!(code, 2, "{msg}");
            assert!(
                msg.contains("--density-gamma (recipe `reconstruction.linearization`)")
                    && msg.contains("--contrast (recipe `look.contrast`)"),
                "{msg}"
            );
            assert!(msg.contains(&format!("Use a {remedy}")), "{msg}");
            let (_, msg) = with(linearization, contrast, KnobNames::KeyOnly).unwrap_err();
            assert!(
                msg.contains("`reconstruction.linearization`") && msg.contains("`look.contrast`"),
                "{msg}"
            );
            assert!(
                !msg.contains("--density-gamma") && !msg.contains("--contrast"),
                "{msg}"
            );
        }
        // The remedy works: bringing either factor back makes the product usable.
        with(1e30, 1.0, KnobNames::FlagAndKey).unwrap();
        with(1.8, 1e10, KnobNames::FlagAndKey).unwrap();
        with(1e-30, 1.0, KnobNames::FlagAndKey).unwrap();
    }

    #[test]
    fn validate_refuses_an_unusable_fit_range_headroom() {
        let with = |stops: f32, names| {
            let mut r = Recipe::default();
            r.fit_range.headroom_stops = stops;
            validate(&r, names).map_err(|e| e.message().to_string())
        };
        with(0.0, KnobNames::FlagAndKey).unwrap();
        with(crate::types::MAX_HEADROOM_STOPS, KnobNames::FlagAndKey).unwrap();
        for bad in [-1.0, f32::NAN, f32::INFINITY] {
            let err = with(bad, KnobNames::FlagAndKey).unwrap_err();
            assert!(
                err.contains("--display-tone-headroom (recipe `fit_range.headroom_stops`)")
                    && err.contains("non-negative"),
                "{bad}: {err}"
            );
        }
        let err = with(25.0, KnobNames::KeyOnly).unwrap_err();
        assert!(err.contains("`fit_range.headroom_stops` is 25"), "{err}");
        assert!(!err.contains("--display-tone-headroom"), "{err}");
    }

    #[test]
    fn the_destination_states_fit_ranges_peak() {
        // The recipe carries the headroom; the peak comes from the destination, so a
        // recipe cannot name a peak its destination does not have.
        let mut r = Recipe::default();
        r.fit_range.headroom_stops = 4.0;
        let p = r.chain_params(DisplayPeak::SDR, DestinationGamut::DisplayP3);
        assert_eq!(p.shared.headroom_stops, 4.0);
        assert_eq!(p.target.peak, DisplayPeak::SDR);
        let err = parse(r#"{"recipe_version": 2, "fit_range": {"peak": 4.9}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("peak"), "{err}");
    }

    #[test]
    fn a_key_both_chains_retired_points_at_the_new_home() {
        // The current chain's migration errors point at its own homes
        // (`reconstruction.density.scale`), so the new chain needs its own wording
        // rather than an opaque "unknown field".
        for (json, needles) in [
            (
                r#"{"recipe_version": 2, "density": {"scale": [1, 1, 1]}}"#,
                &["`density`", "`reconstruction.scale`"][..],
            ),
            (
                r#"{"recipe_version": 2, "film_base": {"source": "auto"}}"#,
                &["`film_base`", "`calibration.film_base`"],
            ),
            (
                r#"{"recipe_version": 2, "algorithm": "density"}"#,
                &["`algorithm`"],
            ),
            (r#"{"recipe_version": 2, "sigmoid": {}}"#, &["`sigmoid`"]),
            (r#"{"recipe_version": 2, "simple": {}}"#, &["`simple`"]),
            (
                r#"{"recipe_version": 2, "input": {"color": "linear"}}"#,
                &["`input.color`", "`input.transfer`"],
            ),
        ] {
            let err = check(json, true).unwrap_err();
            for needle in needles {
                assert!(err.contains(needle), "{json}: {err}");
            }
            // An overlay gets the same diagnosis.
            let overlay = json.replace(r#""recipe_version": 2, "#, "");
            assert!(check(&overlay, false).unwrap_err().contains(needles[0]));
        }
    }

    #[test]
    fn the_projection_carries_the_shared_sections_and_nothing_else() {
        let mut r = Recipe::default();
        r.calibration.film_base = Some(FilmBaseSource::Auto);
        r.measure.inset = 0.1;
        let cfg = r.to_config();
        assert_eq!(cfg.calibration.film_base, Some(FilmBaseSource::Auto));
        assert_eq!(cfg.measure.inset, 0.1);
        assert_eq!(cfg.input, r.input);
        let defaults = ResolvedConfig::default();
        assert_eq!(cfg.reconstruction, defaults.reconstruction);
        assert_eq!(cfg.print, defaults.print);
        assert_eq!(cfg.output, defaults.output);
    }
}
