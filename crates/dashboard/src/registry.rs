//! The deployment admin's Plugins page (W8.2, W8.10 to W8.12, W5.19;
//! contract v25; spec/plugins-are-shared-through-registries, "The
//! deployment's dashboard"): three tabs -- **Installed**, the versions list
//! with each instance's source registry, its update, its widening and its
//! pull warning; **Browse**, the catalogue's copy the conductor keeps,
//! searched and filtered, one line a listing; **Registries**, when the
//! catalogue was last pulled with "Pull now", "Report plugin usage", and
//! the reports sent -- and a listing's page (Overview, Documentation,
//! Versions). Install, update and keep each open as a sheet of their own,
//! the supervision notice first and the act at the foot.
//!
//! Every page reads what the conductor keeps; the platform is never reached
//! in a page's request but by "Pull now". Community is labelled
//! "Unsupervised" wherever it appears.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_bus::BusError;
use meridian_domain::v1::{
    AcknowledgePullRequest, InstallPluginRequest, ListPluginUsageReportsReply,
    ListPluginUsageReportsRequest, ListRegistryPluginsReply, ListRegistryPluginsRequest,
    ListedVersion, PluginCatalogue, PluginLaunch, PluginLaunchState, PluginListing, PluginPull,
    PluginVersion, PullAcknowledgement, PullReasonKind, ReportsPluginUsageSetting,
    SetReportsPluginUsageRequest, UpdatePluginRequest,
};
use prost::Message;

use crate::html::escape;
use crate::session::Session;
use crate::web::App;

pub const REGISTRY_PLUGINS: &str = "platform.config.query.registry-plugins";
pub const INSTALL_PLUGIN: &str = "platform.config.command.install-plugin";
pub const UPDATE_PLUGIN: &str = "platform.config.command.update-plugin";
pub const ACKNOWLEDGE_PULL: &str = "platform.config.command.acknowledge-pull";
pub const SET_REPORTS_PLUGIN_USAGE: &str = "platform.config.command.set-reports-plugin-usage";
pub const LIST_PLUGIN_USAGE_REPORTS: &str = "platform.config.query.list-plugin-usage-reports";

/// How long an install or an update may take: a resolution, an image copied
/// and verified, and for an update the instance made again.
pub const INSTALLING: Duration = Duration::from_secs(300);
const READING: Duration = Duration::from_secs(10);
const PULLING: Duration = Duration::from_secs(60);

/// The Plugins page.
pub const PLUGINS_PATH: &str = "/admin/plugins/versions";

/// A listing's page.
pub fn listing_path(listing: &str) -> String {
    format!("/admin/plugins/registry/{listing}")
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/admin/plugins/versions", get(plugins_page))
        .route(
            "/admin/plugins/registry/{organisation}/{plugin}",
            get(listing_page),
        )
        .route(
            "/admin/plugins/registry/{organisation}/{plugin}/install",
            get(install_sheet),
        )
        .route("/admin/plugins/registry/install", post(install))
        .route(
            "/admin/plugins/registry/update",
            get(update_sheet).post(update),
        )
        .route(
            "/admin/plugins/registry/acknowledge",
            get(acknowledge_sheet).post(acknowledge),
        )
        .route("/admin/plugins/registry/pull", post(pull_now))
        .route("/admin/plugins/registry/usage", post(set_usage))
}

// ── Reading and asking, as the person ───────────────────────────────────

async fn ask<R: Message + Default>(
    app: &App,
    subject: &str,
    topic: &str,
    request_type: &str,
    request: impl Message,
    wait: Duration,
) -> Result<R, String> {
    let (_, bytes) = app
        .bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(wait),
            subject,
        )
        .await
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => meridian_bus::read_refusal(&detail)
                .map_or(detail.clone(), |(_, words)| words.to_string()),
            other => other.to_string(),
        })?;
    R::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

/// Browse's copy, as the conductor keeps it.
pub async fn listings(
    app: &App,
    subject: &str,
    request: ListRegistryPluginsRequest,
) -> Result<ListRegistryPluginsReply, String> {
    let wait = if request.pull_now { PULLING } else { READING };
    ask(
        app,
        subject,
        REGISTRY_PLUGINS,
        "meridian.v1.ListRegistryPluginsRequest",
        request,
        wait,
    )
    .await
}

/// The Registries tab's reports, setting and catalogue age.
pub async fn reports(
    app: &App,
    subject: &str,
    cursor: &str,
) -> Result<ListPluginUsageReportsReply, String> {
    ask(
        app,
        subject,
        LIST_PLUGIN_USAGE_REPORTS,
        "meridian.v1.ListPluginUsageReportsRequest",
        ListPluginUsageReportsRequest {
            page_size: 50,
            cursor: cursor.into(),
        },
        READING,
    )
    .await
}

// ── The words ───────────────────────────────────────────────────────────

/// A source registry's label; community labelled "Unsupervised" wherever
/// it appears (the spec's requirement 52).
pub fn registry_label(registry: &str) -> &'static str {
    match registry {
        "community" => "Community · Unsupervised",
        "verified" => "Verified",
        _ => "Local",
    }
}

