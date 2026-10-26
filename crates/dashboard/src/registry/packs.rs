//! Starter packs in the dashboard (W8.18, W8.19; contract v27's wire, built
//! in v28; spec/starter-packs, requirements 16 to 27, 31 and 32), a
//! deployment admin's:
//!
//! - **Browse's pack view** (requirement 16): Browse's two-way switch,
//!   "Plugins" and "Starter packs", the packs one line each -- title, asset
//!   classes, members, and this deployment's state.
//! - **The install sheet** (requirement 17): one step per group in the
//!   curator's order, the data members ticked and the brokers and venues
//!   to choose, none preselected, each step paging at eight rows; the last
//!   step reviewing what will be installed and what is already, the Terms'
//!   one line once where a ticked member is community, the data sources'
//!   line, the note and Install.
//! - **The pack page** (requirements 19 to 23, 27), the setup checklist: a
//!   tab per group and "About", one line a member -- its versions, its
//!   state, its connection from that instance's "Setting up", and its next
//!   act; "Update all" and "Remove pack", each a sheet with a note.
//!
//! Every page reads what the conductor keeps; the platform is never reached
//! in a page's request. A pack's title and description are its curator's
//! words, data and never instructions.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_domain::v1::{
    AccessRecords, InstallPackRequest, ListRegistryPacksReply, ListRegistryPacksRequest,
    ListRegistryPluginsRequest, ListedVersion, PackInstall, PackMember, PackMemberState,
    PackMemberStatus, PackSelection, PackStatus, PluginListing, PluginPack, ReadPackRequest,
    RemovePackRequest, UpdatePackRequest,
};
use serde_json::{json, Value};

use super::{
    ask, chrome, error_line, field, floor_mark, hidden, notice_line, notice_read, registry_label,
    token_ok, Fields, INSTALLING, NOTICE_FIRST, NOTICE_READ, PLUGINS_PATH, PULLING, READING,
};
use crate::html::escape;
use crate::session::Session;
use crate::web::App;

pub const REGISTRY_PACKS: &str = "platform.config.query.registry-packs";
pub const INSTALL_PACK: &str = "platform.config.command.install-pack";
pub const READ_PACK: &str = "platform.config.query.read-pack";
pub const UPDATE_PACK: &str = "platform.config.command.update-pack";
pub const REMOVE_PACK: &str = "platform.config.command.remove-pack";

/// Rows a step of the install sheet shows before it pages (requirement 17).
const STEP_ROWS: usize = 8;

/// The data sources' line on the install sheet's last step (ruling 356):
/// a pack never accepts a member's terms.
pub const DATA_SOURCES_LINE: &str = "Each data source's terms are accepted on Data sources";

/// A pack's page in the deployment (requirement 19).
pub fn pack_path(pack: &str) -> String {
    format!("/admin/plugins/packs/{pack}")
}

/// Browse's pack view.
pub const PACKS_VIEW: &str = "/admin/plugins/versions?tab=browse&view=packs";

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/admin/plugins/packs/{organisation}/{pack}", get(pack_page))
        .route(
            "/admin/plugins/packs/{organisation}/{pack}/install",
            get(install_sheet),
        )
        .route(
            "/admin/plugins/packs/{organisation}/{pack}/update",
            get(update_sheet),
        )
        .route(
            "/admin/plugins/packs/{organisation}/{pack}/remove",
            get(remove_sheet),
        )
        .route("/admin/plugins/packs/install", post(install))
        .route("/admin/plugins/packs/update", post(update))
        .route("/admin/plugins/packs/remove", post(remove))
}

// ── Asking the conductor, as the person ─────────────────────────────────

/// Browse's pack view, from the conductor's copy of the catalogue.
pub async fn registry_packs(
    app: &App,
    subject: &str,
    request: ListRegistryPacksRequest,
) -> Result<ListRegistryPacksReply, String> {
    let wait = if request.pull_now { PULLING } else { READING };
    ask(
        app,
        subject,
        REGISTRY_PACKS,
        "meridian.v1.ListRegistryPacksRequest",
        request,
        wait,
    )
    .await
}

/// One pack from the copy; none where it is not offered (withdrawn, or not
/// in the catalogue).
pub async fn registry_pack(
    app: &App,
    subject: &str,
    pack: &str,
) -> Result<Option<PluginPack>, String> {
    Ok(registry_packs(
        app,
        subject,
        ListRegistryPacksRequest {
            pack: pack.to_string(),
            page_size: 1,
            ..Default::default()
        },
    )
    .await?
    .packs
    .into_iter()
    .next())
}

/// A pack's status as the conductor keeps it, without the setup steps.
pub async fn pack_status(app: &App, subject: &str, pack: &str) -> Result<PackStatus, String> {
    ask(
        app,
        subject,
        READ_PACK,
        "meridian.v1.ReadPackRequest",
        ReadPackRequest {
            pack: pack.to_string(),
        },
        READING,
    )
    .await
}

/// A launched member's connection, from its instance's "Setting up"
/// (requirement 19): "Set up", the next step's words and where it is done,
/// or "Not answering" where the instance has not reported.
pub async fn connection(
    app: &App,
    instance: &str,
    records: &AccessRecords,
    subject: &str,
) -> (String, Option<String>) {
    let answering = app
        .health
        .view()
        .get(instance)
        .is_some_and(|report| report.registered);
    if !answering {
        return ("Not answering".into(), None);
    }
    let steps = crate::admin::setting_up_steps(app, instance, records, subject, true).await;
    match steps.iter().find(|step| step.done == Some(false)) {
        Some(step) => (step.words.clone(), step.href.clone()),
        None => ("Set up".into(), None),
    }
}

/// The checklist (requirement 19): the pack's status with each launched
/// member's connection, and where its next step is done.
pub struct Checklist {
    pub status: PackStatus,
    /// By listing: where the next setup step is done.
    pub links: BTreeMap<String, String>,
}

pub async fn checklist(
    app: &App,
    subject: &str,
    pack: &str,
    records: &AccessRecords,
) -> Result<Checklist, String> {
    let mut status = pack_status(app, subject, pack).await?;
    let mut links = BTreeMap::new();
    for member in &mut status.members {
        if member.state != PackMemberState::Launched as i32 {
            continue;
        }
        if let Some(instance) = member.instance_ids.first() {
            let (words, href) = connection(app, instance, records, subject).await;
            member.setup = words;
            if let Some(href) = href {
                links.insert(member.listing.clone(), href);
            }
        }
    }
    Ok(Checklist { status, links })
}

