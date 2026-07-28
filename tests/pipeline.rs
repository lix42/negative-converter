//! End-to-end pipeline tests — drive the compiled `nc` binary against the
//! committed real-scan fixtures (`tests/fixtures/`) and assert on exit codes,
//! the JSON report on stdout, and the files written. This exercises the full
//! decode → film-base → algorithm → color → encode path that the unit tests
//! (which stop at module boundaries) can't.
//!
//! stdout must stay pure JSON (the agent contract), so every test parses it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use tiff::encoder::{TiffEncoder, colortype};

/// The binary under test, provided by Cargo for integration tests.
const NC: &str = env!("CARGO_BIN_EXE_nc");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Synthesize a uniform 16-bit RGB TIFF (the `RGB(16)` chunky layout the decoder
/// accepts) with every pixel set to `rgb`, at `path`. Stands in for a fully-exposed
/// reference leader frame in the roll-fixed `Dmax` tests — the real light-struck
/// leaders (Ektar/Phoenix) aren't committed, and the reference path must be
/// exercised with a realistic near-opaque *non-zero* transmission (an all-zero
/// region is now a hard error). Encodes into memory then writes the whole buffer,
/// so the file can't be left truncated by a dropped writer.
fn write_uniform_rgb48(path: &Path, rgb: [u16; 3], w: u32, h: u32) {
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..(w * h) {
        data.extend_from_slice(&rgb);
    }
    let mut buf = Vec::new();
    {
        let mut enc = TiffEncoder::new(std::io::Cursor::new(&mut buf)).unwrap();
        enc.write_image::<colortype::RGB16>(w, h, &data).unwrap();
    }
    std::fs::write(path, &buf).unwrap();
}

/// Minimal synthetic SilverFast XMP packet (the real one is ~150 KB; only the
/// `Silverfast:` mode attributes matter). `attrs` is the attribute list on the
/// `rdf:Description` element.
fn silverfast_xmp(attrs: &str) -> String {
    format!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"\" xmlns:Silverfast=\"LSI/\" {attrs}/>\
         </rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
    )
}

/// Write an 8x8 RGB16 TIFF with optional SilverFast XMP (tag 700), an optional
/// `Software` tag, and an optional matching Gray16 IR page — the levers the
/// provenance gate keys on (and the two holes the adversarial review flagged:
/// Software-only and IR-only).
fn write_rgb16(path: &Path, xmp: Option<&str>, software: Option<&str>, with_ir: bool) {
    use tiff::tags::Tag;
    let (w, h) = (8u32, 8u32);
    let rgb = vec![20000u16; (w * h * 3) as usize];
    let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
    let mut image = enc.new_image::<colortype::RGB16>(w, h).unwrap();
    if let Some(s) = software {
        image.encoder().write_tag(Tag::Software, s).unwrap();
    }
    if let Some(x) = xmp {
        image
            .encoder()
            .write_tag(Tag::Unknown(700), x.as_bytes())
            .unwrap();
    }
    image.write_data(&rgb).unwrap();
    if with_ir {
        let ir = vec![0u16; (w * h) as usize];
        enc.write_image::<colortype::Gray16>(w, h, &ir).unwrap();
    }
}

/// A unique temp directory that removes itself (and its contents) on drop, so a
/// failing test can't leak output TIFFs.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nc-e2e-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `nc` with `args`; return (exit code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    run_env(args, &[])
}

/// Like [`run`], but with extra environment variables set for the child (used to
/// point `NC_TELEMETRY_LOG` at a temp file so telemetry tests never touch the
/// real user data dir).
fn run_env(args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(NC);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn nc binary");
    (
        out.status.code().expect("process terminated by signal"),
        String::from_utf8(out.stdout).expect("stdout is not UTF-8"),
        String::from_utf8(out.stderr).expect("stderr is not UTF-8"),
    )
}

/// Parse stdout as JSON, failing with the raw text if it isn't clean JSON.
fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}):\n{stdout}"))
}

/// The sidecar path for an output (`out.tiff` → `out.tiff.json`).
fn sidecar_of(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.json", output.display()))
}

/// Parse a sidecar document whole: `{ "meta": {…identity…}, "params": {…recipe…} }`
/// (`core/conversion-versioning`).
fn sidecar(output: &Path) -> serde_json::Value {
    let path = sidecar_of(output);
    let txt = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read sidecar {}: {e}", path.display()));
    serde_json::from_str(&txt)
        .unwrap_or_else(|e| panic!("sidecar {} is not valid JSON ({e})", path.display()))
}

/// Just the sidecar's **recipe body** — what used to be the whole document before
/// the identity envelope. Identity rides in `meta` precisely so this body stays a
/// bare, `--params`-reloadable recipe.
fn sidecar_params(output: &Path) -> serde_json::Value {
    let doc = sidecar(output);
    assert!(
        doc.get("params").is_some(),
        "sidecar must be the {{meta, params}} envelope, got keys {:?}",
        doc.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    doc["params"].clone()
}

/// A file that starts with the little-endian TIFF magic ("II", 42 or 43).
fn is_tiff(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    bytes.len() > 4
        && &bytes[0..2] == b"II"
        && matches!(u16::from_le_bytes([bytes[2], bytes[3]]), 42 | 43)
}

#[test]
fn convert_simple_writes_tiff_sidecar_and_report() {
    let tmp = TempDir::new("simple");
    let out = tmp.path("out.tiff");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        // Real scans are holder → rebate → picture, so auto-base fails loudly;
        // supply an explicit base (the documented calibrate-once workflow).
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "simple convert should succeed");
    assert!(is_tiff(&out), "output must be a valid TIFF");
    // Effective-recipe sidecar next to the output, valid JSON — recipe body under
    // `params`, beside the `meta` identity envelope.
    let recipe = sidecar_params(&out);
    assert_eq!(recipe["reconstruction"]["type"], "simple");
    assert_eq!(recipe["reconstruction"]["schema_version"], 1);

    let report = json(&stdout);
    assert_eq!(report["command"], "convert");
    assert_eq!(
        report["reconstruction_result"],
        serde_json::json!({"type": "simple"})
    );
    assert_eq!(report["recipe"]["reconstruction"]["type"], "simple");
    // The pinned working-space mapping is stamped on every convert report
    // (design-spec §8), independent of reconstruction path — here `simple`.
    assert_eq!(report["working_mapping"], "nc-film-rgb-v1");
    assert_eq!(report["output"], out.to_str().unwrap());
    assert!(report["film_base"].is_object(), "film base reported");
    assert!(report["loss"].is_object(), "encode loss reported");
    assert!(report["elapsed_ms"].is_number());
}

#[test]
fn convert_density_f32_avoids_clipping() {
    // f32 output preserves the full scene-referred/HDR range with no clamp, so a
    // density conversion writes zero clipped/non-finite samples regardless of how
    // hot the render is (the u16 path is the one that clamps).
    let tmp = TempDir::new("density-f32");
    let out = tmp.path("out.tiff");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "density",
        "--output-hdr",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "density f32 convert should succeed:\n{stdout}");
    assert!(is_tiff(&out));
    let report = json(&stdout);
    assert_eq!(report["loss"]["clipped_low"], 0);
    assert_eq!(report["loss"]["clipped_high"], 0);
    assert_eq!(report["loss"]["non_finite"], 0);
}

#[test]
fn u16_clipping_is_reported_and_strict_promotes_it() {
    // Force guaranteed u16 clipping with a large positive `--print-exposure`
    // (2^12× gain blows every highlight past 1.0), so this test pins the
    // clip-reporting + `--strict` mechanism *independently* of the density
    // default's baseline exposure (which the dmax-white-anchor task tunes).
    // The HDR fixture carries no IR plane, so the only warning is the clipping —
    // proving clipping alone drives the strict failure.
    let tmp = TempDir::new("u16-clip");
    let base_args = |extra: &[&str], out: &Path| {
        let mut v = vec![
            "convert",
            "__IN__",
            "-o",
            "__OUT__",
            "--reconstruction",
            "density",
            "--film-base",
            "0.9,0.55,0.42",
            "--print-exposure",
            "12",
        ];
        v.extend_from_slice(extra);
        v.into_iter()
            .map(|s| match s {
                "__IN__" => fixture("hdr-48bit.tif").to_str().unwrap().to_string(),
                "__OUT__" => out.to_str().unwrap().to_string(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
    };

    // Non-strict: clipping is a warning, the run still succeeds.
    let out = tmp.path("out.tiff");
    let argv = base_args(&[], &out);
    let (code, stdout, _err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 0, "non-strict clipping run should still succeed");
    let report = json(&stdout);
    assert!(
        report["loss"]["clipped_high"].as_u64().unwrap() > 0,
        "a +12-stop exposure must clip highlights: {report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("clipped")),
        "a clipping warning must be reported: {report}"
    );

    // Strict: the clipping warning becomes a non-zero exit (exactly 1, Other).
    let out2 = tmp.path("out2.tiff");
    let argv = base_args(&["--strict"], &out2);
    let (code, _stdout, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        code, 1,
        "--strict must fail (exit 1) when a warning is present"
    );
    assert!(
        err.contains("strict"),
        "stderr should explain the strict failure: {err}"
    );
}

#[test]
fn inspect_reports_decode_facts() {
    let (code, stdout, _err) = run(&["inspect", fixture("hdri-64bit.tif").to_str().unwrap()]);
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(report["command"], "inspect");
    assert_eq!(report["decode"]["format"], "hdri");
    assert_eq!(report["decode"]["width"], 502);
    assert_eq!(report["decode"]["height"], 462);
    assert_eq!(report["decode"]["ir_present"], true);
    // No image is written by inspect.
    assert!(report["output"].is_null());
}

#[test]
fn estimate_from_region_reports_film_base() {
    let (code, stdout, _err) = run(&[
        "estimate",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--base-region",
        "0,0,60,60",
    ]);
    assert_eq!(code, 0, "region estimate should succeed:\n{stdout}");
    let report = json(&stdout);
    assert_eq!(report["command"], "estimate");
    assert!(report["film_base"]["r"].is_number());
    assert!(report["film_base"]["g"].is_number());
    assert!(report["film_base"]["b"].is_number());
    // Structured source: {"region":[x,y,w,h]}, so the sampled rect is machine-readable.
    assert_eq!(
        report["film_base_source"]["region"],
        serde_json::json!([0, 0, 60, 60])
    );
}

#[test]
fn mixed_base_region_warns_and_strict_refuses_it() {
    // A rectangle mixing image content is a plausible-looking bad base; the
    // uniformity warning must ride the report (estimate), and --strict must
    // promote it to a failure (convert) — while the non-strict convert still
    // succeeds with the warning recorded.
    let (code, stdout, _err) = run(&[
        "estimate",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--base-region",
        "0,0,502,462",
    ]);
    assert_eq!(code, 0, "a mixed region is a warning, not an error");
    let report = json(&stdout);
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("not uniform")),
        "uniformity warning expected: {report}"
    );

    let tmp = TempDir::new("region-warn");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--base-region",
        "0,0,502,462",
        "--strict",
    ]);
    assert_eq!(
        code, 1,
        "--strict must refuse a non-uniform base region: {err}"
    );

    // `estimate --strict` refuses it too — the command that bakes the Dmin a
    // roll is calibrated on must not echo a plausible-looking-but-bad base.
    let (code, _stdout, err) = run(&[
        "estimate",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--base-region",
        "0,0,502,462",
        "--strict",
    ]);
    assert_eq!(
        code, 1,
        "estimate --strict must refuse a mixed region: {err}"
    );
}

#[test]
fn estimate_emits_reuse_ready_output_that_round_trips() {
    // The calibrate-once → reuse workflow (design-spec §8): `estimate` must emit
    // the measured base as a paste-ready `--film-base` flag and a `film_base`
    // recipe fragment, and feeding either back to `convert` must reproduce the
    // exact same base (and thus a byte-identical output).
    let tmp = TempDir::new("reuse");
    let fix = fixture("hdr-48bit.tif");
    // Focus: the reuse round-trip. (This real-photo fixture has no
    // region-uniform patch, so the inward-scan uniformity check warns on any
    // `--base-region` here — `--strict` estimate behavior is covered separately
    // by `mixed_base_region_warns_and_strict_refuses_it`.)
    let (code, stdout, err) = run(&[
        "estimate",
        fix.to_str().unwrap(),
        "--base-region",
        "0,0,60,60",
    ]);
    assert_eq!(code, 0, "estimate should succeed: {err}");
    let report = json(&stdout);
    let base = report["film_base"].clone();

    // The flag string is `--film-base R,G,B` with the measured values.
    let flag = report["film_base_flag"].as_str().expect("flag emitted");
    let value = flag.strip_prefix("--film-base ").expect("flag prefix");
    // The recipe fragment is the documented `{"source":{"explicit":[…]}}` shape,
    // carrying exactly the same numbers as the measurement.
    let fragment = &report["film_base_recipe"];
    assert_eq!(
        fragment["source"]["explicit"],
        serde_json::json!([base["r"], base["g"], base["b"]]),
        "fragment must carry the measured base: {report}"
    );

    // Round-trip A: the flag value fed to `convert` reproduces the base.
    let out_flag = tmp.path("flag.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out_flag.to_str().unwrap(),
        "--output-hdr",
        "--film-base",
        value,
    ]);
    assert_eq!(code, 0, "{err}");
    let convert_report = json(&stdout);
    assert_eq!(
        convert_report["film_base"], base,
        "--film-base from the flag string must reproduce the measured base"
    );

    // Round-trip B: the fragment pasted into a recipe reproduces the base and
    // a byte-identical output (determinism across the two reuse forms).
    let recipe = tmp.path("roll.json");
    std::fs::write(
        &recipe,
        serde_json::json!({ "film_base": fragment }).to_string(),
    )
    .unwrap();
    let out_recipe = tmp.path("recipe.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out_recipe.to_str().unwrap(),
        "--output-hdr",
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "fragment must load as a valid recipe: {err}");
    assert_eq!(json(&stdout)["film_base"], base);
    assert_eq!(
        std::fs::read(&out_flag).unwrap(),
        std::fs::read(&out_recipe).unwrap(),
        "flag and fragment reuse must produce byte-identical outputs"
    );
}

#[test]
fn estimate_measures_roll_fixed_dmax_from_a_reference_region_and_it_round_trips() {
    // The roll-fixed `Dmax` calibration (dmax-reference, design-spec §8): point
    // `estimate --d-max-region` at a fully-exposed (near-opaque) reference frame,
    // with an explicit `--film-base` (the `Dmin` from the unexposed frame), and it
    // measures a single positive scalar `Dmax`, records the region as provenance,
    // and emits reuse-ready `--d-max` / `reconstruction.curve.dmax` forms. Feeding the frozen
    // scalar back to `convert` reproduces it exactly (deterministic apply).
    //
    // A synthesized near-opaque leader stands in for the real one (no real leader
    // frame is committed — real-leader verification, Ektar/Phoenix, is deferred to
    // the user per the task). Uniform ~2% transmission (u16 1311/65535 ≈ 0.0200,
    // within the real leader's ~0.016–0.039 luma), so against the base below it
    // yields a plausible positive scalar `Dmax` (≈ 1.4) — the reference path is
    // exercised with a realistic value, and it clears the plausibility floor (no
    // warning). An all-zero region would now hard-error as degenerate.
    let tmp = TempDir::new("dmaxref");
    let leader = tmp.path("leader.tiff");
    write_uniform_rgb48(&leader, [1311, 1311, 1311], 64, 64);
    let (code, stdout, err) = run(&[
        "estimate",
        leader.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max-region",
        "0,0,16,16",
    ]);
    assert_eq!(code, 0, "reference Dmax estimate should succeed: {err}");
    let report = json(&stdout);
    let dmax = report["dmax"].as_f64().expect("a scalar Dmax is reported");
    assert!(
        dmax > 0.0 && dmax.is_finite(),
        "Dmax must be positive: {report}"
    );
    // Provenance: the sampled region, not a re-read directive.
    assert_eq!(
        report["dmax_region"],
        serde_json::json!([0, 0, 16, 16]),
        "the reference region is recorded as provenance: {report}"
    );
    // Reuse-ready forms carry exactly the measured scalar.
    let flag = report["d_max_flag"].as_str().expect("d_max_flag emitted");
    let value = flag.strip_prefix("--d-max ").expect("flag prefix");
    assert_eq!(
        report["d_max_recipe"]["dmax"]["explicit"], report["dmax"],
        "the recipe fragment must carry the measured scalar: {report}"
    );

    // Freeze A: the `--d-max` flag value fed to `convert` reproduces the anchor.
    let out = tmp.path("flag.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max",
        value,
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        json(&stdout)["dmax"],
        report["dmax"],
        "the frozen --d-max scalar must reproduce the measured anchor"
    );

    // Freeze B: the curve fragment pasted into a roll recipe's tagged
    // `reconstruction.curve` loads and reproduces the same anchor (deterministic
    // apply from the frozen recipe).
    let recipe = tmp.path("roll.json");
    std::fs::write(
        &recipe,
        serde_json::json!({ "reconstruction": { "curve": {
            "type": "exponential", "dmax": report["d_max_recipe"]["dmax"] } } })
        .to_string(),
    )
    .unwrap();
    let out2 = tmp.path("recipe.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "the dmax fragment must load as a valid recipe: {err}"
    );
    assert_eq!(
        json(&stdout)["dmax"],
        report["dmax"],
        "the frozen reconstruction.curve.dmax must reproduce the measured anchor"
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&out2).unwrap(),
        "flag and recipe freeze must produce byte-identical outputs"
    );
}

#[test]
fn convert_default_uses_the_fixed_roll_anchor_not_per_frame_auto() {
    // dmax-reference changed the default render: the anchor is the roll-fixed
    // nominal `Fixed` (NOMINAL_DMAX = 2.0), not the demoted per-frame `Auto`. Pin
    // the default's reported anchor, and that `--auto-d-max` (opt-in) differs from
    // it — proving the default no longer normalizes exposure per frame.
    let tmp = TempDir::new("dmaxdefault");
    let fix = fixture("hdr-48bit.tif");
    let base = ["--film-base", "0.9,0.55,0.42"];

    let out = tmp.path("default.tiff");
    let mut args = vec![
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    args.extend_from_slice(&base);
    let (code, stdout, err) = run(&args);
    assert_eq!(code, 0, "{err}");
    let default_dmax = json(&stdout)["dmax"].as_f64().expect("dmax reported");
    assert!(
        (default_dmax - 2.0).abs() < 1e-6,
        "default anchor must be the fixed nominal 2.0, got {default_dmax}"
    );

    let out2 = tmp.path("auto.tiff");
    let mut args = vec![
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
        "--auto-d-max",
    ];
    args.extend_from_slice(&base);
    let (code, stdout, err) = run(&args);
    assert_eq!(code, 0, "{err}");
    let auto_dmax = json(&stdout)["dmax"].as_f64().expect("dmax reported");
    assert!(
        (auto_dmax - default_dmax).abs() > 1e-3,
        "opt-in --auto-d-max ({auto_dmax}) must differ from the fixed default ({default_dmax})"
    );
}

#[test]
fn estimate_d_max_region_rejects_a_degenerate_all_black_region() {
    // A reference region on the all-black fixture (transmission 0 → floored) is a
    // degenerate / clipped sample, not a fully-exposed leader. `reference_dmax`
    // must hard-error (exit 1) rather than launder the floor into a huge density
    // and freeze a black-rendering anchor — the Dmin "dark holder → zero channel"
    // gotcha, applied to Dmax. (This is exactly what the all-black fixture used to
    // stand in for as a "leader"; that stand-in is now a guarded error.)
    let fix = fixture("black-48bit.tif");
    let (code, _stdout, err) = run(&[
        "estimate",
        fix.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max-region",
        "0,0,16,16",
    ]);
    assert_eq!(
        code, 1,
        "a degenerate (all-black) reference region must fail loudly: {err}"
    );
    assert!(
        err.contains("reference Dmax"),
        "the error names the reference-Dmax failure: {err}"
    );
}

#[test]
fn estimate_d_max_region_warns_on_an_implausibly_low_reference() {
    // A mid-tone region only somewhat denser than base yields a valid but
    // implausibly-low anchor for a fully-exposed leader. `estimate` must not reject
    // it (thin/unusual stock varies) but must emit a loud, `--strict`-promotable
    // warning for the user's manual review.
    let tmp = TempDir::new("dmaxlow");
    let leader = tmp.path("midtone.tiff");
    // Uniform 30% transmission (u16 19660/65535 ≈ 0.300): denser than base on every
    // channel (base min 0.42 > 0.30 ⇒ per-channel density > 0, so no hard error),
    // but the gray-mean density (≈ 0.30) is well below the plausibility floor (1.0).
    write_uniform_rgb48(&leader, [19660, 19660, 19660], 32, 32);
    let region = [
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max-region",
        "0,0,16,16",
    ];

    let mut args = vec!["estimate", leader.to_str().unwrap()];
    args.extend_from_slice(&region);
    let (code, stdout, err) = run(&args);
    assert_eq!(
        code, 0,
        "implausibly-low reference must not hard-fail: {err}"
    );
    let report = json(&stdout);
    let dmax = report["dmax"].as_f64().expect("a dmax is still measured");
    assert!(dmax > 0.0 && dmax < 1.0, "a low positive anchor: {dmax}");
    let warnings = report["warnings"].as_array().expect("warnings present");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("implausibly low")),
        "a plausibility warning must be present: {report}"
    );

    // `--strict` promotes the warning to a failing exit.
    let mut sargs = vec!["estimate", leader.to_str().unwrap()];
    sargs.extend_from_slice(&region);
    sargs.push("--strict");
    let (scode, _s, serr) = run(&sargs);
    assert_eq!(
        scode, 1,
        "--strict promotes the plausibility warning: {serr}"
    );
}

