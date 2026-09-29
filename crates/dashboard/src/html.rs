//! Server-rendered pages, and the one escaping function every value goes
//! through. No template engine and no front-end build: a page is a string, and
//! a value reaches it only through [`escape`].
//!
//! **One product with the plugins' pages.** Every colour, and the type,
//! spacing, radii and shadows, are the plugin UI kit's tokens
//! (spec/plugin-pages-share-one-kit.md): a page links the kit's stylesheet
//! from this dashboard's own origin, where [`crate::kit`] serves it, and the
//! rules here say only where things go, in the kit's properties and never a
//! colour of their own. The scheme is the brand's default, light or dark as
//! the person chose in the header's menu, or as their system says; schemes of
//! an administrator's are kernel/colour-schemes.
//!
//! **One header** across the dashboard and the frame around a plugin's page
//! (spec Q3, the product owner on 2026-09-28): the mark and the way home on
//! the left, where the page is beside it, and on the right the person's name
//! with sign-out and, for a deployment admin, the admin portal.

use std::sync::OnceLock;

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

/// Where things go. Every colour is a kit property (`var(--ink)` and its
/// kind), so a scheme changes the dashboard as it changes a plugin's page.
/// Inline and loading nothing from outside the deployment: the kit's
/// stylesheet is this dashboard's own.
const STYLE: &str = "\
*{box-sizing:border-box}[hidden]{display:none!important}\
html,body{min-height:100%}\
body{margin:0;font:14.5px/1.55 var(--sans);color:var(--ink);background:var(--page);-webkit-font-smoothing:antialiased;\
display:flex;flex-direction:column;min-height:100vh}\
header.bar{position:sticky;top:0;z-index:30;display:flex;align-items:center;gap:.75rem;min-height:56px;padding:0 1.25rem;\
background:var(--card);border-bottom:1px solid var(--line)}\
header.bar a.brand{display:inline-flex;align-items:center;gap:.55rem;color:var(--ink);font-weight:650;\
letter-spacing:-.01em;text-decoration:none;flex-shrink:0}\
header.bar a.brand svg{width:24px;height:24px;color:var(--accent)}\
header.bar .where{font:600 .7rem var(--mono);letter-spacing:.1em;text-transform:uppercase;color:var(--ink-faint)}\
header.bar .crumbs{display:flex;align-items:center;gap:.5rem;min-width:0;padding-left:.75rem;\
border-left:1px solid var(--line);color:var(--ink-soft)}\
header.bar .crumbs a{color:var(--ink-soft);white-space:nowrap}header.bar .crumbs a:hover{color:var(--ink)}\
header.bar .crumbs .here{display:flex;align-items:baseline;gap:.45rem;min-width:0;color:var(--ink)}\
header.bar .crumbs .here strong{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
header.bar .crumbs .here code{color:var(--ink-faint);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
header.bar .crumbs .own-window{color:var(--ink-faint);padding:0 .25rem}\
header.bar .spacer{flex:1 1 auto}\
header.bar .bar-link{display:inline-flex;align-items:center;gap:.4rem;padding:.35rem .7rem;border-radius:var(--radius);\
color:var(--ink-soft);font-weight:550;white-space:nowrap}\
header.bar .bar-link:hover{background:var(--hover);color:var(--ink);text-decoration:none}\
header.bar .bar-link.here{background:var(--accent-wash);color:var(--accent)}\
header.bar .person>summary{display:flex;align-items:center;gap:.5rem;padding:.3rem .5rem}\
header.bar .person .avatar{display:inline-grid;place-items:center;width:28px;height:28px;border-radius:50%;\
background:var(--accent-wash);color:var(--accent);font-weight:650;font-size:.8rem}\
header.bar .person .person-name{max-width:14rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--ink);font-weight:550}\
header.bar .person .menu-pop form{margin:0}\
header.bar .menu-pop a{color:var(--ink)}header.bar .menu-pop button{width:100%;margin:0;padding:.45rem .6rem;border:0;\
background:none;font-weight:400;justify-content:flex-start}header.bar .menu-pop button:hover{background:var(--hover)}\
.menu-label{padding:.35rem .6rem .15rem;font:600 .68rem var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--ink-faint)}\
.menu-pop a[aria-current=true]::after{content:\"\\2713\";margin-left:auto;color:var(--accent)}\
@media (max-width:40rem){header.bar{padding:0 .75rem;gap:.4rem}header.bar .person .person-name,header.bar a.brand span,\
header.bar .where{display:none}header.bar .crumbs{padding-left:.5rem}header.bar .crumbs>a{display:none}\
header.bar .bar-link{padding:.35rem .5rem}}\
main.sheet{width:calc(100% - 2rem);max-width:44rem;margin:2.5rem auto 4rem;padding:1.9rem 2.1rem;background:var(--card);\
border:1px solid var(--line);border-radius:16px;box-shadow:var(--shadow)}\
@media (max-width:44rem){main.sheet{width:100%;margin:0;border:0;border-radius:0;padding:1.25rem 1rem}}\
main.page{width:100%}\
main.frame{flex:1 1 auto;display:flex;min-height:0}\
main.frame iframe{flex:1 1 auto;width:100%;border:0;background:var(--page)}\
main.sheet h1{margin:0 0 .35rem}h2{margin:0 0 .75rem}h3{font-size:.95rem;margin:1.25rem 0 .25rem;font-weight:600}\
p{margin:.5rem 0}a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
a.button{color:var(--ink)}a.button.primary{color:var(--primary-ink)}a.button:hover{text-decoration:none}\
code{font-family:var(--mono);font-size:.86em}\
table{border-collapse:collapse;width:100%;font-size:.93rem}\
td,th{padding:.5rem .6rem;border-bottom:1px solid var(--line-soft);text-align:left}\
th{font:600 .68rem var(--mono);text-transform:uppercase;letter-spacing:.08em;color:var(--ink-faint)}\
label{display:block;margin:0 0 .9rem;font-size:.84rem;font-weight:550}\
input:not([type=checkbox]):not([type=radio]):not([type=hidden]),select,textarea{padding:.5rem .65rem;\
font:inherit;font-size:14.5px;font-weight:400;color:var(--ink);background:var(--card);\
border:1px solid var(--line-strong);border-radius:var(--radius);max-width:100%}\
label>input:not([type=checkbox]):not([type=radio]),label>select,label>textarea{display:block;width:100%;margin-top:.3rem}\
label:has(>input[type=checkbox]){display:flex;gap:.5rem;align-items:center;font-weight:400;font-size:.9rem}\
input:focus,select:focus,textarea:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-wash)}\
input::placeholder{color:var(--ink-faint)}\
.grid-2{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:0 .75rem}\
.grid-wide{display:grid;grid-template-columns:minmax(0,3fr) minmax(0,1fr);gap:0 .75rem}\
@media (max-width:36rem){.grid-2,.grid-wide{grid-template-columns:minmax(0,1fr)}}\
button{display:inline-flex;align-items:center;gap:.4rem;font:inherit;font-weight:550;padding:.48rem .9rem;\
border-radius:var(--radius);border:1px solid var(--line-strong);background:var(--card);color:var(--ink);\
cursor:pointer;margin:.5rem .5rem 0 0}\
button:hover{background:var(--hover)}\
button.primary{background:var(--primary);border-color:var(--primary);color:var(--primary-ink)}\
button.primary:hover{filter:brightness(1.1)}\
button:disabled{opacity:.5;cursor:not-allowed}\
button.reveal{margin:.35rem 0 0;padding:.15rem .6rem;font-size:.8rem;font-weight:500}\
.hint{display:block;color:var(--ink-soft);font-size:.88rem;font-weight:400;margin:.25rem 0 .9rem}\
.refused,.warn,.passed,ul.refusal{border-radius:var(--radius);padding:.65rem .9rem;margin:.75rem 0;font-size:.93rem}\
.refused,ul.refusal{color:var(--danger);background:var(--danger-wash);border:1px solid var(--danger)}\
ul.refusal{padding-left:2rem}\
.warn{color:var(--warn-ink);background:var(--warn-wash)}.passed{color:var(--good);background:var(--good-wash)}\
main.sheet .panel{background:var(--page);border:1px solid var(--line);border-radius:var(--radius);padding:.75rem 1rem;\
margin:.75rem 0;box-shadow:none}\
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
.development{color:var(--warn-ink);background:var(--warn-wash);border-bottom:1px solid var(--warn-ink);\
padding:.5rem 1.25rem;margin:0;font-size:.9rem}\
.page-head{display:flex;justify-content:space-between;align-items:baseline;gap:1rem;flex-wrap:wrap}\
.page-head h1{margin:0}.page-head p{margin:.3rem 0 0}\
nav.tabs{display:flex;flex-wrap:wrap;gap:.25rem;margin:1rem 0 1.25rem;padding-bottom:.75rem;\
border-bottom:1px solid var(--line);font-size:.88rem}\
nav.tabs a{padding:.35rem .8rem;border-radius:99px;color:var(--ink-soft);font-weight:550}\
nav.tabs a:hover{text-decoration:none;background:var(--hover)}\
nav.tabs a.here{background:var(--accent-wash);color:var(--accent);font-weight:600}\
.admin:not(.js) nav.tabs{display:none}\
section.admin-section{margin:0 0 2.25rem}.admin.js section.admin-section{display:none;margin:0}\
.admin.js section.admin-section.current{display:block}\
.plugin-view nav.tabs{margin-bottom:1rem}.plugin-view .stack>*+*{margin-top:1.25rem}\
.section-head{display:flex;justify-content:space-between;align-items:flex-start;gap:1rem;margin:0 0 .5rem}\
.section-head h2{margin:0 0 .15rem}.section-head .hint{margin:0}.section-head button{margin:0;flex-shrink:0}\
.scroll{overflow-x:auto}table.list td{vertical-align:middle}\
@media (max-width:36rem){.section-head{flex-direction:column}td.actions{white-space:normal}}\
table.list tbody tr:hover td{background:var(--line-soft)}\
table.list .name{font-weight:550}table.plugins td:first-child{white-space:nowrap}\
.id{display:block;font:.76rem var(--mono);color:var(--ink-faint);font-weight:400}\
td.actions{text-align:right;white-space:nowrap}td.actions form{display:inline}\
td.actions button,td.actions .button{margin:0 0 0 .35rem;padding:.3rem .7rem;font-size:.84rem}\
.pill,.badge{display:inline-block;padding:.1rem .55rem;border-radius:99px;font-size:.76rem;font-weight:550;\
background:var(--line-soft);color:var(--ink-soft);vertical-align:middle;white-space:nowrap}\
.pill.good,.badge.good{background:var(--good-wash);color:var(--good)}\
.pill.warn,.badge.warn{background:var(--warn-wash);color:var(--warn-ink)}\
.pill.bad,.badge.bad{background:var(--danger-wash);color:var(--danger)}\
.badge.info{background:var(--violet-wash);color:var(--violet)}.badge.accent{background:var(--accent-wash);color:var(--accent)}\
.empty{color:var(--ink-soft);padding:1.5rem;text-align:center;border:1px dashed var(--line);border-radius:var(--radius)}\
dialog{width:min(32rem,calc(100vw - 2rem));padding:0;border:1px solid var(--line);border-radius:var(--radius-lg);\
background:var(--card);color:var(--ink);box-shadow:var(--shadow-pop)}\
dialog::backdrop{background:var(--backdrop)}\
.dialog-head{padding:1rem 1.25rem .25rem}.dialog-head h2{margin:0}.dialog-body{padding:.5rem 1.25rem}\
.dialog-foot{display:flex;justify-content:flex-end;gap:.5rem;padding:.75rem 1.25rem;border-top:1px solid var(--line-soft)}\
.dialog-foot button{margin:0}\
fieldset.checks{border:1px solid var(--line);border-radius:var(--radius);padding:.5rem .75rem;margin:0 0 .9rem;\
max-height:14rem;overflow:auto}fieldset.checks legend{font-size:.84rem;font-weight:550;padding:0 .25rem}\
fieldset.checks label{margin:.25rem 0}\
.view-switch{display:inline-flex;border:1px solid var(--line-strong);border-radius:var(--radius);overflow:hidden}\
.view-switch button{margin:0;border:0;border-radius:0;padding:.35rem .8rem;font-size:.86rem;background:var(--card);color:var(--ink-soft)}\
.view-switch button+button{border-left:1px solid var(--line-strong)}\
.view-switch button[aria-pressed=true]{background:var(--accent-wash);color:var(--accent)}\
.home:not(.js) .view-switch{display:none}\
ul.plugins{list-style:none;margin:1.25rem 0 0;padding:0}\
ul.plugins.list{background:var(--card);border:1px solid var(--line);border-radius:var(--radius-lg);box-shadow:var(--shadow)}\
ul.plugins.list li+li{border-top:1px solid var(--line-soft)}\
.plugin-card{display:flex;align-items:center;gap:.9rem;padding:.85rem 1.1rem;color:var(--ink);min-width:0}\
a.plugin-card:hover{background:var(--hover);text-decoration:none}\
ul.plugins.list li:first-child .plugin-card{border-radius:var(--radius-lg) var(--radius-lg) 0 0}\
ul.plugins.list li:last-child .plugin-card{border-radius:0 0 var(--radius-lg) var(--radius-lg)}\
ul.plugins.list li:only-child .plugin-card{border-radius:var(--radius-lg)}\
.plugin-icon{flex-shrink:0;display:grid;place-items:center;width:38px;height:38px;border-radius:var(--radius);\
background:var(--accent-wash);color:var(--accent);font-weight:700;font-size:1.05rem;text-transform:uppercase}\
.plugin-text{display:flex;flex-direction:column;min-width:0;flex:1 1 auto}\
.plugin-name{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.plugin-instance{font:.8rem var(--mono);color:var(--ink-faint);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.plugin-meta{display:flex;gap:.35rem;flex-wrap:wrap;justify-content:flex-end}\
.plugin-open{color:var(--accent);font-weight:550;white-space:nowrap}\
.plugin-card:not(a) .plugin-open{color:var(--ink-faint);font-weight:400}\
ul.plugins.tiles{display:grid;grid-template-columns:repeat(auto-fill,minmax(14rem,1fr));gap:.9rem}\
ul.plugins.tiles .plugin-card{flex-direction:column;align-items:flex-start;gap:.75rem;height:100%;padding:1.1rem;\
background:var(--card);border:1px solid var(--line);border-radius:var(--radius-lg);box-shadow:var(--shadow)}\
ul.plugins.tiles a.plugin-card:hover{border-color:var(--accent);background:var(--card)}\
ul.plugins.tiles .plugin-icon{width:48px;height:48px;font-size:1.3rem}\
ul.plugins.tiles .plugin-text{flex:0 0 auto;width:100%}ul.plugins.tiles .plugin-meta{justify-content:flex-start}\
ul.plugins.tiles .plugin-open{margin-top:auto}\
@media (max-width:30rem){.plugin-card .plugin-meta{display:none}ul.plugins.tiles{grid-template-columns:1fr 1fr;gap:.6rem}\
ul.plugins.tiles .plugin-card{padding:.85rem}}\
.view-grid{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,22rem);gap:1.25rem;align-items:start}\
.view-grid>*+*{margin-top:0!important}.view-grid .stack>*+*{margin-top:1.25rem}\
@media (max-width:60rem){.view-grid{grid-template-columns:minmax(0,1fr)}}\
.panel>h2,.panel .panel-title{margin:0 0 .25rem}\
.facts{display:grid;grid-template-columns:auto minmax(0,1fr);gap:.35rem 1rem;margin:.75rem 0 0;font-size:.9rem}\
.facts dt{color:var(--ink-faint)}.facts dd{margin:0;overflow-wrap:anywhere}\
.flag{display:block;padding:.55rem .8rem;border-radius:var(--radius);\
background:var(--warn-wash);color:var(--warn-ink);font-size:.9rem;margin:.75rem 0 0}\
.flag a{color:inherit;text-decoration:underline}\
.admin-frame{display:block;width:100%;height:70vh;min-height:28rem;border:1px solid var(--line);\
border-radius:var(--radius);background:var(--page)}\
form.settings .setting{padding:1rem 0;border-top:1px solid var(--line-soft)}\
form.settings .setting:first-of-type{border-top:0;padding-top:.25rem}\
form.settings .setting .hint{margin:.3rem 0 0}\
form.settings label.field{margin:0}\
.setting-head{display:flex;align-items:center;flex-wrap:wrap;gap:.4rem;margin-bottom:.35rem;font-size:.9rem}\
.setting-head .setting-label{font-weight:600;color:var(--ink)}\
.setting-head .id{display:inline;margin-left:auto}\
fieldset.choice{border:0;margin:0;padding:0;min-width:0}fieldset.choice legend{padding:0;width:100%}\
.options{display:grid;grid-template-columns:repeat(auto-fit,minmax(12rem,1fr));gap:.5rem}\
label.option{display:flex;align-items:flex-start;gap:.6rem;margin:0;padding:.65rem .8rem;border:1px solid var(--line-strong);\
border-radius:var(--radius);font-weight:400;cursor:pointer;background:var(--card)}\
label.option:hover{background:var(--hover)}\
label.option:has(input:checked){border-color:var(--accent);background:var(--accent-wash)}\
label.option input{margin:.25rem 0 0;accent-color:var(--accent)}\
label.option .option-label{display:block;font-weight:600}label.option .hint{margin:.1rem 0 0}\
.with-unit{display:flex;align-items:stretch;margin-top:.3rem;max-width:20rem}\
.with-unit input{margin:0!important;border-top-right-radius:0!important;border-bottom-right-radius:0!important;flex:1 1 auto;min-width:0}\
.with-unit .unit{display:flex;align-items:center;padding:0 .75rem;border:1px solid var(--line-strong);border-left:0;\
border-radius:0 var(--radius) var(--radius) 0;background:var(--accent-wash);color:var(--accent);font-weight:600;font-size:.86rem}\
.hint.applies{color:var(--violet)}\
label.check{display:flex;gap:.5rem;align-items:center;font-weight:400;font-size:.88rem;margin:.5rem 0 0}\
.form-foot{display:flex;justify-content:flex-end;padding-top:1rem;border-top:1px solid var(--line-soft)}\
.form-foot button{margin:0}";

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

/// The kit's stylesheet on this dashboard's origin, once it is known to be
/// served (set once at start, from [`crate::kit::Kit`]). Unset, a page is
/// its layout alone, as it is in a test.
static KIT_STYLESHEET: OnceLock<String> = OnceLock::new();

pub fn use_kit(stylesheet: String) {
    let _ = KIT_STYLESHEET.set(stylesheet);
}

/// The mark, as on the platform's pages. It takes the colour it is set in.
const MARK: &str = "<svg viewBox=\"0 0 32 32\" aria-hidden=\"true\"><rect x=\"1\" y=\"1\" width=\"30\" \
     height=\"30\" rx=\"8\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\"/><path d=\"M16 5v22M8 9c3 2 \
     5 4.5 5 7s-2 5-5 7M24 9c-3 2-5 4.5-5 7s2 5 5 7\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
     stroke-linecap=\"round\"/></svg>";

const DEVELOPMENT_BANNER: &str = "<p class=\"development\"><strong>Development \
     deployment.</strong> It runs plugin code as it is being written, which nobody \
     has reviewed. Nothing here is for real use.</p>";

/// The mode cookie: `light` or `dark`, or absent for the system's. Written by
/// the header's menu, read by the script in every page's head and by the
/// frame, which hands it to a plugin's page. Not HttpOnly, since the menu
/// sets it from script; it holds nothing but the mode.
pub const MODE_COOKIE: &str = "meridian_mode";

/// Before first paint: the person's mode on `<html>`, as the kit reads it.
const HEAD_SCRIPT: &str = "(function(){try{var m=document.cookie.match(\
/(?:^|;\\s*)(?:__Host-)?meridian_mode=(light|dark)(?:;|$)/);\
if(m)document.documentElement.setAttribute(\"data-om-mode\",m[1]);}catch(e){}})();";

/// The header's menu and every plugin frame on the page: choosing a mode
/// applies it here, remembers it, and tells each framed page by the frame's
/// message (meridian-ui's contract, version 2), as each frame's load does,
/// so a navigation inside it keeps the person's theme.
const CHROME_SCRIPT: &str = r#"(function () {
  var root = document.documentElement;
  function mode() { return root.getAttribute("data-om-mode") || "system"; }
  function tell(frame) {
    var origin = frame.getAttribute("data-origin");
    if (!origin || !frame.contentWindow) return;
    frame.contentWindow.postMessage({ type: "meridian:theme", version: 2, scheme: "default",
      mode: mode(), direction: "green-up" }, origin);
  }
  var frames = Array.prototype.slice.call(document.querySelectorAll("iframe[data-plugin-frame]"));
  frames.forEach(function (frame) { frame.addEventListener("load", function () { tell(frame); }); });
  function mark() {
    document.querySelectorAll("[data-mode]").forEach(function (a) {
      a.setAttribute("aria-current", a.getAttribute("data-mode") === mode() ? "true" : "false");
    });
  }
  mark();
  document.addEventListener("click", function (event) {
    var chosen = event.target.closest("[data-mode]");
    if (!chosen) return;
    event.preventDefault();
    var value = chosen.getAttribute("data-mode");
    var secure = location.protocol === "https:";
    var name = (secure ? "__Host-" : "") + "meridian_mode";
    document.cookie = name + "=" + (value === "system" ? "" : value) + "; Path=/; SameSite=Lax" +
      (value === "system" ? "; Max-Age=0" : "; Max-Age=31536000") + (secure ? "; Secure" : "");
    if (value === "system") root.removeAttribute("data-om-mode"); else root.setAttribute("data-om-mode", value);
    mark();
    frames.forEach(tell);
    var menu = chosen.closest("details"); if (menu) menu.open = false;
  });
})();"#;

