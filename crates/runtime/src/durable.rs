//! A kept file written whole: synced, then given its name.
//!
//! [`write_whole`] puts `data` at `path` so that once it returns the file is
//! the new one, whole, whatever happens next: a crash, a lost node, or a
//! filesystem that keeps only what it was told to sync. A crash while it runs
//! leaves the old file whole or the new one, never a mix, an empty file or
//! one of zeros. It is the SDK's `write_whole` (`meridian/_durable.py`), for
//! the files this deployment's own components keep.
//!
//! Each step is needed:
//!
//! 1. The data goes to a temporary file beside the target, created
//!    exclusively under a name of its own, so two writers of one file never
//!    write into one temporary file.
//! 2. The temporary file is synced, so its data is on disk before any name
//!    points at it.
//! 3. It is renamed over the target, which is atomic: a reader sees the old
//!    file or the new.
//! 4. The directory is synced, so the rename itself is on disk before the
//!    call returns. A directory this makes is synced into its parent the same
//!    way.
//!
//! The file is made 0o600, readable and writable by its owner only: what a
//! component keeps is its key or a record of its own. The rename keeps that
//! mode, so a target that was wider ends 0o600 too.
//!
//! What it refuses and what it leaves:
//!
//! - A target that is a symbolic link is an error. A rename would replace the
//!   link and leave the place it pointed at without the file.
//! - A directory sync that fails is an error, never passed over: where a
//!   filesystem refuses one, the call fails rather than promise what was not
//!   kept.
//! - The temporary file goes on every way out before the rename, an error or
//!   a panic. A killed process cannot remove its own;
//!   [`remove_stale_parts`] removes what one left.
//!
//! [`create_whole`] is the same for a file made once and never replaced: it
//! gives the synced file its name by a hard link, which fails where the name
//! is taken, and returns what is on disk.
//!
//! A deployment's key was written with `std::fs::write` and nothing synced
//! (LazyFS run 2, 2026-11-27): after a power loss the file was there and
//! empty, and the conductor never served again.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `data` at `path`, whole and synced before this returns; the old file
/// whole or the new one after a crash during it.
pub fn write_whole(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let (parent, name) = placed(path)?;
    let (temporary, mut file) = Temporary::beside(parent, name)?;
    step("written")?;
    file.write_all(data)?;
    step("synced")?;
    file.sync_all()?;
    drop(file);
    step("renamed")?;
    std::fs::rename(&temporary.path, path)?;
    temporary.renamed();
    step("directory synced")?;
    sync_directory(parent)
}

/// `data` at `path` only if nothing is there: for a file made once and never
/// replaced, such as a key. What it returns is what is on disk at `path`:
/// `data` where this call made the file, and the file's own bytes where
/// another writer made it first. That writer's file is left as it is, and the
/// caller uses what is returned, never what it meant to write.
///
/// The steps are [`write_whole`]'s with one changed: the synced temporary
/// file is given the target's name by a hard link, which fails where the name
/// is taken, in place of a rename, which would replace it. The temporary name
/// is then removed and the directory synced.
///
/// A filesystem without hard links (EPERM, ENOTSUP) is an error. There is no
/// falling back to a rename, which is the replacing this is here to prevent.
pub fn create_whole(path: &Path, data: &[u8]) -> std::io::Result<Vec<u8>> {
    let (parent, name) = placed(path)?;
    let (temporary, mut file) = Temporary::beside(parent, name)?;
    step("written")?;
    file.write_all(data)?;
    step("synced")?;
    file.sync_all()?;
    drop(file);
    step("linked")?;
    let on_disk = match std::fs::hard_link(&temporary.path, path) {
        Ok(()) => data.to_vec(),
        Err(there) if there.kind() == std::io::ErrorKind::AlreadyExists => std::fs::read(path)?,
        Err(failed) => {
            return Err(std::io::Error::new(
                failed.kind(),
                format!(
                    "{} could not be given its name by a hard link: {failed}",
                    path.display()
                ),
            ))
        }
    };
    step("unlinked")?;
    temporary.remove()?;
    step("directory synced")?;
    sync_directory(parent)?;
    Ok(on_disk)
}

