//! End-to-end pipeline tests — drive the compiled `nc` binary against the
//! committed real-scan fixtures (`tests/fixtures/`) and assert on exit codes,
//! the JSON report on stdout, and the files written. This exercises the full
//! decode → film-base → algorithm → color → encode path that the unit tests
//! (which stop at module boundaries) can't.
//!
//! stdout must stay pure JSON (the agent contract), so every test parses it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use tiff::encoder::{TiffEncoder, colortype};
use ultrahdr_sys as uhdr;

/// The binary under test, provided by Cargo for integration tests.
const NC: &str = env!("CARGO_BIN_EXE_hanten");

/// A committed fixture by file name.
///
/// `hdri-64bit.tif` carries an IR plane, so every conversion of it without
/// `--export-ir` warns that the plane is "preserved but not used", and a `--strict` run
/// then fails whatever else it tests. To prove a *specific* warning is strict-promotable,
/// use the IR-free `hdr-48bit.tif` (or pass `--export-ir` when the test needs the plane)
/// and add a no-override control run so the assertion is falsifiable.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Synthesize a uniform 16-bit RGB TIFF (the `RGB(16)` chunky layout the decoder
/// accepts) with every pixel set to `rgb`, at `path`. Encodes into memory then writes the whole buffer,
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

fn write_rgb48_pixels(path: &Path, width: u32, height: u32, rgb: &[[u16; 3]]) {
    use tiff::tags::Tag;
    assert_eq!(rgb.len(), (width * height) as usize);
    let data = rgb.iter().flatten().copied().collect::<Vec<_>>();
    let xmp = silverfast_xmp(XMP_NEG);
    let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
    let mut image = enc.new_image::<colortype::RGB16>(width, height).unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(700), xmp.as_bytes())
        .unwrap();
    image.write_data(&data).unwrap();
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

/// Write an HDRi-shaped TIFF: an RGB16 image plus a **marker-verified** Gray16 IR
/// page (`NewSubfileType = 4`, the provenance the holder detector requires),
/// filled with one uniform IR level. Big enough that the rebate scan window and
/// the holder probe band are both real bands.
fn write_hdri_with_uniform_ir(path: &Path, w: u32, h: u32, rgb: [u16; 3], ir: u16) {
    let mut pixels = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..(w * h) {
        pixels.extend_from_slice(&rgb);
    }
    write_hdri(path, w, h, &pixels, &vec![ir; (w * h) as usize]);
}

/// Write an HDRi-shaped TIFF from explicit RGB and IR buffers.
fn write_hdri(path: &Path, w: u32, h: u32, pixels: &[u16], plane: &[u16]) {
    use tiff::tags::Tag;
    assert_eq!(pixels.len(), (w * h * 3) as usize);
    assert_eq!(plane.len(), (w * h) as usize);
    let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
    let image = enc.new_image::<colortype::RGB16>(w, h).unwrap();
    image.write_data(pixels).unwrap();

    let mut ir_page = enc.new_image::<colortype::Gray16>(w, h).unwrap();
    ir_page
        .encoder()
        .write_tag(Tag::NewSubfileType, 4u32)
        .unwrap();
    ir_page.write_data(plane).unwrap();
}

/// An HDRi scan with the real `dark holder -> thin inset rebate -> picture`
/// geometry auto-base detection looks for: a 4 px opaque holder ring, a 6 px
/// unexposed rebate band inset behind it on the bottom and left, and a varied
/// picture interior. `ir_interior` sets the IR transmission of the film itself,
/// which is what the usability verdict measures. `ir_dark_all_edges` puts the
/// IR-dark holder on all four edges (the all-holder case, where the mask would
/// leave the rebate search nothing) rather than only where it really sits.
fn write_hdri_scan_with_rebate(path: &Path, ir_interior: u16, ir_dark_all_edges: bool) {
    const W: u32 = 200;
    const H: u32 = 200;
    const HOLDER: [u16; 3] = [655, 655, 655]; // ~0.01 transmission
    const REBATE: [u16; 3] = [34734, 17040, 10486]; // 0.53 / 0.26 / 0.16
    const IR_HOLDER: u16 = 1300; // ~0.02, as real holders measure

    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![ir_interior; (W * H) as usize];
    let put = |buf: &mut Vec<u16>, x: u32, y: u32, v: [u16; 3]| {
        let i = ((y * W + x) * 3) as usize;
        buf[i..i + 3].copy_from_slice(&v);
    };
    for y in 0..H {
        for x in 0..W {
            // Picture: a varied gradient, dimmer than the rebate on every channel
            // so the brightness gate can tell them apart.
            let t = (x + y) as f32 / (W + H) as f32;
            let px = [
                (3300.0 + 13000.0 * t) as u16,
                (2000.0 + 7000.0 * t) as u16,
                (1300.0 + 3300.0 * t) as u16,
            ];
            put(&mut rgb, x, y, px);
        }
    }
    // Rebate band, inset behind the holder on the bottom and left edges. Rippled
    // along the edge so a percentile has something to choose between.
    for x in 0..W {
        for y in H - 10..H - 4 {
            let f = 0.93 + 0.07 * (x % 10) as f32 / 9.0;
            put(&mut rgb, x, y, REBATE.map(|c| (c as f32 * f) as u16));
        }
    }
    for y in 0..H {
        for x in 4..10 {
            let f = 0.93 + 0.07 * (y % 10) as f32 / 9.0;
            put(&mut rgb, x, y, REBATE.map(|c| (c as f32 * f) as u16));
        }
    }
    // A dark RGB border on all four edges — but IR-dark on only the top and right,
    // where the opaque holder actually sits. The bottom and left border is dense
    // *film* (dark in RGB, transparent in IR) in front of the rebate: the
    // disambiguation RGB alone cannot make, and the reason a full IR-dark ring
    // would leave the search nothing to look at.
    for y in 0..H {
        for x in 0..W {
            if x < 4 || y < 4 || x >= W - 4 || y >= H - 4 {
                put(&mut rgb, x, y, HOLDER);
                if ir_dark_all_edges || y < 4 || x >= W - 4 {
                    ir[(y * W + x) as usize] = IR_HOLDER;
                }
            }
        }
    }
    write_hdri(path, W, H, &rgb, &ir);
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
///
/// Passes `args` through verbatim. A test that writes a TIFF states its output preset
/// itself (`display-p3` for a 16-bit TIFF, `film-master` for f32): this helper used to
/// inject `--output-preset legacy` into any preset-less `.tif` convert, which silently
/// turned a test meant for the default path into a test of another one. That preset
/// retired with `nf-retire/legacy-custom`, and the injection with it.
fn run(args: &[&str]) -> (i32, String, String) {
    spawn(args, &[])
}

/// Like [`run`], but with extra environment variables set for the child (used to
/// point `NC_TELEMETRY_LOG` at a temp file so telemetry tests never touch the
/// real user data dir).
fn run_env(args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    spawn(args, envs)
}

/// Spawn `nc` verbatim — the one place that runs the binary.
fn spawn(args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
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

/// The container a written file's *bytes* actually are, sniffed from its magic.
///
/// `cli::container_for` decides the container an output path is **named** for; a
/// separate exhaustive preset match in `convert_frame` decides which encoder
/// writes it. Both are exhaustive, so a new preset fails to compile in both — but
/// nothing makes them *agree*, and a preset named `.tiff` while dispatched to the
/// AVIF encoder would compile and ship a misnamed file. This is what pins the two
/// together, at the only level that matters: the bytes on disk.
fn sniff_container(path: &Path) -> &'static str {
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes.len() > 12, "{} is too short to sniff", path.display());
    if bytes[0..2] == [0xff, 0xd8] {
        "jpeg"
    } else if (&bytes[0..2] == b"II" || &bytes[0..2] == b"MM")
        && matches!(
            u16::from_le_bytes([bytes[2], bytes[3]]),
            42 | 43 | 0x2a00 | 0x2b00
        )
    {
        "tiff"
    } else if &bytes[4..8] == b"ftyp" {
        // An ISOBMFF file; AVIF says so in the major brand or the compatible list.
        let end = bytes.len().min(64);
        assert!(
            bytes[8..end].windows(4).any(|b| b == b"avif"),
            "{}: ftyp box names no avif brand",
            path.display()
        );
        "avif"
    } else {
        panic!(
            "{}: unrecognised container magic {:?}",
            path.display(),
            &bytes[0..12]
        );
    }
}

/// A file that starts with the little-endian TIFF magic ("II", 42 or 43).
fn is_tiff(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    bytes.len() > 4
        && &bytes[0..2] == b"II"
        && matches!(u16::from_le_bytes([bytes[2], bytes[3]]), 42 | 43)
}

fn primary_jpeg_icc(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    let mut chunks = Vec::new();
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        assert_eq!(bytes[offset], 0xff);
        let marker = bytes[offset + 1];
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let payload = &bytes[offset + 4..offset + 2 + length];
        if marker == 0xe2 && payload.starts_with(b"ICC_PROFILE\0") {
            chunks.push((payload[12], payload[13], payload[14..].to_vec()));
        }
        offset += 2 + length;
    }
    assert!(!chunks.is_empty(), "primary JPEG has no ICC APP2 chunks");
    chunks.sort_by_key(|chunk| chunk.0);
    let total = chunks[0].1;
    assert_eq!(chunks.len(), usize::from(total));
    assert!(
        chunks
            .iter()
            .enumerate()
            .all(
                |(index, (sequence, count, _))| *sequence as usize == index + 1 && *count == total
            )
    );
    chunks
        .into_iter()
        .flat_map(|(_, _, payload)| payload)
        .collect()
}

fn decode_ultra_hdr_pq(path: &Path, display_boost: f32) -> (u32, u32, Vec<[u16; 3]>) {
    let bytes = std::fs::read(path).unwrap();
    let decoder = NonNull::new(unsafe { uhdr::uhdr_create_decoder() }).unwrap();
    let mut image = uhdr::uhdr_compressed_image_t {
        data: bytes.as_ptr().cast_mut().cast(),
        data_sz: bytes.len(),
        capacity: bytes.len(),
        cg: uhdr::uhdr_color_gamut_t::UHDR_CG_UNSPECIFIED,
        ct: uhdr::uhdr_color_transfer_t::UHDR_CT_UNSPECIFIED,
        range: uhdr::uhdr_color_range_t::UHDR_CR_UNSPECIFIED,
    };
    let ok = |status: uhdr::uhdr_error_info_t| {
        assert_eq!(status.error_code, uhdr::uhdr_codec_err_t::UHDR_CODEC_OK);
    };
    unsafe {
        ok(uhdr::uhdr_dec_set_image(decoder.as_ptr(), &mut image));
        ok(uhdr::uhdr_dec_set_out_img_format(
            decoder.as_ptr(),
            uhdr::uhdr_img_fmt_t::UHDR_IMG_FMT_32bppRGBA1010102,
        ));
        ok(uhdr::uhdr_dec_set_out_color_transfer(
            decoder.as_ptr(),
            uhdr::uhdr_color_transfer_t::UHDR_CT_PQ,
        ));
        ok(uhdr::uhdr_dec_set_out_max_display_boost(
            decoder.as_ptr(),
            display_boost,
        ));
        ok(uhdr::uhdr_decode(decoder.as_ptr()));
    }
    let decoded = NonNull::new(unsafe { uhdr::uhdr_get_decoded_image(decoder.as_ptr()) }).unwrap();
    let decoded = unsafe { decoded.as_ref() };
    let packed = decoded.planes[uhdr::UHDR_PLANE_PACKED as usize].cast::<u32>();
    assert!(!packed.is_null());
    let stride = decoded.stride[0] as usize;
    let words = unsafe { std::slice::from_raw_parts(packed, stride * decoded.h as usize) };
    let mut pixels = Vec::with_capacity((decoded.w * decoded.h) as usize);
    for y in 0..decoded.h as usize {
        for x in 0..decoded.w as usize {
            let word = words[y * stride + x];
            pixels.push([
                (word & 0x3ff) as u16,
                ((word >> 10) & 0x3ff) as u16,
                ((word >> 20) & 0x3ff) as u16,
            ]);
        }
    }
    let dimensions = (decoded.w, decoded.h);
    unsafe { uhdr::uhdr_release_decoder(decoder.as_ptr()) };
    (dimensions.0, dimensions.1, pixels)
}

#[test]
fn ultra_hdr_v1_writes_a_deterministic_legacy_gain_map_jpeg() {
    let tmp = TempDir::new("ultra-hdr-v1");
    let first = tmp.path("first.jpg");
    let second = tmp.path("second.jpeg");
    for (index, output) in [&first, &second].into_iter().enumerate() {
        let telemetry = tmp.path(&format!("telemetry-{index}.json"));
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            "ultra-hdr-v1",
            "--film-base",
            "1,1,1",
            "--telemetry-file",
            telemetry.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
        let report = json(&stdout);
        assert_eq!(report["recipe"]["output"]["preset"], "ultra-hdr-v1");
        assert_eq!(
            report["output_render"]["encoding"],
            "legacy-ultra-hdr-v1-xmp-mpf-jpeg"
        );
        let decoded = image::open(output).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (502, 462));
        let timing: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(telemetry).unwrap()).unwrap();
        assert!(
            timing["timing_ms"]["color"]
                .as_f64()
                .is_some_and(|value| value > 0.0),
            "gain-map SDR/HDR rendering must be included in timing_ms.color: {timing}"
        );
    }
    let bytes = std::fs::read(&first).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    assert!(
        bytes
            .windows(b"hdrgm:Version=\"1.0\"".len())
            .any(|window| window == b"hdrgm:Version=\"1.0\"")
    );
    assert!(
        bytes
            .windows(b"Item:Semantic=\"GainMap\"".len())
            .any(|window| window == b"Item:Semantic=\"GainMap\"")
    );
    assert!(!bytes.windows(5).any(|window| window == b"21496"));
    let marker = |needle: &[u8]| {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    };
    assert!(marker(b"JFIF\0") < marker(b"MPF\0"));
    let reference = tmp.path("display-p3-reference.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        reference.to_str().unwrap(),
        "--film-base",
        "1,1,1",
        "--output-preset",
        "display-p3",
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        primary_jpeg_icc(&bytes),
        read_icc_tag(&reference),
        "primary JPEG ICC chunks must reassemble to nc's synthesized Display P3 profile"
    );
    assert_eq!(bytes, std::fs::read(&second).unwrap());
}

/// Walk an AVIF's top-level and `meta` boxes into `(type, body offset)` pairs.
fn avif_boxes(buf: &[u8]) -> Vec<(String, usize)> {
    fn walk(buf: &[u8], start: usize, end: usize, out: &mut Vec<(String, usize)>) {
        const CONTAINERS: [&[u8; 4]; 4] = [b"meta", b"iprp", b"ipco", b"iinf"];
        let mut at = start;
        while at + 8 <= end {
            let size = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
            if size < 8 {
                return;
            }
            let kind: [u8; 4] = buf[at + 4..at + 8].try_into().unwrap();
            out.push((String::from_utf8_lossy(&kind).into_owned(), at + 8));
            if CONTAINERS.contains(&&kind) {
                let skip = match &kind {
                    b"meta" => 4,
                    b"iinf" => 6,
                    _ => 0,
                };
                walk(buf, at + 8 + skip, (at + size).min(end), out);
            }
            at += size;
        }
    }
    let mut out = Vec::new();
    walk(buf, 0, buf.len(), &mut out);
    out
}

/// An HDR container whose signal never rises above the 203-nit reference white is
/// an HDR wrapper around an SDR picture: it costs bit depth and compatibility and
/// buys nothing, while the report still advertises `target_peak_nits: 1000`. Every
/// single-rendition HDR preset must say so, and must stop saying so as soon as the
/// frame actually uses the headroom.
#[test]
fn single_rendition_hdr_presets_warn_when_the_signal_stays_below_reference_white() {
    const MARKER: &str = "HDR output carries an SDR-range signal";
    let tmp = TempDir::new("hdr-sdr-range");
    let warnings = |stdout: &str| -> Vec<String> {
        json(stdout)["warnings"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|w| w.as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let input = fixture("hdr-48bit.tif");
    let convert = |preset: &str, out: &Path, extra: &[&str]| {
        let mut argv = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
        ];
        argv.extend_from_slice(extra);
        run(&argv)
    };

    for (preset, ext) in [
        ("hdr-pq", "avif"),
        ("hdr-hlg", "avif"),
        ("hdr-pq-tiff", "tif"),
        ("hdr-hlg-tiff", "tif"),
        ("hdr-linear-tiff", "tif"),
    ] {
        // Three stops down, this frame peaks under reference white — in a container
        // signalling HDR.
        let low = tmp.path(&format!("{preset}-low.{ext}"));
        let (code, stdout, err) = convert(preset, &low, &["--print-exposure=-5"]);
        assert_eq!(code, 0, "{err}");
        assert!(
            warnings(&stdout).iter().any(|w| w.contains(MARKER)),
            "{preset} must warn that its HDR signal is SDR-range: {:?}",
            warnings(&stdout)
        );

        // The falsifiable control: at the default exposure the same frame does reach
        // past reference white, so the warning must disappear. Without this the
        // assertion above would pass equally for a warning that always fires. The
        // `--strict` here is a second assertion — `hdr-48bit.tif` is the IR-free
        // fixture, so exit 0 proves the run raised *no* promotable warning at all.
        let high = tmp.path(&format!("{preset}-high.{ext}"));
        let (code, stdout, err) = convert(preset, &high, &["--strict"]);
        assert_eq!(code, 0, "{err}");
        assert!(
            !warnings(&stdout).iter().any(|w| w.contains(MARKER)),
            "{preset} must not warn when content exceeds reference white: {:?}",
            warnings(&stdout)
        );
    }

    // `--strict` promotes it. One preset is enough: promotion is the shared
    // `push_warning_buf` path, not anything per-preset.
    let strict = tmp.path("strict.tif");
    let (code, _stdout, err) =
        convert("hdr-pq-tiff", &strict, &["--print-exposure=-5", "--strict"]);
    assert_eq!(
        code, 1,
        "--strict must promote the SDR-range warning: {err}"
    );
    assert!(err.contains(MARKER), "{err}");

    // `ultra-hdr-v1` is dual-rendition — an SDR base image plus a gain map, so a
    // low-headroom render yields an inert gain map rather than a mislabelled HDR
    // container. Different artifact, different diagnosis; this warning stays off it.
    let ultra = tmp.path("ultra.jpg");
    let (code, stdout, err) = convert("ultra-hdr-v1", &ultra, &["--print-exposure=-5"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !warnings(&stdout).iter().any(|w| w.contains(MARKER)),
        "ultra-hdr-v1 must not carry the single-rendition HDR warning: {:?}",
        warnings(&stdout)
    );
}

#[test]
fn hdr_linear_tiff_writes_a_bit_exact_display_linear_bt2020_master() {
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;

    let tmp = TempDir::new("hdr-linear-tiff");
    let first = tmp.path("first.tif");
    let second = tmp.path("second.TIFF");
    for output in [&first, &second] {
        // `hdr-48bit.tif` is the IR-free fixture, so `--strict` is a real assertion
        // here: the run must produce *no* promotable warning at all. On the HDRi
        // fixture every run trips the "IR preserved but not used" warning and this
        // would prove nothing.
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            "hdr-linear-tiff",
            "--film-base",
            "1,1,1",
            // The **exponential** curve, named explicitly. This test's subject is
            // the container — that samples above the 203-nit reference white
            // survive with no transfer or clamp applied — so the fixture has to
            // produce some, whatever the default curve is.
            "--strict",
        ]);
        assert_eq!(code, 0, "{err}");
        let report = json(&stdout);
        assert_eq!(report["recipe"]["output"]["preset"], "hdr-linear-tiff");
        assert_eq!(
            report["output_render"]["encoding"],
            "display-linear-bt2020-float-tiff"
        );
        // Both flags are true: this branch runs the print controls *and* a display
        // render, which is what distinguishes it from `film-master`.
        assert_eq!(report["output_render"]["print_controls"], true);
        assert_eq!(report["output_render"]["display_render"], true);

        let block = &report["hdr_linear_tiff"];
        assert_eq!(
            block["pixel_contract"],
            "rgb-f32-display-linear-bt2020-d65-relative-to-203-nit-reference-white"
        );
        assert_eq!(block["bits_per_sample"], 32);
        assert_eq!(block["sample_format"], 3, "3 == IEEE float");
        assert_eq!(block["bigtiff"], false);
        assert_eq!(block["reference_white_sample"], 1.0);
        assert_eq!(block["reference_white_nits"], 203.0);
        assert_eq!(block["target_peak_nits"], 1000.0);
        assert!(block["icc_bytes"].as_u64().unwrap() > 0);
        let headroom = block["linear_headroom"].as_f64().unwrap();
        assert!(
            (headroom - 1000.0 / 203.0).abs() < 1e-6,
            "headroom {headroom} is not 1000/203"
        );
        // Measured content light, not the mastering policy: a real frame must not
        // report the 1000/203 constants back.
        let cll = block["max_cll_nits"].as_u64().unwrap();
        let fall = block["max_fall_nits"].as_u64().unwrap();
        assert!(fall <= cll, "MaxFALL {fall} exceeds MaxCLL {cll}");
        assert!(cll <= 1000, "MaxCLL {cll} above the mastering peak");
        assert!(
            cll != 1000 || fall != 203,
            "content light looks like the policy constants, not a measurement"
        );
        // No PQ/HLG signalling on this path — it is linear, so there is no transfer
        // to declare and no `avif` block.
        assert!(report["avif"].is_null());
    }

    // Same build, same input ⇒ byte-identical (the ICC dateTime is zeroed).
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "repeated hdr-linear-tiff encodes must be byte-identical"
    );

    // Independently decode the file and check the storage contract plus the linear
    // domain. A PQ-encoded frame would have no sample above 1.0 at all, so the
    // headroom assertion is what proves no transfer was applied.
    let bytes = std::fs::read(&first).unwrap();
    let mut decoder = Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert!(
        decoder.get_tag_u8_vec(Tag::IccProfile).is_ok(),
        "no embedded ICC profile"
    );
    let samples = match decoder.read_image().unwrap() {
        DecodingResult::F32(data) => data,
        other => panic!("expected 32-bit float samples, got {other:?}"),
    };
    assert!(samples.iter().all(|v| v.is_finite()));
    let max = samples.iter().copied().fold(f32::MIN, f32::max);
    assert!(
        max > 1.0,
        "no sample above the 203-nit reference white ({max}); either the fixture \
         has no highlights or a transfer/clamp was applied"
    );
    assert!(
        max <= 1000.0 / 203.0 + 1e-6,
        "sample {max} exceeds the 1000-nit peak"
    );

    // The sidecar rides along and reloads as a recipe.
    let sidecar = PathBuf::from(format!("{}.json", first.display()));
    let envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(envelope["params"]["output"]["preset"], "hdr-linear-tiff");
}

#[test]
fn coded_hdr_tiffs_store_exact_codes_and_signal_cicp_in_the_profile() {
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;

    let tmp = TempDir::new("hdr-coded-tiff");
    for (preset, transfer_code, expect_hlg) in
        [("hdr-pq-tiff", 16u64, false), ("hdr-hlg-tiff", 18, true)]
    {
        let output = tmp.path(&format!("{preset}.tif"));
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
            // The **exponential** curve, named explicitly. This test's subject is the
            // coded container, and it asserts exit 0 under `--strict` on the IR-free
            // fixture — i.e. *no* promotable warning. A curve keeping this frame's
            // peak below the 203-nit reference white would raise one
            // (`single_rendition_hdr_presets_warn_when_the_signal_stays_below_reference_white`)
            // that has nothing to do with PQ/HLG code storage.
            "--strict",
        ]);
        assert_eq!(code, 0, "{preset}: {err}");
        let report = json(&stdout);
        assert_eq!(report["recipe"]["output"]["preset"], preset);

        let block = &report["hdr_coded_tiff"];
        assert_eq!(block["bits_per_sample"], 16);
        assert_eq!(block["sample_format"], 1, "1 == unsigned integer");
        assert_eq!(block["full_range"], true);
        assert_eq!(block["cicp"][0], 9, "BT.2020 primaries");
        assert_eq!(block["cicp"][1], transfer_code);
        // The normative difference from the AVIF block: an RGB ICC profile requires
        // MatrixCoefficients 0, where AVIF writes 9 for the same rendition.
        assert_eq!(block["cicp"][2], 0);
        assert_eq!(block["reference_white_nits"], 203.0);
        assert_eq!(block["target_peak_nits"], 1000.0);
        // Rounding cannot cost more than half a code, and the report must say so
        // with a real measurement rather than a constant.
        let max = block["max_quantization_error_codes"].as_f64().unwrap();
        let rms = block["rms_quantization_error_codes"].as_f64().unwrap();
        assert!(max > 0.0 && max <= 0.5, "{preset}: max error {max}");
        assert!(rms > 0.0 && rms <= max, "{preset}: rms {rms} vs max {max}");
        // Truthful naming, in the artifact.
        let notes = block["interoperability"].as_str().unwrap();
        assert!(notes.contains("limited-interoperability"), "{notes}");
        assert!(
            notes.contains("not one of BT.2100's specified bit depths"),
            "{notes}"
        );
        // HLG carries its reference-display assumptions; PQ has none to carry.
        assert_eq!(block["hlg_system_gamma"].is_null(), !expect_hlg);
        assert!(report["avif"].is_null(), "{preset}: no AVIF block here");
        // No clipping and no non-finite: the domain is verified before quantizing,
        // so `--strict` (exit 0 above) is a real assertion on the IR-free fixture.
        assert_eq!(report["loss"]["clipped_high"], 0);
        assert_eq!(report["loss"]["non_finite"], 0);

        // Independently decode: 16-bit unsigned samples plus an embedded profile
        // whose `cicp` tag a third-party reader can find.
        let bytes = std::fs::read(&output).unwrap();
        let mut decoder = Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let icc = decoder
            .get_tag_u8_vec(Tag::IccProfile)
            .expect("no embedded ICC profile");
        // Walk the ICC tag table (count at byte 128, then 12-byte
        // signature/offset/size entries) to the `cicp` tag data, rather than
        // scanning for the bytes — the first `cicp` in the file is the *table
        // entry*, whose next four bytes are an offset, not the reserved zeros.
        let count = u32::from_be_bytes(icc[128..132].try_into().unwrap()) as usize;
        let mut cicp_at = None;
        for i in 0..count {
            let entry = 132 + i * 12;
            if &icc[entry..entry + 4] == b"cicp" {
                let offset = u32::from_be_bytes(icc[entry + 4..entry + 8].try_into().unwrap());
                let size = u32::from_be_bytes(icc[entry + 8..entry + 12].try_into().unwrap());
                assert_eq!(size, 12, "cicpType is a 12-byte structure");
                cicp_at = Some(offset as usize);
            }
        }
        let tag_at = cicp_at.expect("no cicp tag in the embedded profile");
        let tag = &icc[tag_at..tag_at + 12];
        assert_eq!(&tag[0..4], b"cicp", "cicpType signature");
        assert_eq!(&tag[4..8], &[0, 0, 0, 0], "reserved bytes must be zero");
        assert_eq!(tag[8], 9, "ColourPrimaries");
        assert_eq!(u64::from(tag[9]), transfer_code, "TransferCharacteristics");
        assert_eq!(tag[10], 0, "MatrixCoefficients must be 0 for RGB");
        assert_eq!(tag[11], 1, "VideoFullRangeFlag");

        let samples = match decoder.read_image().unwrap() {
            DecodingResult::U16(data) => data,
            other => panic!("{preset}: expected u16 samples, got {other:?}"),
        };
        // A real frame must use a wide part of the code range, and PQ/HLG place a
        // 203-nit white well below full scale — so a file pinned at 65535 would mean
        // the transfer was skipped.
        let max_code = samples.iter().copied().max().unwrap();
        assert!(
            max_code > 1000,
            "{preset}: max code {max_code} is implausibly low"
        );
    }

    // The two transfers must produce genuinely different files from one input.
    let pq = std::fs::read(tmp.path("hdr-pq-tiff.tif")).unwrap();
    let hlg = std::fs::read(tmp.path("hdr-hlg-tiff.tif")).unwrap();
    assert_ne!(pq, hlg, "PQ and HLG TIFFs must differ");
}

#[test]
fn hdr_tiff_sidecars_carry_the_luminance_contract_and_still_reload() {
    // The task makes the **sidecar** authoritative for semantics the ICC provably
    // cannot carry. Putting them only in the stdout report loses them whenever the
    // report is discarded, so this runs with `--report none` — the way a batch script
    // would call it — and then proves the sidecar is still loadable as a recipe,
    // which is the constraint that forced the contract inside `meta` rather than
    // beside `params` (`SidecarEnvelopeIn` is `deny_unknown_fields`).
    let tmp = TempDir::new("hdr-sidecar");
    for (preset, block, transfer) in [
        ("hdr-pq-tiff", "hdr_coded_tiff", Some(16u64)),
        ("hdr-hlg-tiff", "hdr_coded_tiff", Some(18)),
        ("hdr-linear-tiff", "hdr_linear_tiff", None),
    ] {
        let output = tmp.path(&format!("{preset}.tif"));
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
            "--report",
            "none",
        ]);
        assert_eq!(code, 0, "{preset}: {err}");
        assert!(stdout.trim().is_empty(), "{preset}: --report none printed");

        let sidecar: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(PathBuf::from(format!("{}.json", output.display()))).unwrap(),
        )
        .unwrap();
        // Top-level shape is untouched — a third sibling key would break reloading.
        let mut top: Vec<&String> = sidecar.as_object().unwrap().keys().collect();
        top.sort();
        assert_eq!(
            top,
            vec!["meta", "params"],
            "{preset}: envelope shape moved"
        );

        let contract = &sidecar["meta"][block];
        assert!(!contract.is_null(), "{preset}: no {block} in sidecar meta");
        assert_eq!(contract["reference_white_nits"], 203.0, "{preset}");
        assert_eq!(contract["target_peak_nits"], 1000.0, "{preset}");
        // Identity still rides alongside it.
        assert!(!sidecar["meta"]["params_hash"].is_null(), "{preset}");
        if let Some(code_point) = transfer {
            assert_eq!(contract["cicp"][1], code_point, "{preset}");
            let max = contract["max_quantization_error_codes"].as_f64().unwrap();
            assert!(max > 0.0 && max <= 0.5, "{preset}: quantization {max}");
        } else {
            // The linear TIFF reports measured content light instead.
            assert!(!contract["max_cll_nits"].is_null(), "{preset}");
            assert!(contract["interoperability"].as_str().is_some(), "{preset}");
        }

        // And the sidecar is still a valid recipe.
        let replay = tmp.path(&format!("{preset}-replay.tif"));
        let (code, _, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            replay.to_str().unwrap(),
            "--params",
            &format!("{}.json", output.display()),
            "--report",
            "none",
        ]);
        assert_eq!(code, 0, "{preset}: sidecar failed to reload: {err}");
        assert_eq!(
            std::fs::read(&output).unwrap(),
            std::fs::read(&replay).unwrap(),
            "{preset}: replay from its own sidecar is not byte-identical"
        );
    }
}

#[test]
fn hdr_linear_tiff_rejects_a_non_tiff_path_and_conflicting_flags() {
    let tmp = TempDir::new("hdr-linear-reject");
    let base = [
        "convert",
        "--output-preset",
        "hdr-linear-tiff",
        "--film-base",
        "1,1,1",
    ];
    let input = fixture("hdr-48bit.tif");

    // Wrong suffix: exit 2 and the path is never rewritten.
    let jpg = tmp.path("out.jpg");
    let mut args = vec![
        base[0],
        input.to_str().unwrap(),
        "-o",
        jpg.to_str().unwrap(),
    ];
    args.extend_from_slice(&base[1..]);
    let (code, _, err) = run(&args);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".tif"), "{err}");
    assert!(!jpg.exists(), "a rejected run must write nothing");

    // The retired `--out-depth` is a migration error, never a synonym for this
    // preset's own f32.
    let out = tmp.path("out.tif");
    let mut args = vec![
        base[0],
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    args.extend_from_slice(&base[1..]);
    args.push("--out-depth");
    args.push("f32");
    let (code, _, err) = run(&args);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("was removed"), "{err}");
    assert!(!out.exists(), "a rejected run must write nothing");
}

