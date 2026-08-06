//! The maintenance command: re-derive every artifact and compare it with the
//! shipped literal, in **check** mode (default) or **regeneration** mode.
//!
//! ```text
//! cargo test colorimetry::audit                          # check  (also runs in CI)
//! NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit   # regenerate the artifact
//! ```
//!
//! Check mode runs inside the ordinary `cargo test` gate, so CI exercises it on
//! every PR with no extra step and no Python in the loop.
//!
//! ## What is generated, and what deliberately is not
//!
//! Regeneration rewrites [`AUDIT_PATH`] — a human-readable record of the
//! canonical binary64 derivation and each shipped literal's distance from it. It
//! does **not** rewrite [`super::pinned`]. That asymmetry is the whole safety
//! property: a generator that edits the runtime coefficients could silently
//! change pixels on someone's machine, whereas this one can only ever produce a
//! *diff to review*. Re-pinning a runtime literal stays a deliberate, reviewed
//! edit with a pipeline-version decision attached — see
//! `docs/colorimetry-maintenance.md`.
//!
//! Because the audit file records the shipped values too, editing a literal in
//! `pinned.rs` without regenerating fails the check. Stale derived artifacts
//! cannot go unnoticed in either direction.
//!
//! ## Cross-platform determinism
//!
//! The derivation uses only IEEE-754 binary64 `+ - * /` — no transcendentals, no
//! libm — so the rendered text is bit-identical on macOS/aarch64 and Linux/x86_64.
//! That is what makes this checked-in artifact safe as a CI gate, unlike the
//! whole-frame checksums CLAUDE.md warns against.

use std::fmt::Write as _;

use super::definitions::{
    self, ACESCG, BRADFORD, BRADFORD_PUBLISHED_INVERSE, BT2020, Chromaticity, ColorSpace,
    ConeResponse, DISPLAY_P3, REC709,
};
use super::derive;
use super::pinned;

/// Path of the checked-in audit artifact, relative to the crate root.
pub const AUDIT_PATH: &str = "src/pipeline/colorimetry/derived-artifacts.txt";

const REGEN_ENV: &str = "NC_COLORIMETRY_REGEN";

/// How an artifact's canonical value is obtained.
// `RgbToRgb` carries two `ColorSpace`s and a `ConeResponse` and so is much larger
// than `Tabulated`. That is irrelevant here: the catalog is eight entries built
// once inside a test, so there is nothing to gain by boxing a variant and real
// clarity to lose.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum Source {
    /// Composed linear RGB → linear RGB transform.
    RgbToRgb {
        source: ColorSpace,
        destination: ColorSpace,
        cone: ConeResponse,
    },
    /// Luminance row of a space's normalized primary matrix.
    LumaRow(ColorSpace),
    /// Transcribed from a normative table — *not* derived. Recorded so the audit
    /// file is complete and so the distinction stays visible in review.
    Tabulated([f64; 3]),
    /// Non-constant-luminance R'G'B' → Y'CbCr matrix implied by a luma vector.
    /// Derived from the *tabulated* luma, not from primaries — see
    /// [`pinned::BT2020_NCL_RGB_TO_YCBCR`](super::pinned::BT2020_NCL_RGB_TO_YCBCR).
    YCbCrFromLuma([f64; 3]),
    /// Linear RGB → XYZ adapted to another white: an ICC colorant matrix.
    RgbToXyzAdapted {
        source: ColorSpace,
        destination_white: Chromaticity,
        cone: ConeResponse,
    },
}

/// The shipped literal an artifact is compared against, flattened row-major.
///
/// Held widened to `f64` with a flag for the comparison width rather than as an
/// enum of array shapes: a shipped `f32` widens exactly, so narrowing back for
/// the ulp comparison is lossless, and one representation keeps the rendering
/// loop free of per-shape branching.
struct Shipped {
    values: Vec<f64>,
    compare_as_f32: bool,
}

