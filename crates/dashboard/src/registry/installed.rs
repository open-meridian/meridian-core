//! The Plugins page's Installed tab and a plugin's Versions page (ruling
//! 305, the product owner, 2026-10-18; on existing operations, no contract
//! change).
//!
//! **Installed** is one row per plugin: its name, the version each instance
//! runs (or "not launched"), the newest version kept or offered, how many
//! other versions are kept, and its state; sorted by name, with Browse's one
//! filter bar. **A plugin's Versions** lists every version kept, SemVer
//! newest first (SemVer 2.0 precedence, the product owner, 2026-10-18), each
//! with the instances running it, its source (Local, Community or Verified),
//! whether this deployment can run it -- its contract floor against this
//! deployment's -- and Launch, Update to it and Retire, each at the
//! deployment admin's level as its tool is. The versions this deployment
//! cannot run are hidden behind "Show N versions this deployment can't run"
//! and, shown, say why and offer Retire, and "Retire these" retires them all
//! after a confirmation naming the count, one record per version; a version
//! an instance still runs is never hidden, and is flagged.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use meridian_domain::v1::{
    LaunchPluginRequest, ListRegistryPluginsRequest, ListedVersion, PluginCatalogue, PluginLaunch,
    PluginLaunchState, PluginListing, PluginVersion, RetirePluginVersionRequest,
};

use super::{
    ask, back, error_line, field, hidden, listings, pull_kind, pull_warning, registry_label,
    token_ok, update_for, widening, Fields, PLUGINS_PATH, READING,
};
use crate::html::escape;
use crate::session::Session;
use crate::web::App;

/// A plugin's Versions page: a listing's `organisation/plugin` one path
/// segment, its slash escaped (the route decodes it back).
pub fn versions_path(name: &str) -> String {
    format!(
        "{PLUGINS_PATH}/{}",
        name.replace('%', "%25").replace('/', "%2F")
    )
}

/// Newest first by SemVer 2.0 precedence; a version that is not SemVer after
/// every one that is, by its text.
pub fn newest_first(a: &str, b: &str) -> std::cmp::Ordering {
    match meridian_domain::semver::semver_cmp(b, a) {
        Some(order) => order,
        None => {
            let semver = |v: &str| meridian_domain::semver::semver_cmp(v, v).is_some();
            semver(b).cmp(&semver(a)).then_with(|| b.cmp(a))
        }
    }
}

/// Whether this deployment can run a version, and if not why: its contract
/// floor against this deployment's (W8.10). A version whose floor the
/// deployment holds no record of -- a local one -- is not refused here; its
/// sidecar admits it or not at launch.
pub fn runs_here(version: &PluginVersion, listings: &[PluginListing]) -> Result<(), String> {
    let Some(listed) = listed_of(version, listings) else {
        return Ok(());
    };
    match meridian_sidecar::contract::admit(&listed.schema_version) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "needs contract {}; this deployment runs v{}",
            if listed.schema_version.is_empty() {
                "none stated"
            } else {
                listed.schema_version.as_str()
            },
            meridian_sidecar::contract::CONTRACT_CURRENT
        )),
    }
}

fn listed_of<'a>(
    version: &PluginVersion,
    listings: &'a [PluginListing],
) -> Option<&'a ListedVersion> {
    if version.listing.is_empty() {
        return None;
    }
    let metadata = version.metadata.as_ref()?;
    listings
        .iter()
        .find(|l| l.listing == version.listing)?
        .versions
        .iter()
        .find(|v| v.version == metadata.version && v.registry == version.source_registry)
}

fn live(catalogue: &PluginCatalogue, name: &str) -> Vec<PluginLaunch> {
    let mut out: Vec<PluginLaunch> = catalogue
        .launches
        .iter()
        .filter(|l| l.state == PluginLaunchState::Launched as i32 && l.name == name)
        .cloned()
        .collect();
    out.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    out
}

