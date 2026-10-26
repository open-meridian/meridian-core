//! Bringing a plugin into a deployment: the catalogue, and launching and
//! stopping (W8, spec/the-local-plugin-registry).
//!
//! The conductor decides and records; the launcher only acts, and only when
//! the conductor asks (decisions/019). A version arrives with its metadata --
//! roles from the deployment's fixed list, and no tags (decisions/026) -- and
//! its image's digest; it is recorded once and never replaced. A launch runs a
//! recorded version as an instance, with exactly the roles it declares, which
//! is what the administrator was shown and approved: an approval of anything
//! else is no approval. Its grants are its roles', generated from the matrix,
//! and nothing here writes a grant or a topic.
//!
//! The dashboard asks, for a deployment admin, and is the only one the broker
//! lets ask; who that was comes from the envelope, and is what is recorded.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Envelope};
use meridian_domain::v1::{
    AllowArchiveRequest, CreatePluginReply, CreatePluginRequest, LaunchPluginRequest,
    PluginArchive, PluginCatalogue, PluginCatalogueRequest, PluginLaunch, PluginLaunchState,
    PluginMetadata, PluginVersion, ReadPluginDocumentationReply, ReadPluginDocumentationRequest,
    RecordPluginUploadRequest, RemovePluginReply, RemovePluginRequest, RetirePluginVersionRequest,
    StopPluginRequest, WithdrawArchiveRequest,
};
use meridian_pb::v1::PluginDeclaration;
use meridian_sidecar::Contract;
use prost::Message;

use crate::archive::{
    archive_allowed, edge_roles, roles_of, ArchiveGrant, ArchiveKind, ALLOW_ARCHIVE,
    WITHDRAW_ARCHIVE,
};
use crate::service::{answer_on, author, subject, Clock};
use crate::store::{note_refused, Ending, Snapshot, Store};

pub const RECORD_PLUGIN_UPLOAD: &str = "platform.config.command.record-plugin-upload";
pub const PLUGIN_CATALOGUE: &str = "platform.config.query.plugin-catalogue";
pub const LAUNCH_PLUGIN: &str = "platform.config.command.launch-plugin";
pub const STOP_PLUGIN: &str = "platform.config.command.stop-plugin";
pub const CREATE_PLUGIN: &str = "platform.deployment.command.create-plugin";
pub const REMOVE_PLUGIN: &str = "platform.deployment.command.remove-plugin";
/// W8.15 (contract v25): a version retired from the catalogue.
pub const RETIRE_PLUGIN_VERSION: &str = "platform.config.command.retire-plugin-version";
/// W8.2 (contract v25): one version's declaration and documentation.
pub const PLUGIN_DOCUMENTATION: &str = "platform.config.query.plugin-documentation";

/// What the conductor asks of the deployment's own registry, as it reaches it
/// inside the cluster (spec/the-local-plugin-registry): from contract v25,
/// releasing a retired version's image (W8.15). The registry is reached by
/// this release's own pods alone, and from outside only through the
/// dashboard's upload, which deletes nothing.
pub trait LocalRegistry: Send + Sync {
    /// Release an image: its manifest, by digest, deleted from the
    /// repository; its layers are the registry's to collect.
    fn release<'a>(
        &'a self,
        repository: &'a str,
        digest: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}

/// The repository a version's image sits in, in the deployment's own
/// registry: `plugins/{name}` for a local version, `plugins/{listing}` --
/// `plugins/{organisation}/{plugin}` -- for one from a registry (W8.10), so
/// the two never collide.
pub fn repository_of(version: &PluginVersion) -> String {
    if version.listing.is_empty() {
        format!(
            "plugins/{}",
            version
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or_default()
        )
    } else {
        format!("plugins/{}", version.listing)
    }
}

/// How long the launcher has to make or remove a plugin's workload: it asks
/// the cluster's API and waits for nothing to start.
const LAUNCHER: Duration = Duration::from_secs(20);

struct Plugins {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    /// The deployment's own registry, as each node reaches it:
    /// `localhost:<port>`. Every launch runs an image from here, by digest.
    registry: String,
    /// Where the deployment keeps archives, as its install named it (W7.1,
    /// W8.7, contract v16).
    archive: ArchiveGrant,
    /// The deployment's own registry as the conductor reaches it, where it
    /// does (contract v25): a retired version's image released there.
    local: Option<Arc<dyn LocalRegistry>>,
    /// The community and verified registries, where the deployment reaches
    /// them (contract v25, W8.10 to W8.12).
    registries: Option<Arc<crate::registries::Hold>>,
    /// The starter packs whose install is under way (W8.18, v28).
    packs: packs::Jobs,
}

mod catalogue;
pub mod packs;
mod registry;
pub use registry::{
    ACKNOWLEDGE_PULL, INSTALL_PLUGIN, LIST_PLUGIN_USAGE_REPORTS, REGISTRY_PLUGINS,
    SET_REPORTS_PLUGIN_USAGE, UPDATE_PLUGIN,
};

// ── The rules ───────────────────────────────────────────────────────────────

/// Lowercase letters, digits and single hyphens, a letter first and no
/// hyphen last: a plugin's name, its registry repository, and an instance.
pub fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.contains("--")
}

