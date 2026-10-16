//! A version's documentation (W6.9, W8.2, contract v25;
//! plans/a-plugin-carries-its-own-documentation): the vendor's sections,
//! drawn safely ([`crate::markdown`]), and the Reference, drawn by core from
//! the version's own declaration.
//!
//! Reached three ways, each the same drawing:
//!
//! - **a launched or stopped instance's** Documentation tab in its area, at
//!   any level on the plugin, its launched version's;
//! - **a version never launched**, from the deployment admin's portal: the
//!   versions list, `/admin/plugins/versions`, each linking its page,
//!   `/admin/plugins/versions/{name}/{version}/documentation`;
//! - **an agent's** `dashboard__read_plugin_documentation`
//!   ([`crate::mcp::plugin_area`]), each section under a key saying it is the
//!   vendor's text, a section the screen holds as suspect withheld.
//!
//! One screen: the sections (Overview, Setup, Pages, Troubleshooting, Terms,
//! Reference) are tabs; within a section each level-one and level-two
//! heading is an entry in a contents list beside it, one shown at a time; the
//! Reference pages by part, each a kit table with its pager. Every tab and
//! entry is a link, so none needs script. The supervision notice is drawn
//! beside the vendor's text in full where the screen is wide, and by its
//! title, opening in place, where it is not.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use meridian_bus::BusError;
use meridian_domain::v1::{
    PluginVersion, ReadPluginDocumentationReply, ReadPluginDocumentationRequest,
};
use meridian_pb::v1::{AccessLevel, PluginDeclaration, SettingType};
use prost::Message;

use crate::html::escape;
use crate::web::App;

/// The tab in a plugin's area, at every level.
pub const TAB: &str = "documentation";

/// W8.2 (contract v25): one version's declaration and texts.
pub const READ: &str = "platform.config.query.plugin-documentation";

/// The vendor's sections, as the tab draws them, in order.
pub const SECTIONS: [(&str, &str); 5] = [
    ("overview", "Overview"),
    ("setup", "Setup"),
    ("pages", "Pages"),
    ("troubleshooting", "Troubleshooting"),
    ("terms", "Terms"),
];

/// The Reference: core's, from the declaration, after the vendor's.
pub const REFERENCE: &str = "reference";

/// The Reference's parts, each a kit table.
pub const REFERENCE_PARTS: [(&str, &str); 7] = [
    ("roles", "Roles"),
    ("settings", "Settings"),
    ("pages", "Pages"),
    ("tools", "Tools"),
    ("storage", "Storage"),
    ("datasets", "Datasets"),
    ("not-carried", "Not carried"),
];

/// Where a version's page is in the portal.
pub fn version_path(name: &str, version: &str) -> String {
    format!("/admin/plugins/versions/{name}/{version}/documentation")
}

/// The portal's list of versions.
pub const VERSIONS_PATH: &str = "/admin/plugins/versions";

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/admin/plugins/versions", get(versions_page))
        .route(
            "/admin/plugins/versions/{name}/{version}/documentation",
            get(version_page),
        )
}

/// One version's documentation, read as the person.
pub async fn read(
    app: &App,
    subject: &str,
    name: &str,
    version: &str,
) -> Result<ReadPluginDocumentationReply, String> {
    let (_, bytes) = app
        .bus
        .call_for(
            READ,
            "meridian.v1.ReadPluginDocumentationRequest",
            ReadPluginDocumentationRequest {
                name: name.into(),
                version: version.into(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
            subject,
        )
        .await
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => meridian_bus::read_refusal(&detail)
                .map_or(detail.clone(), |(_, words)| words.to_string()),
            other => other.to_string(),
        })?;
    ReadPluginDocumentationReply::decode(&bytes[..])
        .map_err(|failed| format!("an undecodable reply: {failed}"))
}

/// What the screen finds in one section (W6.20's marking): the rules its
/// visible text, tag characters read as letters, matched; and the first
/// character that hides or reorders words.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub rules: Vec<String>,
    pub hidden: Option<meridian_domain::text::Refused>,
}