impl Shipped {
    fn matrix_f32(m: [[f32; 3]; 3]) -> Self {
        Self {
            values: m.iter().flatten().map(|&v| v as f64).collect(),
            compare_as_f32: true,
        }
    }

    fn matrix_f64(m: [[f64; 3]; 3]) -> Self {
        Self {
            values: m.iter().flatten().copied().collect(),
            compare_as_f32: false,
        }
    }

    fn vector_f32(v: [f32; 3]) -> Self {
        Self {
            values: v.iter().map(|&x| x as f64).collect(),
            compare_as_f32: true,
        }
    }
}

struct Artifact {
    name: &'static str,
    description: &'static str,
    source: Source,
    shipped: Shipped,
}

/// Every artifact under audit. Adding a colour space means adding a row here.
fn catalog() -> Vec<Artifact> {
    vec![
        Artifact {
            name: "NC_FILM_RGB_V1_TO_ACESCG",
            description: "rec709/d65 -> acescg/aces-white, cone=bradford-lindbloom-published-inverse",
            source: Source::RgbToRgb {
                source: REC709,
                destination: ACESCG,
                cone: BRADFORD_PUBLISHED_INVERSE,
            },
            shipped: Shipped::matrix_f64(pinned::NC_FILM_RGB_V1_TO_ACESCG),
        },
        Artifact {
            name: "ACESCG_TO_SRGB",
            description: "acescg/aces-white -> rec709/d65, cone=bradford",
            source: Source::RgbToRgb {
                source: ACESCG,
                destination: REC709,
                cone: BRADFORD,
            },
            shipped: Shipped::matrix_f32(pinned::ACESCG_TO_SRGB),
        },
        Artifact {
            name: "BT2020_TO_XYZ_D50",
            description: "bt2020/d65 -> xyz/d50 colorants, cone=bradford",
            source: Source::RgbToXyzAdapted {
                source: BT2020,
                destination_white: definitions::D50,
                cone: BRADFORD,
            },
            shipped: Shipped::matrix_f64(pinned::BT2020_TO_XYZ_D50),
        },
        Artifact {
            name: "ACESCG_TO_DISPLAY_P3",
            description: "acescg/aces-white -> display-p3/d65, cone=bradford",
            source: Source::RgbToRgb {
                source: ACESCG,
                destination: DISPLAY_P3,
                cone: BRADFORD,
            },
            shipped: Shipped::matrix_f32(pinned::ACESCG_TO_DISPLAY_P3),
        },
        Artifact {
            name: "ACESCG_TO_BT2020",
            description: "acescg/aces-white -> bt2020/d65, cone=bradford",
            source: Source::RgbToRgb {
                source: ACESCG,
                destination: BT2020,
                cone: BRADFORD,
            },
            shipped: Shipped::matrix_f32(pinned::ACESCG_TO_BT2020),
        },
        Artifact {
            name: "BT2020_TO_DISPLAY_P3",
            description: "bt2020/d65 -> display-p3/d65, no adaptation (shared white)",
            source: Source::RgbToRgb {
                source: BT2020,
                destination: DISPLAY_P3,
                cone: BRADFORD,
            },
            shipped: Shipped::matrix_f32(pinned::BT2020_TO_DISPLAY_P3),
        },
        Artifact {
            name: "DISPLAY_P3_LUMA",
            description: "luminance row of display-p3/d65 normalized primary matrix (derived)",
            source: Source::LumaRow(DISPLAY_P3),
            shipped: Shipped::vector_f32(pinned::DISPLAY_P3_LUMA),
        },
        Artifact {
            name: "SRGB_LUMA",
            description: "luminance row of rec709/d65 normalized primary matrix, SHIPPED ROUNDED TO 6 DECIMALS",
            source: Source::LumaRow(REC709),
            shipped: Shipped::vector_f32(pinned::SRGB_LUMA),
        },
        Artifact {
            name: "BT2020_LUMA",
            description: "bt2020 non-constant-luminance luma, TABULATED by the standard (not derived)",
            source: Source::Tabulated(definitions::BT2020_LUMA_TABULATED),
            shipped: Shipped::vector_f32(pinned::BT2020_LUMA),
        },
        Artifact {
            name: "BT2020_NCL_RGB_TO_YCBCR",
            description: "nonlinear R'G'B' -> Y'CbCr from TABULATED bt2020 luma (AVIF matrix_coefficients=9)",
            source: Source::YCbCrFromLuma(definitions::BT2020_LUMA_TABULATED),
            shipped: Shipped::matrix_f32(pinned::BT2020_NCL_RGB_TO_YCBCR),
        },
    ]
}

