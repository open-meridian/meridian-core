//! An instance's own storage claim copied into its shared storage's
//! `shard-0/`, once, before its first launch as several pods (step 3d,
//! checkpoint 5, item 30).
//!
//! The launcher's copy pod runs [`copy`] with the instance's own claim
//! mounted read only at [`FROM`] and its shared storage at [`TO`]; once the
//! copy is whole and on disk, it writes a marker naming the claim it copied,
//! [`marker`]. The instance's pods wait for that marker in an init container
//! ([`wait`]) before their plugin starts, so no plugin reads or writes a
//! partial copy. The copy is made again whole by a pod that failed part way,
//! and not at all once the marker is there. The instance's own claim is
//! never written: it is mounted read only, and kept.

use std::path::{Path, PathBuf};

/// Where the copy pod mounts the instance's own claim, read only.
pub const FROM: &str = "/var/lib/meridian/copy/from";
/// Where the copy pod mounts the instance's shared storage.
pub const TO: &str = "/var/lib/meridian/copy/to";
/// Where an instance's pods mount its storage (MERIDIAN_STORAGE_DIR).
pub const STORAGE: &str = "/var/lib/meridian/storage";
/// The folder of the shared storage the instance's own records go to: the
/// first shard's, as a one-pod instance is shard 0 of one.
pub const SHARD_0: &str = "shard-0";

/// The marker in a shared storage `root` saying `claim` was copied into its
/// `shard-0/` whole.
pub fn marker(root: &Path, claim: &str) -> PathBuf {
    root.join(".meridian").join(format!("copied-from-{claim}"))
}

/// Whether `claim` was copied into the shared storage at `root`.
pub fn copied(root: &Path, claim: &str) -> bool {
    marker(root, claim).exists()
}

/// What [`copy`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// Copied now: so many files, so many bytes.
    Now { files: u64, bytes: u64 },
    /// Copied before: the marker was there, and nothing was written.
    Before,
}

/// The folder a copy is made in before it is renamed `shard-0/` whole
/// (review c5c L3).
pub const COPYING: &str = "shard-0.copying";

/// Copy everything under `from` into `to`'s `shard-0/`, then write the
/// marker for `claim`, each file synced to disk first. A volume's own
/// `lost+found` at the top is not the plugin's, and is left.
///
/// The copy is made in [`COPYING`], made again whole where one failed part
/// way, and renamed `shard-0/` only where there is none: a `shard-0/` with
/// no marker -- the marker removed by hand, say -- holds records the shards
/// wrote, and is never written over (review c5c L3). The marker's
/// `.writing` file is written once the copy is whole and before the
/// rename, so a copy stopped after the rename is finished, not refused.
pub fn copy(from: &Path, to: &Path, claim: &str) -> Result<Copied, String> {
    if copied(to, claim) {
        return Ok(Copied::Before);
    }
    let (into, copying) = (to.join(SHARD_0), to.join(COPYING));
    let marked = marker(to, claim);
    let folder = marked.parent().expect("the marker is in a folder");
    let writing = folder.join(format!(".copied-from-{claim}.writing"));
    let mut counted = (0u64, 0u64);
    if writing.exists() && !into.exists() && !copying.exists() {
        // Marked whole, and the copy since removed by hand: made again.
        std::fs::remove_file(&writing)
            .map_err(|failed| format!("{} could not be removed: {failed}", writing.display()))?;
    }
    if !writing.exists() {
        if into.exists() {
            return Err(format!(
                "{} is there with no mark that {claim} was copied into it: it holds what the \
                 shards wrote, and nothing is copied over it",
                into.display()
            ));
        }
        if copying.exists() {
            std::fs::remove_dir_all(&copying).map_err(|failed| {
                format!("{} could not be removed: {failed}", copying.display())
            })?;
        }
        copy_tree(from, &copying, true, &mut counted)?;
        std::fs::create_dir_all(folder)
            .map_err(|failed| format!("{} could not be made: {failed}", folder.display()))?;
        std::fs::write(
            &writing,
            format!("{} files, {} bytes\n", counted.0, counted.1),
        )
        .and_then(|()| std::fs::File::open(&writing)?.sync_all())
        .and_then(|()| std::fs::File::open(folder)?.sync_all())
        .map_err(|failed| format!("the marker {} was not written: {failed}", writing.display()))?;
    }
    if !into.exists() {
        std::fs::rename(&copying, &into)
            .and_then(|()| std::fs::File::open(to)?.sync_all())
            .map_err(|failed| {
                format!(
                    "{} was not renamed {}: {failed}",
                    copying.display(),
                    into.display()
                )
            })?;
    }
    std::fs::rename(&writing, &marked)
        .and_then(|()| std::fs::File::open(folder)?.sync_all())
        .map_err(|failed| format!("the marker {} was not written: {failed}", marked.display()))?;
    Ok(Copied::Now {
        files: counted.0,
        bytes: counted.1,
    })
}

