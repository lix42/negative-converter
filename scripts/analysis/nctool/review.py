"""Render a described matrix of conversions into a review set for `tools/review-app`.

One entry point, and the matrix is **data**: a JSON file naming the configurations
and the flags each one passes, so changing what is compared never means editing
code. It replaces `scripts/preset-review/generate.py`, which stated its matrix as
a Python list.

Each cell of the matrix is one `hanten convert` of one frame with one configuration.
Beside the rendered image the generator writes that image's **metric record**
(`nctool metrics`), so the review app can draw the tone and cast charts next to
the picture instead of only showing the picture. The record is derived numbers
only — never pixels, never a photograph (CLAUDE.md).

A matrix may also declare a **build axis**: two or more pre-built binaries, each
named by the matrix and *identified* by what it reports about itself. The product
is flattened into cells called `<config>@<build>`, because the app's premise is one
grid cell per (frame, config) — a genuine second axis would mean a second toggle.

Five properties worth keeping:

* **Every config renders through the path being measured.** The matrix states one
  `output_preset` for the whole set, and both the file suffix and the colour space
  the metrics are read in come from *that name* rather than from a guess about the
  bytes.
* **A build's provenance is derived, never declared.** The matrix supplies a short
  name; the identity under it is read back off each render. A name a human typed is
  a claim, and a wrong claim about which binary made a cell is the one failure a
  build comparison cannot survive. A build may state an `expect_commit`, which is
  *checked* against that derived identity and aborts the run on a mismatch — an
  expectation, never a label.
* **A binary that reports two identities inside one run aborts it.** Not a build-axis
  rule: a matrix with no `builds` renders through one *unnamed* build — the `--nc`
  binary — and it is held to the same thing, which is the path most runs take. The
  abort also removes an earlier run's `review.json` from the output directory when
  this run landed on a cell that file indexes.
* **A cell that fails is reported and skipped**, never silently dropped: the other
  cells still make a reviewable page, and `review.json` simply carries no rendition
  for that config — which the app renders as a visible gap.
* **Output goes to a throwaway directory outside the repo.** The frames are the
  user's own photographs and are never committed; only the matrix is.
"""
from __future__ import annotations

import json
import re
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from . import manifest as _manifest
from . import metrics as _metrics

#: The matrix format this module reads. A future shape bumps it.
SCHEMA = 1

#: The `review.json` schema the app parses (`tools/review-app/SCHEMA.md`).
REVIEW_SCHEMA = 1

#: Suffix per output preset, mirroring `cli::derived_extension`.
#:
#: Not load-bearing in the dangerous direction: nc refuses an `-o` whose suffix
#: its resolved preset does not accept, so a stale entry here fails loudly at the
#: first render rather than writing a mislabelled file.
PRESET_SUFFIX: dict[str, str] = {
    "gain-map-hdr": "jpg",
    "ultra-hdr-v1": "jpg",
    "hdr-pq": "avif",
    "hdr-hlg": "avif",
    # Retired from nc; kept for a reference-build arm that still names them.
    "legacy": "tiff",
    "custom": "tiff",
    "film-master": "tiff",
    "display-p3": "tiff",
    "compatibility": "tiff",
    "hdr-linear-tiff": "tiff",
    "hdr-pq-tiff": "tiff",
    "hdr-hlg-tiff": "tiff",
}

#: The binary a matrix with no build axis renders through.
#:
#: The fallback lives here rather than as the `--nc` argparse default because the
#: flag has to be able to say "unset": it is refused beside a matrix that declares
#: `builds`, and a default would make every command line look like it passed one.
DEFAULT_NC = "target/release/hanten"

#: Ids that are safe to build a filename from, as `roll.py` spells it.
#:
#: Every id this checks becomes part of a filename: a cell writes
#: `<frame>-<config>.<suffix>` into the output directory, or
#: `<frame>-<config>@<build>.<suffix>` when the matrix declares a build axis. So an
#: id carrying a path separator would place it somewhere else entirely — `..` up
#: into the repository the output check just refused, or an absolute path that
#: discards the output directory altogether, while `review.json` goes on naming
#: the bare filename.
SAFE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def check_id(value: str, what: str, at: str) -> str:
    if not SAFE_ID.match(value) or ".." in value:
        # Deliberately says "part of", not the composition: this checks frame,
        # config *and* build ids, and naming one layout blames the wrong axis for
        # two of the three.
        raise ReviewError(
            f"{at}: {what} {value!r} is not filename-safe; every id becomes part "
            "of the filename its cell writes into the output directory")
    return value


#: Flags the generator supplies itself, refused in a matrix.
#:
#: `nc` takes the **last** occurrence of a `Set` argument, so a config restating one
#: would be silently overridden — and `--output-preset` decides the file suffix and
#: the colour space besides, so the override would not even be consistently ignored.
#: `--report-file` belongs here too: it sends the report to a file *instead of*
#: stdout, which is where the generator reads each cell's resolved recipe from —
#: so a matrix passing it would lose the measurement on every cell that rendered
#: perfectly well, and every cell would overwrite the same report path.
OWNED_FLAGS = ("--output-preset", "-o", "--output", "--report", "--report-file")

#: Placeholders a config's `args` may use, resolved per frame.
#:
#: `dmin` comes from the fixture declaration the metrics already use, so the two
#: cannot drift. (`film_stock` left with `--film-stock`, `nf-retire/characteristic`,
#: and so did the matrix's `rolls` block that stated it.)
PLACEHOLDERS = ("dmin",)


class ReviewError(Exception):
    """A malformed matrix, or a run that cannot produce a reviewable set."""