#[test]
fn estimate_d_max_region_skipped_on_a_degenerate_grid_base() {
    // When the resolved base is degenerate (a `--grid` on the all-black fixture),
    // the `--d-max-region` measurement is skipped — measuring against an unusable
    // base would only mask the degenerate-base error with a confusing secondary
    // one. The report carries no `dmax`, and the run still hard-errors on the
    // degenerate base itself (exit 1), same as without `--d-max-region`.
    let fix = fixture("black-48bit.tif");
    let (code, stdout, err) = run(&[
        "estimate",
        fix.to_str().unwrap(),
        "--grid",
        "--d-max-region",
        "0,0,16,16",
    ]);
    assert_eq!(code, 1, "the degenerate grid base still hard-errors: {err}");
    let report = json(&stdout);
    assert!(
        report["dmax"].is_null(),
        "no Dmax is measured against a degenerate base: {report}"
    );
    assert!(
        err.contains("finite and positive"),
        "the error is the degenerate-base one, not a secondary Dmax error: {err}"
    );
}

#[test]
fn estimate_grid_reports_spread_and_strict_promotes_disagreement() {
    // `--grid` samples 5 fixed cells; on a real (non-blank) frame the cells
    // disagree, which must be reported loudly — per-cell evidence in the
    // report, a warning, and a failing exit under `--strict` — never averaged
    // away silently.
    let fix = fixture("hdr-48bit.tif");
    let (code, stdout, err) = run(&["estimate", fix.to_str().unwrap(), "--grid"]);
    assert_eq!(
        code, 0,
        "non-strict disagreement is a warning, not fatal: {err}"
    );
    let report = json(&stdout);
    let grid = &report["grid"];
    assert_eq!(grid["cells"].as_array().unwrap().len(), 5);
    assert_eq!(grid["agreement"], false, "picture content must disagree");
    assert!(grid["spread"][0].as_f64().unwrap() > grid["tolerance"].as_f64().unwrap());
    assert!(
        grid["cells"][0]["region"].is_array() && grid["cells"][0]["base"]["r"].is_number(),
        "per-cell evidence must be reported: {report}"
    );
    // The sampled rectangle (the fixture's full 502x462 frame) is recorded as
    // the structured source.
    assert_eq!(
        report["film_base_source"]["region"],
        serde_json::json!([0, 0, 502, 462])
    );
    // The grid path feeds the same reuse-ready output as a single measurement
    // (the combined median base here is valid, so the flag must be present).
    assert!(
        report["film_base_flag"].is_string(),
        "grid runs emit reuse-ready output too: {report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("grid cells disagree")),
        "disagreement must be a report warning: {report}"
    );

    // `--strict` promotes the disagreement warning to exit 1 after the report.
    let (code, stdout, err) = run(&["estimate", fix.to_str().unwrap(), "--grid", "--strict"]);
    assert_eq!(code, 1, "--strict must fail on grid disagreement");
    let _ = json(&stdout); // the report still lands on stdout before the gate
    assert!(err.contains("strict"), "stderr should explain: {err}");
}

#[test]
fn estimate_grid_degenerate_base_hard_errors_without_strict() {
    // A degenerate combined grid base (an all-black frame — the same condition a
    // `--grid --base-region` on the dark holder produces) is not a usable Dmin
    // anchor. The grid path must hard-error on it **without** `--strict`, mapping
    // to the same exit code the single-measurement path's finite-and-positive
    // guard returns for a degenerate base (`NcError::Other` → exit 1) — and the
    // diagnostic report (with `grid.cells`) must still land on stdout first.
    let fix = fixture("black-48bit.tif");

    // The single-measurement degenerate exit code, established on the same input:
    // a `--base-region` on the all-black frame fails `estimate`'s birth guard.
    let (single_code, _stdout, single_err) = run(&[
        "estimate",
        fix.to_str().unwrap(),
        "--base-region",
        "0,0,32,32",
    ]);
    assert_eq!(single_code, 1, "single-path degenerate base is exit 1");
    assert!(
        single_err.contains("finite and positive"),
        "single-path error names the degenerate condition: {single_err}"
    );

    // The grid path on the same frame — no `--strict` — must match that exit code.
    let (code, stdout, err) = run(&["estimate", fix.to_str().unwrap(), "--grid"]);
    assert_eq!(
        code, single_code,
        "grid degenerate base must map to the single-path exit code without --strict: {err}"
    );
    // The report is emitted before the gate: stdout is clean JSON carrying the
    // five grid cells that diagnose the degenerate sample.
    let report = json(&stdout);
    assert_eq!(report["command"], "estimate");
    assert_eq!(report["grid"]["cells"].as_array().unwrap().len(), 5);
    assert_eq!(report["grid"]["agreement"], false);
    // No reuse-ready output for a degenerate base.
    assert!(
        report["film_base_flag"].is_null(),
        "a degenerate base must not be advertised as reusable: {report}"
    );
    assert!(
        err.contains("finite and positive"),
        "the hard error names the degenerate condition: {err}"
    );
}

#[test]
fn export_ir_writes_plane_for_hdri_and_errors_for_hdr() {
    let tmp = TempDir::new("ir");
    // HDRi: the IR plane is written.
    let out = tmp.path("out.tiff");
    let ir = tmp.path("ir.tiff");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--film-base",
        "0.9,0.55,0.42",
        "--export-ir",
        ir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "HDRi export-ir should succeed:\n{stdout}");
    assert!(is_tiff(&ir), "IR plane TIFF must be written");
    assert_eq!(json(&stdout)["ir_exported"], ir.to_str().unwrap());

    // HDR: no IR plane, so --export-ir fails loudly with exit 4 (Unsupported),
    // before writing the main output.
    let out_hdr = tmp.path("out-hdr.tiff");
    let ir_hdr = tmp.path("ir-hdr.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out_hdr.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--export-ir",
        ir_hdr.to_str().unwrap(),
    ]);
    assert_eq!(code, 4, "export-ir on an HDR scan is Unsupported (exit 4)");
    assert!(
        !out_hdr.exists(),
        "no output should be written on the fast-fail path"
    );
    assert!(err.to_lowercase().contains("ir"));
}

#[test]
fn bad_params_are_usage_errors() {
    let tmp = TempDir::new("usage");
    let out = tmp.path("out.tiff");
    // An impossible knob value (zero exponential gamma) is rejected at the CLI
    // boundary (exit 2).
    let (code, _stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--density-gamma",
        "0",
    ]);
    assert_eq!(code, 2, "invalid params must exit 2");
    assert!(!out.exists(), "no output on a usage error");

    // The removed simple clip controls are migration errors (exit 2), and the
    // removed --algorithm selector points at --reconstruction/--density-curve.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--clip-low",
        "0.9",
    ]);
    assert_eq!(code, 2, "a removed flag must exit 2: {err}");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--algorithm",
        "density",
    ]);
    assert_eq!(code, 2, "--algorithm must be a migration error: {err}");
    assert!(
        err.contains("--reconstruction"),
        "the migration error names the replacement: {err}"
    );
    assert!(!out.exists(), "no output on a usage error");
}

#[test]
fn convert_is_deterministic() {
    // The project's defining contract: same inputs + params ⇒ byte-identical
    // output. Convert the same fixture twice and compare the TIFF + sidecar.
    let tmp = TempDir::new("determinism");
    let args = |out: &Path| {
        vec![
            "convert".to_string(),
            fixture("hdri-64bit.tif").to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--reconstruction".to_string(),
            "density".to_string(),
            "--output-hdr".to_string(),
            "--film-base".to_string(),
            "0.9,0.55,0.42".to_string(),
            "--report".to_string(),
            "none".to_string(),
        ]
    };
    let a = tmp.path("a.tiff");
    let b = tmp.path("b.tiff");
    let (ca, _, _) = run(&args(&a).iter().map(String::as_str).collect::<Vec<_>>());
    let (cb, _, _) = run(&args(&b).iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!((ca, cb), (0, 0));
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "output TIFF must be byte-identical across runs"
    );
    assert_eq!(
        std::fs::read(format!("{}.json", a.display())).unwrap(),
        std::fs::read(format!("{}.json", b.display())).unwrap(),
        "sidecar recipe must be byte-identical across runs"
    );
}

#[test]
fn sidecar_recipe_round_trips_through_recipe_in() {
    // Run A writes the effective recipe sidecar; run B consumes it via --params
    // with no other knobs and must produce a byte-identical output — the
    // measure-once-reuse-for-the-roll workflow.
    let tmp = TempDir::new("recipe");
    let out_a = tmp.path("a.tiff");
    let (ca, _, _) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out_a.to_str().unwrap(),
        "--reconstruction",
        "density",
        "--output-hdr",
        "--film-base",
        "0.9,0.55,0.42",
        "--density-gamma",
        "1.8",
        "--report",
        "none",
    ]);
    assert_eq!(ca, 0);
    let sidecar = format!("{}.json", out_a.display());

    let out_b = tmp.path("b.tiff");
    let (cb, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out_b.to_str().unwrap(),
        "--params",
        &sidecar,
        "--report",
        "none",
    ]);
    assert_eq!(
        cb, 0,
        "recipe reload should succeed (deny_unknown_fields clean):\n{err}"
    );
    assert_eq!(
        std::fs::read(&out_a).unwrap(),
        std::fs::read(&out_b).unwrap(),
        "reloading the sidecar recipe must reproduce the output"
    );
}

#[test]
fn sigmoid_sidecar_recipe_round_trips_through_recipe_in() {
    // Same measure-once-reuse workflow for `sigmoid`, with NON-default toe/shoulder
    // so the round-trip actually exercises the sigmoid four-spot serialization +
    // merge (a dropped `sigmoid.*` key or a forgotten merge arm would change the
    // reloaded output). Run A writes the sidecar; run B consumes it and must be
    // byte-identical.
    let tmp = TempDir::new("sigmoid-recipe");
    let out_a = tmp.path("a.tiff");
    let (ca, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out_a.to_str().unwrap(),
        "--density-curve",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--sigmoid-contrast",
        "1.4",
        "--sigmoid-toe",
        "0.12",
        "--sigmoid-shoulder",
        "0.33",
        "--report",
        "none",
    ]);
    assert_eq!(ca, 0, "{err}");
    let sidecar = sidecar_of(&out_a).display().to_string();
    // The sidecar's recipe body carries the sigmoid section verbatim.
    let recipe = sidecar_params(&out_a);
    assert_eq!(recipe["reconstruction"]["curve"]["type"], "sigmoid");
    assert_eq!(recipe["reconstruction"]["curve"]["contrast"], 1.4);
    assert_eq!(recipe["reconstruction"]["curve"]["toe"], 0.12);
    assert_eq!(recipe["reconstruction"]["curve"]["shoulder"], 0.33);

    let out_b = tmp.path("b.tiff");
    let (cb, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out_b.to_str().unwrap(),
        "--params",
        &sidecar,
        "--report",
        "none",
    ]);
    assert_eq!(cb, 0, "sigmoid recipe reload should succeed:\n{err}");
    assert_eq!(
        std::fs::read(&out_a).unwrap(),
        std::fs::read(&out_b).unwrap(),
        "reloading the sigmoid sidecar recipe must reproduce the output"
    );
}

#[test]
fn unreadable_input_is_decode_error_exit_three() {
    let tmp = TempDir::new("decode");
    let bad = tmp.path("not-a.tiff");
    std::fs::write(&bad, b"this is not a TIFF file").unwrap();
    let (code, _stdout, _err) = run(&["inspect", bad.to_str().unwrap()]);
    assert_eq!(code, 3, "a non-TIFF input is a decode error (exit 3)");
}

#[test]
fn unwritable_output_is_write_error_exit_five() {
    // Output into a nonexistent directory: encode's File::create fails → exit 5.
    let tmp = TempDir::new("write");
    let out = tmp.path("no-such-dir/out.tiff");
    let (code, _stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 5,
        "an unwritable output path is a write error (exit 5)"
    );
}

#[test]
fn verbose_keeps_stdout_clean_json_and_logs_to_stderr() {
    // -v adds progress lines; they must go to stderr only — stdout stays pure
    // JSON (the agent contract). --report-file redirects the report off stdout.
    let tmp = TempDir::new("verbose");
    let out = tmp.path("out.tiff");
    let (code, stdout, stderr) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--film-base",
        "0.9,0.55,0.42",
        "-v",
    ]);
    assert_eq!(code, 0);
    // stdout is still a single clean JSON object.
    let _ = json(&stdout);
    // The progress line landed on stderr, not stdout.
    assert!(
        stderr.contains("decoded"),
        "progress log should be on stderr: {stderr}"
    );
    // Check the actual stderr log marker (`nc: decoded …`), not a bare "decoded"
    // substring — the JSON report legitimately carries a `transfer_decoded` field.
    assert!(
        !stdout.contains("nc: decoded"),
        "stdout must not carry log lines"
    );
}

#[test]
fn report_file_writes_json_off_stdout() {
    let tmp = TempDir::new("report-file");
    let out = tmp.path("out.tiff");
    let report = tmp.path("report.json");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "simple",
        "--film-base",
        "0.9,0.55,0.42",
        "--report-file",
        report.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "--report-file must keep stdout empty"
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(written["command"], "convert");
}

// --- write-target collision guards (PR review: never clobber data, exit 0) ----

#[test]
fn convert_rejects_in_place_output() {
    let fix = fixture("hdr-48bit.tif");
    let before = std::fs::read(&fix).unwrap();
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        fix.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "in-place output must be a usage error: {err}");
    assert!(err.contains("overwrite the input"), "stderr: {err}");
    assert_eq!(
        std::fs::read(&fix).unwrap(),
        before,
        "input scan must be untouched"
    );
}

#[test]
fn convert_rejects_report_file_colliding_with_artifacts() {
    let dir = TempDir::new("collide");
    let out = dir.path("out.tiff");
    let fix = fixture("hdr-48bit.tif");
    // --report-file == the output TIFF.
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--report-file",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "report over output must be a usage error: {err}");
    // --report-file == the automatic sidecar.
    let sidecar = dir.path("out.tiff.json");
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--report-file",
        sidecar.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "report over sidecar must be a usage error: {err}");
    // --report-file reaching the output through a `..` traversal (the target
    // doesn't exist yet, so canonicalizing the full path alone can't catch it).
    std::fs::create_dir_all(dir.path("sub")).unwrap();
    let dotted = dir.path("sub/../out.tiff");
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--report-file",
        dotted.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "dotted report over output must be rejected: {err}");
    assert!(
        !out.exists(),
        "no artifact may be written on a rejected run"
    );
}

#[test]
fn inspect_rejects_report_file_over_input() {
    let fix = fixture("hdri-64bit.tif");
    let before = std::fs::read(&fix).unwrap();
    let (code, _, err) = run(&[
        "inspect",
        fix.to_str().unwrap(),
        "--report-file",
        fix.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "report over input must be a usage error: {err}");
    assert_eq!(
        std::fs::read(&fix).unwrap(),
        before,
        "input scan must be untouched"
    );
}

#[test]
fn convert_rejects_unapplied_input_profile() {
    // `--input-profile` is reserved for the deferred scanner-profile-before-density
    // experiment — it must fail loudly (exit 4), not silently ignore the profile.
    let dir = TempDir::new("inprofile");
    let out = dir.path("out.tiff");
    let fix = fixture("hdr-48bit.tif");
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--input-profile",
        "scanner.icc",
    ]);
    assert_eq!(code, 4, "unapplied input profile must exit 4: {err}");
    assert!(err.contains("not supported"), "stderr: {err}");
    assert!(!out.exists());
}

#[test]
fn convert_reports_resolved_input_color_for_real_scan() {
    // A real SilverFast HDR scan resolves independently to a linear transfer and
    // scanner-device meaning, then reaches the render — reported with evidence.
    let tmp = TempDir::new("inputcolor");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "convert should succeed: {err}");
    let ic = &json(&stdout)["input_color"];
    assert_eq!(ic["transfer"], "linear");
    assert_eq!(ic["meaning"], "scanner-device");
    assert_eq!(ic["transfer_decoded"], false);
    assert_eq!(ic["icc_embedded"], false);
    // Both axes carry structural evidence.
    let ev = ic["evidence"].as_array().unwrap();
    assert!(
        ev.iter()
            .any(|e| e["axis"] == "transfer" && e["kind"] == "structural")
    );
    assert!(
        ev.iter()
            .any(|e| e["axis"] == "meaning" && e["kind"] == "structural")
    );
}

#[test]
fn inspect_reports_input_color_evidence() {
    let (code, stdout, err) = run(&["inspect", fixture("hdri-64bit.tif").to_str().unwrap()]);
    assert_eq!(code, 0, "inspect should succeed: {err}");
    let ic = &json(&stdout)["input_color"];
    assert_eq!(ic["transfer"], "linear");
    assert_eq!(ic["meaning"], "scanner-device");
    assert!(ic["evidence"].as_array().is_some_and(|e| !e.is_empty()));
}

#[test]
fn convert_rejects_colorimetric_assertion_on_scanner_scan() {
    // An explicit meaning that contradicts the raw-mode scanner structure fails
    // loudly (usage error, exit 2) — it never overrides container structure.
    let tmp = TempDir::new("colorimetric");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--input-meaning",
        "colorimetric",
    ]);
    assert_eq!(
        code, 2,
        "colorimetric-vs-structure must be a usage error: {err}"
    );
    assert!(err.contains("contradicts"), "stderr: {err}");
    assert!(!out.exists());
}

#[test]
fn convert_rejects_legacy_input_color_recipe_key() {
    // A recipe carrying the removed combined `input.color` key fails to load with
    // a pinned migration message — it never silently asserts both axes.
    let tmp = TempDir::new("legacycolor");
    let out = tmp.path("out.tiff");
    let recipe = tmp.path("recipe.json");
    std::fs::write(&recipe, r#"{"input":{"color":"linear"}}"#).unwrap();
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "legacy input.color must be a usage error: {err}");
    assert!(err.contains("input.transfer"), "stderr: {err}");
    assert!(!out.exists());
}

