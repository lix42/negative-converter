//! Input semantic resolution — transfer encoding and measurement meaning as two
//! **independent** axes, resolved before Dmin/density.
//!
//! `io::decode` parses the container and reports raw [`ContainerColorFacts`] (it
//! decides no semantics). [`resolve`] is a pure, table-testable function that
//! turns those facts plus the recipe/CLI assertions into an
//! [`InputColorMetadata`] carrying per-axis [`InputEvidence`]. It never applies a
//! transfer curve or a color transform: Step-1 SilverFast samples are already
//! linear scanner measurements and pass through untouched — the resolver only
//! *decides and reports* what they are. [`require_convertible`] is the `convert`
//! gate: only a supported **linear** transfer paired with **scanner-device**
//! meaning may enter the density pipeline; everything else is a loud
//! unsupported/ambiguous-input error there, while `inspect` still reports the
//! evidence so the file stays diagnosable.
//!
//! Evidence handling is deterministic and has two parts (it is *not* a single
//! ladder that ranks structure below an assertion — see design-spec §9):
//! 1. **override chain** for descriptive evidence: assertion > descriptive tag >
//!    absence-of-evidence default (an assertion displaces a descriptive tag,
//!    recording it as displaced; nothing displaces it downward).
//! 2. **authoritative structure is not overridable**: an explicit assertion that
//!    *contradicts* authoritative container structure **fails** rather than
//!    winning over it (it can only agree with structure or fail against it), and
//!    a descriptive tag that contradicts structure makes the axis ambiguous
//!    (`Unknown`) rather than letting structure silently win.
//!
//! Gamma establishes only the transfer axis; an embedded ICC is
//! device-characterization metadata that does not by itself establish either axis
//! and is never applied before density.

use serde::Serialize;

use crate::types::{GammaFact, MeaningAssertion, NcError, Result, TransferAssertion};

/// Tolerance for treating a descriptive gamma tag as "linear" (`γ ≈ 1`).
const GAMMA_LINEAR_TOL: f64 = 1e-3;

// ---------------------------------------------------------------------------
// Raw container facts (input to the resolver)
// ---------------------------------------------------------------------------

/// Raw, color-relevant facts a container decoder extracted, with **no semantics
/// decided yet**. `io::decode` fills this from the TIFF; [`resolve`] interprets
/// it. Kept separate from the resolved [`InputColorMetadata`] so decode never
/// decides meaning — it only reports what it parsed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerColorFacts {
    /// Authoritative raw-mode structural evidence, when the container establishes
    /// it. `Some` for a validated SilverFast HDR/HDRi scan (separate linear 16-bit
    /// RGB / IR planes) — positive raw-mode provenance for *both* a linear
    /// transfer and scanner-device meaning. `None` when the container does not
    /// structurally prove raw mode.
    pub raw_mode: Option<RawMode>,
    /// Descriptive transfer/gamma evidence parsed from metadata ([`GammaFact`]).
    /// **Descriptive only** — it ranks below structural evidence and below an
    /// explicit assertion. SilverFast surfaces its `Silverfast:Gamma` here; a
    /// value that agrees with raw-mode linear corroborates, one that contradicts
    /// it (or is present-but-uninterpretable) makes the transfer ambiguous.
    pub gamma: GammaFact,
    /// Embedded ICC profile bytes (TIFF tag 34675), retained verbatim for
    /// inspection. Its presence is device-characterization metadata; it does not
    /// by itself establish scanner-device or colorimetric meaning and is never
    /// applied before density in Step 1.
    pub embedded_icc: Option<Vec<u8>>,
}

/// Authoritative raw-mode structural evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawMode {
    /// SilverFast HDR/HDRi raw mode: linear 16-bit scanner samples in separate
    /// RGB (+IR) IFDs, validated by `io::decode`.
    SilverFastHdr,
}

impl RawMode {
    /// Human-readable container label for evidence/error messages.
    fn label(self) -> &'static str {
        match self {
            RawMode::SilverFastHdr => "SilverFast HDR/HDRi raw mode",
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved axes
// ---------------------------------------------------------------------------

/// Resolved transfer encoding of the input samples (one of the two independent
/// axes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferDescription {
    /// A supported linear transfer — samples are (or are asserted) linear; no
    /// inverse-transfer decoding is required to reach density.
    Linear,
    /// Could not be established (absent, ambiguous, or contradicted). `convert`
    /// rejects; `inspect` still reports the evidence.
    Unknown,
}

/// Resolved measurement meaning of the pixel axes (the second independent axis).
///
/// **Wire shape:** serializes as a flat kebab-case **string**
/// (`"scanner-device"` / `"colorimetric"` / `"unknown"`) via a custom
/// [`Serialize`] — so a consumer can always key `meaning` as a string, never a
/// sometimes-object. The `Colorimetric` reference detail is reported in a sibling
/// field ([`InputColorReport::meaning_reference`]), not nested inside `meaning`.
#[derive(Clone, Debug, PartialEq)]
pub enum MeasurementMeaning {
    /// Scanner-device measurements — the only meaning Step-1 density conversion
    /// consumes without a source→working color transform.
    ScannerDevice,
    /// Colorimetric RGB in some reference. Recognized but unsupported: no inverse
    /// transfer / reconstruction path exists yet, so `convert` rejects it.
    Colorimetric { reference: ColorReference },
    /// Could not be established. `convert` rejects; `inspect` still reports.
    Unknown,
}

impl Serialize for MeasurementMeaning {
    /// Emit a flat kebab-case string (never an object), so `meaning` has one
    /// homogeneous wire type. The colorimetric `reference` rides in a sibling
    /// report field instead of nesting under `meaning`.
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(meaning_label(self))
    }
}

