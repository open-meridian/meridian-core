//! What the edge keeps of its own (contract v11; W4.4, W4.1, W4.5, W4.8).
//!
//! spec/vendor-differences-have-a-place-in-the-contract, slice A: the sidecar
//! is the normalization boundary, and it checks what a plugin at the edge
//! sends beside the contract's values without reading any vendor's meaning:
//!
//! - **A value as reported** -- the one place a vendor's own code crosses,
//!   beside a field's not-known value -- is checked for its shape alone: its
//!   scheme, code and text each present and within the dictionary's length.
//! - **A raw record's reference** names the sending plugin's own instance:
//!   left empty, it is filled with it; naming another, it is refused. No
//!   plugin writes a reference into another's storage (decisions/028).
//! - **A provenance** names its value and one of the four kinds; its texts
//!   within their lengths; its raw record the sender's own.
//! - **A backfill** names its version and field within their lengths.
//!
//! And a version's declaration, at registration (W4.1), and the names not
//! carried a plugin saw, on its heartbeat (W4.5): each bound the dictionary's,
//! a secret setting's name one the plugin declares secret, a reason given, and
//! storage asked for only by a plugin holding an edge role.
//!
//! From contract v16 the declaration's kinds of raw record (W4.1): each
//! named once, in lowercase letters, digits and underscores beginning with
//! a letter, labelled and given a window within the dictionary's bounds; and
//! the two settings the SDK declares for each, `<kind>_window_days` and
//! `<kind>_past_window`, reserved: a setting taking one of those names that
//! is not the SDK's -- a whole number of days, and a choice of archived,
//! kept or deleted -- refuses the registration, naming it. And what each
//! kind holds in storage, on the heartbeat (W4.5): at most one entry per
//! declared kind, a kind the declaration does not name refused by its path.
//!
//! Every bound is the data dictionary's, as `meridian_pb::bounds` generates
//! it; none is written here. A refusal is INVALID_ARGUMENT naming the field.

// The tonic surface returns `Result<_, Status>` everywhere, as typed.rs says;
// boxing the error here alone would buy nothing.
#![allow(clippy::result_large_err)]

use meridian_domain::v1::{
    ExternalAccountsEvent, MissingInstrumentDetectedEvent, RecordHoldingRequest,
    RecordHoldingsStatementRequest,
};
use meridian_pb::bounds::{
    Length, AS_REPORTED_CODE_LENGTH, AS_REPORTED_SCHEME_LENGTH, AS_REPORTED_TEXT_LENGTH,
    BACKFILL_CONTRACT_VERSION_LENGTH, BACKFILL_FIELD_LENGTH,
    HEARTBEAT_REQUEST_NOT_CARRIED_SEEN_COUNT, NOT_CARRIED_NAME_LENGTH, NOT_CARRIED_ROLE_LENGTH,
    NOT_CARRIED_SCHEME_LENGTH, NOT_CARRIED_SEEN_NAME_LENGTH, NOT_CARRIED_SEEN_SCHEME_LENGTH,
    PLUGIN_DECLARATION_NOT_CARRIED_COUNT, PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT,
    PROVENANCE_FIELD_LENGTH, PROVENANCE_PERSON_LENGTH, PROVENANCE_RULE_LENGTH,
    PROVENANCE_SOURCE_LENGTH, RAW_RECORD_KIND_LABEL_LENGTH, RAW_RECORD_KIND_NAME_LENGTH,
    RAW_RECORD_KIND_WINDOW_DAYS_RANGE, RAW_RECORD_REF_KEY_LENGTH,
    RECORD_HOLDINGS_STATEMENT_REQUEST_PROVENANCE_COUNT, RECORD_HOLDING_REQUEST_PENDING_COUNT,
    RECORD_HOLDING_REQUEST_PROVENANCE_COUNT, STORAGE_DECLARATION_RECORD_KINDS_COUNT,
    STORAGE_DECLARATION_RETENTION_DAYS_RANGE, STORED_SPAN_RECORD_KIND_LENGTH,
};
use meridian_pb::v1::{
    AsReported, Backfill, NotCarriedReason, NotCarriedSeen, PluginDeclaration, Provenance,
    ProvenanceKind, RawRecordKind, RawRecordRef, SettingDeclaration, SettingType, StoredSpan,
};
use prost::Message;
use tonic::Status;