#[test]
fn generic_rgb16_without_silverfast_provenance_is_rejected() {
    // A plain RGB16 TIFF with no SilverFast Software tag and no IR plane carries
    // no raw-mode provenance — meaning resolves Unknown, so `convert` rejects it
    // (exit 4, not a silently-wrong negative) and `inspect` reports the ambiguity.
    let tmp = TempDir::new("generic");
    let src = tmp.path("generic.tif");
    write_uniform_rgb48(&src, [30000, 20000, 15000], 8, 8);
    let out = tmp.path("out.tiff");

    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 4, "generic RGB16 must be Unsupported (exit 4): {err}");
    assert!(
        err.contains("--input-transfer linear --input-meaning scanner-device"),
        "error must suggest the explicit-assertion escape hatch: {err}"
    );
    assert!(!out.exists());

    // inspect stays diagnostic — reports meaning unknown with evidence, no failure.
    let (code, stdout, _err) = run(&["inspect", src.to_str().unwrap()]);
    assert_eq!(code, 0, "inspect never fails on ambiguity");
    let ic = &json(&stdout)["input_color"];
    assert_eq!(ic["meaning"], "unknown");
    assert!(ic["evidence"].as_array().is_some_and(|e| !e.is_empty()));
}

#[test]
fn explicit_assertion_escape_hatch_converts_generic_rgb16() {
    // The user can take responsibility for a raw scan lacking provenance by
    // asserting both axes explicitly — that reaches the render (exit 0), and the
    // report records the assertions' provenance.
    let tmp = TempDir::new("escape");
    let src = tmp.path("generic.tif");
    write_uniform_rgb48(&src, [30000, 20000, 15000], 8, 8);
    let out = tmp.path("out.tiff");

    let (code, stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
    ]);
    assert_eq!(
        code, 0,
        "explicit assertions must convert a generic RGB16: {err}"
    );
    assert!(is_tiff(&out));
    let ic = &json(&stdout)["input_color"];
    assert_eq!(ic["transfer"], "linear");
    assert_eq!(ic["meaning"], "scanner-device");
    // Both axes carry a user-assertion evidence record with CLI provenance.
    let ev = ic["evidence"].as_array().unwrap();
    assert!(ev.iter().any(|e| {
        e["kind"] == "user-assertion"
            && e["provenance"]
                .as_str()
                .is_some_and(|p| p.contains("CLI flag"))
    }));
}

#[test]
fn input_assertion_provenance_distinguishes_cli_from_recipe() {
    // M2: the CLI-vs-recipe provenance is observable end-to-end. A recipe-sourced
    // assertion reports `input.… (recipe)`; a CLI-flag assertion reports
    // `--input-… (CLI flag)`.
    let tmp = TempDir::new("prov");
    let src = tmp.path("generic.tif");
    write_uniform_rgb48(&src, [30000, 20000, 15000], 8, 8);
    let recipe = tmp.path("recipe.json");
    std::fs::write(
        &recipe,
        r#"{"input":{"transfer":"linear","meaning":"scanner-device"},
            "film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}"#,
    )
    .unwrap();

    // Recipe-only: both assertions attributed to the recipe.
    let out1 = tmp.path("out1.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out1.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "recipe assertions convert: {err}");
    let ev = json(&stdout)["input_color"]["evidence"].clone();
    let ev = ev.as_array().unwrap();
    assert!(ev.iter().any(|e| {
        e["kind"] == "user-assertion"
            && e["provenance"]
                .as_str()
                .is_some_and(|p| p.contains("(recipe)"))
    }));

    // CLI flag over the recipe: the transfer assertion now reports CLI provenance.
    let out2 = tmp.path("out2.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--input-transfer",
        "linear",
    ]);
    assert_eq!(code, 0, "cli override converts: {err}");
    let ev = json(&stdout)["input_color"]["evidence"].clone();
    let ev = ev.as_array().unwrap();
    assert!(ev.iter().any(|e| {
        e["axis"] == "transfer"
            && e["kind"] == "user-assertion"
            && e["provenance"]
                .as_str()
                .is_some_and(|p| p.contains("CLI flag"))
    }));
}

#[test]
fn assume_linear_flag_is_a_migration_error_through_the_binary() {
    // M3: the deprecated combined flag must fail loudly (exit 2) with migration
    // guidance — it must never silently assert both axes.
    let tmp = TempDir::new("assumelinear");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--assume-linear",
    ]);
    assert_eq!(code, 2, "--assume-linear must be a usage error: {err}");
    assert!(err.contains("--input-transfer"), "stderr: {err}");
    assert!(!out.exists());
}

#[test]
fn ir_plane_bit_identical_across_input_resolution() {
    // H1: IR is measurement data, never color-transformed — so the exported IR
    // plane must be byte-identical regardless of how the input color resolves
    // (auto vs an explicit scanner-device assertion take different resolver paths).
    let tmp = TempDir::new("ir-identity");
    let src = fixture("hdri-64bit.tif");
    let src = src.to_str().unwrap();

    let out_auto = tmp.path("out-auto.tiff");
    let ir_auto = tmp.path("ir-auto.tiff");
    let (code, _o, err) = run(&[
        "convert",
        src,
        "-o",
        out_auto.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--reconstruction",
        "simple",
        "--export-ir",
        ir_auto.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "auto convert: {err}");

    let out_expl = tmp.path("out-expl.tiff");
    let ir_expl = tmp.path("ir-expl.tiff");
    let (code, _o, err) = run(&[
        "convert",
        src,
        "-o",
        out_expl.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--reconstruction",
        "simple",
        "--export-ir",
        ir_expl.to_str().unwrap(),
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
    ]);
    assert_eq!(code, 0, "explicit-assertion convert: {err}");

    let a = std::fs::read(&ir_auto).unwrap();
    let b = std::fs::read(&ir_expl).unwrap();
    assert_eq!(
        a, b,
        "exported IR must be byte-identical across input resolution"
    );
}

#[test]
fn roll_frame_report_includes_resolved_input_color() {
    // P2: a roll frame report must carry the resolved input semantics (mirrors
    // single-frame `convert`), not drop them.
    let tmp = TempDir::new("roll-ic");
    let out_dir = tmp.path("out");
    let recipe = tmp.path("recipe.json");
    std::fs::write(
        &recipe,
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}"#,
    )
    .unwrap();
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "roll should succeed: {err}");
    let frame = &json(&stdout)["frames"][0];
    assert_eq!(frame["input_color"]["transfer"], "linear");
    assert_eq!(frame["input_color"]["meaning"], "scanner-device");
}

#[test]
fn roll_rejects_colorimetric_shared_recipe_before_decode() {
    // M1: an unconditionally-unsupported shared assertion fails fast, before the
    // first (large) scan is decoded — exit 4 with an actionable message.
    let tmp = TempDir::new("roll-colorimetric");
    let out_dir = tmp.path("out");
    let recipe = tmp.path("recipe.json");
    std::fs::write(&recipe, r#"{"input":{"meaning":"colorimetric"}}"#).unwrap();
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 4,
        "colorimetric shared recipe must be Unsupported: {err}"
    );
    assert!(err.contains("colorimetric"), "stderr: {err}");
    // Fail-fast: no output directory contents were produced.
    assert!(
        !out_dir.join("hdr-48bit_positive.tiff").exists(),
        "no frame should be written on the pre-flight reject"
    );
}

// --- XMP-based SilverFast provenance gate (adversarial-review hardening) ------

/// Attribute list for a genuine raw negative scan.
const XMP_NEG: &str = r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="Yes""#;

#[test]
fn rgb16_plus_gray16_without_xmp_is_rejected() {
    // Adversarial hole #1: a generic RGB16 + matching Gray16 multipage (an IR-like
    // second page) must NOT be treated as a raw scanner scan without XMP.
    let tmp = TempDir::new("ir-forge");
    let src = tmp.path("forged.tif");
    write_rgb16(&src, None, None, true); // IR page, no XMP
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 4, "RGB16+Gray16 without XMP must be rejected: {err}");
    assert!(!out.exists());
}

#[test]
fn software_silverfast_string_without_xmp_is_rejected() {
    // Adversarial hole #2: a `Software="SilverFast …"` string (which a processed
    // export keeps) is NOT sufficient provenance without the XMP mode metadata.
    let tmp = TempDir::new("sw-forge");
    let src = tmp.path("sw.tif");
    write_rgb16(&src, None, Some("SilverFast 9.2.8 (Jun 11 2026)"), false);
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 4,
        "Software string without XMP must be rejected: {err}"
    );
    assert!(!out.exists());
}

#[test]
fn silverfast_xmp_negative_converts() {
    // A genuine raw negative (XMP Company+HDRScan=Yes+Gamma=1+Negative=Yes) reaches
    // the render and reports scanner-device / linear.
    let tmp = TempDir::new("xmp-neg");
    let src = tmp.path("neg.tif");
    write_rgb16(&src, Some(&silverfast_xmp(XMP_NEG)), None, false);
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "synthetic SilverFast negative must convert: {err}");
    assert!(is_tiff(&out));
    let ic = &json(&stdout)["input_color"];
    assert_eq!(ic["transfer"], "linear");
    assert_eq!(ic["meaning"], "scanner-device");
}

#[test]
fn silverfast_xmp_nonlinear_gamma_is_rejected() {
    // Contradiction path is LIVE: a raw-mode scan (HDRScan=Yes) whose XMP Gamma is
    // non-linear (a processed export) → ambiguous transfer → convert exits 4;
    // inspect stays diagnostic and reports transfer unknown.
    let tmp = TempDir::new("xmp-gamma");
    let src = tmp.path("g.tif");
    let attrs = r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="2.2" Silverfast:Negative="Yes""#;
    write_rgb16(&src, Some(&silverfast_xmp(attrs)), None, false);
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 4,
        "non-linear gamma on raw mode must be rejected: {err}"
    );
    assert!(!out.exists());

    let (code, stdout, _err) = run(&["inspect", src.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(json(&stdout)["input_color"]["transfer"], "unknown");
}

#[test]
fn silverfast_positive_mode_is_rejected() {
    // A positive-mode scan (XMP Negative=No) passes the transfer/meaning gate but
    // must be rejected loudly with the distinct positive-mode message rather than
    // silently converted as a negative.
    let tmp = TempDir::new("xmp-pos");
    let src = tmp.path("pos.tif");
    let attrs = r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="No""#;
    write_rgb16(&src, Some(&silverfast_xmp(attrs)), None, false);
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 4, "positive-mode scan must be rejected: {err}");
    assert!(err.contains("positive-mode"), "stderr: {err}");
    assert!(!out.exists());
}

#[test]
fn silverfast_malformed_gamma_is_ambiguous_and_rejected() {
    // F1 end-to-end: a raw-mode scan whose XMP Gamma is locale-formatted ("2,2")
    // must NOT silently resolve to linear — decode warns, transfer resolves
    // Unknown, and convert exits 4 (rather than converting a possibly-non-linear
    // scan as linear).
    let tmp = TempDir::new("xmp-badgamma");
    let src = tmp.path("g.tif");
    let attrs = r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="2,2" Silverfast:Negative="Yes""#;
    write_rgb16(&src, Some(&silverfast_xmp(attrs)), None, false);
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 4,
        "malformed gamma must be rejected, not silently linear: {err}"
    );
    assert!(!out.exists());

    // inspect stays diagnostic: transfer unknown + a breadcrumb naming the value.
    let (code, stdout, _err) = run(&["inspect", src.to_str().unwrap()]);
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(report["input_color"]["transfer"], "unknown");
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("2,2")),
        "inspect report must carry the malformed-gamma breadcrumb: {stdout}"
    );
}

#[test]
fn silverfast_unrecognized_negative_value_still_converts_a_negative() {
    // F3 end-to-end: a genuine negative whose `Negative` reads as an unrecognized
    // token (not "yes"/"no") must NOT be misread as positive-mode and rejected —
    // an unrecognized value is `None`, not an explicit "No", so it still converts.
    let tmp = TempDir::new("xmp-weirdneg");
    let src = tmp.path("n.tif");
    let attrs = r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="y""#;
    write_rgb16(&src, Some(&silverfast_xmp(attrs)), None, false);
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 0,
        "an unrecognized Negative value must not trigger positive-mode rejection: {err}"
    );
    assert!(is_tiff(&out));
}

// --- telemetry (opt-in performance + context record) -------------------------

#[test]
fn telemetry_file_writes_full_record() {
    // `--telemetry-file <path>` writes one valid JSON record with every schema
    // field populated (schema_version=3, finite timings, correct dims/bytes).
    let tmp = TempDir::new("tel-file");
    let out = tmp.path("out.tiff");
    let rec = tmp.path("run.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        rec.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "convert with --telemetry-file should succeed:\n{err}"
    );

    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();

    assert_eq!(record["schema_version"], 3);
    assert!(record["timestamp_ms"].as_u64().unwrap() > 0);
    assert!(record["nc_version"].is_string());
    assert!(record["target"].is_string());
    assert!(record["cpu_count"].is_number() || record["cpu_count"].is_null());

    // Image facts match the known HDR fixture (502x462, 3ch, 16-bit, no IR).
    let image = &record["image"];
    assert_eq!(image["format"], "hdr");
    assert_eq!(image["width"], 502);
    assert_eq!(image["height"], 462);
    assert_eq!(image["channels"], 3);
    assert_eq!(image["bit_depth"], 16);
    assert_eq!(image["ir_present"], false);
    let mp = image["megapixels"].as_f64().unwrap();
    assert!(
        (mp - (502.0 * 462.0 / 1_000_000.0)).abs() < 1e-9,
        "megapixels: {mp}"
    );
    assert!(image["input_bytes"].as_u64().unwrap() > 0);
    assert!(image["output_bytes"].as_u64().unwrap() > 0);

    // Per-stage timings are all present and finite.
    let timing = &record["timing_ms"];
    for key in [
        "total",
        "decode",
        "film_base",
        "algorithm",
        "color",
        "encode",
    ] {
        assert!(
            timing[key].as_f64().is_some_and(f64::is_finite),
            "timing_ms.{key} must be finite: {timing}"
        );
    }
    // No IR plane in this fixture → no ir_export timing.
    assert!(timing.get("ir_export").is_none() || timing["ir_export"].is_null());

    let conv = &record["conversion"];
    assert_eq!(conv["preset"], "legacy");
    assert_eq!(conv["reconstruction"], "density");
    assert_eq!(conv["curve"], "exponential");
    assert!(conv["params_hash"].as_str().unwrap().len() == 16);
    assert_eq!(
        conv["film_base_source"]["explicit"],
        serde_json::json!([0.9, 0.55, 0.42])
    );
    assert_eq!(conv["output_hdr"], false);

    let outcome = &record["outcome"];
    // No `success` field today — a record is emitted only on success, so a
    // constant flag would carry no information (see OutcomeInfo).
    assert!(
        outcome.get("success").is_none(),
        "no success field: {outcome}"
    );
    assert!(outcome["warnings"].is_number());
    assert!(outcome["clipped"].is_number());
    assert!(outcome["non_finite"].is_number());
}

#[test]
fn strict_failure_writes_no_telemetry_record() {
    // A telemetry record's existence is the success signal (there is no
    // `outcome.success` field). A `--strict` run that exits non-zero on a warning
    // must therefore leave NO record — otherwise the log would count a failed run
    // as a successful one. Force a clipping warning with a large `--print-exposure`
    // (as in `u16_clipping_is_reported_and_strict_promotes_it`), add `--strict`,
    // and assert exit 1 with no telemetry file created.
    let tmp = TempDir::new("tel-strict");
    let out = tmp.path("out.tiff");
    let rec = tmp.path("run.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--print-exposure",
        "12",
        "--strict",
        "--telemetry-file",
        rec.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "--strict clipping run must exit 1: {err}");
    assert!(
        !rec.exists(),
        "no telemetry record may be written for a --strict failure"
    );
}

#[test]
fn telemetry_file_records_ir_export_timing() {
    // An HDRi conversion with --export-ir carries the ir_export stage timing.
    let tmp = TempDir::new("tel-ir");
    let out = tmp.path("out.tiff");
    let ir = tmp.path("ir.tiff");
    let rec = tmp.path("run.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--export-ir",
        ir.to_str().unwrap(),
        "--telemetry-file",
        rec.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "HDRi export-ir + telemetry should succeed:\n{err}");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
    assert_eq!(record["image"]["ir_present"], true);
    assert!(
        record["timing_ms"]["ir_export"]
            .as_f64()
            .is_some_and(f64::is_finite),
        "ir_export timing must be present when --export-ir ran: {record}"
    );
}

#[test]
fn telemetry_log_appends_one_line_per_run() {
    // `--telemetry` appends exactly one JSONL line per run to NC_TELEMETRY_LOG.
    let tmp = TempDir::new("tel-log");
    let log = tmp.path("telemetry.jsonl");
    let convert = |out: &Path| {
        run_env(
            &[
                "convert",
                fixture("hdr-48bit.tif").to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--film-base",
                "0.9,0.55,0.42",
                "--telemetry",
                "--report",
                "none",
            ],
            &[("NC_TELEMETRY_LOG", log.to_str().unwrap())],
        )
    };
    let out1 = tmp.path("a.tiff");
    let out2 = tmp.path("b.tiff");
    let (c1, _, e1) = convert(&out1);
    let (c2, _, e2) = convert(&out2);
    assert_eq!(
        (c1, c2),
        (0, 0),
        "telemetry runs should succeed:\n{e1}\n{e2}"
    );

    let contents = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "two runs must append two lines: {contents}");
    // Each line is an independent, valid JSON object.
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["schema_version"], 3);
    }
}

#[test]
fn telemetry_both_sinks_receive_the_record() {
    // `--telemetry` + `--telemetry-file` together write to both the JSONL log and
    // the one-off file ("Both").
    let tmp = TempDir::new("tel-both");
    let out = tmp.path("out.tiff");
    let log = tmp.path("telemetry.jsonl");
    let rec = tmp.path("run.json");
    let (code, _stdout, err) = run_env(
        &[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry",
            "--telemetry-file",
            rec.to_str().unwrap(),
            "--report",
            "none",
        ],
        &[("NC_TELEMETRY_LOG", log.to_str().unwrap())],
    );
    assert_eq!(code, 0, "both-sink telemetry should succeed:\n{err}");
    assert!(log.exists(), "JSONL log must be written");
    assert!(rec.exists(), "one-off file must be written");
    let log_line = std::fs::read_to_string(&log).unwrap();
    let file_line = std::fs::read_to_string(&rec).unwrap();
    // Same record content in both sinks (the one-off adds a trailing newline).
    assert_eq!(log_line.trim(), file_line.trim());
}

#[test]
fn telemetry_does_not_perturb_output_or_sidecar() {
    // THE determinism invariant: telemetry on vs off must produce byte-identical
    // output TIFF AND sidecar JSON — telemetry never touches the deterministic
    // path. Point NC_TELEMETRY_LOG at a temp file for the on-run so the default
    // log is never touched.
    let tmp = TempDir::new("tel-invariant");
    let log = tmp.path("telemetry.jsonl");
    let base = |out: &Path| {
        vec![
            "convert".to_string(),
            fixture("hdri-64bit.tif").to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--reconstruction".to_string(),
            "density".to_string(),
            "--film-base".to_string(),
            "0.9,0.55,0.42".to_string(),
            "--report".to_string(),
            "none".to_string(),
        ]
    };

    // Telemetry OFF.
    let off = tmp.path("off.tiff");
    let (c_off, _, _) = run(&base(&off).iter().map(String::as_str).collect::<Vec<_>>());

    // Telemetry ON (both sinks).
    let on = tmp.path("on.tiff");
    let rec = tmp.path("on-run.json");
    let mut on_args = base(&on);
    on_args.extend(["--telemetry", "--telemetry-file", rec.to_str().unwrap()].map(String::from));
    let (c_on, _, _) = run_env(
        &on_args.iter().map(String::as_str).collect::<Vec<_>>(),
        &[("NC_TELEMETRY_LOG", log.to_str().unwrap())],
    );

    assert_eq!((c_off, c_on), (0, 0));
    assert_eq!(
        std::fs::read(&off).unwrap(),
        std::fs::read(&on).unwrap(),
        "output TIFF must be byte-identical with telemetry on vs off"
    );
    assert_eq!(
        std::fs::read(format!("{}.json", off.display())).unwrap(),
        std::fs::read(format!("{}.json", on.display())).unwrap(),
        "sidecar must be byte-identical with telemetry on vs off"
    );
    // The telemetry record itself was produced (sanity: the feature actually ran).
    assert!(rec.exists() && log.exists());
}