/// A pull's reason's kind, as deployments are told it.
pub fn pull_kind(pull: &PluginPull) -> &'static str {
    match PullReasonKind::try_from(pull.kind) {
        Ok(PullReasonKind::CopyrightNotice) => "a copyright notice",
        _ => "a breach of the Platform Terms",
    }
}

/// A count shown only once three or more deployments contribute.
pub fn installs(listing: &PluginListing) -> String {
    if listing.counts_shown {
        listing.installs.to_string()
    } else {
        "Fewer than three deployments".into()
    }
}

/// A listed version's floor against this deployment's sidecar.
pub fn floor_mark(version: &ListedVersion) -> (bool, String) {
    match meridian_sidecar::contract::admit(&version.schema_version) {
        Ok(_) => (true, format!("{} · served here", version.schema_version)),
        Err(why) => (
            false,
            format!("{} · not served here: {why}", version.schema_version),
        ),
    }
}

fn offered(version: &ListedVersion) -> bool {
    version.withdrawn_at_ns == 0
        && version
            .pull
            .as_ref()
            .is_none_or(|pull| pull.restored_at_ns != 0)
}

/// The version Browse shows for a listing: the highest offered release,
/// verified first, or failing that the highest offered pre-release.
pub fn latest(listing: &PluginListing) -> Option<&ListedVersion> {
    let order = |a: &&ListedVersion, b: &&ListedVersion| {
        meridian_domain::semver::semver_cmp(&a.version, &b.version)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    for registry in ["verified", "community"] {
        let live: Vec<&ListedVersion> = listing
            .versions
            .iter()
            .filter(|v| v.registry == registry && offered(v))
            .collect();
        let releases: Vec<&ListedVersion> = live
            .iter()
            .copied()
            .filter(|v| !meridian_domain::semver::is_prerelease(&v.version))
            .collect();
        if let Some(best) = releases
            .iter()
            .copied()
            .max_by(order)
            .or_else(|| live.iter().copied().max_by(order))
        {
            return Some(best);
        }
    }
    None
}

fn roles_of(version: &ListedVersion) -> Vec<String> {
    if version.registry == "verified" && !version.approved_roles.is_empty() {
        version.approved_roles.clone()
    } else {
        version
            .metadata
            .as_ref()
            .map(|m| m.roles.clone())
            .unwrap_or_default()
    }
}

/// An instance's pull warning, where its version is pulled and no deployment
/// admin has kept it since (W8.12): one line.
pub fn pull_warning(
    launch: &PluginLaunch,
    version: &PluginVersion,
    listings: &[PluginListing],
) -> Option<PluginPull> {
    if version.listing.is_empty() {
        return None;
    }
    let pull = listings
        .iter()
        .find(|l| l.listing == version.listing)?
        .versions
        .iter()
        .find(|v| v.version == launch.version && v.registry == version.source_registry)?
        .pull
        .clone()
        .filter(|pull| pull.restored_at_ns == 0)?;
    let kept = launch
        .pull_acknowledgement
        .as_ref()
        .and_then(|ack| ack.pull.as_ref())
        .is_some_and(|acked| acked.pulled_at_ns == pull.pulled_at_ns);
    (!kept).then_some(pull)
}

/// The newest version an instance's source registry offers above what it
/// runs; none for a local one, or one up to date.
pub fn update_for<'a>(
    version: &PluginVersion,
    running: &str,
    listings: &'a [PluginListing],
) -> Option<&'a ListedVersion> {
    if version.listing.is_empty() {
        return None;
    }
    let listing = listings.iter().find(|l| l.listing == version.listing)?;
    let running_pre = meridian_domain::semver::is_prerelease(running);
    listing
        .versions
        .iter()
        .filter(|v| v.registry == version.source_registry && offered(v))
        .filter(|v| running_pre || !meridian_domain::semver::is_prerelease(&v.version))
        .filter(|v| {
            meridian_domain::semver::semver_cmp(&v.version, running)
                == Some(std::cmp::Ordering::Greater)
        })
        .max_by(|a, b| {
            meridian_domain::semver::semver_cmp(&a.version, &b.version)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Whether a version asks for a role the instance does not hold now.
pub fn widening(running: &[String], wanted: &[String]) -> bool {
    wanted.iter().any(|role| !running.contains(role))
}

// ── Pages ───────────────────────────────────────────────────────────────

fn chrome<'a>(session: &'a Session, here: &str) -> crate::html::Chrome<'a> {
    crate::html::Chrome {
        crumbs: format!(
            "{}{}{}",
            crate::html::crumb_link("/admin", "Settings"),
            crate::html::crumb_link(PLUGINS_PATH, "Plugins"),
            crate::html::crumb_here(here, None)
        ),
        ..crate::admin::admin_chrome(session)
    }
}

fn tabs(here: &str) -> String {
    let links: String = [
        ("installed", "Installed"),
        ("browse", "Browse"),
        ("registries", "Registries"),
    ]
    .iter()
    .map(|(key, title)| {
        format!(
            "<a href=\"{PLUGINS_PATH}?tab={key}\" data-tab=\"{key}\"{on}>{title}</a>",
            on = if *key == here {
                " class=\"here\" aria-current=\"page\""
            } else {
                ""
            }
        )
    })
    .collect();
    format!("<nav class=\"tabs plugins-tabs\" aria-label=\"Plugins\">{links}</nav>")
}