fn copy_tree(from: &Path, into: &Path, top: bool, counted: &mut (u64, u64)) -> Result<(), String> {
    std::fs::create_dir_all(into)
        .map_err(|failed| format!("{} could not be made: {failed}", into.display()))?;
    let entries = std::fs::read_dir(from)
        .map_err(|failed| format!("{} could not be read: {failed}", from.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|failed| format!("{} could not be read: {failed}", from.display()))?;
        let name = entry.file_name();
        if top && name == "lost+found" {
            continue;
        }
        let (source, target) = (entry.path(), into.join(&name));
        let kind = entry
            .file_type()
            .map_err(|failed| format!("{} could not be read: {failed}", source.display()))?;
        if kind.is_dir() {
            copy_tree(&source, &target, false, counted)?;
        } else if kind.is_symlink() {
            let points = std::fs::read_link(&source)
                .map_err(|failed| format!("{} could not be read: {failed}", source.display()))?;
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(&points, &target)
                .map_err(|failed| format!("{} could not be made: {failed}", target.display()))?;
        } else {
            let bytes = std::fs::copy(&source, &target)
                .and_then(|bytes| std::fs::File::open(&target)?.sync_all().map(|()| bytes))
                .map_err(|failed| {
                    format!(
                        "{} was not copied to {}: {failed}",
                        source.display(),
                        target.display()
                    )
                })?;
            counted.0 += 1;
            counted.1 += bytes;
        }
    }
    // The folder's own mode last, setgid bit and all, so a folder only
    // readable is still written into first (review c5c H1). A file's is
    // copied with it.
    std::fs::metadata(from)
        .and_then(|held| std::fs::set_permissions(into, held.permissions()))
        .map_err(|failed| {
            format!(
                "{} could not be given {}'s mode: {failed}",
                into.display(),
                from.display()
            )
        })?;
    std::fs::File::open(into)
        .and_then(|folder| folder.sync_all())
        .map_err(|failed| format!("{} could not be synced: {failed}", into.display()))
}

