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
//! **Discipline.** Every **asset-dependent** entry point is `#[ignore]`d and skips with
//! a clear message when the assets are absent, so `cargo test` stays green on a machine
//! with no `../nc-assets` and CI never needs them. Only *derived numbers* are printed —
//! coordinates, percentiles, counts — never pixels. The two `mod tests` / `mod
//! window_tests` blocks are the deliberate exception: they are synthetic unit tests of
//! this harness's own helpers, touch no asset, and run normally.
//!
//! Run the patch proposal with:
//!
//! ```text
//! cargo test --release shadow_metrics::propose -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use crate::algo::density::to_density;
use crate::pipeline::display_tone::{DisplayTone, Headroom};
use crate::types::{
    DensityCurve, DensityParams, DmaxSource, FilmBase, LinearImage, PrintParams, Reconstruction,
    SigmoidParams,
};

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
/// A rendered SDR sample at or above this is treated as saturated — highlight separation
/// compressed against display white. Deliberately not `1.0`: under a **bounded** tone
/// `sdr::render` returns `[0, 1]`, so an equality test against `1.0` would only find
/// samples the shoulder mapped exactly to the ceiling, missing the flattened
/// neighbourhood just below it that is the actual loss.
///
/// The reasoning survives the unbounded tone unchanged, for two reasons: its one
/// consumer (`measure_candidates`) renders with `DisplayTone::shoulder`, and the probes
/// that do use `ExtendedReinhard` measure the **delivered** image, i.e. clamped to
/// display range, which is what `io::encode` writes. A `>=` test is what makes both
/// cases read correctly — an over-range sample counts as saturated, which it is.
const SATURATED_AT: f32 = 0.999;

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

/// Tile the frame interior and compute each tile's tone statistics from the mean of a
/// pixel's finite channels — the right reducer when judging a *patch* as one object.
fn tiles(density: &[f32], width: u32, height: u32) -> Vec<Tile> {
    tiles_of(density, width, height, None)
}

/// Per-channel variant: statistics from channel `ch` alone.
///
/// Required by [`characterise_reference_frames`], which asks whether a leader is *uniform*.
/// The scalar mean cannot answer that: a coloured fogging gradient whose channels move in
/// opposite directions **cancels** in the mean, so a non-uniform leader would read as
/// uniform. That is not hypothetical in this data — the same-stock base comparison showed
/// green and blue drifting ~+0.02 between scan sessions while red did not move with them.
fn channel_tiles(density: &[f32], width: u32, height: u32, ch: usize) -> Vec<Tile> {
    tiles_of(density, width, height, Some(ch))
}

