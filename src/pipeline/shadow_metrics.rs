//! Test-only diagnostic harness for `algo/reference-anchored-sigmoid`.
//!
//! **Why this is test-only and in-crate.** The SDR and HDR renderers are not
//! CLI-reachable — only `legacy`, `film-master` and `ultra-hdr-v1` presets parse, and
//! [`crate::pipeline::sdr`] / [`crate::pipeline::hdr`] are pure stages awaiting
//! `output/presets`. `nc` also has no `[lib]` target, so an integration test in
//! `tests/` could only drive the binary. A `#[cfg(test)]` module is therefore the only
//! way to measure the real render chain, and it keeps this diagnostic out of the
//! shipped binary and adds no product surface that `output/presets` would have to undo.
//!
//! **Discipline.** Every entry point is `#[ignore]`d and skips with a clear message when
//! the assets are absent, so `cargo test` stays green on a machine with no
//! `../nc-assets` and CI never needs them. Only *derived numbers* are printed —
//! coordinates, percentiles, counts — never pixels.
//!
//! Run the patch proposal with:
//!
//! ```text
//! cargo test --release shadow_metrics::propose -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use crate::algo::density::to_density;
use crate::types::{DensityParams, FilmBase};

/// Decode budget for the harness. It bypasses the CLI's `memory::preflight`, so it
/// states its own ceiling rather than inheriting one — matching the shipped 6 GiB
/// default so a frame that converts normally also measures here.
const DECODE_BUDGET_BYTES: u64 = 6 * 1024 * 1024 * 1024;

/// Fraction trimmed from each edge before tiling. Real scans are laid out
/// `dark holder → thin inset rebate → picture` (CLAUDE.md), so a patch taken too close
/// to the edge can land on the rebate — which renders *exactly* at the curve's floor
/// and would silently pass for "deep shadow". 12% per side is deliberately
/// conservative: it costs picture area rather than risking a contaminated patch.
const INTERIOR_INSET: f32 = 0.12;

/// Tiles across the interior's long and short axis.
///
/// **Sized deliberately small.** An earlier 12 × 8 grid produced 328 × 342 patches —
/// 6.3 % × 9.5 % of the frame — and the user review showed they routinely straddled
/// several objects ("dark branch *and* distant forest", "2/3 shadow *and* background
/// forest", "all three are a forest/sky mix"), which makes a patch's *semantics*
/// unstatable. 32 × 22 gives ~123 × 124 patches: about a quarter of the area, small
/// enough to sit on one surface. Override with `NC_TILES=<x>x<y>` when a frame needs a
/// different granularity.
const TILES_X_DEFAULT: u32 = 32;
const TILES_Y_DEFAULT: u32 = 22;

/// Minimum separation between reported candidates, in tiles (Chebyshev distance).
///
/// With small tiles the top-ranked candidates are otherwise near-duplicates of one
/// another — adjacent tiles on the same surface — so the "top 3" would offer one choice,
/// not three. Suppressing neighbours makes them genuinely distinct alternatives.
const MIN_SEPARATION_TILES: u32 = 3;

fn tile_grid() -> (u32, u32) {
    match std::env::var("NC_TILES").ok().and_then(|s| {
        let (x, y) = s.split_once('x')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    }) {
        Some((x, y)) if x > 0 && y > 0 => (x, y),
        _ => (TILES_X_DEFAULT, TILES_Y_DEFAULT),
    }
}

/// The three fixture rolls, locked with the user. Each names its frozen recipe stem
/// under `scripts/real-scan-verify/recipes/` — the recipe supplies the roll's `Dmin`
/// and `Dmax`, so a patch proposal is measured in the same density domain the baseline
/// will use.
const FIXTURES: &[(&str, &str)] = &[
    ("2026-07-24-Gold200", "2026-07-24-Gold200"),
    ("Ektar", "Ektar"),
    ("Portra160-2026-07-22", "Portra160-2026-07-22"),
];