fn notice_line(said: &str) -> String {
    if said.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\" role=\"status\">{}</p>", escape(said))
    }
}

fn error_line(said: &str) -> String {
    format!("<p class=\"error\" role=\"alert\">{}</p>", escape(said))
}

/// `GET /admin/plugins/versions?tab=`: the Plugins page.
async fn plugins_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let tab = asked.get("tab").map(String::as_str).unwrap_or("installed");
    let said = asked.get("done").map(String::as_str).unwrap_or_default();
    let failed = asked.get("failed").map(String::as_str).unwrap_or_default();
    let body = match tab {
        "browse" => browse(&app, &session, &asked).await,
        "registries" => registries_tab(&app, &session, &asked).await,
        _ => installed(&app, &session).await,
    };
    let page = format!(
        "<section class=\"admin-section plugins-page\"><div class=\"section-head\"><div><h1>Plugins</h1></div></div>\
         {tabs}{notice}{failure}{body}</section>",
        tabs = tabs(tab),
        notice = notice_line(said),
        failure = if failed.is_empty() { String::new() } else { error_line(failed) },
    );
    Html(crate::html::page_with(
        "Plugins",
        &page,
        &chrome(&session, "Plugins"),
    ))
    .into_response()
}

async fn installed(app: &App, session: &Session) -> String {
    let catalogue: PluginCatalogue = match crate::catalogue::catalogue(app).await {
        Ok(catalogue) => catalogue,
        Err(failed) => return error_line(&format!("The catalogue could not be read: {failed}")),
    };
    let listings = listings(
        app,
        &session.subject,
        ListRegistryPluginsRequest {
            page_size: 200,
            ..Default::default()
        },
    )
    .await
    .map(|reply| reply.listings)
    .unwrap_or_default();
    let rows: Vec<String> = catalogue
        .versions
        .iter()
        .map(|v| {
            let m = v.metadata.clone().unwrap_or_default();
            let state = if v.retired_at_ns > 0 { "retired" } else { "listed" };
            let sections = m.declaration.as_ref().map_or(0, |d| d.documentation.len());
            // Each instance it runs, with its update and its pull warning.
            let instances: String = catalogue
                .launches
                .iter()
                .filter(|l| l.state == PluginLaunchState::Launched as i32 && l.name == m.name && l.version == m.version)
                .map(|launch| {
                    let mut said = vec![format!("<code>{}</code>", escape(&launch.instance_id))];
                    if let Some(newer) = update_for(v, &launch.version, &listings) {
                        let wants = roles_of(newer);
                        let href = format!(
                            "/admin/plugins/registry/update?instance={}&version={}",
                            escape(&launch.instance_id),
                            escape(&newer.version)
                        );
                        said.push(format!(
                            "<a class=\"button\" href=\"{href}\">Update to {}</a>{}",
                            escape(&newer.version),
                            if widening(&launch.roles, &wants) {
                                " <span class=\"badge warn\">Needs approval</span>"
                            } else {
                                ""
                            }
                        ));
                    }
                    if let Some(pull) = pull_warning(launch, v, &listings) {
                        said.push(format!(
                            "<span class=\"badge danger\" data-pulled>Pulled: {}</span> \
                             <a class=\"button\" href=\"/admin/plugins/registry/acknowledge?instance={}\">Keep or stop</a>",
                            pull_kind(&pull),
                            escape(&launch.instance_id)
                        ));
                    }
                    said.join(" ")
                })
                .collect::<Vec<_>>()
                .join("; ");
            let key_age = if v.key_status_issued_at_ns > 0 {
                format!(
                    " <span class=\"faint\">keys as of {}</span>",
                    &crate::terminal::rfc3339(v.key_status_issued_at_ns)[..10]
                )
            } else {
                String::new()
            };
            format!(
                "<tr data-version=\"{name}@{version}\"><td><a href=\"{href}\">{name}</a></td>\
                 <td>{version}</td><td>{registry}{key_age}</td><td class=\"wide\">{sections}</td>\
                 <td>{instances}</td><td class=\"wide\">{state}</td></tr>",
                href = escape(&crate::documentation::version_path(&m.name, &m.version)),
                name = escape(&m.name),
                version = escape(&m.version),
                registry = registry_label(&v.source_registry),
                instances = if instances.is_empty() { "<span class=\"faint\">none</span>".into() } else { instances },
            )
        })
        .collect();
    let table = if rows.is_empty() {
        "<p class=\"hint\">No version is installed yet: `meridian plugin upload` adds a local one, Browse a listed one.</p>".to_string()
    } else {
        format!(
            "<om-pager search=\"Search versions\"><table class=\"list one-line versions\"><thead><tr>\
             <th>Plugin</th><th>Version</th><th>Source</th><th class=\"wide\">Sections</th><th>Instances</th>\
             <th class=\"wide\">State</th></tr></thead><tbody>{}</tbody></table></om-pager>",
            rows.concat()
        )
    };
    format!("{}{table}", crate::notice::folded())
}