// ── The words ───────────────────────────────────────────────────────────

/// A member's state as the pack page says it (requirement 19).
pub fn state_said(state: i32) -> &'static str {
    match PackMemberState::try_from(state) {
        Ok(PackMemberState::NotInstalled) => "Not installed",
        Ok(PackMemberState::Installing) => "Installing",
        Ok(PackMemberState::Failed) => "Failed",
        Ok(PackMemberState::Installed) => "Installed",
        Ok(PackMemberState::Launched) => "Launched",
        Ok(PackMemberState::Unavailable) => "Unavailable",
        Ok(PackMemberState::NotPermitted) => "Not permitted here",
        _ => "",
    }
}

/// A member's state as the terminal and the tools answer it.
pub fn state_key(state: i32) -> &'static str {
    match PackMemberState::try_from(state) {
        Ok(PackMemberState::NotInstalled) => "not_installed",
        Ok(PackMemberState::Installing) => "installing",
        Ok(PackMemberState::Failed) => "failed",
        Ok(PackMemberState::Installed) => "installed",
        Ok(PackMemberState::Launched) => "launched",
        Ok(PackMemberState::Unavailable) => "unavailable",
        Ok(PackMemberState::NotPermitted) => "not_permitted",
        _ => "unspecified",
    }
}

/// A member's selection as the tools answer it.
pub fn selection_key(selection: i32) -> &'static str {
    match PackSelection::try_from(selection) {
        Ok(PackSelection::Required) => "required",
        Ok(PackSelection::Preselected) => "preselected",
        Ok(PackSelection::Choose) => "choose",
        _ => "unspecified",
    }
}

/// This deployment's state of a pack, as Browse's line says it
/// (requirement 16): "Not installed", or "Installed: 7 of 9 members".
pub fn deployment_state(status: &PackStatus) -> String {
    if status.installed_revision == 0 {
        return "Not installed".into();
    }
    let current: Vec<&PackMemberStatus> = status
        .members
        .iter()
        .filter(|m| m.removed_by_revision == 0)
        .collect();
    let installed = current
        .iter()
        .filter(|m| {
            m.state == PackMemberState::Installed as i32
                || m.state == PackMemberState::Launched as i32
        })
        .count();
    format!("Installed: {installed} of {} members", current.len())
}

/// This deployment's state of a pack as a key, as the terminal and the tools
/// answer it beside its words in `said` (the CLI's queue, 2026-10-26):
/// `installed` or `not_installed`.
pub fn pack_state_key(status: &PackStatus) -> &'static str {
    if status.installed_revision > 0 {
        "installed"
    } else {
        "not_installed"
    }
}

/// A member's terms access basis (requirement 17, ruling 356): "Licensed"
/// where any dataset it declares is licensed, "Open" where every one is open
/// or public domain, nothing where it declares none.
pub fn access_basis(version: &ListedVersion) -> &'static str {
    use meridian_pb::v1::AccessBasis;
    let bases: Vec<i32> = version
        .metadata
        .as_ref()
        .and_then(|m| m.declaration.as_ref())
        .and_then(|d| d.catalogue.as_ref())
        .map(|c| {
            c.datasets
                .iter()
                .filter_map(|d| d.licence_default.as_ref().map(|l| l.access_basis))
                .collect()
        })
        .unwrap_or_default();
    if bases.contains(&(AccessBasis::Licensed as i32)) {
        "Licensed"
    } else if !bases.is_empty()
        && bases
            .iter()
            .all(|b| *b == AccessBasis::Open as i32 || *b == AccessBasis::PublicDomain as i32)
    {
        "Open"
    } else {
        ""
    }
}

fn members_of(pack: &PluginPack) -> &[PackMember] {
    pack.revision
        .as_ref()
        .map(|r| r.members.as_slice())
        .unwrap_or_default()
}

fn title_of(pack: &PluginPack) -> String {
    pack.revision
        .as_ref()
        .map(|r| r.title.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| pack.pack.clone())
}

/// The groups of a pack's members, in the curator's order (requirement 17).
fn groups(members: &[PackMember]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for member in members {
        if !seen.contains(&member.group) {
            seen.push(member.group.clone());
        }
    }
    seen
}

/// A tab's key for a group: its words, lower case, hyphenated.
fn group_key(group: &str) -> String {
    let key: String = group
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let key = key.trim_matches('-').to_string();
    if key.is_empty() {
        "members".into()
    } else {
        key
    }
}

/// The catalogue's listings by name, for each member's name, roles and
/// terms: one read of Browse's copy, and one more per member it missed.
async fn listings_of(
    app: &App,
    subject: &str,
    members: &[PackMember],
) -> BTreeMap<String, PluginListing> {
    let mut found: BTreeMap<String, PluginListing> = super::listings(
        app,
        subject,
        ListRegistryPluginsRequest {
            page_size: 200,
            ..Default::default()
        },
    )
    .await
    .map(|reply| {
        reply
            .listings
            .into_iter()
            .map(|l| (l.listing.clone(), l))
            .collect()
    })
    .unwrap_or_default();
    for member in members {
        if found.contains_key(&member.listing) {
            continue;
        }
        if let Ok(reply) = super::listings(
            app,
            subject,
            ListRegistryPluginsRequest {
                listing: member.listing.clone(),
                ..Default::default()
            },
        )
        .await
        {
            if let Some(listing) = reply.listings.into_iter().next() {
                found.insert(listing.listing.clone(), listing);
            }
        }
    }
    found
}

/// A member's name: its listing's display name, or its plugin part.
fn member_name(listing: Option<&PluginListing>, member: &str) -> String {
    listing.map(super::display_name).unwrap_or_else(|| {
        member
            .split_once('/')
            .map_or(member.to_string(), |(_, plugin)| plugin.to_string())
    })
}

fn version_in<'a>(
    listing: Option<&'a PluginListing>,
    version: &str,
    registry: &str,
) -> Option<&'a ListedVersion> {
    listing?
        .versions
        .iter()
        .find(|v| v.version == version && v.registry == registry)
}

// ── The JSON the tools and the terminal answer ──────────────────────────

