//! Staged writes: write to a same-directory temp, fsync it, then rename into place.
//!
//! **What this guarantees, and what it does not.** POSIX `rename` is atomic *per
//! file*; several files cannot be flipped as one unit. So this module does **not**
//! provide all-or-nothing semantics across an artifact set. It provides two things
//! that are actually achievable:
//!
//! 1. **No truncated file ever appears at a final path.** Every byte is written to
//!    `<final-prefix>.<pid>.<n>.nctmp`, flushed and fsynced, and only then renamed. The
//!    final path holds either the previous content or nothing. This one holds
//!    unconditionally — including on `SIGINT`, `SIGKILL` and power loss, because the
//!    final path is simply never opened for writing.
//!
//!    **Temp cleanup is narrower.** [`Staged`]'s `Drop` removes the temp, so ordinary
//!    error paths (and abandoned writes) leave nothing behind — but a signal that kills
//!    the process does **not** run destructors, so `SIGINT`/`SIGKILL` can leave an inert
//!    `*.nctmp` beside the output. Installing a signal handler or scavenging temps at
//!    startup would close that; neither is done here, so the guarantee is stated as
//!    "ordinary error paths" rather than "always".
//! 2. **A minimal inconsistency window.** The orchestrator stages *every* artifact
//!    first — all the fallible work — and calls [`Staged::commit`] on each only at
//!    the end. A crash between two commits can still leave one final path updated
//!    and another not. That window is inherent to multi-file output and is
//!    documented rather than papered over.
//!
//! Two properties the temp path must have, both load-bearing:
//!
//! - **Same directory as the target.** A temp elsewhere (say `/tmp`) is likely on
//!   another filesystem, and `rename` across filesystems fails with `EXDEV` — it
//!   does *not* silently fall back to a copy. Same directory keeps finalization a
//!   real rename instead of a second full write.
//! - **A name no collision check would have caught.** `cli`'s
//!   `ensure_write_targets_distinct` validates the *final* paths up front; temps are
//!   derived from those by appending a suffix that includes the pid and a
//!   process-unique counter, so two artifacts (or two `roll` frames) can never stage
//!   onto each other, and a temp can never equal a checked final path.
//!
//! **A symlinked target is followed, and an existing file's mode is carried over.**
//! Both restore `File::create` behaviour that a bare rename would have changed:
//! `create` followed a symlink and updated its referent, and it preserved the existing
//! file's permissions because it truncates in place. A rename would instead replace the
//! link itself and install the temp's umask-derived mode — destroying a
//! `latest.tiff`-style link, and turning a deliberate `0600` output into `0644`. See
//! [`resolve_target`] and [`Staged::commit`]. Mode only: ACLs and extended attributes
//! are not carried across.
//!
//! **Overwrite is atomic replace**, matching the previous `File::create`
//! truncate-in-place behaviour: `nc` keeps overwriting its own output rather than
//! refusing. `std::fs::rename` documents replacing an existing target on both Unix
//! (`rename`) and Windows (`MoveFileExW`/`SetFileInformationByHandle`), so the
//! contract holds on every platform this project builds for. One documented Windows
//! caveat that std cannot paper over: a rename can fail there if the destination is
//! held open by another process. CI covers Linux and macOS only, so treat the
//! Windows path as untested rather than guaranteed.
//!
//! **Directory fsync is deliberately out of scope.** Surviving *power loss* across
//! the rename itself requires fsyncing the parent directory too. For a conversion
//! CLI the failures worth defending against are a full disk, a permissions error, a
//! mid-run crash, and `SIGINT` — for all of which the *final path* is already safe,
//! because the rename either happened or it didn't (temp cleanup after a signal is
//! the separate, narrower guarantee above). Adding a directory
//! fsync would buy only power-loss durability, at the cost of a Unix-only code path
//! (`File::open(dir)?.sync_all()` has no portable equivalent) for a tool whose
//! output is reproducible by re-running it.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::{NcError, Result};

/// Process-unique suffix counter, combined with the pid so two artifacts (or two
/// `roll` frames) in one process never stage onto each other.
///
/// It does **not** make the name globally unique: two processes in separate PID
/// namespaces — separate containers sharing an output mount — can both be pid 1 and
/// both start at sequence 0. That is why the temp is created with `create_new`, which
/// fails instead of truncating a live staging file, and why [`stage`] retries with a
/// fresh sequence rather than trusting the name.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Longest single path component the common filesystems accept (`NAME_MAX` is 255
/// bytes on ext4, APFS and NTFS). The staging suffix has to fit *inside* this budget:
/// a legal 245-byte output basename plus a 14-byte suffix is 259 and fails with
/// `ENAMETOOLONG`, so a target that could be written directly would fail to stage.
const MAX_BASENAME_BYTES: usize = 255;

