use super::testing::{folder, names, AT_STEP, FAIL_BEFORE, STEPS};
use super::{create_whole, write_whole};

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

/// The deployment's key goes through it, made once: a start that fails
/// before the key has its name leaves no key file, where `std::fs::write`
/// left one, and one that succeeds took every step. The next start reads the
/// key this one made.
#[test]
fn the_deployments_key_is_written_whole() {
    let dir = folder("key");
    let path = dir.join("keys").join("key.pem");
    let path_text = path.to_str().unwrap();

    FAIL_BEFORE.with(|at| at.set(Some("linked")));
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
        [
            "written",
            "synced",
            "linked",
            "unlinked",
            "directory synced"
        ]
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

/// A panic part-way leaves nothing of this call's beside the target: the
/// temporary file of a key is a whole private key.
#[test]
fn a_panic_before_the_rename_leaves_no_temporary_file() {
    let dir = folder("panics");
    let path = dir.join("kept");
    std::fs::write(&path, b"the old file").unwrap();

    AT_STEP.with(|at| *at.borrow_mut() = Some(("synced", Box::new(|| panic!("a test's panic")))));
    let unwound = std::panic::catch_unwind(|| write_whole(&path, b"the new file"));
    AT_STEP.with(|at| *at.borrow_mut() = None);

    assert!(unwound.is_err(), "the panic came through");
    assert_eq!(std::fs::read(&path).unwrap(), b"the old file");
    assert_eq!(names(&dir), ["kept"], "no temporary file is left");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What a killed start left beside the key is removed by the next one, and
/// only that: the key's own temporary files, by their exact names.
#[test]
fn a_start_removes_the_temporary_files_an_earlier_one_left_beside_the_key() {
    let dir = folder("stale");
    let path = dir.join("key.pem");
    for left in [
        ".key.pem.0123456789abcdef.part",
        ".key.pem.part",
        ".key.enrolled.0123456789abcdef.part",
        "key.pem.0123456789abcdef.part",
    ] {
        std::fs::write(dir.join(left), b"left by a crash").unwrap();
    }

    assert!(crate::key_at(path.to_str().unwrap()).is_ok());

    assert_eq!(
        names(&dir),
        [
            ".key.enrolled.0123456789abcdef.part",
            ".key.pem.part",
            "key.pem",
            "key.pem.0123456789abcdef.part",
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A symbolic link at the target is refused: a rename would replace the link
/// and leave what it pointed at, or where it pointed, without the file.
#[cfg(unix)]
#[test]
fn a_target_that_is_a_symbolic_link_is_refused_and_left() {
    let dir = folder("link");
    let path = dir.join("kept");

    // Dangling first, then pointing at a file.
    for there in [false, true] {
        let elsewhere = dir.join("elsewhere");
        if there {
            std::fs::write(&elsewhere, b"the old file").unwrap();
        }
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

        let failed = write_whole(&path, b"the new file").unwrap_err();

        assert!(
            failed.to_string().contains(path.to_str().unwrap()),
            "the error names the path: {failed}"
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), elsewhere, "the link");
        if there {
            assert_eq!(std::fs::read(&elsewhere).unwrap(), b"the old file");
            assert_eq!(names(&dir), ["elsewhere", "kept"]);
        } else {
            assert_eq!(names(&dir), ["kept"]);
        }
        std::fs::remove_file(&path).unwrap();
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two starts that both found no key: the one that comes second uses the key
/// the first put there, and leaves it. Renaming over it replaced a key the
/// first was already using.
#[test]
fn a_start_that_finds_another_key_just_placed_uses_that_key() {
    let dir = folder("two-writers");
    let path = dir.join("key.pem");
    let path_text = path.to_str().unwrap();
    let other = crate::DeploymentKey::generate();
    let other_pem = other.private_key_pem().unwrap();

    let (target, placed) = (path.clone(), other_pem.clone());
    AT_STEP.with(|at| {
        *at.borrow_mut() = Some((
            "linked",
            Box::new(move || std::fs::write(&target, placed.as_bytes()).unwrap()),
        ))
    });
    let returned = crate::key_at(path_text);
    AT_STEP.with(|at| *at.borrow_mut() = None);
    let Ok(returned) = returned else {
        panic!("the key on disk is a good one");
    };

    // Compared and not printed: a failure here must not put a key in a log.
    assert!(
        std::fs::read(&path).unwrap() == other_pem.as_bytes(),
        "the other writer's key is unchanged"
    );
    assert!(
        returned.public_key_pem().unwrap() == other.public_key_pem().unwrap(),
        "the key returned is the key on disk"
    );
    assert_eq!(names(&dir), ["key.pem"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_made_once_is_synced_before_it_is_linked_and_its_directory_after() {
    let dir = folder("create-order");
    let path = dir.join("a").join("kept");

    let on_disk = create_whole(&path, b"new").unwrap();

    assert_eq!(
        STEPS.with(|steps| steps.borrow().clone()),
        [
            "written",
            "synced",
            "linked",
            "unlinked",
            "directory synced"
        ]
    );
    assert_eq!(on_disk, b"new");
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert_eq!(names(&dir.join("a")), ["kept"], "no temporary file is left");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let kept = std::fs::metadata(&path).unwrap();
        assert_eq!(
            kept.permissions().mode() & 0o777,
            0o600,
            "its owner's alone"
        );
        assert_eq!(kept.nlink(), 1, "under one name");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The file that is there wins, whenever it came: before the call, or between
/// the sync and the link.
#[test]
fn a_file_already_there_is_returned_and_left() {
    let dir = folder("create-there");
    let path = dir.join("kept");
    std::fs::write(&path, b"the first writer's").unwrap();

    assert_eq!(
        create_whole(&path, b"the second writer's").unwrap(),
        b"the first writer's"
    );

    assert_eq!(std::fs::read(&path).unwrap(), b"the first writer's");
    assert_eq!(names(&dir), ["kept"], "no temporary file is left");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn making_a_file_once_that_fails_or_panics_leaves_nothing() {
    for before in ["written", "synced", "linked"] {
        let dir = folder("create-fails");
        let path = dir.join("kept");

        FAIL_BEFORE.with(|at| at.set(Some(before)));
        let failed = create_whole(&path, b"new");
        FAIL_BEFORE.with(|at| at.set(None));

        assert!(failed.is_err(), "failing before {before} is an error");
        assert_eq!(names(&dir), Vec::<String>::new(), "failing before {before}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    let dir = folder("create-panics");
    let path = dir.join("kept");
    AT_STEP.with(|at| *at.borrow_mut() = Some(("synced", Box::new(|| panic!("a test's panic")))));
    let unwound = std::panic::catch_unwind(|| create_whole(&path, b"new"));
    AT_STEP.with(|at| *at.borrow_mut() = None);
    assert!(unwound.is_err(), "the panic came through");
    assert_eq!(names(&dir), Vec::<String>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn making_a_file_once_refuses_a_symbolic_link_and_leaves_it() {
    let dir = folder("create-link");
    let path = dir.join("kept");
    let nowhere = dir.join("nowhere");
    std::os::unix::fs::symlink(&nowhere, &path).unwrap();

    let failed = create_whole(&path, b"new").unwrap_err();

    assert!(
        failed.to_string().contains(path.to_str().unwrap()),
        "the error names the path: {failed}"
    );
    assert_eq!(std::fs::read_link(&path).unwrap(), nowhere, "the link");
    assert_eq!(names(&dir), ["kept"]);
    std::fs::remove_dir_all(&dir).unwrap();
}
