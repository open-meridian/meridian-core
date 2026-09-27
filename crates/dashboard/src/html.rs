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
:root{--ink:#16191d;--ink-soft:#5b646e;--ink-faint:#8b949e;--page:#f6f7f9;--card:#fff;--line:#e2e5e9;\
--line-soft:#eef0f3;--hover:#eef0f3;--accent:#1f5fa8;--accent-wash:#e8f0fa;--accent-ink:#fff;\
--danger:#b3261e;--danger-wash:#fdecea;--good:#1f7a4d;--good-wash:#e8f5ee;--warn-ink:#8a5a10;\
--warn-wash:#fdf3e0;--radius:8px}\
@media (prefers-color-scheme:dark){:root{--ink:#e8eaed;--ink-soft:#a2abb5;--ink-faint:#79828c;\
--page:#14171a;--card:#1c2024;--line:#2c3237;--line-soft:#23282d;--hover:#262b31;--accent:#6ba4e8;\
--accent-wash:#1a2a3d;--accent-ink:#10141a;--danger:#ef8a82;--danger-wash:#3a201f;--good:#6cc48a;\
--good-wash:#17301f;--warn-ink:#e0b874;--warn-wash:#33291a}}\
*{box-sizing:border-box}[hidden]{display:none!important}\
body{margin:0;font:14.5px/1.55 -apple-system,BlinkMacSystemFont,Segoe UI,system-ui,sans-serif;\
color:var(--ink);background:var(--page)}\
main.sheet{max-width:44rem;margin:2.5rem auto 4rem;padding:1.75rem 2rem;background:var(--card);\
border:1px solid var(--line);border-radius:var(--radius)}\
@media (max-width:44rem){main.sheet{margin:0;border:0;border-radius:0;padding:1.25rem 1rem}}\
h1{font-size:1.45rem;letter-spacing:-.02em;margin:0 0 .35rem;font-weight:600}\
h2{font-size:1.05rem;margin:0 0 .75rem;font-weight:600}h3{font-size:.95rem;margin:1.25rem 0 .25rem;font-weight:600}\
p{margin:.5rem 0}a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.86em}\
table{border-collapse:collapse;width:100%;font-size:.93rem}\
td,th{padding:.5rem .6rem;border-bottom:1px solid var(--line-soft);text-align:left}\
th{font-size:.8rem;color:var(--ink-soft);font-weight:550}\
label{display:block;margin:0 0 .9rem;font-size:.84rem;font-weight:550}\
input:not([type=checkbox]):not([type=radio]):not([type=hidden]),select,textarea{padding:.5rem .65rem;\
font:inherit;font-size:14.5px;font-weight:400;color:var(--ink);background:var(--card);\
border:1px solid var(--line);border-radius:var(--radius);max-width:100%}\
label>input:not([type=checkbox]),label>select,label>textarea{display:block;width:100%;margin-top:.3rem}\
label:has(>input[type=checkbox]){display:flex;gap:.5rem;align-items:center;font-weight:400;font-size:.9rem}\
input:focus,select:focus,textarea:focus{outline:2px solid var(--accent);outline-offset:0;border-color:var(--accent)}\
input::placeholder{color:var(--ink-faint)}\
.grid-2{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:0 .75rem}\
.grid-wide{display:grid;grid-template-columns:minmax(0,3fr) minmax(0,1fr);gap:0 .75rem}\
@media (max-width:36rem){.grid-2,.grid-wide{grid-template-columns:minmax(0,1fr)}}\
button{display:inline-flex;align-items:center;gap:.4rem;font:inherit;font-weight:550;padding:.48rem .9rem;\
border-radius:var(--radius);border:1px solid var(--line);background:var(--card);color:var(--ink);\
cursor:pointer;margin:.5rem .5rem 0 0}\
button:hover{background:var(--hover)}\
button.primary{background:var(--accent);border-color:var(--accent);color:var(--accent-ink)}\
button.primary:hover{filter:brightness(1.08)}button:disabled{opacity:.5;cursor:not-allowed}\
button.reveal{margin:.35rem 0 0;padding:.15rem .6rem;font-size:.8rem;font-weight:500}\
.hint{color:var(--ink-soft);font-size:.88rem;font-weight:400;margin:.25rem 0 .9rem}\
.refused,.warn,.passed,ul.refusal{border-radius:var(--radius);padding:.65rem .9rem;margin:.75rem 0;font-size:.93rem}\
.refused,ul.refusal{color:var(--danger);background:var(--danger-wash);border:1px solid var(--danger)}\
ul.refusal{padding-left:2rem}\
.warn{color:var(--warn-ink);background:var(--warn-wash)}.passed{color:var(--good);background:var(--good-wash)}\
.panel{background:var(--page);border:1px solid var(--line);border-radius:var(--radius);padding:.75rem 1rem;margin:.75rem 0}\
nav.steps{display:flex;flex-wrap:wrap;gap:.4rem;margin:1rem 0 1.5rem;font-size:.84rem}\
nav.steps a{padding:.25rem .75rem;border-radius:99px;background:var(--line-soft);color:var(--ink-soft);font-weight:550}\
nav.steps a:hover{text-decoration:none;filter:brightness(1.05)}\
nav.steps a.here{background:var(--accent);color:var(--accent-ink)}\
section.step{border-top:1px solid var(--line);margin-top:1.5rem;padding-top:1rem}\
form.js section.step{display:none;border:0;margin:0;padding:0}form.js section.step.current{display:block}\
section.step>div:last-child{display:flex;justify-content:flex-end;gap:.5rem;margin-top:1.25rem;\
padding-top:1rem;border-top:1px solid var(--line-soft)}\
section.step>div:last-child button{margin:0}\
form:not(.js) [data-next],form:not(.js) [data-back]{display:none}.off{display:none}\
.development{color:var(--warn-ink);background:var(--warn-wash);border:1px solid var(--warn-ink);\
border-radius:var(--radius);padding:.5rem .8rem;margin:0 0 1.25rem;font-size:.9rem}\
main.sheet:has(.admin){max-width:68rem}\
.page-head{display:flex;justify-content:space-between;align-items:baseline;gap:1rem}\
nav.tabs{display:flex;flex-wrap:wrap;gap:.25rem;margin:1rem 0 1.25rem;padding-bottom:.75rem;\
border-bottom:1px solid var(--line);font-size:.88rem}\
nav.tabs a{padding:.35rem .8rem;border-radius:99px;color:var(--ink-soft);font-weight:550}\
nav.tabs a:hover{text-decoration:none;background:var(--hover)}\
nav.tabs a.here{background:var(--accent-wash);color:var(--accent)}\
.admin:not(.js) nav.tabs{display:none}\
section.admin-section{margin:0 0 2.25rem}.admin.js section.admin-section{display:none;margin:0}\
.admin.js section.admin-section.current{display:block}\
.section-head{display:flex;justify-content:space-between;align-items:flex-start;gap:1rem;margin-bottom:.5rem}\
.section-head h2{margin:0 0 .15rem}.section-head .hint{margin:0}.section-head button{margin:0;flex-shrink:0}\
.scroll{overflow-x:auto}table.list td{vertical-align:middle}\
@media (max-width:36rem){.section-head{flex-direction:column}td.actions{white-space:normal}}table.list tbody tr:hover{background:var(--line-soft)}\
table.list .name{font-weight:550}\
table.list .id{display:block;font:.76rem ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--ink-faint)}\
td.actions{text-align:right;white-space:nowrap}td.actions form{display:inline}\
td.actions button{margin:0 0 0 .35rem;padding:.3rem .7rem;font-size:.84rem}\
.pill{display:inline-block;padding:.1rem .55rem;border-radius:99px;font-size:.76rem;font-weight:550;\
background:var(--line-soft);color:var(--ink-soft)}.pill.good{background:var(--good-wash);color:var(--good)}\
.empty{color:var(--ink-soft);padding:1.5rem;text-align:center;border:1px dashed var(--line);border-radius:var(--radius)}\
dialog{width:min(32rem,calc(100vw - 2rem));padding:0;border:1px solid var(--line);border-radius:var(--radius);\
background:var(--card);color:var(--ink);box-shadow:0 12px 40px rgba(0,0,0,.25)}\
dialog::backdrop{background:rgba(10,14,18,.45)}\
.dialog-head{padding:1rem 1.25rem .25rem}.dialog-head h2{margin:0}.dialog-body{padding:.5rem 1.25rem}\
.dialog-foot{display:flex;justify-content:flex-end;gap:.5rem;padding:.75rem 1.25rem;border-top:1px solid var(--line-soft)}\
.dialog-foot button{margin:0}\
fieldset.checks{border:1px solid var(--line);border-radius:var(--radius);padding:.5rem .75rem;margin:0 0 .9rem;\
max-height:14rem;overflow:auto}fieldset.checks legend{font-size:.84rem;font-weight:550;padding:0 .25rem}\
fieldset.checks label{margin:.25rem 0}";

/// Whether this deployment was installed for development
/// (spec/live-plugin-development, ruling 2). Process-wide, set once at start
/// from the chart, and said on every page, since it is the one thing about a
/// deployment nobody should have to find out.
static DEVELOPMENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_development() {
    DEVELOPMENT.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_development() -> bool {
    DEVELOPMENT.load(std::sync::atomic::Ordering::Relaxed)
}

const DEVELOPMENT_BANNER: &str = "<p class=\"development\"><strong>Development \
     deployment.</strong> It runs plugin code as it is being written, which nobody \
     has reviewed. Nothing here is for real use.</p>";

/// A whole page. `body` is already HTML; `title` is text.
pub fn page(title: &str, body: &str) -> String {
    page_for(title, body, is_development())
}

fn page_for(title: &str, body: &str, development: bool) -> String {
    let body = if development {
        format!("{DEVELOPMENT_BANNER}\n{body}")
    } else {
        body.to_string()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{} · Meridian</title><style>{STYLE}</style></head><body>\n\
         <main class=\"sheet\">\n{}\n</main>\n</body></html>\n",
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
    fn a_development_deployment_says_so_on_every_page_and_another_never_does() {
        let marked = page_for("Home", "<h1>Meridian</h1>", true);
        assert!(marked.contains("class=\"development\""), "{marked}");
        assert!(marked.find("Development").unwrap() < marked.find("<h1>").unwrap());
        assert!(!page_for("Home", "<h1>Meridian</h1>", false).contains("class=\"development\""));
    }

    #[test]
    fn a_title_cannot_inject_markup() {
        assert!(page("<b>", "").contains("<title>&lt;b&gt; · Meridian</title>"));
    }
}
