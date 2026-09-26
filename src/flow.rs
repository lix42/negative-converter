//! Which rendering chain a run resolves — the `--new-flow` migration selector.
//!
//! **Scaffolding with a written expiry**, not a feature (`docs/nf-migration.md`,
//! `docs/tasks/nf-core/new-flow-flag.md`). The whole file is deleted by
//! `nf-core/default-flip`, when the new chain becomes the only chain and
//! `--new-flow` becomes a removed-flag error on the `--algorithm` precedent.
//!
//! Two things live here, so the migration's surface is one file rather than a
//! scatter of `if` arms across `cli`:
//!
//! - [`Flow`] — the selected chain. An enum rather than a bool because the
//!   orchestrator passes it down to the render seam, and because the flip deletes
//!   a variant rather than inverting a flag.
//! - The **availability tables** — the flags the new flow refuses, and the two
//!   different sentences it refuses them with ([`Availability`]).
//!
//! The knobs the new flow keeps reach the fixed decode through the new chain's own
//! recipe (`crate::recipe`, which outlives this module), and the render itself —
//! decode, `pipeline::chain`, and the destination (`crate::destination`) — is
//! `cli::convert_frame`'s.
//!
//! `Flow` is *orchestration state*, like an unresolved `calibration.film_base`: it
//! never reaches a stage, and it is never a recipe key (`--new-flow` selects which
//! knobs exist; it does not set one). Unlike `--report` / `--telemetry` /
//! `--max-memory`, though, it is **not** in the "can never change a pixel" class —
//! choosing a chain is exactly a choice of pixels, which is why it must stay out
//! of the recipe rather than merely out of the image.

use crate::cli::ConvertArgs;
use crate::types::{NcError, Result};

/// The rendering chain a run resolves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flow {
    /// The shipped chain (reconstruction → print → output). The default until
    /// `nf-core/default-flip`.
    #[default]
    Legacy,
    /// The chain from `docs/design-update.md`, selected by `--new-flow`.
    New,
}

impl Flow {
    /// Resolve the `--new-flow` presence flag.
    pub fn from_flag(new_flow: bool) -> Self {
        if new_flow { Self::New } else { Self::Legacy }
    }
}

/// Why a knob is unavailable — and which of three *different* sentences it earns.
///
/// The distinction is the user's next action, so it is modelled rather than
/// written into each message: "wait for the stage that carries it", "this idea is
/// gone" and "it is spelled differently here" are not the same advice, and a single
/// generic string would have to pick one and be wrong for most of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Availability {
    /// No counterpart **yet**: the capability is planned, in a stage that has not
    /// landed. `arriving_with` completes "…arrives with {}".
    NotYet { arriving_with: &'static str },
    /// No counterpart **ever**: the new design drops the idea. `reason` completes
    /// "…and will not gain one: {}", and `instead` names a replacement when one
    /// exists.
    Never {
        reason: &'static str,
        instead: Option<&'static str>,
    },
    /// The same capability under **another spelling**, set by the task that built its
    /// stage. `to` is the new flag and `why` completes "…is {to}: {}".
    Renamed { to: &'static str, why: &'static str },
}

/// A knob refused by **flag presence**, checked before `merge`.
struct FlagEntry {
    /// The knob as the user typed it.
    knob: &'static str,
    /// The `convert` flags this row classifies, spelled as `--help` spells them.
    ///
    /// Only flags: every *recipe* path is classified by the new chain's own schema
    /// (`crate::recipe`), which refuses a key it does not define at load — by name,
    /// with where it went, when the key belongs to the current chain's recipe. A flag
    /// needs its own row because the flag parser knows nothing of that schema.
    ///
    /// What this field buys is `every_convert_flag_is_classified`, which reads the
    /// flag surface back out of `cli.rs` and fails on a knob no row mentions. Without
    /// it the tables are a list of refusals, in which "considered and kept" and
    /// "nobody looked" are the same state.
    // Read only by that test — the runtime needs the display `knob`, not the ids.
    // Kept beside the row rather than in the test, because a classification that
    // lives apart from the thing it classifies is one that goes stale.
    #[allow(dead_code)]
    covers: &'static [&'static str],
    present: fn(&ConvertArgs) -> bool,
    availability: Availability,
}