/// The assets root, or `None` when it is absent (the skip case).
fn assets_root() -> Option<PathBuf> {
    let root = std::env::var("NC_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("../nc-assets"));
    root.join("manifest.json").is_file().then_some(root)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The roll's frozen `Dmin` and `Dmax`, read from the committed recipe.
///
/// Parsed as loose JSON rather than through `cli`'s recipe types: this only needs two
/// values, and going through the full resolver would couple a diagnostic to the CLI's
/// merge semantics. It fails loudly — a missing key here means the recipe was not
/// frozen as expected, which must not be papered over with a default.
fn frozen_reference(recipe: &Path) -> (FilmBase, f32) {
    let text =
        std::fs::read_to_string(recipe).unwrap_or_else(|e| panic!("{}: {e}", recipe.display()));
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", recipe.display()));

    let base = v["film_base"]["source"]["explicit"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: film_base.source.explicit missing", recipe.display()));
    let rgb: Vec<f32> = base.iter().map(|x| x.as_f64().unwrap() as f32).collect();
    assert_eq!(rgb.len(), 3, "{}: film base is not RGB", recipe.display());

    let dmax = v["reconstruction"]["curve"]["dmax"]["explicit"]
        .as_f64()
        .unwrap_or_else(|| panic!("{}: curve.dmax.explicit missing", recipe.display()))
        as f32;

    (
        FilmBase {
            r: rgb[0],
            g: rgb[1],
            b: rgb[2],
        },
        dmax,
    )
}

/// Frames of one roll grouped by the manifest's `role`.
///
/// Reading roles from the manifest rather than globbing the directory is load-bearing:
/// a patch proposal over a *leader* or *unexposed* frame is meaningless (a leader is a
/// near-uniform field at ~`Dmax`; an unexposed frame is the base at `D′ ≈ 0`), and an
/// early version of this harness silently ranked "shadow" and "diffuse white" tiles on
/// both.
struct Roles {
    real: Vec<PathBuf>,
    leader: Vec<PathBuf>,
    unexposed: Vec<PathBuf>,
}

fn roles(assets: &Path, roll: &str) -> Roles {
    let path = assets.join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let m: serde_json::Value = serde_json::from_str(&text).unwrap();
    let frames = m["rolls"][roll]["frames"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: rolls.{roll}.frames missing", path.display()));

    let mut out = Roles {
        real: vec![],
        leader: vec![],
        unexposed: vec![],
    };
    for f in frames {
        let rel = f["file"].as_str().unwrap();
        let p = assets.join(rel);
        match f["role"].as_str().unwrap_or("real") {
            "leader" => out.leader.push(p),
            "unexposed" => out.unexposed.push(p),
            _ => out.real.push(p),
        }
    }
    out.real.sort();
    out.leader.sort();
    out.unexposed.sort();
    out
}

/// Nearest-rank percentile of an already-sorted slice. Order-statistic, so it is
/// tie-order independent and therefore deterministic.
fn pct(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

/// One candidate tile's derived statistics. No pixels, only numbers.
struct Tile {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Median of the per-pixel scalar tone `D̄` inside the tile.
    p50: f32,
    /// Low and high tone percentiles — the tile's own spread.
    p05: f32,
    p95: f32,
    /// Texture proxy: the p95−p05 spread of the scalar tone. A flat black hole and a
    /// textured deep shadow have the same median; only the spread separates them.
    spread: f32,
}

/// Per-pixel scalar tone `D̄`: the mean of the *finite* channels, matching the domain
/// `density::regional_balance` uses. A non-finite channel is excluded from the mean
/// rather than poisoning it, but a wholly non-finite pixel yields `NaN` and is dropped
/// by the callers' finite filter — so corrupt input cannot masquerade as a valid patch.
fn tone(px: &[f32]) -> f32 {
    let (sum, n) = px
        .iter()
        .filter(|v| v.is_finite())
        .fold((0.0f32, 0u32), |(s, n), v| (s + v, n + 1));
    if n == 0 { f32::NAN } else { sum / n as f32 }
}

/// Tile the frame interior and compute each tile's tone statistics.
fn tiles(density: &[f32], width: u32, height: u32) -> Vec<Tile> {
    let (tiles_x, tiles_y) = tile_grid();
    let inset_x = (width as f32 * INTERIOR_INSET) as u32;
    let inset_y = (height as f32 * INTERIOR_INSET) as u32;
    let iw = width - 2 * inset_x;
    let ih = height - 2 * inset_y;
    let tw = iw / tiles_x;
    let th = ih / tiles_y;

    let mut out = Vec::with_capacity((tiles_x * tiles_y) as usize);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let x0 = inset_x + tx * tw;
            let y0 = inset_y + ty * th;
            let mut tones = Vec::with_capacity((tw * th) as usize);
            for y in y0..y0 + th {
                for x in x0..x0 + tw {
                    let i = ((y as usize * width as usize) + x as usize) * 3;
                    let t = tone(&density[i..i + 3]);
                    if t.is_finite() {
                        tones.push(t);
                    }
                }
            }
            tones.sort_by(f32::total_cmp);
            let (p05, p50, p95) = (pct(&tones, 0.05), pct(&tones, 0.50), pct(&tones, 0.95));
            out.push(Tile {
                x: x0,
                y: y0,
                w: tw,
                h: th,
                p50,
                p05,
                p95,
                spread: p95 - p05,
            });
        }
    }
    out
}

/// Print the top `n` *spatially distinct* candidates, as paste-ready `x,y,w,h` plus stats.
///
/// Non-maximum suppression matters here: with small tiles the top-ranked cells are
/// near-duplicates of each other on one surface, so an unfiltered "top 3" would offer a
/// single choice dressed as three. Expects `cands` sorted best-first.
fn report(label: &str, cands: Vec<&Tile>, n: usize, dmax: f32) {
    println!("    {label}:");
    let mut picked: Vec<&Tile> = Vec::with_capacity(n);
    for t in cands {
        if picked.len() >= n {
            break;
        }
        let too_close = picked.iter().any(|p| {
            let dx = p.x.abs_diff(t.x) / t.w.max(1);
            let dy = p.y.abs_diff(t.y) / t.h.max(1);
            dx < MIN_SEPARATION_TILES && dy < MIN_SEPARATION_TILES
        });
        if !too_close {
            picked.push(t);
        }
    }
    for t in picked {
        println!(
            "      {:>5},{:>5},{:>4},{:>4}   D'p50 {:>7.4}  p05 {:>7.4}  p95 {:>7.4}  spread {:>6.4}  ({:>5.1}% of Dmax)",
            t.x,
            t.y,
            t.w,
            t.h,
            t.p50,
            t.p05,
            t.p95,
            t.spread,
            100.0 * t.p50 / dmax
        );
    }
}

/// Phase 1: propose shadow / mid-tone / diffuse-white patch candidates and per-frame
/// exposure indicators, for the user to confirm against the actual images.
///
/// Ranking rules, stated so the output is reproducible rather than taste:
/// - **shadow** — lowest `D̄` median, requiring spread above the frame's median spread
///   so a flat black hole cannot win over textured shadow. Note the polarity: low `D′`
///   is *near the base*, i.e. the darkest part of the scene.
/// - **diffuse white** — highest `D̄` median **excluding the top two tiles**, which are
///   where a specular or a light source lands; a diffuse white must also be textured.
/// - **mid-tone** — median `D̄` closest to the frame's own median.
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn propose_patches() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json (set NC_ASSETS to override)");
        return;
    };
    let recipes = repo_root().join("scripts/real-scan-verify/recipes");
    let params = DensityParams::default();

    for (roll, stem) in FIXTURES {
        let (base, dmax) = frozen_reference(&recipes.join(format!("{stem}.json")));
        println!(
            "\n=== {roll}  Dmin=({:.6}, {:.6}, {:.6})  Dmax={dmax:.6}",
            base.r, base.g, base.b
        );

        // `real` frames only — see `Roles`.
        let frames = roles(&assets, roll).real;

        for frame in &frames {
            let name = frame.file_name().unwrap().to_string_lossy().to_string();
            let (image, _info) = match crate::io::decode::decode_within(frame, DECODE_BUDGET_BYTES)
            {
                Ok(v) => v,
                Err(e) => {
                    println!("  {name}: DECODE FAILED: {e}");
                    continue;
                }
            };
            let d = to_density(&image, &base, &params);
            let (w, h) = (d.width, d.height);

            // Frame-wide exposure indicators over the same interior the tiles use, so
            // the label and the candidates describe one region.
            let ts = tiles(&d.density, w, h);
            let mut all: Vec<f32> = ts.iter().map(|t| t.p50).collect();
            all.sort_by(f32::total_cmp);
            let frame_p50 = pct(&all, 0.50);
            let mut spreads: Vec<f32> = ts.iter().map(|t| t.spread).collect();
            spreads.sort_by(f32::total_cmp);
            let median_spread = pct(&spreads, 0.50);

            println!(
                "  {name}  {w}x{h}  interior tile D'p50: min {:.4} / med {:.4} / max {:.4}   median spread {:.4}",
                pct(&all, 0.0),
                frame_p50,
                pct(&all, 1.0),
                median_spread
            );

            let textured: Vec<&Tile> = ts.iter().filter(|t| t.spread >= median_spread).collect();

            let mut shadow = textured.clone();
            shadow.sort_by(|a, b| a.p50.total_cmp(&b.p50));
            report("shadow (textured, lowest D')", shadow.clone(), 3, dmax);

            let mut bright = textured.clone();
            bright.sort_by(|a, b| b.p50.total_cmp(&a.p50));
            // Drop the top two: specular highlights and light sources live there, and a
            // diffuse white must be a *diffuse* reflector.
            let diffuse: Vec<&Tile> = bright.into_iter().skip(2).collect();
            report(
                "diffuse white (textured, high D', top 2 dropped)",
                diffuse.clone(),
                3,
                dmax,
            );

            let mut mid: Vec<&Tile> = ts.iter().collect();
            mid.sort_by(|a, b| {
                (a.p50 - frame_p50)
                    .abs()
                    .total_cmp(&(b.p50 - frame_p50).abs())
            });
            report("mid-tone (nearest frame median D')", mid.clone(), 3, dmax);

            // The Δ the datasheet predicts at 0.36 (0.40 for Gold 200). Printed for
            // orientation only, and it must NOT be read as Check A: the auto-proposed
            // "mid-tone" is the frame's *median tile*, which is not a mid-grey surface,
            // and the "diffuse white" is the brightest textured tile, which need not be
            // a diffuse reflector. Check A requires semantically confirmed patches —
            // exactly what the user confirmation step exists to supply.
            if let (Some(m), Some(w)) = (mid.first(), diffuse.first()) {
                println!(
                    "      [orientation only, NOT Check A] mid→white Δ = {:.4}  (datasheet aim 0.36 / 0.40)",
                    w.p50 - m.p50
                );
            }
        }
    }
}

/// Characterise the roll's *reference* frames — leader and unexposed — rather than
/// trusting the single centre-region scalar the freeze stage recorded.
///
/// Purpose (plan Evidence D): a leader whose density is non-uniform, or shaped
/// differently from roll to roll, is further evidence that the number records the
/// *loading* rather than the film. Reports per-channel interior percentiles plus a
/// crude left↔right and top↔bottom split, which is enough to see a fogging gradient.
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn characterise_reference_frames() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json (set NC_ASSETS to override)");
        return;
    };
    let recipes = repo_root().join("scripts/real-scan-verify/recipes");
    let params = DensityParams::default();

    for (roll, stem) in FIXTURES {
        let (base, dmax) = frozen_reference(&recipes.join(format!("{stem}.json")));
        let r = roles(&assets, roll);
        println!("\n=== {roll}  frozen Dmax={dmax:.6}");

        for (kind, list) in [("leader", &r.leader), ("unexposed", &r.unexposed)] {
            for frame in list.iter() {
                let name = frame.file_name().unwrap().to_string_lossy().to_string();
                let Ok((image, _)) = crate::io::decode::decode_within(frame, DECODE_BUDGET_BYTES)
                else {
                    println!("  {kind:10} {name}: DECODE FAILED");
                    continue;
                };
                let d = to_density(&image, &base, &params);
                let ts = tiles(&d.density, d.width, d.height);

                let mut p50s: Vec<f32> = ts.iter().map(|t| t.p50).collect();
                p50s.sort_by(f32::total_cmp);
                let mut spreads: Vec<f32> = ts.iter().map(|t| t.spread).collect();
                spreads.sort_by(f32::total_cmp);

                // Gradient: mean tile p50 of each half. A fogged-from-one-edge leader
                // shows up here even when the overall range looks tight.
                let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
                let (tiles_x, tiles_y) = tile_grid();
                let mid_x = tiles_x / 2;
                let mid_y = tiles_y / 2;
                let (mut l, mut rt, mut top, mut bot) = (vec![], vec![], vec![], vec![]);
                for (i, t) in ts.iter().enumerate() {
                    let (tx, ty) = (i as u32 % tiles_x, i as u32 / tiles_x);
                    if tx < mid_x {
                        l.push(t.p50)
                    } else {
                        rt.push(t.p50)
                    }
                    if ty < mid_y {
                        top.push(t.p50)
                    } else {
                        bot.push(t.p50)
                    }
                }
                println!(
                    "  {kind:10} {name}  D' tiles: min {:.4} med {:.4} max {:.4} (range {:.4})  \
                     median in-tile spread {:.4}\n              gradient: L−R {:+.4}  T−B {:+.4}   \
                     med/Dmax {:.1}%",
                    p50s[0],
                    pct(&p50s, 0.5),
                    p50s[p50s.len() - 1],
                    p50s[p50s.len() - 1] - p50s[0],
                    pct(&spreads, 0.5),
                    mean(&l) - mean(&rt),
                    mean(&top) - mean(&bot),
                    100.0 * pct(&p50s, 0.5) / dmax,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank_and_handles_edges() {
        let v = [0.0f32, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(pct(&v, 0.0), 0.0);
        assert_eq!(pct(&v, 1.0), 4.0);
        assert_eq!(pct(&v, 0.5), 2.0);
        assert!(pct(&[], 0.5).is_nan());
    }

    #[test]
    fn tone_excludes_non_finite_channels_but_not_whole_pixels() {
        // Approximate, because a 3-way f32 mean is not exact (0.3+0.6+0.9 → 0.59999996).
        let near = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(near(tone(&[0.3, 0.6, 0.9]), 0.6));
        // One bad channel: averaged from the survivors rather than poisoned.
        assert!(near(tone(&[0.4, f32::NAN, 0.6]), 0.5));
        // Wholly non-finite stays NaN so the finite filter drops it.
        assert!(tone(&[f32::NAN, f32::INFINITY, f32::NAN]).is_nan());
    }

    #[test]
    fn tiles_stay_inside_the_interior() {
        // A synthetic gradient is enough: this pins geometry, not statistics.
        let (w, h) = (240u32, 160u32);
        let density: Vec<f32> = (0..(w * h * 3)).map(|i| (i % 97) as f32 / 97.0).collect();
        let ts = tiles(&density, w, h);
        let (gx, gy) = tile_grid();
        assert_eq!(ts.len() as u32, gx * gy);
        let inset_x = (w as f32 * INTERIOR_INSET) as u32;
        let inset_y = (h as f32 * INTERIOR_INSET) as u32;
        for t in &ts {
            assert!(t.x >= inset_x, "tile crosses the left inset");
            assert!(t.y >= inset_y, "tile crosses the top inset");
            assert!(t.x + t.w <= w - inset_x, "tile crosses the right inset");
            assert!(t.y + t.h <= h - inset_y, "tile crosses the bottom inset");
        }
    }

    #[test]
    fn spread_separates_flat_from_textured_at_equal_median() {
        // Two tiles with the same median but different spread must rank differently —
        // the property that keeps a flat black hole from winning "textured shadow".
        // Odd length so the nearest-rank median is the exact middle element, and a
        // symmetric ramp so both tiles share it.
        const N: usize = 65;
        let flat = vec![0.5f32; N];
        let textured: Vec<f32> = (0..N)
            .map(|i| 0.2 + 0.6 * i as f32 / (N - 1) as f32)
            .collect();
        let mut a = flat;
        let mut b = textured;
        a.sort_by(f32::total_cmp);
        b.sort_by(f32::total_cmp);
        assert!(
            (pct(&a, 0.50) - pct(&b, 0.50)).abs() < 1e-6,
            "medians must match"
        );
        assert!(pct(&b, 0.95) - pct(&b, 0.05) > pct(&a, 0.95) - pct(&a, 0.05));
    }
}