/// Canonical values, flattened row-major, alongside their `[i][j]` labels.
fn canonical(source: Source) -> Vec<(String, f64)> {
    match source {
        Source::RgbToRgb {
            source,
            destination,
            cone,
        } => {
            let m = derive::rgb_to_rgb(source, destination, cone);
            (0..3)
                .flat_map(|i| (0..3).map(move |j| (format!("[{i}][{j}]"), m[i][j])))
                .collect()
        }
        Source::LumaRow(space) => derive::luma_row(space)
            .iter()
            .enumerate()
            .map(|(i, &v)| (format!("[{i}]"), v))
            .collect(),
        Source::Tabulated(v) => v
            .iter()
            .enumerate()
            .map(|(i, &v)| (format!("[{i}]"), v))
            .collect(),
        Source::YCbCrFromLuma(luma) => {
            let m = derive::ycbcr_from_luma(luma);
            (0..3)
                .flat_map(|i| (0..3).map(move |j| (format!("[{i}][{j}]"), m[i][j])))
                .collect()
        }
        Source::RgbToXyzAdapted {
            source,
            destination_white,
            cone,
        } => {
            let m = derive::rgb_to_xyz_adapted(source, destination_white, cone);
            (0..3)
                .flat_map(|i| (0..3).map(move |j| (format!("[{i}][{j}]"), m[i][j])))
                .collect()
        }
    }
}

/// Signed distance between two `f32` values in ulps (`0` means bit-identical).
///
/// Each value is first mapped to a **monotonically ordered key** rather than
/// subtracted as raw bits. IEEE-754 is sign-magnitude, so raw patterns are not
/// ordered across the sign: subtracting them overflows for any pair straddling
/// zero (`f32::MIN_POSITIVE` and its negation differ by 2_147_483_648 in raw
/// bits, one past `i32::MAX` — a panic in a debug build, which is how `cargo
/// test` runs, and a wrapped nonsense value in release), and it also *reverses
/// the sign* for two negative values, reporting a larger number as the smaller
/// one.
///
/// That is reachable, not theoretical. Several shipped entries sit near zero
/// (`BT2020_TO_DISPLAY_P3[2][0]`, `ACESCG_TO_DISPLAY_P3[2][0]`,
/// `ACESCG_TO_BT2020[1][0]`), so a standards revision that flips one across zero
/// would make the audit *panic* instead of reporting the difference it exists to
/// report. The ordered key removes the discontinuity and `i64` gives the result
/// room: keys span ±2^31, so their difference always fits.
pub fn ulps_f32(a: f32, b: f32) -> i64 {
    /// Maps `f32` bit patterns onto a single monotonically increasing integer
    /// line. Non-negative values keep their pattern; negative ones are reflected
    /// about `i32::MIN`, which sends `-0.0` to `0` (same key as `+0.0`) and
    /// `-f32::MAX` to the most negative key.
    fn key(v: f32) -> i64 {
        let bits = v.to_bits() as i32;
        if bits < 0 {
            i32::MIN as i64 - bits as i64
        } else {
            bits as i64
        }
    }
    key(a) - key(b)
}