/// Knobs with no new-flow meaning, keyed on the **flag the user typed**.
///
/// The presence tiebreaker: reject a flag when it *forces something the
/// branch cannot produce*, and leave an identity value alone — but only where a recipe
/// could have pinned the knob. The new chain's recipe (`crate::recipe`) has no field for
/// any knob refused here, so an identity value has to earn its acceptance on its own:
/// `--density-curve exponential` does (it names the curve the decode already is). When
/// checking which refusals can still fire here, walk the reachable *values*, not the
/// knobs: a refused knob's spared identity value is reachable too, and a `merge` refusal
/// and a row here can otherwise send a user in a circle.
///
/// **How a knob the user never typed is handled**, which this table alone cannot do.
/// The fixed decode reads its own [`DecodeParams`], which is the new chain's recipe
/// section `reconstruction` field for field (`nf-core/recipe-schema`), so a recipe
/// stating the current chain's keys there is refused at load and `--preset` is refused
/// by presence. Nothing the user *asks for* in the new flow's **reconstruction** is
/// silently dropped. The rest of the surface is closed by the same schema: it has no
/// `print` section, and its `output` section is the destination's axes, so a recipe
/// stating a `print` section or the current chain's `output` keys is refused by name,
/// and every flag under them has a row below. The shared sections (`input`,
/// `calibration`'s film base, `measure`) are read.
///
/// [`DecodeParams`]: crate::algo::fixed::DecodeParams
///
/// The ordering face of the same gap is why these are **presence** rules, checked
/// before `merge`: a value rule cannot run until `merge` has resolved a value, so any
/// command line `merge` itself refuses would be diagnosed by the legacy chain first.
///
/// **The inventory is complete**, and `every_convert_flag_is_classified` is what
/// keeps it so: every conversion flag is either refused by a row here, kept by a row
/// in `KEPT_FLAGS`, or named a non-knob — and `crate::recipe`'s tests hold the recipe
/// half, that every section and key of the current chain's recipe is either shared
/// with the new one or refused by name. The reconstruction rows are
/// `nf-reconstruction/fixed-decode`'s, landed with the decode that strands them; the
/// print, output and kept rows are `nf-core/knob-availability-audit`'s. Adding a row
/// changes no message, ordering or call site.
///
/// The one thing the inventory does **not** carry is a renamed-knob mapping table
/// written ahead of the stages. A `NotYet` names the *task* that will carry the knob,
/// not a flag spelling, because the spelling belongs to the task that builds the
/// stage. When that task lands and chooses a new spelling, the row becomes
/// [`Availability::Renamed`] in the same change (`--print-exposure` → `--exposure`),
/// so a rename is only ever stated once the new flag exists.
const FLAG_ENTRIES: &[FlagEntry] = &[
    // --- what the fixed decode strands (`nf-reconstruction/fixed-decode`) ----------
    //
    // These rows are the reconstruction half of `nf-core/knob-availability-audit`,
    // landed with the decode that strands them rather than left accepted-and-ignored
    // in the meantime. That audit has since landed the rest — the `print.*` and
    // `output.*` rows below, `KEPT_FLAGS`, and the exhaustiveness test. It carries no
    // renamed-knob mapping table, deliberately; see the rustdoc above.
    //
    // The decode's *surviving* knobs are deliberately absent from this table and stay
    // reachable: `--density-scale`, `--density-offset`, `--density-gamma` and
    // `--anchor-mid-offset` are exactly the calibration and the anchor
    // `algo::fixed::DecodeParams` carries — the new chain's recipe section
    // `reconstruction`, into which `crate::recipe::merge` writes them and from which
    // the decode reads.
    FlagEntry {
        knob: "--density-curve",
        covers: &["--density-curve"],
        // `--density-curve exponential` names the curve this flow already decodes
        // with, so it forces nothing and stays accepted — the tiebreaker's identity
        // value. Not to preserve a reset: the new chain's recipe has no curve to pin,
        // so there is none to preserve. `characteristic` selects a curve the fixed
        // decode does not have.
        present: |args| {
            matches!(
                args.density_curve,
                Some(crate::types::DensityCurveType::Characteristic)
            )
        },
        availability: Availability::Never {
            reason: "reconstruction is one fixed decode for every negative — a straight \
                     line in density against log exposure — so which curve to use is no \
                     longer a choice the decode offers",
            instead: Some("`--density-curve exponential`, which names what it already does"),
        },
    },
    FlagEntry {
        knob: "--film-stock",
        covers: &["--film-stock"],
        present: |args| args.density.film_stock.is_some(),
        availability: Availability::NotYet {
            // Planned, not scheduled, and it will choose its own spelling:
            // `nf-look/stock-data-home` settled that `--film-stock` leaves with the
            // `characteristic` curve rather than lingering as provenance.
            arriving_with: "an optional per-stock normalization in the look stage, on top \
                            of the fixed decode: inverting each stock's own curve returns \
                            every stock to the same scene contrast, which is a choice about \
                            how the picture should look rather than a decode of what the \
                            negative holds",
        },
    },
    // The retired anchor placements and reference-density flags have no row: they are
    // removed on both chains (`nf-retire/dmax-machinery`), and `reject_removed_flags`
    // refuses them before this table runs. `--anchor-mid-offset` is absent on purpose
    // too: it *is* the rule's `d`. The regional balance's flags have none either, for
    // the same reason (`nf-retire/regional-balance`).
    // A preset sets knobs on **both** sides of the decode/rendering boundary — the
    // curve, `density.scale`, `print_exposure` — so it cannot be
    // resolved against a chain whose rendering knobs do not exist yet. It is also the
    // one conversion flag with no recipe key, which is why it needs a presence row
    // rather than a value one.
    FlagEntry {
        knob: "--preset",
        covers: &["--preset"],
        present: |args| args.preset.is_some(),
        availability: Availability::NotYet {
            arriving_with: "the look stage's presets, which is where a bundle spanning \
                            decode and rendering can be defined again (`nf-look/look-presets`)",
        },
    },
    // --- the print controls (`nf-core/knob-availability-audit`) --------------------
    //
    // The whole `print.*` family, by the same argument the decode settled: a stage
    // that owns its parameters reads no resolved section, so accepting one of these
    // would be accepting-and-ignoring. `pipeline::chain`'s four stages each carry
    // their own `Params`, so no `print` *key* reaches the new flow — which is
    // why the recipe half needs no value rules either: the new chain's recipe has no
    // `print` section, and refuses one by name at load.
    //
    // **Refused by presence, at every value, and the missing section is why.** The
    // tiebreaker would normally leave an identity value alone to keep the flags-win
    // reset usable — `--linear-range 0,1` resolves the documented default and
    // renders byte-identically. But that exemption exists to
    // let a flag clear a value a *recipe* pinned, and the new chain's recipe has no
    // `print` section, so on this flow there is never a print value to
    // reset. With nothing to protect, presence is the honest rule: each of these names
    // an operation whose stage has not landed, or a knob the new chain spells
    // differently (`--print-exposure`). The one print knob the new flow keeps,
    // `--display-tone-headroom`, is in `KEPT_FLAGS`: fit range landed with it.
    // `--display-tone` and `--highlight-compress` are removed flags on both chains
    // (`nf-retire/display-tones`), so they need no row here.
    //
    // Verdicts: `NotYet` for a stage that has not landed, `Renamed` for
    // `--print-exposure`, and `Never` for `--auto-wb` (the per-frame estimate itself
    // retired, not just its spelling — `measure-roll` replaces it).
    FlagEntry {
        knob: "--auto-wb",
        covers: &["--auto-wb"],
        present: |args| args.print.auto_wb.is_some(),
        availability: Availability::Never {
            reason: "a per-frame estimate reads a sunset as the cast and removes it before \
                     highlight desaturation can protect it, so white balance is measured \
                     once per roll (`nf-scene-correction/roll-white-balance`)",
            instead: Some(
                "`hanten measure-roll`, then its gains as `--white-balance` (recipe \
                 `scene_correction.white_balance`)",
            ),
        },
    },
    FlagEntry {
        knob: "--print-exposure",
        covers: &["--print-exposure"],
        present: |args| args.print.print_exposure.is_some(),
        availability: Availability::Renamed {
            to: "`--exposure`",
            why: "the new chain has no print stage — exposure is a scene-referred \
                  correction, applied before the look and the display fit \
                  (recipe `scene_correction.exposure`)",
        },
    },
    FlagEntry {
        // Not simply renamed: today's single linear subtraction is **two** jobs, and
        // the new chain splits them — flare/fog removal on scene-referred values, and
        // display black in fit range. So there is no one knob to point at yet, which
        // is exactly what `NotYet` says and what an `instead` would get wrong.
        knob: "--black-point",
        covers: &["--black-point"],
        present: |args| args.print.black_point.is_some(),
        availability: Availability::NotYet {
            arriving_with: "the two stages that split it — a flare/fog subtraction on \
                            scene-referred values, and display black in fit range; today \
                            it is one subtraction doing both, which is why it is not a \
                            rename (`nf-scene-correction/flare-removal`)",
        },
    },
    FlagEntry {
        knob: "--linear-range",
        covers: &["--linear-range"],
        present: |args| args.print.linear_range.is_some(),
        availability: Availability::NotYet {
            arriving_with: "whichever stage takes the affine levels remap, under whatever \
                            name it takes there — retiring it outright is a listed outcome, \
                            so this is the one print knob that may not come back at all \
                            (`nf-scene-correction/levels-knob`)",
        },
    },
    // --- output (`nf-destinations/preset-set`) -------------------------------------
    //
    // A destination on the new chain is four separate knobs, not a preset name
    // (`crate::destination`), so the preset is refused and its counterpart named —
    // the specific flags for the preset given, when it has one.
    FlagEntry {
        knob: "--output-preset",
        covers: &["--output-preset"],
        present: |args| args.output_opts.output_preset.is_some(),
        availability: Availability::Renamed {
            to: "--range/--transfer/--gamut/--container, or --film-master",
            why: "a destination is separate knobs (recipe `output`), not a name",
        },
    },
    // Operational, and refused because the record names the resolved output preset,
    // the reconstruction and curve, and times the legacy chain's buckets (`algorithm` /
    // `color`) — so under `--new-flow` it would describe a chain the run did not take,
    // and a telemetry record's existence reads as a successful run of what it names.
    FlagEntry {
        knob: "--telemetry / --telemetry-file",
        covers: &["--telemetry", "--telemetry-file"],
        present: |args| args.telemetry || args.telemetry_file.is_some(),
        availability: Availability::NotYet {
            arriving_with: "the new chain's report and telemetry shape, which decides \
                            which stages a record times and what it says ran \
                            (`nf-core/report-contract`)",
        },
    },
];

