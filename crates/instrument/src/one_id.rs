//! Every instrument named by the platform master's ID (contract v24;
//! decisions/037; spec/the-platform-master-mints-every-instrument-from-a-template,
//! requirements 1 to 26; the one-ID model ruled 2026-10-14).
//!
//! What the store holds of the platform's catalogue and licence list, what
//! it sends the platform and what it never does, how a record of the
//! platform's is kept from the start, how a provisional record resolves to
//! one, and how the replacement feed and a `reference` plugin's offer are
//! followed.
//!
//! **Only open identifiers and definitions travel** (W3's invariant): a
//! request carries identifiers in schemes on the licence list the store read
//! last -- never the `LCL-` ID, an identifier under `open_meridian_local`, or
//! the deployment's name -- and what the record's sources state, offered.
//! [`sendable`] is the one filter, and its tests name it.

use std::collections::BTreeSet;

use meridian_domain::v1::{
    AssetClass, EscalateInstrumentRequest, Identifier as PbIdentifier, InstrumentRecord,
    InstrumentReplacedEvent, InstrumentType, LicenceList, LicencePace, ProvisionalReason,
    ReplacementCause, ResolveIdentifierRequest, SecurityTemplate, TemplateField,
    TemplateFieldValue,
};

use crate::record::{asked_from_wire, asset_class_name, to_wire};
use crate::resolve::{identifier_change, window_for};
use crate::store::{
    Asked, Field, Identifier, IdentifierSet, Instrument, Local, Offer, Replaced, Result, Source,
    Stood, Store, StoreError, Version, Written,
};

/// The deployment-only scheme a resolved `LCL-` ID is kept under on the
/// record it became (the spec's requirement 1; approved 2026-10-13). Never on
/// the licence list, so never sent.
pub const LOCAL_SCHEME: &str = "open_meridian_local";

/// The open schemes the contract names, sendable while no licence list has
/// been read (W3.25).
pub const OPEN_FAMILIES: [&str; 7] = [
    "figi",
    "lei",
    "ticker_and_venue",
    "caip19",
    "iso4217",
    "sec_series",
    "sec_class",
];

/// The family an administrator's own code is licensed by (ruling 545): wire
/// `symbol` with the administrator in its `source`, sent only for an
/// administrator the list names. Not among the open schemes, so a list
/// must be read before one is sent.
pub const ADMINISTRATOR_SYMBOL: &str = "administrator_symbol";

/// The catalogue's first versions, shipped with each release (W3.24; ruled
/// 2026-10-14).
const SHIPPED: &str = include_str!("../templates/first.json");

// ── The template catalogue (W3.24) ─────────────────────────────────────────

