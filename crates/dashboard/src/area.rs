//! A plugin's area, `/plugins/{instance}` (W6.9, spec/plugin-pages-share-one-kit,
//! Q3 and Q5; sdk-contract/a-plugin-has-admins).
//!
//! Reached from the home by a button per level the person holds on the
//! plugin: **Manage** at `admin`, **Open** at `write`, **View** at `read`. The
//! session carries the level chosen, and the tab row shows the pages the
//! plugin declared whose levels include it, in the order declared (W4.8): its
//! pages at `admin` under Manage -- configuration, connections, account links,
//! account agnostic -- and its pages at `write` and `read` under Open and
//! View, one URL for both when a page serves both, adapting by the level in
//! the claims. A plugin declaring no page at `write` or `read` has one, its
//! `/`.
//!
//! One template for every level: the dashboard's heading -- a house Home
//! before the plugin's name (the product owner, 2026-09-30: "in front of the
//! plugin name title, add the house icon as a link to go back to the
//! homepage") -- and one tab row, and under them the page, framed
//! **seamlessly** -- no border of its own, the viewport's height under the
//! dashboard's chrome whatever its page says (the product owner, 2026-10-04:
//! every page fits one screen; the page's own viewport is its height budget,
//! and a page taller than it scrolls inside the frame), with `om-framed=1` on
//! its address so the kit draws no heading or tab row of its own. The page's header actions
//! (`meridian:actions`) are drawn in the area's head, as the admin view drew
//! them for its pages before they moved here (meridian-core 1b2a9ad), and its
//! status dot (`meridian:status`, kit 0.7.0) right after the plugin's name
//! title, centred on it (the product owner, 2026-09-30: "green check circle
//! should be next to plugin name title of the form").
//!
//! The head is the same on every tab and at every level (the product owner,
//! 2026-10-01): the house, the plugin's name and its status dot on the left;
//! on the right the page's actions -- Refresh as kit 0.8.0's circular-arrow
//! icon, any other as its words -- immediately left of the Manage, Open and
//! View switch, which is always the rightmost ("maybe the circular arrow to
//! the left of the toggle"), so actions grow leftward and never move it. On
//! a phone the head stays one row ("the phone width looks weird"; "maybe the
//! toggle becomes a dropdown so they can be shown in the same row?"): the
//! switch is a menu naming the level, and a long name is cut with an
//! ellipsis, whole in its title. The plugin's instance is the line under the
//! name at every width. Under Manage every tab has the dot ("yes,
//! dot on every tab"): on the dashboard's own tabs, and on a page that tells
//! none, the plugin's health as the dashboard knows it ([`crate::health`]);
//! on a page that tells its own, the page's. The frame stays, so the plugin's script is kept
//! from the person's dashboard session (decisions/021). No way to the
//! plugin's tabs in the admin portal is drawn here (the product owner,
//! 2026-09-30: "Remove 'its settings and access' link"); a deployment admin
//! reaches them from Settings, whose Plugins tab lists every instance.
//!
//! Under Manage the dashboard draws two tabs itself, before the plugin's:
//! **Summary**, where Manage opens, and **Settings** (the product owner,
//! 2026-10-01).
//!
//! A tab is a link, `?level=` the session's and `&tab=` the page's title in
//! lower case, so each opens directly and none needs script.

use meridian_access::{button, level_name, AccessLevel, Held};
use meridian_domain::v1::PluginReport;

use crate::health::State;
use crate::html::{escape, HOUSE};

/// One of the plugin's pages, as a tab of its area: what the query names it
/// by, what it is called, and the path it frames; or, `drawn`, a tab the
/// dashboard draws itself, which frames nothing and has no path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub path: String,
    pub drawn: bool,
}

/// The tab the dashboard draws first under Manage, and where Manage opens:
/// the plugin's status, and the figures it reports as tiles (the
/// product owner, 2026-10-01: "think Status, Connections, Account Reached,
/// and Last Read can be their own Summary page", core drawing it).
pub const SUMMARY: &str = "summary";

/// The tab the dashboard draws second under Manage: the plugin's settings
/// form, the admin portal's, drawn here (the product owner, 2026-10-01:
/// "build Settings and the status panel under Manage").
pub const SETTINGS: &str = "settings";

/// Where the dashboard's Settings tab posts the form, coming back to the
/// tab: the admin portal's own address comes back to the portal.
pub fn settings_path(instance: &str) -> String {
    format!("/plugins/{instance}/settings")
}

impl Tab {
    fn page(key: String, title: &str, path: &str) -> Tab {
        Tab {
            key,
            title: title.into(),
            path: path.into(),
            drawn: false,
        }
    }
}

