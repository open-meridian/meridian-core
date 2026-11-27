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
