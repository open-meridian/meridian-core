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
//! **seamlessly** -- no border and no scroll of its own, as tall as the page
//! says it is by `meridian:size`, with `om-framed=1` on its address so the
//! kit draws no heading or tab row of its own. The page's header actions
//! (`meridian:actions`) are drawn in the area's head, as the admin view drew
//! them for its pages before they moved here (meridian-core 1b2a9ad), and its
//! status dot (`meridian:status`, kit 0.7.0) right after the plugin's name
//! title, centred on it (the product owner, 2026-09-30: "green check circle
//! should be next to plugin name title of the form"). The frame stays, so the plugin's script is kept
//! from the person's dashboard session (decisions/021). No way to the
//! plugin's tabs in the admin portal is drawn here (the product owner,
//! 2026-09-30: "Remove 'its settings and access' link"); a deployment admin
//! reaches them from Settings, whose Plugins tab lists every instance.
//!
//! A tab is a link, `?level=` the session's and `&tab=` the page's title in
//! lower case, so each opens directly and none needs script.

use meridian_access::{button, level_name, AccessLevel, Held};
use meridian_domain::v1::PluginReport;

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

/// The tab the dashboard draws first under Manage: the plugin's status and
/// its settings form, the admin portal's, drawn here (the product owner,
/// 2026-10-01: "build Settings and the status panel under Manage").
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
/// At `admin`, before them all, the dashboard's own Settings tab
/// ([`SETTINGS`]): a page of the plugin's titled "Settings" is then
/// `settings-2` in the query.
pub fn tabs(report: Option<&PluginReport>, level: AccessLevel) -> Vec<Tab> {
    let interface = report.and_then(|report| report.declared_interface.as_ref());
    let declared = interface
        .map(|interface| interface.pages.as_slice())
        .unwrap_or_default();
    let mut tabs: Vec<Tab> = Vec::new();
    if level == AccessLevel::Admin {
        tabs.push(Tab {
            key: SETTINGS.into(),
            title: "Settings".into(),
            path: String::new(),
            drawn: true,
        });
    }
    for page in declared
        .iter()
        .filter(|page| page.levels.contains(&(level as i32)))
    {
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
pub const PAGE_ACTIONS: &str = "page-actions";

/// Where a framed page's own status dot goes (meridian-ui's
/// `meridian:status`, kit 0.7.0): right after the plugin's name title, in a
/// place empty, and taking no room, until the page tells it a status, which
/// the chrome's script draws it in; its frame names it.
pub const PAGE_STATUS: &str = "page-status";

/// The page as this dashboard can show it.
pub enum Shown {
    /// Framed seamlessly: the frame's way in, and the plugin's origin its
    /// theme goes to.
    Framed { src: String, origin: String },
    /// A tab the dashboard draws itself ([`SETTINGS`]), as HTML.
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
}

/// The buttons for the levels held, the session's pressed: a person holding
/// `admin` and `write` moves between Manage, Open and View here as on the
/// home.
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
    format!("<nav class=\"level-switch\" aria-label=\"Open it as\">{links}</nav>")
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

pub fn render(area: &Area) -> String {
    let framed = matches!(area.shown, Shown::Framed { .. });
    let (actions, status) = if framed {
        (
            format!(
                "<div class=\"actions\" id=\"{PAGE_ACTIONS}\" role=\"group\" aria-label=\"{} actions\"></div>",
                escape(area.current.map(|tab| tab.title.as_str()).unwrap_or_default())
            ),
            format!("<span class=\"title-status\" id=\"{PAGE_STATUS}\"></span>"),
        )
    } else {
        (String::new(), String::new())
    };
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
        "<div class=\"plugin-area\" data-level=\"{level}\"><div class=\"page-head\"><div>\
         <div class=\"area-title\"><a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\">{HOUSE}</a>\
         <h1>{name}</h1>{status}</div><p><code>{instance}</code></p></div><div class=\"head-side\">{levels}{actions}</div></div>\
         {nav}<div class=\"tab-body\" data-current=\"{current}\">{body}</div></div>",
        level = level_name(area.level),
        name = escape(area.name),
        instance = escape(area.instance),
        levels = levels(area),
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
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    const ADMIN: &[AccessLevel] = &[AccessLevel::Admin];
    const DATA: &[AccessLevel] = &[AccessLevel::Write, AccessLevel::Read];

    #[test]
    fn each_button_shows_the_pages_whose_levels_include_its_level_in_order() {
        let snaptrade = report(&[
            ("/admin/connections", "Connections", ADMIN),
            ("/statements", "Statements", DATA),
            ("/admin/accounts", "Account links", ADMIN),
        ]);
        let titles = |level| -> Vec<String> {
            tabs(Some(&snaptrade), level)
                .into_iter()
                .map(|tab| format!("{} {}", tab.key, tab.path))
                .collect()
        };
        // Under Manage, the dashboard's Settings first, then the plugin's
        // own pages at admin (the product owner, 2026-10-01).
        assert_eq!(
            titles(AccessLevel::Admin),
            [
                "settings ",
                "connections /admin/connections",
                "account-links /admin/accounts"
            ]
        );
        let manage = tabs(Some(&snaptrade), AccessLevel::Admin);
        assert!(manage[0].drawn && manage[0].title == "Settings");
        assert!(manage[1..].iter().all(|tab| !tab.drawn));
        assert!(
            [AccessLevel::Write, AccessLevel::Read]
                .into_iter()
                .all(|level| tabs(Some(&snaptrade), level).iter().all(|tab| !tab.drawn)),
            "no Settings under Open or View"
        );
        assert_eq!(titles(AccessLevel::Write), ["statements /statements"]);
        assert_eq!(
            titles(AccessLevel::Read),
            ["statements /statements"],
            "one URL for Open and View"
        );
    }

    #[test]
    fn a_plugin_with_no_page_at_write_or_read_has_its_root_and_none_at_admin_its_settings_alone() {
        let only_admin = report(&[("/admin", "Admin", ADMIN)]);
        let open = tabs(Some(&only_admin), AccessLevel::Write);
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].path.as_str(), open[0].title.as_str()),
            ("/", "SnapTrade")
        );
        let only_data = report(&[("/", "Home", DATA)]);
        let manage = tabs(Some(&only_data), AccessLevel::Admin);
        assert_eq!(manage.len(), 1);
        assert!(manage[0].drawn && manage[0].key == SETTINGS);
        assert_eq!(tabs(None, AccessLevel::Read)[0].path, "/");
    }

    #[test]
    fn a_plugin_built_before_v5_declaring_no_admin_page_keeps_its_admin() {
        let older = PluginReport {
            contract_version: "v4".into(),
            ..report(&[])
        };
        let manage = tabs(Some(&older), AccessLevel::Admin);
        assert_eq!(manage.len(), 2);
        assert!(manage[0].drawn);
        assert_eq!(
            (manage[1].path.as_str(), manage[1].title.as_str()),
            ("/admin", "Admin page")
        );
        let newer = PluginReport {
            contract_version: "v5".into(),
            ..report(&[])
        };
        let manage = tabs(Some(&newer), AccessLevel::Admin);
        assert!(manage.len() == 1 && manage[0].drawn, "its settings alone");
    }

    #[test]
    fn a_path_not_on_the_plugins_host_or_shown_already_is_left_out() {
        let odd = report(&[
            ("//elsewhere.example", "Away", ADMIN),
            ("/.meridian/enter", "Ours", ADMIN),
            ("/a", "Settings", ADMIN),
            ("/a", "Again", ADMIN),
            ("/b", "Settings", ADMIN),
        ]);
        let keys: Vec<String> = tabs(Some(&odd), AccessLevel::Admin)
            .into_iter()
            .map(|tab| tab.key)
            .collect();
        // The dashboard's Settings keeps its name; the plugin's are numbered.
        assert_eq!(keys, ["settings", "settings-2", "settings-3"]);
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
        });
        let head = page.split("<nav class=\"tabs").next().expect("the heading");
        assert_eq!(
            head,
            format!(
                "<div class=\"plugin-area\" data-level=\"admin\"><div class=\"page-head\"><div>\
                 <div class=\"area-title\"><a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\">\
                 {HOUSE}</a><h1>Snap&lt;Trade&gt;</h1><span class=\"title-status\" id=\"{PAGE_STATUS}\"></span>\
                 </div><p><code>snaptrade</code></p></div>\
                 <div class=\"head-side\"><span class=\"badge accent\" data-level=\"admin\">Manage</span>\
                 <div class=\"actions\" id=\"{PAGE_ACTIONS}\" role=\"group\" aria-label=\"Connections actions\"></div>\
                 </div></div>"
            )
        );
        for portal in ["/admin/plugins/", "data-portal", "settings and access"] {
            assert!(!page.contains(portal), "{portal} in {page}");
        }
        // The house is the header's own, drawn once.
        assert_eq!(page.matches("<svg").count(), 1);
    }

    /// The dashboard's own Settings tab sits in the one tab row with the
    /// plugin's framed pages, first, and is drawn in the area's page, not
    /// framed: no frame, no page to tell a status or actions, and no place
    /// for either.
    #[test]
    fn the_dashboards_settings_tab_is_drawn_in_the_area_first_in_the_one_tab_row() {
        let report = report(&[
            ("/admin/connections", "Connections", ADMIN),
            ("/admin/accounts", "Account links", ADMIN),
        ]);
        let manage = tabs(Some(&report), AccessLevel::Admin);
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
            shown: Shown::Drawn("<section id=\"settings\">the form</section>".into()),
        });
        let nav = page
            .split("<nav class=\"tabs view-tabs\"")
            .nth(1)
            .and_then(|rest| rest.split("</nav>").next())
            .expect("the tab row");
        assert_eq!(
            nav,
            " aria-label=\"The plugin's pages\">\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=settings\" data-tab=\"settings\" data-drawn \
             class=\"here\" aria-current=\"page\">Settings</a>\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=connections\" data-tab=\"connections\" \
             data-page=\"/admin/connections\">Connections</a>\
             <a href=\"/plugins/snaptrade?level=admin&amp;tab=account-links\" data-tab=\"account-links\" \
             data-page=\"/admin/accounts\">Account links</a>"
        );
        assert!(page.contains(
            "<div class=\"tab-body\" data-current=\"settings\"><div class=\"area-drawn\" id=\"plugin-page\">\
             <section id=\"settings\">the form</section></div></div>"
        ));
        for framed in ["<iframe", PAGE_STATUS, PAGE_ACTIONS] {
            assert!(!page.contains(framed), "{framed} in {page}");
        }
        assert!(page.contains("<h1>SnapTrade</h1></div>"), "{page}");
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