/// `channel = None` reduces each pixel with [`tone`]; `Some(c)` reads channel `c` directly.
fn tiles_of(density: &[f32], width: u32, height: u32, channel: Option<usize>) -> Vec<Tile> {
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
                    let t = match channel {
                        None => tone(&density[i..i + 3]),
                        Some(c) => density[i + c],
                    };
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

                // Per channel, not on the scalar mean: channels that fog in opposite
                // directions cancel in a mean and a non-uniform leader would print as
                // uniform. The R row also carries the datasheet comparison, since the
                // published aim densities are red-channel Status M figures.
                println!("  {kind:10} {name}");
                for (ch, label) in [(0usize, 'R'), (1, 'G'), (2, 'B')] {
                    let ts = channel_tiles(&d.density, d.width, d.height, ch);
                    let mut p50s: Vec<f32> = ts.iter().map(|t| t.p50).collect();
                    p50s.sort_by(f32::total_cmp);
                    let mut spreads: Vec<f32> = ts.iter().map(|t| t.spread).collect();
                    spreads.sort_by(f32::total_cmp);

                    // Gradient: mean tile p50 of each half. A fogged-from-one-edge leader
                    // shows up here even when the overall range looks tight.
                    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
                    let (tiles_x, tiles_y) = tile_grid();
                    let (mid_x, mid_y) = (tiles_x / 2, tiles_y / 2);
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
                        "    {label}  D' tiles: min {:.4} med {:.4} max {:.4} (range {:.4})  \
                         median in-tile spread {:.4}\n       gradient: L−R {:+.4}  T−B {:+.4}   \
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
    fn channel_tiles_see_a_gradient_the_scalar_mean_hides() {
        // The exact failure the per-channel pass exists to catch: R rising left→right while
        // B falls by the same amount. The mean is flat everywhere, so `tiles()` reports a
        // uniform field; the per-channel view must show equal and opposite gradients.
        let (w, h) = (240u32, 160u32);
        let mut density = vec![0.0f32; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let t = x as f32 / (w - 1) as f32; // 0 → 1 across the frame
                let i = ((y as usize * w as usize) + x as usize) * 3;
                density[i] = 0.5 + t; // R rises
                density[i + 1] = 0.5; // G flat
                density[i + 2] = 0.5 - t; // B falls
            }
        }
        let half_gradient = |ts: &[Tile]| {
            let (tiles_x, _) = tile_grid();
            let mid_x = tiles_x / 2;
            let (mut l, mut r) = (vec![], vec![]);
            for (i, t) in ts.iter().enumerate() {
                if i as u32 % tiles_x < mid_x {
                    l.push(t.p50)
                } else {
                    r.push(t.p50)
                }
            }
            let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
            mean(&l) - mean(&r)
        };
        // The scalar mean cancels: every pixel averages to 0.5.
        assert!(
            half_gradient(&tiles(&density, w, h)).abs() < 1e-6,
            "the scalar mean should hide this gradient — that is the premise"
        );
        let (r, g, b) = (
            half_gradient(&channel_tiles(&density, w, h, 0)),
            half_gradient(&channel_tiles(&density, w, h, 1)),
            half_gradient(&channel_tiles(&density, w, h, 2)),
        );
        assert!(
            r < -0.3,
            "R gradient should be strongly negative (L<R), got {r}"
        );
        assert!(
            b > 0.3,
            "B gradient should be strongly positive (L>R), got {b}"
        );
        assert!(g.abs() < 1e-6, "G is flat, got {g}");
        assert!((r + b).abs() < 1e-6, "equal and opposite, got {r} and {b}");
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

// ---------------------------------------------------------------------------
// Phase 3: measure the candidate anchoring forms against the frozen fixtures.
// ---------------------------------------------------------------------------

/// Every candidate reduces to **one number**: the sigmoid's anchor `A` (the `curve.dmax`
/// value), plus a contrast. That is the whole reason no new curve code is needed here —
/// `t = contrast·(D′ − A)` is unchanged and only the rule for choosing `A` differs.
///
/// - white pinned at `W`            ⇒ `A = W`
/// - mid pinned at `M` → 0.18       ⇒ `A = M + 0.745/contrast`  (since `10^(c(M−A)) = 0.18`)
/// - black pinned at `T` (at `D′=0`) ⇒ `A = −log10(T)/contrast`
///
/// `MID_OUTPUT_DECADES` is `−log10(0.18)`: how far below display white mid-grey sits.
const MID_OUTPUT_DECADES: f32 = 0.744_727_5;

/// Datasheet mid-grey **above base**, per stock — **provisional**: derived from a
/// chart-read `D-min`, which PR #68 established is not a true Status M density. Used for
/// the reference-driven candidate; the *form* is what is under test, not these values.
fn datasheet_mid_above_base(roll: &str) -> f32 {
    match roll {
        "Ektar" => 0.62,
        "Portra160-2026-07-22" => 0.67,
        "2026-07-24-Gold200" => 0.73,
        other => panic!("no datasheet mid-grey recorded for {other}"),
    }
}

/// Datasheet mid→diffuse-white Δ per stock. `contrast = 0.745/Δ` follows from wanting a
/// mid-grey Δ below white to land exactly at 0.18.
fn datasheet_delta(roll: &str) -> f32 {
    match roll {
        "2026-07-24-Gold200" => 0.40,
        _ => 0.36,
    }
}

/// One candidate: a label, whether it could ever ship, and the resolved (anchor, contrast).
#[derive(Clone, Copy)]
struct Candidate {
    label: &'static str,
    /// Whether this form could be the *default*. A `false` here does not mean "rejected" —
    /// the content-driven forms are legitimate as an **explicit opt-in mode**
    /// (`algo/content-aware-sigmoid-toe`); they simply cannot be the default, because
    /// deriving the anchor from frame content silently corrects exposure.
    default_eligible: bool,
    /// The anchor rule. `Auto` is the shipped content-driven measurement
    /// (`DmaxSource::Auto`, the 99.5th percentile of corrected densities) — resolved per
    /// frame by `reconstruct`, so its value is read back from the report rather than
    /// computed here.
    anchor: AnchorRule,
    contrast: f32,
    /// Toe and shoulder knee widths. The Phase-3 candidates all used `0.2`/`0.2`; the
    /// Tier-2 forms added for `algo/exponential-anchor-placement` vary the toe (the
    /// question is whether a toe can pull the film base to black without moving mid or
    /// white) and use the shipped `0.6` shoulder.
    toe: f32,
    shoulder: f32,
    /// `print.black_point` — a constant subtracted by the shared display stage. Zero for
    /// every Phase-3 form. Added because the Tier-2 run showed a **wider toe raises the
    /// floor** (it softens the approach to black from above), so the toe cannot be what
    /// pulls the film base down; a black-point subtraction can, and it is display
    /// adaptation rather than a change to the film rendering.
    black_point: f32,
    /// `true` renders the **exponential** straight line instead of the sigmoid. Added for
    /// `algo/exponential-anchor-placement`: the Phase-3 set was sigmoid-only, so the task's
    /// own curve had never been through this harness. The exponential has no toe or
    /// shoulder, so `toe`/`shoulder` are ignored for these entries.
    exponential: bool,
    /// `print.highlight_compress` — moves the display shoulder's knee. `shoulder_start` is
    /// `0.5 + 0.25/(1+hc)`, so hc=0 puts it at 0.75 (latest, least compression) and large
    /// hc drives it toward 0.5 (earliest, most room for over-range content).
    hc: f32,
}

#[derive(Clone, Copy)]
enum AnchorRule {
    Explicit(f32),
    Auto,
}

/// Build the candidate set for one frame.
///
/// Two corrections over the first version, both from user review:
///
/// - **The content-driven forms use the shipped `DmaxSource::Auto`**, not a semantically
///   confirmed white patch. Requiring a *valid* white was incoherent: a content-driven mode
///   has no knowledge of what is a real white — it measures the brightest content and
///   adapts. Gating on validity also meant they resolved on 2 frames only, making their
///   statistics useless. `Auto` is exactly that measurement and is already shipped.
/// - **Black-pinning is tested at targets consistent with the contrast.** The first attempt
///   used NLP's 0.00061 at contrast 2.0, which implies an anchor of 1.607 — above every
///   roll's Dmax, so nothing reached white and the whole frame rendered dark. That rejected
///   my parameter, not the form. 0.002 and 0.005 give anchors of 1.349 and 1.151.
fn candidates(roll: &str, roll_dmax: f32) -> Vec<Candidate> {
    let ds_c = MID_OUTPUT_DECADES / datasheet_delta(roll);
    // mid pinned at `m` -> 0.18  =>  A = m + 0.745/c
    let mid = |m: f32, c: f32| AnchorRule::Explicit(m + MID_OUTPUT_DECADES / c);
    // black pinned at `t` at D'=0  =>  A = -log10(t)/c
    let black = |t: f32, c: f32| AnchorRule::Explicit(-t.log10() / c);
    vec![
        Candidate {
            label: "1  white@Dmax, c=1.0 (shipped)",
            default_eligible: true,
            anchor: AnchorRule::Explicit(roll_dmax),
            contrast: 1.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "2  white@Dmax, c=2.0",
            default_eligible: true,
            anchor: AnchorRule::Explicit(roll_dmax),
            contrast: 2.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "3  mid@0.5*Dmax, c=2.0",
            default_eligible: true,
            anchor: mid(0.5 * roll_dmax, 2.0),
            contrast: 2.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "4  auto (content-driven), c=2.0",
            default_eligible: false,
            anchor: AnchorRule::Auto,
            contrast: 2.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "5a black@0.002, c=2.0",
            default_eligible: true,
            anchor: black(0.002, 2.0),
            contrast: 2.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "5b black@0.005, c=2.0",
            default_eligible: true,
            anchor: black(0.005, 2.0),
            contrast: 2.0,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "7  auto (content-driven), c=0.745/D",
            default_eligible: false,
            anchor: AnchorRule::Auto,
            contrast: ds_c,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "8  mid@Dmin+datasheet, c=0.745/D",
            default_eligible: true,
            anchor: mid(datasheet_mid_above_base(roll), ds_c),
            contrast: ds_c,
            toe: 0.2,
            shoulder: 0.2,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        // ---- Tier 2, `algo/exponential-anchor-placement` (2026-08-27) ----
        // These use the **shipped** shoulder (0.6), unlike the Phase-3 set above, because
        // the question is no longer "which anchoring form" but "can a toe close the black
        // gap a correctly-placed mid leaves behind". `D` is the shipped default for
        // reference; `M` varies the toe; `F` tests the corrected mid fraction.
        Candidate {
            label: "D  SHIPPED: mid@0.5*Dmax, c=2.069, toe .2",
            default_eligible: true,
            anchor: mid(0.5 * roll_dmax, crate::types::REFERENCE_CONTRAST),
            contrast: crate::types::REFERENCE_CONTRAST,
            toe: 0.2,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "F  mid@0.393*Dmax, c=2.069, toe .2",
            default_eligible: true,
            anchor: mid(0.393 * roll_dmax, crate::types::REFERENCE_CONTRAST),
            contrast: crate::types::REFERENCE_CONTRAST,
            toe: 0.2,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "M1 mid@base+0.508, c=2.03, toe .2",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.2,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "M2 mid@base+0.508, c=2.03, toe .4",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.4,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "M3 mid@base+0.508, c=2.03, toe .6",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.6,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 0.0,
        },
        Candidate {
            label: "B1 mid@base+0.508, toe .2, blackpt .019",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.2,
            shoulder: 0.6,
            black_point: 0.019,
            exponential: false,
            hc: 0.0,
        },
        // ---- the exponential, this task's own curve. No toe or shoulder exists on it,
        // so highlights are not compressed — they run past 1.0 and the SDR renderer
        // refuses them, which is itself a result worth seeing.
        Candidate {
            label: "X1 EXPO white@Dmax, g=2.0",
            default_eligible: true,
            anchor: AnchorRule::Explicit(roll_dmax),
            contrast: 2.0,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.0,
            exponential: true,
            hc: 0.0,
        },
        Candidate {
            label: "X2 EXPO black@0.005, g=2.0",
            default_eligible: true,
            anchor: black(0.005, 2.0),
            contrast: 2.0,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.0,
            exponential: true,
            hc: 0.0,
        },
        Candidate {
            label: "X3 EXPO mid@base+0.508, g=2.03",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.0,
            exponential: true,
            hc: 0.0,
        },
        Candidate {
            label: "X4 EXPO mid@base+0.508 + blackpt .019",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.019,
            exponential: true,
            hc: 0.0,
        },
        // Does moving the *display* shoulder's knee rescue a shoulder-less reconstruction?
        Candidate {
            label: "Y1 EXPO mid@base+0.508, hc=1 (knee .625)",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.0,
            exponential: true,
            hc: 1.0,
        },
        Candidate {
            label: "Y2 EXPO mid@base+0.508, hc=4 (knee .55)",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.0,
            shoulder: 0.0,
            black_point: 0.0,
            exponential: true,
            hc: 4.0,
        },
        Candidate {
            label: "Y3 SIGMOID default + hc=1",
            default_eligible: true,
            anchor: mid(0.5 * roll_dmax, crate::types::REFERENCE_CONTRAST),
            contrast: crate::types::REFERENCE_CONTRAST,
            toe: 0.2,
            shoulder: 0.6,
            black_point: 0.0,
            exponential: false,
            hc: 1.0,
        },
        Candidate {
            label: "B2 mid@base+0.508, toe 0, blackpt .019",
            default_eligible: true,
            anchor: mid(0.508, 2.03),
            contrast: 2.03,
            toe: 0.0,
            shoulder: 0.6,
            black_point: 0.019,
            exponential: false,
            hc: 0.0,
        },
    ]
}

/// Median of a sample vector, for the summary table.
fn med3(v: &[f32]) -> f32 {
    let mut t = v.to_vec();
    t.sort_by(f32::total_cmp);
    pct(&t, 0.5)
}

/// Build the tagged curve for a candidate at a resolved contrast.
///
/// The harness computes each candidate's anchor itself, so the value it passes is taken
/// literally (`WhiteAtDmax` over an explicit reference) rather than re-derived by a
/// placement rule — otherwise every measurement in the report would shift.
fn curve_for(cand: &Candidate, contrast: f32) -> DensityCurve {
    let dmax = match cand.anchor {
        AnchorRule::Explicit(a) => DmaxSource::Explicit(a),
        AnchorRule::Auto => DmaxSource::Auto,
    };
    if cand.exponential {
        DensityCurve::Exponential(crate::types::ExponentialParams {
            gamma: contrast,
            dmax,
            anchor: crate::types::AnchorPlacement::WhiteAtDmax,
        })
    } else {
        DensityCurve::Sigmoid(SigmoidParams {
            contrast,
            toe: cand.toe,
            shoulder: cand.shoulder,
            dmax,
            anchor: crate::types::AnchorPlacement::WhiteAtDmax,
        })
    }
}

/// Luminance of a rendered-linear Display P3 pixel, using the **pinned** luma vector.
/// The harness defines no coefficient of its own (CLAUDE.md: import, never restate).
fn p3_luma(px: &[f32]) -> f32 {
    use crate::pipeline::colorimetry::pinned::DISPLAY_P3_LUMA as L;
    L[0] * px[0] + L[1] * px[1] + L[2] * px[2]
}

/// sRGB inverse EOTF, for reporting a floor as an 8-bit code value. Presentation only —
/// bounds are kept on the linear values, since the real transfer runs through lcms2 and is
/// build-dependent (design-spec §8).
fn srgb_encode(x: f32) -> f32 {
    if x <= 0.0 {
        0.0
    } else if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Median luminance inside a rectangle of a rendered image.
fn patch_luma_p50(img: &LinearImage, x: u32, y: u32, w: u32, h: u32) -> f32 {
    let mut v = Vec::with_capacity((w * h) as usize);
    for yy in y..(y + h).min(img.height) {
        for xx in x..(x + w).min(img.width) {
            let i = ((yy as usize * img.width as usize) + xx as usize) * 3;
            let l = p3_luma(&img.rgb[i..i + 3]);
            if l.is_finite() {
                v.push(l);
            }
        }
    }
    v.sort_by(f32::total_cmp);
    pct(&v, 0.50)
}

/// One candidate's per-frame gate samples, accumulated across the fixture frames.
///
/// `mids`/`shadows` carry one entry per frame that has that patch declared valid, so their
/// lengths differ between candidates and from `sats` — which is whole-frame and therefore
/// recorded for every frame.
#[derive(Default)]
struct Gate {
    mids: Vec<f32>,
    shadows: Vec<f32>,
    sats: Vec<f32>,
    /// Where the **film base itself** renders, as an 8-bit code. The shadow patch is only
    /// a proxy for "reaches black" — it is the darkest *confirmed content*, which on some
    /// frames sits well above the base. `D′ = 0` is the true floor the curve produces, and
    /// it is what decides whether a toe is doing its job.
    bases: Vec<f32>,
    /// **Shift-invariant** highlight separation: `p99.9 − p99` of rendered luma, in stops.
    ///
    /// `sats` cannot compare configs that differ in `print.black_point` — it counts samples
    /// above a fixed 0.999, so any downward shift deflates it even when separation is
    /// untouched. A ratio between two high percentiles cancels a uniform gain or offset,
    /// so it measures what the shoulder actually did to highlight detail. Larger = more
    /// separation surviving.
    hisep: Vec<f32>,
    /// Share of samples merged onto the frame's own maximum luma — highlight detail the
    /// shoulder destroyed. Unlike `sats` this survives a black-point shift.
    flat: Vec<f32>,
    /// Share of samples at or above 0.999 **absolute** — highlights blown to white.
    blown: Vec<f32>,
    /// p90 -> p99 separation in 8-bit **code** values rather than linear stops.
    csep: Vec<f32>,
    /// HDR: share of samples that exceed reference white, i.e. that actually use the
    /// headroom. Zero means the HDR rendition is the SDR one and a gain map is inert.
    hdr_above: Vec<f32>,
    /// HDR: the frame's peak, in multiples of reference white. The ceiling is 1000/203
    /// = 4.926; a value pinned there means content is slammed against the top rather
    /// than separated below it.
    hdr_peak: Vec<f32>,
    /// HDR: separation *within* the above-white content, p99.9/p99 in stops. This is the
    /// question "do the speculars survive as detail, or as one flat blob".
    hdr_sep: Vec<f32>,
}

/// Phase 3: for each fixture frame and candidate, report the qualitative gates.
///
/// Gates (per the reduced scope — filter forms, do not tune parameters):
/// - **reaches a plausible black** — the confirmed *shadow* patch's SDR level;
/// - **needs no per-frame correction** — how near the confirmed *mid* patch lands to 0.18
///   with **no exposure applied**, and crucially how much that varies *between* frames;
/// - **does not lose highlights to display white** — the share of rendered SDR samples
///   pressed against the top of the range. NOT a clip count: `sdr::render` *errors* on any
///   sample outside `[0, 1]`, so a returned image has zero out-of-range samples by
///   construction and a clip counter here could only ever print `0.00%`. The honest
///   question is how much content the shoulder has flattened *against* white, which is
///   what `SATURATED_AT` measures and which can genuinely differ between candidates;
/// - **preserves exposure spacing** — the spread of frame medians.
///
/// Invalid patches (per `fixtures.json`) are skipped rather than averaged in.
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture --test-threads=1"]
fn measure_candidates() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();

    let mut marks: Vec<String> = fx["frames"].as_object().unwrap().keys().cloned().collect();
    marks.sort();

    // candidate label -> one sample per frame, per gate.
    let mut agg: std::collections::BTreeMap<String, Gate> = Default::default();

    for mk in &marks {
        let f = &fx["frames"][mk];
        let roll = f["roll"].as_str().unwrap();
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let Ok((image, _)) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) else {
            println!("{mk}: DECODE FAILED");
            continue;
        };

        let rect = |c: &str| -> Option<(u32, u32, u32, u32)> {
            let p = &f["patches"][c];
            p["valid"].as_bool().unwrap_or(false).then(|| {
                (
                    p["x"].as_u64().unwrap() as u32,
                    p["y"].as_u64().unwrap() as u32,
                    p["w"].as_u64().unwrap() as u32,
                    p["h"].as_u64().unwrap() as u32,
                )
            })
        };
        println!(
            "\n=== {mk}  {roll}  Dmax {dmax:.4}   valid patches: shadow {} mid {} white {}",
            rect("shadow").is_some(),
            rect("mid").is_some(),
            rect("white").is_some()
        );
        println!(
            "    {:<42}{:>10}{:>12}{:>11}{:>10}{:>8}{:>9}",
            "candidate", "anchor", "contrast", "mid->0.18", "shadow", "base", "sat%"
        );

        for cand in candidates(roll, dmax) {
            // `[explicit-mode only]` is not a rejection: a content-driven form is a valid
            // opt-in mode (`algo/content-aware-sigmoid-toe`), it just cannot be the default.
            let tag = if cand.default_eligible {
                ""
            } else {
                "  [explicit-mode only]"
            };
            let contrast = cand.contrast;
            let recon = Reconstruction::Density {
                density: DensityParams::default(),
                curve: curve_for(&cand, contrast),
            };
            let print = PrintParams {
                black_point: cand.black_point,
                highlight_compress: cand.hc,
                ..PrintParams::default()
            };
            let (film, report) = crate::algo::reconstruct(&image, &base, &recon).unwrap();
            // For `Auto` the anchor is measured inside `reconstruct`; read it back so the
            // printed value is the one actually used rather than a guess.
            let anchor = report.dmax.unwrap_or(f32::NAN);
            let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
            let shared = crate::pipeline::render_split::display_source(aces, &print).unwrap();
            let sdr = match crate::pipeline::sdr::render(
                &shared,
                crate::pipeline::sdr::SdrGamut::DisplayP3,
                DisplayTone::shoulder(cand.hc).unwrap(),
            ) {
                Ok(v) => v,
                // `sdr::render` errors on any sample outside [0, 1]. A shoulder-less curve
                // can legitimately produce them, so report rather than panic — "this form
                // cannot reach the SDR renderer at all" is a finding, not a harness bug.
                Err(e) => {
                    println!(
                        "    {:<42}{:>10.4}{:>12.3}   SDR REFUSED: {e}",
                        cand.label,
                        match cand.anchor {
                            AnchorRule::Explicit(a) => a,
                            AnchorRule::Auto => f32::NAN,
                        },
                        contrast
                    );
                    continue;
                }
            };
            let img = sdr.image();

            // Saturation, not clipping — see this test's doc comment for why a clip count
            // is structurally impossible on this path. A sample within one part in a
            // thousand of display white has had its highlight separation compressed away
            // even though it is still in range.
            let saturated = img.rgb.iter().filter(|v| **v >= SATURATED_AT).count();
            let sat_pct = 100.0 * saturated as f32 / img.rgb.len() as f32;

            // Highlight separation, shift-invariant (see `Gate::hisep`).
            let hi_sep = {
                let mut lum: Vec<f32> = (0..img.rgb.len() / 3)
                    .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .collect();
                lum.sort_by(f32::total_cmp);
                // p90 -> p99, not p99 -> p99.9: with 6-9% of samples flattened against
                // white, both of the higher percentiles land *inside* the flat region and
                // the ratio is identically 1.0, which measures nothing.
                let (a, b) = (pct(&lum, 0.90), pct(&lum, 0.99));
                let sep = if a > 0.0 { (b / a).log2() } else { f32::NAN };
                // Share of samples merged onto the frame's own maximum. Immune to the
                // fixed 0.999 threshold that made `sat%` unusable across black points:
                // if the shoulder collapsed highlights onto one value they stay merged
                // however far the black point later shifts them.
                let mx = *lum.last().unwrap_or(&0.0);
                let flat = lum.iter().filter(|v| (mx - **v) <= 1e-4 * mx).count();
                // Blown to **absolute** white. `flat` above is relative to the frame's own
                // maximum, which for a config dark enough never to reach 1.0 measures a
                // cluster somewhere in the upper midtones instead of clipping — the reason
                // it disagreed with visual review. This one is what "lost the highlight"
                // actually means.
                let blown = lum.iter().filter(|v| **v >= 0.999).count();
                // Separation in **code** space, not linear stops. sRGB's slope in
                // code-per-log-luminance rises with level, so an equal linear ratio is
                // worth fewer visible code values when the highlights sit lower — which is
                // why a darker config can score better in stops and look worse.
                let csep =
                    srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
                (
                    sep,
                    100.0 * flat as f32 / lum.len() as f32,
                    100.0 * blown as f32 / lum.len() as f32,
                    csep,
                )
            };

            // Where does the film base land? Push a 1x1 image whose sample *is* the base
            // (so `D′ = 0` exactly) through the identical chain. Measured rather than
            // derived, because the chain is not a closed form — the working-space map and
            // the SDR renderer both sit between the curve and the code value.
            let base_level = {
                let probe = LinearImage::new(1, 1, vec![base.r, base.g, base.b], None).unwrap();
                // Pin the anchor the *frame* resolved. A one-pixel image sitting exactly on
                // the base has `D′ = 0` everywhere, so a content-driven `DmaxSource::Auto`
                // would re-measure it as 0 and the sigmoid would rightly refuse to run.
                let mut pinned = cand;
                pinned.anchor = AnchorRule::Explicit(anchor);
                let probe_recon = Reconstruction::Density {
                    density: DensityParams::default(),
                    curve: curve_for(&pinned, contrast),
                };
                let (bf, _) = crate::algo::reconstruct(&probe, &base, &probe_recon).unwrap();
                let ba = crate::pipeline::working_space::map_nc_film_rgb_v1(bf);
                let bs = crate::pipeline::render_split::display_source(ba, &print).unwrap();
                let br = crate::pipeline::sdr::render(
                    &bs,
                    crate::pipeline::sdr::SdrGamut::DisplayP3,
                    DisplayTone::shoulder(cand.hc).unwrap(),
                )
                .unwrap();
                srgb_encode(p3_luma(&br.image().rgb[0..3])) * 255.0
            };

            // The HDR half. Every metric above is SDR, but "speculars live in the
            // headroom" is a claim about *this* rendition — so measure it directly.
            let hdr_stats = crate::pipeline::hdr::render_linear(
                &shared,
                DisplayTone::shoulder(cand.hc).unwrap(),
            )
            .ok()
            .map(|hdr| {
                let him = hdr.image();
                let mut hl: Vec<f32> = (0..him.rgb.len() / 3)
                    .map(|i| {
                        use crate::pipeline::colorimetry::pinned::BT2020_LUMA as L;
                        let p = &him.rgb[i * 3..i * 3 + 3];
                        L[0] * p[0] + L[1] * p[1] + L[2] * p[2]
                    })
                    .filter(|v| v.is_finite())
                    .collect();
                hl.sort_by(f32::total_cmp);
                let above = hl.iter().filter(|v| **v > 1.0).count();
                let (a, b) = (pct(&hl, 0.990), pct(&hl, 0.9999));
                (
                    100.0 * above as f32 / hl.len() as f32,
                    *hl.last().unwrap_or(&0.0),
                    if a > 0.0 { (b / a).log2() } else { 0.0 },
                )
            });

            let mid_s = rect("mid").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));
            let sh_s = rect("shadow").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));
            let key = format!("{}{}", cand.label, tag);
            let e = agg.entry(key).or_default();
            if let Some(m) = mid_s {
                e.mids.push(m);
            }
            if let Some(s) = sh_s {
                e.shadows.push(srgb_encode(s) * 255.0);
            }
            // Whole-frame, so it needs no valid patch — recorded for every frame.
            e.sats.push(sat_pct);
            e.bases.push(base_level);
            if hi_sep.0.is_finite() {
                e.hisep.push(hi_sep.0);
            }
            e.flat.push(hi_sep.1);
            e.blown.push(hi_sep.2);
            e.csep.push(hi_sep.3);
            if let Some((above, peak, sep)) = hdr_stats {
                e.hdr_above.push(above);
                e.hdr_peak.push(peak);
                e.hdr_sep.push(sep);
            }

            println!(
                "    {:<42}{:>10.4}{:>12.3}{:>11}{:>10}{:>8}{:>9.2}",
                cand.label,
                anchor,
                contrast,
                mid_s
                    .map(|m| format!("{m:.4}"))
                    .unwrap_or_else(|| "-".into()),
                sh_s.map(|s| format!("{:.0}/255", srgb_encode(s) * 255.0))
                    .unwrap_or_else(|| "-".into()),
                format!("{base_level:.0}"),
                sat_pct
            );
        }
    }

    println!("\n\n=== GATES ACROSS FRAMES (mid target 0.18; shadow wants a low code value) ===");
    println!(
        "{:<42}{:>11}{:>12}{:>10}{:>9}{:>11}{:>9}{:>10}{:>9}",
        "candidate",
        "|EV| to .18",
        "shadow med",
        "BASE med",
        "blown%",
        "code sep",
        "HDR>1%",
        "HDR peak",
        "HDR sep"
    );
    for (
        label,
        Gate {
            mids,
            shadows,
            bases,
            blown,
            csep,
            hdr_above,
            hdr_peak,
            hdr_sep,
            // Recorded per candidate but not columns of this table: `sats` and `flat`
            // both shift with `print.black_point`, so they cannot compare candidates
            // that differ in it, and `hisep` was superseded by the shift-invariant
            // `csep`. Kept in `Gate` because the per-frame table still prints `sat%`
            // and a rerun should not have to re-measure to bring a column back.
            sats: _,
            flat: _,
            hisep: _,
        },
    ) in &agg
    {
        if mids.is_empty() {
            continue;
        }
        let mut m = mids.clone();
        m.sort_by(f32::total_cmp);
        let med = pct(&m, 0.5);
        let ev = (0.18f32 / med).log2().abs();
        let mut s = shadows.clone();
        s.sort_by(f32::total_cmp);
        println!(
            "{:<42}{:>11.2}{:>12.0}{:>10.0}{:>8.2}%{:>11.3}{:>8.2}%{:>10.3}{:>9.3}",
            label,
            ev,
            pct(&s, 0.5),
            {
                let mut b = bases.clone();
                b.sort_by(f32::total_cmp);
                pct(&b, 0.5)
            },
            {
                let mut t = blown.clone();
                t.sort_by(f32::total_cmp);
                pct(&t, 0.5)
            },
            {
                let mut h = csep.clone();
                h.sort_by(f32::total_cmp);
                pct(&h, 0.5)
            },
            med3(hdr_above),
            med3(hdr_peak),
            med3(hdr_sep)
        );
    }
    println!(
        "\n'BASE med' is where the film base itself renders (D' = 0), the true floor — the\n\
         shadow columns are the darkest *confirmed content*, which can sit well above it."
    );
    println!(
        "\nRead: '|EV| to .18' is the residual fixed offset the default would need — lower is\n\
         more reference-driven."
    );
    println!(
        "The per-frame 'sat%' column above is the share of rendered samples at or above \
         {SATURATED_AT}\n— highlight separation the shoulder has compressed against display \
         white. It is NOT a clip\ncount; see the doc comment."
    );
}

