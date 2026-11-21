//! Each credential's own inboxes (v31a, item 2; the access audit's S1).
//!
//! A reply travels on an inbox, a subject its caller listens on. When every
//! credential held `_INBOX.>` to publish and to subscribe, any one of them
//! could hear every answer in the deployment -- every account's data, every
//! plugin's secret settings -- and could answer in another's place. So each
//! credential's client makes its inboxes under its own user's prefix,
//! `_INBOX.<user>.`, and the broker grants each user that prefix alone.
//!
//! A responder still answers whoever asked: the broker lets a client publish
//! one answer to the reply subject of each request it was delivered
//! (`allow_responses`), whatever its grants say. The one answer sent to an
//! inbox no request carried is the lake's to a write it took from its
//! write-ahead stream, and the lake's credential is granted the inboxes of
//! the data plugins that write ahead, by name ([`grant`]).
//!
//! A user's name is one subject token: a plugin's instance is a host label,
//! and a component's name has no dot.

/// The prefix a credential's client makes its inboxes under: `_INBOX.<user>`.
pub fn prefix(user: &str) -> String {
    format!("_INBOX.{user}")
}

/// What a credential is granted of the inboxes: its own, and no other's.
pub fn grant(user: &str) -> String {
    format!("{}.>", prefix(user))
}

/// The user whose inbox `subject` is, by its first token after `_INBOX.`;
/// none where there is no second token, the first is a pattern, or it is no
/// inbox at all.
///
/// The token is read, not checked against the users: async-nats's own
/// default request inbox, `_INBOX.<nuid>.<n>`, has two tokens, so it reads
/// as owned by `<nuid>`, a name no credential has and none is granted. A
/// request asked on one is asked as nobody's credential: [`asked_as`]
/// refuses every sender it names. It fails closed.
pub fn owner(subject: &str) -> Option<&str> {
    let rest = subject.strip_prefix("_INBOX.")?;
    let (user, tail) = rest.split_once('.')?;
    (!user.is_empty() && !tail.is_empty() && !user.contains(['*', '>'])).then_some(user)
}

thread_local! {
    /// INTERIM(v32): the credential whose inbox the request being answered
    /// on this thread is to be answered on, where it came by the broker.
    static ASKED_BY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// INTERIM(v32): run `answer` with the credential that asked known to it --
/// the owner of the reply inbox of a request that came by the broker, empty
/// where that inbox is no user's; none where it did not come by the broker.
/// Deleted when v32 binds a request's sender at the broker by its subject.
pub fn within_asker<R>(asker: Option<String>, answer: impl FnOnce() -> R) -> R {
    let before = ASKED_BY.with(|held| held.replace(asker));
    let answered = answer();
    ASKED_BY.with(|held| *held.borrow_mut() = before);
    answered
}

/// INTERIM(v32): the credential that asked the request this thread answers,
/// by its reply inbox; none where it did not come by the broker.
pub fn asked_by() -> Option<String> {
    ASKED_BY.with(|held| held.borrow().clone())
}

/// INTERIM(v32): whether the request this thread answers was asked by
/// `claimed`'s own credential (v31a, item 3; the access audit's S2,
/// interim). A request that came by the broker is answered on its reply
/// inbox, which only its asker's credential hears: a sender named otherwise
/// is forged, and one under no user's prefix names nobody. One that did not
/// come by the broker came from this process, and is as it says.
pub fn asked_as(claimed: &str) -> bool {
    match asked_by() {
        None => true,
        Some(asker) => !claimed.is_empty() && asker == claimed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbox_is_owned_by_the_user_its_prefix_names() {
        assert_eq!(owner("_INBOX.custody-test-1.abc"), Some("custody-test-1"));
        assert_eq!(owner("_INBOX.runtime.abc.1"), Some("runtime"));
        assert_eq!(
            owner(&format!("{}.x", prefix("dashboard-1"))),
            Some("dashboard-1")
        );
    }

    /// The review of v31a-core-2, F5: async-nats's default request inbox,
    /// `_INBOX.<nuid>.<n>`, reads as owned by its nuid, which is no
    /// credential's name, so a request asked on it is asked as no sender it
    /// names.
    #[test]
    fn a_default_request_inbox_reads_as_its_nuid_and_asks_as_nobody() {
        let default = "_INBOX.Q8w3xXo7Lw2kT5bJ9vRmYc.7";
        assert_eq!(owner(default), Some("Q8w3xXo7Lw2kT5bJ9vRmYc"));
        for claimed in ["custody-test-1", "runtime", "dashboard-1", ""] {
            assert!(
                !within_asker(owner(default).map(String::from), || asked_as(claimed)),
                "a request on a default inbox was asked as {claimed:?}"
            );
        }
    }

    #[test]
    fn a_default_inbox_or_a_pattern_is_owned_by_nobody() {
        // A bare `_INBOX.<id>`: one token after the prefix.
        assert_eq!(owner("_INBOX.Zx81kq"), None);
        assert_eq!(owner("_INBOX.>"), None);
        assert_eq!(owner("_INBOX.*.x"), None);
        assert_eq!(owner("platform.config.query.enrolment"), None);
        assert_eq!(owner("_INBOX..x"), None);
        assert_eq!(owner("_INBOX.a."), None);
    }

    /// v31a, item 3: a request's sender is the credential its reply inbox
    /// is under, which alone hears the answer; a sender the request names
    /// otherwise is forged. One that did not come by the broker came from
    /// this process.
    #[test]
    fn a_request_is_asked_as_the_credential_its_inbox_is_under() {
        assert!(within_asker(Some("custody-test-1".into()), || asked_as(
            "custody-test-1"
        )));
        assert!(!within_asker(Some("custody-test-1".into()), || asked_as(
            "custody-test-2"
        )));
        assert!(!within_asker(Some(String::new()), || asked_as("")));
        assert!(!within_asker(Some(String::new()), || asked_as(
            "custody-test-1"
        )));
        assert!(!within_asker(Some("custody-test-1".into()), || asked_as(
            ""
        )));
        assert!(within_asker(None, || asked_as("custody-test-1")));
        assert_eq!(asked_by(), None, "set only while answering");
    }

    #[test]
    fn a_grant_is_its_users_prefix_and_nothing_wider() {
        assert_eq!(grant("custody-test-1"), "_INBOX.custody-test-1.>");
        assert_ne!(grant("runtime"), "_INBOX.>");
    }
}