impl Screen {
    pub fn suspect(&self) -> bool {
        !self.rules.is_empty() || self.hidden.is_some()
    }

    /// Why, in words an agent and a person read alike.
    pub fn why(&self) -> String {
        let mut said = Vec::new();
        if !self.rules.is_empty() {
            said.push(format!(
                "it reads like an instruction to an agent ({})",
                self.rules.join(", ")
            ));
        }
        if let Some(hidden) = &self.hidden {
            said.push(format!(
                "it holds characters that hide or reorder words (U+{:04X}, {})",
                hidden.character as u32, hidden.kind
            ));
        }
        said.join("; ")
    }
}

pub fn screen(markdown: &str) -> Screen {
    let matched = crate::tickets::quarantine::matched(&crate::markdown::screened_text(markdown));
    Screen {
        rules: crate::tickets::quarantine::names(&matched),
        hidden: crate::markdown::hidden(markdown),
    }
}

/// What a page draws: a version's documentation, its roles, where the
/// person is in it, and where each tab and entry goes.
pub struct Page<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub roles: &'a [String],
    pub reply: &'a ReadPluginDocumentationReply,
    /// The section asked for: one of [`SECTIONS`] present, or [`REFERENCE`].
    pub section: Option<&'a str>,
    pub part: Option<&'a str>,
    /// Where a section and a part are, for this page.
    pub href: &'a dyn Fn(&str, &str) -> String,
}

/// The documentation, drawn.
pub fn drawn(page: &Page) -> String {
    let present: Vec<&str> = SECTIONS
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| page.reply.documentation.iter().any(|t| t.section == *key))
        .collect();
    let section = page
        .section
        .filter(|asked| *asked == REFERENCE || present.contains(asked))
        .or_else(|| present.first().copied())
        .unwrap_or(REFERENCE);
    let tabs: String = present
        .iter()
        .map(|key| (*key, title_of(key)))
        .chain([(REFERENCE, "Reference")])
        .map(|(key, title)| {
            format!(
                "<a href=\"{href}\" data-section=\"{key}\"{here}>{title}</a>",
                href = escape(&(page.href)(key, "")),
                here = here(key == section),
            )
        })
        .collect();
    let (contents, body) = if section == REFERENCE {
        reference(page)
    } else {
        vendor_section(page, section, &present)
    };
    let uploaded = if page.reply.uploaded_at_ns > 0 {
        format!(
            " &middot; uploaded by {} on {}",
            escape(&page.reply.uploaded_by),
            &crate::terminal::rfc3339(page.reply.uploaded_at_ns)[..10]
        )
    } else {
        String::new()
    };
    let none = if present.is_empty() {
        "<p class=\"hint docs-none\">Its vendor wrote none for this version.</p>"
    } else {
        ""
    };
    format!(
        "<div class=\"docs\" data-docs data-version=\"{version}\">\
         <div class=\"docs-head\"><p class=\"docs-what\"><strong>{name}</strong> {version}{uploaded}</p>{folded}</div>\
         {none}<nav class=\"tabs docs-sections\" aria-label=\"Its documentation\">{tabs}</nav>\
         <div class=\"docs-body\">{contents}<div class=\"docs-text\">{body}</div>\
         <aside class=\"docs-notice\" aria-label=\"{notice_title}\">{notice}</aside></div></div>",
        name = escape(page.name),
        version = escape(page.version),
        folded = crate::notice::folded(),
        notice = crate::notice::html(),
        notice_title = escape(crate::notice::TITLE),
    )
}

fn title_of(key: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, title)| *title)
        .unwrap_or("Section")
}

fn here(on: bool) -> &'static str {
    if on {
        " class=\"here\" aria-current=\"page\""
    } else {
        ""
    }
}