use crate::service::Sidecar;

use meridian_domain::EDGE_ROLES;

const EXTERNAL_ACCOUNTS: &str = "meridian.v1.ExternalAccountsEvent";
const MISSING_INSTRUMENT: &str = "meridian.v1.MissingInstrumentDetectedEvent";
const STATEMENT: &str = "meridian.v1.RecordHoldingsStatementRequest";
const HOLDING: &str = "meridian.v1.RecordHoldingRequest";

impl Sidecar {
    /// The message as it may leave: what the edge keeps checked, and each
    /// raw record's instance filled with this one; or the refusal naming the
    /// field. Any other message passes untouched.
    #[allow(clippy::result_large_err)]
    pub(crate) fn kept_at_the_edge<D: Message + Default>(
        &self,
        payload_type: &str,
        message: D,
    ) -> Result<D, Status> {
        let instance = self.instance_id().to_string();
        let checked = match payload_type {
            EXTERNAL_ACCOUNTS => {
                let event = read::<ExternalAccountsEvent>(&message)?;
                for (i, account) in event.accounts.iter().enumerate() {
                    as_reported(
                        &format!("accounts[{i}].account_kind_as_reported"),
                        account.account_kind_as_reported.as_ref(),
                    )?;
                }
                None
            }
            MISSING_INSTRUMENT => {
                let event = read::<MissingInstrumentDetectedEvent>(&message)?;
                as_reported(
                    "asset_class_as_reported",
                    event.asset_class_as_reported.as_ref(),
                )?;
                None
            }
            STATEMENT => {
                let mut statement = read::<RecordHoldingsStatementRequest>(&message)?;
                own_record("raw_record", statement.raw_record.as_mut(), &instance)?;
                provenance(
                    &mut statement.provenance,
                    RECORD_HOLDINGS_STATEMENT_REQUEST_PROVENANCE_COUNT.most,
                    &instance,
                )?;
                Some(statement.encode_to_vec())
            }
            HOLDING => {
                let mut row = read::<RecordHoldingRequest>(&message)?;
                own_record("raw_record", row.raw_record.as_mut(), &instance)?;
                provenance(
                    &mut row.provenance,
                    RECORD_HOLDING_REQUEST_PROVENANCE_COUNT.most,
                    &instance,
                )?;
                if row.pending.len() > RECORD_HOLDING_REQUEST_PENDING_COUNT.most {
                    return Err(refuse(format!(
                        "pending has {} quantities; at most {}",
                        row.pending.len(),
                        RECORD_HOLDING_REQUEST_PENDING_COUNT.most
                    )));
                }
                for (i, pending) in row.pending.iter().enumerate() {
                    if pending.value_date.is_empty() {
                        return Err(refuse(format!(
                            "pending[{i}].value_date is empty; a pending quantity names its date"
                        )));
                    }
                }
                backfill(row.backfill.as_ref())?;
                Some(row.encode_to_vec())
            }
            _ => None,
        };
        match checked {
            None => Ok(message),
            Some(bytes) => D::decode(bytes.as_slice()).map_err(|failed| {
                Status::internal(format!("the checked message did not read back: {failed}"))
            }),
        }
        .inspect_err(|refused| self.note_refusal(refused.message()))
    }
}

fn read<M: Message + Default>(message: &impl Message) -> Result<M, Status> {
    M::decode(message.encode_to_vec().as_slice())
        .map_err(|failed| Status::internal(format!("the message did not read: {failed}")))
}

fn refuse(words: String) -> Status {
    Status::invalid_argument(words)
}

fn length(field: &str, text: &str, bound: Length) -> Result<(), Status> {
    let characters = text.chars().count();
    if bound.admits(characters) {
        return Ok(());
    }
    Err(refuse(if characters == 0 {
        format!(
            "{field} is empty; it is {} to {} characters",
            bound.least, bound.most
        )
    } else {
        format!("{field} is {characters} characters; at most {}", bound.most)
    }))
}