/// A pack from the catalogue, as the tools answer it: its curator's words
/// marked as another's.
pub fn pack_json(pack: &PluginPack) -> Value {
    let revision = pack.revision.clone().unwrap_or_default();
    json!({
        "pack": pack.pack,
        "curator": crate::mcp::others_words(&pack.curator),
        "revision": revision.revision,
        "title": crate::mcp::others_words(&revision.title),
        "description": crate::mcp::others_words(&revision.description),
        "asset_classes": revision.asset_classes,
        "members": revision.members.iter().map(|m| json!({
            "listing": m.listing,
            "registry": m.registry,
            "selection": selection_key(m.selection),
            "group": crate::mcp::others_words(&m.group),
            "minimum_version": m.minimum_version,
        })).collect::<Vec<_>>(),
        "withdrawn_at_ns": pack.withdrawn_at_ns,
    })
}

/// One member's status, as the tools and `--json` answer it.
pub fn member_json(member: &PackMemberStatus) -> Value {
    json!({
        "listing": member.listing,
        "group": crate::mcp::others_words(&member.group),
        "installed_version": member.installed_version,
        "latest_version": member.latest_version,
        "state": state_key(member.state),
        "reason": member.reason,
        "instance_ids": member.instance_ids,
        "setup": member.setup,
        "added_by_revision": member.added_by_revision,
        "removed_by_revision": member.removed_by_revision,
    })
}

/// A pack's status, as the tools and `--json` answer it.
pub fn status_json(status: &PackStatus) -> Value {
    json!({
        "pack": status.pack,
        "installed_revision": status.installed_revision,
        "latest_revision": status.latest_revision,
        "withdrawn": status.withdrawn,
        "state": pack_state_key(status),
        "said": deployment_state(status),
        "members": status.members.iter().map(member_json).collect::<Vec<_>>(),
    })
}

/// The checklist, as `dashboard__read_pack` and the terminal's
/// `GET /terminal/packs/{org}/{pack}` both answer it: the status, with each
/// launched member's `setup_href` where its next step is done.
pub fn checklist_json(checklist: &Checklist) -> Value {
    let mut data = status_json(&checklist.status);
    if let Some(members) = data["members"].as_array_mut() {
        for member in members {
            let listing = member["listing"].as_str().unwrap_or_default().to_string();
            if let Some(href) = checklist.links.get(&listing) {
                member["setup_href"] = json!(href);
            }
        }
    }
    data
}

/// A pack install's answer, as the tools and `--json` answer it.
pub fn install_json(install: &PackInstall) -> Value {
    json!({
        "pack": install.pack,
        "revision": install.revision,
        "members": install.members.iter().map(member_json).collect::<Vec<_>>(),
    })
}

/// The members an install ticks, from a pack's selection and a person's
/// or an agent's choices (requirements 29 and 32): `members` whole where
/// given; otherwise the required and preselected, with `with` ticked and
/// `without` unticked. A name that is not a member is refused before
/// anything moves, by its path; a bare plugin name reads as the member
/// whose plugin part it is.
pub fn ticked(
    pack: &PluginPack,
    members: Option<&[String]>,
    with: &[String],
    without: &[String],
) -> Result<Vec<String>, (String, String)> {
    let all = members_of(pack);
    let named = |asked: &str, path: String| -> Result<String, (String, String)> {
        all.iter()
            .find(|m| {
                m.listing == asked
                    || m.listing
                        .split_once('/')
                        .is_some_and(|(_, plugin)| plugin == asked)
            })
            .map(|m| m.listing.clone())
            .ok_or_else(|| (path, format!("{asked} is not a member of {}", pack.pack)))
    };
    if let Some(members) = members {
        return members
            .iter()
            .enumerate()
            .map(|(n, asked)| named(asked, format!("members[{n}]")))
            .collect();
    }
    let with: Vec<String> = with
        .iter()
        .enumerate()
        .map(|(n, asked)| named(asked, format!("with[{n}]")))
        .collect::<Result<_, _>>()?;
    let without: Vec<String> = without
        .iter()
        .enumerate()
        .map(|(n, asked)| named(asked, format!("without[{n}]")))
        .collect::<Result<_, _>>()?;
    for (n, gone) in without.iter().enumerate() {
        if all
            .iter()
            .any(|m| &m.listing == gone && m.selection == PackSelection::Required as i32)
        {
            return Err((
                format!("without[{n}]"),
                format!("{gone} is required by {} and cannot be left out", pack.pack),
            ));
        }
    }
    Ok(all
        .iter()
        .filter(|m| {
            let default = m.selection == PackSelection::Required as i32
                || m.selection == PackSelection::Preselected as i32;
            (default || with.contains(&m.listing)) && !without.contains(&m.listing)
        })
        .map(|m| m.listing.clone())
        .collect())
}

// ── Browse's pack view ──────────────────────────────────────────────────

/// Browse's two-way switch (requirement 16).
pub fn switch(view: &str) -> String {
    let link = |key: &str, href: &str, said: &str| {
        format!(
            "<a href=\"{href}\" data-view=\"{key}\"{on}>{said}</a>",
            on = if key == view {
                " class=\"here\" aria-current=\"page\""
            } else {
                ""
            }
        )
    };
    format!(
        "<nav class=\"level-switch browse-switch\" aria-label=\"Browse\">{}{}</nav>",
        link("plugins", &format!("{PLUGINS_PATH}?tab=browse"), "Plugins"),
        link("packs", PACKS_VIEW, "Starter packs"),
    )
}