/// The contents beside a section: one entry per part, the one shown marked.
fn contents(entries: &[(String, String, bool)]) -> String {
    if entries.len() < 2 {
        return String::new();
    }
    let links: String = entries
        .iter()
        .map(|(href, title, on)| {
            format!(
                "<li><a href=\"{}\"{}>{}</a></li>",
                escape(href),
                here(*on),
                escape(title)
            )
        })
        .collect();
    format!(
        "<nav class=\"docs-contents\" aria-label=\"Contents\"><details class=\"docs-contents-menu\" open>\
         <summary>Contents</summary><ol>{links}</ol></details></nav>"
    )
}

fn vendor_section(page: &Page, section: &str, present: &[&str]) -> (String, String) {
    let text = page
        .reply
        .documentation
        .iter()
        .find(|t| t.section == section)
        .map(|t| t.markdown.as_str())
        .unwrap_or_default();
    let href = |section: &str, part: &str| (page.href)(section, part);
    let parts = crate::markdown::render(text, present, &href);
    let shown = page
        .part
        .and_then(|asked| parts.iter().position(|p| p.key == asked))
        .unwrap_or(0);
    let title = title_of(section);
    let entries: Vec<(String, String, bool)> = parts
        .iter()
        .enumerate()
        .map(|(at, part)| {
            (
                (page.href)(section, &part.key),
                part.heading.clone().unwrap_or_else(|| title.to_string()),
                at == shown,
            )
        })
        .collect();
    let screened = screen(text);
    let warning = if screened.suspect() {
        format!(
            "<p class=\"docs-warning\" role=\"note\" data-suspect>This section is shown here and \
             withheld from agents: {}.</p>",
            escape(&screened.why())
        )
    } else {
        String::new()
    };
    let html = parts
        .get(shown)
        .map(|p| p.html.as_str())
        .unwrap_or_default();
    (
        contents(&entries),
        format!(
            "<p class=\"docs-by\">Written by its vendor, not by this deployment or Open Meridian.</p>\
             {warning}<article class=\"docs-vendor\" data-section=\"{section}\" aria-label=\"{title}, \
             its vendor's text\">{html}</article>"
        ),
    )
}

// ── The Reference ───────────────────────────────────────────────────────