/// The pages whose levels include `level`, as the plugin's report declares
/// them, in its order. A page whose path is not one on the plugin's host is
/// left out, as is a second tab for a path already shown. None at `write` or
/// `read`, and the plugin's `/` is the one (W4.8). None at `admin` from a
/// plugin built before v5 that declared no admin pages either, and its
/// `/admin` is the one, as the admin view framed it then, so a plugin built
/// before keeps its admin page; from a plugin built for v5, there is none.
///
/// At `admin`, before them all, the dashboard's own Summary and Settings tabs
/// ([`SUMMARY`], [`SETTINGS`]), and after Settings a tab of the dashboard's
/// for each of the plugin's table settings, `tables` as their keys and
/// labels (the product owner, 2026-10-05: "two tabs - plan code links vs
/// cash links"): a page of the plugin's titled "Summary" or "Settings" is
/// then `summary-2` or `settings-2` in the query.
pub fn tabs(
    report: Option<&PluginReport>,
    level: AccessLevel,
    tables: &[(String, String)],
) -> Vec<Tab> {
    tabs_by_role(report, level, &[], tables)
}

/// [`tabs`], by role (W6.9, contract v15): a page naming roles shows when,
/// for at least one of them, the person's level within the button --
/// `roles`, the session's per-role entries -- is one of the page's levels; a
/// page naming none, or a session carrying none, by the button's level as
/// before. Tabs are not grouped or labelled by role: how roles appear inside
/// the plugin is the vendor's.
pub fn tabs_by_role(
    report: Option<&PluginReport>,
    level: AccessLevel,
    roles: &[(String, AccessLevel)],
    tables: &[(String, String)],
) -> Vec<Tab> {
    let interface = report.and_then(|report| report.declared_interface.as_ref());
    let declared = interface
        .map(|interface| interface.pages.as_slice())
        .unwrap_or_default();
    let mut tabs: Vec<Tab> = Vec::new();
    if level == AccessLevel::Admin {
        let own = [(SUMMARY, "Summary"), (SETTINGS, "Settings")];
        let own = own.iter().map(|(key, title)| (*key, *title));
        let tables = tables
            .iter()
            .map(|(key, title)| (key.as_str(), title.as_str()));
        for (key, title) in own.chain(tables) {
            let key = unique(key.into(), &tabs);
            tabs.push(Tab {
                key,
                title: title.into(),
                path: String::new(),
                drawn: true,
            });
        }
    }
    let serves = |page: &&meridian_pb::v1::PageDeclaration| {
        if page.roles.is_empty() || roles.is_empty() {
            return page.levels.contains(&(level as i32));
        }
        roles
            .iter()
            .any(|(role, at)| page.roles.contains(role) && page.levels.contains(&(*at as i32)))
    };
    for page in declared.iter().filter(serves) {
        let path = page.path.trim();
        if crate::plugins::page_path(path).is_err() || tabs.iter().any(|t| t.path == path) {
            continue;
        }
        let title = page.title.trim();
        let title = if title.is_empty() { path } else { title };
        let key = unique(slug(title), &tabs);
        tabs.push(Tab::page(key, title, path));
    }
    let framed = |tabs: &[Tab]| tabs.iter().any(|tab| !tab.drawn);
    let before_pages = report
        .and_then(|report| report.contract_version.strip_prefix('v'))
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version < 5);
    if !framed(&tabs) && level == AccessLevel::Admin && before_pages {
        let key = unique("admin".into(), &tabs);
        tabs.push(Tab::page(key, "Admin page", "/admin"));
    }
    if !framed(&tabs) && level != AccessLevel::Admin {
        let title = interface
            .map(|interface| interface.title.trim())
            .filter(|title| !title.is_empty())
            .unwrap_or("Home");
        tabs.push(Tab::page(slug(title), title, "/"));
    }
    tabs
}

/// A title as the query names its tab: lower case, letters and digits, runs of
/// anything else one hyphen. "Account links" is `account-links`; a title with
/// none of either is `page`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "page".into()
    } else {
        out.into()
    }
}

/// `key`, or `key-2`, `key-3` and on, whichever no tab already has.
fn unique(key: String, tabs: &[Tab]) -> String {
    let taken = |k: &str| tabs.iter().any(|t| t.key == k);
    if !taken(&key) {
        return key;
    }
    (2..)
        .map(|n| format!("{key}-{n}"))
        .find(|k| !taken(k))
        .expect("an unused key")
}

/// Where the area is, at a level and on a tab.
pub fn href(instance: &str, level: AccessLevel, tab: Option<&str>) -> String {
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("a fixed address");
    url.set_path(&format!("/plugins/{instance}"));
    url.query_pairs_mut()
        .append_pair("level", level_name(level));
    if let Some(tab) = tab {
        url.query_pairs_mut().append_pair("tab", tab);
    }
    format!("{}?{}", url.path(), url.query().unwrap_or_default())
}