fn is_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn is_version(version: &str) -> bool {
    (1..=64).contains(&version.len())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// What an upload declares, held to the rules the CLI already checked:
/// the CLI is a convenience, and this is the rule (W8.1).
pub fn upload(
    contract: &Contract,
    request: &RecordPluginUploadRequest,
) -> Result<PluginMetadata, String> {
    let mut metadata = request
        .metadata
        .clone()
        .ok_or("an upload carries no metadata")?;
    // A version built before contract v20 names a renamed role by its old
    // name; it is kept by the new one (decisions/020).
    metadata.roles = meridian_domain::roles_now(&metadata.roles);
    if let Some(declaration) = metadata.declaration.as_mut() {
        for held in &mut declaration.not_carried {
            held.role = meridian_domain::role_now(&held.role).to_string();
        }
    }
    if !is_name(&metadata.name) {
        return Err(format!(
            "`{}` is not a plugin's name: lowercase letters, digits and single hyphens, \
             starting with a letter",
            metadata.name
        ));
    }
    if !is_version(&metadata.version) {
        return Err(format!("`{}` is not a version", metadata.version));
    }
    let mut roles = BTreeSet::new();
    for role in &metadata.roles {
        if contract.is_component(role) {
            return Err(format!(
                "`{role}` is one of the deployment's own components, which a plugin never declares"
            ));
        }
        contract
            .grants_for(std::slice::from_ref(role))
            .map_err(|_| {
                format!("`{role}` is not a role; a plugin's roles are from matrix/roles.tsv")
            })?;
        if !roles.insert(role) {
            return Err(format!("`{role}` is declared twice"));
        }
    }
    if metadata.sdk_version.is_empty() {
        return Err("an upload names no SDK version".into());
    }
    if let Some(declaration) = &metadata.declaration {
        declared(declaration, &metadata.roles)?;
    }
    // The vendor's documentation (W8.1, contract v25): the texts held to
    // the sections the declaration names, by digest and size.
    documented(
        metadata
            .declaration
            .as_ref()
            .map(|declaration| declaration.documentation.as_slice())
            .unwrap_or_default(),
        &request.documentation,
    )?;
    if !is_digest(&request.image_digest) {
        return Err(format!(
            "`{}` is not an image digest; a version is recorded by `sha256:<hex>`",
            request.image_digest
        ));
    }
    Ok(metadata)
}

/// The fixed sections of a version's documentation, in the order it is
/// drawn (W8.1, contract v25): each `docs/<name>.md`.
pub const SECTIONS: [&str; 5] = ["overview", "setup", "troubleshooting", "terms", "pages"];

/// The most bytes a version's documentation holds in all (W8.1).
pub const MOST_DOCUMENTATION_BYTES: u64 = 262_144;

/// A version's documentation held to its declaration (W8.1, contract v25):
/// each section a fixed one, named once, 1 to 64 KiB, the bundle at most
/// 256 KiB; each text sent for a section the declaration names, its SHA-256
/// and size the declaration's; and every section named sent. Refused naming
/// the section.
fn documented(
    sections: &[meridian_pb::v1::DocumentationSection],
    texts: &[meridian_domain::v1::DocumentationText],
) -> Result<(), String> {
    use meridian_pb::bounds::{
        DOCUMENTATION_SECTION_BYTES_RANGE, PLUGIN_DECLARATION_DOCUMENTATION_COUNT,
    };
    use sha2::Digest;
    if !PLUGIN_DECLARATION_DOCUMENTATION_COUNT.admits(sections.len()) {
        return Err(format!(
            "declaration.documentation names {} sections; at most {}",
            sections.len(),
            PLUGIN_DECLARATION_DOCUMENTATION_COUNT.most
        ));
    }
    let mut total = 0u64;
    for (n, section) in sections.iter().enumerate() {
        if !SECTIONS.contains(&section.name.as_str()) {
            return Err(format!(
                "declaration.documentation[{n}] is {:?}, not one of the sections {}",
                section.name,
                SECTIONS.join(", ")
            ));
        }
        if sections[..n].iter().any(|held| held.name == section.name) {
            return Err(format!(
                "declaration.documentation[{n}]: the section {} is named twice",
                section.name
            ));
        }
        if !is_digest(&section.digest) {
            return Err(format!(
                "declaration.documentation[{n}].digest is not `sha256:<hex>`"
            ));
        }
        if !DOCUMENTATION_SECTION_BYTES_RANGE.admits(section.bytes as i64) {
            return Err(format!(
                "the section {} is {} bytes; a section is 1 to {} (64 KiB)",
                section.name, section.bytes, DOCUMENTATION_SECTION_BYTES_RANGE.most
            ));
        }
        total += section.bytes;
    }
    if total > MOST_DOCUMENTATION_BYTES {
        return Err(format!(
            "the documentation holds {total} bytes in all; at most {MOST_DOCUMENTATION_BYTES} \
             (256 KiB)"
        ));
    }
    for (n, text) in texts.iter().enumerate() {
        let Some(section) = sections.iter().find(|held| held.name == text.section) else {
            return Err(format!(
                "documentation[{n}] is the section {:?}, which the declaration does not name",
                text.section
            ));
        };
        if texts[..n].iter().any(|held| held.section == text.section) {
            return Err(format!(
                "documentation[{n}]: the section {} is sent twice",
                text.section
            ));
        }
        let digest = format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(text.markdown.as_bytes()))
        );
        if digest != section.digest || text.markdown.len() as u64 != section.bytes {
            return Err(format!(
                "the section {}'s text is not what the declaration names: {} bytes, {digest}, \
                 where it names {} bytes, {}",
                text.section,
                text.markdown.len(),
                section.bytes,
                section.digest
            ));
        }
    }
    if let Some(missing) = sections
        .iter()
        .find(|section| !texts.iter().any(|text| text.section == section.name))
    {
        return Err(format!(
            "the declaration names the section {} and the upload sent no text for it",
            missing.name
        ));
    }
    Ok(())
}