#[test]
fn hdr_pq_writes_a_deterministic_advanced_profile_avif() {
    let tmp = TempDir::new("hdr-pq");
    let first = tmp.path("first.avif");
    let second = tmp.path("second.AVIF");
    for (index, output) in [&first, &second].into_iter().enumerate() {
        let telemetry = tmp.path(&format!("telemetry-{index}.json"));
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            "hdr-pq",
            "--film-base",
            "1,1,1",
            "--telemetry-file",
            telemetry.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
        let report = json(&stdout);
        assert_eq!(report["recipe"]["output"]["preset"], "hdr-pq");
        assert_eq!(
            report["output_render"]["encoding"],
            "rec2100-pq-10bit-444-avif"
        );
        // The `avif` report block is evidence read back out of the file.
        assert_eq!(report["avif"]["profile"], "advanced");
        assert_eq!(report["avif"]["bit_depth"], 10);
        assert_eq!(report["avif"]["seq_profile"], 1);
        assert_eq!(report["avif"]["full_range"], true);
        assert_eq!(report["avif"]["cicp"][0], 9);
        assert_eq!(report["avif"]["cicp"][1], 16);
        assert_eq!(report["avif"]["cicp"][2], 9);
        assert!(report["avif"]["profile_reason"].is_null());
        // The rendering block is the one part that is *not* read back — no AVIF box
        // can say where diffuse white sits — so it is asserted as declared policy.
        let rendering = &report["avif"]["rendering"];
        assert_eq!(rendering["reference_white_nits"], 203.0);
        assert_eq!(rendering["target_peak_nits"], 1000.0);
        assert_eq!(
            rendering["tone_curve"],
            "extended-reinhard-mid-preserving-v2"
        );
        // The conformance property is the ceiling, not a particular level: a
        // small fixture lands well under it, and pinning the exact value would
        // make a legitimate encoder change look like a conformance failure.
        let level_idx = report["avif"]["seq_level_idx"].as_u64().unwrap();
        assert!(level_idx <= 16, "level index {level_idx} exceeds 6.0");
        assert_eq!(
            report["avif"]["level"],
            format!("{}.{}", 2 + (level_idx >> 2), level_idx & 3)
        );
        let timing: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(telemetry).unwrap()).unwrap();
        assert!(
            timing["timing_ms"]["color"]
                .as_f64()
                .is_some_and(|value| value > 0.0),
            "HDR display rendering must be included in timing_ms.color: {timing}"
        );
    }

    let bytes = std::fs::read(&first).unwrap();
    // Brands, and the absence of any metadata nc did not ask for.
    assert_eq!(&bytes[4..8], b"ftyp");
    assert_eq!(&bytes[8..12], b"avif", "major brand");
    for brand in [b"avif", b"mif1", b"miaf", b"MA1A"] {
        assert!(
            bytes[..32].windows(4).any(|w| w == brand),
            "missing compatible brand {}",
            String::from_utf8_lossy(brand)
        );
    }
    let tree = avif_boxes(&bytes);
    let at = |kind: &str| {
        tree.iter()
            .find(|(name, _)| name == kind)
            .unwrap_or_else(|| panic!("no `{kind}` box in {tree:?}"))
            .1
    };
    // nclx CICP 9/16/9 with the full-range flag, plus PQ's content-light box.
    let colr = at("colr");
    assert_eq!(&bytes[colr..colr + 4], b"nclx");
    assert_eq!(&bytes[colr + 4..colr + 10], &[0, 9, 0, 16, 0, 9]);
    assert_eq!(bytes[colr + 10], 0x80);
    // `clli` states this frame's measured content light: MaxCLL is its brightest
    // pixel in cd/m² and MaxFALL its frame average, both bounded by the 1000-nit
    // mastering peak. Deliberately not frozen literals — the point of the box is
    // that it follows the pixels, which the darker run below proves.
    let clli = at("clli");
    let content_light = |bytes: &[u8], at: usize| {
        (
            u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap()),
            u16::from_be_bytes(bytes[at + 2..at + 4].try_into().unwrap()),
        )
    };
    let (max_cll, max_fall) = content_light(&bytes, clli);
    assert!(
        0 < max_cll && max_cll <= 1000,
        "MaxCLL {max_cll} is outside the rendered 0..=1000 cd/m² range"
    );
    assert!(
        max_fall <= max_cll,
        "MaxFALL {max_fall} exceeds MaxCLL {max_cll}"
    );
    // 10-bit on three channels, and High Profile in `av1C`.
    let pixi = at("pixi");
    assert_eq!(&bytes[pixi + 4..pixi + 8], &[3, 10, 10, 10]);
    let av1c = at("av1C");
    assert_eq!(bytes[av1c], 0x81);
    assert_eq!(bytes[av1c + 1] >> 5, 1, "seq_profile must be High");
    // No EXIF/XMP/ICC is invented. An embedded ICC would appear as a `colr` box
    // of type `prof`; nc signals colour with nclx only.
    assert!(
        tree.iter()
            .filter(|(name, _)| name == "colr")
            .all(|(_, body)| &bytes[*body..*body + 4] == b"nclx"),
        "every colr box must be nclx, never an embedded ICC (`prof`)"
    );
    assert!(
        !bytes.windows(4).any(|w| w == b"Exif"),
        "no EXIF should be written"
    );
    assert!(
        !bytes.windows(3).any(|w| w == b"xml"),
        "no XMP should be written"
    );
    // Byte-identical on repeat, on the same build.
    assert_eq!(bytes, std::fs::read(&second).unwrap());

    // The same frame four stops darker must report lower content light. This is the
    // regression that matters: a `clli` derived from renderer policy instead of
    // pixels would hand both files the identical 1000/203 claim.
    let dark = tmp.path("dark.avif");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        dark.to_str().unwrap(),
        "--output-preset",
        "hdr-pq",
        "--film-base",
        "1,1,1",
        // `=` because clap would otherwise read the leading `-` as a flag.
        "--print-exposure=-4",
    ]);
    assert_eq!(code, 0, "{err}");
    let dark_bytes = std::fs::read(&dark).unwrap();
    let dark_tree = avif_boxes(&dark_bytes);
    let dark_clli = dark_tree.iter().find(|(name, _)| name == "clli").unwrap().1;
    let (dark_cll, dark_fall) = content_light(&dark_bytes, dark_clli);
    assert!(
        dark_cll < max_cll && dark_fall <= dark_cll,
        "a four-stop-darker render reported MaxCLL/MaxFALL {dark_cll}/{dark_fall} against \
         the reference render's {max_cll}/{max_fall}"
    );
}

#[test]
fn hdr_hlg_signals_its_own_transfer_and_omits_content_light_level() {
    let tmp = TempDir::new("hdr-hlg");
    let output = tmp.path("out.avif");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--output-preset",
        "hdr-hlg",
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(
        report["output_render"]["encoding"],
        "rec2100-hlg-10bit-444-avif"
    );
    assert_eq!(report["avif"]["cicp"][1], 18);
    let bytes = std::fs::read(&output).unwrap();
    let tree = avif_boxes(&bytes);
    let colr = tree.iter().find(|(n, _)| n == "colr").unwrap().1;
    assert_eq!(&bytes[colr + 4..colr + 10], &[0, 9, 0, 18, 0, 9]);
    assert!(
        !tree.iter().any(|(n, _)| n == "clli"),
        "HLG is display-referred; absolute content-light metadata must be omitted"
    );
}

#[test]
fn hdr_avif_presets_reject_a_non_avif_suffix_and_roll_with_an_avif_name() {
    let tmp = TempDir::new("hdr-avif-gates");
    for preset in ["hdr-pq", "hdr-hlg"] {
        let output = tmp.path(&format!("{preset}.tiff"));
        let (code, _stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
        ]);
        assert_eq!(code, 2, "{err}");
        assert!(err.contains(".avif"), "{err}");
        assert!(!output.exists(), "nothing may be written on a usage error");
    }
    // Roll runs this preset now and derives a container-correct name. `roll` has no
    // output-selection flags, so the preset arrives via the shared recipe.
    let out_dir = tmp.path("roll-out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let recipe = tmp.path("roll.json");
    std::fs::write(
        &recipe,
        r#"{"output":{"preset":"hdr-pq"},"calibration":{"film_base":{"explicit":[1,1,1]}}}"#,
    )
    .unwrap();
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out_dir.join("hdr-48bit_positive.avif").exists(),
        "roll must derive the preset's own container suffix, not `.tiff`"
    );
}

#[test]
fn ultra_hdr_v1_rejects_a_non_jpeg_suffix_before_writing() {
    let tmp = TempDir::new("ultra-hdr-v1-suffix");
    let output = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--output-preset",
        "ultra-hdr-v1",
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".jpg"), "{err}");
    assert!(!output.exists());
}

#[test]
fn ultra_hdr_v1_native_reconstruction_covers_odd_dimensions_and_hdr_vectors() {
    let tmp = TempDir::new("ultra-hdr-v1-native-odd");
    let input = tmp.path("odd.tiff");
    let output = tmp.path("odd.jpg");
    // The film positives are placed exactly by inverting a stated exponential:
    // `positive = 10^(4·(D − 2))` with `D = −log10(scan)` over a unit base, so a scan
    // of `10^−(2 + log10(p)/4)` renders `p`. The film base itself renders `10^−8`,
    // which is black. The slope is steep so the u16 scan resolves each target finely.
    let scan = |p: f64| (65535.0 * 10f64.powf(-(2.0 + p.log10() / 4.0))).round() as u16;
    let (black, white, peak) = (u16::MAX, scan(0.125), 0);
    let row = [
        [black; 3],            // black positive
        [white; 3],            // 0.125 positive; ×8 exposure = reference white
        [peak; 3],             // neutral peak
        [white, black, black], // saturated red at reference-white scale
        [scan(0.5); 3],        // mid gray
    ];
    let pixels = row.into_iter().cycle().take(15).collect::<Vec<_>>();
    write_rgb48_pixels(&input, 5, 3, &pixels);
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--output-preset",
        "ultra-hdr-v1",
        "--film-base",
        "1,1,1",
        "--density-scale",
        "1,1,1",
        "--density-gamma",
        "4",
        // Mid-grey 1.8138 above the base puts the anchor at `1.8138 + 0.7447/4 = 2`.
        "--anchor-mid-offset",
        "1.8138181",
        "--print-exposure",
        "3",
    ]);
    assert_eq!(code, 0, "{err}");

    let headroom = 1000.0 / 203.0;
    let (width, height, decoded) = decode_ultra_hdr_pq(&output, headroom);
    assert_eq!((width, height), (5, 3));

    // Independent ST 2084 inverse-EOTF oracle, rounded to the decoder's 10-bit
    // packed PQ code domain. JPEG base/map loss is bounded around these anchors.
    let pq_code = |nits: f64| {
        let m1 = 2610.0 / 16384.0;
        let m2 = 2523.0 / 32.0;
        let c1 = 3424.0 / 4096.0;
        let c2 = 2413.0 / 128.0;
        let c3 = 2392.0 / 128.0;
        let p = (nits / 10_000.0).powf(m1);
        (((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2) * 1023.0).round() as i32
    };
    let neutral_error = |pixel: [u16; 3], expected: i32| {
        pixel
            .into_iter()
            .map(|value| (i32::from(value) - expected).abs())
            .max()
            .unwrap()
    };
    assert!(
        neutral_error(decoded[0], pq_code(0.0)) <= 32,
        "black reconstruction outside codec-aware bound: {:?}",
        decoded[0]
    );
    assert!(
        // The half-resolution map deliberately shares support with the adjacent
        // peak before JPEG quantization; allow that bounded upward error while
        // still rejecting a missing/flat gain reconstruction.
        neutral_error(decoded[1], pq_code(203.0)) <= 96,
        "reference-white reconstruction outside codec-aware bound: {:?}",
        decoded[1]
    );
    assert!(
        neutral_error(decoded[2], pq_code(1000.0)) <= 64,
        "peak reconstruction outside codec-aware bound: {:?}",
        decoded[2]
    );
    assert!(
        decoded[3][0] > decoded[3][1] + 80 && decoded[3][0] > decoded[3][2] + 80,
        "saturated-red reconstruction lost channel separation: {:?}",
        decoded[3]
    );
}

/// Every `*.nctmp` staging file left in `dir` — the litter check that must come back
/// empty after any failure (`io/transactional-output-writes`).
fn staging_temps(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .expect("temp dir readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "nctmp"))
        .collect()
}

#[test]
fn a_failing_sidecar_write_leaves_no_primary_output() {
    // The exact scenario the output-atomicity review reproduced: `encode` succeeds,
    // `write_sidecar` fails, and the run used to exit 5 leaving a *complete* primary
    // TIFF with no sidecar beside it. Injected portably by putting a directory where
    // the sidecar file has to go — a write there cannot succeed on any platform.
    let tmp = TempDir::new("sidecar-fails");
    let out = tmp.path("out.tiff");
    std::fs::create_dir(sidecar_of(&out)).expect("occupy the sidecar path");

    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_ne!(code, 0, "a sidecar write failure must fail the run: {err}");
    assert!(
        !out.exists(),
        "the primary output must not exist when a later artifact failed —          this is the orphaned-TIFF regression"
    );
    assert!(
        staging_temps(&tmp.0).is_empty(),
        "a failed run must not leave staging temps: {:?}",
        staging_temps(&tmp.0)
    );
}

#[test]
fn a_failing_ir_export_leaves_no_primary_output() {
    // IR is staged before the primary, so its failure must abort the whole set. The
    // ordering trick that used to provide this (export IR first) only ever helped
    // because IR came first; now it holds because nothing is committed until all
    // three artifacts exist.
    let tmp = TempDir::new("ir-fails");
    let out = tmp.path("out.tiff");
    let ir = tmp.path("ir.tiff");
    std::fs::create_dir(&ir).expect("occupy the IR path");

    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--export-ir",
        ir.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_ne!(code, 0, "a failing IR export must fail the run: {err}");
    assert!(
        !out.exists(),
        "no primary output for an aborted artifact set"
    );
    assert!(
        !sidecar_of(&out).exists(),
        "and no sidecar either — the set is committed together"
    );
    assert!(
        staging_temps(&tmp.0).is_empty(),
        "no staging temps survive: {:?}",
        staging_temps(&tmp.0)
    );
}

#[test]
fn an_interrupted_overwrite_leaves_the_previous_output_intact() {
    // The decided contract is atomic *replace*: `nc` keeps overwriting its own
    // output. What must never happen is a truncated new file where a valid old one
    // was — so a run that fails after the primary is encoded must leave the previous
    // bytes untouched, not a half-written TIFF.
    let tmp = TempDir::new("overwrite");
    let out = tmp.path("out.tiff");
    let input = fixture("hdr-48bit.tif");
    let args = [
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ];
    let (code, _o, _e) = run(&args);
    assert_eq!(code, 0, "first conversion should succeed");
    let original = std::fs::read(&out).expect("first output readable");

    // Now make the sidecar unwritable so the second run fails after encoding.
    std::fs::remove_file(sidecar_of(&out)).expect("remove the first sidecar");
    std::fs::create_dir(sidecar_of(&out)).expect("occupy the sidecar path");
    let (code, _o, err) = run(&args);
    assert_ne!(code, 0, "the second run must fail: {err}");
    assert_eq!(
        std::fs::read(&out).expect("previous output still readable"),
        original,
        "an interrupted overwrite must leave the OLD file intact, byte for byte"
    );
    assert!(staging_temps(&tmp.0).is_empty(), "no staging temps survive");
}

#[test]
fn a_successful_run_leaves_no_staging_temps() {
    // The success path's half of the litter check: every temp is consumed by its
    // rename, so a normal conversion leaves exactly the artifacts and nothing else.
    let tmp = TempDir::new("no-litter");
    let out = tmp.path("out.tiff");
    let ir = tmp.path("ir.tiff");
    let report = tmp.path("report.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--export-ir",
        ir.to_str().unwrap(),
        "--report-file",
        report.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "conversion should succeed: {err}");
    for artifact in [&out, &ir, &report, &sidecar_of(&out)] {
        assert!(artifact.exists(), "missing artifact {}", artifact.display());
    }
    assert!(
        staging_temps(&tmp.0).is_empty(),
        "a successful run must leave no temps: {:?}",
        staging_temps(&tmp.0)
    );
}

#[test]
fn convert_writes_tiff_sidecar_and_report() {
    let tmp = TempDir::new("convert-basic");
    let out = tmp.path("out.tiff");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        // Real scans are holder → rebate → picture, so auto-base fails loudly;
        // supply an explicit base (the documented calibrate-once workflow).
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "convert should succeed");
    assert!(is_tiff(&out), "output must be a valid TIFF");
    // Effective-recipe sidecar next to the output, valid JSON — recipe body under
    // `params`, beside the `meta` identity envelope. Neither retired `type` selector
    // is written.
    let recipe = sidecar_params(&out);
    assert!(recipe["reconstruction"].get("type").is_none(), "{recipe}");
    assert_eq!(recipe["reconstruction"]["schema_version"], 1);
    assert!(
        recipe["reconstruction"]["curve"].get("type").is_none(),
        "{recipe}"
    );

    let report = json(&stdout);
    assert_eq!(report["command"], "convert");
    assert!(report["reconstruction_result"].get("type").is_none());
    assert!(
        report["reconstruction_result"]["curve"]
            .get("type")
            .is_none()
    );
    assert_eq!(report["recipe"]["reconstruction"]["curve"]["gamma"], 2.0);
    // The pinned working-space mapping is stamped on every convert report
    // (design-spec §8), independent of reconstruction path.
    assert_eq!(report["working_mapping"], "nc-film-rgb-v1");
    assert_eq!(report["output"], out.to_str().unwrap());
    assert!(report["film_base"].is_object(), "film base reported");
    assert!(report["loss"].is_object(), "encode loss reported");
    assert!(report["elapsed_ms"].is_number());
}

#[test]
fn u16_clipping_is_reported_and_strict_promotes_it() {
    // Force guaranteed u16 clipping with a large positive `--print-exposure`
    // (2^12× gain blows every highlight past 1.0), so this test pins the
    // clip-reporting + `--strict` mechanism *independently* of the density
    // default's baseline exposure.
    // The HDR fixture carries no IR plane, so the only warning is the clipping —
    // proving clipping alone drives the strict failure.
    let tmp = TempDir::new("u16-clip");
    let base_args = |extra: &[&str], out: &Path| {
        let mut v = vec![
            "convert",
            "__IN__",
            "-o",
            "__OUT__",
            "--output-preset",
            "display-p3",
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
        "--output-preset",
        "display-p3",
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
    // The recipe handoff is the `calibration` object, in the documented
    // `{"film_base":{"explicit":[…]}}` shape, carrying exactly the same numbers as
    // the measurement.
    let calibration = &report["calibration"];
    assert_eq!(
        calibration["film_base"]["explicit"],
        serde_json::json!([base["r"], base["g"], base["b"]]),
        "the calibration must carry the measured base: {report}"
    );
    // Nothing else was measured, so nothing else is claimed — piping this into
    // `--params` must not pin a reference the run never resolved.
    assert!(
        calibration.get("dmax").is_none(),
        "an unmeasured reference must not appear: {report}"
    );

    // Round-trip A: the flag value fed to `convert` reproduces the base.
    let out_flag = tmp.path("flag.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out_flag.to_str().unwrap(),
        "--output-preset",
        "film-master",
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
    // No hand editing: exactly what `jq '{calibration}'` hands `--params`.
    std::fs::write(
        &recipe,
        serde_json::json!({ "calibration": calibration }).to_string(),
    )
    .unwrap();
    let out_recipe = tmp.path("recipe.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out_recipe.to_str().unwrap(),
        // The fragment states only a film base, so the preset comes from the flag
        // (the default `gain-map-hdr` writes a JPEG, not the f32 TIFF compared here).
        "--output-preset",
        "film-master",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
        "--export-ir",
        ir_hdr.to_str().unwrap(),
        "--auto-base",
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
        "--output-preset",
        "display-p3",
        "--density-gamma",
        "0",
    ]);
    assert_eq!(code, 2, "invalid params must exit 2");
    assert!(!out.exists(), "no output on a usage error");

    // The removed simple clip controls are migration errors (exit 2), and the
    // removed --algorithm selector says to drop the flag.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--clip-low",
        "0.9",
    ]);
    assert_eq!(code, 2, "a removed flag must exit 2: {err}");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--algorithm",
        "density",
    ]);
    assert_eq!(code, 2, "--algorithm must be a migration error: {err}");
    assert!(
        err.contains("reconstruction.curve") && err.contains("Drop the flag"),
        "the migration error names what replaced it: {err}"
    );
    assert!(!out.exists(), "no output on a usage error");
}

/// The old calibration spellings are **migration errors**, on every path that loads a
/// recipe, naming the key they moved to.
///
/// Project policy is a migration error rather than an alias (the removed `algorithm`
/// selector is the precedent), and nc is unreleased. Driven through the binary because
/// ordering is the half a direct call cannot test: the `reconstruction.curve.dmax` rule
/// has to out-rank both "unknown field `dmax`" and the characteristic curve's
/// cross-variant message, either of which is true and neither of which tells the user
/// where the value went.
#[test]
fn the_old_calibration_spellings_are_migration_errors() {
    let tmp = TempDir::new("calibration-migration");
    let out = tmp.path("out.tif");
    let scan = fixture("hdr-48bit.tif");

    // (a) the top-level `film_base` section, in all three spellings a recipe can use.
    for body in [
        r#"{"film_base":{"source":"auto"}}"#,
        r#"{"film_base":{"source":{"region":[0,0,8,8]}}}"#,
        r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}"#,
    ] {
        let recipe = write_file(&tmp.path("base.json"), body);
        let (code, _, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{body} must be a migration error: {err}");
        assert!(err.contains("calibration"), "{body}: {err}");
        assert!(
            err.contains(r#""calibration": {"film_base": {"explicit": [r, g, b]}}"#),
            "the remedy must name the flattened spelling: {err}"
        );
        assert!(!out.exists(), "no output on a usage error");
    }

    // (b) `reconstruction.curve.dmax`, on each curve type. On `characteristic` the
    // cross-variant rule also matches — it must not win, or the user is told `dmax` is
    // "a parametric-curve key" and never learns the reference retired.
    for curve in ["exponential", "characteristic"] {
        let recipe = write_file(
            &tmp.path("curve.json"),
            &format!(
                r#"{{"calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}}}},
                    "reconstruction":{{"curve":{{"type":"{curve}","dmax":{{"explicit":1.4}}}}}}}}"#
            ),
        );
        let (code, _, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{curve}: {err}");
        assert!(
            err.contains("no longer a `reconstruction.curve` key")
                && err.contains("mid-at-base-offset"),
            "{curve}: the remedy must name the placement left: {err}"
        );
        // The losing rules, asserted absent: naming the key is not enough to tell two
        // rules apart when both mention it.
        assert!(
            !err.contains("unknown field `dmax`"),
            "{curve}: the generic unknown-field error won: {err}"
        );
        assert!(
            !err.contains("parametric-curve key"),
            "{curve}: the cross-variant rule won: {err}"
        );
    }

    // (c) both old spellings reach the same guidance through a **roll per-frame
    // override**, which is a separate load path.
    let shared = write_file(&tmp.path("shared.json"), ROLL_RECIPE);
    for (body, expect) in [
        (
            r#"{"film_base":{"source":{"explicit":[0.8,0.5,0.4]}}}"#,
            "calibration",
        ),
        (
            r#"{"reconstruction":{"curve":{"dmax":{"explicit":2.4}}}}"#,
            "no longer a `reconstruction.curve` key",
        ),
    ] {
        let manifest = write_file(
            &tmp.path("frames.json"),
            &format!(
                r#"{{ "frames": [ {{ "input": {scan:?}, "params": {body} }} ] }}"#,
                scan = scan.to_str().unwrap()
            ),
        );
        let (code, _, err) = run(&[
            "roll",
            "--frames",
            manifest.to_str().unwrap(),
            "--out-dir",
            tmp.path("out").to_str().unwrap(),
            "--params",
            shared.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{body} must fail up front: {err}");
        assert!(err.contains(expect), "{body}: {err}");
    }
}

/// A recipe carrying **only** a calibration renders exactly what the same values render
/// as flags, and a recipe carrying **no** calibration renders with the base from a flag.
///
/// The two halves of the split, asserted as bytes: a roll calibration and a pipeline
/// profile are separable files, which is what `core/recipe-composition` layers.
#[test]
fn a_calibration_only_recipe_matches_the_same_values_given_as_flags() {
    let tmp = TempDir::new("calibration-split");
    let scan = fixture("hdr-48bit.tif");
    let common = ["--output-preset", "display-p3"];

    // A calibration is "a recipe with nothing else".
    let calibration = write_file(
        &tmp.path("roll-cal.json"),
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}"#,
    );
    let from_recipe = tmp.path("recipe.tif");
    let (code, _, err) = {
        let mut a = vec!["convert", scan.to_str().unwrap(), "-o"];
        a.push(from_recipe.to_str().unwrap());
        a.extend_from_slice(&common);
        a.extend_from_slice(&["--params", calibration.to_str().unwrap()]);
        run(&a)
    };
    assert_eq!(code, 0, "a calibration-only recipe must convert: {err}");

    let from_flags = tmp.path("flags.tif");
    let (code, _, err) = {
        let mut a = vec!["convert", scan.to_str().unwrap(), "-o"];
        a.push(from_flags.to_str().unwrap());
        a.extend_from_slice(&common);
        a.extend_from_slice(&["--film-base", "0.9,0.55,0.42"]);
        run(&a)
    };
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        std::fs::read(&from_recipe).unwrap(),
        std::fs::read(&from_flags).unwrap(),
        "a calibration file and the same values as flags must render identically"
    );

    // A profile is "a recipe with no `calibration` section" — it needs a base from
    // somewhere, which is exactly why `calibration.film_base` has no default.
    // Pins every look value this build writes — anchor and `density.scale` included —
    // so the `--strict` assertion below is about the *absent calibration*, not about a
    // half-written curve.
    let profile = write_file(
        &tmp.path("look.json"),
        r#"{"reconstruction":{
              "curve":{"type":"exponential","gamma":2.0,
                       "anchor":{"mid-at-base-offset":0.62}},
              "density":{"scale":[1.0,0.84,0.73]}},
            "output":{"preset":"display-p3"}}"#,
    );
    let (code, _, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        tmp.path("profile.tif").to_str().unwrap(),
        "--params",
        profile.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "a profile plus a base flag must convert: {err}");

    // **…and under `--strict`.** A profile has no `calibration` section by definition,
    // so anything that treats an absent reference as "floating" makes the documented
    // look shape fail its own guide (`docs/using-nc.md` §4). `unpinned_curve` did,
    // briefly, after the reference moved: the recipe below pins every look value this
    // build would otherwise supply, which is the exact shape that must stay silent.
    let (code, _, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        tmp.path("profile-strict.tif").to_str().unwrap(),
        "--params",
        profile.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--strict",
    ]);
    assert_eq!(code, 0, "a profile must be --strict clean: {err}");
    assert!(
        !err.contains("leaves a value to this build's default"),
        "an absent `calibration` is a profile, not a floating reference: {err}"
    );

    // …and without the base it is refused, not guessed.
    let (code, _, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        tmp.path("unstated.tif").to_str().unwrap(),
        "--params",
        profile.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an unstated base must still be refused: {err}");
    assert!(err.contains("no film base selected"), "{err}");
}

/// An array-shaped `reconstruction.density` in a recipe is a usage error, and the object
/// spelling's stated gain reaches the report.
///
/// Through the binary because that is where the defect was reproduced: `DensityParams` is
/// a plain derive, so serde accepted the positional-array form, and while the gain's
/// default was per-curve a raw-object probe read the array as "not stated" and replaced
/// the stated `[1.2, 1.0, 0.8]` at **exit 0**. The per-curve default is gone
/// (`nf-retire/characteristic`), but the recipe's sections are still objects. The object
/// half is the control: it keeps the guard from over-correcting into "ignore a stated
/// scale".
#[test]
fn an_array_shaped_density_section_is_a_usage_error() {
    let tmp = TempDir::new("density-array");
    let recipe = |name: &str, density: &str| {
        write_file(
            &tmp.path(name),
            &format!(
                r#"{{"reconstruction":{{"type":"density","density":{density},
                     "curve":{{"type":"exponential"}}}},
                   "calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}}}},
                   "output":{{"preset":"display-p3"}}}}"#
            ),
        )
    };
    let array = recipe(
        "array.json",
        r#"[[1.2,1.0,0.8],[0,0,0],[0,0,0],[0,0,0],"auto"]"#,
    );
    let out = tmp.path("array.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        array.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an array-shaped `density` must exit 2: {err}");
    assert!(
        err.contains("reconstruction.density"),
        "the error must name the section: {err}"
    );
    assert!(!out.exists(), "no output on a usage error");

    // Control: the object spelling of the same gain converts and keeps it.
    let object = recipe("object.json", r#"{"scale":[1.2,1.0,0.8]}"#);
    let out = tmp.path("object.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        object.to_str().unwrap(),
        "--report",
        "json",
    ]);
    assert_eq!(
        code, 0,
        "the object spelling must convert:\n{stdout}\n{err}"
    );
    let scale = json(&stdout)["recipe"]["reconstruction"]["density"]["scale"]
        .as_array()
        .unwrap_or_else(|| panic!("no resolved density.scale in the report:\n{stdout}"))
        .iter()
        .map(|v| (v.as_f64().unwrap() * 1000.0).round() / 1000.0)
        .collect::<Vec<_>>();
    assert_eq!(
        scale,
        vec![1.2, 1.0, 0.8],
        "a stated gain must survive resolution:\n{stdout}"
    );
}

#[test]
fn convert_is_deterministic() {
    // The project's defining contract: same inputs + params ⇒ byte-identical
    // output. Convert the same fixture twice and compare the TIFF + sidecar — on
    // `film-master` (f32, no colour transform) and on `display-p3`, whose render runs
    // the lcms2 transform across rayon bands, the parallel path a nondeterminism would
    // most likely hide in.
    let tmp = TempDir::new("determinism");
    for preset in ["film-master", "display-p3"] {
        let args = |out: &Path| {
            vec![
                "convert".to_string(),
                fixture("hdri-64bit.tif").to_str().unwrap().to_string(),
                "-o".to_string(),
                out.to_str().unwrap().to_string(),
                "--output-preset".to_string(),
                preset.to_string(),
                "--film-base".to_string(),
                "0.9,0.55,0.42".to_string(),
                "--report".to_string(),
                "none".to_string(),
            ]
        };
        let a = tmp.path(&format!("{preset}-a.tiff"));
        let b = tmp.path(&format!("{preset}-b.tiff"));
        let (ca, _, _) = run(&args(&a).iter().map(String::as_str).collect::<Vec<_>>());
        let (cb, _, _) = run(&args(&b).iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!((ca, cb), (0, 0), "{preset}");
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "{preset}: output TIFF must be byte-identical across runs"
        );
        assert_eq!(
            std::fs::read(format!("{}.json", a.display())).unwrap(),
            std::fs::read(format!("{}.json", b.display())).unwrap(),
            "{preset}: sidecar recipe must be byte-identical across runs"
        );
    }
}

#[test]
fn a_sidecar_written_before_the_legacy_retirement_still_replays() {
    // Every sidecar the previous build wrote carried `output.depth` /
    // `output_profile` / `bigtiff` at their defaults. Replaying one — here on a
    // surviving preset — must render exactly as the current shape does, on `convert`
    // and as a `roll` per-frame override; only a non-default value is refused.
    let tmp = TempDir::new("old-sidecar");
    let input = fixture("hdr-48bit.tif");
    let old = write_file(
        &tmp.path("old.json"),
        r#"{"meta":{"nc_version":"0.1.0","pipeline_version":5},"params":{
            "calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"display-p3","depth":"u16","output_profile":null,"bigtiff":"auto"}}}"#,
    );
    let current = write_file(
        &tmp.path("current.json"),
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"display-p3"}}"#,
    );
    let convert = |recipe: &Path, out: &Path| {
        run(&[
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ])
    };
    let (a, b) = (tmp.path("old.tiff"), tmp.path("current.tiff"));
    let (code, _, err) = convert(&old, &a);
    assert_eq!(code, 0, "an old default sidecar must replay: {err}");
    let (code, _, err) = convert(&current, &b);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());

    // The same keys in a roll's per-frame override.
    let frames = write_file(
        &tmp.path("frames.json"),
        &serde_json::json!({"frames": [{
            "input": input.to_str().unwrap(),
            "params": {"output": {"depth": "u16", "output_profile": null, "bigtiff": "auto"}}
        }]})
        .to_string(),
    );
    let out_dir = tmp.path("roll");
    let (code, _, err) = run(&[
        "roll",
        "--frames",
        frames.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        current.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "an old default override must apply: {err}");

    // A non-default value asked for something no preset does by that name.
    let bad = write_file(
        &tmp.path("bad.json"),
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"display-p3","depth":"f32"}}"#,
    );
    let (code, _, err) = convert(&bad, &tmp.path("bad.tiff"));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("Remove the key"), "{err}");
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
        "--output-preset",
        "film-master",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
    // Check the actual stderr log marker (`hanten: decoded …`), not a bare "decoded"
    // substring — the JSON report legitimately carries a `transfer_decoded` field.
    assert!(
        !stdout.contains("hanten: decoded"),
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
        // A base must be stated since `film_base.source` has no default; the
        // rule under test is the in-place-output guard, not that one.
        "--auto-base",
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
        "--output-preset",
        "display-p3",
        // A base must be stated (no default); without it all three of these
        // conversions exit 2 on the missing-base gate and never reach the
        // collision check they exist to pin.
        "--film-base",
        "0.9,0.6,0.5",
        "--report-file",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "report over output must be a usage error: {err}");
    // Exit 2 alone cannot say *which* rule fired — that is how this test came to
    // pass on the missing-base gate instead. Pin the reason.
    assert!(
        !err.contains("no film base selected"),
        "must reach the collision check, not the film-base gate: {err}"
    );
    // --report-file == the automatic sidecar.
    let sidecar = dir.path("out.tiff.json");
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        // A base must be stated (no default); without it all three of these
        // conversions exit 2 on the missing-base gate and never reach the
        // collision check they exist to pin.
        "--film-base",
        "0.9,0.6,0.5",
        "--report-file",
        sidecar.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "report over sidecar must be a usage error: {err}");
    assert!(
        !err.contains("no film base selected"),
        "must reach the collision check, not the film-base gate: {err}"
    );
    // --report-file reaching the output through a `..` traversal (the target
    // doesn't exist yet, so canonicalizing the full path alone can't catch it).
    std::fs::create_dir_all(dir.path("sub")).unwrap();
    let dotted = dir.path("sub/../out.tiff");
    let (code, _, err) = run(&[
        "convert",
        fix.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        // A base must be stated (no default); without it all three of these
        // conversions exit 2 on the missing-base gate and never reach the
        // collision check they exist to pin.
        "--film-base",
        "0.9,0.6,0.5",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
            "calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"display-p3"}}"#,
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
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
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
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
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}"#,
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
fn roll_frame_report_makes_the_measurement_area_observable() {
    // `measure.inset` reaches the measurement region from the shared recipe *and*
    // from a per-frame override, so a roll frame has to report the resolved area —
    // otherwise the knob is accepted and its effect invisible, which is exactly the
    // defect reporting it unconditionally on `convert` exists to prevent.
    let tmp = TempDir::new("roll-area");
    let out_dir = tmp.path("out");
    let recipe = tmp.path("recipe.json");
    std::fs::write(
        &recipe,
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}"#,
    )
    .unwrap();
    let frames = tmp.path("frames.json");
    let src = fixture("hdr-48bit.tif");
    std::fs::write(
        &frames,
        format!(
            r#"{{"frames":[{{"input":{:?},"params":{{"measure":{{"inset":0.12}}}}}}]}}"#,
            src.to_str().unwrap()
        ),
    )
    .unwrap();
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        frames.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "roll should succeed: {err}");
    let report = json(&stdout);
    // The shared recipe keeps the default; the frame resolved the override. Both
    // numbers in one document is what makes the override falsifiable — a frame
    // echoing the shared value would look identical without the pair.
    assert_eq!(report["recipe"]["measure"]["inset"], 0.05);
    let area = &report["frames"][0]["effective_area"];
    assert_eq!(
        area["inset"], 55,
        "the per-frame inset must reach the region and be reported: {report}"
    );
    assert_eq!(
        area["region"][0], 55,
        "the reported rectangle must follow the resolved inset: {report}"
    );
}