/// How many times [`stage`] retries a colliding temp name before giving up. Each
/// attempt draws a fresh sequence number, so a handful is plenty — this exists to
/// bound a pathological loop, not to survive sustained contention.
const TEMP_NAME_ATTEMPTS: u32 = 8;

/// A file fully written and fsynced at a temp path, waiting to be renamed onto its
/// final path by [`commit`](Self::commit).
///
/// `#[must_use]`: dropping a `Staged` **discards** the write (the temp is unlinked),
/// which is correct on an error path and a silent data-loss bug anywhere else.
#[must_use = "a staged write is discarded unless committed"]
#[derive(Debug)]
pub struct Staged {
    /// `None` once committed, so `Drop` doesn't unlink a file that is now the real
    /// output under its final name.
    temp: Option<PathBuf>,
    target: PathBuf,
}

impl Staged {
    /// Rename the temp onto the final path, replacing any existing file there.
    ///
    /// This is the only operation that touches the final path, and it is the only
    /// step left after every artifact has been staged — which is what keeps the
    /// inconsistency window down to the renames.
    pub fn commit(mut self) -> Result<()> {
        // Take first: on failure the temp must still be cleaned up by `Drop`, and
        // on success it must not be, because it *is* the target now.
        let temp = self
            .temp
            .take()
            .expect("Staged::commit is by-value, so temp is always present");
        // A rename replaces the target's *inode*, so the promoted file carries the
        // temp's umask-derived mode — turning a deliberately `0600` output into `0644`
        // on the next run. `File::create` preserved the existing mode (it truncates in
        // place), so this restores that behaviour rather than silently widening access.
        // Verified empirically: create keeps 0600, rename alone yields 0644.
        //
        // A hard error, not best-effort: quietly widening access to someone's scan is
        // worse than failing the run. Mode only — ACLs and extended attributes are not
        // carried across, which is a real limitation of doing this with `std`.
        if let Ok(existing) = fs::metadata(&self.target) {
            fs::set_permissions(&temp, existing.permissions()).map_err(|e| {
                self.temp = Some(temp.clone());
                NcError::Write(format!(
                    "finalizing {}: cannot carry the existing file's permissions onto \
                     the staged copy: {e}",
                    self.target.display()
                ))
            })?;
        }
        fs::rename(&temp, &self.target).map_err(|e| {
            // Put it back so `Drop` removes the temp we could not promote — the
            // alternative leaves a stray `.tmp` beside a failed run.
            self.temp = Some(temp.clone());
            NcError::Write(format!(
                "finalizing {}: renaming {} failed: {e}",
                self.target.display(),
                temp.display()
            ))
        })
    }

    /// The temp path bytes are currently at. Test-only introspection — production
    /// code has no business knowing this.
    #[cfg(test)]
    pub(crate) fn temp_path(&self) -> Option<&Path> {
        self.temp.as_deref()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(temp) = &self.temp {
            // Best-effort: the run is already failing (or the caller deliberately
            // abandoned the write), so a cleanup error must not mask the real one.
            // Worst case a `.tmp` is left behind, which is inert.
            let _ = fs::remove_file(temp);
        }
    }
}

/// The temp path for `target`: same directory, named from a **prefix** of the
/// target's basename plus the pid and `seq`.
///
/// A prefix, not the whole basename: appending to a basename already near
/// [`MAX_BASENAME_BYTES`] pushes the temp past the filesystem's component limit, so a
/// perfectly legal output path would fail to stage. Keeping a prefix (rather than a
/// name with no relation to the target, which would also be correct) means a stray
/// temp is still traceable to the artifact it belonged to.
///
/// Pure and `seq`-parameterized so the bounding logic is testable without reaching
/// into the counter.
fn temp_path_for(target: &Path, seq: u64) -> PathBuf {
    let suffix = format!(".{}.{seq}.nctmp", std::process::id());
    // Lossy is fine: the suffix carries the uniqueness, so a non-UTF-8 basename that
    // round-trips imperfectly costs only traceability, never correctness.
    let base = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let budget = MAX_BASENAME_BYTES.saturating_sub(suffix.len());
    let mut kept = String::with_capacity(budget);
    for ch in base.chars() {
        if kept.len() + ch.len_utf8() > budget {
            break;
        }
        kept.push(ch);
    }
    let parent = target.parent().unwrap_or(Path::new("."));
    parent.join(format!("{kept}{suffix}"))
}