async fn browse(app: &App, session: &Session, asked: &HashMap<String, String>) -> String {
    let field = |name: &str| asked.get(name).cloned().unwrap_or_default();
    let reply = match listings(
        app,
        &session.subject,
        ListRegistryPluginsRequest {
            search: field("search"),
            registry: field("registry"),
            role: field("role"),
            page_size: 200,
            ..Default::default()
        },
    )
    .await
    {
        Ok(reply) => reply,
        Err(failed) => return format!("{}{}", crate::notice::folded(), error_line(&failed)),
    };
    let filter = format!(
        "<form class=\"browse-filter\" method=\"get\" action=\"{PLUGINS_PATH}\"><input type=\"hidden\" name=\"tab\" value=\"browse\">\
         <input type=\"search\" name=\"search\" value=\"{search}\" placeholder=\"Search listings\" aria-label=\"Search listings\">\
         <select name=\"registry\" aria-label=\"Registry\"><option value=\"\">Any registry</option>\
         <option value=\"community\"{c}>Community · Unsupervised</option><option value=\"verified\"{v}>Verified</option></select>\
         <input type=\"text\" name=\"role\" value=\"{role}\" placeholder=\"Role\" aria-label=\"Role\">\
         <button type=\"submit\">Find</button></form>",
        search = escape(&field("search")),
        role = escape(&field("role")),
        c = if field("registry") == "community" { " selected" } else { "" },
        v = if field("registry") == "verified" { " selected" } else { "" },
    );
    let rows: Vec<String> = reply
        .listings
        .iter()
        .map(|listing| {
            let best = latest(listing);
            let (floor_ok, floor) = best.map(floor_mark).unwrap_or((true, String::new()));
            format!(
                "<tr data-listing=\"{name}\"><td><a href=\"{href}\">{name}</a></td><td>{label}</td>\
                 <td>{version}</td><td class=\"wide\">{roles}</td><td class=\"wide{floor_class}\">{floor}</td>\
                 <td class=\"wide\">{stars}</td><td class=\"wide\">{installs}</td></tr>",
                name = escape(&listing.listing),
                href = escape(&listing_path(&listing.listing)),
                label = best.map_or("", |v| registry_label(&v.registry)),
                version = escape(best.map_or("", |v| v.version.as_str())),
                roles = escape(&best.map(roles_of).unwrap_or_default().join(", ")),
                floor_class = if floor_ok { "" } else { " refused" },
                floor = escape(&floor),
                stars = listing.stars,
                installs = escape(&installs(listing)),
            )
        })
        .collect();
    let pulled = if reply.catalogue_pulled_at_ns > 0 {
        format!(
            "Catalogue pulled {}.",
            &crate::terminal::rfc3339(reply.catalogue_pulled_at_ns)[..16].replace('T', " ")
        )
    } else {
        "The catalogue has not been pulled yet.".into()
    };
    let table = if rows.is_empty() {
        format!("<p class=\"hint\">No listing matches. {pulled}</p>")
    } else {
        format!(
            "<om-pager search=\"Filter these\"><table class=\"list one-line listings\"><thead><tr>\
             <th>Listing</th><th>Registry</th><th>Latest</th><th class=\"wide\">Roles</th>\
             <th class=\"wide\">Floor</th><th class=\"wide\">Stars</th><th class=\"wide\">Installs</th>\
             </tr></thead><tbody>{}</tbody></table></om-pager><p class=\"hint\">{pulled}</p>",
            rows.concat()
        )
    };
    format!("{}{filter}{table}", crate::notice::folded())
}

async fn registries_tab(app: &App, session: &Session, asked: &HashMap<String, String>) -> String {
    let cursor = asked.get("cursor").map(String::as_str).unwrap_or_default();
    let held = match reports(app, &session.subject, cursor).await {
        Ok(held) => held,
        Err(failed) => return error_line(&failed),
    };
    let token = crate::admin::token_input(session);
    let pulled = if held.catalogue_pulled_at_ns > 0 {
        crate::terminal::rfc3339(held.catalogue_pulled_at_ns)[..16].replace('T', " ")
    } else {
        "never".into()
    };
    let rows: Vec<String> = held
        .reports
        .iter()
        .map(|sent| {
            let report = sent.report.clone().unwrap_or_default();
            format!(
                "<tr><td>{sent}</td><td>{start} to {end}</td><td>{versions}</td><td>{received}</td></tr>",
                sent = &crate::terminal::rfc3339(sent.sent_at_ns)[..10],
                start = escape(&report.period_start),
                end = escape(&report.period_end),
                versions = report.counts.len(),
                received = if sent.received { "received" } else { "not received" },
            )
        })
        .collect();
    let list = if rows.is_empty() {
        "<p class=\"hint\">No report has been sent.</p>".to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line reports\"><thead><tr><th>Sent</th><th>Week</th>\
             <th>Versions</th><th>Platform</th></tr></thead><tbody>{}</tbody></table></om-pager>",
            rows.concat()
        )
    };
    format!(
        "<div class=\"registries\"><form method=\"post\" action=\"/admin/plugins/registry/pull\" class=\"row\">{token}\
         <p>The catalogue was last pulled: <strong data-pulled-at>{pulled}</strong></p>\
         <button type=\"submit\">Pull now</button></form>\
         <form method=\"post\" action=\"/admin/plugins/registry/usage\" class=\"row usage\">{token}\
         <label><input type=\"checkbox\" name=\"reports_plugin_usage\" value=\"on\"{checked}> Report plugin usage</label>\
         <input type=\"text\" name=\"reason\" placeholder=\"Why\" aria-label=\"Why\" maxlength=\"2000\">\
         <button type=\"submit\">Save</button></form>\
         <p class=\"hint\">Weekly, for each installed community or verified version: its instances, how many people \
         hold each level on it, and its daily operations and requests. No person, account, instrument or local \
         plugin is named.</p>{list}</div>",
        checked = if held.reports_plugin_usage { " checked" } else { "" },
    )
}

