//! The community and verified registries, as the conductor holds them
//! (W8.10 to W8.12, W5.19; contract v25; spec/plugins-are-shared-through-
//! registries).
//!
//! - **The catalogue** (requirement 32): pulled from the platform
//!   (`PullCatalogue`, after the last sequence held) hourly and on demand,
//!   its signing keys' status checked against the chart's keys, and kept;
//!   every page reads the copy, so the platform is never reached in a
//!   page's request.
//! - **Fetching a version** (requirements 10, 24, 33): its floor checked
//!   against this deployment's sidecar, then one resolution request, the
//!   listing statement from the reply verified and held to the catalogue's,
//!   the image and its referrers copied by digest into the deployment's own
//!   registry, the provenance and the image's metadata verified, the signer
//!   held to the key status, and the declaration recorded checked to be the
//!   built image's. A check that fails or cannot be made refuses, naming it.
//! - **At launch** (W8.3): the kept statement verified again, with the
//!   referrers in the local registry and the newest key status held.
//!
//! The platform and the registries are reached through [`Registries`],
//! which the conductor's binary implements over HTTPS; everything decided
//! is decided here.

pub mod usage;
pub mod verify;
pub mod wire;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use meridian_clock::Clock;
use meridian_domain::v1::{
    DocumentationText, ListRegistryPluginsReply, ListRegistryPluginsRequest, ListedVersion,
    PluginListing, PluginMetadata, PluginUsageReport, PluginVersion, SigningKeyStatus,
};
use serde_json::Value;

use crate::store::{HeldCatalogue, Store};
use verify::PlatformKeys;

/// A future the conductor's binary answers.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The platform's registry rows and the registries' bytes, as the
/// conductor reaches them (contract v25): JSON as the platform answers it,
/// so its signatures can be checked here.
pub trait Registries: Send + Sync {
    /// PullCatalogue: listings changed after `after_sequence`, and the key
    /// status.
    fn pull_catalogue(&self, after_sequence: u64) -> Fut<'_, Result<Value, String>>;
    /// DownloadListedVersion: one resolution request, signed as the
    /// deployment and counted by the platform.
    fn resolve<'a>(
        &'a self,
        listing: &'a str,
        version: &'a str,
        registry: &'a str,
    ) -> Fut<'a, Result<Value, String>>;
    /// Copy `pull_from@digest`, with its referrers, into the deployment's
    /// own registry at `repository` (with `token` where the reply carried
    /// one), answering each referrer's DSSE envelope as JSON.
    fn copy<'a>(
        &'a self,
        pull_from: &'a str,
        token: &'a str,
        digest: &'a str,
        repository: &'a str,
    ) -> Fut<'a, Result<Vec<String>, String>>;
    /// The DSSE envelopes the deployment's own registry holds as referrers
    /// of `repository@digest`, for the verification at launch.
    fn referrers<'a>(
        &'a self,
        repository: &'a str,
        digest: &'a str,
    ) -> Fut<'a, Result<Vec<String>, String>>;
    /// ReportPluginUsage: whether the platform received it.
    fn report_usage<'a>(&'a self, report: &'a PluginUsageReport) -> Fut<'a, Result<bool, String>>;
}

/// What a fetch verified, for the version to be recorded.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub version: PluginVersion,
    pub statement: String,
    pub listed: verify::Listed,
    pub documentation: Vec<DocumentationText>,
}

/// The registries' state the conductor keeps, and what it decides.
pub struct Hold {
    upstream: Arc<dyn Registries>,
    keys: PlatformKeys,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    /// One pull at a time: a second waits and reads what the first kept.
    pulling: tokio::sync::Mutex<()>,
}

pub use meridian_domain::semver::{is_prerelease, semver_cmp};

/// Whether a listed version is offered for install or update: neither
/// withdrawn by its vendor nor pulled (requirements 30, 37, 42).
pub fn offered(version: &ListedVersion) -> bool {
    version.withdrawn_at_ns == 0
        && version
            .pull
            .as_ref()
            .is_none_or(|pull| pull.restored_at_ns != 0)
}

/// The floor of a listed version against this deployment's sidecar: the
/// contract it declares, which the sidecar must serve (requirement 35).
pub fn floor_refused(version: &ListedVersion) -> Option<String> {
    meridian_sidecar::contract::admit(&version.schema_version)
        .err()
        .map(|why| {
            format!(
                "the floor: {} {} declares {why}",
                version.listing, version.version
            )
        })
}