/// Resolve a symlinked target to the file it points at, so a rename replaces the
/// **referent** instead of the link.
///
/// `File::create(path)` followed a symlink and updated its referent; `rename(temp,
/// path)` would replace the link's own directory entry — destroying a
/// `latest.tiff`-style link and leaving the intended file stale, while the run
/// reported success. Resolving here preserves the previous behaviour, and staging in
/// the *referent's* directory is also what keeps the rename same-filesystem.
///
/// A dangling symlink is resolved one hop by hand (`canonicalize` requires the
/// referent to exist), which covers `latest.tiff -> not-yet-written.tiff`. A dangling
/// *chain* resolves only its first hop — an edge case of an edge case, noted rather
/// than handled.
fn resolve_target(target: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if let Ok(real) = fs::canonicalize(target) {
                return Ok(real);
            }
            let link = fs::read_link(target).map_err(|e| {
                NcError::Write(format!(
                    "cannot resolve the symlink at {}: {e}",
                    target.display()
                ))
            })?;
            Ok(if link.is_absolute() {
                link
            } else {
                target.parent().unwrap_or(Path::new(".")).join(link)
            })
        }
        // Not a symlink, or does not exist yet: write it where the caller asked.
        _ => Ok(target.to_path_buf()),
    }
}

/// Stage a write: create the temp, hand a buffered writer to `write`, then flush
/// and fsync so the bytes are durable *before* anyone can rename them into place.
///
/// The writer is `BufWriter<File>`, which is `Write + Seek` — the TIFF encoder needs
/// `Seek` to backfill IFD offsets, so this cannot be narrowed to `Write`.
///
/// A failure anywhere (create, the closure, flush, fsync) removes the temp before
/// returning, so an error path never litters. `write`'s value is returned alongside
/// the [`Staged`] handle, which lets an encoder report what it wrote (clipping
/// counts, stats) without the staging layer knowing anything about it.
pub fn stage<T>(
    target: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<T>,
) -> Result<(Staged, T)> {
    // Follow a symlinked target so the rename replaces the referent, not the link.
    let target = &resolve_target(target)?;
    // `create_new`, never `create`: two processes in separate PID namespaces can
    // derive the same candidate name, and `create` would silently *truncate* the
    // other one's live staging file — promoting mixed bytes as a complete output.
    // Exclusive creation turns that into a detectable collision we simply retry.
    let mut last_err = None;
    let mut opened = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let candidate = temp_path_for(target, TEMP_SEQ.fetch_add(1, Ordering::Relaxed));
        match File::options()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                opened = Some((candidate, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => {
                return Err(NcError::Write(format!(
                    "creating a staging file for {}: {e}",
                    target.display()
                )));
            }
        }
    }
    let (temp, file) = opened.ok_or_else(|| {
        NcError::Write(format!(
            "creating a staging file for {}: {} name collisions in a row{}",
            target.display(),
            TEMP_NAME_ATTEMPTS,
            last_err.map(|e| format!(" ({e})")).unwrap_or_default()
        ))
    })?;
    // Own the guard from here on, so every `?` below unlinks the temp via `Drop`.
    let staged = Staged {
        temp: Some(temp.clone()),
        target: target.clone(),
    };
    let mut writer = BufWriter::new(file);
    let value = write(&mut writer)?;
    flush_surfacing_errors(&mut writer, target)?;
    // fsync before any rename can promote these bytes. Required, not optional: a
    // rename is only as good as the data it points at.
    writer
        .get_ref()
        .sync_all()
        .map_err(|e| NcError::Write(format!("syncing {}: {e}", target.display())))?;
    Ok((staged, value))
}