/// What a [`MeasurementMeaning::Colorimetric`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorReference {
    /// Asserted by the user (`--input-meaning colorimetric`); nc does not know the
    /// concrete space and has no supported reconstruction for it.
    Asserted,
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// Which axis a piece of evidence bears on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputAxis {
    Transfer,
    Meaning,
}

/// The kind/origin of an evidence record — a label, *not* a total order the
/// resolver blindly maximizes (no `Ord` is derived; a naive "higher wins" ranking
/// would encode a false rule). The two-part logic (see [`resolve`] and
/// design-spec §9): an **override chain** `UserAssertion > Descriptive > Default`
/// (an assertion displaces a descriptive tag, recording it as displaced), and
/// **authoritative `Structural` is not overridable** — an assertion that
/// contradicts it *fails* rather than winning, and a `Descriptive` tag that
/// contradicts it yields `Unknown` rather than letting structure silently win. An
/// `EmbeddedIcc` is informational and establishes no axis on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceKind {
    /// Explicit CLI/recipe assertion — top of the override chain over descriptive
    /// evidence, but it cannot override authoritative structure (it fails against
    /// a contradiction instead).
    UserAssertion,
    /// Authoritative container structure (raw-mode provenance). Not overridable.
    Structural,
    /// Descriptive metadata tag (e.g. a gamma tag).
    Descriptive,
    /// Presence of an embedded ICC profile (device characterization; informational).
    EmbeddedIcc,
    /// Absence of any establishing evidence (the resolved-`Unknown` default).
    Default,
}

/// One evidence record explaining part of an axis resolution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InputEvidence {
    pub axis: InputAxis,
    pub kind: EvidenceKind,
    /// Human-readable finding.
    pub detail: String,
    /// For a user assertion, the knob it came from (`--input-transfer` (CLI) /
    /// `input.meaning` (recipe) …) — the override provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// Parsed metadata this record displaced — an explicit assertion overriding a
    /// descriptive tag records the tag here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displaced: Option<String>,
}

impl InputEvidence {
    fn new(axis: InputAxis, kind: EvidenceKind, detail: impl Into<String>) -> Self {
        Self {
            axis,
            kind,
            detail: detail.into(),
            provenance: None,
            displaced: None,
        }
    }

    fn with_provenance(mut self, provenance: String) -> Self {
        self.provenance = Some(provenance);
        self
    }

    fn with_displaced(mut self, displaced: Option<String>) -> Self {
        self.displaced = displaced;
        self
    }
}

// ---------------------------------------------------------------------------
// Assertions (the merged recipe/CLI knobs, with CLI-vs-recipe provenance)
// ---------------------------------------------------------------------------

/// The resolved input assertions handed to [`resolve`]. The values are the merged
/// recipe/CLI knobs ([`crate::types::InputParams`]); `*_from_cli` records whether
/// a non-`auto` value arrived via a CLI flag (vs the recipe), so provenance in
/// the evidence is literally "CLI" vs "recipe". `merge` applies flags-win
/// precedence before this; here the value is already final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputAssertions {
    pub transfer: TransferAssertion,
    pub meaning: MeaningAssertion,
    pub transfer_from_cli: bool,
    pub meaning_from_cli: bool,
}

impl InputAssertions {
    /// The intrinsic (no-assertion) resolution — both axes `auto`. Used by
    /// `inspect`, which reports the file's own evidence without user assertions.
    pub fn auto() -> Self {
        Self {
            transfer: TransferAssertion::Auto,
            meaning: MeaningAssertion::Auto,
            transfer_from_cli: false,
            meaning_from_cli: false,
        }
    }
}

/// Provenance label for an explicit assertion — literal CLI vs recipe origin.
fn assertion_provenance(axis: &str, from_cli: bool) -> String {
    if from_cli {
        format!("--input-{axis} (CLI flag)")
    } else {
        format!("input.{axis} (recipe)")
    }
}

// ---------------------------------------------------------------------------
// Resolved metadata
// ---------------------------------------------------------------------------