/// What each role reads, hears and publishes: the contract's rows naming
/// it, as the runtime holds them (decisions/020).
fn role_rows(role: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    const TOPICS: &str = include_str!("../../../deploy/topics.tsv");
    let (mut reads, mut hears, mut publishes) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for line in TOPICS.lines() {
        if line.starts_with('#') || line.starts_with("topic\t") || line.trim().is_empty() {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        let [topic, kind, publisher, subscriber, ..] = cells[..] else {
            continue;
        };
        let names = |cell: &str| cell.split(',').any(|name| name.trim() == role);
        match kind {
            "query" if names(publisher) => {
                reads.insert(topic.to_string());
            }
            "event" if names(subscriber) => {
                hears.insert(topic.to_string());
            }
            "command" | "event" if names(publisher) => {
                publishes.insert(topic.to_string());
            }
            _ => {}
        }
    }
    (
        reads.into_iter().collect(),
        hears.into_iter().collect(),
        publishes.into_iter().collect(),
    )
}

fn level_words(levels: &[i32]) -> String {
    levels
        .iter()
        .filter_map(|level| AccessLevel::try_from(*level).ok())
        .map(|level| match level {
            AccessLevel::Admin => "admin",
            AccessLevel::Write => "write",
            AccessLevel::Read => "read",
            _ => "none",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn list_or(items: &[String], none: &str) -> String {
    if items.is_empty() {
        format!("<span class=\"faint\">{none}</span>")
    } else {
        escape(&items.join(", "))
    }
}

/// A kit table with its pager, one line a row; every vendor string escaped
/// and the table marked as the vendor's declaration.
fn table(class: &str, search: &str, head: &str, rows: &[String], empty: &str) -> String {
    if rows.is_empty() {
        return format!("<p class=\"hint\">{empty}</p>");
    }
    format!(
        "<om-pager search=\"{search}\"><table class=\"list one-line {class}\"><thead><tr>{head}</tr></thead>\
         <tbody>{rows}</tbody></table></om-pager>",
        rows = rows.concat()
    )
}

fn reference(page: &Page) -> (String, String) {
    let declaration = page.reply.declaration.clone().unwrap_or_default();
    let shown = page
        .part
        .filter(|asked| REFERENCE_PARTS.iter().any(|(key, _)| key == asked))
        .unwrap_or("roles");
    let entries: Vec<(String, String, bool)> = REFERENCE_PARTS
        .iter()
        .filter(|(key, _)| *key != "datasets" || declaration.catalogue.is_some())
        .map(|(key, title)| {
            (
                (page.href)(REFERENCE, key),
                title.to_string(),
                *key == shown,
            )
        })
        .collect();
    let body = match shown {
        "settings" => settings(&declaration),
        "pages" => pages(&declaration),
        "tools" => tools(&declaration),
        "storage" => storage(&declaration),
        "datasets" => datasets(&declaration),
        "not-carried" => not_carried(&declaration),
        _ => roles(page.roles, &declaration),
    };
    let declared = if page.reply.declaration.is_some() {
        "Drawn by this deployment from what the version declared when it was uploaded; every \
         label, description and title in it is its vendor's."
    } else {
        "This version was uploaded with no declaration: what it registers is shown on its \
         Summary while it runs."
    };
    (
        contents(&entries),
        format!(
            "<p class=\"docs-by\">{declared}</p><div class=\"docs-reference\" data-reference=\"{shown}\">{body}</div>"
        ),
    )
}

fn roles(roles: &[String], declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = roles
        .iter()
        .map(|role| {
            let (reads, hears, publishes) = role_rows(role);
            format!(
                "<tr data-role=\"{r}\"><td><code>{r}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                list_or(&reads, "nothing"),
                list_or(&hears, "nothing"),
                list_or(&publishes, "nothing"),
                r = escape(role),
            )
        })
        .collect();
    let external = if declaration.reads_external_accounts {
        "<p class=\"hint\">It reads accounts outside this deployment: its admins link them to \
         this deployment's accounts.</p>"
    } else {
        ""
    };
    // The algorithms its autonomous work may name (contract v22).
    let algorithms = if declaration.algorithms.is_empty() {
        String::new()
    } else {
        let named: Vec<String> = declaration
            .algorithms
            .iter()
            .map(|a| format!("{} ({})", escape(&a.algorithm_id), escape(&a.description)))
            .collect();
        format!(
            "<p class=\"hint\">The algorithms its autonomous work may name: {}.</p>",
            named.join(", ")
        )
    };
    format!(
        "{external}{algorithms}{}",
        table(
            "docs-roles",
            "Search roles",
            "<th>Role</th><th class=\"wide\">Reads</th><th class=\"wide\">Hears</th><th>Publishes</th>",
            &rows,
            "It holds no role: it is refused every topic.",
        )
    )
}

fn setting_type(setting: &meridian_pb::v1::SettingDeclaration) -> &'static str {
    match SettingType::try_from(setting.r#type) {
        Ok(SettingType::String) => "text",
        Ok(SettingType::Integer) => "whole number",
        Ok(SettingType::Boolean) => "yes or no",
        Ok(SettingType::Choice) => "choice",
        Ok(SettingType::Table) => "table",
        _ => "unspecified",
    }
}

fn settings(declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = declaration
        .settings
        .iter()
        .map(|s| {
            let mut kind = setting_type(s).to_string();
            if !s.choices.is_empty() {
                let choices: Vec<String> = s.choices.iter().map(|c| c.label.clone()).collect();
                kind = format!("{kind}: {}", choices.join(", "));
            }
            if !s.columns.is_empty() {
                let columns: Vec<String> = s.columns.iter().map(|c| c.label.clone()).collect();
                kind = format!("{kind} of {}", columns.join(", "));
            }
            let mut marks = Vec::new();
            if s.secret {
                marks.push("secret");
            }
            if s.required {
                marks.push("required");
            }
            if s.per_person {
                marks.push("each person's own");
            }
            let default = if s.secret || s.default_value.is_empty() {
                String::new()
            } else if s.unit.is_empty() {
                s.default_value.clone()
            } else {
                format!("{} {}", s.default_value, s.unit)
            };
            format!(
                "<tr data-setting=\"{name}\"><td title=\"{description}\">{label} <code>{name}</code></td>\
                 <td>{kind}</td><td class=\"wide\">{default}</td><td>{marks}</td><td class=\"wide\">{roles}</td></tr>",
                name = escape(&s.name),
                label = escape(&s.label),
                description = escape(&s.description),
                kind = escape(&kind),
                default = escape(&default),
                marks = escape(&marks.join(", ")),
                roles = list_or(&s.roles, "every role"),
            )
        })
        .collect();
    table(
        "docs-settings",
        "Search settings",
        "<th>Setting</th><th>Type</th><th class=\"wide\">Default</th><th>Kind</th><th class=\"wide\">Roles</th>",
        &rows,
        "It declares no setting.",
    )
}

fn pages(declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = declaration
        .interface
        .iter()
        .flat_map(|interface| &interface.pages)
        .map(|p| {
            format!(
                "<tr data-page=\"{path}\"><td>{title}</td><td><code>{path}</code></td><td>{levels}</td>\
                 <td class=\"wide\">{roles}</td></tr>",
                title = escape(&p.title),
                path = escape(&p.path),
                levels = escape(&level_words(&p.levels)),
                roles = list_or(&p.roles, "every role"),
            )
        })
        .collect();
    table(
        "docs-pages",
        "Search pages",
        "<th>Page</th><th>Path</th><th>Levels</th><th class=\"wide\">Roles</th>",
        &rows,
        "It serves no page.",
    )
}

fn tools(declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = declaration
        .tools
        .iter()
        .map(|t| {
            format!(
                "<tr data-tool=\"{name}\"><td title=\"{description}\"><code>{name}</code> {title}</td>\
                 <td>{kind}</td><td>{levels}</td><td class=\"wide\">{roles}</td></tr>",
                name = escape(&t.name),
                title = escape(&t.title),
                description = escape(&t.description),
                kind = if t.reads { "reads" } else { "acts" },
                levels = escape(&level_words(&t.levels)),
                roles = list_or(&t.roles, "every role"),
            )
        })
        .collect();
    table(
        "docs-tools",
        "Search tools",
        "<th>Tool</th><th>Reads or acts</th><th>Levels</th><th class=\"wide\">Roles</th>",
        &rows,
        "It offers agents no tool.",
    )
}

fn storage(declaration: &PluginDeclaration) -> String {
    let Some(storage) = &declaration.storage else {
        return "<p class=\"hint\">It asks for no storage of its own.</p>".into();
    };
    let rows: Vec<String> = storage
        .record_kinds
        .iter()
        .map(|kind| {
            format!(
                "<tr data-kind=\"{name}\"><td>{label} <code>{name}</code></td><td>{days} days</td><td>{archive}</td></tr>",
                name = escape(&kind.name),
                label = escape(&kind.label),
                days = kind.window_days,
                archive = if kind.archivable { "can be archived" } else { "not archived" },
            )
        })
        .collect();
    format!(
        "<p class=\"hint\">Storage of its own, kept {} days at most.</p>{}",
        storage.retention_days,
        table(
            "docs-storage",
            "Search kinds",
            "<th>Kind of raw record</th><th>Default window</th><th>Archive</th>",
            &rows,
            "It keeps no kind of raw record.",
        )
    )
}

fn datasets(declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = declaration
        .catalogue
        .iter()
        .flat_map(|catalogue| &catalogue.datasets)
        .map(|d| {
            let modes: Vec<String> = d
                .modes
                .iter()
                .map(|m| crate::declaration::mode_word(*m).to_string())
                .collect();
            format!(
                "<tr data-dataset=\"{key}\"><td><code>{key}</code></td><td>{vendor}</td>\
                 <td class=\"wide\">{types}</td><td>{modes}</td><td class=\"wide\">{day}</td></tr>",
                key = escape(&d.key),
                vendor = escape(&d.vendor),
                types = escape(&d.data_types.join(", ")),
                modes = escape(&modes.join(", ")),
                day = escape(&d.day_time_zone),
            )
        })
        .collect();
    table(
        "docs-datasets",
        "Search datasets",
        "<th>Dataset</th><th>Vendor</th><th class=\"wide\">Data</th><th>Arrives</th><th class=\"wide\">Its day</th>",
        &rows,
        "It declares no dataset.",
    )
}

fn not_carried(declaration: &PluginDeclaration) -> String {
    let rows: Vec<String> = declaration
        .not_carried
        .iter()
        .map(|held| {
            let why = match meridian_pb::v1::NotCarriedReason::try_from(held.reason) {
                Ok(meridian_pb::v1::NotCarriedReason::NoContractMeaning) => {
                    "no meaning in the contract yet"
                }
                Ok(meridian_pb::v1::NotCarriedReason::NotConverted) => "not yet converted",
                _ => "",
            };
            format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td class=\"wide\">{why}</td></tr>",
                escape(&held.role),
                escape(&held.scheme),
                escape(&held.name),
            )
        })
        .collect();
    let secrets = if declaration.secret_settings.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"hint\">Its secret settings, by name only: {}.</p>",
            escape(&declaration.secret_settings.join(", "))
        )
    };
    format!(
        "{secrets}{}",
        table(
            "docs-not-carried",
            "Search what it does not carry",
            "<th>Role</th><th>Scheme</th><th>Name</th><th class=\"wide\">Why</th>",
            &rows,
            "It declares nothing it receives and does not carry.",
        )
    )
}