/// Removes the temporary files an earlier, killed call left beside `path`:
/// the names `.<name>.<anything>.part`, regular files only, and nothing else.
/// For a key each is a whole private key that no deployment uses.
///
/// Called before a write, never during one of another writer's: a writer
/// whose temporary file is taken from under it fails, with the target as it
/// was.
pub fn remove_stale_parts(path: &Path) -> std::io::Result<()> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let parent = parent_of(path);
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(none) if none.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(failed) => return Err(failed),
    };
    let (starts, ends) = (format!(".{name}."), ".part");
    let mut removed = false;
    for entry in entries {
        let entry = entry?;
        let beside = entry.file_name();
        let Some(beside) = beside.to_str() else {
            continue;
        };
        let stale = beside.len() > starts.len() + ends.len()
            && beside.starts_with(&starts)
            && beside.ends_with(ends)
            && entry.file_type()?.is_file();
        if !stale {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed = true,
            Err(gone) if gone.kind() == std::io::ErrorKind::NotFound => {}
            Err(failed) => return Err(failed),
        }
    }
    if removed {
        sync_directory(parent)?;
    }
    Ok(())
}

/// The folder a file is in: `.` for a bare name.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The target's folder, made if it was not there, and its name; an error
/// where the target is a symbolic link.
fn placed(path: &Path) -> std::io::Result<(&Path, &std::ffi::OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        )
    })?;
    if std::fs::symlink_metadata(path).is_ok_and(|found| found.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link, which is neither replaced nor written through",
                path.display()
            ),
        ));
    }
    let parent = parent_of(path);
    make_directory(parent)?;
    Ok((parent, name))
}

/// A call's temporary file, removed when the call ends any way but with the
/// file under its final name: an error, or a panic.
struct Temporary {
    path: PathBuf,
    there: bool,
}

impl Temporary {
    /// A new file beside the target under a name nobody else has: made
    /// exclusively, so a name already there -- another writer's, or one a
    /// crash left -- is passed over, never written into.
    fn beside(parent: &Path, name: &std::ffi::OsStr) -> std::io::Result<(Self, std::fs::File)> {
        loop {
            let mut beside = std::ffi::OsString::from(".");
            beside.push(name);
            beside.push(format!(".{:016x}.part", rand::random::<u64>()));
            let path = parent.join(beside);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((Self { path, there: true }, file)),
                Err(there) if there.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(failed) => return Err(failed),
            }
        }
    }

    /// It carries the target's name now, and is no longer this call's to
    /// remove.
    fn renamed(mut self) {
        self.there = false;
    }

    /// Removes it once the target's name is a second one on the same file,
    /// or is another writer's.
    fn remove(mut self) -> std::io::Result<()> {
        self.there = false;
        std::fs::remove_file(&self.path)
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if self.there {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `directory` and any parent it lacks, each made synced into its parent.
fn make_directory(directory: &Path) -> std::io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    let parent = parent_of(directory);
    make_directory(parent)?;
    match std::fs::create_dir(directory) {
        Ok(()) => sync_directory(parent),
        Err(there) if there.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(failed) => Err(failed),
    }
}

/// The directory's own entries on disk: a rename or a new name in it is not
/// kept until this.
#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Nothing outside a test. In one, the step is recorded on this thread and
/// fails first where the test said to, or runs what the test gave it, so a test reads the order and what a
/// failure at each step leaves.
#[cfg(not(test))]
#[inline]
fn step(_about_to: &'static str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn step(about_to: &'static str) -> std::io::Result<()> {
    if testing::FAIL_BEFORE.with(|at| at.get()) == Some(about_to) {
        return Err(std::io::Error::other(format!(
            "a test failed this before {about_to}"
        )));
    }
    testing::STEPS.with(|steps| steps.borrow_mut().push(about_to));
    let reached = testing::AT_STEP.with(|at| {
        let mut at = at.borrow_mut();
        match at.as_ref() {
            Some((step, _)) if *step == about_to => at.take(),
            _ => None,
        }
    });
    if let Some((_, run)) = reached {
        run();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// The steps [`super::write_whole`] or [`super::create_whole`] went on to, on this thread.
        pub(crate) static STEPS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        /// The step it fails before, where a test names one.
        pub(crate) static FAIL_BEFORE: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// What a test runs as a step is reached, once: another writer's
        /// move, or a panic.
        #[allow(clippy::type_complexity)]
        pub(crate) static AT_STEP: RefCell<Option<(&'static str, Box<dyn FnOnce()>)>> =
            const { RefCell::new(None) };
    }

    /// A folder of this test's own.
    pub(crate) fn folder(of: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "meridian-durable-{of}-{}-{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The names in a folder, sorted.
    pub(crate) fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests;
