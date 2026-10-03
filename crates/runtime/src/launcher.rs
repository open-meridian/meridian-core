//! The launcher: a plugin's Deployment, made from the chart's own shape when
//! the conductor asks, and removed when it asks again (decisions/019, W8.3,
//! W8.4).
//!
//! It fills in one template, which the chart renders from the same
//! definition as the plugins it runs itself, so a launched plugin cannot
//! differ from a configured one but in its instance, image and roles -- and
//! each of those is checked here before it goes into the template: an
//! instance that is a host label, an image from the deployment's own
//! registry by digest and nothing else, roles that are names. It
//! removes only Deployments carrying its label, and makes none for an
//! instance that already has one.
//!
//! A plugin holding an edge role is also given its instance's storage
//! (decisions/028): a claim the chart's own template describes, made once and
//! kept, which its Deployment mounts. Stopping removes the Deployment and
//! never the claim, so launching the same plugin as the same instance again
//! finds what it kept; the launcher has no right to delete a claim at all,
//! and refuses to hand one plugin's storage to another.

use meridian_domain::v1::CreatePluginRequest;

pub const CREATE_PLUGIN: &str = "platform.deployment.command.create-plugin";
pub const REMOVE_PLUGIN: &str = "platform.deployment.command.remove-plugin";

/// A host label, a letter first: an instance, a plugin's name, a role.
pub fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Why a request is refused before anything is made, or nothing.
pub fn checked(request: &CreatePluginRequest, registry: &str) -> Result<(), String> {
    if !is_name(&request.instance_id) {
        return Err(format!("`{}` is not an instance", request.instance_id));
    }
    let from_here = request
        .image
        .strip_prefix(&format!("{registry}/plugins/"))
        .ok_or_else(|| {
            format!(
                "{} is not from the deployment's own registry, {registry}/plugins/",
                request.image
            )
        })?;
    let (name, digest) = from_here
        .split_once("@sha256:")
        .ok_or_else(|| format!("{} is not named by digest; a tag can move", request.image))?;
    if !is_name(name) {
        return Err(format!("`{name}` is not a plugin's name"));
    }
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("{} does not carry a sha256 digest", request.image));
    }
    for role in &request.roles {
        if !is_name(role) {
            return Err(format!("`{role}` is not a role"));
        }
    }
    Ok(())
}

/// The shapes the chart renders for a launched plugin: the plugin shape and,
/// on a development deployment alone, the live shape; and, where the chart
/// gives edge plugins storage, the same two with the instance's storage
/// mounted and the claim it mounts.
#[derive(Debug, Clone, Default)]
pub struct Shapes {
    pub plain: String,
    pub live: Option<String>,
    pub storage: Option<StorageShapes>,
}

/// An edge plugin's shapes (decisions/028), and which roles are the edge's.
#[derive(Debug, Clone, Default)]
pub struct StorageShapes {
    pub plain: String,
    pub live: Option<String>,
    pub claim: String,
    pub edge_roles: Vec<String>,
}

/// What a request is made from: the Deployment's template, and the claim's
/// for a plugin given storage.
#[derive(Debug, PartialEq, Eq)]
pub struct Chosen<'a> {
    pub workload: &'a str,
    pub claim: Option<&'a str>,
}

/// Whether a plugin holding `roles` is an edge plugin, which alone may own
/// storage (decisions/028, ruled point 1, and its amendment for `reporting`).
pub fn at_the_edge(roles: &[String], edge_roles: &[String]) -> bool {
    roles.iter().any(|role| edge_roles.contains(role))
}

/// Which of the chart's templates a request is made from: the plugin shape,
/// or the live shape (spec/live-plugin-development, rulings 2 and 4), each
/// with its instance's storage when the plugin holds an edge role and the
/// chart gives edge plugins storage (decisions/028). A live request is
/// refused on a deployment not installed for development, and on one whose
/// chart rendered no live shape; nothing else is ever made in its place.
pub fn template_for<'a>(
    request: &CreatePluginRequest,
    development: bool,
    shapes: &'a Shapes,
) -> Result<Chosen<'a>, String> {
    // Storage where the version's declaration asks for it, at the edge
    // alone; a version uploaded with no declaration, built before v11, by
    // holding an edge role, as before (W8.3, contract v11; decisions/028).
    let asks = match &request.declaration {
        None => true,
        Some(declaration) => declaration.storage.is_some(),
    };
    let at_edge = shapes
        .storage
        .as_ref()
        .is_some_and(|storage| at_the_edge(&request.roles, &storage.edge_roles));
    if request
        .declaration
        .as_ref()
        .is_some_and(|declaration| declaration.storage.is_some())
        && shapes.storage.is_some()
        && !at_edge
    {
        return Err(format!(
            "{} asks for storage and holds no edge role; only the edge roles own storage \
             (decisions/028)",
            request.instance_id
        ));
    }
    let storage = shapes
        .storage
        .as_ref()
        .filter(|storage| asks && at_the_edge(&request.roles, &storage.edge_roles));
    let claim = storage.map(|storage| storage.claim.as_str());
    if !request.live {
        let workload = storage.map_or(shapes.plain.as_str(), |storage| storage.plain.as_str());
        return Ok(Chosen { workload, claim });
    }
    if !development {
        return Err(format!(
            "{} was asked for live, and this deployment is not installed for development: \
             only a recorded version runs here",
            request.instance_id
        ));
    }
    let live = match storage {
        Some(storage) => storage.live.as_deref(),
        None => shapes.live.as_deref(),
    };
    let workload = live.ok_or_else(|| {
        format!(
            "{} was asked for live, and this deployment's chart renders no live shape",
            request.instance_id
        )
    })?;
    Ok(Chosen { workload, claim })
}

