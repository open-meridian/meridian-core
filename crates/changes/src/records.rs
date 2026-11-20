//! A store's own records in its `control` partition: who took its writing
//! over, and what its scrub found (contract v30, stage D; the resilience
//! spec's Names, "a `control` partition in `lake`, `instrument`, `street` and
//! the configuration store: where assignments, takeovers, moves, switches
//! and findings are kept").
//!
//! Each takeover and each finding stays in the store's own table beside its
//! log (`<prefix>_writer_took_over`, `<prefix>_integrity_finding`), as since
//! step 3b, and is also a change in the store's `control` partition, in the
//! same transaction: `meridian.v1.WriterTookOver` and
//! `meridian.v1.IntegrityFinding`. So the conductor reads every store's over
//! the store's own `platform.{domain}.internal.read-changes`, as followers
//! read the log, for the Resilience page's history (`ReadResilience`). A
//! follower ignores both kinds: they change nothing a follower holds.
//!
//! The lake's control changes carry the record's protobuf bytes, as its
//! other control kinds do. A captured store's carry it as JSON (its log's
//! bodies are `jsonb`), one object of the record's fields by their proto
//! names, enums by their names ([`to_json`]); [`of_change`] reads either.

use meridian_domain::v1::{IntegrityCheck, IntegrityFinding, TakeoverReason, WriterTookOver};
use prost::Message;
use serde_json::{json, Value};

/// A takeover's control change's kind.
pub const TOOK_OVER: &str = "meridian.v1.WriterTookOver";

/// A finding's control change's kind.
pub const FINDING: &str = "meridian.v1.IntegrityFinding";

/// A store's record read from its control partition.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    TookOver(WriterTookOver),
    Finding(IntegrityFinding),
}

/// Why a takeover was made, from the words it is recorded with.
pub fn reason_of(why: &str) -> TakeoverReason {
    match why {
        crate::LEASE_RELEASED => TakeoverReason::SteppedDown,
        crate::timeline::DATABASE_TIMELINE => TakeoverReason::DatabaseTimeline,
        // Its partitions assigned to its writer group (step 3d): the group
        // count changed, and they moved to it.
        crate::groups::ASSIGNED => TakeoverReason::Moved,
        // The lease lapsed, or no member held it: the writer before (where
        // there was one) let it go without handing it over.
        _ => TakeoverReason::LeaseLapsed,
    }
}

/// The record of a takeover of `component`'s `partitions` at `epoch`.
pub fn took_over(
    component: &str,
    by: &crate::TookOver,
    partitions: Vec<String>,
    epoch: u64,
) -> WriterTookOver {
    WriterTookOver {
        component: component.to_string(),
        member: by.member.clone(),
        previous_member: by.previous.clone(),
        partitions,
        epoch,
        reason: reason_of(&by.why) as i32,
        took_over_at_ns: by.at_ns,
    }
}

/// The record of a finding of `component`'s scrub, found at `found_at_ns`.
/// Which check found it is read from where and what it was: the replica
/// scrub's member comparison (its partition `replicas`), a page that failed
/// its checksum, or else the store's own record check (`check`: a row's
/// digest, or the book's journal chain).
pub fn finding(
    component: &str,
    check: IntegrityCheck,
    found: &crate::integrity::Finding,
    found_at_ns: i64,
) -> IntegrityFinding {
    let check = if found.partition == "replicas" {
        IntegrityCheck::MemberDigest
    } else if found.what.contains("checksum") || found.what.contains("invalid page") {
        IntegrityCheck::PageChecksum
    } else {
        check
    };
    IntegrityFinding {
        component: component.to_string(),
        partition: found.partition.clone(),
        check: check as i32,
        first_bad_sequence: found.sequence,
        first_bad_at_ns: found.committed_at_ns,
        // The newest restore point before it is the backups' and the
        // archive's to say; `meridian status` and `doctor` read them.
        restore_point_ns: 0,
        detail: found.what.clone(),
        found_at_ns,
    }
}

/// A record as a captured store's control change carries it.
pub fn to_json(record: &Record) -> Value {
    match record {
        Record::TookOver(r) => json!({
            "component": r.component,
            "member": r.member,
            "previous_member": r.previous_member,
            "partitions": r.partitions,
            "epoch": r.epoch,
            "reason": TakeoverReason::try_from(r.reason)
                .unwrap_or(TakeoverReason::Unspecified)
                .as_str_name(),
            "took_over_at_ns": r.took_over_at_ns,
        }),
        Record::Finding(r) => json!({
            "component": r.component,
            "partition": r.partition,
            "check": IntegrityCheck::try_from(r.check)
                .unwrap_or(IntegrityCheck::Unspecified)
                .as_str_name(),
            "first_bad_sequence": r.first_bad_sequence,
            "first_bad_at_ns": r.first_bad_at_ns,
            "restore_point_ns": r.restore_point_ns,
            "detail": r.detail,
            "found_at_ns": r.found_at_ns,
        }),
    }
}