/// A version's declaration (W8.1, contract v11): what it does not carry
/// named in a role it holds, each with why; its secret settings' names and
/// texts within the dictionary's bounds; and storage asked for only by a
/// version holding an edge role (decisions/028). The rule, as `meridian
/// plugin check` is the convenience.
fn declared(declaration: &PluginDeclaration, roles: &[String]) -> Result<(), String> {
    use meridian_pb::bounds::{
        NOT_CARRIED_NAME_LENGTH, NOT_CARRIED_SCHEME_LENGTH, PLUGIN_DECLARATION_NOT_CARRIED_COUNT,
        PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT, STORAGE_DECLARATION_RETENTION_DAYS_RANGE,
    };
    use meridian_pb::v1::NotCarriedReason;
    if !PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT.admits(declaration.secret_settings.len()) {
        return Err(format!(
            "the declaration names {} secret settings; at most {}",
            declaration.secret_settings.len(),
            PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT.most
        ));
    }
    if let Some(at) = declaration
        .secret_settings
        .iter()
        .position(String::is_empty)
    {
        return Err(format!("declaration.secret_settings[{at}] is empty"));
    }
    if !PLUGIN_DECLARATION_NOT_CARRIED_COUNT.admits(declaration.not_carried.len()) {
        return Err(format!(
            "the declaration names {} it does not carry; at most {}",
            declaration.not_carried.len(),
            PLUGIN_DECLARATION_NOT_CARRIED_COUNT.most
        ));
    }
    for (i, held) in declaration.not_carried.iter().enumerate() {
        if !roles.contains(&held.role) {
            return Err(format!(
                "declaration.not_carried[{i}].role `{}` is not a role this version declares",
                held.role
            ));
        }
        if !NOT_CARRIED_SCHEME_LENGTH.admits(held.scheme.chars().count())
            || !NOT_CARRIED_NAME_LENGTH.admits(held.name.chars().count())
        {
            return Err(format!(
                "declaration.not_carried[{i}] names its scheme in 1 to {} characters and its                  name in 1 to {}",
                NOT_CARRIED_SCHEME_LENGTH.most, NOT_CARRIED_NAME_LENGTH.most
            ));
        }
        if !matches!(
            NotCarriedReason::try_from(held.reason),
            Ok(reason) if reason != NotCarriedReason::Unspecified
        ) {
            return Err(format!(
                "declaration.not_carried[{i}].reason is unspecified; say why it is not carried"
            ));
        }
    }
    // An operations plugin's affirmation deadline (W11.28, W11.34; contract
    // v25): minutes after midnight, 1 to 1440, 0 declaring none; refused on
    // a version not holding operations.
    if declaration.affirmation_deadline != 0 {
        use meridian_pb::bounds::PLUGIN_DECLARATION_AFFIRMATION_DEADLINE_RANGE;
        if !PLUGIN_DECLARATION_AFFIRMATION_DEADLINE_RANGE
            .admits(i64::from(declaration.affirmation_deadline))
        {
            return Err(format!(
                "declaration.affirmation_deadline is {}; minutes after midnight, 1 to {}, or 0 \
                 for none",
                declaration.affirmation_deadline,
                PLUGIN_DECLARATION_AFFIRMATION_DEADLINE_RANGE.most
            ));
        }
        if !roles.iter().any(|role| role == "operations") {
            return Err(
                "declaration.affirmation_deadline is declared by a version not holding \
                 operations; only operations declares the firm's affirmation deadline"
                    .into(),
            );
        }
    }
    // A pricing plugin's models (W12.3, contract v27; ruling 324): refused on
    // a version not holding pricing; at most 32, each model id and version
    // once, each version 1 or more.
    if !declaration.pricing_models.is_empty() {
        use meridian_pb::bounds::{
            PLUGIN_DECLARATION_PRICING_MODELS_COUNT, PRICING_MODEL_DECLARATION_MODEL_VERSION_RANGE,
        };
        if !roles.iter().any(|role| role == "pricing") {
            return Err(
                "declaration.pricing_models is declared by a version not holding pricing; only \
                 pricing declares the models it answers"
                    .into(),
            );
        }
        if !PLUGIN_DECLARATION_PRICING_MODELS_COUNT.admits(declaration.pricing_models.len()) {
            return Err(format!(
                "declaration.pricing_models names {}; at most {}",
                declaration.pricing_models.len(),
                PLUGIN_DECLARATION_PRICING_MODELS_COUNT.most
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, model) in declaration.pricing_models.iter().enumerate() {
            if model.model_id.trim().is_empty() {
                return Err(format!(
                    "declaration.pricing_models[{i}].model_id names no model id"
                ));
            }
            if !PRICING_MODEL_DECLARATION_MODEL_VERSION_RANGE.admits(i64::from(model.model_version))
            {
                return Err(format!(
                    "declaration.pricing_models[{i}].model_version is {}; 1 or more",
                    model.model_version
                ));
            }
            if !seen.insert((model.model_id.clone(), model.model_version)) {
                return Err(format!(
                    "declaration.pricing_models[{i}] names {} v{} twice",
                    model.model_id, model.model_version
                ));
            }
        }
    }
    // A data plugin's catalogue (W8.1, contract v18), as `meridian plugin check`
    // says first and the sidecar again at registration.
    if let Some(catalogue) = &declaration.catalogue {
        if let Some(refused) = meridian_domain::lake::catalogue_refused(catalogue, roles) {
            return Err(refused);
        }
    }
    if let Some(storage) = &declaration.storage {
        if !roles
            .iter()
            .any(|role| meridian_domain::EDGE_ROLES.contains(&role.as_str()))
        {
            return Err(format!(
                "declaration.storage is asked for by a version declaring {}, no edge role; only                  the edge roles ({}) own storage (decisions/028)",
                if roles.is_empty() { "no role".to_string() } else { roles.join(", ") },
                meridian_domain::EDGE_ROLES.join(", ")
            ));
        }
        if !STORAGE_DECLARATION_RETENTION_DAYS_RANGE.admits(i64::from(storage.retention_days)) {
            return Err(format!(
                "declaration.storage.retention_days is {}; {} to {}",
                storage.retention_days,
                STORAGE_DECLARATION_RETENTION_DAYS_RANGE.least,
                STORAGE_DECLARATION_RETENTION_DAYS_RANGE.most
            ));
        }
        // Its kinds of raw record (contract v16): each named once and of its
        // form, labelled, its window within bounds, as `meridian plugin
        // check` says first and the sidecar again at registration.
        if let Some(refused) = meridian_sidecar::edge::kinds_refused(&storage.record_kinds, &[]) {
            return Err(refused);
        }
    }
    Ok(())
}

fn set(items: &[String]) -> BTreeSet<&str> {
    items.iter().map(String::as_str).collect()
}

fn listed(items: &BTreeSet<&str>) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.iter().copied().collect::<Vec<_>>().join(", ")
    }
}

