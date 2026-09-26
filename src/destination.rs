//! Where a new-chain render goes — the destination set (`nf-destinations/preset-set`).
//!
//! **A destination is four separate knobs, not a name per combination** (user,
//! 2026-09-25): the dynamic [`Range`], the [`Transfer`] the samples are stored with,
//! the [`Gamut`] they are rendered into, and the file [`Container`]. Most combinations
//! are not something the code can write, so the set is **one table**, [`ROWS`], and
//! everything that has to agree with it reads it: resolution, the refusals and their
//! remedies, and the container `cli` judges an output path against. A second list
//! anywhere would drift from the first — the `OutputPreset::ALL` lesson.
//!
//! **An axis the user leaves unset is derived from the table** ([`resolve`]), axis by
//! axis in a fixed order: its default when a row consistent with everything decided so
//! far has it, else the one value left, else a refusal naming the choices. So
//! `--transfer pq` alone is an HDR BT.2020 TIFF, and `--gamut bt2020` alone asks which
//! transfer. Derivation ignores whether a row is ready yet, so a command names the same
//! row on every build; a row that is not ready is refused *after* resolution, naming
//! the task it arrives with. The resolved axes are what the report records, so a replay
//! states every one and derives nothing.
//!
//! **Stating a value is not the same as leaving it unset**: `--gamut display-p3` with
//! `--transfer pq` is refused, while `--transfer pq` alone derives BT.2020. That is the
//! point of the derivation — a stated value is what the user asked for, and it is never
//! silently overridden by another axis.
//!
//! `film-master` is not on these axes: it runs no rendering at all, so range, gamut and
//! transfer do not describe it. It is the other arm of the recipe's `output` section
//! ([`OutputSection`]).
//!
//! Outlives `--new-flow`: after `nf-core/default-flip` this is *the* destination set.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::pipeline::fit_gamut::DestinationGamut;
use crate::pipeline::fit_range::DisplayPeak;
use crate::pipeline::hdr::{HdrTransfer, LINEAR_HEADROOM};
use crate::types::Result;

/// One axis of the destination set: its values, its default, and how a message names
/// it. Everything generic over an axis — parsing, serde, derivation, diagnostics — is
/// written once against this.
pub trait Axis: Copy + Eq + fmt::Debug + 'static {
    /// Every value, in help order.
    const ALL: &'static [Self];
    /// The value an unset axis takes when a consistent row has it.
    const DEFAULT: Self;
    /// The command-line flag.
    const FLAG: &'static str;
    /// The recipe key, under `output.display`.
    const KEY: &'static str;
    /// The wire and flag spelling.
    fn name(self) -> &'static str;
    /// This axis's value on a row.
    fn of(row: &Row) -> Self;
    /// This axis's stated value, if any.
    fn stated(axes: &DisplayAxes) -> Option<Self>;
    /// State this axis's value.
    fn set(axes: &mut DisplayAxes, value: Self);
}

/// The recipe keys of `output.display`, in resolution order — one per axis, taken from
/// [`Axis::KEY`] so no second list can drift. A consumer that must tell this object
/// from an externally tagged enum (`cli::merge_json`: both serialize as small objects,
/// and a one-axis `output.display` is a single-key object) reads it.
pub const AXIS_KEYS: [&str; 4] = [Range::KEY, Transfer::KEY, Gamut::KEY, Container::KEY];

/// Parse an axis value. Case-insensitive, like every keyword here. The accepted list is
/// generated from [`Axis::ALL`], so a new value cannot be missing from it.
pub fn parse<A: Axis>(s: &str) -> std::result::Result<A, String> {
    let wanted = s.trim().to_ascii_lowercase();
    A::ALL
        .iter()
        .copied()
        .find(|v| v.name() == wanted)
        .ok_or_else(|| {
            format!(
                "unknown {} value `{}` — accepted: {}",
                A::FLAG,
                s.trim(),
                accepted::<A>()
            )
        })
}

/// An axis's values, comma-separated in help order.
pub fn accepted<A: Axis>() -> String {
    A::ALL
        .iter()
        .map(|v| v.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Serde and clap through [`Axis::ALL`] and [`Axis::name`], so the recipe and the flag
/// accept exactly the same spellings, and neither keeps a list of its own.
macro_rules! axis_serde {
    ($ty:ty) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.name())
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                parse::<$ty>(&s).map_err(serde::de::Error::custom)
            }
        }
        // The flag's accepted values and its `--help` list come from `Axis::ALL` too.
        impl clap::ValueEnum for $ty {
            fn value_variants<'a>() -> &'a [Self] {
                <$ty as Axis>::ALL
            }
            fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
                Some(clap::builder::PossibleValue::new(self.name()))
            }
        }
    };
}

/// The dynamic range the render is fitted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    /// An SDR display: diffuse white is the peak.
    Sdr,
    /// An HDR display peaking at 1000 cd/m² over a 203 cd/m² reference white — the
    /// HDR spike's binding numbers (`docs/spike/hdr-output-spike.md`).
    Hdr,
}

