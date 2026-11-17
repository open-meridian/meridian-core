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
//!
//! And an edge plugin a deployment admin allowed an archive is given it
//! beside its storage (W8.3, W8.7, contract v16): the chart's archive shape
//! -- a directory of the deployment's archive named for the instance, at
//! MERIDIAN_ARCHIVE_DIR, or a bucket's prefix of its own in
//! MERIDIAN_ARCHIVE_BUCKET and the account whose workload identity is scoped
//! to it -- merged into the plugin's container, only when the conductor's
//! CreatePlugin carries one allowed, with its bound in bytes as
//! MERIDIAN_ARCHIVE_MOST_BYTES where it has one. Refused where the chart
//! gives none.
//!
//! And an instance of several pods -- shards, or a standby pod (step 3d,
//! checkpoint 5, item 31) -- is given an account of its own,
//! `<fullname>-shards-<instance>`: a ServiceAccount, and a Role and
//! RoleBinding of that name granting `get` and `update` on its own shards'
//! Leases alone, named one by one, since RBAC names no prefix and cannot
//! narrow `create` to a name. So the launcher makes each shard's Lease
//! before the instance's pods start, and never deletes one: a Lease's
//! `leaseTransitions` then only grows, the floor of the shard's epoch. The
//! Role is written again at each launch, so a launch at another count names
//! that count's Leases; stopping removes the account, its Role and its
//! binding, and keeps the Leases. An allowed bucket archive's workload
//! identity moves onto that account: the annotations of the archive's own
//! account are copied onto it.

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
    /// What an allowed instance is given beside its storage, where the
    /// chart keeps archives (contract v16).
    pub archive: Option<String>,
    /// The plugin shape as several pods of one instance, each run under its
    /// instance's own account (step 3d; the resilience spec's section 7;
    /// checkpoint 5, item 31).
    pub shards: Option<String>,
}