/// Whether an instance name is one of core's own (contract v18; v17's
/// security review, Nit 3): a component's name (`dashboard`, `lake`), a
/// component's instance (`dashboard-1`), or the runtime's broker user. A
/// plugin launched as one would list its tools in core's `dashboard__`
/// namespace or speak as a component on the broker.
pub fn is_reserved(contract: &Contract, instance_id: &str) -> bool {
    let component = |name: &str| contract.is_component(name);
    if instance_id == "runtime" || component(instance_id) {
        return true;
    }
    instance_id.rsplit_once('-').is_some_and(|(head, tail)| {
        !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && component(head)
    })
}

/// The version a launch runs, if it is recorded and the approval is exactly
/// what it declares (W8.3).
pub fn launch<'a>(
    snapshot: &'a Snapshot,
    request: &LaunchPluginRequest,
) -> Result<&'a PluginVersion, String> {
    if !is_name(&request.instance_id) {
        return Err(format!(
            "`{}` is not an instance: lowercase letters, digits and single hyphens, starting \
             with a letter, at most 63",
            request.instance_id
        ));
    }
    if is_reserved(Contract::embedded(), &request.instance_id) {
        return Err(format!(
            "`{}` is one of core's own names, which no plugin instance takes",
            request.instance_id
        ));
    }
    // An instance's ID names one plugin for good (contract v18; v17's
    // security review, Nit 3): its settings, secrets opened for it, the
    // access entries and the delegations naming it would otherwise apply
    // to another plugin's code.
    if let Some(earlier) = snapshot
        .catalogue
        .launches
        .iter()
        .find(|held| held.instance_id == request.instance_id && held.name != request.name)
    {
        return Err(format!(
            "{} was launched as {} before; an instance's ID names one plugin, and its settings, \
             access and delegations stay with it, so {} launches under another ID",
            request.instance_id, earlier.name, request.name
        ));
    }
    let version = snapshot
        .catalogue
        .versions
        .iter()
        .find(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|m| m.name == request.name && m.version == request.version)
        })
        .ok_or_else(|| {
            format!(
                "{} {} is not in the catalogue",
                request.name, request.version
            )
        })?;
    // A retired version is no longer launchable (W8.15, contract v25).
    if version.retired_at_ns != 0 {
        return Err(format!(
            "{} {} was retired from the catalogue; launch a version it still offers",
            request.name, request.version
        ));
    }
    let metadata = version.metadata.as_ref().expect("found by it");
    let (approved, declared) = (set(&request.approved_roles), set(&metadata.roles));
    if approved != declared {
        return Err(format!(
            "the approval names roles {}, and {} {} declares {}; an approval of \
             something other than what runs is no approval",
            listed(&approved),
            request.name,
            request.version,
            listed(&declared)
        ));
    }
    Ok(version)
}

// ── Serving them ────────────────────────────────────────────────────────────

impl Plugins {
    fn snapshot(&self) -> Result<Snapshot, String> {
        self.store.snapshot().map_err(|failed| failed.to_string())
    }

    /// Ask the launcher, from a handler's blocking thread.
    fn ask_launcher<Q: Message, A: Message + Default>(
        &self,
        topic: &str,
        payload_type: &str,
        request: Q,
    ) -> Result<A, String> {
        let asking = self.bus.call(
            topic,
            payload_type,
            request.encode_to_vec(),
            None,
            Some(LAUNCHER),
        );
        let (_, answer) = tokio::runtime::Handle::current()
            .block_on(asking)
            .map_err(|failed| failed.to_string())?;
        A::decode(answer.as_slice())
            .map_err(|failed| format!("the launcher's answer did not read: {failed}"))
    }

