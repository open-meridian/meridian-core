use super::*;

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

#[test]
fn a_confirmed_request_is_exchanged_once_for_a_session_counted_from_the_sign_in() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let issued = terminals
        .exchange(&code, VERIFIER, BACK, T0 + SECOND_NS)
        .expect("exchanged");
    assert_eq!(issued.subject, "local|ada");
    assert_eq!(issued.expires_at_ns, T0 + ABSOLUTE_NS, "from the sign-in");
    assert_eq!(
        terminals.find(&issued.session, T0 + 2 * SECOND_NS),
        Ok(ada(T0))
    );
}

#[test]
fn a_code_used_twice_ends_the_session_its_first_use_made() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let issued = terminals.exchange(&code, VERIFIER, BACK, T0).unwrap();
    assert!(terminals.exchange(&code, VERIFIER, BACK, T0).is_err());
    assert_eq!(terminals.find(&issued.session, T0), Err(Refusal::Ended));
}

#[test]
fn a_code_is_spent_by_a_wrong_verifier_or_address_and_lapses_after_a_minute() {
    let terminals = Terminals::default();

    let code = code_at(&terminals, T0);
    let wrong = "x".repeat(43);
    assert!(terminals.exchange(&code, &wrong, BACK, T0).is_err());
    assert!(
        terminals.exchange(&code, VERIFIER, BACK, T0).is_err(),
        "one wrong guess spends it; there is no second"
    );

    let code = code_at(&terminals, T0);
    assert!(terminals
        .exchange(&code, VERIFIER, "http://127.0.0.1:1/callback", T0)
        .is_err());
    assert!(terminals.exchange(&code, VERIFIER, BACK, T0).is_err());

    let code = code_at(&terminals, T0);
    assert!(terminals
        .exchange(&code, VERIFIER, BACK, T0 + CODE_NS + 1)
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

#[test]
fn a_session_lapses_idle_or_old_and_says_which() {
    let terminals = Terminals::default();
    let code = code_at(&terminals, T0);
    let idle = terminals.exchange(&code, VERIFIER, BACK, T0).unwrap();
    assert_eq!(
        terminals.find(&idle.session, T0 + IDLE_NS + 1),
        Err(Refusal::Lapsed)
    );
    assert_eq!(
        terminals.find(&idle.session, T0 + IDLE_NS + 2),
        Err(Refusal::Lapsed),
        "and keeps saying so"
    );

    let code = code_at(&terminals, T0);
    let used = terminals.exchange(&code, VERIFIER, BACK, T0).unwrap();
    let mut now = T0;
    while now + 20 * MINUTE_NS <= T0 + ABSOLUTE_NS {
        now += 20 * MINUTE_NS;
        assert!(terminals.find(&used.session, now).is_ok());
    }
    assert_eq!(
        terminals.find(&used.session, T0 + ABSOLUTE_NS + 1),
        Err(Refusal::Lapsed)
    );
}

#[test]
fn signing_out_and_an_admin_ending_them_both_read_as_ended() {
    let terminals = Terminals::default();
    let one = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .unwrap();
    terminals.end(&one.session);
    assert_eq!(terminals.find(&one.session, T0), Err(Refusal::Ended));
    terminals.end(&one.session);

    let two = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .unwrap();
    let three = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .unwrap();
    assert_eq!(
        terminals.holders(T0),
        vec![("local|ada".into(), "Ada".into(), 2)]
    );
    assert_eq!(
        terminals.end_person("local|ada"),
        2,
        "all of them, per person"
    );
    assert_eq!(terminals.find(&two.session, T0), Err(Refusal::Ended));
    assert_eq!(terminals.find(&three.session, T0), Err(Refusal::Ended));
    assert!(terminals.holders(T0).is_empty());
    assert_eq!(terminals.find("never-issued", T0), Err(Refusal::Unknown));
}

#[test]
fn only_a_hash_of_a_session_is_held() {
    let terminals = Terminals::default();
    let issued = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .unwrap();
    let inner = terminals.lock();
    assert!(!inner.sessions.contains_key(&issued.session));
    assert!(inner.sessions.contains_key(&hashed(&issued.session)));
}

#[test]
fn a_sweep_forgets_what_is_past_its_bound_and_why_it_ended_once_that_no_longer_matters() {
    let terminals = Terminals::default();
    let open = terminals.open(request(), T0);
    terminals.through_provider("provider-state", &open);
    let issued = terminals
        .exchange(&code_at(&terminals, T0), VERIFIER, BACK, T0)
        .unwrap();
    terminals.end(&issued.session);

    terminals.sweep(T0 + REQUEST_NS + 1);
    let inner = terminals.lock();
    assert!(inner.waiting.is_empty());
    assert!(inner.by_provider_state.is_empty());
    assert!(inner.codes.is_empty());
    assert_eq!(inner.gone.len(), 1, "still within its 12 hours");
    drop(inner);

    terminals.sweep(T0 + ABSOLUTE_NS + 1);
    assert!(terminals.lock().gone.is_empty());
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