def _write_json(path: Path, value: dict) -> None:
    """Atomically write a JSON artifact into the output directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
        os.replace(tmp, path)
    except Exception:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise


def _load_object(path: Path, what: str) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReviewError(f"cannot read {what} {path}: {error}") from error
    if not isinstance(value, dict):
        raise ReviewError(f"{what} {path}: expected a JSON object")
    return value


def _string(value, at: str) -> str:
    if not isinstance(value, str) or not value:
        raise ReviewError(f"{at} must be a non-empty string")
    return value


def _optional_string(value, at: str) -> str | None:
    """An optional string, checked.

    Copied verbatim into `review.json`, where the app's own parser refuses a
    non-string — so an unchecked one here renders every cell and then makes the
    whole set unloadable, which is exactly the twenty-minutes-too-late failure
    this loader exists to prevent.
    """
    if value is None:
        return None
    return _string(value, at)


def _known_keys(record: dict, allowed: set[str], at: str) -> None:
    """Refuse a key this loader does not read.

    The `deny_unknown_fields` rule every recipe struct in this repo follows, and
    for the same reason: `"arg"` for `"args"` loads as *no* arguments, so that
    cell renders the default conversion under a label promising something else —
    five buttons, five labels, identical pixels, exit 0. `"insets"` for `"inset"`
    measures the whole frame, holder included.
    """
    unknown = sorted(set(record) - allowed)
    if unknown:
        raise ReviewError(
            f"{at}: unknown key{'s' if len(unknown) > 1 else ''} "
            f"{', '.join(unknown)}; known: {', '.join(sorted(allowed))}")


def _string_list(value, at: str) -> list[str]:
    if not isinstance(value, list):
        raise ReviewError(f"{at} must be an array of strings")
    return [_string(item, f"{at}[{index}]") for index, item in enumerate(value)]


#: The separator the generator joins a config id and a build id with.
#:
#: Deliberately not a hyphen: `SAFE_ID` admits hyphens *inside* an author's id, so
#: a hyphenated join would be ambiguous — and `colliding_stems` exists because that
#: ambiguity is already a real hazard on the `<frame>-<config>` side. `@` is outside
#: `SAFE_ID` altogether, so a composed id can only ever split one way; the join is
#: injective, which removes a collision class rather than adding one. It is
#: filename-safe on every platform this runs on, and the app hex-escapes it before
#: it reaches a DOM id (`tools/review-app/src/charts/domId.ts`).
BUILD_JOIN = "@"


def composed_id(config_id: str, build_id: str) -> str:
    return f"{config_id}{BUILD_JOIN}{build_id}"


def load_builds(value) -> list[dict]:
    """The binaries a matrix compares, or an empty list when it names none.

    A build states a **name and a path** — never an identity. What build a cell
    actually came from is read back off the render (`cell_identity`), because a
    matrix that could claim its own provenance could claim it wrongly, which is
    the one failure this axis exists to make impossible.
    """
    if value is None:
        return []
    if not isinstance(value, list) or not value:
        raise ReviewError("builds must list at least one build")
    builds: list[dict] = []
    seen: set[str] = set()
    for index, entry in enumerate(value):
        at = f"builds[{index}]"
        if not isinstance(entry, dict):
            raise ReviewError(f"{at} must be an object")
        _known_keys(entry, {"id", "label", "note", "nc", "expect_commit"}, at)
        bid = check_id(_string(entry.get("id"), f"{at}.id"), "build id", f"{at}.id")
        if bid in seen:
            raise ReviewError(f"builds contains two entries with id {bid!r}")
        seen.add(bid)
        builds.append({
            "id": bid,
            "label": _string(entry.get("label", bid), f"{at}.label"),
            "note": _optional_string(entry.get("note"), f"{at}.note"),
            # Resolved against the working directory, exactly as `--nc` is, so the
            # two spellings of "which binary" cannot mean different things.
            "nc": _string(entry.get("nc"), f"{at}.nc"),
            "expect_commit": expected_commit(entry.get("expect_commit"),
                                             f"{at}.expect_commit"),
        })
    return builds


COMMIT = re.compile(r"^[0-9a-f]{7,40}$")


def expected_commit(value, at: str) -> str | None:
    """A build's optional `expect_commit`: a hex commit, at least 7 digits.

    An **expectation, not a label.** The cell is still labelled from what the
    binary reports; this only lets a matrix refuse a run whose binary is not the
    commit it was meant to be — the reference arm pointed at the wrong file is
    otherwise rendered, labelled correctly, and never flagged.
    """
    if value is None:
        return None
    text = _string(value, at).lower()
    if not COMMIT.match(text):
        raise ReviewError(f"{at} must be a hex commit of 7 to 40 digits, not {value!r}")
    return text


VERSION_COMMIT = re.compile(r"^commit: ([0-9a-f]+)(-dirty| \(dirty unknown\))?$", re.M)


def banner_identity(banner: str) -> dict | None:
    """The commit and cleanliness a `--version` banner states, in report-identity shape.

    nc prints `commit: <hex>`, `<hex>-dirty` or `<hex> (dirty unknown)` — the same
    on both sides of the rename — so the pre-flight can hold a build to its
    `expect_commit` before a single cell renders.
    """
    match = VERSION_COMMIT.search(banner)
    if match is None:
        return None
    suffix = match.group(2)
    return {"git_commit": match.group(1),
            "git_dirty": None if suffix and "unknown" in suffix else bool(suffix)}


def commit_mismatch(expected: str, identity: dict | None) -> str | None:
    """Why a cell's identity is not a clean build of `expected`, or `None`.

    nc reports a 12-digit commit and the matrix may state more or fewer, so the two
    agree when the shorter is a prefix of the longer. A dirty build — or one whose
    dirtiness is unknown — is refused: its commit does not identify its source.
    """
    commit = (identity or {}).get("git_commit")
    if not isinstance(commit, str) or not commit:
        return "no commit"
    short, long = sorted((expected, commit.lower()), key=len)
    if not long.startswith(short):
        return f"commit {commit}"
    dirty = identity.get("git_dirty")
    if dirty is not False:
        return f"commit {commit} from a {'dirty' if dirty else 'possibly dirty'} tree"
    return None


def config_builds(value, builds: list[dict], at: str) -> list[str] | None:
    """The builds one config renders under — `None` meaning all of them.

    The opt-out is for the asymmetric pair: a flag the *other* build does not
    accept would otherwise render as a failed cell on every frame of the roll,
    which is noise rather than information.
    """
    if value is None:
        return None
    if not builds:
        raise ReviewError(f"{at} names a build, but the matrix declares no `builds`")
    wanted = _string_list(value, at)
    if not wanted:
        raise ReviewError(f"{at} must name at least one build")
    known = {build["id"] for build in builds}
    unknown = [name for name in wanted if name not in known]
    if unknown:
        raise ReviewError(
            f"{at}: no such build: {', '.join(unknown)} "
            f"(declared: {', '.join(build['id'] for build in builds)})")
    repeated = sorted({name for name in wanted if wanted.count(name) > 1})
    if repeated:
        raise ReviewError(f"{at} names {', '.join(repeated)} more than once")
    return wanted


def expand_configs(builds: list[dict], configs: list[dict]) -> list[dict]:
    """One entry per rendered cell: the build axis, flattened into config ids.

    The axis is expanded **here** rather than carried into `review.json` as a
    second dimension. The app's premise is that every rendition of a frame
    occupies one grid cell, so switching cannot move the picture by a pixel; a
    genuine second axis would mean a second toggle and a second thing to hold in
    your head while looking for a difference you can only see by toggling.

    Builds nest *inside* configs so the two arms of a before/after sit next to
    each other — `h`/`l` then steps between the cells being compared, which is
    the gesture the comparison is made of.

    A matrix that declares no builds passes through untouched, down to the ids:
    every set rendered before this existed re-renders byte-identically.
    """
    if not builds:
        return [{**config, "build": None} for config in configs]
    by_id = {build["id"]: build for build in builds}
    expanded: list[dict] = []
    for config in configs:
        for build_id in config["builds"] or list(by_id):
            build = by_id[build_id]
            expanded.append({
                **config,
                "id": composed_id(config["id"], build["id"]),
                "label": f"{config['label']} · {build['label']}",
                "build": build,
            })
    return expanded


def placeholders_in(args: list[str], at: str) -> set[str]:
    """The placeholder names one argument list uses, refusing unknown ones.

    Checked when the matrix is read rather than when a frame is rendered, so a
    typo is one error at the top instead of the same error once per frame — or,
    worse, a literal `{dmn}` handed to `nc` as a flag value.
    """
    used: set[str] = set()
    for arg in args:
        rest = arg
        while "{" in rest:
            head, _, rest = rest.partition("{")
            del head
            name, closed, rest = rest.partition("}")
            if not closed:
                raise ReviewError(f"{at}: unterminated placeholder in {arg!r}")
            if name not in PLACEHOLDERS:
                known = ", ".join(f"{{{p}}}" for p in PLACEHOLDERS)
                raise ReviewError(
                    f"{at}: unknown placeholder {{{name}}} in {arg!r}; known: {known}")
            used.add(name)
    return used


def reject_owned_flags(args: list[str], at: str) -> None:
    """Refuse a matrix that states a flag the generator supplies itself."""
    for arg in args:
        # `-o/tmp/x.jpg` is one token to clap, so splitting on `=` alone misses it —
        # and it would then beat the generator's own `-o`, writing the image
        # somewhere `review.json` does not name.
        name = arg[:2] if arg.startswith("-o") and not arg.startswith("--") else arg.split("=", 1)[0]
        if name in OWNED_FLAGS:
            raise ReviewError(
                f"{at}: {name} is set by the generator, not by the matrix"
                + (" — state it once as output_preset" if name == "--output-preset" else ""))


def expand_args(args: list[str], values: dict[str, str]) -> list[str]:
    """Substitute the per-frame values into one config's arguments."""
    out = []
    for arg in args:
        for name, value in values.items():
            arg = arg.replace("{" + name + "}", value)
        out.append(arg)
    return out