/// Each plugin's kept versions, by name, newest first; retired ones apart.
fn by_plugin(catalogue: &PluginCatalogue) -> BTreeMap<String, Vec<&PluginVersion>> {
    let mut out: BTreeMap<String, Vec<&PluginVersion>> = BTreeMap::new();
    for version in &catalogue.versions {
        let name = version
            .metadata
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_default();
        out.entry(name).or_default().push(version);
    }
    for versions in out.values_mut() {
        versions.sort_by(|a, b| {
            newest_first(
                &a.metadata.clone().unwrap_or_default().version,
                &b.metadata.clone().unwrap_or_default().version,
            )
        });
    }
    out
}

fn version_of(version: &PluginVersion) -> String {
    version
        .metadata
        .as_ref()
        .map(|m| m.version.clone())
        .unwrap_or_default()
}

/// The one filter bar Browse and Installed share: the search grows, then
/// the selects, then Find; below 40rem the search keeps its line and the
/// selects share the next (the platform's v25.5 pattern).
pub fn filter_bar(tab: &str, search: &str, registry: &str, role: &str, roles: &[String]) -> String {
    let option = |value: &str, said: &str, chosen: &str| {
        format!(
            "<option value=\"{v}\"{s}>{t}</option>",
            v = escape(value),
            s = if value == chosen { " selected" } else { "" },
            t = escape(said)
        )
    };
    let registries: String = [
        ("", "Any registry"),
        ("community", "Community"),
        ("verified", "Verified"),
    ]
    .iter()
    .chain(if tab == "installed" {
        [("local", "Local")].iter()
    } else {
        [].iter()
    })
    .map(|(value, said)| option(value, said, registry))
    .collect();
    let role_options: String = std::iter::once(option("", "Any role", role))
        .chain(roles.iter().map(|r| option(r, r, role)))
        .collect();
    format!(
        "<form class=\"filter-bar\" method=\"get\" action=\"{PLUGINS_PATH}\" role=\"search\">\
         <input type=\"hidden\" name=\"tab\" value=\"{tab}\">\
         <input class=\"grow\" type=\"search\" name=\"search\" value=\"{search}\" placeholder=\"Search plugins\" aria-label=\"Search plugins\">\
         <span class=\"filter-selects\"><select name=\"registry\" aria-label=\"Registry\">{registries}</select>\
         <select name=\"role\" aria-label=\"Role\">{role_options}</select></span>\
         <button type=\"submit\">Find</button></form>",
        tab = escape(tab),
        search = escape(search),
    )
}

/// `?tab=installed`: one row per plugin.
pub(super) async fn installed(
    app: &App,
    session: &Session,
    asked: &HashMap<String, String>,
) -> String {
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
    let asked_for = |name: &str| asked.get(name).cloned().unwrap_or_default();
    let (search, registry, role) = (
        asked_for("search"),
        asked_for("registry"),
        asked_for("role"),
    );
    let grouped = by_plugin(&catalogue);
    let mut roles: Vec<String> = catalogue
        .versions
        .iter()
        .filter_map(|v| v.metadata.as_ref())
        .flat_map(|m| m.roles.clone())
        .collect();
    roles.sort();
    roles.dedup();
    let rows: Vec<String> = grouped
        .iter()
        .filter(|(name, versions)| {
            (search.is_empty() || name.to_lowercase().contains(&search.to_lowercase()))
                && (registry.is_empty()
                    || versions.iter().any(|v| {
                        let source = if v.source_registry.is_empty() {
                            "local"
                        } else {
                            v.source_registry.as_str()
                        };
                        source == registry
                    }))
                && (role.is_empty()
                    || versions
                        .iter()
                        .filter_map(|v| v.metadata.as_ref())
                        .any(|m| m.roles.contains(&role)))
        })
        .map(|(name, versions)| plugin_row(name, versions, &catalogue, &listings))
        .collect();
    let bar = filter_bar("installed", &search, &registry, &role, &roles);
    if rows.is_empty() {
        let none = if grouped.is_empty() {
            "No plugin is installed yet: `meridian plugin upload` adds a local one, Browse a listed one."
        } else {
            "No plugin matches."
        };
        return format!("{bar}<p class=\"hint\">{none}</p>");
    }
    format!(
        "{bar}<om-pager><table class=\"list plugin-table installed\"><thead><tr>\
         <th>Name</th><th>Running</th><th class=\"mid\">Newest</th><th class=\"wide\">Other versions</th>\
         <th>State</th></tr></thead><tbody>{}</tbody></table></om-pager>",
        rows.concat()
    )
}