/// Browse's "Starter packs" view: one line a pack (requirement 16).
pub async fn view(app: &App, session: &Session, asked: &HashMap<String, String>) -> String {
    let search = asked.get("search").cloned().unwrap_or_default();
    let reply = match registry_packs(
        app,
        &session.subject,
        ListRegistryPacksRequest {
            search: search.clone(),
            page_size: 200,
            ..Default::default()
        },
    )
    .await
    {
        Ok(reply) => reply,
        Err(failed) => return error_line(&failed),
    };
    let mut rows = Vec::new();
    for pack in &reply.packs {
        let status = pack_status(app, &session.subject, &pack.pack).await.ok();
        let state = status.as_ref().map(deployment_state).unwrap_or_default();
        let href = if status.as_ref().is_some_and(|s| s.installed_revision > 0) {
            pack_path(&pack.pack)
        } else {
            format!("{}/install", pack_path(&pack.pack))
        };
        let revision = pack.revision.clone().unwrap_or_default();
        rows.push(format!(
            "<tr data-pack=\"{id}\"><td class=\"keep\"><a href=\"{href}\" title=\"{id}\">{title}</a></td>\
             <td class=\"mid\">{classes}</td><td>{members}</td><td data-state>{state}</td></tr>",
            id = escape(&pack.pack),
            href = escape(&href),
            title = escape(&title_of(pack)),
            classes = escape(&revision.asset_classes.join(", ")),
            members = revision.members.len(),
            state = escape(&state),
        ));
    }
    let bar = format!(
        "<form method=\"get\" action=\"{PLUGINS_PATH}\" class=\"filter-bar\" role=\"search\">\
         <input type=\"hidden\" name=\"tab\" value=\"browse\"><input type=\"hidden\" name=\"view\" value=\"packs\">\
         <input type=\"search\" class=\"grow\" name=\"search\" value=\"{s}\" placeholder=\"Search starter packs\" aria-label=\"Search starter packs\">\
         <button type=\"submit\">Search</button></form>",
        s = escape(&search)
    );
    let table = if rows.is_empty() {
        "<p class=\"hint\">No starter pack is in the catalogue this deployment holds.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line plugin-table packs\"><thead><tr><th>Title</th>\
             <th class=\"mid\">Asset classes</th><th>Members</th><th>State</th></tr></thead>\
             <tbody>{}</tbody></table></om-pager>",
            rows.concat()
        )
    };
    format!("{bar}{table}")
}

// ── The install sheet ───────────────────────────────────────────────────

fn sheet_page(session: &Session, title: &str, body: &str) -> Response {
    Html(crate::html::page_with(title, body, &chrome(session, title))).into_response()
}

fn to_pack(pack: &str, done: Result<String, String>) -> Response {
    let mut url = reqwest::Url::parse("http://dashboard.invalid/").expect("fixed");
    url.set_path(&pack_path(pack));
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

fn to_browse(failed: &str) -> Response {
    let mut url =
        reqwest::Url::parse("http://dashboard.invalid/admin/plugins/versions").expect("fixed");
    url.query_pairs_mut()
        .append_pair("tab", "browse")
        .append_pair("view", "packs")
        .append_pair("failed", failed);
    Redirect::to(&format!(
        "{}?{}",
        url.path(),
        url.query().unwrap_or_default()
    ))
    .into_response()
}

/// What the query string carries of the install sheet: the step, and the
/// members ticked so far (`member`, repeated), once the sheet was begun.
fn asked_ticks(query: &str) -> (usize, Option<Vec<String>>) {
    let pairs: Vec<(String, String)> = url_pairs(query);
    let step = pairs
        .iter()
        .find(|(k, _)| k == "step")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let begun = pairs.iter().any(|(k, _)| k == "ticked");
    let members: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "member")
        .map(|(_, v)| v.clone())
        .collect();
    (step, (begun || !members.is_empty()).then_some(members))
}

