//! Embedded, opt-in performance + context telemetry for `nc convert`.
//!
//! After a real conversion succeeds, the orchestrator gathers a full metadata
//! record — image facts, per-stage timings, a compact conversion summary, and the
//! outcome — and emits it as JSON to a persistent append-only JSONL log and/or a
//! one-off file (design-spec §8/§9). The record is designed for a future
//! background uploader (the separate `telemetry-upload` task) to drain the JSONL
//! queue and ship to a server; this module only *produces* the record and writes
//! the local sink(s).
//!
//! Two deliberate design boundaries:
//!
//! - **Determinism (critical):** the record (timings, timestamp, system info)
//!   never enters the recipe sidecar and never changes the image bytes. Telemetry
//!   on or off, the output TIFF and sidecar JSON are byte-identical. This module
//!   only *reads* the finished conversion's facts.
//! - **Fail-soft (a documented deviation from the house fail-loudly rule):** a
//!   telemetry *write* failure must not fail a successful conversion. The image
//!   already succeeded, and telemetry is non-critical observability, so the
//!   orchestrator warns on stderr and continues (exit stays 0; `--strict` does
//!   not promote it). A telemetry-file that *collides* with a real output is the
//!   exception — that is a config error caught up front by the CLI, not a runtime
//!   write failure, and stays a loud usage error.
//!
//! The builder ([`build_record`]) is a pure function of its inputs: the caller
//! injects the wall-clock timestamp and CPU count (via [`RecordInputs`], the way
//! [`default_log_path`] injects the environment into the pure [`resolve_log_path`]),
//! and the crate version + target triple are compile-time constants baked into the
//! binary. The sink writers are the only I/O.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::io::decode::{DecodeInfo, SilverFastFormat};
use crate::types::{Algorithm, EncodeReport, FilmBaseSource};

/// Telemetry record schema version. Bump on any change to [`TelemetryRecord`]'s
/// shape so a server can ingest old and new records side by side. Note the record
/// embeds domain enums (`Algorithm`, `FilmBaseSource`, `SilverFastFormat`) whose
/// serde representation lives elsewhere — a change to *their* wire form is also a
/// schema change and must bump this too.
pub const SCHEMA_VERSION: u32 = 1;

/// Default local JSONL log path, honoring `NC_TELEMETRY_LOG` then the platform
/// data dir; `None` when no home/data dir can be located (the caller then warns
/// and skips the persistent sink, fail-soft). Reads the environment and defers
/// the precedence to the pure [`resolve_log_path`] (so the ordering is unit
/// testable without mutating process-global env vars).
pub fn default_log_path() -> Option<PathBuf> {
    // `APPDATA` is a Windows convention, so only consult it there; every other
    // platform falls through to the `HOME`/`.local/share` XDG default base.
    let appdata = if cfg!(windows) {
        non_empty_env("APPDATA")
    } else {
        None
    };
    resolve_log_path(
        non_empty_env("NC_TELEMETRY_LOG"),
        non_empty_env("XDG_DATA_HOME"),
        appdata,
        non_empty_env("HOME"),
    )
}

/// Pure log-path precedence (dependency-free, per the task's minimal-deps
/// preference), highest priority first:
/// 1. `NC_TELEMETRY_LOG` — explicit override (the full file path).
/// 2. `$XDG_DATA_HOME/nc/telemetry.jsonl`.
/// 3. `%APPDATA%\nc\telemetry.jsonl` (Windows; `None` on other platforms).
/// 4. `$HOME/.local/share/nc/telemetry.jsonl` — the XDG default base, and the
///    universal last-resort fallback on any platform with `HOME` set.
///
/// Returns `None` only when every source is absent.
fn resolve_log_path(
    explicit: Option<std::ffi::OsString>,
    xdg_data_home: Option<std::ffi::OsString>,
    appdata: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    if let Some(x) = xdg_data_home {
        return Some(PathBuf::from(x).join("nc").join("telemetry.jsonl"));
    }
    if let Some(a) = appdata {
        return Some(PathBuf::from(a).join("nc").join("telemetry.jsonl"));
    }
    Some(
        PathBuf::from(home?)
            .join(".local")
            .join("share")
            .join("nc")
            .join("telemetry.jsonl"),
    )
}