/// The resolved input color description shared by decode, inspect, and convert.
/// Mirrors the task's `InputColorMetadata`: the two independent resolved axes,
/// the retained embedded ICC bytes, and the evidence trail.
#[derive(Clone, Debug, PartialEq)]
pub struct InputColorMetadata {
    pub transfer: TransferDescription,
    pub meaning: MeasurementMeaning,
    /// Embedded ICC bytes, retained for inspection. Never serialized as bytes
    /// (summarized separately via [`summarize_icc`]); carried through untouched.
    pub embedded_icc: Option<Vec<u8>>,
    pub evidence: Vec<InputEvidence>,
}

// ---------------------------------------------------------------------------
// Resolver (pure)
// ---------------------------------------------------------------------------

/// Resolve the two input axes from container facts + user assertions.
///
/// Pure and table-testable. Returns `Err` **only** for a usage error the user can
/// fix — an explicit assertion that contradicts authoritative container structure
/// (e.g. `--input-meaning colorimetric` on raw-mode scanner data). Ambiguity is
/// *not* an error here: it resolves to `Unknown` so `inspect` can report it; the
/// `convert` gate ([`require_convertible`]) is what rejects `Unknown`.
pub fn resolve(facts: &ContainerColorFacts, a: &InputAssertions) -> Result<InputColorMetadata> {
    let mut evidence = Vec::new();
    let transfer = resolve_transfer(facts, a, &mut evidence);
    let meaning = resolve_meaning(facts, a, &mut evidence)?;

    // Embedded ICC: informational device-characterization evidence on the meaning
    // axis. It establishes no axis and is never applied before density.
    if facts.embedded_icc.is_some() {
        evidence.push(InputEvidence::new(
            InputAxis::Meaning,
            EvidenceKind::EmbeddedIcc,
            "embedded ICC profile present (scanner device characterization); retained \
             for inspection, not applied before density and not sufficient on its own \
             to establish measurement meaning",
        ));
    }

    Ok(InputColorMetadata {
        transfer,
        meaning,
        embedded_icc: facts.embedded_icc.clone(),
        evidence,
    })
}

/// Classify a descriptive gamma tag as linear (`γ ≈ 1`) or not.
fn gamma_is_linear(g: f64) -> bool {
    (g - 1.0).abs() <= GAMMA_LINEAR_TOL
}

/// The descriptive-gamma evidence, reduced to what the transfer axis cares about.
enum DescriptiveGamma {
    /// No gamma tag.
    Absent,
    /// A gamma tag that reads as linear (`γ ≈ 1`).
    Linear(f64),
    /// A gamma tag that reads as non-linear.
    NonLinear(f64),
    /// A gamma tag present but uninterpretable (e.g. locale-formatted) — ambiguous.
    Malformed(String),
}

fn descriptive_gamma(gamma: &GammaFact) -> DescriptiveGamma {
    match gamma {
        GammaFact::Absent => DescriptiveGamma::Absent,
        GammaFact::Value(g) if gamma_is_linear(*g) => DescriptiveGamma::Linear(*g),
        GammaFact::Value(g) => DescriptiveGamma::NonLinear(*g),
        GammaFact::Malformed(raw) => DescriptiveGamma::Malformed(raw.clone()),
    }
}

