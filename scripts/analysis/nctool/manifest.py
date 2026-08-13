"""Generate, validate, and query the nc-assets `manifest.json`.

The manifest is the tracked inventory + source-of-truth for nc-assets: every roll
frame (with its role), every standalone sample, and every converted output (nc +
NLP). It lives **at the assets root** with paths relative to its own directory, so
it is portable across machines / Drive mounts.

This module NEVER reads sample pixels into an agent context — only derived numbers
(via `nc inspect`) and streamed file checksums.

Commands (dispatched from `python -m nctool manifest …`):
- `generate` — walk the asset root, fill derived fields, write `manifest.json`.
  Idempotent and role-preserving (human fields survive re-scans and renames).
- `validate` — report drift (checksum mismatch), orphans (on disk, not in the
  manifest), and missing (in the manifest, not on disk). REPORTS only; never
  deletes.
- `roles` — emit the per-roll `roll|unexposed|leader|real…` triples the real-scan
  harness needs, sourced from the manifest instead of a hard-coded array.
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile

# Seeds applied ONLY when neither the existing manifest nor a prior run supplies
# the value (i.e. first-ever generation). Human edits in manifest.json always win.
SEED_ROLES = {
    "Ektar": {"20260713-nikon-963": "unexposed", "20260715-nikon-1009": "leader"},
    "phoenix": {"20260712-nikon-933": "unexposed", "20260715-nikon-1010": "leader"},
    "Portra160": {"20260720-nikon-1059": "unexposed", "20260720-nikon-1058": "leader"},
    "Portra400": {"20260714-nikon-994": "unexposed", "20260717-nikon-1032": "leader"},
    "Portra400-leica-flaw": {"20260719-nikon-1034": "unexposed",
                             "20260719-nikon-1033": "leader"},
    # Confirmed 2026-08-02 while freezing the sigmoid-baseline fixtures. These two rolls
    # had no seed, so a from-scratch generation (no `prev` to inherit roles from) left every
    # frame `real`, `manifest roles` skipped both rolls, and the committed freeze recipes
    # became unreproducible. Seeds exist precisely to survive that case.
    "Portra160-2026-07-22": {"20260722-nikon-1097": "unexposed",
                             "20260722-nikon-1096": "leader"},
    "2026-07-24-Gold200": {"20260724-leica-1130": "unexposed",
                           "20260724-leica-1129": "leader"},
    "portra400-2026-08-04": {"20260803-film-1230": "unexposed",
                              "20260803-film-1229": "leader"},
}
SEED_STOCK = {
    "Ektar": "Kodak Ektar 100", "phoenix": "Harman Phoenix 200",
    "Portra160": "Kodak Portra 160", "Portra160-2026-07-22": "Kodak Portra 160",
    "Portra400": "Kodak Portra 400", "Portra400-leica-flaw": "Kodak Portra 400",
    "2026-07-24-Gold200": "Kodak Gold 200",
}
SEED_ROLL_NOTE = {
    # The earlier "no in-roll unexposed/leader reference frame" claim was wrong: this
    # roll has leader 20260722-nikon-1096 and unexposed 20260722-nikon-1097, confirmed
    # by `manifest roles` on 2026-08-02.
    #
    # A seed is NOT a migration: `build_manifest` prefers `prev.roll[roll].note`, so this
    # value only applies when no prior note exists. Fixing the seed alone would have left
    # the wrong sentence in place on every ordinary regeneration — the live manifest and
    # `scripts/analysis/manifest.sample.json` were corrected directly for that reason.
    "Portra160-2026-07-22": "NLP comparison source",
}
SEED_SAMPLE = {
    "largest.tif": {"kind": "perf-worst-case",
                    "note": "largest/highest-res scan available (~4x a standard 18.7 MP "
                            "frame); memory-preflight / streaming-tiled-io worst case"},
}
IMG_EXT = (".tif", ".tiff")
SUFFIXES = ("_positive_hdr.tiff", "_positive.tiff", "_corr.tif", "_pos.tif",
            "-positive.tif")


# --------------------------------------------------------------------------- nc

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
    validated separately by resolve_nc() — a bad explicit path is a hard error, not
    a silent fallback."""
    for cand in ("target/release/nc", "target/debug/nc", shutil.which("nc")):
        if cand and os.path.exists(cand) and is_nc(cand):
            return os.path.abspath(cand)
    return None