fn url_pairs(query: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://dashboard.invalid/?{query}"))
        .map(|url| {
            url.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a member can be ticked here (requirement 26): not unavailable,
/// and not refused by this deployment.
fn tickable(status: Option<&PackMemberStatus>) -> bool {
    status.is_none_or(|s| {
        s.state != PackMemberState::Unavailable as i32
            && s.state != PackMemberState::NotPermitted as i32
    })
}

/// `GET /admin/plugins/packs/{organisation}/{pack}/install?step=&member=`:
/// the install sheet, a step per group and then the review.
async fn install_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, pack)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{pack}");
    let held = match registry_pack(&app, &session.subject, &name).await {
        Ok(Some(held)) => held,
        Ok(None) => {
            return to_browse(&format!(
                "No starter pack {name} is offered in the catalogue"
            ))
        }
        Err(failed) => return to_browse(&failed),
    };
    let status = match pack_status(&app, &session.subject, &name).await {
        Ok(status) => status,
        Err(failed) => return to_browse(&failed),
    };
    let members = members_of(&held).to_vec();
    let listings = listings_of(&app, &session.subject, &members).await;
    let statuses: BTreeMap<&str, &PackMemberStatus> = status
        .members
        .iter()
        .map(|m| (m.listing.as_str(), m))
        .collect();
    let (step, asked) = asked_ticks(query.as_deref().unwrap_or_default());
    // Begun: what was ticked; first: the curator's defaults (339).
    let ticked: Vec<String> = asked.unwrap_or_else(|| {
        members
            .iter()
            .filter(|m| {
                (m.selection == PackSelection::Required as i32
                    || m.selection == PackSelection::Preselected as i32)
                    && tickable(statuses.get(m.listing.as_str()).copied())
            })
            .map(|m| m.listing.clone())
            .collect()
    });
    let groups = groups(&members);
    let step = step.min(groups.len());
    let title = format!("Install {}", title_of(&held));
    let base = format!("{}/install", pack_path(&name));
    let revision = held.revision.as_ref().map_or(0, |r| r.revision);
    // The steps, one line: the groups in the curator's order, then Review.
    let steps_said: Vec<String> = groups
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("Review"))
        .enumerate()
        .map(|(n, said)| {
            if n == step {
                format!("<strong aria-current=\"step\">{}</strong>", escape(said))
            } else {
                escape(said)
            }
        })
        .collect();
    let steps_nav = format!(
        "<p class=\"hint pack-steps\" aria-label=\"Steps\">Step {} of {}: {}</p>",
        step + 1,
        groups.len() + 1,
        steps_said.join(" · ")
    );
    let body = if step < groups.len() {
        let group = &groups[step];
        let rows: String = members
            .iter()
            .filter(|m| &m.group == group)
            .map(|m| {
                let listing = listings.get(&m.listing);
                let held_status = statuses.get(m.listing.as_str()).copied();
                let resolved = held_status
                    .map(|s| s.latest_version.as_str())
                    .unwrap_or_default();
                let version = version_in(listing, resolved, &m.registry);
                let can = tickable(held_status);
                let required = m.selection == PackSelection::Required as i32;
                let checked = ticked.contains(&m.listing) && can;
                let roles = version.map(super::roles_of).unwrap_or_default();
                let floor = version.map(|v| floor_mark(v).1).unwrap_or_default();
                // The version as resolved, or what keeps the member from
                // being installed, its reason beneath the table.
                let state = held_status
                    .filter(|s| s.state != PackMemberState::NotInstalled as i32)
                    .map(|s| state_said(s.state).to_string())
                    .unwrap_or_default();
                let why = held_status.map(|s| s.reason.as_str()).unwrap_or_default();
                let basis = version.map(access_basis).unwrap_or_default();
                format!(
                    "<tr data-member=\"{id}\"><td class=\"tick\"><input type=\"checkbox\" name=\"member\" value=\"{id}\" \
                     aria-label=\"Install {name}\"{checked}{disabled}>{fixed}</td>\
                     <td title=\"{id}\">{name}</td><td class=\"version\" title=\"{why}\">{said}</td>\
                     <td class=\"mid\">{roles}</td><td class=\"wide\">{floor}</td><td class=\"mid\">{registry}</td>\
                     <td class=\"wide\"><a href=\"{terms}\">{basis}</a></td></tr>",
                    id = escape(&m.listing),
                    name = escape(&member_name(listing, &m.listing)),
                    checked = if checked { " checked" } else { "" },
                    disabled = if !can || required { " disabled" } else { "" },
                    fixed = if required && can {
                        hidden("member", &m.listing)
                    } else {
                        String::new()
                    },
                    said = if state.is_empty() {
                        escape(resolved)
                    } else {
                        format!("<span data-member-state>{}</span>", escape(&state))
                    },
                    why = escape(why),
                    roles = escape(&roles.join(", ")),
                    floor = escape(&floor),
                    registry = registry_label(&m.registry),
                    terms = escape(&format!(
                        "{}?tab=documentation&section=terms",
                        super::listing_path(&m.listing)
                    )),
                    basis = if basis.is_empty() { "Terms" } else { basis },
                )
            })
            .collect();
        // Why a member is installed already, unavailable or not permitted
        // here, beneath the table (requirements 25 and 26).
        let reasons: String = members
            .iter()
            .filter(|m| &m.group == group)
            .filter_map(|m| {
                let held = statuses.get(m.listing.as_str())?;
                (!held.reason.is_empty()).then(|| {
                    format!(
                        "<p class=\"hint\" data-reason=\"{id}\">{name}: {state}, {why}.</p>",
                        id = escape(&m.listing),
                        name = escape(&member_name(listings.get(&m.listing), &m.listing)),
                        state = state_said(held.state).to_lowercase(),
                        why = escape(&held.reason),
                    )
                })
            })
            .collect();
        // The other groups' ticks ride along.
        let carried: String = ticked
            .iter()
            .filter(|t| {
                members
                    .iter()
                    .any(|m| &m.listing == *t && &m.group != group)
            })
            .map(|t| hidden("member", t))
            .collect();
        format!(
            "<form method=\"get\" action=\"{base}\" class=\"sheet-act pack-step\">\
             <input type=\"hidden\" name=\"ticked\" value=\"1\">{carried}\
             <h2>{group}</h2><om-pager rows=\"{STEP_ROWS}\"><table class=\"list one-line plugin-table pack-members\">\
             <thead><tr><th class=\"tick\"><span class=\"visually-hidden\">Install</span></th><th>Name</th>\
             <th class=\"version\">Version</th><th class=\"mid\">Roles</th><th class=\"wide\">Floor</th>\
             <th class=\"mid\">Registry</th><th class=\"wide\">Terms</th></tr></thead><tbody>{rows}</tbody></table>\
             </om-pager>{reasons}\
             <div class=\"sheet-foot\"><a class=\"button\" href=\"{PACKS_VIEW}\">Cancel</a>{back}\
             <button type=\"submit\" name=\"step\" value=\"{next}\" class=\"primary\">Next</button></div></form>",
            base = escape(&base),
            group = escape(group),
            back = if step > 0 {
                format!(
                    "<button type=\"submit\" name=\"step\" value=\"{}\">Back</button>",
                    step - 1
                )
            } else {
                String::new()
            },
            next = step + 1,
        )
    } else {
        review(
            &session, &name, revision, &members, &ticked, &statuses, &listings, &base,
        )
    };
    let page = format!(
        "<section class=\"sheet-dialog pack-sheet\" aria-labelledby=\"sheet-title\">\
         <h1 id=\"sheet-title\">{t}</h1><p class=\"hint\">{curator} · revision {revision}</p>{steps_nav}{body}</section>",
        t = escape(&title),
        curator = escape(&held.curator),
    );
    sheet_page(&session, "Install", &page)
}

/// The install sheet's last step (requirement 17).
#[allow(clippy::too_many_arguments)]
fn review(
    session: &Session,
    name: &str,
    revision: u64,
    members: &[PackMember],
    ticked: &[String],
    statuses: &BTreeMap<&str, &PackMemberStatus>,
    listings: &BTreeMap<String, PluginListing>,
    base: &str,
) -> String {
    let chosen: Vec<&PackMember> = members
        .iter()
        .filter(|m| {
            ticked.contains(&m.listing) && tickable(statuses.get(m.listing.as_str()).copied())
        })
        .collect();
    let installed = |m: &&PackMember| {
        statuses.get(m.listing.as_str()).is_some_and(|s| {
            s.state == PackMemberState::Installed as i32
                || s.state == PackMemberState::Launched as i32
        })
    };
    let line = |m: &PackMember| {
        let s = statuses.get(m.listing.as_str());
        format!(
            "<li data-review=\"{id}\">{name} {version}{reason}</li>",
            id = escape(&m.listing),
            name = escape(&member_name(listings.get(&m.listing), &m.listing)),
            version = escape(s.map(|s| s.latest_version.as_str()).unwrap_or_default()),
            reason = s
                .filter(|s| !s.reason.is_empty())
                .map(|s| format!(" <span class=\"hint\">{}</span>", escape(&s.reason)))
                .unwrap_or_default(),
        )
    };
    let to_install: String = chosen
        .iter()
        .filter(|m| !installed(m))
        .map(|m| line(m))
        .collect();
    let already: String = chosen
        .iter()
        .filter(|m| installed(m))
        .map(|m| line(m))
        .collect();
    let community = chosen.iter().any(|m| m.registry == "community");
    let carried: String = chosen
        .iter()
        .map(|m| hidden("member", &m.listing))
        .collect();
    let back_to = format!(
        "{base}?ticked=1&step={}{}",
        groups(members).len().saturating_sub(1),
        chosen
            .iter()
            .map(|m| format!("&member={}", m.listing.replace('/', "%2F")))
            .collect::<String>()
    );
    format!(
        "<div class=\"pack-review\"><h2>Will be installed</h2>{install}\
         {already}</div>{terms}<p class=\"hint\" data-data-sources-line>{DATA_SOURCES_LINE}.</p>\
         <form method=\"post\" action=\"/admin/plugins/packs/install\" class=\"sheet-act\">{token}{NOTICE_READ}\
         {pack}{rev}{carried}<label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{back}\">Back</a>\
         <button type=\"submit\" class=\"primary\">Install</button></div></form>",
        install = if to_install.is_empty() {
            "<p class=\"hint\">Nothing new: every member ticked is installed already.</p>".to_string()
        } else {
            format!("<ul class=\"one-line\">{to_install}</ul>")
        },
        already = if already.is_empty() {
            String::new()
        } else {
            format!("<h2>Already installed</h2><ul class=\"one-line\">{already}</ul>")
        },
        terms = if community { crate::notice::line() } else { String::new() },
        token = crate::admin::token_input(session),
        pack = hidden("pack", name),
        rev = hidden("revision", &revision.to_string()),
        back = escape(&back_to),
    )
}