    fn record_upload(
        &self,
        request: RecordPluginUploadRequest,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        // A plugin bundle names its listing (W8.14): verified as at install.
        if !request.listing.is_empty() {
            return self.carry(request, envelope);
        }
        let metadata = upload(Contract::embedded(), &request)?;
        let version = PluginVersion {
            metadata: Some(metadata.clone()),
            image_digest: request.image_digest,
            uploaded_by: subject(envelope),
            uploaded_at_ns: self.clock.now_ns(),
            source_registry: "local".to_string(),
            ..PluginVersion::default()
        };
        // The texts in the declaration's order, kept with the version in the
        // same step, never replaced (contract v25).
        let declared_order: Vec<String> = metadata
            .declaration
            .as_ref()
            .map(|declaration| {
                declaration
                    .documentation
                    .iter()
                    .map(|s| s.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut texts = request.documentation.clone();
        texts.sort_by_key(|text| declared_order.iter().position(|name| *name == text.section));
        if !self
            .store
            .record_plugin_version_documented(&version, &texts)
            .map_err(|f| f.to_string())?
        {
            return Err(format!(
                "{} {} is already recorded; an uploaded version is never replaced, so a change \
                 is a new version",
                metadata.name, metadata.version
            ));
        }
        tracing::info!(
            plugin = metadata.name,
            version = metadata.version,
            by = version.uploaded_by,
            "a plugin version recorded"
        );
        Ok(version)
    }

    fn launch(
        &self,
        request: LaunchPluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginLaunch, String> {
        let snapshot = self.snapshot()?;
        let version = launch(&snapshot, &request)?;
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        // A registry version is verified again at launch (W8.3; the spec's
        // requirement 24): its kept listing statement, the referrers in the
        // deployment's own registry and the newest key status held.
        if !version.listing.is_empty() {
            let hold = self.registries.as_ref().ok_or_else(|| {
                format!(
                    "{} {} came from a registry, and this deployment reaches none to verify it \
                     again",
                    request.name, request.version
                )
            })?;
            let statement = self
                .store
                .listing_statement(&request.name, &request.version)
                .map_err(|failed| failed.to_string())?;
            tokio::runtime::Handle::current()
                .block_on(hold.reverify(version, &statement))
                .map_err(|why| format!("the verification at launch: {why}"))?;
        }
        let metadata = version.metadata.clone().expect("found by it");
        // Stamped as every change is: the person, and the delegation and
        // client they acted through (W8.3, contract v17).
        let by = author(envelope);
        let launched = PluginLaunch {
            instance_id: request.instance_id.clone(),
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            image_digest: version.image_digest.clone(),
            roles: metadata.roles.clone(),
            launched_by: by.by,
            launched_at_ns: self.clock.now_ns(),
            state: PluginLaunchState::Launched as i32,
            live: request.live,
            acting_through_delegation: by.delegation,
            client_name: by.client,
            note: request.note.clone(),
            prepared_call_id: by.prepared_call_id,
            ..Default::default()
        };
        // Recorded before the launcher is asked, so what runs is never
        // something the conductor has no record of.
        if !self
            .store
            .begin_launch(&launched, &request.note)
            .map_err(|f| f.to_string())?
        {
            return Err(format!(
                "{} is already launched; stop it first",
                request.instance_id
            ));
        }
        let create = self.create_request(
            &request.instance_id,
            version,
            request.live,
            archive_allowed(&snapshot, &request.instance_id).cloned(),
        );
        match self.ask_launcher::<_, CreatePluginReply>(
            CREATE_PLUGIN,
            "meridian.v1.CreatePluginRequest",
            create,
        ) {
            Ok(made) => {
                tracing::info!(
                    instance = launched.instance_id,
                    plugin = launched.name,
                    version = launched.version,
                    workload = made.workload,
                    by = launched.launched_by,
                    "a plugin launched"
                );
                // Its datasets join the data configuration (W8.3, v18).
                crate::data::publish(&self.bus, self.store.as_ref(), self.clock.now_ns());
                Ok(launched)
            }
            Err(failed) => {
                // Recorded as failed, which frees the instance to try again.
                let ending = Ending::failed(self.clock.now_ns(), failed.clone());
                self.store
                    .end_launch(&request.instance_id, &ending)
                    .map_err(|f| f.to_string())?;
                Err(format!(
                    "the launcher did not create {}: {failed}",
                    request.instance_id
                ))
            }
        }
    }

    /// What the launcher is asked to make for an instance of a recorded
    /// version.
    fn create_request(
        &self,
        instance_id: &str,
        version: &PluginVersion,
        live: bool,
        archive: Option<PluginArchive>,
    ) -> CreatePluginRequest {
        let metadata = version.metadata.clone().unwrap_or_default();
        CreatePluginRequest {
            instance_id: instance_id.to_string(),
            // plugins/{name} for a local version, plugins/{listing} for one
            // from a registry (W8.10).
            image: format!(
                "{}/{}@{}",
                self.registry,
                repository_of(version),
                version.image_digest
            ),
            roles: metadata.roles.clone(),
            interface: metadata.interface,
            // Whether the deployment is for development is the launcher's to
            // know and to refuse on (spec/live-plugin-development, ruling 2).
            live,
            // Approved with the roles: storage of its own where it asks for
            // it, and none where it asks for none (W8.3, contract v11).
            declaration: metadata.declaration,
            // The archive a deployment admin allowed it, beside its storage
            // (W8.3, W8.7, contract v16); none otherwise.
            archive,
        }
    }

    /// W8.7: a deployment admin allows an instance at the edge an archive,
    /// or changes its bound; recorded, and the instance, where the catalogue
    /// launched it, restarted through CreatePlugin carrying it (the names'
    /// choice e).
    fn allow_archive(
        &self,
        request: AllowArchiveRequest,
        envelope: &Envelope,
    ) -> Result<PluginArchive, String> {
        let by = author(envelope);
        if by.by.is_empty() {
            return Err(
                "an archive is a deployment admin's to allow, and this is sent for nobody".into(),
            );
        }
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        if self.archive.kind == ArchiveKind::None {
            return Err(format!(
                "this deployment was installed with no archive, so {} cannot be allowed one; \
                 its records past their window are kept (W7.1)",
                request.instance_id
            ));
        }
        let snapshot = self.snapshot()?;
        let Some(roles) = roles_of(&snapshot, &request.instance_id) else {
            return Err(format!(
                "{} is no plugin this deployment has launched or heard from",
                request.instance_id
            ));
        };
        if edge_roles(roles).is_empty() {
            return Err(format!(
                "{} holds {}, no edge role; only a plugin at the edge keeps raw records, and so \
                 an archive (decisions/028)",
                request.instance_id,
                if roles.is_empty() {
                    "no role".to_string()
                } else {
                    roles.join(", ")
                }
            ));
        }
        if i64::try_from(request.most_bytes).is_err() {
            return Err(format!(
                "a bound of {} bytes is past what is kept",
                request.most_bytes
            ));
        }
        let archive = PluginArchive {
            instance_id: request.instance_id.clone(),
            allowed: true,
            most_bytes: request.most_bytes,
            updated_by: by.by,
            updated_at_ns: self.clock.now_ns(),
            acting_through_delegation: by.delegation,
            client_name: by.client,
            note: request.note.clone(),
            prepared_call_id: by.prepared_call_id,
        };
        self.store
            .put_archive(&archive, &request.note)
            .map_err(|f| f.to_string())?;
        tracing::info!(
            instance = archive.instance_id,
            most_bytes = archive.most_bytes,
            by = archive.updated_by,
            "an archive allowed"
        );
        self.restart(&archive.instance_id, Some(archive.clone()))?;
        Ok(archive)
    }

    /// W8.7: a deployment admin withdraws an instance's archive; recorded,
    /// and the instance restarted without it. What the archive holds is
    /// kept. An instance never allowed one is answered as it stands, and
    /// nothing recorded.
    fn withdraw_archive(
        &self,
        request: WithdrawArchiveRequest,
        envelope: &Envelope,
    ) -> Result<PluginArchive, String> {
        let by = author(envelope);
        if by.by.is_empty() {
            return Err(
                "an archive is a deployment admin's to withdraw, and this is sent for nobody"
                    .into(),
            );
        }
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let snapshot = self.snapshot()?;
        let standing = snapshot
            .archives
            .iter()
            .find(|archive| archive.instance_id == request.instance_id)
            .cloned();
        let Some(standing) = standing.filter(|archive| archive.allowed) else {
            return Ok(PluginArchive {
                instance_id: request.instance_id,
                ..Default::default()
            });
        };
        let archive = PluginArchive {
            allowed: false,
            updated_by: by.by,
            updated_at_ns: self.clock.now_ns(),
            acting_through_delegation: by.delegation,
            client_name: by.client,
            note: request.note.clone(),
            prepared_call_id: by.prepared_call_id,
            ..standing
        };
        self.store
            .put_archive(&archive, &request.note)
            .map_err(|f| f.to_string())?;
        tracing::info!(
            instance = archive.instance_id,
            by = archive.updated_by,
            "an archive withdrawn; what it holds is kept"
        );
        self.restart(&archive.instance_id, None)?;
        Ok(archive)
    }

    /// The instance's live launch, made again by the launcher with or
    /// without its archive: removed, then created, since a mount cannot be
    /// added to a running pod (W8.7, the names' choice e). An instance the
    /// catalogue did not launch -- one the chart or a harness runs -- is
    /// given it at its next start, and nothing is restarted here.
    fn restart(&self, instance: &str, archive: Option<PluginArchive>) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        let Some(launch) = snapshot
            .catalogue
            .launches
            .iter()
            .find(|held| {
                held.instance_id == instance && held.state == PluginLaunchState::Launched as i32
            })
            .cloned()
        else {
            tracing::info!(
                instance,
                "not launched from the catalogue: given its archive, or none, at its next start"
            );
            return Ok(());
        };
        let version = snapshot
            .catalogue
            .versions
            .iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == launch.name && m.version == launch.version)
            })
            .ok_or_else(|| {
                format!(
                    "recorded; {} {} is not in the catalogue, so {instance} was not restarted",
                    launch.name, launch.version
                )
            })?;
        let _: RemovePluginReply = self
            .ask_launcher(
                REMOVE_PLUGIN,
                "meridian.v1.RemovePluginRequest",
                RemovePluginRequest {
                    instance_id: instance.to_string(),
                },
            )
            .map_err(|failed| {
                format!("recorded; the launcher did not stop {instance} to restart it: {failed}")
            })?;
        let create = self.create_request(instance, version, launch.live, archive);
        match self.ask_launcher::<_, CreatePluginReply>(
            CREATE_PLUGIN,
            "meridian.v1.CreatePluginRequest",
            create,
        ) {
            Ok(made) => {
                tracing::info!(
                    instance,
                    workload = made.workload,
                    "a plugin restarted with its archive as allowed"
                );
                Ok(())
            }
            Err(failed) => {
                let ending = Ending::failed(self.clock.now_ns(), failed.clone());
                self.store
                    .end_launch(instance, &ending)
                    .map_err(|f| f.to_string())?;
                Err(format!(
                    "recorded; the launcher did not create {instance} again: {failed}"
                ))
            }
        }
    }