/// `GET /admin/plugins/registry/{organisation}/{plugin}?tab=`.
async fn listing_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, plugin)): Path<(String, String)>,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{plugin}");
    let reply = listings(
        &app,
        &session.subject,
        ListRegistryPluginsRequest {
            listing: name.clone(),
            ..Default::default()
        },
    )
    .await;
    let Some(listing) = reply
        .ok()
        .and_then(|reply| reply.listings.into_iter().next())
    else {
        return (
            StatusCode::NOT_FOUND,
            Html(crate::html::page_with(
                "No such listing",
                &error_line(&format!(
                    "No listing {name} is in the catalogue the conductor holds."
                )),
                &chrome(&session, &name),
            )),
        )
            .into_response();
    };
    let tab = asked.get("tab").map(String::as_str).unwrap_or("overview");
    let chosen = asked
        .get("version")
        .and_then(|asked| listing.versions.iter().find(|v| &v.version == asked))
        .or_else(|| latest(&listing))
        .or(listing.versions.first());
    let base = listing_path(&name);
    let nav: String = [
        ("overview", "Overview"),
        ("documentation", "Documentation"),
        ("versions", "Versions"),
    ]
    .iter()
    .map(|(key, title)| {
        format!(
            "<a href=\"{base}?tab={key}\"{on}>{title}</a>",
            base = escape(&base),
            on = if *key == tab {
                " class=\"here\" aria-current=\"page\""
            } else {
                ""
            }
        )
    })
    .collect();
    let body = match tab {
        "documentation" => match chosen {
            Some(version) => listed_documentation(&name, version),
            None => "<p class=\"hint\">It lists no version.</p>".into(),
        },
        "versions" => versions_tab(&listing),
        _ => overview(&listing, chosen),
    };
    let install = chosen
        .filter(|v| offered(v))
        .map(|v| {
            format!(
                "<a class=\"button primary\" href=\"{base}/install?version={version}&amp;registry={registry}\">Install {version}</a>",
                base = escape(&base),
                version = escape(&v.version),
                registry = escape(&v.registry),
            )
        })
        .unwrap_or_default();
    let page = format!(
        "<section class=\"admin-section listing-page\"><div class=\"section-head\"><div><h1>{name}</h1></div>{install}</div>\
         <nav class=\"tabs\" aria-label=\"The listing\">{nav}</nav>{body}</section>",
        name = escape(&name),
    );
    let page = crate::html::page_with(&name, &page, &chrome(&session, &name));
    if tab == "documentation" {
        crate::documentation::with_policy(page)
    } else {
        Html(page).into_response()
    }
}

fn overview(listing: &PluginListing, chosen: Option<&ListedVersion>) -> String {
    let build = chosen.and_then(|v| v.build.clone()).unwrap_or_default();
    let floor = chosen
        .map(floor_mark)
        .map(|(_, said)| said)
        .unwrap_or_default();
    let vendor = format!(
        "<dl class=\"facts vendor\" aria-label=\"Stated by its vendor\"><dt>Regions</dt><dd>{}</dd>\
         <dt>Asset classes</dt><dd>{}</dd>{}</dl><p class=\"hint\">Stated by its vendor.</p>",
        escape(&listing.regions.join(", ")),
        escape(&listing.asset_classes.join(", ")),
        if listing.support_contact.is_empty() {
            String::new()
        } else {
            format!("<dt>Support</dt><dd>{}</dd>", escape(&listing.support_contact))
        }
    );
    format!(
        "<div class=\"listing-overview\">{notice}<dl class=\"facts\"><dt>Registry</dt><dd>{registry}</dd>\
         <dt>Repository</dt><dd>{repository}</dd><dt>Build</dt><dd>{tag} at <code>{commit}</code>, signed by \
         <code>{signer}</code></dd><dt>Floor</dt><dd>{floor}</dd><dt>Stars</dt><dd>{stars}</dd>\
         <dt>Installs</dt><dd>{installs}</dd></dl>{vendor}</div>",
        notice = crate::notice::full(),
        registry = chosen.map_or("", |v| registry_label(&v.registry)),
        repository = escape(&listing.repository),
        tag = escape(&build.tag),
        commit = escape(build.commit.get(..12).unwrap_or(&build.commit)),
        signer = escape(&build.signer_fingerprint),
        floor = escape(&floor),
        stars = listing.stars,
        installs = escape(&installs(listing)),
    )
}