/// A value as reported: its shape and length, never its meaning (W4.4).
pub(crate) fn as_reported(field: &str, value: Option<&AsReported>) -> Result<(), Status> {
    let Some(value) = value else {
        return Ok(());
    };
    for (part, text, bound) in [
        ("scheme", &value.scheme, AS_REPORTED_SCHEME_LENGTH),
        ("code", &value.code, AS_REPORTED_CODE_LENGTH),
        ("text", &value.text, AS_REPORTED_TEXT_LENGTH),
    ] {
        if text.is_empty() {
            return Err(refuse(format!(
                "{field}.{part} is empty; a value as reported carries its scheme, code and text"
            )));
        }
        length(&format!("{field}.{part}"), text, bound)?;
    }
    Ok(())
}

/// A raw record's reference: the sender's own instance, filled when empty.
pub(crate) fn own_record(
    field: &str,
    raw: Option<&mut RawRecordRef>,
    instance: &str,
) -> Result<(), Status> {
    let Some(raw) = raw else {
        return Ok(());
    };
    if raw.instance_id.is_empty() {
        raw.instance_id = instance.to_string();
    } else if raw.instance_id != instance {
        return Err(refuse(format!(
            "{field}.instance_id names {}, and a plugin references only its own raw records",
            raw.instance_id
        )));
    }
    length(&format!("{field}.key"), &raw.key, RAW_RECORD_REF_KEY_LENGTH)
}

/// Each provenance: its value named, its kind one of the four, its texts
/// within their lengths, its raw record the sender's own.
pub(crate) fn provenance(
    provenance: &mut [Provenance],
    most: usize,
    instance: &str,
) -> Result<(), Status> {
    if provenance.len() > most {
        return Err(refuse(format!(
            "provenance has {} entries; at most {most}",
            provenance.len()
        )));
    }
    for (i, held) in provenance.iter_mut().enumerate() {
        let at = format!("provenance[{i}]");
        length(&format!("{at}.field"), &held.field, PROVENANCE_FIELD_LENGTH)?;
        match ProvenanceKind::try_from(held.kind) {
            Ok(ProvenanceKind::Unspecified) => {
                return Err(refuse(format!("{at}.kind is unspecified")))
            }
            Err(_) => {
                return Err(refuse(format!(
                    "{at}.kind is {}, which the contract does not define",
                    held.kind
                )))
            }
            Ok(_) => {}
        }
        length(
            &format!("{at}.source"),
            &held.source,
            PROVENANCE_SOURCE_LENGTH,
        )?;
        length(
            &format!("{at}.person"),
            &held.person,
            PROVENANCE_PERSON_LENGTH,
        )?;
        length(&format!("{at}.rule"), &held.rule, PROVENANCE_RULE_LENGTH)?;
        own_record(
            &format!("{at}.raw_record"),
            held.raw_record.as_mut(),
            instance,
        )?;
    }
    Ok(())
}

/// A backfill's cause, within its lengths.
pub(crate) fn backfill(backfill: Option<&Backfill>) -> Result<(), Status> {
    let Some(backfill) = backfill else {
        return Ok(());
    };
    length(
        "backfill.contract_version",
        &backfill.contract_version,
        BACKFILL_CONTRACT_VERSION_LENGTH,
    )?;
    length("backfill.field", &backfill.field, BACKFILL_FIELD_LENGTH)
}

