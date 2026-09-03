"""nctool — the nc conversion-analysis toolkit.

The command groups are `manifest` (asset inventory), `compare` (the build-version
harness), `roll` (manifest-driven calibration, conversion, and deterministic
analysis artifacts), and `metrics` (pixel-derived measurement of one converted
image, whatever produced it).

All but `metrics` are stdlib-only, because they read derived JSON and stream
checksums rather than loading pixels into Python. `metrics` needs `numpy` and
`tifffile` (`scripts/analysis/requirements.txt`), imported inside the functions
that touch pixels so that importing this package stays free.
"""

__all__ = ["compare", "manifest", "metrics", "roll"]