impl Range {
    /// The peak fit range compresses against.
    pub fn peak(self) -> Result<DisplayPeak> {
        match self {
            Range::Sdr => Ok(DisplayPeak::SDR),
            Range::Hdr => DisplayPeak::new(LINEAR_HEADROOM),
        }
    }
}

impl Axis for Range {
    const ALL: &'static [Self] = &[Range::Sdr, Range::Hdr];
    const DEFAULT: Self = Range::Sdr;
    const FLAG: &'static str = "--range";
    const KEY: &'static str = "range";
    fn name(self) -> &'static str {
        match self {
            Range::Sdr => "sdr",
            Range::Hdr => "hdr",
        }
    }
    fn of(row: &Row) -> Self {
        row.range
    }
    fn stated(axes: &DisplayAxes) -> Option<Self> {
        axes.range
    }
    fn set(axes: &mut DisplayAxes, value: Self) {
        axes.range = Some(value);
    }
}
axis_serde!(Range);

/// How the samples are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// The gamut's own display curve: the sRGB piecewise curve for Display P3, the
    /// `563/256` power law for Adobe RGB. A gain-map JPEG's base uses it too.
    Native,
    /// No transfer: display-linear samples, relative to reference white.
    Linear,
    /// Rec.2100 PQ (ST 2084).
    Pq,
    /// Rec.2100 HLG, with the reference 1000-nit display OOTF.
    Hlg,
}

impl Axis for Transfer {
    const ALL: &'static [Self] = &[
        Transfer::Native,
        Transfer::Linear,
        Transfer::Pq,
        Transfer::Hlg,
    ];
    const DEFAULT: Self = Transfer::Native;
    const FLAG: &'static str = "--transfer";
    const KEY: &'static str = "transfer";
    fn name(self) -> &'static str {
        match self {
            Transfer::Native => "native",
            Transfer::Linear => "linear",
            Transfer::Pq => "pq",
            Transfer::Hlg => "hlg",
        }
    }
    fn of(row: &Row) -> Self {
        row.transfer
    }
    fn stated(axes: &DisplayAxes) -> Option<Self> {
        axes.transfer
    }
    fn set(axes: &mut DisplayAxes, value: Self) {
        axes.transfer = Some(value);
    }
}
axis_serde!(Transfer);

/// The primaries the render is mapped into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gamut {
    DisplayP3,
    AdobeRgb,
    Bt2020,
}

impl Gamut {
    /// The gamut fit gamut maps into.
    pub fn destination(self) -> DestinationGamut {
        match self {
            Gamut::DisplayP3 => DestinationGamut::DisplayP3,
            Gamut::AdobeRgb => DestinationGamut::AdobeRgb,
            Gamut::Bt2020 => DestinationGamut::Bt2020,
        }
    }
}

impl Axis for Gamut {
    const ALL: &'static [Self] = &[Gamut::DisplayP3, Gamut::AdobeRgb, Gamut::Bt2020];
    const DEFAULT: Self = Gamut::DisplayP3;
    const FLAG: &'static str = "--gamut";
    const KEY: &'static str = "gamut";
    fn name(self) -> &'static str {
        match self {
            Gamut::DisplayP3 => "display-p3",
            Gamut::AdobeRgb => "adobe-rgb",
            Gamut::Bt2020 => "bt2020",
        }
    }
    fn of(row: &Row) -> Self {
        row.gamut
    }
    fn stated(axes: &DisplayAxes) -> Option<Self> {
        axes.gamut
    }
    fn set(axes: &mut DisplayAxes, value: Self) {
        axes.gamut = Some(value);
    }
}
axis_serde!(Gamut);

/// The file containers Hanten writes. Which spellings a path may state, and which one
/// Hanten supplies when it completes or derives a name, both hang off this — for the
/// current chain's presets (`cli::container_for`) and for this set alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Tiff,
    Jpeg,
    Avif,
}

impl Container {
    /// Every spelling accepted on a *stated* output path, in any case.
    pub fn accepted(self) -> &'static [&'static str] {
        match self {
            Self::Tiff => &["tif", "tiff"],
            Self::Jpeg => &["jpg", "jpeg"],
            Self::Avif => &["avif"],
        }
    }

    /// The one spelling Hanten writes when it supplies the suffix itself — a completed
    /// `convert` path or a derived `roll` name.
    ///
    /// Deliberately **not** `accepted()[0]`: that lists `tif` first, and taking the
    /// head would have renamed every existing roll output from `_positive.tiff`.
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Tiff => "tiff",
            Self::Jpeg => "jpg",
            Self::Avif => "avif",
        }
    }
}

impl Axis for Container {
    const ALL: &'static [Self] = &[Container::Tiff, Container::Jpeg, Container::Avif];
    const DEFAULT: Self = Container::Tiff;
    const FLAG: &'static str = "--container";
    const KEY: &'static str = "container";
    fn name(self) -> &'static str {
        match self {
            Container::Tiff => "tiff",
            Container::Jpeg => "jpeg",
            Container::Avif => "avif",
        }
    }
    fn of(row: &Row) -> Self {
        row.container
    }
    fn stated(axes: &DisplayAxes) -> Option<Self> {
        axes.container
    }
    fn set(axes: &mut DisplayAxes, value: Self) {
        axes.container = Some(value);
    }
}
axis_serde!(Container);