/// Who is signed in, for the header.
pub struct Viewer<'a> {
    pub display_name: &'a str,
    pub form_token: &'a str,
    /// A deployment admin, who is shown the way to the admin portal.
    pub admin: bool,
}

/// What a page is, around its body.
#[derive(Default)]
pub struct Chrome<'a> {
    pub viewer: Option<Viewer<'a>>,
    /// Beside the mark: where in the deployment the page is. Already HTML.
    pub crumbs: String,
    /// The main element's class: `sheet` for a form (the default), `page`
    /// for a wide page, `frame` for a plugin's page filling the window.
    pub main: &'a str,
    /// The admin portal is where the person is.
    pub in_admin: bool,
}

/// A whole page with no person in its header. `body` is already HTML;
/// `title` is text.
pub fn page(title: &str, body: &str) -> String {
    page_with(title, body, &Chrome::default())
}

/// A whole page, with the person and where they are in its header.
pub fn page_with(title: &str, body: &str, chrome: &Chrome) -> String {
    document(title, body, chrome, is_development())
}

fn header(chrome: &Chrome) -> String {
    let crumbs = if chrome.crumbs.is_empty() {
        "<span class=\"where\">Deployment</span>".to_string()
    } else {
        format!(
            "<nav class=\"crumbs\" aria-label=\"Where you are\">{}</nav>",
            chrome.crumbs
        )
    };
    let right = match &chrome.viewer {
        None => String::new(),
        Some(viewer) => {
            let admin = if viewer.admin {
                format!(
                    "<a class=\"bar-link{}\" href=\"/admin\">Admin portal</a>",
                    if chrome.in_admin { " here" } else { "" }
                )
            } else {
                String::new()
            };
            let initial: String = viewer
                .display_name
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_else(|| "?".into());
            format!(
                "{admin}<details class=\"menu person\"><summary aria-label=\"{name}\">\
                 <span class=\"avatar\" aria-hidden=\"true\">{initial}</span>\
                 <span class=\"person-name\">{name}</span></summary>\
                 <div class=\"menu-pop\"><div class=\"menu-label\">Signed in as <strong>{name}</strong></div><hr>\
                 <div class=\"menu-label\">Appearance</div>\
                 <a href=\"/mode?set=system\" data-mode=\"system\">System</a>\
                 <a href=\"/mode?set=light\" data-mode=\"light\">Light</a>\
                 <a href=\"/mode?set=dark\" data-mode=\"dark\">Dark</a><hr>\
                 <form method=\"post\" action=\"/sign-out\"><input type=\"hidden\" name=\"form_token\" \
                 value=\"{token}\"><button type=\"submit\">Sign out</button></form></div></details>",
                name = escape(viewer.display_name),
                initial = escape(&initial),
                token = escape(viewer.form_token),
            )
        }
    };
    format!(
        "<header class=\"bar\"><a class=\"brand\" href=\"/\">{MARK}<span>Open Meridian</span></a>\
         {crumbs}<span class=\"spacer\"></span>{right}</header>"
    )
}