/// The plugin's name, from its image in the deployment's own registry.
fn plugin_of(image: &str) -> Option<&str> {
    let (_, after) = image.split_once("/plugins/")?;
    let (name, _) = after.split_once("@sha256:")?;
    Some(name)
}

/// The label a claim carries naming the plugin whose records it holds.
pub const PLUGIN_LABEL: &str = "meridian.dev/plugin";

/// The claim template, filled in for a checked request: the instance's
/// storage, labelled with the plugin it belongs to.
pub fn claim(template: &str, request: &CreatePluginRequest) -> Result<serde_json::Value, String> {
    let plugin =
        plugin_of(&request.image).ok_or_else(|| format!("{} names no plugin", request.image))?;
    let filled = template
        .replace("__INSTANCE__", &request.instance_id)
        .replace("__PLUGIN__", plugin);
    if let Some(left) = placeholder(&filled) {
        return Err(format!(
            "the claim template has a placeholder this does not fill: {left}"
        ));
    }
    let claim: serde_json::Value = serde_json::from_str(&filled)
        .map_err(|failed| format!("the claim template is not JSON: {failed}"))?;
    let labels = &claim["metadata"]["labels"];
    if labels["meridian.dev/launched"] != "true" || labels[PLUGIN_LABEL] != plugin {
        return Err(
            "the claim template does not mark what it makes as the launcher's, \
                    for its plugin"
                .into(),
        );
    }
    Ok(claim)
}

/// Whether a claim already there for the instance may be mounted by this
/// request: only one the launcher made, for the same plugin. Another
/// plugin's records are never a channel to this one (decisions/028), so
/// launching a different plugin as an instance whose storage is kept is
/// refused until an administrator removes that storage.
pub fn reusable(existing: &serde_json::Value, request: &CreatePluginRequest) -> Result<(), String> {
    let plugin = plugin_of(&request.image).unwrap_or_default();
    let name = existing["metadata"]["name"].as_str().unwrap_or_default();
    let labels = &existing["metadata"]["labels"];
    if labels["meridian.dev/launched"] != "true" {
        return Err(format!(
            "{name}, the storage {} would mount, was not made by the launcher; \
             it is kept, and an administrator decides what becomes of it",
            request.instance_id
        ));
    }
    match labels[PLUGIN_LABEL].as_str() {
        Some(held) if held == plugin => Ok(()),
        held => Err(format!(
            "{name}, {}'s storage, holds the records of {}, not {plugin}; it is kept, \
             so launch {} as another instance, or have an administrator remove the storage",
            request.instance_id,
            held.unwrap_or("another plugin"),
            plugin,
        )),
    }
}

/// The template, filled in for a checked request, as the Deployment to
/// create. Nothing placed in it can carry a quote, since each part was held
/// to a name or to the registry's form above.
pub fn manifest(
    template: &str,
    request: &CreatePluginRequest,
) -> Result<serde_json::Value, String> {
    let filled = template
        .replace("__INSTANCE__", &request.instance_id)
        .replace("__IMAGE__", &request.image)
        .replace("__ROLES__", &request.roles.join(","));
    if let Some(left) = placeholder(&filled) {
        return Err(format!(
            "the template has a placeholder this does not fill: {left}"
        ));
    }
    let manifest: serde_json::Value = serde_json::from_str(&filled)
        .map_err(|failed| format!("the template is not JSON: {failed}"))?;
    if manifest["metadata"]["labels"]["meridian.dev/launched"] != "true" {
        return Err("the template does not mark what it makes as the launcher's".into());
    }
    Ok(manifest)
}

/// A `__NAME__` still in the text, which a newer chart's template would
/// have and this launcher would not know to fill.
fn placeholder(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(start) = rest.find("__") {
        let after = &rest[start + 2..];
        let length = after
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
            .count();
        if length > 2 && after[..length].ends_with("__") {
            return Some(&rest[start..start + 2 + length]);
        }
        rest = &rest[start + 2..];
    }
    None
}

#[cfg(test)]
mod tests;