    fn stop(
        &self,
        request: StopPluginRequest,
        envelope: &Envelope,
    ) -> Result<PluginLaunch, String> {
        let live = self.snapshot()?.catalogue.launches.iter().any(|held| {
            held.instance_id == request.instance_id
                && held.state == PluginLaunchState::Launched as i32
        });
        if !live {
            return Err(format!("no launch of {} is live", request.instance_id));
        }
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        // Removed first, and recorded as stopped once it is: a launcher that
        // could not be reached leaves the launch live, and stopping can be
        // asked again, rather than recorded stopped while it runs.
        let removed: RemovePluginReply = self.ask_launcher(
            REMOVE_PLUGIN,
            "meridian.v1.RemovePluginRequest",
            RemovePluginRequest {
                instance_id: request.instance_id.clone(),
            },
        )?;
        // Stamped as the launch is (W8.4, contract v17).
        let by = author(envelope);
        let ending = Ending {
            state: PluginLaunchState::Stopped,
            by: by.by,
            delegation: by.delegation,
            client: by.client,
            note: request.note.clone(),
            at_ns: self.clock.now_ns(),
            failure: String::new(),
            prepared_call_id: by.prepared_call_id,
        };
        let stopped = self
            .store
            .end_launch(&request.instance_id, &ending)
            .map_err(|f| f.to_string())?
            .ok_or_else(|| format!("no launch of {} is live", request.instance_id))?;
        tracing::info!(
            instance = stopped.instance_id,
            removed = removed.removed,
            by = stopped.stopped_by,
            "a plugin stopped"
        );
        crate::data::publish(&self.bus, self.store.as_ref(), self.clock.now_ns());
        Ok(stopped)
    }