fn document(title: &str, body: &str, chrome: &Chrome, development: bool) -> String {
    let banner = if development { DEVELOPMENT_BANNER } else { "" };
    let kit = KIT_STYLESHEET
        .get()
        .map(|href| format!("<link rel=\"stylesheet\" href=\"{}\">", escape(href)))
        .unwrap_or_default();
    let main = if chrome.main.is_empty() {
        "sheet"
    } else {
        chrome.main
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{} · Open Meridian</title><script>{HEAD_SCRIPT}</script>{kit}<style>{STYLE}</style></head><body>\n\
         {}\n{banner}\n<main class=\"{main}\">\n{}\n</main>\n<script>{CHROME_SCRIPT}</script>\n</body></html>\n",
        escape(title),
        header(chrome),
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
        let chrome = Chrome::default();
        let marked = document("Home", "<h1>Meridian</h1>", &chrome, true);
        assert!(marked.contains("class=\"development\""), "{marked}");
        assert!(marked.find("Development").unwrap() < marked.find("<h1>").unwrap());
        assert!(!document("Home", "<h1>Meridian</h1>", &chrome, false)
            .contains("class=\"development\""));
    }

    #[test]
    fn a_title_cannot_inject_markup() {
        assert!(page("<b>", "").contains("<title>&lt;b&gt; · Open Meridian</title>"));
    }

    #[test]
    fn the_header_names_the_person_signs_them_out_and_shows_an_admin_the_portal() {
        let viewer = |admin| Viewer {
            display_name: "Ada <Park>",
            form_token: "tok-1",
            admin,
        };
        let admin = page_with(
            "Home",
            "",
            &Chrome {
                viewer: Some(viewer(true)),
                ..Default::default()
            },
        );
        let head = admin.split("</header>").next().unwrap();
        assert!(head.contains("Ada &lt;Park&gt;"), "{head}");
        assert!(head.contains("action=\"/sign-out\"") && head.contains("value=\"tok-1\""));
        assert!(head.contains("href=\"/admin\">Admin portal<"));

        let person = page_with(
            "Home",
            "",
            &Chrome {
                viewer: Some(viewer(false)),
                ..Default::default()
            },
        );
        assert!(!person.contains("Admin portal"));
        assert!(
            !page("Sign in", "").contains("/sign-out"),
            "nobody to sign out"
        );
    }

    #[test]
    fn no_colour_is_written_here_only_the_kits_properties() {
        // decisions/025: a colour a scheme cannot change is one no contrast
        // check has seen. The dashboard is held to the kit's rule.
        for raw in ["#", "rgb(", "rgba(", "hsl("] {
            assert!(!STYLE.contains(raw), "a raw colour ({raw}) in the style");
        }
    }
}
