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
header.bar .crumbs{display:flex;align-items:center;gap:.5rem;min-width:0;padding-left:.75rem;\
border-left:1px solid var(--line);font:400 1em/1.55 var(--sans);color:var(--ink-soft)}\
header.bar .crumbs a{color:var(--ink-soft);white-space:nowrap}header.bar .crumbs a:hover{color:var(--ink)}\
header.bar .crumbs .sep{color:var(--ink-faint)}\
header.bar .crumbs .here{min-width:0;color:var(--ink);font-weight:500;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
header.bar .crumbs .own-window{color:var(--ink-faint);padding:0 .25rem}\
header.bar .spacer{flex:1 1 auto}\
header.bar .bar-link{display:inline-flex;align-items:center;gap:.4rem;padding:.35rem .7rem;border-radius:var(--radius);\
color:var(--ink-soft);font-weight:550;white-space:nowrap}\
header.bar .bar-link:hover{background:var(--hover);color:var(--ink);text-decoration:none}\
header.bar .bar-link.here{background:var(--accent-wash);color:var(--accent)}\
header.bar .bar-link.side{padding:.4rem}header.bar .bar-link.side svg{display:block;width:20px;height:20px}\
header.bar .person>summary{display:flex;align-items:center;gap:.5rem;padding:.3rem .5rem}\
header.bar .person .avatar{display:inline-grid;place-items:center;width:28px;height:28px;border-radius:50%;\
background:var(--accent-wash);color:var(--accent);font-weight:650;font-size:.8rem}\
header.bar .person .person-name{max-width:14rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--ink);font-weight:550}\
header.bar .person .menu-pop form{margin:0}\
header.bar .menu-pop a{color:var(--ink)}header.bar .menu-pop button{width:100%;margin:0;padding:.45rem .6rem;border:0;\
background:none;font-weight:400;justify-content:flex-start}header.bar .menu-pop button:hover{background:var(--hover)}\
.menu-label{padding:.35rem .6rem .15rem;font:600 .68rem var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--ink-faint)}\
.menu-pop a[aria-current=true]::after{content:\"\\2713\";margin-left:auto;color:var(--accent)}\
@media (max-width:40rem){header.bar{padding:0 .75rem;gap:.4rem}header.bar .person .person-name,header.bar a.brand span{display:none}\
header.bar .crumbs{padding-left:.5rem}header.bar .crumbs>a,header.bar .crumbs>.sep{display:none}\
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
button.danger{background:var(--danger-wash);border-color:var(--danger);color:var(--danger)}\
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
.page-head .actions{display:flex;gap:.5rem;flex-wrap:wrap}.page-head .actions button{margin:0}\
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
table.list .name{font-weight:550}table.list .hint{margin:.15rem 0 0}\
table.list .note{overflow-wrap:anywhere}\
.admin.js table.list .note{max-width:22rem;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
button.note-mark{display:inline-grid;place-items:center;width:1.05rem;height:1.05rem;margin:0 0 0 .4rem;padding:0;\
border-radius:50%;border:1px solid var(--line-strong);background:var(--card);color:var(--ink-soft);\
font:600 .66rem/1 var(--mono);vertical-align:.1em;cursor:help}\
button.note-mark::before{content:\"i\"}\
button.note-mark:hover,button.note-mark:focus-visible{border-color:var(--accent);color:var(--accent);background:var(--card)}\
button.note-mark:focus-visible{outline:none;box-shadow:0 0 0 3px var(--accent-wash)}\
.admin:not(.js) button.note-mark{display:none}\
.note-bubble{position:absolute;z-index:20;max-width:min(24rem,calc(100vw - 1rem));padding:.55rem .75rem;\
background:var(--card);color:var(--ink);border:1px solid var(--line);border-radius:var(--radius);\
box-shadow:var(--shadow-pop);font-size:.88rem;line-height:1.45;white-space:pre-wrap;overflow-wrap:anywhere}\
input.filter{display:block;width:min(100%,26rem);margin:0 0 .9rem}table.plugins td:first-child{white-space:nowrap}\
.filter-row{display:flex;align-items:center;gap:.75rem;flex-wrap:wrap;margin:0 0 .9rem}.filter-row input.filter{margin:0}\
.filter-count{color:var(--ink-faint);font-size:.84rem}\
th button.sort{margin:0;padding:0;border:0;background:none;font:inherit;color:inherit;letter-spacing:inherit;text-transform:inherit;cursor:pointer}\
th[aria-sort=ascending] button.sort::after{content:\" \\2191\"}th[aria-sort=descending] button.sort::after{content:\" \\2193\"}\
.more{color:var(--ink-faint)}\
fieldset.checks.picker{max-height:none;overflow:visible;min-width:0}\
.picker-tools{display:flex;gap:.5rem;align-items:center;flex-wrap:wrap;margin:.25rem 0 .5rem}\
.picker-tools input[type=search]{flex:1 1 12rem;min-width:0}.picker-tools button{margin:0;padding:.3rem .7rem;font-size:.84rem}\
.picker-status{margin:0 0 .35rem;color:var(--ink-faint);font-size:.84rem}\
ul.picker-chosen{list-style:none;display:flex;flex-wrap:wrap;gap:.35rem;margin:0 0 .5rem;padding:0}\
ul.picker-chosen:empty{display:none}\
ul.picker-chosen button{margin:0;padding:.1rem .55rem;border-radius:99px;font-size:.8rem;font-weight:500;\
background:var(--accent-wash);border-color:var(--accent);color:var(--accent)}\
ul.picker-chosen .more{align-self:center;font-size:.8rem}\
.picker-options{max-height:16rem;overflow:auto;border-top:1px solid var(--line-soft);padding-top:.25rem}\
.picker-option{display:flex;align-items:center;gap:.5rem;justify-content:space-between}\
.picker-option label.check{margin:.2rem 0;min-width:0;flex:1 1 auto;overflow-wrap:anywhere}\
.picker-option label.check .id{display:inline;margin-left:.35rem}\
.picker-option select{width:auto;min-height:0;padding:.2rem .4rem;font-size:.84rem;flex-shrink:0}\
.picker-none{margin:.5rem 0;color:var(--ink-soft);font-size:.88rem}\
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
.admin-frame{display:block;width:100%;height:70vh;min-height:28rem;border:0;border-radius:0;background:none;\
color-scheme:light dark}.admin-frame[data-sized]{min-height:0}\
html[data-om-mode=light] .admin-frame{color-scheme:light}html[data-om-mode=dark] .admin-frame{color-scheme:dark}\
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

/// The side button's icons (the product owner, 2026-09-30): a gear for
/// Settings and a house for the Dashboard, drawn here in the header's line
/// and taking the colour they are set in. No icon font, nothing from outside.
const GEAR: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\"><path d=\"M19 9.7 \
     21.4 9.9 21.4 14.1 19 14.3 18.6 15.4 20.1 17.2 17.2 20.1 15.4 18.6 14.3 19 14.1 21.4 9.9 21.4 9.7 19 8.6 18.6 \
     6.8 20.1 3.9 17.2 5.4 15.4 5 14.3 2.6 14.1 2.6 9.9 5 9.7 5.4 8.6 3.9 6.8 6.8 3.9 8.6 5.4 9.7 5 9.9 2.6 14.1 2.6 \
     14.3 5 15.4 5.4 17.2 3.9 20.1 6.8 18.6 8.6Z\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.7\" \
     stroke-linejoin=\"round\"/><circle cx=\"12\" cy=\"12\" r=\"3\" fill=\"none\" stroke=\"currentColor\" \
     stroke-width=\"1.7\"/></svg>";

const HOUSE: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\"><path d=\"M3.5 11.2 \
     12 4l8.5 7.2M5.8 9.4V20h12.4V9.4M10 20v-5.5h4V20\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.7\" \
     stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

/// A deployment admin's way between the two sides: a gear to Settings from
/// the dashboard, a house to the Dashboard from Settings. Named for a screen
/// reader and a pointer alike, since it has no words.
fn side_button(in_admin: bool) -> String {
    let (href, name, icon) = if in_admin {
        ("/", "Dashboard", HOUSE)
    } else {
        ("/admin", "Settings", GEAR)
    };
    format!(
        "<a class=\"bar-link side\" href=\"{href}\" aria-label=\"{name}\" title=\"{name}\">{icon}</a>"
    )
}

/// The breadcrumb's last crumb: where the person is, never a link to itself.
/// `tooltip` is shown on hover (a plugin's instance ID beside its name).
pub fn crumb_here(name: &str, tooltip: Option<&str>) -> String {
    let title = tooltip
        .map(|t| format!(" title=\"{}\"", escape(t)))
        .unwrap_or_default();
    format!(
        "<span class=\"here\" aria-current=\"page\"{title}>{}</span>",
        escape(name)
    )
}

/// A crumb on the way back, and the separator after it.
pub fn crumb_link(href: &str, name: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a><span class=\"sep\" aria-hidden=\"true\">/</span>",
        escape(href),
        escape(name)
    )
}

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
/// message (meridian-ui's contract), as each frame's load does, so a
/// navigation inside it keeps the person's theme. A seamless frame
/// (`data-seamless`, the admin view's) is told version 3 with `framed: true`,
/// and is as tall as its page says it is; the full-page frame is told
/// version 2, which says nothing of framing.
const CHROME_SCRIPT: &str = r#"(function () {
  var root = document.documentElement;
  function mode() { return root.getAttribute("data-om-mode") || "system"; }
  function tell(frame) {
    var origin = frame.getAttribute("data-origin");
    if (!origin || !frame.contentWindow) return;
    var seamless = frame.hasAttribute("data-seamless");
    var message = { type: "meridian:theme", version: seamless ? 3 : 2, scheme: "default",
      mode: mode(), direction: "green-up" };
    if (seamless) message.framed = true;
    frame.contentWindow.postMessage(message, origin);
  }
  var frames = Array.prototype.slice.call(document.querySelectorAll("iframe[data-plugin-frame]"));
  // Each load is a new page: its header actions go until it offers its own.
  frames.forEach(function (frame) { frame.addEventListener("load", function () { draw(frame, []); tell(frame); }); });
  // A seamless frame's height is its page's, by meridian:size (meridian-ui's
  // README, "The frame: seamless"): taken only from that frame's own window,
  // from exactly the origin its theme is told to, as a whole number of
  // pixels, and never more than TALLEST, since a height is only the page's
  // request. Until the first, the stylesheet's height holds, so a page on a
  // kit without the message still shows and the frame is never 0 tall.
  var TALLEST = 20000;
  window.addEventListener("message", function (event) {
    var data = event.data;
    frames.forEach(function (frame) {
      if (!frame.hasAttribute("data-seamless") || !frame.contentWindow) return;
      if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;
      if (!data || data.type !== "meridian:size" || data.version !== 1) return;
      if (!Number.isInteger(data.height) || data.height < 0) return;
      frame.style.height = Math.min(data.height, TALLEST) + "px";
      frame.setAttribute("data-sized", "");
    });
  });
  // A seamless frame's header actions, by meridian:actions (meridian-ui's
  // README, "The frame: seamless"): the page's own buttons, drawn in the
  // header's area its frame names (data-actions), under the size's guards and
  // only in the kit's shape, else not at all. A label is text, never markup;
  // a click is told back to the page, at the plugin's origin alone, and the
  // page presses its own button, so its form posts with its own token.
  var MOST_ACTIONS = 4;
  var LONGEST_LABEL = 40;
  var ACTION_ID = /^[a-z0-9][a-z0-9-]{0,31}$/;
  function offered(list) {
    if (!Array.isArray(list) || list.length > MOST_ACTIONS) return null;
    var ids = [];
    for (var i = 0; i < list.length; i++) {
      var a = list[i];
      if (!a || typeof a !== "object" || Array.isArray(a)) return null;
      if (typeof a.id !== "string" || !ACTION_ID.test(a.id) || ids.indexOf(a.id) !== -1) return null;
      if (typeof a.label !== "string" || !a.label.trim() || a.label.length > LONGEST_LABEL) return null;
      if (a.tone !== undefined && a.tone !== "primary" && a.tone !== "danger") return null;
      if (a.disabled !== undefined && typeof a.disabled !== "boolean") return null;
      ids.push(a.id);
    }
    return list;
  }
  function draw(frame, list) {
    var area = frame.hasAttribute("data-actions") && document.getElementById(frame.getAttribute("data-actions"));
    if (!area) return;
    area.replaceChildren.apply(area, list.map(function (a) {
      var button = document.createElement("button");
      button.type = "button";
      button.textContent = a.label;
      if (a.tone) button.className = a.tone;
      button.disabled = a.disabled === true;
      button.addEventListener("click", function () {
        if (!frame.contentWindow) return;
        frame.contentWindow.postMessage({ type: "meridian:action", version: 1, id: a.id }, frame.dataset.origin);
      });
      return button;
    }));
  }
  window.addEventListener("message", function (event) {
    var data = event.data;
    frames.forEach(function (frame) {
      if (!frame.hasAttribute("data-seamless") || !frame.contentWindow) return;
      if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;
      if (!data || data.type !== "meridian:actions" || data.version !== 1) return;
      var list = offered(data.actions);
      if (list) draw(frame, list);
    });
  });
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
  // A search box names the list it narrows (data-filter="id"): a table's
  // rows, or a list's items. Those whose text holds every word typed stay,
  // the rest hide. Each item's text is read once, and a keystroke only flips
  // the items whose state changes, at most once a frame, so a few thousand
  // stay quick. data-filter-none="id" is said when nothing matches, and
  // data-filter-count="id" how many are shown. Without script, every item
  // shows and the box stays hidden.
  function each(selector, act) { Array.prototype.forEach.call(document.querySelectorAll(selector), act); }
  each("input[data-filter]", function (box) {
    var id = box.getAttribute("data-filter");
    var list = document.getElementById(id);
    if (!list) return;
    var items = Array.prototype.slice.call(list.tagName === "TABLE" ? list.querySelectorAll("tbody tr") : list.children);
    var texts = items.map(function (item) { return item.textContent.toLowerCase(); });
    var none = document.querySelector("[data-filter-none=\"" + id + "\"]");
    var count = document.querySelector("[data-filter-count=\"" + id + "\"]");
    var pending = false;
    function narrow() {
      pending = false;
      var words = box.value.toLowerCase().split(/\s+/).filter(Boolean);
      var shown = 0;
      for (var i = 0; i < items.length; i++) {
        var hide = !words.every(function (w) { return texts[i].indexOf(w) !== -1; });
        if (items[i].hidden !== hide) items[i].hidden = hide;
        if (!hide) shown++;
      }
      if (none) none.hidden = shown !== 0;
      if (count) count.textContent = words.length ? shown + " of " + items.length + " shown" : items.length + " in all";
    }
    box.hidden = false;
    if (count) { count.hidden = false; narrow(); }
    box.addEventListener("input", function () {
      if (pending) return;
      pending = true;
      window.requestAnimationFrame(narrow);
    });
  });
  // A table marked data-sortable sorts by a column when its heading is
  // pressed, and back the other way when pressed again: text in the page's
  // language, numbers as numbers. Without script, the order is the server's.
  each("table[data-sortable]", function (table) {
    var body = table.tBodies[0];
    if (!body) return;
    var collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
    Array.prototype.forEach.call(table.tHead ? table.tHead.rows[0].cells : [], function (th, column) {
      if (!th.textContent.trim()) return;
      var button = document.createElement("button");
      button.type = "button";
      button.className = "sort";
      button.textContent = th.textContent;
      th.textContent = "";
      th.appendChild(button);
      button.addEventListener("click", function () {
        var up = th.getAttribute("aria-sort") !== "ascending";
        Array.prototype.forEach.call(table.tHead.rows[0].cells, function (other) { other.removeAttribute("aria-sort"); });
        th.setAttribute("aria-sort", up ? "ascending" : "descending");
        var rows = Array.prototype.slice.call(body.rows);
        rows.sort(function (a, b) {
          var x = a.cells[column] ? a.cells[column].textContent.trim() : "";
          var y = b.cells[column] ? b.cells[column].textContent.trim() : "";
          return up ? collator.compare(x, y) : collator.compare(y, x);
        });
        var sorted = document.createDocumentFragment();
        rows.forEach(function (row) { sorted.appendChild(row); });
        body.appendChild(sorted);
      });
    });
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
    // Home is the Dashboard: a crumb as any other page's last one.
    let crumbs = format!(
        "<nav class=\"crumbs\" aria-label=\"Where you are\">{}</nav>",
        if chrome.crumbs.is_empty() {
            crumb_here("Dashboard", None)
        } else {
            chrome.crumbs.clone()
        }
    );
    let right = match &chrome.viewer {
        None => String::new(),
        Some(viewer) => {
            // An admin moves between the two sides: one button to the side
            // they are not on, Settings from the dashboard and the Dashboard
            // from Settings, each of which has its Plugins (the product
            // owner, 2026-09-29), drawn as a gear and a house (2026-09-30).
            // A person with no admin has one side, and no button.
            let admin = if viewer.admin {
                side_button(chrome.in_admin)
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

    /// The size listener, as it is written: the workspace runs no script, so
    /// what it takes is held here line by line (and was run in a browser
    /// against a stand-in plugin page on another origin when written).
    #[test]
    fn a_seamless_frame_takes_its_height_only_from_its_own_page_as_a_whole_number() {
        let listener = CHROME_SCRIPT
            .split("window.addEventListener(\"message\"")
            .nth(1)
            .expect("the size listener")
            .split("\n  });\n")
            .next()
            .unwrap();
        for guard in [
            // Only a seamless frame, and only from its own window,
            "if (!frame.hasAttribute(\"data-seamless\") || !frame.contentWindow) return;",
            // from exactly the plugin's origin its theme is told to,
            "if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;",
            // as the message it is, at the version it is,
            "if (!data || data.type !== \"meridian:size\" || data.version !== 1) return;",
            // and a whole number of pixels, none less than none;
            "if (!Number.isInteger(data.height) || data.height < 0) return;",
            // then no taller than the dashboard's cap.
            "frame.style.height = Math.min(data.height, TALLEST) + \"px\";",
        ] {
            assert!(listener.contains(guard), "{guard}\nnot in:{listener}");
        }
        let guards = [
            "hasAttribute",
            "event.source",
            "\"meridian:size\"",
            "Number.isInteger",
            "Math.min",
        ];
        let at: Vec<usize> = guards.iter().map(|g| listener.find(g).unwrap()).collect();
        assert!(
            at.windows(2).all(|w| w[0] < w[1]),
            "every check before the height is set"
        );
        assert!(CHROME_SCRIPT.contains("var TALLEST = 20000;"));

        // A seamless frame is told it is framed, at version 3; the full-page
        // frame is told version 2, which says nothing of framing.
        assert!(CHROME_SCRIPT.contains("version: seamless ? 3 : 2,"));
        assert!(CHROME_SCRIPT.contains("if (seamless) message.framed = true;"));
        assert!(CHROME_SCRIPT.contains("postMessage(message, origin)"));
        assert!(!CHROME_SCRIPT.contains("\"*\""), "never to any origin");
    }

    #[test]
    fn a_seamless_frame_has_no_edge_of_its_own_and_a_height_until_its_page_says() {
        let frame = STYLE
            .split(".admin-frame{")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        for rule in [
            "border:0",
            "border-radius:0",
            "background:none",
            "width:100%",
            "height:70vh",
            "min-height:28rem",
        ] {
            assert!(frame.contains(rule), "{rule} not in {frame}");
        }
        // Once the page has said, its height alone, however small.
        assert!(STYLE.contains(".admin-frame[data-sized]{min-height:0}"));
        // The person's mode, as the page's, so the transparent page sits on
        // the dashboard rather than on an opaque canvas.
        assert!(frame.contains("color-scheme:light dark"));
        assert!(STYLE.contains("html[data-om-mode=light] .admin-frame{color-scheme:light}"));
        assert!(STYLE.contains("html[data-om-mode=dark] .admin-frame{color-scheme:dark}"));
    }

    #[test]
    fn a_title_cannot_inject_markup() {
        assert!(page("<b>", "").contains("<title>&lt;b&gt; · Open Meridian</title>"));
    }

    #[test]
    fn the_header_names_the_person_signs_them_out_and_shows_an_admin_the_other_side() {
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
        // From the dashboard, a gear to Settings: named, and with a tooltip,
        // since it has no words.
        let side = head
            .split("<a class=\"bar-link side\" ")
            .nth(1)
            .and_then(|rest| rest.split("</a>").next())
            .expect("the side button");
        assert!(
            side.starts_with("href=\"/admin\" aria-label=\"Settings\" title=\"Settings\">"),
            "{side}"
        );
        assert!(side.contains(GEAR) && !side.contains(HOUSE), "{side}");
        assert!(
            !head.contains("aria-label=\"Dashboard\""),
            "and not the side they are on"
        );
        let in_admin = page_with(
            "Settings",
            "",
            &Chrome {
                viewer: Some(viewer(true)),
                in_admin: true,
                ..Default::default()
            },
        );
        let head = in_admin.split("</header>").next().unwrap();
        assert!(
            head.contains(&format!(
                "<a class=\"bar-link side\" href=\"/\" aria-label=\"Dashboard\" title=\"Dashboard\">{HOUSE}</a>"
            )) && !head.contains("aria-label=\"Settings\""),
            "in Settings, a house back to the Dashboard, and only that: {head}"
        );

        let person = page_with(
            "Home",
            "",
            &Chrome {
                viewer: Some(viewer(false)),
                ..Default::default()
            },
        );
        assert!(
            !person.contains("bar-link side"),
            "a person with no admin has one side"
        );
        assert!(!person.contains("aria-label=\"Settings\"") && !person.contains("href=\"/admin\""));
        assert!(
            !page("Sign in", "").contains("/sign-out"),
            "nobody to sign out"
        );
    }

    #[test]
    fn the_side_buttons_icons_are_drawn_here_in_the_colour_they_are_set_in() {
        for icon in [GEAR, HOUSE] {
            assert!(icon.starts_with(
                "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\">"
            ));
            assert!(icon.ends_with("</svg>"));
            assert!(icon.contains("stroke=\"currentColor\""), "{icon}");
            // Nothing from elsewhere: no reference, no font, no other colour.
            for outside in ["href", "url(", "xlink", "<use", "<image", "#", "rgb"] {
                assert!(!icon.contains(outside), "{outside} in {icon}");
            }
        }
        assert!(
            STYLE.contains("header.bar .bar-link.side svg{display:block;width:20px;height:20px}")
        );
    }

    /// Every crumb in the header, on every kind of page, is drawn one way: the
    /// body's font at one size, links soft, the current page ink at one
    /// weight and never a link to itself, the separators faint.
    #[test]
    fn every_crumb_is_one_style_and_the_current_one_is_never_a_link() {
        let crumbs = |chrome: &Chrome| -> String {
            let head = header(chrome);
            head.split("<nav class=\"crumbs\" aria-label=\"Where you are\">")
                .nth(1)
                .and_then(|rest| rest.split("</nav>").next())
                .expect("the crumbs")
                .to_string()
        };
        // Home: the Dashboard, as any page's last crumb.
        let home = crumbs(&Chrome::default());
        assert_eq!(
            home,
            "<span class=\"here\" aria-current=\"page\">Dashboard</span>"
        );
        assert!(!STYLE.contains(".where"), "no crumb of a style of its own");
        // Deeper: links back, then where the person is, named, its ID on hover.
        let deep = crumbs(&Chrome {
            crumbs: format!(
                "{}{}{}",
                crumb_link("/admin", "Settings"),
                crumb_link("/admin#plugins", "Plugins"),
                crumb_here("Snap <Trade>", Some("snaptrade-1"))
            ),
            ..Default::default()
        });
        assert_eq!(
            deep,
            "<a href=\"/admin\">Settings</a><span class=\"sep\" aria-hidden=\"true\">/</span>\
             <a href=\"/admin#plugins\">Plugins</a><span class=\"sep\" aria-hidden=\"true\">/</span>\
             <span class=\"here\" aria-current=\"page\" title=\"snaptrade-1\">Snap &lt;Trade&gt;</span>"
        );
        for markup in ["<strong", "<code", "class=\"where\""] {
            assert!(!deep.contains(markup) && !home.contains(markup), "{markup}");
        }
        for rule in [
            "header.bar .crumbs{display:flex;align-items:center;gap:.5rem;min-width:0;padding-left:.75rem;\
             border-left:1px solid var(--line);font:400 1em/1.55 var(--sans);color:var(--ink-soft)}",
            "header.bar .crumbs a{color:var(--ink-soft);white-space:nowrap}",
            "header.bar .crumbs .sep{color:var(--ink-faint)}",
            "header.bar .crumbs .here{min-width:0;color:var(--ink);font-weight:500;",
        ] {
            assert!(STYLE.contains(rule), "{rule}");
        }
        // On a phone, the leading crumbs go and the current one stays.
        assert!(STYLE.contains(
            "header.bar .crumbs{padding-left:.5rem}header.bar .crumbs>a,header.bar .crumbs>.sep{display:none}"
        ));
    }

    /// The header-actions listener, as it is written (as the size's is held
    /// above): the same guards as the size's, then the kit's shape, before
    /// anything is drawn; drawn as text; a click told to the plugin alone.
    #[test]
    fn a_seamless_frames_header_actions_are_taken_only_from_its_own_page_in_the_kits_shape() {
        let listener = CHROME_SCRIPT
            .split("window.addEventListener(\"message\"")
            .nth(2)
            .expect("the actions listener")
            .split("\n  });\n")
            .next()
            .unwrap();
        let guards = [
            "if (!frame.hasAttribute(\"data-seamless\") || !frame.contentWindow) return;",
            "if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;",
            "if (!data || data.type !== \"meridian:actions\" || data.version !== 1) return;",
            "var list = offered(data.actions);",
            "if (list) draw(frame, list);",
        ];
        let mut at = Vec::new();
        for guard in guards {
            at.push(
                listener
                    .find(guard)
                    .unwrap_or_else(|| panic!("{guard}\nnot in:{listener}")),
            );
        }
        assert!(
            at.windows(2).all(|w| w[0] < w[1]),
            "every check before anything is drawn"
        );

        // The shape: at most a few, each id the kit's and once, a short label,
        // a tone it knows and a boolean; anything else refuses the message.
        let shape = CHROME_SCRIPT
            .split("function offered(list) {")
            .nth(1)
            .and_then(|rest| rest.split("\n  }\n").next())
            .expect("the shape");
        for check in [
            "if (!Array.isArray(list) || list.length > MOST_ACTIONS) return null;",
            "if (!a || typeof a !== \"object\" || Array.isArray(a)) return null;",
            "if (typeof a.id !== \"string\" || !ACTION_ID.test(a.id) || ids.indexOf(a.id) !== -1) return null;",
            "if (typeof a.label !== \"string\" || !a.label.trim() || a.label.length > LONGEST_LABEL) return null;",
            "if (a.tone !== undefined && a.tone !== \"primary\" && a.tone !== \"danger\") return null;",
            "if (a.disabled !== undefined && typeof a.disabled !== \"boolean\") return null;",
        ] {
            assert!(shape.contains(check), "{check}\nnot in:{shape}");
        }
        assert!(CHROME_SCRIPT.contains("var MOST_ACTIONS = 4;"));
        assert!(CHROME_SCRIPT.contains("var LONGEST_LABEL = 40;"));
        assert!(CHROME_SCRIPT.contains("var ACTION_ID = /^[a-z0-9][a-z0-9-]{0,31}$/;"));

        let draw = CHROME_SCRIPT
            .split("function draw(frame, list) {")
            .nth(1)
            .and_then(|rest| rest.split("\n  }\n").next())
            .expect("the drawing");
        assert!(
            draw.contains("button.textContent = a.label;"),
            "a label is text"
        );
        assert!(
            !draw.contains("innerHTML") && !CHROME_SCRIPT.contains("innerHTML"),
            "never markup"
        );
        assert!(
            draw.contains("if (a.tone) button.className = a.tone;"),
            "a tone already checked"
        );
        assert!(draw.contains(
            "frame.contentWindow.postMessage({ type: \"meridian:action\", version: 1, id: a.id }, frame.dataset.origin);"
        ), "the click, to the plugin's origin alone");
        assert!(!CHROME_SCRIPT.contains("\"*\""), "never to any origin");
        // A new page in the frame is offered nothing until it says.
        assert!(CHROME_SCRIPT.contains(
            "frame.addEventListener(\"load\", function () { draw(frame, []); tell(frame); });"
        ));
        // The tones the header can draw.
        assert!(STYLE.contains("button.danger{background:var(--danger-wash);border-color:var(--danger);color:var(--danger)}"));
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