/// The header's area for a framed page's own actions (meridian-ui's
/// `meridian:actions`), which the chrome's script draws; its frame names it.
/// It sits immediately left of the level switch, in one right-hand group.
pub const PAGE_ACTIONS: &str = "page-actions";

/// Where the status dot goes: right after the plugin's name title. A framed
/// page's own (meridian-ui's `meridian:status`, kit 0.7.0) is drawn there by
/// the chrome's script, whose frame names it; until it tells one, or where it
/// tells none, the place holds what the dashboard drew: under Manage the
/// plugin's health ([`Area::health`]), and otherwise nothing, taking no room.
pub const PAGE_STATUS: &str = "page-status";

/// The page as this dashboard can show it.
pub enum Shown {
    /// Framed seamlessly: the frame's way in, and the plugin's origin its
    /// theme goes to.
    Framed { src: String, origin: String },
    /// A tab the dashboard draws itself ([`SUMMARY`], [`SETTINGS`]), as HTML.
    Drawn(String),
}

/// What the area shows.
pub struct Area<'a> {
    pub instance: &'a str,
    pub name: &'a str,
    /// What the person holds on the plugin: the buttons they may switch
    /// between.
    pub held: &'a Held,
    pub level: AccessLevel,
    pub tabs: &'a [Tab],
    pub current: Option<&'a Tab>,
    pub shown: Shown,
    /// The plugin's health as the dashboard knows it, drawn as the status dot
    /// where no page tells its own: given under Manage, so every tab there
    /// has a dot; `None` at Open and View, where the dot is the page's alone.
    pub health: Option<&'a State>,
}

/// The plugin's health as a status dot, drawn as the chrome's script draws a
/// page's (the same button, its mark the state, its word its name and its
/// note's first line, its why what describes it), so the two look and read
/// alike. Every word is escaped.
fn health_dot(state: &State) -> String {
    let word = escape(state.word);
    let detail = state.detail.trim();
    let (described, about) = if detail.is_empty() {
        (String::new(), String::new())
    } else {
        (
            format!(" aria-describedby=\"{PAGE_STATUS}-about-0\""),
            format!(
                "<span id=\"{PAGE_STATUS}-about-0\" hidden>{}</span>",
                escape(detail)
            ),
        )
    };
    format!(
        "<button type=\"button\" class=\"status-dot\" data-state=\"{state}\" aria-label=\"{word}\" \
         data-note=\"{word}\"{described}></button>{about}",
        state = state.dot,
    )
}

/// The buttons for the levels held, the session's pressed: a person holding
/// `admin` and `write` moves between Manage, Open and View here as on the
/// home. Drawn twice, the stylesheet showing one: side by side, and on a
/// phone a menu naming the session's level, so the head stays one row (the
/// product owner, 2026-10-01: "maybe the toggle becomes a dropdown so they
/// can be shown in the same row?"). The menu is a disclosure, needing no
/// script: its summary a button a key opens, its levels the same links,
/// which close it by going there.
fn levels(area: &Area) -> String {
    let held = area.held.levels();
    if held.len() < 2 {
        return format!(
            "<span class=\"badge accent\" data-level=\"{}\">{}</span>",
            level_name(area.level),
            button(area.level)
        );
    }
    let links: String = held
        .iter()
        .map(|level| {
            format!(
                "<a href=\"{href}\" data-level=\"{name}\"{pressed}>{said}</a>",
                href = escape(&href(area.instance, *level, None)),
                name = level_name(*level),
                pressed = if *level == area.level {
                    " class=\"here\" aria-current=\"page\""
                } else {
                    ""
                },
                said = button(*level),
            )
        })
        .collect();
    let said = button(area.level);
    format!(
        "<nav class=\"level-switch\" aria-label=\"Open it as\">{links}</nav>\
         <details class=\"menu level-menu\"><summary aria-label=\"Open it as: {said}\">{said}</summary>\
         <nav class=\"menu-pop\" aria-label=\"Open it as\">{links}</nav></details>"
    )
}

fn nav(area: &Area) -> String {
    let links: String = area
        .tabs
        .iter()
        .map(|tab| {
            let here = area.current.is_some_and(|current| current.key == tab.key);
            // A page is named by the path it frames; the dashboard's own tab
            // by what it is.
            let page = if tab.drawn {
                " data-drawn".to_string()
            } else {
                format!(" data-page=\"{}\"", escape(&tab.path))
            };
            format!(
                "<a href=\"{href}\" data-tab=\"{key}\"{page}{here}>{title}</a>",
                href = escape(&href(area.instance, area.level, Some(&tab.key))),
                key = escape(&tab.key),
                here = if here {
                    " class=\"here\" aria-current=\"page\""
                } else {
                    ""
                },
                title = escape(&tab.title),
            )
        })
        .collect();
    format!("<nav class=\"tabs view-tabs\" aria-label=\"The plugin's pages\">{links}</nav>")
}