#[test]
fn roll_rejects_colorimetric_shared_recipe_before_decode() {
    // M1: an unconditionally-unsupported shared assertion fails fast, before the
    // first (large) scan is decoded — exit 4 with an actionable message.
    let tmp = TempDir::new("roll-colorimetric");
    let out_dir = tmp.path("out");
    let recipe = tmp.path("recipe.json");
    // The shared recipe states a base (no default) so the rejection under test
    // is the colorimetric one, not the missing-base usage error.
    std::fs::write(
        &recipe,
        r#"{"input":{"meaning":"colorimetric"},"calibration":{"film_base":{"explicit":[0.9,0.6,0.5]}}}"#,
    )
    .unwrap();
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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

    assert_eq!(record["schema_version"], 7);
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
    assert_eq!(conv["preset"], "display-p3");
    // Schema 5 dropped the one-valued reconstruction type, and schema 7 the one-valued
    // curve.
    assert!(conv.get("reconstruction").is_none(), "{conv}");
    assert!(conv.get("curve").is_none(), "{conv}");
    assert!(conv["params_hash"].as_str().unwrap().len() == 16);
    assert_eq!(
        conv["film_base_source"]["explicit"],
        serde_json::json!([0.9, 0.55, 0.42])
    );
    assert_eq!(conv["output_depth"], "u16");

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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
                "--output-preset",
                "display-p3",
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
        assert_eq!(v["schema_version"], 7);
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
            "--output-preset",
            "display-p3",
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
            "--output-preset".to_string(),
            "display-p3".to_string(),
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
        "--output-preset",
        "film-master",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "display-p3",
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
            "--output-preset",
            "display-p3",
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
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        "-",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "telemetry to stdout should succeed:\n{err}");
    let record = json(&stdout);
    assert_eq!(record["schema_version"], 7);
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
            "--output-preset",
            "display-p3",
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
            "--output-preset",
            "display-p3",
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
        "--output-preset",
        "display-p3",
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
        "--output-preset",
        "film-master", // f32 never clips, so the IR-ignored warning is isolated
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
fn telemetry_params_hash_covers_the_curve() {
    // params_hash (over the effective recipe JSON) must cover the curve keys, so
    // tweaking one changes the hash.
    let tmp = TempDir::new("tel-curve");
    let fix = fixture("hdr-48bit.tif");
    let convert = |out: &Path, extra: &[&str]| -> serde_json::Value {
        let out = out.to_str().unwrap();
        let mut argv = vec![
            "convert",
            fix.to_str().unwrap(),
            "-o",
            out,
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.55,0.42",
            "--telemetry-file",
            "-",
            "--report",
            "none",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "telemetry should succeed:\n{err}");
        json(&stdout)
    };
    let ra = convert(&tmp.path("a.tiff"), &[]);
    let rb = convert(&tmp.path("b.tiff"), &["--density-gamma", "1.5"]);

    assert_ne!(
        ra["conversion"]["params_hash"], rb["conversion"]["params_hash"],
        "a changed curve knob must change params_hash"
    );
}

#[test]
fn convert_reports_the_default_curve_and_its_base_derived_anchor() {
    // The default render end to end: the report names the curve's placement and the
    // anchor it derived, and the sidecar recipe carries the curve.
    let tmp = TempDir::new("default-curve");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let curve = &report["reconstruction_result"]["curve"];
    assert!(curve.get("type").is_none(), "{curve}");
    assert!(
        curve["anchor"].get("mid-at-base-offset").is_some(),
        "{curve}"
    );
    // 0.62 + 0.745 / 2.0, the fixed decode's anchor.
    let anchor = curve["anchor_value"].as_f64().expect("anchor_value");
    assert!((anchor - 0.992_363_75).abs() < 1e-6, "{anchor}");
    assert_eq!(report["working_mapping"], "nc-film-rgb-v1");
    let recipe = sidecar_params(&out);
    assert_eq!(recipe["reconstruction"]["curve"]["gamma"], 2.0);
}

#[test]
fn auto_wb_reports_gains_that_reproduce_the_output_when_reused() {
    // The measure-once-reuse-for-the-roll contract, end to end: an `--auto-wb`
    // run reports the resolved gains, and a second run feeding them back through
    // the ordinary `--white-balance` flag must produce a byte-identical TIFF —
    // proving the auto gains are applied through the shared print controls' standard
    // slot, not a post-hoc multiply. f32 output (`hdr-linear-tiff`, which applies the print
    // controls) so the comparison covers full precision.
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
            "--output-preset".to_string(),
            "hdr-linear-tiff".to_string(),
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

// ---------------------------------------------------------------------------
// roll (batch) — convert N frames from one shared, frozen recipe
// ---------------------------------------------------------------------------

/// Write `contents` to `path`, returning the path (for building recipes /
/// manifests in a test's temp dir).
fn write_file(path: &Path, contents: &str) -> PathBuf {
    std::fs::write(path, contents).unwrap();
    path.to_path_buf()
}

/// A hand-authored frozen roll recipe: an explicit roll-fixed film base, so
/// every frame converts deterministically without auto-base (real scans are
/// holder → rebate → picture, where auto-base fails loudly).
/// The shared roll recipe these tests convert with.
///
/// It states `output.preset` explicitly because `roll` has **no** output-preset
/// flag — the recipe is the only place a roll can choose one — and because the
/// product default became `gain-map-hdr` (a JPEG) in `output/presets`. Every roll
/// test below asserts TIFF names, TIFF sidecars or byte-identical TIFF reruns, so
/// `legacy` is the preset they always meant; the container-aware naming these tests
/// would otherwise be silently retesting has its own coverage in
/// `roll_checks_explicit_manifest_suffixes_and_derives_per_frame_preset_names`.
const ROLL_RECIPE: &str = r#"{
  "calibration": {
    "film_base": { "explicit": [0.9, 0.55, 0.42] }
  },
  "reconstruction": {
    "type": "density",
    "curve": { "type": "exponential" }
  },
  "output": { "preset": "display-p3" }
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
    // The shared frozen recipe (roll-fixed Dmin) appears once, at the top.
    // f32 round-trips through JSON as f64, so compare the base approximately.
    let fb: Vec<f64> = report["recipe"]["calibration"]["film_base"]["explicit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect();
    assert!(
        (fb[0] - 0.9).abs() < 1e-6 && (fb[1] - 0.55).abs() < 1e-6 && (fb[2] - 0.42).abs() < 1e-6,
        "recipe film base: {fb:?}"
    );
    assert!(report["recipe"]["calibration"].get("dmax").is_none());
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
    // roll output byte-for-byte against the equivalent single `hanten convert`.
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
  "calibration": { "film_base": { "region": [0, 0, 502, 462] } },
  "output": { "preset": "display-p3" }
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
            && m.as_str().unwrap().contains("hanten estimate")),
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
                "params": {{ "calibration": {{ "film_base": {{ "explicit": [0.8, 0.5, 0.4] }} }}, "output": {{ "preset": "display-p3" }} }} }}
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
fn roll_failed_frame_keeps_a_warning_raised_before_the_failure() {
    // A frame that warns (a non-uniform `--base-region` sample, raised by the film-base
    // estimate) and *then* fails (its output path is an existing directory, so the write
    // fails after the render) still reports the earlier warning.
    let tmp = TempDir::new("roll-warn-then-fail");
    let recipe = write_file(
        &tmp.path("warn-then-fail.json"),
        r#"{ "calibration": { "film_base": { "region": [0, 0, 40, 40] } },
             "output": { "preset": "display-p3" } }"#,
    );
    let out = tmp.path("out");
    std::fs::create_dir_all(out.join("frame.tiff")).unwrap();
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {:?}, "output": "frame.tiff" }} ] }}"#,
            fixture("hdr-48bit.tif").to_str().unwrap()
        ),
    );
    let (code, stdout, _err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
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
            .any(|w| w.as_str().unwrap().contains("is not uniform")),
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

#[test]
fn a_roll_manifest_output_naming_the_out_dir_is_refused_not_written_beside_it() {
    // `"output": "."` is the natural manifest spelling for "put it in the
    // --out-dir", and `out_dir.join(".")` makes `<out-dir>/.`. `Path::file_name()`
    // normalises the `.` away, so completing it wrote `<out-dir>.jpg` — every frame
    // of the roll *outside* the directory the user named, at exit 0, with the report
    // agreeing and `ensure_roll_targets_distinct` unable to see it (it only compares
    // targets against each other). Refused at exit 2 instead.
    let tmp = TempDir::new("roll-out-dir-dot");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr = fixture("hdr-48bit.tif");
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {hdr:?}, "output": "." }} ] }}"#,
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
    assert_eq!(code, 2, "{stdout}\n{err}");
    assert!(err.contains("names a directory"), "{err}");
    // Attributed to the frame, with a remedy the manifest can act on — and *not*
    // the convert-shaped one, which would tell a reader already running
    // `hanten roll --out-dir` to use it. Asserting the losing wording is absent is
    // the only check that can tell the two arms apart.
    assert!(err.contains("frame "), "{err}");
    assert!(err.contains(hdr.to_str().unwrap()), "{err}");
    assert!(!err.contains("hanten roll --out-dir"), "{err}");
    // The file that used to appear beside the out-dir must not exist.
    assert!(
        !tmp.path("out.tiff").exists() && !tmp.path("out.jpg").exists(),
        "a sibling of the --out-dir was written: {err}"
    );

    // Falsifiable control: the same manifest with a file name works, and lands
    // *inside* the out-dir.
    let ok_manifest = write_file(
        &tmp.path("frames-ok.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {hdr:?}, "output": "frame" }} ] }}"#,
            hdr = hdr.to_str().unwrap(),
        ),
    );
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        ok_manifest.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}\n{err}");
    assert!(is_tiff(&out_dir.join("frame.tiff")), "{err}");
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
    // A steep slope and a low anchor are *reconstruction* controls (which the master
    // accepts), chosen so the placement pushes most samples well above 1.0 — that is
    // what makes the unclamped round-trip observable instead of vacuous. Value magnitudes only; no whole-file or post-transform checksum.
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
        // This asserts the master is *unclamped*, which needs samples above 1.0: a
        // steep slope with mid-grey just above the base puts the anchor at
        // `0.05 + 0.745/5 ≈ 0.2`, pushing plenty of content past it.
        "--density-gamma",
        "5",
        "--anchor-mid-offset",
        "0.05",
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
    let anchor = report["reconstruction_result"]["curve"]["anchor_value"]
        .as_f64()
        .expect("the derived anchor is reported");
    assert!((anchor - 0.198_945_5).abs() < 1e-5, "{anchor}");
    assert!(
        report["reconstruction_result"]["curve"]
            .get("dmax")
            .is_none()
    );
    assert!(report.get("dmax").is_none());
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
    // Every rejection the master owes the user, through the real binary: each
    // non-default downstream control. All are usage errors (exit 2) — never a
    // quietly-adjusted or quietly-unadjusted image.
    //
    // Each `expect` is a phrase distinctive to *this* rule, so it would not stay green
    // if the rule it names disappeared.
    let tmp = TempDir::new("film-master-reject");
    let input = fixture("hdri-64bit.tif");
    let base = ["--film-base", "0.9,0.55,0.42"];
    for (extra, expect) in [
        (vec!["--print-exposure", "0.5"], "print_exposure"),
        (vec!["--black-point", "0.01"], "black_point"),
        (vec!["--white-balance", "1.05,1,0.93"], "white_balance"),
        (vec!["--auto-wb", "percentile"], "white_balance"),
        (
            vec!["--display-tone-headroom", "3"],
            "fit_range.headroom_stops",
        ),
        (vec!["--linear-range", "0.02,0.97"], "linear_range"),
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
}

#[test]
fn film_master_embeds_its_own_icc_distinct_from_the_display_presets() {
    // The written master's ICC tag is what tells a downstream tool the pixels are
    // linear ACEScg. That it *is* the ACEScg profile is pinned in-process
    // (`stages`' film-master test compares it with `icc_profile(AcesCg)`); what only
    // the binary can show is that the tag reaches the file and is not a display
    // profile — two runs of one build, never a checked-in ICC hash (lcms2's bytes
    // differ per target).
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
    let icc = read_icc_tag(&master);
    assert!(
        icc.len() > 100,
        "an ICC profile must be embedded: {}",
        icc.len()
    );
    for display in ["display-p3", "compatibility"] {
        let other = convert(&format!("{display}.tiff"), &["--output-preset", display]);
        assert_ne!(
            icc,
            read_icc_tag(&other),
            "the master must not carry {display}'s profile"
        );
    }
}

#[test]
fn film_master_ir_sidecar_follows_the_preset_depth_and_carries_the_plane() {
    // `--export-ir` writes the sidecar at `OutputParams::depth()`, so under
    // `film-master` it flips 16-bit → f32 even though `output.hdr` stays at its
    // default. Correct by construction (one depth for the whole run), but it is a
    // user-visible container change, so pin it — together with the Step-1 rule that
    // the IR plane is *carried*, never converted: the f32 sidecar's samples must equal
    // a 16-bit preset's sidecar, up to u16 quantization.
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

    // A 16-bit preset: a 16-bit unsigned-integer IR sidecar.
    let sdr_ir = convert("sdr", &["--output-preset", "display-p3"]);
    let (sdr_bits, sdr_format, sdr_samples) = read_gray_tiff(&sdr_ir);
    assert_eq!(
        (sdr_bits, sdr_format),
        (16, 1),
        "a 16-bit preset's IR sidecar is 16-bit unsigned integer"
    );
    let GraySamples::U16(sdr_u16) = sdr_samples else {
        panic!("the display-p3 IR sidecar must be u16, got {sdr_samples:?}");
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
    assert_eq!(master_f32.len(), sdr_u16.len());
    for (i, (&f, &q)) in master_f32.iter().zip(&sdr_u16).enumerate() {
        let requantized = (f.clamp(0.0, 1.0) * 65535.0).round() as u16;
        assert!(
            requantized.abs_diff(q) <= 1,
            "IR sample {i}: f32 {f} requantizes to {requantized}, the u16 sidecar was {q}"
        );
    }
}

#[test]
fn film_master_telemetry_names_the_preset_and_the_written_depth() {
    // The record's `conversion.preset` is what distinguishes a master from a display
    // run, and `conversion.output_depth` says which depth was written — which under
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
    assert_eq!(record["schema_version"], 7);
    assert_eq!(conv["preset"], "film-master");
    assert_eq!(
        conv["output_depth"], "f32",
        "the master writes f32, so the record's depth must say so: {conv}"
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

    // …and a 16-bit preset on the same fixture reports `u16`, so the assertion above
    // is about the preset and not a constant.
    let sdr_out = tmp.path("sdr.tiff");
    let sdr_rec = tmp.path("sdr.json");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        sdr_out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--telemetry-file",
        sdr_rec.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
    let sdr: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sdr_rec).unwrap()).unwrap();
    assert_eq!(sdr["conversion"]["preset"], "display-p3");
    assert_eq!(sdr["conversion"]["output_depth"], "u16");
}

#[test]
fn film_master_content_names_the_placement_it_made() {
    // The master's reported `content` names what placed mid-grey: the curve's
    // film-base-derived anchor — never a reference density that no longer exists.
    let tmp = TempDir::new("film-master-content");
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

    let report = convert("default.tiff", &[]);
    let content = report["output_render"]["content"].as_str().unwrap();
    assert!(content.contains("film-base-derived anchor"), "{content}");
    assert!(content.contains("not a physical scene-linear"), "{content}");
    assert!(!content.contains("Dmax"), "{content}");
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

    // An unknown name is a typo, and the diagnosis lists every accepted preset — and
    // only those: the retired `custom` is not advertised back to a user who misspelt
    // it.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "custome",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("unknown output preset"), "{err}");
    assert!(err.contains("`display-p3`"), "{err}");
    assert!(!err.contains("`custom`"), "{err}");
}

#[test]
fn roll_accepts_a_film_master_recipe() {
    // `hanten roll` has no output flags at all — its output policy comes only from the
    // shared recipe — so `output.preset` must be honoured there too, and the
    // automatic `<stem>_positive.tiff` name is already correct for the master's TIFF
    // container. (Preset-aware suffix resolution stays with `output/presets`.)
    let tmp = TempDir::new("roll-film-master");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
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
    // `output.preset` is roll-fixed like `film_base` and `reconstruction.curve.anchor`,
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
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "output":{"preset":"film-master"}}"#,
    );
    let manifest_for = |name: &str, body: &str| -> PathBuf { write_file(&tmp.path(name), body) };
    let overridden = manifest_for(
        "frames.json",
        &format!(
            r#"{{ "frames": [
                 {{ "input": {i:?}, "output": "master.tiff" }},
                 {{ "input": {i:?}, "output": "downgraded.tiff",
                    "params": {{ "output": {{ "preset": "display-p3" }} }} }}
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

/// The convert invocation the identity tests share: a `display-p3` TIFF (stated, so
/// the preset is visible at the one place that picks it), an explicit film base (so
/// nothing is estimated per frame), and a clean stdout report.
fn convert_p3(input: &Path, out: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args = vec![
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
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
    let (code, stdout, err) = convert_p3(&fixture("hdri-64bit.tif"), &out, &[]);
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
fn recipe_dumped_by_this_build_replays_clean_under_strict() {
    // The documented reproducibility path is `--dump-params` → replay, and it must
    // survive `--strict`. This gate exists because the moved-default curve warning
    // broke it three separate times: the predicate was tuned against hand-written
    // JSON each round while nothing checked the one file the tool itself writes.
    // The output being byte-identical is what makes the failure unambiguous — a
    // warning claiming the render moved, on a render that provably did not.
    //
    // The IR-free fixture is required: `hdri-64bit.tif` emits the "IR preserved but
    // not used" warning on every frame, which would fail `--strict` here no matter
    // what the curve warning did.
    let tmp = TempDir::new("dumpreplay");
    let first = tmp.path("first.tiff");
    let dump = tmp.path("params.json");
    let (code, _, err) = convert_p3(
        &fixture("hdr-48bit.tif"),
        &first,
        &["--dump-params", dump.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{err}");

    let replay = tmp.path("replay.tiff");
    let (code, _, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        replay.to_str().unwrap(),
        "--params",
        dump.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(
        code, 0,
        "a recipe this build just dumped must replay clean under --strict; stderr:\n{err}"
    );
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&replay).unwrap(),
        "the replay must be byte-identical, or the warning had a point"
    );

    // Falsifiable: the same replay of a recipe that genuinely leaves the curve
    // unpinned — a shape this build never writes — still fails.
    let bare = tmp.path("bare.json");
    std::fs::write(
        &bare,
        r#"{"reconstruction":{"type":"density"},"output":{"preset":"display-p3"}}"#,
    )
    .unwrap();
    let (code, _, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("bare.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        bare.to_str().unwrap(),
        "--strict",
    ]);
    assert_ne!(code, 0, "a curve-less recipe must still warn");
    assert!(err.contains("reconstruction.curve"), "{err}");
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
    let (code, stdout, err) = convert_p3(
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
    let (c2, s2, _) = convert_p3(
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
    let (ca, _, err) = convert_p3(
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
             "params": { "print": { "print_exposure": 0.25 },
                          "output": { "preset": "display-p3" } } }"#,
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
                      "params": {{ "calibration": {{ "film_base": {{ "explicit": [0.9, 0.55, 0.42] }} }}, "output": {{ "preset": "display-p3" }} }} }}"#
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
                      "params": {{ "calibration": {{ "film_base": {{ "explicit": [0.9, 0.55, 0.42] }} }}, "output": {{ "preset": "display-p3" }} }} }}"#
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
    let (code, stdout, err) = convert_p3(&fixture("hdri-64bit.tif"), &out, &[]);
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

    // f32 output is written verbatim, so the mean is reported in *that* domain
    // (unclamped) — the reason `nctool compare` records the depth beside the mean.
    let master = tmp.path("master.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        master.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(
        report["output_stats"]["mean"].as_array().map(Vec::len),
        Some(3),
        "output_stats must be reported for an f32 output too: {report}"
    );

    // A blown-out render ties the two report fields together: the clamped samples
    // the mean is taken over are the same ones `loss` counts. The display tone
    // overshoots display white by design, so its loss reaches the u16 encode.
    let clipped = tmp.path("clipped.tiff");
    let (code, stdout, err) = convert_p3(
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
                            "curve": { "type": "exponential" } },
                         "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } } }"#,
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
             "params": { "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } },
                          "output": { "preset": "display-p3" } } }"#,
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
            "--output-preset",
            "display-p3",
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
                  "params": {{ "calibration": {{ "film_base": {{ "explicit": [0.9, 0.55, 0.42] }} }}, "output": {{ "preset": "display-p3" }} }} }}"#
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
    let (code, _, err) = convert_p3(
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
            "--output-preset",
            "display-p3",
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

// ---------------------------------------------------------------------------
// Memory preflight (`io/memory-preflight`)
// ---------------------------------------------------------------------------

#[test]
fn memory_preflight_reports_the_estimate_and_budget_decision() {
    // Every command that decodes reports what the preflight decided, with the
    // per-phase breakdown behind the number the gate compared.
    let tmp = TempDir::new("mem-report");
    let out = tmp.path("out.tiff");
    let (code, stdout, _err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0);
    let mem = json(&stdout)["memory"].clone();
    assert_eq!(mem["budget_source"], "default");
    assert_eq!(mem["budget_bytes"], 6u64 * 1024 * 1024 * 1024);
    assert_eq!(mem["decision"], "ok");
    let peak = mem["estimated_peak_bytes"].as_u64().unwrap();
    let accounted = mem["accounted_bytes"].as_u64().unwrap();
    assert!(peak > accounted, "the estimate includes the allowance");
    // The full-pipeline profile sizes all four phases. Which one peaks is per profile
    // (`memory`'s `which_phase_peaks_is_per_profile_and_measured_not_assumed`): for the
    // SDR TIFF profile it is the render, where the display source and the rendition
    // coexist, with the encode below it. The film-base phase — one image, since an
    // explicit `--film-base` samples nothing — is below both.
    assert!(mem["decode_bytes"].as_u64().unwrap() > 0);
    assert_eq!(accounted, mem["render_bytes"].as_u64().unwrap());
    assert!(mem["encode_bytes"].as_u64().unwrap() < accounted, "{mem}");
    let film_base = mem["film_base_bytes"].as_u64().unwrap();
    assert!(
        film_base > 0 && film_base < accounted,
        "film-base phase must be sized and below the render peak: {mem}"
    );

    // `inspect` gates on the decode-only profile — no render, no encode. It runs
    // auto detection, so its peak is the film-base phase (the decoded image plus the
    // sampled interior), *above* the decode phase.
    let (code, stdout, _err) = run(&["inspect", fixture("hdri-64bit.tif").to_str().unwrap()]);
    assert_eq!(code, 0);
    let mem = json(&stdout)["memory"].clone();
    assert_eq!(mem["render_bytes"], 0);
    assert_eq!(mem["encode_bytes"], 0);
    assert_eq!(mem["accounted_bytes"], mem["film_base_bytes"]);
    assert!(
        mem["film_base_bytes"].as_u64().unwrap() > mem["decode_bytes"].as_u64().unwrap(),
        "the auto interior sample must be counted: {mem}"
    );

    // `estimate` reports the same block on the same profile — and its sampling plan
    // reaches the model rather than being a constant: the film-base term scales
    // with the rectangle actually sampled.
    let (code, stdout, err) = run(&[
        "estimate",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--base-region",
        "0,0,60,60",
    ]);
    assert_eq!(code, 0, "{err}");
    let small = json(&stdout)["memory"].clone();
    assert_eq!(small["budget_source"], "default");
    assert_eq!(small["decision"], "ok");
    assert_eq!(small["render_bytes"], 0);
    assert_eq!(small["encode_bytes"], 0);
    assert_eq!(small["accounted_bytes"], small["decode_bytes"]);

    let (code, stdout, err) = run(&[
        "estimate",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--grid",
    ]);
    assert_eq!(code, 0, "{err}");
    let grid = json(&stdout)["memory"].clone();
    // `--grid` over the whole frame samples five cells one at a time, so it is
    // charged for one cell (~1/16 of the frame) — more than a 60x60 rectangle, but
    // far less than the whole frame, and on a fixture this small still under
    // decode's 18 B/px. (An earlier model charged the whole enclosing rectangle,
    // a ~16x over-count that made this phase the peak here.)
    assert!(
        grid["film_base_bytes"].as_u64().unwrap() > small["film_base_bytes"].as_u64().unwrap(),
        "a whole-frame grid must cost more than a 60x60 rectangle:\n{grid}\n{small}"
    );
    let whole_frame_sample = 12 * 502 * 462; // if it charged the whole rectangle
    assert!(
        grid["film_base_bytes"].as_u64().unwrap()
            < grid["decode_bytes"].as_u64().unwrap() + whole_frame_sample,
        "a grid cell must cost far less than the whole rectangle:\n{grid}"
    );
    assert_eq!(
        grid["accounted_bytes"].as_u64().unwrap(),
        grid["decode_bytes"]
            .as_u64()
            .unwrap()
            .max(grid["film_base_bytes"].as_u64().unwrap()),
        "accounted is the max over phases:\n{grid}"
    );
}

#[test]
fn over_budget_convert_is_rejected_before_decoding_with_exit_six() {
    // The gate must fire *before* the pipeline allocates or writes anything: exit
    // 6 (resource), a message naming both numbers, and no output file / sidecar
    // left behind.
    let tmp = TempDir::new("mem-reject");
    let out = tmp.path("out.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--max-memory",
        "1MiB",
    ]);
    assert_eq!(code, 6, "over-budget must exit 6:\n{stdout}\n{err}");
    assert!(err.contains("resource:"), "{err}");
    assert!(err.contains("--max-memory"), "{err}");
    assert!(err.contains("1.0 MiB"), "message names the budget:\n{err}");
    assert!(
        err.contains("estimated peak"),
        "message names the estimate:\n{err}"
    );
    assert!(!out.exists(), "no output image may be written");
    assert!(
        !PathBuf::from(format!("{}.json", out.display())).exists(),
        "no sidecar may be written"
    );
    assert!(
        stdout.is_empty(),
        "a rejected run emits no report:\n{stdout}"
    );
}

/// A **header-only** classic TIFF: IFD0 advertises `width`x`height` 16-bit RGB in
/// one strip, and no strip data is written at all. `probe` reads tags only, so it
/// reports the advertised shape; anything that actually decodes fails. The file is
/// ~130 bytes whatever the advertised dimensions, which is what makes an
/// "oversized input" test fast and portable.
fn write_header_only_rgb16_tiff(path: &std::path::Path, width: u32, height: u32) {
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    // 9 entries: dimensions, bits/sample, compression, photometric, strip offsets,
    // samples/pixel, rows/strip, strip byte counts (ascending tag order).
    let ifd_end = 8 + 2 + 9 * 12 + 4; // IFD0 starts at 8
    let bits_offset = ifd_end as u32; // [16, 16, 16] doesn't fit in 4 bytes
    let data_offset = bits_offset + 6;

    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(b"II"); // little-endian
    b.extend_from_slice(&42u16.to_le_bytes());
    b.extend_from_slice(&8u32.to_le_bytes()); // offset of IFD0
    b.extend_from_slice(&9u16.to_le_bytes()); // entry count
    let entry = |tag: u16, ty: u16, count: u32, value: u32, b: &mut Vec<u8>| {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        // Little-endian: a SHORT value sits in the low two bytes of the field.
        b.extend_from_slice(&value.to_le_bytes());
    };
    entry(256, LONG, 1, width, &mut b); // ImageWidth
    entry(257, LONG, 1, height, &mut b); // ImageLength
    entry(258, SHORT, 3, bits_offset, &mut b); // BitsPerSample
    entry(259, SHORT, 1, 1, &mut b); // Compression = none
    entry(262, SHORT, 1, 2, &mut b); // PhotometricInterpretation = RGB
    entry(273, LONG, 1, data_offset, &mut b); // StripOffsets
    entry(277, SHORT, 1, 3, &mut b); // SamplesPerPixel
    entry(278, LONG, 1, height, &mut b); // RowsPerStrip (one strip)
    entry(279, LONG, 1, 6, &mut b); // StripByteCounts (deliberately short)
    b.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    assert_eq!(b.len(), ifd_end);
    b.extend_from_slice(
        &[
            16u16.to_le_bytes(),
            16u16.to_le_bytes(),
            16u16.to_le_bytes(),
        ]
        .concat(),
    );
    std::fs::write(path, &b).unwrap();
}

#[test]
fn an_oversized_header_is_rejected_while_the_heap_is_still_empty() {
    // The central claim of `io/memory-preflight`: the gate runs *before* the large
    // allocation. A header-only TIFF advertising 100000x100000 RGB16 (a 30 GB
    // convert peak) with no pixel data is what discriminates the two orderings:
    // `probe` reads tags only and succeeds, so a preflight *before* decode rejects
    // it with the resource error (exit 6) having allocated nothing, whereas a gate
    // placed after decode would have to try the read first and would surface a
    // decode/limits error (exit 3) instead — or OOM.
    let tmp = TempDir::new("mem-header-only");
    let input = tmp.path("oversized.tif");
    write_header_only_rgb16_tiff(&input, 100_000, 100_000);
    assert!(
        std::fs::metadata(&input).unwrap().len() < 1024,
        "the oversized input must stay a tiny file"
    );
    let out = tmp.path("out.tiff");

    let (code, stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(
        code, 6,
        "an oversized header must be rejected as a resource error, not decoded:\n{stdout}\n{err}"
    );
    assert!(err.contains("resource:"), "{err}");
    assert!(err.contains("100000x100000"), "{err}");
    assert!(!out.exists(), "nothing may be written");
    assert!(
        stdout.is_empty(),
        "a rejected run emits no report:\n{stdout}"
    );

    // Same file, same command, with a budget large enough to admit the estimate:
    // now the run gets as far as the decode, which fails on the absent pixel data.
    // That is the proof the exit 6 above came from the preflight rather than from
    // the file being unreadable.
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--max-memory",
        "512GiB",
    ]);
    assert_ne!(
        code, 6,
        "with room in the budget the gate must not fire:\n{err}"
    );
    assert!(!out.exists());
}

#[test]
fn roll_reports_the_preflight_decision_per_frame() {
    // Frames can differ in dimensions (so in estimated peak) under one shared
    // budget, and the gate runs per frame — so the decision is reported per frame,
    // not once for the roll.
    let tmp = TempDir::new("mem-roll-report");
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
        "--max-memory",
        "2GiB",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let frames = report["frames"].as_array().expect("frames array");
    assert_eq!(frames.len(), 2);
    for frame in frames {
        let mem = &frame["memory"];
        assert_eq!(mem["budget_source"], "flag");
        assert_eq!(mem["budget_bytes"], 2u64 * 1024 * 1024 * 1024);
        assert_eq!(mem["decision"], "ok");
        assert!(mem["estimated_peak_bytes"].as_u64().unwrap() > 0);
    }
    // The HDRi frame carries an IR plane, so it must estimate above the HDR one.
    let hdr = frames[0]["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    let hdri = frames[1]["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        hdri > hdr,
        "the IR-carrying frame must estimate higher ({hdri} vs {hdr})"
    );
}

#[test]
fn roll_gates_each_frame_against_the_shared_budget() {
    // The gate is per frame, not per roll: a budget between the two frames'
    // estimates must convert the smaller one and fail only its sibling — with the
    // sibling's resource error in its own frame entry, and the roll still exiting
    // non-zero.
    let tmp = TempDir::new("mem-roll-mixed");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let hdr_in = fixture("hdr-48bit.tif");
    let hdri_in = fixture("hdri-64bit.tif");

    // Read both estimates from a roll that fits, rather than hardcoding fixture
    // arithmetic that would rot with the model.
    let probe_dir = tmp.path("probe");
    let (code, stdout, err) = run(&[
        "roll",
        hdr_in.to_str().unwrap(),
        hdri_in.to_str().unwrap(),
        "--out-dir",
        probe_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let frames = json(&stdout)["frames"].as_array().unwrap().clone();
    let small = frames[0]["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    let large = frames[1]["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    assert!(small < large);
    let between = ((small + large) / 2).to_string();

    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        hdr_in.to_str().unwrap(),
        hdri_in.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--max-memory",
        &between,
    ]);
    assert_ne!(code, 0, "the over-budget frame must fail the roll:\n{err}");
    let report = json(&stdout);
    assert_eq!(report["summary"]["succeeded"], 1);
    assert_eq!(report["summary"]["failed"], 1);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames[0]["status"], "ok");
    assert_eq!(frames[1]["status"], "failed");
    let error = frames[1]["error"].as_str().unwrap();
    assert!(
        error.contains("resource:") && error.contains("estimated peak"),
        "the failed frame must carry its own resource error: {error}"
    );
    // The frame that fitted was written; its sibling was not.
    assert!(
        out_dir.join("hdr-48bit_positive.tiff").exists(),
        "the in-budget frame must still be converted"
    );
    assert!(
        !out_dir.join("hdri-64bit_positive.tiff").exists(),
        "the over-budget frame must write nothing"
    );
}

#[test]
fn over_budget_rejection_covers_inspect_estimate_and_roll() {
    // All four decoding commands are gated, each on its own profile.
    let tmp = TempDir::new("mem-reject-all");
    let input = fixture("hdri-64bit.tif");
    let in_str = input.to_str().unwrap();

    let (code, _out, err) = run(&["inspect", in_str, "--max-memory", "1KiB"]);
    assert_eq!(code, 6, "inspect must be gated too:\n{err}");
    let (code, _out, err) = run(&["estimate", in_str, "--max-memory", "1KiB"]);
    assert_eq!(code, 6, "estimate must be gated too:\n{err}");

    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let out_dir = tmp.path("out");
    let (code, _out, err) = run(&[
        "roll",
        in_str,
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--max-memory",
        "1KiB",
    ]);
    // Roll gates per frame; a frame that fails the preflight fails the roll (its
    // own exit code is the roll-level "frames failed" error, not exit 6).
    assert_ne!(code, 0, "an over-budget frame must fail the roll:\n{err}");
    assert!(
        err.contains("resource:") || err.contains("estimated peak"),
        "the frame's resource error must surface:\n{err}"
    );
}