/// What a ready row's encoder writes. The render dispatch matches on this, exhaustively,
/// so a new row cannot reach an encoder it was not written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// One SDR rendition, the gamut's own curve, 16-bit integer TIFF.
    SdrTiff,
    /// One HDR rendition, display-linear, 32-bit float TIFF.
    HdrLinearTiff,
    /// One HDR rendition, a Rec.2100 signal as full-range 16-bit TIFF codes.
    HdrCodedTiff(HdrTransfer),
    /// One HDR rendition, a Rec.2100 signal as 10-bit 4:4:4 AVIF.
    HdrAvif(HdrTransfer),
}

/// Whether the code can write a row yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ready(Encoding),
    /// Planned; `arriving_with` completes "…arrives with {}".
    NotYet {
        arriving_with: &'static str,
    },
}

/// One destination: a combination of the four axes, and whether it can be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub range: Range,
    pub transfer: Transfer,
    pub gamut: Gamut,
    pub container: Container,
    pub status: Status,
}

const fn row(
    range: Range,
    transfer: Transfer,
    gamut: Gamut,
    container: Container,
    status: Status,
) -> Row {
    Row {
        range,
        transfer,
        gamut,
        container,
        status,
    }
}

/// **The destination set.** Every combination not listed is refused.
///
/// Why the gaps: PQ and HLG are Rec.2100 signals, so BT.2020 only; Adobe RGB is an
/// SDR editing space; AVIF is written only for a Rec.2100 signal; a linear float TIFF
/// is the HDR interchange master; a JPEG is 8-bit, so it carries HDR only as a gain map.
pub const ROWS: &[Row] = &[
    row(
        Range::Sdr,
        Transfer::Native,
        Gamut::DisplayP3,
        Container::Tiff,
        Status::Ready(Encoding::SdrTiff),
    ),
    row(
        Range::Sdr,
        Transfer::Native,
        Gamut::AdobeRgb,
        Container::Tiff,
        Status::Ready(Encoding::SdrTiff),
    ),
    row(
        Range::Hdr,
        Transfer::Linear,
        Gamut::Bt2020,
        Container::Tiff,
        Status::Ready(Encoding::HdrLinearTiff),
    ),
    row(
        Range::Hdr,
        Transfer::Pq,
        Gamut::Bt2020,
        Container::Tiff,
        Status::Ready(Encoding::HdrCodedTiff(HdrTransfer::Pq)),
    ),
    row(
        Range::Hdr,
        Transfer::Hlg,
        Gamut::Bt2020,
        Container::Tiff,
        Status::Ready(Encoding::HdrCodedTiff(HdrTransfer::Hlg)),
    ),
    row(
        Range::Hdr,
        Transfer::Pq,
        Gamut::Bt2020,
        Container::Avif,
        Status::Ready(Encoding::HdrAvif(HdrTransfer::Pq)),
    ),
    row(
        Range::Hdr,
        Transfer::Hlg,
        Gamut::Bt2020,
        Container::Avif,
        Status::Ready(Encoding::HdrAvif(HdrTransfer::Hlg)),
    ),
    row(
        Range::Hdr,
        Transfer::Native,
        Gamut::DisplayP3,
        Container::Jpeg,
        Status::NotYet {
            arriving_with: "the gain-map destination, an SDR base with a per-channel \
                            ISO 21496-1 gain map (`nf-destinations/gain-map-destination`)",
        },
    ),
    row(
        Range::Sdr,
        Transfer::Native,
        Gamut::DisplayP3,
        Container::Jpeg,
        Status::NotYet {
            arriving_with: "the SDR JPEG destination (`output/sdr-jpeg-preset`)",
        },
    ),
];

/// The recipe's `output` section: a rendered destination, or the film master.
///
/// One enum rather than a `film_master` bool beside the axes: the two are mutually
/// exclusive, and a bool would let a recipe state both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum OutputSection {
    /// A rendered destination, chosen by its axes.
    Display(DisplayAxes),
    /// The fixed decode's linear ACEScg, unclamped 32-bit float, with no rendering
    /// stage — not scene correction, the look, fit range or fit gamut.
    FilmMaster,
}

impl Default for OutputSection {
    fn default() -> Self {
        OutputSection::Display(DisplayAxes::default())
    }
}

/// The four axes as stated. Each is optional: an unset one is derived ([`resolve`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayAxes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<Transfer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamut: Option<Gamut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

impl DisplayAxes {
    /// Whether `row` agrees with every stated axis.
    fn admits(&self, row: &Row) -> bool {
        admits::<Range>(self, row)
            && admits::<Transfer>(self, row)
            && admits::<Gamut>(self, row)
            && admits::<Container>(self, row)
    }

    /// The stated axes, in resolution order, as `(flag, key, value)`.
    pub fn stated_axes(&self) -> Vec<StatedAxis> {
        let mut out = Vec::new();
        push_stated::<Range>(self, &mut out);
        push_stated::<Transfer>(self, &mut out);
        push_stated::<Gamut>(self, &mut out);
        push_stated::<Container>(self, &mut out);
        out
    }
}