/// An edge plugin's shapes (decisions/028), and which roles are the edge's.
#[derive(Debug, Clone, Default)]
pub struct StorageShapes {
    pub plain: String,
    pub live: Option<String>,
    pub claim: String,
    pub edge_roles: Vec<String>,
    /// Where the chart renders shards and standby pods: their shape
    /// mounting the instance's shared storage, the shared claim it mounts,
    /// and the pod that copies the instance's own claim into it (step 3d,
    /// checkpoint 5, item 30).
    pub shards: Option<String>,
    pub shared_claim: Option<String>,
    pub copy: Option<String>,
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
    // Either side may name a role renamed at contract v20 by its old name.
    let edge_roles = meridian_domain::roles_now(edge_roles);
    meridian_domain::roles_now(roles)
        .iter()
        .any(|role| edge_roles.contains(role))
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

/// The archive's bound as the plugin is told it, beside where the archive
/// is (W8.3): `PluginArchive.most_bytes`, set only where it is not none.
pub const ARCHIVE_MOST_BYTES: &str = "MERIDIAN_ARCHIVE_MOST_BYTES";

/// Whether a request carries an archive a deployment admin allowed.
pub fn archive_allowed(request: &CreatePluginRequest) -> bool {
    request
        .archive
        .as_ref()
        .is_some_and(|archive| archive.allowed)
}

/// The Deployment with its instance's archive merged into the plugin's
/// container (W8.3, W8.7, contract v16): the chart's archive shape filled in
/// for the checked request -- its env and mounts on the container named
/// `plugin`, its volumes on the pod, and the account it runs as, where the
/// shape names one. Unchanged for a request carrying none allowed; refused
/// for one at no edge role, and where the chart keeps no archive.
pub fn with_archive(
    mut manifest: serde_json::Value,
    template: Option<&str>,
    request: &CreatePluginRequest,
    edge_roles: &[String],
) -> Result<serde_json::Value, String> {
    if !archive_allowed(request) {
        return Ok(manifest);
    }
    if !at_the_edge(&request.roles, edge_roles) {
        return Err(format!(
            "{} was allowed an archive and holds no edge role; only a plugin at the edge \
             keeps raw records (decisions/028)",
            request.instance_id
        ));
    }
    let template = template.ok_or_else(|| {
        format!(
            "{} was allowed an archive, and this deployment's chart keeps none \
             (pluginArchive)",
            request.instance_id
        )
    })?;
    let filled = template.replace("__INSTANCE__", &request.instance_id);
    if let Some(left) = placeholder(&filled) {
        return Err(format!(
            "the archive template has a placeholder this does not fill: {left}"
        ));
    }
    let shape: serde_json::Value = serde_json::from_str(&filled)
        .map_err(|failed| format!("the archive template is not JSON: {failed}"))?;
    let pod = &mut manifest["spec"]["template"]["spec"];
    let container = pod["containers"]
        .as_array_mut()
        .and_then(|containers| containers.iter_mut().find(|c| c["name"] == "plugin"))
        .ok_or("the template has no container named plugin to give the archive to")?;
    for (field, from) in [("env", "env"), ("volumeMounts", "volumeMounts")] {
        if let Some(added) = shape[from].as_array() {
            match container[field].as_array_mut() {
                Some(held) => held.extend(added.iter().cloned()),
                None => container[field] = serde_json::Value::Array(added.clone()),
            }
        }
    }
    // Its bound beside it, in bytes (named 2026-10-07); unset for none.
    let most_bytes = request
        .archive
        .as_ref()
        .map_or(0, |archive| archive.most_bytes);
    if most_bytes > 0 {
        let bound = serde_json::json!({
            "name": ARCHIVE_MOST_BYTES,
            "value": most_bytes.to_string(),
        });
        match container["env"].as_array_mut() {
            Some(held) => held.push(bound),
            None => container["env"] = serde_json::Value::Array(vec![bound]),
        }
    }
    if let Some(added) = shape["volumes"].as_array() {
        match pod["volumes"].as_array_mut() {
            Some(held) => held.extend(added.iter().cloned()),
            None => pod["volumes"] = serde_json::Value::Array(added.clone()),
        }
    }
    // An instance of several pods keeps its own account: the archive's
    // workload identity moves onto it ([`identity_of`]; item 31).
    if let Some(account) = shape["serviceAccountName"].as_str() {
        if !several(request) {
            pod["serviceAccountName"] = serde_json::Value::String(account.to_string());
        }
    }
    Ok(manifest)
}

/// The account a bucket archive's shape runs an allowed instance under,
/// whose workload identity is scoped to the bucket: none for a local
/// archive, for a request carrying none allowed, or where the chart keeps
/// none.
pub fn archive_account(template: Option<&str>, request: &CreatePluginRequest) -> Option<String> {
    if !archive_allowed(request) {
        return None;
    }
    let shape: serde_json::Value = serde_json::from_str(template?).ok()?;
    shape["serviceAccountName"].as_str().map(String::from)
}

/// The annotations a ServiceAccount's workload identity is: GKE's, EKS's
/// (IRSA) and Azure Workload Identity's. The launcher's admission policy
/// admits an instance's own account carrying these alone (review c5b L2).
pub const IDENTITY_ANNOTATIONS: [&str; 8] = [
    "iam.gke.io/gcp-service-account",
    "eks.amazonaws.com/role-arn",
    "eks.amazonaws.com/audience",
    "eks.amazonaws.com/sts-regional-endpoints",
    "eks.amazonaws.com/token-expiration",
    "azure.workload.identity/client-id",
    "azure.workload.identity/tenant-id",
    "azure.workload.identity/service-account-token-expiration",
];

/// The workload identity a ServiceAccount carries, as its annotations, to
/// copy onto an instance's own account (item 31: the archive's bucket
/// identity moves onto it): those of [`IDENTITY_ANNOTATIONS`] alone. What a
/// tool, the cluster or anyone else keeps on the account is not identity,
/// and is left (review c5b L2).
pub fn identity_of(account: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    account["metadata"]["annotations"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| value.is_string() && IDENTITY_ANNOTATIONS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The Deployment with the instance's own share at its sidecar (contract
/// v30, W8.3; the resilience spec's section 10a; ruled 2026-11-11): each
/// field its launch set written into the `sidecar` container's
/// MERIDIAN_SIDECAR_* over the chart's sidecar.limits; a field 0, or none
/// set, leaves the chart's. Refused where its answers in flight would leave
/// the sidecar nothing of its own budget, as the chart refuses the same.
pub fn with_limits(
    mut manifest: serde_json::Value,
    request: &CreatePluginRequest,
) -> Result<serde_json::Value, String> {
    let Some(limits) = &request.limits else {
        return Ok(manifest);
    };
    let said = [
        (
            "MERIDIAN_SIDECAR_IN_FLIGHT",
            "limits.in_flight",
            limits.in_flight,
        ),
        (
            "MERIDIAN_SIDECAR_REQUESTS_PER_SECOND",
            "limits.requests_per_second",
            limits.requests_per_second,
        ),
        (
            "MERIDIAN_SIDECAR_ANSWER_MIB",
            "limits.answer_mib",
            limits.answer_mib,
        ),
        // A data plugin's lake write share (step 3d; section 6.9).
        (
            "MERIDIAN_SIDECAR_WRITE_MIB_PER_SECOND",
            "limits.write_mib_per_second",
            limits.write_mib_per_second,
        ),
    ];
    if said.iter().all(|(_, _, value)| *value == 0) {
        return Ok(manifest);
    }
    let container = manifest["spec"]["template"]["spec"]["containers"]
        .as_array_mut()
        .and_then(|containers| containers.iter_mut().find(|c| c["name"] == "sidecar"))
        .ok_or("the template has no container named sidecar to give the plugin's share to")?;
    if limits.answer_mib > 0 {
        let env_of = |name: &str| {
            container["env"].as_array().and_then(|env| {
                env.iter()
                    .find(|e| e["name"] == name)
                    .and_then(|e| e["value"].as_str().map(str::to_string))
            })
        };
        let percent = env_of("MERIDIAN_MEMORY_BUDGET_PERCENT")
            .and_then(|p| p.parse::<u64>().ok())
            .unwrap_or(80);
        if let Some(limit) = container["resources"]["limits"]["memory"]
            .as_str()
            .and_then(crate::preflight::quantity)
        {
            let working = limit / 1024 / 1024 * percent / 100;
            let held = u64::from(limits.answer_mib) + 1;
            if working < held + 4 {
                return Err(format!(
                    "limits.answer_mib {} leaves {}'s sidecar nothing of its own budget: its \
                     {} MiB working budget less the plugin's answers in flight and its \
                     session; at most {}",
                    limits.answer_mib,
                    request.instance_id,
                    working,
                    working.saturating_sub(5)
                ));
            }
        }
    }
    let env = container["env"]
        .as_array_mut()
        .ok_or("the template's sidecar container has no environment")?;
    for (name, _, value) in said {
        if value == 0 {
            continue;
        }
        env.retain(|e| e["name"] != name);
        env.push(serde_json::json!({"name": name, "value": value.to_string()}));
    }
    Ok(manifest)
}

/// The shard count the launcher tells each sidecar of an instance run as
/// several pods (approved 2026-11-15).
pub const SHARDS_ENV: &str = crate::shards::SHARDS_ENV;

/// Whether a request asks for several pods of its instance: shards above
/// one, or a standby pod -- or, once [`one_pod_as_shard`] has said so, one
/// shard on its instance's shared storage.
pub fn several(request: &CreatePluginRequest) -> bool {
    request.shards >= 1 || request.standby
}

/// A request as the launcher runs it: shards 1 alone is one pod, as 0 is
/// (contract v30: "0 and 1 each one pod"); and an instance of one pod whose
/// storage is shared since it ran as several -- `shared` -- runs as one
/// shard of one on it, so it reaches every shard's records and writes its
/// own under `shard-0/` (item 30; accepted under A 2026-11-17).
pub fn one_pod_as_shard(mut request: CreatePluginRequest, shared: bool) -> CreatePluginRequest {
    if request.shards == 1 && !request.standby {
        request.shards = 0;
    }
    if shared && request.shards == 0 && !request.standby {
        request.shards = 1;
    }
    request
}

/// The refusal of an instance of several pods keeping storage where the
/// chart offers it none to share (item 30): approved by the product owner
/// as item 69 (2026-11-17).
pub fn no_shared_storage_said(instance: &str) -> String {
    format!(
        "{instance} was asked shards or a standby pod, and its plugin keeps storage: this \
         deployment's chart offers no shared storage (pluginStorage.shared.storageClassName)"
    )
}

/// The template an instance launched with shards or a standby pod is made
/// from (step 3d; the resilience spec's section 7): the chart's shards
/// shape in place of `chosen`, which is kept for any other request; for a
/// plugin keeping storage, the shape mounting its instance's shared
/// storage, and the shared claim (item 30). Refused where the chart renders
/// none, for more shards than a declaration may name, and for a live
/// instance.
pub fn shards_template<'a>(
    chosen: Chosen<'a>,
    request: &CreatePluginRequest,
    shapes: &'a Shapes,
) -> Result<Chosen<'a>, String> {
    if !several(request) {
        return Ok(chosen);
    }
    let id = &request.instance_id;
    if request.shards > crate::shards::MOST {
        return Err(format!(
            "shards: {id} was asked {} shards; at most {}",
            request.shards,
            crate::shards::MOST
        ));
    }
    if request.live {
        return Err(format!(
            "{id} was asked for live with shards or a standby pod: a live instance is one pod"
        ));
    }
    if chosen.claim.is_some() {
        // Its pods share one claim, a folder per shard (item 30).
        let storage = shapes.storage.as_ref();
        return match storage.and_then(|storage| {
            Some((storage.shards.as_deref()?, storage.shared_claim.as_deref()?))
        }) {
            Some((workload, claim)) => Ok(Chosen {
                workload,
                claim: Some(claim),
            }),
            None => Err(no_shared_storage_said(id)),
        };
    }
    let workload = shapes.shards.as_deref().ok_or_else(|| {
        format!(
            "{id} was asked shards or a standby pod, and this deployment's chart renders no \
             shape for them: its sidecars take their shards' Leases under an account the \
             launcher makes for the instance, which the chart lets it make where a store runs a \
             standby (the ha profile)"
        )
    })?;
    Ok(Chosen {
        workload,
        claim: None,
    })
}

/// The Deployment of an instance launched with shards or a standby pod: a
/// pod per shard and one more for a standby, each sidecar told the shard
/// count ([`SHARDS_ENV`]), 1 for a standby alone, and run under its
/// instance's own account ([`shards_account`]). Unchanged for any other
/// request.
pub fn with_shards(
    mut manifest: serde_json::Value,
    request: &CreatePluginRequest,
) -> Result<serde_json::Value, String> {
    if !several(request) {
        return Ok(manifest);
    }
    let shards = request.shards.max(1);
    manifest["spec"]["replicas"] = serde_json::json!(shards + u32::from(request.standby));
    // Rolled a shard at a time, where the shape rolls: one pod may be
    // away, and one more for a standby pod, which is never ready -- with
    // fewer the rollout could stop no old pod, and with more it stops
    // shards it need not.
    if let Some(rolling) = manifest["spec"]["strategy"]["rollingUpdate"].as_object_mut() {
        rolling.insert(
            "maxUnavailable".into(),
            serde_json::json!(1 + u32::from(request.standby)),
        );
    }
    let env = manifest["spec"]["template"]["spec"]["containers"]
        .as_array_mut()
        .and_then(|containers| containers.iter_mut().find(|c| c["name"] == "sidecar"))
        .and_then(|sidecar| sidecar["env"].as_array_mut())
        .ok_or("the shards template has no sidecar container to tell the shard count")?;
    if !env.iter().any(|e| e["name"] == SHARDS_ENV) {
        return Err(format!(
            "the shards template does not tell its sidecar {SHARDS_ENV}"
        ));
    }
    // Each shard holds at least one of each limit, so shards above the
    // in-flight or answer limit its sidecars are given -- the chart's, or
    // the launch's own, written just before -- would together hold more
    // than the instance's share (review finding 11; accepted 2026-11-15).
    for (name, said) in [
        ("MERIDIAN_SIDECAR_IN_FLIGHT", "sidecar.limits.inFlight"),
        ("MERIDIAN_SIDECAR_ANSWER_MIB", "sidecar.limits.answerMiB"),
    ] {
        let limit = env
            .iter()
            .find(|e| e["name"] == name)
            .and_then(|e| e["value"].as_str())
            .and_then(|value| value.trim().parse::<u32>().ok());
        if let Some(limit) = limit.filter(|limit| shards > *limit) {
            return Err(format!(
                "shards: {} was asked {shards} shards, and its {said} is {limit}: each shard                  holds at least one, so its shards together would hold more than the                  instance's share; at most {limit} shards, or a higher limit at its launch",
                request.instance_id
            ));
        }
    }
    env.retain(|e| e["name"] != SHARDS_ENV);
    env.push(serde_json::json!({"name": SHARDS_ENV, "value": shards.to_string()}));
    // And its plugin, the same value and meaning (PO item 73, 2026-11-17):
    // which shard it serves it learns at registration.
    if let Some(plugin) = manifest["spec"]["template"]["spec"]["containers"]
        .as_array_mut()
        .and_then(|containers| containers.iter_mut().find(|c| c["name"] == "plugin"))
    {
        if !plugin["env"].is_array() {
            plugin["env"] = serde_json::Value::Array(Vec::new());
        }
        let env = plugin["env"].as_array_mut().expect("made an array");
        env.retain(|e| e["name"] != SHARDS_ENV);
        env.push(serde_json::json!({"name": SHARDS_ENV, "value": shards.to_string()}));
    }
    shards_account(&manifest, request)?;
    Ok(manifest)
}

/// The suffix of an instance's own account: `<fullname>-shards-<instance>`
/// (item 31).
fn account_suffix(instance: &str) -> String {
    format!("-shards-{instance}")
}

/// The account an instance of several pods runs under, as its shards shape
/// names it, filled in for the request: `<fullname>-shards-<instance>`
/// (item 31). Refused for a shape that names another.
pub fn shards_account(
    manifest: &serde_json::Value,
    request: &CreatePluginRequest,
) -> Result<String, String> {
    let account = manifest["spec"]["template"]["spec"]["serviceAccountName"]
        .as_str()
        .unwrap_or_default();
    let fullname = account
        .strip_suffix(&account_suffix(&request.instance_id))
        .unwrap_or_default();
    if fullname.is_empty() {
        return Err(format!(
            "the shards template runs its pods under {account:?}, not the instance's own \
             account, <fullname>-shards-{}",
            request.instance_id
        ));
    }
    Ok(account.to_string())
}

/// The account of `instance`, from the chart's shards shape, unfilled: what
/// stopping removes (item 31). None where the chart renders no such shape.
pub fn shards_account_of(template: Option<&str>, instance: &str) -> Option<String> {
    let filled = template?.replace("__INSTANCE__", instance);
    let shape: serde_json::Value = serde_json::from_str(&filled).ok()?;
    let account = shape["spec"]["template"]["spec"]["serviceAccountName"].as_str()?;
    account
        .ends_with(&account_suffix(instance))
        .then(|| account.to_string())
}

/// What the launcher keeps for an instance of several pods (item 31), and
/// the one pod it makes, copying an instance's own storage (item 30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Lease,
    ServiceAccount,
    Role,
    RoleBinding,
    Pod,
    PersistentVolumeClaim,
}