/// Tone-mapping shape probe for `algo/reconstruction-render-curve-split`.
///
/// The display renderers compress with a **fixed-ceiling Hermite knee**, which cannot
/// hold content that overshoots by orders of magnitude: measured 2026-08-28, an
/// unbounded exponential reconstruction put 20.8% of the frame above reference white and
/// every one of those samples landed on the ceiling with *zero* separation, on both the
/// SDR and the HDR rendition. Moving the knee did not help (21.38% -> 21.58%).
///
/// This probe asks the next question: can a tone-mapping **operator** hold that content?
/// It composes candidate operators into the reconstruction curve rather than editing the
/// shipped renderers — mathematically the same shape question, and it keeps the
/// experiment out of the product. Where such an operator should eventually live is an
/// architecture decision this probe does not make.
///
/// It writes one downsampled PPM per operator to `../temp/tonemap-probe/` — a sibling
/// of the checkout, like `../nc-assets`, so nothing lands in the repo. If that
/// directory cannot be created the probe skips rather than failing.
///
/// ```text
/// NC_TONEMAP_FRAME=P3 cargo test --release shadow_metrics::tone_map_probe \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn tone_map_probe() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let mk = std::env::var("NC_TONEMAP_FRAME").unwrap_or_else(|_| "P3".into());
    let f = &fx["frames"][&mk];
    let roll = f["roll"].as_str().unwrap();
    let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
    let base = FilmBase {
        r: base_arr[0].as_f64().unwrap() as f32,
        g: base_arr[1].as_f64().unwrap() as f32,
        b: base_arr[2].as_f64().unwrap() as f32,
    };
    let path = assets
        .join("rolls")
        .join(roll)
        .join(f["file"].as_str().unwrap());
    let (image, _) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES).unwrap();

    // X3's reconstruction: exponential, mid pinned `X3_MID_OFFSET` above the base,
    // unbounded. Taken from the shared constants so retuning X3 moves every probe.
    let contrast = X3_CONTRAST;
    let anchor = x3_reference_anchor();
    // Outside the checkout (see the doc comment). Not writable everywhere, and this is
    // an `--ignored` probe, so a missing sibling directory is a skip, not a failure.
    let outdir = repo_root().join("../temp/tonemap-probe");
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        eprintln!("SKIP: cannot create {}: {e}", outdir.display());
        return;
    }

    // Extended Reinhard: maps input `w` exactly to 1.0, slope 1 at the origin, and
    // approaches the ceiling asymptotically instead of reaching it with zero slope.
    let reinhard = |w: f32| move |v: f32| v * (1.0 + v / (w * w)) / (1.0 + v);
    // Hyperbolic shoulder: identity below `t`, then asymptotic to 1.0 — see its `ops`
    // entry below for why never reaching the ceiling is the point.
    let hyper = |t: f32| {
        move |v: f32| {
            if v <= t {
                v
            } else {
                let w = 1.0 - t;
                t + w * (v - t) / ((v - t) + w)
            }
        }
    };
    // The shipped display shape, for reference: linear to `t`, then a Hermite that hits
    // 1.0 with zero slope — the operator whose ceiling is the problem.
    let hermite = |t: f32| {
        move |v: f32| {
            if v <= t {
                v
            } else {
                let peak = 1.0f32;
                let x = ((v - t) / (peak - t)).min(1.0);
                t + (peak - t) * (x * (1.0 - x * x / 3.0) * 1.5).min(1.0)
            }
        }
    };

    type Op = (String, Box<dyn Fn(f32) -> f32 + Sync>);
    let ops: Vec<Op> = vec![
        ("none (X3 baseline)".into(), Box::new(|v: f32| v)),
        (
            "hermite t=0.75 (shipped shape)".into(),
            Box::new(hermite(0.75)),
        ),
        ("reinhard W=2".into(), Box::new(reinhard(2.0))),
        ("reinhard W=4".into(), Box::new(reinhard(4.0))),
        ("reinhard W=8".into(), Box::new(reinhard(8.0))),
        ("reinhard W=16".into(), Box::new(reinhard(16.0))),
        ("reinhard W=64".into(), Box::new(reinhard(64.0))),
        // Hyperbolic shoulder: identity below `t`, then asymptotic to 1.0 with the width
        // chosen for C1 continuity (`w = 1-t`, so the slope matches at the knee). Unlike
        // the Hermite this never reaches the ceiling, so it keeps spreading content over
        // decades instead of flattening it after one stop; unlike Reinhard it leaves
        // everything below the knee untouched, so midtones do not move.
        ("hyperbolic t=0.5".into(), Box::new(hyper(0.5))),
        ("hyperbolic t=0.7".into(), Box::new(hyper(0.7))),
        ("hyperbolic t=0.85".into(), Box::new(hyper(0.85))),
    ];

    println!("\nframe {mk}  {roll}  {}", f["file"].as_str().unwrap());
    println!("exponential reconstruction, contrast {contrast}, anchor {anchor:.4} (unbounded)");
    println!(
        "\n{:<32}{:>9}{:>10}{:>11}{:>10}",
        "tone map", "mid", "blown%", "code sep", "peak"
    );
    for (label, op) in &ops {
        let mut density = to_density(&image, &base, &DensityParams::default());
        let _ = crate::algo::density::regional_balance(&mut density, &DensityParams::default());
        let film =
            crate::algo::density::apply_curve(density, |d| op(10f32.powf(contrast * (d - anchor))));
        let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
        let shared =
            crate::pipeline::render_split::display_source(aces, &PrintParams::default()).unwrap();
        let Ok(sdr) = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        ) else {
            println!("{label:<32}   SDR REFUSED (samples outside [0,1])");
            continue;
        };
        let img = sdr.image();
        let mut lum: Vec<f32> = (0..img.rgb.len() / 3)
            .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
            .filter(|v| v.is_finite())
            .collect();
        lum.sort_by(f32::total_cmp);
        let blown = 100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
        let csep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
        let mid = rect_of(f, "mid").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));
        println!(
            "{label:<32}{:>9}{blown:>9.2}%{csep:>11.1}{:>10.3}",
            mid.map(|m| format!("{m:.4}")).unwrap_or_else(|| "-".into()),
            lum.last().copied().unwrap_or(0.0)
        );
        write_ppm(&outdir.join(format!("{mk}-{}.ppm", slug(label))), img, 4);
    }
    // Benchmark: the shipped sigmoid on this same frame, for scale. It is a different
    // reconstruction, not a tone map, so it is printed apart from the table above.
    {
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let recon = Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams {
                contrast: crate::types::REFERENCE_CONTRAST,
                toe: 0.2,
                shoulder: 0.6,
                dmax: DmaxSource::Explicit(
                    0.5 * dmax + MID_OUTPUT_DECADES / crate::types::REFERENCE_CONTRAST,
                ),
                anchor: crate::types::AnchorPlacement::WhiteAtDmax,
            }),
        };
        let (film, _) = crate::algo::reconstruct(&image, &base, &recon).unwrap();
        let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
        let shared =
            crate::pipeline::render_split::display_source(aces, &PrintParams::default()).unwrap();
        let sdr = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let img = sdr.image();
        let mut lum: Vec<f32> = (0..img.rgb.len() / 3)
            .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
            .filter(|v| v.is_finite())
            .collect();
        lum.sort_by(f32::total_cmp);
        let blown = 100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
        let csep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
        let mid = rect_of(f, "mid").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));
        println!(
            "{:<32}{:>9}{blown:>9.2}%{csep:>11.1}{:>10.3}   <- BENCHMARK (shipped sigmoid)",
            "[sigmoid D]",
            mid.map(|m| format!("{m:.4}")).unwrap_or_else(|| "-".into()),
            lum.last().copied().unwrap_or(0.0)
        );
        write_ppm(&outdir.join(format!("{mk}-sigmoid-D.ppm")), img, 4);
    }
    println!("\nimages (every 4th pixel) -> {}", outdir.display());
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn rect_of(f: &serde_json::Value, c: &str) -> Option<(u32, u32, u32, u32)> {
    let p = &f["patches"][c];
    p["valid"].as_bool().unwrap_or(false).then(|| {
        (
            p["x"].as_u64().unwrap() as u32,
            p["y"].as_u64().unwrap() as u32,
            p["w"].as_u64().unwrap() as u32,
            p["h"].as_u64().unwrap() as u32,
        )
    })
}