/// A listed version's documentation before it is installed: the catalogue
/// carries its declaration, drawn as the Documentation tab's Reference; its
/// sections' texts arrive with the install, verified (W8.10).
fn listed_documentation(name: &str, version: &ListedVersion) -> String {
    let metadata = version.metadata.clone().unwrap_or_default();
    let reply = meridian_domain::v1::ReadPluginDocumentationReply {
        declaration: metadata.declaration.clone(),
        documentation: Vec::new(),
        uploaded_by: String::new(),
        uploaded_at_ns: version.listed_at_ns,
    };
    let roles = roles_of(version);
    let base = format!(
        "{}?tab=documentation&version={}",
        listing_path(name),
        version.version
    );
    let href = |section: &str, part: &str| crate::documentation::section_href(&base, section, part);
    let sections = metadata
        .declaration
        .as_ref()
        .map(|d| {
            d.documentation
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!(
        "<p class=\"hint\">Its sections ({}) are read once it is installed, verified against the digests below; \
         its Reference is its declaration.</p>{}",
        if sections.is_empty() { "none".to_string() } else { escape(&sections) },
        crate::documentation::drawn(&crate::documentation::Page {
            name,
            version: &version.version,
            roles: &roles,
            reply: &reply,
            note: "",
            section: Some(crate::documentation::REFERENCE),
            part: None,
            href: &href,
        })
    )
}

fn versions_tab(listing: &PluginListing) -> String {
    let mut versions: Vec<&ListedVersion> = listing.versions.iter().collect();
    versions.sort_by(|a, b| {
        meridian_domain::semver::semver_cmp(&b.version, &a.version)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let rows: String = versions
        .iter()
        .map(|v| {
            let state = if v.withdrawn_at_ns != 0 {
                "withdrawn by its vendor".to_string()
            } else if let Some(pull) = v.pull.as_ref().filter(|p| p.restored_at_ns == 0) {
                format!("pulled: {}", pull_kind(pull))
            } else {
                "offered".to_string()
            };
            format!(
                "<tr data-version=\"{version}\"><td>{version}</td><td>{label}</td><td class=\"wide\">{listed}</td><td>{state}</td></tr>",
                version = escape(&v.version),
                label = registry_label(&v.registry),
                listed = if v.listed_at_ns > 0 { crate::terminal::rfc3339(v.listed_at_ns)[..10].to_string() } else { String::new() },
            )
        })
        .collect();
    format!(
        "<om-pager><table class=\"list one-line listed-versions\"><thead><tr><th>Version</th><th>Registry</th>\
         <th class=\"wide\">Listed</th><th>State</th></tr></thead><tbody>{rows}</tbody></table></om-pager>"
    )
}

// ── The sheets and their acts ───────────────────────────────────────────

fn sheet(title: &str, body: &str, action: &str, fields: &str, submit: &str, token: &str) -> String {
    format!(
        "<section class=\"sheet-dialog\" aria-labelledby=\"sheet-title\"><h1 id=\"sheet-title\">{title}</h1>\
         {notice}{body}<form method=\"post\" action=\"{action}\" class=\"sheet-act\">{token}{fields}\
         <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{PLUGINS_PATH}\">Cancel</a>\
         <button type=\"submit\" class=\"primary\">{submit}</button></div></form></section>",
        title = escape(title),
        notice = crate::notice::full(),
    )
}

fn hidden(name: &str, value: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
        escape(value)
    )
}

/// `GET .../install?version=&registry=`: the install sheet.
async fn install_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, plugin)): Path<(String, String)>,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{plugin}");
    let wanted = asked.get("version").cloned().unwrap_or_default();
    let listing = listings(
        &app,
        &session.subject,
        ListRegistryPluginsRequest {
            listing: name.clone(),
            ..Default::default()
        },
    )
    .await
    .ok()
    .and_then(|reply| reply.listings.into_iter().next());
    let Some(version) = listing
        .as_ref()
        .and_then(|l| l.versions.iter().find(|v| v.version == wanted))
        .cloned()
    else {
        return Redirect::to(&format!("{PLUGINS_PATH}?tab=browse&failed=No+such+version"))
            .into_response();
    };
    let usage = reports(&app, &session.subject, "")
        .await
        .map(|r| r.reports_plugin_usage)
        .unwrap_or(true);
    let (floor_ok, floor) = floor_mark(&version);
    let roles = roles_of(&version);
    let token = crate::admin::token_input(&session);
    let body = format!(
        "<dl class=\"facts\"><dt>Version</dt><dd>{v} · {label}</dd><dt>Roles</dt><dd>{roles}</dd>\
         <dt>Floor</dt><dd class=\"{floor_class}\">{floor}</dd><dt>Verification</dt><dd>made at the install: its \
         listing statement, provenance and signer against the platform's keys this deployment carries</dd>\
         <dt>Usage report</dt><dd>{usage}, set on the Registries tab</dd></dl>",
        v = escape(&version.version),
        label = registry_label(&version.registry),
        roles = escape(&roles.join(", ")),
        floor_class = if floor_ok { "" } else { "refused" },
        floor = escape(&floor),
        usage = if usage { "on" } else { "off" },
    );
    let mut page = sheet(
        &format!("Install {name} {}", version.version),
        &body,
        "/admin/plugins/registry/install",
        &format!(
            "{}{}{}",
            hidden("listing", &name),
            hidden("version", &version.version),
            hidden("registry", &version.registry)
        ),
        "Install",
        &token,
    );
    // An instance running a local version of the same plugin may move to
    // this listing instead (the spec's requirement 39).
    if let Ok(catalogue) = crate::catalogue::catalogue(&app).await {
        let moving: Vec<&PluginLaunch> = catalogue
            .launches
            .iter()
            .filter(|l| l.state == PluginLaunchState::Launched as i32 && l.name == plugin)
            .collect();
        for launch in moving {
            page.push_str(&format!(
                "<form method=\"post\" action=\"/admin/plugins/registry/update\" class=\"sheet-act move\">{token}\
                 {i}{l}{v}{roles}<p>{instance} runs the local {plugin} {running}.</p>\
                 <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
                 <button type=\"submit\">Move to this listing</button></form>",
                i = hidden("instance_id", &launch.instance_id),
                l = hidden("listing", &name),
                v = hidden("version", &version.version),
                roles = roles.iter().map(|r| hidden("approved_role", r)).collect::<String>(),
                instance = escape(&launch.instance_id),
                plugin = escape(&plugin),
                running = escape(&launch.version),
            ));
        }
    }
    Html(crate::html::page_with(
        "Install",
        &page,
        &chrome(&session, "Install"),
    ))
    .into_response()
}