/// Why a version's declaration cannot stand, or nothing (W4.1, contract v11).
pub fn declaration_refused(
    declaration: &PluginDeclaration,
    settings: &[SettingDeclaration],
    roles: &[String],
) -> Option<String> {
    if !PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT.admits(declaration.secret_settings.len()) {
        return Some(format!(
            "declaration.secret_settings names {}; at most {}",
            declaration.secret_settings.len(),
            PLUGIN_DECLARATION_SECRET_SETTINGS_COUNT.most
        ));
    }
    for (i, name) in declaration.secret_settings.iter().enumerate() {
        let secret = settings
            .iter()
            .any(|setting| setting.name == *name && setting.secret);
        if !secret {
            return Some(format!(
                "declaration.secret_settings[{i}] names {name:?}, which the plugin does not \
                 declare as a secret setting"
            ));
        }
    }
    if !PLUGIN_DECLARATION_NOT_CARRIED_COUNT.admits(declaration.not_carried.len()) {
        return Some(format!(
            "declaration.not_carried names {}; at most {}",
            declaration.not_carried.len(),
            PLUGIN_DECLARATION_NOT_CARRIED_COUNT.most
        ));
    }
    for (i, held) in declaration.not_carried.iter().enumerate() {
        let at = format!("declaration.not_carried[{i}]");
        for (part, text, bound) in [
            ("role", &held.role, NOT_CARRIED_ROLE_LENGTH),
            ("scheme", &held.scheme, NOT_CARRIED_SCHEME_LENGTH),
            ("name", &held.name, NOT_CARRIED_NAME_LENGTH),
        ] {
            if let Err(refused) = length(&format!("{at}.{part}"), text, bound) {
                return Some(refused.message().to_string());
            }
        }
        if !roles.contains(&held.role) {
            return Some(format!(
                "{at}.role is {:?}, which this plugin was not launched as",
                held.role
            ));
        }
        if !matches!(
            NotCarriedReason::try_from(held.reason),
            Ok(reason) if reason != NotCarriedReason::Unspecified
        ) {
            return Some(format!(
                "{at}.reason is unspecified; say why it is not carried"
            ));
        }
    }
    if let Some(storage) = &declaration.storage {
        if !STORAGE_DECLARATION_RETENTION_DAYS_RANGE.admits(i64::from(storage.retention_days)) {
            return Some(format!(
                "declaration.storage.retention_days is {}; {} to {}",
                storage.retention_days,
                STORAGE_DECLARATION_RETENTION_DAYS_RANGE.least,
                STORAGE_DECLARATION_RETENTION_DAYS_RANGE.most
            ));
        }
        if !roles.iter().any(|role| EDGE_ROLES.contains(&role.as_str())) {
            return Some(
                "declaration.storage is asked for by a plugin holding no edge role; only the \
                 edge roles own storage (decisions/028)"
                    .into(),
            );
        }
        if let Some(refused) = kinds_refused(&storage.record_kinds, settings) {
            return Some(refused);
        }
    }
    None
}

/// What may be done with a kind's records past its window (W6.11, contract
/// v16): the choices of `<kind>_past_window`, the SDK's for every edge plugin.
pub const PAST_WINDOW: [&str; 3] = ["archived", "kept", "deleted"];

/// The name of a kind's window setting, `<kind>_window_days` (W6.11).
pub fn window_setting(kind: &str) -> String {
    format!("{kind}_window_days")
}

/// The name of a kind's past-the-window setting, `<kind>_past_window`.
pub fn past_window_setting(kind: &str) -> String {
    format!("{kind}_past_window")
}