/// Templates as the platform's JSON spells them (W1's description: a class
/// in lower case without its prefix).
pub fn templates_from_json(
    value: &serde_json::Value,
) -> std::result::Result<Vec<SecurityTemplate>, String> {
    let mut out = Vec::new();
    for item in value
        .get("templates")
        .and_then(|templates| templates.as_array())
        .ok_or("no templates")?
    {
        let text = |key: &str| {
            item.get(key)
                .map(|value| match value {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Number(number) => number.to_string(),
                    _ => String::new(),
                })
                .unwrap_or_default()
        };
        let template_id = text("template_id");
        if !template_id.starts_with("TPL-") {
            return Err(format!(
                "a template arrived as {template_id:?}, not the platform's"
            ));
        }
        let asset_class = meridian_domain::asset_class::read(&text("asset_class"))
            .ok_or_else(|| format!("{template_id}'s asset class is not one the contract names"))?;
        let instrument_type = meridian_domain::instrument_type::read(&text("instrument_type"))
            .ok_or_else(|| format!("{template_id}'s type is not one the contract names"))?;
        let fields = item
            .get("fields")
            .and_then(|fields| fields.as_array())
            .map(|fields| {
                fields
                    .iter()
                    .map(|field| TemplateField {
                        path: field
                            .get("path")
                            .and_then(|path| path.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        r#type: field
                            .get("type")
                            .and_then(|kind| kind.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        required: field
                            .get("required")
                            .and_then(|required| required.as_bool())
                            .unwrap_or(false),
                        // Contract v28 (574): the level, read from required
                        // on a version before it.
                        requirement: meridian_domain::template::requirement_of(
                            field
                                .get("requirement")
                                .and_then(|level| level.as_str())
                                .unwrap_or_default(),
                            field
                                .get("required")
                                .and_then(|required| required.as_bool())
                                .unwrap_or(false),
                        ) as i32,
                        repeated: field
                            .get("repeated")
                            .and_then(|repeated| repeated.as_bool())
                            .unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let match_rule = item
            .get("match_rule")
            .and_then(|rule| rule.as_array())
            .map(|rule| {
                rule.iter()
                    .filter_map(|scheme| scheme.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(SecurityTemplate {
            template_id,
            template_version: text("template_version"),
            name: text("name"),
            asset_class: asset_class as i32,
            instrument_type: instrument_type as i32,
            base: item
                .get("base")
                .and_then(|base| base.as_bool())
                .unwrap_or(false),
            calculation: text("calculation"),
            fields,
            match_rule,
            // Its calculation package (contract v26, W1.24), refused whole
            // where it does not stand: data the engine must equal exactly.
            calculation_package: meridian_domain::template::package_from_json(
                item.get("calculation_package"),
            )?,
            // Contract v28 (579): withdrawn, and the version new mints use.
            withdrawn: item
                .get("withdrawn")
                .and_then(|withdrawn| withdrawn.as_bool())
                .unwrap_or(false),
            successor_version: text("successor_version"),
        });
    }
    Ok(out)
}

/// The first versions every release ships.
pub fn shipped() -> Vec<SecurityTemplate> {
    serde_json::from_str::<serde_json::Value>(SHIPPED)
        .ok()
        .and_then(|value| templates_from_json(&value).ok())
        .unwrap_or_default()
}

fn version_number(version: &str) -> i64 {
    version.trim().parse().unwrap_or(0)
}

/// Every version the store answers by: what the release shipped and what the
/// platform's pulls added, each once.
pub fn catalogue(store: &dyn Store) -> Vec<SecurityTemplate> {
    let mut all = shipped();
    for held in store.templates().unwrap_or_default() {
        if !all.iter().any(|template| {
            template.template_id == held.template_id
                && template.template_version == held.template_version
        }) {
            all.push(held);
        }
    }
    all.sort_by(|a, b| {
        (&a.template_id, version_number(&a.template_version))
            .cmp(&(&b.template_id, version_number(&b.template_version)))
    });
    all
}

/// Each template at its latest version.
pub fn latest(catalogue: &[SecurityTemplate]) -> Vec<SecurityTemplate> {
    let mut out: Vec<SecurityTemplate> = Vec::new();
    for template in catalogue {
        match out
            .iter_mut()
            .find(|held| held.template_id == template.template_id)
        {
            Some(held) => {
                if version_number(&template.template_version)
                    > version_number(&held.template_version)
                {
                    *held = template.clone();
                }
            }
            None => out.push(template.clone()),
        }
    }
    out
}

/// One template at a version, or its latest where the version is empty.
pub fn read_template(
    catalogue: &[SecurityTemplate],
    template_id: &str,
    version: &str,
) -> Option<SecurityTemplate> {
    if version.is_empty() {
        return latest(catalogue)
            .into_iter()
            .find(|template| template.template_id == template_id);
    }
    catalogue
        .iter()
        .find(|template| {
            template.template_id == template_id && template.template_version == version
        })
        .cloned()
}

/// The template a record is held to, from the class and type its source
/// stated (requirement 16): its type's where the catalogue holds one, else
/// its class's own (a class with one template), else the class's base, and
/// none where no class is known ("class needed"). With the type proposed
/// where no template meets it.
pub fn pick(
    catalogue: &[SecurityTemplate],
    class: AssetClass,
    kind: InstrumentType,
) -> Option<(SecurityTemplate, bool)> {
    if class == AssetClass::Unspecified {
        return None;
    }
    let latest = latest(catalogue);
    let of_class = |base: bool, typed: i32| {
        latest
            .iter()
            .find(|template| {
                template.asset_class == class as i32
                    && template.base == base
                    && template.instrument_type == typed
            })
            .cloned()
    };
    if kind != InstrumentType::Unspecified {
        if let Some(template) = of_class(false, kind as i32) {
            return Some((template, false));
        }
        return of_class(true, 0).map(|template| (template, true));
    }
    of_class(false, 0)
        .map(|template| (template, false))
        .or_else(|| of_class(true, 0).map(|template| (template, false)))
}

/// Each required field of its template the record does not hold, by its
/// path (W3.7; `InstrumentRecord.missing_fields`): a value in force or
/// offered counts as held, a person still to accept it.
pub fn missing_fields(record: &Instrument, template: &SecurityTemplate) -> Vec<String> {
    let held = |field: Field| {
        !record.value(field).trim().is_empty()
            || record.offers.iter().any(|offer| offer.field == field)
    };
    let kind = meridian_domain::instrument_type::read(&record.instrument_type)
        .unwrap_or(InstrumentType::Unspecified);
    let listed = matches!(
        kind,
        InstrumentType::ListedOption | InstrumentType::Future | InstrumentType::FutureOption
    );
    template
        .fields
        .iter()
        // Contract v28 (574): a mint or complete field can be missing, an
        // optional one never; before v28, required.
        .filter(|field| {
            match meridian_domain::v1::FieldRequirement::try_from(field.requirement)
                .unwrap_or(meridian_domain::v1::FieldRequirement::Unspecified)
            {
                meridian_domain::v1::FieldRequirement::Mint
                | meridian_domain::v1::FieldRequirement::Complete => true,
                meridian_domain::v1::FieldRequirement::Optional => false,
                meridian_domain::v1::FieldRequirement::Unspecified => field.required,
            }
        })
        .filter(|field| match field.path.as_str() {
            path if listed
                && ![
                    "identifiers",
                    "description",
                    "currency",
                    "instrument_type",
                    "listing_venue_id",
                ]
                .contains(&path) =>
            {
                meridian_domain::instrument_type::listed_path_missing(
                    kind,
                    &record.listed_terms,
                    path,
                )
            }
            "identifiers" => record.held_now().next().is_none(),
            "description" => !held(Field::Description),
            "currency" => !held(Field::Currency),
            "instrument_type" => !held(Field::InstrumentType),
            "listing_venue_id" => record.listing_venue_id.is_empty(),
            "money_market_fund" => record.money_market_fund.is_empty(),
            path if path.starts_with("binary_event_contract") => {
                record.binary_event_contract.is_empty()
            }
            _ => false,
        })
        .map(|field| field.path.clone())
        .collect()
}

// ── The licence list (W3.25) ───────────────────────────────────────────────

/// The list a deployment that has read none sends by: the open schemes the
/// contract names, at version 0.
pub fn default_licence() -> LicenceList {
    LicenceList {
        version: 0,
        schemes: OPEN_FAMILIES
            .iter()
            .map(|scheme| scheme.to_string())
            .collect(),
        pace: Some(LicencePace {
            batch_size: 100,
            batches_per_minute: 6,
        }),
        administrators: Vec::new(),
    }
}

/// The list read last, or the open schemes where none was read.
pub fn licence(store: &dyn Store) -> LicenceList {
    store
        .licence_list()
        .ok()
        .flatten()
        .unwrap_or_else(default_licence)
}

/// The family a wire scheme is licensed by (W3.25, ruled 2026-10-14): the
/// three FIGI levels one family, a ticker with its venue `ticker_and_venue`.
pub fn family(scheme: &str) -> &str {
    match scheme {
        "figi" | "share_class_figi" | "composite_figi" => "figi",
        "ticker" => "ticker_and_venue",
        other => other,
    }
}

/// The family an identifier held is licensed by under a list: a `symbol`
/// whose source is an administrator the list names is
/// `administrator_symbol` (ruling 545); any other, its scheme's family.
pub fn family_held<'a>(scheme: &'a str, source: &str, list: &LicenceList) -> &'a str {
    if scheme == "symbol"
        && !source.is_empty()
        && list
            .administrators
            .iter()
            .any(|administrator| administrator == source)
    {
        ADMINISTRATOR_SYMBOL
    } else {
        family(scheme)
    }
}

/// The identifiers a request to the platform may carry (W3's invariant;
/// W3.25): each in a scheme whose family the list names; never the `LCL-`
/// ID, never `open_meridian_local`, never the platform's own ID scheme, and
/// never a source-scoped code but a ticker carrying its venue and, where
/// the list names the `administrator_symbol` family, a symbol whose source
/// is an administrator it admits (ruling 545). A vendor's symbol never is.
pub fn sendable(identifiers: &[Asked], list: &LicenceList) -> Vec<PbIdentifier> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for asked in identifiers {
        if asked.scheme == LOCAL_SCHEME
            || asked.scheme == meridian_symbology::GLOBAL_ID
            || asked.value.starts_with(crate::ids::LOCAL_PREFIX)
            || asked.value.trim().is_empty()
        {
            continue;
        }
        let licensed = family_held(&asked.scheme, &asked.source, list);
        if !asked.source.is_empty() && asked.scheme != "ticker" && licensed != ADMINISTRATOR_SYMBOL
        {
            continue;
        }
        if !list.schemes.iter().any(|scheme| scheme == licensed) {
            continue;
        }
        if seen.insert((
            asked.scheme.clone(),
            asked.value.clone(),
            asked.source.clone(),
        )) {
            out.push(crate::record::asked_to_wire(asked));
        }
    }
    out
}

/// Whether a resolve's source states the minimum a record needs (W3.7; the
/// one-ID model's rule 3): an identifier, a description and a currency, each
/// as the source stated it -- a currency's own code states both, and a
/// crypto asset is its own unit, so it needs no currency.
pub fn meets_minimum(request: &ResolveIdentifierRequest) -> bool {
    let identifiers: Vec<&PbIdentifier> = request
        .identifiers
        .iter()
        .filter(|identifier| !identifier.value.trim().is_empty())
        .collect();
    if identifiers.is_empty() {
        return false;
    }
    if identifiers
        .iter()
        .any(|identifier| identifier.scheme == "iso4217")
    {
        return true;
    }
    // The currency the source states of the security, or the one it
    // reports the holding in beside the symbol (W2.3's `currency`).
    let crypto = request.stated_asset_class == AssetClass::CryptoAsset as i32;
    let currency =
        !request.stated_currency.trim().is_empty() || !request.currency.trim().is_empty();
    !request.stated_description.trim().is_empty() && (crypto || currency)
}

/// A request key: random, a new one for every new ask (W3.4; ruled
/// 2026-10-14), a retry of a lost answer the only reuse.
pub fn request_key() -> String {
    use rand::RngCore;
    let mut random = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut random);
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The matching and minting request for a set nothing matched (W3.4): the
/// sendable identifiers, the template and its version, and what the source
/// stated, offered; `None` where nothing is sendable.
pub fn escalation(
    asked: &[Asked],
    request: &ResolveIdentifierRequest,
    template: Option<&SecurityTemplate>,
    list: &LicenceList,
    instance_id: &str,
    now_ns: i64,
) -> Option<EscalateInstrumentRequest> {
    let identifiers = sendable(asked, list);
    if identifiers.is_empty() {
        return None;
    }
    let offered = |path: &str, value: &str| TemplateFieldValue {
        path: path.to_string(),
        value: value.to_string(),
        provenance: Some(meridian_pb::v1::Provenance {
            kind: meridian_pb::v1::ProvenanceKind::Reported as i32,
            source: instance_id.to_string(),
            ..Default::default()
        }),
    };
    let mut fields = Vec::new();
    if !request.stated_description.trim().is_empty() {
        fields.push(offered("description", request.stated_description.trim()));
    }
    if !request.stated_currency.trim().is_empty() {
        fields.push(offered("currency", request.stated_currency.trim()));
    }
    Some(EscalateInstrumentRequest {
        source: instance_id.to_string(),
        asset_class: request.stated_asset_class,
        identifiers,
        as_of_ns: if request.as_of_ns > 0 {
            request.as_of_ns
        } else {
            now_ns
        },
        template_id: template.map(|t| t.template_id.clone()).unwrap_or_default(),
        template_version: template
            .map(|t| t.template_version.clone())
            .unwrap_or_default(),
        fields,
        request_key: request_key(),
    })
}

/// The same for a record the store holds already: a provisional record asked
/// again, or a migration's line (W3.4, W3.26).
pub fn escalation_for(
    record: &Instrument,
    list: &LicenceList,
    now_ns: i64,
) -> Option<EscalateInstrumentRequest> {
    let asked: Vec<Asked> = record.held_now().map(Identifier::asked).collect();
    let offered = |field: Field| -> Option<String> {
        let value = record.value(field);
        if !value.is_empty() {
            return Some(value.to_string());
        }
        record
            .offers
            .iter()
            .find(|offer| offer.field == field)
            .map(|offer| offer.value.clone())
    };
    let class = offered(Field::AssetClass)
        .map(|name| crate::record::asset_class_value(&name))
        .unwrap_or(0);
    let request = ResolveIdentifierRequest {
        stated_asset_class: class,
        stated_description: offered(Field::Description).unwrap_or_default(),
        stated_currency: offered(Field::Currency).unwrap_or_default(),
        as_of_ns: now_ns,
        ..Default::default()
    };
    let mut asking = escalation(&asked, &request, None, list, "", now_ns)?;
    asking.template_id = record.local.template_id.clone();
    asking.template_version = record.local.template_version.clone();
    Some(asking)
}

// ── A record of the platform's, kept from the start (W3.1, W3.5) ──────────

fn platform_words(master: &InstrumentRecord) -> String {
    format!(
        "the platform, record {} version {}",
        master.instrument_id, master.version
    )
}

fn sourced(field: Field, identifier: Option<Asked>, words: &str, now_ns: i64) -> Source {
    Source {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        prepared_call_id: String::new(),
        field,
        identifier,
        source: words.to_string(),
        person: String::new(),
        instance_id: String::new(),
        recorded_at_ns: now_ns,
        note: String::new(),
    }
}

/// The values a platform record states, by field.
/// The fields a person set or accepted on `record` that the platform's
/// answer `master` contradicts (contract v25, W3.26): its value in force
/// sourced to a person, and the platform's non-empty and different. In the
/// order the master's values are read.
pub fn contradicted_fields(record: &Instrument, master: &InstrumentRecord) -> Vec<Field> {
    master_values(master)
        .into_iter()
        .filter(|(field, said)| {
            let held = record.value(*field).trim();
            let by_a_person = record
                .source_of(*field, None)
                .is_some_and(|source| !source.person.is_empty());
            by_a_person && !said.is_empty() && !held.is_empty() && held != said
        })
        .map(|(field, _)| field)
        .collect()
}

fn master_values(master: &InstrumentRecord) -> Vec<(Field, String)> {
    vec![
        (Field::AssetClass, asset_class_name(master.asset_class)),
        (Field::Currency, master.currency.trim().to_string()),
        (Field::Description, master.description.trim().to_string()),
        (
            Field::InstrumentType,
            meridian_domain::instrument_type::name(
                InstrumentType::try_from(master.instrument_type)
                    .unwrap_or(InstrumentType::Unspecified),
            )
            .to_string(),
        ),
    ]
}

/// The platform's open identifiers on its record, each sourced to it.
fn master_identifiers(master: &InstrumentRecord) -> Vec<Asked> {
    master
        .identifiers
        .iter()
        .filter(|identifier| identifier.valid_until_ns == 0)
        .map(asked_from_wire)
        .filter(|asked| {
            asked.scheme != LOCAL_SCHEME && asked.scheme != meridian_symbology::GLOBAL_ID
        })
        .collect()
}

/// Each listing identifier's trading currency, as the platform's master holds
/// it per listing (contract v25, W3.1), set on the record's open identifier
/// it names; an identifier the master gives none keeps none. True when one
/// changed.
/// A listed series' terms as the platform's record states them (contract
/// v28; rulings 559, 575): taken whole where they differ, an adjustment being
/// the record's next version with its ID kept. True when they changed.
pub fn take_listed_terms(record: &mut Instrument, master: &InstrumentRecord) -> bool {
    let terms = meridian_domain::instrument_type::listed_terms_to_text(master);
    if terms.is_empty() || terms == record.listed_terms {
        return false;
    }
    record.listed_terms = terms;
    true
}

pub fn take_trading_currencies(record: &mut Instrument, master: &InstrumentRecord) -> bool {
    let mut changed = false;
    for held in record.identifiers.iter_mut().filter(|held| held.open()) {
        let said = master
            .identifiers
            .iter()
            .filter(|identifier| identifier.valid_until_ns == 0)
            .find(|identifier| {
                identifier.scheme == held.scheme
                    && identifier.value == held.value
                    && identifier.source == held.source
            })
            .map(|identifier| identifier.trading_currency.trim().to_ascii_uppercase())
            .unwrap_or_default();
        if !said.is_empty() && held.trading_currency != said {
            held.trading_currency = said;
            changed = true;
        }
    }
    changed
}

/// The platform's record, kept as the deployment's under its `INS-` ID (W3.1,
/// W3.5; requirement 19's v20 pull too): the asked identifiers and the
/// platform's open ones, its values in force sourced to it, what the
/// plugin's source stated kept as offers where it differs. Keyed by the
/// asked set, so two racing resolves keep one record; a record held already
/// under the ID is joined, not minted again.
pub fn keep_platform_record(
    store: &dyn Store,
    asked: &[Asked],
    master: &InstrumentRecord,
    template: Option<&SecurityTemplate>,
    offers: Vec<Offer>,
    now_ns: i64,
) -> Result<(Instrument, bool)> {
    let words = platform_words(master);
    if let Some(mut held) = store.by_id(&master.instrument_id)? {
        let expected = held.version;
        let mut changes = Vec::new();
        for identifier in asked {
            if held.holds(identifier)
                || !holders_elsewhere(store, identifier, &held.instrument_id)?.is_empty()
            {
                continue;
            }
            held.identifiers.push(window_for(store, identifier)?);
            held.set_source(sourced(
                Field::Identifier,
                Some(identifier.clone()),
                &words,
                now_ns,
            ));
            changes.push(identifier_change(identifier, &words));
        }
        let offered = crate::resolve::keep_offers(&mut held, offers);
        let listed = take_trading_currencies(&mut held, master);
        let terms = take_listed_terms(&mut held, master);
        if changes.is_empty() && !offered && !listed && !terms {
            return Ok((held, false));
        }
        held.version = expected + 1;
        held.record_time_ns = now_ns;
        let entry = version_of(
            &held,
            if changes.is_empty() { "offer" } else { "join" },
            changes,
            "",
            now_ns,
        );
        return match store.write(held.clone(), expected, entry)? {
            Written::Stored => Ok((held, false)),
            _ => Ok((store.by_id(&master.instrument_id)?.unwrap_or(held), false)),
        };
    }
    let mut identifiers: Vec<Asked> = asked.to_vec();
    for identifier in master_identifiers(master) {
        if !identifiers.contains(&identifier) {
            identifiers.push(identifier);
        }
    }
    let mut sources: Vec<Source> = identifiers
        .iter()
        .map(|identifier| sourced(Field::Identifier, Some(identifier.clone()), &words, now_ns))
        .collect();
    let mut candidate = Instrument {
        instrument_id: master.instrument_id.clone(),
        identifiers: identifiers
            .iter()
            .map(|identifier| window_for(store, identifier))
            .collect::<Result<Vec<_>>>()?,
        lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
        listing_venue_id: master.listing_venue_id.clone(),
        version: 1,
        record_time_ns: now_ns,
        local: Local {
            template_id: template
                .map(|t| t.template_id.clone())
                .unwrap_or_else(|| master.template_id.clone()),
            template_version: template
                .map(|t| t.template_version.clone())
                .unwrap_or_else(|| master.template_version.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    for (field, value) in master_values(master) {
        if !value.is_empty() {
            candidate.set_value(field, value);
            sources.push(sourced(field, None, &words, now_ns));
        }
    }
    take_trading_currencies(&mut candidate, master);
    take_listed_terms(&mut candidate, master);
    if let Some(contract) = &master.binary_event_contract {
        candidate.binary_event_contract =
            meridian_domain::instrument_type::contract_to_text(contract);
    }
    candidate.sources = sources;
    crate::resolve::keep_offers(&mut candidate, offers);
    let first = version_of(
        &candidate,
        "platform",
        identifiers
            .iter()
            .map(|identifier| identifier_change(identifier, &words))
            .collect(),
        &format!(
            "the platform's record, matched or minted at this deployment's request with no \
             person (contract v24, W3.4): {words}"
        ),
        now_ns,
    );
    let key = format!("ins:{}", master.instrument_id);
    let (record, stood) = store.mint(candidate, &key, first)?;
    Ok((record, stood == Stood::Minted))
}

fn holders_elsewhere(store: &dyn Store, asked: &Asked, here: &str) -> Result<Vec<String>> {
    let mut others = Vec::new();
    for record in store.matching(&asked.scheme, &asked.value, &asked.source, i64::MAX / 2)? {
        let became = crate::replace::current(store, &record.instrument_id)?;
        if became != here && !others.contains(&became) {
            others.push(became);
        }
    }
    Ok(others)
}

pub(crate) fn version_of(
    record: &Instrument,
    operation: &str,
    changes: Vec<crate::store::Change>,
    note: &str,
    now_ns: i64,
) -> Version {
    Version {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        prepared_call_id: String::new(),
        instrument_id: record.instrument_id.clone(),
        version: record.version,
        operation: operation.into(),
        changes,
        person: String::new(),
        instance_id: String::new(),
        note: note.into(),
        merged_instrument_id: String::new(),
        record_time_ns: now_ns,
    }
}

/// A provisional record's one-ID state (W3.7): its template and what of it
/// it lacks, and why it is provisional.
pub fn provisional_state(
    candidate: &Instrument,
    template: Option<&(SecurityTemplate, bool)>,
    reason: ProvisionalReason,
    proposed_type: &str,
    now_ns: i64,
) -> Local {
    let (template_id, template_version, missing) = match template {
        Some((template, _)) => (
            template.template_id.clone(),
            template.template_version.clone(),
            missing_fields(candidate, template),
        ),
        None => (String::new(), String::new(), Vec::new()),
    };
    // What the platform said comes first: a record sent and queued for its
    // staff, or not reached, says so whatever it lacks (W3.1: sent with no
    // template where no class is stated); otherwise "class needed" where no
    // class names a template, and "incomplete" where its template's fields
    // are missing.
    let platform_said = matches!(
        reason,
        ProvisionalReason::AwaitingStaff | ProvisionalReason::Unreachable
    );
    let reason = if platform_said {
        reason
    } else if template.is_none() {
        ProvisionalReason::ClassNeeded
    } else if reason == ProvisionalReason::HeldBack {
        reason
    } else {
        // The contract's five reasons and no sixth (W3.7, W3.11): a record
        // kept local -- nothing on the licence list to look for, the
        // current stage -- or one incomplete against its template is
        // INCOMPLETE, "the incomplete or offline state", its missing
        // fields listed where it lacks any.
        ProvisionalReason::Incomplete
    };
    Local {
        provisional: true,
        reason: reason as i32,
        template_id,
        template_version,
        missing_fields: missing,
        proposed_type: proposed_type.to_string(),
        asked_at_ns: now_ns,
        ..Default::default()
    }
}

// ── A provisional record resolved (W3.8; requirement 1) ───────────────────

/// What a resolution did: the record that stays at its new version, and the
/// replacement to announce.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub stays: Instrument,
    pub event: InstrumentReplacedEvent,
    pub minted: bool,
}

/// Replace `replaced_id` by the platform's record `master` (W3.8), its cause
/// `cause`: the `INS-` record starts from the replaced one -- every value in
/// force with its source, person and time, its offers, its identifiers with
/// their windows -- and carries the replaced ID under `open_meridian_local`
/// from its minting, never closed; the platform's differing values arrive as
/// offers; `open_meridian` windows close; the replaced record's identifiers
/// leave it. A record held under the `INS-` ID already is joined. `None`
/// where it was replaced already.
pub fn resolve_to(
    store: &dyn Store,
    replaced_id: &str,
    master: &InstrumentRecord,
    cause: ReplacementCause,
    now_ns: i64,
) -> Result<Option<Resolved>> {
    resolve_to_as(
        store,
        replaced_id,
        master,
        cause,
        &crate::complete::Actor::default(),
        "",
        now_ns,
    )
}

/// [`resolve_to`], each version it writes naming who made it: the person,
/// the delegation and client they acted through, and the prepared change
/// (W6.27), as a migration's line made by the deployment admin is (W3.26),
/// and why they said they made it, where they said.
pub fn resolve_to_as(
    store: &dyn Store,
    replaced_id: &str,
    master: &InstrumentRecord,
    cause: ReplacementCause,
    by: &crate::complete::Actor,
    why: &str,
    now_ns: i64,
) -> Result<Option<Resolved>> {
    let stamp = |mut version: Version| {
        if !why.is_empty() {
            version.note = format!("{}: {why}", version.note);
        }
        version.person = by.person.clone();
        version.acting_through_delegation = by.delegation.clone();
        version.client_name = by.client.clone();
        version.prepared_call_id = by.prepared_call_id.clone();
        version
    };
    if store.replacement_of(replaced_id)?.is_some() || replaced_id == master.instrument_id {
        return Ok(None);
    }
    let Some(replaced) = store.by_id(replaced_id)? else {
        return Ok(None);
    };
    let words = platform_words(master);
    let minted_at = store
        .history(replaced_id)?
        .last()
        .map(|first| first.record_time_ns)
        .unwrap_or(replaced.record_time_ns);
    // An `LCL-` record is linked under `open_meridian_local`; an `INS-`
    // record the platform merged is followed by its replacement alone.
    let local_link = replaced_id.starts_with(crate::ids::LOCAL_PREFIX);
    let local_id = Asked {
        scheme: LOCAL_SCHEME.into(),
        value: replaced_id.to_string(),
        source: String::new(),
    };
    let reason_words = match cause {
        ReplacementCause::PlatformMerged => "the platform merged it, its staff deciding it",
        _ => "a provisional record resolved to the platform's",
    };
    let note = format!(
        "{reason_words}: {replaced_id} became {} ({words}), contract v24 (W3.8)",
        master.instrument_id
    );
    let (stays, minted) = match store.by_id(&master.instrument_id)? {
        None => {
            let mut stays = replaced.clone();
            stays.instrument_id = master.instrument_id.clone();
            stays.version = 1;
            stays.record_time_ns = now_ns;
            stays.local = Local {
                template_id: if master.template_id.is_empty() {
                    replaced.local.template_id.clone()
                } else {
                    master.template_id.clone()
                },
                template_version: if master.template_id.is_empty() {
                    replaced.local.template_version.clone()
                } else {
                    master.template_version.clone()
                },
                ..Default::default()
            };
            // Only what is on the record now travels; windows kept.
            stays.identifiers.retain(|identifier| identifier.open());
            take_trading_currencies(&mut stays, master);
            take_listed_terms(&mut stays, master);
            // The open_meridian identifier stops being written (requirement 2).
            for identifier in stays.identifiers.iter_mut() {
                if identifier.scheme == meridian_symbology::GLOBAL_ID {
                    identifier.valid_to_ns = Some(now_ns.max(identifier.valid_from_ns));
                }
            }
            if local_link {
                stays.identifiers.push(Identifier {
                    scheme: LOCAL_SCHEME.into(),
                    value: replaced_id.to_string(),
                    source: String::new(),
                    valid_from_ns: minted_at,
                    valid_to_ns: None,
                    trading_currency: String::new(),
                });
                stays.set_source(sourced(
                    Field::Identifier,
                    Some(local_id.clone()),
                    &note,
                    now_ns,
                ));
            }
            for identifier in master_identifiers(master) {
                if !stays.carries(&identifier) {
                    stays.identifiers.push(window_for(store, &identifier)?);
                    stays.set_source(sourced(Field::Identifier, Some(identifier), &words, now_ns));
                }
            }
            let mut offers = Vec::new();
            for (field, value) in master_values(master) {
                if value.is_empty() {
                    continue;
                }
                if stays.value(field).is_empty() {
                    stays.set_value(field, value);
                    stays.set_source(sourced(field, None, &words, now_ns));
                } else if stays.value(field) != value {
                    offers.push(Offer {
                        field,
                        value,
                        identifier: None,
                        source: words.clone(),
                        instance_id: String::new(),
                        offered_at_ns: now_ns,
                        valid_until_ns: 0,
                        provenance: Vec::new(),
                    });
                }
            }
            if stays.listing_venue_id.is_empty() {
                stays.listing_venue_id = master.listing_venue_id.clone();
            }
            crate::resolve::keep_offers(&mut stays, offers);
            let mut first = stamp(version_of(&stays, "resolve", Vec::new(), &note, now_ns));
            first.merged_instrument_id = replaced_id.to_string();
            if local_link {
                first.changes = vec![identifier_change(&local_id, &note)];
            }
            let (record, stood) =
                store.mint(stays, &format!("ins:{}", master.instrument_id), first)?;
            (record, stood == Stood::Minted)
        }
        Some(mut held) => {
            let expected = held.version;
            let mut changes = Vec::new();
            if local_link {
                changes.push(identifier_change(&local_id, &note));
                held.identifiers.push(Identifier {
                    scheme: LOCAL_SCHEME.into(),
                    value: replaced_id.to_string(),
                    source: String::new(),
                    valid_from_ns: minted_at,
                    valid_to_ns: None,
                    trading_currency: String::new(),
                });
                held.set_source(sourced(
                    Field::Identifier,
                    Some(local_id.clone()),
                    &note,
                    now_ns,
                ));
            }
            for identifier in replaced.held_now() {
                let asked = identifier.asked();
                if asked.scheme == meridian_symbology::GLOBAL_ID || held.holds(&asked) {
                    continue;
                }
                held.identifiers.push(identifier.clone());
                if let Some(source) = replaced.source_of(Field::Identifier, Some(&asked)) {
                    held.set_source(source.clone());
                }
                changes.push(identifier_change(&asked, &note));
            }
            // A value a person set on the replaced record stays in force
            // where the record that stays has none; otherwise offered.
            let mut offers = replaced.offers.clone();
            for field in [
                Field::AssetClass,
                Field::Currency,
                Field::Description,
                Field::InstrumentType,
            ] {
                let theirs = replaced.value(field).to_string();
                if theirs.is_empty() {
                    continue;
                }
                if held.value(field).is_empty() {
                    held.set_value(field, theirs);
                    if let Some(source) = replaced.source_of(field, None) {
                        held.set_source(source.clone());
                    }
                } else if held.value(field) != theirs {
                    offers.push(Offer {
                        field,
                        value: theirs,
                        identifier: None,
                        source: format!("{replaced_id}, resolved into this record"),
                        instance_id: String::new(),
                        offered_at_ns: now_ns,
                        valid_until_ns: 0,
                        provenance: Vec::new(),
                    });
                }
            }
            crate::resolve::keep_offers(&mut held, offers);
            held.version = expected + 1;
            held.record_time_ns = now_ns;
            let mut entry = stamp(version_of(&held, "resolve", changes, &note, now_ns));
            entry.merged_instrument_id = replaced_id.to_string();
            match store.write(held.clone(), expected, entry)? {
                Written::Stored => (held, false),
                _ => {
                    return Err(StoreError::Unavailable(format!(
                        "{} changed while {replaced_id} was resolved into it; asked again later",
                        master.instrument_id
                    )))
                }
            }
        }
    };
    // The replaced record's identifiers leave it, kept closed.
    let mut gone = replaced.clone();
    let mut left = Vec::new();
    for identifier in replaced.held_now() {
        let asked = identifier.asked();
        gone.close(&asked, now_ns);
        left.push(crate::store::Change {
            field: Field::Identifier.name().into(),
            scheme: asked.scheme.clone(),
            namespace: asked.source.clone(),
            before: asked.value.clone(),
            after: String::new(),
            source: format!("resolved into {}", master.instrument_id),
        });
    }
    gone.local.provisional = false;
    gone.version = replaced.version + 1;
    gone.record_time_ns = now_ns;
    let mut entry = stamp(version_of(&gone, "resolved-into", left, &note, now_ns));
    entry.merged_instrument_id = master.instrument_id.clone();
    let _ = store.write(gone, replaced.version, entry)?;
    let replaced_at =
        match store.replace_because(replaced_id, &master.instrument_id, cause as i32, now_ns)? {
            Replaced::Recorded => now_ns,
            Replaced::AlreadyRecorded { .. } => return Ok(None),
        };
    let sequence = store
        .replacement_record(replaced_id)?
        .map(|kept| kept.sequence)
        .unwrap_or_default();
    Ok(Some(Resolved {
        event: InstrumentReplacedEvent {
            replaced_instrument_id: replaced_id.to_string(),
            instrument: Some(to_wire(&stays)),
            replaced_at_ns: replaced_at,
            cause: cause as i32,
            sequence,
        },
        stays,
        minted,
    }))
}

/// What keeping the platform's answer for a record did: the replacement to
/// announce, and each record at its new version.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub replaced: Option<InstrumentReplacedEvent>,
    pub changed: Vec<Instrument>,
}

/// The platform's answer for a record the store holds (W3.4, W3.8): a
/// provisional record resolved to the `INS-` record it names; the request
/// queued for staff said on it; nothing where it was not reached or the
/// record is not provisional.
pub fn apply_answer(
    store: &dyn Store,
    instrument_id: &str,
    said: &crate::resolve::PlatformSaid,
    now_ns: i64,
) -> Result<Applied> {
    use crate::resolve::PlatformSaid;
    let became = crate::replace::current(store, instrument_id)?;
    let Some(record) = store.by_id(&became)? else {
        return Ok(Applied::default());
    };
    if !record.local.provisional {
        return Ok(Applied::default());
    }
    match said {
        PlatformSaid::Record(master) => {
            let Some(resolved) =
                resolve_to(store, &became, master, ReplacementCause::Resolved, now_ns)?
            else {
                return Ok(Applied::default());
            };
            let mut changed = vec![resolved.stays.clone()];
            if let Some(gone) = store.by_id(&became)? {
                changed.push(gone);
            }
            Ok(Applied {
                replaced: Some(resolved.event),
                changed,
            })
        }
        PlatformSaid::Pending => {
            let awaiting = ProvisionalReason::AwaitingStaff as i32;
            if record.local.reason == awaiting {
                return Ok(Applied::default());
            }
            let expected = record.version;
            let mut marked = record.clone();
            marked.local.reason = awaiting;
            marked.local.asked_at_ns = now_ns;
            marked.version = expected + 1;
            marked.record_time_ns = now_ns;
            let entry = version_of(
                &marked,
                "provisional",
                Vec::new(),
                "the platform queued the request for its staff's review; asked again daily \
                 (W3.4)",
                now_ns,
            );
            match store.write(marked.clone(), expected, entry)? {
                Written::Stored => Ok(Applied {
                    replaced: None,
                    changed: vec![marked],
                }),
                _ => Ok(Applied::default()),
            }
        }
        PlatformSaid::Unreachable | PlatformSaid::NotAsked => Ok(Applied::default()),
    }
}

/// A replacement as every role hears and reads it (W3.8, W3.22): the record
/// it became as it stands now.
pub fn replacement_event(
    store: &dyn Store,
    kept: &crate::store::ReplacementKept,
) -> Result<InstrumentReplacedEvent> {
    let became = crate::replace::current(store, &kept.replaced_by)?;
    let instrument = store.by_id(&became)?.map(|record| to_wire(&record));
    Ok(InstrumentReplacedEvent {
        replaced_instrument_id: kept.replaced_id.clone(),
        instrument: instrument.or_else(|| {
            Some(InstrumentRecord {
                instrument_id: became,
                ..Default::default()
            })
        }),
        replaced_at_ns: kept.replaced_at_ns,
        cause: kept.cause,
        sequence: kept.sequence,
    })
}

/// W3.22: the replacements after a sequence, at most a thousand, and the
/// sequence read to.
pub fn list_replacements(
    store: &dyn Store,
    after_sequence: i64,
) -> Result<meridian_domain::v1::ListReplacementsReply> {
    let kept = store.replacements_after(after_sequence.max(0), 1000)?;
    let sequence = kept
        .last()
        .map(|last| last.sequence)
        .unwrap_or(after_sequence.max(0));
    Ok(meridian_domain::v1::ListReplacementsReply {
        replacements: kept
            .iter()
            .map(|kept| replacement_event(store, kept))
            .collect::<Result<Vec<_>>>()?,
        sequence,
    })
}

/// When the licence list read last was kept: a provisional record asked
/// before it is asked again under it (W3.25's backfill).
pub const LICENCE_KEPT_AT: &str = "licence_list_kept_at_ns";

/// The feed's cursor, kept by the store (W3.27).
pub const FEED_CURSOR: &str = "replacement_feed_sequence";

/// W3.27: the platform's replacements and decommissions, followed for each
/// record the store holds -- a merge replaces it (`_PLATFORM_MERGED`), a
/// decommission is shown on it -- and the sequence read to kept. An entry at
/// or before the cursor was followed already.
pub fn apply_feed(
    store: &dyn Store,
    entries: &[meridian_domain::v1::ReplacementFeedEntry],
    now_ns: i64,
) -> Result<Vec<Applied>> {
    let cursor: i64 = store
        .state(FEED_CURSOR)?
        .and_then(|text| text.parse().ok())
        .unwrap_or(0);
    let mut entries: Vec<&meridian_domain::v1::ReplacementFeedEntry> = entries
        .iter()
        .filter(|entry| entry.sequence > cursor)
        .collect();
    entries.sort_by_key(|entry| entry.sequence);
    let mut applied = Vec::new();
    let mut read_to = cursor;
    for entry in entries {
        if store.by_id(&entry.instrument_id)?.is_some() {
            // Contract v28 (579): a series past its last lifecycle date,
            // marked expired, its ID kept and never reused; nothing moves.
            if entry.expired {
                if let Some(shown) = expire(store, &entry.instrument_id, now_ns)? {
                    applied.push(Applied {
                        replaced: None,
                        changed: vec![shown],
                    });
                }
            } else if entry.decommissioned {
                if let Some(shown) = decommission(store, &entry.instrument_id, now_ns)? {
                    applied.push(Applied {
                        replaced: None,
                        changed: vec![shown],
                    });
                }
            } else if entry.replaced_by.starts_with("INS-") {
                let successor = InstrumentRecord {
                    instrument_id: entry.replaced_by.clone(),
                    ..Default::default()
                };
                if let Some(resolved) = resolve_to(
                    store,
                    &entry.instrument_id,
                    &successor,
                    ReplacementCause::PlatformMerged,
                    now_ns,
                )? {
                    let mut changed = vec![resolved.stays.clone()];
                    if let Some(gone) = store.by_id(&entry.instrument_id)? {
                        changed.push(gone);
                    }
                    applied.push(Applied {
                        replaced: Some(resolved.event),
                        changed,
                    });
                }
            }
        }
        read_to = entry.sequence;
        store.set_state(FEED_CURSOR, &read_to.to_string())?;
    }
    let _ = read_to;
    Ok(applied)
}

/// The record's lifecycle state for an expired series (contract v28).
pub const EXPIRED: &str = "INSTRUMENT_LIFECYCLE_STATE_EXPIRED";

/// An expiry the platform's feed said (contract v28, ruling 579): the held
/// series marked expired as its next version, its ID kept, never merged nor
/// decommissioned and moving no position; a position still held after it
/// is a break the book shows. `None` where it was shown already.
pub fn expire(store: &dyn Store, instrument_id: &str, now_ns: i64) -> Result<Option<Instrument>> {
    let Some(mut record) = store.by_id(instrument_id)? else {
        return Ok(None);
    };
    if record.lifecycle_state == EXPIRED {
        return Ok(None);
    }
    let expected = record.version;
    record.lifecycle_state = EXPIRED.to_string();
    record.version = expected + 1;
    record.record_time_ns = now_ns;
    let entry = version_of(
        &record,
        "expired",
        Vec::new(),
        "the platform marked the series expired past its last lifecycle date (its replacement \
         feed, W3.27; ruling 579); its ID is kept and no position moves",
        now_ns,
    );
    match store.write(record.clone(), expected, entry)? {
        Written::Stored => Ok(Some(record)),
        _ => Ok(None),
    }
}

/// A decommission the platform's feed said, shown on the held record and
/// moving nothing (W3.27): its new version, where it was not shown already.
pub fn decommission(
    store: &dyn Store,
    instrument_id: &str,
    now_ns: i64,
) -> Result<Option<Instrument>> {
    let Some(mut record) = store.by_id(instrument_id)? else {
        return Ok(None);
    };
    if record.local.decommissioned {
        return Ok(None);
    }
    let expected = record.version;
    record.local.decommissioned = true;
    record.version = expected + 1;
    record.record_time_ns = now_ns;
    let entry = version_of(
        &record,
        "decommissioned",
        Vec::new(),
        "the platform decommissioned it (its replacement feed, W3.27); no position moves",
        now_ns,
    );
    match store.write(record.clone(), expected, entry)? {
        Written::Stored => Ok(Some(record)),
        _ => Ok(None),
    }
}

/// W3.23: a `reference` plugin's offer of a record's template fields, kept as
/// offers with the plugin the source, never in force, never over a person's.
pub fn offer_template_fields(
    store: &dyn Store,
    request: &meridian_domain::v1::OfferTemplateFieldsRequest,
    instance_id: &str,
    now_ns: i64,
) -> Result<std::result::Result<Instrument, (String, String)>> {
    let catalogue = catalogue(store);
    let Some(template) = read_template(&catalogue, &request.template_id, &request.template_version)
        .filter(|_| !request.template_version.is_empty())
    else {
        return Ok(Err((
            "template_version".into(),
            format!(
                "the catalogue holds no {} version {:?}",
                request.template_id, request.template_version
            ),
        )));
    };
    let became = crate::replace::current(store, &request.instrument_id)?;
    let Some(mut record) = store.by_id(&became)? else {
        return Ok(Err((
            "instrument_id".into(),
            format!("the store holds no record {}", request.instrument_id),
        )));
    };
    let mut offers = Vec::new();
    for (index, field) in request.fields.iter().enumerate() {
        if !template.fields.iter().any(|held| held.path == field.path) {
            return Ok(Err((
                format!("fields[{index}].path"),
                format!("{} has no field {}", template.template_id, field.path),
            )));
        }
        let target = match field.path.as_str() {
            "description" => Field::Description,
            "currency" => Field::Currency,
            "instrument_type" => Field::InstrumentType,
            "asset_class" => Field::AssetClass,
            _ => continue,
        };
        if target == Field::Currency && !crate::record::is_currency(field.value.trim()) {
            return Ok(Err((
                format!("fields[{index}].value"),
                format!("{:?} is not an ISO 4217 code", field.value),
            )));
        }
        let source = field
            .provenance
            .as_ref()
            .map(|provenance| provenance.source.clone())
            .filter(|source| !source.is_empty())
            .unwrap_or_else(|| instance_id.to_string());
        offers.push(Offer {
            field: target,
            value: field.value.trim().to_string(),
            identifier: None,
            source: format!("offered by {instance_id} from {source}"),
            instance_id: instance_id.to_string(),
            offered_at_ns: now_ns,
            valid_until_ns: 0,
            provenance: field
                .provenance
                .as_ref()
                .map(prost::Message::encode_to_vec)
                .unwrap_or_default(),
        });
    }
    let expected = record.version;
    if !crate::resolve::keep_offers(&mut record, offers) {
        return Ok(Ok(record));
    }
    record.version = expected + 1;
    record.record_time_ns = now_ns;
    let mut entry = version_of(&record, "offer", Vec::new(), "", now_ns);
    entry.instance_id = instance_id.to_string();
    match store.write(record.clone(), expected, entry)? {
        Written::Stored => Ok(Ok(record)),
        _ => Ok(Err((
            "instrument_id".into(),
            format!(
                "{} changed while its fields were offered",
                record.instrument_id
            ),
        ))),
    }
}

/// The identifier set a record of the store was minted for, as its key
/// reads (for a re-ask's dedup).
pub fn set_key(record: &Instrument) -> String {
    IdentifierSet::new(record.held_now().map(Identifier::asked)).key()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(scheme: &str, value: &str, source: &str) -> Asked {
        Asked {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
        }
    }

    #[test]
    fn the_first_templates_ship_with_the_release_six_and_a_base_per_class() {
        let shipped = shipped();
        for id in [
            "TPL-cash",
            "TPL-crypto-asset",
            "TPL-equity",
            "TPL-fund",
            "TPL-money-market-fund",
            "TPL-binary-event-contract",
        ] {
            assert!(
                shipped.iter().any(|t| t.template_id == id && !t.base),
                "{id}"
            );
        }
        // From contract v27, the referential class's base too.
        assert_eq!(shipped.iter().filter(|t| t.base).count(), 8);
        let equity = shipped
            .iter()
            .find(|t| t.template_id == "TPL-equity")
            .unwrap();
        assert_eq!(equity.match_rule[0], "share_class_figi");
    }

    #[test]
    fn a_template_is_picked_by_class_and_type_and_a_type_without_one_names_the_base() {
        let all = shipped();
        let (equity, _) = pick(&all, AssetClass::Equity, InstrumentType::Unspecified).unwrap();
        assert_eq!(equity.template_id, "TPL-equity");
        let (fund, _) = pick(&all, AssetClass::Fund, InstrumentType::MoneyMarketFund).unwrap();
        assert_eq!(fund.template_id, "TPL-money-market-fund");
        let (debt, _) = pick(&all, AssetClass::Debt, InstrumentType::Unspecified).unwrap();
        assert_eq!(debt.template_id, "TPL-debt-base");
        let (proposed, base) = pick(
            &all,
            AssetClass::Equity,
            InstrumentType::BinaryEventContract,
        )
        .unwrap();
        assert!(base && proposed.template_id == "TPL-equity-base");
        assert!(pick(&all, AssetClass::Unspecified, InstrumentType::Unspecified).is_none());
        // Contract v27: each referential type its own template, matched by
        // FIGI, ISIN or the administrator's source-scoped symbol.
        for (kind, id) in [
            (InstrumentType::InterestRate, "TPL-interest-rate"),
            (InstrumentType::RateCurve, "TPL-rate-curve"),
            (InstrumentType::Index, "TPL-index"),
        ] {
            let (template, base) = pick(&all, AssetClass::Referential, kind).unwrap();
            assert!(!base && template.template_id == id, "{id}");
            assert_eq!(template.match_rule, ["figi", "isin", "symbol"]);
        }
        let (base, _) = pick(&all, AssetClass::Referential, InstrumentType::Unspecified).unwrap();
        assert_eq!(base.template_id, "TPL-referential-base");
    }

    #[test]
    fn only_schemes_on_the_licence_list_are_sendable_and_a_local_id_never_is() {
        let list = default_licence();
        let held = vec![
            asked("cusip", "037833100", ""),
            asked("isin", "US0378331005", ""),
            asked("share_class_figi", "BBG001S5N8V8", ""),
            asked("symbol", "AAPL", "snaptrade"),
            asked("ticker", "AAPL", "XNAS"),
            asked(LOCAL_SCHEME, "LCL-01J8", ""),
            asked(meridian_symbology::GLOBAL_ID, "INS-01J8", ""),
            asked("figi", "LCL-SNEAKY", ""),
            asked("iso4217", "USD", ""),
        ];
        let sent: Vec<String> = sendable(&held, &list)
            .into_iter()
            .map(|identifier| identifier.scheme)
            .collect();
        assert_eq!(sent, vec!["share_class_figi", "ticker", "iso4217"]);
        // A scheme the platform licenses later is sent from the next read.
        let mut later = list.clone();
        later.schemes.push("cusip".into());
        assert!(sendable(&held, &later)
            .iter()
            .any(|identifier| identifier.scheme == "cusip"));
        // A list without FIGI sends none of its three levels.
        let mut narrow = list;
        narrow.schemes.retain(|scheme| scheme != "figi");
        assert!(!sendable(&held, &narrow)
            .iter()
            .any(|identifier| identifier.scheme.contains("figi")));
    }

    #[test]
    fn an_administrators_symbol_is_sendable_only_where_the_list_admits_it() {
        let held = vec![
            asked("symbol", "SOFR", "nyfed"),
            asked("symbol", "ESTR", "ecb"),
            asked("symbol", "AAPL", "snaptrade"),
        ];
        let symbols = |list: &LicenceList| -> Vec<(String, String)> {
            sendable(&held, list)
                .into_iter()
                .filter(|identifier| identifier.scheme == "symbol")
                .map(|identifier| (identifier.value, identifier.source))
                .collect()
        };
        // Ruling 545: the family listed and the source admitted.
        let mut admitting = default_licence();
        admitting.schemes.push(ADMINISTRATOR_SYMBOL.into());
        admitting.administrators = vec!["nyfed".into(), "frb".into()];
        assert_eq!(symbols(&admitting), vec![("SOFR".into(), "nyfed".into())]);
        // A source the list does not admit, an administrator or a vendor,
        // is not sent.
        assert!(!symbols(&admitting)
            .iter()
            .any(|(_, source)| source == "ecb"));
        assert!(!symbols(&admitting)
            .iter()
            .any(|(_, source)| source == "snaptrade"));
        // The administrators named without the family: none is sent.
        let mut unlisted = admitting.clone();
        unlisted
            .schemes
            .retain(|scheme| scheme != ADMINISTRATOR_SYMBOL);
        assert!(symbols(&unlisted).is_empty());
        // The list held before any pull admits none (W3.25's open schemes).
        assert!(symbols(&default_licence()).is_empty());
        assert!(!OPEN_FAMILIES.contains(&ADMINISTRATOR_SYMBOL));
        // A symbol is still never sent under a list naming a bare `symbol`
        // family, whatever its source.
        let mut bare = default_licence();
        bare.schemes.push("symbol".into());
        assert!(symbols(&bare).is_empty());
    }

    #[test]
    fn a_source_thinner_than_the_minimum_is_no_record() {
        let thin = ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "symbol".into(),
                value: "ZZ1".into(),
                source: "snaptrade".into(),
                ..Default::default()
            }],
            stated_description: "ZZ One".into(),
            ..Default::default()
        };
        assert!(!meets_minimum(&thin), "no currency");
        let mut enough = thin.clone();
        enough.stated_currency = "USD".into();
        assert!(meets_minimum(&enough));
        let code = ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "iso4217".into(),
                value: "CHF".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(meets_minimum(&code), "a currency's code states both");
    }

    // ── The resolve, the resolution, the feed and the migration ──────────

    use crate::resolve::{
        resolve_identifier, resolve_matched, resolve_unmatched, Matched, PlatformSaid,
    };
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn equity_figi(figi: &str) -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "share_class_figi".into(),
                value: figi.into(),
                ..Default::default()
            }],
            as_of_ns: NOW,
            stated_asset_class: AssetClass::Equity as i32,
            stated_description: "ZZ Top Holdings Inc. common stock".into(),
            stated_currency: "USD".into(),
            ..Default::default()
        }
    }

    fn master(id: &str, figi: &str) -> InstrumentRecord {
        InstrumentRecord {
            instrument_id: id.into(),
            identifiers: vec![PbIdentifier {
                scheme: "share_class_figi".into(),
                value: figi.into(),
                ..Default::default()
            }],
            asset_class: AssetClass::Equity as i32,
            currency: "USD".into(),
            description: "ZZ TOP HOLDINGS INC".into(),
            version: 1,
            template_id: "TPL-equity".into(),
            template_version: "1".into(),
            ..Default::default()
        }
    }

    fn unmatched(
        store: &MemoryStore,
        request: &ResolveIdentifierRequest,
        said: PlatformSaid,
    ) -> crate::resolve::Resolution {
        match resolve_matched(store, request, "custody-snaptrade-1", NOW).unwrap() {
            Matched::Unmatched(asked) => {
                resolve_unmatched(store, request, asked, "custody-snaptrade-1", NOW, said).unwrap()
            }
            Matched::Answered(answered) => *answered,
        }
    }

    #[test]
    fn a_platform_value_contradicting_a_persons_is_named() {
        // Contract v25 (W3.26): a value in force a person set or accepted,
        // and the platform's non-empty and different.
        let platform = master("INS-01J8ZZTP", "BBG001S5N8V8");
        let mut record = Instrument {
            instrument_id: "LCL-1".into(),
            currency: "GBP".into(),
            description: "ZZ TOP HOLDINGS INC".into(),
            ..Default::default()
        };
        let mut by_person = sourced(Field::Currency, None, "set by a person", 1);
        by_person.person = "local|ada".into();
        record.set_source(by_person);
        record.set_source(sourced(
            Field::Description,
            None,
            "reported by custody-1",
            1,
        ));
        assert_eq!(
            contradicted_fields(&record, &platform),
            vec![Field::Currency]
        );
        // The same value is no contradiction; nor is a plugin's report.
        record.currency = "USD".into();
        assert!(contradicted_fields(&record, &platform).is_empty());
        record.description = "SOMETHING ELSE".into();
        assert!(contradicted_fields(&record, &platform).is_empty());
    }

    #[test]
    fn a_listing_identifier_keeps_the_currency_its_listing_trades_in() {
        // Contract v25 (W3.1): a Canadian cross-listing reads CAD beside the
        // security's USD; the record's own currency stays the security's.
        let store = MemoryStore::new();
        let mut platform = master("INS-01J8ZZTP", "BBG001S5N8V8");
        platform.identifiers.push(PbIdentifier {
            scheme: "figi".into(),
            value: "BBG001S5TSX1".into(),
            trading_currency: "cad".into(),
            ..Default::default()
        });
        unmatched(
            &store,
            &equity_figi("BBG001S5N8V8"),
            PlatformSaid::Record(Box::new(platform.clone())),
        );
        let held = store.by_id("INS-01J8ZZTP").unwrap().unwrap();
        assert_eq!(held.currency, "USD");
        let listing = held
            .identifiers
            .iter()
            .find(|identifier| identifier.value == "BBG001S5TSX1")
            .unwrap();
        assert_eq!(listing.trading_currency, "CAD");
        let share_class = held
            .identifiers
            .iter()
            .find(|identifier| identifier.value == "BBG001S5N8V8")
            .unwrap();
        assert!(share_class.trading_currency.is_empty());
        let wire = crate::record::to_wire(&held);
        assert!(wire
            .identifiers
            .iter()
            .any(|identifier| identifier.value == "BBG001S5TSX1"
                && identifier.trading_currency == "CAD"));
    }

    #[test]
    fn the_platforms_record_is_kept_under_its_own_id_from_the_start() {
        let store = MemoryStore::new();
        let request = equity_figi("BBG001S5N8V8");
        let first = unmatched(
            &store,
            &request,
            PlatformSaid::Record(Box::new(master("INS-01J8ZZTP", "BBG001S5N8V8"))),
        );
        assert!(first.reply.found && first.reply.minted && !first.reply.provisional);
        assert_eq!(first.reply.instrument_id, "INS-01J8ZZTP");
        let held = store.by_id("INS-01J8ZZTP").unwrap().unwrap();
        assert_eq!(
            held.description, "ZZ TOP HOLDINGS INC",
            "the platform's value in force"
        );
        assert!(
            held.offers
                .iter()
                .any(|offer| offer.value == "ZZ Top Holdings Inc. common stock"),
            "what the plugin's source stated, offered"
        );
        assert!(!held
            .identifiers
            .iter()
            .any(|i| i.scheme == meridian_symbology::GLOBAL_ID));
        // Asked again, matched: the same record, not minted.
        let again = resolve_identifier(&store, &request, "custody-snaptrade-1", NOW + 1).unwrap();
        assert_eq!(again.reply.instrument_id, "INS-01J8ZZTP");
        assert!(!again.reply.minted);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_record_the_platform_cannot_answer_is_provisional_with_its_reason_and_template() {
        let store = MemoryStore::new();
        let pending = unmatched(&store, &equity_figi("BBG00PENDING"), PlatformSaid::Pending);
        assert!(pending.reply.minted && pending.reply.provisional);
        assert!(pending
            .reply
            .instrument_id
            .starts_with(crate::ids::LOCAL_PREFIX));
        let held = store.by_id(&pending.reply.instrument_id).unwrap().unwrap();
        assert_eq!(held.local.reason, ProvisionalReason::AwaitingStaff as i32);
        assert_eq!(held.local.template_id, "TPL-equity");
        assert!(
            held.local.missing_fields.is_empty(),
            "offers count as held: {:?}",
            held.local.missing_fields
        );

        let unreached = unmatched(
            &store,
            &equity_figi("BBG00AWAY000"),
            PlatformSaid::Unreachable,
        );
        let held = store
            .by_id(&unreached.reply.instrument_id)
            .unwrap()
            .unwrap();
        assert_eq!(held.local.reason, ProvisionalReason::Unreachable as i32);

        // No class stated: "class needed", naming no template.
        let mut classless = equity_figi("BBG00NOCLASS");
        classless.stated_asset_class = 0;
        let none = unmatched(&store, &classless, PlatformSaid::NotAsked);
        let held = store.by_id(&none.reply.instrument_id).unwrap().unwrap();
        assert_eq!(held.local.reason, ProvisionalReason::ClassNeeded as i32);
        assert!(held.local.template_id.is_empty());
        // Sent all the same and queued: the platform's answer is its reason.
        classless.identifiers[0].value = "BBG00NOCLASS2".into();
        let queued = unmatched(&store, &classless, PlatformSaid::Pending);
        let held = store.by_id(&queued.reply.instrument_id).unwrap().unwrap();
        assert_eq!(held.local.reason, ProvisionalReason::AwaitingStaff as i32);
        assert!(held.local.template_id.is_empty());

        // Thinner than the minimum: not found, nothing minted.
        let count = store.count().unwrap();
        let mut thin = equity_figi("BBG00THIN000");
        thin.stated_description.clear();
        let missed = unmatched(&store, &thin, PlatformSaid::NotAsked);
        assert!(!missed.reply.found && missed.changed.is_none());
        assert_eq!(
            missed.reply.miss_reason,
            meridian_domain::v1::MissReason::NotFound as i32
        );
        assert_eq!(store.count().unwrap(), count);
    }

    #[test]
    fn a_provisional_record_resolves_to_the_platforms_and_a_persons_value_stays() {
        let store = MemoryStore::new();
        let minted = unmatched(
            &store,
            &equity_figi("BBG001S5N8V8"),
            PlatformSaid::Unreachable,
        );
        let lcl = minted.reply.instrument_id.clone();
        // A person sets the description.
        crate::complete::complete_as(
            &store,
            &meridian_domain::v1::CompleteInstrumentsRequest {
                completions: vec![meridian_domain::v1::InstrumentCompletion {
                    instrument_id: lcl.clone(),
                    against_version: 1,
                    values: vec![meridian_domain::v1::InstrumentValue {
                        value: Some(meridian_domain::v1::instrument_value::Value::Description(
                            "ZZ Top, our words".into(),
                        )),
                        source: "the statement".into(),
                    }],
                    note: String::new(),
                }],
            },
            &crate::complete::Actor::person("ada@example.com"),
            NOW + 1,
        )
        .unwrap()
        .unwrap();
        let applied = apply_answer(
            &store,
            &lcl,
            &PlatformSaid::Record(Box::new(master("INS-01J8ZZTP", "BBG001S5N8V8"))),
            NOW + 2,
        )
        .unwrap();
        let event = applied.replaced.expect("announced");
        assert_eq!(event.cause, ReplacementCause::Resolved as i32);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.replaced_instrument_id, lcl);
        let stays = store.by_id("INS-01J8ZZTP").unwrap().unwrap();
        assert_eq!(
            stays.description, "ZZ Top, our words",
            "a person's value stays in force"
        );
        assert_eq!(
            stays.source_of(Field::Description, None).unwrap().person,
            "ada@example.com"
        );
        assert!(stays
            .offers
            .iter()
            .any(|offer| offer.value == "ZZ TOP HOLDINGS INC"));
        let link = stays
            .identifiers
            .iter()
            .find(|i| i.scheme == LOCAL_SCHEME)
            .expect("the LCL- ID under open_meridian_local");
        assert_eq!(link.value, lcl);
        assert!(link.valid_to_ns.is_none() && link.valid_from_ns == NOW);
        assert_eq!(
            crate::replace::current(&store, &lcl).unwrap(),
            "INS-01J8ZZTP"
        );
        // A resolve by the identifier answers the INS- record.
        let found = resolve_identifier(
            &store,
            &equity_figi("BBG001S5N8V8"),
            "custody-snaptrade-1",
            NOW + 3,
        )
        .unwrap();
        assert_eq!(found.reply.instrument_id, "INS-01J8ZZTP");
        assert!(!found.reply.provisional);
        // The history reads across the link.
        let first = store.history("INS-01J8ZZTP").unwrap().pop().unwrap();
        assert_eq!(first.merged_instrument_id, lcl);
        let listed = list_replacements(&store, 0).unwrap();
        assert_eq!(listed.sequence, 1);
        assert_eq!(
            listed.replacements[0]
                .instrument
                .as_ref()
                .unwrap()
                .instrument_id,
            "INS-01J8ZZTP"
        );
        assert!(list_replacements(&store, 1)
            .unwrap()
            .replacements
            .is_empty());
    }

    #[test]
    fn the_feed_follows_a_platform_merge_and_shows_a_decommission() {
        let store = MemoryStore::new();
        unmatched(
            &store,
            &equity_figi("BBG000AAAA01"),
            PlatformSaid::Record(Box::new(master("INS-A", "BBG000AAAA01"))),
        );
        unmatched(
            &store,
            &equity_figi("BBG000CCCC01"),
            PlatformSaid::Record(Box::new(master("INS-C", "BBG000CCCC01"))),
        );
        let entries = vec![
            meridian_domain::v1::ReplacementFeedEntry {
                sequence: 1043,
                instrument_id: "INS-A".into(),
                replaced_by: "INS-B".into(),
                decommissioned: false,
                expired: false,
            },
            meridian_domain::v1::ReplacementFeedEntry {
                sequence: 1044,
                instrument_id: "INS-NOT-HELD".into(),
                replaced_by: "INS-D".into(),
                decommissioned: false,
                expired: false,
            },
            meridian_domain::v1::ReplacementFeedEntry {
                sequence: 1045,
                instrument_id: "INS-C".into(),
                replaced_by: String::new(),
                decommissioned: true,
                expired: false,
            },
        ];
        let applied = apply_feed(&store, &entries, NOW + 5).unwrap();
        assert_eq!(applied.len(), 2);
        let event = applied[0].replaced.as_ref().unwrap();
        assert_eq!(event.cause, ReplacementCause::PlatformMerged as i32);
        assert_eq!(crate::replace::current(&store, "INS-A").unwrap(), "INS-B");
        assert!(store.by_id("INS-B").unwrap().is_some());
        assert!(
            store.by_id("INS-D").unwrap().is_none(),
            "nothing held, nothing followed"
        );
        let c = store.by_id("INS-C").unwrap().unwrap();
        assert!(c.local.decommissioned);
        assert!(
            crate::replace::current(&store, "INS-C").unwrap() == "INS-C",
            "a decommission moves nothing"
        );
        assert_eq!(store.state(FEED_CURSOR).unwrap().as_deref(), Some("1045"));
        assert!(
            apply_feed(&store, &entries, NOW + 6).unwrap().is_empty(),
            "read once"
        );
    }

    #[test]
    fn a_listed_series_keeps_its_terms_lists_what_it_lacks_and_expires_by_the_feed() {
        // Contract v28 (rulings 559, 574, 575, 579): the platform's option
        // record with its terms; its last trade date and deliverable not yet
        // stated (complete), its settlement time optional on a physically
        // settled option; an adjustment the next version; expired by the
        // feed, its ID kept.
        use meridian_domain::v1::{
            ExerciseStyle, FieldRequirement, ListedOption, PutOrCall, SettlementMethod,
            TemplateField,
        };
        let store = MemoryStore::new();
        let osi = ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "occ_osi".into(),
                value: "AAPL  261120C00230000".into(),
                ..Default::default()
            }],
            as_of_ns: NOW,
            stated_asset_class: AssetClass::Derivative as i32,
            ..Default::default()
        };
        let terms = ListedOption {
            underlying_instrument_id: "INS-AAPL".into(),
            put_or_call: PutOrCall::Call as i32,
            strike_price: Some(meridian_domain::v1::Money {
                amount: Some(
                    "230"
                        .parse::<meridian_domain::exact::Exact>()
                        .unwrap()
                        .to_wire(),
                ),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            expiration_date: "2026-11-20".into(),
            exercise_style: ExerciseStyle::American as i32,
            settlement_method: SettlementMethod::Physical as i32,
            contract_multiplier: Some(
                "100"
                    .parse::<meridian_domain::exact::Exact>()
                    .unwrap()
                    .to_wire(),
            ),
            option_root: "AAPL".into(),
            ..Default::default()
        };
        let option = InstrumentRecord {
            instrument_id: "INS-AAPLC230".into(),
            identifiers: osi.identifiers.clone(),
            asset_class: AssetClass::Derivative as i32,
            instrument_type: InstrumentType::ListedOption as i32,
            currency: "USD".into(),
            description: "AAPL Nov 20 2026 230 call".into(),
            version: 1,
            template_id: "TPL-listed-option".into(),
            template_version: "1".into(),
            listed_option: Some(terms.clone()),
            ..Default::default()
        };
        unmatched(&store, &osi, PlatformSaid::Record(Box::new(option.clone())));
        let held = store.by_id("INS-AAPLC230").unwrap().unwrap();
        let wire = crate::record::to_wire(&held);
        assert_eq!(wire.listed_option.as_ref(), Some(&terms));
        assert!(wire.future.is_none());
        let field = |path: &str, requirement: FieldRequirement| TemplateField {
            path: path.into(),
            r#type: "text".into(),
            required: requirement == FieldRequirement::Mint,
            requirement: requirement as i32,
            repeated: path == "deliverable",
        };
        let template = SecurityTemplate {
            fields: vec![
                field("underlying_instrument_id", FieldRequirement::Mint),
                field("strike_price", FieldRequirement::Mint),
                field("option_root", FieldRequirement::Mint),
                field("deliverable", FieldRequirement::Complete),
                field("last_trade_date", FieldRequirement::Complete),
                field("settlement_time", FieldRequirement::Complete),
                field("contract_month", FieldRequirement::Optional),
            ],
            ..Default::default()
        };
        assert_eq!(
            missing_fields(&held, &template),
            vec!["deliverable".to_string(), "last_trade_date".to_string()]
        );
        // An adjustment: the platform's next version, the deliverable 150.
        let mut adjusted = option.clone();
        let mut new_terms = terms.clone();
        new_terms.contract_multiplier = Some(
            "150"
                .parse::<meridian_domain::exact::Exact>()
                .unwrap()
                .to_wire(),
        );
        adjusted.listed_option = Some(new_terms.clone());
        adjusted.version = 2;
        let (kept, minted) =
            keep_platform_record(&store, &[], &adjusted, None, Vec::new(), NOW + 1).unwrap();
        assert!(!minted);
        assert_eq!(kept.version, held.version + 1);
        assert_eq!(crate::record::to_wire(&kept).listed_option, Some(new_terms));
        // Expired by the feed: kept, never merged or decommissioned.
        let entries = vec![meridian_domain::v1::ReplacementFeedEntry {
            sequence: 7,
            instrument_id: "INS-AAPLC230".into(),
            replaced_by: String::new(),
            decommissioned: false,
            expired: true,
        }];
        assert_eq!(apply_feed(&store, &entries, NOW + 2).unwrap().len(), 1);
        let expired = store.by_id("INS-AAPLC230").unwrap().unwrap();
        assert_eq!(expired.lifecycle_state, EXPIRED);
        assert!(!expired.local.decommissioned);
        assert_eq!(
            crate::replace::current(&store, "INS-AAPLC230").unwrap(),
            "INS-AAPLC230"
        );
    }

    /// The platform's answers by the identifier asked about; not reached
    /// for any other.
    struct Says(
        std::sync::Mutex<Vec<(String, crate::reask::Answer)>>,
        std::sync::Mutex<Vec<EscalateInstrumentRequest>>,
    );

    impl crate::reask::Ask for Says {
        fn ask(&self, request: EscalateInstrumentRequest) -> crate::reask::Answer {
            let value = request
                .identifiers
                .first()
                .map(|i| i.value.clone())
                .unwrap_or_default();
            self.1.lock().unwrap().push(request);
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|(asked, _)| *asked == value)
                .map(|(_, answer)| answer.clone())
                .unwrap_or_else(|| PlatformSaid::Unreachable.into())
        }
    }

    fn kept_before_v24(store: &MemoryStore, id: &str, scheme: &str, value: &str) {
        let record = Instrument {
            instrument_id: id.into(),
            identifiers: vec![Identifier {
                scheme: scheme.into(),
                value: value.into(),
                source: String::new(),
                valid_from_ns: 0,
                valid_to_ns: None,
                trading_currency: String::new(),
            }],
            asset_class: "ASSET_CLASS_EQUITY".into(),
            currency: "USD".into(),
            description: "kept since v10".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 1,
            ..Default::default()
        };
        let first = Version {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            prepared_call_id: String::new(),
            instrument_id: id.into(),
            version: 1,
            operation: "mint".into(),
            changes: Vec::new(),
            person: String::new(),
            instance_id: String::new(),
            note: String::new(),
            merged_instrument_id: String::new(),
            record_time_ns: 0,
        };
        store.mint(record, &format!("held:{id}"), first).unwrap();
    }

    #[test]
    fn the_migration_sends_each_record_kept_before_v24_and_records_its_line() {
        let store = MemoryStore::new();
        kept_before_v24(&store, "LCL-FIGI", "share_class_figi", "BBG001S5N8V8");
        kept_before_v24(&store, "LCL-CUSIP", "cusip", "037833100");
        kept_before_v24(&store, "LCL-AWAY", "figi", "BBG000AWAY01");
        let says = Says(
            std::sync::Mutex::new(vec![(
                "BBG001S5N8V8".to_string(),
                crate::reask::Answer {
                    said: PlatformSaid::Record(Box::new(master("INS-01J8ZZTP", "BBG001S5N8V8"))),
                    minted: true,
                    near_match: false,
                },
            )]),
            std::sync::Mutex::new(Vec::new()),
        );
        let writing = std::sync::Mutex::new(());
        // Passes until the unreachable one stops none of the others.
        let mut changed = Vec::new();
        for at in 0..3 {
            changed.extend(
                crate::reask::migrate(&store, &says, &writing, 100, NOW + at)
                    .unwrap()
                    .1,
            );
        }
        // The cusip-only record is kept local, never sent; the unreachable
        // one says so, and is sent again later.
        assert!(says.1.lock().unwrap().iter().all(|request| request
            .identifiers
            .iter()
            .all(|identifier| identifier.scheme != "cusip")));
        let figi = store.by_id("LCL-FIGI").unwrap().unwrap();
        assert!(changed
            .iter()
            .any(|record| record.instrument_id == "LCL-FIGI"));
        assert_eq!(figi.local.migration, crate::reask::section::MINTED);
        assert_eq!(figi.local.migration_answer, "INS-01J8ZZTP");
        assert!(figi.local.provisional, "provisional until its line is made");
        assert_eq!(
            crate::replace::current(&store, "LCL-FIGI").unwrap(),
            "LCL-FIGI",
            "never applied without the admin"
        );
        let live = store.all().unwrap();
        let counts = crate::reask::migration_counts(&store, &live)
            .unwrap()
            .unwrap();
        assert_eq!(
            (counts.minted, counts.kept_local, counts.unreachable),
            (1, 1, 1)
        );
        // Resumable: the next pass sends what remains.
        let says_again = Says(
            std::sync::Mutex::new(vec![(
                "BBG000AWAY01".to_string(),
                PlatformSaid::Pending.into(),
            )]),
            std::sync::Mutex::new(Vec::new()),
        );
        for at in 1..4 {
            crate::reask::migrate(&store, &says_again, &writing, 100, NOW + at).unwrap();
        }
        let live = store.all().unwrap();
        let counts = crate::reask::migration_counts(&store, &live)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                counts.minted,
                counts.kept_local,
                counts.awaiting_staff,
                counts.unreachable,
                counts.remaining
            ),
            (1, 1, 1, 0, 0)
        );
        assert!(store.state(crate::reask::MIGRATED).unwrap().is_some());
        // Kept local, nothing to look for: the contract's INCOMPLETE, no
        // sixth reason.
        let cusip = store.by_id("LCL-CUSIP").unwrap().unwrap();
        assert_eq!(cusip.local.migration, crate::reask::section::KEPT_LOCAL);
        assert_eq!(cusip.local.reason, ProvisionalReason::Incomplete as i32);
    }

    #[test]
    fn a_provisional_record_is_asked_again_and_resolved_once_reached() {
        let store = MemoryStore::new();
        let minted = unmatched(
            &store,
            &equity_figi("BBG001S5N8V8"),
            PlatformSaid::Unreachable,
        );
        let says = Says(
            std::sync::Mutex::new(vec![(
                "BBG001S5N8V8".to_string(),
                PlatformSaid::Record(Box::new(master("INS-01J8ZZTP", "BBG001S5N8V8"))).into(),
            )]),
            std::sync::Mutex::new(Vec::new()),
        );
        let writing = std::sync::Mutex::new(());
        let mut asked = std::collections::HashMap::new();
        // Not yet due: asked at its minting, and an unreachable record is
        // tried again only after a few minutes.
        assert!(
            crate::reask::ask_again(&store, &says, &writing, &mut asked, 100, NOW + 1)
                .unwrap()
                .is_empty()
        );
        let later = NOW + crate::reask::UNREACHED_AGAIN_NS;
        let applied =
            crate::reask::ask_again(&store, &says, &writing, &mut asked, 100, later).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(
            crate::replace::current(&store, &minted.reply.instrument_id).unwrap(),
            "INS-01J8ZZTP"
        );
        // Each ask a new request key; never the LCL- ID.
        let sent = says.1.lock().unwrap();
        assert!(!sent[0].request_key.is_empty());
        assert!(sent[0]
            .identifiers
            .iter()
            .all(|i| !i.value.starts_with("LCL-") && i.scheme != LOCAL_SCHEME));
    }

    /// W3.26 (contract v24; the names approved 2026-10-15): the admin makes
    /// the lines -- the exact ones together, a duplicate line's two records
    /// replaced by the one INS- record, one held back keeping its LCL- ID --
    /// all or nothing, each version naming the person and the change.
    #[test]
    fn the_admin_makes_the_lines_together_or_not_at_all() {
        use crate::complete::Actor;
        use crate::reask::{apply_migration, LinesRefused};
        use meridian_domain::v1::ApplyInstrumentMigrationRequest as Apply;
        let store = MemoryStore::new();
        kept_before_v24(&store, "LCL-A", "share_class_figi", "BBG000000AAA");
        kept_before_v24(&store, "LCL-B", "share_class_figi", "BBG000000BBB");
        kept_before_v24(&store, "LCL-C", "share_class_figi", "BBG000000CCC");
        kept_before_v24(&store, "LCL-CUSIP", "cusip", "037833100");
        let answer = |figi: &str, ins: &str| {
            (
                figi.to_string(),
                crate::reask::Answer {
                    said: PlatformSaid::Record(Box::new(master(ins, figi))),
                    minted: false,
                    near_match: false,
                },
            )
        };
        let says = Says(
            std::sync::Mutex::new(vec![
                answer("BBG000000AAA", "INS-ONE"),
                // The same security kept twice: a duplicate line.
                answer("BBG000000BBB", "INS-ONE"),
                answer("BBG000000CCC", "INS-TWO"),
            ]),
            std::sync::Mutex::new(Vec::new()),
        );
        let writing = std::sync::Mutex::new(());
        crate::reask::migrate(&store, &says, &writing, 100, NOW).unwrap();
        // Answered, not yet made: INCOMPLETE, still provisional (ruled
        // 2026-10-15).
        for id in ["LCL-A", "LCL-B", "LCL-C"] {
            let line = store.by_id(id).unwrap().unwrap();
            assert_eq!(
                line.local.reason,
                ProvisionalReason::Incomplete as i32,
                "{id}"
            );
            assert_eq!(line.local.migration, crate::reask::section::MATCHED, "{id}");
            assert!(line.local.provisional);
        }
        let admin = Actor {
            person: "admin@firm".into(),
            prepared_call_id: "PREP-1".into(),
            ..Actor::default()
        };
        // A record that is no line the platform answered: refused by its
        // path, and nothing changes.
        let refused = apply_migration(
            &store,
            &Apply {
                instrument_ids: vec!["LCL-A".into(), "LCL-CUSIP".into()],
                ..Apply::default()
            },
            &admin,
            NOW + 1,
        )
        .unwrap()
        .unwrap_err();
        assert!(
            matches!(&refused, LinesRefused::NotALine { path, .. } if path == "instrument_ids[1]"),
            "{refused:?}"
        );
        assert_eq!(crate::replace::current(&store, "LCL-A").unwrap(), "LCL-A");
        // Who: a person, and through a delegation a note; at most 500.
        assert_eq!(
            apply_migration(&store, &Apply::default(), &Actor::default(), NOW)
                .unwrap()
                .unwrap_err(),
            LinesRefused::ActorRequired
        );
        let through = Actor {
            delegation: "DLG-1".into(),
            ..admin.clone()
        };
        assert_eq!(
            apply_migration(&store, &Apply::default(), &through, NOW)
                .unwrap()
                .unwrap_err(),
            LinesRefused::NoteRequired
        );
        let many = Apply {
            held_back_ids: (0..501).map(|n| format!("LCL-{n}")).collect(),
            ..Apply::default()
        };
        assert_eq!(
            apply_migration(&store, &many, &admin, NOW)
                .unwrap()
                .unwrap_err(),
            LinesRefused::TooMany("held_back_ids".into())
        );

        // The exact lines made, one held back.
        let made = apply_migration(
            &store,
            &Apply {
                instrument_ids: vec!["LCL-A".into(), "LCL-B".into()],
                held_back_ids: vec!["LCL-C".into()],
                note: "the exact lines".into(),
            },
            &admin,
            NOW + 2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(made.applied.len(), 2);
        assert_eq!(crate::replace::current(&store, "LCL-A").unwrap(), "INS-ONE");
        assert_eq!(crate::replace::current(&store, "LCL-B").unwrap(), "INS-ONE");
        let event = made.applied[0].replaced.as_ref().unwrap();
        assert_eq!(event.cause, ReplacementCause::Resolved as i32);
        let history = store.history("INS-ONE").unwrap();
        assert!(history.iter().all(|version| version.person == "admin@firm"
            && version.prepared_call_id == "PREP-1"
            && version.note.ends_with(": the exact lines")));
        let held = store.by_id("LCL-C").unwrap().unwrap();
        assert_eq!(held.local.reason, ProvisionalReason::HeldBack as i32);
        assert_eq!(held.local.migration, crate::reask::section::HELD_BACK);
        assert!(held.local.provisional, "keeps its LCL- ID");
        assert!(
            store.by_id("INS-TWO").unwrap().is_none(),
            "nothing minted for it"
        );
        let live: Vec<_> = store
            .all()
            .unwrap()
            .into_iter()
            .filter(|record| {
                store
                    .replacement_of(&record.instrument_id)
                    .unwrap()
                    .is_none()
            })
            .collect();
        let counts = crate::reask::migration_counts(&store, &live)
            .unwrap()
            .unwrap();
        assert_eq!(
            (counts.matched, counts.held_back, counts.kept_local),
            (0, 1, 1)
        );

        // A line made already: unchanged. A made line is not held back.
        let again = apply_migration(
            &store,
            &Apply {
                instrument_ids: vec!["LCL-A".into()],
                ..Apply::default()
            },
            &admin,
            NOW + 3,
        )
        .unwrap()
        .unwrap();
        assert!(again.applied.is_empty() && again.held_back.is_empty());
        let refused = apply_migration(
            &store,
            &Apply {
                held_back_ids: vec!["LCL-B".into()],
                ..Apply::default()
            },
            &admin,
            NOW + 3,
        )
        .unwrap()
        .unwrap_err();
        assert!(
            matches!(&refused, LinesRefused::NotALine { path, .. } if path == "held_back_ids[0]")
        );
        // The held-back line made later, as any line is.
        let later = apply_migration(
            &store,
            &Apply {
                instrument_ids: vec!["LCL-C".into()],
                ..Apply::default()
            },
            &admin,
            NOW + 4,
        )
        .unwrap()
        .unwrap();
        assert_eq!(later.applied.len(), 1);
        assert_eq!(crate::replace::current(&store, "LCL-C").unwrap(), "INS-TWO");
    }
}