/// "Report a problem" in the area's head (W6.21), filling in this plugin:
/// the instance, and the version the deployment knows it runs, set at
/// filing. Leftmost of the right-hand group, so the switch stays last; on a
/// phone its flag alone.
pub fn report_problem(instance: &str) -> String {
    format!(
        "<a class=\"button report-problem\" href=\"/tickets/new?concerns=plugin&amp;instance={i}\" \
         aria-label=\"Report a problem\" title=\"Report a problem\">{flag}<span class=\"bar-label\">Report a problem</span></a>",
        i = escape(instance),
        flag = crate::html::FLAG,
    )
}

pub fn render(area: &Area) -> String {
    let framed = matches!(area.shown, Shown::Framed { .. });
    // The page's actions, left of the switch: a framed page's, which it tells.
    let actions = if framed {
        format!(
            "<div class=\"actions\" id=\"{PAGE_ACTIONS}\" role=\"group\" aria-label=\"{} actions\"></div>",
            escape(area.current.map(|tab| tab.title.as_str()).unwrap_or_default())
        )
    } else {
        String::new()
    };
    let status = format!(
        "<span class=\"title-status\" id=\"{PAGE_STATUS}\">{}</span>",
        area.health.map(health_dot).unwrap_or_default()
    );
    let body = match &area.shown {
        Shown::Framed { src, origin } => {
            let tab = area.current.expect("a framed page is a tab's");
            format!(
                "<iframe class=\"plugin-frame\" id=\"plugin-page\" data-page=\"{path}\" src=\"{src}\" \
                 title=\"{name} &middot; {title}\" data-plugin-frame data-seamless data-origin=\"{origin}\" \
                 data-actions=\"{PAGE_ACTIONS}\" data-status=\"{PAGE_STATUS}\"></iframe>",
                path = escape(&tab.path),
                src = escape(src),
                name = escape(area.name),
                title = escape(&tab.title),
                origin = escape(origin),
            )
        }
        Shown::Drawn(html) => format!("<div class=\"area-drawn\" id=\"plugin-page\">{html}</div>"),
    };
    let nav = if area.tabs.is_empty() {
        String::new()
    } else {
        nav(area)
    };
    format!(
        "<div class=\"plugin-area\" data-level=\"{level}\"><div class=\"page-head\">\
         <div class=\"area-title\"><a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\">{HOUSE}</a>\
         <h1 title=\"{name}\">{name}</h1>{status}</div><p class=\"area-id\"><code>{instance}</code></p>\
         <div class=\"head-side\">{report}{actions}{levels}</div></div>\
         {nav}<div class=\"tab-body\" data-current=\"{current}\">{body}</div></div>",
        level = level_name(area.level),
        name = escape(area.name),
        instance = escape(area.instance),
        levels = levels(area),
        report = report_problem(area.instance),
        current = escape(area.current.map(|tab| tab.key.as_str()).unwrap_or_default()),
    )
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::{InterfaceDeclaration, PageDeclaration};

    use super::*;

    fn report(pages: &[(&str, &str, &[AccessLevel])]) -> PluginReport {
        PluginReport {
            declared_interface: Some(InterfaceDeclaration {
                loopback_port: 8000,
                title: "SnapTrade".into(),
                pages: pages
                    .iter()
                    .map(|(path, title, levels)| PageDeclaration {
                        path: path.to_string(),
                        title: title.to_string(),
                        levels: levels.iter().map(|l| *l as i32).collect(),
                        roles: vec![],
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    const ADMIN: &[AccessLevel] = &[AccessLevel::Admin];
    const DATA: &[AccessLevel] = &[AccessLevel::Write, AccessLevel::Read];

    #[test]
    fn a_page_shows_by_the_persons_level_on_one_of_its_roles() {
        // A plugin holding custody and operations (contract v15): a page
        // naming a role shows when the person's level on it within the
        // button is one of the page's levels.
        let page = |path: &str, title: &str, levels: &[AccessLevel], roles: &[&str]| PageDeclaration {
            path: path.into(),
            title: title.into(),
            levels: levels.iter().map(|l| *l as i32).collect(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        };
        let report = PluginReport {
            declared_interface: Some(InterfaceDeclaration {
                loopback_port: 8000,
                title: "Ops".into(),
                pages: vec![
                    page("/balances", "Balances", &[AccessLevel::Write, AccessLevel::Read], &["operations"]),
                    page("/statements", "Statements", &[AccessLevel::Write], &["custody"]),
                    page("/holdings", "Holdings", &[AccessLevel::Read], &["custody"]),
                    page("/links", "Account links", &[AccessLevel::Admin], &["custody"]),
                ],
            }),
            ..Default::default()
        };
        let titles = |tabs: Vec<Tab>| tabs.into_iter().map(|t| t.title).collect::<Vec<_>>();
        let open = tabs_by_role(
            Some(&report),
            AccessLevel::Write,
            &[("custody".into(), AccessLevel::Read), ("operations".into(), AccessLevel::Write)],
            &[],
        );
        assert_eq!(titles(open), ["Balances", "Holdings"]);
        let manage = tabs_by_role(
            Some(&report),
            AccessLevel::Admin,
            &[("operations".into(), AccessLevel::Admin)],
            &[],
        );
        assert_eq!(titles(manage), ["Summary", "Settings"]);
        let custody_admin = tabs_by_role(
            Some(&report),
            AccessLevel::Admin,
            &[("custody".into(), AccessLevel::Admin)],
            &[],
        );
        assert_eq!(titles(custody_admin), ["Summary", "Settings", "Account links"]);
    }

    #[test]
    fn each_button_shows_the_pages_whose_levels_include_its_level_in_order() {
        let snaptrade = report(&[
            ("/admin/connections", "Connections", ADMIN),
            ("/statements", "Statements", DATA),
            ("/admin/accounts", "Account links", ADMIN),
        ]);
        let titles = |level| -> Vec<String> {
            tabs(Some(&snaptrade), level, &[])
                .into_iter()
                .map(|tab| format!("{} {}", tab.key, tab.path))
                .collect()
        };
        // Under Manage, the dashboard's Summary and Settings first, then the
        // plugin's own pages at admin (the product owner, 2026-10-01).
        assert_eq!(
            titles(AccessLevel::Admin),
            [
                "summary ",
                "settings ",
                "connections /admin/connections",
                "account-links /admin/accounts"
            ]
        );
        let manage = tabs(Some(&snaptrade), AccessLevel::Admin, &[]);
        assert!(manage[0].drawn && manage[0].title == "Summary");
        assert!(manage[1].drawn && manage[1].title == "Settings");
        assert!(manage[2..].iter().all(|tab| !tab.drawn));
        assert!(
            [AccessLevel::Write, AccessLevel::Read]
                .into_iter()
                .all(|level| tabs(Some(&snaptrade), level, &[])
                    .iter()
                    .all(|tab| !tab.drawn)),
            "no Summary or Settings under Open or View"
        );
        assert_eq!(titles(AccessLevel::Write), ["statements /statements"]);
        assert_eq!(
            titles(AccessLevel::Read),
            ["statements /statements"],
            "one URL for Open and View"
        );
    }

    #[test]
    fn a_plugin_with_no_page_at_write_or_read_has_its_root_and_none_at_admin_the_dashboards_tabs_alone(
    ) {
        let only_admin = report(&[("/admin", "Admin", ADMIN)]);
        let open = tabs(Some(&only_admin), AccessLevel::Write, &[]);
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].path.as_str(), open[0].title.as_str()),
            ("/", "SnapTrade")
        );
        let only_data = report(&[("/", "Home", DATA)]);
        let keys: Vec<(String, bool)> = tabs(Some(&only_data), AccessLevel::Admin, &[])
            .into_iter()
            .map(|tab| (tab.key, tab.drawn))
            .collect();
        assert_eq!(
            keys,
            [(SUMMARY.to_string(), true), (SETTINGS.to_string(), true)]
        );
        assert_eq!(tabs(None, AccessLevel::Read, &[])[0].path, "/");
    }

    #[test]
    fn a_plugin_built_before_v5_declaring_no_admin_page_keeps_its_admin() {
        let older = PluginReport {
            contract_version: "v4".into(),
            ..report(&[])
        };
        let manage = tabs(Some(&older), AccessLevel::Admin, &[]);
        assert_eq!(manage.len(), 3);
        assert!(manage[0].drawn && manage[1].drawn);
        assert_eq!(
            (manage[2].path.as_str(), manage[2].title.as_str()),
            ("/admin", "Admin page")
        );
        let newer = PluginReport {
            contract_version: "v5".into(),
            ..report(&[])
        };
        let manage = tabs(Some(&newer), AccessLevel::Admin, &[]);
        assert!(
            manage.len() == 2 && manage.iter().all(|tab| tab.drawn),
            "the dashboard's tabs alone"
        );
    }

    #[test]
    fn a_path_not_on_the_plugins_host_or_shown_already_is_left_out() {
        let odd = report(&[
            ("//elsewhere.example", "Away", ADMIN),
            ("/.meridian/enter", "Ours", ADMIN),
            ("/a", "Settings", ADMIN),
            ("/a", "Again", ADMIN),
            ("/b", "Settings", ADMIN),
            ("/c", "Summary", ADMIN),
        ]);
        let keys: Vec<String> = tabs(Some(&odd), AccessLevel::Admin, &[])
            .into_iter()
            .map(|tab| tab.key)
            .collect();
        // The dashboard's Summary and Settings keep their names; the
        // plugin's are numbered.
        assert_eq!(
            keys,
            [
                "summary",
                "settings",
                "settings-2",
                "settings-3",
                "summary-2"
            ]
        );
    }

    #[test]
    fn the_area_frames_the_page_seamlessly_under_one_tab_row() {
        let tab = Tab {
            key: "statements".into(),
            title: "Statements".into(),
            path: "/statements".into(),
            drawn: false,
        };
        let held = Held {
            admin: true,
            data: Some(AccessLevel::Write),
            ..Default::default()
        };
        let page = render(&Area {
            instance: "snaptrade-1",
            name: "SnapTrade",
            held: &held,
            level: AccessLevel::Write,
            tabs: std::slice::from_ref(&tab),
            current: Some(&tab),
            shown: Shown::Framed {
                src: "/plugins/snaptrade-1/enter?path=%2Fstatements&level=write".into(),
                origin: "https://snaptrade-1.plugins.meridian.example".into(),
            },
            health: None,
        });
        assert!(page.contains("data-seamless"));
        assert!(page.contains(&format!("data-actions=\"{PAGE_ACTIONS}\"")));
        assert!(page.contains(&format!("data-status=\"{PAGE_STATUS}\"")));
        assert_eq!(page.matches("<nav class=\"tabs").count(), 1, "one tab row");
        for (level, said) in [("admin", "Manage"), ("write", "Open"), ("read", "View")] {
            assert!(
                page.contains(&format!("data-level=\"{level}\">{said}<"))
                    || page.contains(&format!(
                        "data-level=\"{level}\" class=\"here\" aria-current=\"page\">{said}<"
                    )),
                "{said} offered: {page}"
            );
        }
    }

    /// The heading under Manage, as the product owner asked for it on
    /// 2026-09-30: the house before the plugin's name, a link Home named for
    /// a screen reader and a pointer alike, since it has no words; the name
    /// itself no link; and no way to the plugin's tabs in the admin portal,
    /// for its admin or anybody.
    #[test]
    fn the_heading_is_a_house_home_then_the_name_and_no_way_to_the_portal() {
        let tab = Tab {
            key: "connections".into(),
            title: "Connections".into(),
            path: "/admin/connections".into(),
            drawn: false,
        };
        let held = Held {
            admin: true,
            ..Default::default()
        };
        let page = render(&Area {
            instance: "snaptrade",
            name: "Snap<Trade>",
            held: &held,
            level: AccessLevel::Admin,
            tabs: std::slice::from_ref(&tab),
            current: Some(&tab),
            shown: Shown::Framed {
                src: "/plugins/snaptrade/enter?path=%2Fadmin%2Fconnections&level=admin".into(),
                origin: "https://snaptrade.plugins.meridian.example".into(),
            },
            health: None,
        });
        let head = page.split("<nav class=\"tabs").next().expect("the heading");
        assert_eq!(
            head,
            format!(
                "<div class=\"plugin-area\" data-level=\"admin\"><div class=\"page-head\">\
                 <div class=\"area-title\"><a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\">\
                 {HOUSE}</a><h1 title=\"Snap&lt;Trade&gt;\">Snap&lt;Trade&gt;</h1><span class=\"title-status\" id=\"{PAGE_STATUS}\"></span>\
                 </div><p class=\"area-id\"><code>snaptrade</code></p>\
                 <div class=\"head-side\">{report}\
                 <div class=\"actions\" id=\"{PAGE_ACTIONS}\" role=\"group\" aria-label=\"Connections actions\"></div>\
                 <span class=\"badge accent\" data-level=\"admin\">Manage</span></div></div>",
                report = report_problem("snaptrade")
            )
        );
        for portal in ["/admin/plugins/", "data-portal", "settings and access"] {
            assert!(!page.contains(portal), "{portal} in {page}");
        }
        // The house is the header's own, drawn once; the other icon is
        // "Report a problem"'s flag.
        assert_eq!(page.matches("<svg").count(), 2);
        assert_eq!(page.matches(HOUSE).count(), 1);
    }

    /// Each of the plugin's table settings is a tab of the dashboard's own
    /// under Manage, after Settings and before the plugin's pages, titled
    /// with its label (the product owner, 2026-10-05: "two tabs - plan code
    /// links vs cash links"); none under Open or View.
    #[test]
    fn each_table_setting_is_a_drawn_tab_after_settings_under_manage_alone() {
        let report = report(&[
            ("/admin/accounts", "Account links", ADMIN),
            ("/", "Home", DATA),
        ]);
        let tables = [
            (
                "setting-plan_code_links".to_string(),
                "Plan-code links".to_string(),
            ),
            (
                "setting-counted_as_cash".to_string(),
                "Cash links".to_string(),
            ),
        ];
        let manage: Vec<(String, String, bool)> = tabs(Some(&report), AccessLevel::Admin, &tables)
            .into_iter()
            .map(|tab| (tab.key, tab.title, tab.drawn))
            .collect();
        assert_eq!(
            manage,
            [
                ("summary".to_string(), "Summary".to_string(), true),
                ("settings".to_string(), "Settings".to_string(), true),
                (
                    "setting-plan_code_links".to_string(),
                    "Plan-code links".to_string(),
                    true
                ),
                (
                    "setting-counted_as_cash".to_string(),
                    "Cash links".to_string(),
                    true
                ),
                (
                    "account-links".to_string(),
                    "Account links".to_string(),
                    false
                ),
            ]
        );
        for level in [AccessLevel::Write, AccessLevel::Read] {
            assert!(tabs(Some(&report), level, &tables)
                .iter()
                .all(|tab| !tab.drawn));
        }
    }

    /// The dashboard's own Summary and Settings tabs sit in the one tab row
    /// with the plugin's framed pages, first, and are drawn in the area's
    /// page, not framed: no frame, no page to tell a status or actions, and
    /// no place for either.
    #[test]
    fn the_dashboards_tabs_are_drawn_in_the_area_first_in_the_one_tab_row() {
        let report = report(&[
            ("/admin/connections", "Connections", ADMIN),
            ("/admin/accounts", "Account links", ADMIN),
        ]);
        let manage = tabs(Some(&report), AccessLevel::Admin, &[]);
        let held = Held {
            admin: true,
            ..Default::default()
        };
        let page = render(&Area {
            instance: "snaptrade",
            name: "SnapTrade",
            held: &held,
            level: AccessLevel::Admin,
            tabs: &manage,
            current: Some(&manage[0]),
            shown: Shown::Drawn("<section id=\"status\">the status</section>".into()),
            health: None,
        });
        let nav = page
            .split("<nav class=\"tabs view-tabs\"")
            .nth(1)
            .and_then(|rest| rest.split("</nav>").next())
            .expect("the tab row");
        assert_eq!(
            nav,
            " aria-label=\"The plugin's pages\">\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=summary\" data-tab=\"summary\" data-drawn \
             class=\"here\" aria-current=\"page\">Summary</a>\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=settings\" data-tab=\"settings\" data-drawn>\
             Settings</a>\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=connections\" data-tab=\"connections\" \
             data-page=\"/admin/connections\">Connections</a>\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=account-links\" data-tab=\"account-links\" \
             data-page=\"/admin/accounts\">Account links</a>"
        );
        assert!(page.contains(
            "<div class=\"tab-body\" data-current=\"summary\"><div class=\"area-drawn\" id=\"plugin-page\">\
             <section id=\"status\">the status</section></div></div>"
        ));
        for framed in ["<iframe", PAGE_ACTIONS] {
            assert!(!page.contains(framed), "{framed} in {page}");
        }
        assert!(
            page.contains(&format!(
                "<h1 title=\"SnapTrade\">SnapTrade</h1><span class=\"title-status\" id=\"{PAGE_STATUS}\"></span></div>"
            )),
            "{page}"
        );
    }

    /// The head as the product owner agreed it on 2026-10-01 ("yes, dot on
    /// every tab"; "make the level toggle consistent across all level pages";
    /// "maybe the circular arrow to the left of the toggle"): on every tab
    /// under Manage -- the dashboard's Summary and Settings and the plugin's
    /// Account links alike -- the house, the name and the dot right after it;
    /// and in the head's right-hand group the page's actions, where it has
    /// any, immediately left of the switch, which is always the group's last.
    #[test]
    fn every_tab_under_manage_has_the_dot_after_the_name_and_the_switch_last_on_the_right() {
        let report = report(&[
            ("/admin/connections", "Connections", ADMIN),
            ("/admin/accounts", "Account links", ADMIN),
            ("/statements", "Statements", DATA),
        ]);
        let manage = tabs(Some(&report), AccessLevel::Admin, &[]);
        let held = Held {
            admin: true,
            data: Some(AccessLevel::Write),
            ..Default::default()
        };
        let health = State {
            word: "Not healthy",
            good: false,
            dot: "error",
            detail: "The last read failed: <503>".into(),
        };
        let at = |tab: &Tab| {
            render(&Area {
                instance: "snaptrade",
                name: "SnapTrade",
                held: &held,
                level: AccessLevel::Admin,
                tabs: &manage,
                current: Some(tab),
                shown: if tab.drawn {
                    Shown::Drawn("<section>drawn</section>".into())
                } else {
                    Shown::Framed {
                        src: "/plugins/snaptrade/enter?path=%2Fadmin%2Faccounts&level=admin".into(),
                        origin: "https://snaptrade.plugins.meridian.example".into(),
                    }
                },
                health: Some(&health),
            })
        };
        let dot = format!(
            "<span class=\"title-status\" id=\"{PAGE_STATUS}\"><button type=\"button\" class=\"status-dot\" \
             data-state=\"error\" aria-label=\"Not healthy\" data-note=\"Not healthy\" \
             aria-describedby=\"{PAGE_STATUS}-about-0\"></button><span id=\"{PAGE_STATUS}-about-0\" hidden>\
             The last read failed: &lt;503&gt;</span></span>"
        );
        let switch = "<nav class=\"level-switch\" aria-label=\"Open it as\">\
             <a href=\"/plugins/snaptrade?level=admin\" data-level=\"admin\" class=\"here\" aria-current=\"page\">\
             Manage</a><a href=\"/plugins/snaptrade?level=write\" data-level=\"write\">Open</a>\
             <a href=\"/plugins/snaptrade?level=read\" data-level=\"read\">View</a></nav>";
        // And the same links as a menu naming the level, for a phone's one row.
        let menu = "<details class=\"menu level-menu\"><summary aria-label=\"Open it as: Manage\">Manage</summary>\
             <nav class=\"menu-pop\" aria-label=\"Open it as\">\
             <a href=\"/plugins/snaptrade?level=admin\" data-level=\"admin\" class=\"here\" aria-current=\"page\">\
             Manage</a><a href=\"/plugins/snaptrade?level=write\" data-level=\"write\">Open</a>\
             <a href=\"/plugins/snaptrade?level=read\" data-level=\"read\">View</a></nav></details>";
        for key in ["summary", "settings", "account-links"] {
            let tab = manage.iter().find(|tab| tab.key == key).expect("the tab");
            let page = at(tab);
            let head = page.split("<nav class=\"tabs").next().expect("the heading");
            let title = head
                .split("<div class=\"area-title\">")
                .nth(1)
                .and_then(|rest| rest.split("</div><p class=\"area-id\">").next())
                .expect("the title row");
            // The name and its dot, and nothing else, on the left.
            assert_eq!(
                title,
                format!(
                    "<a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\">{HOUSE}</a>\
                     <h1 title=\"SnapTrade\">SnapTrade</h1>{dot}"
                ),
                "{key}"
            );
            // The right-hand group: a framed page's actions, then the switch,
            // the head's last, in the same place on every tab; a drawn tab
            // has no actions.
            let side = head
                .split("<div class=\"head-side\">")
                .nth(1)
                .expect("the right-hand group");
            let actions = format!(
                "<div class=\"actions\" id=\"{PAGE_ACTIONS}\" role=\"group\" \
                 aria-label=\"Account links actions\"></div>"
            );
            let report = report_problem("snaptrade");
            let expected = if tab.drawn {
                format!("{report}{switch}{menu}</div></div>")
            } else {
                format!("{report}{actions}{switch}{menu}</div></div>")
            };
            assert_eq!(side, expected, "{key}");
        }
        // A healthy plugin's dot says so, with no why to describe it.
        let well = State {
            word: "Healthy",
            good: true,
            dot: "ok",
            detail: String::new(),
        };
        let page = render(&Area {
            instance: "snaptrade",
            name: "SnapTrade",
            held: &held,
            level: AccessLevel::Admin,
            tabs: &manage,
            current: Some(&manage[0]),
            shown: Shown::Drawn(String::new()),
            health: Some(&well),
        });
        assert!(page.contains(&format!(
            "<h1 title=\"SnapTrade\">SnapTrade</h1><span class=\"title-status\" id=\"{PAGE_STATUS}\"><button type=\"button\" \
             class=\"status-dot\" data-state=\"ok\" aria-label=\"Healthy\" data-note=\"Healthy\"></button></span>"
        )));
    }

    #[test]
    fn a_tab_is_a_link_naming_the_level_and_the_page() {
        assert_eq!(
            href("snaptrade-1", AccessLevel::Admin, Some("account-links")),
            "/plugins/snaptrade-1?level=admin&tab=account-links"
        );
        assert_eq!(
            href("snaptrade-1", AccessLevel::Read, None),
            "/plugins/snaptrade-1?level=read"
        );
    }
}