#[test]
fn telemetry_write_failure_is_fail_soft_even_under_strict() {
    // A telemetry write failure must NOT fail a successful conversion, and
    // --strict must not promote it (the image already succeeded). Force a write
    // failure by pointing --telemetry-file under a path whose parent is a regular
    // file (so create_dir_all fails). Use --output-hdr so the conversion itself
    // raises no warnings (f32 never clips; the HDR fixture has no IR plane), which
    // isolates the telemetry failure from any legitimate --strict trigger.
    let tmp = TempDir::new("tel-failsoft");
    let out = tmp.path("out.tiff");
    let blocker = tmp.path("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let bad = tmp.path("blocker/rec.json"); // parent is a file → write fails

    let (code, _stdout, stderr) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "density",
        "--output-hdr",
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        bad.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(
        code, 0,
        "a telemetry write failure must not fail the run, even with --strict:\n{stderr}"
    );
    assert!(is_tiff(&out), "the output TIFF must still be written");
    assert!(
        stderr.to_lowercase().contains("telemetry"),
        "the telemetry failure must be warned on stderr: {stderr}"
    );
}

#[test]
fn telemetry_file_colliding_with_output_is_usage_error() {
    // A --telemetry-file that would clobber the output (a config error, distinct
    // from a runtime write failure) fails loudly up front, before decoding.
    let tmp = TempDir::new("tel-collide");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        out.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 2,
        "telemetry-file over the output must be a usage error: {err}"
    );
    assert!(
        !out.exists(),
        "no artifact may be written on a rejected run"
    );
}

#[test]
fn telemetry_file_colliding_with_sidecar_is_usage_error() {
    // The sidecar (`out.tiff.json`) is the likeliest footgun for --telemetry-file;
    // it must be caught by the same collision guard as the output.
    let tmp = TempDir::new("tel-collide-sidecar");
    let out = tmp.path("out.tiff");
    let sidecar = tmp.path("out.tiff.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        sidecar.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 2,
        "telemetry-file over the sidecar must be a usage error: {err}"
    );
    assert!(
        !out.exists(),
        "no artifact may be written on a rejected run"
    );
}

#[test]
fn telemetry_log_colliding_with_output_is_usage_error() {
    // The persistent `--telemetry` log (here via NC_TELEMETRY_LOG) is guarded the
    // same way as --telemetry-file: a path that would append into the output is a
    // loud usage error up front, not a silent post-write corruption.
    let tmp = TempDir::new("tel-log-collide");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run_env(
        &[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry",
        ],
        &[("NC_TELEMETRY_LOG", out.to_str().unwrap())],
    );
    assert_eq!(
        code, 2,
        "telemetry log over the output must be a usage error: {err}"
    );
    assert!(
        !out.exists(),
        "no artifact may be written on a rejected run"
    );
}

#[test]
fn telemetry_file_dash_writes_json_to_stdout() {
    // `-` = stdout. Paired with --report none so stdout is exactly the one
    // telemetry line (a single parseable JSON object), and it must NOT be rejected
    // as a collision.
    let tmp = TempDir::new("tel-stdout");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        "-",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "telemetry to stdout should succeed:\n{err}");
    let record = json(&stdout);
    assert_eq!(record["schema_version"], 3);
    assert_eq!(record["image"]["format"], "hdr");
}

#[test]
fn telemetry_params_hash_matches_identical_conversions() {
    // The load-bearing dedup contract: identical params ⇒ identical params_hash
    // (and identical sidecar bytes); a changed knob ⇒ a different hash.
    let tmp = TempDir::new("tel-hash");
    let fix = fixture("hdr-48bit.tif");
    let convert = |out: &Path, extra: &[&str]| -> serde_json::Value {
        let out = out.to_str().unwrap();
        let mut argv = vec![
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out,
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry-file",
            "-",
            "--report",
            "none",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{err}");
        json(&stdout)
    };
    let a = tmp.path("a.tiff");
    let b = tmp.path("b.tiff");
    let c = tmp.path("c.tiff");
    let ra = convert(&a, &[]);
    let rb = convert(&b, &[]);
    let rc = convert(&c, &["--density-gamma", "1.8"]);

    let ha = ra["conversion"]["params_hash"].as_str().unwrap();
    let hb = rb["conversion"]["params_hash"].as_str().unwrap();
    let hc = rc["conversion"]["params_hash"].as_str().unwrap();
    assert_eq!(ha, hb, "identical params must share a hash");
    assert_ne!(ha, hc, "a changed knob must change the hash");
    // The hash tracks the sidecar bytes, so equal hashes ⇒ equal sidecars.
    assert_eq!(
        std::fs::read(format!("{}.json", a.display())).unwrap(),
        std::fs::read(format!("{}.json", b.display())).unwrap(),
    );
}

#[test]
fn telemetry_log_write_failure_is_fail_soft() {
    // The JSONL-log sink is fail-soft too: point NC_TELEMETRY_LOG under a path
    // whose parent is a regular file (create_dir_all fails), and the conversion
    // must still exit 0 with a stderr warning.
    let tmp = TempDir::new("tel-log-failsoft");
    let out = tmp.path("out.tiff");
    let blocker = tmp.path("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let bad_log = tmp.path("blocker/telemetry.jsonl"); // parent is a file

    let (code, _stdout, stderr) = run_env(
        &[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry",
            "--report",
            "none",
        ],
        &[("NC_TELEMETRY_LOG", bad_log.to_str().unwrap())],
    );
    assert_eq!(
        code, 0,
        "a JSONL-log write failure must not fail the run:\n{stderr}"
    );
    assert!(is_tiff(&out), "the output TIFF must still be written");
    assert!(
        stderr.to_lowercase().contains("telemetry"),
        "the log write failure must be warned on stderr: {stderr}"
    );
}

#[test]
fn telemetry_outcome_reports_clipping_and_warnings() {
    // End-to-end pinning of the orchestrator → record `outcome` wiring
    // (`report.warnings.len()` and `EncodeReport::clipped_total`), which the
    // shape-only tests never exercise. A +12-stop `--print-exposure` guarantees
    // u16 clipping (and thus a clipping warning), so both counters must be > 0.
    let tmp = TempDir::new("tel-outcome-clip");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "density",
        "--film-base",
        "0.9,0.55,0.42",
        "--print-exposure",
        "12",
        "--telemetry-file",
        "-",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "clipping run should still succeed:\n{err}");
    let record = json(&stdout);
    let outcome = &record["outcome"];
    assert!(
        outcome["clipped"].as_u64().unwrap() > 0,
        "a +12-stop exposure must report clipped samples: {outcome}"
    );
    assert!(
        outcome["warnings"].as_u64().unwrap() >= 1,
        "the clipping warning must be counted in outcome.warnings: {outcome}"
    );
}

#[test]
fn telemetry_outcome_counts_ir_ignored_warning() {
    // A separate warning source than clipping: converting an HDRi scan *without*
    // --export-ir raises the "IR plane preserved but not used" warning, which must
    // flow into outcome.warnings — proving the count isn't clipping-specific.
    let tmp = TempDir::new("tel-outcome-ir");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--reconstruction",
        "density",
        "--output-hdr", // f32 never clips, so the IR-ignored warning is isolated
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        "-",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "HDRi convert should succeed:\n{err}");
    let record = json(&stdout);
    let outcome = &record["outcome"];
    assert_eq!(outcome["clipped"].as_u64().unwrap(), 0, "f32 must not clip");
    assert!(
        outcome["warnings"].as_u64().unwrap() >= 1,
        "the IR-ignored warning must be counted in outcome.warnings: {outcome}"
    );
}

#[test]
fn telemetry_key_in_recipe_is_rejected() {
    // Telemetry flags are *operational*, not recipe keys: a recipe (`--params`)
    // carrying a `telemetry` key must be rejected by `deny_unknown_fields` (exit 2,
    // usage), never silently accepted as if telemetry were a conversion knob.
    let tmp = TempDir::new("tel-recipe-key");
    let recipe = tmp.path("recipe.json");
    std::fs::write(
        &recipe,
        r#"{"reconstruction":{"type":"density"},"telemetry":true}"#,
    )
    .unwrap();
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 2,
        "a telemetry key in a recipe must be a usage error (exit 2): {err}"
    );
    assert!(
        !out.exists(),
        "no artifact may be written on a rejected recipe"
    );
}

#[test]
fn telemetry_records_sigmoid_curve_and_params_hash() {
    // The record's conversion summary must handle the sigmoid curve: the
    // reconstruction/curve pair serializes "density"/"sigmoid", and params_hash
    // (over the effective recipe JSON) must cover the curve keys, so tweaking
    // one changes the hash.
    let tmp = TempDir::new("tel-sigmoid");
    let fix = fixture("hdr-48bit.tif");
    let convert = |out: &Path, extra: &[&str]| -> serde_json::Value {
        let out = out.to_str().unwrap();
        let mut argv = vec![
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out,
            "--density-curve",
            "sigmoid",
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry-file",
            "-",
            "--report",
            "none",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "sigmoid + telemetry should succeed:\n{err}");
        json(&stdout)
    };
    let a = tmp.path("a.tiff");
    let b = tmp.path("b.tiff");
    let ra = convert(&a, &[]);
    let rb = convert(&b, &["--sigmoid-contrast", "1.5"]);

    assert_eq!(
        ra["conversion"]["reconstruction"], "density",
        "the record names the reconstruction type: {ra}"
    );
    assert_eq!(
        ra["conversion"]["curve"], "sigmoid",
        "the record must name the sigmoid curve: {ra}"
    );
    // sigmoid shares the density anchor, so a resolved dmax still rides along.
    assert!(
        ra["conversion"]["dmax"]
            .as_f64()
            .is_some_and(f64::is_finite),
        "sigmoid should report a resolved dmax anchor: {ra}"
    );
    assert_ne!(
        ra["conversion"]["params_hash"], rb["conversion"]["params_hash"],
        "a changed sigmoid knob must change params_hash"
    );
}

#[test]
fn convert_sigmoid_runs_end_to_end_and_reports_the_anchor() {
    // `--density-curve sigmoid` selects the S-curve end to end: the JSON
    // report names the resolved curve, carries the resolved Dmax anchor, and the
    // sidecar recipe round-trips the tagged curve.
    let tmp = TempDir::new("sigmoid");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--density-curve",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--sigmoid-contrast",
        "1.2",
    ]);
    assert_eq!(code, 0, "sigmoid convert should succeed: {err}");
    assert!(is_tiff(&out));
    let report = json(&stdout);
    assert_eq!(report["reconstruction_result"]["type"], "density");
    assert_eq!(report["reconstruction_result"]["curve"]["type"], "sigmoid");
    assert_eq!(
        report["reconstruction_result"]["curve"]["dmax"]["policy"],
        "fixed"
    );
    assert_eq!(
        report["reconstruction_result"]["curve"]["dmax"]["provenance"],
        "default"
    );
    // Same pinned mapping identifier on the density/sigmoid path as on simple.
    assert_eq!(report["working_mapping"], "nc-film-rgb-v1");
    assert!(
        report["dmax"].as_f64().is_some_and(f64::is_finite),
        "the shared anchor must be reported: {report}"
    );
    let recipe = sidecar_params(&out);
    assert_eq!(recipe["reconstruction"]["curve"]["type"], "sigmoid");
    assert_eq!(recipe["reconstruction"]["curve"]["contrast"], 1.2);

    // The anchored shoulder keeps every rendered sample at or below display
    // white, so — unlike the straight line — the default u16 encode cannot
    // clip highlights.
    assert_eq!(
        report["loss"]["clipped_high"], 0,
        "the shoulder must prevent u16 highlight clipping: {report}"
    );
}

#[test]
fn sigmoid_small_anchor_does_not_clip_highlights() {
    // Regression for the toe-lift overshoot bug: a small explicit anchor
    // (`--d-max 0.1`) with the default toe (0.2) made the old shoulder-then-toe
    // order lift the white asymptote to ≈ 1.056, so the u16 encode clipped
    // highlights — defeating sigmoid's headline "shoulder means highlights can't
    // clip" guarantee. With the toe-then-shoulder reorder the ceiling is
    // inviolable: clipped_high must be 0.
    let tmp = TempDir::new("sigmoid-smallanchor");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--density-curve",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max",
        "0.1",
    ]);
    assert_eq!(
        code, 0,
        "sigmoid small-anchor convert should succeed: {err}"
    );
    let report = json(&stdout);
    assert_eq!(
        report["loss"]["clipped_high"], 0,
        "a small anchor must not overshoot display white / clip highlights: {report}"
    );
}

#[test]
fn sigmoid_rejects_density_gamma_as_a_usage_error() {
    // `gamma` exists only in the exponential curve variant, so `--density-gamma`
    // with a resolved sigmoid curve is an invalid tagged combination — a loud
    // post-merge usage error (exit 2), never the pre-reconstruction
    // warned-and-ignored behavior. Flag *presence* is the trigger (even the
    // default value 1.0 — the flag can only mean the exponential knob).
    let tmp = TempDir::new("gamma-reject");
    let fix = fixture("hdr-48bit.tif");
    let gamma_run = |extra: &[&str], out: &Path| -> (i32, String) {
        let mut argv = vec![
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
        ];
        argv.extend_from_slice(extra);
        let (code, _stdout, err) = run(&argv);
        (code, err)
    };

    // sigmoid + custom gamma → usage error naming the analogue knob, no output.
    let out = tmp.path("a.tiff");
    let (code, err) = gamma_run(
        &["--density-curve", "sigmoid", "--density-gamma", "1.5"],
        &out,
    );
    assert_eq!(code, 2, "sigmoid + --density-gamma must exit 2: {err}");
    assert!(
        err.contains("--sigmoid-contrast"),
        "the error names the sigmoid analogue: {err}"
    );
    assert!(!out.exists(), "no output on a usage error");

    // ...even at the default value (the flag is exponential-only).
    let (code, _err) = gamma_run(
        &["--density-curve", "sigmoid", "--density-gamma", "1.0"],
        &tmp.path("b.tiff"),
    );
    assert_eq!(code, 2, "flag presence is the trigger, not the value");

    // The exponential (default) curve consumes gamma normally.
    let (code, err) = gamma_run(&["--density-gamma", "1.5"], &tmp.path("c.tiff"));
    assert_eq!(code, 0, "exponential consumes gamma: {err}");
}

#[test]
fn sigmoid_rejects_no_d_max() {
    // The S-curve is anchored on [0, Dmax]; --no-d-max must be a usage error.
    let tmp = TempDir::new("sigmoid-nodmax");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--density-curve",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--no-d-max",
    ]);
    assert_eq!(code, 2, "sigmoid + --no-d-max must exit 2: {err}");
    assert!(!out.exists(), "no output on a usage error");
}