def load_matrix(path: Path) -> dict:
    """Read and validate a review matrix.

    Validation is deliberately front-loaded: a matrix error found after twenty
    minutes of rendering is a matrix error found too late.
    """
    raw = _load_object(path, "matrix")
    _known_keys(raw, {"schema_version", "title", "description", "output_dir",
                      "output_preset", "common_args", "frames", "metrics",
                      "builds", "configs"}, str(path))
    version = raw.get("schema_version")
    if version != SCHEMA:
        raise ReviewError(
            f"{path}: schema_version must be {SCHEMA}, got {version!r}")

    preset = _string(raw.get("output_preset"), "output_preset")
    if preset not in PRESET_SUFFIX:
        known = ", ".join(sorted(PRESET_SUFFIX))
        raise ReviewError(f"unknown output_preset {preset!r}; known: {known}")

    common = _string_list(raw.get("common_args", []), "common_args")
    placeholders_in(common, "common_args")
    reject_owned_flags(common, "common_args")

    builds = load_builds(raw.get("builds"))

    configs_raw = raw.get("configs")
    if not isinstance(configs_raw, list) or not configs_raw:
        raise ReviewError("configs must list at least one configuration")
    configs = []
    seen: set[str] = set()
    for index, entry in enumerate(configs_raw):
        at = f"configs[{index}]"
        if not isinstance(entry, dict):
            raise ReviewError(f"{at} must be an object")
        _known_keys(entry, {"id", "label", "note", "args", "builds"}, at)
        cid = check_id(_string(entry.get("id"), f"{at}.id"), "config id", f"{at}.id")
        if cid in seen:
            raise ReviewError(f"configs contains two entries with id {cid!r}")
        seen.add(cid)
        args = _string_list(entry.get("args", []), f"{at}.args")
        reject_owned_flags(args, f"{at}.args")
        configs.append({
            "id": cid,
            "label": _string(entry.get("label", cid), f"{at}.label"),
            "note": _optional_string(entry.get("note"), f"{at}.note"),
            "args": args,
            "needs": placeholders_in(args, at + ".args") | placeholders_in(common, at),
            "builds": config_builds(entry.get("builds"), builds, f"{at}.builds"),
        })

    frames = raw.get("frames")
    if frames is not None:
        frames = _string_list(frames, "frames")

    measure = raw.get("metrics", {})
    if not isinstance(measure, dict):
        raise ReviewError("metrics must be an object")
    _known_keys(measure, {"inset"}, "metrics")
    inset = measure.get("inset", 0.0)
    if not isinstance(inset, (int, float)) or not 0.0 <= inset < 0.5:
        raise ReviewError("metrics.inset must be a fraction in [0, 0.5)")

    return {
        "title": _optional_string(raw.get("title"), "title"),
        "description": _optional_string(raw.get("description"), "description"),
        "output_preset": preset,
        "suffix": PRESET_SUFFIX[preset],
        "common_args": common,
        "builds": builds,
        # **Expanded, not declared.** Everything downstream — the collision check,
        # the render loop, `review.json` — wants one entry per rendered cell, and
        # having exactly one place turn the declaration into that list is what
        # keeps the three from disagreeing about how many cells there are.
        "configs": expand_configs(builds, configs),
        "frames": frames,
        "inset": float(inset),
        "output_dir": _optional_string(raw.get("output_dir"), "output_dir"),
    }


def metrics_space(preset: str) -> tuple[str | None, str]:
    """Whether a preset's output can be measured at all, before anything renders.

    A pre-flight only, so a matrix whose output this toolkit cannot read (AVIF,
    PQ-encoded) says so once up front instead of once per cell. It is **not** the
    answer used to measure — see `cell_space`. Such a matrix still renders a page
    and is still perfectly reviewable by eye; it just has no charts.
    """
    if preset in _metrics.PRESET_UNREADABLE:
        return None, f"{preset} {_metrics.PRESET_UNREADABLE[preset]}"
    space = _metrics.PRESET_SPACES.get(preset)
    if space is None:
        return None, f"no verified colour space for {preset}"
    return space, ""