fn resolve_transfer(
    facts: &ContainerColorFacts,
    a: &InputAssertions,
    ev: &mut Vec<InputEvidence>,
) -> TransferDescription {
    let structural = facts.raw_mode; // structural transfer, when present, is linear
    let descriptive = descriptive_gamma(&facts.gamma);

    match a.transfer {
        // Explicit `linear`. Structural transfer is only ever linear (raw mode) or
        // absent, so a linear assertion never contradicts structure — honor it,
        // recording any contradicting / uninterpretable descriptive gamma as
        // displaced evidence (the user takes responsibility for the transfer).
        TransferAssertion::Linear => {
            let displaced = match &descriptive {
                DescriptiveGamma::NonLinear(g) => {
                    Some(format!("descriptive gamma tag {g} (non-linear)"))
                }
                DescriptiveGamma::Malformed(raw) => {
                    Some(format!("uninterpretable gamma tag {raw:?}"))
                }
                DescriptiveGamma::Absent | DescriptiveGamma::Linear(_) => None,
            };
            ev.push(
                InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::UserAssertion,
                    "transfer asserted linear (no inverse-transfer decoding applied)",
                )
                .with_provenance(assertion_provenance("transfer", a.transfer_from_cli))
                .with_displaced(displaced),
            );
            if let Some(m) = structural {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Structural,
                    format!("{}: raw linear scanner samples (corroborates)", m.label()),
                ));
            }
            TransferDescription::Linear
        }

        TransferAssertion::Auto => match (structural, descriptive) {
            // Authoritative raw-mode linear contradicted by a non-linear gamma tag
            // → ambiguous. Do not silently trust structure: leave it Unknown so
            // `convert` rejects and `inspect` explains. An explicit
            // `--input-transfer linear` resolves it (branch above).
            (Some(m), DescriptiveGamma::NonLinear(g)) => {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Structural,
                    format!("{}: raw linear scanner samples", m.label()),
                ));
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Descriptive,
                    format!(
                        "gamma tag {g} contradicts raw linear semantics — transfer is \
                         ambiguous (assert --input-transfer linear to override the tag)"
                    ),
                ));
                TransferDescription::Unknown
            }
            // A gamma tag present but uninterpretable is ambiguous, not linear —
            // even alongside raw-mode structure. Never resolve linear off a value
            // we could not read (a malformed non-linear gamma must not slip through).
            (structural_mode, DescriptiveGamma::Malformed(raw)) => {
                if let Some(m) = structural_mode {
                    ev.push(InputEvidence::new(
                        InputAxis::Transfer,
                        EvidenceKind::Structural,
                        format!("{}: raw linear scanner samples", m.label()),
                    ));
                }
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Descriptive,
                    format!(
                        "gamma tag {raw:?} is present but uninterpretable — transfer is \
                         ambiguous (assert --input-transfer linear if it is a raw linear scan)"
                    ),
                ));
                TransferDescription::Unknown
            }
            // Structural raw-mode linear (descriptive absent or agreeing).
            (Some(m), maybe_desc) => {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Structural,
                    format!("{}: raw linear scanner samples", m.label()),
                ));
                if let DescriptiveGamma::Linear(g) = maybe_desc {
                    ev.push(InputEvidence::new(
                        InputAxis::Transfer,
                        EvidenceKind::Descriptive,
                        format!("gamma tag {g} agrees (linear)"),
                    ));
                }
                TransferDescription::Linear
            }
            // No structure, descriptive linear gamma only: proves the transfer is
            // linear, but nothing about raw-mode provenance (that is the meaning
            // axis, resolved independently).
            (None, DescriptiveGamma::Linear(g)) => {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Descriptive,
                    format!(
                        "gamma tag {g}: linear transfer (does not prove raw-mode \
                         provenance or measurement meaning)"
                    ),
                ));
                TransferDescription::Linear
            }
            // No structure, descriptive non-linear gamma: an encoded transfer with
            // no supported inverse — unsupported.
            (None, DescriptiveGamma::NonLinear(g)) => {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Descriptive,
                    format!("gamma tag {g}: non-linear transfer, no supported inverse"),
                ));
                TransferDescription::Unknown
            }
            // Nothing establishes the transfer.
            (None, DescriptiveGamma::Absent) => {
                ev.push(InputEvidence::new(
                    InputAxis::Transfer,
                    EvidenceKind::Default,
                    "no transfer evidence (no raw-mode structure, no gamma tag)",
                ));
                TransferDescription::Unknown
            }
        },
    }
}