// ── The security policy ─────────────────────────────────────────────────

/// The response's content security policy (the plan's "safe subset"): no
/// script but the dashboard's own -- its stylesheet's and modules' origin and
/// its inline scripts by their hashes -- and no image from elsewhere.
pub fn policy() -> String {
    format!(
        "default-src 'self'; script-src 'self' {hashes}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'self'; frame-src 'none'; frame-ancestors 'self'",
        hashes = crate::html::inline_script_hashes().join(" ")
    )
}

/// A page under the policy.
pub fn with_policy(html: String) -> Response {
    let mut response = Html(html).into_response();
    if let Ok(value) = HeaderValue::from_str(&policy()) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

// ── The portal's pages ──────────────────────────────────────────────────

/// Every version in the catalogue, none for a catalogue not read in time.
pub(crate) async fn versions(app: &App) -> Result<Vec<PluginVersion>, String> {
    crate::catalogue::catalogue(app)
        .await
        .map(|catalogue| catalogue.versions)
}

/// `GET /admin/plugins/versions`: each uploaded version on one line, a
/// deployment admin's, each linking its Documentation page.
async fn versions_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let listed = versions(&app).await;
    let body = match listed {
        Err(failed) => format!(
            "<p class=\"error\" role=\"alert\">The catalogue could not be read: {}</p>",
            escape(&failed)
        ),
        Ok(versions) => {
            let rows: Vec<String> = versions
                .iter()
                .map(|v| {
                    let m = v.metadata.clone().unwrap_or_default();
                    let state = if v.retired_at_ns > 0 { "retired" } else { "listed" };
                    let sections = m
                        .declaration
                        .as_ref()
                        .map_or(0, |d| d.documentation.len());
                    format!(
                        "<tr data-version=\"{name}@{version}\"><td><a href=\"{href}\">{name}</a></td>\
                         <td>{version}</td><td class=\"wide\">{roles}</td><td>{sections}</td>\
                         <td class=\"wide\">{uploaded}</td><td>{state}</td></tr>",
                        href = escape(&version_path(&m.name, &m.version)),
                        name = escape(&m.name),
                        version = escape(&m.version),
                        roles = list_or(&m.roles, "none"),
                        uploaded = &crate::terminal::rfc3339(v.uploaded_at_ns)[..10],
                    )
                })
                .collect();
            format!(
                "{notice}{table}",
                notice = crate::notice::folded(),
                table = table(
                    "versions",
                    "Search versions",
                    "<th>Plugin</th><th>Version</th><th class=\"wide\">Roles</th><th>Sections</th>\
                     <th class=\"wide\">Uploaded</th><th>State</th>",
                    &rows,
                    "No version is uploaded yet: `meridian plugin upload` adds one.",
                )
            )
        }
    };
    let body = format!(
        "<section class=\"admin-section versions-page\"><div class=\"section-head\"><div>\
         <h1>Uploaded versions</h1><p class=\"hint\">Each version in this deployment's catalogue, \
         launched or not. Open one to read its documentation before you launch it.</p></div></div>{body}</section>"
    );
    with_policy(crate::html::page_with(
        "Uploaded versions",
        &body,
        &crate::html::Chrome {
            crumbs: format!(
                "{}{}",
                crate::html::crumb_link("/admin#plugins", "Plugins"),
                crate::html::crumb_here("Uploaded versions", None)
            ),
            ..crate::admin::admin_chrome(&session)
        },
    ))
}