    /// W8.2 (contract v25): one version's declaration and the vendor's texts,
    /// launched or not, retired or not.
    fn documentation(
        &self,
        request: ReadPluginDocumentationRequest,
    ) -> Result<ReadPluginDocumentationReply, String> {
        let snapshot = self.snapshot()?;
        let held = snapshot
            .catalogue
            .versions
            .iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == request.name && m.version == request.version)
            })
            .ok_or_else(|| {
                format!(
                    "{} {} is not in the catalogue",
                    request.name, request.version
                )
            })?;
        Ok(ReadPluginDocumentationReply {
            declaration: held.metadata.as_ref().and_then(|m| m.declaration.clone()),
            documentation: self
                .store
                .plugin_documentation(&request.name, &request.version)
                .map_err(|f| f.to_string())?,
            uploaded_by: held.uploaded_by.clone(),
            uploaded_at_ns: held.uploaded_at_ns,
        })
    }

    /// W8.15 (contract v25; decision 59): retire a version no instance runs
    /// or ran and no record names, with a note -- recorded, its image then
    /// released from the deployment's own registry.
    fn retire(
        &self,
        request: RetirePluginVersionRequest,
        envelope: &Envelope,
    ) -> Result<PluginVersion, String> {
        if request.name.is_empty() {
            return Err("name: names the version's plugin".into());
        }
        if request.version.is_empty() {
            return Err("version: names the version to retire".into());
        }
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let snapshot = self.snapshot()?;
        let held = snapshot
            .catalogue
            .versions
            .iter()
            .find(|held| {
                held.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == request.name && m.version == request.version)
            })
            .ok_or_else(|| {
                format!(
                    "{} {} is not in the catalogue",
                    request.name, request.version
                )
            })?;
        if held.retired_at_ns != 0 {
            return Err(format!(
                "{} {} was retired already, by {}",
                request.name, request.version, held.retired_by
            ));
        }
        // An instance, live or stopped, names it: its launch is kept in
        // history, and the lake's rows and the book's entries it recorded
        // name the version through it (the lake stamps each row's source
        // with the version its instance was launched at).
        let mut naming: Vec<String> = snapshot
            .catalogue
            .launches
            .iter()
            .filter(|launch| launch.name == request.name && launch.version == request.version)
            .map(|launch| {
                let state = match PluginLaunchState::try_from(launch.state) {
                    Ok(PluginLaunchState::Launched) => "running",
                    Ok(PluginLaunchState::Stopped) => "stopped",
                    Ok(PluginLaunchState::Failed) => "failed to launch",
                    _ => "launched",
                };
                format!("{} ({state})", launch.instance_id)
            })
            .collect();
        naming.sort();
        naming.dedup();
        if !naming.is_empty() {
            return Err(format!(
                "{} {} is named by {}: an instance's launch is kept in history, and what it \
                 recorded names the version, so it stays in the catalogue",
                request.name,
                request.version,
                naming.join(", ")
            ));
        }
        let mut by = author(envelope);
        by.note = request.note.clone();
        let retired = self
            .store
            .retire_plugin_version(&request.name, &request.version, &by, self.clock.now_ns())
            .map_err(|f| f.to_string())?
            .ok_or_else(|| format!("{} {} was retired already", request.name, request.version))?;
        tracing::info!(
            plugin = request.name,
            version = request.version,
            by = retired.retired_by,
            "a plugin version retired"
        );
        // Its image released, unless a version still offered from the same
        // repository runs the same image.
        let repository = repository_of(&retired);
        let shared = snapshot.catalogue.versions.iter().any(|other| {
            other.retired_at_ns == 0
                && other.image_digest == retired.image_digest
                && repository_of(other) == repository
                && other.metadata.as_ref().map(|m| m.version.as_str())
                    != Some(request.version.as_str())
        });
        match (&self.local, shared) {
            (Some(local), false) => {
                let releasing = local.release(&repository, &retired.image_digest);
                if let Err(failed) = tokio::runtime::Handle::current().block_on(releasing) {
                    // Recorded either way: what stays in the registry is never
                    // launched again, and a later retirement cannot repeat.
                    tracing::warn!(
                        plugin = request.name,
                        version = request.version,
                        %failed,
                        "a retired version's image was not released"
                    );
                }
            }
            (None, false) => tracing::info!(
                plugin = request.name,
                "no registry to release a retired version's image from"
            ),
            (_, true) => {}
        }
        Ok(retired)
    }
}

/// Serve W8's commands and query. `registry` is where a node reaches the
/// deployment's own registry, `localhost:<port>`. A deployment with no
/// archive ([`serve_plugins_with`] names one).
pub fn serve_plugins(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    registry: String,
) {
    serve_plugins_with(bus, store, clock, registry, ArchiveGrant::default())
}

/// [`serve_plugins`], for a deployment whose install named where archives
/// are kept (W7.1), so an archive may be allowed (W8.7, contract v16).
pub fn serve_plugins_with(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    registry: String,
    archive: ArchiveGrant,
) {
    serve_plugins_reaching(bus, store, clock, registry, archive, None)
}

/// [`serve_plugins_with`], reaching the deployment's own registry as the
/// conductor does inside the cluster (contract v25): a retired version's
/// image released there (W8.15).
pub fn serve_plugins_reaching(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    registry: String,
    archive: ArchiveGrant,
    local: Option<Arc<dyn LocalRegistry>>,
) {
    serve_plugins_and_registries(bus, store, clock, registry, archive, local, None)
}