#[test]
fn density_report_carries_resolved_dmax() {
    // The auto-measured anchor must ride into the convert report (merge-time
    // wiring of Converter::convert_reported), and disappear with --no-d-max.
    let dir = TempDir::new("dmaxreport");
    let fix = fixture("hdr-48bit.tif");
    let out = dir.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert!(
        report["dmax"].as_f64().is_some_and(f64::is_finite),
        "auto anchor must be reported: {report}"
    );

    let out2 = dir.path("out2.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--no-d-max",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert!(
        report.get("dmax").is_none_or(|v| v.is_null()),
        "no anchor must be reported for --no-d-max: {report}"
    );
}

#[test]
fn auto_wb_reports_gains_that_reproduce_the_output_when_reused() {
    // The measure-once-reuse-for-the-roll contract, end to end: an `--auto-wb`
    // run reports the resolved gains, and a second run feeding them back through
    // the ordinary `--white-balance` flag must produce a byte-identical TIFF —
    // proving the auto gains are applied through the standard stage-4 slot, not
    // a post-hoc multiply. f32 output so the comparison covers full precision.
    let dir = TempDir::new("autowb");
    let fix = fixture("hdr-48bit.tif");
    let base_args = |out: &Path, wb: &[&str]| {
        let mut v = vec![
            "convert".to_string(),
            fix.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--film-base".to_string(),
            "0.9,0.55,0.42".to_string(),
            "--output-hdr".to_string(),
        ];
        v.extend(wb.iter().map(|s| s.to_string()));
        v
    };

    // Auto run: gains land in the report, green-anchored.
    let out_auto = dir.path("auto.tiff");
    let argv = base_args(&out_auto, &["--auto-wb", "percentile"]);
    let (code, stdout, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let gains = report["white_balance"]
        .as_array()
        .unwrap_or_else(|| panic!("resolved gains must be reported: {report}"));
    assert_eq!(gains.len(), 3);
    assert_eq!(gains[1].as_f64().unwrap(), 1.0, "green-anchored");
    // The sidecar recipe records the *auto mode* (the run's parameters), so
    // re-running the sidecar re-estimates; the report carries the frozen gains.
    assert_eq!(
        sidecar_params(&out_auto)["print"]["white_balance"],
        "percentile"
    );

    // Reuse run: the reported gains via the explicit flag ⇒ byte-identical TIFF.
    // (JSON prints the f32 gains as shortest-round-trip f64, which parses back
    // to the identical f32.)
    let wb_arg = gains
        .iter()
        .map(|g| g.as_f64().unwrap().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out_reuse = dir.path("reuse.tiff");
    let argv = base_args(&out_reuse, &["--white-balance", &wb_arg]);
    let (code, stdout, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        std::fs::read(&out_auto).unwrap(),
        std::fs::read(&out_reuse).unwrap(),
        "reusing the reported gains must reproduce the auto output byte-for-byte"
    );
    // The explicit run reports the same resolved gains.
    assert_eq!(json(&stdout)["white_balance"], report["white_balance"]);
}

#[test]
fn density_report_carries_resolved_balance_range() {
    // The roll-reuse workflow reads `balance_range` from the report and feeds it
    // back via --balance-range, so the measured [lo, hi] must ride into the
    // stdout JSON when a balance is requested — and stay absent for the neutral
    // default (guards the `run_convert` wiring, not just `ConvertReport`).
    let dir = TempDir::new("balreport");
    let fix = fixture("hdr-48bit.tif");
    let out = dir.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--shadow-balance",
        "-0.05,0,0.02",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let range = report["balance_range"]
        .as_array()
        .unwrap_or_else(|| panic!("measured range must be reported: {report}"));
    let (lo, hi) = (range[0].as_f64().unwrap(), range[1].as_f64().unwrap());
    assert!(lo.is_finite() && hi.is_finite() && lo < hi, "{report}");

    // Neutral balances → the field is omitted (no regional pass ran).
    let out2 = dir.path("out2.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert!(
        report.get("balance_range").is_none_or(|v| v.is_null()),
        "no range must be reported for neutral balances: {report}"
    );
}

#[test]
fn auto_measured_balance_range_reproduces_the_output_when_reused() {
    // THE measure-once-reuse workflow, end-to-end: measure a frame's tone range
    // under Auto, freeze it, and replay it on the next frame of the roll. This
    // closes the loop the report/recipe tests only cover in halves and crosses
    // the report-field ↔ recipe-key boundary CLAUDE.md flags as bug-prone —
    // `Report.balance_range` must ride out as JSON text and feed straight back
    // in via `--balance-range` with no precision drift.
    let dir = TempDir::new("balreuse");
    let fix = fixture("hdr-48bit.tif");

    // A real crossover cast (shadows warm, highlights cool), so the regional
    // pass runs and Auto has a non-degenerate range to measure.
    let balances = [
        "--shadow-balance",
        "-0.15,0,0.08",
        "--highlight-balance",
        "0.15,0,-0.08",
    ];

    // Frame 1: Auto measures the range and reports it.
    let auto_out = dir.path("auto.tiff");
    let mut auto_args = vec![
        "convert",
        fix.to_str().unwrap(),
        "-o",
        auto_out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--auto-balance-range",
    ];
    auto_args.extend_from_slice(&balances);
    let (code, stdout, err) = run(&auto_args);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let range = report["balance_range"]
        .as_array()
        .unwrap_or_else(|| panic!("measured range must be reported: {report}"));
    // Take the numbers' verbatim JSON text — exactly what an agent reading the
    // report would paste back — so no reformatting can mask (or introduce) drift.
    let lo_hi = format!("{},{}", range[0], range[1]);

    // Frame 2: freeze the reported range via Explicit `--balance-range`, same
    // balances. Byte-identical output proves the range survived the JSON text
    // round-trip and that `Report.balance_range` feeds back cleanly as input.
    let reuse_out = dir.path("reuse.tiff");
    let mut reuse_args = vec![
        "convert",
        fix.to_str().unwrap(),
        "-o",
        reuse_out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--balance-range",
        &lo_hi,
    ];
    reuse_args.extend_from_slice(&balances);
    let (code, _stdout, err) = run(&reuse_args);
    assert_eq!(code, 0, "{err}");

    assert_eq!(
        std::fs::read(&auto_out).unwrap(),
        std::fs::read(&reuse_out).unwrap(),
        "reusing the reported range via --balance-range must reproduce the \
         auto-measured output byte-for-byte"
    );
}

#[test]
fn auto_measured_balance_range_is_deterministic_in_the_report() {
    // The convert-determinism test proves the RGB output is stable, but the
    // reported anchors are the roll-reuse contract — an agent freezes them and
    // replays them, so the measured [lo, hi] must itself be exactly repeatable.
    let dir = TempDir::new("baldet");
    let fix = fixture("hdr-48bit.tif");
    let range = |tag: &str| {
        let out = dir.path(tag);
        let (code, stdout, err) = run(&[
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--auto-balance-range",
            "--shadow-balance",
            "-0.05,0,0.02",
        ]);
        assert_eq!(code, 0, "{err}");
        json(&stdout)["balance_range"].clone()
    };
    let (a, b) = (range("a.tiff"), range("b.tiff"));
    assert!(
        a.as_array().is_some_and(|r| r.len() == 2),
        "range must be reported: {a}"
    );
    assert_eq!(a, b, "auto-measured balance_range must be deterministic");
}

// ---------------------------------------------------------------------------
// roll (batch) — convert N frames from one shared, frozen recipe
// ---------------------------------------------------------------------------

/// Write `contents` to `path`, returning the path (for building recipes /
/// manifests in a test's temp dir).
fn write_file(path: &Path, contents: &str) -> PathBuf {
    std::fs::write(path, contents).unwrap();
    path.to_path_buf()
}

/// A hand-authored frozen roll recipe: explicit roll-fixed film base + Dmax, so
/// every frame converts deterministically without auto-base (real scans are
/// holder → rebate → picture, where auto-base fails loudly).
const ROLL_RECIPE: &str = r#"{
  "reconstruction": {
    "type": "density",
    "curve": { "type": "exponential", "dmax": { "explicit": 1.6 } }
  },
  "film_base": { "source": { "explicit": [0.9, 0.55, 0.42] } }
}"#;

#[test]
fn roll_converts_a_batch_from_a_shared_frozen_recipe() {
    let tmp = TempDir::new("roll-batch");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "roll should succeed:\n{stdout}\n{err}");

    // Per-frame outputs (named <stem>_positive.tiff) + their sidecars.
    let hdr_out = out_dir.join("hdr-48bit_positive.tiff");
    let hdri_out = out_dir.join("hdri-64bit_positive.tiff");
    assert!(is_tiff(&hdr_out), "first frame output must be a TIFF");
    assert!(is_tiff(&hdri_out), "second frame output must be a TIFF");
    assert!(out_dir.join("hdr-48bit_positive.tiff.json").exists());
    assert!(out_dir.join("hdri-64bit_positive.tiff.json").exists());

    let report = json(&stdout);
    assert_eq!(report["command"], "roll");
    // The shared frozen recipe (roll-fixed Dmin/Dmax) appears once, at the top.
    // f32 round-trips through JSON as f64, so compare the anchors approximately.
    let fb: Vec<f64> = report["recipe"]["film_base"]["source"]["explicit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect();
    assert!(
        (fb[0] - 0.9).abs() < 1e-6 && (fb[1] - 0.55).abs() < 1e-6 && (fb[2] - 0.42).abs() < 1e-6,
        "recipe film base: {fb:?}"
    );
    assert!(
        (report["recipe"]["reconstruction"]["curve"]["dmax"]["explicit"]
            .as_f64()
            .unwrap()
            - 1.6)
            .abs()
            < 1e-6
    );
    assert_eq!(report["summary"]["total"], 2);
    assert_eq!(report["summary"]["succeeded"], 2);
    assert_eq!(report["summary"]["failed"], 0);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for f in frames {
        assert_eq!(f["status"], "ok");
        assert!(f["film_base"].is_object(), "per-frame film base reported");
    }
}

#[test]
fn roll_is_byte_identical_on_rerun() {
    // Determinism: the same batch + same recipe ⇒ byte-identical output per frame.
    let tmp = TempDir::new("roll-determinism");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let run_into = |dir: &Path| {
        let (code, _out, err) = run(&[
            "roll",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
        std::fs::read(dir.join("hdr-48bit_positive.tiff")).unwrap()
    };
    let a = run_into(&tmp.path("out-a"));
    let b = run_into(&tmp.path("out-b"));
    assert_eq!(a, b, "re-running a roll must be byte-identical");
}

#[test]
fn roll_frame_local_override_applies_to_just_that_frame() {
    // A manifest gives frame 2 a per-frame print-exposure override; frame 1 runs
    // the shared recipe unchanged. Prove per-frame isolation by matching each
    // roll output byte-for-byte against the equivalent single `nc convert`.
    let tmp = TempDir::new("roll-override");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let hdri = fixture("hdri-64bit.tif");
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [
                 {{ "input": {hdr:?} }},
                 {{ "input": {hdri:?}, "params": {{ "print": {{ "print_exposure": 0.5 }} }} }}
               ] }}"#,
            hdr = hdr.to_str().unwrap(),
            hdri = hdri.to_str().unwrap(),
        ),
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "roll with manifest should succeed:\n{stdout}\n{err}"
    );

    // The override is recorded on frame 2 only.
    let report = json(&stdout);
    let frames = report["frames"].as_array().unwrap();
    assert!(frames[0].get("overrides").is_none() || frames[0]["overrides"].is_null());
    assert_eq!(frames[1]["overrides"]["print"]["print_exposure"], 0.5);

    // Frame 1 (no override) == single convert with just the shared recipe.
    let ref1 = tmp.path("ref1.tiff");
    let (c1, _o, e1) = run(&[
        "convert",
        hdr.to_str().unwrap(),
        "-o",
        ref1.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(c1, 0, "{e1}");
    assert_eq!(
        std::fs::read(out_dir.join("hdr-48bit_positive.tiff")).unwrap(),
        std::fs::read(&ref1).unwrap(),
        "un-overridden frame must match a plain convert"
    );

    // Frame 2 == single convert with the shared recipe + the same override.
    let ref2 = tmp.path("ref2.tiff");
    let (c2, _o, e2) = run(&[
        "convert",
        hdri.to_str().unwrap(),
        "-o",
        ref2.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--print-exposure",
        "0.5",
    ]);
    assert_eq!(c2, 0, "{e2}");
    assert_eq!(
        std::fs::read(out_dir.join("hdri-64bit_positive.tiff")).unwrap(),
        std::fs::read(&ref2).unwrap(),
        "overridden frame must match a convert carrying the same override"
    );
    // The override actually changed the pixels (frame 2 differs from its no-override form).
    let ref2_plain = tmp.path("ref2-plain.tiff");
    let (c3, _o, e3) = run(&[
        "convert",
        hdri.to_str().unwrap(),
        "-o",
        ref2_plain.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(c3, 0, "{e3}");
    assert_ne!(
        std::fs::read(&ref2).unwrap(),
        std::fs::read(&ref2_plain).unwrap(),
        "the print-exposure override must change the output"
    );
}

#[test]
fn roll_records_a_failed_frame_and_exits_nonzero() {
    // Batch resilience: a bad frame (missing input → decode error) is recorded in
    // the report and the roll continues, converting the good frame; the roll then
    // exits non-zero. stdout stays the JSON report even on the failing exit.
    let tmp = TempDir::new("roll-partial");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let out_dir = tmp.path("out");
    let missing = tmp.path("does-not-exist.tif");
    let (code, stdout, _err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        missing.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "a failed frame must make the roll exit non-zero");
    let report = json(&stdout);
    assert_eq!(report["summary"]["succeeded"], 1);
    assert_eq!(report["summary"]["failed"], 1);
    // The good frame still produced an output.
    assert!(is_tiff(&out_dir.join("hdr-48bit_positive.tiff")));
    // The failed frame carries an error message and "failed" status.
    let failed = report["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["status"] == "failed")
        .expect("a failed frame entry");
    assert!(
        failed["error"].is_string(),
        "failed frame has an error: {failed}"
    );
}

#[test]
fn roll_rejects_same_stem_output_collision() {
    // Two inputs with the same stem in different dirs map to one output name —
    // caught loudly up front, before anything is written.
    let tmp = TempDir::new("roll-collision");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let dir_a = tmp.path("a");
    let dir_b = tmp.path("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::copy(fixture("hdr-48bit.tif"), dir_a.join("frame.tif")).unwrap();
    std::fs::copy(fixture("hdr-48bit.tif"), dir_b.join("frame.tif")).unwrap();
    let (code, _out, err) = run(&[
        "roll",
        dir_a.join("frame.tif").to_str().unwrap(),
        dir_b.join("frame.tif").to_str().unwrap(),
        "--out-dir",
        tmp.path("out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an output-name collision is a usage error");
    assert!(err.contains("collides"), "stderr should explain: {err}");
}

#[test]
fn roll_directory_input_expands_to_sorted_tiffs() {
    // A positional directory expands to its .tif/.tiff files (sorted), non-TIFFs
    // ignored. Copy the fixture under two names + a stray .txt, roll the dir.
    let tmp = TempDir::new("roll-dir");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let scans = tmp.path("scans");
    std::fs::create_dir_all(&scans).unwrap();
    std::fs::copy(fixture("hdr-48bit.tif"), scans.join("b.tif")).unwrap();
    std::fs::copy(fixture("hdr-48bit.tif"), scans.join("a.tiff")).unwrap();
    std::fs::write(scans.join("notes.txt"), b"not a scan").unwrap();
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        scans.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "directory roll should succeed:\n{stdout}\n{err}");
    let report = json(&stdout);
    assert_eq!(report["summary"]["total"], 2, "only the two TIFFs convert");
    // Expanded in sorted order: a.tiff before b.tif.
    let frames = report["frames"].as_array().unwrap();
    assert!(
        frames[0]["input"].as_str().unwrap().ends_with("a.tiff"),
        "frames are sorted: {report}"
    );
    assert!(frames[1]["input"].as_str().unwrap().ends_with("b.tif"));
    assert!(is_tiff(&out_dir.join("a_positive.tiff")));
    assert!(is_tiff(&out_dir.join("b_positive.tiff")));
    // The .txt is not treated as a frame.
    assert!(!out_dir.join("notes_positive.tiff").exists());
}

#[test]
fn roll_empty_batch_errors_loudly_on_both_paths() {
    // An empty `--frames` manifest and positional inputs matching no files both
    // fail loudly as usage errors (exit 2), before anything is written.
    let tmp = TempDir::new("roll-empty");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);

    // (a) empty manifest.
    let manifest = write_file(&tmp.path("empty.json"), r#"{ "frames": [] }"#);
    let (code, _out, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        tmp.path("out-a").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an empty manifest is a usage error");
    assert!(
        err.contains("lists no frames"),
        "stderr should explain: {err}"
    );

    // (b) a positional directory that contains no TIFFs.
    let empty_dir = tmp.path("empty-dir");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let (code, _out, err) = run(&[
        "roll",
        empty_dir.to_str().unwrap(),
        "--out-dir",
        tmp.path("out-b").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "inputs matching no files is a usage error");
    assert!(
        err.contains("matched no files"),
        "stderr should explain: {err}"
    );
}

/// A shared recipe with a NON-explicit (region) film base — every frame
/// re-estimates its own Dmin, so the roll is not truly frozen.
const ROLL_RECIPE_REGION: &str = r#"{
  "reconstruction": { "type": "density" },
  "film_base": { "source": { "region": [0, 0, 502, 462] } }
}"#;

#[test]
fn roll_warns_when_film_base_is_not_frozen() {
    // A non-explicit shared base is a loud roll-level warning (the roll is not
    // color-consistent), but not a hard failure — the batch still converts.
    let tmp = TempDir::new("roll-notfrozen");
    let recipe = write_file(&tmp.path("region.json"), ROLL_RECIPE_REGION);
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        tmp.path("out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "a non-frozen base warns, not fails:\n{stdout}\n{err}"
    );
    let report = json(&stdout);
    assert_eq!(report["summary"]["succeeded"], 1);
    // The roll-level warning names the problem and the fix, and is echoed to stderr.
    let w = report["warnings"]
        .as_array()
        .expect("roll-level warnings array");
    assert!(
        w.iter().any(|m| m.as_str().unwrap().contains("NOT frozen")
            && m.as_str().unwrap().contains("nc estimate")),
        "roll-level not-frozen warning present: {report}"
    );
    assert!(
        err.contains("NOT frozen"),
        "warning echoed to stderr: {err}"
    );
}

#[test]
fn roll_strict_promotes_a_warning_while_still_emitting_the_report() {
    // `--strict` turns the not-frozen roll-level warning into a non-zero exit, but
    // the machine-readable report still lands on stdout first (pairs with the
    // warning test above). The frames themselves convert (failed == 0).
    let tmp = TempDir::new("roll-strict");
    let recipe = write_file(&tmp.path("region.json"), ROLL_RECIPE_REGION);
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        tmp.path("out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(code, 1, "--strict promotes the warning to a failing exit");
    let report = json(&stdout); // report still emitted before the gate
    assert_eq!(
        report["summary"]["failed"], 0,
        "the frame converted; the non-zero exit is the strict gate, not a frame failure"
    );
    assert!(
        !report["warnings"].as_array().unwrap().is_empty(),
        "the promoted warning is still in the report: {report}"
    );
    assert!(err.contains("strict"), "stderr should explain: {err}");
}

#[test]
fn roll_warns_on_per_frame_film_base_override() {
    // film_base is meant to be roll-fixed, but a per-frame override that sets it is
    // applied (the frame converts with its overridden base) with a loud,
    // `--strict`-promotable warning — not rejected.
    let tmp = TempDir::new("roll-fb-override");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let manifest_txt = format!(
        r#"{{ "frames": [
             {{ "input": {hdr:?},
                "params": {{ "film_base": {{ "source": {{ "explicit": [0.8, 0.5, 0.4] }} }} }} }}
           ] }}"#,
        hdr = hdr.to_str().unwrap(),
    );
    let manifest = write_file(&tmp.path("frames.json"), &manifest_txt);
    let roll_args = |out: &str, strict: bool| -> Vec<String> {
        let mut a = vec![
            "roll".to_string(),
            "--frames".to_string(),
            manifest.to_str().unwrap().to_string(),
            "--out-dir".to_string(),
            tmp.path(out).to_str().unwrap().to_string(),
            "--params".to_string(),
            recipe.to_str().unwrap().to_string(),
        ];
        if strict {
            a.push("--strict".to_string());
        }
        a
    };

    // Without --strict: the frame converts (exit 0) with a loud roll-level warning.
    let args = roll_args("out", false);
    let (code, stdout, err) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        code, 0,
        "an override warns, it does not fail:\n{stdout}\n{err}"
    );
    let report = json(&stdout);
    assert_eq!(
        report["summary"]["succeeded"], 1,
        "the frame still converts"
    );
    let w = report["warnings"]
        .as_array()
        .expect("roll-level warnings array");
    assert!(
        w.iter().any(|m| m
            .as_str()
            .unwrap()
            .contains("overriding the roll-fixed base")),
        "the per-frame film_base override warns loudly: {report}"
    );
    assert!(
        err.contains("overriding the roll-fixed base"),
        "warning echoed to stderr: {err}"
    );

    // With --strict: the same warning promotes to a non-zero exit, report still emits.
    let args = roll_args("out-strict", true);
    let (code, stdout, err) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        code, 1,
        "--strict promotes the override warning to a failing exit"
    );
    let report = json(&stdout);
    assert_eq!(
        report["summary"]["failed"], 0,
        "the frame converted; the exit is the strict gate"
    );
    assert!(!report["warnings"].as_array().unwrap().is_empty());
    assert!(err.contains("strict"), "stderr should explain: {err}");
}

#[test]
fn roll_warns_on_per_frame_dmax_override() {
    // reconstruction.curve.dmax is a roll-fixed calibration (like film_base) since the
    // dmax-reference task, but a per-frame override that sets it is applied (the
    // frame converts with its overridden anchor) with a loud, `--strict`-promotable
    // warning — not rejected. Mirrors `roll_warns_on_per_frame_film_base_override`.
    let tmp = TempDir::new("roll-dmax-override");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let manifest_txt = format!(
        r#"{{ "frames": [
             {{ "input": {hdr:?},
                "params": {{ "reconstruction": {{ "curve": {{ "dmax": {{ "explicit": 2.4 }} }} }} }} }}
           ] }}"#,
        hdr = hdr.to_str().unwrap(),
    );
    let manifest = write_file(&tmp.path("frames.json"), &manifest_txt);
    let roll_args = |out: &str, strict: bool| -> Vec<String> {
        let mut a = vec![
            "roll".to_string(),
            "--frames".to_string(),
            manifest.to_str().unwrap().to_string(),
            "--out-dir".to_string(),
            tmp.path(out).to_str().unwrap().to_string(),
            "--params".to_string(),
            recipe.to_str().unwrap().to_string(),
        ];
        if strict {
            a.push("--strict".to_string());
        }
        a
    };

    // Without --strict: the frame converts (exit 0) with a loud roll-level warning.
    let args = roll_args("out", false);
    let (code, stdout, err) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        code, 0,
        "an override warns, it does not fail:\n{stdout}\n{err}"
    );
    let report = json(&stdout);
    assert_eq!(
        report["summary"]["succeeded"], 1,
        "the frame still converts"
    );
    let w = report["warnings"]
        .as_array()
        .expect("roll-level warnings array");
    assert!(
        w.iter().any(|m| m
            .as_str()
            .unwrap()
            .contains("overriding the roll-fixed display-white anchor")),
        "the per-frame reconstruction.curve.dmax override warns loudly: {report}"
    );
    assert!(
        err.contains("overriding the roll-fixed display-white anchor"),
        "warning echoed to stderr: {err}"
    );

    // With --strict: the same warning promotes to a non-zero exit, report still emits.
    let args = roll_args("out-strict", true);
    let (code, stdout, err) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        code, 1,
        "--strict promotes the override warning to a failing exit"
    );
    let report = json(&stdout);
    assert_eq!(
        report["summary"]["failed"], 0,
        "the frame converted; the exit is the strict gate"
    );
    assert!(!report["warnings"].as_array().unwrap().is_empty());
    assert!(err.contains("strict"), "stderr should explain: {err}");
}