def resolve_nc(explicit: str | None, src: str) -> tuple[str | None, str | None]:
    """Resolve the nc binary. An explicit --nc / $NC must be *this* project's CLI —
    a bad/typo'd path (or netcat) is a hard error, not a silent degrade to exiftool.
    Returns (nc_path, error): error is a message string when an explicit path is
    invalid; nc_path is None (with no error) when auto-discovery falls back."""
    if explicit is not None:
        if not os.path.exists(explicit):
            return None, f"{src} path does not exist: {explicit}"
        if not is_nc(explicit):
            return None, (f"{src}={explicit} is not this project's nc CLI "
                          "(its `--version` did not report `nc <ver>`)")
        return os.path.abspath(explicit), None
    return find_nc(), None


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def inspect(nc: str | None, path: str) -> dict:
    """Derived metadata for one image. Prefer `nc inspect` — the only
    authoritative source for `format`/`ir_present` — and fall back to exiftool
    when nc is unavailable, times out, or *rejects* the file (non-zero exit; e.g.
    the NLP/V0 positive TIFFs nc does not recognize). Fallback entries are tagged
    `metadata_source: "exiftool"`; their `format`/`ir_present` are best-effort
    placeholders (`tiff`/`false`), never authoritative.

    A `nc inspect` that exits 0 but whose JSON is unparseable / missing expected
    keys is NOT a rejection — nc accepted and decoded the file — so it is a loud
    per-file error (`{"error": ..., "metadata_source": "none"}`), never a silent
    downgrade to the exiftool placeholders (which would corrupt `format`/
    `ir_present` for a file nc actually understands). Total inspection failure
    likewise yields `{"error": ..., "metadata_source": "none"}`."""
    if nc:
        out = None
        try:
            out = subprocess.run([nc, "inspect", path], capture_output=True,
                                 text=True, timeout=180)
        except (subprocess.TimeoutExpired, OSError):
            out = None  # nc could not run to completion → treat as unavailable
        if out is not None and out.returncode == 0:
            # nc accepted and decoded the file: its JSON is authoritative and MUST
            # parse. A parse/key failure here is a real defect, not a rejection —
            # report it loudly rather than falling through to placeholders.
            try:
                d = json.loads(out.stdout)["decode"]
                return dict(width=d["width"], height=d["height"], channels=d["channels"],
                            bits=d["bits_per_sample"], format=d["format"],
                            ir_present=d["ir_present"])
            except (json.JSONDecodeError, KeyError, ValueError) as ex:
                return dict(error=f"nc inspect exited 0 but its output was unparseable "
                                  f"({type(ex).__name__}: {ex})",
                            metadata_source="none")
        # nc absent, timed out, or rejected the file (non-zero exit): fall through
        # to the exiftool fallback below.
    try:
        ex = subprocess.run(["exiftool", "-s", "-s", "-s", "-ImageWidth",
                             "-ImageHeight", "-BitsPerSample", "-SamplesPerPixel", path],
                            capture_output=True, text=True, timeout=180).stdout.split("\n")
        return dict(width=int(ex[0]), height=int(ex[1]), channels=int(ex[3]),
                    bits=int(ex[2].split()[0]), format="tiff", ir_present=False,
                    metadata_source="exiftool")
    except Exception as e:
        return dict(error=str(e), metadata_source="none")


# ------------------------------------------------------------------ asset walks
# One set of directory-walking rules, shared by generate (which builds structured
# entries) and validate (which flattens to an on-disk file set). Keeping the walk
# in one place guarantees orphan/missing detection matches what generate records.

def rel(*parts: str) -> str:
    return "/".join(parts)


def list_imgs(d: str) -> list[str]:
    return sorted(f for f in os.listdir(d)
                  if f.lower().endswith(IMG_EXT) and os.path.isfile(os.path.join(d, f)))


def walk_rolls(A: str) -> list[tuple[str, list[str]]]:
    """[(roll_name, [frame relpaths])] — sorted roll dirs, sorted frames."""
    rolls_dir = os.path.join(A, "rolls")
    out: list[tuple[str, list[str]]] = []
    for roll in (sorted(os.listdir(rolls_dir)) if os.path.isdir(rolls_dir) else []):
        rp = os.path.join(rolls_dir, roll)
        if not os.path.isdir(rp):
            continue
        out.append((roll, [rel("rolls", roll, fn) for fn in list_imgs(rp)]))
    return out


