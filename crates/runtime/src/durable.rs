//! A kept file written whole: sync and rename.
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
//! A deployment's key was written with `std::fs::write` and nothing synced
//! (LazyFS run 2, 2026-11-27): after a power loss the file was there and
//! empty, and the conductor never served again.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `data` at `path`, whole and synced before this returns; the old file
/// whole or the new one after a crash during it.
pub fn write_whole(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        )
    })?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    make_directory(parent)?;

    let (temporary, mut file) = temporary_beside(parent, name)?;
    let renamed = (|| {
        step("written")?;
        file.write_all(data)?;
        step("synced")?;
        file.sync_all()?;
        drop(file);
        step("renamed")?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(failed) = renamed {
        // The target is untouched; only this call's own temporary file goes.
        let _ = std::fs::remove_file(&temporary);
        return Err(failed);
    }
    step("directory synced")?;
    sync_directory(parent)
}

/// A new file beside the target under a name nobody else has: made
/// exclusively, so a name already there -- another writer's, or one a crash
/// left -- is passed over, never written into.
fn temporary_beside(
    parent: &Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<(PathBuf, std::fs::File)> {
    loop {
        let mut beside = std::ffi::OsString::from(".");
        beside.push(name);
        beside.push(format!(".{:016x}.part", rand::random::<u64>()));
        let temporary = parent.join(beside);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(there) if there.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(failed) => return Err(failed),
        }
    }
}

/// `directory` and any parent it lacks, each made synced into its parent.
fn make_directory(directory: &Path) -> std::io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    let parent = match directory.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
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
/// fails first where the test said to, so a test reads the order and what a
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
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// The steps [`super::write_whole`] went on to, on this thread.
        pub(crate) static STEPS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        /// The step it fails before, where a test names one.
        pub(crate) static FAIL_BEFORE: Cell<Option<&'static str>> = const { Cell::new(None) };
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
