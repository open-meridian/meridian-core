//! Server-rendered pages, and the one escaping function every value goes
//! through. No template engine and no front-end build: a page is a string, and
//! a value reaches it only through [`escape`].

/// Escape text for an HTML element or a quoted attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A whole page. `body` is already HTML; `title` is text.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{} · Meridian</title>\
         <style>body{{font:15px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem}}\
         table{{border-collapse:collapse}}td,th{{padding:.25rem .75rem;border-bottom:1px solid #ddd;text-align:left}}\
         .refused{{color:#a00}}</style></head><body>\n{}\n</body></html>\n",
        escape(title),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_character_that_could_open_markup_is_escaped() {
        assert_eq!(
            escape(r#"<script>"x" & 'y'</script>"#),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn a_title_cannot_inject_markup() {
        assert!(page("<b>", "").contains("<title>&lt;b&gt; · Meridian</title>"));
    }
}