def cell_space(recipe: dict, expected_preset: str) -> tuple[str | None, str]:
    """The colour space one rendered cell is measured in, from its **own** recipe.

    Resolved per cell from what `nc` reports it resolved, not from the matrix's
    preset name, because the preset name does not determine the space: `legacy`
    and `custom` accept `--output-profile`, which a matrix is free to pass, and
    `space_for_recipe` is what maps it — and what refuses an `f32` output whose
    transfer this toolkit has not verified. Reading the name alone would measure
    ProPhoto pixels as sRGB and report every tone and cast number as if it were
    right, which is the plausible wrong answer the metrics module exists to
    refuse.

    The recipe must **state** the preset the matrix asked for. `space_for_recipe`
    defaults an unstated one to `gain-map-hdr`, so a report that could not be read
    would otherwise resolve Display P3 for a `film-master` render — linear ACEScg
    pixels measured as an encoded display space, every number wrong and every one
    of them plausible.
    """
    output = recipe.get("output") if isinstance(recipe.get("output"), dict) else {}
    preset = output.get("preset")
    if preset != expected_preset:
        return None, (
            f"the render reports output preset {preset!r}, not the matrix's "
            f"{expected_preset!r}; not measuring pixels this run cannot identify")
    try:
        space, _why = _metrics.space_for_recipe(recipe)
    except _metrics.MetricsError as error:
        return None, str(error)
    return space, ""


def _dimensions(record: dict) -> dict:
    """The rendition's pixel size, as `review.json` states it.

    Both or neither: the app refuses half a size, because half of one reserves no
    box. Absent when a cell is not measured, which the schema allows — the page
    then sizes itself from the image, as it did before this existed.
    """
    image = record.get("image") if isinstance(record.get("image"), dict) else {}
    width, height = image.get("width"), image.get("height")
    if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
        return {"width": width, "height": height}
    return {}


def rendition_stem(frame: str, config_id: str) -> str:
    return f"{frame}-{config_id}"


def colliding_stems(frames: list[str], config_ids: list[str]) -> list[str]:
    """Cells whose filenames would land on top of each other.

    `<frame>-<config>` is not injective when either id may contain a hyphen —
    and config ids here routinely do (`chr-generic`). Frame `a-b` with config `c`
    and frame `a` with config `b-c` both write `a-b-c`, so the second render
    overwrites the first while both `review.json` entries point at the surviving
    bytes: a comparison of one rendition with itself, under two labels. Cheaper to
    refuse than to encode around, since a set that trips it is misnamed anyway.
    """
    seen: dict[str, str] = {}
    clashes = []
    for frame in frames:
        for config_id in config_ids:
            stem = rendition_stem(frame, config_id)
            cell = f"{frame}/{config_id}"
            # Compared **case-folded**: the default macOS volume — and Windows —
            # treat `A-c.jpg` and `a-c.jpg` as one file, so an exact-string check
            # would pass while the second render silently replaced the first.
            key = stem.casefold()
            if key in seen:
                clashes.append(f"{seen[key]} and {cell} would both write {stem}")
            else:
                seen[key] = cell
    return clashes


def is_measured(record: dict, digest: str, region: dict, space: str,
                decoder: str | None = None) -> bool:
    """Whether a stored record already describes these exact bytes and region.

    Measuring a 74 MP frame is minutes of work, and a generator run re-renders
    every cell — so the check is against the rendered file's **checksum**, which
    the record already carries, rather than against an mtime a re-render always
    moves.

    The **declared space** is part of that identity, not a detail: the same file
    measured as sRGB and as Display P3 gives different tone and cast numbers, and
    `nctool metrics image --space …` beside the same image is a documented way to
    produce one. Reusing the wrong one charts a record this run did not mean.
    """
    if record.get("schema_version") != _metrics.SCHEMA:
        return False
    if record.get("sha256") != digest:
        return False
    declared = record.get("space") if isinstance(record.get("space"), dict) else {}
    if declared.get("declared") != space:
        return False
    # A JPEG's samples are whatever its decoder says they are — which is why the
    # record names the decoder at all. Reusing across a Pillow or libjpeg upgrade
    # mixes measurements from two decoders inside one set, in whichever cells
    # happened not to be re-rendered.
    image = record.get("image") if isinstance(record.get("image"), dict) else {}
    stored_decoder = image.get("decoder")
    if stored_decoder is not None and decoder is not None and stored_decoder != decoder:
        return False
    stored = record.get("region")
    if not isinstance(stored, dict):
        return False
    return all(stored.get(key) == region.get(key) for key in ("x", "y", "width", "height"))


#: The identity keys that belong to the **build** rather than to the run.
#:
#: `params_hash` is deliberately absent: it identifies the resolved recipe, which
#: differs from config to config by design, so including it would make every second
#: cell of a build look like a different binary.
BUILD_IDENTITY_KEYS = ("nc_version", "git_commit", "git_dirty", "pipeline_version",
                       "target")


def build_identity(identity) -> dict | None:
    """The build-identifying part of one run's `identity` block, or `None`."""
    if not isinstance(identity, dict):
        return None
    kept = {key: identity[key] for key in BUILD_IDENTITY_KEYS if key in identity}
    return kept or None


def cell_identity(report: dict, dest: Path) -> dict | None:
    """One cell's run identity — from the report, falling back to its sidecar.

    The two are the same value by construction (nc's `SidecarMeta` serializes the
    very `&Identity` the report carries, flattened into `meta`), and the report is
    already parsed here — so it is read first. The sidecar is a cheap second look
    rather than a known need: no binary has been shown to write one without the
    other, so the fallback is kept because it costs a line, not because a version
    requiring it has been established.

    `None` is an ordinary answer, not a failure: a build that identifies itself
    nowhere still renders pictures, and the run says once that it cannot label
    them.
    """
    identity = report.get("identity")
    if isinstance(identity, dict):
        return identity
    meta = _stored_record(dest.with_name(dest.name + ".json")).get("meta")
    return meta if isinstance(meta, dict) else None