fn resolve_meaning(
    facts: &ContainerColorFacts,
    a: &InputAssertions,
    ev: &mut Vec<InputEvidence>,
) -> Result<MeasurementMeaning> {
    let structural = facts.raw_mode; // structural meaning, when present, is scanner-device

    match a.meaning {
        // Explicit `scanner-device`. Never contradicts structure (structural
        // meaning is scanner-device or absent) — honor it, recording missing
        // structural corroboration as displaced evidence.
        MeaningAssertion::ScannerDevice => {
            let displaced = structural
                .is_none()
                .then(|| "no structural raw-mode evidence".to_string());
            ev.push(
                InputEvidence::new(
                    InputAxis::Meaning,
                    EvidenceKind::UserAssertion,
                    "measurement meaning asserted scanner-device",
                )
                .with_provenance(assertion_provenance("meaning", a.meaning_from_cli))
                .with_displaced(displaced),
            );
            if let Some(m) = structural {
                ev.push(InputEvidence::new(
                    InputAxis::Meaning,
                    EvidenceKind::Structural,
                    format!("{}: scanner-device measurements (corroborates)", m.label()),
                ));
            }
            Ok(MeasurementMeaning::ScannerDevice)
        }

        // Explicit `colorimetric`. If the container structurally proves
        // scanner-device measurements, the assertion contradicts authoritative
        // structure → fail loudly rather than override it. Otherwise it is
        // honored as a *recognized but unsupported* meaning (the convert gate
        // rejects it; an explicit override cannot make it supported).
        MeaningAssertion::Colorimetric => {
            if let Some(m) = structural {
                return Err(NcError::Usage(format!(
                    "--input-meaning colorimetric contradicts authoritative {} \
                     (scanner-device measurements); an explicit assertion cannot override \
                     container structure",
                    m.label()
                )));
            }
            ev.push(
                InputEvidence::new(
                    InputAxis::Meaning,
                    EvidenceKind::UserAssertion,
                    "measurement meaning asserted colorimetric (recognized but unsupported: \
                     no inverse-transfer/reconstruction path exists)",
                )
                .with_provenance(assertion_provenance("meaning", a.meaning_from_cli)),
            );
            Ok(MeasurementMeaning::Colorimetric {
                reference: ColorReference::Asserted,
            })
        }

        MeaningAssertion::Auto => match structural {
            Some(m) => {
                ev.push(InputEvidence::new(
                    InputAxis::Meaning,
                    EvidenceKind::Structural,
                    format!("{}: scanner-device measurements", m.label()),
                ));
                Ok(MeasurementMeaning::ScannerDevice)
            }
            None => {
                ev.push(InputEvidence::new(
                    InputAxis::Meaning,
                    EvidenceKind::Default,
                    "no raw-mode structure to establish measurement meaning (an embedded \
                     ICC, if any, does not establish it)",
                ));
                Ok(MeasurementMeaning::Unknown)
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Convert gate
// ---------------------------------------------------------------------------

/// The `convert`/`roll` gate: only a supported **linear** transfer paired with
/// **scanner-device** meaning may enter Dmin/density without a source→working
/// color transform. Anything else is a loud unsupported/ambiguous-input error
/// (exit 4). `inspect` never calls this — it reports the evidence regardless.
pub fn require_convertible(m: &InputColorMetadata) -> Result<()> {
    match (m.transfer, &m.meaning) {
        (TransferDescription::Linear, MeasurementMeaning::ScannerDevice) => Ok(()),
        _ => Err(NcError::Unsupported(format!(
            "input is not a supported linear scanner-device negative (resolved \
             transfer={}, meaning={}). Only scanner-device measurements with a supported \
             linear transfer enter density; colorimetric/encoded negatives and ambiguous \
             inputs are unsupported. Run `nc inspect` to see the per-axis evidence. If you \
             know this is a raw linear scanner scan, assert it explicitly with \
             `--input-transfer linear --input-meaning scanner-device`.",
            transfer_label(m.transfer),
            meaning_label(&m.meaning),
        ))),
    }
}

fn transfer_label(t: TransferDescription) -> &'static str {
    match t {
        TransferDescription::Linear => "linear",
        TransferDescription::Unknown => "unknown",
    }
}

fn meaning_label(m: &MeasurementMeaning) -> &'static str {
    match m {
        MeasurementMeaning::ScannerDevice => "scanner-device",
        MeasurementMeaning::Colorimetric { .. } => "colorimetric",
        MeasurementMeaning::Unknown => "unknown",
    }
}

// ---------------------------------------------------------------------------
// ICC summary + report view (safe fields only, never raw bytes)
// ---------------------------------------------------------------------------

/// A safe, human-readable summary of an embedded ICC profile — class, color
/// space, PCS, version, and description — **never the raw bytes**. Built via
/// lcms2. Deterministic given the same profile.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IccSummary {
    /// Byte length of the embedded profile.
    pub size_bytes: usize,
    /// Profile/device class (e.g. `InputClass` for a scanner `scnr` profile).
    pub class: String,
    /// Data color space (e.g. `RgbData`).
    pub color_space: String,
    /// Profile connection space (e.g. `XYZData` / `LabData`).
    pub pcs: String,
    /// ICC version (e.g. `2.4`).
    pub version: String,
    /// Profile description tag, when parsable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Summarize embedded ICC bytes into safe fields, or `None` when the bytes don't
/// parse as an ICC profile (the caller still reports `icc_embedded: true`).
pub fn summarize_icc(bytes: &[u8]) -> Option<IccSummary> {
    let profile = lcms2::Profile::new_icc(bytes).ok()?;
    Some(IccSummary {
        size_bytes: bytes.len(),
        class: format!("{:?}", profile.device_class()),
        color_space: format!("{:?}", profile.color_space()),
        pcs: format!("{:?}", profile.pcs()),
        version: format!("{:.1}", profile.version()),
        description: profile.info(lcms2::InfoType::Description, lcms2::Locale::none()),
    })
}

/// Serialize-only view of the resolved input color for the JSON report / inspect,
/// with a safe ICC summary instead of raw profile bytes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InputColorReport {
    /// Resolved transfer encoding, with evidence in `evidence`.
    pub transfer: TransferDescription,
    /// Resolved measurement meaning (a flat string), with evidence in `evidence`.
    pub meaning: MeasurementMeaning,
    /// For a `colorimetric` meaning, what it refers to — the detail kept out of
    /// the flat `meaning` string so `meaning` stays a homogeneous string type.
    /// `None`/omitted for scanner-device / unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning_reference: Option<ColorReference>,
    /// Whether an embedded ICC profile is present in the container.
    pub icc_embedded: bool,
    /// Safe ICC summary (class/space/PCS/version/description), when parsable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icc: Option<IccSummary>,
    /// Whether any inverse-transfer decoding was performed to reach density.
    /// Always `false` in Step 1 — only already-linear samples are accepted, so no
    /// transfer curve is ever applied.
    pub transfer_decoded: bool,
    /// Per-axis evidence for the resolution.
    pub evidence: Vec<InputEvidence>,
}

impl InputColorReport {
    /// Build the report view from resolved metadata, summarizing the embedded ICC
    /// (if any) into safe fields.
    pub fn from_metadata(m: &InputColorMetadata) -> Self {
        let icc = m.embedded_icc.as_deref().and_then(summarize_icc);
        let meaning_reference = match &m.meaning {
            MeasurementMeaning::Colorimetric { reference } => Some(*reference),
            _ => None,
        };
        Self {
            transfer: m.transfer,
            meaning: m.meaning.clone(),
            meaning_reference,
            icc_embedded: m.embedded_icc.is_some(),
            icc,
            transfer_decoded: false,
            evidence: m.evidence.clone(),
        }
    }

    /// Whether an embedded ICC is present but could not be summarized — the caller
    /// surfaces this as a (non-fatal) warning.
    pub fn icc_unparsable(&self) -> bool {
        self.icc_embedded && self.icc.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(raw: bool, gamma: Option<f64>, icc: Option<Vec<u8>>) -> ContainerColorFacts {
        facts_gamma(
            raw,
            match gamma {
                Some(g) => GammaFact::Value(g),
                None => GammaFact::Absent,
            },
            icc,
        )
    }

    fn facts_gamma(raw: bool, gamma: GammaFact, icc: Option<Vec<u8>>) -> ContainerColorFacts {
        ContainerColorFacts {
            raw_mode: raw.then_some(RawMode::SilverFastHdr),
            gamma,
            embedded_icc: icc,
        }
    }

    fn assertions(t: TransferAssertion, m: MeaningAssertion) -> InputAssertions {
        InputAssertions {
            transfer: t,
            meaning: m,
            transfer_from_cli: false,
            meaning_from_cli: false,
        }
    }

    // --- allowed / forbidden transfer × meaning combinations -----------------

    #[test]
    fn silverfast_raw_auto_is_linear_scanner_and_convertible() {
        let m = resolve(&facts(true, None, None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Linear);
        assert_eq!(m.meaning, MeasurementMeaning::ScannerDevice);
        assert!(require_convertible(&m).is_ok());
        // Both axes resolved from independent structural evidence.
        assert!(
            m.evidence
                .iter()
                .any(|e| e.axis == InputAxis::Transfer && e.kind == EvidenceKind::Structural)
        );
        assert!(
            m.evidence
                .iter()
                .any(|e| e.axis == InputAxis::Meaning && e.kind == EvidenceKind::Structural)
        );
    }

    #[test]
    fn gamma_one_without_raw_evidence_stays_unknown_meaning() {
        // Gamma 1 proves ONLY the transfer axis; meaning stays Unknown without
        // raw-mode evidence — it must NOT be treated as scanner measurements.
        let m = resolve(&facts(false, Some(1.0), None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Linear);
        assert_eq!(m.meaning, MeasurementMeaning::Unknown);
        let err = require_convertible(&m).unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn no_evidence_at_all_is_unknown_on_both_axes() {
        let m = resolve(&facts(false, None, None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Unknown);
        assert_eq!(m.meaning, MeasurementMeaning::Unknown);
        assert!(require_convertible(&m).is_err());
    }

    #[test]
    fn non_linear_gamma_without_raw_is_unsupported_transfer() {
        let m = resolve(&facts(false, Some(2.2), None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Unknown);
        assert!(require_convertible(&m).is_err());
    }

    #[test]
    fn malformed_gamma_on_raw_is_ambiguous_not_linear() {
        // A present-but-uninterpretable gamma (e.g. locale "2,2") on a raw-mode scan
        // is ambiguous, NOT silently linear — it must not skip the contradiction
        // path. Auto resolves Unknown; convert rejects.
        let m = resolve(
            &facts_gamma(true, GammaFact::Malformed("2,2".into()), None),
            &InputAssertions::auto(),
        )
        .unwrap();
        assert_eq!(m.transfer, TransferDescription::Unknown);
        assert_eq!(m.meaning, MeasurementMeaning::ScannerDevice);
        assert!(require_convertible(&m).is_err());
        assert!(m.evidence.iter().any(|e| {
            e.axis == InputAxis::Transfer
                && e.kind == EvidenceKind::Descriptive
                && e.detail.contains("uninterpretable")
        }));

        // An explicit `--input-transfer linear` still overrides it (user takes
        // responsibility), recording the uninterpretable tag as displaced.
        let m = resolve(
            &facts_gamma(true, GammaFact::Malformed("2,2".into()), None),
            &assertions(TransferAssertion::Linear, MeaningAssertion::Auto),
        )
        .unwrap();
        assert_eq!(m.transfer, TransferDescription::Linear);
        assert!(require_convertible(&m).is_ok());
        assert!(m.evidence.iter().any(|e| {
            e.kind == EvidenceKind::UserAssertion
                && e.displaced.as_deref().is_some_and(|d| d.contains("2,2"))
        }));
    }

    #[test]
    fn agreeing_gamma_on_raw_corroborates_linear_transfer() {
        // Raw-mode linear + a linear (γ≈1) gamma tag: the descriptive tag agrees,
        // so it corroborates rather than conflicting — transfer stays Linear and
        // the agreement is recorded as descriptive evidence.
        let m = resolve(&facts(true, Some(1.0), None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Linear);
        assert_eq!(m.meaning, MeasurementMeaning::ScannerDevice);
        assert!(require_convertible(&m).is_ok());
        assert!(m.evidence.iter().any(|e| {
            e.axis == InputAxis::Transfer
                && e.kind == EvidenceKind::Descriptive
                && e.detail.contains("agrees")
        }));
    }

    // --- contradictions ------------------------------------------------------

    #[test]
    fn contradictory_gamma_on_raw_is_ambiguous_and_not_convertible() {
        // Authoritative raw-mode linear + a non-linear gamma tag → ambiguous
        // transfer (Unknown); `require_convertible` rejects, inspect still reports
        // both. (Named for the gate it exercises — `require_convertible` — not the
        // full `convert` command; the end-to-end reject is covered in
        // tests/pipeline.rs.)
        let m = resolve(&facts(true, Some(2.2), None), &InputAssertions::auto()).unwrap();
        assert_eq!(m.transfer, TransferDescription::Unknown);
        assert_eq!(m.meaning, MeasurementMeaning::ScannerDevice);
        assert!(require_convertible(&m).is_err());
        assert!(
            m.evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::Descriptive && e.detail.contains("contradicts"))
        );
    }

    #[test]
    fn explicit_linear_overrides_contradicting_gamma_and_records_displaced() {
        // An explicit assertion resolves the ambiguity by overriding the
        // descriptive gamma, which is recorded as displaced evidence.
        let m = resolve(
            &facts(true, Some(2.2), None),
            &assertions(TransferAssertion::Linear, MeaningAssertion::Auto),
        )
        .unwrap();
        assert_eq!(m.transfer, TransferDescription::Linear);
        assert!(require_convertible(&m).is_ok());
        let assertion = m
            .evidence
            .iter()
            .find(|e| e.axis == InputAxis::Transfer && e.kind == EvidenceKind::UserAssertion)
            .unwrap();
        assert!(assertion.provenance.is_some());
        assert!(
            assertion
                .displaced
                .as_deref()
                .is_some_and(|d| d.contains("2.2"))
        );
    }

    #[test]
    fn explicit_colorimetric_on_raw_scanner_fails_loudly() {
        // An explicit assertion that contradicts authoritative structure fails
        // rather than overriding it (usage error, exit 2).
        let err = resolve(
            &facts(true, None, None),
            &assertions(TransferAssertion::Auto, MeaningAssertion::Colorimetric),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn explicit_colorimetric_without_structure_is_recognized_but_unsupported() {
        // No structural meaning → the assertion is honored (recorded) but the
        // convert gate still rejects colorimetric.
        let m = resolve(
            &facts(false, Some(1.0), None),
            &assertions(TransferAssertion::Linear, MeaningAssertion::Colorimetric),
        )
        .unwrap();
        assert!(matches!(
            m.meaning,
            MeasurementMeaning::Colorimetric {
                reference: ColorReference::Asserted
            }
        ));
        assert!(require_convertible(&m).is_err());
    }

    // --- explicit overrides + provenance -------------------------------------

    #[test]
    fn explicit_scanner_device_without_structure_records_provenance_and_displaced() {
        let a = InputAssertions {
            transfer: TransferAssertion::Linear,
            meaning: MeaningAssertion::ScannerDevice,
            transfer_from_cli: true,
            meaning_from_cli: true,
        };
        let m = resolve(&facts(false, None, None), &a).unwrap();
        assert_eq!(m.meaning, MeasurementMeaning::ScannerDevice);
        assert_eq!(m.transfer, TransferDescription::Linear);
        // Both axes convertible once explicitly asserted.
        assert!(require_convertible(&m).is_ok());
        let meaning = m
            .evidence
            .iter()
            .find(|e| e.axis == InputAxis::Meaning && e.kind == EvidenceKind::UserAssertion)
            .unwrap();
        assert_eq!(
            meaning.provenance.as_deref(),
            Some("--input-meaning (CLI flag)")
        );
        assert!(meaning.displaced.is_some());
    }

    #[test]
    fn recipe_vs_cli_provenance_is_distinguished() {
        let recipe = InputAssertions {
            transfer: TransferAssertion::Linear,
            meaning: MeaningAssertion::Auto,
            transfer_from_cli: false,
            meaning_from_cli: false,
        };
        let m = resolve(&facts(true, None, None), &recipe).unwrap();
        let assertion = m
            .evidence
            .iter()
            .find(|e| e.kind == EvidenceKind::UserAssertion)
            .unwrap();
        assert_eq!(
            assertion.provenance.as_deref(),
            Some("input.transfer (recipe)")
        );
    }

    // --- embedded vs non-embedded ICC: same path, reported difference --------

    #[test]
    fn embedded_and_non_embedded_same_raw_pick_same_path() {
        let without = resolve(&facts(true, None, None), &InputAssertions::auto()).unwrap();
        // A minimal valid sRGB profile as the "embedded" ICC.
        let icc = crate::pipeline::color::icc_profile(&crate::pipeline::color::OutputSpace::SRgb)
            .unwrap();
        let with = resolve(
            &facts(true, None, Some(icc.clone())),
            &InputAssertions::auto(),
        )
        .unwrap();
        // Same conversion decision on both axes...
        assert_eq!(without.transfer, with.transfer);
        assert_eq!(without.meaning, with.meaning);
        assert!(require_convertible(&without).is_ok());
        assert!(require_convertible(&with).is_ok());
        // ...but inspection reports the profile difference.
        let rwithout = InputColorReport::from_metadata(&without);
        let rwith = InputColorReport::from_metadata(&with);
        assert!(!rwithout.icc_embedded);
        assert!(rwith.icc_embedded);
        assert!(rwith.icc.is_some());
    }

    #[test]
    fn embedded_icc_alone_does_not_establish_meaning() {
        // An embedded ICC with NO raw-mode structure: meaning stays Unknown.
        let icc = crate::pipeline::color::icc_profile(&crate::pipeline::color::OutputSpace::SRgb)
            .unwrap();
        let m = resolve(&facts(false, None, Some(icc)), &InputAssertions::auto()).unwrap();
        assert_eq!(m.meaning, MeasurementMeaning::Unknown);
        assert!(require_convertible(&m).is_err());
    }

    #[test]
    fn icc_summary_has_safe_fields_and_no_bytes() {
        let icc = crate::pipeline::color::icc_profile(&crate::pipeline::color::OutputSpace::SRgb)
            .unwrap();
        let summary = summarize_icc(&icc).expect("sRGB ICC summarizes");
        assert_eq!(summary.size_bytes, icc.len());
        assert_eq!(summary.color_space, "RgbData");
        // Serialized summary must not contain a raw byte array.
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains('['), "summary must not dump bytes: {json}");
    }

    #[test]
    fn unparsable_icc_reports_embedded_but_no_summary() {
        let m = resolve(
            &facts(true, None, Some(b"not an icc".to_vec())),
            &InputAssertions::auto(),
        )
        .unwrap();
        let report = InputColorReport::from_metadata(&m);
        assert!(report.icc_embedded);
        assert!(report.icc.is_none());
        assert!(report.icc_unparsable());
    }

    #[test]
    fn transfer_decoded_is_always_false_in_step1() {
        let m = resolve(&facts(true, None, None), &InputAssertions::auto()).unwrap();
        assert!(!InputColorReport::from_metadata(&m).transfer_decoded);
    }

    #[test]
    fn meaning_serializes_as_a_flat_string_with_sibling_reference() {
        // `meaning` must always be a plain string on the wire (never sometimes an
        // object), and the colorimetric detail rides in the sibling
        // `meaning_reference` field so consumers can key `meaning` uniformly.
        for m in [
            MeasurementMeaning::ScannerDevice,
            MeasurementMeaning::Unknown,
            MeasurementMeaning::Colorimetric {
                reference: ColorReference::Asserted,
            },
        ] {
            let json = serde_json::to_value(&m).unwrap();
            assert!(
                json.is_string(),
                "meaning must serialize as a string: {json}"
            );
        }

        // A colorimetric report carries the flat string plus the sibling reference.
        let meta = resolve(
            &facts(false, Some(1.0), None),
            &assertions(TransferAssertion::Linear, MeaningAssertion::Colorimetric),
        )
        .unwrap();
        let report = serde_json::to_value(InputColorReport::from_metadata(&meta)).unwrap();
        assert_eq!(report["meaning"], "colorimetric");
        assert_eq!(report["meaning_reference"], "asserted");

        // A scanner-device report omits the sibling entirely.
        let meta = resolve(&facts(true, None, None), &InputAssertions::auto()).unwrap();
        let report = serde_json::to_value(InputColorReport::from_metadata(&meta)).unwrap();
        assert_eq!(report["meaning"], "scanner-device");
        assert!(report.get("meaning_reference").is_none());
    }
}