/// `POST /admin/plugins/packs/install`: InstallPack.
async fn install(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let pack = field(&fields, "pack").to_string();
    if !token_ok(&session, &fields) {
        return to_pack(
            &pack,
            Err("this form did not come from your session".into()),
        );
    }
    if !notice_read(&fields) {
        return to_pack(&pack, Err(NOTICE_FIRST.into()));
    }
    let done: Result<PackInstall, String> = ask(
        &app,
        &session.subject,
        INSTALL_PACK,
        "meridian.v1.InstallPackRequest",
        InstallPackRequest {
            pack: pack.clone(),
            revision: field(&fields, "revision").parse().unwrap_or_default(),
            members: fields
                .iter()
                .filter(|(k, _)| k == "member")
                .map(|(_, v)| v.clone())
                .collect(),
            note: field(&fields, "note").into(),
        },
        INSTALLING,
    )
    .await;
    to_pack(
        &pack,
        done.map(|answer| {
            let installing = answer
                .members
                .iter()
                .filter(|m| m.state == PackMemberState::Installing as i32)
                .count();
            if installing == 0 {
                "Every member ticked is installed already.".to_string()
            } else {
                format!("Installing {installing} members, one by one. Reload to follow.")
            }
        }),
    )
}

// ── The pack page ───────────────────────────────────────────────────────