#[derive(serde::Deserialize, Default)]
struct Asked {
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    part: Option<String>,
}

/// `GET /admin/plugins/versions/{name}/{version}/documentation`: one
/// version's documentation, before it is launched, a deployment admin's.
async fn version_page(
    State(app): State<Arc<App>>,
    Path((name, version)): Path<(String, String)>,
    Query(asked): Query<Asked>,
    headers: HeaderMap,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let reply = match read(&app, &session.subject, &name, &version).await {
        Ok(reply) => reply,
        Err(failed) => {
            return (
                StatusCode::NOT_FOUND,
                Html(crate::html::page_with(
                    "No such version",
                    &format!("<p class=\"error\" role=\"alert\">{}</p>", escape(&failed)),
                    &crate::admin::admin_chrome(&session),
                )),
            )
                .into_response()
        }
    };
    let roles = versions(&app)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.metadata)
        .find(|m| m.name == name && m.version == version)
        .map(|m| m.roles)
        .unwrap_or_default();
    let base = version_path(&name, &version);
    let href = |section: &str, part: &str| section_href(&base, section, part);
    let body = drawn(&Page {
        name: &name,
        version: &version,
        roles: &roles,
        reply: &reply,
        section: asked.section.as_deref(),
        part: asked.part.as_deref(),
        href: &href,
    });
    with_policy(crate::html::page_with(
        &format!("{name} {version} · Documentation"),
        &format!(
            "<div class=\"docs-page\"><h1 class=\"docs-title\">Documentation</h1>{body}</div>"
        ),
        &crate::html::Chrome {
            crumbs: format!(
                "{}{}{}",
                crate::html::crumb_link("/admin#plugins", "Plugins"),
                crate::html::crumb_link(VERSIONS_PATH, "Uploaded versions"),
                crate::html::crumb_here(&format!("{name} {version}"), None)
            ),
            ..crate::admin::admin_chrome(&session)
        },
    ))
}

/// A page's address at a section and a part, its other query kept.
pub fn section_href(base: &str, section: &str, part: &str) -> String {
    let joiner = if base.contains('?') { '&' } else { '?' };
    let mut url = format!("{base}{joiner}section={}", urlencode(section));
    if !part.is_empty() {
        url.push_str(&format!("&part={}", urlencode(part)));
    }
    url
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