/// `std::env::var_os` filtered to reject an empty value (an empty env var is
/// treated as unset, matching the XDG spec's handling of `XDG_DATA_HOME`).
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Record schema (serialize-only — nothing deserializes a telemetry record here;
// the server owns ingestion)
// ---------------------------------------------------------------------------

/// One full telemetry record for a single `nc convert` run (design-spec §9).
///
/// Optional fields follow two wire conventions: an absent `cpu_count` /
/// `image.input_bytes` / `image.output_bytes` serializes as JSON `null` (the key
/// is always present, fixed shape), whereas `timing_ms.ir_export` and
/// `conversion.dmax` are `skip_serializing_if = "Option::is_none"` and vanish from
/// the JSON entirely when not applicable to the run.
#[derive(Clone, Debug, Serialize)]
pub struct TelemetryRecord {
    /// Record schema version ([`SCHEMA_VERSION`]) for server forward-compat.
    pub schema_version: u32,
    /// Wall-clock time the record was built, UNIX epoch milliseconds. The server
    /// formats it; carrying a raw integer avoids a date-crate dependency.
    pub timestamp_ms: u64,
    /// `nc` crate version (`CARGO_PKG_VERSION`).
    pub nc_version: &'static str,
    /// Compile target triple (captured by `build.rs` into `NC_TARGET`).
    pub target: &'static str,
    /// Available parallelism (`std::thread::available_parallelism`); `None` when
    /// the platform can't report it. Fixed-width for a stable wire shape.
    pub cpu_count: Option<u32>,
    pub image: ImageInfo,
    pub timing_ms: TimingInfo,
    pub conversion: ConversionInfo,
    pub outcome: OutcomeInfo,
}

/// Image facts (from the decoder plus the on-disk file sizes).
#[derive(Clone, Debug, Serialize)]
pub struct ImageInfo {
    /// SilverFast variant (`"hdr"` / `"hdri"`).
    pub format: SilverFastFormat,
    pub width: u32,
    pub height: u32,
    /// `width * height / 1e6`, for quick scale bucketing on the server.
    pub megapixels: f64,
    /// Bits per sample of the primary image (16 for accepted scans).
    pub bit_depth: u8,
    /// RGB channels in the primary image (3).
    pub channels: u16,
    pub ir_present: bool,
    /// Input scan size in bytes; `None` if it couldn't be stat'd.
    pub input_bytes: Option<u64>,
    /// Written output TIFF size in bytes; `None` if it couldn't be stat'd.
    pub output_bytes: Option<u64>,
}

/// Per-stage wall-clock timings in milliseconds. `total` is the whole
/// orchestrated run up to the sidecar write (the clock stops before the report is
/// emitted); the per-stage values sum to less than it (the remainder is recipe
/// merge, validation, and the sidecar write).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TimingInfo {
    pub total: f64,
    pub decode: f64,
    pub film_base: f64,
    pub algorithm: f64,
    pub color: f64,
    pub encode: f64,
    /// IR-export time, present only when `--export-ir` ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir_export: Option<f64>,
}

/// Compact conversion summary. A full `params_hash` (over the effective recipe
/// JSON) lets the server dedup / group by exact parameters without the record
/// carrying the whole recipe; a few high-signal knobs ride alongside it.
#[derive(Clone, Debug, Serialize)]
pub struct ConversionInfo {
    pub algorithm: Algorithm,
    /// Stable 64-bit hash (hex) of the effective recipe JSON — the same bytes
    /// written to the sidecar, so identical conversions share a hash.
    pub params_hash: String,
    /// Film-base provenance (`"auto"` / `{"region":…}` / `{"explicit":…}`).
    pub film_base_source: FilmBaseSource,
    /// Resolved display-white anchor density the density render used, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmax: Option<f32>,
    /// Whether HDR (32-bit float) output was written.
    pub output_hdr: bool,
}

/// Run outcome — the quality signals a server watches for regressions. Today a
/// record is emitted only after a conversion succeeds, so there is no explicit
/// `success` flag: a constant `true` would carry no information and could even
/// contradict `non_finite > 0`. A `success`/`status` field returns with the
/// failure-path record in the `telemetry-strategy`/`telemetry-upload` follow-up,
/// where it actually varies.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct OutcomeInfo {
    /// Number of report warnings raised (clipping, IR-ignored, BigTIFF promote…).
    /// Fixed-width for a stable wire shape.
    pub warnings: u32,
    /// Finite samples clamped at a range end (`EncodeReport::clipped_total`).
    pub clipped: u64,
    /// Non-finite (`NaN`/`±inf`) output samples — a numerical fault signal.
    pub non_finite: u64,
}

