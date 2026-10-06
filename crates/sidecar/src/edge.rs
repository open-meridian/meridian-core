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
    PROVENANCE_SOURCE_LENGTH, RAW_RECORD_REF_KEY_LENGTH,
    RECORD_HOLDINGS_STATEMENT_REQUEST_PROVENANCE_COUNT, RECORD_HOLDING_REQUEST_PENDING_COUNT,
    RECORD_HOLDING_REQUEST_PROVENANCE_COUNT, STORAGE_DECLARATION_RETENTION_DAYS_RANGE,
};
use meridian_pb::v1::{
    AsReported, Backfill, NotCarriedReason, NotCarriedSeen, PluginDeclaration, Provenance,
    ProvenanceKind, RawRecordRef, SettingDeclaration,
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
}
