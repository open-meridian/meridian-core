//! The registries' one line (contract v26; ruling 274, 2026-10-18, as
//! ruling 278 placed the notice in the platform Terms' §9 "Plugins and
//! registries"): the notice's paragraphs left every page, and the install
//! and update dialogs keep one line linking the Terms. Core's one constant,
//! so a change is made in one place. An agent's catalogue answer carries a
//! listing's `reviewed` and the Terms' address in its place.

use crate::html::escape;

/// The line, as a person reads it.
pub const LINE: &str = "Community plugins aren't reviewed. See the Terms of Service.";

/// The Terms' section the line links (ruling 278).
pub const TERMS_URL: &str = "https://open-meridian.com/terms#plugins-and-registries";

/// The line, as HTML: "Terms of Service" the link.
pub fn line() -> String {
    format!(
        "<p class=\"terms-line\" data-terms-line>Community plugins aren't reviewed. See the \
         <a href=\"{url}\" rel=\"noopener\" target=\"_blank\">Terms of Service</a>.</p>",
        url = escape(TERMS_URL)
    )
}

/// The Terms' address a catalogue answer names: the platform's own where its
/// catalogue carried one (PullCatalogueReply.terms_url, PluginListing's),
/// else the published one.
pub fn terms_url(said: &str) -> &str {
    if said.starts_with("https://") {
        said
    } else {
        TERMS_URL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ruling 274's line, word for word, linking §9.
    #[test]
    fn the_line_is_word_for_word_and_links_the_terms() {
        assert_eq!(
            LINE,
            "Community plugins aren't reviewed. See the Terms of Service."
        );
        let html = line();
        assert!(html.contains("Community plugins aren't reviewed. See the <a href=\"https://open-meridian.com/terms#plugins-and-registries\""));
        assert!(html.contains(">Terms of Service</a>."));
        assert!(!html.to_lowercase().contains("unsupervised"));
        assert_eq!(terms_url(""), TERMS_URL);
        assert_eq!(terms_url("https://example.org/t"), "https://example.org/t");
        assert_eq!(terms_url("javascript:x"), TERMS_URL);
    }
}
