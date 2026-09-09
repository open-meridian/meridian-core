//! Topic patterns.
//!
//! Topics are dot-separated segments. Two wildcards, and the difference between
//! them is deliberate:
//!
//! - `*`  matches exactly one segment
//! - `**` matches one or more segments, and only as the final segment
//!
//! `**` is restricted to the tail because a mid-pattern `**` makes matching
//! ambiguous to read and to implement, and no subscription in the registry
//! wants one. A pattern that needs it is a pattern that has outgrown the
//! naming convention.
//!
//! Neither wildcard matches zero segments. `platform.custody.*` does not match
//! `platform.custody`, because a subscriber asking for one instance's topics
//! should not silently receive the parent.

/// Does `topic` match `pattern`?
///
/// A pattern with no wildcard is an exact match, which is the common case and
/// is checked first.
pub fn matches(pattern: &str, topic: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == topic;
    }

    let pat: Vec<&str> = pattern.split('.').collect();
    let top: Vec<&str> = topic.split('.').collect();

    for (i, segment) in pat.iter().enumerate() {
        match *segment {
            // Tail wildcard: everything from here on, provided there is at
            // least one segment left to consume.
            "**" => return i < top.len() && i + 1 == pat.len(),
            "*" => {
                if i >= top.len() {
                    return false;
                }
            }
            literal => {
                if top.get(i) != Some(&literal) {
                    return false;
                }
            }
        }
    }

    pat.len() == top.len()
}

/// Is this a well-formed topic to publish on?
///
/// Publishing on a pattern is always a bug: it means a topic was built by
/// string formatting that left a placeholder behind, and the message would go
/// to a topic nobody subscribes to under that literal name.
pub fn is_publishable(topic: &str) -> bool {
    !topic.is_empty()
        && !topic.contains('*')
        && !topic.contains("..")
        && !topic.starts_with('.')
        && !topic.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(matches(
            "platform.kernel.event.position-updated",
            "platform.kernel.event.position-updated"
        ));
        assert!(!matches(
            "platform.kernel.event.position-updated",
            "platform.kernel.event.statement-recorded"
        ));
    }

    #[test]
    fn single_wildcard_takes_exactly_one_segment() {
        let pattern = "platform.custody.*.event.sync-status";
        assert!(matches(
            pattern,
            "platform.custody.snaptrade-1.event.sync-status"
        ));
        // Zero segments: the instance is missing entirely.
        assert!(!matches(pattern, "platform.custody.event.sync-status"));
        // Two segments where one was asked for.
        assert!(!matches(pattern, "platform.custody.a.b.event.sync-status"));
    }

    #[test]
    fn tail_wildcard_takes_one_or_more() {
        assert!(matches(
            "platform.kernel.**",
            "platform.kernel.event.position-updated"
        ));
        assert!(matches("platform.kernel.**", "platform.kernel.command"));
        // Must consume at least one segment.
        assert!(!matches("platform.kernel.**", "platform.kernel"));
    }

    #[test]
    fn tail_wildcard_only_matches_at_the_tail() {
        // Not supported by design: a mid-pattern ** never matches, rather than
        // matching something surprising.
        assert!(!matches("platform.**.event", "platform.kernel.event"));
    }

    #[test]
    fn wildcard_does_not_cross_into_a_different_domain() {
        assert!(!matches(
            "platform.kernel.**",
            "platform.reference.event.instrument-applied"
        ));
    }

    #[test]
    fn publishable_rejects_patterns_and_malformed_topics() {
        assert!(is_publishable("platform.kernel.command.record-holding"));
        assert!(!is_publishable("platform.custody.*.event.sync-status"));
        assert!(!is_publishable("platform.kernel.**"));
        assert!(!is_publishable(""));
        assert!(!is_publishable("platform..kernel"));
        assert!(!is_publishable(".platform.kernel"));
        assert!(!is_publishable("platform.kernel."));
    }
}