/// Write a decimated 8-bit PPM so the probe's renders can be looked at. Presentation
/// only — the numbers above are the measurement.
fn write_ppm(path: &Path, img: &LinearImage, step: u32) {
    let (w, h) = (img.width / step, img.height / step);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            let i = (((y * step) as usize * img.width as usize) + (x * step) as usize) * 3;
            for c in 0..3 {
                out.push((srgb_encode(img.rgb[i + c].clamp(0.0, 1.0)) * 255.0).round() as u8);
            }
        }
    }
    let _ = std::fs::write(path, out);
}

/// Does removing the display tone curve actually improve the picture?
/// (`output/linear-render`.)
///
/// Renders every `scripts/sigmoid-baseline` fixture frame through the **shipped
/// default** reconstruction — sigmoid, default contrast/toe/shoulder/anchor, the
/// roll's own measured `Dmax` as the reference — and measures the SDR rendition
/// twice: with the Hermite shoulder and with no display tone curve at all. The
/// reconstruction is identical between the two rows, so every difference is the
/// display stage.
///
/// Read `blown%` (share at or above absolute white) with `code sep` (p90→p99 in
/// 8-bit code values); `sat%` from `measure_candidates` is deliberately absent —
/// it shifts with the black point and so cannot compare these two. `mid` is the
/// declared mid patch, printed to confirm the two modes did *not* move midtones,
/// which is the claim that makes the pair comparable at all.
///
/// The HDR columns exist for the second half of the same question: the HDR knee sits
/// at ~3.94, so on a bounded reconstruction it never fires and both modes should
/// report the same peak, while the gain map goes exactly flat.
///
/// ```text
/// cargo test --release shadow_metrics::linear_render_probe -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn linear_render_probe() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();

    println!(
        "\nshipped default reconstruction (sigmoid, roll Dmax as the reference), \
         neutral print controls"
    );
    println!(
        "\n{:<6}{:<10}{:>9}{:>9}{:>10}{:>9}{:>10}{:>10}",
        "frame", "display", "mid", "blown%", "code sep", "peak", "hdr peak", "gain max"
    );

    for (mark, f) in fx["frames"].as_object().unwrap() {
        let roll = f["roll"].as_str().unwrap();
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let (image, _) = match crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) {
            Ok(v) => v,
            Err(e) => {
                println!("{mark:<6}DECODE FAILED: {e}");
                continue;
            }
        };

        // The shipped default curve, roll-calibrated exactly as a user would with
        // `--d-max`: only `dmax` departs from `SigmoidParams::default()`.
        let recon = Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams {
                dmax: DmaxSource::Explicit(f["roll_dmax"].as_f64().unwrap() as f32),
                ..SigmoidParams::default()
            }),
        };
        let print = PrintParams::default();
        let (film, _) = crate::algo::reconstruct(&image, &base, &recon).unwrap();
        let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
        let shared = crate::pipeline::render_split::display_source(aces, &print).unwrap();

        for (label, tone) in [
            ("shoulder", DisplayTone::DEFAULT),
            ("none", DisplayTone::None),
        ] {
            let sdr = match crate::pipeline::sdr::render(
                &shared,
                crate::pipeline::sdr::SdrGamut::DisplayP3,
                tone,
            ) {
                Ok(v) => v,
                // Self-policing in action: without a curve, a reconstruction that
                // overshoots reference white is refused. That is a finding, not a
                // harness bug.
                Err(e) => {
                    println!("{mark:<6}{label:<10}SDR REFUSED: {e}");
                    continue;
                }
            };
            let img = sdr.image();
            let mut lum: Vec<f32> = (0..img.rgb.len() / 3)
                .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
                .filter(|v| v.is_finite())
                .collect();
            lum.sort_by(f32::total_cmp);
            let blown =
                100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
            let csep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
            let mid = rect_of(f, "mid").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));

            let hdr = crate::pipeline::hdr::render_linear(&shared, tone);
            let hdr_peak = hdr.as_ref().ok().map(|h| {
                use crate::pipeline::colorimetry::pinned::BT2020_LUMA as L;
                let im = h.image();
                (0..im.rgb.len() / 3)
                    .map(|i| {
                        let p = &im.rgb[i * 3..i * 3 + 3];
                        L[0] * p[0] + L[1] * p[1] + L[2] * p[2]
                    })
                    .fold(0.0f32, f32::max)
            });
            let gain_max = crate::pipeline::gain_map::render(
                &shared,
                crate::pipeline::gain_map::GainMapConfig::ultra_hdr_v1(tone),
            )
            .ok()
            .map(|g| g.gain().rgb().iter().copied().fold(0.0f32, f32::max));

            let show = |v: Option<f32>| v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "-".into());
            println!(
                "{mark:<6}{label:<10}{:>9}{blown:>8.2}%{csep:>10.1}{:>9.3}{:>10}{:>10}",
                show(mid),
                lum.last().copied().unwrap_or(0.0),
                show(hdr_peak),
                show(gain_max),
            );
        }
    }
    println!(
        "\nRead: lower 'blown%' with higher 'code sep' is more highlight separation, and \
         the 'none' row's\nblown% is the share the *reconstruction* put at absolute \
         white — the display stage cannot fix\nthat part. 'mid' must not move between \
         the rows; if it does, the comparison is confounded.\nNo p99/p99.9 column: with \
         several percent flattened against white both land inside the flat\nregion and \
         measure nothing (see `measure_candidates`)."
    );
}

/// The acceptance probe for `output/display-tone-mapping`: the same operator
/// `tone_map_probe` measured, but applied **in the render stage** instead of composed
/// into the reconstruction curve.
///
/// `tone_map_probe` folds the operator into `apply_curve` because `AcesCgImage` is
/// only constructible inside `working_space` — a harness convenience, and the reason
/// this one exists. Reproducing its figures from `sdr::render` is what shows the
/// operator survived the move to the stage that will actually own it. Reconstruction
/// here is X3 (exponential, mid pinned 0.508 above the base, unbounded).
///
/// The Hermite row is a real measurement, not a refusal: its plateau caps luminance
/// at display white and the gamut stage intersects the cube, so no sample can leave
/// `[0, 1]` and the only reachable `REFUSED` is a non-finite one, which X3 does not
/// produce. What that row shows instead is the shipped operator *pinning* — a large
/// blown fraction at zero separation.
///
/// ```text
/// NC_TONEMAP_FRAME=E1 cargo test --release shadow_metrics::tone_map_stage_probe \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn tone_map_stage_probe() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let mk = std::env::var("NC_TONEMAP_FRAME").unwrap_or_else(|_| "E1".into());
    let f = &fx["frames"][&mk];
    let roll = f["roll"].as_str().unwrap();
    let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
    let base = FilmBase {
        r: base_arr[0].as_f64().unwrap() as f32,
        g: base_arr[1].as_f64().unwrap() as f32,
        b: base_arr[2].as_f64().unwrap() as f32,
    };
    let path = assets
        .join("rolls")
        .join(roll)
        .join(f["file"].as_str().unwrap());
    let (image, _) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES).unwrap();

    // X3's reconstruction, with **no** operator folded in: the stage applies it.
    // Shared constants, so this probe cannot end up measuring a different X3.
    let contrast = X3_CONTRAST;
    let anchor = x3_reference_anchor();
    let x3_source = || {
        let mut density = to_density(&image, &base, &DensityParams::default());
        let _ = crate::algo::density::regional_balance(&mut density, &DensityParams::default());
        let film =
            crate::algo::density::apply_curve(density, |d| 10f32.powf(contrast * (d - anchor)));
        let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
        crate::pipeline::render_split::display_source(aces, &PrintParams::default()).unwrap()
    };

    // Measure the **delivered** image, i.e. clamped to display range, because that
    // is what `io::encode` writes. Two reasons this is not a detail: `srgb_encode`
    // is only defined on display-range values, so a code separation taken over
    // unclamped samples is unbounded (`W = 2` scores 1855 on a peak of 308 — pure
    // artefact); and an unbounded operator's over-range content is a *loss* at
    // encode, so counting it as retained separation would flatter it. `pre_peak`
    // keeps the un-clamped peak visible, since it is what the loss is measured from.
    let measure = |img: &LinearImage| {
        let raw: Vec<f32> = (0..img.rgb.len() / 3)
            .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
            .filter(|v| v.is_finite())
            .collect();
        let pre_peak = raw.iter().copied().fold(0.0f32, f32::max);
        let mut lum: Vec<f32> = raw.iter().map(|v| v.clamp(0.0, 1.0)).collect();
        lum.sort_by(f32::total_cmp);
        let blown = 100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
        let csep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
        let mid = rect_of(f, "mid").map(|(x, y, w, h)| patch_luma_p50(img, x, y, w, h));
        (mid, blown, csep, pre_peak)
    };

    println!("\nframe {mk}  {roll}  {}", f["file"].as_str().unwrap());
    println!("X3 reconstruction (exponential, contrast {contrast}, anchor {anchor:.4}, unbounded)");
    println!("operator applied in `sdr::render`, not in the curve");
    println!(
        "\n{:<34}{:>9}{:>10}{:>11}{:>10}",
        "sdr::render tone map", "mid", "blown%", "code sep", "pre-clamp"
    );

    let shared = x3_source();
    let mut rows: Vec<(String, DisplayTone)> =
        vec![("hermite hc=0 (shipped)".into(), DisplayTone::DEFAULT)];
    for w in [2.0f32, 8.0, 16.0, 64.0, 256.0] {
        rows.push((
            format!("reinhard W={w}"),
            DisplayTone::ExtendedReinhard(Headroom::new((w).log2()).unwrap()),
        ));
    }
    for (label, tone) in rows {
        match crate::pipeline::sdr::render(&shared, crate::pipeline::sdr::SdrGamut::DisplayP3, tone)
        {
            Ok(sdr) => {
                let (mid, blown, csep, peak) = measure(sdr.image());
                println!(
                    "{label:<34}{:>9}{blown:>9.2}%{csep:>11.1}{peak:>10.3}",
                    mid.map(|m| format!("{m:.4}")).unwrap_or_else(|| "-".into()),
                );
            }
            Err(e) => println!("{label:<34}   REFUSED: {e}"),
        }
    }

    // The shipped sigmoid on the same frame, for scale. A different reconstruction,
    // not a tone map, so it is printed apart.
    {
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let recon = Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams {
                contrast: crate::types::REFERENCE_CONTRAST,
                toe: 0.2,
                shoulder: 0.6,
                dmax: DmaxSource::Explicit(
                    0.5 * dmax + MID_OUTPUT_DECADES / crate::types::REFERENCE_CONTRAST,
                ),
                anchor: crate::types::AnchorPlacement::WhiteAtDmax,
            }),
        };
        let (film, _) = crate::algo::reconstruct(&image, &base, &recon).unwrap();
        let aces = crate::pipeline::working_space::map_nc_film_rgb_v1(film);
        let shared =
            crate::pipeline::render_split::display_source(aces, &PrintParams::default()).unwrap();
        let sdr = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let (mid, blown, csep, peak) = measure(sdr.image());
        println!(
            "{:<34}{:>9}{blown:>9.2}%{csep:>11.1}{peak:>10.3}   <- BENCHMARK (shipped sigmoid)",
            "[sigmoid D]",
            mid.map(|m| format!("{m:.4}")).unwrap_or_else(|| "-".into()),
        );
    }
}