fn admits<A: Axis>(axes: &DisplayAxes, row: &Row) -> bool {
    A::stated(axes).is_none_or(|v| A::of(row) == v)
}

fn push_stated<A: Axis>(axes: &DisplayAxes, out: &mut Vec<StatedAxis>) {
    if let Some(v) = A::stated(axes) {
        out.push(StatedAxis {
            flag: A::FLAG,
            key: A::KEY,
            value: v.name(),
        });
    }
}

/// One stated axis, as a message names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatedAxis {
    pub flag: &'static str,
    pub key: &'static str,
    pub value: &'static str,
}

/// A destination ready to render: its axes and its encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub range: Range,
    pub transfer: Transfer,
    pub gamut: Gamut,
    pub container: Container,
    pub encoding: Encoding,
}

impl Resolved {
    /// The axes, all stated — what the report records and a replay states.
    pub fn axes(&self) -> DisplayAxes {
        DisplayAxes {
            range: Some(self.range),
            transfer: Some(self.transfer),
            gamut: Some(self.gamut),
            container: Some(self.container),
        }
    }
}

/// A destination the axes cannot resolve to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// No row has every stated value. `conflicting` is the smallest set of stated axes
    /// that already has no row (a pair when one exists), and `changes` the single-axis
    /// edits of those that resolve to a ready row, each with any axis it then leaves to
    /// choose. When no single edit works (a third stated axis rules out every fix),
    /// `changes` is empty and `instead` lists the ready destinations sharing the most
    /// stated values, each as the fewest axes that name it on their own.
    Conflict {
        conflicting: Vec<StatedAxis>,
        changes: Vec<Change>,
        instead: Vec<DisplayAxes>,
    },
    /// An unset axis the table cannot decide: its default is not on any consistent row,
    /// and more than one value is. `choices` are the values that lead to a ready row.
    Ambiguous {
        flag: &'static str,
        key: &'static str,
        choices: Vec<&'static str>,
    },
    /// The axes name a row that is planned but not written yet. `adding` lists the ready
    /// destinations that keep every stated axis, each as the fewest axes to add; when
    /// none does, `instead` lists those sharing the most stated values, each as the
    /// fewest axes that name it on their own. Exactly one of the two is non-empty.
    NotYet {
        row: Row,
        arriving_with: &'static str,
        adding: Vec<DisplayAxes>,
        instead: Vec<DisplayAxes>,
    },
}

/// One way out of a [`Fault::Conflict`]: set `flag` to `value`, and then choose
/// `then` (another axis and its choices) when that change leaves one open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub flag: &'static str,
    pub key: &'static str,
    pub value: &'static str,
    pub then: Option<(&'static str, &'static str, Vec<&'static str>)>,
}

/// A row as a complete set of axes.
fn complete(row: &Row) -> DisplayAxes {
    DisplayAxes {
        range: Some(row.range),
        transfer: Some(row.transfer),
        gamut: Some(row.gamut),
        container: Some(row.container),
    }
}

/// Resolve stated axes to a destination.
///
/// Unset axes are derived in the order range, transfer, gamut, container — each from the
/// rows consistent with everything decided before it (see the module docs). A row that
/// is not ready is refused only after resolution, so what a command names does not
/// depend on which rows this build can write.
pub fn resolve(axes: &DisplayAxes) -> std::result::Result<Resolved, Fault> {
    match derive(axes) {
        Derivation::Row(row) => match row.status {
            Status::Ready(encoding) => Ok(Resolved {
                range: row.range,
                transfer: row.transfer,
                gamut: row.gamut,
                container: row.container,
                encoding,
            }),
            Status::NotYet { arriving_with } => {
                let adding: Vec<DisplayAxes> = ROWS
                    .iter()
                    .filter(|r| is_ready(r) && axes.admits(r))
                    .map(|r| fewest(axes, r))
                    .collect();
                let instead = if adding.is_empty() {
                    closest(axes)
                } else {
                    Vec::new()
                };
                Err(Fault::NotYet {
                    row,
                    arriving_with,
                    adding,
                    instead,
                })
            }
        },
        Derivation::NoRow => Err(conflict(axes)),
        Derivation::Open {
            flag,
            key,
            candidates,
        } => Err(Fault::Ambiguous {
            flag,
            key,
            choices: resolving(&candidates),
        }),
    }
}

