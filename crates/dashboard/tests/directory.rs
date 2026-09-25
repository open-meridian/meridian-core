//! Signing in against a real LDAP server. Decision 018.
//!
//! Against a real one, because what is worth proving here is the server's
//! behaviour rather than ours: that a wrong password is refused by the
//! directory rather than compared by us, that `memberOf` arrives only when
//! the overlay is loaded, and that a filter value cannot widen a search. A
//! stand-in would assert what we already believe.
//!
//! Run by `make test-directory`, which starts the server and loads the tree.
//! They fail loudly when it is missing rather than skipping: a test that
//! quietly does not run is how a gate reports success without doing its job.

use meridian_dashboard::directory::{Directory, Failure};

const PEOPLE: &str = "ou=people,dc=example,dc=org";
const ALICE: &str = "uid=alice,ou=people,dc=example,dc=org";
const GROUP_A: &str = "cn=ldap-group-a,ou=groups,dc=example,dc=org";
const GROUP_B: &str = "cn=ldap-group-b,ou=groups,dc=example,dc=org";

fn directory() -> Directory {
    let url = std::env::var("MERIDIAN_TEST_LDAP_URL").expect(
        "MERIDIAN_TEST_LDAP_URL is not set. These tests need a real directory; \
         run them with `make test-directory`.",
    );
    Directory {
        servers: vec![url],
        base_dn: PEOPLE.into(),
        bind_dn: "cn=admin,dc=example,dc=org".into(),
        bind_password: "ldap-admin-dev-only".into(),
        user_filter: "(uid={})".into(),
        group_attribute: "memberOf".into(),
    }
}

#[tokio::test]
async fn a_person_signs_in_and_brings_their_groups() {
    let found = directory()
        .authenticate("bob", "bobpass")
        .await
        .expect("bob signs in");

    // The DN, because it is what survives a rename. Half of a login.
    assert_eq!(found.subject, "uid=bob,ou=people,dc=example,dc=org");
    assert_eq!(found.name, "Bob Ldap");
    assert_eq!(found.email, "bob@ldap.example.org");

    let mut groups = found.groups.clone();
    groups.sort();
    assert_eq!(groups, vec![GROUP_A.to_string(), GROUP_B.to_string()]);
}

#[tokio::test]
async fn somebody_in_one_group_brings_one() {
    let found = directory()
        .authenticate("alice", "alicepass")
        .await
        .expect("alice signs in");

    assert_eq!(found.subject, ALICE);
    assert_eq!(found.groups, vec![GROUP_A.to_string()]);
}

#[tokio::test]
async fn a_wrong_password_is_refused_by_the_directory() {
    assert_eq!(
        directory().authenticate("alice", "not-her-password").await,
        Err(Failure::Refused)
    );
}

#[tokio::test]
async fn an_empty_password_does_not_sign_anybody_in() {
    // Read this one for what it does not prove. An empty password on a simple
    // bind is an unauthenticated bind (RFC 4513), which some servers answer
    // with success -- so a sign-in passing one through admits anybody who
    // types a name. **This server is not one of them**: removing our own
    // guard leaves this test passing, because OpenLDAP 2.6 refuses the bind
    // itself.
    //
    // So this asserts the outcome and not the reason, and the guard is proven
    // by the unit test beside it, which has no server to fall back on. Kept
    // here anyway: it is the behaviour a reader expects to find asserted, and
    // its absence would be read as nobody having thought about it.
    assert_eq!(
        directory().authenticate("alice", "").await,
        Err(Failure::Refused)
    );
}

#[tokio::test]
async fn a_name_that_is_not_there_reads_the_same_as_a_wrong_password() {
    // Deliberately identical. A message telling them apart enumerates a
    // firm's staff for anybody who can reach the sign-in page.
    assert_eq!(
        directory().authenticate("nobody", "whatever").await,
        Err(Failure::Refused)
    );
}

#[tokio::test]
async fn a_filter_cannot_be_opened_from_the_sign_in_form() {
    // `*` unescaped matches every person in the tree. Escaped it matches
    // somebody literally called `*`, of whom there are none, so it reads as
    // an unknown name -- which is what it is.
    assert_eq!(
        directory().authenticate("*", "alicepass").await,
        Err(Failure::Refused)
    );
    assert_eq!(
        directory().authenticate("alice)(uid=*", "alicepass").await,
        Err(Failure::Refused)
    );
}

#[tokio::test]
async fn our_own_credentials_being_wrong_is_our_problem_and_says_so() {
    // Distinct from Refused deliberately: nobody signing in can do anything
    // about it, and an operator needs to know which half broke.
    let mut ours = directory();
    ours.bind_password = "not-the-service-password".into();

    match ours.authenticate("alice", "alicepass").await {
        Err(Failure::NotOurs(_)) => {}
        other => panic!("expected the deployment's own bind to fail, got {other:?}"),
    }
}

#[tokio::test]
async fn no_server_answering_is_not_a_refusal() {
    let nowhere = Directory {
        servers: vec!["ldap://127.0.0.1:1".into()],
        ..directory()
    };

    match nowhere.authenticate("alice", "alicepass").await {
        Err(Failure::Unreachable(_)) => {}
        other => panic!("expected unreachable, got {other:?}"),
    }
}

// The wizard's check (W7.4): the firm's answer, tested before it is applied.

#[tokio::test]
async fn a_right_answer_passes_the_check() {
    directory()
        .check()
        .await
        .expect("the answer the e2e tree was made for");
}

#[tokio::test]
async fn a_wrong_bind_password_fails_the_check_as_ours() {
    let mut wrong = directory();
    wrong.bind_password = "not-the-service-password".into();
    match wrong.check().await {
        Err(Failure::NotOurs(_)) => {}
        other => panic!("expected the service bind to fail, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_bind_password_fails_the_check_before_anything_is_dialled() {
    // A server that does not exist, so a pass could only come from binding
    // anonymously somewhere -- and the refusal has to come first.
    let anonymous = Directory {
        servers: vec!["ldap://127.0.0.1:1".into()],
        bind_password: String::new(),
        ..directory()
    };
    match anonymous.check().await {
        Err(Failure::NotOurs(_)) => {}
        other => panic!("expected an empty password to be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn a_base_that_is_not_there_fails_the_check() {
    let elsewhere = Directory {
        base_dn: "ou=nobody,dc=example,dc=org".into(),
        ..directory()
    };
    match elsewhere.check().await {
        Err(Failure::Confused(_)) => {}
        other => panic!("expected a missing base to fail, got {other:?}"),
    }
}

#[tokio::test]
async fn no_server_answering_fails_the_check() {
    let nowhere = Directory {
        servers: vec!["ldap://127.0.0.1:1".into()],
        ..directory()
    };
    match nowhere.check().await {
        Err(Failure::Unreachable(_)) => {}
        other => panic!("expected unreachable, got {other:?}"),
    }
}
