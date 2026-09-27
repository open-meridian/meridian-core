//! The launcher: a plugin's Deployment, made from the chart's own shape when
//! the conductor asks, and removed when it asks again (decisions/019, W8.3,
//! W8.4).
//!
//! It fills in one template, which the chart renders from the same
//! definition as the plugins it runs itself, so a launched plugin cannot
//! differ from a configured one but in its instance, image, roles and tags --
//! and each of those is checked here before it goes into the template:
//! an instance that is a host label, an image from the deployment's own
//! registry by digest and nothing else, roles and tags that are names. It
//! removes only Deployments carrying its label, and makes none for an
//! instance that already has one.

use meridian_domain::v1::CreatePluginRequest;

pub const CREATE_PLUGIN: &str = "platform.deployment.command.create-plugin";
pub const REMOVE_PLUGIN: &str = "platform.deployment.command.remove-plugin";

/// A host label, a letter first: an instance, a plugin's name, a role, a tag.
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
    for part in request.roles.iter().chain(&request.tags) {
        if !is_name(part) {
            return Err(format!("`{part}` is not a role or a tag"));
        }
    }
    Ok(())
}

/// Which of the chart's templates a request is made from: the plugin shape,
/// or the live shape (spec/live-plugin-development, rulings 2 and 4). A live
/// request is refused on a deployment not installed for development, and on
/// one whose chart rendered no live shape; nothing else is ever made in its
/// place.
pub fn template_for<'a>(
    request: &CreatePluginRequest,
    development: bool,
    plain: &'a str,
    live: Option<&'a str>,
) -> Result<&'a str, String> {
    if !request.live {
        return Ok(plain);
    }
    if !development {
        return Err(format!(
            "{} was asked for live, and this deployment is not installed for development: \
             only a recorded version runs here",
            request.instance_id
        ));
    }
    live.ok_or_else(|| {
        format!(
            "{} was asked for live, and this deployment's chart renders no live shape",
            request.instance_id
        )
    })
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
        .replace("__ROLES__", &request.roles.join(","))
        .replace("__TAGS__", &request.tags.join(","));
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