/// The matched-midtone comparison `output/display-tone-mapping` gates its verdict on.
///
/// `tone_map_stage_probe` leaves midtone placement free, so its numbers mix "better
/// operator" with "brighter picture" — the confound the task file flags, and the
/// reason a verdict cannot be read off it. This probe removes it.
///
/// **Matching is done by moving the reconstruction anchor, which is exact rather than
/// approximate.** The exponential renders `10^(c·(D − A))`, so shifting `A` by `ΔA`
/// multiplies every linear value by `10^(−c·ΔA)` — a pure gain. Since the shared print
/// controls are linear at their defaults and constant-luminance gamut mapping preserves
/// luma, the rendered mid patch is `operator(gain · L_mid)` with `L_mid` measured once,
/// so the gain that lands the mid on the benchmark's is solved on a scalar instead of
/// re-rendering. It is also the *intended* division of labour: the task's premise is that
/// the anchor absorbs the operator's fixed midtone cost while `W` stays a pure highlight
/// control, so this measures exactly the configuration that premise describes.
///
/// The solved gain is verified against a real render every time (`mid err`); a model this
/// probe cannot confirm is reported as a failure rather than quietly compared.
///
/// ```text
/// cargo test --release shadow_metrics::tone_map_matched_probe -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn tone_map_matched_probe() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();

    let contrast = X3_CONTRAST;
    let reference_anchor = x3_reference_anchor();

    // Operators to compare, all applied in `sdr::render`. The Hermite is the control:
    // it isolates "the operator changed" from "the reconstruction changed", since it
    // is the shipped one running on the same X3 source at the same matched midtone.
    let operators: Vec<(String, DisplayTone)> = {
        let mut v = vec![("hermite (control)".to_string(), DisplayTone::DEFAULT)];
        for w in [8.0f32, 16.0, 64.0, 256.0] {
            v.push((
                format!("reinhard W={w}"),
                DisplayTone::ExtendedReinhard(Headroom::new((w).log2()).unwrap()),
            ));
        }
        v
    };

    let only = std::env::var("NC_TONEMAP_FRAME").ok();
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut measured = 0usize;

    for (key, f) in fx["frames"].as_object().unwrap() {
        if only.as_deref().is_some_and(|k| k != key) {
            continue;
        }
        let Some(mid_rect) = rect_of(f, "mid") else {
            println!("\n{key}: no valid mid patch — cannot match midtones, skipped");
            continue;
        };
        let roll = f["roll"].as_str().unwrap();
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let Ok((image, _)) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) else {
            println!("\n{key}: decode failed, skipped");
            continue;
        };

        let measure = |img: &LinearImage| {
            let raw: Vec<f32> = (0..img.rgb.len() / 3)
                .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
                .filter(|v| v.is_finite())
                .collect();
            let mut lum: Vec<f32> = raw.iter().map(|v| v.clamp(0.0, 1.0)).collect();
            lum.sort_by(f32::total_cmp);
            let blown =
                100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
            let csep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
            let (x, y, w, h) = mid_rect;
            (patch_luma_p50(img, x, y, w, h), blown, csep)
        };

        // The benchmark: the shipped sigmoid at its own defaults. Its mid is the target.
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let sigmoid = benchmark_sigmoid(dmax);
        let (film, _) = crate::algo::reconstruct(&image, &base, &sigmoid).unwrap();
        let shared = crate::pipeline::render_split::display_source(
            crate::pipeline::working_space::map_nc_film_rgb_v1(film),
            &PrintParams::default(),
        )
        .unwrap();
        let bench = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let (bench_mid, bench_blown, bench_sep) = measure(bench.image());

        // `L_mid`: the mid patch's destination luminance *before* any operator, at the
        // reference anchor. Taken from the adjusted ACEScg the renderer itself consumes,
        // through the same pinned matrix and luma vector `sdr::destination_rgb` uses.
        let density = to_density(&image, &base, &DensityParams::default());
        let l_mid = pre_operator_mid_luma(&x3_shared(&density, reference_anchor), mid_rect);

        println!("\nframe {key}  {roll}  {}", f["file"].as_str().unwrap());
        println!(
            "X3 reconstruction, contrast {contrast}, reference anchor {reference_anchor:.4}; \
             pre-operator mid luma {l_mid:.4}"
        );
        println!("matched to the shipped sigmoid's mid of {bench_mid:.4}");
        println!(
            "\n{:<22}{:>8}{:>9}{:>9}{:>10}{:>9}",
            "tone map", "anchor", "mid", "mid err", "blown%", "code sep"
        );
        println!(
            "{:<22}{:>8}{bench_mid:>9.4}{:>9}{bench_blown:>9.2}%{bench_sep:>9.1}   <- BENCHMARK",
            "[sigmoid, default]", "-", "-"
        );

        for (label, tone) in &operators {
            let resolved = *tone;
            let Some(anchor) = matched_anchor(l_mid, bench_mid, scalar_operator(resolved)) else {
                println!("{label:<22}   target unreachable (operator saturates below it)");
                continue;
            };

            let shared = x3_shared(&density, anchor);
            let Ok(sdr) = crate::pipeline::sdr::render(
                &shared,
                crate::pipeline::sdr::SdrGamut::DisplayP3,
                resolved,
            ) else {
                println!("{label:<22}   REFUSED by sdr::render");
                continue;
            };
            let (mid, blown, csep) = measure(sdr.image());
            let err = mid - bench_mid;
            println!("{label:<22}{anchor:>8.4}{mid:>9.4}{err:>9.4}{blown:>9.2}%{csep:>9.1}");
            // The self-check. A gain solved from the linear-gain model but not confirmed
            // by the render means the model is wrong and the row below is meaningless.
            assert!(
                err.abs() < 0.01,
                "{key} {label}: solved gain did not land the midtone \
                 (target {bench_mid:.4}, got {mid:.4}) — the linear-gain model is wrong"
            );
            if label.starts_with("reinhard") {
                measured += 1;
                if blown <= bench_blown && csep >= bench_sep {
                    wins += 1;
                } else {
                    losses += 1;
                }
            }
        }
    }
    println!(
        "\nreinhard rows at matched midtone: {wins} beat the sigmoid on both metrics, \
         {losses} did not (of {measured})"
    );
}

/// Does matching at the **mid patch** cause the darkness the user saw, or is it inherent
/// to the compression?
///
/// `tone_map_matched_probe` matches one point — the mid patch — and the user's verdict was
/// that every Reinhard render still "looks darker than the default". Matching one point
/// does not match a curve: Reinhard compresses globally, so with mid pinned, everything
/// above mid lands lower. This asks whether a *different* match point buys back the
/// brightness while keeping the highlight gain, or whether the two genuinely trade.
///
/// The decisive column is **mean lightness**: if a match point exists where lightness
/// equals the sigmoid's *and* `blown%` / `code sep` still beat it, the darkness is an
/// artefact of where we matched. If not, it is the operator's price.
///
/// ```text
/// cargo test --release shadow_metrics::tone_map_match_point_probe -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn tone_map_match_point_probe() {
    const SUBSAMPLE: usize = 8;

    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let only = std::env::var("NC_TONEMAP_FRAME").ok();
    let reinhard = DisplayTone::ExtendedReinhard(Headroom::new((64.0f32).log2()).unwrap());
    let apply = scalar_operator(reinhard);

    for (key, f) in fx["frames"].as_object().unwrap() {
        if only.as_deref().is_some_and(|k| k != key) {
            continue;
        }
        let Some(mid_rect) = rect_of(f, "mid") else {
            continue;
        };
        let roll = f["roll"].as_str().unwrap();
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let Ok((image, _)) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) else {
            continue;
        };

        let (bx, by, bw, bh) = mid_rect;
        let measure = |img: &LinearImage| {
            let raw: Vec<f32> = (0..img.rgb.len() / 3)
                .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
                .filter(|v| v.is_finite())
                .collect();
            let mut lum: Vec<f32> = raw.iter().map(|v| v.clamp(0.0, 1.0)).collect();
            lum.sort_by(f32::total_cmp);
            let blown =
                100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
            let sep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
            let light =
                lum.iter().map(|v| f64::from(srgb_encode(*v))).sum::<f64>() / lum.len() as f64;
            (
                patch_luma_p50(img, bx, by, bw, bh),
                light as f32,
                blown,
                sep,
            )
        };

        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let (film, _) = crate::algo::reconstruct(&image, &base, &benchmark_sigmoid(dmax)).unwrap();
        let shared = crate::pipeline::render_split::display_source(
            crate::pipeline::working_space::map_nc_film_rgb_v1(film),
            &PrintParams::default(),
        )
        .unwrap();
        let bench = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let (b_mid, b_light, b_blown, b_sep) = measure(bench.image());

        let density = to_density(&image, &base, &DensityParams::default());
        let reference = x3_shared(&density, x3_reference_anchor());
        let l_mid = pre_operator_mid_luma(&reference, mid_rect);
        let samples = pre_operator_luma_samples(&reference, SUBSAMPLE);
        drop(reference);

        println!("\nframe {key}  {roll}  {}", f["file"].as_str().unwrap());
        println!(
            "\n{:<24}{:>8}{:>9}{:>10}{:>10}{:>9}",
            "match point", "anchor", "mid", "lightness", "blown%", "code sep"
        );
        println!(
            "{:<24}{:>8}{b_mid:>9.4}{b_light:>10.4}{b_blown:>9.2}%{b_sep:>9.1}   <- BENCHMARK",
            "[sigmoid, default]", "-"
        );

        // Two match points that bracket the question: the mid patch (one point, what the
        // user reviewed) and mean lightness (the whole distribution, the brightness the
        // eye actually integrates).
        for target in ["mid patch", "mean lightness"] {
            let gain = {
                let (mut lo, mut hi) = (1e-6f32, 1e6f32);
                for _ in 0..80 {
                    let g = 0.5 * (lo + hi);
                    let below = match target {
                        "mid patch" => apply(g * l_mid) < b_mid,
                        _ => mean_lightness(&samples, g, &apply) < b_light,
                    };
                    if below {
                        lo = g;
                    } else {
                        hi = g;
                    }
                }
                0.5 * (lo + hi)
            };
            let anchor = x3_reference_anchor() - gain.log10() / X3_CONTRAST;
            let shared = x3_shared(&density, anchor);
            let sdr = crate::pipeline::sdr::render(
                &shared,
                crate::pipeline::sdr::SdrGamut::DisplayP3,
                reinhard,
            )
            .unwrap();
            let (mid, light, blown, sep) = measure(sdr.image());
            println!(
                "{:<24}{anchor:>8.4}{mid:>9.4}{light:>10.4}{blown:>9.2}%{sep:>9.1}",
                format!("reinhard64 @ {target}")
            );
            // The solve is only meaningful if the render confirms the statistic it aimed at.
            let (got, want) = if target == "mid patch" {
                (mid, b_mid)
            } else {
                (light, b_light)
            };
            assert!(
                (got - want).abs() < 0.01,
                "{key}: solving for {target} missed (want {want:.4}, got {got:.4})"
            );
        }
    }
    println!(
        "\nIf `mean lightness` reaches the benchmark while blown%/sep still beat it, the\n\
         darkness was the match point. If blown%/sep give way, it is the operator's price."
    );
}