fn back(done: Result<String, String>, tab: &str) -> Response {
    let mut url =
        reqwest::Url::parse("http://dashboard.invalid/admin/plugins/versions").expect("fixed");
    url.query_pairs_mut().append_pair("tab", tab);
    match done {
        Ok(said) => url.query_pairs_mut().append_pair("done", &said),
        Err(failed) => url.query_pairs_mut().append_pair("failed", &failed),
    };
    Redirect::to(&format!(
        "{}?{}",
        url.path(),
        url.query().unwrap_or_default()
    ))
    .into_response()
}

type Fields = Vec<(String, String)>;

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim())
        .unwrap_or_default()
}

fn token_ok(session: &Session, fields: &Fields) -> bool {
    field(fields, "form_token") == session.form_token
}

/// `POST /admin/plugins/registry/install`: InstallPlugin.
async fn install(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if !token_ok(&session, &fields) {
        return back(
            Err("this form did not come from your session".into()),
            "installed",
        );
    }
    let installed: Result<PluginVersion, String> = ask(
        &app,
        &session.subject,
        INSTALL_PLUGIN,
        "meridian.v1.InstallPluginRequest",
        InstallPluginRequest {
            listing: field(&fields, "listing").into(),
            version: field(&fields, "version").into(),
            registry: field(&fields, "registry").into(),
            note: field(&fields, "note").into(),
        },
        INSTALLING,
    )
    .await;
    back(
        installed.map(|v| {
            let m = v.metadata.unwrap_or_default();
            format!(
                "{} {} is installed, verified. Launch it to run it.",
                m.name, m.version
            )
        }),
        "installed",
    )
}

/// `GET /admin/plugins/registry/update?instance=&version=`: the update
/// sheet, the difference and the notice.
async fn update_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let instance = asked.get("instance").cloned().unwrap_or_default();
    let wanted = asked.get("version").cloned().unwrap_or_default();
    let Ok(catalogue) = crate::catalogue::catalogue(&app).await else {
        return back(Err("the catalogue could not be read".into()), "installed");
    };
    let Some(launch) = catalogue
        .launches
        .iter()
        .find(|l| l.instance_id == instance && l.state == PluginLaunchState::Launched as i32)
    else {
        return back(Err(format!("no launch of {instance} is live")), "installed");
    };
    let listing_name = catalogue
        .versions
        .iter()
        .filter_map(|v| v.metadata.as_ref().map(|m| (m, v)))
        .find(|(m, _)| m.name == launch.name && m.version == launch.version)
        .map(|(_, v)| v.listing.clone())
        .unwrap_or_default();
    let listing = listings(
        &app,
        &session.subject,
        ListRegistryPluginsRequest {
            listing: listing_name.clone(),
            ..Default::default()
        },
    )
    .await
    .ok()
    .and_then(|reply| reply.listings.into_iter().next());
    let Some(target) = listing
        .as_ref()
        .and_then(|l| l.versions.iter().find(|v| v.version == wanted))
    else {
        return back(
            Err(format!("{listing_name} lists no version {wanted}")),
            "installed",
        );
    };
    let wants = roles_of(target);
    let token = crate::admin::token_input(&session);
    let body = format!(
        "<dl class=\"facts\"><dt>Instance</dt><dd><code>{instance}</code></dd><dt>From</dt><dd>{from}</dd>\
         <dt>To</dt><dd>{to} · {label}</dd><dt>Roles now</dt><dd>{now}</dd><dt>Roles asked</dt><dd>{asked}</dd></dl>{widening}",
        instance = escape(&instance),
        from = escape(&launch.version),
        to = escape(&target.version),
        label = registry_label(&target.registry),
        now = escape(&launch.roles.join(", ")),
        asked = escape(&wants.join(", ")),
        widening = if widening(&launch.roles, &wants) {
            "<p class=\"warn\" data-widening>Needs approval: it asks for more than the instance holds. Approving moves it.</p>"
        } else {
            ""
        },
    );
    let page = sheet(
        &format!("Update {instance} to {}", target.version),
        &body,
        "/admin/plugins/registry/update",
        &format!(
            "{}{}{}{}",
            hidden("instance_id", &instance),
            hidden("listing", &listing_name),
            hidden("version", &target.version),
            wants
                .iter()
                .map(|r| hidden("approved_role", r))
                .collect::<String>()
        ),
        if widening(&launch.roles, &wants) {
            "Approve and update"
        } else {
            "Update"
        },
        &token,
    );
    Html(crate::html::page_with(
        "Update",
        &page,
        &chrome(&session, "Update"),
    ))
    .into_response()
}