#[test]
fn roll_failed_frame_keeps_a_warning_raised_before_the_failure() {
    // A frame that warns (an explicit --d-max combined with non-default
    // density-scale fires the pre-decode anchor-domain warning) and *then* fails
    // (missing input → decode error) still reports the earlier warning.
    let tmp = TempDir::new("roll-warn-then-fail");
    let recipe = write_file(
        &tmp.path("dmax-domain.json"),
        r#"{ "reconstruction": {
               "type": "density",
               "density": { "scale": [1.1, 1.0, 0.9] },
               "curve": { "type": "exponential", "dmax": { "explicit": 1.6 } } },
             "film_base": { "source": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let missing = tmp.path("does-not-exist.tif");
    let (code, stdout, _err) = run(&[
        "roll",
        missing.to_str().unwrap(),
        "--out-dir",
        tmp.path("out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "the failed frame makes the roll exit non-zero");
    let report = json(&stdout);
    let f = &report["frames"][0];
    assert_eq!(f["status"], "failed");
    assert!(f["error"].is_string(), "failed frame carries an error: {f}");
    assert!(
        f["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("re-measure --d-max")),
        "the warning raised before the failure survives in the report: {f}"
    );
}

#[test]
fn roll_two_frame_output_is_byte_identical_on_rerun() {
    // Determinism across a MULTI-frame batch: every per-frame output is
    // byte-identical when the same batch + recipe runs twice.
    let tmp = TempDir::new("roll-determinism2");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let run_into = |dir: &Path| {
        let (code, _out, err) = run(&[
            "roll",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
    };
    let a = tmp.path("out-a");
    let b = tmp.path("out-b");
    run_into(&a);
    run_into(&b);
    for name in ["hdr-48bit_positive.tiff", "hdri-64bit_positive.tiff"] {
        assert_eq!(
            std::fs::read(a.join(name)).unwrap(),
            std::fs::read(b.join(name)).unwrap(),
            "{name} must be byte-identical across runs"
        );
    }
}

#[test]
fn roll_frame_sidecar_records_the_merged_recipe() {
    // Each frame's sidecar records that frame's MERGED effective recipe — an
    // overridden frame's sidecar carries its own overridden value, not the shared.
    let tmp = TempDir::new("roll-sidecar");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let hdri = fixture("hdri-64bit.tif");
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [
                 {{ "input": {hdr:?} }},
                 {{ "input": {hdri:?}, "params": {{ "print": {{ "print_exposure": 0.5 }} }} }}
               ] }}"#,
            hdr = hdr.to_str().unwrap(),
            hdri = hdri.to_str().unwrap(),
        ),
    );
    let out_dir = tmp.path("out");
    let (code, _out, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    // Each roll frame gets the same `{meta, params}` sidecar a single `convert`
    // writes; the merged per-frame recipe is the `params` body.
    let read_sidecar = |stem: &str| -> serde_json::Value { sidecar_params(&out_dir.join(stem)) };
    let overridden = read_sidecar("hdri-64bit_positive.tiff");
    let shared = read_sidecar("hdr-48bit_positive.tiff");
    assert_eq!(
        overridden["print"]["print_exposure"].as_f64().unwrap(),
        0.5,
        "the overridden frame's sidecar records its merged (overridden) value"
    );
    assert_ne!(
        shared["print"]["print_exposure"].as_f64().unwrap(),
        0.5,
        "the un-overridden frame's sidecar keeps the shared value, not the override"
    );
}

#[test]
fn roll_manifest_output_into_subdirectory_is_created() {
    // A manifest output naming a subdirectory (`sub/x.tiff`) has its parent
    // created before the encode, so the write succeeds.
    let tmp = TempDir::new("roll-subdir");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {hdr:?}, "output": "sub/deep/x.tiff" }} ] }}"#,
            hdr = hdr.to_str().unwrap(),
        ),
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "subdir output should be created:\n{stdout}\n{err}");
    assert!(
        is_tiff(&out_dir.join("sub/deep/x.tiff")),
        "the manifest subdirectory output was written"
    );
}

// --- named output presets: film-master ---------------------------------------

/// Read the interleaved f32 samples out of a float TIFF, together with the
/// per-sample bit depth and TIFF `SampleFormat` code (3 = IEEE float). Used to
/// prove the `film-master` container is genuinely unclamped 32-bit float rather
/// than a quantized image that merely happens to look right.
fn read_f32_tiff(path: &Path) -> (Vec<f32>, u16, u16) {
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;
    let mut dec = Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .unwrap()
        .with_limits(tiff::decoder::Limits::unlimited());
    // Both tags are 3-element SHORT arrays here (one entry per sample), so read
    // them as vectors and require every channel to agree.
    let mut all_equal = |tag: Tag| -> u16 {
        let v = dec.get_tag_u16_vec(tag).unwrap();
        assert_eq!(v.len(), 3, "{tag:?} must have one entry per sample: {v:?}");
        assert!(
            v.iter().all(|x| *x == v[0]),
            "{tag:?} channels differ: {v:?}"
        );
        v[0]
    };
    let bits = all_equal(Tag::BitsPerSample);
    let format = all_equal(Tag::SampleFormat);
    let samples = match dec.read_image().unwrap() {
        DecodingResult::F32(v) => v,
        other => panic!("film-master must be a float TIFF, got a different sample type: {other:?}"),
    };
    (samples, bits, format)
}

/// The per-sample bit depth of a written TIFF, without caring about the sample type
/// — for asserting that a run landed on 16-bit where `read_f32_tiff` would panic.
fn read_tiff_bits(path: &Path) -> u16 {
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;
    let mut dec = Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .unwrap()
        .with_limits(tiff::decoder::Limits::unlimited());
    let v = dec.get_tag_u16_vec(Tag::BitsPerSample).unwrap();
    assert!(
        v.iter().all(|x| *x == v[0]),
        "BitsPerSample channels differ: {v:?}"
    );
    v[0]
}

/// The embedded ICC blob (`ICCProfile`, tag 34675) of a written TIFF. Only ever
/// compared against *another run of the same binary* — lcms2's synthesized bytes
/// differ per target, so a checked-in hash would be red on the other CI host.
fn read_icc_tag(path: &Path) -> Vec<u8> {
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;
    let mut dec = Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .unwrap()
        .with_limits(tiff::decoder::Limits::unlimited());
    dec.get_tag_u8_vec(Tag::Unknown(34675))
        .unwrap_or_else(|e| panic!("{} has no ICCProfile tag: {e}", path.display()))
}

/// The samples of a single-channel TIFF, in whichever type it was written as.
#[derive(Debug)]
enum GraySamples {
    U16(Vec<u16>),
    F32(Vec<f32>),
}

/// Read a one-channel TIFF (the `--export-ir` sidecar): per-sample bit depth, TIFF
/// `SampleFormat` code (1 = unsigned int, 3 = IEEE float), and the samples.
fn read_gray_tiff(path: &Path) -> (u16, u16, GraySamples) {
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;
    let mut dec = Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .unwrap()
        .with_limits(tiff::decoder::Limits::unlimited());
    let one = |tag: Tag, dec: &mut Decoder<_>| -> u16 {
        let v = dec.get_tag_u16_vec(tag).unwrap();
        assert_eq!(v.len(), 1, "{tag:?} must have one entry: {v:?}");
        v[0]
    };
    let bits = one(Tag::BitsPerSample, &mut dec);
    let format = one(Tag::SampleFormat, &mut dec);
    let samples = match dec.read_image().unwrap() {
        DecodingResult::U16(v) => GraySamples::U16(v),
        DecodingResult::F32(v) => GraySamples::F32(v),
        other => panic!("unexpected IR sample type: {other:?}"),
    };
    (bits, format, samples)
}

#[test]
fn film_master_writes_unclamped_float_acescg_and_reports_the_branch() {
    // The master round-trips unclamped finite ACEScg through a float TIFF and says
    // in the report exactly what it is: no print controls, no display render,
    // NC film RGB v1 provenance, and no claim of physical scene recovery.
    //
    // `--d-max 0.2` is a *reconstruction* control (roll-fixed explicit anchor, which
    // the master accepts) chosen so the placement pushes every sample well above
    // 1.0 — that is what makes the unclamped round-trip observable instead of
    // vacuous. Value magnitudes only; no whole-file or post-transform checksum.
    let tmp = TempDir::new("film-master");
    let out = tmp.path("master.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--film-base",
        "0.9,0.55,0.42",
        "--d-max",
        "0.2",
    ]);
    assert_eq!(
        code, 0,
        "film-master convert should succeed:\n{stdout}\n{err}"
    );
    assert!(is_tiff(&out));

    let (samples, bits, format) = read_f32_tiff(&out);
    assert_eq!(bits, 32, "the master is 32-bit");
    assert_eq!(format, 3, "the master is IEEE float (SampleFormat 3)");
    let above_one = samples.iter().filter(|v| **v > 1.0).count();
    assert!(
        above_one > samples.len() / 2,
        "the fixture must actually exercise the unclamped range \
         ({above_one} of {} samples above 1.0)",
        samples.len()
    );
    assert!(
        samples.iter().all(|v| v.is_finite()),
        "every written sample must be finite here"
    );

    let report = json(&stdout);
    // Nothing was clipped or lost: the float path never reaches the u16 quantizer.
    assert_eq!(report["loss"]["clipped_low"], 0);
    assert_eq!(report["loss"]["clipped_high"], 0);
    assert_eq!(report["loss"]["non_finite"], 0);
    // The branch record (design-spec §5/§8).
    let branch = &report["output_render"];
    assert_eq!(branch["preset"], "film-master");
    assert_eq!(branch["print_controls"], false);
    assert_eq!(branch["display_render"], false);
    assert_eq!(branch["encoding"], "unclamped-linear-acescg-float-tiff");
    assert_eq!(branch["working_mapping"], "nc-film-rgb-v1");
    assert_eq!(branch["reconstruction_schema_version"], 1);
    let content = branch["content"].as_str().unwrap();
    assert!(content.contains("not a physical scene-linear"), "{content}");
    // …and the versions the master depends on are all recorded.
    assert_eq!(report["working_mapping"], "nc-film-rgb-v1");
    assert_eq!(report["reconstruction_result"]["type"], "density");
    assert_eq!(
        report["reconstruction_result"]["curve"]["type"],
        "exponential"
    );
    assert_eq!(
        report["reconstruction_result"]["curve"]["dmax"],
        serde_json::json!({"policy": "explicit", "value": 0.2, "provenance": "cli"})
    );
    assert_eq!(report["dmax"], 0.2);
    // No white-balance stage ran, so the master claims no resolved gains.
    assert!(report.get("white_balance").is_none());
    // The pre-release name must appear nowhere in the report.
    assert!(!stdout.contains("scene-master"));

    // The sidecar records the preset and reloads cleanly (deny_unknown_fields),
    // reproducing the master byte-for-byte on the same build.
    let sidecar_path = sidecar_of(&out);
    let recipe = sidecar_params(&out);
    assert_eq!(recipe["output"]["preset"], "film-master");
    assert_eq!(
        recipe["print"]["linear_range"],
        serde_json::json!([0.0, 1.0])
    );
    let again = tmp.path("master2.tiff");
    let (code, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        again.to_str().unwrap(),
        "--params",
        sidecar_path.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "the film-master sidecar must reload:\n{err}");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&again).unwrap(),
        "reloading the film-master sidecar must reproduce the master"
    );
}

#[test]
fn film_master_never_silently_ignores_a_requested_adjustment() {
    // Every rejection the master owes the user, through the real binary: the two
    // frame-local measurements, each non-default downstream control, and each
    // non-default legacy depth/profile/container selector a named preset resolves
    // itself. All are usage errors (exit 2) — never a quietly-adjusted or
    // quietly-unadjusted image.
    //
    // Each `expect` is a phrase distinctive to *this* rule: `contains("auto")` alone
    // would also match a `balance_range: "auto"`, a film-base `"auto"`, or `--auto-wb`,
    // so it would stay green if the rule it names disappeared.
    let tmp = TempDir::new("film-master-reject");
    let input = fixture("hdri-64bit.tif");
    let base = ["--film-base", "0.9,0.55,0.42"];
    for (extra, expect) in [
        (
            vec!["--auto-d-max"],
            "rejects a frame-local auto display-white",
        ),
        (
            vec!["--shadow-balance", "0.1,0,0", "--auto-balance-range"],
            "rejects a frame-local auto regional-balance range",
        ),
        // …and the same balance is accepted with an explicit roll range, so the
        // rejection above is about the *measurement*, not the correction.
        (vec!["--print-exposure", "0.5"], "print_exposure"),
        (vec!["--black-point", "0.01"], "black_point"),
        (vec!["--white-balance", "1.05,1,0.93"], "white_balance"),
        (vec!["--auto-wb", "percentile"], "white_balance"),
        (vec!["--highlight-compress", "0.2"], "highlight_compress"),
        (vec!["--linear-range", "0.02,0.97"], "linear_range"),
        (vec!["--output-hdr"], "output.hdr"),
        (vec!["--output-profile", "srgb"], "output.output_profile"),
        (vec!["--bigtiff", "on"], "output.bigtiff"),
    ] {
        let out = tmp.path(&format!(
            "m{}.tiff",
            extra.join("_").replace(['-', ',', '.'], "")
        ));
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "film-master",
        ];
        args.extend_from_slice(&base);
        args.extend_from_slice(&extra);
        let (code, _stdout, err) = run(&args);
        assert_eq!(code, 2, "{extra:?} must be a usage error:\n{err}");
        assert!(err.contains(expect), "{extra:?}: message was {err}");
        assert!(err.contains("film-master"), "{extra:?}: message was {err}");
        assert!(!out.exists(), "{extra:?}: no output may be written");
    }

    // `--output-sdr` is rejected by flag **presence**, not by resolved value: it forces
    // the default 16-bit integer TIFF, which the master cannot produce, so the request
    // is contradicted rather than redundant. Rejected regardless of where the *preset*
    // came from, and regardless of what the recipe says about `hdr`.
    let hdr_recipe = write_file(
        &tmp.path("hdr-recipe.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},"output":{"hdr":true}}"#,
    );
    let preset_recipe = write_file(
        &tmp.path("preset-recipe.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master"}}"#,
    );
    let master_hdr_recipe = write_file(
        &tmp.path("preset-hdr-recipe.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master","hdr":true}}"#,
    );
    for (name, args) in [
        (
            "flag preset",
            vec!["--output-preset", "film-master", "--output-sdr"],
        ),
        (
            "recipe preset",
            vec!["--params", preset_recipe.to_str().unwrap(), "--output-sdr"],
        ),
        (
            // The case that motivated rejecting it: 16-bit requested twice, f32 written.
            "recipe preset + recipe hdr:true",
            vec![
                "--params",
                master_hdr_recipe.to_str().unwrap(),
                "--output-sdr",
            ],
        ),
    ] {
        let out = tmp.path(&format!("sdr-{}.tiff", name.replace([' ', ':', '+'], "-")));
        let mut argv = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ];
        argv.extend_from_slice(&base);
        argv.extend_from_slice(&args);
        let (code, _stdout, err) = run(&argv);
        assert_eq!(
            code, 2,
            "{name}: --output-sdr must be a usage error:\n{err}"
        );
        assert!(err.contains("--output-sdr"), "{name}: {err}");
        assert!(err.contains("16-bit integer"), "{name}: {err}");
        assert!(!out.exists(), "{name}: no output may be written");
    }
    // …while `--output-sdr` keeps its entire legacy job when no named preset is in play,
    // including resetting a recipe `hdr: true` back to the 16-bit default.
    let sdr_legacy = tmp.path("sdr-legacy.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        sdr_legacy.to_str().unwrap(),
        "--params",
        hdr_recipe.to_str().unwrap(),
        "--output-sdr",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "--output-sdr on the legacy path must work:\n{err}");
    assert_eq!(
        read_tiff_bits(&sdr_legacy),
        16,
        "--output-sdr must still reset a recipe hdr:true to 16-bit"
    );

    // A legacy selector whose value *already equals* the documented default asks the
    // preset for nothing it does not do, so it stays accepted — `--bigtiff auto` means
    // "decide for me". This is the value rule, contrasted with the presence rule above.
    let ok = tmp.path("ok-bigtiff-auto.tiff");
    let mut argv = vec![
        "convert",
        input.to_str().unwrap(),
        "-o",
        ok.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--bigtiff",
        "auto",
        "--report",
        "none",
    ];
    argv.extend_from_slice(&base);
    let (code, _stdout, err) = run(&argv);
    assert_eq!(code, 0, "--bigtiff auto must be accepted:\n{err}");
    assert_eq!(read_f32_tiff(&ok).1, 32);
    // A recipe `hdr: false` likewise asserts nothing (it is the serde default).
    let ok_false = tmp.path("ok-hdr-false.tiff");
    let hdr_false = write_file(
        &tmp.path("preset-hdr-false.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master","hdr":false}}"#,
    );
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        ok_false.to_str().unwrap(),
        "--params",
        hdr_false.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "a recipe hdr:false must be accepted:\n{err}");
    assert_eq!(read_f32_tiff(&ok_false).1, 32);
    // …while a recipe `hdr: true` is the loud value-rule error.
    let bad = tmp.path("bad-recipe.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        bad.to_str().unwrap(),
        "--params",
        master_hdr_recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "a recipe hdr:true must be rejected:\n{err}");
    assert!(err.contains("output.hdr"), "{err}");
    assert!(!bad.exists());

    // A measured balance range is rejected, but the *same* balance with an explicit
    // roll range is accepted — the rejection is about the per-frame measurement.
    let out = tmp.path("balanced.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--film-base",
        "0.9,0.55,0.42",
        "--shadow-balance",
        "0.1,0,0",
        "--balance-range",
        "0.2,1.6",
        "--report",
        "none",
    ]);
    assert_eq!(
        code, 0,
        "an explicit roll balance-range must be accepted:\n{err}"
    );
    assert_eq!(read_f32_tiff(&out).1, 32);
}

#[test]
fn film_master_writes_a_negative_sample_through_unclamped() {
    // "Unclamped" is only half-proven by samples above 1.0 (the other master test):
    // an f32 clamp-to-zero, or a future gamut clamp anywhere on the branch, would be
    // invisible to it. A film base *below* some pixels' transmission makes `simple`'s
    // `1 − scan/Dmin` negative, and NC film RGB v1's matrix is all-positive with rows
    // summing to 1, so those negatives survive the mapping — which is exactly the
    // property being pinned.
    let tmp = TempDir::new("film-master-negative");
    let out = tmp.path("master.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--reconstruction",
        "simple",
        "--film-base",
        "0.2,0.2,0.2",
    ]);
    assert_eq!(
        code, 0,
        "film-master convert should succeed:\n{stdout}\n{err}"
    );
    let (samples, bits, format) = read_f32_tiff(&out);
    assert_eq!((bits, format), (32, 3));
    let below_zero = samples.iter().filter(|v| **v < 0.0).count();
    assert!(
        below_zero > 0,
        "the master must write negative samples through unclamped \
         (min was {:?})",
        samples.iter().cloned().fold(f32::INFINITY, f32::min)
    );
    // Note the report's `clipped_low`/`clipped_high` are structurally 0 on the f32
    // path (only the u16 quantizer clamps), so they are NOT the unclamped proof —
    // the sample values above are.
    let report = json(&stdout);
    assert_eq!(report["loss"]["clipped_low"], 0);
    assert_eq!(report["loss"]["clipped_high"], 0);
}

#[test]
fn film_master_embeds_the_same_acescg_icc_the_binary_writes_for_that_space() {
    // The written master's ICC tag is what tells a downstream tool the pixels are
    // linear ACEScg, and no other test reads it end-to-end. Compare it against the
    // blob the *same binary* embeds when asked for `acescg` explicitly — two runs of
    // one build, never a checked-in ICC hash (lcms2's bytes differ per target).
    let tmp = TempDir::new("film-master-icc");
    let input = fixture("hdri-64bit.tif");
    let convert = |name: &str, extra: &[&str]| -> PathBuf {
        let out = tmp.path(name);
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--report",
            "none",
        ];
        args.extend_from_slice(extra);
        let (code, _stdout, err) = run(&args);
        assert_eq!(code, 0, "{name} should convert:\n{err}");
        out
    };
    let master = convert("master.tiff", &["--output-preset", "film-master"]);
    let legacy_acescg = convert(
        "legacy-acescg.tiff",
        &["--output-hdr", "--output-profile", "acescg"],
    );
    let icc = read_icc_tag(&master);
    assert!(
        icc.len() > 100,
        "an ICC profile must be embedded: {}",
        icc.len()
    );
    assert_eq!(
        icc,
        read_icc_tag(&legacy_acescg),
        "the master must carry the same ACEScg profile the binary embeds for `acescg`"
    );
    // A different space really does produce different bytes, so the equality above is
    // not vacuous.
    let legacy_srgb = convert("legacy-srgb.tiff", &["--output-profile", "srgb"]);
    assert_ne!(icc, read_icc_tag(&legacy_srgb));
}

