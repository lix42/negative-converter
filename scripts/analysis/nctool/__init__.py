"""nctool — the nc conversion-analysis toolkit (stdlib-only for now).

The command groups are `manifest` (asset inventory), `compare` (the build-version
harness), and `roll` (manifest-driven calibration, conversion, and deterministic
analysis artifacts). The future `metrics` / `thumbs` modules require image libraries; the
current commands stay dependency-free because they read derived JSON and stream
checksums rather than loading pixels into Python.
"""

__all__ = ["compare", "manifest", "roll"]
