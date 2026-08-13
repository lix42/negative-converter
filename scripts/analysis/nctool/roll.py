"""Manifest-driven roll calibration, conversion, and analysis.

This module turns the manual workflow in ``docs/using-nc.md`` into one command:
measure Dmin from the manifest's unexposed frame, measure Dmax from its leader,
freeze both into a partial recipe, and run ``nc roll`` over every real frame.
The durable ``tags.json`` and ``roll-report.json`` can be normalized into a
deterministic ``analysis.json`` artifact. Ordinary diff tools can then compare
configurations without opening their image pixels again.
"""
from __future__ import annotations

import datetime as _datetime
import hashlib
import json
import math
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

from . import manifest as _manifest

TAG_SCHEMA = 1
ANALYSIS_SCHEMA = 1
CONFIG_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def _load_object(path: Path) -> tuple[dict | None, str | None]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return None, f"cannot read {path}: {error}"
    if not isinstance(value, dict):
        return None, f"{path}: expected a JSON object"
    return value, None


def _write_json(path: Path, value: dict) -> None:
    """Atomically write a JSON artifact beside the conversion outputs."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
    except Exception:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise


def _asset_manifest(asset_root: Path) -> tuple[dict | None, str | None]:
    path = asset_root / "manifest.json"
    data, error = _manifest.load_manifest(str(path))
    if error:
        return None, error
    if not data:
        return None, f"no manifest.json at {asset_root}; run `nctool manifest generate` first"
    return data, None


def _roll_frames(data: dict, roll: str) -> tuple[dict | None, str | None]:
    spec = data.get("rolls", {}).get(roll)
    if not isinstance(spec, dict):
        available = ", ".join(sorted(data.get("rolls", {}))) or "(none)"
        return None, f"unknown roll {roll!r}; available: {available}"
    frames = spec.get("frames")
    if not isinstance(frames, list):
        return None, f"roll {roll!r} has no frames list"
    by_role: dict[str, list[dict]] = {"unexposed": [], "leader": [], "real": []}
    for frame in frames:
        if not isinstance(frame, dict) or not isinstance(frame.get("file"), str):
            return None, f"roll {roll!r} contains a malformed frame entry"
        role = frame.get("role", "real")
        if role not in by_role:
            return None, f"roll {roll!r} frame {frame['file']} has unsupported role {role!r}"
        by_role[role].append(frame)
    if len(by_role["unexposed"]) != 1 or len(by_role["leader"]) != 1:
        return None, (f"roll {roll!r} needs exactly one unexposed and one leader frame; "
                      f"found {len(by_role['unexposed'])} and {len(by_role['leader'])}")
    if not by_role["real"]:
        return None, f"roll {roll!r} has no real frames to convert"
    return by_role, None


def _region(raw: str | None, frame: dict, label: str,
            fraction: float = .8) -> tuple[str | None, str | None]:
    if raw is None:
        width, height = frame.get("width"), frame.get("height")
        if not isinstance(width, int) or not isinstance(height, int) or width <= 0 or height <= 0:
            return None, f"{label} frame lacks valid manifest dimensions"
        margin = (1 - fraction) / 2
        return (f"{round(margin * width)},{round(margin * height)},"
                f"{round(fraction * width)},{round(fraction * height)}"), None
    try:
        values = [int(item) for item in raw.split(",")]
    except ValueError:
        values = []
    if len(values) != 4 or any(value < 0 for value in values[:2]) or any(value <= 0 for value in values[2:]):
        return None, f"invalid {label} region {raw!r}; expected X,Y,W,H with positive W/H"
    return ",".join(map(str, values)), None


def _run_json(argv: list[str], label: str) -> tuple[dict | None, str | None]:
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    except OSError as error:
        return None, f"{label} could not start {argv[0]!r}: {error}"
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or "no diagnostic"
        return None, f"{label} failed (exit {proc.returncode}): {detail}"
    try:
        value = json.loads(proc.stdout)
    except json.JSONDecodeError as error:
        return None, f"{label} emitted invalid JSON: {error}"
    if not isinstance(value, dict):
        return None, f"{label} emitted a non-object JSON report"
    return value, None


def _recipe_input(path: str | None) -> tuple[dict | None, str | None]:
    if path is None:
        return {}, None
    value, error = _load_object(Path(path))
    if error:
        return None, error
    assert value is not None
    if set(value) == {"meta", "params"}:
        value = value.get("params")
        if not isinstance(value, dict):
            return None, f"{path}: sidecar `params` must be an object"
    return json.loads(json.dumps(value)), None


def _deep_merge(base: dict, overlay: dict) -> dict:
    """Recipe merge: objects recurse; every other overlay value replaces."""
    result = json.loads(json.dumps(base))
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(result.get(key), dict):
            # Tagged recipe objects have disjoint key sets. Switching sigmoid to
            # exponential while retaining the default sigmoid's contrast/toe/etc.
            # would create a recipe nc correctly rejects as mixed-curve input.
            old_type, new_type = result[key].get("type"), value.get("type")
            result[key] = (json.loads(json.dumps(value))
                           if new_type is not None and old_type != new_type
                           else _deep_merge(result[key], value))
        else:
            result[key] = json.loads(json.dumps(value))
    return result


def _freeze_recipe(base: dict, dmin: list[float], dmax: float,
                   film_type: str | None, preset: str | None,
                   exposure: float | None) -> tuple[dict | None, str | None]:
    """Overlay measured calibration and the convenience flags on a partial recipe."""
    recipe = json.loads(json.dumps(base))
    film_base = recipe.setdefault("film_base", {})
    if not isinstance(film_base, dict):
        return None, "recipe `film_base` must be an object"
    film_base["source"] = {"explicit": dmin}

    reconstruction = recipe.setdefault("reconstruction", {})
    if not isinstance(reconstruction, dict):
        return None, "recipe `reconstruction` must be an object"
    if reconstruction.get("type", "density") == "simple":
        return None, "manifest roll conversion requires density reconstruction so Dmax can be frozen"
    curve = reconstruction.setdefault("curve", {"type": "sigmoid"})
    if not isinstance(curve, dict):
        return None, "recipe `reconstruction.curve` must be an object"
    curve.setdefault("type", "sigmoid")
    curve["dmax"] = {"explicit": dmax}

    if film_type:
        input_cfg = recipe.setdefault("input", {})
        if not isinstance(input_cfg, dict):
            return None, "recipe `input` must be an object"
        input_cfg["film_type"] = film_type
    if preset:
        output = recipe.setdefault("output", {})
        if not isinstance(output, dict):
            return None, "recipe `output` must be an object"
        output["preset"] = preset
    if exposure is not None:
        print_cfg = recipe.setdefault("print", {})
        if not isinstance(print_cfg, dict):
            return None, "recipe `print` must be an object"
        print_cfg["print_exposure"] = exposure
    return recipe, None


def _float3(report: dict, key: str) -> list[float] | None:
    value = report.get(key)
    if isinstance(value, dict):
        value = [value.get("r"), value.get("g"), value.get("b")]
    if (isinstance(value, list) and len(value) == 3
            and all(isinstance(item, (int, float)) and math.isfinite(item) for item in value)):
        return [float(item) for item in value]
    return None


def _config_id(recipe: dict) -> str:
    payload = json.dumps(recipe, sort_keys=True, separators=(",", ":")).encode()
    return "config-" + hashlib.sha256(payload).hexdigest()[:12]


def _relative(path: Path, root: Path) -> str:
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return str(path.resolve())


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def cmd_convert(args) -> int:
    root = Path(args.asset_root).resolve()
    data, error = _asset_manifest(root)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert data is not None
    roles, error = _roll_frames(data, args.roll)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert roles is not None
    defaults, error = _run_json([args.nc, "params"], "default recipe query")
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    partial, error = _recipe_input(args.recipe)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert defaults is not None and partial is not None
    base = _deep_merge(defaults, partial)

    unexposed, leader = roles["unexposed"][0], roles["leader"][0]
    dmin_region, error = _region(args.dmin_region, unexposed, "Dmin")
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    dmax_region, error = _region(args.dmax_region, leader, "Dmax")
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert dmin_region and dmax_region

    operational = ["--max-memory", args.max_memory]
    strict = ["--strict"] if args.strict_estimate else []
    film_type = ["--film-type", args.film_type] if args.film_type else []
    unexposed_path = root / unexposed["file"]
    leader_path = root / leader["file"]
    all_frames = roles["unexposed"] + roles["leader"] + roles["real"]
    sources = []
    for frame in all_frames:
        path, label = root / frame["file"], frame.get("role", "frame")
        if not path.is_file():
            print(f"error: {label} frame is missing: {path}", file=sys.stderr)
            return 2
        expected = frame.get("sha256")
        if not isinstance(expected, str) or not expected:
            print(f"error: manifest frame has no sha256: {frame['file']}; regenerate it first",
                  file=sys.stderr)
            return 2
        try:
            actual = _sha256(path)
        except OSError as hash_error:
            print(f"error: cannot checksum {path}: {hash_error}", file=sys.stderr)
            return 2
        if actual != expected:
            print(f"error: manifest checksum drift for {frame['file']}; run "
                  "`nctool manifest generate` before converting", file=sys.stderr)
            return 1
        sources.append({"file": frame["file"], "role": frame.get("role", "real"),
                        "sha256": actual})

    dmin_mode = ["--grid"] if args.dmin_mode == "grid" else []
    dmin_report, error = _run_json(
        [args.nc, "estimate", str(unexposed_path), "--base-region", dmin_region,
         *dmin_mode, *film_type, *strict, *operational], "Dmin estimation")
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    assert dmin_report is not None
    dmin = _float3(dmin_report, "film_base")
    if dmin is None:
        print("error: Dmin report has no finite three-channel `film_base`", file=sys.stderr)
        return 1

    if args.d_max is not None:
        if not math.isfinite(args.d_max) or args.d_max <= 0:
            print("error: --d-max must be finite and greater than zero", file=sys.stderr)
            return 2
        dmax = args.d_max
        dmax_report = None
        dmax_source = "explicit-override"
    else:
        dmax_report, error = _run_json(
            [args.nc, "estimate", str(leader_path), "--film-base", ",".join(map(str, dmin)),
             "--d-max-region", dmax_region, *film_type, *strict, *operational],
            "Dmax estimation")
        if error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        assert dmax_report is not None
        dmax = dmax_report.get("dmax")
        if not isinstance(dmax, (int, float)) or not math.isfinite(dmax):
            print("error: Dmax report has no finite numeric `dmax`", file=sys.stderr)
            return 1
        dmax_source = "measured-reference"

    recipe, error = _freeze_recipe(base, dmin, float(dmax), args.film_type,
                                   args.output_preset, args.print_exposure)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert recipe is not None
    config = args.config or _config_id(recipe)
    if not CONFIG_RE.fullmatch(config):
        print("error: --config must contain only letters, digits, dot, underscore, or hyphen",
              file=sys.stderr)
        return 2
    out_dir = (Path(args.out_dir).resolve() if args.out_dir else
               root / "converted" / "nc" / config / args.roll)
    if out_dir.exists() and any(out_dir.iterdir()):
        print(f"error: output directory is not empty: {out_dir}", file=sys.stderr)
        return 2
    out_dir.mkdir(parents=True, exist_ok=True)
    recipe_path = out_dir / "recipe.json"
    calibration_path = out_dir / "calibration.json"
    report_path = out_dir / "roll-report.json"
    tags_path = out_dir / "tags.json"
    calibration = {
        "schema_version": 1,
        "roll": args.roll,
        "dmin": {"frame": unexposed["file"], "region": dmin_region,
                 "mode": args.dmin_mode, "value": dmin, "report": dmin_report},
        "dmax": {"source": dmax_source, "frame": leader["file"], "region": dmax_region,
                 "value": float(dmax), "report": dmax_report},
    }
    _write_json(recipe_path, recipe)
    _write_json(calibration_path, calibration)

    real_paths = [str(root / frame["file"]) for frame in roles["real"]]
    argv = [args.nc, "roll", *real_paths, "--out-dir", str(out_dir),
            "--params", str(recipe_path), "--report-file", str(report_path),
            "--max-memory", args.max_memory]
    if args.strict_roll:
        argv.append("--strict")
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    except OSError as run_error:
        print(f"error: roll conversion could not start {args.nc!r}: {run_error}",
              file=sys.stderr)
        return 2
    if proc.stdout.strip():
        print(proc.stdout, end="", file=sys.stderr)
    if proc.stderr:
        print(proc.stderr, end="", file=sys.stderr)
    roll_report, report_error = _load_object(report_path)
    if report_error:
        print(f"error: roll conversion produced no usable report: {report_error}", file=sys.stderr)
        return proc.returncode or 1
    assert roll_report is not None
    tags = {
        "schema_version": TAG_SCHEMA,
        "kind": "nctool-roll-conversion",
        "created": _datetime.datetime.now(_datetime.timezone.utc).isoformat(),
        "config": config,
        "roll": args.roll,
        "asset_root_manifest_generated": data.get("generated"),
        "source_frames": sources,
        "output_dir": _relative(out_dir, root),
        "recipe_file": _relative(recipe_path, root),
        "calibration_file": _relative(calibration_path, root),
        "report_file": _relative(report_path, root),
        "identity": roll_report.get("identity"),
        "recipe": recipe,
        "calibration": {"dmin": calibration["dmin"] | {"report": None},
                        "dmax": calibration["dmax"] | {"report": None}},
        "summary": roll_report.get("summary"),
    }
    tags["calibration"]["dmin"].pop("report")
    tags["calibration"]["dmax"].pop("report")
    _write_json(tags_path, tags)
    if proc.returncode != 0:
        print(f"error: nc roll exited {proc.returncode}; tags preserve the failed run",
              file=sys.stderr)
        return 1
    print(json.dumps(tags, indent=2, sort_keys=True))
    return 0


def _tag_path(root: Path, roll: str, ref: str) -> Path:
    path = Path(ref)
    if path.is_file() or path.name == "tags.json" or os.sep in ref:
        return path.resolve()
    return root / "converted" / "nc" / ref / roll / "tags.json"


def _depth(recipe: dict) -> str:
    output = recipe.get("output") if isinstance(recipe.get("output"), dict) else {}
    preset = output.get("preset", "gain-map-hdr")
    if preset in ("gain-map-hdr", "ultra-hdr-v1"):
        return "u8"
    if preset in ("hdr-pq", "hdr-hlg"):
        return "u10"
    if preset in ("film-master", "hdr-linear-tiff"):
        return "f32"
    if preset in ("legacy", "custom") and output.get("depth") == "f32":
        return "f32"
    return "u16"


def _clip(frame: dict) -> float | None:
    loss = frame.get("loss")
    if not isinstance(loss, dict):
        return None
    total = loss.get("total_samples")
    low, high = loss.get("clipped_low"), loss.get("clipped_high")
    if not all(isinstance(value, (int, float)) for value in (total, low, high)):
        return None
    return (low + high) / total if total else 0.0


def _analysis_frame(frame: dict, source_by_name: dict[str, str]) -> dict:
    """Select stable, conversion-relevant fields from one roll report row."""
    input_ref = frame.get("input")
    name = Path(input_ref).name if isinstance(input_ref, str) else "(unknown)"
    result = {
        "source": source_by_name.get(name, name),
        "status": frame.get("status"),
    }
    for key in ("film_base", "dmax", "white_balance", "input_color", "loss",
                "output_stats", "identity", "warnings", "error"):
        if key in frame:
            result[key] = frame[key]
    clip_fraction = _clip(frame)
    if clip_fraction is not None:
        result["clip_fraction"] = clip_fraction
    return result


def cmd_analyze(args) -> int:
    """Write one deterministic, diff-friendly artifact for a converted roll."""
    root = Path(args.asset_root).resolve()
    tag_path = _tag_path(root, args.roll, args.run)
    tag, error = _load_object(tag_path)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert tag is not None
    if tag.get("schema_version") != TAG_SCHEMA or tag.get("kind") != "nctool-roll-conversion":
        print(f"error: {tag_path} is not an nctool roll tag v{TAG_SCHEMA}", file=sys.stderr)
        return 2
    if tag.get("roll") != args.roll:
        print(f"error: tags are for roll {tag.get('roll')!r}, not {args.roll!r}",
              file=sys.stderr)
        return 2
    sources = tag.get("source_frames")
    if not isinstance(sources, list) or not sources:
        print("error: tags have no source-frame checksum inventory", file=sys.stderr)
        return 2
    report_ref = tag.get("report_file")
    if not isinstance(report_ref, str):
        print("error: tags have no report_file", file=sys.stderr)
        return 2
    report_path = Path(report_ref)
    if not report_path.is_absolute():
        report_path = root / report_path
    report, error = _load_object(report_path)
    if error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    assert report is not None

    stable_sources = sorted(
        ({key: source.get(key) for key in ("file", "role", "sha256")}
         for source in sources if isinstance(source, dict)),
        key=lambda source: str(source.get("file")),
    )
    source_by_name = {
        Path(source["file"]).name: source["file"]
        for source in stable_sources if isinstance(source.get("file"), str)
    }
    frames = [
        _analysis_frame(frame, source_by_name)
        for frame in report.get("frames", []) if isinstance(frame, dict)
    ]
    frames.sort(key=lambda frame: str(frame.get("source")))
    output = {
        "schema_version": ANALYSIS_SCHEMA,
        "kind": "nctool-roll-analysis",
        "roll": args.roll,
        "config": tag.get("config"),
        "source_frames": stable_sources,
        "identity": tag.get("identity"),
        "output_depth": _depth(tag.get("recipe", {})),
        "recipe": tag.get("recipe"),
        "calibration": tag.get("calibration"),
        "summary": report.get("summary", tag.get("summary")),
        "frames": frames,
        "note": ("Stable analysis of nc's recorded per-frame conversion facts; it does "
                 "not prove pixel identity or judge visual quality."),
    }
    out_path = Path(args.out).resolve() if args.out else tag_path.with_name("analysis.json")
    try:
        _write_json(out_path, output)
    except OSError as write_error:
        print(f"error: cannot write {out_path}: {write_error}", file=sys.stderr)
        return 2
    print(f"wrote {out_path} ({len(frames)} frames)", file=sys.stderr)
    return 0
