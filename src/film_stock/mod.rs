//! The digitized film-stock data: ten stocks' published characteristic curves, their
//! aim tables, and the registry that names them.
//!
//! # What this is, and who reads it
//!
//! **Data, not a stage.** Each table is one dye layer's `density → log exposure` relation,
//! read off a manufacturer's sheet in `docs/datasheets/` by
//! `scripts/analysis/digitize_datasheets.py` (see that directory's README for the
//! pipeline). Nothing here transforms a pixel.
//!
//! **Test-only: its one consumer is the evidence for the fixed decode's constants.**
//! The `characteristic` curve that inverted these tables at runtime retired
//! (`nf-retire/characteristic`); the data outlives it (`nf-look/stock-data-home`) because
//! `algo::fixed::MID_ABOVE_BASE` is `generic-c41`'s mid aim, and `docs/design-update.md`
//! Part 1 argues for fixed, stock-agnostic values from these tables' own spread (`d`
//! 0.542–0.699, red film gamma 0.53–0.61). The tests below are that provenance, checked on
//! every run — the `pipeline::colorimetry::derive` precedent.
//!
//! Its natural future runtime reader is **per-stock normalization**, an optional look
//! control (design-update Part 2), which brings its own flag. It must not come back as a
//! decode: a per-stock value inside reconstruction is the per-stock normalization the
//! fixed decode exists to refuse.
//!
//! No render path reads a sheet's published `D-min` (`algo/film-stock-profiles`,
//! Constraint 1): the measured roll base stays authoritative.

pub mod curves;

use curves::{STOCKS, StockCurves};

/// A film stock with a digitized characteristic curve — the registry's key.
///
/// No longer recipe or CLI vocabulary: `--film-stock` and `reconstruction.curve.stock`
/// left with the `characteristic` curve, and per-stock normalization will bring its own
/// flag. A future wire spelling must be [`FilmStock::as_str`], not a serde rename —
/// `kebab-case` turns `Portra400` into `portra400`, which is not the curve-table key.
///
/// The variants are exactly the entries in [`curves::STOCKS`]; a test pins that
/// correspondence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilmStock {
    /// The average of the nine measured stocks (ten ship; this one is derived) — red
    /// mid-scale gamma 0.541 with mid-grey
    /// 0.624 density above base. For scale: the per-stock spread is 0.50–0.61 and
    /// 0.54–0.70, and ACES's own generic film model sits at 0.55 / 0.70.
    #[default]
    GenericC41,
    Ektar100,
    Portra160,
    /// The discontinued vivid-colour Portra. Kept because its aim table is the evidence
    /// that `Δ` is genuinely stock-dependent (0.41 against the NC pair's 0.36 at the same
    /// speed), which a registry of only current stocks would not show.
    Portra160vc,
    Portra400,
    Portra400vc,
    /// Box speed (EI 800). The published push curves are a separate response and are not
    /// in the registry — a pushed roll is a different development, not a different stock.
    Portra800,
    Gold200,
    Ultramax400,
    /// Box speed (EI 800). Digitizes to the **same curve as [`Self::Portra800`]** — D-min
    /// and per-channel gamma agree to 0.003 — from a different publication, year and page,
    /// which is an independent check on the extraction as much as a fact about the film.
    Ultramax800,
}

impl FilmStock {
    /// Every variant, in the order `--film-stock` and the parse diagnostics list them.
    pub const ALL: &'static [FilmStock] = &[
        FilmStock::GenericC41,
        FilmStock::Ektar100,
        FilmStock::Portra160,
        FilmStock::Portra160vc,
        FilmStock::Portra400,
        FilmStock::Portra400vc,
        FilmStock::Portra800,
        FilmStock::Gold200,
        FilmStock::Ultramax400,
        FilmStock::Ultramax800,
    ];

    /// The stock's name — the key into the pinned curve table.
    pub fn as_str(self) -> &'static str {
        match self {
            FilmStock::GenericC41 => "generic-c41",
            FilmStock::Ektar100 => "ektar-100",
            FilmStock::Portra160 => "portra-160",
            FilmStock::Portra160vc => "portra-160vc",
            FilmStock::Portra400 => "portra-400",
            FilmStock::Portra400vc => "portra-400vc",
            FilmStock::Portra800 => "portra-800",
            FilmStock::Gold200 => "gold-200",
            FilmStock::Ultramax400 => "ultramax-400",
            FilmStock::Ultramax800 => "ultramax-800",
        }
    }
}