/// A knob the new flow **reads** — the other half of the inventory.
///
/// `#[cfg(test)]` because nothing at runtime consults it: a kept knob is kept by
/// *not* being refused, so this table's only job is to make
/// [`every_convert_flag_is_classified`] able to fail. It is still the place to read
/// a verdict from — an absence from [`FLAG_ENTRIES`] is not one.
///
/// A refusal table alone cannot say whether a knob was considered, so every
/// conversion flag not in [`FLAG_ENTRIES`] is listed here with the reason it
/// survives. Nothing consults this at runtime: its job is to make
/// [`every_convert_flag_is_classified`] able to fail, and to give the next reader a
/// verdict rather than an absence.
#[cfg(test)]
struct KeptEntry {
    covers: &'static [&'static str],
    /// Why the new flow keeps it. For a knob that survives under a different name,
    /// this says which stage takes it — a rename is not a removal.
    why: &'static str,
}

/// Every conversion flag the new flow accepts, and why.
#[cfg(test)]
const KEPT_FLAGS: &[KeptEntry] = &[
    // Decode and film base are shared: the seam is taken after both, so these reach
    // the new flow unchanged and mean exactly what they mean today.
    KeptEntry {
        covers: &["--input-transfer", "--input-meaning"],
        why: "stage-1b input semantics, which both flows decode through",
    },
    KeptEntry {
        covers: &["--film-type"],
        why: "provenance only since `ir-usability-detection` — it gates nothing on either flow",
    },
    KeptEntry {
        covers: &["--film-base", "--base-region", "--auto-base"],
        why: "the film base is measured before the seam and is shared by both flows",
    },
    KeptEntry {
        covers: &["--export-ir"],
        why: "the IR plane is written from the decoded image after the render, at the \
              destination's depth — u16, or f32 for a float TIFF destination",
    },
    KeptEntry {
        covers: &["--measure-inset"],
        why: "it resolves the reported `effective_area`, which every decoding run \
              reports on either flow, and which `hanten measure-roll` pools a roll's \
              white over",
    },
    // The decode's own knobs — the calibration and the anchor that
    // `algo::fixed::DecodeParams` carries, which is also the new recipe's
    // `reconstruction` section; `crate::recipe::merge` sets each one there.
    KeptEntry {
        covers: &["--density-scale", "--density-offset"],
        why: "the decode's own calibration — `nf-calibration/scale-gamma-loop` owns the \
              values, this gate only keeps them reachable",
    },
    KeptEntry {
        covers: &["--density-gamma"],
        why: "the fixed decode's linearization (recipe `reconstruction.linearization`) — \
              the calibrated half of `gamma`, which `nf-calibration/scale-gamma-loop` tunes \
              with `scale`; print contrast is `--contrast`",
    },
    // Scene correction (`nf-scene-correction/stage`). `--exposure` is the new chain's
    // own spelling; white balance keeps the current chain's, since the knob means the
    // same thing on both — a per-channel gain on linear ACEScg, after the 3×3.
    KeptEntry {
        covers: &["--white-balance"],
        why: "scene correction's white balance (recipe `scene_correction.white_balance`) — \
              stated gains, which `hanten measure-roll` measures once per roll",
    },
    KeptEntry {
        covers: &["--contrast"],
        why: "the look's print contrast (recipe `look.contrast`) — the half of `gamma` \
              `nf-reconstruction/gamma-split` moved out of the decode, new-flow only",
    },
    KeptEntry {
        covers: &["--channel-grade"],
        why: "the look's per-channel grade (recipe `look.channel_grade`), new-flow only",
    },
    KeptEntry {
        covers: &[
            "--highlight-desaturation",
            "--highlight-desaturation-start",
            "--highlight-desaturation-band",
        ],
        why: "the look's highlight desaturation (recipe `look.highlight_desaturation`), \
              new-flow only",
    },
    KeptEntry {
        covers: &["--exposure"],
        why: "scene correction's exposure (recipe `scene_correction.exposure`) — the new \
              chain's spelling of `--print-exposure`",
    },
    // Fit range (`nf-display-stages/fit-range`). The flag keeps its current spelling;
    // renaming it is `nf-retire/print-prefix-rename`'s.
    KeptEntry {
        covers: &["--display-tone-headroom"],
        why: "fit range's headroom (recipe `fit_range.headroom_stops`) — the same key and \
              the same reinhard white point, `2^stops`, the current chain's display tone \
              reads",
    },
    KeptEntry {
        covers: &[
            "--range",
            "--transfer",
            "--gamut",
            "--container",
            "--film-master",
        ],
        why: "the destination (recipe `output`, `crate::destination`) — four separate \
              knobs and the film master, new-flow only",
    },
    KeptEntry {
        covers: &["--anchor-mid-offset"],
        why: "the fixed decode's anchor (recipe `reconstruction.anchor`), `mid-at-base-offset`'s \
              `d`: every conversion knob is a flag and a recipe key, and \
              `nf-look/path-to-white` tunes against it directly",
    },
];