def walk_samples(A: str) -> list[str]:
    """Sample relpaths: top-level images first, then one level of subdirs."""
    samples_dir = os.path.join(A, "samples")
    out: list[str] = []
    if os.path.isdir(samples_dir):
        for fn in list_imgs(samples_dir):
            out.append(rel("samples", fn))
        for sub in sorted(os.listdir(samples_dir)):
            sp = os.path.join(samples_dir, sub)
            if os.path.isdir(sp):
                for fn in list_imgs(sp):
                    out.append(rel("samples", sub, fn))
    return out


def walk_converted(A: str) -> list[tuple[str, str, list[tuple[str | None, str]]]]:
    """[(producer, version, [(roll_or_None, relpath)])] — outputs may live directly
    under version/ (nlp) or under version/<roll>/ (nc)."""
    conv_dir = os.path.join(A, "converted")
    out: list[tuple[str, str, list[tuple[str | None, str]]]] = []
    for producer in sorted(os.listdir(conv_dir)) if os.path.isdir(conv_dir) else []:
        pdir = os.path.join(conv_dir, producer)
        if not os.path.isdir(pdir):
            continue
        for version in sorted(os.listdir(pdir)):
            vdir = os.path.join(pdir, version)
            if not os.path.isdir(vdir):
                continue
            files: list[tuple[str | None, str]] = []
            roll_dirs = [d for d in sorted(os.listdir(vdir))
                         if os.path.isdir(os.path.join(vdir, d))]
            walk = [(None, vdir)] + [(d, os.path.join(vdir, d)) for d in roll_dirs]
            for roll, wd in walk:
                for fn in list_imgs(wd):
                    r = rel(*[p for p in ("converted", producer, version, roll, fn) if p])
                    files.append((roll, r))
            out.append((producer, version, files))
    return out


def disk_files(A: str) -> list[str]:
    """Flat list of every image relpath in the recognized layout — the on-disk
    inventory validate uses for missing / drift (a manifest entry lives in a
    recognized location by construction)."""
    files: list[str] = []
    for _roll, frames in walk_rolls(A):
        files.extend(frames)
    files.extend(walk_samples(A))
    for _p, _v, outs in walk_converted(A):
        files.extend(r for _roll, r in outs)
    return files


def all_disk_images(A: str) -> list[str]:
    """Every image relpath anywhere under the asset root (recursive) — a superset
    of `disk_files`'s recognized-layout walk, used for ORPHAN detection so a
    misplaced or deeply-nested scan (a root-level stray, `rolls/<roll>/sub/x.tif`,
    `samples/icc/sub/x.tif`) is still flagged instead of being invisible.

    Only image artifacts (`.tif`/`.tiff`) are returned: documented companions
    (`.json`/`.jpg` sidecars) and `manifest.json` itself are intentionally
    untracked and so are excluded by the extension filter — they must never be
    reported as orphans."""
    files: list[str] = []
    for dirpath, dirnames, filenames in os.walk(A):
        dirnames.sort()
        for fn in sorted(filenames):
            if fn.lower().endswith(IMG_EXT):
                relpath = os.path.relpath(os.path.join(dirpath, fn), A)
                files.append(relpath.replace(os.sep, "/"))
    return files


# ----------------------------------------------------------------- prev-manifest

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

    def sha(self, rel_: str, size: int) -> str | None:
        e = self.file.get(rel_)
        if e and e.get("bytes") == size and "sha256" in e:
            return e["sha256"]
        return None


def load_manifest(mpath: str) -> tuple[dict, str | None]:
    """Load an existing manifest.json. Returns (data, error). A missing file yields
    ({}, None); invalid JSON or an unsupported schema_version yields a message."""
    if not os.path.exists(mpath):
        return {}, None
    try:
        with open(mpath) as f:
            data = json.load(f)
    except (json.JSONDecodeError, OSError) as e:
        return {}, f"{mpath} is not valid JSON ({e})"
    # Syntactically valid JSON can still be a non-object (`[]`, `null`, a string);
    # reading schema fields off it would raise AttributeError. Reject loudly.
    if not isinstance(data, dict):
        return {}, (f"{mpath} is not a JSON object "
                    f"(top-level {type(data).__name__})")
    ver = data.get("schema_version")
    if ver is not None and ver != 1:
        return {}, (f"{mpath} has unsupported schema_version {ver} "
                    "(this tool writes v1)")
    # The top-level object can still carry malformed nested collections (e.g.
    # `"rolls": []`); consumers iterate `rolls`/`converted` as dicts and `samples`
    # as a list, so a wrong-typed collection would raise mid-iteration (AttributeError
    # → traceback + exit 1) instead of the documented operational exit 2. Reject here.
    for key in ("rolls", "converted"):
        if key in data and not isinstance(data[key], dict):
            return {}, (f"{mpath} has malformed {key!r} "
                        f"(expected a JSON object, got {type(data[key]).__name__})")
    if "samples" in data and not isinstance(data["samples"], list):
        return {}, (f"{mpath} has malformed 'samples' "
                    f"(expected a JSON array, got {type(data['samples']).__name__})")
    return data, None