/// The curve set for a resolved stock.
///
/// Infallible: [`FilmStock`] is an enum whose every variant is generated alongside the
/// table, and `stocks_cover_every_film_stock_variant` pins that.
pub fn curves_for(stock: FilmStock) -> &'static StockCurves {
    let name = stock.as_str();
    STOCKS
        .iter()
        .find(|s| s.name == name)
        .expect("every FilmStock variant has a pinned curve set")
}

/// Decades between an 18 % grey card and a ~89 % paper white — `log10(0.89 / 0.18)`.
///
/// The interval the *Judging Negative Exposures* aim pair spans, and therefore the
/// interval any comparison against the curve has to use.
pub(crate) const AIM_SEPARATION_DECADES: f32 = 0.694;

/// The sheets whose two published halves disagree by too much for the aim table to
/// correct anything, with the measured reason.
///
/// **Not derived from a threshold.** The corpus contains sheets that disagree by 11 %
/// and are still usable (Ektar 100, Ultramax 400), so any cut-off separating those from
/// these two would be a number invented to fit the answer. These are named because the
/// inconsistency was established per sheet: both tabulate `Δ = 0.25` against their own
/// curves' ~0.36 rise (+44 %), which is a different kind of disagreement from a curve
/// read slightly steep.
const NO_USABLE_AIM_DELTA: &[&str] = &["portra-800", "ultramax-800"];

impl StockCurves {
    /// Density on one channel's published curve at relative log exposure `x`, linearly
    /// interpolated between table points and extrapolated from the end segment outside
    /// them — the forward reading of a table.
    pub(crate) fn density_at(&self, channel: usize, x: f32) -> f32 {
        let t = self.channels[channel];
        let i = t.partition_point(|p| p.0 <= x).clamp(1, t.len() - 1);
        let ((x0, d0), (x1, d1)) = (t[i - 1], t[i]);
        d0 + (x - x0) * (d1 - d0) / (x1 - x0)
    }

    /// The published `Δ` (paper white − grey card, Status M red), or `None` when this
    /// sheet states none that can be used — the derived generic, which has no aim table
    /// at all, and the two 800-speed sheets in [`NO_USABLE_AIM_DELTA`].
    pub(crate) fn usable_aim_delta(&self) -> Option<f32> {
        if NO_USABLE_AIM_DELTA.contains(&self.name) {
            return None;
        }
        self.aims.map(|[grey, white]| white - grey)
    }
}

/// `generic-c41`'s own mid-grey aim above base, red — the averaged curve's, not the mean
/// of `STOCK_MID_ABOVE_BASE` (0.617), which averages numbers rather than curves.
#[cfg(test)]
pub(crate) const GENERIC_MID_ABOVE_BASE: f32 = 0.624;

/// `mid aim − D-min` per stock, from the datasheets (progress log, 2026-09-04). No render
/// reads them — the curve carries the placement — so they exist to prove the log-exposure
/// axis was shifted correctly when the curves were generated, and to bound the fixed
/// decode's hand-frozen `d`.
#[cfg(test)]
pub(crate) const STOCK_MID_ABOVE_BASE: &[(&str, f32)] = &[
    ("ektar-100", 0.611),
    ("portra-160", 0.640),
    ("portra-400", 0.600),
    ("portra-160vc", 0.651),
    ("portra-400vc", 0.651),
    ("portra-800", 0.542),
    ("gold-200", 0.699),
    ("ultramax-400", 0.615),
    ("ultramax-800", 0.542),
];