#[test]
fn decode_only_commands_pass_a_budget_that_rejects_the_full_pipeline() {
    // The per-profile gate is not cosmetic: a budget between the decode-only and
    // full-pipeline estimates must admit `inspect` while rejecting `convert`.
    let tmp = TempDir::new("mem-profile");
    let input = fixture("hdri-64bit.tif");
    let in_str = input.to_str().unwrap();

    // Read the two estimates from the reports themselves rather than hardcoding
    // fixture-size arithmetic that would rot with the model.
    let (_c, stdout, _e) = run(&["inspect", in_str]);
    let decode_only = json(&stdout)["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    let out = tmp.path("out.tiff");
    let (_c, stdout, _e) = run(&[
        "convert",
        in_str,
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    let full = json(&stdout)["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        decode_only < full,
        "decode-only ({decode_only}) must estimate below the full pipeline ({full})"
    );

    // A budget in between: inspect proceeds, convert is rejected.
    let between = (decode_only + full) / 2;
    let budget = between.to_string();
    let (code, _out, err) = run(&["inspect", in_str, "--max-memory", &budget]);
    assert_eq!(code, 0, "inspect fits the in-between budget:\n{err}");
    let out2 = tmp.path("out2.tiff");
    let (code, _out, err) = run(&[
        "convert",
        in_str,
        "-o",
        out2.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--max-memory",
        &budget,
    ]);
    assert_eq!(code, 6, "convert exceeds the in-between budget:\n{err}");
    assert!(!out2.exists());
}

#[test]
fn max_memory_is_operational_not_a_recipe_key() {
    // Like `--report`/`--strict`/`--telemetry`: the budget must not enter the
    // recipe, must not appear in the sidecar, and must not change a single output
    // byte. A recipe *carrying* the key must be rejected (`deny_unknown_fields`).
    let tmp = TempDir::new("mem-not-recipe");
    let input = fixture("hdri-64bit.tif");
    let in_str = input.to_str().unwrap();

    let plain = tmp.path("plain.tiff");
    let budgeted = tmp.path("budgeted.tiff");
    for (out, extra) in [
        (&plain, Vec::new()),
        (&budgeted, vec!["--max-memory", "3GiB"]),
    ] {
        let mut args = vec![
            "convert",
            in_str,
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.55,0.42",
        ];
        args.extend_from_slice(&extra);
        let (code, _out, err) = run(&args);
        assert_eq!(code, 0, "{err}");
    }
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&budgeted).unwrap(),
        "--max-memory must not perturb the output image"
    );
    let sidecar = std::fs::read_to_string(format!("{}.json", budgeted.display())).unwrap();
    // Only the key itself: a bare `contains("memory")` over the whole sidecar would
    // fail on any future recipe key that merely has the substring in its name.
    assert!(
        !sidecar.contains("max_memory"),
        "the budget must not appear in the effective recipe:\n{sidecar}"
    );

    // …and it is not accepted as a recipe key.
    let recipe = write_file(
        &tmp.path("bad.json"),
        r#"{"max_memory": 4294967296, "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}}}"#,
    );
    let out = tmp.path("nope.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        in_str,
        "-o",
        out.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an unknown recipe key is a usage error:\n{err}");
    assert!(!out.exists());
}

#[test]
fn malformed_max_memory_is_a_usage_error() {
    let tmp = TempDir::new("mem-bad-flag");
    let out = tmp.path("out.tiff");
    for bad in ["0", "lots", "4.5GiB", "12PiB"] {
        let (code, _stdout, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.55,0.42",
            "--max-memory",
            bad,
        ]);
        assert_eq!(
            code, 2,
            "--max-memory {bad:?} must be a usage error:\n{err}"
        );
        assert!(!out.exists());
    }
}

#[test]
fn convert_requires_a_stated_film_base_but_estimate_does_not() {
    // The contract this PR introduces, end to end at the binary boundary.
    let tmp = TempDir::new("stated-base");
    let out = tmp.path("out.tif");
    let scan = fixture("hdr-48bit.tif");

    // convert with no base: usage error (exit 2), before anything is written.
    let (code, _stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
    ]);
    assert_eq!(
        code, 2,
        "an unstated film base must be a usage error: {err}"
    );
    assert!(err.contains("no film base selected"), "stderr: {err}");
    assert!(
        !out.exists(),
        "nothing may be written on the fast-fail path"
    );

    // The same run with a stated base gets past the gate. (This fixture is
    // synthetic and has no rebate band, so `--auto-base` would legitimately fail
    // in the *detector*; an explicit base isolates the gate under test.)
    let (code, _stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.6,0.5",
    ]);
    assert_eq!(code, 0, "a stated base must convert: {err}");
    assert!(out.exists());

    // `estimate` exists to *produce* a base, so it must not require one —
    // otherwise the documented "measure once, reuse" workflow is circular. It
    // resolves the unstated source to `auto` and reaches the detector, which on
    // this rebate-less fixture fails on its own merits (exit 1, not exit 2).
    let (code, _stdout, err) = run(&["estimate", scan.to_str().unwrap()]);
    assert_ne!(
        code, 2,
        "estimate must not demand a base it is being asked to measure: {err}"
    );
    assert!(
        !err.contains("no film base selected"),
        "estimate must not emit the convert-only requirement: {err}"
    );
}

#[test]
fn roll_requires_a_stated_film_base_and_says_so_in_roll_terms() {
    // `roll` converts, so it must state a base too — but `RollArgs` accepts none
    // of the three film-base flags, so the diagnosis has to point at the shared
    // `--params` recipe. A message naming `--auto-base` here would be advice the
    // user cannot follow (that flag exits 2 on `roll`).
    let tmp = TempDir::new("roll-stated-base");
    let out_dir = tmp.path("out");
    let scan = fixture("hdr-48bit.tif");

    let (code, _stdout, err) = run(&[
        "roll",
        scan.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 2,
        "roll with no stated film base must be a usage error: {err}"
    );
    assert!(err.contains("no film base selected"), "stderr: {err}");
    assert!(
        err.contains("--params") && err.contains("calibration.film_base"),
        "roll's message must send the user to the shared recipe: {err}"
    );
    // The flags it does not have must not be offered as the way out. (`--base-region`
    // does appear, but only inside the recommended `hanten estimate` invocation — a
    // different command, which accepts it.)
    assert!(
        !err.contains("--auto-base") && !err.contains("--film-base"),
        "roll must not advise flags it rejects: {err}"
    );
    assert!(
        !out_dir.exists(),
        "nothing may be written on the fast-fail path"
    );

    // Falsifiable control: the same invocation with a recipe carrying
    // `film_base.source` gets past the gate and converts.
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{"calibration": {"film_base": {"explicit": [0.9, 0.6, 0.5]}},
            "output": {"preset": "display-p3"}}"#,
    );
    let (code, stdout, err) = run(&[
        "roll",
        scan.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "a stated base must convert:\n{stdout}\n{err}");
    assert!(out_dir.join("hdr-48bit_positive.tiff").exists());
}

#[test]
fn roll_reports_the_specific_problem_before_the_missing_base() {
    // Ordering, not just correctness: `validate`'s own policy is
    // least-specific-diagnosis-last, and "no film base selected" is the least
    // specific diagnosis there is. A recipe that is *both* baseless and
    // roll-invalid must name the roll-invalid setting, or the user adds a base
    // only to be told about a second, unrelated problem.
    let tmp = TempDir::new("roll-order");
    let out_dir = tmp.path("out");
    let recipe = tmp.path("recipe.json");
    // Baseless AND colorimetric — two independent reasons to refuse.
    std::fs::write(&recipe, r#"{"input":{"meaning":"colorimetric"}}"#).unwrap();
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 4, "the colorimetric rejection must win: {err}");
    assert!(
        !err.contains("no film base selected"),
        "the least-specific diagnosis must not pre-empt the specific one: {err}"
    );
}

#[test]
fn a_suffix_mismatch_outranks_the_missing_base() {
    // Same least-specific-diagnosis-last policy as the roll gate: the output
    // path's suffix is a property of *this invocation*, while "no film base
    // selected" is the least specific diagnosis available. A run that is wrong
    // both ways must name the suffix, or the user supplies a base only to be
    // told the path was never going to work.
    let tmp = TempDir::new("suffix-order");
    let bad = tmp.path("out.jpg");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        bad.to_str().unwrap(),
        "--output-preset",
        "hdr-pq",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".avif"), "the suffix rule must win: {err}");
    assert!(
        !err.contains("no film base selected"),
        "the least-specific diagnosis must not pre-empt it: {err}"
    );
    assert!(!bad.exists());
}

#[test]
fn sdr_presets_write_lossless_16_bit_tiffs_through_the_modern_pipeline() {
    // The two SDR presets: same render, different destination gamut, both 16-bit
    // integer TIFF (lossless) and both `convert`-only.
    let tmp = TempDir::new("sdr-presets");
    let scan = fixture("hdr-48bit.tif");

    for (preset, encoding) in [
        ("compatibility", "srgb-u16-tiff"),
        ("display-p3", "display-p3-u16-tiff"),
    ] {
        let out = tmp.path(&format!("{preset}.tiff"));
        let (code, stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "0.9,0.6,0.5",
        ]);
        assert_eq!(code, 0, "{preset}: {err}");
        assert!(is_tiff(&out), "{preset} must write a TIFF");

        let report = json(&stdout);
        assert_eq!(report["output_render"]["preset"], preset);
        assert_eq!(report["output_render"]["encoding"], encoding);
        // Both run the shared print controls *and* a display render — that is what
        // separates them from the legacy TIFF path, which does neither.
        assert_eq!(report["output_render"]["print_controls"], true);
        assert_eq!(report["output_render"]["display_render"], true);
        assert_eq!(report["output_render"]["working_mapping"], "nc-film-rgb-v1");

        // A `.jpg` path is refused before anything is written.
        let bad = tmp.path(&format!("{preset}.jpg"));
        let (code, _stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            bad.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "0.9,0.6,0.5",
        ]);
        assert_eq!(code, 2, "{preset} must reject a .jpg path");
        assert!(err.contains(".tif"), "{preset}: {err}");
        assert!(!bad.exists());
    }
}

#[test]
fn the_display_tone_headroom_reaches_the_pixels() {
    let tmp = TempDir::new("reinhard-tone");
    let scan = fixture("hdr-48bit.tif");
    let render = |tag: &str, extra: &[&str]| {
        let out = tmp.path(&format!("{tag}.tiff"));
        let mut argv = vec![
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
        ];
        argv.extend_from_slice(extra);
        let (code, _stdout, err) = run(&argv);
        assert_eq!(code, 0, "{tag}: {err}");
        std::fs::read(&out).unwrap()
    };

    // The headroom reaches the operator, and naming the default is naming nothing. (That
    // zero headroom is the exact identity is pinned bit-for-bit in `display_tone`.)
    let default = render("default", &[]);
    assert_eq!(default, render("w64", &["--display-tone-headroom", "6"]));
    let w16 = render("w16", &["--display-tone-headroom", "4"]);
    assert_ne!(default, w16, "the headroom did not reach the render");
}

#[test]
fn a_non_zero_headroom_has_its_overshoot_counted_at_the_encode_boundary() {
    // The SDR range policy: at a non-zero headroom the tone does not *refuse* content past
    // the ceiling, it lets the loss ride to `io::encode`,
    // which counts it — and `docs/using-nc.md` turns that count into user procedure
    // ("read `.loss.clipped_high` … raise the headroom until the fraction is what you
    // intend"). Every other test of this path stops inside `sdr::render`'s own buffer,
    // so nothing covered the part the user is told to act on.
    //
    // It is worth an end-to-end test rather than a unit one because it depends on Little
    // CMS evaluating the sRGB TRC as an unbounded *parametric* segment — the fragility
    // `pipeline::color` documents. Were that ever a sampled curve, the overshoot would
    // saturate to exactly 1.0, `clipped_high` would read 0, no warning would fire, and a
    // flat-white frame would exit 0: a quietly wrong image, in the one mode whose design
    // rests on the count.
    let tmp = TempDir::new("reinhard-loss");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("over.tif");
    // `scan` and the output paths outlive every call, but the closure cannot prove it, so
    // build the vector from owned pieces the caller keeps alive instead.
    let scan_s = scan.to_str().unwrap().to_string();
    let argv = |tone: &[&str], out: &str| -> Vec<String> {
        let mut v = vec![
            "convert".to_string(),
            scan_s.clone(),
            "-o".to_string(),
            out.to_string(),
            "--output-preset".to_string(),
            "display-p3".to_string(),
            "--film-base".to_string(),
            "0.9,0.55,0.42".to_string(),
            "--print-exposure".to_string(),
            "3".to_string(),
        ];
        v.extend(tone.iter().map(|s| s.to_string()));
        v
    };
    fn as_argv(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    let a = argv(&["--display-tone-headroom", "1"], out.to_str().unwrap());
    let (code, stdout, err) = run(&as_argv(&a));
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let clipped = report["loss"]["clipped_high"].as_u64().unwrap();
    let total = report["loss"]["total_samples"].as_u64().unwrap();
    // A fraction, not a fixed count: the exact number is a colour-transform product and
    // so is target-dependent, but "most of this frame was lost" is not.
    assert!(
        clipped * 2 > total,
        "expected the overshoot to be counted, got {clipped} of {total}"
    );
    // ...and it is reported, not merely counted.
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("clipped")),
        "the counted loss was not surfaced as a warning: {}",
        report["warnings"]
    );

    // Falsifiable control: zero headroom — the identity — refuses the identical argv
    // rather than counting it. If this ever also exits 0, the two range policies have
    // collapsed into one and the assertion above stops meaning anything.
    let refused = tmp.path("refused.tif");
    let b = argv(&["--display-tone-headroom", "0"], refused.to_str().unwrap());
    let (code, _stdout, err) = run(&as_argv(&b));
    assert_eq!(
        code, 1,
        "zero headroom should refuse the same overshoot: {err}"
    );
    assert!(err.contains("above reference white"), "{err}");
    assert!(
        !refused.exists(),
        "zero headroom wrote a file before refusing"
    );
}

#[test]
fn the_retired_display_tone_flags_are_refused_with_a_migration_error() {
    // Removed-value errors, not clap's parse failure listing the old names — on both
    // chains, since the removed-flag check runs before the new flow's availability table.
    let tmp = TempDir::new("display-tone-removed");
    let scan = fixture("hdr-48bit.tif");
    for new_flow in [false, true] {
        for extra in [
            &["--display-tone", "shoulder"][..],
            &["--display-tone", "none"],
            &["--display-tone", "reinhard"],
            &["--highlight-compress", "0.5"],
        ] {
            let out = tmp.path("out.tif");
            let mut argv = vec![
                "convert",
                scan.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--film-base",
                "0.9,0.6,0.5",
            ];
            if new_flow {
                argv.push("--new-flow");
            } else {
                argv.extend(["--output-preset", "display-p3"]);
            }
            argv.extend_from_slice(extra);
            let (code, _stdout, err) = run(&argv);
            assert_eq!(code, 2, "{extra:?}: {err}");
            assert!(err.contains("was removed"), "{extra:?}: {err}");
            assert!(err.contains("--display-tone-headroom"), "{extra:?}: {err}");
            assert!(!err.contains("possible values"), "{extra:?}: {err}");
            assert!(!out.exists(), "{extra:?}: a refused run wrote a file");
        }
    }
    // The recipe key is refused too, its old default included: replaying `"shoulder"`
    // would render differently.
    for tone in [
        r#""shoulder""#,
        r#""none""#,
        r#"{"reinhard":{"headroom_stops":4}}"#,
    ] {
        let recipe = write_file(
            &tmp.path("r.json"),
            &format!(
                r#"{{ "calibration": {{ "film_base": {{ "explicit": [0.9, 0.6, 0.5] }} }},
                     "output": {{ "preset": "display-p3" }},
                     "print": {{ "display_tone": {tone} }} }}"#
            ),
        );
        let out = tmp.path("out.tif");
        let (code, _stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{tone}: {err}");
        assert!(err.contains("`print.display_tone`"), "{tone}: {err}");
        assert!(err.contains("fit_range.headroom_stops"), "{tone}: {err}");
    }
}

#[test]
fn the_display_tone_headroom_is_gated_before_anything_is_opened() {
    // The headroom bound is a **value** rule, so it belongs to `cli::validate` — not to
    // `Headroom::new`, which runs after the decode. Proof that they moved: a nonexistent input still exits 2 (usage), never 3
    // (decode). Before the fix these reached the decoder, and `--dump-params` had
    // already written the invalid recipe to disk by then.
    let tmp = TempDir::new("reinhard-gate");
    let missing = tmp.path("does-not-exist.tif");
    let dumped = tmp.path("dumped.json");
    //
    // Each case asserts the **rule's own wording**, not just exit 2: clap also exits 2,
    // so a bare code check cannot tell a parse error from the validation rule. That
    // mattered here — `-1` was refused by clap as an "unexpected argument" until
    // `--display-tone-headroom` gained `allow_hyphen_values`, leaving
    // `check_headroom_stops`'s negative branch unreachable from the flag whose name its
    // own message prints.
    let bad = [
        (
            vec!["--display-tone-headroom", "30"],
            "beyond the supported maximum",
        ),
        (
            vec!["--display-tone-headroom", "-1"],
            "must be finite and non-negative",
        ),
    ];
    for (extra, expected) in bad {
        let out = tmp.path("out.tiff");
        let mut argv = vec![
            "convert",
            missing.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
            "--dump-params",
            dumped.to_str().unwrap(),
        ];
        argv.extend(extra.iter().copied());
        let (code, _stdout, err) = run(&argv);
        assert_eq!(code, 2, "{extra:?} reached the decoder: {err}");
        assert!(err.contains(expected), "{extra:?}: {err}");
        assert!(
            err.contains("--display-tone-headroom"),
            "{extra:?}: clap answered for the validation rule: {err}"
        );
        assert!(
            !dumped.exists(),
            "{extra:?}: an invalid recipe was written to disk before failing"
        );
    }
    // Falsifiable control: the same invocation with a usable headroom gets past
    // validation and fails at the *decode* instead.
    let out = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        missing.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.6,0.5",
        "--display-tone-headroom",
        "6",
    ]);
    assert_eq!(code, 3, "a valid headroom must reach the decoder: {err}");
}

#[test]
fn roll_refuses_an_out_of_range_headroom_in_the_shared_recipe() {
    // `roll` never calls `validate_convert`, so a rule that lives only there — or only
    // in the stage — is no gate at all for it: every frame decoded and reconstructed
    // before failing, once per frame. The shared recipe is validated up front, so this
    // must exit 2 with nothing written.
    let tmp = TempDir::new("roll-headroom");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{
  "calibration": {
    "film_base": { "explicit": [0.9, 0.55, 0.42] }
  },
  "reconstruction": {
    "type": "density",
    "curve": { "type": "exponential" }
  },
  "output": { "preset": "display-p3" },
  "fit_range": { "headroom_stops": 60.0 }
}"#,
    );
    let out_dir = tmp.path("out");
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "the shared recipe must be refused up front: {err}");
    assert!(err.contains("--display-tone-headroom"), "{err}");
    assert!(!out_dir.exists(), "a frame was written before refusing");
}

#[test]
fn the_single_rendition_hdr_presets_apply_the_lifted_tone() {
    // The HDR half of `output/display-tone-mapping`, end to end. These presets have no SDR
    // rendition to keep in range, so the lifted form's asymptotic base is enough: it holds
    // the composite strictly inside the declared 1000-nit peak, measured 4.912–4.919
    // against 4.926 across seven fixture frames.
    let tmp = TempDir::new("hdr-lifted");
    let scan = fixture("hdr-48bit.tif");
    for (preset, ext) in [
        ("hdr-pq", "avif"),
        ("hdr-hlg", "avif"),
        ("hdr-linear-tiff", "tiff"),
        ("hdr-pq-tiff", "tiff"),
        ("hdr-hlg-tiff", "tiff"),
    ] {
        let plain = tmp.path(&format!("{preset}-w16.{ext}"));
        let lifted = tmp.path(&format!("{preset}-lifted.{ext}"));
        let run_one = |out: &std::path::Path, extra: &[&str]| {
            let mut argv: Vec<String> = vec![
                "convert".into(),
                scan.to_str().unwrap().into(),
                "-o".into(),
                out.to_str().unwrap().into(),
                "--output-preset".into(),
                preset.into(),
                "--film-base".into(),
                "0.9,0.6,0.5".into(),
            ];
            argv.extend(extra.iter().map(|s| s.to_string()));
            let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
            let (code, stdout, err) = run(&borrowed);
            assert_eq!(code, 0, "{preset}: {err}");
            serde_json::from_str::<serde_json::Value>(&stdout).unwrap()
        };
        run_one(&plain, &["--display-tone-headroom", "4"]);
        let report = run_one(&lifted, &[]);
        // Reported as the tone that ran, in the block every preset emits.
        assert_eq!(
            report["output_render"]["display_tone"]["headroom_stops"], 6.0,
            "{preset}: {}",
            report["output_render"]["display_tone"]
        );
        // ...and it reached the pixels.
        assert_ne!(
            std::fs::read(&plain).unwrap(),
            std::fs::read(&lifted).unwrap(),
            "{preset}: the headroom did not reach the lifted tone"
        );
        // The declared peak still holds: nothing clipped on the way out, which is what the
        // asymptotic base buys and what a hard ceiling clamp would have flattened instead.
        assert_eq!(
            report["loss"]["clipped_high"], 0,
            "{preset}: the lifted tone left the declared headroom: {}",
            report["loss"]
        );
    }
}

#[test]
fn the_two_sdr_presets_differ_only_in_gamut() {
    // They share a render and differ in destination gamut, so the files must not
    // be identical — the falsifiable half of "same render, different gamut". If
    // they ever match byte-for-byte, one of them is not applying its own gamut.
    let tmp = TempDir::new("sdr-gamut");
    let scan = fixture("hdr-48bit.tif");
    let mut bytes = Vec::new();
    for preset in ["compatibility", "display-p3"] {
        let out = tmp.path(&format!("{preset}.tiff"));
        let (code, _stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "0.9,0.6,0.5",
        ]);
        assert_eq!(code, 0, "{preset}: {err}");
        bytes.push(std::fs::read(&out).unwrap());
    }
    assert_ne!(
        bytes[0], bytes[1],
        "sRGB and Display P3 renditions must not be byte-identical"
    );
}

#[test]
fn every_tiff_preset_now_states_its_suffix_including_the_oldest_two() {
    // Before the SDR presets landed, `film-master` (and the since-retired `legacy`)
    // pinned no suffix, so `hanten convert -o out.jpg` wrote a TIFF named `.jpg` with
    // exit 0 and no warning — the silently-misnamed-file mistake every newer preset
    // guards. An **extensionless** path is a different case and is *completed* rather
    // than refused (design-spec §5); the diagnosis for a stated-but-wrong one must be
    // about the path rather than about a preset flag the user need never have typed.
    let tmp = TempDir::new("suffix-symmetry");
    let scan = fixture("hdr-48bit.tif");
    let convert = |out: &std::path::Path, argv: &[&str]| {
        let mut full = vec![
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.6,0.5",
        ];
        full.extend_from_slice(argv);
        run(&full)
    };
    for argv in [
        vec!["--output-preset", "display-p3"],
        vec!["--output-preset", "film-master"],
    ] {
        let bad = tmp.path("out.jpg");
        let (code, _stdout, err) = convert(&bad, &argv);
        assert_eq!(code, 2, "{argv:?} must reject `out.jpg`: {err}");
        assert!(!bad.exists(), "{argv:?} must write nothing for `out.jpg`");
        // The extensionless path is the *other* half of the same table: it is
        // completed to this preset's own container rather than refused.
        let stem = tmp.path(if argv[1] == "display-p3" {
            "sdr-stem"
        } else {
            "master-stem"
        });
        let (code, _stdout, err) = convert(&stem, &argv);
        assert_eq!(
            code,
            0,
            "{argv:?} must complete `{}`: {err}",
            stem.display()
        );
        assert!(
            PathBuf::from(format!("{}.tiff", stem.display())).exists(),
            "{argv:?} must complete to its own container: {err}"
        );
        // The positive control: without it, a rule that rejected *every* path would
        // pass the assertions above.
        let good = tmp.path(if argv[1] == "display-p3" {
            "sdr.tiff"
        } else {
            "master.tiff"
        });
        let (code, _stdout, err) = convert(&good, &argv);
        assert_eq!(code, 0, "{argv:?} must accept a .tiff path: {err}");
        assert!(good.exists(), "{argv:?} must write the file");
    }
    // The default path's diagnosis names the *default preset* and the requirement,
    // never a flag nobody passed, because this is about what a bare
    // invocation resolves.
    let (_code, _stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        tmp.path("out.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.6,0.5",
    ]);
    assert!(err.contains(".jpg"), "{err}");
    assert!(err.contains("gain-map-hdr"), "{err}");
    assert!(
        !err.contains("--output-preset gain-map-hdr"),
        "the default path must not blame an unpassed flag: {err}"
    );
}

#[test]
fn gain_map_hdr_carries_both_dialects_over_the_same_pixels_as_ultra_hdr_v1() {
    // The two gain-map presets are one render packaged two ways. This pins both
    // halves of that: the *pixels* must be identical (so a future edit cannot let
    // them drift into two renders), and the *metadata* must differ in exactly the
    // ISO segments — which is the entire reason `gain-map-hdr` exists, since Apple
    // platforms read only the ISO dialect and open `ultra-hdr-v1` as plain SDR.
    let tmp = TempDir::new("gain-map-hdr-dialects");
    let convert = |preset: &str, name: &str| -> (serde_json::Value, Vec<u8>) {
        let output = tmp.path(name);
        let (code, stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
        ]);
        assert_eq!(code, 0, "{err}");
        (json(&stdout), std::fs::read(&output).unwrap())
    };
    let (legacy_report, legacy_bytes) = convert("ultra-hdr-v1", "legacy.jpg");
    let (dual_report, dual_bytes) = convert("gain-map-hdr", "dual.jpg");

    assert_eq!(dual_report["recipe"]["output"]["preset"], "gain-map-hdr");
    assert_eq!(
        dual_report["output_render"]["encoding"],
        "dual-dialect-gain-map-jpeg"
    );
    // Same pixels: `output_stats` is measured on the normalized 8-bit buffer handed
    // to the compressor, so it witnesses the render rather than the container.
    assert_eq!(
        dual_report["output_stats"], legacy_report["output_stats"],
        "the two gain-map presets must package one identical render"
    );

    let has =
        |bytes: &[u8], needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    // Both carry the legacy dialect — dual-dialect is additive, and the shared gain
    // map is the achromatic one legacy XMP can describe.
    for bytes in [&legacy_bytes, &dual_bytes] {
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        assert!(has(bytes, b"hdrgm:Version=\"1.0\""));
        assert!(has(bytes, b"Item:Semantic=\"GainMap\""));
    }
    // Only the dual file carries ISO 21496-1, and `ultra-hdr-v1`'s ISO-free
    // contract is asserted from its own side too.
    assert!(
        has(&dual_bytes, b"urn:iso:std:iso:ts:21496:-1"),
        "gain-map-hdr must carry the published ISO 21496-1 URN"
    );
    assert!(
        !has(&legacy_bytes, b"21496"),
        "ultra-hdr-v1 is contractually ISO-free"
    );
    // The ISO segment count is 2 (one per image), not 1: a baseline-only file
    // parses in exiftool but decodes as SDR, which is exactly how a placement
    // defect once shipped.
    assert_eq!(
        dual_bytes
            .windows(b"urn:iso:std:iso:ts:21496:-1".len())
            .filter(|window| *window == b"urn:iso:std:iso:ts:21496:-1")
            .count(),
        2,
        "both the baseline and the gain-map image must carry an ISO segment"
    );
    // Placement: the baseline's ISO segment must precede both SOF0 and the MPF
    // label. Meeting only the second constraint produces a well-formed file no
    // decoder ever parses.
    let at = |needle: &[u8]| {
        dual_bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap_or_else(|| panic!("missing marker"))
    };
    let iso = at(b"urn:iso:std:iso:ts:21496:-1");
    assert!(iso < at(&[0xff, 0xc0]), "ISO segment must precede SOF0");
    assert!(iso < at(b"MPF\0"), "ISO segment must precede the MPF label");
}

#[test]
fn gain_map_hdr_rejects_a_non_jpeg_suffix_and_rolls_with_a_jpg_name() {
    let tmp = TempDir::new("gain-map-hdr-refusals");
    // Same container as `ultra-hdr-v1`, so the same suffix rule — asserted for this
    // preset in its own right rather than assumed from the shared table.
    let output = tmp.path("out.tiff");
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--output-preset",
        "gain-map-hdr",
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".jpg"), "{err}");
    assert!(!output.exists());

    // Roll runs it and derives `<stem>_positive.jpg`. Roll takes its preset from the
    // shared recipe, since it accepts no output-preset flag.
    let out_dir = tmp.path("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{"output":{"preset":"gain-map-hdr"},"calibration":{"film_base":{"explicit":[1,1,1]}}}"#,
    );
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let rolled = out_dir.join("hdr-48bit_positive.jpg");
    assert!(
        rolled.exists(),
        "roll must derive `.jpg` for a JPEG container"
    );
    // And the rolled frame really is the dual-dialect file, not a renamed TIFF.
    let bytes = std::fs::read(&rolled).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    assert!(
        bytes
            .windows(b"urn:iso:std:iso:ts:21496:-1".len())
            .any(|w| w == b"urn:iso:std:iso:ts:21496:-1")
    );
}

#[test]
fn roll_checks_explicit_manifest_suffixes_and_derives_per_frame_preset_names() {
    // The manifest path is the half `validate_convert` never reached: an explicit
    // `"output"` was resolved on a code path that called `validate` alone, so a
    // frame could be pointed at a container its preset cannot write. It now goes
    // through the same rule `convert` uses.
    let tmp = TempDir::new("roll-container-naming");
    let out_dir = tmp.path("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let input = fixture("hdr-48bit.tif");

    let manifest = |body: &str| -> PathBuf { write_file(&tmp.path("frames.json"), body) };
    let roll = |frames: &Path| {
        run(&[
            "roll",
            "--frames",
            frames.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--params",
            tmp.path("shared.json").to_str().unwrap(),
        ])
    };
    write_file(
        &tmp.path("shared.json"),
        r#"{"output":{"preset":"gain-map-hdr"},"calibration":{"film_base":{"explicit":[1,1,1]}}}"#,
    );

    // An explicit path whose suffix contradicts the resolved container fails up
    // front, naming the frame and the escape hatch.
    let bad = manifest(&format!(
        r#"{{"frames":[{{"input":"{}","output":"frame.tiff"}}]}}"#,
        input.display()
    ));
    let (code, _stdout, err) = roll(&bad);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("gain-map-hdr"), "{err}");
    assert!(err.contains(".jpg"), "{err}");
    assert!(err.contains("frame"), "{err}");

    // A matching explicit path is accepted verbatim and never renamed.
    let good = manifest(&format!(
        r#"{{"frames":[{{"input":"{}","output":"chosen.jpeg"}}]}}"#,
        input.display()
    ));
    let (code, _stdout, err) = roll(&good);
    assert_eq!(code, 0, "{err}");
    assert!(out_dir.join("chosen.jpeg").exists(), "{err}");

    // A per-frame `output.preset` override changes that frame's container, so its
    // *derived* name must follow the frame's preset rather than the roll's. The
    // override still warns loudly (different image class) — that is unchanged.
    let mixed = manifest(&format!(
        r#"{{"frames":[
             {{"input":"{0}"}},
             {{"input":"{0}","output":"as-master.tiff","params":{{"output":{{"preset":"film-master"}}}}}}
           ]}}"#,
        input.display()
    ));
    let (code, stdout, err) = roll(&mixed);
    assert_eq!(code, 0, "{err}");
    assert!(out_dir.join("hdr-48bit_positive.jpg").exists(), "{err}");
    assert!(out_dir.join("as-master.tiff").exists(), "{err}");
    let report = json(&stdout);
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("output.preset")),
        "a per-frame preset override must still warn: {report}"
    );

    // An explicit path stating **no** suffix is completed from the frame's own
    // resolved container, exactly as a `convert` path is — the manifest shares the
    // whole rule, not just its refusing half.
    let bare = manifest(&format!(
        r#"{{"frames":[{{"input":"{}","output":"stem-only"}}]}}"#,
        input.display()
    ));
    let (code, stdout, err) = roll(&bare);
    assert_eq!(code, 0, "{err}");
    assert!(out_dir.join("stem-only.jpg").exists(), "{err}");
    assert!(!out_dir.join("stem-only").exists(), "{err}");
    let report = json(&stdout);
    assert_eq!(
        report["frames"][0]["output"],
        out_dir.join("stem-only.jpg").to_str().unwrap(),
        "the roll report must name the completed path: {report}"
    );
}

#[test]
fn a_bare_output_stem_takes_the_presets_container_and_everything_names_it() {
    // The change this task ships: `-o out` no longer has to know the container.
    // Asserted per container, and on *all four* things that derive from the path —
    // the file written, the sidecar, the report, and what is left absent.
    let tmp = TempDir::new("bare-output-stem");
    let input = fixture("hdr-48bit.tif");
    for (preset, ext, container) in [
        // The no-preset case: this is a test *about* the default.
        (None, "jpg", "jpeg"),
        (Some("display-p3"), "tiff", "tiff"),
        (Some("hdr-pq"), "avif", "avif"),
        (Some("film-master"), "tiff", "tiff"),
    ] {
        let stem = tmp.path(preset.unwrap_or("default"));
        let mut argv: Vec<&str> = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            stem.to_str().unwrap(),
            "--film-base",
            "1,1,1",
        ];
        if let Some(name) = preset {
            argv.extend_from_slice(&["--output-preset", name]);
        }
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{preset:?}: {err}");
        let written = PathBuf::from(format!("{}.{ext}", stem.display()));
        assert!(
            written.exists(),
            "{preset:?}: {} missing",
            written.display()
        );
        assert!(
            !stem.exists(),
            "{preset:?}: the stem itself must not be written"
        );
        assert!(
            sidecar_of(&written).exists(),
            "{preset:?}: the sidecar must sit beside the completed path, not the stem"
        );
        assert!(
            !sidecar_of(&stem).exists(),
            "{preset:?}: no sidecar may be written for the stem"
        );
        // The name is only half of it: the bytes must be the container the name
        // claims. Nothing in the type system couples `cli::container_for` to the
        // render dispatch in `convert_frame`, so this is the coupling.
        assert_eq!(
            sniff_container(&written),
            container,
            "{preset:?}: {} is named .{ext} but its bytes are not {container}",
            written.display()
        );
        let report = json(&stdout);
        assert_eq!(
            report["output"],
            written.to_str().unwrap(),
            "{preset:?}: the report must name what was written"
        );
    }
}