def record_identity(identities: dict, build: dict, cell: str,
                    report: dict, dest: Path) -> str | None:
    """Remember what a build says it is, or report that it has changed its mind.

    The first cell of a build establishes its identity and every later cell must
    agree. Disagreement is a **run** fault, not a cell fault: if the binary at a
    path changed halfway through, every cell already rendered under that name is
    suspect too, so there is nothing to salvage by carrying on — which is why this
    returns a message for the caller to stop on rather than skipping the cell.

    This is also the check the task asks for as "a cell whose sidecar disagrees
    with the matrix's claimed build is flagged". The matrix claims no identity, so
    the only thing a cell can disagree with is the rest of its own build — unless
    the build states an `expect_commit`, which every cell must then satisfy. That
    is a run fault for the same reason: the binary at that path is the wrong one.
    """
    identity = build_identity(cell_identity(report, dest))
    expected = build.get("expect_commit")
    if expected and (mismatch := commit_mismatch(expected, identity)):
        return (f"build {build['id']!r} ({build['nc']}) expects a clean build of "
                f"{expected}, but {cell} reports {mismatch}; its cells would carry "
                "the build's name without being that build")
    if identity is None:
        return None
    known = identities.get(build["id"])
    if known is None:
        identities[build["id"]] = identity
        return None
    if known == identity:
        return None
    # A matrix with no build axis renders through one *unnamed* build, so there is
    # no id to print — the same guard `resolve_binaries` uses. Naming it anyway
    # reads `build None`, which is the path most runs take.
    named = (f"build {build['id']!r} ({build['nc']})" if build["id"]
             else f"the binary {build['nc']}")
    return (f"{named} reported one identity and then "
            f"another: {json.dumps(known, sort_keys=True)} before {cell}, "
            f"{json.dumps(identity, sort_keys=True)} at it. The binary changed "
            "under the run, so every cell rendered under it is suspect")


def parse_build_overrides(values: list[str] | None) -> dict[str, str]:
    """`--build <id>=<path>`, the one thing about a build a command line may move.

    A build's *name* is the matrix's, because it is what the review set is labelled
    by; its path is worth overriding, because the daily loop of a default move
    rebuilds one arm over and over and should not mean editing a committed matrix.

    This runs **before** the output-directory check, unlike the two rules in
    `check_build_arguments` that run after it. Deliberate, and the distinction is
    parse versus contradiction: `--build justanid` is not a request this tool can
    hold two readings of, so there is nothing for a more specific fault to be more
    specific *than*. `--build typo=/x` is well-formed and merely names a build the
    matrix does not have, which the destination outranks.
    """
    overrides: dict[str, str] = {}
    for value in values or []:
        build_id, sep, path = value.partition("=")
        if not sep or not build_id or not path:
            raise ReviewError(
                f"--build {value!r} must be <build id>=<path to a hanten binary>")
        if build_id in overrides:
            raise ReviewError(f"--build names {build_id!r} twice")
        overrides[build_id] = path
    return overrides


def duplicate_binary_warnings(resolved: list[dict]) -> list[str]:
    """Builds that are, byte for byte, the same file.

    A **warning and not an error**, because the task's own acceptance check is
    that the same binary declared twice yields byte-identical cells — a generator
    that refused it would refuse its own probe. The digest is also the only fact
    here that can tell two builds apart with certainty; two dirty builds of one
    commit report the same identity and differ only in these bytes.
    """
    by_digest: dict[str, list[str]] = {}
    for build in resolved:
        by_digest.setdefault(build["digest"], []).append(build["id"])
    return [f"builds {' and '.join(ids)} are the same binary "
            f"(sha256 {digest[:12]}); their cells will be byte-identical"
            for digest, ids in by_digest.items() if len(ids) > 1]


def indistinguishable_identity_warnings(resolved: list[dict]) -> list[str]:
    """Different binaries that describe themselves identically.

    The realistic case is two dirty builds of one commit — exactly the shape a
    patched-versus-shipped spike takes. It cannot be called an error: the binaries
    really do differ. What it can be is said out loud, so nobody reads the labels
    as provenance they are not.
    """
    by_identity: dict[str, list[dict]] = {}
    for build in resolved:
        identity = build.get("identity")
        if identity:
            by_identity.setdefault(json.dumps(identity, sort_keys=True), []).append(build)
    warnings = []
    for identity, group in by_identity.items():
        # Builds that are the same *file* are already reported by
        # `duplicate_binary_warnings`, and saying they are "different binaries" here
        # too would be both noise and false.
        if len({build["digest"] for build in group}) < 2:
            continue
        warnings.append(
            f"builds {' and '.join(build['id'] for build in group)} are different "
            f"binaries that report the same identity ({identity}); the labels "
            "cannot tell them apart")
    return warnings


def check_build_arguments(builds: list[dict], overrides: dict[str, str],
                          nc: str | None) -> None:
    """The two ways a command line can contradict the matrix about which binary.

    Kept out of `resolve_binaries` because neither reads the filesystem: they are
    faults in **what was asked**, and diagnosing them behind the frame list or the
    binary pre-flight blames whatever else happens to be wrong first — `--nc`
    beside a `builds` matrix used to be reported as "no such frame" whenever the
    same line also mistyped one.

    Both run **after** the output directory is checked and **before** the frames
    are resolved: where the pictures land outranks which binary makes them, and a
    frame the fixtures do not declare is the coarser of the remaining diagnoses.
    `parse_build_overrides` is the exception and states why — a malformed `--build`
    is a parse, not a contradiction, so it is settled before any of this.
    Ordered within itself too — `--nc` beside `builds` is a contradiction between
    the only two things that name a binary, so it is diagnosed before an override
    that merely names a build the matrix does not have.
    """
    if builds and nc is not None:
        raise ReviewError(
            "--nc names one binary, but the matrix declares `builds`; repoint a "
            "build with --build <id>=<path>")
    unknown = sorted(set(overrides) - {build["id"] for build in builds})
    if unknown:
        raise ReviewError(
            f"--build names no such build: {', '.join(unknown)}"
            + (f" (declared: {', '.join(build['id'] for build in builds)})" if builds
               else "; the matrix declares no `builds`"))


def resolve_binaries(builds: list[dict], overrides: dict[str, str],
                     nc: str | None) -> list[dict]:
    """Verify every build's binary before a single cell renders.

    Three facts are established here rather than discovered mid-run: the path
    exists, it really is this project's CLI, and its sha256. What the *command
    line* said is already settled by `check_build_arguments`, which runs earlier.

    The second check goes through `manifest.is_nc`, which accepts the **pre-rename
    `nc` banner** as well as `hanten`. That is load-bearing rather than tidy: the
    reference build this axis exists to compare against (`reserve`, from tag
    `pre-new-flow`) predates the rename and prints `nc`.
    """
    # A matrix with no build axis is one unnamed build — the `--nc` binary — so the
    # render loop has a single shape rather than two.
    declared = builds or [{"id": None, "label": None, "note": None,
                           "nc": nc or DEFAULT_NC}]
    resolved = []
    for build in declared:
        # Resolved, because `Path("./fakenc")` normalises to a bare name that
        # `is_file()` accepts and `subprocess` then looks up on PATH instead.
        path = Path(overrides.get(build["id"], build["nc"])).resolve()
        named = f"build {build['id']!r}: " if build["id"] else ""
        if not path.is_file():
            raise ReviewError(f"{named}no hanten binary at {path}; "
                              "`cargo build --release` first")
        if not _manifest.is_nc(str(path)):
            raise ReviewError(
                f"{named}{path} is not this project's CLI (its `--version` reported "
                "neither `hanten <ver>` nor the pre-rename `nc <ver>`)")
        # Checked here as well as per cell: refusing now writes nothing, where the
        # render-time check has already rendered over the cell it refuses.
        if build.get("expect_commit"):
            banner = subprocess.run([str(path), "--version"], capture_output=True,
                                    text=True).stdout
            mismatch = commit_mismatch(build["expect_commit"], banner_identity(banner))
            if mismatch:
                raise ReviewError(f"{named}{path} is not a clean build of "
                                  f"{build['expect_commit']} (`--version` reports "
                                  f"{mismatch})")
        resolved.append({**build, "nc": str(path), "digest": _manifest.sha256(str(path))})
    return resolved