// Every test here reads the tables **forward** (`density_at`).
#[cfg(test)]
mod tests {
    use super::*;

    /// Density on red at relative exposure `e` — the forward reading every bound below
    /// is stated in.
    fn red_density_at(sc: &StockCurves, e: f32) -> f32 {
        sc.density_at(0, e.log10())
    }

    /// Every sheet's **own aim table** must agree with its **own curve**.
    ///
    /// The two halves of a datasheet are independent measurements of the same film: the
    /// *Judging Negative Exposures* table gives the grey-card and paper-white densities, and
    /// the characteristic curve gives density against exposure. A grey card and a paper white
    /// are `log10(0.89/0.18) ≈ 0.694` decades apart, so `Δ / 0.694` must equal the curve's own
    /// mid-scale slope. When it does not, the *sheet* disagrees with itself, and re-reading
    /// the artwork cannot fix it — verified by extracting Ektar through two independent paths
    /// (raw content stream and SVG), which agreed to 0.004.
    ///
    /// This test therefore documents the corpus rather than guarding the extraction: the two
    /// sheets that fail are named with their measured error, so the state of the data is
    /// visible in code instead of remembered.
    ///
    /// It is **not** a predictor of rendered colour. Portra 160 passes at +1% yet still shows
    /// a measured green residual of +0.48 stops/density on real scans — see the 2026-09-06
    /// entries in `docs/progress/algo.md` for what that points at.
    #[test]
    fn aim_table_agrees_with_the_curve() {
        // Sheets whose two halves disagree by more than 10%, with the measured error. Named
        // rather than skipped: an unexplained failure must not look like a tolerance choice.
        const KNOWN_INCONSISTENT: &[(&str, i32)] = &[
            ("ektar-100", 11),
            // Its curve rises 11% more than its own aim table says across the aims' own
            // separation, and its `γ_G/γ_R` is 1.002 against every other stock's 1.02-1.05
            // — i.e. this sheet draws red and green as nearly parallel, so it predicts
            // almost no green divergence (+0.22 stops/density) where the scans show the
            // most of any stock (+1.26). Its aim table is also digit-for-digit Portra
            // 400's, which a higher-contrast film should not share.

            // The opposite direction, and untested by eye — no roll of it in the fixtures.
            ("ultramax-400", -11),
        ];
        for sc in STOCKS {
            // Skips the derived generic (no aim table) and the two 800-speed sheets,
            // whose Δ is unusable — the same predicate `aim_red_scale` refuses on, so a
            // sheet can never be correctable by one and unchecked by the other.
            let Some(tabulated) = sc.usable_aim_delta() else {
                continue;
            };
            // Compare the two published quantities **over the same interval**: the curve's
            // own density rise across exactly the separation the aims span, starting at the
            // grey aim (which the axis is built to put at `log10(0.18)`).
            //
            // Not a local slope. A first version measured gamma over ±0.35 decade around
            // mid-grey and divided the tabulated Δ by 0.694; that made Ektar look 15% out
            // with a `γ_G/γ_R` of 0.970 — both artefacts of a narrow window landing on a
            // local wiggle in that one curve. Widening to ±0.5 decade moves Ektar's ratio
            // to 1.002 and leaves every other stock unchanged, which is how the artefact
            // was found. Interval-matched quantities have no such freedom.
            let log18 = 0.18f32.log10();
            let from_curve =
                sc.density_at(0, log18 + AIM_SEPARATION_DECADES) - sc.density_at(0, log18);
            let error_pct = (100.0 * (from_curve / tabulated - 1.0)).round() as i32;

            match KNOWN_INCONSISTENT.iter().find(|(n, _)| *n == sc.name) {
                Some((_, recorded)) => assert!(
                    (error_pct - recorded).abs() <= 1,
                    "{}: the recorded inconsistency moved from {recorded}% to {error_pct}% \
                     — re-check the sheet and update the record",
                    sc.name
                ),
                None => assert!(
                    error_pct.abs() <= 10,
                    "{}: the aim table gives Δ = {tabulated:.3} but its own curve rises \
                     {from_curve:.3} over the same interval ({error_pct}%). Either the sheet \
                     disagrees with itself — add it to KNOWN_INCONSISTENT with the measured \
                     figure — or the extraction is wrong.",
                    sc.name
                ),
            }
        }
    }