#[test]
fn an_output_path_naming_a_directory_is_refused_not_completed_to_a_sibling() {
    // `-o positives/` is the muscle-memory mistake — roll's sibling flag is spelled
    // `--out-dir positives/`. `Path::file_name()` normalises the trailing separator
    // away, so completing it would write `positives.jpg` *next to* the directory.
    // Refused at exit 2 instead; no byte that decides *which file* is named may be
    // altered or dropped.
    //
    // Both spellings, because they are one hole: `dir/.` escapes a trailing-separator
    // test but `file_name()` normalises its `.` away just the same.
    let tmp = TempDir::new("names-a-directory");
    let input = fixture("hdr-48bit.tif");
    let dir = tmp.path("dir");
    std::fs::create_dir_all(&dir).unwrap();
    for given in [
        format!("{}/", dir.display()),
        format!("{}/.", dir.display()),
    ] {
        let (code, _stdout, err) = run(&[
            "convert",
            input.to_str().unwrap(),
            "-o",
            &given,
            "--film-base",
            "1,1,1",
        ]);
        assert_eq!(code, 2, "{given}: {err}");
        assert!(err.contains("names a directory"), "{given}: {err}");
        assert!(
            !tmp.path("dir.jpg").exists(),
            "{given}: a sibling of the directory was written: {err}"
        );
    }

    // Falsifiable control: the same path without the separator is a stem and works.
    let stem = tmp.path("stem");
    let (code, _stdout, err) = run(&[
        "convert",
        input.to_str().unwrap(),
        "-o",
        stem.to_str().unwrap(),
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(tmp.path("stem.jpg").exists(), "{err}");
}

#[test]
fn a_stated_suffix_survives_verbatim_and_a_dotted_stem_keeps_its_dot() {
    // The two halves completion must not disturb: a spelling the container accepts
    // is never normalised, and a dot-segment no preset claims is a stem.
    let tmp = TempDir::new("stated-suffix");
    let input = fixture("hdr-48bit.tif");
    let convert = |given: &Path| -> (i32, String, String) {
        run(&[
            "convert",
            input.to_str().unwrap(),
            "-o",
            given.to_str().unwrap(),
            "--film-base",
            "1,1,1",
        ])
    };

    // `.jpeg` is the non-canonical spelling: nc writes `.jpg` when it chooses, and
    // must not rewrite the user's choice to match.
    let stated = tmp.path("stated.jpeg");
    let (code, stdout, err) = convert(&stated);
    assert_eq!(code, 0, "{err}");
    assert!(stated.exists(), "{err}");
    assert!(
        !tmp.path("stated.jpg").exists(),
        "the spelling was rewritten"
    );
    assert_eq!(json(&stdout)["output"], stated.to_str().unwrap());

    // `.v2` is not a container nc knows, so the whole thing is the stem.
    let dotted = tmp.path("scan.v2");
    let (code, stdout, err) = convert(&dotted);
    assert_eq!(code, 0, "{err}");
    let written = tmp.path("scan.v2.jpg");
    assert!(written.exists(), "{err}");
    assert!(!tmp.path("scan.jpg").exists(), "the stem's dot was eaten");
    assert_eq!(json(&stdout)["output"], written.to_str().unwrap());

    // And a *known* spelling the container refuses is still the usage error it has
    // always been — completion never rescues a stated suffix.
    let (code, _stdout, err) = convert(&tmp.path("wrong.tiff"));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("gain-map-hdr"), "{err}");
}

#[test]
fn the_write_target_guard_sees_the_completed_path() {
    // Ordering: the path is resolved *before* the collision check, so a
    // `--report-file` that only clashes once the suffix is completed is still
    // caught. Resolving after the guard would let the report be overwritten by the
    // image (or vice versa) with every check green.
    let tmp = TempDir::new("completed-target-guard");
    let input = fixture("hdr-48bit.tif");
    let stem = tmp.path("clash");
    let convert = |report_file: &Path| -> (i32, String, String) {
        run(&[
            "convert",
            input.to_str().unwrap(),
            "-o",
            stem.to_str().unwrap(),
            "--film-base",
            "1,1,1",
            "--report-file",
            report_file.to_str().unwrap(),
        ])
    };
    let (code, _stdout, err) = convert(&tmp.path("clash.jpg"));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--report-file"), "{err}");
    // Falsifiable: the *stem* is not a write target, so pointing the report there
    // is fine — the guard is reacting to the completed path, not to any overlap.
    let (code, _stdout, err) = convert(&stem);
    assert_eq!(code, 0, "{err}");
    assert!(tmp.path("clash.jpg").exists(), "{err}");
}

#[test]
fn the_default_output_is_the_dual_dialect_gain_map_jpeg() {
    // The `output/presets` default migration, asserted through the binary with no
    // preset named.
    let tmp = TempDir::new("default-preset");
    let out = tmp.path("positive.jpg");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(report["recipe"]["output"]["preset"], "gain-map-hdr");
    assert_eq!(
        report["output_render"]["encoding"],
        "dual-dialect-gain-map-jpeg"
    );
    assert_eq!(report["identity"]["pipeline_version"], 7);
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8], "the default writes a JPEG");
    assert!(
        bytes
            .windows(b"urn:iso:std:iso:ts:21496:-1".len())
            .any(|w| w == b"urn:iso:std:iso:ts:21496:-1"),
        "the default must carry the ISO dialect — that is what makes it HDR on Apple"
    );

    // The documented cost of the migration: a `.tif` path with no preset is now a
    // usage error rather than a 16-bit TIFF, and the message says how to get one.
    let (code, _stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("positive.tiff").to_str().unwrap(),
        "--film-base",
        "1,1,1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("gain-map-hdr"), "{err}");
    assert!(
        err.contains("display-p3"),
        "the way out must be named: {err}"
    );
    assert!(!tmp.path("positive.tiff").exists());
}

#[test]
fn telemetry_reports_the_primary_containers_depth_not_the_ir_planes() {
    // Regression: `OutputParams::depth()` is the *IR TIFF* depth for the JPEG and
    // AVIF presets, so recording it verbatim labelled a gain-map run `u16` when its
    // primary is a fixed 8-bit JPEG — and the default is now a gain-map JPEG, so
    // that mislabelled the ordinary case.
    let tmp = TempDir::new("telemetry-depth");
    for (preset, ext, want) in [
        ("gain-map-hdr", "jpg", "u8"),
        ("ultra-hdr-v1", "jpg", "u8"),
        ("hdr-pq", "avif", "u10"),
        ("display-p3", "tiff", "u16"),
        ("film-master", "tiff", "f32"),
    ] {
        let out = tmp.path(&format!("{preset}.{ext}"));
        let rec = tmp.path(&format!("{preset}.telemetry.json"));
        let (code, _stdout, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            preset,
            "--film-base",
            "1,1,1",
            "--telemetry-file",
            rec.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{preset}: {err}");
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        assert_eq!(
            record["conversion"]["output_depth"], want,
            "{preset} must report its primary container's depth"
        );
    }
}

#[test]
fn the_headroom_changes_the_sdr_render_and_the_recipe_records_it() {
    // The knob's product claim: on a display preset it is a real pixel change, and the
    // report's recipe says which headroom produced them.
    let tmp = TempDir::new("display-tone");
    let scan = fixture("hdr-48bit.tif");
    let mut bytes = Vec::new();
    for stops in ["6", "0"] {
        let out = tmp.path(&format!("w{stops}.tiff"));
        // Pulled down far enough that zero headroom renders this frame rather than
        // refusing its highlights (no shipped reconstruction is bounded at white).
        let (code, stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
            "--print-exposure=-2.2",
            "--display-tone-headroom",
            stops,
        ]);
        assert_eq!(code, 0, "{stops}: {err}");
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            report["recipe"]["fit_range"]["headroom_stops"],
            stops.parse::<f64>().unwrap(),
            "{stops}"
        );
        bytes.push(std::fs::read(&out).unwrap());
    }
    assert_ne!(
        bytes[0], bytes[1],
        "the headroom must change the rendered pixels"
    );
}

#[test]
fn the_coded_hdr_report_block_names_the_display_tone() {
    // `hdr_coded_tiff` carries the rendition's tone identifier, so a consumer reading
    // the contract block alone knows which operator produced it.
    let tmp = TempDir::new("display-tone-coded");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("pq.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "hdr-pq-tiff",
        "--film-base",
        "0.9,0.6,0.5",
    ]);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["hdr_coded_tiff"]["tone_curve"],
        "extended-reinhard-mid-preserving-v2"
    );
}

#[test]
fn output_render_reports_the_display_tone_that_ran() {
    // `output_render` is the one block every preset emits, and it must never assert a
    // tone curve the run skipped. The SDR presets emit no per-preset contract block at
    // all and the AVIF pair's carries no rendering policy, so for them this field is
    // the report's only statement of tone.
    let tmp = TempDir::new("display-tone-output-render");
    let scan = fixture("hdr-48bit.tif");
    for (preset, ext) in [("display-p3", "tiff"), ("hdr-linear-tiff", "tiff")] {
        for stops in [6.0, 0.0] {
            let out = tmp.path(&format!("{preset}-{stops}.{ext}"));
            let stated = stops.to_string();
            let mut args = vec![
                "convert",
                scan.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--output-preset",
                preset,
                "--film-base",
                "0.9,0.6,0.5",
                // Low enough that zero headroom renders rather than refusing the
                // highlights.
                "--print-exposure=-4",
                "--display-tone-headroom",
            ];
            args.push(&stated);
            let (code, stdout, err) = run(&args);
            assert_eq!(code, 0, "{preset}/{stops}: {err}");
            let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            // Zero headroom moves no pixel, so it names no operator — the new chain's rule.
            let operator = if stops == 0.0 {
                "identity"
            } else {
                "extended-reinhard-mid-preserving-v2"
            };
            assert_eq!(
                report["output_render"]["display_tone"],
                serde_json::json!({ "operator": operator, "headroom_stops": stops }),
                "{preset}/{stops}"
            );
            // And the prose must not contradict it by naming a curve of its own.
            let content = report["output_render"]["content"].as_str().unwrap();
            assert!(
                !content.contains("reinhard"),
                "{preset}/{stops}: content names a tone curve: {content}"
            );
        }
    }
    // A branch with no display tone stage omits the field rather than claiming a
    // curve that never ran.
    let out = tmp.path("master.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "film-master",
        "--film-base",
        "0.9,0.6,0.5",
    ]);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let branch = &report["output_render"];
    // The block has to be shown present *first*: indexing a missing key yields
    // `Value::Null`, and `Null.get(…)` is also `None`, so asserting the field's absence
    // alone would pass just as well if `output_render` disappeared entirely.
    assert_eq!(branch["preset"], "film-master", "{branch}");
    assert!(branch.get("display_tone").is_none(), "{branch}");
}

#[test]
fn zero_headroom_refuses_a_reconstruction_that_overshoots_reference_white() {
    // Zero headroom is the identity, and it polices itself: the default reconstruction
    // is unbounded at white, and the render fails naming the pixel rather than clipping
    // it quietly.
    let tmp = TempDir::new("display-tone-overshoot");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("overshoot.tiff");
    let args = |extra: Vec<&str>| {
        let mut args = vec![
            "convert".to_string(),
            scan.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--output-preset".to_string(),
            "display-p3".to_string(),
            "--film-base".to_string(),
            "0.9,0.6,0.5".to_string(),
        ];
        args.extend(extra.into_iter().map(str::to_string));
        args
    };
    let unbounded = args(vec!["--display-tone-headroom", "0"]);
    let (code, _stdout, err) = run(&unbounded.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("zero display-tone headroom"), "{err}");
    assert!(err.contains("above reference white"), "{err}");
    assert!(err.contains("pixel "), "{err}");

    // Falsifiable: the same reconstruction renders at the default headroom, so the
    // refusal is the identity's bound and not a broken fixture.
    let toned = args(vec![]);
    let (code, _stdout, err) = run(&toned.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 0, "{err}");
}

#[test]
fn the_zero_headroom_ceiling_is_per_branch_not_one_reference_white() {
    // `docs/using-nc.md` promises the SDR presets stop at reference white while the
    // HDR ones stop at the 1000-nit peak, so the *same* overshoot is refused on one
    // and renders on the other. Both single-branch bounds are unit-tested; this pins
    // the difference between them, which is the part a reader acts on — and the part
    // that makes HDR headroom reachable at `--display-tone-headroom 0`.
    let tmp = TempDir::new("display-tone-ceilings");
    let scan = fixture("hdr-48bit.tif");
    let run_preset = |preset: &str, name: &str| {
        let out = tmp.path(name);
        let (code, _stdout, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            preset,
            "--display-tone-headroom",
            "0",
            // The default render, pulled down a little: still across reference white,
            // comfortably inside the peak.
            "--print-exposure=-0.2",
            "--film-base",
            "0.9,0.6,0.5",
        ]);
        (code, err)
    };

    let (code, err) = run_preset("display-p3", "sdr.tiff");
    assert_eq!(
        code, 1,
        "SDR must refuse an overshoot of reference white: {err}"
    );
    assert!(err.contains("above reference white"), "{err}");

    let (code, err) = run_preset("hdr-pq-tiff", "hdr.tiff");
    assert_eq!(
        code, 0,
        "the same overshoot is well inside the HDR peak and must render: {err}"
    );
}

#[test]
fn zero_headroom_on_hdr_gets_the_explanatory_ceiling_error() {
    let tmp = TempDir::new("hdr-zero-headroom-remedy");
    let out = tmp.path("o.avif");
    let scan = fixture("hdr-48bit.tif");
    let (code, _stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--output-preset",
        "hdr-pq",
        // Overshoots the peak, which is what the range check exists to catch: the
        // default reconstruction is unbounded, and a stop up puts its highlights well
        // past 1000 nits.
        "--print-exposure",
        "1",
        // **Identity per-channel gain, deliberately.** This test is about the display
        // operator's ceiling, not the colour calibration: at the shipped gain the frame
        // rendered dark enough to sit *under* the ceiling, and the premise evaporated.
        "--density-scale",
        "1,1,1",
        "--display-tone-headroom",
        "0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "expected the ceiling error, got:\n{err}");
    // The explanatory diagnosis, naming the headroom by both its spellings — not a bare
    // out-of-range line.
    assert!(err.contains("has no curve to roll off"), "{err}");
    assert!(
        err.contains(
            "raise the headroom above 0 (--display-tone-headroom / fit_range.headroom_stops)"
        ),
        "{err}"
    );
    assert!(!err.contains("produced an out-of-range sample"), "{err}");
}

#[test]
fn film_master_refuses_a_stated_headroom_and_accepts_the_reset() {
    // `film-master` applies no display tone, so a non-default headroom would be silently
    // ignored — refused by value. The default is the flags-win reset that lets one recipe
    // serve every preset, so it is accepted.
    let tmp = TempDir::new("master-headroom");
    let scan = fixture("hdr-48bit.tif");
    let run_master = |stops: &str| {
        let out = tmp.path(&format!("master-{stops}.tiff"));
        run(&[
            "convert",
            scan.to_str().unwrap(),
            "--film-base",
            "1,1,1",
            "--output-preset",
            "film-master",
            "--display-tone-headroom",
            stops,
            "-o",
            out.to_str().unwrap(),
        ])
    };
    let (code, _o, err) = run_master("3");
    assert_eq!(code, 2, "expected a usage error, got:\n{err}");
    assert!(err.contains("fit_range.headroom_stops"), "{err}");
    assert!(
        err.contains("bypasses all print and display controls"),
        "{err}"
    );
    let (code, _o, err) = run_master("6");
    assert_eq!(
        code, 0,
        "the default headroom must reset, not refuse:\n{err}"
    );
}

/// IR-assisted holder detection is decided by measuring the IR plane, not by a
/// declared `--film-type` (`film-base/ir-usability-detection`). Chemistry is the
/// wrong predictor: separability tracks the *frame's* accumulated density, so an
/// unexposed silver frame separates ~20:1 while its own leader is opaque.
#[test]
fn ir_holder_detection_is_decided_by_measurement_not_by_declaration() {
    let dir = TempDir::new("ir-usability");

    // A frame whose film is IR-transparent — 0.63, where real chromogenic scans
    // measure (0.576-0.728 over 25 frames). No `--film-type` is passed.
    let clear = dir.path("clear.tif");
    write_hdri_with_uniform_ir(&clear, 200, 200, [20000, 12000, 8000], 41_000);
    let (code, stdout, _err) = run(&["inspect", clear.to_str().unwrap()]);
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(
        report["ir_separability"]["usable"], true,
        "an IR-transparent frame must measure usable undeclared: {report}"
    );
    assert!(
        report["holder_mask"].is_array(),
        "the holder mask must build with no --film-type: {report}"
    );

    // A declaration is echoed back rather than parsed and dropped: `inspect` and
    // `estimate` resolve no recipe, so the report is the only place a declaration
    // they were given can survive. Absent when not declared — not `null`.
    let (_, declared_out, _) = run(&[
        "inspect",
        "--film-type",
        "chromogenic",
        clear.to_str().unwrap(),
    ]);
    assert_eq!(json(&declared_out)["film_type"], "chromogenic");
    assert!(
        report.get("film_type").is_none(),
        "an undeclared run must omit the field, not report null: {report}"
    );
    let (_, est_out, _) = run(&[
        "estimate",
        "--film-type",
        "silver",
        // A region, not `--auto-base`: this fixture is uniform, so auto has no
        // rebate to find and would refuse before emitting a report.
        "--base-region",
        "20,20,40,40",
        clear.to_str().unwrap(),
    ]);
    let est = json(&est_out);
    assert_eq!(est["film_type"], "silver");
    assert_eq!(
        est["ir_separability"]["usable"], true,
        "the calibration command must carry the measurement, not just warn: {est}"
    );

    // The declaration is inert now: `silver` used to force this path off, and
    // `chromogenic` used to be the only way to turn it on. Both must produce the
    // same report as stating nothing.
    for declared in ["silver", "chromogenic"] {
        let (code, out, _err) = run(&["inspect", "--film-type", declared, clear.to_str().unwrap()]);
        assert_eq!(code, 0);
        let mut with_flag = json(&out);
        let mut without = report.clone();
        // Wall-clock legitimately differs run to run, and `film_type` is the
        // declaration itself echoed back as provenance. Everything else — the
        // verdict, the mask, the candidates — must be identical: the declaration
        // is recorded, and it decides nothing.
        for k in ["elapsed_ms", "film_type"] {
            with_flag[k] = serde_json::Value::Null;
            without[k] = serde_json::Value::Null;
        }
        assert_eq!(
            with_flag, without,
            "--film-type {declared} must change nothing but its own echo"
        );
    }

    // A frame whose own film is IR-opaque — the Ilford HP5 leader, interior median
    // 0.0165. Holder and film are indistinguishable, so the plane is refused and
    // detection falls back to RGB-only rather than labelling the film holder.
    let opaque = dir.path("opaque.tif");
    write_hdri_with_uniform_ir(&opaque, 200, 200, [20000, 12000, 8000], 1_081);
    let (code, stdout, _err) = run(&["inspect", opaque.to_str().unwrap()]);
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(report["ir_separability"]["usable"], false);
    assert!(
        report["holder_mask"].is_null(),
        "an opaque IR plane must build no mask: {report}"
    );
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("cannot separate the film holder")
            && w.as_str().unwrap().contains("0.016")),
        "the fallback must name the measurement that caused it: {warnings:?}"
    );
}

/// The measured verdict is not just reported — it decides whether `convert`
/// actually consumes the IR plane for the film base, which is what clears the
/// "IR preserved but not used" warning under `--strict`.
#[test]
fn a_usable_ir_plane_is_consumed_by_the_auto_film_base() {
    let dir = TempDir::new("ir-usability-convert");

    // Film IR-transparent (0.63): the holder mask applies, so the plane is
    // consumed and no "carried but unused" warning is left for --strict to promote.
    let usable = dir.path("usable.tif");
    write_hdri_scan_with_rebate(&usable, 41_000, false);
    let out = dir.path("usable-out.tif");
    let (code, stdout, err) = run(&[
        "convert",
        "--auto-base",
        "--strict",
        // The synthetic fixture carries no SilverFast XMP, so state the input
        // semantics the provenance gate would otherwise resolve from it.
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--output-preset",
        "display-p3",
        usable.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "a consumed IR plane must leave --strict clean:\n{err}"
    );
    let report = json(&stdout);
    assert_eq!(
        report["ir_separability"]["usable"],
        serde_json::Value::Null,
        "the verdict is an `inspect` diagnostic, not a convert report field"
    );
    assert!(
        report["warnings"].as_array().is_none_or(|ws| ws
            .iter()
            .all(|w| !w.as_str().unwrap().contains("preserved but not used"))),
        "the IR plane was consumed, so it must not be reported unused: {report}"
    );

    // The other side of that claim, and why consumption must be read off stage 2
    // rather than predicted from the inputs: the same frame with the holder on
    // *every* edge is marker-verified and measures usable, yet produces no mask
    // (the all-holder fallback). Predicting consumption from those three facts
    // suppressed this warning — and with it `--strict` — on exactly this case.
    let all_holder = dir.path("all-holder.tif");
    write_hdri_scan_with_rebate(&all_holder, 41_000, true);
    let (code, stdout, err) = run(&[
        "convert",
        "--auto-base",
        "--strict",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--output-preset",
        "display-p3",
        all_holder.to_str().unwrap(),
        "-o",
        dir.path("all-holder-out.tif").to_str().unwrap(),
    ]);
    assert_eq!(
        code, 1,
        "an unconsumed IR plane must still fail --strict:\n{err}"
    );
    assert!(
        json(&stdout)["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("preserved but not used")),
        "the fallback leaves the plane unused, and that must be reported:\n{stdout}"
    );

    // The same geometry with IR-opaque film: the plane cannot separate holder from
    // film, so detection falls back to RGB-only and says why. The base still
    // resolves — the fallback is the path that was always there.
    let opaque = dir.path("opaque.tif");
    write_hdri_scan_with_rebate(&opaque, 1_081, false);
    let out = dir.path("opaque-out.tif");
    let (code, stdout, err) = run(&[
        "convert",
        "--auto-base",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--output-preset",
        "display-p3",
        opaque.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "the RGB-only fallback must still convert:\n{err}");
    let report = json(&stdout);
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("cannot separate the film holder")),
        "the fallback must be reported, not silent: {warnings:?}"
    );
    // Both frames resolve the same base: the rebate is where it always was, and
    // the mask only ever restricted *where* the search looked.
    let usable_base =
        json(&run(&["estimate", "--auto-base", usable.to_str().unwrap()]).1)["film_base"].clone();
    assert_eq!(usable_base, report["film_base"]);
}

/// `--export-ir` is the documented escape hatch that keeps `--strict` usable on an
/// HDRi scan: the user is taking the plane themselves, so no IR note may fire —
/// including the fallback notes for a plane that cannot serve holder detection.
#[test]
fn export_ir_keeps_strict_clean_when_the_plane_cannot_serve_detection() {
    let dir = TempDir::new("ir-export-strict");
    let opaque = dir.path("opaque.tif");
    write_hdri_scan_with_rebate(&opaque, 1_081, false); // film itself IR-opaque
    let (code, _stdout, err) = run(&[
        "convert",
        "--auto-base",
        "--strict",
        "--export-ir",
        dir.path("ir.tif").to_str().unwrap(),
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--output-preset",
        "display-p3",
        opaque.to_str().unwrap(),
        "-o",
        dir.path("out.tif").to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "--strict --export-ir must stay usable on an HDRi scan:\n{err}"
    );
}

/// A holder that occludes every edge leaves the masked rebate search nothing to
/// scan, which is strictly worse than not masking. The mask falls back rather than
/// claiming a mask it doesn't have — and the holder *march*, which does not inherit
/// that decline, measures the ring instead, so the plane is still consumed.
#[test]
fn an_all_holder_border_falls_back_instead_of_emptying_the_search() {
    let dir = TempDir::new("ir-all-holder");
    let path = dir.path("ringed.tif");
    const W: u32 = 200;
    const H: u32 = 200;
    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![41_000u16; (W * H) as usize]; // film: IR-transparent
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = x < 6 || y < 6 || x >= W - 6 || y >= H - 6;
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            if holder {
                ir[(y * W + x) as usize] = 1_300; // holder: IR-dark, all four edges
            }
        }
    }
    write_hdri(&path, W, H, &rgb, &ir);

    let (code, stdout, _err) = run(&["inspect", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(
        report["ir_separability"]["usable"], true,
        "the frame's film is IR-transparent, so the verdict must be usable: {report}"
    );
    assert!(
        report["holder_mask"].is_null(),
        "an all-holder mask must fall back rather than be reported: {report}"
    );
    // The plane is still consumed — by the *other* IR reader. The holder march
    // deliberately does not inherit the mask's all-holder decline, so it measures
    // the ring and moves the reported rectangle. Keying the note on the mask alone
    // made one report carry both a measured `effective_area.holder` and "preserved
    // but not used" (`film-base/holder-depth-mask` review, 2026-09-17).
    let area = &report["effective_area"];
    assert_eq!(
        area["holder_applied"], true,
        "the march must measure the ring the mask declined: {report}"
    );
    assert!(
        area["holder"]["left"].as_u64().unwrap() > 0,
        "and report a real depth for it: {report}"
    );
    assert!(
        !report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("preserved but not used")),
        "a plane the march consumed must not be reported as unused: {report}"
    );
}

/// The measurement region is resolved and reported on **every** `convert`, so
/// `--measure-inset` is observable rather than accepted-and-ignored — and the flag
/// moves the reported rectangle without moving a pixel, because nothing under the
/// default anchor consumes the region (`film-base/holder-depth-mask` review).
#[test]
fn convert_always_reports_the_measurement_region_and_the_inset_flag_moves_it() {
    let dir = TempDir::new("measure-inset-convert");
    let (a, b) = (dir.path("a.tif"), dir.path("b.tif"));

    let base = ["--film-base", "0.9,0.6,0.5"];
    let mut args = vec!["convert", "--output-preset", "display-p3"];
    args.extend(base);
    args.extend(["-o", a.to_str().unwrap(), "tests/fixtures/hdr-48bit.tif"]);
    let (code, stdout, _) = run(&args);
    assert_eq!(code, 0);
    let default_area = json(&stdout)["effective_area"].clone();
    assert!(
        !default_area.is_null(),
        "a bare convert must report the area it resolved: {default_area}"
    );

    let mut args = vec![
        "convert",
        "--output-preset",
        "display-p3",
        "--measure-inset",
        "0.2",
    ];
    args.extend(base);
    args.extend(["-o", b.to_str().unwrap(), "tests/fixtures/hdr-48bit.tif"]);
    let (code, stdout, _) = run(&args);
    assert_eq!(code, 0);
    let wider = json(&stdout)["effective_area"].clone();
    assert_ne!(
        wider["inset"], default_area["inset"],
        "the flag must reach the resolved region: {wider} vs {default_area}"
    );

    // And nothing consumed it, so the pixels are untouched — observability is not
    // a pixel change.
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "an unconsumed region must not move a pixel"
    );
}

/// `--strict` must keep failing on the "IR preserved but not used" note when the
/// plane never reaches a pixel — even when the effective-area march *measured* a
/// holder with it. Nothing in a `convert` render reads the region (its one consumer,
/// the auto reference density, retired), so a marched holder is reported, never used.
#[test]
fn strict_still_fails_when_the_ir_marched_region_reaches_no_pixel() {
    let dir = TempDir::new("ir-region-strict");
    let path = dir.path("ringed.tif");
    const W: u32 = 200;
    const H: u32 = 200;
    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![41_000u16; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = x < 6 || y < 6 || x >= W - 6 || y >= H - 6;
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            if holder {
                ir[(y * W + x) as usize] = 1_300;
            }
        }
    }
    write_hdri(&path, W, H, &rgb, &ir);

    let out = dir.path("out.tif");
    let case = |extra: &[&str]| -> (i32, String) {
        let mut args = vec![
            "convert",
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
            "--input-transfer",
            "linear",
            "--input-meaning",
            "scanner-device",
            // Under white, so the display tone's by-design overshoot adds no clipping
            // warning of its own to trip `--strict`: the IR note is the only candidate.
            "--print-exposure=-3",
        ];
        args.extend(extra);
        args.extend(["-o", out.to_str().unwrap(), path.to_str().unwrap()]);
        let (code, _, err) = run(&args);
        (code, err)
    };

    // Falsifiable control: without `--strict` the same run succeeds, so the failure
    // below is the note being promoted and nothing else.
    let (code, err) = case(&[]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("preserved but not used"), "{err}");

    let (code, err) = case(&["--strict"]);
    assert_eq!(code, 1, "the note must still fail --strict: {err}");
    assert!(
        err.contains("preserved but not used"),
        "and for the right reason: {err}"
    );
}

/// An empty measurement region is a warning on `convert`, never a refusal
/// (`film-base/holder-depth-mask` ship review, M1): nothing in a conversion measures
/// over it since the auto reference density retired, so refusing would fail a run at
/// exit 2 over a measurement no stage read — while `inspect`/`estimate` degrade the
/// identical measurement to a warning at exit 0.
#[test]
fn an_empty_measurement_region_is_a_warning_not_a_refusal() {
    let dir = TempDir::new("empty-measure-region");
    let path = dir.path("ringed.tif");
    const W: u32 = 200;
    const H: u32 = 200;
    const RING: u32 = 30;
    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![41_000u16; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = x < RING || y < RING || x >= W - RING || y >= H - RING;
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            if holder {
                ir[(y * W + x) as usize] = 1_300;
            }
        }
    }
    write_hdri(&path, W, H, &rgb, &ir);

    // 30 px of measured holder plus a 40% (80 px) inset on each side of a 200 px
    // frame leaves nothing.
    let convert_with = |extra: &[&str], out: &std::path::Path| -> (i32, String, String) {
        let mut args = vec![
            "convert",
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
            "--input-transfer",
            "linear",
            "--input-meaning",
            "scanner-device",
            "--measure-inset",
            "0.4",
        ];
        args.extend(extra);
        args.extend(["-o", out.to_str().unwrap(), path.to_str().unwrap()]);
        run(&args)
    };

    // Nothing measures over the region: a warning, a written file, and no
    // `effective_area` — there is no region to report.
    let out = dir.path("unread.tif");
    let (code, stdout, err) = convert_with(&[], &out);
    assert_eq!(code, 0, "an unread region must not fail the run: {err}");
    assert!(out.exists(), "and the output must be written");

    // The reason the refusal was wrong: the output is byte-identical to the same
    // run with a region that resolves. If this ever differs, the empty region *is*
    // reaching the render and the refusal belongs back.
    let control = dir.path("control.tif");
    let (code, _, err) = run(&[
        "convert",
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.6,0.5",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "-o",
        control.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&control).unwrap(),
        "an unread empty region must not move a pixel"
    );
    let report = json(&stdout);
    assert!(
        report["effective_area"].is_null(),
        "a refused region has nothing to report: {report}"
    );
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("measurement region is empty")),
        "the warning is the observable: {warnings:?}"
    );
    // The measured depths and the inset must be separate quantities: the message
    // used to print the post-inset totals as "holder depths", sending a reader
    // after a 110 px holder that measured 30.
    let empty = warnings
        .iter()
        .find_map(|w| {
            let w = w.as_str().unwrap();
            w.contains("measurement region is empty").then_some(w)
        })
        .unwrap();
    assert!(
        empty.contains("measured holder depths (top 30, bottom 30, left 30, right 30)")
            && empty.contains("80 px inset"),
        "{empty}"
    );
    // And the remedy must be one that exists — `effective_area` never sees a
    // user-stated region, so "state a region explicitly" could not work.
    assert!(
        empty.contains("Lower the inset fraction") && !empty.contains("state a region"),
        "{empty}"
    );

    // On the new flow a conversion measures nothing over the region — its one
    // per-frame measurement, an auto white balance, retired in favour of the roll's
    // (`measure-roll`). So the empty region is a warning there too, whatever the
    // white balance, and `--auto-wb` is refused by name before anything is decoded.
    let new_flow = |extra: &[&str], out: &std::path::Path| {
        let mut args = vec![
            "convert",
            "--new-flow",
            "--film-base",
            "0.9,0.6,0.5",
            "--input-transfer",
            "linear",
            "--input-meaning",
            "scanner-device",
            "--measure-inset",
            "0.4",
        ];
        args.extend(extra);
        args.extend(["-o", out.to_str().unwrap(), path.to_str().unwrap()]);
        run(&args)
    };
    let (code, stdout, err) = new_flow(&["--white-balance", "1.1,1,0.9"], &dir.path("nf.tiff"));
    assert_eq!(code, 0, "stated gains read no region: {err}");
    assert!(json(&stdout)["effective_area"].is_null(), "{stdout}");
    let (code, _, err) = new_flow(&["--auto-wb", "gray-world"], &dir.path("nf-auto.tiff"));
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("--auto-wb") && err.contains("measure-roll"),
        "{err}"
    );

    // The inset's value bound is checked before any chain resolves, so its remedy must
    // not send a new-flow user to a flag that flow refuses.
    let bound_out = dir.path("nf-bound.tiff");
    let (code, _, err) = run(&[
        "convert",
        "--new-flow",
        "--film-base",
        "0.9,0.6,0.5",
        "--measure-inset",
        "0.5",
        "-o",
        bound_out.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("beyond the supported maximum")
            && err.contains("measure-roll")
            && !err.contains("--auto-wb"),
        "{err}"
    );

    // At an inset that leaves a region, nothing on the new flow renders from the
    // holder cut, so "IR preserved but not used" holds whatever the white balance.
    let out = dir.path("nf-stated.tiff");
    let (code, stdout, err) = run(&[
        "convert",
        "--new-flow",
        "--film-base",
        "0.9,0.6,0.5",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--white-balance",
        "1.1,1,0.9",
        "-o",
        out.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(report["effective_area"]["holder_applied"], true, "{report}");
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("preserved but not used")),
        "{report}"
    );
}