/// The HDR half of `output/display-tone-mapping`: does the highlight-lifted operator
/// produce a **live** gain map, and does the HDR rendition stay under its ceiling?
///
/// This is the task's own acceptance criterion, which has never been met: *"the HDR
/// rendition measures a peak below the ceiling with non-zero separation above reference
/// white, and the resulting `gain-map-hdr` reports `GainMapMax > 1.0`"*. Since
/// `pipeline_version` 3 that number has decoded as exactly 1.0x, because the shipped
/// sigmoid's shoulder removes every above-white value during *reconstruction*, so both
/// display branches receive identical input and their ratio is 1 by construction.
///
/// This computes the HDR branch's luminance directly, through the same pinned matrix and
/// luma vector `hdr::render_pixel_checked` uses, rather than calling `hdr::render_linear`.
/// That started as a necessity — `render_linear` refused the unbounded tone until
/// 2026-09-02 — and the refusal is now gone, so the reason it stays is the remaining one:
/// a probe that characterizes the renderer should not be routed through the function it is
/// characterizing, and it needs the ratio *before* packaging, which `render_linear`'s
/// typed result does not hand back in that form. Duplicated arithmetic, shared constants.
/// It measures the
/// **canonical gain** — the per-pixel HDR/SDR ratio the container encodes — rather than
/// packaging a JPEG, because the packaging step is mechanical and the ratio is the thing
/// under test.
///
/// The reconstruction is X3 (exponential, unbounded), matched to the shipped sigmoid's
/// mean lightness, because a *bounded* reconstruction cannot produce a live gain map at
/// any tone setting: with nothing above the crossover the lift is identically zero. That
/// is the same finding from the other direction, and it is why this task and
/// `algo/reconstruction-render-curve-split` are one question.
///
/// ```text
/// NC_TONEMAP_FRAME=P3 cargo test --release shadow_metrics::hdr_gain_probe \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn hdr_gain_probe() {
    use crate::pipeline::colorimetry::pinned::{ACESCG_TO_BT2020, BT2020_LUMA};
    use crate::pipeline::display_tone::{extended_reinhard, highlight_lifted_reinhard};

    const CEILING: f32 = crate::pipeline::hdr::LINEAR_HEADROOM;

    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let only = std::env::var("NC_TONEMAP_FRAME").ok();

    for (key, f) in fx["frames"].as_object().unwrap() {
        if only.as_deref().is_some_and(|k| k != key) {
            continue;
        }
        // A mid patch is required only so this frame is one the matched-lightness rows
        // elsewhere also cover; the measurement below uses whole-frame lightness.
        if rect_of(f, "mid").is_none() {
            continue;
        }
        let roll = f["roll"].as_str().unwrap();
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let Ok((image, _)) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) else {
            continue;
        };

        // Match the shipped sigmoid's mean lightness, per chunk 4: comparing at equal
        // brightness is the only way these numbers mean anything.
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let (film, _) = crate::algo::reconstruct(&image, &base, &benchmark_sigmoid(dmax)).unwrap();
        let bench = crate::pipeline::sdr::render(
            &crate::pipeline::render_split::display_source(
                crate::pipeline::working_space::map_nc_film_rgb_v1(film),
                &PrintParams::default(),
            )
            .unwrap(),
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let bench_light = {
            let img = bench.image();
            let n = img.rgb.len() / 3;
            (0..n)
                .map(|i| {
                    f64::from(srgb_encode(
                        p3_luma(&img.rgb[i * 3..i * 3 + 3]).clamp(0.0, 1.0),
                    ))
                })
                .sum::<f64>()
                / n as f64
        } as f32;
        drop(bench);

        let density = to_density(&image, &base, &DensityParams::default());
        let reference = x3_shared(&density, x3_reference_anchor());
        let samples = pre_operator_luma_samples(&reference, 8);
        drop(reference);

        println!("\nframe {key}  {roll}  {}", f["file"].as_str().unwrap());
        println!(
            "ceiling {CEILING:.4} (1000/203, binding); matched to sigmoid lightness {bench_light:.4}"
        );
        println!(
            "\n{:<26}{:>10}{:>11}{:>10}{:>11}{:>9}",
            "W / crossover", "hdr peak", "sep >RW", "max gain", "lifted %", "verdict"
        );

        // Three ways to bound the multiplicative lift. The lift can only be bounded by
        // bounding its base, so this is the whole design space: leave the base alone
        // (unbounded), clamp it hard (flat top), or give it an asymptotic form (soft top
        // at the cost of a tiny agreement deficit below the crossover).
        for base_mode in ["raw", "clamped", "soft"] {
            println!("  base: {base_mode}");
            for w in [16.0f32, 64.0] {
                for crossover in [1.0f32, 2.0] {
                    // Solve the gain that matches SDR lightness at this W, so each row is a
                    // like-for-like render rather than a different exposure.
                    let sdr_op = |v: f32| extended_reinhard(v, w);
                    let (mut lo, mut hi) = (1e-6f32, 1e6f32);
                    for _ in 0..80 {
                        let g = 0.5 * (lo + hi);
                        if mean_lightness(&samples, g, &sdr_op) < bench_light {
                            lo = g;
                        } else {
                            hi = g;
                        }
                    }
                    let gain = 0.5 * (lo + hi);
                    let anchor = x3_reference_anchor() - gain.log10() / X3_CONTRAST;
                    let shared = x3_shared(&density, anchor);

                    // The HDR branch's own luminance: ACEScg → BT.2020, then its luma vector.
                    let rgb = shared.source.rgb();
                    let mut hdr: Vec<f32> = Vec::with_capacity(rgb.len() / 3);
                    let mut gains: Vec<f32> = Vec::with_capacity(rgb.len() / 3);
                    for i in (0..rgb.len() / 3).step_by(4) {
                        let aces = [rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]];
                        let bt = [
                            dot3(ACESCG_TO_BT2020[0], aces),
                            dot3(ACESCG_TO_BT2020[1], aces),
                            dot3(ACESCG_TO_BT2020[2], aces),
                        ];
                        let luma = dot3(BT2020_LUMA, bt);
                        if !luma.is_finite() || luma <= 0.0 {
                            continue;
                        }
                        let sdr_v = extended_reinhard(luma, w);
                        // Each variant constructs its own base and multiplies the **pure**
                        // lift. Deriving the lift as `shipped / sdr_v` instead — which this
                        // did until 2026-09-02 — silently re-applies whatever base
                        // `highlight_lifted_reinhard` happens to use, and that is exactly
                        // how these three stopped being three: when the shipped base became
                        // asymptotic, `raw` became the shipped operator (peak 4.92594, i.e.
                        // *bounded*, against a label reading "unbounded"), `soft` became the
                        // base applied twice, and `clamped` became non-monotonic — 4.85 at
                        // v=64 falling to 0.84 at v=20000. The rows still printed plausible
                        // numbers, so nothing failed; the probe had merely stopped measuring
                        // its own conclusion.
                        let lift = |v: f32| -> f32 {
                            if v <= crossover || CEILING <= 1.0 {
                                return 1.0;
                            }
                            let (lo, hi) = (crossover.log2(), w.log2());
                            if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
                                return 1.0;
                            }
                            let t = ((v.log2() - lo) / (hi - lo)).clamp(0.0, 1.0);
                            let smooth = t * t * (3.0 - 2.0 * t);
                            1.0 + (CEILING - 1.0) * smooth
                        };
                        // `raw`: the base as written, `f(v, W)` — unbounded, because `f` is.
                        // `clamped`: that base held at reference white, which only bites
                        // above `W` (6+ stops over diffuse white, where SDR already clips).
                        // `soft`: the asymptotic base `extended_reinhard(v, inf) = v/(1+v)`,
                        // so the composite approaches the ceiling without attaining it. This
                        // last one is the shipped design, reconstructed here from its parts
                        // rather than by calling the shipped function — which is what makes
                        // the assertion below a drift detector instead of a tautology.
                        let hdr_v = match base_mode {
                            "soft" => extended_reinhard(luma, f32::INFINITY) * lift(luma),
                            "clamped" => sdr_v.min(1.0) * lift(luma),
                            _ => sdr_v * lift(luma),
                        };
                        if base_mode == "soft" {
                            // The one line that would have caught the drift above: the
                            // hand-built mirror of the shipped design must equal the shipped
                            // function. If its base changes again, this fires instead of the
                            // table quietly re-labelling itself.
                            let shipped = highlight_lifted_reinhard(luma, w, crossover, CEILING);
                            assert!(
                                (hdr_v - shipped).abs() <= 1e-5 * shipped.abs().max(1.0),
                                "the `soft` row no longer mirrors highlight_lifted_reinhard \
                                 at v={luma}, W={w}, crossover={crossover}: {hdr_v} vs \
                                 {shipped}"
                            );
                        }
                        if sdr_v > 0.0 {
                            gains.push(hdr_v / sdr_v);
                        }
                        hdr.push(hdr_v);
                    }
                    hdr.sort_by(f32::total_cmp);
                    let peak = hdr.last().copied().unwrap_or(0.0);
                    // Separation among content above reference white, in the HDR domain: if
                    // the speculars arrive as one flat blob this is ~0 and the headroom is
                    // being saturated rather than used.
                    let above: Vec<f32> = hdr.iter().copied().filter(|v| *v > 1.0).collect();
                    let sep = if above.len() > 20 {
                        pct(&above, 0.99) - pct(&above, 0.50)
                    } else {
                        0.0
                    };
                    let lifted = 100.0 * above.len() as f32 / hdr.len().max(1) as f32;
                    let max_gain = gains.iter().copied().fold(0.0f32, f32::max);
                    let live = max_gain > 1.0;
                    let under = peak <= CEILING;
                    let separated = sep > 0.0;
                    let verdict = match (live, under, separated) {
                        (true, true, true) => "PASS",
                        (false, _, _) => "inert",
                        (_, false, _) => "over ceiling",
                        _ => "flat",
                    };
                    println!(
                        "  W={w:<5} xo={crossover:<5}{peak:>10.3}{sep:>11.3}{max_gain:>10.3}{lifted:>10.2}%{verdict:>10}"
                    );
                }
            }
        }
        println!(
            "\nPASS needs all three: max gain > 1.0 (the map carries information), peak <= \
             {CEILING:.3} (inside\nthe declared headroom), and non-zero separation above \
             reference white (detail, not a blob)."
        );
    }
}

/// Subsampled pre-operator destination luminance for the whole frame.
///
/// Same transform `sdr::destination_rgb` applies, so `operator(gain * sample)` is the
/// rendered luminance for any gain — constant-luminance gamut mapping preserves it, and
/// the anchor is a pure gain. That lets a match target be solved over the *distribution*
/// without re-rendering per candidate. `step` subsamples; a mean over every 8th pixel is
/// far tighter than any difference this probe is looking at.
fn pre_operator_luma_samples(
    shared: &crate::pipeline::render_split::SharedDisplaySource,
    step: usize,
) -> Vec<f32> {
    use crate::pipeline::colorimetry::pinned::{ACESCG_TO_DISPLAY_P3, DISPLAY_P3_LUMA};
    let rgb = shared.source.rgb();
    (0..rgb.len() / 3)
        .step_by(step)
        .filter_map(|i| {
            let aces = [rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]];
            let dest = [
                dot3(ACESCG_TO_DISPLAY_P3[0], aces),
                dot3(ACESCG_TO_DISPLAY_P3[1], aces),
                dot3(ACESCG_TO_DISPLAY_P3[2], aces),
            ];
            let luma = dot3(DISPLAY_P3_LUMA, dest);
            luma.is_finite().then_some(luma)
        })
        .collect()
}

/// Mean **encoded** lightness of a rendered set: the display transfer approximates the
/// eye's response, so this tracks "how bright the picture looks" far better than mean
/// linear luminance, which is dominated by highlights. Clamped because anything above
/// display white is shown as white.
fn mean_lightness(samples: &[f32], gain: f32, apply: &impl Fn(f32) -> f32) -> f32 {
    let total: f64 = samples
        .iter()
        .map(|v| f64::from(srgb_encode(apply(gain * v).clamp(0.0, 1.0))))
        .sum();
    (total / samples.len() as f64) as f32
}