# ------------------------------------------------------------------ build (v1)

def build_manifest(A: str, nc: str | None, reuse_hash: bool,
                   prev_data: dict) -> tuple[dict, dict]:
    """Assemble the v1 manifest dict from the asset tree. Returns (manifest,
    diagnostics), where diagnostics carries the derived counts / warnings the CLI
    prints (`carried`, `unresolved`, `exiftool`, `errors`). Pure w.r.t. the
    filesystem apart from reading files (nc inspect + streamed hashes)."""
    prev = Prev(prev_data)
    carried: list[str] = []  # files whose authoritative nc metadata was preserved

    def hashed(relpath: str, size: int, regenerable: bool) -> str | None:
        if regenerable:
            return None
        # Recompute by default (source-of-truth integrity); reuse_hash trusts an
        # unchanged byte size, which is faster but misses same-size edits.
        if reuse_hash:
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
    rolls_dir = os.path.join(A, "rolls")
    rename_map: dict[str, str] = {}  # old frame path -> new frame path (source_frame retarget)
    for roll, frame_rels in walk_rolls(A):
        frames = []
        for r in frame_rels:
            stem = os.path.splitext(os.path.basename(r))[0]
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
    for r in walk_samples(A):
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
    def resolve_source(roll: str, ident: str) -> str | None:
        rp = os.path.join(rolls_dir, roll)
        if not os.path.isdir(rp):
            return None
        for fn in list_imgs(rp):
            stem = os.path.splitext(fn)[0]
            if stem == ident or stem.endswith(f"-{ident}"):
                return rel("rolls", roll, fn)
        return None

    for producer, version, files in walk_converted(A):
        name = f"{producer}/{version}"
        pb = prev.bucket.get(name, {})
        regenerable = pb.get("regenerable", producer == "nc" and version != "V0")
        bucket = {"producer": producer, "regenerable": regenerable}
        for k in ("nc_version", "recipe_dir", "note"):
            if pb.get(k):
                bucket[k] = pb[k]
        outputs = []
        for roll, r in files:
            fn = os.path.basename(r)
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
            # source_frame: resolve by stem; if that fails because the source frame
            # was renamed, retarget the prior link through the rename map so the
            # nc↔NLP↔source identity survives.
            src = resolve_source(src_roll, stem)
            if src is None and prevf.get("source_frame") in rename_map:
                src = rename_map[prevf["source_frame"]]
            o["source_frame"] = src
            if prevf.get("note"):
                o["note"] = prevf["note"]
            # encoding: infer from inspected bit depth, not filename — a V0
            # `_corr.tif` is a 16-bit sRGB corrected variant, not float. nc float
            # outputs are linear; nc 16-bit are sRGB.
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

    unresolved = [o["file"] for b in m["converted"].values() for o in b["outputs"]
                  if "source_frame" in o and o["source_frame"] is None]
    n_ex = sum(1 for e in iter_meta(m) if e.get("metadata_source") == "exiftool")
    errs = [e for e in iter_meta(m) if "error" in e]
    return m, {"carried": carried, "unresolved": unresolved,
               "exiftool": n_ex, "errors": errs}


def iter_meta(m: dict):
    for r in m["rolls"].values():
        yield from r["frames"]
    yield from m["samples"]
    for b in m["converted"].values():
        yield from b["outputs"]


