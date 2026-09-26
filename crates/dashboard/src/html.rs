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

/// How every page looks: one stylesheet, inline, loading nothing from
/// anywhere -- a deployment runs inside a firm's network, and a page that
/// fetched a font or a script from outside it would be a request nobody
/// asked for. Light and dark follow the browser.
const STYLE: &str = "\
:root{--bg:#fff;--fg:#1b1f24;--muted:#57606a;--line:#d8dee4;--panel:#f6f8fa;\
--accent:#0b5cad;--accent-fg:#fff;--bad:#b3261e;--bad-bg:#fdecea;--good:#1a7f37;--good-bg:#e9f7ee;\
--warn:#8a5a00;--warn-bg:#fff4d6}\
@media (prefers-color-scheme:dark){:root{--bg:#0f1419;--fg:#e6edf3;--muted:#9aa5b1;--line:#30363d;\
--panel:#161b22;--accent:#4c9be8;--accent-fg:#0f1419;--bad:#ff8a80;--bad-bg:#3a1714;--good:#6fdd8b;\
--good-bg:#10301b;--warn:#f2c14e;--warn-bg:#33270a}}\
*{box-sizing:border-box}\
body{font:15px/1.55 system-ui,-apple-system,Segoe UI,sans-serif;color:var(--fg);background:var(--bg);\
max-width:46rem;margin:0 auto;padding:2rem 1rem 4rem}\
h1{font-size:1.6rem;margin:0 0 1rem}h2{font-size:1.2rem;margin:2rem 0 .5rem}h3{font-size:1rem;margin:1.25rem 0 .25rem}\
p{margin:.5rem 0}a{color:var(--accent)}code{font-size:.9em}\
table{border-collapse:collapse;width:100%}td,th{padding:.35rem .6rem;border-bottom:1px solid var(--line);text-align:left}\
label{display:block;margin:.75rem 0 0;font-weight:500}\
label>input:not([type=checkbox]),label>select,label>textarea{display:block;width:100%;margin-top:.25rem;\
padding:.5rem .6rem;font:inherit;color:inherit;background:var(--bg);border:1px solid var(--line);border-radius:6px}\
label:has(>input[type=checkbox]){font-weight:400}\
input:focus,select:focus,textarea:focus{outline:2px solid var(--accent);outline-offset:1px}\
button{font:inherit;padding:.45rem 1rem;border-radius:6px;border:1px solid var(--line);background:var(--panel);\
color:var(--fg);cursor:pointer;margin:.75rem .5rem 0 0}\
button.primary{background:var(--accent);border-color:var(--accent);color:var(--accent-fg)}\
.hint{color:var(--muted);font-size:.9rem;margin:.25rem 0 0}\
.refused,.warn{padding:.6rem .8rem;border-radius:6px}\
.refused{color:var(--bad);background:var(--bad-bg)}.warn{color:var(--warn);background:var(--warn-bg)}\
ul.refusal{color:var(--bad);background:var(--bad-bg);border-radius:6px;padding:.6rem .8rem .6rem 2rem;margin:.75rem 0}\
.passed{color:var(--good);background:var(--good-bg);padding:.6rem .8rem;border-radius:6px}\
.panel{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:.75rem 1rem;margin:.75rem 0}\
nav.steps{display:flex;flex-wrap:wrap;gap:.25rem 1.25rem;margin:0 0 1.5rem;font-size:.9rem}\
nav.steps a{color:var(--muted);text-decoration:none}nav.steps a.here{color:var(--fg);font-weight:600}\
section.step{border-top:1px solid var(--line);margin-top:1.5rem}form.js section.step{display:none;border:0;margin:0}\
form.js section.step.current{display:block}form:not(.js) [data-next],form:not(.js) [data-back]{display:none}\
.off{display:none}button:disabled{opacity:.5;cursor:not-allowed}";

/// A whole page. `body` is already HTML; `title` is text.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{} · Meridian</title><style>{STYLE}</style></head><body>\n{}\n</body></html>\n",
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