/// `POST /admin/plugins/registry/update`: UpdatePlugin.
async fn update(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if !token_ok(&session, &fields) {
        return back(
            Err("this form did not come from your session".into()),
            "installed",
        );
    }
    let moved: Result<PluginLaunch, String> = ask(
        &app,
        &session.subject,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        UpdatePluginRequest {
            instance_id: field(&fields, "instance_id").into(),
            listing: field(&fields, "listing").into(),
            version: field(&fields, "version").into(),
            approved_roles: fields
                .iter()
                .filter(|(key, _)| key == "approved_role")
                .map(|(_, value)| value.clone())
                .collect(),
            note: field(&fields, "note").into(),
        },
        INSTALLING,
    )
    .await;
    back(
        moved.map(|l| format!("{} runs {} {}.", l.instance_id, l.name, l.version)),
        "installed",
    )
}

/// `GET /admin/plugins/registry/acknowledge?instance=`: keep or stop.
async fn acknowledge_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let instance = asked.get("instance").cloned().unwrap_or_default();
    let token = crate::admin::token_input(&session);
    let body = format!(
        "<p>The version <code>{i}</code> runs was pulled from the catalogue. Keep it running, with a note \
         saying why, or stop it from its Manage page; either is its own record.</p>",
        i = escape(&instance)
    );
    let page = sheet(
        &format!("Keep {instance}"),
        &body,
        "/admin/plugins/registry/acknowledge",
        &hidden("instance_id", &instance),
        "Keep it",
        &token,
    );
    Html(crate::html::page_with(
        "Keep",
        &page,
        &chrome(&session, "Keep"),
    ))
    .into_response()
}

/// `POST /admin/plugins/registry/acknowledge`: AcknowledgePull.
async fn acknowledge(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if !token_ok(&session, &fields) {
        return back(
            Err("this form did not come from your session".into()),
            "installed",
        );
    }
    let kept: Result<PullAcknowledgement, String> = ask(
        &app,
        &session.subject,
        ACKNOWLEDGE_PULL,
        "meridian.v1.AcknowledgePullRequest",
        AcknowledgePullRequest {
            instance_id: field(&fields, "instance_id").into(),
            note: field(&fields, "note").into(),
        },
        READING,
    )
    .await;
    back(
        kept.map(|_| format!("{} is kept.", field(&fields, "instance_id"))),
        "installed",
    )
}

/// `POST /admin/plugins/registry/pull`: "Pull now".
async fn pull_now(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if !token_ok(&session, &fields) {
        return back(
            Err("this form did not come from your session".into()),
            "registries",
        );
    }
    let pulled = listings(
        &app,
        &session.subject,
        ListRegistryPluginsRequest {
            pull_now: true,
            page_size: 1,
            ..Default::default()
        },
    )
    .await;
    back(
        pulled.map(|_| "The catalogue was pulled.".to_string()),
        "registries",
    )
}

/// `POST /admin/plugins/registry/usage`: SetReportsPluginUsage.
async fn set_usage(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if !token_ok(&session, &fields) {
        return back(
            Err("this form did not come from your session".into()),
            "registries",
        );
    }
    let on = field(&fields, "reports_plugin_usage") == "on";
    let set: Result<ReportsPluginUsageSetting, String> = ask(
        &app,
        &session.subject,
        SET_REPORTS_PLUGIN_USAGE,
        "meridian.v1.SetReportsPluginUsageRequest",
        SetReportsPluginUsageRequest {
            reports_plugin_usage: on,
            reason: field(&fields, "reason").into(),
        },
        READING,
    )
    .await;
    back(
        set.map(|s| {
            if s.reports_plugin_usage {
                "Plugin usage is reported.".to_string()
            } else {
                "Plugin usage is not reported.".to_string()
            }
        }),
        "registries",
    )
}

#[cfg(test)]
mod tests;