impl Kind {
    /// Its API path below `/namespaces/<namespace>/`'s group root, and its
    /// plural.
    pub fn root_and_plural(self) -> (&'static str, &'static str) {
        match self {
            Kind::Lease => ("/apis/coordination.k8s.io/v1", "leases"),
            Kind::ServiceAccount => ("/api/v1", "serviceaccounts"),
            Kind::Role => ("/apis/rbac.authorization.k8s.io/v1", "roles"),
            Kind::RoleBinding => ("/apis/rbac.authorization.k8s.io/v1", "rolebindings"),
            Kind::Pod => ("/api/v1", "pods"),
            Kind::PersistentVolumeClaim => ("/api/v1", "persistentvolumeclaims"),
        }
    }
}

/// The API, as far as an instance's account and Leases need it; the real
/// one is the in-cluster API server, and tests stand one in memory.
pub trait Objects: Send + Sync {
    /// One by name, or None where there is none.
    fn get(
        &self,
        kind: Kind,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<serde_json::Value>, String>> + Send;
    /// Made: true, or false where one by its name is there already (409).
    fn create(
        &self,
        kind: Kind,
        object: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;
    /// Written over the one there, conditional on its resourceVersion.
    fn replace(
        &self,
        kind: Kind,
        object: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Deleted: false where it was not there.
    fn delete(
        &self,
        kind: Kind,
        name: &str,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;
}

/// What an instance of several pods is given beside its Deployment (item
/// 31): its shards' Leases, made free, and its account, Role and binding.
#[derive(Debug, Clone, PartialEq)]
pub struct ShardsAccount {
    pub leases: Vec<serde_json::Value>,
    pub account: serde_json::Value,
    pub role: serde_json::Value,
    pub binding: serde_json::Value,
}

/// The objects an instance of several pods is given, for its Deployment as
/// made (`manifest`, after [`with_shards`]) in `namespace`: a Lease per
/// shard, `<instance>-shard-<n>`, made free; the account its pods run
/// under, carrying no secret and `identity` as its annotations; a Role of
/// the account's name with one rule -- `get` and `update` on exactly those
/// Leases, by name; and a binding of that Role to that account alone. Each
/// labelled as the Deployment is, but for what says it is a sidecar.
pub fn shards_objects(
    manifest: &serde_json::Value,
    request: &CreatePluginRequest,
    namespace: &str,
    identity: &serde_json::Map<String, serde_json::Value>,
) -> Result<ShardsAccount, String> {
    let account = shards_account(manifest, request)?;
    let mut labels = manifest["metadata"]["labels"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    labels.remove("meridian.dev/component");
    labels.remove("meridian.dev/live");
    if labels.get("meridian.dev/launched").and_then(|v| v.as_str()) != Some("true")
        || labels.get("meridian.dev/instance").and_then(|v| v.as_str())
            != Some(request.instance_id.as_str())
    {
        return Err(
            "the shards template does not mark what it makes as the launcher's, for its instance"
                .into(),
        );
    }
    let names: Vec<String> = (0..request.shards.max(1))
        .map(|shard| crate::shards::lease_name(&request.instance_id, shard))
        .collect();
    let leases = names
        .iter()
        .map(|name| {
            serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": {"name": name, "labels": labels},
                "spec": {},
            })
        })
        .collect();
    let mut account_metadata = serde_json::json!({"name": account, "labels": labels});
    if !identity.is_empty() {
        account_metadata["annotations"] = serde_json::Value::Object(identity.clone());
    }
    Ok(ShardsAccount {
        leases,
        account: serde_json::json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": account_metadata,
            "automountServiceAccountToken": false,
        }),
        role: serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "Role",
            "metadata": {"name": account, "labels": labels},
            "rules": [{
                "apiGroups": ["coordination.k8s.io"],
                "resources": ["leases"],
                "verbs": ["get", "update"],
                "resourceNames": names,
            }],
        }),
        binding: serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "RoleBinding",
            "metadata": {"name": account, "labels": labels},
            "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": namespace}],
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": account},
        }),
    })
}