/// A capped or unsettled holder march warns, so `--strict` can see it
/// (`film-base/holder-depth-mask` ship review, M4).
///
/// `capped` and `converged` both mean "the reported rectangle is not a
/// measurement", and as `Serialize`-only fields nothing on any command read them.
/// That is the channel that let a tenfold over-cut through at exit 0 during
/// implementation, caught only because someone was reading the numbers.
#[test]
fn a_capped_holder_march_warns_and_strict_promotes_it() {
    let dir = TempDir::new("capped-march-warns");
    // 400x400 → march cap 100. A 120 px top holder is beyond it, which also
    // inflates left/right from their true 10 px to the cap.
    let path = dir.path("deep.tif");
    const W: u32 = 400;
    const H: u32 = 400;
    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![41_000u16; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = y < 120 || !(10..W - 10).contains(&x) || y >= H - 10;
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            if holder {
                ir[(y * W + x) as usize] = 1_300;
            }
        }
    }
    write_hdri(&path, W, H, &rgb, &ir);

    // `--export-ir` keeps the unrelated "IR preserved but not used" note off the
    // `--strict` run below, so the only thing that can fail it is the cap warning.
    let (out, ir_out) = (dir.path("out.tif"), dir.path("ir.tif"));
    let convert_with = |extra: &[&str]| -> (i32, String, String) {
        let mut args = vec![
            "convert",
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.6,0.5",
            "--input-transfer",
            "linear",
            "--input-meaning",
            "scanner-device",
            "--export-ir",
            ir_out.to_str().unwrap(),
        ];
        args.extend(extra);
        args.extend(["-o", out.to_str().unwrap(), path.to_str().unwrap()]);
        run(&args)
    };

    let (code, stdout, err) = convert_with(&[]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let holder = &report["effective_area"]["holder"];
    assert_eq!(
        (
            holder["top"].as_u64(),
            holder["bottom"].as_u64(),
            holder["left"].as_u64(),
            holder["right"].as_u64()
        ),
        (Some(100), Some(10), Some(100), Some(100)),
        "the beyond-cap frame the warning exists for: {holder}"
    );
    assert_eq!(
        holder["capped"],
        serde_json::json!({"top": true, "bottom": false, "left": true, "right": true}),
        "per-edge, so an inflated edge is distinguishable from a measured one"
    );
    assert!(
        holder["converged"].as_bool().unwrap(),
        "and the cap settles, which is why `converged` cannot be read alone"
    );
    let warnings = report["warnings"].as_array().unwrap();
    let capped = warnings
        .iter()
        .find_map(|w| {
            let w = w.as_str().unwrap();
            w.contains("depth march hit its cap").then_some(w)
        })
        .unwrap_or_else(|| panic!("the cap must warn: {warnings:?}"));
    assert!(
        capped.contains("top, left, right") && capped.contains("artifacts"),
        "naming the edges and the consequence: {capped}"
    );

    // Falsifiability, and the `--strict` half: the same frame with a sub-cap holder
    // warns about nothing, while the capped one fails.
    let (code, _, err) = convert_with(&["--strict"]);
    assert_eq!(code, 1, "--strict must promote it: {err}");
    assert!(err.contains("depth march hit its cap"), "{err}");

    let shallow = dir.path("shallow.tif");
    for y in 0..120u32 {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = y < 10 || !(10..W - 10).contains(&x);
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            ir[(y * W + x) as usize] = if holder { 1_300 } else { 41_000 };
        }
    }
    write_hdri(&shallow, W, H, &rgb, &ir);
    let (code, stdout, err) = run(&[
        "convert",
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.6,0.5",
        "--input-transfer",
        "linear",
        "--input-meaning",
        "scanner-device",
        "--export-ir",
        dir.path("ir2.tif").to_str().unwrap(),
        "--strict",
        "-o",
        dir.path("out2.tif").to_str().unwrap(),
        shallow.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "falsifiability: a sub-cap march must not warn at all: {err}"
    );
    assert_eq!(
        json(&stdout)["effective_area"]["holder"]["capped"],
        serde_json::json!({"top": false, "bottom": false, "left": false, "right": false}),
        "{stdout}"
    );
}

// ---------------------------------------------------------------------------
// `--new-flow` — the migration selector (`nf-core/new-flow-flag`)
// ---------------------------------------------------------------------------
//
// Every test below is scaffolding with the same expiry as the flag itself:
// `nf-core/default-flip` deletes the flag and these tests with it. Since
// `nf-core/minimal-end-to-end` the flag renders — the fixed decode, the new chain and
// one destination, a Display P3 16-bit TIFF — so "accepted" means exit 0 and a file.

#[test]
fn without_new_flow_nothing_moves() {
    // The flag's first contract: absent, it changes nothing, and it never becomes a
    // recipe key. The *parity* half of this test is gone and deliberately so — since
    // `nf-reconstruction/fixed-decode` the new flow decodes through its own params,
    // so the resolved legacy recipe no longer describes what it would render, and
    // under the flag `--dump-params` writes the new chain's own document
    // (`nf-core/recipe-schema`) instead. What is assertable: the no-flag path dumps and
    // converts exactly as before, nothing named `new_flow` reaches either recipe, and
    // the new-flow dump round-trips under the flag and is refused without it.
    let tmp = TempDir::new("new-flow-params");
    let dump_off = tmp.path("off.json");
    let dump_on = tmp.path("on.json");
    // `--output-preset display-p3` rides in `flow` rather than the shared list: the new
    // flow refuses that flag (it has one fixed destination), while the current-chain run
    // needs it to accept a `.tif` path.
    let args = |dump: &Path, out: &Path, flow: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = vec![
            "convert".into(),
            fixture("hdr-48bit.tif").display().to_string(),
            "-o".into(),
            out.display().to_string(),
            "--film-base".into(),
            "0.9,0.55,0.42".into(),
            "--dump-params".into(),
            dump.display().to_string(),
            "--report".into(),
            "none".into(),
        ];
        v.extend(flow.iter().map(|s| (*s).to_string()));
        v
    };
    fn borrow(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    let off = args(
        &dump_off,
        &tmp.path("off.tif"),
        &["--output-preset", "display-p3"],
    );
    let (code, _out, err) = run(&borrow(&off));
    assert_eq!(code, 0, "the no-flag path still converts: {err}");

    let dumped = std::fs::read_to_string(&dump_off).unwrap();
    assert!(
        !dumped.contains("new_flow"),
        "`--new-flow` must not leak into the recipe — it is not a recipe key: {dumped}"
    );

    // With the flag, the dump is the new chain's document: it states its version and
    // none of the current chain's sections, and reloads under the same flag to the
    // same document — the round-trip the recipe schema is gated on.
    let on = args(&dump_on, &tmp.path("on.tif"), &["--new-flow"]);
    let (code, _out, err) = run(&borrow(&on));
    assert_eq!(code, 0, "{err}");
    let dumped = std::fs::read_to_string(&dump_on).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&dumped).unwrap();
    assert_eq!(doc["recipe_version"], 2, "{dumped}");
    for gone in ["print", "output"] {
        assert!(doc.get(gone).is_none(), "`{gone}` leaked into {dumped}");
    }
    assert!(!dumped.contains("new_flow"), "{dumped}");
    assert!(
        !dumped.contains("\"dmax\""),
        "no reference density in {dumped}"
    );

    let redump = tmp.path("redump.json");
    let replay: Vec<String> = [
        "convert".to_string(),
        fixture("hdr-48bit.tif").display().to_string(),
        "-o".into(),
        tmp.path("replay.tif").display().to_string(),
        "--new-flow".into(),
        "--params".into(),
        dump_on.display().to_string(),
        "--dump-params".into(),
        redump.display().to_string(),
        "--report".into(),
        "none".into(),
    ]
    .to_vec();
    let (code, _out, err) = run(&borrow(&replay));
    assert_eq!(code, 0, "the dump reloads and renders: {err}");
    assert_eq!(
        std::fs::read_to_string(&redump).unwrap(),
        dumped,
        "a new-flow dump must reload to byte-identical output"
    );

    // …and the same document is refused without the flag, by name rather than as an
    // unknown field.
    let mut off_replay = replay.clone();
    off_replay.retain(|a| a != "--new-flow");
    let (code, _out, err) = run(&borrow(&off_replay));
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("recipe_version") && err.contains("only `--new-flow` reads"),
        "{err}"
    );
}

/// The `u16` samples of a TIFF `hanten` wrote.
fn read_u16_tiff(path: &Path) -> Vec<u16> {
    use tiff::decoder::{Decoder, DecodingResult};
    let mut dec =
        Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap())).unwrap();
    match dec.read_image().unwrap() {
        DecodingResult::U16(data) => data,
        other => panic!(
            "expected u16 samples, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Per-channel means of interleaved RGB `u16` samples.
fn channel_means(samples: &[u16]) -> [f64; 3] {
    let mut sum = [0f64; 3];
    for px in samples.as_chunks::<3>().0 {
        for c in 0..3 {
            sum[c] += f64::from(px[c]);
        }
    }
    sum.map(|v| v / (samples.len() / 3) as f64)
}

#[test]
fn new_flow_applies_scene_correction() {
    // `nf-scene-correction/stage` through the binary: each knob reaches the pixels,
    // and the report states what was applied.
    let tmp = TempDir::new("new-flow-scene");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let convert = |name: &str, extra: &[&str]| {
        let out = tmp.path(name);
        let mut argv = vec![
            "convert",
            input.as_str(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{extra:?}: {err}");
        (out, json(&stdout))
    };
    let applied = |report: &serde_json::Value| report["new_flow"]["stages"][0]["applied"].clone();

    let (plain, report) = convert("plain.tiff", &[]);
    let sc = &report["new_flow"]["scene_correction"];
    assert_eq!(
        *sc,
        serde_json::json!({"white_balance": [1.0, 1.0, 1.0], "exposure": 0.0}),
        "the gains are always stated, so there is no provenance to report"
    );
    assert_eq!(applied(&report), "identity");
    let plain_means = channel_means(&read_u16_tiff(&plain));

    // Exposure: one stop down darkens every channel.
    let (darker, report) = convert("darker.tiff", &["--exposure", "-1"]);
    assert_eq!(applied(&report), "exposure");
    assert_eq!(report["new_flow"]["scene_correction"]["exposure"], -1.0);
    let darker_means = channel_means(&read_u16_tiff(&darker));
    for c in 0..3 {
        assert!(
            darker_means[c] < plain_means[c],
            "channel {c}: {darker_means:?}"
        );
    }

    // Stated white balance: warms red against blue, and is reported as applied.
    let (warm, report) = convert("warm.tiff", &["--white-balance", "1.3,1,0.7"]);
    assert_eq!(applied(&report), "white-balance");
    let sc = &report["new_flow"]["scene_correction"];
    assert_eq!(
        sc["white_balance"],
        serde_json::json!([1.3, 1.0, 0.7]),
        "{sc}"
    );
    let warm_means = channel_means(&read_u16_tiff(&warm));
    assert!(
        warm_means[0] / warm_means[2] > plain_means[0] / plain_means[2],
        "{warm_means:?} vs {plain_means:?}"
    );

    // The recipe spelling reaches the same knobs, and a dump writes them back.
    let recipe = write_file(
        &tmp.path("scene.json"),
        r#"{ "recipe_version": 2,
             "scene_correction": { "white_balance": {"explicit": [1.3, 1.0, 0.7]} } }"#,
    );
    let dump = tmp.path("dump.json");
    let (from_recipe, _) = convert(
        "recipe.tiff",
        &[
            "--params",
            recipe.to_str().unwrap(),
            "--dump-params",
            dump.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(&from_recipe).unwrap(),
        std::fs::read(&warm).unwrap(),
        "the recipe key and the flag are one knob"
    );
    let dumped: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).unwrap()).unwrap();
    assert_eq!(
        dumped["scene_correction"],
        serde_json::json!({"white_balance": {"explicit": [1.3, 1.0, 0.7]}, "exposure": 0.0})
    );
}

#[test]
fn new_flow_fits_the_scene_range_with_the_stated_headroom() {
    // `nf-display-stages/fit-range` through the binary: the headroom flag reaches the
    // stage, the report names the operator and its arguments rather than describing
    // them, and zero headroom is the identity — which clips far more of an unbounded
    // decode at the encode than the default does.
    let tmp = TempDir::new("new-flow-fit-range");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let convert = |name: &str, extra: &[&str]| {
        let out = tmp.path(name);
        let mut argv = vec![
            "convert",
            input.as_str(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{extra:?}: {err}");
        (out, json(&stdout))
    };
    // A run that clipped nothing carries no `warnings` array at all.
    let clipped = |report: &serde_json::Value| {
        report["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w.as_str())
            .find_map(|w| w.strip_prefix("output lost "))
            .and_then(|w| w.split(' ').next())
            .map_or(0, |n| n.parse::<u64>().unwrap())
    };

    let (default, report) = convert("default.tiff", &[]);
    assert_eq!(report["new_flow"]["fit_range"]["headroom_stops"], 6.0);
    let default_clipped = clipped(&report);

    let (four, report) = convert("four.tiff", &["--display-tone-headroom", "4"]);
    let fr = &report["new_flow"]["fit_range"];
    assert_eq!(fr["operator"], "reinhard-peak-lifted-v1", "{fr}");
    assert_eq!(fr["headroom_stops"], 4.0);
    assert_eq!(fr["white_point"], 16.0);
    assert_ne!(read_u16_tiff(&four), read_u16_tiff(&default));

    let (_, report) = convert("zero.tiff", &["--display-tone-headroom", "0"]);
    assert_eq!(report["new_flow"]["fit_range"]["operator"], "identity");
    assert_eq!(report["new_flow"]["stages"][2]["applied"], "identity");
    assert!(
        clipped(&report) > default_clipped,
        "the identity must clip more than reinhard: {} vs {default_clipped}",
        clipped(&report)
    );

    // A bad headroom is refused by its recipe key as well as its flag.
    let (code, _out, err) = run(&[
        "convert",
        input.as_str(),
        "-o",
        tmp.path("bad.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--new-flow",
        "--display-tone-headroom",
        "-1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`fit_range.headroom_stops`"), "{err}");
}

#[test]
fn exposure_is_the_new_flows_spelling_and_each_chain_refuses_the_other() {
    let tmp = TempDir::new("exposure-spelling");
    let out = tmp.path("out.tif");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let base = [
        "convert",
        input.as_str(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--output-preset",
        "display-p3",
    ];
    let (code, _, err) = run(&[&base[..], &["--exposure", "1"]].concat());
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("--exposure") && err.contains("`--print-exposure`"),
        "{err}"
    );
    // Control: the same line with the current chain's spelling is accepted.
    let (code, _, err) = run(&[&base[..], &["--print-exposure", "1"]].concat());
    assert_eq!(code, 0, "{err}");
}

#[test]
fn new_flow_renders_a_display_p3_tiff() {
    // `nf-core/minimal-end-to-end`: the fixed decode → the new chain → its one
    // destination. Both fixtures, so the IR-carrying path is covered too. The pass bar
    // is "a file that decodes and is not obviously broken" — whether it *looks* right
    // is `nf-calibration`'s question.
    let tmp = TempDir::new("new-flow-render");
    for name in ["hdr-48bit.tif", "hdri-64bit.tif"] {
        // A bare stem: the path is completed from the new flow's destination.
        let stem = tmp.path(name.trim_end_matches(".tif"));
        let (code, stdout, err) = run(&[
            "convert",
            fixture(name).to_str().unwrap(),
            "-o",
            stem.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
        ]);
        assert_eq!(code, 0, "{name}: {err}");
        let out = PathBuf::from(format!("{}.tiff", stem.display()));
        assert!(is_tiff(&out), "{name}: {} must be a TIFF", out.display());
        assert_eq!(read_tiff_bits(&out), 16, "{name}");

        let report = json(&stdout);
        assert_eq!(report["output"], out.to_str().unwrap());
        let nf = &report["new_flow"];
        assert_eq!(nf["destination"], "display-p3-u16-tiff", "{stdout}");
        assert_eq!(nf["gamut"], "display-p3");
        assert_eq!(nf["decode"]["anchor_rule"], "mid-at-base-offset");
        assert_eq!(nf["decode"]["reads_reference"], false);
        let applied: Vec<&str> = nf["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["applied"].as_str().unwrap())
            .collect();
        assert_eq!(
            applied,
            [
                "identity",
                "contrast+highlight-desaturation",
                "reinhard-peak-lifted-v1",
                "acescg-to-display-p3-matrix+neutral-axis-radial-boundary-v2"
            ],
            "the report states what each stage did, identities included"
        );
        assert_eq!(
            nf["fit_range"],
            serde_json::json!({
                "operator": "reinhard-peak-lifted-v1",
                "headroom_stops": 6.0,
                "white_point": 64.0,
                "display_peak": 1.0,
            }),
            "{stdout}"
        );
        // No legacy-chain section claims an operation this run did not perform, and no
        // sidecar or recipe echo describes a chain it did not select.
        for absent in [
            "reconstruction_result",
            "output_render",
            "dmax",
            "white_balance",
            "recipe",
        ] {
            assert!(
                report.get(absent).is_none(),
                "{absent} must be absent: {stdout}"
            );
        }
        assert!(report["identity"].get("params_hash").is_none(), "{stdout}");
        assert_eq!(nf["sidecar_written"], false);
        assert!(!sidecar_of(&out).exists(), "no sidecar under --new-flow");
    }
}

#[test]
fn new_flow_embeds_the_profile_the_display_p3_preset_embeds() {
    // "The declared profile matches the pixels": the chain's exit is in linear Display
    // P3 and the destination applies only the sRGB curve, so the profile must be the
    // same synthesized Display P3 profile the shipped `display-p3` preset embeds. The
    // pixel half — code values are that curve over the matrix — is pinned by unit tests
    // in `pipeline::color` and `pipeline::chain`. Compared against another run of the
    // same binary, never a checked-in hash (lcms2 bytes differ per target).
    let tmp = TempDir::new("new-flow-profile");
    let new = tmp.path("new.tiff");
    let preset = tmp.path("preset.tiff");
    let base = ["--film-base", "0.9,0.55,0.42", "--report", "none"];
    let input = fixture("hdr-48bit.tif");
    let mut a = vec![
        "convert",
        input.to_str().unwrap(),
        "-o",
        new.to_str().unwrap(),
        "--new-flow",
    ];
    a.extend_from_slice(&base);
    let (code, _o, err) = run(&a);
    assert_eq!(code, 0, "{err}");
    let mut b = vec![
        "convert",
        input.to_str().unwrap(),
        "-o",
        preset.to_str().unwrap(),
        "--output-preset",
        "display-p3",
    ];
    b.extend_from_slice(&base);
    let (code, _o, err) = run(&b);
    assert_eq!(code, 0, "{err}");
    assert_eq!(read_icc_tag(&new), read_icc_tag(&preset));
}

#[test]
fn new_flow_convert_is_deterministic() {
    let tmp = TempDir::new("new-flow-determinism");
    let render = |out: &Path| {
        let (code, _o, err) = run(&[
            "convert",
            fixture("hdri-64bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            "--report",
            "none",
        ]);
        assert_eq!(code, 0, "{err}");
        std::fs::read(out).unwrap()
    };
    assert_eq!(
        render(&tmp.path("a.tiff")),
        render(&tmp.path("b.tiff")),
        "the same inputs must produce identical bytes"
    );
}

#[test]
fn new_flow_memory_gate_admits_at_the_modelled_peak_and_rejects_below_it() {
    // The preflight sizes the new flow with its own `RunProfile`; a budget one byte
    // under the estimate it reports must exit 6 before anything is written, and the
    // estimate itself must pass.
    let tmp = TempDir::new("new-flow-memory");
    let run_with = |out: &Path, budget: Option<&str>| {
        let mut argv = vec![
            "convert".to_string(),
            fixture("hdri-64bit.tif").display().to_string(),
            "-o".into(),
            out.display().to_string(),
            "--film-base".into(),
            "0.9,0.55,0.42".into(),
            "--new-flow".into(),
        ];
        if let Some(b) = budget {
            argv.extend(["--max-memory".into(), b.to_string()]);
        }
        run(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let (code, stdout, err) = run_with(&tmp.path("probe.tiff"), None);
    assert_eq!(code, 0, "{err}");
    let peak = json(&stdout)["memory"]["estimated_peak_bytes"]
        .as_u64()
        .unwrap();

    let under = tmp.path("under.tiff");
    let (code, _o, err) = run_with(&under, Some(&(peak - 1).to_string()));
    assert_eq!(code, 6, "{err}");
    assert!(!under.exists(), "a rejected run writes nothing");

    let (code, _o, err) = run_with(&tmp.path("at.tiff"), Some(&peak.to_string()));
    assert_eq!(code, 0, "the modelled peak itself must be admitted: {err}");
}

#[test]
fn new_flow_refuses_telemetry() {
    // The record names the resolved output preset and times the legacy chain's
    // buckets, so under the flag it would describe a chain the run did not take.
    let tmp = TempDir::new("new-flow-telemetry");
    let (code, _o, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("out.tiff").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--new-flow",
        "--telemetry-file",
        tmp.path("t.json").to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--telemetry"), "{err}");
    assert!(err.contains("nf-core/report-contract"), "{err}");
    assert!(!tmp.path("out.tiff").exists());
}

#[test]
fn a_recipe_cannot_select_the_flow() {
    // CLI-only means a recipe naming it is rejected, not ignored. Free from
    // `deny_unknown_fields` — pinned so a future `new_flow` field on `ResolvedConfig`
    // (which would make the flag a knob) cannot land quietly.
    let tmp = TempDir::new("new-flow-recipe");
    let recipe = write_file(&tmp.path("recipe.json"), r#"{"new_flow": true}"#);
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("out.tif").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("unknown field `new_flow`"), "{err}");
}

#[test]
fn new_flow_refuses_a_knob_whose_counterpart_has_not_landed() {
    // The presence half of the availability gate, and its wording: "not yet" advises
    // waiting, where the other verdict advises replacing. Asserting the *losing*
    // wording is absent is the only way to tell the two rules apart — both name the
    // knob, so `err.contains(<knob>)` cannot distinguish them.
    let tmp = TempDir::new("new-flow-not-yet");
    let out = tmp.path("out.tif");
    let flags: &[&str] = &[
        "convert",
        &fixture("hdr-48bit.tif").display().to_string(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--black-point",
        "0.01",
        "--report",
        "none",
    ];
    let mut with_flow = flags.to_vec();
    with_flow.push("--new-flow");
    let (code, _out, err) = run(&with_flow);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--black-point"), "names the knob typed: {err}");
    assert!(err.contains("no counterpart for it yet"), "{err}");
    assert!(
        !err.contains("will not gain one"),
        "the losing verdict's wording must be absent: {err}"
    );
    assert!(
        !err.contains('*'),
        "terminal output carries no markdown: {err}"
    );

    // Falsifiability: the same knob is accepted on the current chain.
    let (code, _out, err) = run(flags);
    assert_eq!(code, 0, "the control run must succeed: {err}");
}

#[test]
fn new_flow_refuses_a_knob_the_design_drops() {
    // The other verdict: a knob the new design drops for good names its replacement.
    let tmp = TempDir::new("new-flow-never");
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("out.tif").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--auto-wb",
        "gray-world",
        "--new-flow",
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("will not gain one"), "{err}");
    assert!(
        err.contains("measure-roll"),
        "a `never` verdict names the replacement: {err}"
    );
    assert!(
        !err.contains("no counterpart for it yet"),
        "the losing verdict's wording must be absent: {err}"
    );

    // Falsifiability: the same knob renders on the current chain.
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("control.tif").to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--auto-wb",
        "gray-world",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "the control run must succeed: {err}");
}

#[test]
fn the_availability_gate_outranks_the_rules_it_would_confuse() {
    // Ordering, driven through the binary rather than by calling a rule directly —
    // a direct call exercises the rule and never the ordering, which is how the
    // fourth circular-remedy defect reached CI. With no film base *and* an
    // unavailable knob, the unavailable knob must be diagnosed first: "no film base
    // selected" is the least-specific diagnosis in `validate`, and following it
    // would just earn the user this error on the next run.
    let tmp = TempDir::new("new-flow-order");
    for knob in [["--auto-wb", "gray-world"], ["--black-point", "0.01"]] {
        let (code, _out, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            tmp.path("out.tif").to_str().unwrap(),
            knob[0],
            knob[1],
            "--new-flow",
            "--report",
            "none",
        ]);
        assert_eq!(code, 2, "{err}");
        assert!(
            err.contains("has no meaning under `--new-flow`"),
            "{knob:?} must be diagnosed before the missing film base: {err}"
        );
        assert!(
            !err.contains("no film base selected"),
            "the less specific diagnosis must not win: {err}"
        );
    }
}

/// The reference density and the three anchor placements that read it (or pinned
/// black) retired together (`nf-retire/dmax-machinery`), on both chains: every flag is
/// a usage error naming the one placement left, a recipe's `calibration.dmax` replays
/// at its old default and is refused otherwise, and a retired `anchor` in a recipe is
/// refused by name.
#[test]
fn the_reference_density_and_retired_placements_are_migration_errors() {
    let tmp = TempDir::new("dmax-retired");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("out.tif");

    // (a) Each removed flag, on each chain. The remedy must itself be accepted there.
    for flags in [
        vec!["--d-max", "1.6"],
        vec!["--fixed-d-max"],
        vec!["--auto-d-max"],
        vec!["--no-d-max"],
        vec!["--anchor-white-at-reference"],
        vec!["--anchor-mid-fraction", "0.5"],
        vec!["--anchor-black-floor", "0.005"],
    ] {
        for new_flow in [false, true] {
            let mut argv = vec![
                "convert",
                scan.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--film-base",
                "0.9,0.55,0.42",
            ];
            argv.push(if new_flow {
                "--new-flow"
            } else {
                "--output-preset"
            });
            if !new_flow {
                argv.push("display-p3");
            }
            argv.extend_from_slice(&flags);
            let (code, _, err) = run(&argv);
            assert_eq!(code, 2, "{flags:?} (new flow {new_flow}): {err}");
            assert!(
                err.contains(flags[0]) && err.contains("was removed"),
                "{flags:?}: {err}"
            );
            assert!(err.contains("--anchor-mid-offset"), "{flags:?}: {err}");
            assert!(!out.exists(), "{flags:?}: nothing may be written");
        }
    }
    for new_flow in [false, true] {
        let mut argv = vec![
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--anchor-mid-offset",
            "0.5",
            "--report",
            "none",
        ];
        argv.extend(if new_flow {
            vec!["--new-flow"]
        } else {
            vec!["--output-preset", "display-p3"]
        });
        let (code, _, err) = run(&argv);
        assert_eq!(
            code, 0,
            "the named remedy must work (new flow {new_flow}): {err}"
        );
        std::fs::remove_file(&out).ok();
    }

    // (b) `estimate`'s reference half.
    let (code, _, err) = run(&[
        "estimate",
        scan.to_str().unwrap(),
        "--d-max-region",
        "0,0,1,1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("--d-max-region") && err.contains("was removed"),
        "{err}"
    );

    // (c) A recipe's `calibration.dmax`: the old default replays, anything else is
    // refused — on `convert` and on a roll per-frame override alike.
    let convert_with = |name: &str, body: &str| {
        let recipe = write_file(&tmp.path(name), body);
        let o = tmp.path(&format!("{name}.tif"));
        let r = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            o.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        (r.0, r.2)
    };
    let (code, err) = convert_with(
        "fixed.json",
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]},"dmax":"fixed"}}"#,
    );
    assert_eq!(
        code, 0,
        "an old sidecar's default reference must replay: {err}"
    );
    for (name, dmax) in [
        ("explicit.json", r#"{"explicit":1.5}"#),
        ("auto.json", r#""auto""#),
        ("none.json", r#""none""#),
    ] {
        let (code, err) = convert_with(
            name,
            &format!(
                r#"{{"calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}},"dmax":{dmax}}}}}"#
            ),
        );
        assert_eq!(code, 2, "{dmax}: {err}");
        assert!(err.contains("calibration.dmax"), "{dmax}: {err}");
        assert!(err.contains("mid-at-base-offset"), "{dmax}: {err}");
    }
    let shared = write_file(&tmp.path("shared.json"), ROLL_RECIPE);
    for (dmax, want) in [(r#""fixed""#, 0), (r#"{"explicit":1.5}"#, 2)] {
        let manifest = write_file(
            &tmp.path("frames.json"),
            &format!(
                r#"{{ "frames": [ {{ "input": {scan:?},
                        "params": {{ "calibration": {{ "dmax": {dmax} }} }} }} ] }}"#,
                scan = scan.to_str().unwrap()
            ),
        );
        let (code, _, err) = run(&[
            "roll",
            "--frames",
            manifest.to_str().unwrap(),
            "--out-dir",
            tmp.path(&format!("roll-{want}")).to_str().unwrap(),
            "--params",
            shared.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(code, want, "per-frame {dmax}: {err}");
        if want == 2 {
            assert!(err.contains("calibration.dmax"), "{err}");
        }
    }

    // (d) A retired placement in a recipe, in both of the spellings serde wrote.
    for anchor in [r#""white-at-dmax""#, r#"{"black-at-base":0.005}"#] {
        let (code, err) = convert_with(
            "anchor.json",
            &format!(
                r#"{{"calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}}}},
                    "reconstruction":{{"curve":{{"type":"exponential","anchor":{anchor}}}}}}}"#
            ),
        );
        assert_eq!(code, 2, "{anchor}: {err}");
        assert!(
            err.contains("was removed") && err.contains("mid-at-base-offset"),
            "{anchor}: {err}"
        );
    }
}

/// `nf-retire/characteristic`: `--density-curve`, `--film-stock` and `--preset` are
/// migration errors on both chains at every value, diagnosed before anything coarser; a
/// sidecar's retired `"type": "exponential"` replays byte-identically; a recipe naming
/// the `characteristic` curve is refused, and the remedy its message gives renders.
#[test]
fn the_characteristic_curve_is_a_migration_error() {
    let tmp = TempDir::new("characteristic-retired");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("out.tif");

    // (a) Each flag, on each chain, with no film base: the removed flag is the more
    // specific diagnosis and must win over "no film base selected".
    for flags in [
        vec!["--density-curve", "characteristic"],
        vec!["--density-curve", "exponential"],
        vec!["--density-curve"],
        vec!["--film-stock", "portra-400"],
        vec!["--preset", "characteristic-generic"],
        vec!["--preset", "sigmoid-knees"],
    ] {
        for new_flow in [false, true] {
            let mut argv = vec![
                "convert",
                scan.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--report",
                "none",
            ];
            if new_flow {
                argv.push("--new-flow");
            }
            argv.extend_from_slice(&flags);
            let (code, _, err) = run(&argv);
            assert_eq!(code, 2, "{flags:?} (new flow {new_flow}): {err}");
            assert!(
                err.contains(&format!("{} was removed", flags[0])),
                "{flags:?}: {err}"
            );
            assert!(!err.contains("no film base selected"), "{flags:?}: {err}");
            assert!(!out.exists(), "no output on a usage error");
        }
    }

    // (b) A recipe: every earlier sidecar carries the curve's `"type": "exponential"`,
    // which replays byte-identically to the same curve without it.
    let render_with = |name: &str, reconstruction: &str| {
        let recipe = write_file(
            &tmp.path(name),
            &format!(
                r#"{{"calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}}}},
                    "reconstruction":{reconstruction}}}"#
            ),
        );
        let o = tmp.path(&format!("{name}.tif"));
        let (code, _, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            o.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        (code, err, o)
    };
    let (code, err, plain) = render_with(
        "plain.json",
        r#"{"curve":{"gamma":2.0,"anchor":{"mid-at-base-offset":0.62}}}"#,
    );
    assert_eq!(code, 0, "{err}");
    let (code, err, tagged) = render_with(
        "old-sidecar.json",
        r#"{"curve":{"type":"exponential","gamma":2.0,"anchor":{"mid-at-base-offset":0.62}}}"#,
    );
    assert_eq!(code, 0, "an old sidecar's curve tag must replay: {err}");
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&tagged).unwrap(),
        "the stripped tag renders exactly as its absence"
    );

    // (c) A characteristic sidecar is refused, naming its identity gain, and the remedy
    // (drop the curve and that gain) renders the default.
    let (code, err, _) = render_with(
        "characteristic.json",
        r#"{"density":{"scale":[1.0,1.0,1.0]},
            "curve":{"type":"characteristic","stock":"portra-400"}}"#,
    );
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("`characteristic` density curve")
            && err.contains("reconstruction.density.scale"),
        "{err}"
    );
    let (code, err, _) = render_with("remedy.json", "{}");
    assert_eq!(code, 0, "the remedy must render: {err}");

    // (d) Nothing the tool writes carries the retired surface.
    let (code, stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert!(report.get("conversion_preset").is_none(), "{report}");
    let curve = &report["reconstruction_result"]["curve"];
    for key in ["type", "stock", "out_of_table"] {
        assert!(
            curve.get(key).is_none(),
            "report curve carries `{key}`: {curve}"
        );
    }
    assert!(
        sidecar_params(&out)["reconstruction"]["curve"]
            .get("type")
            .is_none()
    );
}

/// `nf-retire/regional-balance`: the four flags and three recipe keys are migration
/// errors on both chains, their neutral defaults replay, and the report no longer
/// carries a balance range.
#[test]
fn the_regional_balance_is_a_migration_error() {
    let tmp = TempDir::new("balance-retired");
    let scan = fixture("hdr-48bit.tif");
    let out = tmp.path("out.tif");
    let convert = |extra: &[&str], new_flow: bool| {
        let mut argv = vec![
            "convert",
            scan.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--report",
            "none",
        ];
        argv.extend(if new_flow {
            vec!["--new-flow"]
        } else {
            vec!["--output-preset", "display-p3"]
        });
        argv.extend_from_slice(extra);
        let r = run(&argv);
        std::fs::remove_file(&out).ok();
        (r.0, r.2)
    };

    // (a) Each removed flag, on each chain, at every value — the identity `0,0,0`, a
    // negative value in both spellings, a bare flag — reaches the migration message,
    // not clap's.
    for flags in [
        vec!["--shadow-balance", "0.1,0,0"],
        vec!["--shadow-balance", "-0.05,0,0"],
        vec!["--shadow-balance=-0.05,0,0"],
        vec!["--highlight-balance", "0,0,0"],
        vec!["--balance-range", "0.2,1.2"],
        vec!["--balance-range"],
        vec!["--auto-balance-range"],
    ] {
        let flag = flags[0].split('=').next().unwrap();
        for new_flow in [false, true] {
            let (code, err) = convert(&flags, new_flow);
            assert_eq!(code, 2, "{flags:?} (new flow {new_flow}): {err}");
            assert!(
                err.contains(&format!("{flag} was removed with the regional balance")),
                "{flags:?}: {err}"
            );
            // The grade is named with the chain that has it, and the message says the
            // current chain has none — never "use --channel-grade" bare, which the
            // current chain refuses.
            assert!(
                err.contains(
                    "`--channel-grade R,B` (recipe `look.channel_grade`) under `--new-flow`"
                ) && err.contains("the current chain has no counterpart")
                    && err.contains("Drop the flag"),
                "{flags:?}: {err}"
            );
            // It says why the grade is not a rename: the measurement is gone.
            assert!(err.contains("measures nothing"), "{flags:?}: {err}");
        }
    }
    // Both named remedies are accepted where the message says they are.
    let (code, err) = convert(&["--channel-grade", "0.95,1.05"], true);
    assert_eq!(code, 0, "the grade under --new-flow: {err}");
    for new_flow in [false, true] {
        let (code, err) = convert(&["--density-offset", "0.05,0,-0.02"], new_flow);
        assert_eq!(code, 0, "--density-offset (new flow {new_flow}): {err}");
    }

    // (b) A recipe: every earlier sidecar carries the three keys at their neutral
    // defaults, which replay — byte-identically to a recipe without them.
    let render_with = |name: &str, density: &str| {
        let recipe = write_file(
            &tmp.path(name),
            &format!(
                r#"{{"calibration":{{"film_base":{{"explicit":[0.9,0.55,0.42]}}}},
                    "reconstruction":{{"density":{{"scale":[1.0,0.84,0.73],
                    "offset":[0.0,0.0,0.0]{density}}}}}}}"#
            ),
        );
        let o = tmp.path(&format!("{name}.tif"));
        let (code, _, err) = run(&[
            "convert",
            scan.to_str().unwrap(),
            "-o",
            o.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
        ]);
        (code, err, o)
    };
    let (code, err, plain) = render_with("plain.json", "");
    assert_eq!(code, 0, "{err}");
    let (code, err, old) = render_with(
        "old-sidecar.json",
        r#","shadow_balance":[0.0,0.0,0.0],"highlight_balance":[0.0,0.0,0.0],
           "balance_range":"auto""#,
    );
    assert_eq!(
        code, 0,
        "an old sidecar's neutral balance must replay: {err}"
    );
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&old).unwrap(),
        "the stripped neutral balance renders exactly as its absence"
    );
    // Neutral as the old f32 fields read it: `-0.0`, integer `0` and a value underflowing
    // f32 were all `[0, 0, 0]` there, so they strip too.
    let (code, err, tiny) = render_with(
        "f32-neutral.json",
        r#","shadow_balance":[1e-50,0,-0.0],"highlight_balance":[0,0,0]"#,
    );
    assert_eq!(code, 0, "an f32-neutral balance must replay: {err}");
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&tiny).unwrap(),
        "an f32-neutral balance renders exactly as its absence"
    );

    // Anything else is refused, naming the key and the grade. A differing pair is a
    // lost render, with or without an explicit range beside it.
    for (name, density, key) in [
        (
            "shadow.json",
            r#","shadow_balance":[0.1,0.0,0.0]"#,
            "reconstruction.density.shadow_balance",
        ),
        (
            "crossover.json",
            r#","shadow_balance":[0.1,0.0,0.0],"highlight_balance":[-0.1,0.0,0.0]"#,
            "reconstruction.density.shadow_balance",
        ),
        (
            "crossover-range.json",
            r#","shadow_balance":[0.1,0.0,0.0],"highlight_balance":[-0.1,0.0,0.0],
               "balance_range":{"explicit":[0.2,1.6]}"#,
            "reconstruction.density.shadow_balance",
        ),
    ] {
        let (code, err, _) = render_with(name, density);
        assert_eq!(code, 2, "{name}: {err}");
        assert!(err.contains(key), "{name}: {err}");
        assert!(err.contains("look.channel_grade"), "{name}: {err}");
        assert!(err.contains("reference build"), "{name}: {err}");
        assert!(
            !err.contains("reconstruction.density.offset"),
            "{name}: {err}"
        );
    }
    // An equal pair was a uniform offset: the message names the exact replacement.
    let (code, err, _) = render_with(
        "equal.json",
        r#","shadow_balance":[0.05,0.0,-0.02],"highlight_balance":[0.05,0.0,-0.02]"#,
    );
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(
            "set `reconstruction.density.offset` to the offset this run resolves plus the pair"
        ) && err.contains("replays the render (exactly over a zero offset"),
        "{err}"
    );
    assert!(!err.contains("reference build"), "{err}");
    // Equal as the old f32 fields, though not as JSON numbers: still the offset remedy.
    let (code, err, _) = render_with(
        "equal-f32.json",
        r#","shadow_balance":[0.1,0.0,0.0],"highlight_balance":[0.1000000001,0.0,0.0]"#,
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("reconstruction.density.offset"), "{err}");
    assert!(!err.contains("reference build"), "{err}");

    // An explicit range beside equal (here absent) balances was never consulted: it is
    // refused (only the old default is stripped), but its remedy renders unchanged.
    // This is also what an equal pair plus a range reads once the pair has moved.
    let (code, err, _) = render_with("range.json", r#","balance_range":{"explicit":[0.2,1.6]}"#);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("reconstruction.density.balance_range"),
        "{err}"
    );
    assert!(err.contains("the render is unchanged"), "{err}");
    assert!(!err.contains("reference build"), "{err}");
    assert!(!err.contains("reconstruction.density.offset"), "{err}");

    // (c) A roll per-frame override takes the same path.
    let shared = write_file(&tmp.path("shared.json"), ROLL_RECIPE);
    for (balance, want) in [("[0.0,0.0,0.0]", 0), ("[0.1,0.0,0.0]", 2)] {
        let manifest = write_file(
            &tmp.path("frames.json"),
            &format!(
                r#"{{ "frames": [ {{ "input": {scan:?},
                        "params": {{ "reconstruction": {{ "density":
                            {{ "shadow_balance": {balance} }} }} }} }} ] }}"#,
                scan = scan.to_str().unwrap()
            ),
        );
        let (code, _, err) = run(&[
            "roll",
            "--frames",
            manifest.to_str().unwrap(),
            "--out-dir",
            tmp.path(&format!("roll-{want}")).to_str().unwrap(),
            "--params",
            shared.to_str().unwrap(),
            "--report",
            "none",
        ]);
        assert_eq!(code, want, "per-frame {balance}: {err}");
        if want == 2 {
            assert!(
                err.contains("reconstruction.density.shadow_balance"),
                "{err}"
            );
        }
    }

    // (d) Neither the report nor the resolved parameters carry a balance any more.
    let (code, stdout, err) = run(&[
        "convert",
        scan.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--output-preset",
        "display-p3",
        "--report",
        "json",
    ]);
    assert_eq!(code, 0, "{err}");
    let sidecar = std::fs::read_to_string(tmp.path("out.tif.json")).expect("the sidecar recipe");
    let (code, params, err) = run(&["params"]);
    assert_eq!(code, 0, "{err}");
    for (what, text) in [
        ("report", &stdout),
        ("sidecar", &sidecar),
        ("params", &params),
    ] {
        for key in ["balance_range", "shadow_balance", "highlight_balance"] {
            assert!(!text.contains(key), "{what} still carries {key}: {text}");
        }
    }
}