/// [`serve_plugins_reaching`], reaching the community and verified
/// registries too (contract v25, W8.10 to W8.12, W5.19): Browse's copy of
/// the catalogue, installs, updates, pull acknowledgements and the plugin
/// usage report's setting. Without them those rows answer that the
/// deployment reaches no registry.
pub fn serve_plugins_and_registries(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    registry: String,
    archive: ArchiveGrant,
    local: Option<Arc<dyn LocalRegistry>>,
    registries: Option<Arc<crate::registries::Hold>>,
) {
    let plugins = Arc::new(Plugins {
        bus: Arc::clone(&bus),
        store,
        clock,
        registry,
        archive,
        local,
        registries,
        packs: packs::Jobs::default(),
    });
    // A pack install a restart interrupted, carried on (616).
    plugins.resume_pack_jobs();
    answer_on(
        &bus,
        &plugins,
        REGISTRY_PLUGINS,
        (
            "meridian.v1.ListRegistryPluginsRequest",
            "meridian.v1.ListRegistryPluginsReply",
        ),
        |plugins, request, _| plugins.list_registry_plugins(request),
    );
    answer_on(
        &bus,
        &plugins,
        INSTALL_PLUGIN,
        (
            "meridian.v1.InstallPluginRequest",
            "meridian.v1.PluginVersion",
        ),
        |plugins, request, envelope| plugins.install(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        UPDATE_PLUGIN,
        (
            "meridian.v1.UpdatePluginRequest",
            "meridian.v1.PluginLaunch",
        ),
        |plugins, request, envelope| plugins.update(request, envelope),
    );
    // Starter packs (contract v27's wire, built in v28; W8.18, W8.19).
    answer_on(
        &bus,
        &plugins,
        packs::REGISTRY_PACKS,
        (
            "meridian.v1.ListRegistryPacksRequest",
            "meridian.v1.ListRegistryPacksReply",
        ),
        |plugins, request, _| plugins.list_registry_packs(request),
    );
    let installing = Arc::clone(&plugins);
    answer_on(
        &bus,
        &plugins,
        packs::INSTALL_PACK,
        ("meridian.v1.InstallPackRequest", "meridian.v1.PackInstall"),
        move |_, request, envelope| installing.install_pack(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        packs::READ_PACK,
        ("meridian.v1.ReadPackRequest", "meridian.v1.PackStatus"),
        |plugins, request, _| plugins.read_pack(request),
    );
    answer_on(
        &bus,
        &plugins,
        packs::UPDATE_PACK,
        ("meridian.v1.UpdatePackRequest", "meridian.v1.PackStatus"),
        |plugins, request, envelope| plugins.update_pack(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        packs::REMOVE_PACK,
        ("meridian.v1.RemovePackRequest", "meridian.v1.PackStatus"),
        |plugins, request, envelope| plugins.remove_pack(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        ACKNOWLEDGE_PULL,
        (
            "meridian.v1.AcknowledgePullRequest",
            "meridian.v1.PullAcknowledgement",
        ),
        |plugins, request, envelope| plugins.acknowledge_pull(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        SET_REPORTS_PLUGIN_USAGE,
        (
            "meridian.v1.SetReportsPluginUsageRequest",
            "meridian.v1.ReportsPluginUsageSetting",
        ),
        |plugins, request, envelope| plugins.set_reports_plugin_usage(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        LIST_PLUGIN_USAGE_REPORTS,
        (
            "meridian.v1.ListPluginUsageReportsRequest",
            "meridian.v1.ListPluginUsageReportsReply",
        ),
        |plugins, request, _| plugins.list_plugin_usage_reports(request),
    );
    answer_on(
        &bus,
        &plugins,
        ALLOW_ARCHIVE,
        (
            "meridian.v1.AllowArchiveRequest",
            "meridian.v1.PluginArchive",
        ),
        |plugins, request: AllowArchiveRequest, envelope| plugins.allow_archive(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        WITHDRAW_ARCHIVE,
        (
            "meridian.v1.WithdrawArchiveRequest",
            "meridian.v1.PluginArchive",
        ),
        |plugins, request: WithdrawArchiveRequest, envelope| {
            plugins.withdraw_archive(request, envelope)
        },
    );
    answer_on(
        &bus,
        &plugins,
        RECORD_PLUGIN_UPLOAD,
        (
            "meridian.v1.RecordPluginUploadRequest",
            "meridian.v1.PluginVersion",
        ),
        |plugins, request: RecordPluginUploadRequest, envelope| {
            plugins.record_upload(request, envelope)
        },
    );
    answer_on(
        &bus,
        &plugins,
        PLUGIN_CATALOGUE,
        (
            "meridian.v1.PluginCatalogueRequest",
            "meridian.v1.PluginCatalogue",
        ),
        |plugins, request: PluginCatalogueRequest, _| -> Result<PluginCatalogue, String> {
            // A retired version is no longer listed (W8.15, contract v25);
            // its record is kept.
            let mut catalogue = plugins.snapshot()?.catalogue;
            catalogue.versions.retain(|held| held.retired_at_ns == 0);
            // One page of it (contract v28's amendment, ruling 696).
            catalogue::page(catalogue, &request)
        },
    );
    answer_on(
        &bus,
        &plugins,
        PLUGIN_DOCUMENTATION,
        (
            "meridian.v1.ReadPluginDocumentationRequest",
            "meridian.v1.ReadPluginDocumentationReply",
        ),
        |plugins, request: ReadPluginDocumentationRequest, _| plugins.documentation(request),
    );
    answer_on(
        &bus,
        &plugins,
        RETIRE_PLUGIN_VERSION,
        (
            "meridian.v1.RetirePluginVersionRequest",
            "meridian.v1.PluginVersion",
        ),
        |plugins, request: RetirePluginVersionRequest, envelope| plugins.retire(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        LAUNCH_PLUGIN,
        (
            "meridian.v1.LaunchPluginRequest",
            "meridian.v1.PluginLaunch",
        ),
        |plugins, request: LaunchPluginRequest, envelope| plugins.launch(request, envelope),
    );
    answer_on(
        &bus,
        &plugins,
        STOP_PLUGIN,
        ("meridian.v1.StopPluginRequest", "meridian.v1.PluginLaunch"),
        |plugins, request: StopPluginRequest, envelope| plugins.stop(request, envelope),
    );
}

#[cfg(test)]
mod tests;