fn plugin_row(
    name: &str,
    versions: &[&PluginVersion],
    catalogue: &PluginCatalogue,
    listings: &[PluginListing],
) -> String {
    let kept: Vec<&&PluginVersion> = versions.iter().filter(|v| v.retired_at_ns == 0).collect();
    let launches = live(catalogue, name);
    let running: Vec<String> = launches.iter().map(|l| l.version.clone()).collect();
    let running_said = if launches.is_empty() {
        "<span class=\"faint\">not launched</span>".to_string()
    } else {
        launches
            .iter()
            .map(|l| {
                format!(
                    "<span title=\"{i}\">{v}</span>",
                    i = escape(&l.instance_id),
                    v = escape(&l.version)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    // The newest kept or offered: a local version, or its registry's.
    let mut candidates: Vec<String> = kept.iter().map(|v| version_of(v)).collect();
    for version in &kept {
        if let Some(listing) = listings.iter().find(|l| l.listing == version.listing) {
            candidates.extend(
                listing
                    .versions
                    .iter()
                    .filter(|v| v.registry == version.source_registry && super::offered(v))
                    .map(|v| v.version.clone()),
            );
        }
    }
    candidates.sort_by(|a, b| newest_first(a, b));
    candidates.dedup();
    let newest = candidates.first().cloned().unwrap_or_default();
    let others = kept
        .iter()
        .filter(|v| {
            let version = version_of(v);
            !running.contains(&version) && version != newest
        })
        .count();
    let pulled = launches.iter().any(|launch| {
        versions
            .iter()
            .any(|v| version_of(v) == launch.version && pull_warning(launch, v, listings).is_some())
    });
    let state = if pulled {
        "<span class=\"badge danger\" data-pulled>Pulled</span>"
    } else if !launches.is_empty() {
        "running"
    } else if kept.is_empty() {
        "retired"
    } else {
        "installed"
    };
    // Ruling 306's Name: the newest kept version's interface title, else
    // the plugin's name; the whole name its tooltip.
    let mut newest_kept: Vec<&&PluginVersion> = kept.clone();
    newest_kept.sort_by(|a, b| newest_first(&version_of(a), &version_of(b)));
    let title = newest_kept
        .first()
        .and_then(|v| v.metadata.as_ref())
        .and_then(|m| m.declaration.as_ref())
        .and_then(|d| d.interface.as_ref())
        .map(|i| i.title.trim().to_string())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            name.split_once('/')
                .map_or(name, |(_, plugin)| plugin)
                .to_string()
        });
    format!(
        "<tr data-plugin=\"{n}\"><td class=\"keep\"><a href=\"{href}\" title=\"{n}\">{shown}</a></td><td>{running_said}</td>\
         <td class=\"mid\">{newest}</td><td class=\"wide\">{others}</td><td>{state}</td></tr>",
        n = escape(name),
        shown = escape(&title),
        href = escape(&versions_path(name)),
        newest = escape(&newest),
    )
}

/// `GET /admin/plugins/versions/{name}?show=all`: a plugin's Versions.
pub(super) async fn versions_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let catalogue: PluginCatalogue = match crate::catalogue::catalogue(&app).await {
        Ok(catalogue) => catalogue,
        Err(failed) => {
            return back(
                Err(format!("the catalogue could not be read: {failed}")),
                "installed",
            )
        }
    };
    let listings = listings(
        &app,
        &session.subject,
        ListRegistryPluginsRequest {
            page_size: 200,
            ..Default::default()
        },
    )
    .await
    .map(|reply| reply.listings)
    .unwrap_or_default();
    let grouped = by_plugin(&catalogue);
    let versions: Vec<&PluginVersion> = grouped
        .get(&name)
        .map(|versions| {
            versions
                .iter()
                .copied()
                .filter(|v| v.retired_at_ns == 0)
                .collect()
        })
        .unwrap_or_default();
    let retired = grouped
        .get(&name)
        .map(|versions| versions.iter().filter(|v| v.retired_at_ns != 0).count())
        .unwrap_or(0);
    let launches = live(&catalogue, &name);
    let show_all = asked.get("show").is_some_and(|show| show == "all");
    let token = crate::admin::token_input(&session);
    let kept: Vec<String> = versions.iter().map(|v| version_of(v)).collect();
    let mut hidden_versions: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for version in &versions {
        let v = version_of(version);
        let instances: Vec<&PluginLaunch> = launches.iter().filter(|l| l.version == v).collect();
        let runs = runs_here(version, &listings);
        if runs.is_err() && instances.is_empty() {
            hidden_versions.push(v.clone());
            if !show_all {
                continue;
            }
        }
        rows.push(version_row(
            &name, version, &instances, &launches, &runs, &listings, &kept, &token,
        ));
    }
    let said = asked.get("done").map(String::as_str).unwrap_or_default();
    let failed = asked.get("failed").map(String::as_str).unwrap_or_default();
    let hidden_line = match (hidden_versions.len(), show_all) {
        (0, _) => String::new(),
        (n, false) => format!(
            "<p class=\"hint\"><a href=\"{href}?show=all\" data-show-unrunnable>Show {n} version{s} this deployment can't run</a></p>",
            href = escape(&versions_path(&name)),
            s = if n == 1 { "" } else { "s" },
        ),
        (n, true) => format!(
            "<p class=\"hint\"><a href=\"{href}\">Hide the {n} version{s} this deployment can't run</a> · \
             <a class=\"button\" href=\"{href}/retire-these\" data-retire-these>Retire these</a></p>",
            href = escape(&versions_path(&name)),
            s = if n == 1 { "" } else { "s" },
        ),
    };
    let table = if rows.is_empty() {
        "<p class=\"hint\">No version of it runs here or is kept that this deployment can run.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager><table class=\"list plugin-table plugin-versions\"><thead><tr><th>Version</th>\
             <th>Instances</th><th class=\"mid\">Source</th><th class=\"wide\">Runs here</th><th>Act</th></tr></thead>\
             <tbody>{}</tbody></table></om-pager>",
            rows.concat()
        )
    };
    let page = format!(
        "<section class=\"admin-section plugin-versions-page\"><div class=\"section-head\"><div><h1>{n}</h1>\
         <p class=\"hint\">{count} version{s} kept{retired}.</p></div></div>{notice}{failure}{hidden_line}{table}</section>",
        n = escape(&name),
        count = versions.len(),
        s = if versions.len() == 1 { "" } else { "s" },
        retired = if retired > 0 { format!(", {retired} retired") } else { String::new() },
        notice = if said.is_empty() { String::new() } else { format!("<p class=\"passed\" role=\"status\">{}</p>", escape(said)) },
        failure = if failed.is_empty() { String::new() } else { error_line(failed) },
    );
    Html(crate::html::page_with(
        &name,
        &page,
        &super::chrome(&session, &name),
    ))
    .into_response()
}