def producer_block(build: dict | None, identity: dict | None) -> dict | None:
    """What a config's `producer` block says in `review.json`.

    Tagged by `kind` so `analysis/review-reference-cells` adds a variant rather
    than inventing a second block: an outside producer's cell asks the same
    question this one does — which cell did nc-as-configured *not* render — and
    answering it in two formats would be the mistake.

    The identity is **derived** from what the binary reported. The matrix names a
    build; it does not get to say what that build is.
    """
    if build is None or build["id"] is None:
        return None
    return {
        "kind": "hanten",
        "label": build["label"],
        **({"note": build["note"]} if build["note"] else {}),
        # Filtered **here**, beside the spread that would otherwise carry an
        # unexpected key straight into `review.json`. Callers pass
        # `build_identity`'s output already, and it is idempotent — so this costs
        # nothing and keeps the key set defined in one place rather than two
        # functions apart.
        **(build_identity(identity) or {}),
    }


def build_review(matrix: dict, images: list[dict],
                 identities: dict[str, dict] | None = None) -> dict:
    """The `review.json` document for a finished run.

    `identities` maps a build id to what that build reported about itself, so the
    `producer` block each config carries is derived rather than declared. It is
    additive and optional, which is why `REVIEW_SCHEMA` stays 1: a matrix with no
    build axis produces the same document it always did, byte for byte.
    """
    identities = identities or {}
    return {
        "schema_version": REVIEW_SCHEMA,
        **({"title": matrix["title"]} if matrix["title"] else {}),
        **({"description": matrix["description"]} if matrix["description"] else {}),
        "configs": [
            {"id": c["id"], "label": c["label"],
             **({"note": c["note"]} if c["note"] else {}),
             **({"producer": producer}
                if (producer := producer_block(c.get("build"),
                                               identities.get((c.get("build") or {})
                                                              .get("id")))) else {})}
            for c in matrix["configs"]
        ],
        "images": images,
    }


def dest_key(path: Path) -> str:
    """The identity two spellings of one destination file share.

    **Case-folded**, for the reason `colliding_stems` gives: the default macOS
    volume and Windows treat `F1-Dflt.jpg` and `F1-dflt.jpg` as one file, so an
    exact-string compare says two runs touched different cells while the second
    render sat on top of the first. Over-matching on a case-sensitive volume is
    the safe direction here — it deletes a stale index that was arguably still
    true, which is what this check did for everything before it existed.
    """
    return str(path).casefold()


def indexed_sources(review: dict, base: Path) -> tuple[set[str], bool]:
    """Where a written `review.json` points, and whether all of it was read.

    Read back off an *earlier* run's set to decide whether this run's renders
    landed on anything it indexes. Returns `dest_key`s resolved against `base`
    (the directory the set lives in, which is what `src` is relative to —
    `tools/review-app/SCHEMA.md`), so a bare `F1-a.jpg`, a `./F1-a.jpg` and a
    `renders/../F1-a.jpg` are one answer rather than three.

    The second value is the one that matters for what the caller may *claim*.
    "I recognised nothing" and "there was nothing" are different facts, and only
    the second licenses telling someone their set still describes its own pixels
    — so anything this could not read (an unparseable file, a shape the schema
    does not have, a path that will not resolve) drops it to `False`.

    Both rendition shapes are read. The bare **string** is the one SCHEMA.md
    calls the common case; seeing only the object form made the headline spelling
    invisible to the whole check.
    """
    complete = True
    sources: set[str] = set()
    images = review.get("images")
    if not isinstance(images, list):
        return sources, False
    for image in images:
        renditions = image.get("renditions") if isinstance(image, dict) else None
        if not isinstance(renditions, dict):
            complete = False
            continue
        for rendition in renditions.values():
            if isinstance(rendition, str):
                src = rendition
            elif isinstance(rendition, dict) and isinstance(rendition.get("src"), str):
                src = rendition["src"]
            else:
                complete = False
                continue
            try:
                sources.add(dest_key((base / src).resolve()))
            except (OSError, ValueError):
                complete = False
    return sources, complete


def name_some(names: list[str], limit: int = 5) -> str:
    """A few names for an error line, not all of them.

    A set drifting late — five configs over fourteen frames — has seventy
    destinations, and inlining them all buries the sentence that says what went
    wrong. The count is stated separately, so this only has to be enough to
    recognise the set by.
    """
    if len(names) <= limit:
        return ", ".join(names)
    return ", ".join(names[:limit]) + f", and {len(names) - limit} more"


def _frames_to_render(matrix: dict, fixtures: dict,
                      requested: str | None) -> list[str]:
    known = list(fixtures.get("frames", {}))
    if requested:
        names = [name.strip() for name in requested.split(",") if name.strip()]
    elif matrix["frames"]:
        names = list(matrix["frames"])
    else:
        names = known
    seen: set[str] = set()
    repeated = sorted({name for name in names if name in seen or seen.add(name)})
    if repeated:
        # Two entries for one frame render every cell twice and write two images
        # with the same id, which the app refuses — after the expensive part.
        raise ReviewError(f"frame named more than once: {', '.join(repeated)}")
    for name in names:
        check_id(name, "frame", "frames")
    missing = [name for name in names if name not in known]
    if missing:
        raise ReviewError(
            f"no such frame in the fixtures: {', '.join(missing)} "
            f"(known: {', '.join(known)})")
    if not names:
        raise ReviewError("no frames to render")
    return names


