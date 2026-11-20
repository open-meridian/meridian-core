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

/// The user whose inbox `subject` is, by its prefix; none where it is not an
/// inbox under a user's prefix -- a client's default `_INBOX.<id>`, which no
/// credential is granted, or no inbox at all.
pub fn owner(subject: &str) -> Option<&str> {
    let rest = subject.strip_prefix("_INBOX.")?;
    let (user, tail) = rest.split_once('.')?;
    (!user.is_empty() && !tail.is_empty() && !user.contains(['*', '>'])).then_some(user)
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

    #[test]
    fn a_default_inbox_or_a_pattern_is_owned_by_nobody() {
        // A client's own default, `_INBOX.<id>`: one token after the prefix.
        assert_eq!(owner("_INBOX.Zx81kq"), None);
        assert_eq!(owner("_INBOX.>"), None);
        assert_eq!(owner("_INBOX.*.x"), None);
        assert_eq!(owner("platform.config.query.enrolment"), None);
        assert_eq!(owner("_INBOX..x"), None);
        assert_eq!(owner("_INBOX.a."), None);
    }

    #[test]
    fn a_grant_is_its_users_prefix_and_nothing_wider() {
        assert_eq!(grant("custody-test-1"), "_INBOX.custody-test-1.>");
        assert_ne!(grant("runtime"), "_INBOX.>");
    }
}