/// A record from its JSON, as [`to_json`] wrote it; `None` for another kind.
pub fn from_json(kind: &str, value: &Value) -> Option<Record> {
    let text = |field: &str| value[field].as_str().unwrap_or_default().to_string();
    let int = |field: &str| value[field].as_i64().unwrap_or_default();
    let uint = |field: &str| value[field].as_u64().unwrap_or_default();
    match kind {
        TOOK_OVER => Some(Record::TookOver(WriterTookOver {
            component: text("component"),
            member: text("member"),
            previous_member: text("previous_member"),
            partitions: value["partitions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect(),
            epoch: uint("epoch"),
            reason: TakeoverReason::from_str_name(&text("reason"))
                .unwrap_or(TakeoverReason::Unspecified) as i32,
            took_over_at_ns: int("took_over_at_ns"),
        })),
        FINDING => Some(Record::Finding(IntegrityFinding {
            component: text("component"),
            partition: text("partition"),
            check: IntegrityCheck::from_str_name(&text("check"))
                .unwrap_or(IntegrityCheck::Unspecified) as i32,
            first_bad_sequence: uint("first_bad_sequence"),
            first_bad_at_ns: int("first_bad_at_ns"),
            restore_point_ns: int("restore_point_ns"),
            detail: text("detail"),
            found_at_ns: int("found_at_ns"),
        })),
        _ => None,
    }
}

/// A record as protobuf bytes, as the lake's control change carries it.
pub fn to_bytes(record: &Record) -> (&'static str, Vec<u8>) {
    match record {
        Record::TookOver(r) => (TOOK_OVER, r.encode_to_vec()),
        Record::Finding(r) => (FINDING, r.encode_to_vec()),
    }
}

/// The store records a control change holds: the lake's (the change's kind
/// the record's, its body the protobuf), or a captured store's (kind
/// `rows`, its body the transaction's rows, each `{"kind", "row"}`). Every
/// other change holds none.
pub fn of_change(change: &crate::Change) -> Vec<Record> {
    match change.kind.as_str() {
        TOOK_OVER => WriterTookOver::decode(change.body.as_slice())
            .map(|r| vec![Record::TookOver(r)])
            .unwrap_or_default(),
        FINDING => IntegrityFinding::decode(change.body.as_slice())
            .map(|r| vec![Record::Finding(r)])
            .unwrap_or_default(),
        "rows" => serde_json::from_slice::<Vec<Value>>(&change.body)
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| from_json(entry["kind"].as_str()?, &entry["row"]))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(why: &str) -> crate::TookOver {
        crate::TookOver {
            member: "om-lake-1".into(),
            previous: "om-lake-0".into(),
            why: why.into(),
            at_ns: 7,
        }
    }

    /// A takeover's record reads back the same from either body.
    #[test]
    fn a_takeover_reads_back_from_either_body() {
        let record = Record::TookOver(took_over(
            "street",
            &by(crate::LEASE_LAPSED),
            vec!["S0".into(), "control".into()],
            4,
        ));
        let (kind, body) = to_bytes(&record);
        let lake = crate::Change {
            kind: kind.into(),
            body,
            ..Default::default()
        };
        assert_eq!(of_change(&lake), vec![record.clone()]);
        let rows = json!([
            {"kind": "street_order.insert", "row": {"id": 1}},
            {"kind": TOOK_OVER, "row": to_json(&record)},
        ]);
        let captured = crate::Change {
            kind: "rows".into(),
            body: rows.to_string().into_bytes(),
            ..Default::default()
        };
        assert_eq!(of_change(&captured), vec![record]);
    }

    /// The words a takeover is recorded with name its reason.
    #[test]
    fn a_takeovers_words_name_its_reason() {
        assert_eq!(reason_of(crate::LEASE_LAPSED), TakeoverReason::LeaseLapsed);
        assert_eq!(reason_of(crate::LEASE_FREE), TakeoverReason::LeaseLapsed);
        assert_eq!(
            reason_of(crate::LEASE_RELEASED),
            TakeoverReason::SteppedDown
        );
        assert_eq!(
            reason_of(crate::timeline::DATABASE_TIMELINE),
            TakeoverReason::DatabaseTimeline
        );
    }

    /// A finding's check is read from where and what it was.
    #[test]
    fn a_findings_check_is_read_from_where_it_was() {
        let found = |partition: &str, what: &str| crate::integrity::Finding {
            partition: partition.into(),
            sequence: 3,
            committed_at_ns: 2,
            what: what.into(),
        };
        let check = |f| {
            IntegrityCheck::try_from(finding("lake", IntegrityCheck::RowDigest, &f, 9).check)
                .unwrap()
        };
        assert_eq!(
            check(found("replicas", "om-database-1 differs")),
            IntegrityCheck::MemberDigest
        );
        assert_eq!(
            check(found("", "a page of lake_row failed its checksum: x")),
            IntegrityCheck::PageChecksum
        );
        assert_eq!(
            check(found("ds", "a row does not match its digest")),
            IntegrityCheck::RowDigest
        );
        let record = Record::Finding(finding(
            "conductor",
            IntegrityCheck::RowDigest,
            &found("config", "a row does not match its digest"),
            9,
        ));
        assert_eq!(from_json(FINDING, &to_json(&record)), Some(record.clone()));
        assert_eq!(from_json("street_order.insert", &to_json(&record)), None);
    }
}