/// Everything the orchestrator hands the pure [`build_record`] builder. Grouped
/// into one struct so the builder signature stays readable and the call site
/// names each field.
pub struct RecordInputs<'a> {
    pub info: &'a DecodeInfo,
    /// Wall-clock time the record is built, UNIX epoch milliseconds. Injected by
    /// the orchestrator (see [`now_unix_millis`]) so the builder stays pure.
    pub timestamp_ms: u64,
    /// Available parallelism, injected by the orchestrator (see [`cpu_count`]);
    /// `None` when the platform can't report it.
    pub cpu_count: Option<u32>,
    pub timings: TimingInfo,
    pub loss: EncodeReport,
    pub input_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub algorithm: Algorithm,
    pub params_hash: String,
    pub film_base_source: FilmBaseSource,
    pub dmax: Option<f32>,
    pub output_hdr: bool,
    pub warnings: usize,
}

/// Build a full [`TelemetryRecord`] from the finished conversion's facts. A pure
/// function of `inputs`: the timestamp and CPU count are injected by the caller,
/// and the crate version + target are compile-time constants — nothing here reads
/// ambient state or touches the image/sidecar.
pub fn build_record(inputs: RecordInputs<'_>) -> TelemetryRecord {
    let info = inputs.info;
    let megapixels = (info.width as f64 * info.height as f64) / 1_000_000.0;
    TelemetryRecord {
        schema_version: SCHEMA_VERSION,
        timestamp_ms: inputs.timestamp_ms,
        nc_version: env!("CARGO_PKG_VERSION"),
        target: env!("NC_TARGET"),
        cpu_count: inputs.cpu_count,
        image: ImageInfo {
            format: info.format,
            width: info.width,
            height: info.height,
            megapixels,
            bit_depth: info.bits_per_sample,
            channels: info.channels,
            ir_present: info.ir_present,
            input_bytes: inputs.input_bytes,
            output_bytes: inputs.output_bytes,
        },
        timing_ms: inputs.timings,
        conversion: ConversionInfo {
            algorithm: inputs.algorithm,
            params_hash: inputs.params_hash,
            film_base_source: inputs.film_base_source,
            dmax: inputs.dmax,
            output_hdr: inputs.output_hdr,
        },
        outcome: OutcomeInfo {
            warnings: warning_count(inputs.warnings),
            clipped: inputs.loss.clipped_total(),
            non_finite: inputs.loss.non_finite,
        },
    }
}

/// Saturate a warning count into the fixed-width wire field (a run never
/// realistically raises `u32::MAX` warnings).
fn warning_count(n: usize) -> u32 {
    n.min(u32::MAX as usize) as u32
}

/// UNIX epoch milliseconds now. Clamps a pre-epoch clock (`duration_since` errs
/// only if the system clock is before 1970) to 0 rather than failing — telemetry
/// is best-effort and a bogus clock must not abort the record. The orchestrator
/// reads this and hands it to [`build_record`] via [`RecordInputs`], keeping the
/// builder pure.
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Available parallelism as the record's fixed-width `cpu_count`; `None` when the
/// platform can't report it. Like [`now_unix_millis`], the orchestrator reads this
/// and injects it so [`build_record`] performs no ambient reads.
pub fn cpu_count() -> Option<u32> {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .ok()
}