#[test]
fn the_fixed_decodes_own_knobs_reach_the_decode_under_the_new_flow() {
    // The falsifiable control for the refusal table above: everything the fixed
    // decode reads must still be accepted **and must arrive** — an accepted flag the
    // decode never saw is the accepted-and-ignored defect, so the report's resolved
    // `new_flow.decode` block is the witness, not the exit code.
    let tmp = TempDir::new("new-flow-surviving");
    let decode_of = |extra: &[&str], name: &str| -> (i32, serde_json::Value, String) {
        let out = tmp.path(name);
        let mut argv: Vec<String> = vec![
            "convert".into(),
            fixture("hdr-48bit.tif").display().to_string(),
            "-o".into(),
            out.display().to_string(),
            "--film-base".into(),
            "0.9,0.55,0.42".into(),
            "--new-flow".into(),
        ];
        argv.extend(extra.iter().map(|s| (*s).to_string()));
        let (code, stdout, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        let decode = if code == 0 {
            json(&stdout)["new_flow"]["decode"].clone()
        } else {
            serde_json::Value::Null
        };
        (code, decode, err)
    };
    let close = |v: &serde_json::Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-5;

    // The defaults: the decode's own constants.
    let (code, d, err) = decode_of(&[], "default.tiff");
    assert_eq!(code, 0, "{err}");
    assert!(close(&d["linearization"], 1.8), "{d}");
    assert_eq!(d["scale"], serde_json::json!([1.0, 0.84, 0.73]), "{d}");
    // mid-grey 0.62 above base at the linearization 1.8 ⇒ anchor 0.62 + 0.745/1.8.
    assert!(close(&d["anchor"], 0.62 + 0.744_727_5 / 1.8), "{d}");

    let (code, d, err) = decode_of(&["--density-scale", "1,0.9,0.8"], "scale.tiff");
    assert_eq!(code, 0, "{err}");
    assert_eq!(d["scale"], serde_json::json!([1.0, 0.9, 0.8]), "{d}");

    let (code, d, err) = decode_of(&["--density-offset", "0,-0.03,-0.05"], "offset.tiff");
    assert_eq!(code, 0, "{err}");
    assert!(close(&d["offset"][2], -0.05), "{d}");

    let (code, d, err) = decode_of(&["--anchor-mid-offset", "0.7"], "anchor.tiff");
    assert_eq!(code, 0, "{err}");
    assert!(close(&d["anchor"], 0.7 + 0.744_727_5 / 1.8), "{d}");

    // Bare `--density-gamma` too. Before `nf-core/recipe-schema` the new flow merged
    // its flags into the current chain's config, whose default curve was then the
    // sigmoid, so `merge` refused the flag unless `--density-curve exponential` came
    // with it.
    // The new chain's recipe has no curve to disagree with: the flag sets
    // `reconstruction.linearization` directly.
    let (code, d, err) = decode_of(&["--density-gamma", "1.7"], "bare-gamma.tiff");
    assert_eq!(code, 0, "{err}");
    assert!(close(&d["linearization"], 1.7), "{d}");

    // The look's contrast is not the decode's: it leaves the decode block untouched.
    let (code, d, err) = decode_of(&["--contrast", "1.5"], "look-contrast.tiff");
    assert_eq!(code, 0, "{err}");
    assert!(close(&d["linearization"], 1.8), "{d}");
    assert!(close(&d["anchor"], 0.62 + 0.744_727_5 / 1.8), "{d}");
}

/// `--new-flow` refuses a recipe-stated `calibration.dmax` — and still accepts
/// `calibration.film_base`.
///
/// The new chain's `calibration` section holds the film base alone, since the fixed
/// decode's anchor rule reads no reference density. A recipe stating `dmax` there is
/// refused at load by name (`crate::recipe::check_body`) — and so is a `roll`
/// per-frame overlay, which runs the same check.
#[test]
fn convert_under_the_new_flow_refuses_a_recipe_calibration_dmax() {
    let tmp = TempDir::new("new-flow-calibration");
    let run_with = |body: &str, name: &str| -> (i32, String) {
        let recipe = write_file(&tmp.path(name), body);
        let (code, _out, err) = run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            tmp.path("out.tiff").to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--report",
            "none",
            "--new-flow",
        ]);
        (code, err)
    };

    let (code, err) = run_with(
        r#"{"recipe_version":2,
            "calibration":{"film_base":{"explicit":[0.9,0.55,0.42]},
                           "dmax":{"explicit":1.45}}}"#,
        "with-dmax.json",
    );
    assert_eq!(code, 2, "a recipe-stated reference must be refused: {err}");
    assert!(err.contains("`calibration.dmax`"), "{err}");
    assert!(err.contains("reference-free"), "{err}");

    // Falsifiable both ways. The base half is still read, so it renders…
    let (code, err) = run_with(
        r#"{"recipe_version":2,"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}"#,
        "base-only.json",
    );
    assert_eq!(code, 0, "the base half must still be accepted: {err}");
    // A `roll` **per-frame** override states it too, and is refused by the same load
    // check, run on the overlay.
    let shared = write_file(
        &tmp.path("shared.json"),
        r#"{"recipe_version":2,
            "calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},
            "measure":{"inset":0.05}}"#,
    );
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {scan:?},
                    "params": {{ "calibration": {{ "dmax": {{ "explicit": 2.4 }} }} }} }} ] }}"#,
            scan = fixture("hdr-48bit.tif").to_str().unwrap()
        ),
    );
    let (code, _out, err) = run(&[
        "roll",
        "--frames",
        manifest.to_str().unwrap(),
        "--out-dir",
        tmp.path("roll-out").to_str().unwrap(),
        "--params",
        shared.to_str().unwrap(),
        "--new-flow",
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "a per-frame override must be refused too: {err}");
    assert!(err.contains("`calibration.dmax`"), "{err}");

    // …while the current chain still replays the key at its old default `"fixed"`,
    // which every sidecar it wrote carries.
    let recipe = write_file(
        &tmp.path("legacy.json"),
        r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]},
                           "dmax":"fixed"},
            "output":{"preset":"display-p3"}}"#,
    );
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("legacy.tif").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn convert_under_the_new_flow_refuses_the_current_chains_recipe() {
    // The provenance neither availability table can see: a recipe written for the
    // other chain. It would otherwise parse and be read by nothing —
    // `deny_unknown_fields` catches an unknown key and is blind to a known but
    // meaningless one. The document version is what makes it visible.
    let tmp = TempDir::new("new-flow-recipe");
    let run_with = |body: &str, name: &str, flow: &[&str]| -> (i32, String) {
        let recipe = write_file(&tmp.path(name), body);
        let mut v: Vec<String> = vec![
            "convert".into(),
            fixture("hdr-48bit.tif").display().to_string(),
            "-o".into(),
            tmp.path(&format!("{name}.tif")).display().to_string(),
            "--params".into(),
            recipe.display().to_string(),
            "--report".into(),
            "none".into(),
        ];
        v.extend(flow.iter().map(|s| (*s).to_string()));
        let borrowed: Vec<&str> = v.iter().map(String::as_str).collect();
        let (code, _out, err) = run(&borrowed);
        (code, err)
    };
    let current = r#"{
  "reconstruction": { "type": "density" },
  "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } },
  "output": { "preset": "display-p3" }
}"#;

    // (1) No version: the current chain's document, refused as a whole.
    let (code, err) = run_with(current, "current.json", &["--new-flow"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("\"recipe_version\": 2"), "{err}");
    assert!(err.contains("hanten params --new-flow"), "{err}");
    // Falsifiability: the same recipe converts without the flag.
    let (code, err) = run_with(current, "current.json", &[]);
    assert_eq!(code, 0, "{err}");

    // (2) Versioned, but still carrying the current chain's reconstruction keys:
    // refused by key, with where each one went.
    let (code, err) = run_with(
        r#"{"recipe_version": 2,
            "reconstruction": {"density": {"scale": [1, 0.9, 0.8]}},
            "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}}}"#,
        "old-keys.json",
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`reconstruction.density`"), "{err}");
    assert!(err.contains("`reconstruction.scale`"), "{err}");

    // (3) A new-chain recipe stating the decode renders, with its values.
    let (code, err) = run_with(
        r#"{"recipe_version": 2,
            "reconstruction": {"scale": [1, 0.9, 0.8], "linearization": 1.7,
                               "anchor": {"mid-at-base-offset": 0.6}},
            "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}},
            "measure": {"inset": 0.05},
            "look": {}}"#,
        "new.json",
        &["--new-flow"],
    );
    assert_eq!(code, 0, "{err}");
    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("new-report.tif").to_str().unwrap(),
        "--params",
        tmp.path("new.json").to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    let decode = &json(&stdout)["new_flow"]["decode"];
    let close = |v: &serde_json::Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-5;
    assert!(close(&decode["linearization"], 1.7), "{decode}");
    assert!(close(&decode["scale"][1], 0.9), "{decode}");

    // (4) Its values are checked, whichever provenance set them.
    let (code, err) = run_with(
        r#"{"recipe_version": 2, "reconstruction": {"linearization": 0},
            "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}}}"#,
        "bad.json",
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`reconstruction.linearization`"), "{err}");

    // (5) A stage refuses a key it does not have.
    let (code, err) = run_with(
        r#"{"recipe_version": 2, "look": {"saturation": 1.1},
            "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}}}"#,
        "look.json",
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("saturation"), "{err}");

    // (6) The decode's slope before the split is refused by name, with the split's
    // remedy — never read as the linearization it no longer is.
    let (code, err) = run_with(
        r#"{"recipe_version": 2, "reconstruction": {"contrast": 2.0},
            "calibration": {"film_base": {"explicit": [0.9, 0.55, 0.42]}}}"#,
        "old-contrast.json",
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("`reconstruction.contrast` split in two") && err.contains("look.contrast"),
        "{err}"
    );
}

#[test]
fn availability_outranks_the_decodes_value_rules() {
    // Both faults at once: the unavailable knob must be diagnosed first, or the user
    // fixes a value only to be told the flag carrying it must go anyway. The refusal is
    // by presence, before any recipe merges, so it wins by construction — pinned here
    // so a later value rule placed ahead of it reds.
    let tmp = TempDir::new("new-flow-order");
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("out").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--new-flow",
        "--print-exposure",
        "1",
        "--density-scale",
        "0,1,1",
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--print-exposure"), "{err}");
    assert!(!err.contains("reconstruction.scale"), "{err}");
}

#[test]
fn hanten_params_writes_the_schema_the_flag_selects() {
    let (code, legacy, err) = run(&["params"]);
    assert_eq!(code, 0, "{err}");
    let (code, new, err) = run(&["params", "--new-flow"]);
    assert_eq!(code, 0, "{err}");
    let legacy: serde_json::Value = serde_json::from_str(&legacy).unwrap();
    let new: serde_json::Value = serde_json::from_str(&new).unwrap();
    assert!(legacy.get("recipe_version").is_none());
    assert_eq!(new["recipe_version"], 2);
    assert!(new.get("print").is_none() && legacy.get("print").is_some());

    // What it writes is what `--new-flow` reads, once a film base is stated.
    let tmp = TempDir::new("params-new-flow");
    let mut doc = new.clone();
    doc["calibration"]["film_base"] = serde_json::json!({"explicit": [0.9, 0.55, 0.42]});
    let recipe = write_file(&tmp.path("r.json"), &doc.to_string());
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        tmp.path("out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--new-flow",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn new_flow_refuses_every_print_control() {
    // The print family, driven through the binary one flag at a time. Each names the
    // stage that will carry it — or, for `--print-exposure`, the flag that already
    // does — so the refusal tells a user where the knob went rather than only that it
    // is gone. Stated white balance is not here: scene correction reads it under its
    // own spelling (`new_flow_applies_scene_correction`). The per-frame auto one is —
    // retired, with the roll measurement named as its replacement. Nor is
    // `--display-tone-headroom`, which fit range reads
    // (`new_flow_fits_the_scene_range_with_the_stated_headroom`).
    //
    // One of these resolves the documented **default** (`--linear-range 0,1`) and
    // is still refused, which is the tiebreaker applied
    // rather than waived: an identity value is spared to keep the flags-win reset
    // usable, and the new chain's recipe has no `print` section, so on this flow there
    // is no pinned value for one to clear.
    let tmp = TempDir::new("new-flow-print");
    let cases: &[(&[&str], &str)] = &[
        (&["--print-exposure", "1"], "Use `--exposure`"),
        (
            &["--black-point", "0.01"],
            "nf-scene-correction/flare-removal",
        ),
        (
            &["--linear-range", "0,1"],
            "nf-scene-correction/levels-knob",
        ),
        (&["--auto-wb", "percentile"], "hanten measure-roll"),
    ];
    for (i, (extra, expect)) in cases.iter().enumerate() {
        let out = tmp.path(&format!("out{i}.tif"));
        let mut argv: Vec<&str> = vec![
            "convert",
            "FIXTURE",
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            "--report",
            "none",
        ];
        let fixture_path = fixture("hdr-48bit.tif").display().to_string();
        argv[1] = &fixture_path;
        argv.extend_from_slice(extra);
        let (code, _out, err) = run(&argv);
        assert_eq!(code, 2, "{extra:?} must be refused: {err}");
        assert!(err.contains(extra[0]), "{extra:?} must be named: {err}");
        assert!(
            err.contains(expect),
            "{extra:?} must say where it went: {err}"
        );
    }
}

#[test]
fn every_print_control_is_accepted_without_the_flag() {
    // The falsifiable half of the test above, and the whole contract of the gate: a
    // row added by the audit must not start refusing commands that work today. Driven
    // through the binary on the legacy path with the same values.
    let tmp = TempDir::new("print-controls-legacy");
    for (i, extra) in [
        vec!["--print-exposure", "1"],
        vec!["--black-point", "0.01"],
        vec!["--white-balance", "1,1,1"],
        vec!["--auto-wb", "gray-world"],
        vec!["--linear-range", "0,1"],
        vec!["--display-tone-headroom", "6"],
    ]
    .into_iter()
    .enumerate()
    {
        let out = tmp.path(&format!("ok{i}.tif"));
        let mut argv: Vec<&str> = vec![
            "convert",
            "FIXTURE",
            "-o",
            out.to_str().unwrap(),
            "--output-preset",
            "display-p3",
            "--film-base",
            "0.9,0.55,0.42",
            "--report",
            "none",
        ];
        let fixture_path = fixture("hdr-48bit.tif").display().to_string();
        argv[1] = &fixture_path;
        argv.extend_from_slice(&extra);
        let (code, _out, err) = run(&argv);
        assert_eq!(
            code, 0,
            "{extra:?} must still convert without the flag: {err}"
        );
    }
}

#[test]
fn new_flow_refuses_the_output_policy_flags() {
    // The new flow renders into exactly one destination, so there is no output policy
    // to choose. Refused for a different reason than the print family — not "the stage
    // that carries it is empty" — and the message says so by naming the task that
    // settles the destination set.
    //
    // The three selectors that retired with `legacy`/`custom` are not a new-flow
    // question at all: they are removed-flag errors on either chain.
    let tmp = TempDir::new("new-flow-output");
    for (i, extra) in [
        vec!["--output-preset", "display-p3"],
        vec!["--out-depth", "u16"],
        vec!["--output-profile", "srgb"],
        vec!["--bigtiff", "auto"],
    ]
    .into_iter()
    .enumerate()
    {
        let retired = extra[0] != "--output-preset";
        let out = tmp.path(&format!("out{i}.tif"));
        let mut argv: Vec<&str> = vec![
            "convert",
            "FIXTURE",
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            "--report",
            "none",
        ];
        let fixture_path = fixture("hdr-48bit.tif").display().to_string();
        argv[1] = &fixture_path;
        argv.extend_from_slice(&extra);
        let (code, _out, err) = run(&argv);
        assert_eq!(code, 2, "{extra:?} must be refused: {err}");
        assert!(err.contains(extra[0]), "{extra:?} must be named: {err}");
        if retired {
            assert!(err.contains("was removed"), "{extra:?}: {err}");
            // The remedy must work on this chain: every preset flag is refused under
            // `--new-flow`, so advice to pick one would only trade errors.
            assert!(err.contains("drop it"), "{extra:?}: {err}");
            assert!(!err.contains("`display-p3` preset"), "{extra:?}: {err}");
        } else {
            assert!(
                err.contains("nf-destinations/preset-set"),
                "{extra:?} must name the task that settles the destination set: {err}"
            );
        }
    }
}

#[test]
fn a_print_or_output_recipe_section_is_refused_whole() {
    // The provenance no flag row can see, and the reason none of these knobs needs a
    // value rule: between the rows above and this, both spellings are covered. The
    // new chain's recipe has neither section, and names each one so a user fixing a
    // recipe knows which key to remove and where its knobs went.
    let tmp = TempDir::new("new-flow-sections");
    for (name, body, went) in [
        (
            "print",
            r#"{ "recipe_version": 2, "print": { "print_exposure": 1.0 } }"#,
            "scene_correction",
        ),
        (
            "output",
            r#"{ "recipe_version": 2, "output": { "preset": "display-p3" } }"#,
            "nf-destinations/preset-set",
        ),
    ] {
        let recipe = write_file(&tmp.path(&format!("{name}.json")), body);
        let out = tmp.path(&format!("{name}.tif"));
        let input = fixture("hdr-48bit.tif");
        let argv = vec![
            "convert",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--params",
            recipe.to_str().unwrap(),
            "--new-flow",
            "--report",
            "none",
        ];
        let (code, _out, err) = run(&argv);
        assert_eq!(code, 2, "a recipe `{name}` section must be refused: {err}");
        assert!(err.contains(&format!("`{name}` is a section")), "{err}");
        assert!(err.contains(went), "{err}");

        // Falsifiability: the same section is fine on the current chain, in a recipe
        // without the version. It needs a preset that writes TIFF, since the `.tif`
        // path is judged there — which is the very rule
        // `the_suffix_rule_stands_down_under_the_new_flow` covers.
        let mut current: serde_json::Value = serde_json::from_str(body).unwrap();
        current.as_object_mut().unwrap().remove("recipe_version");
        let current_recipe = write_file(
            &tmp.path(&format!("{name}-current.json")),
            &current.to_string(),
        );
        let mut legacy = argv.clone();
        legacy.retain(|a| *a != "--new-flow");
        let at = legacy.iter().position(|a| *a == "--params").unwrap() + 1;
        legacy[at] = current_recipe.to_str().unwrap();
        legacy.extend_from_slice(&["--output-preset", "display-p3"]);
        let (code, _out, err) = run(&legacy);
        assert_eq!(code, 0, "the same section must still convert: {err}");
    }
}

#[test]
fn the_new_flow_judges_the_suffix_against_its_own_destination() {
    // Under `--new-flow` the output preset is refused, so the suffix rule is judged
    // against the new flow's one destination — a TIFF — never against the default
    // preset nobody selected. A stated TIFF suffix is kept, an absent one completed to
    // `.tiff`, and anything else refused with a remedy that does not name
    // `--output-preset` (a flag this flow rejects).
    let tmp = TempDir::new("new-flow-suffix");
    let run_to = |out: &Path| {
        run(&[
            "convert",
            fixture("hdr-48bit.tif").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            "--report",
            "none",
        ])
    };
    let (code, _o, err) = run_to(&tmp.path("kept.tif"));
    assert_eq!(code, 0, "{err}");
    assert!(
        tmp.path("kept.tif").exists(),
        "a stated suffix is kept verbatim"
    );

    let (code, _o, err) = run_to(&tmp.path("stem"));
    assert_eq!(code, 0, "{err}");
    assert!(
        tmp.path("stem.tiff").exists(),
        "an absent suffix is completed"
    );

    let (code, _o, err) = run_to(&tmp.path("wrong.jpg"));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("does not end in .tif or .tiff"), "{err}");
    assert!(err.contains("--new-flow"), "{err}");
    assert!(
        !err.contains("--output-preset"),
        "the remedy must not name a flag this flow refuses: {err}"
    );
    assert!(!tmp.path("wrong.jpg").exists());
}

#[test]
fn the_anchor_guard_recommends_only_a_slope() {
    // `nf-core/knob-availability-audit`, finding #2. The remedy used to end "or
    // --anchor-white-at-reference, which needs no such division" — a placement the
    // rule never checked was available, and one `--new-flow` refuses. Every flag in
    // this command line is one the new flow accepts, which is what made it reachable.
    //
    // The losing wording is asserted **absent**, not merely the new one present:
    // both sentences name the same flag (the explanation still does, as a fact about
    // the arithmetic), so a `contains` on the flag alone cannot tell them apart.
    let tmp = TempDir::new("anchor-guard-remedy");
    let base: Vec<String> = vec![
        "convert".into(),
        fixture("hdr-48bit.tif").display().to_string(),
        "-o".into(),
        // A bare stem, completed on both flows, so the suffix rule — which runs
        // before this one and differs by flow — cannot be what answers.
        tmp.path("out").display().to_string(),
        "--film-base".into(),
        "0.9,0.55,0.42".into(),
        "--density-gamma".into(),
        "2e-39".into(),
        "--anchor-mid-offset".into(),
        "0.62".into(),
        "--report".into(),
        "none".into(),
    ];
    for flow in [vec![], vec!["--new-flow".to_string()]] {
        let mut argv = base.clone();
        let under_new_flow = !flow.is_empty();
        argv.extend(flow);
        let (code, _out, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(code, 2, "{err}");
        // Each chain has its own guard — the new one checks the decode's recipe
        // (`crate::recipe::validate`) — and both remedies name only the slope and the
        // offset, which is what this command line can change.
        if under_new_flow {
            assert!(err.contains("the decode's anchor is not usable"), "{err}");
            assert!(err.contains("Use a larger --density-gamma"), "{err}");
        } else {
            assert!(err.contains("Use a photographic slope"), "{err}");
        }
        assert!(
            !err.contains("which needs no such division"),
            "the remedy must not recommend a placement it never checked ({}): {err}",
            if under_new_flow { "new flow" } else { "legacy" }
        );
    }
}

#[test]
fn new_flow_writes_the_ir_export() {
    // The export is staged after the render at the destination's depth; the new flow's
    // destination is 16-bit, so the plane is written as u16 — the same samples the
    // legacy `display-p3` preset writes, since both read the decoded image.
    let tmp = TempDir::new("new-flow-export-ir");
    let export = |ir: &Path, flow: &[&str]| {
        let mut v: Vec<String> = vec![
            "convert".into(),
            fixture("hdri-64bit.tif").display().to_string(),
            "-o".into(),
            tmp.path(&format!(
                "{}.tif",
                ir.file_stem().unwrap().to_str().unwrap()
            ))
            .display()
            .to_string(),
            "--film-base".into(),
            "0.9,0.55,0.42".into(),
            "--export-ir".into(),
            ir.display().to_string(),
            "--report".into(),
            "none".into(),
        ];
        v.extend(flow.iter().map(|s| (*s).to_string()));
        let (code, _out, err) = run(&v.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(code, 0, "{flow:?}: {err}");
        read_gray_tiff(ir)
    };
    let (bits, format, new) = export(&tmp.path("new-ir.tiff"), &["--new-flow"]);
    assert_eq!((bits, format), (16, 1), "u16 at the destination's depth");
    let (_, _, legacy) = export(
        &tmp.path("legacy-ir.tiff"),
        &["--output-preset", "display-p3"],
    );
    match (new, legacy) {
        (GraySamples::U16(a), GraySamples::U16(b)) => assert_eq!(a, b),
        other => panic!("both exports must be u16: {other:?}"),
    }
}

#[test]
fn the_new_flow_writes_no_sidecar_and_guards_no_phantom_one() {
    // No sidecar is written under `--new-flow` (its `params` would describe a chain the
    // run did not select), so none is a write target either: `-o out --report-file
    // out.tiff.json` must not be refused for colliding with a file that is never
    // written.
    let tmp = TempDir::new("new-flow-sidecar");
    let stem = tmp.path("out");
    let report = tmp.path("out.tiff.json");
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        stem.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--report-file",
        report.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("collides with the sidecar"), "{err}");
    assert!(json(&std::fs::read_to_string(&report).unwrap())["new_flow"].is_object());

    // Falsifiability: the same pair *is* a collision on the current chain, which does
    // write `out.tiff.json` as its sidecar.
    let (code, _out, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        stem.to_str().unwrap(),
        "--output-preset",
        "display-p3",
        "--film-base",
        "0.9,0.55,0.42",
        "--report-file",
        report.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{err}");

    // And the guard's *real* checks still run under the flag, so a report file aimed
    // at the input scan is still refused.
    let victim = tmp.path("victim.tif");
    std::fs::copy(fixture("hdr-48bit.tif"), &victim).unwrap();
    let before = std::fs::metadata(&victim).unwrap().len();
    let (code, _out, err) = run(&[
        "convert",
        victim.to_str().unwrap(),
        "-o",
        tmp.path("o.tif").to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--new-flow",
        "--report-file",
        victim.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("overwrite the input scan"), "{err}");
    assert_eq!(
        std::fs::metadata(&victim).unwrap().len(),
        before,
        "the input must be untouched"
    );
}

#[test]
fn the_new_flow_removes_a_stale_sidecar_and_nothing_else() {
    // A legacy run leaves `out.tiff.json` beside `out.tiff`; a `--new-flow` run then
    // replaces the image and writes no sidecar of its own, so the old one would sit
    // there describing a picture that no longer exists. It is removed, and the report
    // says so — but only a file that *is* one of nc's `{meta, params}` sidecars.
    let tmp = TempDir::new("new-flow-stale-sidecar");
    let out = tmp.path("out.tiff");
    let convert = |flow: &[&str]| {
        let mut argv = vec![
            "convert".to_string(),
            fixture("hdr-48bit.tif").display().to_string(),
            "-o".into(),
            out.display().to_string(),
            "--film-base".into(),
            "0.9,0.55,0.42".into(),
        ];
        argv.extend(flow.iter().map(|s| (*s).to_string()));
        let (code, stdout, err) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(code, 0, "{flow:?}: {err}");
        json(&stdout)
    };
    convert(&["--output-preset", "display-p3"]);
    assert!(sidecar_of(&out).exists(), "the legacy run writes a sidecar");

    let report = convert(&["--new-flow"]);
    assert!(
        !sidecar_of(&out).exists(),
        "the stale sidecar must be removed"
    );
    assert_eq!(
        report["new_flow"]["removed_sidecar"],
        sidecar_of(&out).to_str().unwrap(),
        "and the removal is reported"
    );

    // A file sharing the name that is not an nc sidecar is not ours to remove —
    // including one that merely has the envelope's two key names.
    for body in [r#"{"notes": "mine"}"#, r#"{"meta": null, "params": null}"#] {
        std::fs::write(sidecar_of(&out), body).unwrap();
        let report = convert(&["--new-flow"]);
        assert!(
            sidecar_of(&out).exists(),
            "a user's own file must survive: {body}"
        );
        assert!(report["new_flow"].get("removed_sidecar").is_none());
    }
}

#[test]
fn the_new_flow_never_removes_the_recipe_it_read() {
    // Found in review: an enveloped recipe carries a sidecar's identity fields and can
    // sit exactly where the stale sidecar would — here, a legacy sidecar whose `params`
    // were rewritten to a new-chain recipe, which `--params` loads under the flag.
    // Reading it and then deleting it as "stale" would destroy the run's own input. It
    // stays, and the run says why.
    let tmp = TempDir::new("new-flow-recipe-is-the-sidecar");
    let out = tmp.path("out.tiff");
    let (code, _o, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
        "--output-preset",
        "display-p3",
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
    let sidecar = sidecar_of(&out);
    let mut doc = json(&std::fs::read_to_string(&sidecar).unwrap());
    doc["params"] = serde_json::json!({
        "recipe_version": 2,
        "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } }
    });
    std::fs::write(&sidecar, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    let (code, stdout, err) = run(&[
        "convert",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--params",
        sidecar.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(sidecar.exists(), "the run's own recipe must survive");
    let report = json(&stdout);
    assert!(report["new_flow"].get("removed_sidecar").is_none());
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("because this run read it")),
        "{stdout}"
    );
}

#[test]
fn roll_judges_a_manifest_suffix_against_the_new_flow_destination() {
    // A roll manifest's explicit output goes through the same rule `convert` uses, so
    // under `--new-flow` it is judged against the new flow's TIFF destination — and
    // the refusal names the frame.
    let tmp = TempDir::new("new-flow-roll-suffix");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{ "recipe_version": 2,
             "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let roll_to = |output: &str, dir: &str| {
        let manifest = write_file(
            &tmp.path(&format!("{dir}.json")),
            &format!(
                r#"{{ "frames": [ {{ "input": {:?}, "output": {output:?} }} ] }}"#,
                fixture("hdr-48bit.tif").display().to_string()
            ),
        );
        run(&[
            "roll",
            "--frames",
            manifest.to_str().unwrap(),
            "--out-dir",
            tmp.path(dir).to_str().unwrap(),
            "--params",
            recipe.to_str().unwrap(),
            "--new-flow",
            "--report",
            "none",
        ])
    };
    let (code, _out, err) = roll_to("one.tif", "ok");
    assert_eq!(code, 0, "{err}");
    assert!(tmp.path("ok").join("one.tif").exists());

    let (code, _out, err) = roll_to("one.jpg", "bad");
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("frame "), "the refusal names the frame: {err}");
    assert!(err.contains("does not end in .tif or .tiff"), "{err}");
    assert!(!err.contains("does not match output preset"), "{err}");
}

#[test]
fn roll_under_the_new_flow_renders_every_frame() {
    // `roll --new-flow` runs every frame through the same frame function `convert`
    // uses: derived names take the destination's `.tiff`, no sidecar is written, and
    // neither the roll report nor any frame claims the legacy recipe describes it.
    let tmp = TempDir::new("new-flow-roll");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{
  "recipe_version": 2,
  "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } },
  "measure": { "inset": 0.05 }
}"#,
    );
    let out_dir = tmp.path("out");
    let (code, stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        fixture("hdri-64bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    for stem in ["hdr-48bit", "hdri-64bit"] {
        let out = out_dir.join(format!("{stem}_positive.tiff"));
        assert!(is_tiff(&out), "{}", out.display());
        assert!(!sidecar_of(&out).exists(), "no sidecar under --new-flow");
    }
    let report = json(&stdout);
    assert!(report.get("recipe").is_none(), "{stdout}");
    assert!(report["identity"].get("params_hash").is_none(), "{stdout}");
    for frame in report["frames"].as_array().unwrap() {
        assert_eq!(frame["status"], "ok", "{frame}");
        assert_eq!(frame["new_flow"]["destination"], "display-p3-u16-tiff");
    }
}

#[test]
fn a_roll_frame_override_reaches_scene_correction() {
    // Exposure is per frame by nature (a bracket, a frame shot a stop over), so a
    // per-frame `scene_correction` overlay must reach that frame and only that one.
    let tmp = TempDir::new("new-flow-roll-scene");
    let shared = write_file(
        &tmp.path("roll.json"),
        r#"{ "recipe_version": 2,
             "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let (a, b) = (fixture("hdr-48bit.tif"), fixture("hdri-64bit.tif"));
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [
                 {{ "input": {a:?}, "params": {{ "scene_correction": {{ "exposure": -1.5 }} }} }},
                 {{ "input": {b:?} }} ] }}"#
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
        shared.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    let exposures: Vec<f64> = report["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            f["new_flow"]["scene_correction"]["exposure"]
                .as_f64()
                .unwrap()
        })
        .collect();
    assert_eq!(exposures, [-1.5, 0.0], "{stdout}");

    // A value the stage refuses is refused per frame, naming the key — roll takes no
    // conversion flags, so a flag spelling would be a remedy the user cannot type.
    let bad = write_file(
        &tmp.path("bad.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {a:?},
                 "params": {{ "scene_correction": {{ "white_balance": {{ "explicit": [1, 0, 1] }} }} }} }} ] }}"#
        ),
    );
    let (code, _, err) = run(&[
        "roll",
        "--frames",
        bad.to_str().unwrap(),
        "--out-dir",
        tmp.path("bad-out").to_str().unwrap(),
        "--params",
        shared.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_ne!(code, 0, "{err}");
    assert!(
        err.contains("`scene_correction.white_balance`") && !err.contains("--white-balance"),
        "{err}"
    );
}

#[test]
fn roll_under_the_new_flow_refuses_an_unread_section_in_a_frame_override() {
    // A per-frame overlay can state a section the new flow never reads; now that the
    // roll renders, accepting it would be accepted-and-ignored. Refused, naming the
    // frame, before anything is written.
    let tmp = TempDir::new("new-flow-roll-overlay");
    let recipe = write_file(
        &tmp.path("roll.json"),
        r#"{ "recipe_version": 2,
             "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let manifest = write_file(
        &tmp.path("frames.json"),
        &format!(
            r#"{{ "frames": [ {{ "input": {:?},
                    "params": {{ "print": {{ "print_exposure": 0.5 }} }} }} ] }}"#,
            fixture("hdr-48bit.tif").display().to_string()
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
        "--new-flow",
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("frame "), "{err}");
    assert!(err.contains("`print` is a section"), "{err}");
    assert!(!out_dir.exists(), "refused before anything is created");
}