/// Render the audit artifact. Deterministic: same inputs ⇒ same bytes.
fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# @generated — do not edit by hand.\n\
         #\n\
         # Canonical binary64 derivation of every pinned colorimetry artifact, with the\n\
         # shipped literal's distance from it. Produced from the source definitions in\n\
         # src/pipeline/colorimetry/definitions.rs.\n\
         #\n\
         #   check:      cargo test colorimetry::audit\n\
         #   regenerate: NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit\n\
         #\n\
         # A non-zero ulp distance is not automatically a bug. The chromaticities these\n\
         # matrices derive from are specified to three decimals, and perturbing one\n\
         # primary by its own rounding moves entries ~3,500x further than one f32 ulp.\n\
         # See docs/colorimetry-maintenance.md before changing a shipped literal — that\n\
         # is a pixel change and needs a pipeline-version decision.\n",
    );

    for artifact in catalog() {
        let values = canonical(artifact.source);
        let Shipped {
            values: shipped_values,
            compare_as_f32,
        } = artifact.shipped;
        assert_eq!(
            values.len(),
            shipped_values.len(),
            "{}: catalog shape mismatch",
            artifact.name
        );

        let _ = write!(
            out,
            "\n[{}]\n  {}\n  compared as {}\n",
            artifact.name,
            artifact.description,
            if compare_as_f32 { "f32" } else { "f64" }
        );

        for ((label, derived_value), shipped_value) in values.iter().zip(&shipped_values) {
            if compare_as_f32 {
                let ulps = ulps_f32(*derived_value as f32, *shipped_value as f32);
                let _ = writeln!(
                    out,
                    "  {label:6} derived={:<24} shipped={:<16} ulps={ulps}",
                    format!("{derived_value:?}"),
                    format!("{:?}", *shipped_value as f32),
                );
            } else {
                let _ = writeln!(
                    out,
                    "  {label:6} derived={:<24} shipped={:<24} absdiff={:.3e}",
                    format!("{derived_value:?}"),
                    format!("{shipped_value:?}"),
                    (derived_value - shipped_value).abs(),
                );
            }
        }
    }
    out
}