/// Where the reconstruction curve moves colour, and how that varies with tone.
///
/// The user's visual review found the X3 renders bluer than the sigmoid's. The display
/// tone mapper cannot be the cause — it scales all three channels by one common factor,
/// so chromaticity is preserved — which leaves the reconstruction curve, applied **per
/// channel**. This measures that directly: channel ratios against green, bucketed by
/// luminance percentile, under both curves at matched midtone.
///
/// If the divergence grows toward the highlights, the mechanism is curve *slope* — the
/// exponential holds a constant contrast where the sigmoid's shoulder is flattening — and
/// not a constant cast.
///
/// ```text
/// NC_TONEMAP_FRAME=P3 cargo test --release shadow_metrics::curve_colour_probe \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn curve_colour_probe() {
    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let key = std::env::var("NC_TONEMAP_FRAME").unwrap_or_else(|_| "P3".into());
    let f = &fx["frames"][&key];
    let Some(mid_rect) = rect_of(f, "mid") else {
        println!("{key}: no mid patch, cannot match");
        return;
    };
    let roll = f["roll"].as_str().unwrap();
    let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
    let base = FilmBase {
        r: base_arr[0].as_f64().unwrap() as f32,
        g: base_arr[1].as_f64().unwrap() as f32,
        b: base_arr[2].as_f64().unwrap() as f32,
    };
    let path = assets
        .join("rolls")
        .join(roll)
        .join(f["file"].as_str().unwrap());
    let (image, _) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES).unwrap();
    println!(
        "\nframe {key}  {roll}  film base {:?}",
        [base.r, base.g, base.b]
    );

    // Channel ratios against green, in luminance percentile buckets. Green is the
    // reference because it carries most of the luma, so R/G and B/G read as the colour
    // the eye assigns to that tone.
    let ratios = |img: &LinearImage, label: &str| {
        let n = img.rgb.len() / 3;
        let mut idx: Vec<usize> = (0..n)
            .filter(|i| {
                img.rgb[i * 3..i * 3 + 3]
                    .iter()
                    .all(|v| v.is_finite() && *v > 1e-6)
            })
            .collect();
        idx.sort_by(|a, b| {
            p3_luma(&img.rgb[a * 3..a * 3 + 3]).total_cmp(&p3_luma(&img.rgb[b * 3..b * 3 + 3]))
        });
        print!("{label:<26}");
        for (name, lo, hi) in [
            ("p05", 0.03, 0.07),
            ("p25", 0.23, 0.27),
            ("p50", 0.48, 0.52),
            ("p75", 0.73, 0.77),
            ("p95", 0.93, 0.97),
        ] {
            let (a, b) = (
                (idx.len() as f64 * lo) as usize,
                (idx.len() as f64 * hi) as usize,
            );
            let (mut sr, mut sg, mut sb) = (0.0f64, 0.0f64, 0.0f64);
            for &i in &idx[a..b.max(a + 1).min(idx.len())] {
                sr += f64::from(img.rgb[i * 3]);
                sg += f64::from(img.rgb[i * 3 + 1]);
                sb += f64::from(img.rgb[i * 3 + 2]);
            }
            let _ = name;
            print!("{:>7.3}{:>7.3}", sr / sg, sb / sg);
        }
        println!();
    };

    println!(
        "\n{:<29}p05          p25          p50          p75          p95",
        " "
    );
    println!(
        "{:<29}R/G  B/G    R/G  B/G    R/G  B/G    R/G  B/G    R/G  B/G",
        "curve"
    );

    let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
    let (film, _) = crate::algo::reconstruct(&image, &base, &benchmark_sigmoid(dmax)).unwrap();
    let shared = crate::pipeline::render_split::display_source(
        crate::pipeline::working_space::map_nc_film_rgb_v1(film),
        &PrintParams::default(),
    )
    .unwrap();
    let bench = crate::pipeline::sdr::render(
        &shared,
        crate::pipeline::sdr::SdrGamut::DisplayP3,
        DisplayTone::DEFAULT,
    )
    .unwrap();
    let (bx, by, bw, bh) = mid_rect;
    let bench_mid = patch_luma_p50(bench.image(), bx, by, bw, bh);
    ratios(bench.image(), "sigmoid (shipped)");

    let density = to_density(&image, &base, &DensityParams::default());
    let l_mid = pre_operator_mid_luma(&x3_shared(&density, x3_reference_anchor()), mid_rect);
    for (label, tone) in [
        ("X3 exponential + hermite", DisplayTone::DEFAULT),
        (
            "X3 exponential + reinhard64",
            DisplayTone::ExtendedReinhard(Headroom::new((64.0f32).log2()).unwrap()),
        ),
    ] {
        let anchor = matched_anchor(l_mid, bench_mid, scalar_operator(tone)).unwrap();
        let shared = x3_shared(&density, anchor);
        let sdr =
            crate::pipeline::sdr::render(&shared, crate::pipeline::sdr::SdrGamut::DisplayP3, tone)
                .unwrap();
        ratios(sdr.image(), label);
    }

    // The default print white balance is `Explicit([1, 1, 1])` — i.e. **none**. If the
    // cast is a per-channel gain, an auto mode that already ships removes it with no new
    // machinery; if it is tone-dependent, it will not, and the answer is a per-channel
    // contrast instead. This is what distinguishes the two.
    let reinhard = DisplayTone::ExtendedReinhard(Headroom::new((64.0f32).log2()).unwrap());
    let anchor = matched_anchor(l_mid, bench_mid, scalar_operator(reinhard)).unwrap();
    for (label, wb) in [
        (
            "  ...+ auto WB gray-world",
            crate::types::WbSource::GrayWorld,
        ),
        (
            "  ...+ auto WB percentile",
            crate::types::WbSource::Percentile,
        ),
    ] {
        let print = PrintParams {
            white_balance: wb,
            ..PrintParams::default()
        };
        let mut d = density.clone();
        let _ = crate::algo::density::regional_balance(&mut d, &DensityParams::default());
        let film = crate::algo::density::apply_curve(d, |v| 10f32.powf(X3_CONTRAST * (v - anchor)));
        let shared = crate::pipeline::render_split::display_source(
            crate::pipeline::working_space::map_nc_film_rgb_v1(film),
            &print,
        )
        .unwrap();
        let sdr = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            reinhard,
        )
        .unwrap();
        ratios(sdr.image(), label);
    }
    println!(
        "\nB/G above 1 is blue-leaning. Compare the two X3 rows: the tone mapper cannot move\n\
         chromaticity, so any difference between them is gamut mapping, not the operator."
    );
}

/// Renders the matched-midtone configs to colour-managed TIFFs for visual review.
///
/// The metrics in `tone_map_matched_probe` say extended Reinhard wins; the highlight
/// metrics in this harness have disagreed with the eye twice, so a default cannot move on
/// them alone. Every config here is at **matched midtone**, so what differs between two
/// files is the operator and not the exposure — which is the only way an eye comparison
/// means anything.
///
/// Two views per config, because they answer different questions: a decimated full frame
/// for overall look, colour and midtone rendering, and a **1:1** crop on the frame's
/// brightest region for hard-clip-versus-soft-roll-off, which decimation would average
/// away. The crop window is located once per frame from the benchmark render and reused
/// for every config, so the crops are pixel-aligned across a frame.
///
/// Files go through the shipped `encode_rendered_sdr` → `io::encode` path with the
/// Display P3 profile embedded, so they are what `nc` would write rather than a
/// re-implementation.
///
/// ```text
/// cargo test --release shadow_metrics::tone_map_visual_review -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires ../nc-assets; run with --ignored --nocapture"]
fn tone_map_visual_review() {
    const CROP: u32 = 640;
    const OVERVIEW_MAX: u32 = 1600;

    let Some(assets) = assets_root() else {
        eprintln!("SKIP: no ../nc-assets/manifest.json");
        return;
    };
    let fx: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("scripts/sigmoid-baseline/fixtures.json"))
            .unwrap(),
    )
    .unwrap();
    let outdir = repo_root().join("../temp/tonemap-review");
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        eprintln!("SKIP: cannot create {}: {e}", outdir.display());
        return;
    }

    let frames: Vec<String> = std::env::var("NC_TONEMAP_FRAMES")
        .unwrap_or_else(|_| "P3,P4,G2,E2".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // The Hermite is the control: the shipped operator on the same X3 source at the same
    // matched midtone, so a visible difference is the operator alone. `W = 8` is left out
    // — it loses on blown% on all seven measured frames, so it needs no eye time.
    let operators: Vec<(&str, DisplayTone)> = vec![
        ("x3-hermite", DisplayTone::DEFAULT),
        (
            "x3-reinhard-w16",
            DisplayTone::ExtendedReinhard(Headroom::new((16.0f32).log2()).unwrap()),
        ),
        (
            "x3-reinhard-w64",
            DisplayTone::ExtendedReinhard(Headroom::new((64.0f32).log2()).unwrap()),
        ),
        (
            "x3-reinhard-w256",
            DisplayTone::ExtendedReinhard(Headroom::new((256.0f32).log2()).unwrap()),
        ),
    ];

    let mut notes: Vec<String> = Vec::new();
    // Rows for the review page. Emitted from the same measurement the probe prints, so
    // the page can never disagree with the numbers — nothing here is transcribed.
    let mut page_frames: Vec<String> = Vec::new();

    for key in &frames {
        let f = &fx["frames"][key];
        if f.is_null() {
            println!("{key}: not in fixtures.json, skipped");
            continue;
        }
        let Some(mid_rect) = rect_of(f, "mid") else {
            println!("{key}: no valid mid patch — midtones cannot be matched, skipped");
            continue;
        };
        let roll = f["roll"].as_str().unwrap();
        let base_arr = fx["rolls"][roll]["dmin"].as_array().unwrap();
        let base = FilmBase {
            r: base_arr[0].as_f64().unwrap() as f32,
            g: base_arr[1].as_f64().unwrap() as f32,
            b: base_arr[2].as_f64().unwrap() as f32,
        };
        let path = assets
            .join("rolls")
            .join(roll)
            .join(f["file"].as_str().unwrap());
        let Ok((image, _)) = crate::io::decode::decode_within(&path, DECODE_BUDGET_BYTES) else {
            println!("{key}: decode failed, skipped");
            continue;
        };

        // Benchmark: the shipped sigmoid at its defaults. Sets the midtone target and the
        // crop window every other config reuses.
        let dmax = f["roll_dmax"].as_f64().unwrap() as f32;
        let (film, _) = crate::algo::reconstruct(&image, &base, &benchmark_sigmoid(dmax)).unwrap();
        let shared = crate::pipeline::render_split::display_source(
            crate::pipeline::working_space::map_nc_film_rgb_v1(film),
            &PrintParams::default(),
        )
        .unwrap();
        let bench = crate::pipeline::sdr::render(
            &shared,
            crate::pipeline::sdr::SdrGamut::DisplayP3,
            DisplayTone::DEFAULT,
        )
        .unwrap();
        let (bx, by, bw, bh) = mid_rect;
        let measure = |img: &LinearImage| {
            let raw: Vec<f32> = (0..img.rgb.len() / 3)
                .map(|i| p3_luma(&img.rgb[i * 3..i * 3 + 3]))
                .filter(|v| v.is_finite())
                .collect();
            let mut lum: Vec<f32> = raw.iter().map(|v| v.clamp(0.0, 1.0)).collect();
            lum.sort_by(f32::total_cmp);
            let blown =
                100.0 * lum.iter().filter(|v| **v >= 0.999).count() as f32 / lum.len() as f32;
            let sep = srgb_encode(pct(&lum, 0.99)) * 255.0 - srgb_encode(pct(&lum, 0.90)) * 255.0;
            (patch_luma_p50(img, bx, by, bw, bh), blown, sep)
        };
        let (bench_mid, bench_blown, bench_sep) = measure(bench.image());
        let mut metrics: Vec<String> = vec![format!(
            "\"sigmoid-default\":{{\"mid\":{bench_mid},\"blown\":{bench_blown},\"sep\":{bench_sep}}}"
        )];
        // Window located on rendered-linear luminance, before the transfer encode, so it
        // is the brightest region by actual luminance.
        let window = brightest_window(bench.image(), CROP);
        let step = overview_step(bench.image(), OVERVIEW_MAX);
        write_review_pair(&outdir, key, "sigmoid-default", bench, window, CROP, step);

        println!(
            "\n{key} {roll} {}  mid target {bench_mid:.4}  crop at {:?}  overview step {step}",
            f["file"].as_str().unwrap(),
            window
        );
        notes.push(format!(
            "| {key} | {roll} | {} | {bench_mid:.4} | {},{} | {step} |",
            f["file"].as_str().unwrap(),
            window.0,
            window.1
        ));

        let density = to_density(&image, &base, &DensityParams::default());
        let l_mid = pre_operator_mid_luma(&x3_shared(&density, x3_reference_anchor()), mid_rect);

        for (label, tone) in &operators {
            let Some(anchor) = matched_anchor(l_mid, bench_mid, scalar_operator(*tone)) else {
                println!("  {label}: target unreachable, skipped");
                continue;
            };
            let shared = x3_shared(&density, anchor);
            let Ok(sdr) = crate::pipeline::sdr::render(
                &shared,
                crate::pipeline::sdr::SdrGamut::DisplayP3,
                *tone,
            ) else {
                println!("  {label}: REFUSED by sdr::render");
                continue;
            };
            let (mid, blown, sep) = measure(sdr.image());
            // Same self-check the matched probe runs: an unconfirmed gain would make the
            // images incomparable, which is worse than not writing them.
            assert!(
                (mid - bench_mid).abs() < 0.01,
                "{key} {label}: midtone did not match (target {bench_mid:.4}, got {mid:.4})"
            );
            println!(
                "  {label}: anchor {anchor:.4}, mid {mid:.4}, {blown:.2}% blown, sep {sep:.1}"
            );
            metrics.push(format!(
                "\"{label}\":{{\"mid\":{mid},\"blown\":{blown},\"sep\":{sep}}}"
            ));
            write_review_pair(&outdir, key, label, sdr, window, CROP, step);
        }
        page_frames.push(format!(
            "{{\"key\":\"{key}\",\"file\":\"{}\",\"metrics\":{{{}}}}}",
            f["file"].as_str().unwrap(),
            metrics.join(",")
        ));
    }

    // The page is written from the template beside this module, with the measured data
    // inlined — a `fetch` of a sibling JSON is blocked under `file://` in most browsers,
    // and this review is opened as a local file by design (it references local images).
    let data = format!(
        "{{\"configs\":[\"sigmoid-default\",{}],\"frames\":[{}]}}",
        operators
            .iter()
            .map(|(l, _)| format!("\"{l}\""))
            .collect::<Vec<_>>()
            .join(","),
        page_frames.join(",")
    );
    std::fs::write(
        outdir.join("index.html"),
        include_str!("tone_map_review.html").replace("__DATA__", &data),
    )
    .unwrap();

    let readme = format!(
        "# Display tone-mapping visual review\n\n\
         Generated by `shadow_metrics::tone_map_visual_review`.\n\n\
         **Every config is at matched midtone**, so a difference between two files is the\n\
         tone-mapping operator and not the exposure. Matching is done by moving the X3\n\
         reconstruction anchor, which is an exact linear gain; each render is verified to\n\
         land the mid patch on the benchmark's before it is written.\n\n\
         Files are `<frame>-<config>-{{full,crop}}.tif`, Display P3 with the profile\n\
         embedded, written through the shipped encode path.\n\n\
         - `full` — decimated overview: overall look, colour, midtone rendering.\n\
         - `crop` — **1:1** on the frame's brightest region, identical coordinates across\n\
         every config of that frame: hard clip versus soft roll-off.\n\n\
         ## Configs\n\n\
         | config | what it is |\n| --- | --- |\n\
         | `sigmoid-default` | the shipped render — the thing to beat |\n\
         | `x3-hermite` | X3 reconstruction, **shipped** operator — the control |\n\
         | `x3-reinhard-w16` | marginal: loses to the sigmoid on G2 by 0.05pp |\n\
         | `x3-reinhard-w64` | the candidate: wins on both metrics on all 7 measured frames |\n\
         | `x3-reinhard-w256` | best blown%, but effectively classic Reinhard — watch for flatness |\n\n\
         ## Frames\n\n\
         | frame | roll | file | mid target | crop x,y | overview step |\n\
         | --- | --- | --- | --- | --- | --- |\n{}\n\n\
         ## What to judge\n\n\
         1. Does `w64` look better than `sigmoid-default` in the highlights, or just different?\n\
         2. Does `w256` look over-compressed despite scoring best? The metrics cannot see this.\n\
         3. Do the Reinhard highlights gradate where the sigmoid's snap?\n\
         4. Anything the metrics miss: highlight colour shifts, midtone flatness, local contrast.\n",
        notes.join("\n")
    );
    std::fs::write(outdir.join("README.md"), readme).unwrap();
    println!("\nreview -> {}", outdir.display());
}

