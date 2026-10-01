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
.plugin-area nav.tabs{margin-bottom:var(--space-5)}.plugin-area .head-side{display:flex;gap:.6rem;align-items:center;flex-wrap:wrap}\
.plugin-area .area-drawn>*+*{margin-top:1.25rem}\
.figures{display:grid;grid-template-columns:repeat(auto-fill,minmax(11rem,1fr));gap:.75rem}.figures:empty{display:none}\
.reserved{margin:.75rem 0 0;padding:.55rem .8rem;border:1px dashed var(--line-strong);border-radius:var(--radius);\
color:var(--ink-faint);font-size:.88rem}\
.plugin-area .area-title{display:flex;align-items:center;gap:.45rem;min-width:0}.plugin-area .area-title h1{min-width:0}\
a.home-link{display:inline-flex;flex-shrink:0;padding:.3rem;border-radius:var(--radius);color:var(--ink-soft)}\
a.home-link:hover{background:var(--hover);color:var(--ink);text-decoration:none}\
a.home-link:focus-visible{outline:none;box-shadow:0 0 0 3px var(--accent-wash)}a.home-link svg{display:block;width:22px;height:22px}\
.level-switch{display:inline-flex;border:1px solid var(--line-strong);border-radius:var(--radius);overflow:hidden}\
.level-switch a{padding:.3rem .75rem;font-size:.86rem;color:var(--ink-soft);background:var(--card)}\
.level-switch a+a{border-left:1px solid var(--line-strong)}\
.level-switch a:hover{text-decoration:none;background:var(--hover)}\
.level-switch a.here{background:var(--accent-wash);color:var(--accent);font-weight:600}\
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
.note-bubble.on-bar{position:fixed;z-index:31}html[data-script] .noted{display:none}\
button.badge,button.pill{margin:0;border:0;font-family:inherit;line-height:inherit;vertical-align:middle}\
html[data-script] [data-note]{cursor:help}\
button.badge:focus-visible,button.pill:focus-visible{outline:none;box-shadow:0 0 0 3px var(--accent-wash)}\
.plugin-area .title-status{display:inline-flex;align-items:center;flex-shrink:0}\
.plugin-area .title-status:empty{display:none}\
.status-dot{display:inline-flex;align-items:center;justify-content:center;min-width:1.5rem;min-height:1.5rem;\
margin:-.275rem;padding:0;border:0;border-radius:50%;background:none;flex-shrink:0}.status-dot:hover{background:none}\
.status-dot:focus-visible{outline:none;box-shadow:0 0 0 3px var(--accent-wash)}\
.status-dot::before{content:\"\";display:inline-flex;align-items:center;justify-content:center;box-sizing:border-box;\
width:.95rem;height:.95rem;border-radius:50%;background:var(--ink-faint);color:var(--card);font:800 .62rem/1 var(--sans)}\
.status-dot[data-state=ok]::before{background:var(--good);content:\"\\2713\";content:\"\\2713\" / \"\"}\
.status-dot[data-state=error]::before{background:var(--danger);content:\"!\";content:\"!\" / \"\"}\
.status-dot[data-state=warn]::before{background:var(--warn-ink);content:\"!\";content:\"!\" / \"\";width:1.05rem;\
border-radius:0;padding-top:.2rem;clip-path:polygon(50% 0,100% 100%,0 100%)}\
.status-dot[data-state=busy]::before{background:transparent;border:2px solid var(--warn-ink);border-top-color:transparent;\
animation:status-turn 1.4s linear infinite}@keyframes status-turn{to{transform:rotate(1turn)}}\
@media (prefers-reduced-motion:reduce){.status-dot[data-state=busy]::before{animation:none}}\
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
.plugin-main{display:flex;align-items:center;gap:.9rem;min-width:0;flex:1 1 auto;color:inherit}\
.plugin-levels{display:flex;gap:.35rem;flex-wrap:wrap;justify-content:flex-end}\
a.plugin-level{padding:.28rem .7rem;border:1px solid var(--line-strong);border-radius:var(--radius);\
font-size:.86rem;font-weight:550;color:var(--accent);background:var(--card);white-space:nowrap}\
a.plugin-level:hover{text-decoration:none;background:var(--accent-wash);border-color:var(--accent)}\
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
.plugin-frame{display:block;width:100%;height:70vh;min-height:28rem;border:0;border-radius:0;background:none;\
color-scheme:light dark}.plugin-frame[data-sized]{min-height:0}\
html[data-om-mode=light] .plugin-frame{color-scheme:light}html[data-om-mode=dark] .plugin-frame{color-scheme:dark}\
form.settings{max-width:60rem}\
form.settings .fields{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.7rem 1.25rem;align-items:start}\
form.settings .setting{min-width:0}form.settings .setting.wide{grid-column:1/-1}\
@media (max-width:40rem){form.settings .fields{grid-template-columns:minmax(0,1fr)}}\
.setting-head{display:flex;align-items:center;flex-wrap:wrap;gap:.25rem .35rem;margin-bottom:.2rem;font-size:.86rem;line-height:1.35}\
.setting-head .setting-label{display:inline;margin:0;font-size:inherit;font-weight:600;color:var(--ink)}\
.setting-head .badge{padding:.02rem .45rem;font-size:.7rem}\
.setting-head .id{display:inline;margin-left:auto}.setting-head button.note-mark{margin:0 .1rem 0 0}\
form.settings:not(.js) button.note-mark{display:none}\
form.settings input:not([type=checkbox]):not([type=radio]):not([type=hidden]),form.settings select{padding:.4rem .6rem}\
form.settings .setting>select{display:block;width:100%}\
.secret-row{display:flex;align-items:center;gap:.75rem}.secret-row input{flex:1 1 auto;min-width:0}\
.secret-row label.check{margin:0;flex-shrink:0;white-space:nowrap}\
form.settings .setting>input{display:block;width:100%}\
form.settings .hint.about{margin:.2rem 0 0;font-size:.8rem;line-height:1.4}form.settings.js .hint.about{display:none}\
.note-bubble.hints{pointer-events:none}\
fieldset.choice{border:0;margin:0;padding:0;min-width:0}fieldset.choice legend{padding:0;width:100%}\
.options{display:flex;flex-wrap:wrap;gap:.4rem}\
label.option{display:inline-flex;align-items:center;gap:.45rem;margin:0;padding:.4rem .8rem;border:1px solid var(--line-strong);\
border-radius:var(--radius);font-size:.9rem;font-weight:400;cursor:pointer;background:var(--card)}\
label.option:hover{background:var(--hover)}\
label.option:has(input:checked){border-color:var(--accent);background:var(--accent-wash)}\
label.option input{margin:0;accent-color:var(--accent)}label.option .option-label{font-weight:550}\
.with-unit{display:flex;align-items:stretch;max-width:20rem}\
.with-unit input{margin:0!important;border-top-right-radius:0!important;border-bottom-right-radius:0!important;flex:1 1 auto;min-width:0}\
.with-unit .unit{display:flex;align-items:center;padding:0 .75rem;border:1px solid var(--line-strong);border-left:0;\
border-radius:0 var(--radius) var(--radius) 0;background:var(--accent-wash);color:var(--accent);font-weight:600;font-size:.86rem}\
label.check{display:flex;gap:.5rem;align-items:center;font-weight:400;font-size:.88rem;margin:.5rem 0 0}\
details.developer{margin:.7rem 0 0;padding-top:.5rem;border-top:1px solid var(--line-soft)}\
details.developer>summary{cursor:pointer;font-size:.86rem;font-weight:600;color:var(--ink-soft)}\
details.developer .summary-note{margin-left:.35rem;font-weight:400;color:var(--ink-faint)}\
details.developer>.fields{margin-top:.6rem}\
.form-foot{position:sticky;bottom:0;z-index:5;display:flex;justify-content:flex-end;margin-top:.7rem;padding:.5rem 0;\
border-top:1px solid var(--line-soft);background:var(--card)}\
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

/// The house: Home, from Settings and from a plugin's area, before its name.
pub const HOUSE: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\"><path d=\"M3.5 11.2 \
     12 4l8.5 7.2M5.8 9.4V20h12.4V9.4M10 20v-5.5h4V20\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.7\" \
     stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

/// A deployment admin's way between the two sides: a gear to Settings from
/// the dashboard, a house Home from Settings. Named for a screen
/// reader and a pointer alike, since it has no words.
fn side_button(in_admin: bool) -> String {
    let (href, name, icon) = if in_admin {
        ("/", "Home", HOUSE)
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

/// Before first paint: the person's mode on `<html>`, as the kit reads it;
/// and `data-script`, which says the page's script runs, so what the chrome's
/// script shows in a note ([`noted_badge`]) is not also a line of the page.
const HEAD_SCRIPT: &str = "(function(){var r=document.documentElement;\
r.setAttribute(\"data-script\",\"\");try{var m=document.cookie.match(\
/(?:^|;\\s*)(?:__Host-)?meridian_mode=(light|dark)(?:;|$)/);\
if(m)r.setAttribute(\"data-om-mode\",m[1]);}catch(e){}})();";

/// A badge (or a pill: `class` says which, and its tone) whose why is a note
/// on hover (the product owner, 2026-09-30: "let's put 'Its sidecar has
/// stopped reporting.' in note when hovering the silent button (similar
/// concept for other 'notes')"). With a note, the badge is a button, so a
/// keyboard and a tap reach it, described by the note, which the chrome's
/// script shows in the one bubble an account's note is shown in; the note is
/// returned second, for the caller to place where it reads as a line without
/// script, and with script it is not shown but in the bubble. `id` is the
/// note's, unique on the page. Without a note, the badge alone, as it was.
pub fn noted_badge(class: &str, word: &str, note: &str, id: &str) -> (String, String) {
    let class = escape(class);
    if note.trim().is_empty() {
        return (
            format!("<span class=\"{class}\">{}</span>", escape(word)),
            String::new(),
        );
    }
    let id = escape(id);
    (
        format!(
            "<button type=\"button\" class=\"{class}\" data-note aria-describedby=\"{id}\">{}</button>",
            escape(word)
        ),
        format!("<span class=\"hint noted\" id=\"{id}\">{}</span>", escape(note)),
    )
}

/// The header's menu and every plugin frame on the page: choosing a mode
/// applies it here, remembers it, and tells each framed page by the frame's
/// message (meridian-ui's contract), as each frame's load does, so a
/// navigation inside it keeps the person's theme. A seamless frame
/// (`data-seamless`, the plugin area's) is told version 3 with `framed: true`,
/// and is as tall as its page says it is, and its page's header actions and
/// status dot are drawn by the dashboard; the full-page frame is told
/// version 2, which says nothing of framing, so its page keeps its own. And
/// a note on hover for whatever on the page carries one (`data-note`).
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
  // Each load is a new page: its header actions and its status go until it
  // offers its own.
  frames.forEach(function (frame) {
    frame.addEventListener("load", function () { draw(frame, []); status(frame, null); tell(frame); });
  });
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
  // A seamless frame's header status, by meridian:status (kit 0.7.0;
  // meridian-ui's README, "The frame: seamless"): the page's own status dot,
  // drawn right after the plugin's name title in the area's heading, in the
  // place its frame names (data-status), so the page spends no line of its
  // own on it (the product owner, 2026-09-30). Taken under the size's guards and only in the
  // kit's shape, else not at all; state null takes the dot away, as a new
  // load of the frame does. Its label is its name and its note's first line,
  // its detail and moment its description; every word is text, never markup.
  var STATES = ["ok", "busy", "warn", "error"];
  var MOMENT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:\d{2})$/;
  function words(v, least, most) { return typeof v === "string" && v.trim().length >= least && v.length <= most; }
  function told(d) {
    if (d.state === null) return null;
    if (STATES.indexOf(d.state) === -1 || !words(d.label, 1, 80)) return undefined;
    if (d.detail !== undefined && !words(d.detail, 0, 300)) return undefined;
    if (d.at !== undefined && !(words(d.at, 1, 40) && MOMENT.test(d.at) && !isNaN(Date.parse(d.at)))) return undefined;
    if (d.at_label !== undefined && !words(d.at_label, 1, 40)) return undefined;
    return d;
  }
  function status(frame, said) {
    var place = frame.hasAttribute("data-status") && document.getElementById(frame.getAttribute("data-status"));
    if (!place) return;
    if (noted && place.contains(noted)) unnote();
    if (!said) { place.replaceChildren(); return; }
    var about = [];
    if (said.detail && said.detail.trim()) about.push(said.detail);
    if (said.at) {
      // As this dashboard shows every moment: to the minute, in UTC.
      var at = new Date(said.at).toISOString();
      about.push((said.at_label || "Updated") + " " + at.slice(0, 10) + " " + at.slice(11, 16) + " UTC");
    }
    var dot = document.createElement("button");
    dot.type = "button";
    dot.className = "status-dot";
    dot.setAttribute("data-state", said.state);
    dot.setAttribute("aria-label", said.label);
    dot.setAttribute("data-note", said.label);
    var lines = about.map(function (text, i) {
      var line = document.createElement("span");
      line.id = place.id + "-about-" + i;
      line.hidden = true;
      line.textContent = text;
      return line;
    });
    if (lines.length) dot.setAttribute("aria-describedby", lines.map(function (l) { return l.id; }).join(" "));
    place.replaceChildren.apply(place, [dot].concat(lines));
  }
  window.addEventListener("message", function (event) {
    var data = event.data;
    frames.forEach(function (frame) {
      if (!frame.hasAttribute("data-seamless") || !frame.contentWindow) return;
      if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;
      if (!data || data.type !== "meridian:status" || data.version !== 1) return;
      var said = told(data);
      if (said !== undefined) status(frame, said);
    });
  });
  // A note on hover (the product owner, 2026-09-30, "similar concept for
  // other 'notes'"): an element marked data-note -- a badge with a why, the
  // header's status dot -- shows its data-note's words, when it has some, and
  // what describes it (aria-describedby), in the one bubble an account's note
  // is shown in: under it, or over it when there is no room below. Pointing
  // at it, focusing it or pressing it shows the note; it stays while the
  // pointer is on it or on the bubble, and goes on leaving them, on Escape,
  // on a press elsewhere, and when the window resizes, a list scrolls or a
  // search is typed. The words reach the bubble as text, never as markup,
  // and the bubble is hidden from a screen reader, which has the note as the
  // element's description.
  var bubble = null;
  var noted = null;
  var leaving = 0;
  function unnote() {
    window.clearTimeout(leaving);
    noted = null;
    if (bubble) bubble.hidden = true;
  }
  function unnoteLater() {
    window.clearTimeout(leaving);
    leaving = window.setTimeout(unnote, 150);
  }
  function note(el) {
    window.clearTimeout(leaving);
    if (el === noted) return;
    var lines = [el.getAttribute("data-note")];
    (el.getAttribute("aria-describedby") || "").split(/\s+/).forEach(function (id) {
      var about = id && document.getElementById(id);
      if (about) lines.push(about.textContent);
    });
    var text = lines.map(function (line) { return (line || "").trim(); }).filter(Boolean).join("\n");
    if (!text) { unnote(); return; }
    if (!bubble) {
      bubble = document.createElement("div");
      bubble.className = "note-bubble";
      bubble.setAttribute("aria-hidden", "true");
      bubble.addEventListener("mouseenter", function () { window.clearTimeout(leaving); });
      bubble.addEventListener("mouseleave", function (event) {
        if (!noted || !noted.contains(event.relatedTarget)) unnoteLater();
      });
      document.body.appendChild(bubble);
    }
    noted = el;
    // In the header, which stays put as the page scrolls, the bubble stays
    // with it, over it.
    var onBar = !!el.closest("header.bar");
    bubble.classList.toggle("on-bar", onBar);
    bubble.textContent = text;
    bubble.hidden = false;
    var edge = 8;
    var gap = 4;
    var box = el.getBoundingClientRect();
    var width = bubble.offsetWidth;
    var height = bubble.offsetHeight;
    var top = box.bottom + gap;
    if (top + height > window.innerHeight - edge && box.top - height - gap >= edge) top = box.top - height - gap;
    var left = Math.max(edge, Math.min(box.left, window.innerWidth - width - edge));
    bubble.style.top = top + (onBar ? 0 : window.scrollY) + "px";
    bubble.style.left = left + (onBar ? 0 : window.scrollX) + "px";
  }
  function notedAt(target) { return target && target.closest ? target.closest("[data-note]") : null; }
  document.addEventListener("mouseover", function (event) {
    var el = notedAt(event.target);
    if (el) note(el);
  });
  document.addEventListener("mouseout", function (event) {
    if (!noted || notedAt(event.target) !== noted) return;
    if (noted.contains(event.relatedTarget) || (bubble && bubble.contains(event.relatedTarget))) return;
    unnoteLater();
  });
  document.addEventListener("focusin", function (event) {
    var el = notedAt(event.target);
    if (el) note(el);
  });
  document.addEventListener("focusout", function (event) {
    if (noted && notedAt(event.target) === noted) unnote();
  });
  document.addEventListener("click", function (event) {
    var el = notedAt(event.target);
    if (el) { note(el); return; }
    if (noted && !(bubble && bubble.contains(event.target))) unnote();
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && noted) unnote();
  });
  window.addEventListener("resize", function () { if (noted) unnote(); });
  document.addEventListener("scroll", function (event) { if (noted && event.target !== document) unnote(); }, true);
  document.addEventListener("input", function () { if (noted) unnote(); });
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
    // Home: a crumb as any other page's last one.
    let crumbs = format!(
        "<nav class=\"crumbs\" aria-label=\"Where you are\">{}</nav>",
        if chrome.crumbs.is_empty() {
            crumb_here("Home", None)
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
    fn a_plugins_settings_form_is_two_columns_on_a_wide_screen_one_on_a_phone_and_keeps_save_in_view(
    ) {
        for rule in [
            "form.settings .fields{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));",
            "form.settings .setting.wide{grid-column:1/-1}",
            "@media (max-width:40rem){form.settings .fields{grid-template-columns:minmax(0,1fr)}}",
            ".options{display:flex;flex-wrap:wrap;",
            // A hint is a small line without script, and the bubble's with it.
            "form.settings.js .hint.about{display:none}",
            "form.settings:not(.js) button.note-mark{display:none}",
            ".form-foot{position:sticky;bottom:0;",
        ] {
            assert!(STYLE.contains(rule), "{rule}");
        }
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
            .split(".plugin-frame{")
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
        assert!(STYLE.contains(".plugin-frame[data-sized]{min-height:0}"));
        // The person's mode, as the page's, so the transparent page sits on
        // the dashboard rather than on an opaque canvas.
        assert!(frame.contains("color-scheme:light dark"));
        assert!(STYLE.contains("html[data-om-mode=light] .plugin-frame{color-scheme:light}"));
        assert!(STYLE.contains("html[data-om-mode=dark] .plugin-frame{color-scheme:dark}"));
    }

    /// The area owns the gap between its tab row and the framed page (the
    /// product owner, 2026-09-30: "can we have consistent spacing"): the kit
    /// drops a framed page's own padding, so a page whose first element is a
    /// card sits as far below the tabs as one opening with a paragraph, and
    /// as the dashboard's other tab rows sit above what they show.
    #[test]
    fn the_areas_page_sits_one_space_below_its_tab_row_as_under_every_tab_row() {
        assert!(STYLE.contains(".plugin-area nav.tabs{margin-bottom:var(--space-5)}"));
        // --space-5 is the kit's 20px, the 1.25rem every other tab row keeps.
        assert!(STYLE
            .contains("nav.tabs{display:flex;flex-wrap:wrap;gap:.25rem;margin:1rem 0 1.25rem;"));
        assert!(!STYLE.contains(".plugin-area nav.tabs{margin-bottom:0}"));
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
            !head.contains("aria-label=\"Home\""),
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
                "<a class=\"bar-link side\" href=\"/\" aria-label=\"Home\" title=\"Home\">{HOUSE}</a>"
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
        // Home, as any page's last crumb.
        let home = crumbs(&Chrome::default());
        assert_eq!(
            home,
            "<span class=\"here\" aria-current=\"page\">Home</span>"
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
            "frame.addEventListener(\"load\", function () { draw(frame, []); status(frame, null); tell(frame); });"
        ));
        // The tones the header can draw.
        assert!(STYLE.contains("button.danger{background:var(--danger-wash);border-color:var(--danger);color:var(--danger)}"));
    }

    /// The header-status listener, as it is written (kit 0.7.0's
    /// `meridian:status`; the product owner, 2026-09-30: "put the green icon
    /// ... next to the plugin name"): the size's guards, its own type at
    /// version 1, then the kit's shape, before anything is drawn; drawn
    /// right after the plugin's name title as text (the product owner,
    /// 2026-09-30: "green check circle should be next to plugin name title of
    /// the form"); gone with state null or a new load.
    #[test]
    fn a_seamless_frames_status_is_taken_only_from_its_own_page_and_drawn_beside_the_name() {
        let listener = CHROME_SCRIPT
            .split("window.addEventListener(\"message\"")
            .nth(3)
            .expect("the status listener")
            .split("\n  });\n")
            .next()
            .unwrap();
        let guards = [
            // Only a seamless frame, from its own window,
            "if (!frame.hasAttribute(\"data-seamless\") || !frame.contentWindow) return;",
            // from exactly the plugin's origin (another origin is ignored),
            "if (event.source !== frame.contentWindow || event.origin !== frame.dataset.origin) return;",
            // as the status, at version 1 (another version is ignored),
            "if (!data || data.type !== \"meridian:status\" || data.version !== 1) return;",
            // in the kit's shape,
            "var said = told(data);",
            // or nothing is drawn, and the dot drawn before stays.
            "if (said !== undefined) status(frame, said);",
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

        // The shape, as the kit's README gives the host's half: state null,
        // or a state it knows with a short label; a detail, a moment that
        // parses and its label, each short, or not there at all.
        let shape = CHROME_SCRIPT
            .split("function told(d) {")
            .nth(1)
            .and_then(|rest| rest.split("\n  }\n").next())
            .expect("the shape");
        for check in [
            "if (d.state === null) return null;",
            "if (STATES.indexOf(d.state) === -1 || !words(d.label, 1, 80)) return undefined;",
            "if (d.detail !== undefined && !words(d.detail, 0, 300)) return undefined;",
            "if (d.at !== undefined && !(words(d.at, 1, 40) && MOMENT.test(d.at) && !isNaN(Date.parse(d.at)))) return undefined;",
            "if (d.at_label !== undefined && !words(d.at_label, 1, 40)) return undefined;",
        ] {
            assert!(shape.contains(check), "{check}\nnot in:{shape}");
        }
        assert!(CHROME_SCRIPT.contains("var STATES = [\"ok\", \"busy\", \"warn\", \"error\"];"));
        assert!(CHROME_SCRIPT.contains(
            "function words(v, least, most) { return typeof v === \"string\" && v.trim().length >= least && v.length <= most; }"
        ));

        // Drawn in the place the frame names, after the plugin's name: a dot
        // whose state is its mark, its label its name and its note's first
        // line, its detail and moment what describes it; all of it as text.
        let draw = CHROME_SCRIPT
            .split("function status(frame, said) {")
            .nth(1)
            .and_then(|rest| rest.split("\n  }\n").next())
            .expect("the drawing");
        for line in [
            "var place = frame.hasAttribute(\"data-status\") && document.getElementById(frame.getAttribute(\"data-status\"));",
            "if (!said) { place.replaceChildren(); return; }",
            "dot.className = \"status-dot\";",
            "dot.setAttribute(\"data-state\", said.state);",
            "dot.setAttribute(\"aria-label\", said.label);",
            "dot.setAttribute(\"data-note\", said.label);",
            "line.textContent = text;",
            "place.replaceChildren.apply(place, [dot].concat(lines));",
        ] {
            assert!(draw.contains(line), "{line}\nnot in:{draw}");
        }
        assert!(!CHROME_SCRIPT.contains("innerHTML"), "never markup");
        assert!(!CHROME_SCRIPT.contains("\"*\""), "never to any origin");
        // A mark for each state, never colour alone.
        for rule in [
            ".status-dot[data-state=ok]::before{background:var(--good);content:\"\\2713\"",
            ".status-dot[data-state=error]::before{background:var(--danger);content:\"!\"",
            ".status-dot[data-state=warn]::before{background:var(--warn-ink);content:\"!\"",
            ".status-dot[data-state=busy]::before{background:transparent;border:2px solid var(--warn-ink)",
            "@media (prefers-reduced-motion:reduce){.status-dot[data-state=busy]::before{animation:none}}",
            // Beside the name title, centred on it, and taking no room
            // until there is a status.
            ".plugin-area .area-title{display:flex;align-items:center;",
            ".plugin-area .title-status{display:inline-flex;align-items:center;flex-shrink:0}",
            ".plugin-area .title-status:empty{display:none}",
            // A note on the header stays with it as the page scrolls.
            ".note-bubble.on-bar{position:fixed;z-index:31}",
        ] {
            assert!(STYLE.contains(rule), "{rule}");
        }
    }

    #[test]
    fn a_badge_with_a_why_is_a_button_described_by_its_note_and_one_without_is_a_badge() {
        let (badge, note) = noted_badge(
            "badge warn",
            "Silent",
            "Its sidecar has <stopped> reporting.",
            "n-1",
        );
        assert_eq!(
            badge,
            "<button type=\"button\" class=\"badge warn\" data-note aria-describedby=\"n-1\">Silent</button>"
        );
        assert_eq!(
            note,
            "<span class=\"hint noted\" id=\"n-1\">Its sidecar has &lt;stopped&gt; reporting.</span>"
        );
        let (badge, note) = noted_badge("badge good", "Healthy", " ", "n-2");
        assert_eq!(badge, "<span class=\"badge good\">Healthy</span>");
        assert!(note.is_empty());

        // Script says it runs before first paint, and the note is then the
        // bubble's alone; without script it is a line under the badge.
        assert!(HEAD_SCRIPT.starts_with(
            "(function(){var r=document.documentElement;r.setAttribute(\"data-script\",\"\");try{"
        ));
        assert!(STYLE.contains("html[data-script] .noted{display:none}"));
        assert!(STYLE.contains("button.badge,button.pill{margin:0;border:0;"));

        // The one bubble, the accounts' note's: filled as text, hidden from a
        // screen reader, which has the note as the badge's description; on
        // pointing, focus or a press, and gone on Escape.
        let script = CHROME_SCRIPT
            .split("function note(el) {")
            .nth(1)
            .and_then(|rest| rest.split("\n  }\n").next())
            .expect("the note");
        for line in [
            "var lines = [el.getAttribute(\"data-note\")];",
            "(el.getAttribute(\"aria-describedby\") || \"\").split(/\\s+/).forEach(function (id) {",
            "bubble.className = \"note-bubble\";",
            "bubble.setAttribute(\"aria-hidden\", \"true\");",
            "bubble.textContent = text;",
        ] {
            assert!(script.contains(line), "{line}\nnot in:{script}");
        }
        for listened in [
            "document.addEventListener(\"mouseover\", function (event) {",
            "document.addEventListener(\"focusin\", function (event) {",
            "document.addEventListener(\"click\", function (event) {\n    var el = notedAt(event.target);",
            "if (event.key === \"Escape\" && noted) unnote();",
        ] {
            assert!(CHROME_SCRIPT.contains(listened), "{listened}");
        }
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