fn audit_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(AUDIT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The maintenance command. Check mode by default; regeneration when
    /// `NC_COLORIMETRY_REGEN` is set.
    #[test]
    fn audit_artifact_is_current() {
        let rendered = render();
        let path = audit_file();

        if std::env::var_os(REGEN_ENV).is_some() {
            std::fs::write(&path, &rendered).expect("write audit artifact");
            eprintln!("regenerated {}", path.display());
            return;
        }

        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read {} ({e}).\nRegenerate with: {REGEN_ENV}=1 cargo test colorimetry::audit",
                path.display()
            )
        });

        if checked_in != rendered {
            let first_difference = checked_in
                .lines()
                .zip(rendered.lines())
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("checked in: {a}\nderived   : {b}"))
                .unwrap_or_else(|| "(files differ in length)".to_string());
            panic!(
                "{} is stale — a source definition or a shipped literal changed \
                 without regenerating the audit artifact.\n\n{first_difference}\n\n\
                 Review the change against docs/colorimetry-maintenance.md, then:\n  \
                 {REGEN_ENV}=1 cargo test colorimetry::audit",
                path.display()
            );
        }
    }

    #[test]
    fn regeneration_is_deterministic_and_idempotent() {
        // Same inputs => same bytes, so a second regeneration run produces no
        // diff. Cheap to assert directly rather than by shelling out twice.
        assert_eq!(render(), render());
    }

    /// The task's required negative fixture: intentionally changing a source
    /// definition must make the check fail.
    ///
    /// Perturbs the BT.2020 red primary by 1e-4 — *smaller* than the standard's
    /// own three-decimal rounding, so this also demonstrates the check is
    /// sensitive well below the level at which a real standards revision would
    /// move a value.
    #[test]
    fn a_changed_source_definition_fails_the_check() {
        use definitions::{Chromaticity, Primaries};

        let tampered = ColorSpace {
            name: "bt2020",
            primaries: Primaries::new(
                Chromaticity::new(0.708 + 1e-4, 0.292),
                BT2020.primaries.green,
                BT2020.primaries.blue,
            ),
            white: BT2020.white,
        };

        let honest = derive::rgb_to_rgb(tampered, DISPLAY_P3, BRADFORD);
        let shipped = pinned::BT2020_TO_DISPLAY_P3;
        let moved = (0..3)
            .flat_map(|i| (0..3).map(move |j| (i, j)))
            .any(|(i, j)| ulps_f32(honest[i][j] as f32, shipped[i][j]).abs() > 1);
        assert!(
            moved,
            "tampering with a source primary left every derived entry within \
             tolerance — the check cannot detect stale derived artifacts"
        );

        // And the rendered artifact must differ from the text check mode
        // compares against. Deliberately compared with the in-memory `render()`
        // rather than by reading the file: under `NC_COLORIMETRY_REGEN` this test
        // runs in parallel with `audit_artifact_is_current` rewriting that very
        // file, so a read here can observe a truncated one and pass vacuously —
        // the guard would stop guarding during exactly the run that changes it.
        // `render()` is deterministic and needs no I/O, and check mode already
        // proves it equals the checked-in bytes.
        let current = render();
        let mut tampered_line = String::new();
        let _ = write!(tampered_line, "{:?}", honest[0][0]);
        assert!(
            !current.contains(&tampered_line),
            "tampered derivation coincidentally matches the audited artifact"
        );
    }

    /// `ulps_f32` must stay finite and correctly signed across zero. The raw-bit
    /// subtraction it replaced overflowed `i32` here and panicked in a debug
    /// build, which is how `cargo test` runs.
    #[test]
    fn ulps_across_zero_measures_distance_instead_of_overflowing() {
        // The adjacent representable values either side of zero are the smallest
        // *subnormals*, so +eps -> ±0 -> -eps is two steps on the ordered line.
        let eps = f32::from_bits(1);
        assert_eq!(ulps_f32(eps, -eps), 2);
        assert_eq!(ulps_f32(-eps, eps), -2);

        // The straddle that used to overflow: `f32::MIN_POSITIVE` is the smallest
        // normal, so every subnormal lies between it and its negation — 2^24
        // ordered steps, but 2_147_483_648 in raw bits, which is one past
        // `i32::MAX`.
        let tiny = f32::MIN_POSITIVE;
        assert_eq!(ulps_f32(tiny, -tiny), 16_777_216);
        assert_eq!(ulps_f32(-tiny, tiny), -16_777_216);

        // Both zeros key identically, so they are zero ulps apart either way.
        assert_eq!(ulps_f32(0.0, -0.0), 0);
        assert_eq!(ulps_f32(-0.0, 0.0), 0);
        assert_eq!(ulps_f32(-0.0, -0.0), 0);

        // One step off each zero, and the ordinary same-sign cases. The negative
        // pair is the one raw-bit subtraction got backwards: -1.0 is *below*
        // its next-larger-magnitude neighbour.
        assert_eq!(ulps_f32(eps, 0.0), 1);
        assert_eq!(ulps_f32(-eps, -0.0), -1);
        assert_eq!(ulps_f32(1.0, f32::from_bits(1.0_f32.to_bits() + 1)), -1);
        assert_eq!(ulps_f32(-1.0, f32::from_bits((-1.0_f32).to_bits() + 1)), 1);

        // The full span stays in range rather than saturating or wrapping.
        assert_eq!(ulps_f32(f32::MAX, -f32::MAX), 4_278_190_078);
        assert_eq!(
            ulps_f32(f32::MAX, -f32::MAX),
            -ulps_f32(-f32::MAX, f32::MAX)
        );
    }
}
