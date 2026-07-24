#!/usr/bin/env python3
"""Scan the nc-assets folder and generate / update its `manifest.json`.

The manifest is the tracked inventory + source-of-truth for nc-assets: every
roll frame (with its role), every standalone sample, and every converted output
(nc + NLP). It lives **at the assets root** with paths relative to its own
directory, so it is portable across machines / Drive mounts.

This is the precursor implementation for the `asset-manifest` task's eventual
`nctool manifest generate` command. Stdlib only (no venv). It NEVER reads sample
pixels into an agent context — only derived numbers (via `nc inspect`) and file
checksums.

Usage:
    python3 scripts/analysis/generate_manifest.py [ASSET_ROOT] [--nc PATH]
                                                  [--reuse-hash] [--dry-run]

ASSET_ROOT defaults to $NC_ASSET_ROOT, else ../nc-assets (the machine-local
symlink). The `nc` binary is found via --nc, $NC, ./target/release/nc,
./target/debug/nc, or `nc` on PATH — each candidate is verified to be this
project's CLI (via `--version`), so the system netcat (`/usr/bin/nc`) is never
mistaken for it.

Update behavior (idempotent): an existing manifest.json is loaded and its
**human-maintained** fields are preserved — roll frame `role`, roll `stock`/`note`,
sample `kind`/`note`, per-output `note`, and each converted bucket's
`regenerable` / `nc_version` / `recipe_dir` / `note`. Preserved fields survive an
asset **rename** by matching sha256 identity (only when the old path is gone, so a
*copy* does not clone a calibration role). Checksums are **recomputed every run**
by default (the manifest is a source-of-truth); pass --reuse-hash to reuse an
existing `sha256` when the byte size is unchanged (faster, but a same-size edit
would go undetected).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

# Seeds applied ONLY when neither the existing manifest nor a prior run supplies
# the value (i.e. first-ever generation). Human edits in manifest.json always win.
SEED_ROLES = {
    "Ektar": {"20260713-nikon-963": "unexposed", "20260715-nikon-1009": "leader"},
    "phoenix": {"20260712-nikon-933": "unexposed", "20260715-nikon-1010": "leader"},
    "Portra160": {"20260720-nikon-1059": "unexposed", "20260720-nikon-1058": "leader"},
    "Portra400": {"20260714-nikon-994": "unexposed", "20260717-nikon-1032": "leader"},
    "Portra400-leica-flaw": {"20260719-nikon-1034": "unexposed",
                             "20260719-nikon-1033": "leader"},
}
SEED_STOCK = {
    "Ektar": "Kodak Ektar 100", "phoenix": "Harman Phoenix 200",
    "Portra160": "Kodak Portra 160", "Portra160-2026-07-22": "Kodak Portra 160",
    "Portra400": "Kodak Portra 400", "Portra400-leica-flaw": "Kodak Portra 400",
}
SEED_ROLL_NOTE = {
    "Portra160-2026-07-22": "NLP comparison source; no in-roll unexposed/leader reference frame",
}
SEED_SAMPLE = {
    "largest.tif": {"kind": "perf-worst-case",
                    "note": "largest/highest-res scan available (~4x a standard 18.7 MP "
                            "frame); memory-preflight / streaming-tiled-io worst case"},
}
IMG_EXT = (".tif", ".tiff")


def is_nc(cand: str) -> bool:
    """Confirm `cand` is *this* project's `nc` CLI, not something else on PATH.
    `nc` is also the system netcat (`/usr/bin/nc`), which would be invoked once
    per image and silently degrade to exiftool. Our CLI prints `nc <version>` to
    stdout for `--version`; netcat writes its usage/error to stderr (empty stdout)."""
    try:
        out = subprocess.run([cand, "--version"], capture_output=True, text=True, timeout=15)
    except Exception:
        return False
    v = out.stdout.strip()
    return out.returncode == 0 and v.startswith("nc ") and any(c.isdigit() for c in v)


def find_nc() -> str | None:
    """Auto-discover this project's nc (verified). An explicit --nc / $NC is
    validated separately in main() — a bad explicit path is a hard error, not a
    silent fallback."""
    for cand in ("target/release/nc", "target/debug/nc", shutil.which("nc")):
        if cand and os.path.exists(cand) and is_nc(cand):
            return os.path.abspath(cand)
    return None


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def inspect(nc: str | None, path: str) -> dict:
    """Derived metadata for one image. Prefer `nc inspect` — the only
    authoritative source for `format`/`ir_present` — and fall back to exiftool
    whenever nc is unavailable or fails to decode the file (non-zero exit, timeout,
    or unparseable output; e.g. the NLP/V0 positive TIFFs nc does not recognize).
    Fallback entries are tagged `metadata_source: "exiftool"`; their
    `format`/`ir_present` are best-effort placeholders (`tiff`/`false`), never
    authoritative. Total failure yields `{"error": ..., "metadata_source": "none"}`."""
    if nc:
        try:
            out = subprocess.run([nc, "inspect", path], capture_output=True,
                                 text=True, timeout=180)
            if out.returncode == 0:
                d = json.loads(out.stdout)["decode"]
                return dict(width=d["width"], height=d["height"], channels=d["channels"],
                            bits=d["bits_per_sample"], format=d["format"],
                            ir_present=d["ir_present"])
            # non-zero exit: fall through to exiftool (nc rejects some positives)
        except (subprocess.TimeoutExpired, json.JSONDecodeError, KeyError,
                ValueError, OSError):
            pass
    try:
        ex = subprocess.run(["exiftool", "-s", "-s", "-s", "-ImageWidth",
                             "-ImageHeight", "-BitsPerSample", "-SamplesPerPixel", path],
                            capture_output=True, text=True, timeout=180).stdout.split("\n")
        return dict(width=int(ex[0]), height=int(ex[1]), channels=int(ex[3]),
                    bits=int(ex[2].split()[0]), format="tiff", ir_present=False,
                    metadata_source="exiftool")
    except Exception as e:
        return dict(error=str(e), metadata_source="none")


def list_imgs(d: str) -> list[str]:
    return sorted(f for f in os.listdir(d)
                  if f.lower().endswith(IMG_EXT) and os.path.isfile(os.path.join(d, f)))


class Prev:
    """Index into a previously written manifest for field preservation."""

    def __init__(self, data: dict):
        self.file: dict[str, dict] = {}
        self.roll: dict[str, dict] = {}
        self.bucket: dict[str, dict] = {}
        # sha256 -> [entries] (a list: byte-identical duplicates share a checksum,
        # and rename-matching must be able to pick the one whose old path is gone).
        self.frame_by_sha: dict[str, list[dict]] = {}   # role survival across renames
        self.sample_by_sha: dict[str, list[dict]] = {}  # kind/note survival
        self.output_by_sha: dict[str, list[dict]] = {}  # note survival

        def by_sha(idx: dict, e: dict) -> None:
            if e.get("sha256"):
                idx.setdefault(e["sha256"], []).append(e)

        for roll, r in data.get("rolls", {}).items():
            self.roll[roll] = r
            for fr in r.get("frames", []):
                self.file[fr["file"]] = fr
                by_sha(self.frame_by_sha, fr)
        for s in data.get("samples", []):
            self.file[s["file"]] = s
            by_sha(self.sample_by_sha, s)
        for name, b in data.get("converted", {}).items():
            self.bucket[name] = b
            for o in b.get("outputs", []):
                self.file[o["file"]] = o
                by_sha(self.output_by_sha, o)

    def sha(self, rel: str, size: int) -> str | None:
        e = self.file.get(rel)
        if e and e.get("bytes") == size and "sha256" in e:
            return e["sha256"]
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description="Generate/update nc-assets manifest.json")
    ap.add_argument("asset_root", nargs="?",
                    default=os.environ.get("NC_ASSET_ROOT", "../nc-assets"))
    ap.add_argument("--nc", help="path to the nc binary")
    ap.add_argument("--reuse-hash", action="store_true",
                    help="reuse an existing sha256 when the byte size is unchanged "
                         "(faster, but misses same-size edits; default recomputes all)")
    ap.add_argument("--dry-run", action="store_true",
                    help="print summary but do not write manifest.json")
    args = ap.parse_args()

    A = os.path.abspath(args.asset_root)
    if not os.path.isdir(A):
        print(f"error: asset root not found: {A}", file=sys.stderr)
        return 2
    # An explicit --nc / $NC must be *this* project's CLI — a bad/typo'd path (or
    # netcat) is a hard error, not a silent degrade to exiftool.
    explicit_nc = args.nc if args.nc is not None else os.environ.get("NC")
    if explicit_nc is not None:
        src = "--nc" if args.nc is not None else "$NC"
        if not os.path.exists(explicit_nc):
            print(f"error: {src} path does not exist: {explicit_nc}", file=sys.stderr)
            return 2
        if not is_nc(explicit_nc):
            print(f"error: {src}={explicit_nc} is not this project's nc CLI "
                  "(its `--version` did not report `nc <ver>`)", file=sys.stderr)
            return 2
        nc: str | None = os.path.abspath(explicit_nc)
    else:
        nc = find_nc()
        if not nc:
            print("warning: nc binary not found; falling back to exiftool for metadata",
                  file=sys.stderr)

    mpath = os.path.join(A, "manifest.json")
    prev_data: dict = {}
    if os.path.exists(mpath):
        try:
            with open(mpath) as f:
                prev_data = json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            print(f"error: existing {mpath} is not valid JSON ({e}); "
                  "fix it or delete it and re-run", file=sys.stderr)
            return 2
    # Refuse to update a manifest written by a newer/unsupported schema — this
    # tool only emits v1 and would silently downgrade it, discarding fields the
    # old Prev index doesn't understand.
    prev_ver = prev_data.get("schema_version")
    if prev_ver is not None and prev_ver != 1:
        print(f"error: existing {mpath} has unsupported schema_version {prev_ver} "
              "(this tool writes v1); refusing to overwrite it", file=sys.stderr)
        return 2
    prev = Prev(prev_data)
    carried: list[str] = []  # files whose authoritative nc metadata was preserved

    def rel(*parts: str) -> str:
        return "/".join(parts)

    def hashed(relpath: str, size: int, regenerable: bool) -> str | None:
        if regenerable:
            return None
        # Recompute by default (source-of-truth integrity); --reuse-hash trusts an
        # unchanged byte size, which is faster but misses same-size edits.
        if args.reuse_hash:
            reuse = prev.sha(relpath, size)
            if reuse:
                return reuse
        return sha256(os.path.join(A, relpath))

    def meta(relpath: str, regenerable: bool = False) -> dict:
        ap_ = os.path.join(A, relpath)
        e: dict = {}
        e.update(inspect(nc, ap_))
        # A file vanishing / becoming unreadable between listing and here must not
        # abort the whole (possibly hours-long) scan — record a per-file error.
        try:
            size = os.path.getsize(ap_)
            e["bytes"] = size
            s = hashed(relpath, size, regenerable)
            if s:
                e["sha256"] = s
        except OSError as ex:
            return {"error": f"{type(ex).__name__}: {ex}", "metadata_source": "none"}
        # Carry authoritative nc metadata forward when a *successful* exiftool
        # fallback would otherwise clobber it (e.g. nc missing on a negative) — but
        # only when the bytes still identify the same file (checksum match), so a
        # file replaced at the same path is not paired with the old metadata, and
        # only when the prior values are richer than the exiftool placeholders. A
        # *total* failure ("none") is never carried: it keeps its `error` so
        # corruption is reported and the run exits non-zero.
        if e.get("metadata_source") == "exiftool":
            pv = prev.file.get(relpath)
            if (pv and pv.get("sha256") and pv["sha256"] == e.get("sha256")
                    and (pv.get("format") not in (None, "tiff") or pv.get("ir_present") is True)):
                for k in ("width", "height", "channels", "bits", "format", "ir_present"):
                    if k in pv:
                        e[k] = pv[k]
                e.pop("metadata_source", None)
                e.pop("error", None)
                carried.append(relpath)
        if "width" in e and "height" in e:
            e["megapixels"] = round(e["width"] * e["height"] / 1e6, 2)
        return e

    def renamed_prev(by_sha: dict, sha: str | None) -> dict | None:
        """A prior entry with this checksum whose old path is gone from disk — a
        rename (preserve fields), not a copy (the original still exists → don't).
        With byte-identical duplicates, scan all candidates for one whose path
        disappeared, so a renamed calibration frame isn't missed because a copy
        still sits at its old path."""
        for cand in (by_sha.get(sha) or []) if sha else []:
            if not os.path.exists(os.path.join(A, cand["file"])):
                return cand
        return None

    m: dict = {
        "schema_version": 1,
        # Explicit env wins; otherwise keep the prior manifest's date so a plain
        # update stays byte-identical; "auto" only on first generation.
        "generated": os.environ.get("NC_MANIFEST_DATE") or prev_data.get("generated") or "auto",
        "note": ("Inventory + source-of-truth for nc-assets. Paths are relative to "
                 "this file's directory (the asset root), so the manifest is "
                 "portable across machines / Drive mounts. Metadata from `nc inspect`."),
        "rolls": {}, "samples": [], "converted": {}, "coverage_gaps": [],
    }

    # rolls/<roll>/<frame>.tif
    rename_map: dict[str, str] = {}  # old frame path -> new frame path (for source_frame retarget)
    rolls_dir = os.path.join(A, "rolls")
    for roll in (sorted(os.listdir(rolls_dir)) if os.path.isdir(rolls_dir) else []):
        rp = os.path.join(rolls_dir, roll)
        if not os.path.isdir(rp):
            continue
        frames = []
        for fn in list_imgs(rp):
            stem = os.path.splitext(fn)[0]
            r = rel("rolls", roll, fn)
            fm = meta(r)  # compute first so the checksum is available for identity
            # Role preservation: by path, else by checksum for a genuine rename
            # (old path gone — not a copy), else seed, else real.
            prevf = prev.file.get(r)
            if prevf is None:
                prevf = renamed_prev(prev.frame_by_sha, fm.get("sha256"))
                if prevf is not None:
                    rename_map[prevf["file"]] = r  # remember rename for source_frame retarget
            prevf = prevf or {}
            role = prevf.get("role") or SEED_ROLES.get(roll, {}).get(stem) or "real"
            frames.append({"file": r, "role": role, **fm})
        entry = {"stock": prev.roll.get(roll, {}).get("stock")
                 or SEED_STOCK.get(roll, "unknown"), "frames": frames}
        note = prev.roll.get(roll, {}).get("note") or SEED_ROLL_NOTE.get(roll)
        if note:
            entry["note"] = note
        m["rolls"][roll] = entry

    # samples/ (+ one level of subdirs, e.g. samples/icc)
    samples_dir = os.path.join(A, "samples")
    sample_files: list[str] = []
    if os.path.isdir(samples_dir):
        for fn in list_imgs(samples_dir):
            sample_files.append(rel("samples", fn))
        for sub in sorted(os.listdir(samples_dir)):
            sp = os.path.join(samples_dir, sub)
            if os.path.isdir(sp):
                for fn in list_imgs(sp):
                    sample_files.append(rel("samples", sub, fn))
    for r in sample_files:
        e = {"file": r, **meta(r)}
        # kind/note preservation: by path, else by checksum for a genuine rename.
        prevf = prev.file.get(r) or renamed_prev(prev.sample_by_sha, e.get("sha256")) or {}
        base = os.path.basename(r)
        seed = SEED_SAMPLE.get(base, {})
        e["kind"] = prevf.get("kind") or seed.get("kind") or (
            "icc-embed-test" if "/icc/" in r else "sample")
        note = prevf.get("note") or seed.get("note")
        if note:
            e["note"] = note
        m["samples"].append(e)

    # converted/<producer>/<version>/...
    conv_dir = os.path.join(A, "converted")

    def resolve_source(roll: str, ident: str) -> str | None:
        rp = os.path.join(rolls_dir, roll)
        if not os.path.isdir(rp):
            return None
        for fn in list_imgs(rp):
            stem = os.path.splitext(fn)[0]
            if stem == ident or stem.endswith(f"-{ident}"):
                return rel("rolls", roll, fn)
        return None

    SUFFIXES = ("_positive_hdr.tiff", "_positive.tiff", "_corr.tif", "_pos.tif",
                "-positive.tif")

    for producer in sorted(os.listdir(conv_dir)) if os.path.isdir(conv_dir) else []:
        pdir = os.path.join(conv_dir, producer)
        if not os.path.isdir(pdir):
            continue
        for version in sorted(os.listdir(pdir)):
            vdir = os.path.join(pdir, version)
            if not os.path.isdir(vdir):
                continue
            name = f"{producer}/{version}"
            pb = prev.bucket.get(name, {})
            regenerable = pb.get("regenerable", producer == "nc" and version != "V0")
            bucket = {"producer": producer, "regenerable": regenerable}
            for k in ("nc_version", "recipe_dir", "note"):
                if pb.get(k):
                    bucket[k] = pb[k]
            outputs = []
            # outputs may be directly under version/ (nlp) or under version/<roll>/ (nc)
            roll_dirs = [d for d in sorted(os.listdir(vdir))
                         if os.path.isdir(os.path.join(vdir, d))]
            walk = ([(None, vdir)] +
                    [(d, os.path.join(vdir, d)) for d in roll_dirs])
            for roll, wd in walk:
                for fn in list_imgs(wd):
                    r = rel(*([p for p in ("converted", producer, version, roll, fn) if p]))
                    stem = fn
                    for suf in SUFFIXES:
                        if fn.endswith(suf):
                            stem = fn[:-len(suf)]
                            break
                    src_roll = roll or "Portra160-2026-07-22"  # nlp default source roll
                    o = {"file": r}
                    if roll:
                        o["roll"] = roll
                    o.update(meta(r, regenerable=regenerable))  # sha needed for rename match
                    # Prior entry: by path, else by checksum for a genuine rename
                    # (so a renamed non-regenerable output keeps its human `note`).
                    prevf = (prev.file.get(r)
                             or renamed_prev(prev.output_by_sha, o.get("sha256")) or {})
                    # source_frame: resolve by stem; if that fails because the
                    # source frame was renamed, retarget the prior link through the
                    # rename map so the nc↔NLP↔source identity survives.
                    src = resolve_source(src_roll, stem)
                    if src is None and prevf.get("source_frame") in rename_map:
                        src = rename_map[prevf["source_frame"]]
                    o["source_frame"] = src
                    if prevf.get("note"):
                        o["note"] = prevf["note"]
                    # encoding: infer from inspected bit depth, not filename — a
                    # V0 `_corr.tif` is a 16-bit sRGB corrected variant, not float.
                    # nc float outputs are linear; nc 16-bit are sRGB.
                    bits = o.get("bits")
                    if bits == 32:
                        o["encoding"] = "f32-linear" if producer == "nc" else "f32"
                    elif bits == 16:
                        o["encoding"] = "u16-srgb" if producer == "nc" else "u16"
                    outputs.append(o)
            bucket["outputs"] = outputs
            m["converted"][name] = bucket

    # coverage gaps: NLP source frames with no NLP output
    nlp_sources = set()
    for name, b in m["converted"].items():
        if b["producer"] == "nlp":
            for o in b["outputs"]:
                if o.get("source_frame"):
                    nlp_sources.add(o["source_frame"])
    src_roll_dir = os.path.join(rolls_dir, "Portra160-2026-07-22")
    if os.path.isdir(src_roll_dir):
        allsrc = {rel("rolls", "Portra160-2026-07-22", f) for f in list_imgs(src_roll_dir)}
        missing = sorted(os.path.basename(x) for x in (allsrc - nlp_sources))
        if missing:
            m["coverage_gaps"].append(
                "NLP: Portra160-2026-07-22 frames without an NLP output: "
                + ", ".join(missing))

    print(f"asset root: {A}")
    print("rolls:", {k: len(v["frames"]) for k, v in m["rolls"].items()})
    print("samples:", len(m["samples"]))
    print("converted:", {k: f"{len(v['outputs'])} outputs, regenerable={v['regenerable']}"
                         for k, v in m["converted"].items()})
    print("coverage_gaps:", m["coverage_gaps"] or "none")
    n_ex = sum(1 for e in _iter_meta(m) if e.get("metadata_source") == "exiftool")
    errs = [e for e in _iter_meta(m) if "error" in e]
    unresolved = [o["file"] for b in m["converted"].values() for o in b["outputs"]
                  if "source_frame" in o and o["source_frame"] is None]
    if unresolved:
        print(f"WARNING: {len(unresolved)} converted output(s) have an unresolved "
              "source_frame (source renamed? stem mismatch?) — the nc↔NLP↔source "
              "link is broken for: " + ", ".join(os.path.basename(u) for u in unresolved),
              file=sys.stderr)
    if carried:
        print(f"note: carried forward authoritative nc metadata for {len(carried)} "
              "file(s) — this run used the exiftool fallback for them (nc missing/failed)",
              file=sys.stderr)
    if n_ex:
        print(f"note: {n_ex} entr(ies) used the exiftool fallback; their "
              "format/ir_present are best-effort, not authoritative", file=sys.stderr)
    if errs:
        print(f"WARNING: {len(errs)} file(s) failed all metadata inspection: "
              + ", ".join(os.path.basename(e.get("file", "?")) for e in errs),
              file=sys.stderr)

    if args.dry_run:
        print("(dry run — manifest.json not written)")
        return 1 if errs else 0
    # Atomic write: serialize to a same-dir temp, then replace — an interrupted or
    # failed write never truncates the existing source-of-truth manifest. Clean up
    # the temp if serialization/replace fails, so no stray .tmp is left behind.
    tmp = mpath + ".tmp"
    try:
        with open(tmp, "w") as f:
            json.dump(m, f, indent=2)
            f.write("\n")
        os.replace(tmp, mpath)
    except Exception:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise
    print("wrote", mpath)
    return 1 if errs else 0


def _iter_meta(m: dict):
    for r in m["rolls"].values():
        yield from r["frames"]
    yield from m["samples"]
    for b in m["converted"].values():
        yield from b["outputs"]


if __name__ == "__main__":
    raise SystemExit(main())