/// Refuse a **flag** the new flow has no meaning for.
///
/// Runs **before `merge`**, and that placement is load-bearing: a presence rule
/// placed after it is unreachable whenever `merge` refuses the same command line
/// first, and the user then gets a remedy pointing at a knob this flow rejects.
pub fn reject_unavailable_flags(flow: Flow, args: &ConvertArgs) -> Result<()> {
    if flow == Flow::Legacy {
        return reject_new_flow_only_flags(args);
    }
    for entry in FLAG_ENTRIES {
        if (entry.present)(args) {
            let err = refusal(entry.knob, entry.availability);
            return Err(match output_preset_counterpart(args) {
                Some(detail) if entry.knob == "--output-preset" => {
                    NcError::Usage(format!("{} {detail}", err.message()))
                }
                _ => err,
            });
        }
    }
    Ok(())
}

/// What the new chain offers for a stated `--output-preset`, as a sentence — `None` when
/// the value is not a preset name (the table's general sentence then stands alone).
fn output_preset_counterpart(args: &ConvertArgs) -> Option<String> {
    let name = args.output_opts.output_preset.as_deref()?;
    let preset = crate::types::OutputPreset::parse(name).ok()?;
    Some(match counterpart(preset) {
        Counterpart::Flags(flags) => format!("For `{}`, pass {flags}.", preset.name()),
        Counterpart::Default => format!(
            "`{}` is the new flow's default destination, so drop the flag.",
            preset.name()
        ),
        Counterpart::NotYet { flags, instead }
        | Counterpart::Unnamed {
            what: flags,
            instead,
        } => {
            format!(
                "`{}`'s counterpart, {flags}, is not written yet; {instead}.",
                preset.name()
            )
        }
    })
}

/// The new chain's counterpart of a current-chain output preset.
#[derive(Debug)]
enum Counterpart {
    /// These flags write the same kind of file.
    Flags(&'static str),
    /// The default destination is the counterpart: no flag needed.
    Default,
    /// Planned but not written yet; `instead` names what is.
    NotYet {
        flags: &'static str,
        instead: &'static str,
    },
    /// Planned, but no axis value names it yet, so `what` is prose rather than flags;
    /// `instead` names what is written. `counterparts_resolve` holds that no axis spells
    /// it, so the variant moves to [`Counterpart::NotYet`] when one does.
    Unnamed {
        what: &'static str,
        instead: &'static str,
    },
}

/// Scaffolding with the rest of this module: the output presets retire with the current
/// chain. `counterparts_resolve` holds each named flag set to a destination that is
/// written, or to one that is refused as not yet.
fn counterpart(preset: crate::types::OutputPreset) -> Counterpart {
    use crate::types::OutputPreset as P;
    match preset {
        P::DisplayP3 => Counterpart::Default,
        P::FilmMaster => Counterpart::Flags("--film-master"),
        P::HdrLinearTiff => Counterpart::Flags("--transfer linear"),
        P::HdrPqTiff => Counterpart::Flags("--transfer pq"),
        P::HdrHlgTiff => Counterpart::Flags("--transfer hlg"),
        P::HdrPq => Counterpart::Flags("--transfer pq --container avif"),
        P::HdrHlg => Counterpart::Flags("--transfer hlg --container avif"),
        P::GainMapHdr | P::UltraHdrV1 => Counterpart::NotYet {
            flags: "--range hdr --container jpeg",
            instead: "the HDR destinations written today are --transfer linear, pq or hlg",
        },
        P::Compatibility => Counterpart::Unnamed {
            what: "an sRGB gamut",
            instead: "the SDR destinations written today are --gamut display-p3 (the \
                      default) and --gamut adobe-rgb",
        },
    }
}

/// A flag only the new chain reads, refused on the current one.
struct NewFlowOnlyEntry {
    /// The flags the row covers, as `convert` spells them.
    #[cfg_attr(not(test), allow(dead_code))]
    // `every_kept_flag_is_read_by_one_chain_or_refused_on_the_other`
    covers: &'static [&'static str],
    present: fn(&ConvertArgs) -> bool,
    message: &'static str,
}