/// Decimation step that brings the longer edge under `max_edge`.
fn overview_step(img: &LinearImage, max_edge: u32) -> u32 {
    let longest = img.width.max(img.height);
    longest.div_ceil(max_edge).max(1)
}

/// Top-left of the `size`×`size` window with the highest mean luminance, searched over the
/// picture interior only — see [`INTERIOR_INSET`].
///
/// Coarse on purpose: candidates step by a third of the window and each is averaged over
/// every 4th pixel. This only has to *locate* a bright region for the eye, and an exact
/// search over a 75 MP frame would dominate the probe's runtime.
fn brightest_window(img: &LinearImage, size: u32) -> (u32, u32) {
    let size = size.min(img.width).min(img.height);
    let stride = (size / 3).max(1);
    let inset_x = (img.width as f32 * INTERIOR_INSET) as u32;
    let inset_y = (img.height as f32 * INTERIOR_INSET) as u32;
    // Fall back to the whole frame if the inset would leave no room for a window.
    let (x0, x1, y0, y1) = if img.width.saturating_sub(2 * inset_x) >= size
        && img.height.saturating_sub(2 * inset_y) >= size
    {
        (inset_x, img.width - inset_x, inset_y, img.height - inset_y)
    } else {
        (0, img.width, 0, img.height)
    };
    let mut best = (x0, y0);
    let mut best_mean = f32::NEG_INFINITY;
    let mut y = y0;
    while y + size <= y1 {
        let mut x = x0;
        while x + size <= x1 {
            let mut sum = 0.0f64;
            let mut n = 0u32;
            let mut row = y;
            while row < y + size {
                let mut col = x;
                while col < x + size {
                    let i = (row as usize * img.width as usize + col as usize) * 3;
                    if i + 2 < img.rgb.len() {
                        let luma = p3_luma(&img.rgb[i..i + 3]);
                        if luma.is_finite() {
                            sum += f64::from(luma);
                            n += 1;
                        }
                    }
                    col += 4;
                }
                row += 4;
            }
            if n > 0 {
                let mean = (sum / f64::from(n)) as f32;
                if mean > best_mean {
                    best_mean = mean;
                    best = (x, y);
                }
            }
            x += stride;
        }
        y += stride;
    }
    best
}

fn decimate(img: &LinearImage, step: u32) -> LinearImage {
    if step <= 1 {
        return LinearImage::new(img.width, img.height, img.rgb.clone(), None).unwrap();
    }
    let width = img.width.div_ceil(step);
    let height = img.height.div_ceil(step);
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for row in (0..img.height).step_by(step as usize) {
        for col in (0..img.width).step_by(step as usize) {
            let i = (row as usize * img.width as usize + col as usize) * 3;
            rgb.extend_from_slice(&img.rgb[i..i + 3]);
        }
    }
    LinearImage::new(width, height, rgb, None).unwrap()
}

fn crop(img: &LinearImage, x: u32, y: u32, size: u32) -> LinearImage {
    let width = size.min(img.width.saturating_sub(x));
    let height = size.min(img.height.saturating_sub(y));
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for row in y..y + height {
        let start = (row as usize * img.width as usize + x as usize) * 3;
        rgb.extend_from_slice(&img.rgb[start..start + (width as usize) * 3]);
    }
    LinearImage::new(width, height, rgb, None).unwrap()
}

/// Transfer-encode once, then write the decimated overview and the 1:1 crop from the
/// *encoded* image — so neither view re-runs any pipeline stage.
fn write_review_pair(
    outdir: &Path,
    frame: &str,
    label: &str,
    rendered: crate::pipeline::sdr::RenderedSdr,
    window: (u32, u32),
    crop_size: u32,
    step: u32,
) {
    let (encoded, icc, _) = crate::pipeline::color::encode_rendered_sdr(rendered).unwrap();
    let params = crate::types::OutputParams::default();
    for (suffix, img) in [
        ("full", decimate(&encoded, step)),
        ("crop", crop(&encoded, window.0, window.1, crop_size)),
    ] {
        let path = outdir.join(format!("{frame}-{label}-{suffix}.tif"));
        let (staged, _) = crate::io::encode::encode(&img, &params, Some(&icc), &path).unwrap();
        staged.commit().unwrap();
    }
}

/// X3's reconstruction contrast. Shared by **every** probe in this module — including
/// the ones defined above this line — so retuning X3 cannot leave one measuring a
/// different reconstruction.
const X3_CONTRAST: f32 = 2.03;
/// X3's mid-above-base offset, and the anchor it resolves to. Provisional and fitted —
/// see `algo/exponential-anchor-placement`.
const X3_MID_OFFSET: f32 = 0.508;

fn x3_reference_anchor() -> f32 {
    X3_MID_OFFSET + MID_OUTPUT_DECADES / X3_CONTRAST
}

/// The shipped sigmoid at its defaults, for the roll's `Dmax`. The benchmark every
/// comparison is measured against.
fn benchmark_sigmoid(dmax: f32) -> Reconstruction {
    Reconstruction::Density {
        density: DensityParams::default(),
        curve: DensityCurve::Sigmoid(SigmoidParams {
            contrast: crate::types::REFERENCE_CONTRAST,
            toe: 0.2,
            shoulder: 0.6,
            dmax: DmaxSource::Explicit(
                0.5 * dmax + MID_OUTPUT_DECADES / crate::types::REFERENCE_CONTRAST,
            ),
            anchor: crate::types::AnchorPlacement::WhiteAtDmax,
        }),
    }
}

/// X3 at `anchor`, taken to the shared display source. `density` is cloned because
/// `apply_curve` consumes it and the caller re-renders at several anchors.
fn x3_shared(
    density: &crate::algo::density::DensityImage,
    anchor: f32,
) -> crate::pipeline::render_split::SharedDisplaySource {
    let mut d = density.clone();
    let _ = crate::algo::density::regional_balance(&mut d, &DensityParams::default());
    let film = crate::algo::density::apply_curve(d, |v| 10f32.powf(X3_CONTRAST * (v - anchor)));
    crate::pipeline::render_split::display_source(
        crate::pipeline::working_space::map_nc_film_rgb_v1(film),
        &PrintParams::default(),
    )
    .unwrap()
}

/// The mid patch's destination luminance *before* any tone operator, through the same
/// pinned matrix and luma vector `sdr::destination_rgb` uses.
fn pre_operator_mid_luma(
    shared: &crate::pipeline::render_split::SharedDisplaySource,
    rect: (u32, u32, u32, u32),
) -> f32 {
    use crate::pipeline::colorimetry::pinned::{ACESCG_TO_DISPLAY_P3, DISPLAY_P3_LUMA};
    let (px, py, pw, ph) = rect;
    let width = shared.source.width() as usize;
    let rgb = shared.source.rgb();
    let mut v: Vec<f32> = Vec::new();
    for row in py..py.saturating_add(ph) {
        for col in px..px.saturating_add(pw) {
            let i = (row as usize * width + col as usize) * 3;
            if i + 2 >= rgb.len() {
                continue;
            }
            let aces = [rgb[i], rgb[i + 1], rgb[i + 2]];
            let dest = [
                dot3(ACESCG_TO_DISPLAY_P3[0], aces),
                dot3(ACESCG_TO_DISPLAY_P3[1], aces),
                dot3(ACESCG_TO_DISPLAY_P3[2], aces),
            ];
            let luma = dot3(DISPLAY_P3_LUMA, dest);
            if luma.is_finite() {
                v.push(luma);
            }
        }
    }
    // `pct` on an empty slice is NaN, which propagates silently into every gain solve
    // downstream. Only reachable if the rect falls entirely outside the frame —
    // `rect_of` keeps it inside today — so say so loudly rather than measure nothing.
    assert!(
        !v.is_empty(),
        "mid-patch rect {rect:?} selected no in-frame pixel of a \
         {}x{} source",
        shared.source.width(),
        shared.source.height()
    );
    v.sort_by(f32::total_cmp);
    pct(&v, 0.5)
}

/// The scalar tone curve a resolved `DisplayTone` applies, for solving the match gain
/// without rendering.
fn scalar_operator(tone: DisplayTone) -> impl Fn(f32) -> f32 {
    move |value: f32| match tone {
        DisplayTone::HermiteShoulder(_) => {
            hermite_reference(value, tone.knee_position().expect("a shoulder has a knee"))
        }
        DisplayTone::None => value,
        DisplayTone::ExtendedReinhard(headroom) => {
            crate::pipeline::display_tone::extended_reinhard(value, headroom.white_point())
        }
    }
}

/// The SDR branch's Hermite, duplicated here because `sdr`'s own `shoulder` is private
/// and this harness only needs it to *predict* a render it then verifies against the
/// real one. Every probe that uses it asserts the prediction.
///
/// `start` is the resolved knee position, taken from the `DisplayTone` rather than
/// hardcoded at the default `0.75`: a probe run at a non-default `highlight_compress`
/// would otherwise mis-predict the match gain, and only the downstream `mid err` assert
/// would notice.
fn hermite_reference(value: f32, start: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else if value <= start {
        value
    } else if value >= 1.0 {
        1.0
    } else {
        let span = 1.0 - start;
        let t = (value - start) / span;
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * start + (t3 - 2.0 * t2 + t) * span + (-2.0 * t3 + 3.0 * t2)
    }
}

/// The X3 anchor whose rendered mid patch lands on `target_mid`.
///
/// Shifting the anchor by `ΔA` scales every linear value by `10^(−c·ΔA)`, so this is a
/// gain solve on one scalar rather than a search over renders. `None` when the operator
/// saturates below the target. Callers must still confirm the result against a real
/// render — see the matched probe's `mid err`.
fn matched_anchor(l_mid: f32, target_mid: f32, apply: impl Fn(f32) -> f32) -> Option<f32> {
    let (mut lo, mut hi) = (1e-6f32, 1e6f32);
    if apply(hi * l_mid) < target_mid {
        return None;
    }
    for _ in 0..200 {
        let gain = 0.5 * (lo + hi);
        if apply(gain * l_mid) < target_mid {
            lo = gain;
        } else {
            hi = gain;
        }
    }
    let gain = 0.5 * (lo + hi);
    Some(x3_reference_anchor() - gain.log10() / X3_CONTRAST)
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Unit tests for the highlight-window search, kept beside it.
///
/// Not `#[ignore]`d, and correctly so: the `#[ignore]` discipline in this module's docs
/// is about the **asset-dependent entry points**, which need `../nc-assets` and print
/// derived numbers. These are synthetic and self-contained, exactly like the `tests`
/// module above, so they run in a plain `cargo test` and are meant to.
#[cfg(test)]
mod window_tests {
    use super::*;

    /// The holder guard, on a synthetic frame: a bright edge band (the inverted holder)
    /// beside a dimmer interior peak. Without the inset the search returns the band.
    #[test]
    fn the_highlight_search_ignores_a_bright_frame_edge() {
        let (w, h) = (400u32, 400u32);
        let mut rgb = vec![0.05f32; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y as usize * w as usize + x as usize) * 3;
                // Inverted holder: a bright band down the left edge, sized so a
                // 64 px window sitting on it outscores the interior peak. A narrower
                // band would let the peak win even with no inset, and the test would
                // pass without exercising the guard at all.
                if x < 40 {
                    rgb[i..i + 3].fill(1.0);
                }
                // A dimmer genuine highlight in the interior.
                if (150..250).contains(&x) && (150..250).contains(&y) {
                    rgb[i..i + 3].fill(0.6);
                }
            }
        }
        let img = LinearImage::new(w, h, rgb, None).unwrap();
        let (x, _) = brightest_window(&img, 64);
        assert!(
            x >= (w as f32 * INTERIOR_INSET) as u32,
            "search returned the frame edge at x = {x}"
        );
        assert!(
            (120..=250).contains(&x),
            "expected the interior peak, got x = {x}"
        );
    }
}