/// A member's next act on the checklist (requirement 19): Install, Launch
/// through the existing sheet, Retry, or the setup step's link.
fn next_act(name: &str, member: &PackMemberStatus, link: Option<&String>) -> String {
    let install_href = |said: &str| {
        format!(
            "<a class=\"button\" href=\"{href}\">{said}</a>",
            href = escape(&format!(
                "{}/install?ticked=1&step=99&member={}",
                pack_path(name),
                member.listing.replace('/', "%2F")
            )),
        )
    };
    match PackMemberState::try_from(member.state) {
        Ok(PackMemberState::NotInstalled) if member.removed_by_revision == 0 => {
            install_href("Install")
        }
        Ok(PackMemberState::Failed) => install_href("Retry"),
        Ok(PackMemberState::Installed) => format!(
            "<a class=\"button\" href=\"{href}\">Launch</a>",
            href = escape(&format!(
                "{}/launch?version={}",
                super::installed::versions_path(&member.listing),
                member.installed_version
            )),
        ),
        // The setup step's own words, a link, cut on a phone with its
        // whole text on hover.
        Ok(PackMemberState::Launched) => match (link, member.instance_ids.first()) {
            (Some(href), _) => format!(
                "<a href=\"{}\" title=\"{said}\" data-setup-link>{said}</a>",
                escape(href),
                said = escape(&member.setup)
            ),
            // Requirement 20: a newer version offered, on its own Update
            // sheet, where a widening is approved on its own.
            (None, Some(instance))
                if meridian_domain::semver::semver_cmp(
                    &member.latest_version,
                    &member.installed_version,
                ) == Some(std::cmp::Ordering::Greater) =>
            {
                format!(
                    "<a href=\"{href}\" data-update-link>Update to {v}</a>",
                    href = escape(&format!(
                        "/admin/plugins/registry/update?instance={instance}&version={}",
                        member.latest_version
                    )),
                    v = escape(&member.latest_version),
                )
            }
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn member_row(
    name: &str,
    member: &PackMemberStatus,
    listing: Option<&PluginListing>,
    link: Option<&String>,
    withdrawn: bool,
) -> String {
    let versions = match (
        member.installed_version.as_str(),
        member.latest_version.as_str(),
    ) {
        ("", "") => String::new(),
        ("", latest) => latest.to_string(),
        (installed, latest) if latest.is_empty() || installed == latest => installed.to_string(),
        (installed, latest) => format!("{installed} · {latest}"),
    };
    let mut state = state_said(member.state).to_string();
    if member.state == PackMemberState::Failed as i32 && !member.reason.is_empty() {
        state = format!("Failed: {}", member.reason);
    }
    if member.removed_by_revision > 0 {
        state = format!("No longer in this pack · {state}");
    }
    let reason = if member.state != PackMemberState::Failed as i32 && !member.reason.is_empty() {
        member.reason.clone()
    } else {
        String::new()
    };
    format!(
        "<tr data-member=\"{id}\" data-state=\"{key}\"><td title=\"{id}\">{label}</td>\
         <td class=\"version\" title=\"{versions}\">{versions}</td><td title=\"{state_title}\">{state}</td>\
         <td class=\"mid\" data-setup title=\"{setup}\">{setup}</td>\
         <td class=\"actions\">{act}</td></tr>",
        id = escape(&member.listing),
        key = state_key(member.state),
        label = escape(&member_name(listing, &member.listing)),
        versions = escape(&versions),
        state_title = escape(&if reason.is_empty() { state.clone() } else { format!("{state}: {reason}") }),
        state = escape(&state),
        setup = escape(&member.setup),
        act = if withdrawn { String::new() } else { next_act(name, member, link) },
    )
}

/// `GET /admin/plugins/packs/{organisation}/{pack}?tab=`: the pack page.
async fn pack_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, pack)): Path<(String, String)>,
    Query(asked): Query<HashMap<String, String>>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{pack}");
    let checklist = match checklist(&app, &session.subject, &name, &records).await {
        Ok(checklist) => checklist,
        Err(failed) => {
            return (
                StatusCode::NOT_FOUND,
                Html(crate::html::page_with(
                    "No such starter pack",
                    &error_line(&failed),
                    &chrome(&session, &name),
                )),
            )
                .into_response()
        }
    };
    let status = &checklist.status;
    let held = registry_pack(&app, &session.subject, &name)
        .await
        .ok()
        .flatten();
    let title = held.as_ref().map(title_of).unwrap_or_else(|| name.clone());
    let definitions: Vec<PackMember> = held
        .as_ref()
        .map(|p| members_of(p).to_vec())
        .unwrap_or_default();
    let listings = listings_of(&app, &session.subject, &definitions).await;
    let group_of = |m: &PackMemberStatus| m.group.clone();
    let mut groups: Vec<String> = Vec::new();
    for member in &status.members {
        if !groups.contains(&group_of(member)) {
            groups.push(group_of(member));
        }
    }
    let tab = asked
        .get("tab")
        .cloned()
        .filter(|t| t == "about" || groups.iter().any(|g| &group_key(g) == t))
        .unwrap_or_else(|| {
            groups
                .first()
                .map(|g| group_key(g))
                .unwrap_or_else(|| "about".into())
        });
    let base = pack_path(&name);
    let nav: String = groups
        .iter()
        .map(|g| (group_key(g), g.clone()))
        .chain(std::iter::once(("about".to_string(), "About".to_string())))
        .map(|(key, said)| {
            format!(
                "<a href=\"{base}?tab={key}\" data-tab=\"{key}\"{on}>{said}</a>",
                base = escape(&base),
                on = if key == tab {
                    " class=\"here\" aria-current=\"page\""
                } else {
                    ""
                },
                said = escape(&said),
            )
        })
        .collect();
    let said = asked.get("done").map(String::as_str).unwrap_or_default();
    let failed = asked.get("failed").map(String::as_str).unwrap_or_default();
    let mut lines = Vec::new();
    if status.withdrawn {
        lines.push("<p class=\"warn\" data-withdrawn>Withdrawn by its curator.</p>".to_string());
    }
    if status.installed_revision == 0 {
        lines.push(format!(
            "<p class=\"hint\" data-not-installed>Not installed. <a href=\"{}/install\">Install</a></p>",
            escape(&base)
        ));
    }
    // Requirement 21: a revision's added members, one line, with Install.
    let added: Vec<&PackMemberStatus> = status
        .members
        .iter()
        .filter(|m| m.added_by_revision > 0 && m.state == PackMemberState::NotInstalled as i32)
        .collect();
    if !added.is_empty() && !status.withdrawn {
        let names: Vec<String> = added
            .iter()
            .map(|m| member_name(listings.get(&m.listing), &m.listing))
            .collect();
        let href = format!(
            "{base}/install?ticked=1&step=99{}",
            added
                .iter()
                .map(|m| format!("&member={}", m.listing.replace('/', "%2F")))
                .collect::<String>()
        );
        lines.push(format!(
            "<p class=\"row\" data-added>Added to this pack: {} <a class=\"button\" href=\"{}\">Install</a></p>",
            escape(&names.join(", ")),
            escape(&href)
        ));
    }
    let installing = status
        .members
        .iter()
        .any(|m| m.state == PackMemberState::Installing as i32);
    if installing {
        lines.push(format!(
            "<p class=\"hint\" role=\"status\" data-installing>Installing, one member at a time. \
             <a href=\"{}?tab={}\">Reload</a> to follow.</p>",
            escape(&base),
            escape(&tab)
        ));
    }
    let body = if tab == "about" {
        let revision = held
            .as_ref()
            .and_then(|p| p.revision.clone())
            .unwrap_or_default();
        format!(
            "<dl class=\"facts\" data-about><dt>Pack</dt><dd><code>{pack}</code></dd><dt>Curator</dt><dd>{curator}</dd>\
             <dt>Description</dt><dd>{description}</dd><dt>Asset classes</dt><dd>{classes}</dd>\
             <dt>Revision</dt><dd>{installed} installed · {latest} in the catalogue</dd>\
             <dt>Here</dt><dd>{state}</dd></dl>",
            pack = escape(&name),
            curator = escape(held.as_ref().map(|p| p.curator.as_str()).unwrap_or_default()),
            description = escape(&revision.description),
            classes = escape(&revision.asset_classes.join(", ")),
            installed = status.installed_revision,
            latest = status.latest_revision,
            state = escape(&deployment_state(status)),
        )
    } else {
        let rows: String = status
            .members
            .iter()
            .filter(|m| group_key(&m.group) == tab)
            .map(|m| {
                member_row(
                    &name,
                    m,
                    listings.get(&m.listing),
                    checklist.links.get(&m.listing),
                    status.withdrawn,
                )
            })
            .collect();
        // A failed member's why, beneath the table, whole (requirement 24).
        let reasons: String = status
            .members
            .iter()
            .filter(|m| group_key(&m.group) == tab && m.state == PackMemberState::Failed as i32)
            .map(|m| {
                format!(
                    "<p class=\"hint\" data-reason=\"{id}\">{name}: failed, {why}.</p>",
                    id = escape(&m.listing),
                    name = escape(&member_name(listings.get(&m.listing), &m.listing)),
                    why = escape(&m.reason),
                )
            })
            .collect();
        format!(
            "<om-pager rows=\"{STEP_ROWS}\"><table class=\"list one-line plugin-table checklist\"><thead><tr>\
             <th>Name</th><th class=\"version\">Version</th><th>State</th><th class=\"mid\">Connection</th>\
             <th class=\"actions\"><span class=\"visually-hidden\">Next</span></th></tr></thead>\
             <tbody>{rows}</tbody></table></om-pager>{reasons}"
        )
    };
    let acts = if status.installed_revision == 0 {
        String::new()
    } else if status.withdrawn {
        format!(
            "<a class=\"button\" href=\"{}/remove\">Remove pack</a>",
            escape(&base)
        )
    } else {
        format!(
            "<a class=\"button primary\" href=\"{b}/update\">Update all</a><a class=\"button\" href=\"{b}/remove\">Remove pack</a>",
            b = escape(&base)
        )
    };
    let page = format!(
        "<section class=\"admin-section pack-page\"><div class=\"section-head\"><div><h1>{t}</h1></div>\
         <div class=\"row acts\">{acts}</div></div>{notice}{failure}{lines}\
         <nav class=\"tabs\" aria-label=\"The pack\">{nav}</nav>{body}</section>",
        t = escape(&title),
        notice = notice_line(said),
        failure = if failed.is_empty() { String::new() } else { error_line(failed) },
        lines = lines.concat(),
    );
    Html(crate::html::page_with(
        &title,
        &page,
        &chrome(&session, &title),
    ))
    .into_response()
}