/// Every flag only the new chain reads — the other direction of the
/// accepted-and-ignored hole: the current chain's `merge` has no arm for these, so
/// without a row here they would parse and do nothing.
/// `every_kept_flag_is_read_by_one_chain_or_refused_on_the_other` holds the table
/// complete: every kept flag either moves the current chain's resolved config or has a
/// row here.
const NEW_FLOW_ONLY_FLAGS: &[NewFlowOnlyEntry] = &[
    NewFlowOnlyEntry {
        covers: &[
            "--range",
            "--transfer",
            "--gamut",
            "--container",
            "--film-master",
        ],
        present: |args| args.destination.any(),
        message: "the destination flags (--range, --transfer, --gamut, --container, \
                  --film-master) choose the new chain's destination (recipe `output`) and \
                  have no meaning without `--new-flow`; the current chain's is \
                  --output-preset",
    },
    NewFlowOnlyEntry {
        covers: &["--exposure"],
        present: |args| args.scene.exposure.is_some(),
        message: "--exposure sets the new chain's scene-correction exposure (recipe \
                  `scene_correction.exposure`) and has no meaning without `--new-flow`; \
                  the current chain's exposure is `--print-exposure`",
    },
    NewFlowOnlyEntry {
        covers: &[
            "--contrast",
            "--channel-grade",
            "--highlight-desaturation",
            "--highlight-desaturation-start",
            "--highlight-desaturation-band",
        ],
        present: |args| args.look.any(),
        message: "the look's flags (--contrast, --channel-grade, --highlight-desaturation, \
                  --highlight-desaturation-start, --highlight-desaturation-band) set the \
                  new chain's look (recipe `look`) and have no meaning without \
                  `--new-flow`: the current chain has no look stage. Its contrast is the \
                  whole `--density-gamma`",
    },
];

/// Refuse, on the **current** chain, a flag only the new chain reads
/// ([`NEW_FLOW_ONLY_FLAGS`]).
fn reject_new_flow_only_flags(args: &ConvertArgs) -> Result<()> {
    match NEW_FLOW_ONLY_FLAGS.iter().find(|e| (e.present)(args)) {
        Some(entry) => Err(NcError::Usage(entry.message.into())),
        None => Ok(()),
    }
}

