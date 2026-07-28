//! Build script: capture build-time identity into env vars the crate can read at
//! runtime — the compile target triple plus the git commit the binary was built
//! from.
//!
//! Cargo sets `TARGET` for build scripts but not for the crate compile itself, so
//! the telemetry record's `target` field (design-spec §9) would otherwise be
//! unavailable without a dependency. Re-exporting it as `NC_TARGET` keeps that
//! field dependency-free (`env!("NC_TARGET")`).
//!
//! `NC_GIT_COMMIT` / `NC_GIT_DIRTY` extend the same mechanism for
//! `core/conversion-versioning`'s build identity (report `identity.git_commit` /
//! `git_dirty`, and `nc --version`). Both are **fail-soft**: a source tarball with
//! no `.git`, a machine with no `git` on `PATH`, or a checkout whose enclosing
//! repository isn't nc's yields `"unknown"` rather than failing the build — build
//! metadata is provenance, not correctness. Real errors (a failing compile) still
//! fail loudly; only the metadata degrades.
//!
//! **It must be nc's own repository.** `git rev-parse` walks *up* from the package
//! directory, so a copy of this crate sitting inside an unrelated checkout
//! (`cargo vendor`, a path-dependency checkout, a tarball unpacked under a
//! dotfiles/`$HOME` repo) would otherwise stamp *that* repo's commit — wrong
//! provenance presented as a valid, possibly `dirty: false` hash, with nothing to
//! notice. [`git_identity`] therefore requires `rev-parse --show-toplevel` to be
//! the package directory itself and reports `"unknown"` otherwise. (A future
//! workspace layout that puts this crate in a subdirectory would trip that check
//! and degrade to `"unknown"`; the check would then need to compare against the
//! workspace root instead.)
//!
//! **Freshness.** The captured commit and dirty flag are only as fresh as the last
//! time Cargo re-ran this script, so the rules are named explicitly
//! ([`emit_rerun_rules`]) rather than left to Cargo's default. That default —
//! re-run whenever *any* package file changes — keeps the *dirty* flag honest
//! while sources are edited but never notices a **commit**: no commit touches a
//! package file, so the ordinary edit → build → commit → run-the-binary path would
//! leave the binary reporting the **parent** commit *and* `dirty: true` on a
//! now-clean tree, indistinguishable from the pre-commit build that emitted the
//! same `true`. Naming the source directories restores the dirty-flag freshness,
//! and naming git's `HEAD`, `index`, `refs` and `packed-refs` closes the commit gap
//! — `HEAD` alone does not, because on a branch its contents do not change when you
//! commit (see [`emit_rerun_rules`]).
//!
//! Two things the rules deliberately do not cover, because Cargo's directory scan
//! walks a named directory recursively and naming the package root would sweep in
//! `target/` (changed by every build ⇒ a re-run every build): a **new untracked
//! file** outside the named directories, and edits to files in directories not
//! listed. Both can leave `git_dirty` stale until the next watched-path change;
//! `cargo clean -p nc` forces a re-read.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Package-relative paths whose contents feed the dirty flag. Cargo scans a named
/// directory recursively, so these cover the tracked tree without naming the
/// package root (which would include `target/`).
const WATCHED: &[&str] = &[
    "src",
    "tests",
    "docs",
    "scripts",
    ".github",
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
];

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=NC_TARGET={target}");

    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let ours = is_nc_repository(&dir);
    emit_rerun_rules(&dir, ours);

    let (commit, dirty) = git_identity(&dir, ours);
    println!("cargo:rustc-env=NC_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=NC_GIT_DIRTY={dirty}");
}

/// Tell Cargo when to re-run this script (see the freshness note in the module
/// doc). Naming any path replaces Cargo's default any-package-file rule, so the
/// source directories are listed explicitly and the git refs are added on top.
fn emit_rerun_rules(dir: &str, ours: bool) {
    for rel in WATCHED {
        println!("cargo:rerun-if-changed={rel}");
    }
    if !ours {
        return;
    }
    // Two traps here, both verified empirically rather than assumed:
    //
    // 1. `.git` is a **file** (a `gitdir:` pointer) inside a linked worktree, so the
    //    literal paths `.git/HEAD` / `.git/index` do NOT exist there — and linked
    //    worktrees are how this repo's feature branches are developed.
    //    `rev-parse --git-path` resolves both layouts, so ask git.
    // 2. Watching `HEAD` alone does **not** notice a commit. On a branch, `HEAD` is
    //    a `ref: refs/heads/<branch>` pointer whose *contents* never change when you
    //    commit; the branch ref does. Measured: with only `HEAD` + `index` watched, a
    //    plain `cargo build` after `git commit` still reported the parent commit.
    //    So `refs` (scanned recursively by Cargo, catching loose-ref writes) and
    //    `packed-refs` (catching a gc that packs them) are watched too. `HEAD` still
    //    matters for a detached checkout, where it holds the hash directly.
    for name in ["HEAD", "index", "refs", "packed-refs"] {
        let Some(p) = git(dir, &["rev-parse", "--git-path", name]) else {
            continue;
        };
        let path = if Path::new(&p).is_absolute() {
            PathBuf::from(p)
        } else {
            Path::new(dir).join(p)
        };
        // Only name paths that exist: `packed-refs` is absent until a gc, and Cargo
        // re-runs the script unconditionally for a path it cannot stat.
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Whether the git repository `git` discovers from `dir` is rooted **at** `dir`,
/// i.e. it is nc's own checkout rather than some enclosing repository this copy of
/// the crate happens to sit inside (see the module doc).
fn is_nc_repository(dir: &str) -> bool {
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (std::fs::canonicalize(&top), std::fs::canonicalize(dir)) {
        (Ok(a), Ok(b)) => a == b,
        // Either path could not be resolved — refuse to claim provenance.
        _ => false,
    }
}

/// The short commit hash and working-tree cleanliness of the source tree being
/// built, as `("<hash>" | "unknown", "true" | "false" | "unknown")`.
///
/// `dirty` is reported `"unknown"` whenever the commit itself is unknown (no git,
/// no repository, or a repository that isn't this package's) — an unqualified
/// `"false"` there would claim a clean checkout of a tree we know nothing about.
/// The converse is *not* symmetric: a readable `HEAD` with an unreadable index
/// legitimately yields a known commit with unknown cleanliness, and
/// `version::Identity` models exactly that one-directional invariant.
fn git_identity(dir: &str, ours: bool) -> (String, String) {
    if !ours {
        return ("unknown".to_string(), "unknown".to_string());
    }
    let Some(commit) = git(dir, &["rev-parse", "--short=12", "HEAD"]) else {
        return ("unknown".to_string(), "unknown".to_string());
    };
    // `--porcelain` prints one line per modified/untracked path; empty = clean. A
    // failure here (e.g. an unreadable index) leaves cleanliness unknown rather
    // than guessing either way.
    let dirty = match git(dir, &["status", "--porcelain"]) {
        Some(out) => if out.is_empty() { "false" } else { "true" }.to_string(),
        None => "unknown".to_string(),
    };
    (commit, dirty)
}

/// Run `git` in `dir` and return its trimmed stdout, or `None` when git is
/// absent, the directory is not a repository, or the command fails.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