/// A kind's name: lowercase letters, digits and underscores, beginning with
/// a letter, within the dictionary's length.
fn kind_name_reads(name: &str) -> bool {
    RAW_RECORD_KIND_NAME_LENGTH.admits(name.chars().count())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Why the kinds of raw record a version declares cannot stand, or nothing
/// (W4.1, contract v16): their count, each name once and of its form, each
/// label and window within the dictionary's bounds; and each kind's two
/// window settings, where declared, the SDK's.
pub fn kinds_refused(kinds: &[RawRecordKind], settings: &[SettingDeclaration]) -> Option<String> {
    if !STORAGE_DECLARATION_RECORD_KINDS_COUNT.admits(kinds.len()) {
        return Some(format!(
            "declaration.storage.record_kinds names {}; at most {}",
            kinds.len(),
            STORAGE_DECLARATION_RECORD_KINDS_COUNT.most
        ));
    }
    for (i, kind) in kinds.iter().enumerate() {
        let at = format!("declaration.storage.record_kinds[{i}]");
        if !kind_name_reads(&kind.name) {
            return Some(format!(
                "{at}.name is {:?}; a kind's name is 1 to {} lowercase letters, digits and \
                 underscores, beginning with a letter",
                kind.name, RAW_RECORD_KIND_NAME_LENGTH.most
            ));
        }
        if kinds[..i].iter().any(|earlier| earlier.name == kind.name) {
            return Some(format!(
                "{at}.name is {:?}, which an earlier kind names; each kind is named once",
                kind.name
            ));
        }
        if let Err(refused) = length(
            &format!("{at}.label"),
            &kind.label,
            RAW_RECORD_KIND_LABEL_LENGTH,
        ) {
            return Some(refused.message().to_string());
        }
        if !RAW_RECORD_KIND_WINDOW_DAYS_RANGE.admits(i64::from(kind.window_days)) {
            return Some(format!(
                "{at}.window_days is {}; {} to {}",
                kind.window_days,
                RAW_RECORD_KIND_WINDOW_DAYS_RANGE.least,
                RAW_RECORD_KIND_WINDOW_DAYS_RANGE.most
            ));
        }
        for setting in settings {
            if setting.name == window_setting(&kind.name) && !is_window_setting(setting) {
                return Some(format!(
                    "the setting {} takes the name of {}'s window, which is the SDK's: a whole \
                     number of days, not secret (W6.11)",
                    setting.name, kind.name
                ));
            }
            if setting.name == past_window_setting(&kind.name) && !is_past_window_setting(setting) {
                return Some(format!(
                    "the setting {} takes the name of what is done with {}'s records past \
                     their window, which is the SDK's: a choice of {}, not secret (W6.11)",
                    setting.name,
                    kind.name,
                    PAST_WINDOW.join(", ")
                ));
            }
        }
    }
    None
}

/// The SDK's window setting: a whole number of days, not secret.
fn is_window_setting(setting: &SettingDeclaration) -> bool {
    setting.r#type == SettingType::Integer as i32 && !setting.secret
}

/// The SDK's past-the-window setting: a choice of archived, kept or deleted,
/// each once, not secret.
fn is_past_window_setting(setting: &SettingDeclaration) -> bool {
    let mut offered: Vec<&str> = setting.choices.iter().map(|c| c.value.as_str()).collect();
    offered.sort_unstable();
    let mut expected = PAST_WINDOW;
    expected.sort_unstable();
    setting.r#type == SettingType::Choice as i32 && !setting.secret && offered == expected
}

/// Why what a heartbeat says each kind holds in storage cannot stand, as the
/// field's path and the words; or nothing (W4.5, contract v16).
pub fn stored_refused(
    stored: &[StoredSpan],
    declaration: Option<&PluginDeclaration>,
) -> Option<(String, String)> {
    if stored.is_empty() {
        return None;
    }
    use meridian_pb::bounds::HEARTBEAT_REQUEST_STORED_COUNT;
    if !HEARTBEAT_REQUEST_STORED_COUNT.admits(stored.len()) {
        return Some((
            "stored".into(),
            format!(
                "stored names {} kinds; at most {}",
                stored.len(),
                HEARTBEAT_REQUEST_STORED_COUNT.most
            ),
        ));
    }
    let kinds: Vec<&str> = declaration
        .and_then(|declaration| declaration.storage.as_ref())
        .map(|storage| {
            storage
                .record_kinds
                .iter()
                .map(|k| k.name.as_str())
                .collect()
        })
        .unwrap_or_default();
    for (i, span) in stored.iter().enumerate() {
        let at = format!("stored[{i}].record_kind");
        if let Err(refused) = length(&at, &span.record_kind, STORED_SPAN_RECORD_KIND_LENGTH) {
            return Some((at, refused.message().to_string()));
        }
        if !kinds.contains(&span.record_kind.as_str()) {
            return Some((
                at.clone(),
                format!(
                    "{at} is {:?}, a kind this version's declaration does not name",
                    span.record_kind
                ),
            ));
        }
        if stored[..i]
            .iter()
            .any(|earlier| earlier.record_kind == span.record_kind)
        {
            return Some((
                "stored".into(),
                format!(
                    "stored names {} twice; one entry per kind",
                    span.record_kind
                ),
            ));
        }
        if span.record_count == 0 && (span.first_received_ns != 0 || span.last_received_ns != 0) {
            return Some((
                format!("stored[{i}].first_received_ns"),
                format!("stored[{i}] holds no record, and so no span"),
            ));
        }
        if span.record_count > 0
            && (span.first_received_ns <= 0 || span.last_received_ns < span.first_received_ns)
        {
            return Some((
                format!("stored[{i}].last_received_ns"),
                format!(
                    "stored[{i}]'s span runs from {} to {}; the first is after the epoch and \
                     the last never before it",
                    span.first_received_ns, span.last_received_ns
                ),
            ));
        }
    }
    None
}

/// Why the names not carried a heartbeat reports cannot stand, or nothing.
pub fn seen_refused(seen: &[NotCarriedSeen]) -> Option<String> {
    if !HEARTBEAT_REQUEST_NOT_CARRIED_SEEN_COUNT.admits(seen.len()) {
        return Some(format!(
            "not_carried_seen names {}; at most {}",
            seen.len(),
            HEARTBEAT_REQUEST_NOT_CARRIED_SEEN_COUNT.most
        ));
    }
    for (i, held) in seen.iter().enumerate() {
        for (part, text, bound) in [
            ("scheme", &held.scheme, NOT_CARRIED_SEEN_SCHEME_LENGTH),
            ("name", &held.name, NOT_CARRIED_SEEN_NAME_LENGTH),
        ] {
            if let Err(refused) = length(&format!("not_carried_seen[{i}].{part}"), text, bound) {
                return Some(refused.message().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_pb::v1::{NotCarried, StorageDeclaration};

    fn reported(scheme: &str, code: &str, text: &str) -> AsReported {
        AsReported {
            scheme: scheme.into(),
            code: code.into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_value_as_reported_is_refused_for_a_part_missing_or_too_long() {
        assert!(as_reported(
            "k",
            Some(&reported(
                "snaptrade:account-type",
                "INDIVIDUAL",
                "Individual"
            ))
        )
        .is_ok());
        assert!(as_reported("k", None).is_ok());
        let missing = as_reported(
            "accounts[0].account_kind_as_reported",
            Some(&reported("s", "c", "")),
        )
        .unwrap_err();
        assert_eq!(missing.code(), tonic::Code::InvalidArgument);
        assert!(missing
            .message()
            .starts_with("accounts[0].account_kind_as_reported.text is empty"));
        let long = as_reported("k", Some(&reported("s", &"x".repeat(129), "t"))).unwrap_err();
        assert_eq!(long.message(), "k.code is 129 characters; at most 128");
    }

    #[test]
    fn a_raw_record_is_the_senders_own_and_filled_when_empty() {
        let mut raw = RawRecordRef {
            instance_id: String::new(),
            key: "positions/A/1".into(),
        };
        own_record("raw_record", Some(&mut raw), "snaptrade-1").unwrap();
        assert_eq!(raw.instance_id, "snaptrade-1");
        let mut other = RawRecordRef {
            instance_id: "another".into(),
            key: "k".into(),
        };
        let refused = own_record("raw_record", Some(&mut other), "snaptrade-1").unwrap_err();
        assert!(refused.message().contains("names another"));
        let mut keyless = RawRecordRef::default();
        assert!(own_record("raw_record", Some(&mut keyless), "snaptrade-1").is_err());
    }

    #[test]
    fn a_provenance_names_its_kind() {
        let mut held = vec![Provenance {
            field: "settle_date_quantity".into(),
            rule: "no trade pending".into(),
            ..Default::default()
        }];
        let refused = provenance(&mut held, 32, "i").unwrap_err();
        assert_eq!(refused.message(), "provenance[0].kind is unspecified");
        held[0].kind = ProvenanceKind::Derived as i32;
        assert!(provenance(&mut held, 32, "i").is_ok());
    }

    #[test]
    fn a_declaration_asks_for_storage_only_at_the_edge_and_names_its_secrets() {
        let settings = vec![SettingDeclaration {
            name: "consumer_key".into(),
            secret: true,
            ..Default::default()
        }];
        let custody = vec!["custody".to_string()];
        let mut declaration = PluginDeclaration {
            secret_settings: vec!["consumer_key".into()],
            not_carried: vec![NotCarried {
                role: "custody".into(),
                scheme: "snaptrade:position".into(),
                name: "open_pnl".into(),
                reason: NotCarriedReason::NoContractMeaning as i32,
            }],
            storage: Some(StorageDeclaration {
                retention_days: 2555,
                ..Default::default()
            }),
        };
        assert_eq!(declaration_refused(&declaration, &settings, &custody), None);
        assert!(
            declaration_refused(&declaration, &settings, &["portfolio".into()])
                .is_some_and(|why| why.contains("role") || why.contains("edge role"))
        );
        declaration.secret_settings.push("not_a_secret".into());
        assert!(declaration_refused(&declaration, &settings, &custody)
            .is_some_and(|why| why.contains("secret_settings[1]")));
    }

    fn kind(name: &str, archivable: bool) -> RawRecordKind {
        RawRecordKind {
            name: name.into(),
            label: "Reported activity".into(),
            window_days: 2555,
            archivable,
        }
    }

    fn window(kind: &str) -> Vec<SettingDeclaration> {
        let choice = |value: &str| meridian_pb::v1::SettingChoice {
            value: value.into(),
            ..Default::default()
        };
        vec![
            SettingDeclaration {
                name: window_setting(kind),
                r#type: SettingType::Integer as i32,
                ..Default::default()
            },
            SettingDeclaration {
                name: past_window_setting(kind),
                r#type: SettingType::Choice as i32,
                choices: vec![choice("archived"), choice("kept"), choice("deleted")],
                ..Default::default()
            },
        ]
    }

    #[test]
    fn the_kinds_are_named_once_and_their_window_settings_are_the_sdks() {
        let kinds = vec![kind("activity", true), kind("responses", true)];
        let mut settings = window("activity");
        settings.extend(window("responses"));
        assert_eq!(kinds_refused(&kinds, &settings), None);
        assert_eq!(kinds_refused(&kinds, &[]), None);

        let twice = vec![kind("activity", true), kind("activity", false)];
        assert!(kinds_refused(&twice, &[])
            .is_some_and(|why| why.starts_with("declaration.storage.record_kinds[1].name")));
        for bad in ["Activity", "1st", "", "a-b", &"a".repeat(41)] {
            assert!(
                kinds_refused(&[kind(bad, true)], &[])
                    .is_some_and(|why| why.starts_with("declaration.storage.record_kinds[0].name")),
                "{bad:?} was admitted"
            );
        }
        let zero = RawRecordKind {
            window_days: 0,
            ..kind("activity", true)
        };
        assert!(kinds_refused(&[zero], &[])
            .is_some_and(|why| why.contains("record_kinds[0].window_days")));
        let unlabelled = RawRecordKind {
            label: String::new(),
            ..kind("activity", true)
        };
        assert!(kinds_refused(&[unlabelled], &[])
            .is_some_and(|why| why.contains("record_kinds[0].label")));
        let many: Vec<_> = (0..17).map(|n| kind(&format!("k{n}"), true)).collect();
        assert!(kinds_refused(&many, &[])
            .is_some_and(|why| why.starts_with("declaration.storage.record_kinds names 17")));

        // A setting taking a window's name that is not the SDK's.
        let taken = vec![SettingDeclaration {
            name: "activity_window_days".into(),
            r#type: SettingType::String as i32,
            ..Default::default()
        }];
        assert!(kinds_refused(&kinds, &taken)
            .is_some_and(|why| why.starts_with("the setting activity_window_days takes")));
        let mut short = window("activity");
        short[1].choices.pop();
        assert!(kinds_refused(&kinds, &short)
            .is_some_and(|why| why.starts_with("the setting activity_past_window takes")));
        let mut secret = window("activity");
        secret[0].secret = true;
        assert!(kinds_refused(&kinds, &secret).is_some());
    }

    #[test]
    fn what_storage_holds_is_one_entry_per_declared_kind() {
        let declaration = PluginDeclaration {
            storage: Some(StorageDeclaration {
                retention_days: 2555,
                record_kinds: vec![kind("activity", true), kind("responses", true)],
            }),
            ..Default::default()
        };
        let span = |kind: &str, count: u64| StoredSpan {
            record_kind: kind.into(),
            record_count: count,
            first_received_ns: if count > 0 {
                1_554_076_800_000_000_000
            } else {
                0
            },
            last_received_ns: if count > 0 {
                1_790_380_500_000_000_000
            } else {
                0
            },
        };
        assert_eq!(
            stored_refused(
                &[span("activity", 48_210), span("responses", 0)],
                Some(&declaration)
            ),
            None
        );
        assert_eq!(
            stored_refused(&[span("statements", 3)], Some(&declaration)).map(|(path, _)| path),
            Some("stored[0].record_kind".into())
        );
        assert_eq!(
            stored_refused(
                &[span("activity", 1), span("activity", 2)],
                Some(&declaration)
            )
            .map(|(path, _)| path),
            Some("stored".into())
        );
        let many: Vec<_> = (0..17).map(|_| span("activity", 1)).collect();
        assert_eq!(
            stored_refused(&many, Some(&declaration)).map(|(path, _)| path),
            Some("stored".into())
        );
        assert!(stored_refused(&[span("activity", 1)], None).is_some());
        let backwards = StoredSpan {
            last_received_ns: 1,
            ..span("activity", 1)
        };
        assert!(stored_refused(&[backwards], Some(&declaration)).is_some());
    }
}