/// What the table makes of stated axes, before anything is diagnosed.
enum Derivation {
    /// The row they name, ready or not.
    Row(Row),
    /// No row has every stated value.
    NoRow,
    /// An unset axis the table cannot decide. Each candidate is a value it could take,
    /// with the axes as they would be with that value stated.
    Open {
        flag: &'static str,
        key: &'static str,
        candidates: Vec<(&'static str, DisplayAxes)>,
    },
}

fn is_ready(row: &Row) -> bool {
    matches!(row.status, Status::Ready(_))
}

fn derive(axes: &DisplayAxes) -> Derivation {
    let mut rows: Vec<Row> = ROWS.iter().copied().filter(|r| axes.admits(r)).collect();
    if rows.is_empty() {
        return Derivation::NoRow;
    }
    let decided = narrow::<Range>(axes, &mut rows)
        .and_then(|()| narrow::<Transfer>(axes, &mut rows))
        .and_then(|()| narrow::<Gamut>(axes, &mut rows))
        .and_then(|()| narrow::<Container>(axes, &mut rows));
    if let Err(open) = decided {
        return open;
    }
    match rows.as_slice() {
        [row] => Derivation::Row(*row),
        // Every axis is decided and the table lists no combination twice, which
        // `no_combination_is_listed_twice` holds.
        _ => unreachable!("four decided axes select exactly one row"),
    }
}

/// Decide one unset axis over the rows consistent so far, and keep only those rows.
fn narrow<A: Axis>(axes: &DisplayAxes, rows: &mut Vec<Row>) -> std::result::Result<(), Derivation> {
    if A::stated(axes).is_some() {
        return Ok(());
    }
    let values: Vec<A> = A::ALL
        .iter()
        .copied()
        .filter(|v| rows.iter().any(|r| A::of(r) == *v))
        .collect();
    let chosen = if values.contains(&A::DEFAULT) {
        A::DEFAULT
    } else if let [only] = values.as_slice() {
        *only
    } else {
        return Err(Derivation::Open {
            flag: A::FLAG,
            key: A::KEY,
            candidates: values
                .iter()
                .map(|v| {
                    let mut stated = *axes;
                    A::set(&mut stated, *v);
                    (v.name(), stated)
                })
                .collect(),
        });
    };
    rows.retain(|r| A::of(r) == chosen);
    Ok(())
}

/// Whether stated axes reach a ready destination — directly, or after the user settles
/// each axis the table leaves open. Terminates: every level states one more axis.
fn resolves(axes: &DisplayAxes) -> bool {
    match derive(axes) {
        Derivation::Row(row) => is_ready(&row),
        Derivation::NoRow => false,
        Derivation::Open { candidates, .. } => candidates.iter().any(|(_, a)| resolves(a)),
    }
}

/// The candidates of an open axis that reach a ready destination — the only choices a
/// refusal offers, so every remedy works.
fn resolving(candidates: &[(&'static str, DisplayAxes)]) -> Vec<&'static str> {
    candidates
        .iter()
        .filter(|(_, a)| resolves(a))
        .map(|(name, _)| *name)
        .collect()
}

/// Diagnose stated axes that no row has.
///
/// The most specific diagnosis first: the first **pair** of stated axes (in resolution
/// order) with no row, so a third stated axis that has nothing to do with the conflict
/// is not blamed. Only when every pair has a row and the whole set does not is the set
/// named. The changes offered are single-axis edits of a conflicting axis that resolve
/// with every *other* stated axis kept — so each remedy works as written.
fn conflict(axes: &DisplayAxes) -> Fault {
    let stated = axes.stated_axes();
    let has_row = |subset: &DisplayAxes| ROWS.iter().any(|r| subset.admits(r));
    let mut conflicting = stated.clone();
    'pairs: for (i, a) in stated.iter().enumerate() {
        for b in &stated[i + 1..] {
            if !has_row(&only(axes, &[a.flag, b.flag])) {
                conflicting = vec![*a, *b];
                break 'pairs;
            }
        }
    }
    let mut changes = Vec::new();
    for axis in &conflicting {
        push_changes::<Range>(axes, axis.flag, &mut changes);
        push_changes::<Transfer>(axes, axis.flag, &mut changes);
        push_changes::<Gamut>(axes, axis.flag, &mut changes);
        push_changes::<Container>(axes, axis.flag, &mut changes);
    }
    let instead = if changes.is_empty() {
        closest(axes)
    } else {
        Vec::new()
    };
    Fault::Conflict {
        conflicting,
        changes,
        instead,
    }
}

/// The ready destinations that write `container`, as the axes to state **over** `stated`
/// to reach each — how a message offers a way to write a stated suffix.
///
/// Computed against what the run stated, because a stated axis cannot be unstated by
/// following an offer: a flag overrides a recipe's axis, it never removes it. So each
/// offer names `container`, every stated axis whose value the row needs changed, and the
/// fewest further axes that settle the rest; stated over `stated`, it resolves to that
/// row (`every_container_offer_resolves_over_what_was_stated`). Only the rows needing the
/// fewest stated axes changed are offered. Empty when no ready row writes `container`,
/// so a message offers nothing it cannot deliver.
pub fn writing(container: Container, stated: &DisplayAxes) -> Vec<DisplayAxes> {
    // The stated axes (other than the container) each row would change.
    let changed = |r: &Row| {
        [
            stated.range.is_some_and(|v| v != r.range),
            stated.transfer.is_some_and(|v| v != r.transfer),
            stated.gamut.is_some_and(|v| v != r.gamut),
        ]
        .into_iter()
        .filter(|&b| b)
        .count()
    };
    let rows: Vec<&Row> = ROWS
        .iter()
        .filter(|r| is_ready(r) && r.container == container)
        .collect();
    let fewest_changed = rows.iter().map(|r| changed(r)).min().unwrap_or(0);
    rows.into_iter()
        .filter(|r| changed(r) == fewest_changed)
        .map(|r| {
            // What must be stated: the container and each stated axis the row differs on.
            let change = DisplayAxes {
                range: stated.range.filter(|v| *v != r.range).map(|_| r.range),
                transfer: stated
                    .transfer
                    .filter(|v| *v != r.transfer)
                    .map(|_| r.transfer),
                gamut: stated.gamut.filter(|v| *v != r.gamut).map(|_| r.gamut),
                container: Some(container),
            };
            let base = over(stated, &change);
            let added = fewest(&base, r);
            DisplayAxes {
                range: change.range.or(added.range),
                transfer: change.transfer.or(added.transfer),
                gamut: change.gamut.or(added.gamut),
                container: Some(container),
            }
        })
        .collect()
}

/// `stated` with every axis `flags` states overriding it — how flags apply over a recipe.
pub fn over(stated: &DisplayAxes, flags: &DisplayAxes) -> DisplayAxes {
    DisplayAxes {
        range: flags.range.or(stated.range),
        transfer: flags.transfer.or(stated.transfer),
        gamut: flags.gamut.or(stated.gamut),
        container: flags.container.or(stated.container),
    }
}

/// The ready destinations sharing the most stated values with `axes`, each as the fewest
/// axes that name it alone — the fallback remedy when nothing closer works. Never empty
/// while a row is ready.
fn closest(axes: &DisplayAxes) -> Vec<DisplayAxes> {
    let shared = |r: &Row| {
        [
            axes.range.is_some_and(|v| v == r.range),
            axes.transfer.is_some_and(|v| v == r.transfer),
            axes.gamut.is_some_and(|v| v == r.gamut),
            axes.container.is_some_and(|v| v == r.container),
        ]
        .into_iter()
        .filter(|&b| b)
        .count()
    };
    let ready = ROWS.iter().filter(|r| is_ready(r));
    let most = ready.clone().map(shared).max().unwrap_or(0);
    ready
        .filter(|r| shared(r) == most)
        .map(|r| fewest(&DisplayAxes::default(), r))
        .collect()
}

/// The fewest of `row`'s axes that, added to `base`, resolve to `row` — how a message
/// offers a destination. Smallest sets first, then in axis order (range, transfer,
/// gamut, container): of two sets the same size, the one with the earlier first axis
/// wins. `base` must not contradict the row.
fn fewest(base: &DisplayAxes, row: &Row) -> DisplayAxes {
    let full = complete(row);
    // Bit `i` is axis `i`. Reversed, the earliest axis is the most significant bit, so
    // a larger reversed mask is earlier in axis order — hence the `Reverse`.
    let mut masks: Vec<u8> = (0..16).collect();
    masks.sort_by_key(|m| (m.count_ones(), std::cmp::Reverse(m.reverse_bits())));
    for mask in masks {
        let on = |bit: u8| mask & (1 << bit) != 0;
        let added = DisplayAxes {
            range: full.range.filter(|_| on(0) && base.range.is_none()),
            transfer: full.transfer.filter(|_| on(1) && base.transfer.is_none()),
            gamut: full.gamut.filter(|_| on(2) && base.gamut.is_none()),
            container: full.container.filter(|_| on(3) && base.container.is_none()),
        };
        let with = DisplayAxes {
            range: base.range.or(added.range),
            transfer: base.transfer.or(added.transfer),
            gamut: base.gamut.or(added.gamut),
            container: base.container.or(added.container),
        };
        if matches!(derive(&with), Derivation::Row(r) if r == *row) {
            return added;
        }
    }
    full
}

/// `axes` with only the axes whose flags are listed kept.
fn only(axes: &DisplayAxes, flags: &[&str]) -> DisplayAxes {
    let keep = |flag: &str| flags.contains(&flag);
    DisplayAxes {
        range: axes.range.filter(|_| keep(Range::FLAG)),
        transfer: axes.transfer.filter(|_| keep(Transfer::FLAG)),
        gamut: axes.gamut.filter(|_| keep(Gamut::FLAG)),
        container: axes.container.filter(|_| keep(Container::FLAG)),
    }
}

/// The values of axis `A` (when it is the one named `flag`) that, replacing the stated
/// one, make the axes resolve.
fn push_changes<A: Axis>(axes: &DisplayAxes, flag: &str, out: &mut Vec<Change>) {
    if A::FLAG != flag {
        return;
    }
    for v in A::ALL.iter().copied() {
        if Some(v) == A::stated(axes) {
            continue;
        }
        let mut changed = *axes;
        A::set(&mut changed, v);
        let then = match derive(&changed) {
            Derivation::Row(row) if is_ready(&row) => None,
            Derivation::Open {
                flag,
                key,
                candidates,
            } => match resolving(&candidates) {
                choices if !choices.is_empty() => Some((flag, key, choices)),
                _ => continue,
            },
            _ => continue,
        };
        out.push(Change {
            flag: A::FLAG,
            key: A::KEY,
            value: v.name(),
            then,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn axes(
        range: Option<Range>,
        transfer: Option<Transfer>,
        gamut: Option<Gamut>,
        container: Option<Container>,
    ) -> DisplayAxes {
        DisplayAxes {
            range,
            transfer,
            gamut,
            container,
        }
    }

    fn every_stated_combination() -> Vec<DisplayAxes> {
        let mut out = Vec::new();
        fn opt<A: Axis>() -> Vec<Option<A>> {
            std::iter::once(None)
                .chain(A::ALL.iter().copied().map(Some))
                .collect()
        }
        for r in opt::<Range>() {
            for t in opt::<Transfer>() {
                for g in opt::<Gamut>() {
                    for c in opt::<Container>() {
                        out.push(axes(r, t, g, c));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn no_combination_is_listed_twice() {
        let keys: BTreeSet<_> = ROWS
            .iter()
            .map(|r| {
                (
                    r.range.name(),
                    r.transfer.name(),
                    r.gamut.name(),
                    r.container.name(),
                )
            })
            .collect();
        assert_eq!(keys.len(), ROWS.len());
    }

    #[test]
    fn every_value_of_every_axis_is_on_some_row() {
        // A value no row has could only ever be refused, so it should not be offered.
        fn check<A: Axis>() {
            for v in A::ALL {
                assert!(
                    ROWS.iter().any(|r| A::of(r) == *v),
                    "{} {} is on no row",
                    A::FLAG,
                    v.name()
                );
            }
        }
        check::<Range>();
        check::<Transfer>();
        check::<Gamut>();
        check::<Container>();
    }

    #[test]
    fn nothing_stated_is_the_display_p3_sdr_tiff() {
        // The default destination, so the default render does not move.
        let r = resolve(&DisplayAxes::default()).unwrap();
        assert_eq!(
            (r.range, r.transfer, r.gamut, r.container, r.encoding),
            (
                Range::Sdr,
                Transfer::Native,
                Gamut::DisplayP3,
                Container::Tiff,
                Encoding::SdrTiff
            )
        );
    }

    #[test]
    fn an_unset_axis_takes_the_one_value_left() {
        let r = resolve(&axes(None, Some(Transfer::Pq), None, None)).unwrap();
        assert_eq!(
            (r.range, r.gamut, r.container),
            (Range::Hdr, Gamut::Bt2020, Container::Tiff)
        );
        let r = resolve(&axes(None, None, Some(Gamut::AdobeRgb), None)).unwrap();
        assert_eq!(r.encoding, Encoding::SdrTiff);
        let r = resolve(&axes(None, Some(Transfer::Linear), None, None)).unwrap();
        assert_eq!(r.encoding, Encoding::HdrLinearTiff);
    }

    #[test]
    fn an_open_axis_is_refused_with_only_choices_that_resolve() {
        let err = resolve(&axes(None, None, Some(Gamut::Bt2020), None)).unwrap_err();
        assert_eq!(
            err,
            Fault::Ambiguous {
                flag: "--transfer",
                key: "transfer",
                choices: vec!["linear", "pq", "hlg"],
            }
        );
        let err = resolve(&axes(None, None, None, Some(Container::Avif))).unwrap_err();
        assert!(
            matches!(err, Fault::Ambiguous { flag: "--transfer", ref choices, .. }
            if choices == &vec!["pq", "hlg"])
        );
    }

    #[test]
    fn a_row_not_ready_is_refused_after_resolution_with_what_is_ready() {
        // `--range hdr` alone names the gain map (transfer's default is on its row):
        // derivation does not depend on which rows this build can write.
        let Err(Fault::NotYet {
            row, adding: ready, ..
        }) = resolve(&axes(Some(Range::Hdr), None, None, None))
        else {
            panic!("expected NotYet");
        };
        assert_eq!(row.container, Container::Jpeg);
        assert_eq!(
            ready.len(),
            5,
            "every ready HDR destination is offered: {ready:?}"
        );
        let offered: Vec<_> = ready.iter().map(|a| a.stated_axes()).collect();
        // Each as the fewest flags to add: `--transfer pq` alone, not four axes.
        assert!(
            offered.iter().any(|a| a.len() == 1 && a[0].value == "pq"),
            "{offered:?}"
        );
    }

    #[test]
    fn a_conflict_names_the_pair_not_a_bystander() {
        // `--container tiff` is stated and innocent; the pair is range × gamut.
        let err = resolve(&axes(
            Some(Range::Hdr),
            None,
            Some(Gamut::AdobeRgb),
            Some(Container::Tiff),
        ))
        .unwrap_err();
        let Fault::Conflict { conflicting, .. } = err else {
            panic!("expected Conflict: {err:?}")
        };
        let flags: Vec<_> = conflicting.iter().map(|a| a.flag).collect();
        assert_eq!(flags, ["--range", "--gamut"]);
    }

    #[test]
    fn every_offered_remedy_resolves() {
        // Walk every stated combination: whatever a refusal suggests must work when
        // applied, with the axis it leaves open settled by one of its choices.
        for stated in every_stated_combination() {
            match resolve(&stated) {
                Ok(_) => {}
                Err(Fault::Ambiguous { choices, flag, .. }) => {
                    assert!(!choices.is_empty(), "{stated:?}: no choice offered");
                    for c in choices {
                        let mut s = stated;
                        set_by_flag(&mut s, flag, c);
                        assert!(resolves(&s), "{stated:?}: {flag} {c} does not resolve");
                    }
                }
                Err(Fault::NotYet {
                    adding, instead, ..
                }) => {
                    assert!(adding.is_empty() != instead.is_empty(), "{stated:?}");
                    for add in adding {
                        assert!(
                            resolve(&union(&stated, &add)).is_ok(),
                            "{stated:?}: {add:?}"
                        );
                    }
                    for alone in instead {
                        assert!(resolve(&alone).is_ok(), "{stated:?}: {alone:?}");
                    }
                }
                Err(Fault::Conflict {
                    changes,
                    conflicting,
                    instead,
                }) => {
                    assert!(
                        changes.is_empty() != instead.is_empty(),
                        "{stated:?} {conflicting:?}: exactly one kind of way out"
                    );
                    for axes in instead {
                        assert!(resolve(&axes).is_ok(), "{stated:?}: {axes:?}");
                    }
                    for ch in changes {
                        let mut s = stated;
                        set_by_flag(&mut s, ch.flag, ch.value);
                        match &ch.then {
                            None => assert!(resolve(&s).is_ok(), "{stated:?}: {ch:?}"),
                            Some((flag, _, choices)) => {
                                for c in choices {
                                    let mut s2 = s;
                                    set_by_flag(&mut s2, flag, c);
                                    assert!(resolves(&s2), "{stated:?}: {ch:?} then {c}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn union(a: &DisplayAxes, b: &DisplayAxes) -> DisplayAxes {
        DisplayAxes {
            range: a.range.or(b.range),
            transfer: a.transfer.or(b.transfer),
            gamut: a.gamut.or(b.gamut),
            container: a.container.or(b.container),
        }
    }

    fn set_by_flag(s: &mut DisplayAxes, flag: &str, value: &str) {
        match flag {
            "--range" => Range::set(s, parse(value).unwrap()),
            "--transfer" => Transfer::set(s, parse(value).unwrap()),
            "--gamut" => Gamut::set(s, parse(value).unwrap()),
            "--container" => Container::set(s, parse(value).unwrap()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn every_container_offer_resolves_over_what_was_stated() {
        // What a suffix refusal offers must work when stated on top of what the run
        // already stated — a flag overrides a recipe's axis but cannot remove it.
        for stated in every_stated_combination() {
            for c in Container::ALL.iter().copied() {
                let offers = writing(c, &stated);
                let any_ready = ROWS.iter().any(|r| is_ready(r) && r.container == c);
                assert_eq!(!offers.is_empty(), any_ready, "{c:?} over {stated:?}");
                for offer in offers {
                    let applied = over(&stated, &offer);
                    let got = resolve(&applied)
                        .unwrap_or_else(|f| panic!("{offer:?} over {stated:?}: {f:?}"));
                    assert_eq!(got.container, c, "{offer:?} over {stated:?}");
                }
            }
        }
        // A PQ AVIF asked for as a TIFF: only the container changes.
        let pq_avif = axes(None, Some(Transfer::Pq), None, Some(Container::Avif));
        assert_eq!(
            writing(Container::Tiff, &pq_avif),
            [axes(None, None, None, Some(Container::Tiff))]
        );
        // A stated Adobe RGB gamut cannot reach an AVIF unless the offer restates it.
        let adobe = axes(None, None, Some(Gamut::AdobeRgb), None);
        for offer in writing(Container::Avif, &adobe) {
            assert_eq!(offer.gamut, Some(Gamut::Bt2020), "{offer:?}");
        }
        // No ready row writes a JPEG yet, so nothing is offered.
        assert!(writing(Container::Jpeg, &pq_avif).is_empty());
    }

    #[test]
    fn a_resolved_destination_replays_exactly() {
        // The report records every resolved axis; stating them all must name the same row.
        for stated in every_stated_combination() {
            if let Ok(r) = resolve(&stated) {
                assert_eq!(resolve(&r.axes()), Ok(r));
            }
        }
    }

    #[test]
    fn parse_is_case_insensitive_and_lists_every_value() {
        assert_eq!(parse::<Gamut>(" Display-P3 "), Ok(Gamut::DisplayP3));
        let err = parse::<Gamut>("srgb").unwrap_err();
        for v in Gamut::ALL {
            assert!(err.contains(v.name()), "{err}");
        }
    }

    #[test]
    fn the_recipe_section_round_trips() {
        let section =
            OutputSection::Display(axes(Some(Range::Hdr), Some(Transfer::Pq), None, None));
        let json = serde_json::to_string(&section).unwrap();
        assert_eq!(json, r#"{"display":{"range":"hdr","transfer":"pq"}}"#);
        assert_eq!(
            serde_json::from_str::<OutputSection>(&json).unwrap(),
            section
        );
        assert_eq!(
            serde_json::to_string(&OutputSection::FilmMaster).unwrap(),
            r#""film-master""#
        );
        assert!(serde_json::from_str::<OutputSection>(r#"{"display":{"depth":"u16"}}"#).is_err());
        let err = serde_json::from_str::<OutputSection>(r#"{"display":{"gamut":"srgb"}}"#)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("accepted: display-p3, adobe-rgb, bt2020"),
            "{err}"
        );
    }
}