/// Whether an object was made by the launcher.
fn launchers(object: &serde_json::Value) -> bool {
    object["metadata"]["labels"]["meridian.dev/launched"] == "true"
}

/// Make what [`shards_objects`] describes, before the instance's
/// Deployment: each Lease where it is not there -- one there is kept as it
/// is, its transitions with it -- and the account, Role and binding made,
/// or written over the launcher's own, so a launch at another count names
/// its own Leases. One there that the launcher did not make is never
/// written over.
pub async fn make_shards_account<O: Objects>(api: &O, made: &ShardsAccount) -> Result<(), String> {
    for lease in &made.leases {
        api.create(Kind::Lease, lease).await?;
    }
    for (kind, object) in [
        (Kind::ServiceAccount, &made.account),
        (Kind::Role, &made.role),
        (Kind::RoleBinding, &made.binding),
    ] {
        let name = object["metadata"]["name"].as_str().unwrap_or_default();
        match api.get(kind, name).await? {
            None => {
                if !api.create(kind, object).await? {
                    return Err(format!(
                        "{kind:?} {name} was made by another while the launcher made it; \
                         launch again"
                    ));
                }
            }
            Some(there) if !launchers(&there) => {
                return Err(format!(
                    "{kind:?} {name} is there, and was not made by the launcher; it is kept, and \
                     an administrator decides what becomes of it"
                ));
            }
            Some(there) => {
                let mut object = object.clone();
                object["metadata"]["resourceVersion"] =
                    there["metadata"]["resourceVersion"].clone();
                api.replace(kind, &object).await?;
            }
        }
    }
    Ok(())
}