/// Stable 64-bit FNV-1a hash of `bytes`, hex-formatted. Hand-rolled (not
/// `std::hash::DefaultHasher`, whose output isn't guaranteed stable across
/// toolchains) so the `params_hash` a server sees is reproducible build to build.
pub fn params_hash(recipe_json: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in recipe_json.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Sinks (the only I/O)
// ---------------------------------------------------------------------------

/// Append one compact JSON line to the persistent JSONL log, creating parent
/// directories and the file (create-append) as needed. One object per line so an
/// uploader can drain the queue line by line.
///
/// The record — body *and* its trailing newline — is assembled into one buffer and
/// emitted with a single [`write_all`](Write::write_all) to a file opened
/// `O_APPEND` (`append(true)`). A file opened `O_APPEND` seeks-to-end and writes
/// as one atomic step on a local POSIX filesystem, so two concurrent
/// `nc convert --telemetry` sharing one log can't interleave one record's body
/// with another's newline and corrupt the one-object-per-line JSONL the uploader
/// drains. `writeln!` would split the body and the newline into separate `write`
/// calls — two independent atomic appends another writer could slip between —
/// forfeiting that guarantee. (This is the `O_APPEND` offset-then-write atomicity
/// guarantee, distinct from the `PIPE_BUF` bound that governs *pipe* writes.)
pub fn append_jsonl(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(buf.as_bytes())
}

/// Write the record as a single line to a one-off file (overwrite), creating
/// parent directories as needed.
pub fn write_oneoff(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut contents = String::with_capacity(line.len() + 1);
    contents.push_str(line);
    contents.push('\n');
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> DecodeInfo {
        DecodeInfo {
            format: SilverFastFormat::Hdri,
            width: 2000,
            height: 3000,
            channels: 3,
            bits_per_sample: 16,
            ir_present: true,
            make: None,
            model: None,
            software: None,
            warnings: vec![],
        }
    }

    fn sample_timings() -> TimingInfo {
        TimingInfo {
            total: 100.0,
            decode: 40.0,
            film_base: 5.0,
            algorithm: 20.0,
            color: 15.0,
            encode: 18.0,
            ir_export: Some(2.0),
        }
    }

    #[test]
    fn build_record_derives_image_fields() {
        let info = sample_info();
        let rec = build_record(RecordInputs {
            info: &info,
            timestamp_ms: 1_752_566_400_000,
            cpu_count: Some(14),
            timings: sample_timings(),
            loss: EncodeReport {
                total_samples: 100,
                clipped_low: 1,
                clipped_high: 2,
                non_finite: 7,
            },
            input_bytes: Some(12_345),
            output_bytes: Some(67_890),
            algorithm: Algorithm::Density,
            params_hash: "deadbeef".into(),
            film_base_source: FilmBaseSource::Auto,
            dmax: Some(1.8),
            output_hdr: false,
            warnings: 4,
        });

        assert_eq!(rec.schema_version, 1);
        assert_eq!(rec.image.width, 2000);
        assert_eq!(rec.image.height, 3000);
        // 2000 * 3000 = 6e6 pixels → 6.0 MP.
        assert_eq!(rec.image.megapixels, 6.0);
        assert_eq!(rec.image.channels, 3);
        assert_eq!(rec.image.bit_depth, 16);
        assert!(rec.image.ir_present);
        assert_eq!(rec.image.input_bytes, Some(12_345));
        assert_eq!(rec.image.output_bytes, Some(67_890));
        assert_eq!(rec.outcome.clipped, 3); // clipped_low + clipped_high
        assert_eq!(rec.outcome.non_finite, 7);
        assert_eq!(rec.outcome.warnings, 4);
        // Injected ambient values are echoed through verbatim (builder is pure).
        assert_eq!(rec.timestamp_ms, 1_752_566_400_000);
        assert_eq!(rec.cpu_count, Some(14));
        // Compile-time build identity.
        assert_eq!(rec.nc_version, env!("CARGO_PKG_VERSION"));
        assert!(!rec.target.is_empty());
    }

    #[test]
    fn missing_ir_has_no_ir_export_timing() {
        let mut info = sample_info();
        info.format = SilverFastFormat::Hdr;
        info.ir_present = false;
        let mut timings = sample_timings();
        timings.ir_export = None;
        let rec = build_record(RecordInputs {
            info: &info,
            timestamp_ms: 1,
            cpu_count: None,
            timings,
            loss: EncodeReport::default(),
            input_bytes: None,
            output_bytes: None,
            algorithm: Algorithm::Simple,
            params_hash: "0".into(),
            film_base_source: FilmBaseSource::Explicit([0.9, 0.5, 0.4]),
            dmax: None,
            output_hdr: true,
            warnings: 0,
        });
        assert!(!rec.image.ir_present);
        assert!(rec.timing_ms.ir_export.is_none());
        // A serialized record omits the absent optional fields entirely.
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("ir_export"),
            "absent IR export must be omitted"
        );
        assert!(!json.contains("\"dmax\""), "absent dmax must be omitted");
    }

    #[test]
    fn resolve_log_path_precedence() {
        use std::ffi::OsString;
        let os = |s: &str| Some(OsString::from(s));

        // Explicit override wins over everything.
        assert_eq!(
            resolve_log_path(os("/x/tel.jsonl"), os("/xdg"), os("C:/app"), os("/home")),
            Some(PathBuf::from("/x/tel.jsonl"))
        );
        // Then XDG_DATA_HOME.
        assert_eq!(
            resolve_log_path(None, os("/xdg"), os("C:/app"), os("/home")),
            Some(PathBuf::from("/xdg/nc/telemetry.jsonl"))
        );
        // Then APPDATA (the Windows tier) before the HOME fallback.
        assert_eq!(
            resolve_log_path(None, None, os("C:/app"), os("/home")),
            Some(PathBuf::from("C:/app/nc/telemetry.jsonl"))
        );
        // Then the HOME/.local/share default base (universal last resort).
        assert_eq!(
            resolve_log_path(None, None, None, os("/home")),
            Some(PathBuf::from("/home/.local/share/nc/telemetry.jsonl"))
        );
        // Nothing set → no path (caller warns and skips the persistent sink).
        assert_eq!(resolve_log_path(None, None, None, None), None);
    }

    #[test]
    fn params_hash_is_stable_and_input_sensitive() {
        let a = params_hash(r#"{"algorithm":"density"}"#);
        let b = params_hash(r#"{"algorithm":"density"}"#);
        let c = params_hash(r#"{"algorithm":"simple"}"#);
        assert_eq!(a, b, "same input → same hash");
        assert_ne!(a, c, "different input → different hash");
        assert_eq!(a.len(), 16, "hex-formatted 64-bit hash");
    }

    #[test]
    fn append_jsonl_appends_one_line_per_call() {
        let dir = std::env::temp_dir().join(format!("nc-tel-{}", std::process::id()));
        let path = dir.join("telemetry.jsonl");
        let _ = fs::remove_dir_all(&dir);
        append_jsonl(&path, r#"{"a":1}"#).unwrap();
        append_jsonl(&path, r#"{"a":2}"#).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines, vec![r#"{"a":1}"#, r#"{"a":2}"#]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_jsonl_is_atomic_under_concurrency() {
        // Many threads, each opening its own O_APPEND handle per call (as separate
        // processes would), hammer one log. Every line must survive intact — no
        // interleaved body/newline — so the readback has exactly N well-formed
        // one-object-per-line records. This exercises the single-`write_all`
        // atomicity the JSONL contract depends on.
        //
        // Payloads are padded past a filesystem page (> 4 KiB) so the pre-fix
        // two-write shape (`writeln!` = body then a separate `\n`) would leave a
        // wide interleave window and actually corrupt a line — a ~30-byte record
        // is too small to reliably fail the buggy version this guards against.
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("nc-tel-atomic-{}", std::process::id()));
        let path = Arc::new(dir.join("telemetry.jsonl"));
        let _ = fs::remove_dir_all(&dir);

        const THREADS: usize = 8;
        const PER_THREAD: usize = 100;
        const PAD: usize = 6000; // comfortably past a 4 KiB page
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        // Distinct payload per write, padded past a page. The pad
                        // char differs per thread so a cross-thread splice is
                        // visible as a JSON parse failure below.
                        let pad = std::iter::repeat_n(char::from(b'a' + t as u8), PAD)
                            .collect::<String>();
                        let line = format!(r#"{{"thread":{t},"seq":{i},"pad":"{pad}"}}"#);
                        append_jsonl(&path, &line).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let contents = fs::read_to_string(&*path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), THREADS * PER_THREAD, "one line per append");
        for line in lines {
            // A torn write would leave a line that isn't a standalone JSON object.
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("corrupt JSONL line (len {}): {e}", line.len()));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_wire_shape_is_pinned() {
        // Snapshot the exact serialized JSON for a fully-populated record and a
        // minimal one. This catches silent wire-shape drift — a renamed/added/
        // removed field, a reordered struct, or a changed foreign-enum
        // representation (`Algorithm`/`FilmBaseSource`/`SilverFastFormat`) — any of
        // which is a `SCHEMA_VERSION` bump. If this test fails, update the snapshot
        // *and* bump `SCHEMA_VERSION` (and the design-spec / SKILL examples).
        // `nc_version`/`target` are set to fixed literals here so the snapshot is
        // build- and platform-independent.
        let full = TelemetryRecord {
            schema_version: SCHEMA_VERSION,
            timestamp_ms: 1_700_000_000_000,
            nc_version: "9.9.9",
            target: "test-triple",
            cpu_count: Some(8),
            image: ImageInfo {
                format: SilverFastFormat::Hdri,
                width: 100,
                height: 200,
                megapixels: 0.25,
                bit_depth: 16,
                channels: 3,
                ir_present: true,
                input_bytes: Some(1000),
                output_bytes: Some(2000),
            },
            timing_ms: TimingInfo {
                total: 30.0,
                decode: 5.0,
                film_base: 1.0,
                algorithm: 10.0,
                color: 8.0,
                encode: 4.0,
                ir_export: Some(2.0),
            },
            conversion: ConversionInfo {
                algorithm: Algorithm::Density,
                params_hash: "0123456789abcdef".into(),
                film_base_source: FilmBaseSource::Explicit([0.5, 0.25, 0.125]),
                dmax: Some(1.5),
                output_hdr: false,
            },
            outcome: OutcomeInfo {
                warnings: 1,
                clipped: 2,
                non_finite: 0,
            },
        };
        let expected_full = concat!(
            r#"{"schema_version":1,"timestamp_ms":1700000000000,"nc_version":"9.9.9","#,
            r#""target":"test-triple","cpu_count":8,"#,
            r#""image":{"format":"hdri","width":100,"height":200,"megapixels":0.25,"#,
            r#""bit_depth":16,"channels":3,"ir_present":true,"input_bytes":1000,"#,
            r#""output_bytes":2000},"#,
            r#""timing_ms":{"total":30.0,"decode":5.0,"film_base":1.0,"algorithm":10.0,"#,
            r#""color":8.0,"encode":4.0,"ir_export":2.0},"#,
            r#""conversion":{"algorithm":"density","params_hash":"0123456789abcdef","#,
            r#""film_base_source":{"explicit":[0.5,0.25,0.125]},"dmax":1.5,"output_hdr":false},"#,
            r#""outcome":{"warnings":1,"clipped":2,"non_finite":0}}"#,
        );
        assert_eq!(serde_json::to_string(&full).unwrap(), expected_full);

        // Minimal: cpu_count / input_bytes / output_bytes serialize as null;
        // ir_export and dmax are skipped entirely.
        let minimal = TelemetryRecord {
            schema_version: SCHEMA_VERSION,
            timestamp_ms: 0,
            nc_version: "9.9.9",
            target: "test-triple",
            cpu_count: None,
            image: ImageInfo {
                format: SilverFastFormat::Hdr,
                width: 1,
                height: 1,
                megapixels: 0.0,
                bit_depth: 16,
                channels: 3,
                ir_present: false,
                input_bytes: None,
                output_bytes: None,
            },
            timing_ms: TimingInfo {
                total: 0.0,
                decode: 0.0,
                film_base: 0.0,
                algorithm: 0.0,
                color: 0.0,
                encode: 0.0,
                ir_export: None,
            },
            conversion: ConversionInfo {
                algorithm: Algorithm::Simple,
                params_hash: "0".into(),
                film_base_source: FilmBaseSource::Auto,
                dmax: None,
                output_hdr: false,
            },
            outcome: OutcomeInfo {
                warnings: 0,
                clipped: 0,
                non_finite: 0,
            },
        };
        let expected_minimal = concat!(
            r#"{"schema_version":1,"timestamp_ms":0,"nc_version":"9.9.9","#,
            r#""target":"test-triple","cpu_count":null,"#,
            r#""image":{"format":"hdr","width":1,"height":1,"megapixels":0.0,"#,
            r#""bit_depth":16,"channels":3,"ir_present":false,"input_bytes":null,"#,
            r#""output_bytes":null},"#,
            r#""timing_ms":{"total":0.0,"decode":0.0,"film_base":0.0,"algorithm":0.0,"#,
            r#""color":0.0,"encode":0.0},"#,
            r#""conversion":{"algorithm":"simple","params_hash":"0","#,
            r#""film_base_source":"auto","output_hdr":false},"#,
            r#""outcome":{"warnings":0,"clipped":0,"non_finite":0}}"#,
        );
        assert_eq!(serde_json::to_string(&minimal).unwrap(), expected_minimal);
    }

    #[test]
    fn write_oneoff_overwrites() {
        let dir = std::env::temp_dir().join(format!("nc-tel-oneoff-{}", std::process::id()));
        let path = dir.join("run.json");
        let _ = fs::remove_dir_all(&dir);
        write_oneoff(&path, r#"{"a":1}"#).unwrap();
        write_oneoff(&path, r#"{"a":2}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":2}\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
