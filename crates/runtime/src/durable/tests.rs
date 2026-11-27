use super::testing::{folder, names, FAIL_BEFORE, STEPS};
use super::write_whole;

/// The brief's order, which is the whole of the helper: the data synced
/// before any name points at it, and the rename synced before the call
/// returns. A power loss cannot be run in a unit test (the LazyFS run is that
/// proof), so the order is what is held here.
#[test]
fn the_file_is_synced_before_it_is_renamed_and_its_directory_after() {
    let dir = folder("order");
    write_whole(&dir.join("kept"), b"new").unwrap();
    assert_eq!(
        STEPS.with(|steps| steps.borrow().clone()),
        ["written", "synced", "renamed", "directory synced"]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_new_file_is_whole_once_it_returns_and_nothing_is_left_beside_it() {
    let dir = folder("whole");
    let path = dir.join("kept");
    std::fs::write(&path, b"the old file, longer than the new").unwrap();

    write_whole(&path, b"the new file").unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"the new file");
    assert_eq!(names(&dir), ["kept"], "no temporary file is left");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "its owner's alone");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A write that fails anywhere before the rename leaves the old file as it
/// was, whole, and none of its own.
#[test]
fn a_write_that_fails_before_the_rename_leaves_the_old_file_whole() {
    for before in ["written", "synced", "renamed"] {
        let dir = folder("fails");
        let path = dir.join("kept");
        std::fs::write(&path, b"the old file").unwrap();

        FAIL_BEFORE.with(|at| at.set(Some(before)));
        let failed = write_whole(&path, b"the new file");
        FAIL_BEFORE.with(|at| at.set(None));

        assert!(failed.is_err(), "failing before {before} is an error");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"the old file",
            "failing before {before}"
        );
        assert_eq!(names(&dir), ["kept"], "failing before {before}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// And where there was no file, there is none: never an empty one.
#[test]
fn a_first_write_that_fails_before_the_rename_leaves_no_file() {
    let dir = folder("first");
    FAIL_BEFORE.with(|at| at.set(Some("renamed")));
    let failed = write_whole(&dir.join("kept"), b"the new file");
    FAIL_BEFORE.with(|at| at.set(None));
    assert!(failed.is_err());
    assert_eq!(names(&dir), Vec::<String>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_folder_it_makes_holds_the_file() {
    let dir = folder("makes");
    let path = dir.join("a").join("b").join("kept");
    write_whole(&path, b"new").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The deployment's key goes through it: a start that fails before the
/// rename leaves no key file, where `std::fs::write` left one, and one that
/// succeeds took every step. The next start reads the key this one made.
#[test]
fn the_deployments_key_is_written_whole() {
    let dir = folder("key");
    let path = dir.join("keys").join("key.pem");
    let path_text = path.to_str().unwrap();

    FAIL_BEFORE.with(|at| at.set(Some("renamed")));
    let failed = crate::key_at(path_text);
    FAIL_BEFORE.with(|at| at.set(None));
    assert!(
        failed.is_err(),
        "the key that could not be kept is not used"
    );
    assert_eq!(
        names(&dir.join("keys")),
        Vec::<String>::new(),
        "no key file, whole or empty, from a write that failed"
    );

    STEPS.with(|steps| steps.borrow_mut().clear());
    let made = crate::key_at(path_text).unwrap();
    assert_eq!(
        STEPS.with(|steps| steps.borrow().clone()),
        ["written", "synced", "renamed", "directory synced"]
    );
    assert_eq!(names(&dir.join("keys")), ["key.pem"]);
    let read = crate::key_at(path_text).unwrap();
    assert_eq!(
        made.public_key_pem().unwrap(),
        read.public_key_pem().unwrap(),
        "the next start reads the key this one made"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A key that is there and damaged is an error, never a new key: a new key
/// is another deployment. One byte gone bad (not text any more) used to read
/// as "no key", and the start replaced it.
#[test]
fn a_key_with_a_bad_byte_is_an_error_and_stays_as_it_is() {
    let dir = folder("bad-byte");
    let path = dir.join("key.pem");
    let path_text = path.to_str().unwrap();
    assert!(crate::key_at(path_text).is_ok());
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] = 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let Err(said) = crate::key_at(path_text) else {
        panic!("a damaged key was replaced by a new one");
    };

    assert!(said.contains(path_text), "the error names the path: {said}");
    assert!(!said.contains("PRIVATE KEY"), "and never the key");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "the bytes on disk");
    assert_eq!(names(&dir), ["key.pem"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A key that is there and may not be read is an error too, and the file is
/// the one that was there. Skipped where the read is allowed anyway (root).
#[cfg(unix)]
#[test]
fn a_key_that_may_not_be_read_is_an_error_and_stays_the_same_file() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let dir = folder("unreadable");
    let path = dir.join("key.pem");
    let path_text = path.to_str().unwrap();
    assert!(crate::key_at(path_text).is_ok());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let before = std::fs::metadata(&path).unwrap();

    let Err(said) = crate::key_at(path_text) else {
        panic!("a key that could not be read was replaced by a new one");
    };

    assert!(said.contains(path_text), "the error names the path: {said}");
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(after.ino(), before.ino(), "the same file");
    assert_eq!(after.len(), before.len());
    assert_eq!(names(&dir), ["key.pem"]);
    std::fs::remove_dir_all(&dir).unwrap();
}