#[test]
fn film_master_ir_sidecar_follows_the_preset_depth_and_carries_the_plane() {
    // `--export-ir` writes the sidecar at `OutputParams::depth()`, so under
    // `film-master` it flips 16-bit → f32 even though `output.hdr` stays at its
    // default. Correct by construction (one depth for the whole run), but it is a
    // user-visible container change, so pin it — together with the Step-1 rule that
    // the IR plane is *carried*, never converted: the f32 sidecar's samples must equal
    // the 16-bit legacy sidecar's, up to u16 quantization.
    let tmp = TempDir::new("film-master-ir");
    let input = fixture("hdri-64bit.tif");
    let convert = |name: &str, extra: &[&str]| -> PathBuf {
        let out = tmp.path(&format!("{name}.tiff"));
        let ir = tmp.path(&format!("{name}-ir.tiff"));
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--export-ir",
            ir.to_str().unwrap(),
            "--report",
            "none",
        ];
        args.extend_from_slice(extra);
        let (code, _stdout, err) = run(&args);
        assert_eq!(code, 0, "{name} should convert:\n{err}");
        ir
    };

    // The legacy default: a 16-bit unsigned-integer IR sidecar.
    let legacy_ir = convert("legacy", &[]);
    let (legacy_bits, legacy_format, legacy_samples) = read_gray_tiff(&legacy_ir);
    assert_eq!(
        (legacy_bits, legacy_format),
        (16, 1),
        "the legacy default IR sidecar is 16-bit unsigned integer"
    );
    let GraySamples::U16(legacy_u16) = legacy_samples else {
        panic!("the legacy IR sidecar must be u16, got {legacy_samples:?}");
    };

    // Under the preset the same flag writes f32 — the preset's depth, unasked for.
    let master_ir = convert("master", &["--output-preset", "film-master"]);
    let (bits, format, master_samples) = read_gray_tiff(&master_ir);
    assert_eq!(
        (bits, format),
        (32, 3),
        "film-master's IR sidecar follows the preset's f32 depth"
    );
    let GraySamples::F32(master_f32) = master_samples else {
        panic!("the film-master IR sidecar must be f32");
    };

    // Same plane, carried not consumed: the f32 samples reproduce the u16 ones.
    assert_eq!(master_f32.len(), legacy_u16.len());
    for (i, (&f, &q)) in master_f32.iter().zip(&legacy_u16).enumerate() {
        let requantized = (f.clamp(0.0, 1.0) * 65535.0).round() as u16;
        assert!(
            requantized.abs_diff(q) <= 1,
            "IR sample {i}: f32 {f} requantizes to {requantized}, legacy u16 was {q}"
        );
    }
}

#[test]
fn film_master_telemetry_names_the_preset_and_the_written_depth() {
    // The record's `conversion.preset` is what distinguishes a master from a legacy
    // run, and `conversion.output_hdr` means "an f32 TIFF was written" — which under
    // the preset is true while `output.hdr` stays at its default. Reading the switch
    // directly reported `false` for a 4-bytes-per-sample file; this pins the fix
    // end-to-end, and the byte count pins that f32 is what actually landed on disk.
    let tmp = TempDir::new("film-master-telemetry");
    let out = tmp.path("master.tiff");
    let rec = tmp.path("run.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        rec.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "film-master + telemetry should succeed:\n{err}");

    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
    let conv = &record["conversion"];
    assert_eq!(record["schema_version"], 3);
    assert_eq!(conv["preset"], "film-master");
    assert_eq!(
        conv["output_hdr"], true,
        "the master writes f32, so the depth flag must say so: {conv}"
    );
    // Cross-check against the file the run actually wrote: 4 bytes per sample.
    let (samples, bits, _) = read_f32_tiff(&out);
    assert_eq!(bits, 32);
    let bytes = record["image"]["output_bytes"].as_u64().unwrap();
    assert!(
        bytes >= samples.len() as u64 * 4,
        "output_bytes {bytes} must cover {} f32 samples",
        samples.len()
    );

    // …and the legacy default on the same fixture still reports `false`, so the
    // assertion above is about the preset and not a constant.
    let legacy_out = tmp.path("legacy.tiff");
    let legacy_rec = tmp.path("legacy.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        legacy_out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        legacy_rec.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
    let legacy: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&legacy_rec).unwrap()).unwrap();
    assert_eq!(legacy["conversion"]["preset"], "legacy");
    assert_eq!(legacy["conversion"]["output_hdr"], false);
}

#[test]
fn film_master_without_a_dmax_anchor_does_not_claim_one() {
    // The master's reported `content` must not invent a Dmax placement: validation
    // deliberately accepts exponential `--no-d-max` (the scene-referred unity
    // placement) and `simple` has no anchor at all. The other master e2e test only
    // runs `--d-max 0.2`, so the anchorless wording was never exercised.
    let tmp = TempDir::new("film-master-no-dmax");
    let input = fixture("hdri-64bit.tif");
    let convert = |name: &str, extra: &[&str]| -> serde_json::Value {
        let out = tmp.path(name);
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "film-master",
            "--film-base",
            "0.9,0.55,0.42",
        ];
        args.extend_from_slice(extra);
        let (code, stdout, err) = run(&args);
        assert_eq!(code, 0, "{name} should convert:\n{err}");
        assert_eq!(read_f32_tiff(&out).1, 32, "{name} is still an f32 master");
        json(&stdout)
    };

    for (name, extra) in [
        ("no-dmax.tiff", vec!["--no-d-max"]),
        ("simple.tiff", vec!["--reconstruction", "simple"]),
    ] {
        let report = convert(name, &extra);
        let content = report["output_render"]["content"].as_str().unwrap();
        assert!(
            content.contains("placed no Dmax anchor"),
            "{name}: {content}"
        );
        assert!(!content.contains("roll-fixed Dmax"), "{name}: {content}");
        assert!(
            content.contains("not a physical scene-linear"),
            "{name}: {content}"
        );
        // No anchor was resolved, so none is reported either.
        assert!(report.get("dmax").is_none(), "{name}: {report}");
    }

    // The default fixed anchor DOES claim the placement — otherwise the assertions
    // above would pass against a message that never mentions Dmax at all.
    let report = convert("fixed.tiff", &[]);
    let content = report["output_render"]["content"].as_str().unwrap();
    assert!(content.contains("resolved roll-fixed Dmax"), "{content}");
    assert!(report["dmax"].as_f64().is_some());
}

#[test]
fn scene_master_is_rejected_as_an_unreleased_schema_break() {
    // `film-master` is the name. The pre-release `scene-master` is not an alias —
    // it wrongly implied physical scene-linear recovery — so both the flag and the
    // recipe key must reject it and point at the new name.
    let tmp = TempDir::new("scene-master");
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "scene-master",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 2, "scene-master must be a usage error:\n{err}");
    assert!(err.contains("scene-master"), "{err}");
    assert!(err.contains("film-master"), "{err}");

    let recipe = write_file(
        &tmp.path("recipe.json"),
        r#"{"output":{"preset":"scene-master"}}"#,
    );
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 2, "the recipe key must reject it too:\n{err}");
    assert!(err.contains("film-master"), "{err}");

    // A planned-but-unaccepted preset name gets its own "not yet" diagnosis.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "gain-map-hdr",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("does not accept yet"), "{err}");
}

#[test]
fn legacy_no_preset_output_is_unchanged_by_the_preset_machinery() {
    // The legacy no-preset TIFF path keeps its ordering and its pixels until the
    // output-preset migration: an explicit `--output-preset legacy` must produce a
    // byte-identical file to passing no preset at all, and must stay compatible with
    // the legacy depth/profile/container flags a named preset rejects.
    //
    // Note what this does *not* prove: both sides are the legacy branch, so swapping
    // `render`'s two match arms would leave both files identical and this test green.
    // The branch identity is pinned in-process by `stages`'
    // `legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence`.
    let tmp = TempDir::new("legacy-preset");
    let input = fixture("hdri-64bit.tif");
    let convert = |name: &str, extra: &[&str]| -> PathBuf {
        let out = tmp.path(name);
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--report",
            "none",
        ];
        args.extend_from_slice(extra);
        let (code, _out, err) = run(&args);
        assert_eq!(code, 0, "{name} should convert:\n{err}");
        out
    };
    let implicit = convert("implicit.tiff", &[]);
    let explicit = convert("explicit.tiff", &["--output-preset", "legacy"]);
    assert_eq!(
        std::fs::read(&implicit).unwrap(),
        std::fs::read(&explicit).unwrap(),
        "`--output-preset legacy` IS the no-preset path"
    );
    // …and it still accepts the legacy selectors, unlike a named preset.
    let with_flags = convert(
        "flags.tiff",
        &[
            "--output-preset",
            "legacy",
            "--output-hdr",
            "--bigtiff",
            "on",
        ],
    );
    assert!(is_tiff(&with_flags));
}

#[test]
fn roll_accepts_a_film_master_recipe() {
    // `nc roll` has no output flags at all — its output policy comes only from the
    // shared recipe — so `output.preset` must be honoured there too, and the
    // automatic `<stem>_positive.tiff` name is already correct for the master's TIFF
    // container. (Preset-aware suffix resolution stays with `output/presets`.)
    let tmp = TempDir::new("roll-film-master");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master"}}"#,
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "a film-master roll recipe must convert:\n{err}");
    let report = json(&stdout);
    assert_eq!(report["summary"]["succeeded"], 1);
    assert_eq!(report["recipe"]["output"]["preset"], "film-master");
    let out = out_dir.join("hdri-64bit_positive.tiff");
    let (_, bits, format) = read_f32_tiff(&out);
    assert_eq!((bits, format), (32, 3), "each frame is an f32 master");
    // A roll with no per-frame override must stay warning-free about the preset, so the
    // warning asserted below is genuinely caused by the override.
    let warnings = report["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("output.preset")),
        "an un-overridden roll must not warn about the preset: {warnings:?}"
    );
}

