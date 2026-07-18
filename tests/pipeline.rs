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

/// The binary under test, provided by Cargo for integration tests.
const NC: &str = env!("CARGO_BIN_EXE_nc");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
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
        "--algorithm",
        "simple",
        // Real scans are holder → rebate → picture, so auto-base fails loudly;
        // supply an explicit base (the documented calibrate-once workflow).
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "simple convert should succeed");
    assert!(is_tiff(&out), "output must be a valid TIFF");
    // Effective-recipe sidecar next to the output, valid JSON.
    let sidecar = PathBuf::from(format!("{}.json", out.display()));
    let recipe: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(recipe["algorithm"], "simple");

    let report = json(&stdout);
    assert_eq!(report["command"], "convert");
    assert_eq!(report["algorithm"], "simple");
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
        "--algorithm",
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
            "--algorithm",
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
        "--algorithm",
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
        "--algorithm",
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
    // clip_low >= clip_high is rejected at the CLI boundary (exit 2).
    let (code, _stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--algorithm",
        "simple",
        "--clip-low",
        "0.9",
        "--clip-high",
        "0.1",
    ]);
    assert_eq!(code, 2, "invalid params must exit 2");
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
            "--algorithm".to_string(),
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
        "--algorithm",
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
        "--algorithm",
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
    let sidecar = format!("{}.json", out_a.display());
    // The sidecar carries the sigmoid section verbatim.
    let recipe: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(recipe["algorithm"], "sigmoid");
    assert_eq!(recipe["sigmoid"]["contrast"], 1.4);
    assert_eq!(recipe["sigmoid"]["toe"], 0.12);
    assert_eq!(recipe["sigmoid"]["shoulder"], 0.33);

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
        "--algorithm",
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
        "--algorithm",
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
    assert!(
        !stdout.contains("decoded"),
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
        "--algorithm",
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
    // `input.color = profile` is parsed but input-side CM isn't implemented in
    // Step 1 — it must fail loudly (exit 4), not silently ignore the profile.
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
    assert!(err.contains("not implemented"), "stderr: {err}");
    assert!(!out.exists());
}

// --- telemetry (opt-in performance + context record) -------------------------

#[test]
fn telemetry_file_writes_full_record() {
    // `--telemetry-file <path>` writes one valid JSON record with every schema
    // field populated (schema_version=1, finite timings, correct dims/bytes).
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

    assert_eq!(record["schema_version"], 1);
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
    assert_eq!(conv["algorithm"], "density");
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
        assert_eq!(v["schema_version"], 1);
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
            "--algorithm".to_string(),
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
        "--algorithm",
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
    assert_eq!(record["schema_version"], 1);
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
        "--algorithm",
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
        "--algorithm",
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
    std::fs::write(&recipe, r#"{"algorithm":"density","telemetry":true}"#).unwrap();
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
fn telemetry_records_sigmoid_algorithm_and_params_hash() {
    // The record's conversion summary must handle `--algorithm sigmoid`: the
    // Algorithm enum serializes "sigmoid", and params_hash (over the effective
    // recipe JSON) must cover the sigmoid.* keys, so tweaking one changes the hash.
    let tmp = TempDir::new("tel-sigmoid");
    let fix = fixture("hdr-48bit.tif");
    let convert = |out: &Path, extra: &[&str]| -> serde_json::Value {
        let out = out.to_str().unwrap();
        let mut argv = vec![
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out,
            "--algorithm",
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
        ra["conversion"]["algorithm"], "sigmoid",
        "the record must name the sigmoid algorithm: {ra}"
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
    // `--algorithm sigmoid` selects the S-curve converter end to end: the JSON
    // report names the algorithm, carries the resolved Dmax anchor, and the
    // sidecar recipe round-trips the sigmoid section.
    let tmp = TempDir::new("sigmoid");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--algorithm",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--sigmoid-contrast",
        "1.2",
    ]);
    assert_eq!(code, 0, "sigmoid convert should succeed: {err}");
    assert!(is_tiff(&out));
    let report = json(&stdout);
    assert_eq!(report["algorithm"], "sigmoid");
    assert!(
        report["dmax"].as_f64().is_some_and(f64::is_finite),
        "the shared anchor must be reported: {report}"
    );
    let sidecar: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{}.json", out.display())).unwrap())
            .unwrap();
    assert_eq!(sidecar["algorithm"], "sigmoid");
    assert_eq!(sidecar["sigmoid"]["contrast"], 1.2);

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
        "--algorithm",
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
fn sigmoid_warns_when_density_gamma_is_ignored() {
    // `--algorithm sigmoid` consumes the density section except density_gamma
    // (the S-curve replaces the straight line it parameterizes), so a customized
    // gamma is a silent no-op unless surfaced — run_convert warns, and --strict
    // promotes that warning to exit 1. The warning must NOT fire at the default
    // gamma, nor under --algorithm density (where gamma is consumed).
    let tmp = TempDir::new("gamma-warn");
    let gamma_warns = |algo: &str, gamma: &str, out: &Path| -> serde_json::Value {
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            algo,
            "--film-base",
            "0.9,0.55,0.42",
            "--density-gamma",
            gamma,
        ]);
        assert_eq!(code, 0, "{err}");
        json(&stdout)
    };
    // `warnings` is omitted from the report when empty (skip_serializing_if), so
    // a missing array counts as "no warnings".
    let has_gamma_warning = |r: &serde_json::Value| {
        r["warnings"].as_array().is_some_and(|ws| {
            ws.iter()
                .any(|w| w.as_str().unwrap().contains("ignores --density-gamma"))
        })
    };

    // sigmoid + custom gamma → warns.
    let r = gamma_warns("sigmoid", "1.5", &tmp.path("a.tiff"));
    assert!(
        has_gamma_warning(&r),
        "sigmoid must warn on custom gamma: {r}"
    );
    // sigmoid + default gamma (1.0) → no warning.
    let r = gamma_warns("sigmoid", "1.0", &tmp.path("b.tiff"));
    assert!(
        !has_gamma_warning(&r),
        "no warning at the default gamma: {r}"
    );
    // density + custom gamma → no warning (gamma is consumed there).
    let r = gamma_warns("density", "1.5", &tmp.path("c.tiff"));
    assert!(
        !has_gamma_warning(&r),
        "density consumes gamma, must not warn: {r}"
    );

    // --strict promotes the sigmoid warning to a non-zero exit.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("d.tiff").to_str().unwrap(),
        "--algorithm",
        "sigmoid",
        "--film-base",
        "0.9,0.55,0.42",
        "--density-gamma",
        "1.5",
        "--strict",
    ]);
    assert_eq!(
        code, 1,
        "--strict must fail on the gamma-ignored warning: {err}"
    );
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
        "--algorithm",
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
