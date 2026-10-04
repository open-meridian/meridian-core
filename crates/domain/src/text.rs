//! The characters a ticket's text may hold (requirement 59 of
//! spec/a-problem-seen-in-a-deployment-reaches-someone-who-can-act; W4.12,
//! W6.21): plain text, refused when it holds a control character other than
//! a newline or a tab, a bidirectional override or isolate, a zero-width or
//! tag character, or a private-use character.
//!
//! Here because two components check it and must check it alike: a plugin's
//! sidecar first, so a flood stops at the plugin's own sidecar, and the
//! dashboard again for every filing, a page's and a client's included. Each
//! refuses in its own words, naming the field; this says only which
//! character, and where. Pure, and reading no table but the ones below:
//! what hides words from a reader is a short list, written out.

/// Why a text may not be kept: the first character it holds that the rule
/// refuses, by its position among the text's characters (from 0) and its
/// code point, with what kind of character it is, in words a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub at: usize,
    pub character: char,
    pub kind: &'static str,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plain text only: character {} is U+{:04X}, {}",
            self.at + 1,
            self.character as u32,
            self.kind
        )
    }
}

/// The kind of a character the rule refuses, or None for one it keeps.
pub fn refused_kind(c: char) -> Option<&'static str> {
    let code = c as u32;
    if c == '\n' || c == '\t' {
        return None;
    }
    if c.is_control() {
        return Some("a control character");
    }
    match code {
        // Embeddings and overrides, and the isolates (UAX #9).
        0x202A..=0x202E | 0x2066..=0x2069 => Some("a bidirectional override or isolate"),
        // The marks that set direction without a glyph, and the joiners and
        // spaces that have no width: each can hide or reorder words.
        0x200B..=0x200F | 0x2060..=0x2064 | 0x061C | 0xFEFF | 0x180E | 0x034F => {
            Some("a zero-width character")
        }
        0xE0000..=0xE007F => Some("a tag character"),
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD => {
            Some("a private-use character")
        }
        // A line or paragraph separator ends a line as a newline does, and
        // is not one.
        0x2028 | 0x2029 => Some("a control character"),
        _ => None,
    }
}

/// The first character `text` holds that the rule refuses, or None.
pub fn refused(text: &str) -> Option<Refused> {
    text.chars().enumerate().find_map(|(at, character)| {
        refused_kind(character).map(|kind| Refused {
            at,
            character,
            kind,
        })
    })
}

/// A text's length as the dictionary's bounds count it: in characters, as a
/// person reads them.
pub fn characters(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newline_and_a_tab_are_kept_and_every_other_control_character_is_refused() {
        assert_eq!(refused("one line\nand\tanother"), None);
        for c in ['\r', '\u{0}', '\u{7}', '\u{1b}', '\u{7f}', '\u{85}'] {
            let found = refused(&format!("a{c}b")).expect("refused");
            assert_eq!((found.at, found.character), (1, c));
            assert_eq!(found.kind, "a control character");
        }
    }

    #[test]
    fn what_hides_or_reorders_words_is_refused_by_its_kind() {
        for (c, kind) in [
            ('\u{202E}', "a bidirectional override or isolate"),
            ('\u{2067}', "a bidirectional override or isolate"),
            ('\u{200B}', "a zero-width character"),
            ('\u{200D}', "a zero-width character"),
            ('\u{FEFF}', "a zero-width character"),
            ('\u{E0063}', "a tag character"),
            ('\u{E000}', "a private-use character"),
            ('\u{10FFFD}', "a private-use character"),
            ('\u{2028}', "a control character"),
        ] {
            assert_eq!(refused(&format!("x{c}")).unwrap().kind, kind, "{c:?}");
        }
    }

    #[test]
    fn ordinary_text_in_any_script_is_kept() {
        for kept in [
            "Cash on ACC-GROWTH differs by 1.17 USD",
            "Différence de trésorerie — 12 400,00 €",
            "現金残高が一致しません",
            "Ｆｕｌｌｗｉｄｔｈ letters, and an emoji 🙂",
            "",
        ] {
            assert_eq!(refused(kept), None, "{kept}");
        }
    }

    #[test]
    fn the_refusal_names_the_character_by_its_place_and_code_point() {
        let said = refused("ab\u{200B}c").unwrap().to_string();
        assert_eq!(
            said,
            "plain text only: character 3 is U+200B, a zero-width character"
        );
    }
}