#[test]
fn roll_frame_override_of_output_preset_warns_and_is_strict_promotable() {
    // `output.preset` is roll-fixed like `film_base` and `reconstruction.curve.dmax`,
    // and it is the coarsest of the three: overriding it per frame emits a frame of a
    // different *image class* (a rendered u16 TIFF among unclamped linear ACEScg
    // masters). Its two siblings each warn; this one silently produced the odd frame.
    //
    // `FrameStatus` carries no `output_render` block (that field is convert-only), so
    // without this warning the only trace is the `frames[].overrides` echo.
    //
    // **The fixture must be IR-free.** `hdri-64bit.tif` carries an IR plane, so every
    // frame raises a per-frame "IR preserved but not used" warning, and
    // `strict_failure` is already true via `frames.iter().any(|f| !f.warnings.is_empty())`
    // — a no-override roll on that fixture exits 1 under `--strict` all by itself, which
    // made the promotion assertion below unfalsifiable (gutting `sets_output_preset` to
    // `|_| false` left it green). `hdr-48bit.tif` has no IR plane, so `--strict` there
    // exits 0 unless *this* warning fires, and the control run below pins that.
    let tmp = TempDir::new("roll-preset-override");
    let input = fixture("hdr-48bit.tif");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master"}}"#,
    );
    let manifest_for = |name: &str, body: &str| -> PathBuf { write_file(&tmp.path(name), body) };
    let overridden = manifest_for(
        "frames.json",
        &format!(
            r#"{{ "frames": [
                 {{ "input": {i:?}, "output": "master.tiff" }},
                 {{ "input": {i:?}, "output": "downgraded.tiff",
                    "params": {{ "output": {{ "preset": "legacy" }} }} }}
               ] }}"#,
            i = input.to_str().unwrap(),
        ),
    );
    let control = manifest_for(
        "frames-control.json",
        &format!(
            r#"{{ "frames": [ {{ "input": {i:?}, "output": "master.tiff" }} ] }}"#,
            i = input.to_str().unwrap(),
        ),
    );
    let roll = |manifest: &Path, out_dir: &Path, extra: &[&str]| -> (i32, String, String) {
        let mut argv: Vec<String> = [
            "roll",
            "--frames",
            manifest.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        argv.extend(extra.iter().map(|s| s.to_string()));
        run(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    };

    let out_dir = tmp.path("out");
    let (code, stdout, err) = roll(&overridden, &out_dir, &[]);
    assert_eq!(code, 0, "the override is applied, not rejected:\n{err}");

    // The warning names the frame and the key, and rides in the roll report (not just
    // stderr) so an agent piping stdout sees it.
    let report = json(&stdout);
    let warnings: Vec<String> = report["warnings"]
        .as_array()
        .expect("roll report must carry a warnings array")
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    let hit = warnings
        .iter()
        .find(|w| w.contains("output.preset"))
        .unwrap_or_else(|| panic!("no output.preset warning in {warnings:?}"));
    assert!(hit.contains("hdr-48bit.tif"), "{hit}");
    assert!(hit.contains("image class"), "{hit}");
    assert!(err.contains("output.preset"), "and on stderr too: {err}");

    // The override really did produce a different image class — that is the harm.
    assert_eq!(read_tiff_bits(&out_dir.join("master.tiff")), 32);
    assert_eq!(read_tiff_bits(&out_dir.join("downgraded.tiff")), 16);

    // Same shape as its two siblings: `--strict` promotes it to a non-zero exit…
    let (code, _stdout, err) = roll(&overridden, &tmp.path("strict-out"), &["--strict"]);
    assert_ne!(code, 0, "--strict must promote the warning:\n{err}");

    // …and the control that makes that falsifiable: the *same* recipe, fixture, and
    // `--strict` flag with no per-frame override exits 0 with no roll-level warning. So
    // the promotion above is caused by this warning and nothing else.
    let (code, stdout, err) = roll(&control, &tmp.path("control-out"), &["--strict"]);
    assert_eq!(
        code, 0,
        "an un-overridden --strict roll on the IR-free fixture must exit 0:\n{err}"
    );
    let control_report = json(&stdout);
    assert!(
        control_report["warnings"].is_null()
            || control_report["warnings"].as_array().unwrap().is_empty(),
        "control run must raise no roll-level warning: {}",
        control_report["warnings"]
    );
}

// ---------------------------------------------------------------------------
// Conversion identity + versioning (`core/conversion-versioning`)
// ---------------------------------------------------------------------------

/// FNV-1a over `text`, hex — a deliberate **independent reimplementation** of
/// `version::stable_hash` (integration tests can't link the binary crate's
/// internals). Pinning the algorithm from outside is the point: `params_hash` is a
/// wire value an agent reproduces by hashing `--dump-params`, so this test suite
/// must be able to compute it without trusting the code under test.
fn fnv1a_hex(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in text.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The default convert invocation the identity tests share: an explicit film base
/// (so nothing is estimated per frame) and a clean stdout report.
fn convert_default(input: &Path, out: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args = vec![
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ];
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn report_carries_every_identity_layer() {
    // Verify bullet 1: every report carries nc_version, the git commit, the
    // behavioral pipeline_version, and the params_hash.
    let tmp = TempDir::new("identity");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = convert_default(&fixture("hdri-64bit.tif"), &out, &[]);
    assert_eq!(code, 0, "{err}");
    let id = &json(&stdout)["identity"];

    assert_eq!(id["nc_version"], env!("CARGO_PKG_VERSION"));
    // This worktree is a git checkout, so the commit must be a real short hash —
    // never the string "unknown" (absence is modelled as an omitted field).
    let commit = id["git_commit"]
        .as_str()
        .unwrap_or_else(|| panic!("git_commit must be present in a git build: {id}"));
    assert!(
        commit.len() >= 7 && commit.chars().all(|c| c.is_ascii_hexdigit()),
        "git_commit must be a short hex hash, got {commit:?}"
    );
    assert!(
        id["git_dirty"].is_boolean(),
        "git_dirty must be a bool: {id}"
    );
    // The report's pipeline_version must be THIS build's, not merely "an integer":
    // cross-check it against the only other place the binary publishes the label.
    assert_eq!(
        id["pipeline_version"].as_u64(),
        Some(pipeline_version_from_version_flag()),
        "the report's pipeline_version must match `nc --version`: {id}"
    );
    let hash = id["params_hash"].as_str().expect("params_hash");
    assert_eq!(hash.len(), 16, "params_hash is a 64-bit hex digest: {hash}");
    assert!(!id["target"].as_str().unwrap().is_empty());
}

/// The `pipeline_version` this binary prints from `--version` — the independent
/// witness a report's value is checked against.
fn pipeline_version_from_version_flag() -> u64 {
    let (code, stdout, err) = run(&["--version"]);
    assert_eq!(code, 0, "{err}");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("pipeline_version: "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("--version must print `pipeline_version: <n>`:\n{stdout}"))
        .parse()
        .expect("pipeline_version must be an integer")
}

#[test]
fn inspect_and_estimate_carry_build_identity_without_a_params_hash() {
    // "`identity`, every report" (design-spec §9) — not just conversions. An
    // `inspect`/`estimate` result is an artifact someone files, and `params_hash` is
    // genuinely absent there because no recipe was resolved (which is what makes
    // `Identity::new`'s `None` a real state rather than a construction artifact).
    let expected_version = pipeline_version_from_version_flag();
    for args in [
        vec!["inspect", fixture("hdri-64bit.tif").to_str().unwrap()],
        vec![
            "estimate",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "--base-region",
            "0,0,502,462",
        ],
    ] {
        let (code, stdout, err) = run(&args);
        assert_eq!(code, 0, "{args:?}: {err}");
        let report = json(&stdout);
        let id = report
            .get("identity")
            .unwrap_or_else(|| panic!("{args:?}: no identity in {report}"));
        assert_eq!(id["nc_version"], env!("CARGO_PKG_VERSION"), "{args:?}");
        assert_eq!(id["pipeline_version"].as_u64(), Some(expected_version));
        assert!(!id["target"].as_str().unwrap().is_empty(), "{args:?}");
        assert!(
            id.get("params_hash").is_none(),
            "{args:?} resolves no recipe, so params_hash must be OMITTED: {id}"
        );
    }
}

#[test]
fn params_hash_is_the_hash_of_the_dump_params_bytes() {
    // The advertised hash must be reproducible by an agent: hash the exact bytes
    // `--dump-params` writes and you get `identity.params_hash`. That equality is
    // what makes the hash a usable cross-frame/cross-version config identity
    // instead of an opaque number.
    let tmp = TempDir::new("hash");
    let out = tmp.path("out.tiff");
    let dump = tmp.path("params.json");
    let (code, stdout, err) = convert_default(
        &fixture("hdri-64bit.tif"),
        &out,
        &[
            "--dump-params",
            dump.to_str().unwrap(),
            "--density-gamma",
            "1.7",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let dumped = std::fs::read_to_string(&dump).unwrap();
    let advertised = json(&stdout)["identity"]["params_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        fnv1a_hex(&dumped),
        advertised,
        "params_hash must be the hash of the --dump-params bytes"
    );

    // The sidecar carries the same recipe and advertises the same hash in `meta`, so
    // the sidecar and the report can never disagree. Compared as parsed JSON, not
    // text: re-serializing a `serde_json::Value` sorts keys (no `preserve_order`
    // feature here), so only the *document* is comparable, not the byte order —
    // the byte-level claim is the `--dump-params` equality asserted above.
    let doc = sidecar(&out);
    assert_eq!(doc["meta"]["params_hash"].as_str().unwrap(), advertised);
    assert_eq!(
        doc["params"],
        serde_json::from_str::<serde_json::Value>(&dumped).unwrap(),
        "the sidecar's params body is the --dump-params document"
    );

    // A changed knob ⇒ a different hash (the hash is actually sensitive).
    let out2 = tmp.path("out2.tiff");
    let (c2, s2, _) = convert_default(
        &fixture("hdri-64bit.tif"),
        &out2,
        &["--density-gamma", "1.8"],
    );
    assert_eq!(c2, 0);
    assert_ne!(
        json(&s2)["identity"]["params_hash"].as_str().unwrap(),
        advertised
    );
}

#[test]
fn version_flag_prints_the_full_build_identity() {
    // `nc --version` must be enough to attribute an output on its own.
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
    assert!(stdout.contains("pipeline_version:"), "{stdout}");
    assert!(stdout.contains("commit:"), "{stdout}");
    assert!(stdout.contains("target:"), "{stdout}");
}

#[test]
fn enveloped_sidecar_and_bare_legacy_recipe_both_reload_identically() {
    // Verify bullet 1, the round-trip trap, BOTH directions:
    //  (a) the new `{meta, params}` sidecar reloads through `--params`;
    //  (b) a BARE recipe object (a hand-written recipe, `--dump-params` output, or
    //      a pre-envelope sidecar) still reloads — the established shape must not
    //      be broken by the envelope;
    // and all three outputs are byte-identical, so the envelope costs no pixels.
    let tmp = TempDir::new("envelope");
    let input = fixture("hdri-64bit.tif");

    let out_a = tmp.path("a.tiff");
    let dump = tmp.path("bare.json");
    let (ca, _, err) = convert_default(
        &input,
        &out_a,
        &[
            "--dump-params",
            dump.to_str().unwrap(),
            "--density-gamma",
            "1.6",
            "--report",
            "none",
        ],
    );
    assert_eq!(ca, 0, "{err}");

    // (a) reload the enveloped sidecar.
    let out_b = tmp.path("b.tiff");
    let envelope = sidecar_of(&out_a);
    let (cb, _, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        out_b.to_str().unwrap(),
        "--params",
        envelope.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(cb, 0, "the enveloped sidecar must reload:\n{err}");

    // (b) reload the bare recipe (`--dump-params` output — the legacy shape).
    let out_c = tmp.path("c.tiff");
    let (cc, _, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        out_c.to_str().unwrap(),
        "--params",
        dump.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(cc, 0, "a bare legacy recipe must still reload:\n{err}");

    let (a, b, c) = (
        std::fs::read(&out_a).unwrap(),
        std::fs::read(&out_b).unwrap(),
        std::fs::read(&out_c).unwrap(),
    );
    assert_eq!(a, b, "enveloped reload must reproduce the output");
    assert_eq!(
        a, c,
        "bare-recipe reload must reproduce the same output as the envelope"
    );
}

#[test]
fn identity_fields_are_not_recipe_keys() {
    // The whole reason for the envelope: identity must NEVER be a recipe key. Each
    // one, placed bare in a recipe, is a loud unknown-key usage error (exit 2) —
    // if any of these silently deserialized, `deny_unknown_fields` would have been
    // weakened and future sidecars would smuggle provenance into the config.
    let tmp = TempDir::new("not-keys");
    for key in [
        r#""nc_version": "0.1.0""#,
        r#""pipeline_version": 1"#,
        r#""params_hash": "0000000000000000""#,
        r#""git_commit": "abc123""#,
        r#""identity": {}"#,
    ] {
        let recipe = write_file(&tmp.path("r.json"), &format!("{{ {key} }}"));
        let out = tmp.path("out.tiff");
        let (code, _, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(code, 2, "bare identity key {key} must be rejected: {err}");
    }
}

#[test]
fn meta_without_params_is_a_pointed_usage_error() {
    // A half-written envelope is a malformed envelope, not a bare recipe: it gets a
    // pointed message instead of the opaque `unknown field 'meta'` serde default.
    let tmp = TempDir::new("half-envelope");
    let recipe = write_file(
        &tmp.path("r.json"),
        r#"{ "meta": { "pipeline_version": 1 } }"#,
    );
    let out = tmp.path("out.tiff");
    let (code, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("`meta` block but no `params`"),
        "the error must name the envelope shape: {err}"
    );
}

#[test]
fn unknown_meta_fields_are_ignored_but_the_recipe_body_is_still_strict() {
    // `meta` is provenance, so an OLDER build must tolerate a NEWER build's extra
    // meta fields (forward compatibility) — while the `params` body keeps its full
    // `deny_unknown_fields` strictness.
    let tmp = TempDir::new("meta-fwd");
    let out = tmp.path("out.tiff");
    let ok = write_file(
        &tmp.path("ok.json"),
        r#"{ "meta": { "invented_future_field": [1, 2], "pipeline_version": 1 },
             "params": { "print": { "print_exposure": 0.25 } } }"#,
    );
    let (code, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        ok.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "unknown meta fields must be ignored:\n{err}");

    // Same flags as the accepted case above, so the ONLY difference is the typo —
    // otherwise the exit code could be blamed on the missing `--film-base`.
    let bad = write_file(
        &tmp.path("bad.json"),
        r#"{ "meta": {}, "params": { "print": { "print_exposur": 0.25 } } }"#,
    );
    let (code, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("bad.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        bad.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "a typo inside `params` must still be loud: {err}");
    assert!(
        err.contains("print_exposur"),
        "the error must name the offending key, not just fail: {err}"
    );

    // `params` itself is the envelope discriminator, so a `params` key *inside* the
    // recipe body is an unknown recipe key — pinning that a future stage section
    // can't quietly claim the name and turn every recipe into an envelope.
    let nested = write_file(
        &tmp.path("nested.json"),
        r#"{ "meta": {}, "params": { "params": {} } }"#,
    );
    let (code, _, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("nested.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        nested.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "`params` must not be a recipe key: {err}");
}

#[test]
fn a_non_object_recipe_body_is_refused_instead_of_converting_with_defaults() {
    // serde's derived visitor accepts a *sequence* for a struct and every recipe
    // field has a default, so both of these used to convert with ALL-DEFAULT
    // parameters at exit 0, advertising a params_hash byte-identical to the default
    // recipe's — a truncated or mis-generated sidecar silently ignoring the recipe
    // the operator believes is applied.
    let tmp = TempDir::new("non-object");
    for (tag, body) in [
        ("params-array", r#"{ "params": [] }"#),
        ("bare-array", "[]"),
        ("params-number", r#"{ "params": 3 }"#),
    ] {
        let recipe = write_file(&tmp.path(&format!("{tag}.json")), body);
        let (code, _, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            tmp.path(&format!("{tag}.tiff")).to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(code, 2, "{tag} must be refused: {err}");
        assert!(err.contains("must be a"), "{tag}: {err}");
    }
}

#[test]
fn an_unreadable_meta_pipeline_version_is_loud_not_silently_ignored() {
    // Mapped to `None`, an unreadable value is indistinguishable from an absent one
    // and disables the skew check entirely; truncated with `as u32`, 4294967297
    // becomes 1 and *matches* a build at pipeline_version 1, suppressing the warning
    // by pretending to agree with it. Both are the silent replay this label exists
    // to prevent.
    let tmp = TempDir::new("bad-meta-version");
    for (tag, value) in [
        ("float", "1.0"),
        ("string", "\"1\""),
        ("negative", "-1"),
        ("null", "null"),
        ("overflow", "4294967297"),
    ] {
        let recipe = write_file(
            &tmp.path(&format!("{tag}.json")),
            &format!(
                r#"{{ "meta": {{ "pipeline_version": {value} }},
                      "params": {{ "film_base": {{ "source": {{ "explicit": [0.9, 0.55, 0.42] }} }} }} }}"#
            ),
        );
        let (code, _, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            tmp.path(&format!("{tag}.tiff")).to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(
            code, 2,
            "meta.pipeline_version {value} must be refused: {err}"
        );
        assert!(err.contains("meta.pipeline_version"), "{tag}: {err}");
    }
}

#[test]
fn a_malformed_meta_container_is_refused_like_a_malformed_field() {
    // The container/field asymmetry: a corrupt *field* inside `meta` was already a
    // loud exit 2, but a corrupt `meta` *block* degraded to "records no version" and
    // replayed with no skew check at all — silently reproducing the very mismatch the
    // label exists to surface.
    let tmp = TempDir::new("bad-meta-container");
    for (tag, meta) in [
        ("null", "null"),
        ("string", "\"x\""),
        ("array", "[]"),
        ("number", "123"),
    ] {
        let recipe = write_file(
            &tmp.path(&format!("{tag}.json")),
            &format!(
                r#"{{ "meta": {meta},
                      "params": {{ "film_base": {{ "source": {{ "explicit": [0.9, 0.55, 0.42] }} }} }} }}"#
            ),
        );
        let (code, _, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            tmp.path(&format!("{tag}.tiff")).to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(code, 2, "meta={meta} must be refused: {err}");
        assert!(err.contains("`meta` must be an object"), "{tag}: {err}");
    }
}

#[test]
fn output_stats_report_the_written_samples_for_both_depths() {
    // `output_stats.mean` is the entire cross-version comparison basis — `nctool
    // compare` hard-fails without it — so its presence and shape are a contract, not
    // an implementation detail.
    let tmp = TempDir::new("output-stats");
    let out = tmp.path("u16.tiff");
    let (code, stdout, err) = convert_default(&fixture("hdri-64bit.tif"), &out, &[]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let mean = report["output_stats"]["mean"]
        .as_array()
        .unwrap_or_else(|| panic!("output_stats.mean must be present: {report}"));
    assert_eq!(mean.len(), 3, "one mean per channel: {mean:?}");
    for v in mean {
        let v = v.as_f64().expect("a finite number");
        assert!(
            v.is_finite() && (0.0..=1.0).contains(&v),
            "a u16 mean is the quantized value normalized into [0,1], got {v}"
        );
    }

    // f32/HDR output is written verbatim, so the mean is reported in *that* domain
    // (unclamped) — the reason `nctool compare` records the depth beside the mean.
    let hdr = tmp.path("hdr.tiff");
    let (code, stdout, err) = convert_default(&fixture("hdri-64bit.tif"), &hdr, &["--output-hdr"]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(
        report["output_stats"]["mean"].as_array().map(Vec::len),
        Some(3),
        "output_stats must be reported for --output-hdr too: {report}"
    );

    // A blown-out render ties the two report fields together: the clamped samples
    // the mean is taken over are the same ones `loss` counts.
    let clipped = tmp.path("clipped.tiff");
    let (code, stdout, err) = convert_default(
        &fixture("hdri-64bit.tif"),
        &clipped,
        &["--print-exposure", "40.0"],
    );
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert!(
        report["loss"]["clipped_high"].as_u64().unwrap_or(0) > 0,
        "a heavily over-exposed print must clip high: {report}"
    );
    let mean = report["output_stats"]["mean"][0].as_f64().unwrap();
    assert!(
        mean > 0.9,
        "the mean of the CLAMPED written samples must sit near display white, got {mean}"
    );
}

#[test]
fn roll_frames_carry_their_own_identity_and_comparison_basis() {
    // A roll's shared identity labels the frozen recipe; a per-frame override
    // genuinely changes THAT frame's effective recipe, so the difference has to be
    // visible per frame or the docs' claim that a roll is comparable is empty.
    let tmp = TempDir::new("roll-frame-identity");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let hdri = fixture("hdri-64bit.tif");
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [
                 {{ "input": {hdr:?} }},
                 {{ "input": {hdri:?}, "params": {{ "print": {{ "print_exposure": 0.5 }} }} }}
               ] }}"#,
            hdr = hdr.to_str().unwrap(),
            hdri = hdri.to_str().unwrap(),
        ),
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let shared_hash = report["identity"]["params_hash"].as_str().unwrap();

    let by_stem = |stem: &str| -> serde_json::Value {
        report["frames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["input"].as_str().unwrap().contains(stem))
            .unwrap_or_else(|| panic!("no frame for {stem} in {report}"))
            .clone()
    };
    let plain = by_stem("hdr-48bit");
    let overridden = by_stem("hdri-64bit");

    // Every ok frame carries a full identity plus its comparison basis.
    for (label, frame) in [("plain", &plain), ("overridden", &overridden)] {
        assert_eq!(
            frame["identity"]["nc_version"],
            env!("CARGO_PKG_VERSION"),
            "{label}: {frame}"
        );
        assert_eq!(
            frame["output_stats"]["mean"].as_array().map(Vec::len),
            Some(3),
            "{label} frame must carry output_stats: {frame}"
        );
    }

    // The un-overridden frame's hash is the shared recipe's; the overridden frame's
    // is not — and each frame's sidecar `meta` agrees with its report entry.
    let plain_hash = plain["identity"]["params_hash"].as_str().unwrap();
    let over_hash = overridden["identity"]["params_hash"].as_str().unwrap();
    assert_eq!(
        plain_hash, shared_hash,
        "no override ⇒ the shared recipe's hash"
    );
    assert_ne!(
        over_hash, shared_hash,
        "a per-frame override changes that frame's effective recipe, so its hash must differ"
    );
    assert_eq!(
        sidecar(&out_dir.join("hdr-48bit_positive.tiff"))["meta"]["params_hash"]
            .as_str()
            .unwrap(),
        plain_hash
    );
    assert_eq!(
        sidecar(&out_dir.join("hdri-64bit_positive.tiff"))["meta"]["params_hash"]
            .as_str()
            .unwrap(),
        over_hash
    );
}

#[test]
fn roll_warns_about_a_version_skewed_shared_recipe() {
    // `roll` has its own skew wiring, distinct from `convert`'s: the mismatch is a
    // roll-level fact (one shared recipe, N frames), so it rides the roll's warnings
    // rather than any single frame's.
    let tmp = TempDir::new("roll-skew");
    let stale = write_file(
        &tmp.path("stale.json"),
        r#"{ "meta": { "pipeline_version": 9999 },
             "params": { "reconstruction": { "type": "density",
                            "curve": { "type": "exponential", "dmax": { "explicit": 1.6 } } },
                         "film_base": { "source": { "explicit": [0.9, 0.55, 0.42] } } } }"#,
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        stale.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "the recipe still applies:\n{err}");
    let report = json(&stdout);
    assert!(
        report["warnings"]
            .to_string()
            .contains("pipeline_version 9999"),
        "the roll-level warnings must carry the skew: {report}"
    );

    // And `--strict` promotes it, after the report lands.
    let (code, stdout, _) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        tmp.path("strict").to_str().unwrap(),
        "--params",
        stale.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(
        code, 1,
        "--strict must promote the roll's version-skew warning"
    );
    assert_eq!(json(&stdout)["command"], "roll", "the report still lands");
}

#[test]
fn replaying_another_pipeline_versions_recipe_warns_and_strict_promotes_it() {
    // A recipe captured under a different behavioral pipeline_version still
    // applies, but its default render has changed underneath it — the loud,
    // `--strict`-promotable warning is the whole point of the version label.
    let tmp = TempDir::new("version-skew");
    let stale = write_file(
        &tmp.path("stale.json"),
        r#"{ "meta": { "pipeline_version": 9999 },
             "params": { "film_base": { "source": { "explicit": [0.9, 0.55, 0.42] } } } }"#,
    );
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        stale.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "the recipe still applies:\n{err}");
    let warnings = json(&stdout)["warnings"].to_string();
    assert!(
        warnings.contains("pipeline_version 9999"),
        "the version skew must be reported: {warnings}"
    );

    // Same recipe under --strict ⇒ non-zero exit, report still emitted.
    let (code, stdout, _) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("strict.tiff").to_str().unwrap(),
        "--params",
        stale.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(code, 1, "--strict must promote the version-skew warning");
    assert_eq!(
        json(&stdout)["command"],
        "convert",
        "the report still lands"
    );

    // A recipe recording THIS build's version does not warn.
    let current = json(
        &run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            tmp.path("cur.tiff").to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
        ])
        .1,
    )["identity"]["pipeline_version"]
        .as_u64()
        .unwrap();
    let matching = write_file(
        &tmp.path("matching.json"),
        &format!(
            r#"{{ "meta": {{ "pipeline_version": {current} }},
                  "params": {{ "film_base": {{ "source": {{ "explicit": [0.9, 0.55, 0.42] }} }} }} }}"#
        ),
    );
    // No `--strict` here: this HDRi fixture legitimately warns about its unconsumed
    // IR plane, so a strict exit would prove nothing about the version label. The
    // assertion is on the warning *text* — no version-skew warning appears.
    let (code, stdout, _) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("match.tiff").to_str().unwrap(),
        "--params",
        matching.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "a matching pipeline_version must convert cleanly");
    assert!(
        !json(&stdout)["warnings"]
            .to_string()
            .contains("pipeline_version"),
        "a matching pipeline_version must not warn"
    );
}

#[test]
fn identity_stamping_does_not_perturb_the_output_pixels() {
    // Identity is operational metadata in the same class as `--report`/telemetry:
    // it must never move a pixel. Drive the same conversion through every path that
    // touches the identity code — report on/off, a bare recipe, an enveloped
    // sidecar carrying a `meta` block, and a version-skew warning — and assert one
    // single set of TIFF bytes across all of them.
    let tmp = TempDir::new("no-perturb");
    let input = fixture("hdri-64bit.tif");
    let base = tmp.path("base.tiff");
    let dump = tmp.path("bare.json");
    let (code, _, err) = convert_default(
        &input,
        &base,
        &["--dump-params", dump.to_str().unwrap(), "--report", "none"],
    );
    assert_eq!(code, 0, "{err}");
    let expected = std::fs::read(&base).unwrap();

    let bare = std::fs::read_to_string(&dump).unwrap();
    let skewed = write_file(
        &tmp.path("skew.json"),
        &format!(r#"{{ "meta": {{ "pipeline_version": 9999 }}, "params": {bare} }}"#),
    );
    let envelope = sidecar_of(&base);
    let variants: [(&str, Vec<&str>); 4] = [
        ("report json", vec!["--params", dump.to_str().unwrap()]),
        (
            "report none",
            vec!["--params", dump.to_str().unwrap(), "--report", "none"],
        ),
        (
            "enveloped sidecar",
            vec!["--params", envelope.to_str().unwrap()],
        ),
        ("version skew", vec!["--params", skewed.to_str().unwrap()]),
    ];
    for (label, extra) in variants {
        let out = tmp.path(&format!("{}.tiff", label.replace(' ', "-")));
        let mut args = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(&extra);
        let (code, _, err) = run(&args);
        assert_eq!(code, 0, "{label}: {err}");
        assert_eq!(
            std::fs::read(&out).unwrap(),
            expected,
            "{label} must produce byte-identical pixels"
        );
    }
}

#[test]
fn roll_report_carries_the_shared_recipes_identity() {
    // A roll stamps identity once, for the SHARED frozen recipe; each frame's own
    // sidecar carries its own (possibly overridden) params_hash.
    let tmp = TempDir::new("roll-identity");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let id = &report["identity"];
    assert_eq!(id["nc_version"], env!("CARGO_PKG_VERSION"));
    assert!(id["pipeline_version"].as_u64().is_some(), "{id}");
    let shared_hash = id["params_hash"].as_str().expect("shared params_hash");
    // The frame ran with no override, so its sidecar advertises the same hash as the
    // roll's shared identity.
    assert_eq!(
        sidecar(&out_dir.join("hdr-48bit_positive.tiff"))["meta"]["params_hash"]
            .as_str()
            .unwrap(),
        shared_hash
    );
    // And it is the same hash a single `convert` from that recipe reports — the
    // roll/convert equivalence guarantee extended to config identity. (Asserted
    // against a real run rather than a re-serialized `Value`, whose key order serde
    // would sort.)
    let single = tmp.path("single.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        single.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        json(&stdout)["identity"]["params_hash"].as_str().unwrap(),
        shared_hash,
        "a roll frame and the equivalent single convert share one params_hash"
    );
}
