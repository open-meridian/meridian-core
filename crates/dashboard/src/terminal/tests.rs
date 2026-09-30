use super::*;
use crate::clock::MINUTE_NS;
use crate::session::IDLE_NS;

const T0: i64 = 1_790_380_800_000_000_000;
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
/// RFC 7636, appendix B: the challenge for the verifier above.
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const BACK: &str = "http://127.0.0.1:53682/callback";

fn request() -> Request {
    check(BACK, CHALLENGE, "S256", "st-1").expect("a good request")
}

fn ada(at: i64) -> Person {
    Person {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        directory_groups: vec![],
        signed_in_at_ns: at,
    }
}

/// Open, sign in and confirm: the code a terminal would receive.
fn code_at(terminals: &Terminals, at: i64) -> String {
    let id = terminals.open(request(), at);
    let confirm = terminals.signed_in(&id, ada(at), at).expect("signed in");
    let (_, code) = terminals.decide(&id, &confirm, true, at).expect("decided");
    code.expect("confirmed")
}

#[test]
fn the_rfc_7636_example_verifies() {
    assert!(verifies(VERIFIER, CHALLENGE));
    assert!(!verifies(
        "not-the-verifier-not-the-verifier-not-the-ver",
        CHALLENGE
    ));
    assert!(!verifies("short", CHALLENGE), "RFC 7636 wants 43 to 128");
}

#[test]
fn only_a_loopback_callback_is_a_place_to_send_a_code() {
    for good in [
        "http://127.0.0.1:53682/callback",
        "http://[::1]:1/callback",
        "http://127.0.0.1:65535/callback",
    ] {
        assert!(loopback(good), "{good}");
    }
    for bad in [
        // The one that reads as the same thing and is not: a hosts file
        // decides where it goes.
        "http://localhost:53682/callback",
        "https://127.0.0.1:53682/callback",
        "http://127.0.0.1/callback",
        "http://127.0.0.1:0/callback",
        "http://127.0.0.1:053682/callback",
        "http://127.0.0.1:70000/callback",
        "http://127.0.0.1:53682/elsewhere",
        "http://127.0.0.1:53682/callback/",
        "http://127.0.0.1:53682/callback?next=x",
        "http://127.0.0.1.attacker.example:53682/callback",
        "http://attacker.example/127.0.0.1:53682/callback",
        "http://user@127.0.0.1:53682/callback",
    ] {
        assert!(!loopback(bad), "{bad}");
    }
}

#[test]
fn a_request_is_refused_before_anybody_signs_in_to_it() {
    assert!(check(BACK, CHALLENGE, "plain", "s").is_err(), "S256 only");
    assert!(check(BACK, "short", "S256", "s").is_err());
    assert!(check(BACK, CHALLENGE, "S256", "").is_err());
    assert!(
        check(BACK, CHALLENGE, "S256", "a&b=c").is_err(),
        "state is sent back unescaped"
    );
    assert!(check(BACK, CHALLENGE, "S256", &"s".repeat(257)).is_err());
}

#[tokio::test]
async fn a_confirmed_request_is_exchanged_once_for_a_session_counted_from_the_sign_in() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let issued = terminals
        .exchange(&code, VERIFIER, BACK, T0 + SECOND_NS)
        .await
        .unwrap()
        .expect("exchanged");
    assert_eq!(issued.subject, "local|ada");
    assert_eq!(issued.expires_at_ns, T0 + ABSOLUTE_NS, "from the sign-in");
    assert_eq!(
        terminals
            .find(&issued.session, T0 + 2 * SECOND_NS)
            .await
            .unwrap(),
        Ok(ada(T0))
    );
}

#[tokio::test]
async fn a_code_used_twice_ends_the_session_its_first_use_made() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let issued = terminals
        .exchange(&code, VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    assert!(terminals
        .exchange(&code, VERIFIER, BACK, T0)
        .await
        .unwrap()
        .is_err());
    assert_eq!(
        terminals.find(&issued.session, T0).await.unwrap(),
        Err(Refusal::Ended)
    );
}

#[tokio::test]
async fn a_code_is_spent_by_a_wrong_verifier_or_address_and_lapses_after_a_minute() {
    let terminals = Terminals::default();

    let code = code_at(&terminals, T0);
    let wrong = "x".repeat(43);
    assert!(terminals
        .exchange(&code, &wrong, BACK, T0)
        .await
        .unwrap()
        .is_err());
    assert!(
        terminals
            .exchange(&code, VERIFIER, BACK, T0)
            .await
            .unwrap()
            .is_err(),
        "one wrong guess spends it; there is no second"
    );

    let code = code_at(&terminals, T0);
    assert!(terminals
        .exchange(&code, VERIFIER, "http://127.0.0.1:1/callback", T0)
        .await
        .unwrap()
        .is_err());
    assert!(terminals
        .exchange(&code, VERIFIER, BACK, T0)
        .await
        .unwrap()
        .is_err());

    let code = code_at(&terminals, T0);
    assert!(terminals
        .exchange(&code, VERIFIER, BACK, T0 + CODE_NS + 1)
        .await
        .unwrap()
        .is_err());
}