/// Remove an instance's account, its Role and its binding, where the
/// launcher made them; never its Leases (item 31). Says what it removed.
pub async fn remove_shards_account<O: Objects>(
    api: &O,
    account: &str,
) -> Result<Vec<Kind>, String> {
    let mut removed = Vec::new();
    for kind in [Kind::RoleBinding, Kind::Role, Kind::ServiceAccount] {
        match api.get(kind, account).await? {
            Some(there) if launchers(&there) => {
                if api.delete(kind, account).await? {
                    removed.push(kind);
                }
            }
            _ => {}
        }
    }
    Ok(removed)
}

/// What is left of a stopped instance (review c5b2 H1, c5b3 L2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    /// Nothing: no Deployment of it, and none of its pods.
    Nothing,
    /// Its pods, still stopping after their Deployment was deleted.
    Pods,
    /// A Deployment of it: a launch made since the stop.
    Relaunched,
}

/// What became of a stopped instance's account (review c5b L5, c5b3 L2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// Removed once its pods had ended: what was removed.
    Removed(Vec<Kind>),
    /// Kept: a launch of the instance made since writes it over, and is
    /// the account's now.
    Relaunched,
    /// Kept: its pods had not ended in time; a later launch writes it over.
    Stopping,
}

/// The cluster's workloads, as far as stopping an instance needs them; the
/// real one is the in-cluster API server, and tests stand one in memory.
pub trait Workloads: Send + Sync {
    /// The Deployments carrying `selector`, as a list.
    fn deployments(
        &self,
        selector: &str,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send;
    /// The pods carrying `selector`, as a list.
    fn pods(
        &self,
        selector: &str,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send;
    /// Delete a Deployment at once, its pods left to stop on their own:
    /// false where it was not there.
    fn delete_deployment(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;
}

fn launched_of(instance: &str) -> String {
    format!(
        "{},{}={instance}",
        crate::launched::LAUNCHED_SELECTOR,
        crate::launched::INSTANCE_LABEL
    )
}

fn names(list: &serde_json::Value) -> Vec<String> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["metadata"]["name"].as_str().map(String::from))
        .collect()
}

/// Stop an instance the launcher launched: each of its Deployments deleted
/// at once, so a launch straight after -- a restart with an archive, a move
/// to another version -- finds none (review c5b2 H1); its pods stop on their
/// own. Says which were deleted.
pub async fn stop_launched<W: Workloads>(api: &W, instance: &str) -> Result<Vec<String>, String> {
    let mut deleted = Vec::new();
    for name in names(&api.deployments(&launched_of(instance)).await?) {
        if api.delete_deployment(&name).await? {
            deleted.push(name);
        }
    }
    Ok(deleted)
}

/// What is left of a stopped instance the launcher launched: a Deployment
/// of it is a launch made since; else its pods, until each has stopped.
pub async fn left_of<W: Workloads>(api: &W, instance: &str) -> Result<Left, String> {
    let selector = launched_of(instance);
    if !names(&api.deployments(&selector).await?).is_empty() {
        return Ok(Left::Relaunched);
    }
    if !names(&api.pods(&selector).await?).is_empty() {
        return Ok(Left::Pods);
    }
    Ok(Left::Nothing)
}

/// Remove an instance's account, Role and binding once its pods have ended
/// (review c5b L5): removed while they run, its sidecars' tokens die with
/// the account, so none can release its shard's Lease and a launch after
/// waits out a whole lapse. `left` says what is left of the instance
/// ([`left_of`]), and is asked every `every`, for `within` at most; a
/// launch of it made since ends the wait at once, the account the new
/// launch's (review c5b3 L2). Each ask, and the removal after it, is made
/// holding `launching`, which a launch holds while it makes the account and
/// its Deployment, so a launch between never loses the account it made.
pub async fn remove_shards_account_once_ended<O, F, Asked>(
    api: &O,
    account: &str,
    launching: &tokio::sync::Mutex<()>,
    mut left: F,
    every: std::time::Duration,
    within: std::time::Duration,
) -> Result<Removal, String>
where
    O: Objects,
    F: FnMut() -> Asked,
    Asked: std::future::Future<Output = Result<Left, String>>,
{
    let until = tokio::time::Instant::now() + within;
    loop {
        {
            let _launching = launching.lock().await;
            match left().await? {
                Left::Nothing => {
                    return remove_shards_account(api, account)
                        .await
                        .map(Removal::Removed)
                }
                Left::Relaunched => return Ok(Removal::Relaunched),
                Left::Pods => {}
            }
        }
        if tokio::time::Instant::now() >= until {
            return Ok(Removal::Stopping);
        }
        tokio::time::sleep(every).await;
    }
}

// ── An instance's own storage, copied into its shared storage (item 30) ──

/// The SDK's command an instance's own storage is copied, and waited for,
/// with, in its own plugin image as its own user (PO items 86 and 88,
/// 2026-11-17): `meridian-storage copy <claim>` in the copy pod,
/// `meridian-storage copied <claim>` in its pods' init container. Its
/// behaviour is [`copy`]'s, which its tests specify.
pub const STORAGE_COMMAND: &str = "meridian-storage";

/// The copy pod's template, filled in for a checked request: the chart's
/// storage-copy shape, marked the launcher's, for its instance, run in its
/// own plugin image (item 86).
pub fn copy_pod(
    template: &str,
    request: &CreatePluginRequest,
) -> Result<serde_json::Value, String> {
    let filled = template
        .replace("__INSTANCE__", &request.instance_id)
        .replace("__IMAGE__", &request.image);
    if let Some(left) = placeholder(&filled) {
        return Err(format!(
            "the storage copy template has a placeholder this does not fill: {left}"
        ));
    }
    let pod: serde_json::Value = serde_json::from_str(&filled)
        .map_err(|failed| format!("the storage copy template is not JSON: {failed}"))?;
    let labels = &pod["metadata"]["labels"];
    if pod["kind"] != "Pod"
        || labels["meridian.dev/launched"] != "true"
        || labels["meridian.dev/component"] != "storage-copy"
        || labels["meridian.dev/instance"] != request.instance_id.as_str()
    {
        return Err(
            "the storage copy template does not mark what it makes as the launcher's copy, for \
             its instance"
                .into(),
        );
    }
    Ok(pod)
}

/// The init container an instance's pods wait in until its own claim is
/// copied into its shared storage (item 30): the copy pod's own container,
/// its plugin's image, as `meridian-storage copied` (items 86, 88), the
/// shared storage mounted read only where the plugin mounts it, and
/// nothing else.
pub fn copied_gate(copy: &serde_json::Value) -> Result<serde_json::Value, String> {
    let container = &copy["spec"]["containers"][0];
    let command = container["command"].as_array();
    let claim = command
        .filter(|command| {
            command.len() == 3
                && command[0] == STORAGE_COMMAND
                && command[1] == "copy"
                && container["args"].is_null()
        })
        .and_then(|command| command[2].as_str())
        .ok_or("the storage copy template does not run meridian-storage copy")?;
    let mut gate = serde_json::json!({
        "name": "storage-copied",
        "image": container["image"],
        "command": [STORAGE_COMMAND, "copied", claim],
        "securityContext": container["securityContext"],
        "volumeMounts": [{
            "name": "storage",
            "mountPath": copy::STORAGE,
            "readOnly": true,
        }],
    });
    for field in ["imagePullPolicy", "resources"] {
        if !container[field].is_null() {
            gate[field] = container[field].clone();
        }
    }
    Ok(gate)
}

/// The Deployment with `gate` before its containers: its pods start once
/// the copy is whole. Refused for a shape with no storage to mount.
pub fn with_gate(
    mut manifest: serde_json::Value,
    gate: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let pod = &mut manifest["spec"]["template"]["spec"];
    let mounts_storage = pod["volumes"]
        .as_array()
        .is_some_and(|volumes| volumes.iter().any(|v| v["name"] == "storage"));
    if !mounts_storage {
        return Err("the shards storage template has no volume named storage to wait on".into());
    }
    match pod["initContainers"].as_array_mut() {
        Some(held) => held.push(gate),
        None => pod["initContainers"] = serde_json::Value::Array(vec![gate]),
    }
    Ok(manifest)
}

/// The pods of a list still running, or yet to, that mount `claim` --
/// but the copy's own: an instance's earlier pods, which may still write
/// their last records into it.
pub fn holding(list: &serde_json::Value, claim: &str) -> Vec<String> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pod| pod["metadata"]["labels"]["meridian.dev/component"] != "storage-copy")
        .filter(|pod| {
            !matches!(
                pod["status"]["phase"].as_str(),
                Some("Succeeded" | "Failed")
            )
        })
        .filter(|pod| {
            pod["spec"]["volumes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["persistentVolumeClaim"]["claimName"] == claim)
        })
        .filter_map(|pod| pod["metadata"]["name"].as_str().map(String::from))
        .collect()
}