/// Commit a whole artifact set, checking first that every rename can plausibly
/// succeed.
///
/// **Why the pre-check exists.** Staging removes *write* failures from the commit
/// phase, but not every rename failure: a target path occupied by a **directory**
/// cannot be renamed onto on any platform, and that is detectable up front. Without
/// this pass the commits run in order and a later failure leaves earlier artifacts
/// already promoted — reintroducing exactly the orphaned-primary case this module
/// exists to prevent. (Found by the failure-injection test, not by reasoning: the
/// obvious way to make a sidecar write fail — occupy its path with a directory —
/// fails at the rename, not at the write.)
///
/// **What it still does not promise.** The pre-check narrows the window; it cannot
/// close it. A rename can fail for reasons no cheap check predicts (permissions
/// revoked mid-run, the filesystem going read-only, a crash between two renames), and
/// an already-renamed artifact cannot be un-renamed — its previous content is gone.
/// So callers should order the set with the artifact whose *presence implies success*
/// last; `cli` commits the primary output after the sidecar for that reason.
pub fn commit_all(artifacts: Vec<Staged>) -> Result<()> {
    for a in &artifacts {
        if a.target.is_dir() {
            return Err(NcError::Write(format!(
                "cannot write {}: a directory exists at that path",
                a.target.display()
            )));
        }
    }
    for a in artifacts {
        a.commit()?;
    }
    Ok(())
}

/// Flush explicitly and surface the error.
///
/// `BufWriter`'s implicit flush on drop **discards** its error — a full disk on the
/// final block would silently truncate the file, which is the trap this project's
/// "fail loudly" rule exists to close. Generic over the writer so the failure path
/// stays testable with a mock; a real `File` cannot be made to fail portably.
fn flush_surfacing_errors<W: Write>(writer: &mut W, target: &Path) -> Result<()> {
    writer
        .flush()
        .map_err(|e| NcError::Write(format!("flushing {}: {e}", target.display())))
}

