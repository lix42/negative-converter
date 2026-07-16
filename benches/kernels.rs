//! Criterion benches for the per-pixel hot kernels (`perf-instrumentation`
//! task): density conversion, the anchored print render (with and without the
//! auto-Dmax percentile), the simple inversion, and u16/f32 encode.
//!
//! Inputs are **deterministic synthetic images** (a fixed arithmetic pattern,
//! no RNG) sized to be meaningful (~3.1 MP — big enough that the rayon loops
//! dominate setup noise), so numbers are comparable across runs on a machine.
//! Encode benches write into an in-memory cursor to isolate the kernel from
//! disk I/O. Not a CI gate (bench runners are too noisy for a hard gate);
//! baselines are recorded in `docs/progress.md` — compare against those when
//! touching a kernel.

use std::hint::black_box;
use std::io::Cursor;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use nc::algo::density::{DensityImage, to_density};
use nc::algo::{AlgoParams, build};
use nc::io::encode::encode_to_writer;
use nc::types::{
    BigTiff, DensityParams, DmaxSource, FilmBase, LinearImage, OutputParams, PrintParams,
    SimpleParams,
};

/// Benchmark image dimensions: ~3.1 MP (real scans are 20–60 MP; this is large
/// enough for per-pixel cost to dominate while keeping `cargo bench` quick).
const W: u32 = 2048;
const H: u32 = 1536;

/// The film base the synthetic negative is built around (an orange-ish Dmin).
const BASE: [f32; 3] = [0.9, 0.55, 0.42];

/// A deterministic synthetic negative: transmissions in `(0, BASE_c]` from a
/// fixed arithmetic pattern (no RNG), so every run benches identical data.
fn synthetic_negative(w: u32, h: u32) -> LinearImage {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            for (c, base) in BASE.iter().enumerate() {
                // Pseudo-texture in [0.05, 1.0), scaled under the base channel.
                let t = ((x as usize * 31 + y as usize * 17 + c * 7) % 953) as f32 / 1000.0;
                rgb.push((0.05 + t) * base);
            }
        }
    }
    LinearImage::new(w, h, rgb, None).unwrap()
}

fn film_base() -> FilmBase {
    FilmBase::from(BASE)
}

fn bench_kernels(c: &mut Criterion) {
    let img = synthetic_negative(W, H);
    let base = film_base();
    let pixels = u64::from(W) * u64::from(H);

    let mut group = c.benchmark_group("kernels");
    group.sample_size(20);
    group.throughput(Throughput::Elements(pixels));

    // Stage 1–2 kernel: transmission → corrected density (log10 per sample).
    let density_params = DensityParams::default();
    group.bench_function("to_density", |b| {
        b.iter(|| {
            to_density(
                black_box(&img),
                black_box(&base),
                black_box(&density_params),
            )
        });
    });

    // Stage 3–4 kernel: density → positive (pow10 per sample). `render`
    // consumes its input, so each iteration gets a fresh clone via
    // `iter_batched`. `auto` additionally runs the auto-Dmax percentile
    // (strided sample + select_nth); the delta against `none` is its cost.
    let dimg = to_density(&img, &base, &density_params);
    let print = PrintParams::default();
    for (name, dmax) in [
        ("render_auto", DmaxSource::Auto),
        ("render_none", DmaxSource::None),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || dimg.clone(),
                |d: DensityImage| nc::algo::density::render(d, 1.0, dmax, &print),
                BatchSize::LargeInput,
            );
        });
    }

    // The simple channel-inversion baseline, via the public Converter seam.
    let simple = build(AlgoParams::Simple(SimpleParams::default()));
    group.bench_function("simple_invert", |b| {
        b.iter(|| simple.convert(black_box(&img), black_box(&base)).unwrap());
    });

    // Encode kernels: quantize/scan + TIFF write into memory (no disk I/O).
    // `hdr` picks the depth: false ⇒ u16 (quantize), true ⇒ f32 (verbatim).
    for (name, hdr) in [("encode_u16", false), ("encode_f32", true)] {
        let params = OutputParams {
            hdr,
            output_profile: None,
            bigtiff: BigTiff::Off,
        };
        let capacity = img.rgb.len() * 4 + 4096;
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut buf = Cursor::new(Vec::with_capacity(capacity));
                encode_to_writer(&mut buf, black_box(&img), &params, None).unwrap()
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_kernels);
criterion_main!(benches);
