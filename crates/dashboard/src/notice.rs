//! The supervision notice (contract v25, registries item 52; v25 Q12, as
//! changed by the product owner's ruling 172 of 2026-10-16): core's one
//! constant, so a change is made in one place. Drawn in full beside the
//! vendor's text on the Documentation tab and on the version page, in the
//! install and approval dialogs and on every listing, and by its title,
//! opening the full text in place, at the head of every list; carried in
//! every catalogue answer to an agent.

use crate::html::escape;

/// The notice's title.
pub const TITLE: &str = "Naming a plugin here is not an endorsement";

/// The notice's paragraphs, each a bold lead and the rest, word for word.
pub const PARAGRAPHS: [(&str, &str); 3] = [
    (
        "Local and community plugins are unsupervised.",
        "Nobody reviews them. A community version passes only the platform's automatic \
         build checks, which are not a review, an audit or a warranty. A local plugin is \
         not checked at all. Whoever installs one carries its risk.",
    ),
    (
        "Verified plugins are checked at best effort",
        ": the platform's review and a version floor. That is not an audit, a warranty, or \
         due diligence on the vendor.",
    ),
    (
        "In every case the firm carries its own vendor due diligence",
        ": the vendor's terms and licences, its security, its regulatory status, and \
         whether the plugin is fit for the firm's purpose.",
    ),
];

/// Where the Platform Terms are, which govern.
pub const TERMS: &str = "https://open-meridian.com/terms";

/// The bold lead's separator: a paragraph whose rest starts with
/// punctuation runs on from the lead; otherwise a space.
fn joined(rest: &str) -> &'static str {
    if rest.starts_with([':', ',', '.']) {
        ""
    } else {
        " "
    }
}

/// The notice in full, as HTML: a titled section.
pub fn html() -> String {
    let mut out = format!(
        "<section class=\"notice supervision\" aria-label=\"{title}\"><h3>{title}</h3>",
        title = escape(TITLE)
    );
    for (lead, rest) in PARAGRAPHS {
        out.push_str(&format!(
            "<p><strong>{}</strong>{}{}</p>",
            escape(lead),
            joined(rest),
            escape(rest)
        ));
    }
    out.push_str(&format!(
        "<p>This is a summary. The <a href=\"{TERMS}\" rel=\"noopener\" \
         target=\"_blank\">Platform Terms</a> govern.</p></section>"
    ));
    out
}

/// The notice by its title, opening the full text in place.
pub fn folded() -> String {
    format!(
        "<details class=\"notice supervision\"><summary>{}</summary>{}</details>",
        escape(TITLE),
        html()
    )
}

/// The notice in full as Markdown, for an agent's answer.
pub fn markdown() -> String {
    let mut out = format!("**{TITLE}**\n\n");
    for (lead, rest) in PARAGRAPHS {
        out.push_str(&format!("**{lead}**{}{rest}\n\n", joined(rest)));
    }
    out.push_str(&format!(
        "This is a summary. The [Platform Terms]({TERMS}) govern."
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ruling 172's first paragraph, word for word, and the other two
    /// unchanged from the spec's item 52.
    #[test]
    fn the_notice_is_word_for_word() {
        let text = markdown();
        assert!(text.contains(
            "**Local and community plugins are unsupervised.** Nobody reviews them. A \
             community version passes only the platform's automatic build checks, which are \
             not a review, an audit or a warranty. A local plugin is not checked at all. \
             Whoever installs one carries its risk."
        ));
        assert!(text.contains(
            "**Verified plugins are checked at best effort**: the platform's review and a \
             version floor. That is not an audit, a warranty, or due diligence on the vendor."
        ));
        assert!(text.contains(
            "**In every case the firm carries its own vendor due diligence**: the vendor's \
             terms and licences, its security, its regulatory status, and whether the plugin \
             is fit for the firm's purpose."
        ));
        assert!(text.ends_with(
            "This is a summary. The [Platform Terms](https://open-meridian.com/terms) govern."
        ));
        assert!(!text.contains("completely unsupervised"));
        let page = html();
        assert!(page.contains(
            "<p><strong>Local and community plugins are unsupervised.</strong> Nobody reviews \
             them."
        ));
        assert!(page.contains("<strong>Verified plugins are checked at best effort</strong>: the"));
    }
}