/// The refusal of a first launch as several pods while the instance's
/// earlier pods still stop, which may still write into its own claim:
/// approved by the product owner as item 69 (2026-11-17).
pub fn still_stopping_said(instance: &str) -> String {
    format!(
        "{instance}'s storage is copied into its shared storage before it runs as several \
         pods, and its earlier pods are still stopping; launch it again once they have"
    )
}

/// What becomes of a copy pod already there, by its phase: kept while it
/// runs, made again once it failed; once it has copied, kept -- done --
/// where it copied into the shared claim there now, and made again where
/// that claim was made since, so holds no marker (review c5c L3). One that
/// never ends is made again too, as the instance's pods say once they stop
/// waiting (review c5c2fold M1): Pending past their wait
/// ([`copy::WAIT_LIMIT`]), Unknown on a lost node, or being removed. One
/// Running is kept however long, a copy of much storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPod {
    Make,
    Keep,
    Done,
    Remake,
}

/// `shared_since` is the shared claim's `creationTimestamp` (RFC 3339, as
/// the API server writes it, so ordered as text).
/// `now_ns` is the moment of the launch.
pub fn copy_pod_now(there: Option<&serde_json::Value>, shared_since: &str, now_ns: i64) -> CopyPod {
    let Some(pod) = there else {
        return CopyPod::Make;
    };
    if !pod["metadata"]["deletionTimestamp"].is_null() {
        return CopyPod::Remake;
    }
    match pod["status"]["phase"].as_str().unwrap_or_default() {
        "Failed" | "Unknown" => CopyPod::Remake,
        "Pending" if past_the_wait(pod, now_ns) => CopyPod::Remake,
        "Succeeded" => {
            let made = pod["metadata"]["creationTimestamp"]
                .as_str()
                .unwrap_or_default();
            if shared_since > made {
                CopyPod::Remake
            } else {
                CopyPod::Done
            }
        }
        _ => CopyPod::Keep,
    }
}