#[test]
fn a_declined_request_gives_no_code_and_cannot_be_confirmed_after() {
    let terminals = Terminals::default();
    let id = terminals.open(request(), T0);
    let confirm = terminals.signed_in(&id, ada(T0), T0).unwrap();
    let (back, code) = terminals.decide(&id, &confirm, false, T0).unwrap();
    assert_eq!(back.state, "st-1");
    assert!(code.is_none());
    assert!(terminals.decide(&id, &confirm, true, T0).is_err());
}

#[test]
fn a_confirmation_needs_the_token_its_sign_in_was_given() {
    let terminals = Terminals::default();
    let id = terminals.open(request(), T0);
    assert!(
        terminals.decide(&id, "", true, T0).is_err(),
        "nobody has signed in yet"
    );
    let confirm = terminals.signed_in(&id, ada(T0), T0).unwrap();
    assert!(terminals.decide(&id, "guessed", true, T0).is_err());
    assert!(
        terminals.signed_in(&id, ada(T0), T0).is_none(),
        "and a second sign-in cannot take it over"
    );
    assert!(terminals.decide(&id, &confirm, true, T0).is_ok());
}

#[test]
fn a_request_lapses_after_ten_minutes() {
    let terminals = Terminals::default();
    let id = terminals.open(request(), T0);
    assert!(terminals
        .signed_in(&id, ada(T0), T0 + REQUEST_NS + 1)
        .is_none());
}

#[test]
fn requests_nobody_finishes_cannot_fill_the_dashboard() {
    let terminals = Terminals::default();
    let first = terminals.open(request(), T0);
    for n in 1..=MAX_REQUESTS as i64 {
        terminals.open(request(), T0 + n);
    }
    assert_eq!(terminals.lock().waiting.len(), MAX_REQUESTS);
    assert!(
        terminals.signed_in(&first, ada(T0), T0).is_none(),
        "the oldest went first"
    );
}

#[tokio::test]
async fn a_session_lapses_idle_or_old_and_says_which() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let idle = terminals
        .exchange(&code, VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminals
            .find(&idle.session, T0 + IDLE_NS + 1)
            .await
            .unwrap(),
        Err(Refusal::Lapsed)
    );
    assert_eq!(
        terminals
            .find(&idle.session, T0 + IDLE_NS + 2)
            .await
            .unwrap(),
        Err(Refusal::Lapsed),
        "and keeps saying so"
    );

    let code = code_at(&terminals, T0);
    let used = terminals
        .exchange(&code, VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    let mut now = T0;
    while now + 20 * MINUTE_NS <= T0 + ABSOLUTE_NS {
        now += 20 * MINUTE_NS;
        assert!(terminals.find(&used.session, now).await.unwrap().is_ok());
    }
    assert_eq!(
        terminals
            .find(&used.session, T0 + ABSOLUTE_NS + 1)
            .await
            .unwrap(),
        Err(Refusal::Lapsed)
    );
}

#[tokio::test]
async fn signing_out_and_an_admin_ending_them_both_read_as_ended() {
    let terminals = Terminals::default();
    let one = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    terminals.end(&one.session).await.unwrap();
    assert_eq!(
        terminals.find(&one.session, T0).await.unwrap(),
        Err(Refusal::Ended)
    );
    terminals.end(&one.session).await.unwrap();

    let two = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    let three = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminals.holders(T0).await.unwrap(),
        vec![("local|ada".into(), "Ada".into(), 2)]
    );
    assert_eq!(
        terminals.end_person("local|ada").await.unwrap(),
        2,
        "all of them, per person"
    );
    assert_eq!(
        terminals.find(&two.session, T0).await.unwrap(),
        Err(Refusal::Ended)
    );
    assert_eq!(
        terminals.find(&three.session, T0).await.unwrap(),
        Err(Refusal::Ended)
    );
    assert!(terminals.holders(T0).await.unwrap().is_empty());
    assert_eq!(
        terminals.find("never-issued", T0).await.unwrap(),
        Err(Refusal::Unknown)
    );
}

/// Terminals over a store the test can look into, as a dashboard's state
/// over the database it keeps sessions in.
fn over(store: &Arc<InMemory>) -> Terminals {
    Terminals::keeping(Arc::clone(store) as Arc<dyn TerminalSessions>)
}

#[tokio::test]
async fn only_a_hash_of_a_session_is_kept() {
    let store = Arc::new(InMemory::default());
    let terminals = over(&store);
    let issued = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    let (sessions, _) = store.keys();
    assert_eq!(sessions, vec![hashed(&issued.session)]);
    assert!(!sessions.contains(&issued.session));
}