/// `GET .../update`: "Update all"'s sheet, what each member would do.
async fn update_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, pack)): Path<(String, String)>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{pack}");
    let status = match pack_status(&app, &session.subject, &name).await {
        Ok(status) => status,
        Err(failed) => return to_pack(&name, Err(failed)),
    };
    let lines: String = status
        .members
        .iter()
        .filter(|m| {
            (m.state == PackMemberState::Launched as i32
                || m.state == PackMemberState::Installed as i32)
                && !m.reason.is_empty()
        })
        .map(|m| {
            format!(
                "<li data-update=\"{id}\">{id}: {reason}</li>",
                id = escape(&m.listing),
                reason = escape(&m.reason)
            )
        })
        .collect();
    let body = format!(
        "<section class=\"sheet-dialog\" aria-labelledby=\"sheet-title\"><h1 id=\"sheet-title\">Update all</h1>\
         <p>Each launched member moves to its own latest version, each its own update; a member asking \
         for more roles than it holds is skipped as \"{needs}\" and approved on its own Update sheet; an \
         installed member not launched gets the newer version. Members added since are not installed.</p>\
         {list}<form method=\"post\" action=\"/admin/plugins/packs/update\" class=\"sheet-act\">{token}{pack}\
         <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{back}\">Cancel</a>\
         <button type=\"submit\" class=\"primary\">Update all</button></div></form></section>",
        needs = "Needs approval",
        list = if lines.is_empty() {
            "<p class=\"hint\">No member has a newer version offered.</p>".to_string()
        } else {
            format!("<ul class=\"one-line\">{lines}</ul>")
        },
        token = crate::admin::token_input(&session),
        pack = hidden("pack", &name),
        back = escape(&pack_path(&name)),
    );
    sheet_page(&session, "Update all", &body)
}

/// `POST /admin/plugins/packs/update`: UpdatePack.
async fn update(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let pack = field(&fields, "pack").to_string();
    if !token_ok(&session, &fields) {
        return to_pack(
            &pack,
            Err("this form did not come from your session".into()),
        );
    }
    let done: Result<PackStatus, String> = ask(
        &app,
        &session.subject,
        UPDATE_PACK,
        "meridian.v1.UpdatePackRequest",
        UpdatePackRequest {
            pack: pack.clone(),
            note: field(&fields, "note").into(),
        },
        INSTALLING,
    )
    .await;
    to_pack(
        &pack,
        done.map(|status| {
            let held = status
                .members
                .iter()
                .filter(|m| m.reason == "Needs approval")
                .count();
            let failed = status
                .members
                .iter()
                .filter(|m| m.state == PackMemberState::Failed as i32)
                .count();
            match (held, failed) {
                (0, 0) => "Updated: each member is at its latest.".to_string(),
                (held, 0) => {
                    format!("Updated; {held} members need approval on their own Update sheet.")
                }
                (held, failed) => format!(
                    "Updated; {failed} members failed and {held} need approval: each says why below."
                ),
            }
        }),
    )
}

/// `GET .../remove`: "Remove pack"'s sheet, every member that stays.
async fn remove_sheet(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((organisation, pack)): Path<(String, String)>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let name = format!("{organisation}/{pack}");
    let status = match pack_status(&app, &session.subject, &name).await {
        Ok(status) => status,
        Err(failed) => return to_pack(&name, Err(failed)),
    };
    let staying: String = status
        .members
        .iter()
        .filter(|m| {
            m.state == PackMemberState::Launched as i32
                || m.state == PackMemberState::Installed as i32
        })
        .map(|m| {
            let href = m
                .instance_ids
                .first()
                .map(|instance| {
                    crate::area::href(instance, meridian_pb::v1::AccessLevel::Admin, None)
                })
                .unwrap_or_else(|| super::installed::versions_path(&m.listing));
            format!(
                "<li data-stays=\"{id}\"><a href=\"{href}\">{id}</a> · {state}</li>",
                id = escape(&m.listing),
                href = escape(&href),
                state = state_said(m.state),
            )
        })
        .collect();
    let body = format!(
        "<section class=\"sheet-dialog\" aria-labelledby=\"sheet-title\"><h1 id=\"sheet-title\">Remove pack</h1>\
         <p>Only this deployment's record of {n} is removed. Every member stays installed, and running \
         where it runs; each is managed on its own page.</p>{list}\
         <form method=\"post\" action=\"/admin/plugins/packs/remove\" class=\"sheet-act\" \
         data-confirm=\"Remove {n}'s record? No member is stopped or uninstalled.\">{token}{pack}\
         <label>Why <input type=\"text\" name=\"note\" required maxlength=\"2000\"></label>\
         <div class=\"sheet-foot\"><a class=\"button\" href=\"{back}\">Cancel</a>\
         <button type=\"submit\" class=\"danger\">Remove pack</button></div></form></section>",
        n = escape(&name),
        list = if staying.is_empty() {
            "<p class=\"hint\">No member is installed.</p>".to_string()
        } else {
            format!("<ul class=\"one-line\">{staying}</ul>")
        },
        token = crate::admin::token_input(&session),
        pack = hidden("pack", &name),
        back = escape(&pack_path(&name)),
    );
    sheet_page(&session, "Remove pack", &body)
}

/// `POST /admin/plugins/packs/remove`: RemovePack.
async fn remove(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let pack = field(&fields, "pack").to_string();
    if !token_ok(&session, &fields) {
        return to_pack(
            &pack,
            Err("this form did not come from your session".into()),
        );
    }
    let done: Result<PackStatus, String> = ask(
        &app,
        &session.subject,
        REMOVE_PACK,
        "meridian.v1.RemovePackRequest",
        RemovePackRequest {
            pack: pack.clone(),
            note: field(&fields, "note").into(),
        },
        READING,
    )
    .await;
    match done {
        Ok(_) => {
            let mut url = reqwest::Url::parse("http://dashboard.invalid/admin/plugins/versions")
                .expect("fixed");
            url.query_pairs_mut()
                .append_pair("tab", "browse")
                .append_pair("view", "packs")
                .append_pair(
                    "done",
                    &format!("{pack}'s record is removed; every member stays installed."),
                );
            Redirect::to(&format!(
                "{}?{}",
                url.path(),
                url.query().unwrap_or_default()
            ))
            .into_response()
        }
        Err(failed) => to_pack(&pack, Err(failed)),
    }
}

#[cfg(test)]
mod tests;