/// Whether a copy pod was made longer ago than its instance's pods wait for
/// it: made at a time the API server wrote, before `now_ns` by more than
/// [`copy::WAIT_LIMIT`].
fn past_the_wait(pod: &serde_json::Value, now_ns: i64) -> bool {
    let limit = i64::try_from(copy::WAIT_LIMIT.as_nanos()).unwrap_or(i64::MAX);
    pod["metadata"]["creationTimestamp"]
        .as_str()
        .and_then(meridian_config::registries::verify::rfc3339_ns)
        .is_some_and(|made| now_ns.saturating_sub(made) > limit)
}

/// The copy step's refusals (item 30), asked before any claim is made, so
/// a refused launch leaves its instance as it was, its storage not yet
/// shared (review c5c L4): another plugin's claim, and the instance's
/// earlier pods still mounting its own. The own claim's name, where there
/// is one to copy.
pub fn copy_refused(
    own: Option<&serde_json::Value>,
    pods: &serde_json::Value,
    request: &CreatePluginRequest,
) -> Result<Option<String>, String> {
    let Some(own) = own else {
        return Ok(None);
    };
    reusable(own, request)?;
    let claim = own["metadata"]["name"]
        .as_str()
        .ok_or("the instance's own claim has no name")?
        .to_string();
    if !holding(pods, &claim).is_empty() {
        return Err(still_stopping_said(&request.instance_id));
    }
    Ok(Some(claim))
}