def _render(nc: Path, source: Path, dest: Path, args: list[str]) -> dict:
    """Run one conversion, returning nc's report or raising with its message."""
    cmd = [str(nc), "convert", str(source), "-o", str(dest), "--report", "json", *args]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode:
        tail = result.stderr.strip().splitlines()
        message = tail[-1][:200] if tail else "no message"
        raise ReviewError(f"nc exited {result.returncode}: {message}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return {}


def _cast_note(report: dict) -> str | None:
    """The one-line G/R B/R summary the page shows beside the heading.

    Only for a **positive** red mean, which is not the same as a non-zero one:
    the unclamped-float presets (`film-master`, `hdr-linear-tiff`) can report a
    negative mean, and dividing by it prints sign-flipped ratios rather than no
    ratio at all.
    """
    mean = report.get("output_stats", {}).get("mean")
    if not isinstance(mean, list) or len(mean) != 3:
        return None
    if not isinstance(mean[0], (int, float)) or mean[0] <= 0:
        return None
    return f"G/R {mean[1] / mean[0]:.3f} B/R {mean[2] / mean[0]:.3f}"


def cmd_generate(args) -> int:
    """Render a matrix and write the review set."""
    try:
        matrix = load_matrix(Path(args.matrix))
        overrides = parse_build_overrides(args.build)

        # **What was asked is checked before what is installed**, and the gates
        # down to the collision check are the "asked" half: the output directory,
        # the two `--nc`/`--build` contradictions, the fixtures file, the frames
        # it declares, and their filename collisions. The fixtures read is the one
        # filesystem touch among them — it is here because a frame list cannot be
        # checked without it, and the file is itself something the command line
        # named. An unbuildable environment is the less specific fault — telling
        # someone to build the binary when their real problem is `--out .`, a
        # mistyped frame or a `--nc` the matrix has no room for costs them a round
        # trip and then says something else.
        out = Path(args.out or matrix["output_dir"] or "../temp/review").resolve()
        # The frames are the user's own photographs and are never committed, so
        # the one destination this refuses is the repository itself (CLAUDE.md).
        # It is refused **before** anything about `--build`, deliberately: where
        # the pictures land outranks which binary makes them.
        repo = Path(__file__).resolve().parents[3]
        if out == repo or repo in out.parents:
            raise ReviewError(
                f"{out} is inside the repository ({repo}); a review set is rendered "
                "photographs and must go to a throwaway directory outside it")

        check_build_arguments(matrix["builds"], overrides, args.nc)

        fixtures = _load_object(Path(args.fixtures), "fixtures")
        frames = _frames_to_render(matrix, fixtures, args.frames)
        clashes = colliding_stems(frames, [c["id"] for c in matrix["configs"]])
        if clashes:
            raise ReviewError("; ".join(clashes))

        # From here on the faults are environmental.
        builds = resolve_binaries(matrix["builds"], overrides, args.nc)
        assets = Path(args.asset_root).resolve()
        if not (assets / "manifest.json").is_file():
            raise ReviewError(f"no assets at {assets}")

        out.mkdir(parents=True, exist_ok=True)
    except ReviewError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    # Said up front, before twenty minutes of rendering: a set whose two arms are
    # the same file is still worth having (it is how byte-identity is checked), but
    # it must not be mistaken for a comparison.
    for build in builds:
        if build["id"]:
            print(f"build {build['id']:8} {build['nc']} "
                  f"(sha256 {build['digest'][:12]})", file=sys.stderr)
    for warning in duplicate_binary_warnings(builds):
        print(f"note: {warning}", file=sys.stderr)

    readable, why_not = metrics_space(matrix["output_preset"])
    measuring = readable is not None and not args.no_metrics
    if readable is None and not args.no_metrics:
        print(f"note: no metrics — {why_not}", file=sys.stderr)
    if measuring:
        # Asked **once**, before anything renders. Measuring is the only part of
        # this toolkit that is not stdlib-only, and a fresh checkout has no venv
        # (it is gitignored) — so without this the setup instructions would print
        # once per cell, seventy-odd lines of it, after every render had already
        # run. The page is still worth having without charts.
        try:
            _metrics.require_dependencies()
        except _metrics.MetricsError as error:
            print(f"note: no metrics — {error}", file=sys.stderr)
            measuring = False

    if matrix["suffix"] not in ("jpg", "avif"):
        print(f"note: {matrix['output_preset']} writes {matrix['suffix'].upper()}, which most "
              "browsers do not display in an <img> (Safari does); the set will render but "
              "most of it will show as broken images", file=sys.stderr)

    fraction = _metrics.inset_fraction(matrix["inset"]) if matrix["inset"] else (
        0.0, 0.0, 1.0, 1.0)

    images: list[dict] = []
    failures: list[str] = []
    by_build = {build["id"]: build for build in builds}
    identities: dict = {}
    drift: str | None = None
    # The destinations this run may have put bytes into: `dest_key` -> the bare
    # filename the message names it by. "May have", not "did" — see the render
    # loop; only these can have made an earlier run's `review.json` describe
    # pixels it did not index, and the abort below reads it.
    written: dict[str, str] = {}
    for key in frames:
        frame = fixtures["frames"][key]
        roll = frame["roll"]
        source = assets / "rolls" / roll / frame["file"]
        if not source.is_file():
            print(f"{key}: {source} missing, skipped", file=sys.stderr)
            continue
        roll_fixture = fixtures.get("rolls", {}).get(roll, {})
        values = {
            "dmin": ",".join(str(c) for c in roll_fixture.get("dmin", [])),
        }

        renditions: dict[str, object] = {}
        notes: list[str] = []
        for config in matrix["configs"]:
            cell = f"{key}/{config['id']}"
            # A config needing a value this roll does not state loses only its own
            # cell. That is the difference between a stock with no digitized sheet
            # costing one column and costing the whole frame.
            unmet = [name for name in config["needs"] if not values.get(name)]
            if unmet:
                print(f"{cell}: roll {roll} states no {', '.join(unmet)}, skipped",
                      file=sys.stderr)
                failures.append(cell)
                continue

            build = by_build[(config.get("build") or {}).get("id")]
            dest = out / f"{rendition_stem(key, config['id'])}.{matrix['suffix']}"
            # Recorded **before** the render, not after it succeeds: a `hanten`
            # that exits nonzero may already have written this file. `--strict`
            # gates *after* encoding, so a frame carrying the IR warning writes
            # the image and its sidecar and then exits 1 — measured against the
            # release binary, and `--strict` is not an owned flag, so an ordinary
            # config may pass it. Booking it here makes a render that failed
            # before writing anything over-match, which is the same trade
            # `dest_key` makes for case: over-matching only deletes a stale index
            # that was arguably still true, while a miss leaves a lie in place and
            # vouches for it.
            written[dest_key(dest.resolve())] = dest.name
            try:
                report = _render(Path(build["nc"]), source, dest,
                                 expand_args(matrix["common_args"] + config["args"], values)
                                 + ["--output-preset", matrix["output_preset"]])
            except ReviewError as error:
                print(f"{cell}: {error}", file=sys.stderr)
                failures.append(cell)
                continue

            drift = record_identity(identities, build, cell, report, dest)
            if drift:
                break

            rendition: dict[str, object] = {"src": dest.name}
            if measuring:
                # The space this cell is measured in comes from the recipe `nc`
                # reports it resolved — provenance, not the matrix's preset name.
                space, why = cell_space(report.get("recipe", {}), matrix["output_preset"])
                record_path = dest.with_name(dest.name + ".metrics.json")
                if space is None:
                    print(f"{cell}: metrics skipped — {why}", file=sys.stderr)
                else:
                    try:
                        record = _measure(dest, record_path, space, fraction, args.force)
                    except ReviewError as error:
                        print(f"{cell}: metrics skipped — {error}", file=sys.stderr)
                    else:
                        rendition["metrics"] = record_path.name
                        rendition.update(_dimensions(record))
            renditions[config["id"]] = rendition

            note = _cast_note(report)
            if note:
                notes.append(f"{config['id']} {note}")
            print(f"{key:4} {config['id']:12} -> {dest.name}", file=sys.stderr)

        if drift:
            break

        # Kept even when every cell failed. The set is the record of what was
        # compared, and a frame that silently vanishes tells a later reader
        # nothing; an empty rendition map draws the gaps the app already has.
        images.append({
            "id": key,
            "label": f"{key} — {roll} · {frame['file']}",
            **({"note": " · ".join(notes)} if notes else {}),
            "renditions": renditions,
        })

    if drift:
        # **Removing an earlier run's `review.json` is part of the abort — but only
        # when this run actually overwrote a cell it names.** This run writes no
        # set, and the cells it had already re-rendered went into place on top of
        # the old ones, so a set left by a previous run would go on indexing new
        # pixels under the old build's identity; the app *watches* the set, so a
        # page already open refreshes onto exactly that. That is the lie this check
        # exists to catch, and the rebuild-into-the-same-directory loop `--build`
        # invites. A run that landed on none of its cells — different frames,
        # different configs — told it no lie, so it is left alone and said so:
        # deleting it would destroy a set that is still true. But "landed on none
        # of them" has to be *established*, not inferred from an empty answer: a
        # set this cannot read indexes nothing as far as the compare is concerned,
        # and the difference between recognising nothing and there being nothing
        # is the difference between hedging and vouching. Hence three outcomes,
        # not two.
        stale = out / "review.json"
        if not stale.is_file():
            outcome = ""
        else:
            indexed, complete = indexed_sources(_stored_record(stale), out)
            overwritten = sorted(name for key, name in written.items()
                                 if key in indexed)
            if overwritten:
                named = name_some(overwritten)
                try:
                    stale.unlink()
                    outcome = (f"; removed the stale {stale}, which names "
                               f"{len(overwritten)} cell(s) this run had already "
                               f"overwritten ({named})")
                except FileNotFoundError:
                    outcome = ""
                except OSError as error:
                    outcome = (f"; could NOT remove the stale {stale} ({error}) — it "
                               f"names {len(overwritten)} cell(s) this run overwrote "
                               f"({named}), so delete it by hand")
            elif complete:
                outcome = (f"; left the earlier {stale} alone — this run overwrote "
                           "none of the cells it names, so it still describes its "
                           "own pixels")
            else:
                # Not the same statement, and the difference is the whole point:
                # an empty intersection here means *nothing was recognised*, which
                # is no evidence at all. Leaving the file is still right — the app
                # refuses a set it cannot parse, loudly — but the run must not
                # vouch for it. Worded to share no phrase with the branch above, so
                # that a test asserting the other one is absent can tell them apart.
                outcome = (f"; left the earlier {stale} in place, but could not read "
                           "every rendition it names — whether this run landed on "
                           "one of its cells is unchecked; delete it by hand if a "
                           "page is open on this set")
        print(f"error: {drift}. No review.json was written{outcome}", file=sys.stderr)
        return 1

    if not images:
        print("error: no frame could be read — no review set written", file=sys.stderr)
        return 1

    # Only knowable now: identity is read off a render, so two builds that describe
    # themselves identically cannot be spotted before at least one cell of each has
    # run.
    for warning in indistinguishable_identity_warnings(
            [{**build, "identity": identities.get(build["id"])}
             for build in builds if build["id"]]):
        print(f"note: {warning}", file=sys.stderr)

    _write_json(out / "review.json", build_review(matrix, images, identities))
    rendered = sum(len(image["renditions"]) for image in images)
    print(f"\n{len(images)} frames x {len(matrix['configs'])} configs -> "
          f"{out}/review.json", file=sys.stderr)
    if failures:
        print(f"FAILED cells ({len(failures)}): {', '.join(failures)}", file=sys.stderr)
    print(f"\n  cd tools/review-app && pnpm dev {out}/review.json", file=sys.stderr)
    # The set is written either way — it is the record of what was attempted —
    # but a run that rendered nothing did not produce a comparison, and says so
    # with its exit status rather than only in the lines above.
    return 0 if rendered else 1


def _stored_record(path: Path) -> dict:
    """A previously written record, or an empty one when it cannot be read."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def _measure(image: Path, record_path: Path, space: str,
             fraction: tuple[float, float, float, float], force: bool) -> dict:
    """Measure one rendered image, reusing a record that already describes it.

    Returns the record — freshly measured or the reused one — because it is also
    the only place the rendition's pixel dimensions are known: `nc`'s report does
    not carry them, and the review file wants them so the page can reserve the
    right box before the image loads. A measurement that cannot run (no numpy, an
    unreadable container) raises instead, so the caller can say *why* the charts
    will be missing.
    """
    try:
        _metrics.require_dependencies()
    except _metrics.MetricsError as error:
        raise ReviewError(str(error)) from error

    # Only JPEG records name a decoder, and `_jpeg_decoder_identity` imports
    # Pillow, so it is asked for once per cell rather than at import time.
    decoder = _metrics._jpeg_decoder_identity() if _metrics._is_jpeg(image) else None
    digest = _metrics.sha256(image)
    if not force and record_path.is_file():
        stored = _stored_record(record_path)
        size = stored.get("image") if isinstance(stored.get("image"), dict) else {}
        width, height = size.get("width"), size.get("height")
        if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
            region = _metrics.resolve_region(width, height, fraction)
            if is_measured(stored, digest, region, space, decoder):
                return stored

    try:
        record = _metrics.measure(image, space, fraction, digest=True)
    except _metrics.MetricsError as error:
        raise ReviewError(str(error)) from error
    _write_json(record_path, record)
    return record
