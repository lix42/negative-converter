# scripts/analysis (`nctool`) — agent notes

`README.md` is the reference (commands, venv, the test command). This file holds
only the traps it does not.

- **Run the gate with the venv**, since CI sets `NCTOOL_REQUIRE_DEPS=1` and a
  missing `numpy`/`tifffile` then fails instead of skipping:
  `NCTOOL_REQUIRE_DEPS=1 PYTHONPATH=scripts/analysis .venv/bin/python -m unittest discover -s scripts/analysis -p "test_*.py"`.
  No `.venv` in a fresh worktree: `uv venv --python 3.12 && uv pip install -r scripts/analysis/requirements.txt`.
  The harness test needs `target/debug/hanten` (`cargo build`; release alone is not enough).
- **A stale `__pycache__` can shadow an edit.** CPython validates a `.pyc` by mtime
  and size, so a same-length edit within the same second runs the old bytecode
  while tracebacks quote the new source. If a fix seems not to take, clear
  `__pycache__` or run with `PYTHONDONTWRITEBYTECODE=1` before trusting the failure.
- **`manifest.BANNERS` must keep both `hanten ` and `nc `.** `is_nc` identifies the
  binary by its `--version` banner, and the pre-rename reference build prints `nc`.
  Drop the old prefix and every command pointed at the reference build (`--nc`,
  `$NC`, a review matrix's `builds`) refuses it. Auto-discovery (`find_nc`) is
  deliberately `hanten`-only, so a stale `target/*/nc` is never picked up.