#[allow(clippy::too_many_arguments)]
fn version_row(
    name: &str,
    version: &PluginVersion,
    instances: &[&PluginLaunch],
    launches: &[PluginLaunch],
    runs: &Result<(), String>,
    listings: &[PluginListing],
    kept: &[String],
    token: &str,
) -> String {
    let v = version_of(version);
    let instances_said = if instances.is_empty() {
        "<span class=\"faint\">none</span>".to_string()
    } else {
        instances
            .iter()
            .map(|l| format!("<code>{}</code>", escape(&l.instance_id)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let runs_said = match runs {
        Ok(()) => "yes".to_string(),
        Err(why) if !instances.is_empty() => format!(
            "<span class=\"badge warn\" data-runs-anyway title=\"{w}\">no: {w}</span>",
            w = escape(why)
        ),
        Err(why) => format!("<span class=\"faint\">no: {}</span>", escape(why)),
    };
    let mut acts = Vec::new();
    if runs.is_ok() {
        acts.push(format!(
            "<a class=\"button\" href=\"{base}/launch?version={v}\">Launch</a>",
            base = escape(&versions_path(name)),
            v = escape(&v),
        ));
    }
    // Update to it: each instance of the plugin on another version of the
    // same registry, where this one is newer (W8.11).
    for launch in launches.iter().filter(|l| l.version != v) {
        let running = PluginVersion {
            listing: version.listing.clone(),
            source_registry: version.source_registry.clone(),
            ..Default::default()
        };
        if update_for(&running, &launch.version, listings).is_some_and(|newer| newer.version == v) {
            acts.push(format!(
                "<a class=\"button\" href=\"/admin/plugins/registry/update?instance={i}&amp;version={v}\">Update {i} to it</a>",
                i = escape(&launch.instance_id),
                v = escape(&v),
            ));
        }
    }
    // An instance on this version, where its registry offers a newer one
    // not yet kept here: Update installs it and moves the instance (W8.11).
    for launch in instances {
        if let Some(newer) =
            update_for(version, &v, listings).filter(|n| !kept.contains(&n.version))
        {
            acts.push(format!(
                "<a class=\"button\" href=\"/admin/plugins/registry/update?instance={i}&amp;version={n}\">Update {i} to {n}</a>{w}",
                i = escape(&launch.instance_id),
                n = escape(&newer.version),
                w = if widening(&launch.roles, &super::roles_of(newer)) {
                    " <span class=\"badge warn\">Needs approval</span>"
                } else {
                    ""
                },
            ));
        }
        // A pulled version's instance: keep it or stop it (W8.12).
        if pull_warning(launch, version, listings).is_some() {
            acts.push(format!(
                "<a class=\"button\" href=\"/admin/plugins/registry/acknowledge?instance={}\">Keep or stop</a>",
                escape(&launch.instance_id)
            ));
        }
    }
    if instances.is_empty() {
        acts.push(format!(
            "<form method=\"post\" action=\"{PLUGINS_PATH}/retire\" class=\"inline-act\" \
             data-confirm=\"Retire {said} {sv}? Nothing can launch it again until it is uploaded again.\">{token}\
             {n}{ver}<button type=\"submit\">Retire</button></form>",
            n = hidden("name", name),
            ver = hidden("version", &v),
            said = escape(name),
            sv = escape(&v),
        ));
    }
    let wants = version
        .metadata
        .as_ref()
        .map(|m| m.roles.clone())
        .unwrap_or_default();
    let widens = instances.iter().any(|l| widening(&l.roles, &wants));
    let pulled = listed_of(version, listings)
        .and_then(|listed| listed.pull.as_ref().filter(|p| p.restored_at_ns == 0))
        .map(|pull| {
            format!(
                " <span class=\"badge danger\" data-pulled title=\"Pulled: {k}\" aria-label=\"Pulled: {k}\">Pulled</span>",
                k = super::pull_kind(pull)
            )
        })
        .unwrap_or_default();
    // A version this deployment cannot run, said short beside it, its whole
    // reason in its title and in the wider column.
    let cannot = match runs {
        Err(why) => format!(
            " <span class=\"badge\" title=\"{w}\">{short}</span>",
            w = escape(why),
            short = escape(
                why.split(';')
                    .next()
                    .unwrap_or("can't run")
                    .replace("contract ", "")
                    .trim()
            ),
        ),
        Ok(()) => String::new(),
    };
    // One act inline; more in the row's menu, so the row stays one line.
    let acts = match acts.len() {
        0 => String::new(),
        1 => acts.remove(0),
        _ => format!(
            "<details class=\"menu row-acts\"><summary aria-label=\"Act on {v}\">Act</summary>\
             <div class=\"menu-pop\">{}</div></details>",
            acts.join(""),
            v = escape(&v),
        ),
    };
    format!(
        "<tr data-version=\"{v}\"><td class=\"keep\"><a href=\"{doc}\">{v}</a>{w}{pulled}{cannot}</td><td>{instances_said}</td>\
         <td class=\"mid\"><span class=\"pill\">{source}</span>{key_age}</td><td class=\"wide\">{runs_said}</td>\
         <td class=\"actions\">{acts}</td></tr>",
        v = escape(&v),
        doc = escape(&crate::documentation::version_path(name, &v)),
        w = if widens { " <span class=\"badge warn\" title=\"Needs approval: it asks a role more\">Needs approval</span>" } else { "" },
        source = registry_label(&version.source_registry),
        key_age = if version.key_status_issued_at_ns > 0 {
            format!(
                " <span class=\"faint\">keys as of {}</span>",
                &crate::terminal::rfc3339(version.key_status_issued_at_ns)[..10]
            )
        } else {
            String::new()
        },
    )
}

/// `GET /admin/plugins/versions/{name}/launch?version=`: the launch sheet.
pub(super) async fn launch_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let wanted = asked.get("version").cloned().unwrap_or_default();
    let Ok(catalogue) = crate::catalogue::catalogue(&app).await else {
        return back(Err("the catalogue could not be read".into()), "installed");
    };
    let Some(version) = catalogue.versions.iter().find(|v| {
        v.metadata
            .as_ref()
            .is_some_and(|m| m.name == name && m.version == wanted)
    }) else {
        return back(
            Err(format!("{name} {wanted} is not installed")),
            "installed",
        );
    };
    let roles = version
        .metadata
        .as_ref()
        .map(|m| m.roles.clone())
        .unwrap_or_default();
    let token = crate::admin::token_input(&session);
    let taken: Vec<&str> = catalogue
        .launches
        .iter()
        .map(|l| l.instance_id.as_str())
        .collect();
    let suggested = (1..)
        .map(|n| format!("{name}-{n}"))
        .find(|id| !taken.contains(&id.as_str()))
        .unwrap_or_default();
    let page = format!(
        "<section class=\"sheet-dialog\" aria-labelledby=\"sheet-title\"><h1 id=\"sheet-title\">Launch {n} {v}</h1>\
         <dl class=\"facts\"><dt>Source</dt><dd>{source}</dd><dt>Roles approved</dt><dd>{r}</dd></dl>\
         <form method=\"post\" action=\"{PLUGINS_PATH}/launch\" class=\"sheet-act\">{token}{hn}{hv}{hr}\
         <label>Instance <input type=\"text\" name=\"instance_id\" required maxlength=\"63\" value=\"{suggested}\"></label>\
         <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{back}\">Cancel</a>\
         <button type=\"submit\" class=\"primary\">Launch</button></div></form></section>",
        n = escape(&name),
        v = escape(&wanted),
        source = registry_label(&version.source_registry),
        r = escape(&roles.join(", ")),
        hn = hidden("name", &name),
        hv = hidden("version", &wanted),
        hr = roles.iter().map(|r| hidden("approved_role", r)).collect::<String>(),
        suggested = escape(&suggested),
        back = escape(&versions_path(&name)),
    );
    Html(crate::html::page_with(
        "Launch",
        &page,
        &super::chrome(&session, "Launch"),
    ))
    .into_response()
}

fn to_versions(name: &str, done: Result<String, String>) -> Response {
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("fixed");
    url.set_path(&versions_path(name));
    match done {
        Ok(said) => url.query_pairs_mut().append_pair("done", &said),
        Err(failed) => url.query_pairs_mut().append_pair("failed", &failed),
    };
    axum::response::Redirect::to(&format!(
        "{}?{}",
        url.path(),
        url.query().unwrap_or_default()
    ))
    .into_response()
}

/// `POST /admin/plugins/versions/launch`: LaunchPlugin, as the launch tool
/// sends it (W8.3).
pub(super) async fn launch(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = field(&fields, "name").to_string();
    if !token_ok(&session, &fields) {
        return to_versions(
            &name,
            Err("this form did not come from your session".into()),
        );
    }
    let launched: Result<PluginLaunch, String> = ask(
        &app,
        &session.subject,
        crate::catalogue::LAUNCH_PLUGIN,
        "meridian.v1.LaunchPluginRequest",
        LaunchPluginRequest {
            name: name.clone(),
            version: field(&fields, "version").into(),
            instance_id: field(&fields, "instance_id").into(),
            approved_roles: fields
                .iter()
                .filter(|(key, _)| key == "approved_role")
                .map(|(_, value)| value.clone())
                .collect(),
            live: false,
            note: field(&fields, "note").into(),
        },
        std::time::Duration::from_secs(40),
    )
    .await;
    to_versions(
        &name,
        launched.map(|l| format!("{} runs {} {}.", l.instance_id, l.name, l.version)),
    )
}

/// `POST /admin/plugins/versions/retire`: RetirePluginVersion (W8.15), its
/// own record.
pub(super) async fn retire(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = field(&fields, "name").to_string();
    if !token_ok(&session, &fields) {
        return to_versions(
            &name,
            Err("this form did not come from your session".into()),
        );
    }
    let version = field(&fields, "version").to_string();
    let retired = retire_one(&app, &session, &name, &version, field(&fields, "note")).await;
    to_versions(
        &name,
        retired.map(|()| format!("{name} {version} is retired.")),
    )
}

async fn retire_one(
    app: &App,
    session: &Session,
    name: &str,
    version: &str,
    note: &str,
) -> Result<(), String> {
    ask::<PluginVersion>(
        app,
        &session.subject,
        crate::catalogue::RETIRE_PLUGIN_VERSION,
        "meridian.v1.RetirePluginVersionRequest",
        RetirePluginVersionRequest {
            name: name.into(),
            version: version.into(),
            note: note.into(),
        },
        READING * 3,
    )
    .await
    .map(|_| ())
}

/// The versions of `name` this deployment cannot run that no instance runs.
async fn unrunnable(app: &App, session: &Session, name: &str) -> Result<Vec<String>, String> {
    let catalogue = crate::catalogue::catalogue(app).await?;
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
    let launches = live(&catalogue, name);
    Ok(by_plugin(&catalogue)
        .get(name)
        .map(|versions| {
            versions
                .iter()
                .filter(|v| v.retired_at_ns == 0)
                .filter(|v| !launches.iter().any(|l| l.version == version_of(v)))
                .filter(|v| runs_here(v, &listings).is_err())
                .map(|v| version_of(v))
                .collect()
        })
        .unwrap_or_default())
}

/// `GET /admin/plugins/versions/{name}/retire-these`: the confirmation,
/// naming the count.
pub(super) async fn retire_these_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let versions = match unrunnable(&app, &session, &name).await {
        Ok(versions) => versions,
        Err(failed) => return to_versions(&name, Err(failed)),
    };
    let n = versions.len();
    let token = crate::admin::token_input(&session);
    let page = format!(
        "<section class=\"sheet-dialog\" aria-labelledby=\"sheet-title\"><h1 id=\"sheet-title\">Retire {n} version{s} of {name}?</h1>\
         <p>Each is one this deployment can't run and no instance runs: {list}. Each retirement is its own record.</p>\
         <form method=\"post\" action=\"{PLUGINS_PATH}/retire-these\" class=\"sheet-act\">{token}{hn}{hc}\
         <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{back}?show=all\">Cancel</a>\
         <button type=\"submit\" class=\"primary\">Retire these {n}</button></div></form></section>",
        s = if n == 1 { "" } else { "s" },
        name = escape(&name),
        list = escape(&versions.join(", ")),
        hn = hidden("name", &name),
        hc = hidden("count", &n.to_string()),
        back = escape(&versions_path(&name)),
    );
    Html(crate::html::page_with(
        "Retire",
        &page,
        &super::chrome(&session, "Retire"),
    ))
    .into_response()
}

/// `POST /admin/plugins/versions/retire-these`: each version this
/// deployment cannot run retired, one record each; refused where the count
/// confirmed is no longer the count.
pub(super) async fn retire_these(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = field(&fields, "name").to_string();
    if !token_ok(&session, &fields) {
        return to_versions(
            &name,
            Err("this form did not come from your session".into()),
        );
    }
    let versions = match unrunnable(&app, &session, &name).await {
        Ok(versions) => versions,
        Err(failed) => return to_versions(&name, Err(failed)),
    };
    if field(&fields, "count") != versions.len().to_string() {
        return to_versions(
            &name,
            Err(format!(
                "the versions changed since you confirmed: {} now; confirm again",
                versions.len()
            )),
        );
    }
    let note = field(&fields, "note").to_string();
    let mut retired = 0;
    for version in &versions {
        if let Err(failed) = retire_one(&app, &session, &name, version, &note).await {
            return to_versions(
                &name,
                Err(format!(
                    "{retired} retired; {version} was refused: {failed}"
                )),
            );
        }
        retired += 1;
    }
    to_versions(
        &name,
        Ok(format!("{retired} versions of {name} are retired.")),
    )
}

/// A pulled version's words, for a line that names one.
pub fn pulled_said(listing: &ListedVersion) -> Option<String> {
    listing
        .pull
        .as_ref()
        .filter(|p| p.restored_at_ns == 0)
        .map(|p| format!("pulled: {}", pull_kind(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_newest_first_by_semver_with_a_prerelease_below_its_release() {
        let mut versions = vec!["0.2.0", "0.10.0", "0.11.0", "0.11.0-rc.1", "0.9.9", "dev"];
        versions.sort_by(|a, b| newest_first(a, b));
        assert_eq!(
            versions,
            vec!["0.11.0", "0.11.0-rc.1", "0.10.0", "0.9.9", "0.2.0", "dev"]
        );
    }

    #[test]
    fn the_catalogue_lists_each_plugins_versions_newest_first() {
        let version = |name: &str, v: &str| PluginVersion {
            metadata: Some(meridian_domain::v1::PluginMetadata {
                name: name.into(),
                version: v.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut held = PluginCatalogue {
            versions: vec![
                version("alpaca", "0.10.0"),
                version("alpaca", "0.2.0"),
                version("snaptrade", "0.18.0"),
                version("alpaca", "0.11.0"),
                version("alpaca", "0.11.0-rc.2"),
            ],
            launches: Vec::new(),
        };
        crate::catalogue::sort_versions(&mut held);
        let said: Vec<String> = held
            .versions
            .iter()
            .map(|v| {
                let m = v.metadata.clone().unwrap();
                format!("{} {}", m.name, m.version)
            })
            .collect();
        assert_eq!(
            said,
            vec![
                "alpaca 0.11.0",
                "alpaca 0.11.0-rc.2",
                "alpaca 0.10.0",
                "alpaca 0.2.0",
                "snaptrade 0.18.0"
            ]
        );
    }

    #[test]
    fn a_registry_version_whose_floor_this_deployment_does_not_serve_says_why() {
        let version = |schema: &str| PluginListing {
            listing: "open-meridian/fx".into(),
            versions: vec![ListedVersion {
                listing: "open-meridian/fx".into(),
                version: "1.0.0".into(),
                registry: "community".into(),
                schema_version: schema.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let installed = PluginVersion {
            metadata: Some(meridian_domain::v1::PluginMetadata {
                name: "fx".into(),
                version: "1.0.0".into(),
                ..Default::default()
            }),
            listing: "open-meridian/fx".into(),
            source_registry: "community".into(),
            ..Default::default()
        };
        assert!(runs_here(&installed, &[version("v25")]).is_ok());
        let why = runs_here(&installed, &[version("v99")]).unwrap_err();
        assert!(
            why.starts_with("needs contract v99; this deployment runs v"),
            "{why}"
        );
        // A local version: its floor not recorded here, never refused.
        let local = PluginVersion {
            listing: String::new(),
            ..installed
        };
        assert!(runs_here(&local, &[version("v99")]).is_ok());
    }
}