/// How long an instance's pod waits for its copy before its init container
/// ends, failed, saying why (review c5c M1); the kubelet starts it again.
pub const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Wait, every `every`, until `claim` was copied into the shared storage at
/// `root`: an instance's pods' init container, so its plugin never starts
/// on a partial copy. Says so once a minute while it waits, and past
/// `limit` ends, refused, so the pod shows that its copy has not come.
pub fn wait(
    root: &Path,
    claim: &str,
    every: std::time::Duration,
    limit: std::time::Duration,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut said = started;
    while !copied(root, claim) {
        if started.elapsed() >= limit {
            return Err(format!(
                "{claim} was not copied into the instance's shared storage within {} s: its \
                 copy pod failed or has not run; launching the instance again makes the copy again",
                limit.as_secs()
            ));
        }
        if said.elapsed() >= std::time::Duration::from_secs(60) {
            tracing::info!(
                claim,
                waited_seconds = started.elapsed().as_secs(),
                "the instance's own storage is still being copied into its shared storage"
            );
            said = std::time::Instant::now();
        }
        std::thread::sleep(every);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own under the system's temporary folder, removed
    /// when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Scratch {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "meridian-copy-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn tree(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    /// Item 30: the instance's own records land under shard-0/ as they
    /// were, its `#line`s with them, the volume's lost+found left; the
    /// marker is written last, and a copy made again finds it and writes
    /// nothing.
    #[test]
    fn an_own_claim_is_copied_into_shard_0_once_and_marked_last() {
        let (from, to) = (scratch(), scratch());
        tree(
            from.path(),
            &[
                ("responses/2026-11-15/quotes/q.jsonl", "a\nb\n"),
                (".meridian/archive/index.json", "{}"),
                ("lost+found/x", "not the plugin's"),
            ],
        );
        assert!(!copied(to.path(), "rel-storage-feed-1"));
        assert_eq!(
            copy(from.path(), to.path(), "rel-storage-feed-1").unwrap(),
            Copied::Now { files: 2, bytes: 6 }
        );
        assert_eq!(
            read(to.path(), "shard-0/responses/2026-11-15/quotes/q.jsonl"),
            "a\nb\n"
        );
        assert_eq!(
            read(to.path(), "shard-0/.meridian/archive/index.json"),
            "{}"
        );
        assert!(!to.path().join("shard-0/lost+found").exists());
        assert!(copied(to.path(), "rel-storage-feed-1"));
        // Its own claim untouched.
        assert_eq!(
            read(from.path(), "responses/2026-11-15/quotes/q.jsonl"),
            "a\nb\n"
        );
        // Made again -- a launch after -- it writes nothing over what the
        // shards wrote since.
        tree(
            to.path(),
            &[("shard-0/responses/2026-11-15/quotes/q.jsonl", "a\nb\nc\n")],
        );
        assert_eq!(
            copy(from.path(), to.path(), "rel-storage-feed-1").unwrap(),
            Copied::Before
        );
        assert_eq!(
            read(to.path(), "shard-0/responses/2026-11-15/quotes/q.jsonl"),
            "a\nb\nc\n"
        );
    }

    /// A copy that failed part way left no marker, so it is made again
    /// whole, and the pods kept waiting meanwhile.
    #[test]
    fn a_copy_failed_part_way_leaves_no_marker_and_is_made_again_whole() {
        let (from, to) = (scratch(), scratch());
        tree(from.path(), &[("a/one", "1"), ("b/two", "2")]);
        // Part way: one file there, half written, one not the copy's, and
        // no marker; made in shard-0.copying/, never in shard-0/ (review
        // c5c L3).
        tree(
            to.path(),
            &[
                ("shard-0.copying/a/one", ""),
                ("shard-0.copying/z/left", "x"),
            ],
        );
        assert!(!copied(to.path(), "c"));
        copy(from.path(), to.path(), "c").unwrap();
        assert_eq!(read(to.path(), "shard-0/a/one"), "1");
        assert_eq!(read(to.path(), "shard-0/b/two"), "2");
        assert!(!to.path().join("shard-0/z").exists(), "made again whole");
        assert!(!to.path().join(COPYING).exists());
        assert!(copied(to.path(), "c"));
        // The marker of another claim is not this one's.
        assert!(!copied(to.path(), "d"));
    }

    /// Review c5c L3: a shard-0/ with no marker -- the marker removed by
    /// hand -- holds what the shards wrote since, so nothing is copied over
    /// it: refused, every file as it was. A copy stopped after its rename,
    /// marked whole but not yet marked, is finished, not refused.
    #[test]
    fn a_shard_0_with_no_marker_is_never_copied_over() {
        let (from, to) = (scratch(), scratch());
        tree(from.path(), &[("responses/q.jsonl", "a\n")]);
        tree(to.path(), &[("shard-0/responses/q.jsonl", "a\nb\nc\n")]);
        let refused = copy(from.path(), to.path(), "c").unwrap_err();
        assert!(refused.contains("no mark that c was copied"), "{refused}");
        assert_eq!(read(to.path(), "shard-0/responses/q.jsonl"), "a\nb\nc\n");
        assert!(!copied(to.path(), "c"));
        // Stopped after the rename: the `.writing` mark there, shard-0/
        // whole, no marker.
        let (from, to) = (scratch(), scratch());
        tree(from.path(), &[("responses/q.jsonl", "a\n")]);
        tree(
            to.path(),
            &[
                ("shard-0/responses/q.jsonl", "a\n"),
                (".meridian/.copied-from-c.writing", "1 files, 2 bytes\n"),
            ],
        );
        copy(from.path(), to.path(), "c").unwrap();
        assert!(copied(to.path(), "c"));
        assert_eq!(read(to.path(), "shard-0/responses/q.jsonl"), "a\n");
    }

    /// Review c5c H1: every folder keeps its mode, setgid bit and all, and
    /// every file its own, so what was the plugin's to write on stays so,
    /// and its private files stay private; `shard-0/` is the own claim's
    /// root as it was.
    #[test]
    fn copied_folders_and_files_keep_their_modes() {
        use std::os::unix::fs::PermissionsExt;
        let (from, to) = (scratch(), scratch());
        tree(
            from.path(),
            &[
                (".meridian/archive/index.json", "{}"),
                ("responses/d/x.jsonl", "a\n"),
            ],
        );
        let set = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        set(&from.path().join(".meridian/archive/index.json"), 0o600);
        set(&from.path().join(".meridian/archive"), 0o700);
        set(&from.path().join("responses"), 0o2770);
        set(from.path(), 0o2775);
        copy(from.path(), to.path(), "c").unwrap();
        let into = to.path().join(SHARD_0);
        assert_eq!(mode(&into), 0o2775, "shard-0/");
        assert_eq!(mode(&into.join(".meridian/archive")), 0o700);
        assert_eq!(mode(&into.join("responses")), 0o2770);
        assert_eq!(mode(&into.join(".meridian/archive/index.json")), 0o600);
        assert_eq!(
            mode(&into.join("responses/d/x.jsonl")) & 0o777,
            mode(&from.path().join("responses/d/x.jsonl")) & 0o777
        );
    }

    /// Review c5c M1: a pod's wait for a copy that never comes ends, saying
    /// which claim and that the instance's next launch makes the copy
    /// again, so the pod shows the failure; once the marker is there it
    /// passes at once.
    #[test]
    fn a_wait_for_a_copy_that_never_comes_ends_saying_so() {
        let root = scratch();
        let every = std::time::Duration::from_millis(10);
        let limit = std::time::Duration::from_millis(100);
        let said = wait(root.path(), "own-claim", every, limit).unwrap_err();
        assert!(
            said.contains("own-claim") && said.contains("launch"),
            "{said}"
        );
        std::fs::create_dir_all(root.path().join(".meridian")).unwrap();
        std::fs::write(marker(root.path(), "own-claim"), "").unwrap();
        assert_eq!(wait(root.path(), "own-claim", every, limit), Ok(()));
    }
}