/// The one generic rejection, built in one place so a new table row needs no
/// wording of its own.
fn refusal(knob: &str, availability: Availability) -> NcError {
    // Verdict and remedy are resolved together: "wait for the stage that carries it"
    // and "this idea is gone" call for different next actions, and a remedy that fits
    // only one of them is the circular-advice defect in miniature.
    let (verdict, remedy) = match availability {
        Availability::NotYet { arriving_with } => (
            // No markdown emphasis: this is a terminal string, and asterisks print
            // as asterisks (the same reason the `--new-flow` help text is plain prose).
            format!(
                "the new flow has no counterpart for it yet — one arrives with \
                 {arriving_with}."
            ),
            "Drop it for now".to_string(),
        ),
        Availability::Never { reason, instead } => (
            format!("the new flow has no counterpart for it, and will not gain one: {reason}."),
            instead.map_or_else(|| "Drop it".to_string(), |i| format!("Use {i}")),
        ),
        Availability::Renamed { to, why } => (
            format!("the new flow's counterpart is {to}: {why}."),
            format!("Use {to}"),
        ),
    };
    // The trailing clause says only what this rule inspected. "…the current chain
    // still accepts it" was an unconditional claim about the *legacy* path made by a
    // rule that looked at the new one, and it is false whenever the same command line
    // is independently invalid there (`--density-curve characteristic
    // --anchor-mid-offset 0.6` is refused by the legacy merge).
    // Sending the user to a branch that then refuses them is the circular-advice
    // defect this module's ordering exists to avoid.
    NcError::Usage(format!(
        "{knob} has no meaning under `--new-flow`: {verdict} {remedy}, or run without \
         `--new-flow`, where this knob has a meaning. (`--new-flow` is transitional \
         scaffolding; see docs/nf-migration.md.)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// `convert` flags that are **not** conversion knobs, so the inventory owes them
    /// no row.
    ///
    /// Listed by name rather than skipped by a property — `hide = true` would also
    /// skip a hidden *knob*, and "takes no value" would skip `--auto-base`. Adding a
    /// flag to any of these groups therefore still forces a deliberate choice.
    const NON_KNOB_FLAGS: &[&str] = &[
        // Operational: arg-struct only, never a recipe key. `--dump-params` writes the
        // recipe of whichever chain the run selected — under `--new-flow`, the new
        // chain's document (`crate::recipe`).
        "--dump-params",
        "--strict",
        "--seed",
        // Plumbing: paths and the recipe itself, not settings inside it.
        "--output",
        "--params",
        // The selector. CLI-only for a third reason again — it chooses which knobs
        // exist — so it is not one of them.
        "--new-flow",
        // Removed-flag stubs: hidden args that exist only to emit a migration error on
        // the `--algorithm` precedent. Nothing resolves them, on either flow.
        "--algorithm",
        "--reconstruction",
        "--sigmoid-contrast",
        "--sigmoid-toe",
        "--sigmoid-shoulder",
        "--sigmoid-mid-fraction",
        "--sigmoid-white-at-d-max",
        "--d-max",
        "--fixed-d-max",
        "--auto-d-max",
        "--no-d-max",
        "--anchor-white-at-reference",
        "--anchor-mid-fraction",
        "--anchor-black-floor",
        "--shadow-balance",
        "--highlight-balance",
        "--balance-range",
        "--auto-balance-range",
        "--assume-linear",
        "--input-profile",
        "--invert-white-balance",
        "--clip-low",
        "--clip-high",
        "--output-hdr",
        "--output-sdr",
        "--out-depth",
        "--output-profile",
        "--bigtiff",
        "--display-tone",
        "--highlight-compress",
    ];

    /// Argument groups `ConvertArgs` flattens that carry no conversion knob.
    const NON_KNOB_GROUPS: &[&str] = &["MemoryArgs", "ReportArgs"];

    fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
        let needle = format!("pub struct {name} {{");
        let start = src
            .find(&needle)
            .unwrap_or_else(|| panic!("no struct {name}"))
            + needle.len();
        let end = start
            + src[start..]
                .find("\n}\n")
                .unwrap_or_else(|| panic!("unterminated struct {name}"));
        &src[start..end]
    }

    /// The types `ConvertArgs` pulls in with `#[command(flatten)]`.
    fn flattened_groups(convert: &str) -> Vec<&str> {
        convert
            .match_indices("#[command(flatten)]")
            .map(|(i, _)| {
                let rest = &convert[i..];
                let field = rest.find("pub ").expect("a field after the attribute") + 4;
                let colon = rest[field..].find(':').expect("a typed field") + field + 1;
                let end = rest[colon..].find(',').expect("a terminated field") + colon;
                rest[colon..end].trim()
            })
            .collect()
    }

    /// Every `--long` flag declared in one struct body.
    ///
    /// Reads the declarations back out of the source, the way
    /// `a_boundary_type_can_be_minted_only_by_its_own_stage` reads the boundary types:
    /// the alternative is a hand-kept list of flags sitting beside the flags.
    fn flags_in(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut idx = 0;
        while let Some(rel) = body[idx..].find("#[arg(") {
            let open = idx + rel + "#[arg".len();
            let mut depth = 0usize;
            let mut end = 0usize;
            for (i, ch) in body[open..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            assert!(end > open, "unbalanced #[arg(...)] near byte {open}");
            let attr = &body[open + 1..end];
            let after = &body[end..];
            let name = if let Some(p) = attr.find("long = \"") {
                let rest = &attr[p + "long = \"".len()..];
                rest[..rest.find('"').expect("a closed string")].to_string()
            } else if attr.split(',').any(|t| t.trim() == "long") {
                // `#[arg(long)]` — clap derives the flag from the field name.
                let field = after.find("pub ").expect("a field after the attribute") + 4;
                let colon = after[field..].find(':').expect("a typed field") + field;
                after[field..colon].trim().replace('_', "-")
            } else {
                // A positional, or an attribute carrying only `conflicts_with`/`alias`.
                idx = end;
                continue;
            };
            out.push(format!("--{name}"));
            idx = end;
        }
        out
    }

    /// Every conversion flag `convert` accepts.
    fn convert_flag_surface() -> BTreeSet<String> {
        let src = include_str!("cli.rs");
        let convert = body_of(src, "ConvertArgs");
        let mut flags: BTreeSet<String> = flags_in(convert).into_iter().collect();
        let groups = flattened_groups(convert);
        assert!(
            groups.len() >= 10,
            "ConvertArgs' flattened groups were not found — the parser is reading the \
             wrong thing: {groups:?}"
        );
        for group in groups {
            if NON_KNOB_GROUPS.contains(&group) {
                continue;
            }
            flags.extend(flags_in(body_of(src, group)));
        }
        flags
    }

    /// Every id the inventory classifies, refused or kept.
    fn classified_ids() -> Vec<&'static str> {
        FLAG_ENTRIES
            .iter()
            .flat_map(|e| e.covers)
            .chain(KEPT_FLAGS.iter().flat_map(|e| e.covers))
            .copied()
            .collect()
    }

    #[test]
    fn every_convert_flag_is_classified() {
        // The inventory's whole contract. Without it these tables are a list of
        // refusals, in which a knob nobody considered and a knob deliberately kept are
        // the same state — and the failure the task exists to prevent is exactly that
        // silent one.
        let surface = convert_flag_surface();
        let classified: BTreeSet<&str> = classified_ids().into_iter().collect();

        let unclassified: Vec<&String> = surface
            .iter()
            .filter(|f| !classified.contains(f.as_str()) && !NON_KNOB_FLAGS.contains(&f.as_str()))
            .collect();
        for entry in KEPT_FLAGS {
            assert!(
                !entry.why.is_empty(),
                "{:?} is kept with no recorded reason — an absence is not a verdict",
                entry.covers
            );
        }
        assert!(
            unclassified.is_empty(),
            "unclassified conversion flags — give each a row in FLAG_ENTRIES or \
             KEPT_FLAGS, or a line in NON_KNOB_FLAGS: {unclassified:?}"
        );

        // And the other direction, so a renamed or deleted flag cannot leave a row
        // classifying nothing. A stale row is how an inventory starts lying.
        let dead: Vec<&str> = classified
            .iter()
            .copied()
            .chain(NON_KNOB_FLAGS.iter().copied())
            .filter(|id| !surface.contains(*id))
            .collect();
        assert!(
            dead.is_empty(),
            "rows classify flags that do not exist: {dead:?}"
        );
    }

    #[test]
    fn no_flag_is_classified_twice() {
        // Two rows matching one flag would make the diagnosis depend on row order.
        let ids = classified_ids();
        let mut seen = BTreeSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "{id} is classified by two rows");
        }
        for id in &ids {
            assert!(
                !NON_KNOB_FLAGS.contains(id),
                "{id} is both classified and listed as a non-knob"
            );
        }
    }

    type Landed = fn(&crate::recipe::Recipe) -> bool;

    /// One command line per kept flag, each setting a non-default value, and the one
    /// field of the new chain's recipe it must land in — "the recipe changed" alone
    /// would pass a flag wired to the wrong knob.
    fn kept_flag_samples() -> &'static [(&'static str, &'static [&'static str], Landed)] {
        use crate::algo::fixed::AnchorRule;
        use crate::pipeline::scene_correction::WhiteBalance;
        use crate::types::{FilmBaseSource, FilmType, MeaningAssertion, TransferAssertion};
        &[
            ("--input-transfer", &["--input-transfer", "linear"], |r| {
                r.input.transfer == TransferAssertion::Linear
            }),
            (
                "--input-meaning",
                &["--input-meaning", "scanner-device"],
                |r| r.input.meaning == MeaningAssertion::ScannerDevice,
            ),
            ("--film-type", &["--film-type", "silver"], |r| {
                r.input.film_type == FilmType::Silver
            }),
            ("--film-base", &["--film-base", "0.5,0.4,0.3"], |r| {
                matches!(r.calibration.film_base, Some(FilmBaseSource::Explicit(_)))
            }),
            ("--base-region", &["--base-region", "0,0,10,10"], |r| {
                matches!(r.calibration.film_base, Some(FilmBaseSource::Region(_)))
            }),
            ("--auto-base", &["--auto-base"], |r| {
                r.calibration.film_base == Some(FilmBaseSource::Auto)
            }),
            ("--export-ir", &["--export-ir", "ir.tiff"], |r| {
                r.input.export_ir.as_deref() == Some("ir.tiff")
            }),
            ("--measure-inset", &["--measure-inset", "0.1"], |r| {
                r.measure.inset == 0.1
            }),
            ("--density-scale", &["--density-scale", "1,0.9,0.8"], |r| {
                r.reconstruction.scale == [1.0, 0.9, 0.8]
            }),
            ("--density-offset", &["--density-offset", "0,0.1,0"], |r| {
                r.reconstruction.offset == [0.0, 0.1, 0.0]
            }),
            ("--density-gamma", &["--density-gamma", "1.9"], |r| {
                r.reconstruction.linearization == 1.9
            }),
            (
                "--anchor-mid-offset",
                &["--anchor-mid-offset", "0.5"],
                |r| r.reconstruction.anchor == AnchorRule::MidAboveBase(0.5),
            ),
            ("--white-balance", &["--white-balance", "1.1,1,0.9"], |r| {
                r.scene_correction.white_balance == WhiteBalance::Explicit([1.1, 1.0, 0.9])
            }),
            ("--exposure", &["--exposure", "-0.5"], |r| {
                r.scene_correction.exposure == -0.5
            }),
            ("--contrast", &["--contrast", "1.3"], |r| {
                r.look.contrast == 1.3
            }),
            ("--channel-grade", &["--channel-grade", "1.1,0.9"], |r| {
                r.look.channel_grade == [1.1, 0.9]
            }),
            (
                "--highlight-desaturation",
                &["--highlight-desaturation", "0.5"],
                |r| r.look.highlight_desaturation.strength == 0.5,
            ),
            (
                "--highlight-desaturation-start",
                &["--highlight-desaturation-start", "-2"],
                |r| r.look.highlight_desaturation.start_stops == -2.0,
            ),
            (
                "--highlight-desaturation-band",
                &["--highlight-desaturation-band", "0.01,0.03"],
                |r| r.look.highlight_desaturation.band == [0.01, 0.03],
            ),
            ("--range", &["--range", "hdr"], |r| {
                r.output == display(|a| a.range = Some(crate::destination::Range::Hdr))
            }),
            ("--transfer", &["--transfer", "pq"], |r| {
                r.output == display(|a| a.transfer = Some(crate::destination::Transfer::Pq))
            }),
            ("--gamut", &["--gamut", "adobe-rgb"], |r| {
                r.output == display(|a| a.gamut = Some(crate::destination::Gamut::AdobeRgb))
            }),
            ("--container", &["--container", "avif"], |r| {
                r.output == display(|a| a.container = Some(crate::destination::Container::Avif))
            }),
            ("--film-master", &["--film-master"], |r| {
                r.output == crate::destination::OutputSection::FilmMaster
            }),
            (
                "--display-tone-headroom",
                &["--display-tone-headroom", "4"],
                |r| r.fit_range.headroom_stops == 4.0,
            ),
        ]
    }

    /// A rendered destination with the axes `set` states.
    fn display(set: fn(&mut crate::destination::DisplayAxes)) -> crate::destination::OutputSection {
        let mut axes = crate::destination::DisplayAxes::default();
        set(&mut axes);
        crate::destination::OutputSection::Display(axes)
    }

    /// Parse `convert` with `extra` appended, with or without `--new-flow`.
    fn parse_convert(new_flow: bool, extra: &[&str]) -> ConvertArgs {
        use crate::cli::{Cli, Command};
        use clap::Parser;
        let flag: &[&str] = if new_flow { &["--new-flow"] } else { &[] };
        let argv = ["hanten", "convert", "in.tif", "-o", "out"]
            .iter()
            .chain(flag)
            .chain(extra)
            .copied();
        let Command::Convert(args) = Cli::try_parse_from(argv).unwrap().command else {
            unreachable!()
        };
        args
    }

    /// A kept flag is kept because the new flow *reads* it, so each one must reach the
    /// new chain's recipe — a kept flag with no arm in `recipe::merge` would be the
    /// accepted-and-ignored defect this inventory exists to prevent. Driven over
    /// [`KEPT_FLAGS`] itself, so a row added later without a sample reds.
    #[test]
    fn every_kept_flag_reaches_the_recipe() {
        use crate::recipe::{self, Recipe};
        for entry in KEPT_FLAGS {
            for flag in entry.covers {
                let (_, extra, landed) = kept_flag_samples()
                    .iter()
                    .find(|(f, _, _)| f == flag)
                    .unwrap_or_else(|| panic!("kept flag {flag} has no sample here"));
                let args = parse_convert(true, extra);
                let merged = recipe::merge(Recipe::default(), &args);
                assert!(
                    landed(&merged),
                    "{flag} did not land in its field: {merged:?}"
                );
                // Falsifiability: the default does not already satisfy the check.
                assert!(!landed(&Recipe::default()), "{flag}'s check is vacuous");
            }
        }
    }

    /// The other direction: a kept flag the **current** chain has no arm for would be
    /// accepted and ignored there. So each kept flag either moves the current chain's
    /// resolved config, or has a [`NEW_FLOW_ONLY_FLAGS`] row refusing it without
    /// `--new-flow` — never both, since a flag the current chain reads must not be
    /// refused on it.
    #[test]
    fn every_kept_flag_is_read_by_one_chain_or_refused_on_the_other() {
        use crate::cli::{ResolvedConfig, merge};
        let only: BTreeSet<&str> = NEW_FLOW_ONLY_FLAGS
            .iter()
            .flat_map(|e| e.covers)
            .copied()
            .collect();
        for flag in KEPT_FLAGS.iter().flat_map(|e| e.covers) {
            let (_, extra, _) = kept_flag_samples()
                .iter()
                .find(|(f, _, _)| f == flag)
                .unwrap_or_else(|| panic!("kept flag {flag} has no sample"));
            let args = parse_convert(false, extra);
            let refused = reject_unavailable_flags(Flow::Legacy, &args);
            if only.contains(flag) {
                let err = refused.expect_err(flag);
                assert!(
                    err.message().contains(flag),
                    "{flag}'s refusal does not name it: {}",
                    err.message()
                );
            } else {
                refused.unwrap_or_else(|e| panic!("{flag} refused on the current chain: {e:?}"));
                let merged = merge(ResolvedConfig::default(), &args)
                    .unwrap_or_else(|e| panic!("{flag}: {}", e.message()));
                assert_ne!(
                    merged,
                    ResolvedConfig::default(),
                    "{flag} moves nothing on the current chain and has no \
                     NEW_FLOW_ONLY_FLAGS row: it would be accepted and ignored there"
                );
            }
        }
        for flag in &only {
            assert!(
                KEPT_FLAGS.iter().any(|e| e.covers.contains(flag)),
                "{flag} is refused on the current chain but is not a kept new-flow flag"
            );
        }
    }

    #[test]
    fn the_two_verdicts_read_differently() {
        // The audit's open question is which sentence a knob gets, so the two must be
        // distinguishable in the output rather than only in the type.
        let not_yet = refusal("--knob", Availability::NotYet { arriving_with: "X" });
        let never = refusal(
            "--knob",
            Availability::Never {
                reason: "Y",
                instead: None,
            },
        );
        assert!(not_yet.message().contains("no counterpart for it yet"));
        assert!(!not_yet.message().contains("will not gain one"));
        assert!(never.message().contains("will not gain one"));
        assert!(!never.message().contains("no counterpart for it yet"));
        // Terminal output, so no markdown survives into either sentence.
        assert!(!not_yet.message().contains('*') && !never.message().contains('*'));
    }

    #[test]
    fn no_table_lists_one_knob_twice() {
        // Two rows matching one knob would make the diagnosis depend on row order.
        let knobs: Vec<&str> = FLAG_ENTRIES.iter().map(|e| e.knob).collect();
        for knob in &knobs {
            assert!(!knob.is_empty(), "FLAG_ENTRIES has an unnamed row");
        }
        let mut sorted = knobs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), knobs.len(), "duplicate knob in FLAG_ENTRIES");
    }

    /// A counterpart's flags as the destination they state: `--film-master`, or axes.
    fn stated(flags: &str) -> Option<crate::destination::DisplayAxes> {
        use crate::destination::{Container, Gamut, Range, Transfer, parse};
        let words: Vec<&str> = flags.split_whitespace().collect();
        if words == ["--film-master"] {
            return None;
        }
        let mut axes = crate::destination::DisplayAxes::default();
        for pair in words.chunks(2) {
            let [flag, value] = pair else {
                panic!("`{flags}`: a flag without a value")
            };
            match *flag {
                "--range" => axes.range = Some(parse::<Range>(value).unwrap()),
                "--transfer" => axes.transfer = Some(parse::<Transfer>(value).unwrap()),
                "--gamut" => axes.gamut = Some(parse::<Gamut>(value).unwrap()),
                "--container" => axes.container = Some(parse::<Container>(value).unwrap()),
                other => panic!("`{flags}`: {other} is not a destination flag"),
            }
        }
        Some(axes)
    }

    #[test]
    fn counterparts_resolve() {
        use crate::destination::{Fault, Gamut, parse, resolve};
        for preset in crate::types::OutputPreset::ALL {
            match counterpart(preset) {
                Counterpart::Default => {
                    resolve(&Default::default()).unwrap();
                }
                Counterpart::Flags(flags) => {
                    if let Some(axes) = stated(flags) {
                        let r = resolve(&axes);
                        assert!(r.is_ok(), "{preset:?}: `{flags}` must be written: {r:?}");
                    }
                }
                Counterpart::NotYet { flags, .. } => {
                    let axes = stated(flags).expect("the film master is written");
                    let r = resolve(&axes);
                    assert!(
                        matches!(r, Err(Fault::NotYet { .. })),
                        "{preset:?}: `{flags}` must name a planned row: {r:?}"
                    );
                }
                // Prose because no axis spells it: when a gamut does, this fails and
                // the counterpart becomes flags.
                Counterpart::Unnamed { what, .. } => {
                    assert_eq!(what, "an sRGB gamut", "{preset:?}");
                    assert!(parse::<Gamut>("srgb").is_err(), "{preset:?}");
                }
            }
        }
    }
}
