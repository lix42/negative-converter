"""nctool — the nc conversion-analysis toolkit (stdlib-only for now).

Two command groups so far: `manifest` (the `asset-manifest` task) and `compare`
(the version-comparison harness of `core/conversion-versioning`). The rest of the
package skeleton — `metrics`, `thumbs`, a venv with numpy/tifffile/Pillow —
arrives with the downstream `conversion-metrics` task. Keep this **dependency-free**
so `python -m nctool …` runs on the bare system Python; `compare` in particular
must stay stdlib-only because it reads derived numbers out of JSON, never pixels.
"""

__all__ = ["compare", "manifest"]