#[tokio::test]
async fn a_sweep_forgets_what_is_past_its_bound_and_why_it_ended_once_that_no_longer_matters() {
    let store = Arc::new(InMemory::default());
    let terminals = over(&store);
    let open = terminals.open(request(), T0);
    terminals.through_provider("provider-state", &open);
    let issued = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    terminals.end(&issued.session).await.unwrap();

    terminals.sweep(T0 + REQUEST_NS + 1).await.unwrap();
    {
        let inner = terminals.lock();
        assert!(inner.waiting.is_empty());
        assert!(inner.by_provider_state.is_empty());
        assert!(inner.codes.is_empty());
    }
    assert_eq!(
        store.keys().1.len(),
        1,
        "why it ended, still within its 12 hours"
    );

    terminals.sweep(T0 + ABSOLUTE_NS + 1).await.unwrap();
    assert!(store.keys().1.is_empty());
}

#[tokio::test]
async fn a_sweep_removes_a_lapsed_session_and_keeps_saying_why() {
    let store = Arc::new(InMemory::default());
    let terminals = over(&store);
    let idle = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    terminals.sweep(T0 + IDLE_NS + 1).await.unwrap();
    assert!(store.keys().0.is_empty(), "the session is gone");
    assert_eq!(
        terminals
            .find(&idle.session, T0 + IDLE_NS + 2)
            .await
            .unwrap(),
        Err(Refusal::Lapsed),
        "and why is kept"
    );
}

#[tokio::test]
async fn an_expired_session_met_on_use_is_refused_and_removed_then() {
    let store = Arc::new(InMemory::default());
    let terminals = over(&store);
    let issued = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminals
            .find(&issued.session, T0 + IDLE_NS + 1)
            .await
            .unwrap(),
        Err(Refusal::Lapsed)
    );
    let (sessions, gone) = store.keys();
    assert!(
        sessions.is_empty(),
        "removed on use, not left for the sweep"
    );
    assert_eq!(gone, vec![hashed(&issued.session)]);
}

#[tokio::test]
async fn a_session_outlives_the_dashboard_state_that_issued_it() {
    // A restart, as far as a session can tell: everything held in memory
    // gone, the store the same.
    let store = Arc::new(InMemory::default());
    let before = over(&store);
    let issued = before
        .exchange(&code_at(&before, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    drop(before);

    let after = over(&store);
    assert_eq!(
        after.find(&issued.session, T0 + MINUTE_NS).await.unwrap(),
        Ok(ada(T0))
    );
    // The bounds carry across it too: still 12 hours from the sign-in, and
    // idle counted from the last use, whichever state saw it.
    assert_eq!(
        after
            .find(&issued.session, T0 + MINUTE_NS + IDLE_NS)
            .await
            .unwrap(),
        Ok(ada(T0))
    );
    assert_eq!(
        after
            .find(&issued.session, T0 + ABSOLUTE_NS + 1)
            .await
            .unwrap(),
        Err(Refusal::Lapsed)
    );
}

#[tokio::test]
async fn signing_out_on_one_dashboard_ends_the_session_on_another() {
    let store = Arc::new(InMemory::default());
    let one = over(&store);
    let other = over(&store);
    let issued = one
        .exchange(&code_at(&one, T0), VERIFIER, BACK, T0)
        .await
        .unwrap()
        .unwrap();
    other.end(&issued.session).await.unwrap();
    assert_eq!(
        one.find(&issued.session, T0).await.unwrap(),
        Err(Refusal::Ended)
    );
}

/// A store that cannot be asked, as a database that is away.
struct Away;

impl TerminalSessions for Away {
    fn keep(&self, _: &str, _: &Person, _: i64) -> Result<(), String> {
        Err("away".into())
    }
    fn find(&self, _: &str, _: i64) -> Result<Result<Person, Refusal>, String> {
        Err("away".into())
    }
    fn is_live(&self, _: &str, _: i64) -> Result<bool, String> {
        Err("away".into())
    }
    fn end(&self, _: &str) -> Result<(), String> {
        Err("away".into())
    }
    fn end_person(&self, _: &str) -> Result<usize, String> {
        Err("away".into())
    }
    fn holders(&self, _: i64) -> Result<Vec<(String, String, usize)>, String> {
        Err("away".into())
    }
    fn sweep(&self, _: i64) -> Result<(), String> {
        Err("away".into())
    }
}

#[tokio::test]
async fn a_store_that_cannot_be_asked_is_unavailable_and_never_a_refusal() {
    let terminals = Terminals::keeping(Arc::new(Away));
    let code = code_at(&terminals, T0);
    assert!(terminals.exchange(&code, VERIFIER, BACK, T0).await.is_err());
    assert!(terminals.find("anything", T0).await.is_err());
}

#[test]
fn a_moment_is_written_as_rfc_3339() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(T0), "2026-09-26T00:00:00Z");
    assert_eq!(rfc3339(951_782_400 * SECOND_NS), "2000-02-29T00:00:00Z");
    assert_eq!(
        rfc3339(T0 + ABSOLUTE_NS + 61 * SECOND_NS + 999_999_999),
        "2026-09-26T12:01:01Z"
    );
}