/// The organisation part of a listing.
fn organisation_of(listing: &str) -> &str {
    listing
        .split_once('/')
        .map_or(listing, |(organisation, _)| organisation)
}

/// One listed version from a copy of the catalogue.
fn listed_in(
    held: Option<HeldCatalogue>,
    listing: &str,
    version: &str,
    registry: &str,
) -> Result<(PluginListing, ListedVersion), String> {
    let held = held.ok_or("the catalogue has not been pulled yet; pull it first")?;
    let found = held
        .listings
        .into_iter()
        .find(|held| held.listing == listing)
        .ok_or_else(|| format!("no listing {listing} is in the catalogue"))?;
    let chosen = found
        .versions
        .iter()
        .find(|held| held.version == version && (registry.is_empty() || held.registry == registry))
        .cloned()
        .ok_or_else(|| {
            format!(
                "{listing} has no {}version {version}",
                if registry.is_empty() {
                    String::new()
                } else {
                    format!("{registry} ")
                }
            )
        })?;
    Ok((found, chosen))
}

impl Hold {
    pub fn new(
        upstream: Arc<dyn Registries>,
        keys: PlatformKeys,
        store: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
    ) -> Hold {
        Hold {
            upstream,
            keys,
            store,
            clock,
            pulling: tokio::sync::Mutex::new(()),
        }
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    pub fn upstream(&self) -> &Arc<dyn Registries> {
        &self.upstream
    }

    /// The copy held, none before the first pull. For a caller on a thread
    /// that may block; the async paths below read it through `read_held`,
    /// since the store blocks.
    pub fn held(&self) -> Result<Option<HeldCatalogue>, String> {
        self.store
            .registry_catalogue()
            .map_err(|failed| failed.to_string())
    }

    /// The copy held, read on a blocking thread: the store blocks, and an
    /// async task -- the hourly pull, a fetch the bus's handler waits on --
    /// must not block its runtime's.
    async fn read_held(&self) -> Result<Option<HeldCatalogue>, String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            store
                .registry_catalogue()
                .map_err(|failed| failed.to_string())
        })
        .await
        .map_err(|failed| format!("the catalogue's copy could not be read: {failed}"))?
    }

    /// The copy kept, written on a blocking thread, as `read_held` reads.
    async fn keep(&self, kept: HeldCatalogue) -> Result<(), String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            store
                .put_registry_catalogue(&kept)
                .map_err(|failed| failed.to_string())
        })
        .await
        .map_err(|failed| format!("the catalogue's copy could not be kept: {failed}"))?
    }

    /// Pull the catalogue after the sequence held, merge what changed --
    /// each listing replaced whole -- and keep it, the key status checked
    /// against the chart's keys first. A pull that fails changes nothing.
    pub async fn pull(&self) -> Result<HeldCatalogue, String> {
        let _one = self.pulling.lock().await;
        let held = self.read_held().await?.unwrap_or_default();
        let answered = self.upstream.pull_catalogue(held.sequence).await?;
        let reply = wire::catalogue(&answered)
            .map_err(|why| format!("the catalogue did not read: {why}"))?;
        let key_status = match answered
            .get("key_status")
            .filter(|status| status.is_object())
        {
            Some(status) => {
                verify::key_status_signed(status, &self.keys)?;
                status.to_string()
            }
            None if !held.key_status.is_empty() => held.key_status.clone(),
            None => return Err("the catalogue carried no signing keys' status".into()),
        };
        let mut listings = held.listings;
        for listing in reply.listings {
            match listings
                .iter_mut()
                .find(|held| held.listing == listing.listing)
            {
                Some(slot) => *slot = listing,
                None => listings.push(listing),
            }
        }
        listings.sort_by(|a, b| a.listing.cmp(&b.listing));
        // The packs (contract v27, requirement 14): each touched since the
        // sequence arrives whole and replaces what was held of it.
        let mut packs = held.packs;
        for pack in reply.packs {
            match packs.iter_mut().find(|held| held.pack == pack.pack) {
                Some(slot) => *slot = pack,
                None => packs.push(pack),
            }
        }
        packs.sort_by(|a, b| a.pack.cmp(&b.pack));
        let kept = HeldCatalogue {
            listings,
            sequence: reply.sequence.max(held.sequence),
            key_status,
            pulled_at_ns: self.clock.now_ns(),
            pull_status: held.pull_status,
            packs,
        };
        self.keep(kept.clone()).await?;
        Ok(kept)
    }

    /// The newest key status held, its signature checked again.
    pub fn key_status(&self) -> Result<SigningKeyStatus, String> {
        self.key_status_of(self.held()?)
    }

    fn key_status_of(&self, held: Option<HeldCatalogue>) -> Result<SigningKeyStatus, String> {
        let held = held
            .filter(|held| !held.key_status.is_empty())
            .ok_or("the signing key: no key status is held; pull the catalogue first")?;
        let document: Value = serde_json::from_str(&held.key_status)
            .map_err(|_| "the signing key: the key status held does not read".to_string())?;
        verify::key_status_signed(&document, &self.keys)
            .map_err(|why| format!("the signing key: {why}"))?;
        Ok(wire::key_status(&document))
    }

    /// The listings a copy holds, with the pulls a bundle brought applied
    /// (ruling 196): what a starter pack's members resolve against (W8.18).
    pub fn listings_of(&self, held: &HeldCatalogue) -> Vec<PluginListing> {
        with_bundle_pulls(held.listings.clone(), held, &self.keys)
    }

    /// One listed version from the copy held.
    pub fn listed(
        &self,
        listing: &str,
        version: &str,
        registry: &str,
    ) -> Result<(PluginListing, ListedVersion), String> {
        listed_in(self.held()?, listing, version, registry)
    }

    /// ListRegistryPlugins (W8.10): Browse and a listing's page, from the
    /// copy held, pulled first where asked.
    pub async fn list(
        &self,
        request: &ListRegistryPluginsRequest,
    ) -> Result<ListRegistryPluginsReply, String> {
        let held = if request.pull_now {
            self.pull().await?
        } else {
            self.read_held().await?.unwrap_or_default()
        };
        let words: Vec<String> = request
            .search
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut listings: Vec<PluginListing> =
            with_bundle_pulls(held.listings.clone(), &held, &self.keys)
                .into_iter()
                .filter(|listing| request.listing.is_empty() || listing.listing == request.listing)
                .filter(|listing| {
                    request.registry.is_empty()
                        || listing
                            .versions
                            .iter()
                            .any(|v| v.registry == request.registry)
                })
                .filter(|listing| {
                    request.role.is_empty()
                        || listing.versions.iter().any(|v| {
                            v.metadata
                                .as_ref()
                                .is_some_and(|m| m.roles.contains(&request.role))
                        })
                })
                .filter(|listing| {
                    let haystack = format!(
                        "{} {} {}",
                        listing.listing,
                        listing.regions.join(" "),
                        listing.asset_classes.join(" ")
                    )
                    .to_lowercase();
                    words.iter().all(|word| haystack.contains(word))
                })
                .collect();
        // Ranked by name match: a listing whose plugin part is a word first.
        listings.sort_by_key(|listing| {
            let plugin = listing.listing.split('/').nth(1).unwrap_or_default();
            (
                !words.iter().any(|word| plugin == word),
                listing.listing.clone(),
            )
        });
        let size = usize::try_from(request.page_size)
            .ok()
            .filter(|size| *size > 0)
            .unwrap_or(50)
            .min(200);
        let start: usize = request.cursor.parse().unwrap_or(0);
        let next = start + size;
        let next_cursor = if next < listings.len() {
            next.to_string()
        } else {
            String::new()
        };
        Ok(ListRegistryPluginsReply {
            listings: listings.into_iter().skip(start).take(size).collect(),
            next_cursor,
            catalogue_pulled_at_ns: held.pulled_at_ns,
        })
    }

    /// Resolve, pull and verify a listed version (requirements 10, 24, 33),
    /// answering what to record; nothing is recorded here.
    pub async fn fetch(
        &self,
        listing: &str,
        version: &str,
        registry: &str,
    ) -> Result<Fetched, String> {
        let held = self.read_held().await?;
        let (_, chosen) = listed_in(held.clone(), listing, version, registry)?;
        if !offered(&chosen) {
            return Err(format!(
                "{listing} {version} is not offered: it was {}",
                if chosen.withdrawn_at_ns != 0 {
                    "withdrawn by its vendor"
                } else {
                    "pulled"
                }
            ));
        }
        if let Some(refusal) = floor_refused(&chosen) {
            return Err(refusal);
        }
        let reply = wire::resolution(
            &self
                .upstream
                .resolve(listing, version, &chosen.registry)
                .await?,
        )?;
        if reply.image_digest != chosen.image_digest {
            return Err(format!(
                "the digest: the resolution names {} and the catalogue {}",
                reply.image_digest, chosen.image_digest
            ));
        }
        if reply.listing_statement.trim().is_empty() {
            return Err("the listing statement: the resolution carried none".into());
        }
        if !chosen.listing_statement.is_empty()
            && !same_envelope(&reply.listing_statement, &chosen.listing_statement)
        {
            return Err("the listing statement: the resolution's is not the catalogue's".into());
        }
        let listed = verify::listing_statement(
            &reply.listing_statement,
            &self.keys,
            listing,
            version,
            &chosen.registry,
            &chosen.image_digest,
        )?;
        let repository = format!("plugins/{listing}");
        let envelopes = self
            .upstream
            .copy(
                &reply.pull_from,
                &reply.pull_token,
                &chosen.image_digest,
                &repository,
            )
            .await?;
        let key_status = self.key_status_of(held)?;
        self.verified(
            Asked {
                listing,
                version,
                registry: &chosen.registry,
                digest: &chosen.image_digest,
                catalogued: chosen.metadata.as_ref(),
            },
            &envelopes,
            &listed,
            &key_status,
        )
        .map(|(version, documentation)| Fetched {
            version,
            statement: reply.listing_statement,
            listed,
            documentation,
        })
    }

    /// A version carried in by a plugin bundle (W8.14; the spec's
    /// requirement 45), its image and referrers already in the deployment's
    /// own registry under plugins/{listing}: verified against the chart's
    /// keys as at install -- the listing statement, the provenance and the
    /// image's metadata among the referrers, the signer against the newer of
    /// the bundle's key status and the one held -- and answered with the key
    /// status's issue time. Nothing is recorded here; the bundle's key
    /// status is kept as the newest held where it is newer, and only once
    /// the bundle verified.
    pub async fn carried(&self, bundle: &Bundle<'_>) -> Result<Fetched, String> {
        if !matches!(bundle.registry, "community" | "verified") {
            return Err(format!(
                "the source registry: {:?} is neither community nor verified",
                bundle.registry
            ));
        }
        let document: Value = serde_json::from_str(bundle.key_status)
            .map_err(|_| "the key status: the bundle's does not read".to_string())?;
        verify::key_status_signed(&document, &self.keys)
            .map_err(|why| format!("the key status: {why}"))?;
        let carried = wire::key_status(&document);
        // Contract v26 (ruling 196): the bundle's pull status, checked
        // against the chart's keys before anything is kept.
        let pulls = if bundle.pull_status.trim().is_empty() {
            None
        } else {
            let document: Value = serde_json::from_str(bundle.pull_status)
                .map_err(|_| "the pull status: the bundle's does not read".to_string())?;
            verify::pull_status_signed(&document, &self.keys)
                .map_err(|why| format!("the pull status: {why}"))?;
            let issued = wire::pull_status(&document)
                .map(|status| status.issued_at_ns)
                .unwrap_or_default();
            Some((document, issued))
        };
        let listed = verify::listing_statement(
            bundle.listing_statement,
            &self.keys,
            bundle.listing,
            bundle.version,
            bundle.registry,
            bundle.digest,
        )?;
        if !listed.floor.is_empty() {
            meridian_sidecar::contract::admit(&listed.floor).map_err(|why| {
                format!(
                    "the floor: {} {} declares {why}",
                    bundle.listing, bundle.version
                )
            })?;
        }
        let held = self.read_held().await?;
        let newest_held = held
            .as_ref()
            .filter(|held| !held.key_status.is_empty())
            .and_then(|held| serde_json::from_str::<Value>(&held.key_status).ok())
            .filter(|document| verify::key_status_signed(document, &self.keys).is_ok())
            .map(|document| wire::key_status(&document))
            .filter(|status| status.issued_at_ns > carried.issued_at_ns);
        let checked = newest_held.unwrap_or_else(|| carried.clone());
        let repository = format!("plugins/{}", bundle.listing);
        let envelopes = self.upstream.referrers(&repository, bundle.digest).await?;
        let (mut version, documentation) = self.verified(
            Asked {
                listing: bundle.listing,
                version: bundle.version,
                registry: bundle.registry,
                digest: bundle.digest,
                catalogued: None,
            },
            &envelopes,
            &listed,
            &checked,
        )?;
        version.key_status_issued_at_ns = checked.issued_at_ns;
        let fetched = Fetched {
            version,
            statement: bundle.listing_statement.to_string(),
            listed,
            documentation,
        };
        let mut kept = held.unwrap_or_default();
        let mut changed = false;
        if checked.issued_at_ns == carried.issued_at_ns {
            kept.key_status = document.to_string();
            changed = true;
        }
        // The newer pull status kept: the bundle's, where it was issued
        // after the one held.
        if let Some((document, issued)) = pulls {
            let held_issued = serde_json::from_str::<Value>(&kept.pull_status)
                .ok()
                .and_then(|held| wire::pull_status(&held))
                .map_or(i64::MIN, |status| status.issued_at_ns);
            if issued > held_issued {
                kept.pull_status = document.to_string();
                changed = true;
            }
        }
        if changed {
            self.keep(kept).await?;
        }
        Ok(fetched)
    }

    /// What install and a bundle share once the listing statement verified
    /// and the referrers are in hand (requirements 20, 22, 24, 25): the
    /// provenance's signer, trusted at the build; the declaration the built
    /// image carries; and the roles a launch may approve. Answers the version
    /// to record and its documentation's texts, from the image's verified
    /// metadata.
    fn verified(
        &self,
        asked: Asked<'_>,
        envelopes: &[String],
        listed: &verify::Listed,
        key_status: &SigningKeyStatus,
    ) -> Result<(PluginVersion, Vec<DocumentationText>), String> {
        let attested = self.attestations(envelopes, asked.digest, listed)?;
        verify::signer_trusted(
            key_status,
            organisation_of(asked.listing),
            &listed.signer_key,
            attested.built_at_ns.unwrap_or(listed.listed_at_ns),
        )?;
        // The declaration recorded is the built image's (requirement 24).
        if let Some(catalogued) = asked.catalogued {
            if catalogued.declaration != attested.metadata.declaration {
                return Err(
                    "the declaration: the catalogue's is not the one the built image carries"
                        .into(),
                );
            }
        }
        // What is approved is what is signed (requirement 25): a verified
        // version's roles are its review's; a community version's, its
        // declaration's.
        let roles = if asked.registry == "verified" {
            listed.approved_roles.clone()
        } else {
            attested.metadata.roles.clone()
        };
        if asked.registry == "community" && listed.roles != roles {
            return Err("the roles: the listing statement's are not the built image's".into());
        }
        let metadata = PluginMetadata {
            name: asked.listing.to_string(),
            version: asked.version.to_string(),
            roles,
            interface: attested.metadata.interface,
            sdk_version: attested.metadata.sdk_version.clone(),
            declaration: attested.metadata.declaration.clone(),
        };
        let version = PluginVersion {
            metadata: Some(metadata),
            image_digest: asked.digest.to_string(),
            source_registry: asked.registry.to_string(),
            listing: asked.listing.to_string(),
            uploaded_at_ns: self.clock.now_ns(),
            ..Default::default()
        };
        Ok((version, attested.documentation))
    }

    /// The provenance and the image's metadata among `envelopes`, each
    /// verified (requirements 20, 22, 24).
    fn attestations(
        &self,
        envelopes: &[String],
        digest: &str,
        listed: &verify::Listed,
    ) -> Result<Attested, String> {
        let find = |predicate: &str| {
            let mut last = format!("none of the image's referrers is its {predicate}");
            for envelope in envelopes {
                match verify::statement(envelope, &self.keys, predicate, digest) {
                    Ok(statement) => return Ok(statement),
                    Err(why) if why.contains("it states") => continue,
                    Err(why) => last = why,
                }
            }
            Err(last)
        };
        let provenance =
            find(verify::PROVENANCE_PREDICATE).map_err(|why| format!("the provenance: {why}"))?;
        let signer = provenance["predicate"]["signer"]
            .as_str()
            .unwrap_or_default();
        if signer != listed.signer_key {
            return Err(format!(
                "the provenance: it names the signer key {signer:?}, the listing statement {:?}",
                listed.signer_key
            ));
        }
        let metadata_statement = find(verify::METADATA_PREDICATE)
            .map_err(|why| format!("the image's metadata: {why}"))?;
        let predicate = &metadata_statement["predicate"];
        let metadata = wire_metadata(&predicate["metadata"])
            .map_err(|why| format!("the image's metadata: {why}"))?;
        let documentation: Vec<DocumentationText> = predicate["documentation"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|section| {
                Some(DocumentationText {
                    section: section["name"].as_str()?.to_string(),
                    markdown: section["text"].as_str()?.to_string(),
                })
            })
            .collect();
        let built_at_ns = ["runDetails", "metadata"]
            .iter()
            .try_fold(&provenance["predicate"], |at, step| at.get(step))
            .and_then(|meta| meta["startedOn"].as_str().or(meta["finishedOn"].as_str()))
            .and_then(verify::rfc3339_ns);
        Ok(Attested {
            metadata,
            documentation,
            built_at_ns,
        })
    }

    /// The verification again at launch (W8.3; requirement 24): the kept
    /// statement, the referrers the deployment's own registry holds, and
    /// the newest key status.
    pub async fn reverify(&self, version: &PluginVersion, statement: &str) -> Result<(), String> {
        let metadata = version.metadata.clone().unwrap_or_default();
        let listed = verify::listing_statement(
            statement,
            &self.keys,
            &version.listing,
            &metadata.version,
            &version.source_registry,
            &version.image_digest,
        )?;
        let repository = format!("plugins/{}", version.listing);
        let envelopes = self
            .upstream
            .referrers(&repository, &version.image_digest)
            .await?;
        let attested = self.attestations(&envelopes, &version.image_digest, &listed)?;
        verify::signer_trusted(
            &self.key_status_of(self.read_held().await?)?,
            organisation_of(&version.listing),
            &listed.signer_key,
            attested.built_at_ns.unwrap_or(listed.listed_at_ns),
        )?;
        if attested.metadata.declaration != metadata.declaration {
            return Err(
                "the declaration: the one recorded is not the one the built image carries".into(),
            );
        }
        Ok(())
    }
}

