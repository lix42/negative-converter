"""nctool — the nc conversion-analysis toolkit (stdlib-only for now).

This is a **minimal seed**: only the `manifest` command group lands here (the
`asset-manifest` task). The full package skeleton — `metrics`, `thumbs`, a venv
with numpy/tifffile/Pillow — arrives with the downstream `conversion-metrics`
task. Keep this dependency-free so `python -m nctool manifest …` runs on the bare
system Python.
"""

__all__ = ["manifest"]