    /// The pinned literals must match the digitized extraction they were generated from.
    ///
    /// This is the audit half of the `pipeline/colorimetry/` pattern: `curves.json` is
    /// produced from the publications in `docs/datasheets/` by a script that needs poppler
    /// and is run by hand; this test needs neither poppler nor network, so CI checks the
    /// correspondence on every run. A hand-edited literal — or a regeneration that was
    /// never carried across — fails here instead of quietly moving pixels.
    #[test]
    fn curves_match_the_digitized_json() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("curves.json")).expect("curves.json parses");
        let object = json.as_object().expect("curves.json is an object");
        assert_eq!(
            object.len(),
            STOCKS.len(),
            "curves.json and the pinned table hold different stocks"
        );
        for sc in STOCKS {
            let entry = object
                .get(sc.name)
                .unwrap_or_else(|| panic!("{} is missing from curves.json", sc.name));
            assert_eq!(entry["publication"], sc.publication, "{}", sc.name);
            assert_eq!(entry["revision"], sc.revision, "{}", sc.name);
            match sc.aims {
                Some([grey, white]) => {
                    assert_eq!(
                        entry["aim_grey_red"].as_f64().unwrap() as f32,
                        grey,
                        "{}",
                        sc.name
                    );
                    assert_eq!(
                        entry["aim_white_red"].as_f64().unwrap() as f32,
                        white,
                        "{}",
                        sc.name
                    );
                }
                None => assert!(entry["aim_grey_red"].is_null(), "{}", sc.name),
            }
            for (c, channel) in ["R", "G", "B"].iter().enumerate() {
                let points = entry["channels"][channel]["points"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{} channel {channel}: no points", sc.name));
                assert_eq!(
                    points.len(),
                    sc.channels[c].len(),
                    "{} channel {channel}: point count",
                    sc.name
                );
                for (i, (want, got)) in points.iter().zip(sc.channels[c]).enumerate() {
                    let (wx, wd) = (
                        want[0].as_f64().unwrap() as f32,
                        want[1].as_f64().unwrap() as f32,
                    );
                    assert_eq!(
                        (wx, wd),
                        *got,
                        "{} channel {channel} point {i} drifted from curves.json",
                        sc.name
                    );
                }
            }
        }
    }

    /// The invariants `curves` promises and anything reading a table relies on:
    /// **strictly increasing** in both coordinates (so the curve is single-valued either
    /// way), a **first point at `D′ = 0`** (the film base), and enough points to
    /// interpolate.
    #[test]
    fn every_table_rises_strictly_from_the_base() {
        for sc in STOCKS {
            for (c, table) in sc.channels.iter().enumerate() {
                assert!(
                    table.len() >= 8,
                    "{} ch{c}: only {} points",
                    sc.name,
                    table.len()
                );
                assert_eq!(table[0].1, 0.0, "{} ch{c}: must start at the base", sc.name);
                for w in table.windows(2) {
                    assert!(
                        w[1].0 > w[0].0 && w[1].1 > w[0].1,
                        "{} ch{c}: not strictly increasing at {:?} -> {:?}",
                        sc.name,
                        w[0],
                        w[1]
                    );
                }
            }
        }
    }

    #[test]
    fn stocks_cover_every_film_stock_variant() {
        for stock in FilmStock::ALL {
            let sc = curves_for(*stock);
            assert_eq!(sc.name, stock.as_str());
        }
        assert_eq!(
            STOCKS.len(),
            FilmStock::ALL.len(),
            "the pinned table and the enum have drifted apart"
        );
    }

    /// The axis convention: each stock's published mid-grey aim sits at relative exposure
    /// 0.18 on its own curve. It is what makes the tables self-anchoring, so it is the
    /// load-bearing property of the generated data.
    ///
    /// Stated as a bracket on the forward curve — the aim lies between the densities at
    /// 0.174 and 0.186 — which, the curve being increasing, is exactly "it reads back as
    /// 0.18 ± 0.006".
    #[test]
    fn published_mid_grey_sits_at_eighteen_percent() {
        for (name, mid_above_base) in STOCK_MID_ABOVE_BASE {
            let sc = STOCKS.iter().find(|s| s.name == *name).unwrap();
            // Inside the table, or `density_at` extrapolates and the bracket is not data.
            let red = sc.channels[0];
            assert!(
                red[0].0 <= 0.174f32.log10() && 0.186f32.log10() <= red[red.len() - 1].0,
                "{name}: mid-grey falls outside its own table"
            );
            let (lo, hi) = (red_density_at(sc, 0.174), red_density_at(sc, 0.186));
            assert!(
                (lo..=hi).contains(mid_above_base),
                "{name}: the mid aim {mid_above_base} is outside {lo:.4}..={hi:.4}, the \
                 curve's densities at 0.18 ± 0.006"
            );
        }
    }

    /// The derived generic must sit inside the spread of the stocks it averages, or it is
    /// not an average — it is a ninth opinion.
    #[test]
    fn generic_sits_inside_the_measured_spread() {
        let generic = curves_for(FilmStock::GenericC41);
        let (lo, hi) = (red_density_at(generic, 0.17), red_density_at(generic, 0.19));
        assert!(
            (lo..=hi).contains(&GENERIC_MID_ABOVE_BASE),
            "the generic's own mid-above-base must read back as 0.18"
        );
        // Mid-scale gamma, red: the per-stock measurements run 0.50–0.61.
        let d = |x: f32| generic.density_at(0, x);
        let log18 = 0.18f32.log10();
        let gamma = (d(log18 + 0.35) - d(log18 - 0.35)) / 0.7;
        assert!(
            (0.50..=0.61).contains(&gamma),
            "generic red gamma {gamma:.3} is outside the measured per-stock range"
        );
    }

    /// The fixed decode's `d` is this generic's mid aim, rounded — the provenance
    /// `algo::fixed::MID_ABOVE_BASE` claims, checked here because only this module's tests
    /// may read the datasheet figures. Moving the constant means restating its derivation
    /// there and here, not loosening this.
    #[test]
    fn the_fixed_decode_mid_is_the_generic_aim() {
        let d = crate::algo::fixed::MID_ABOVE_BASE;
        assert_eq!(
            d,
            (GENERIC_MID_ABOVE_BASE * 100.0).round() / 100.0,
            "the fixed decode's d is no longer the generic aim rounded"
        );

        // A convention for every stock, so it must sit inside what the stocks measure.
        let (lo, hi) = STOCK_MID_ABOVE_BASE
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), (_, m)| {
                (lo.min(*m), hi.max(*m))
            });
        assert!(
            (lo..=hi).contains(&d),
            "d {d} is outside the stocks' {lo}..={hi}"
        );
    }

    /// Every stock's blue layer is steeper than its red one — the property that makes a
    /// single scalar contrast wrong, and the reason the decode carries a per-channel
    /// `density.scale`. If a regenerated
    /// table ever loses it, the data is wrong, not the film.
    #[test]
    fn blue_is_steeper_than_red_on_every_stock() {
        for stock in FilmStock::ALL {
            let sc = curves_for(*stock);
            let log18 = 0.18f32.log10();
            let gamma = |ch: usize| {
                let at = |x: f32| sc.density_at(ch, x);
                (at(log18 + 0.35) - at(log18 - 0.35)) / 0.7
            };
            let (r, b) = (gamma(0), gamma(2));
            assert!(
                b > r * 1.05,
                "{}: blue gamma {b:.3} is not meaningfully steeper than red {r:.3}",
                stock.as_str()
            );
        }
    }
}