/// Two DSSE envelopes as JSON, the same envelope: compared as JSON, so a
/// difference in spacing or key order is none.
fn same_envelope(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// What a verification is about.
struct Asked<'a> {
    listing: &'a str,
    version: &'a str,
    registry: &'a str,
    digest: &'a str,
    /// The catalogue's metadata for the version, where one is held: an
    /// install's, never a bundle's.
    catalogued: Option<&'a PluginMetadata>,
}

/// The catalogue's copy with the pulls a bundle brought applied (W8.14;
/// ruling 196): each pulled version the newest pull status names takes its
/// pull, or its restoration, where it is newer than what the copy holds; a
/// status whose signature no longer checks is passed over.
pub fn with_bundle_pulls(
    mut listings: Vec<PluginListing>,
    held: &HeldCatalogue,
    keys: &PlatformKeys,
) -> Vec<PluginListing> {
    let Some(status) = serde_json::from_str::<Value>(&held.pull_status)
        .ok()
        .filter(|document| verify::pull_status_signed(document, keys).is_ok())
        .and_then(|document| wire::pull_status(&document))
    else {
        return listings;
    };
    for pull in status.pulls {
        let Some(listing) = listings.iter_mut().find(|l| l.listing == pull.listing) else {
            continue;
        };
        for version in listing
            .versions
            .iter_mut()
            .filter(|v| v.version == pull.version)
        {
            let newer = version.pull.as_ref().is_none_or(|had| {
                pull.pulled_at_ns.max(pull.restored_at_ns)
                    > had.pulled_at_ns.max(had.restored_at_ns)
            });
            if newer {
                version.pull = Some(pull.clone());
            }
        }
    }
    listings
}