/// Stage a complete byte buffer — the JSON artifacts (sidecar, `--dump-params`,
/// `--report-file`), which are built in memory and have no incremental writer.
pub fn stage_bytes(target: &Path, bytes: &[u8]) -> Result<Staged> {
    let (staged, ()) = stage(target, |w| {
        w.write_all(bytes)
            .map_err(|e| NcError::Write(format!("writing {}: {e}", target.display())))
    })?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp directory that cleans itself up, so these tests leave nothing behind
    /// even when they fail (no `tempfile` dependency in this crate).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("nc-staged-{tag}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn join(&self, n: &str) -> PathBuf {
            self.0.join(n)
        }
        /// Every `*.nctmp` left in the directory — the litter check.
        fn temps(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "nctmp"))
                .collect()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_target_does_not_exist_until_commit() {
        // The whole point: bytes are durable somewhere else first, and the final
        // path only ever appears fully-formed.
        let dir = TempDir::new("commit");
        let target = dir.join("out.bin");
        let staged = stage_bytes(&target, b"hello").unwrap();
        assert!(
            !target.exists(),
            "the final path must not exist before commit"
        );
        assert_eq!(dir.temps().len(), 1, "exactly one temp is staged");
        staged.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        assert!(dir.temps().is_empty(), "commit consumes its temp");
    }

    #[test]
    fn the_temp_sits_in_the_target_directory() {
        // Load-bearing: a temp on another filesystem cannot be renamed into place
        // at all (`EXDEV`), so this is not cosmetic.
        let dir = TempDir::new("samedir");
        let target = dir.join("out.bin");
        let staged = stage_bytes(&target, b"x").unwrap();
        let temp = staged.temp_path().unwrap().to_path_buf();
        assert_eq!(
            temp.parent(),
            target.parent(),
            "temp must be a sibling of the target"
        );
        // And it must not equal any final path a collision check validated.
        assert_ne!(temp, target);
    }

    #[test]
    fn dropping_without_commit_discards_the_write() {
        let dir = TempDir::new("drop");
        let target = dir.join("out.bin");
        drop(stage_bytes(&target, b"abandoned").unwrap());
        assert!(!target.exists(), "an abandoned write must not appear");
        assert!(dir.temps().is_empty(), "and must not leave a temp behind");
    }

    #[test]
    fn a_failing_writer_leaves_neither_target_nor_temp() {
        // The error path is where litter accumulates, so pin it.
        let dir = TempDir::new("failwrite");
        let target = dir.join("out.bin");
        let err = stage(&target, |_w| -> Result<()> {
            Err(NcError::Write("injected".into()))
        })
        .unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(!target.exists());
        assert!(dir.temps().is_empty(), "no temp survives a failed stage");
    }

    #[test]
    fn commit_replaces_existing_content_atomically() {
        // The decided contract: `nc` overwrites its own output rather than refusing.
        let dir = TempDir::new("replace");
        let target = dir.join("out.bin");
        fs::write(&target, b"old content that is longer").unwrap();
        let staged = stage_bytes(&target, b"new").unwrap();
        // Until the commit the old bytes are intact — an interrupted overwrite
        // leaves the previous file, never a truncated new one.
        assert_eq!(fs::read(&target).unwrap(), b"old content that is longer");
        staged.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(dir.temps().is_empty());
    }

    #[test]
    fn a_failed_stage_leaves_an_existing_target_untouched() {
        let dir = TempDir::new("keepold");
        let target = dir.join("out.bin");
        fs::write(&target, b"previous").unwrap();
        drop(stage(&target, |_w| -> Result<()> {
            Err(NcError::Write("injected".into()))
        }));
        assert_eq!(
            fs::read(&target).unwrap(),
            b"previous",
            "a failed write must not disturb the previous output"
        );
    }

    #[test]
    fn staging_two_artifacts_at_once_uses_distinct_temps() {
        // `convert` stages IR + primary + sidecar before committing any, and `roll`
        // stages per frame — so temps must never collide, including for the same
        // target path.
        let dir = TempDir::new("distinct");
        let target = dir.join("out.bin");
        let a = stage_bytes(&target, b"a").unwrap();
        let b = stage_bytes(&target, b"b").unwrap();
        assert_ne!(a.temp_path().unwrap(), b.temp_path().unwrap());
        assert_eq!(dir.temps().len(), 2);
        // Last commit wins, and neither leaves litter.
        a.commit().unwrap();
        b.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"b");
        assert!(dir.temps().is_empty());
    }

    #[test]
    fn flush_error_is_surfaced_not_swallowed() {
        // Migrated from `io::encode` when staging took over flushing. Still worth
        // pinning: a writer whose flush fails must produce an `NcError::Write`, never
        // be silently dropped (the BufWriter-drop-swallows-errors trap). A real
        // `File` cannot be made to fail portably, hence the mock.
        struct FailFlush;
        impl Write for FailFlush {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
        }
        let err = flush_surfacing_errors(&mut FailFlush, Path::new("out.tiff")).unwrap_err();
        assert!(matches!(err, NcError::Write(msg) if msg.contains("disk full")));
    }

    #[test]
    fn commit_all_rejects_the_whole_set_when_one_target_is_a_directory() {
        // The case that motivated the pre-check: without it the first artifact is
        // promoted and the second's rename fails, leaving a partial set.
        let dir = TempDir::new("commitall");
        let good = dir.join("good.bin");
        let blocked = dir.join("blocked.bin");
        fs::create_dir(&blocked).unwrap();
        let a = stage_bytes(&good, b"a").unwrap();
        let b = stage_bytes(&blocked, b"b").unwrap();
        let err = commit_all(vec![a, b]).unwrap_err();
        assert!(err.to_string().contains("blocked.bin"), "{err}");
        assert!(
            !good.exists(),
            "no artifact may be promoted when another cannot be"
        );
        assert!(dir.temps().is_empty(), "and the set leaves no temps");
    }

    #[test]
    fn commit_all_promotes_every_artifact_on_the_happy_path() {
        let dir = TempDir::new("commitall-ok");
        let (a, b) = (dir.join("a.bin"), dir.join("b.bin"));
        commit_all(vec![
            stage_bytes(&a, b"aa").unwrap(),
            stage_bytes(&b, b"bb").unwrap(),
        ])
        .unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"aa");
        assert_eq!(fs::read(&b).unwrap(), b"bb");
        assert!(dir.temps().is_empty());
    }

    #[test]
    fn a_long_basename_still_stages_within_the_component_limit() {
        // Regression: a legal 245-byte basename plus the old full-basename suffix was
        // 259 bytes and failed with ENAMETOOLONG — a path that could be written
        // directly could not be staged.
        let dir = TempDir::new("longname");
        let target = dir.join(&"a".repeat(245));
        let staged = stage_bytes(&target, b"x").unwrap();
        let temp = staged.temp_path().unwrap().to_path_buf();
        let temp_base = temp.file_name().unwrap().to_string_lossy().len();
        assert!(
            temp_base <= MAX_BASENAME_BYTES,
            "temp basename is {temp_base} bytes, over the {MAX_BASENAME_BYTES} limit"
        );
        assert_eq!(temp.parent(), target.parent(), "still a sibling");
        staged.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn the_temp_name_keeps_a_prefix_of_the_target_for_traceability() {
        // Bounding must not degenerate into an opaque name: a stray temp should still
        // point back at the artifact it belonged to.
        let temp = temp_path_for(Path::new("/tmp/out.tiff"), 7);
        let name = temp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("out.tiff."), "{name}");
        assert!(name.ends_with(".7.nctmp"), "{name}");
        // And a pathological basename is truncated, not rejected.
        let long = format!("/tmp/{}", "b".repeat(300));
        let temp = temp_path_for(Path::new(&long), 0);
        assert!(temp.file_name().unwrap().to_string_lossy().len() <= MAX_BASENAME_BYTES);
    }

    #[test]
    fn staging_creates_exclusively_rather_than_truncating() {
        // The primitive the collision retry rests on: `create_new` must refuse an
        // existing file, where `create` would truncate it. Two processes in separate
        // PID namespaces can derive the same candidate name, and truncating the other
        // one's live staging file would promote mixed bytes as a complete output.
        let dir = TempDir::new("exclusive");
        let occupied = dir.join("occupied");
        fs::write(&occupied, b"live staging data").unwrap();
        let err = File::options()
            .write(true)
            .create_new(true)
            .open(&occupied)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&occupied).unwrap(),
            b"live staging data",
            "create_new must not have touched the existing bytes"
        );
    }

    #[test]
    fn committing_preserves_a_restrictive_mode_on_the_replaced_file() {
        // Regression: a rename installs the temp's umask-derived mode, so a deliberately
        // 0600 output became 0644 on the next run — silently widening access to a scan.
        // `File::create` preserved it because it truncates in place.
        let dir = TempDir::new("perms");
        let target = dir.join("out.bin");
        fs::write(&target, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            stage_bytes(&target, b"new").unwrap().commit().unwrap();
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the replaced file must keep its mode");
            assert_eq!(fs::read(&target).unwrap(), b"new");
        }
        // On a non-Unix target the mode has no meaning; the replace itself must still work.
        #[cfg(not(unix))]
        {
            stage_bytes(&target, b"new").unwrap().commit().unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"new");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_updates_the_referent_and_survives() {
        // Regression: `File::create` followed the link and wrote the referent, but a
        // bare rename replaces the link's own entry — destroying a `latest.tiff`-style
        // link and leaving the intended file stale while the run reports success.
        let dir = TempDir::new("symlink");
        let real = dir.join("real.bin");
        let link = dir.join("latest.bin");
        fs::write(&real, b"old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let staged = stage_bytes(&link, b"new").unwrap();
        // The temp is a sibling of the *referent*, which is also what keeps the rename
        // on one filesystem. Compared canonically because resolving through
        // `canonicalize` also canonicalizes the directory part (on macOS `/var` becomes
        // `/private/var`), so the two spellings differ while naming the same directory.
        let canon = |p: &Path| fs::canonicalize(p).expect("directory exists");
        assert_eq!(
            canon(staged.temp_path().unwrap().parent().unwrap()),
            canon(real.parent().unwrap())
        );
        staged.commit().unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive the write"
        );
        assert_eq!(
            fs::read(&real).unwrap(),
            b"new",
            "and its referent must carry the new bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_target_writes_the_file_it_points_at() {
        // `canonicalize` can't resolve a link whose referent does not exist yet, so this
        // is the hand-resolved hop. `latest.bin -> not-yet.bin` must create `not-yet.bin`
        // rather than replacing the link.
        let dir = TempDir::new("dangling");
        let link = dir.join("latest.bin");
        std::os::unix::fs::symlink("not-yet.bin", &link).unwrap();
        stage_bytes(&link, b"fresh").unwrap().commit().unwrap();
        assert_eq!(fs::read(dir.join("not-yet.bin")).unwrap(), b"fresh");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself is untouched"
        );
    }

    #[test]
    fn a_create_failure_is_a_write_error_naming_the_target() {
        // Unwritable location: the message must name the artifact, not just errno.
        let target = Path::new("/nonexistent-nc-dir-xyz/out.bin");
        let err = stage_bytes(target, b"x").unwrap_err();
        assert!(matches!(err, NcError::Write(_)), "{err}");
        assert!(err.to_string().contains("out.bin"), "{err}");
    }

    #[test]
    fn commit_failure_reports_the_target_and_cleans_up() {
        // Rename onto a *directory* fails on every platform we build for. The error
        // must name the artifact, and the temp must not survive.
        let dir = TempDir::new("commitfail");
        let target = dir.join("out.bin");
        fs::create_dir(&target).unwrap();
        let staged = stage_bytes(&target, b"x").unwrap();
        let temp = staged.temp_path().unwrap().to_path_buf();
        let err = staged.commit().unwrap_err();
        assert!(matches!(err, NcError::Write(_)), "{err}");
        assert!(err.to_string().contains("out.bin"), "{err}");
        assert!(!temp.exists(), "a failed commit must still clean its temp");
    }
}
