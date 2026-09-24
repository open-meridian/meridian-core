use super::*;

const T0: i64 = 1_790_380_800_000_000_000;

fn with_ada() -> InMemory {
    let accounts = InMemory::default();
    accounts
        .put(&LocalAccount {
            name: "ada".into(),
            display_name: "Ada Park".into(),
            password_hash: hash_password("correct horse battery").expect("hashed"),
            groups: vec!["meridian-admins".into()],
            created_at_ns: T0,
            ..Default::default()
        })
        .expect("stored");
    accounts
}

#[test]
fn the_decoy_hash_is_a_hash() {
    // It is verified against when no account matches, so an unknown name
    // costs what a known one does. A decoy that does not parse fails fast,
    // restoring exactly the timing difference it exists to remove -- and
    // nothing else would notice, because the answer is the same either way.
    let empty = InMemory::default();
    let started = std::time::Instant::now();
    assert_eq!(
        authenticate(&empty, "nobody", "whatever", T0),
        Outcome::Refused
    );
    let unknown = started.elapsed();

    let accounts = with_ada();
    let started = std::time::Instant::now();
    authenticate(&accounts, "ada", "wrong", T0);
    let known = started.elapsed();

    // The same order of magnitude, not a constant time. What this catches is
    // the decoy being skipped, which is microseconds against milliseconds.
    assert!(
        unknown * 10 > known,
        "an unknown name cost {unknown:?} and a known one {known:?}: the decoy did not run"
    );
}

#[test]
fn the_right_password_signs_in_and_brings_its_groups() {
    match authenticate(&with_ada(), "ada", "correct horse battery", T0) {
        Outcome::SignedIn {
            subject,
            display_name,
            groups,
        } => {
            assert_eq!(subject, "local|ada");
            assert_eq!(display_name, "Ada Park");
            assert_eq!(groups, vec!["meridian-admins".to_string()]);
        }
        other => panic!("expected a sign-in, got {other:?}"),
    }
}

#[test]
fn a_name_that_is_not_there_is_refused_exactly_as_a_wrong_password_is() {
    let accounts = with_ada();
    // Identical values, deliberately: anything that told them apart would
    // tell a stranger who works here.
    assert_eq!(
        authenticate(&accounts, "ada", "not it", T0),
        authenticate(&accounts, "nobody", "not it", T0)
    );
}

#[test]
fn a_name_is_matched_however_it_was_typed() {
    let signed_in = authenticate(&with_ada(), "  Ada  ", "correct horse battery", T0);
    match signed_in {
        // And the subject is the stored form, so one person is one login
        // rather than two holding different permissions.
        Outcome::SignedIn { subject, .. } => assert_eq!(subject, "local|ada"),
        other => panic!("expected a sign-in, got {other:?}"),
    }
}

#[test]
fn enough_wrong_passwords_lock_the_account() {
    let accounts = with_ada();
    for _ in 0..LOCK_AFTER {
        assert_eq!(
            authenticate(&accounts, "ada", "not it", T0),
            Outcome::Refused
        );
    }

    // And now the right one does not work either, which is the point: a lock
    // the correct password lifts is not a lock.
    assert_eq!(
        authenticate(&accounts, "ada", "correct horse battery", T0),
        Outcome::Locked
    );
}

#[test]
fn a_lock_lifts_by_itself() {
    let accounts = with_ada();
    for _ in 0..LOCK_AFTER {
        authenticate(&accounts, "ada", "not it", T0);
    }

    // A time rather than a flag, so nothing has to remember to unlock it.
    let after = T0 + LOCK_FOR_NS + 1;
    assert!(matches!(
        authenticate(&accounts, "ada", "correct horse battery", after),
        Outcome::SignedIn { .. }
    ));
}

#[test]
fn signing_in_clears_the_count_so_a_typo_is_not_cumulative() {
    let accounts = with_ada();
    for _ in 0..(LOCK_AFTER - 1) {
        authenticate(&accounts, "ada", "not it", T0);
    }
    assert!(matches!(
        authenticate(&accounts, "ada", "correct horse battery", T0),
        Outcome::SignedIn { .. }
    ));

    // Without the clear, one more mistake next week locks the account on a
    // count nobody remembers accumulating.
    for _ in 0..(LOCK_AFTER - 1) {
        authenticate(&accounts, "ada", "not it", T0);
    }
    assert!(matches!(
        authenticate(&accounts, "ada", "correct horse battery", T0),
        Outcome::SignedIn { .. }
    ));
}

#[test]
fn an_empty_password_never_matches() {
    // No guard is needed, unlike LDAP, where an empty password is a bind a
    // server may answer with success. Asserted so the difference between the
    // two branches is written down rather than assumed.
    assert_eq!(authenticate(&with_ada(), "ada", "", T0), Outcome::Refused);
}

#[test]
fn a_store_that_cannot_count_refuses_rather_than_admits() {
    struct Unwritable(InMemory);
    impl Accounts for Unwritable {
        fn by_name(&self, name: &str) -> Result<Option<LocalAccount>, String> {
            self.0.by_name(name)
        }
        fn put(&self, account: &LocalAccount) -> Result<(), String> {
            self.0.put(account)
        }
        fn count_attempt(&self, _: &str, _: bool, _: i64) -> Result<(), String> {
            Err("the database is not there".into())
        }
    }

    // Even with the right password. A store that cannot be written cannot
    // lock, and admitting somebody then removes the limit at the moment it
    // is needed.
    let accounts = Unwritable(with_ada());
    assert!(matches!(
        authenticate(&accounts, "ada", "correct horse battery", T0),
        Outcome::Unavailable(_)
    ));
}