/// The copy step (item 30), before the Deployment of an instance of
/// several pods keeping storage is made: where the launcher kept the
/// instance's own claim -- for the same plugin, never another's -- and none
/// of the instance's earlier pods still mounts it, the copy pod made (or
/// kept, or made again once failed, or into a shared claim made since), and
/// the Deployment's pods held until it has copied. Says which claim this
/// launch copies; none where none is kept, or where it was copied before
/// into the shared claim there, `shared` (review c5c L6), so the record
/// of the copy is the launch's that copied.
#[allow(clippy::too_many_arguments)]
pub async fn copy_step<O: Objects>(
    api: &O,
    own: Option<&serde_json::Value>,
    pods: &serde_json::Value,
    copy_template: &str,
    manifest: serde_json::Value,
    request: &CreatePluginRequest,
    shared: &serde_json::Value,
    now_ns: i64,
) -> Result<(serde_json::Value, Option<String>), String> {
    let Some(claim) = copy_refused(own, pods, request)? else {
        return Ok((manifest, None));
    };
    let pod = copy_pod(copy_template, request)?;
    let name = pod["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let since = shared["metadata"]["creationTimestamp"]
        .as_str()
        .unwrap_or_default();
    let copies = match copy_pod_now(api.get(Kind::Pod, &name).await?.as_ref(), since, now_ns) {
        CopyPod::Keep => true,
        CopyPod::Done => false,
        CopyPod::Make => {
            if !api.create(Kind::Pod, &pod).await? {
                return Err(format!(
                    "{name} was made by another meanwhile; launch again"
                ));
            }
            true
        }
        CopyPod::Remake => {
            api.delete(Kind::Pod, &name).await?;
            if !api.create(Kind::Pod, &pod).await? {
                return Err(format!(
                    "{name} failed and is still being removed; launch again in a moment"
                ));
            }
            true
        }
    };
    let manifest = with_gate(manifest, copied_gate(&pod)?)?;
    Ok((manifest, copies.then_some(claim)))
}

/// The refusal of an instance of several pods whose shared storage, kept,
/// was made for one node, where the chart now shares it across nodes
/// (review c5c L5): approved by the product owner as item 81 (2026-11-17).
pub fn not_shareable_said(instance: &str, name: &str, held: &str, wanted: &str) -> String {
    format!(
        "{name}, {instance}'s shared storage, was made {held}, for one node, and this \
         deployment's chart now asks {wanted} (pluginStorage.shared.storageClassName); it is \
         kept, and an administrator decides what becomes of it"
    )
}

/// The instance's storage, there before its Deployment is: the claim it
/// kept, when the same plugin launched as it before, or a new one; never
/// another plugin's (decisions/028), nor a kept one that cannot be mounted
/// as the chart now asks (review c5c L5). The claim as the API server
/// holds it.
pub async fn storage<O: Objects>(
    api: &O,
    template: &str,
    request: &CreatePluginRequest,
) -> Result<serde_json::Value, String> {
    let wanted = claim(template, request)?;
    let name = wanted["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if let Some(existing) = api.get(Kind::PersistentVolumeClaim, &name).await? {
        reusable(&existing, request)?;
        let modes = |claim: &serde_json::Value| -> Vec<String> {
            claim["spec"]["accessModes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|mode| mode.as_str().map(String::from))
                .collect()
        };
        // Kept for one node where the chart now asks it shared across
        // nodes (item 81); kept ReadWriteMany, it mounts as ever, whatever
        // the chart asks now (review c5c2fold L4).
        let (held, asked) = (modes(&existing), modes(&wanted));
        let many = "ReadWriteMany".to_string();
        if asked.contains(&many) && !held.contains(&many) {
            return Err(not_shareable_said(
                &request.instance_id,
                &name,
                &held.join(", "),
                &asked.join(", "),
            ));
        }
        tracing::info!(
            instance = request.instance_id,
            claim = name,
            "its storage, kept"
        );
        return Ok(existing);
    }
    if !api.create(Kind::PersistentVolumeClaim, &wanted).await? {
        return Err(format!(
            "{name} was made by another meanwhile; launch again"
        ));
    }
    tracing::info!(
        instance = request.instance_id,
        claim = name,
        "its storage, made"
    );
    api.get(Kind::PersistentVolumeClaim, &name)
        .await?
        .ok_or_else(|| format!("{name} was made and is not there; launch again"))
}

/// The refusal of an instance of several pods whose own claim is kept
/// where the chart renders no copy pod (review c5c L9): approved by the
/// product owner as item 80 (2026-11-17).
pub fn no_copy_pod_said(instance: &str, claim: &str) -> String {
    format!(
        "{instance} was asked shards or a standby pod, and its own storage {claim} is kept: \
         this deployment's chart renders no pod to copy it into its shared storage \
         (pluginStorage.shared), so it is not launched as several"
    )
}

/// The refusal of an instance whose storage is shared since it ran as
/// several, where the chart renders no shape mounting shared storage
/// (review c5c L4): approved by the product owner as item 83 (2026-11-17).
pub fn no_shared_shape_said(instance: &str) -> String {
    format!(
        "{instance}'s storage is shared and kept, and this deployment's chart renders no pod \
         that mounts shared storage (pluginStorage.shared), so it is not launched"
    )
}

/// The shared claim beside an instance's own (item 30):
/// `<fullname>-shared-storage-<instance>` for `<fullname>-storage-<instance>`.
fn shared_name_of(own: &str, instance: &str) -> Option<String> {
    own.strip_suffix(&format!("-storage-{instance}"))
        .map(|fullname| format!("{fullname}-shared-storage-{instance}"))
}

/// Whether the instance's storage is shared since it ran as several: its
/// shared claim there, made by the launcher for the same plugin (item 30).
/// Where the chart no longer renders shared storage, its claim is looked
/// for by name, and an instance whose storage is shared is refused rather
/// than run on its own claim without its shards' records (review c5c L4;
/// item 83).
///
/// Not shared yet where its own claim is kept and its copy never came
/// (review c5c2fold M2): no pod of it has run on the shared claim, which
/// they start on only once the copy is whole, so one pod runs on its own
/// claim as it did. That is a copy pod that failed, one Pending past its
/// pods' wait (removed here, so it never copies under a pod on the own
/// claim), or one that copied into a shared claim made since. One that
/// copied, runs, waits within the wait, is being removed or is not there
/// -- removed once it copied -- leaves the storage shared.
pub async fn shared_kept<O: Objects>(
    api: &O,
    shapes: &Shapes,
    request: &CreatePluginRequest,
    now_ns: i64,
) -> Result<bool, String> {
    let Some(storage) = shapes.storage.as_ref() else {
        return Ok(false);
    };
    let rendered = storage.shards.is_some() && storage.shared_claim.is_some();
    let name = match storage.shared_claim.as_deref() {
        Some(template) => claim(template, request)?["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        None => {
            let own = claim(&storage.claim, request)?;
            let own = own["metadata"]["name"].as_str().unwrap_or_default();
            match shared_name_of(own, &request.instance_id) {
                Some(name) => name,
                None => return Ok(false),
            }
        }
    };
    match api.get(Kind::PersistentVolumeClaim, &name).await? {
        None => Ok(false),
        Some(existing) => {
            reusable(&existing, request)?;
            if !rendered {
                return Err(no_shared_shape_said(&request.instance_id));
            }
            copied_or_copying(api, storage, request, &existing, now_ns).await
        }
    }
}

/// Whether an instance's shared claim, there, holds its storage now or will
/// once its copy pod ends ([`shared_kept`], review c5c2fold M2).
async fn copied_or_copying<O: Objects>(
    api: &O,
    storage: &StorageShapes,
    request: &CreatePluginRequest,
    shared: &serde_json::Value,
    now_ns: i64,
) -> Result<bool, String> {
    let Some(copy_template) = storage.copy.as_deref() else {
        return Ok(true);
    };
    let own = claim(&storage.claim, request)?;
    let own = own["metadata"]["name"].as_str().unwrap_or_default();
    if api.get(Kind::PersistentVolumeClaim, own).await?.is_none() {
        return Ok(true);
    }
    let name = copy_pod(copy_template, request)?["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let Some(pod) = api.get(Kind::Pod, &name).await? else {
        return Ok(true);
    };
    let since = shared["metadata"]["creationTimestamp"]
        .as_str()
        .unwrap_or_default();
    if copy_pod_now(Some(&pod), since, now_ns) != CopyPod::Remake {
        return Ok(true);
    }
    let removing = !pod["metadata"]["deletionTimestamp"].is_null();
    match pod["status"]["phase"].as_str().unwrap_or_default() {
        // It may still run, on a node out of reach.
        "Unknown" | "Running" => Ok(true),
        _ if removing => Ok(true),
        "Pending" => {
            api.delete(Kind::Pod, &name).await?;
            tracing::info!(
                instance = request.instance_id,
                pod = name,
                "its copy pod never ran within its pods' wait: removed, and its own storage kept"
            );
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// An instance's storage and, for several pods keeping it, the copy of its
/// own claim into its shared storage (item 30), in this order: the copy's
/// refusals before any claim is made (review c5c L4); the claim kept or
/// made ([`storage`]); then the copy step ([`copy_step`]), where the chart
/// renders a copy pod. `pods` are the instance's launched pods, listed for
/// a launch of several. Says which claim this launch copies, or none.
pub async fn storage_and_copy<O: Objects>(
    api: &O,
    shapes: &Shapes,
    claim_template: &str,
    pods: &serde_json::Value,
    manifest: serde_json::Value,
    request: &CreatePluginRequest,
    now_ns: i64,
) -> Result<(serde_json::Value, Option<String>), String> {
    let shaped = shapes.storage.as_ref().filter(|_| several(request));
    let copying = shaped.and_then(|shaped| shaped.copy.as_deref());
    let own = match shaped {
        Some(shaped) => {
            let own = claim(&shaped.claim, request)?;
            let name = own["metadata"]["name"].as_str().unwrap_or_default();
            api.get(Kind::PersistentVolumeClaim, name).await?
        }
        None => None,
    };
    // Its own claim kept, and no copy pod rendered: never started on a
    // shard-0/ without it (review c5c L9; item 80).
    if let (Some(own), None) = (&own, copying) {
        return Err(no_copy_pod_said(
            &request.instance_id,
            own["metadata"]["name"].as_str().unwrap_or_default(),
        ));
    }
    copy_refused(own.as_ref(), pods, request)?;
    let held = storage(api, claim_template, request).await?;
    match copying {
        Some(copy_template) => {
            copy_step(
                api,
                own.as_ref(),
                pods,
                copy_template,
                manifest,
                request,
                &held,
                now_ns,
            )
            .await
        }
        None => Ok((manifest, None)),
    }
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

pub mod copy;

#[cfg(test)]
mod tests;