/// A plugin bundle as `meridian plugin upload --bundle` names it (W8.14):
/// its image and referrers already pushed under plugins/{listing}.
#[derive(Debug, Clone, Copy)]
pub struct Bundle<'a> {
    pub listing: &'a str,
    pub version: &'a str,
    /// `community` or `verified`.
    pub registry: &'a str,
    pub digest: &'a str,
    /// The bundle's dated SigningKeyStatus, as the platform signed it.
    pub key_status: &'a str,
    pub listing_statement: &'a str,
    /// Contract v26 (ruling 196): the bundle's dated PluginPullStatus, as
    /// the platform signed it; empty for a v25 bundle.
    pub pull_status: &'a str,
}

struct Attested {
    metadata: PluginMetadata,
    documentation: Vec<DocumentationText>,
    built_at_ns: Option<i64>,
}

fn wire_metadata(value: &Value) -> Result<PluginMetadata, String> {
    // The same reading as the catalogue's: a listed version's metadata.
    let holder = serde_json::json!({
        "listing": "a/b",
        "version": "0",
        "registry": "community",
        "image_digest": format!("sha256:{}", "0".repeat(64)),
        "metadata": value,
    });
    wire::listed_version(&holder, "metadata").map(|version| version.metadata.unwrap_or_default())
}

#[cfg(test)]
pub(crate) mod tests;