def write_manifest(mpath: str, m: dict) -> None:
    """Atomic durable write: serialize to a UNIQUE same-dir temp, fsync it, then
    `os.replace` — an interrupted or failed write never truncates the existing
    source-of-truth manifest, the fsync means a crash right after replace can't
    leave a zero-length file, and the unique temp name (mkstemp) means two
    concurrent `generate` runs can't clobber each other's temp. Clean up the temp
    if serialization/replace fails, so no stray tempfile is left behind."""
    d = os.path.dirname(mpath) or "."
    fd, tmp = tempfile.mkstemp(dir=d, prefix=".manifest.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(m, f, indent=2)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        # mkstemp forces 0600; replacing an existing (possibly group/world-readable,
        # e.g. 0664 on a shared Drive folder) manifest must not silently tighten its
        # permissions. Preserve the existing mode; for a brand-new file fall back to
        # a umask-respecting default rather than the restrictive 0600.
        try:
            mode = stat.S_IMODE(os.stat(mpath).st_mode)
        except FileNotFoundError:
            umask = os.umask(0)
            os.umask(umask)
            mode = 0o666 & ~umask
        os.chmod(tmp, mode)
        os.replace(tmp, mpath)
    except Exception:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise


# --------------------------------------------------------------------- commands

def _asset_root(args) -> str:
    return os.path.abspath(args.asset_root)


def cmd_generate(args) -> int:
    A = _asset_root(args)
    if not os.path.isdir(A):
        print(f"error: asset root not found: {A}", file=sys.stderr)
        return 2
    explicit = args.nc if args.nc is not None else os.environ.get("NC")
    src = "--nc" if args.nc is not None else "$NC"
    nc, err = resolve_nc(explicit, src)
    if err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    if nc is None:
        # A wholesale nc-absent run would produce an all-placeholder manifest
        # (format/ir_present are exiftool best-effort, not authoritative). That is
        # a degraded artifact, so — matching the bad-explicit-nc path — refuse it
        # loudly by default; the exiftool-only mode must be opted into explicitly.
        if not getattr(args, "allow_exiftool_fallback", False):
            print("error: nc binary not found; refusing to build an exiftool-only "
                  "manifest by default (format/ir_present would be non-authoritative "
                  "placeholders).", file=sys.stderr)
            print("       build it (cargo build --release) or pass --nc PATH; to "
                  "deliberately build a degraded exiftool-only manifest, pass "
                  "--allow-exiftool-fallback.", file=sys.stderr)
            return 2
        print("warning: nc binary not found; --allow-exiftool-fallback set → building "
              "a DEGRADED exiftool-only manifest (format/ir_present are placeholders)",
              file=sys.stderr)

    mpath = os.path.join(A, "manifest.json")
    prev_data, err = load_manifest(mpath)
    if err:
        print(f"error: existing {err}; fix it or delete it and re-run", file=sys.stderr)
        return 2

    m, diag = build_manifest(A, nc, args.reuse_hash, prev_data)

    print(f"asset root: {A}")
    print("rolls:", {k: len(v["frames"]) for k, v in m["rolls"].items()})
    print("samples:", len(m["samples"]))
    print("converted:", {k: f"{len(v['outputs'])} outputs, regenerable={v['regenerable']}"
                         for k, v in m["converted"].items()})
    print("coverage_gaps:", m["coverage_gaps"] or "none")
    if diag["unresolved"]:
        print(f"WARNING: {len(diag['unresolved'])} converted output(s) have an unresolved "
              "source_frame (source renamed? stem mismatch?) — the nc↔NLP↔source "
              "link is broken for: "
              + ", ".join(os.path.basename(u) for u in diag["unresolved"]), file=sys.stderr)
    if diag["carried"]:
        print(f"note: carried forward authoritative nc metadata for {len(diag['carried'])} "
              "file(s) — this run used the exiftool fallback for them (nc missing/failed)",
              file=sys.stderr)
    if diag["exiftool"]:
        print(f"note: {diag['exiftool']} entr(ies) used the exiftool fallback; their "
              "format/ir_present are best-effort, not authoritative", file=sys.stderr)
    if diag["errors"]:
        print(f"WARNING: {len(diag['errors'])} file(s) failed all metadata inspection: "
              + ", ".join(os.path.basename(e.get("file", "?")) for e in diag["errors"]),
              file=sys.stderr)

    if args.dry_run:
        print("(dry run — manifest.json not written)")
        return 1 if diag["errors"] else 0
    write_manifest(mpath, m)
    print("wrote", mpath)
    return 1 if diag["errors"] else 0


def cmd_validate(args) -> int:
    """Report drift / orphans / missing against the on-disk tree. Never deletes —
    it surfaces disposable/stale candidates for the user's explicit decision.
    Exit 0 = clean, 1 = discrepancies found, 2 = operational error."""
    A = _asset_root(args)
    if not os.path.isdir(A):
        print(f"error: asset root not found: {A}", file=sys.stderr)
        return 2
    mpath = os.path.join(A, "manifest.json")
    if not os.path.exists(mpath):
        print(f"error: no manifest.json at {A}; run `nctool manifest generate` first",
              file=sys.stderr)
        return 2
    data, err = load_manifest(mpath)
    if err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    # Index manifest entries by relpath (recorded sha256 + bytes where present).
    # `regenerable_files` are the ONLY entries for which a missing sha256 is
    # legitimate (their bucket is explicitly regenerable: true — the harness
    # reproduces them, so they are deliberately not hashed). Every other entry
    # without a sha256 is a real integrity gap (see below).
    recorded: dict[str, dict] = {}
    regenerable_files: set[str] = set()
    for r in data.get("rolls", {}).values():
        for fr in r.get("frames", []):
            recorded[fr["file"]] = fr
    for s in data.get("samples", []):
        recorded[s["file"]] = s
    for b in data.get("converted", {}).values():
        regen = b.get("regenerable") is True
        for o in b.get("outputs", []):
            recorded[o["file"]] = o
            if regen:
                regenerable_files.add(o["file"])

    on_disk = set(disk_files(A))            # recognized layout: missing / drift
    on_disk_all = set(all_disk_images(A))   # full tree: orphan detection
    in_manifest = set(recorded)

    missing = sorted(in_manifest - on_disk)     # recorded, but gone from disk
    orphans = sorted(on_disk_all - in_manifest)  # on disk anywhere, not recorded
    both = sorted(in_manifest & on_disk)

    # Entries whose metadata inspection failed outright (nc-parse failure, total
    # inspection failure, vanished-mid-scan): they carry no authoritative facts and
    # must be surfaced as problems, not silently accepted.
    errored = sorted(r for r, e in recorded.items()
                     if e.get("error") or e.get("metadata_source") == "none")
    errored_set = set(errored)

    drift: list[str] = []        # recorded sha256 no longer matches file bytes
    unchecked: list[str] = []    # regenerable bucket, no sha256 → legitimately unverifiable
    no_checksum: list[str] = []  # NON-regenerable entry missing sha256 → integrity gap
    read_errors: list[str] = []
    for r in both:
        if r in errored_set:
            continue  # reported under ERRORS; no authoritative sha to compare
        e = recorded[r]
        want = e.get("sha256")
        if not want:
            # A missing sha256 is only OK for an explicitly regenerable output.
            # Anything else (roll frame, sample, non-regenerable/V0/NLP output)
            # should have been hashed — treat the gap as a problem.
            (unchecked if r in regenerable_files else no_checksum).append(r)
            continue
        ap_ = os.path.join(A, r)
        try:
            size = os.path.getsize(ap_)
            # Cheap byte-size pre-check before hashing hundreds of MB.
            if e.get("bytes") is not None and e["bytes"] != size:
                drift.append(r)
                continue
            if sha256(ap_) != want:
                drift.append(r)
        except OSError as ex:
            read_errors.append(f"{r} ({type(ex).__name__})")

    print(f"asset root: {A}")
    print(f"manifest:   {len(in_manifest)} entries · on disk: {len(on_disk_all)} files")

    def report(label: str, items: list[str]) -> None:
        if items:
            print(f"\n{label} ({len(items)}):")
            for it in items:
                print(f"  {it}")

    report("DRIFT (checksum changed — file edited in place)", drift)
    report("MISSING (in manifest, not on disk)", missing)
    report("ORPHANS (on disk, not in manifest)", orphans)
    report("ERRORS (entry carries an inspection error — no authoritative metadata)", errored)
    report("NO CHECKSUM (non-regenerable entry lacks sha256 — integrity unverifiable)",
           no_checksum)
    report("UNREADABLE (could not hash)", read_errors)
    if unchecked:
        print(f"\nunchecked: {len(unchecked)} regenerable output(s) without a recorded "
              "sha256 (drift not verifiable — regenerate to confirm)")

    problems = (drift or missing or orphans or errored or no_checksum or read_errors)
    if not problems:
        print("\nOK — no drift, orphans, or missing files.")
        print("(This tool only REPORTS. It never deletes; act on the above yourself.)")
        return 0
    print("\nReview the above and act deliberately — validate never deletes anything.")
    print("Re-run `nctool manifest generate` to fold new/renamed files into the manifest.")
    return 1


def cmd_roles(args) -> int:
    """Emit `roll|unexposed|leader|real1 real2 …` triples for the real-scan harness,
    sourced from the manifest. Only rolls with exactly one unexposed and one leader
    frame are emitted (a calibration pair the harness can freeze); all-real rolls
    like the NLP source are skipped."""
    A = _asset_root(args)
    mpath = os.path.join(A, "manifest.json")
    data, err = load_manifest(mpath)
    if err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    if not data:
        print(f"error: no manifest.json at {A}; run `nctool manifest generate` first",
              file=sys.stderr)
        return 2
    # Buffer emitted rows split by IR capability: the harness's `stage_ir` uses
    # ROLLS[0], so a roll that has an IR-capable real frame must sort before one
    # that has none (finding 4 — the per-roll IR-first sort below doesn't help
    # cross-roll if a non-IR roll sorts alphabetically first). Rows are appended in
    # alphabetical roll order, so each group stays alphabetically stable.
    ir_rows: list[str] = []
    non_ir_rows: list[str] = []
    for roll, r in sorted(data.get("rolls", {}).items()):
        by_role: dict[str, list[dict]] = {"unexposed": [], "leader": [], "real": []}
        for fr in r.get("frames", []):
            role = fr.get("role", "real")
            if role not in by_role:
                # An unknown/typo'd role must not silently vanish (the old
                # `setdefault` bucketed it into a phantom key, dropping a possible
                # calibration frame from every triple). Warn loudly and fold it
                # into `real` so the frame is still converted, never lost.
                print(f"WARNING: roll {roll!r} frame {os.path.basename(fr['file'])!r} has "
                      f"unrecognized role {role!r} (expected unexposed|leader|real); "
                      "treating it as 'real' so it is not silently dropped — fix the "
                      "role in manifest.json", file=sys.stderr)
                role = "real"
            by_role[role].append(fr)
        un, ld, reals = by_role["unexposed"], by_role["leader"], by_role["real"]
        if len(un) != 1 or len(ld) != 1:
            print(f"note: skipping roll {roll!r} (unexposed={len(un)}, leader={len(ld)}; "
                  "need exactly one of each to freeze a calibration recipe)",
                  file=sys.stderr)
            continue
        if not reals:
            # A calibration pair with no `real` frames has nothing for the harness
            # to convert; emitting `roll|un|ld|` (empty 4th field) would drive the
            # convert / ir / determinism stages over an empty frame list.
            print(f"note: skipping roll {roll!r} (unexposed+leader pair but no real "
                  "frames to convert)", file=sys.stderr)
            continue
        # Order IR-capable frames first so the harness's IR stage (which takes the
        # first real frame) gets an HDRi frame whenever the roll has one. The sort
        # is stable, so a roll whose real frames all share `ir_present` (the common
        # case) keeps its manifest order unchanged.
        reals.sort(key=lambda f: not f.get("ir_present", False))
        un_name, ld_name = os.path.basename(un[0]["file"]), os.path.basename(ld[0]["file"])
        real_basenames = [os.path.basename(f["file"]) for f in reals]
        # The output row is `|`-joined with a space-delimited real-frame field by
        # design; a basename containing whitespace would make that field ambiguous
        # (the harness splits it with `for fr in $reals`). The current naming has no
        # spaces, so rather than re-architect the contract, fail loudly.
        bad = [n for n in (un_name, ld_name, *real_basenames)
               if any(c.isspace() for c in n)]
        if bad:
            print(f"error: roll {roll!r} has frame name(s) containing whitespace, which "
                  f"would make the space-delimited roles output ambiguous: "
                  f"{', '.join(repr(n) for n in bad)}. Rename the file(s) in the assets "
                  "and regenerate the manifest.", file=sys.stderr)
            return 2
        row = f"{roll}|{un_name}|{ld_name}|{' '.join(real_basenames)}"
        (ir_rows if reals[0].get("ir_present", False) else non_ir_rows).append(row)
    for row in ir_rows + non_ir_rows:
        print(row)
    if not ir_rows and not non_ir_rows:
        print("error: no roll had a complete unexposed+leader calibration pair",
              file=sys.stderr)
        return 1
    return 0