#[test]
fn roll_under_the_new_flow_refuses_the_current_chains_recipe() {
    // `roll` accepts no conversion flags, so its shared recipe is the *only* way it
    // can state a reconstruction — and therefore the only place the
    // accepted-and-ignored hole could open for it. A recipe without the document
    // version is the current chain's, refused at exit 2 before anything is created.
    let tmp = TempDir::new("new-flow-roll-recipe");
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let out_dir = tmp.path("out");
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--new-flow",
        "--report",
        "none",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("\"recipe_version\": 2"), "{err}");
    assert!(!out_dir.exists(), "refused before anything is created");

    // Falsifiability: the same recipe converts without the flag.
    let (code, _stdout, err) = run(&[
        "roll",
        fixture("hdr-48bit.tif").to_str().unwrap(),
        "--out-dir",
        tmp.path("legacy-out").to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
        "--report",
        "none",
    ]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn roll_refuses_the_current_chains_keys_from_either_recipe_site() {
    // Roll accepts no conversion flags, so what it can reach arrives in a recipe —
    // and it reads recipes in **two** places. Both are pinned, because a check composed
    // at one site and forgotten at the other is exactly how `OutputPreset::is_atomic`'s
    // three call sites lost one. Before `nf-core/recipe-schema` the per-frame half was
    // a hole: an overlay was merged onto the shared config with no section check.
    let tmp = TempDir::new("new-flow-roll-knob");
    let out_dir = tmp.path("out");
    let roll = |manifest_or_input: &[&str], shared: &Path, out: &Path, flow: &[&str]| {
        let mut v: Vec<String> = vec!["roll".into()];
        v.extend(manifest_or_input.iter().map(|s| (*s).to_string()));
        v.extend([
            "--out-dir".to_string(),
            out.display().to_string(),
            "--params".into(),
            shared.display().to_string(),
            "--report".into(),
            "none".into(),
        ]);
        v.extend(flow.iter().map(|s| (*s).to_string()));
        let borrowed: Vec<&str> = v.iter().map(String::as_str).collect();
        let (code, _out, err) = run(&borrowed);
        (code, err)
    };
    let input = fixture("hdr-48bit.tif").display().to_string();

    // (1) the shared recipe.
    let shared_typed = write_file(
        &tmp.path("shared.json"),
        r#"{
             "recipe_version": 2,
             "reconstruction": { "type": "density" },
             "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } }
           }"#,
    );
    let (code, err) = roll(&[&input], &shared_typed, &out_dir, &["--new-flow"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`reconstruction.type`"), "{err}");

    // (2) a per-frame override, on a shared recipe the gate accepts.
    let shared = write_file(
        &tmp.path("roll-new.json"),
        r#"{ "recipe_version": 2,
             "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let manifest_with = |name: &str, params: &str| {
        write_file(
            &tmp.path(name),
            &format!(r#"{{ "frames": [ {{ "input": {input:?}, "params": {params} }} ] }}"#),
        )
    };
    // The current chain's retired selector, at the value every earlier sidecar wrote.
    let typed = manifest_with(
        "typed.json",
        r#"{ "reconstruction": { "type": "density" } }"#,
    );
    let (code, err) = roll(
        &["--frames", typed.to_str().unwrap()],
        &shared,
        &tmp.path("out2"),
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("per-frame `params` override"), "{err}");
    assert!(err.contains("`reconstruction.type`"), "{err}");
    let print = manifest_with("print.json", r#"{ "print": { "print_exposure": 0.5 } }"#);
    let (code, err) = roll(
        &["--frames", print.to_str().unwrap()],
        &shared,
        &tmp.path("out2"),
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`print` is a section"), "{err}");

    // The overlay lands on the new chain's document: a decode key it states is read
    // and checked there, not refused as unknown…
    let bad = manifest_with(
        "bad.json",
        r#"{ "reconstruction": { "linearization": -1 } }"#,
    );
    let (code, err) = roll(
        &["--frames", bad.to_str().unwrap()],
        &shared,
        &tmp.path("out2"),
        &["--new-flow"],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("`reconstruction.linearization`"), "{err}");
    // It names the frame, and only the recipe key: `roll` accepts no `--density-gamma`.
    assert!(err.contains("per-frame `params` override"), "{err}");
    assert!(!err.contains("--density-gamma"), "{err}");
    // …and a valid one resolves and reaches the frame's render — the frame's own
    // recipe, not the shared one, which is what the decode reads.
    let good = manifest_with(
        "good.json",
        r#"{ "reconstruction": { "linearization": 1.7 } }"#,
    );
    let (code, err) = roll(
        &["--frames", good.to_str().unwrap()],
        &shared,
        &tmp.path("out2"),
        &["--new-flow"],
    );
    assert_eq!(code, 0, "{err}");
    let (code, stdout, err) = run(&[
        "roll",
        "--frames",
        good.to_str().unwrap(),
        "--out-dir",
        tmp.path("out5").to_str().unwrap(),
        "--params",
        shared.to_str().unwrap(),
        "--new-flow",
    ]);
    assert_eq!(code, 0, "{err}");
    let frame = &json(&stdout)["frames"][0];
    let linearization = frame["new_flow"]["decode"]["linearization"]
        .as_f64()
        .unwrap();
    assert!((linearization - 1.7).abs() < 1e-5, "{frame}");

    // The reverse at the override site: without the flag, an override that states the
    // version is refused by name rather than as an unknown field. (An unversioned one
    // cannot be told apart from a mistyped current-chain override, so serde's
    // unknown-field error is the honest answer there.)
    let recipe = write_file(&tmp.path("roll.json"), ROLL_RECIPE);
    let versioned = manifest_with(
        "versioned.json",
        r#"{ "recipe_version": 2, "reconstruction": { "linearization": 1.8 } }"#,
    );
    let (code, err) = roll(
        &["--frames", versioned.to_str().unwrap()],
        &recipe,
        &tmp.path("out4"),
        &[],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("only `--new-flow` reads"), "{err}");

    // Falsifiability: the same overlay runs on the current chain, where the old value
    // is accepted as asking for nothing.
    let (code, err) = roll(
        &["--frames", typed.to_str().unwrap()],
        &recipe,
        &tmp.path("out3"),
        &[],
    );
    assert_eq!(code, 0, "the control roll must succeed: {err}");
}

#[test]
fn help_documents_the_flag_as_transitional() {
    // The expiry is part of the design, so it has to reach the user who meets the
    // flag in `--help` rather than living only in the migration doc.
    for command in ["convert", "roll"] {
        let (code, stdout, _err) = run(&[command, "--help"]);
        assert_eq!(code, 0);
        assert!(stdout.contains("--new-flow"), "{command}: {stdout}");
        assert!(
            stdout.contains("Transitional"),
            "{command} must say the flag is transitional: {stdout}"
        );
    }
}

// ---------------------------------------------------------------------------
// measure-roll (`nf-scene-correction/roll-white-balance`)
// ---------------------------------------------------------------------------

/// A new-chain recipe for synthetic scans: they carry no SilverFast metadata, so the
/// input's transfer and meaning are stated, as `convert` would take them by flag.
fn roll_white_recipe(dir: &TempDir, base: &str) -> PathBuf {
    write_file(
        &dir.path("roll.json"),
        &format!(
            r#"{{ "recipe_version": 2,
                 "input": {{ "transfer": "linear", "meaning": "scanner-device" }},
                 "calibration": {{ "film_base": {{ "explicit": [{base}] }} }} }}"#
        ),
    )
}

#[test]
fn measure_roll_gains_reach_convert_unchanged_by_flag_and_by_recipe() {
    // The contract the command exists for: measure once, state the gains, and every
    // frame renders under exactly them — by the reported flag and by the reported
    // recipe fragment alike.
    let tmp = TempDir::new("measure-roll-reuse");
    let frame = fixture("hdr-48bit.tif").display().to_string();
    let second = tmp.path("second.tif");
    std::fs::copy(&frame, &second).unwrap();
    let (code, stdout, err) = run(&[
        "measure-roll",
        &frame,
        second.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ]);
    assert_eq!(code, 0, "{err}");
    let report = json(&stdout);
    assert_eq!(report["command"], "measure-roll");
    let gains = report["white_balance"]["gains"].as_array().unwrap().clone();
    assert_eq!(gains[1], 1.0, "green-anchored: {report}");
    assert_ne!(
        report["white_balance"]["gains"],
        serde_json::json!([1.0, 1.0, 1.0]),
        "not vacuous: the fixture's roll white is off neutral"
    );
    assert_eq!(report["frames"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["frames"][0]["sampled"], report["frames"][0]["kept"],
        "nothing guarded without a leader: {report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("no --leader")),
        "an unguarded run says so: {report}"
    );
    // Measured at the decode's output, which runs at the linearization alone.
    assert!(
        (report["decode"]["linearization"].as_f64().unwrap() - 1.8).abs() < 1e-6,
        "{report}"
    );

    let by_flag = tmp.path("flag.tiff");
    let flag = report["reuse"]["flag"].as_str().unwrap();
    let flag_gains = flag.strip_prefix("--white-balance ").unwrap();
    let (code, stdout, err) = run(&[
        "convert",
        &frame,
        "--new-flow",
        "--film-base",
        "0.9,0.55,0.42",
        "--white-balance",
        flag_gains,
        "-o",
        by_flag.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        json(&stdout)["new_flow"]["scene_correction"]["white_balance"],
        report["white_balance"]["gains"],
        "the flag's text round-trips the gains exactly"
    );

    let mut recipe = report["reuse"]["recipe"].clone();
    recipe["recipe_version"] = 2.into();
    let recipe = write_file(&tmp.path("wb.json"), &recipe.to_string());
    let by_recipe = tmp.path("recipe.tiff");
    let (code, _, err) = run(&[
        "convert",
        &frame,
        "--new-flow",
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        recipe.to_str().unwrap(),
        "-o",
        by_recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        std::fs::read(&by_flag).unwrap(),
        std::fs::read(&by_recipe).unwrap(),
        "the reported flag and recipe fragment are one knob"
    );
}

#[test]
fn measure_roll_keeps_a_fully_exposed_frame_out_of_the_white() {
    // A roll of the picture fixture plus one fully exposed frame — a copy of the
    // leader, mixed in (the leader itself as an input is refused below). Guarded by
    // `--leader`, the gains are the picture's alone; unguarded, the exposed frame
    // becomes the roll's white.
    let tmp = TempDir::new("measure-roll-guard");
    let recipe = roll_white_recipe(&tmp, "0.9,0.55,0.42");
    let leader = tmp.path("leader.tif");
    // Dense, cast, and IR-transparent (so no holder is measured).
    write_hdri_with_uniform_ir(&leader, 64, 64, [900, 700, 400], 40_000);
    let blown = tmp.path("blown.tif");
    std::fs::copy(&leader, &blown).unwrap();
    let (frame, other) = (tmp.path("frame.tif"), tmp.path("other.tif"));
    std::fs::copy(fixture("hdr-48bit.tif"), &frame).unwrap();
    std::fs::copy(fixture("hdr-48bit.tif"), &other).unwrap();
    let (frame, other, leader, blown) = (
        frame.to_str().unwrap(),
        other.to_str().unwrap(),
        leader.to_str().unwrap(),
        blown.to_str().unwrap(),
    );
    let params = recipe.to_str().unwrap();
    let gains = |args: &[&str]| {
        let (code, stdout, err) = run(&[&["measure-roll", "--params", params][..], args].concat());
        assert_eq!(code, 0, "{args:?}: {err}");
        json(&stdout)
    };

    let clean = gains(&[frame, other, "--leader", leader]);
    let guarded = gains(&[frame, other, blown, "--leader", leader]);
    let unguarded = gains(&[frame, other, blown]);
    assert_eq!(
        guarded["white_balance"]["gains"], clean["white_balance"]["gains"],
        "{guarded}"
    );
    assert_eq!(guarded["frames"][2]["kept"], 0, "{guarded}");
    assert!(
        guarded["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("contributed no pixel")),
        "{guarded}"
    );
    assert_ne!(
        unguarded["white_balance"]["gains"], clean["white_balance"]["gains"],
        "the guard must be what keeps it out: {unguarded}"
    );
    let ceiling = &clean["leader"]["ceiling"];
    assert!(
        ceiling.is_array() && clean["leader"]["guard_density"] == 0.1,
        "{clean}"
    );

    // The leader itself among the frames is refused: its unguarded edges would pool.
    let (code, _, err) = run(&[
        "measure-roll",
        "--params",
        params,
        frame,
        leader,
        "--leader",
        leader,
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("is both the --leader and an input frame"),
        "{err}"
    );
}

#[test]
fn measure_roll_refuses_what_it_cannot_measure_under() {
    let tmp = TempDir::new("measure-roll-refusals");
    let frame = fixture("hdr-48bit.tif").display().to_string();

    // No stated base: a per-frame estimate would decode each frame differently.
    let (code, stdout, err) = run(&["measure-roll", &frame]);
    assert_eq!(code, 2, "{err}");
    assert!(stdout.is_empty());
    assert!(
        err.contains("film base stated explicitly") && err.contains("estimate --grid"),
        "{err}"
    );

    // A current-chain recipe is not the new chain's.
    let legacy = write_file(
        &tmp.path("legacy.json"),
        r#"{ "calibration": { "film_base": { "explicit": [0.9, 0.55, 0.42] } } }"#,
    );
    let (code, _, err) = run(&["measure-roll", &frame, "--params", legacy.to_str().unwrap()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("recipe_version"), "{err}");

    // Refused before any decode, so the whole roll is not read for an answer known
    // up front: `--strict` without a leader, a frame named twice, a bad inset — the
    // last blamed on the flag, not on the first frame that would have measured it.
    let base = ["--film-base", "0.9,0.55,0.42"];
    for (extra, expect) in [
        (
            vec![frame.as_str(), "--strict"],
            "--strict refuses an unguarded measurement",
        ),
        (vec![frame.as_str(), frame.as_str()], "is named twice"),
        (
            vec![frame.as_str(), "--measure-inset", "0.6"],
            "beyond the supported maximum",
        ),
    ] {
        let (code, stdout, err) = run(&[&["measure-roll"][..], &base, &extra].concat());
        assert_eq!(code, 2, "{extra:?}: {err}");
        assert!(stdout.is_empty(), "{extra:?}");
        assert!(err.contains(expect), "{extra:?}: {err}");
        assert!(
            !err.contains("decoded"),
            "{extra:?} must refuse before decoding: {err}"
        );
    }
    let (_, _, err) = run(&[
        &["measure-roll"][..],
        &base,
        &[&frame, "--measure-inset", "0.6"],
    ]
    .concat());
    assert!(
        !err.contains("hdr-48bit.tif:"),
        "the flag is at fault, not a frame: {err}"
    );

    // The recipe's scene correction is what this command measures, and its look is
    // never applied, so neither is read: a value `convert` would refuse there does not
    // refuse the measurement.
    for (name, section) in [
        ("scene.json", r#""scene_correction": { "exposure": 500 }"#),
        (
            "look.json",
            r#""look": { "highlight_desaturation": { "strength": 1.5 } }"#,
        ),
    ] {
        let recipe = write_file(
            &tmp.path(name),
            &format!(
                r#"{{ "recipe_version": 2,
                     "calibration": {{ "film_base": {{ "explicit": [0.9, 0.55, 0.42] }} }},
                     {section} }}"#
            ),
        );
        let (code, _, err) = run(&["measure-roll", &frame, "--params", recipe.to_str().unwrap()]);
        assert_eq!(code, 0, "{name}: {err}");
    }

    // A retired per-frame mode is still refused at load, and the remedy works from
    // here too: drop it, then state the gains this command reports.
    let retired = write_file(
        &tmp.path("retired.json"),
        r#"{ "recipe_version": 2, "scene_correction": { "white_balance": "percentile" } }"#,
    );
    let (code, _, err) = run(&[
        "measure-roll",
        &frame,
        "--film-base",
        "0.9,0.55,0.42",
        "--params",
        retired.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("Drop it, then state the gains"), "{err}");
}

#[test]
fn measure_roll_warns_when_a_frames_region_is_not_a_measurement() {
    // The capped-march frame (`a_capped_holder_march_warns_and_strict_promotes_it`):
    // its region keeps holder strips, which the pool would take as picture. The
    // warning `convert` gives must reach this report too, so `--strict` can see it.
    let dir = TempDir::new("measure-roll-capped");
    let path = dir.path("deep.tif");
    const W: u32 = 400;
    const H: u32 = 400;
    let mut rgb = vec![0u16; (W * H * 3) as usize];
    let mut ir = vec![41_000u16; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            let holder = y < 120 || !(10..W - 10).contains(&x) || y >= H - 10;
            rgb[i..i + 3].copy_from_slice(&if holder {
                [655, 655, 655]
            } else {
                [12000, 7000, 4000]
            });
            if holder {
                ir[(y * W + x) as usize] = 1_300;
            }
        }
    }
    write_hdri(&path, W, H, &rgb, &ir);
    let recipe = roll_white_recipe(&dir, "0.9,0.6,0.5");
    let (code, stdout, err) = run(&[
        "measure-roll",
        path.to_str().unwrap(),
        "--params",
        recipe.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let warnings = json(&stdout)["warnings"].clone();
    assert!(
        warnings
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("deep.tif: ")
                && w.as_str().unwrap().contains("cap")),
        "{warnings}"
    );
}

// ---------------------------------------------------------------------------
// The look: highlight desaturation (`nf-look/path-to-white`)
// ---------------------------------------------------------------------------

#[test]
fn highlight_desaturation_reaches_the_pixels_by_flag_and_by_recipe() {
    let tmp = TempDir::new("look-desat");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let convert = |name: &str, extra: &[&str]| {
        let out = tmp.path(name);
        let mut argv = vec![
            "convert",
            input.as_str(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            // Push the fixture's highlights past diffuse white, where the operator acts.
            "--exposure",
            "2",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{extra:?}: {err}");
        (std::fs::read(&out).unwrap(), json(&stdout))
    };
    let look = |r: &serde_json::Value| r["new_flow"]["stages"][1].clone();

    // On by default at 0.8, after the look's default contrast, and the report says so.
    let (plain, report) = convert("plain.tiff", &[]);
    assert_eq!(
        look(&report)["applied"],
        "contrast+highlight-desaturation",
        "{report}"
    );
    assert_eq!(
        report["new_flow"]["look"]["highlight_desaturation"],
        serde_json::json!({"strength": 0.8, "start_stops": -1.0, "band": [0.015, 0.025]})
    );
    // Strength 0 is off: the look then runs its contrast alone, different from the
    // default, and with the other two knobs inert — a moved band or start changes
    // nothing when off.
    let (off, report) = convert("off.tiff", &["--highlight-desaturation", "0"]);
    assert_eq!(look(&report)["applied"], "contrast", "{report}");
    assert_ne!(plain, off, "the default must move the fixture's highlights");
    let (off_moved, _) = convert(
        "off-moved.tiff",
        &[
            "--highlight-desaturation",
            "0",
            "--highlight-desaturation-start",
            "-3",
            "--highlight-desaturation-band",
            "0.001,0.3",
        ],
    );
    assert_eq!(off, off_moved, "off is off whatever the band and start");

    // A stronger pull moves further.
    let (on, report) = convert("on.tiff", &["--highlight-desaturation", "1"]);
    assert_eq!(look(&report)["stage"], "look");
    assert_ne!(plain, on, "strength must change the pixels");

    // The recipe key is the same knob, and a dump writes it back.
    let recipe = write_file(
        &tmp.path("look.json"),
        r#"{ "recipe_version": 2,
             "look": { "highlight_desaturation": { "strength": 1 } } }"#,
    );
    let dump = tmp.path("dump.json");
    let (from_recipe, _) = convert(
        "recipe.tiff",
        &[
            "--params",
            recipe.to_str().unwrap(),
            "--dump-params",
            dump.to_str().unwrap(),
        ],
    );
    assert_eq!(on, from_recipe, "the recipe key and the flag are one knob");
    let dumped: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).unwrap()).unwrap();
    assert_eq!(dumped["look"]["highlight_desaturation"]["strength"], 1.0);

    // A flag wins over the recipe, down to the identity: the recipe's pull is live
    // (so the comparison can fail), and the flag resets it to off.
    assert_ne!(
        from_recipe, off,
        "the recipe's strength must move the pixels"
    );
    let (reset, report) = convert(
        "reset.tiff",
        &[
            "--params",
            recipe.to_str().unwrap(),
            "--highlight-desaturation",
            "0",
        ],
    );
    assert_eq!(
        report["new_flow"]["look"]["highlight_desaturation"]["strength"], 0.0,
        "{report}"
    );
    assert_eq!(reset, off, "the flag's 0 must win over the recipe's 1");

    // A narrower band moves fewer pixels: the band is live.
    let (narrow, _) = convert(
        "narrow.tiff",
        &[
            "--highlight-desaturation",
            "1",
            "--highlight-desaturation-band",
            "0.001,0.002",
        ],
    );
    assert_ne!(narrow, on, "the band must change which pixels are pulled");
}

/// The look's print contrast (`nf-reconstruction/gamma-split`): reachable by flag and
/// by recipe, one knob, the flag winning; `1` is the identity, reported as such; and
/// the decode it split from is untouched by it.
#[test]
fn the_look_contrast_reaches_the_pixels_by_flag_and_by_recipe() {
    let tmp = TempDir::new("look-contrast");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let convert = |name: &str, extra: &[&str]| {
        let out = tmp.path(name);
        let mut argv = vec![
            "convert",
            input.as_str(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            // Off, so the look's `applied` reads the contrast alone.
            "--highlight-desaturation",
            "0",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{extra:?}: {err}");
        (std::fs::read(&out).unwrap(), json(&stdout))
    };
    let applied = |r: &serde_json::Value| r["new_flow"]["stages"][1]["applied"].clone();

    let (default, report) = convert("default.tiff", &[]);
    assert_eq!(applied(&report), "contrast", "{report}");
    let reported = report["new_flow"]["look"]["contrast"].as_f64().unwrap();
    let default_decode = report["new_flow"]["decode"].clone();
    assert!((reported - 2.0 / 1.8).abs() < 1e-6, "{report}");

    let (unity, report) = convert("unity.tiff", &["--contrast", "1"]);
    assert_eq!(applied(&report), "identity", "{report}");
    assert_ne!(unity, default, "the default contrast must move the pixels");

    let (steep, report) = convert("steep.tiff", &["--contrast", "1.5"]);
    assert_ne!(steep, default);
    // The decode block is the same at every look contrast.
    assert_eq!(
        report["new_flow"]["decode"], default_decode,
        "the look contrast reached the decode"
    );

    // The recipe key is the same knob, and a flag wins over it.
    let recipe = write_file(
        &tmp.path("look.json"),
        r#"{ "recipe_version": 2, "look": { "contrast": 1.5 } }"#,
    );
    let (from_recipe, _) = convert("recipe.tiff", &["--params", recipe.to_str().unwrap()]);
    assert_eq!(
        steep, from_recipe,
        "the recipe key and the flag are one knob"
    );
    let (reset, _) = convert(
        "reset.tiff",
        &["--params", recipe.to_str().unwrap(), "--contrast", "1"],
    );
    assert_eq!(reset, unity, "the flag's 1 must win over the recipe's 1.5");
}

#[test]
fn the_look_contrast_is_refused_where_it_cannot_apply() {
    let input = fixture("hdr-48bit.tif").display().to_string();
    let tmp = TempDir::new("look-contrast-refused");
    let out = tmp.path("x.tiff");
    let base = [
        "convert",
        input.as_str(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ];
    // The current chain has no look stage; its contrast is the whole --density-gamma.
    let (code, _, err) = run(&[
        &base[..],
        &["--output-preset", "display-p3", "--contrast", "1.2"],
    ]
    .concat());
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("--contrast")
            && err.contains("no look stage")
            && err.contains("--density-gamma"),
        "{err}"
    );
    // A non-positive value is refused naming the flag and the key.
    for value in ["0", "-1"] {
        let (code, _, err) = run(&[&base[..], &["--new-flow", "--contrast", value]].concat());
        assert_eq!(code, 2, "{value}: {err}");
        assert!(err.contains("--contrast (recipe `look.contrast`)"), "{err}");
    }
    // So is a pair each usable alone whose product leaves f32 — a usage error naming
    // both knobs, not an internal one from highlight desaturation.
    for (gamma, contrast) in [("1e30", "1e10"), ("1e-30", "1e-20")] {
        let (code, _, err) = run(&[
            &base[..],
            &[
                "--new-flow",
                "--density-gamma",
                gamma,
                "--contrast",
                contrast,
            ],
        ]
        .concat());
        assert_eq!(code, 2, "{gamma} × {contrast}: {err}");
        assert!(
            err.contains("--density-gamma (recipe `reconstruction.linearization`)")
                && err.contains("--contrast (recipe `look.contrast`)"),
            "{err}"
        );
    }
}

/// The look's per-channel grade (`nf-look/per-channel-grade`): reachable by flag and by
/// recipe, one knob, the flag winning down to the identity, reported as it ran; and
/// refused where it cannot apply.
#[test]
fn the_channel_grade_reaches_the_pixels_by_flag_and_by_recipe() {
    let tmp = TempDir::new("look-channel-grade");
    let input = fixture("hdr-48bit.tif").display().to_string();
    let convert = |name: &str, extra: &[&str]| {
        let out = tmp.path(name);
        let mut argv = vec![
            "convert",
            input.as_str(),
            "-o",
            out.to_str().unwrap(),
            "--film-base",
            "0.9,0.55,0.42",
            "--new-flow",
            // Contrast and desaturation off, so `applied` reads the grade alone.
            "--contrast",
            "1",
            "--highlight-desaturation",
            "0",
        ];
        argv.extend_from_slice(extra);
        let (code, stdout, err) = run(&argv);
        assert_eq!(code, 0, "{extra:?}: {err}");
        (std::fs::read(&out).unwrap(), json(&stdout))
    };
    let applied = |r: &serde_json::Value| r["new_flow"]["stages"][1]["applied"].clone();

    let (identity, report) = convert("identity.tiff", &[]);
    assert_eq!(applied(&report), "identity", "{report}");
    assert_eq!(
        report["new_flow"]["look"]["channel_grade"],
        serde_json::json!([1.0, 1.0]),
        "{report}"
    );

    let (graded, report) = convert("graded.tiff", &["--channel-grade", "1.2,0.85"]);
    assert_eq!(applied(&report), "channel-grade", "{report}");
    assert_ne!(graded, identity, "the grade must move the pixels");

    let recipe = write_file(
        &tmp.path("look.json"),
        r#"{ "recipe_version": 2, "look": { "channel_grade": [1.2, 0.85] } }"#,
    );
    let (from_recipe, _) = convert("recipe.tiff", &["--params", recipe.to_str().unwrap()]);
    assert_eq!(
        graded, from_recipe,
        "the recipe key and the flag are one knob"
    );
    let (reset, report) = convert(
        "reset.tiff",
        &[
            "--params",
            recipe.to_str().unwrap(),
            "--channel-grade",
            "1,1",
        ],
    );
    assert_eq!(applied(&report), "identity", "{report}");
    assert_eq!(
        reset, identity,
        "the flag's 1,1 must win over the recipe's grade"
    );
}

#[test]
fn the_channel_grade_is_refused_where_it_cannot_apply() {
    let input = fixture("hdr-48bit.tif").display().to_string();
    let tmp = TempDir::new("look-channel-grade-refused");
    let out = tmp.path("x.tiff");
    let base = [
        "convert",
        input.as_str(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ];
    // The current chain has no look stage.
    let (code, _, err) = run(&[
        &base[..],
        &[
            "--output-preset",
            "display-p3",
            "--channel-grade",
            "1.1,0.9",
        ],
    ]
    .concat());
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("--channel-grade") && err.contains("no look stage"),
        "{err}"
    );
    // A non-positive exponent, and a spread that would fold the tone scale.
    for value in ["0,1", "1.1,-0.5", "1.6,0.5"] {
        let (code, _, err) = run(&[&base[..], &["--new-flow", "--channel-grade", value]].concat());
        assert_eq!(code, 2, "{value}: {err}");
        assert!(
            err.contains("--channel-grade (recipe `look.channel_grade`)"),
            "{err}"
        );
    }
}

#[test]
fn highlight_desaturation_is_refused_where_it_cannot_apply() {
    let input = fixture("hdr-48bit.tif").display().to_string();
    let tmp = TempDir::new("look-desat-refused");
    let out = tmp.path("x.tiff");
    let base = [
        "convert",
        input.as_str(),
        "-o",
        out.to_str().unwrap(),
        "--film-base",
        "0.9,0.55,0.42",
    ];
    // The current chain has no look stage.
    for flag in [
        ["--highlight-desaturation", "0.5"],
        ["--highlight-desaturation-start", "-2"],
        ["--highlight-desaturation-band", "0.01,0.02"],
    ] {
        let (code, _, err) = run(&[&base[..], &["--output-preset", "display-p3"], &flag].concat());
        assert_eq!(code, 2, "{flag:?}: {err}");
        assert!(
            err.contains("--new-flow") && err.contains("no look stage"),
            "{err}"
        );
    }
    // Out-of-range values are refused naming the flag and the key.
    for (flag, expect) in [
        (["--highlight-desaturation", "1.5"], "within [0, 1]"),
        // A negative value reaches the value rule rather than clap's parser.
        (["--highlight-desaturation", "-0.5"], "within [0, 1]"),
        (
            ["--highlight-desaturation-band", "-0.01,0.02"],
            "0 <= s0 < s1",
        ),
        (["--highlight-desaturation-start", "0"], "must be negative"),
        (
            ["--highlight-desaturation-band", "0.03,0.02"],
            "0 <= s0 < s1",
        ),
    ] {
        let (code, _, err) = run(&[&base[..], &["--new-flow"], &flag].concat());
        assert_eq!(code, 2, "{flag:?}: {err}");
        assert!(
            err.contains(flag[0])
                && err.contains("look.highlight_desaturation")
                && err.contains(expect),
            "{flag:?}: {err}"
        );
    }
}
