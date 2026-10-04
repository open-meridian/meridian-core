//! A plugin files a ticket through its sidecar (W4.12), against an in-memory
//! bus with a stand-in dashboard that answers the two topics and keeps what
//! reached it, and a conductor holding the plugin's read scope.

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{Envelope, PluginConfiguration};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccessLevel, CallerAssertion, CallerClaims, FileTicketReply, FileTicketRequest, FiledTicket,
    ReadFiledTicketsReply, ReadFiledTicketsRequest, Refusal, RegisterRequest, TicketKind,
    TicketReference, TicketSubject,
};
use prost::Message;
use serde_json::Value;
use tonic::{Code, Request, Status};

use crate::front_door::{Verifier, HEADER};
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};
use crate::typed::{FILED_TICKETS, FILE_TICKET, REFUSAL_METADATA};

const KEY_ID: &str = "k-test";
const INSTANCE: &str = "operations-1";

/// What reached the stand-in dashboard: each filing, with its envelope.
type Heard = Arc<Mutex<Vec<(FileTicketRequest, Envelope)>>>;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n\
         platform.config.command.file-ticket\tcommand\tsidecar\tdashboard\n\
         platform.config.query.filed-tickets\tquery\tsidecar\tdashboard\n",
        "name\tkind\noperations\trole\nsidecar\tcomponent\nconductor\tcomponent\n\
         dashboard\tcomponent\n",
    )
    .unwrap()
}

/// A registered sidecar for `operations-1`, holding `roles`, whose read
/// scope is ACC-GROWTH and ACC-BETA; and a dashboard that answers a filing
/// `made` the first time a key is seen and `unchanged` after, counting.
async fn registered(roles: &[&str]) -> (Sidecar, Heard) {
    let bus = Arc::new(Bus::single(
        INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: INSTANCE.into(),
                read_account_ids: vec!["ACC-GROWTH".into(), "ACC-BETA".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let heard: Heard = Arc::default();
    let keeping = Arc::clone(&heard);
    bus.serve(FILE_TICKET, move |envelope| {
        let filing = FileTicketRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        let mut heard = keeping.lock().unwrap();
        let before = heard
            .iter()
            .filter(|(f, _)| f.idempotency_key == filing.idempotency_key)
            .count() as i64;
        heard.push((filing, envelope));
        Ok((
            "meridian.v1.FileTicketReply".into(),
            FileTicketReply {
                ticket_id: format!("TKT-{}", heard.len()),
                outcome: if before == 0 { "made" } else { "unchanged" }.into(),
                seen_count: before + 1,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(FILED_TICKETS, |envelope| {
        let asked =
            ReadFiledTicketsRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        let meta = envelope.meta.unwrap_or_default();
        Ok((
            "meridian.v1.ReadFiledTicketsReply".into(),
            ReadFiledTicketsReply {
                tickets: asked
                    .idempotency_keys
                    .iter()
                    .map(|key| FiledTicket {
                        ticket_id: format!("TKT-for-{}", meta.acting_for_subject),
                        idempotency_key: key.clone(),
                        state: 1,
                        seen_count: 1,
                        ..Default::default()
                    })
                    .collect(),
                next_cursor: String::new(),
            }
            .encode_to_vec(),
        ))
    });
    let roles = roles.iter().map(|r| r.to_string()).collect();
    let sidecar =
        Sidecar::under(&contract(), bus, "DEP-test", Identity::new(INSTANCE, roles)).with_verifier(
            Arc::new(Verifier::holding(INSTANCE, KEY_ID, key().verifying_key())),
        );
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v13".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, heard)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// The header a page at `level` hands the plugin for Ben, reading `read`.
fn header_for(level: AccessLevel, read: &[&str], audience: &str, delegation: &str) -> String {
    let issued = now();
    let claims = CallerClaims {
        subject: "local|ben".into(),
        display_name: "Ben Ito".into(),
        audience_instance_id: audience.into(),
        read_account_ids: read.iter().map(|a| a.to_string()).collect(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-1".into(),
        level: level as i32,
        delegation_id: delegation.into(),
        client_name: if delegation.is_empty() {
            String::new()
        } else {
            "Ben's agent".into()
        },
        ..CallerClaims::default()
    }
    .encode_to_vec();
    URL_SAFE_NO_PAD.encode(
        CallerAssertion {
            signature: key().sign(&claims).to_bytes().to_vec(),
            claims,
            key_id: KEY_ID.into(),
        }
        .encode_to_vec(),
    )
}

fn ben_viewing() -> String {
    header_for(AccessLevel::Read, &["ACC-GROWTH"], INSTANCE, "")
}

/// The fixture's filing (fixtures/sidecar/file-ticket-for-person.yaml).
fn filing() -> FileTicketRequest {
    FileTicketRequest {
        title: "Break on the growth account still open after its cause was confirmed".into(),
        seen: "I confirmed the cause of the break on ACC-GROWTH on 3 October. The \
               reconciliation page still lists it as open this morning."
            .into(),
        kind: TicketKind::Defect as i32,
        concerns: Some(TicketSubject {
            kind: "plugin".into(),
            ..Default::default()
        }),
        step: "W9.7".into(),
        operation: "ResolveBreak".into(),
        references: vec![TicketReference {
            kind: "break".into(),
            value: "BRK-01J8XQ7B000000000000000001".into(),
            account_id: "ACC-GROWTH".into(),
        }],
        idempotency_key: "break-still-open-BRK-01J8XQ7B000000000000000001".into(),
        ..Default::default()
    }
}

fn carrying(header: Option<&str>, filing: FileTicketRequest) -> Request<FileTicketRequest> {
    let mut request = Request::new(filing);
    if let Some(header) = header {
        request
            .metadata_mut()
            .insert(HEADER, header.parse().unwrap());
    }
    request
}

async fn file(
    sidecar: &Sidecar,
    header: Option<&str>,
    filing: FileTicketRequest,
) -> Result<FileTicketReply, Status> {
    sidecar
        .file_ticket(carrying(header, filing))
        .await
        .map(|reply| reply.into_inner())
}

/// The paths a refusal names in its metadata.
fn paths(refused: &Status) -> Vec<String> {
    refused
        .metadata()
        .get_bin(REFUSAL_METADATA)
        .and_then(|value| value.to_bytes().ok())
        .and_then(|bytes| Refusal::decode(bytes.as_ref()).ok())
        .map(|refusal| refusal.fields)
        .unwrap_or_default()
}

#[tokio::test]
async fn a_plugin_holding_no_role_files_for_a_person_viewing_its_page_and_is_stamped_with_them() {
    let (sidecar, heard) = registered(&[]).await;
    let mut sent = filing();
    // Whatever the plugin says of its version, the deployment sets it.
    sent.concerns.as_mut().unwrap().version = "9.9.9".into();
    let reply = file(&sidecar, Some(&ben_viewing()), sent).await.unwrap();
    assert_eq!(reply.outcome, "made");
    assert_eq!(reply.seen_count, 1);

    let heard = heard.lock().unwrap();
    let (filed, envelope) = &heard[0];
    let concerns = filed.concerns.as_ref().unwrap();
    assert_eq!(concerns.kind, "plugin");
    assert_eq!(concerns.instance, INSTANCE, "set by the sidecar");
    assert_eq!(
        concerns.version, "",
        "the deployment's to set, never the plugin's"
    );
    let meta = envelope.meta.as_ref().unwrap();
    assert_eq!(meta.acting_for_subject, "local|ben");
    assert_eq!(meta.acting_through_delegation, "");
    assert_eq!(meta.publisher_instance_id, INSTANCE);
    assert!(!meta.account_scope_applies);
}

#[tokio::test]
async fn a_person_through_a_delegation_is_stamped_with_it_and_its_client() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let header = header_for(AccessLevel::Write, &["ACC-GROWTH"], INSTANCE, "DLG-1");
    file(&sidecar, Some(&header), filing()).await.unwrap();
    let heard = heard.lock().unwrap();
    let meta = heard[0].1.meta.as_ref().unwrap();
    assert_eq!(meta.acting_through_delegation, "DLG-1");
    assert_eq!(meta.acting_through_client, "Ben's agent");
}

#[tokio::test]
async fn a_plugin_filing_as_itself_is_refused_and_nothing_reaches_the_dashboard() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let refused = file(&sidecar, None, filing()).await.unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "a plugin files a ticket only for a person it acts for"
    );
    assert!(heard.lock().unwrap().is_empty());

    let read = sidecar
        .filed_tickets(Request::new(ReadFiledTicketsRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(read.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn an_assertion_for_another_instance_or_not_the_dashboards_is_unauthenticated() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let theirs = header_for(AccessLevel::Read, &["ACC-GROWTH"], "custody-1", "");
    for header in [theirs.as_str(), "not-an-assertion"] {
        let refused = file(&sidecar, Some(header), filing()).await.unwrap_err();
        assert_eq!(refused.code(), Code::Unauthenticated, "{header}");
    }
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_ticket_about_another_plugin_is_refused_naming_it() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let mut about = filing();
    about.concerns = Some(TicketSubject {
        kind: "plugin".into(),
        instance: "custody-1".into(),
        version: String::new(),
    });
    let refused = file(&sidecar, Some(&ben_viewing()), about)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "concerns.instance: custody-1 is another plugin; a plugin files about itself, a part \
         of core or the platform"
    );
    assert_eq!(paths(&refused), ["concerns.instance"]);
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_ticket_about_a_part_of_core_or_the_platform_is_filed_as_it_says() {
    let (sidecar, heard) = registered(&["operations"]).await;
    for kind in ["bor", "platform"] {
        let mut about = filing();
        about.idempotency_key = format!("about-{kind}");
        about.concerns = Some(TicketSubject {
            kind: kind.into(),
            ..Default::default()
        });
        file(&sidecar, Some(&ben_viewing()), about).await.unwrap();
    }
    let heard = heard.lock().unwrap();
    assert_eq!(heard[1].0.concerns.as_ref().unwrap().kind, "platform");
    assert_eq!(heard[1].0.concerns.as_ref().unwrap().instance, "");
}

#[tokio::test]
async fn an_account_outside_the_scope_or_the_persons_read_set_is_refused_by_path() {
    let (sidecar, heard) = registered(&["operations"]).await;
    for (account, words) in [
        ("ACC-INCOME", "is not in this plugin's read scope"),
        ("ACC-BETA", "may not read ACC-BETA through this plugin"),
    ] {
        let mut naming = filing();
        naming.references = vec![
            TicketReference {
                kind: "instrument".into(),
                value: "LCL-1".into(),
                account_id: String::new(),
            },
            TicketReference {
                kind: "account".into(),
                value: account.into(),
                account_id: String::new(),
            },
        ];
        let refused = file(&sidecar, Some(&ben_viewing()), naming)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied, "{account}");
        assert_eq!(paths(&refused), ["references[1].account_id"]);
        assert!(refused.message().contains(words), "{}", refused.message());
    }
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_break_without_its_account_is_refused_by_path() {
    let (sidecar, _) = registered(&["operations"]).await;
    let mut unplaced = filing();
    unplaced.references[0].account_id.clear();
    let refused = file(&sidecar, Some(&ben_viewing()), unplaced)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(paths(&refused), ["references[0].account_id"]);
}

#[tokio::test]
async fn bounds_and_a_key_are_held_before_anything_leaves() {
    let (sidecar, heard) = registered(&["operations"]).await;
    let mut long = filing();
    long.title = "x".repeat(121);
    let mut empty_title = filing();
    empty_title.title.clear();
    let mut long_seen = filing();
    long_seen.seen = "y".repeat(8001);
    let mut many = filing();
    many.references = (0..51)
        .map(|n| TicketReference {
            kind: "instrument".into(),
            value: format!("LCL-{n}"),
            account_id: String::new(),
        })
        .collect();
    let mut keyless = filing();
    keyless.idempotency_key.clear();
    let mut kindless = filing();
    kindless.kind = 0;
    let mut subjectless = filing();
    subjectless.concerns = None;
    for (sent, path) in [
        (long, "title"),
        (empty_title, "title"),
        (long_seen, "seen"),
        (many, "references"),
        (keyless, "idempotency_key"),
        (kindless, "kind"),
        (subjectless, "concerns"),
    ] {
        let refused = file(&sidecar, Some(&ben_viewing()), sent)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::InvalidArgument, "{path}");
        assert_eq!(paths(&refused), [path]);
    }
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_twenty_first_filing_in_an_hour_is_refused_and_a_repeat_is_not_counted() {
    let (sidecar, heard) = registered(&["operations"]).await;
    // One key, filed 21 times: folded into one ticket, never counted past one.
    for _ in 0..21 {
        file(&sidecar, Some(&ben_viewing()), filing())
            .await
            .unwrap();
    }
    // Nineteen more keys reach the twenty.
    for n in 1..20 {
        let mut other = filing();
        other.idempotency_key = format!("key-{n}");
        assert_eq!(
            file(&sidecar, Some(&ben_viewing()), other)
                .await
                .unwrap()
                .outcome,
            "made"
        );
    }
    let mut twenty_first = filing();
    twenty_first.idempotency_key = "key-21".into();
    let refused = file(&sidecar, Some(&ben_viewing()), twenty_first)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::ResourceExhausted);
    // A repeat under a key it filed is still folded in.
    let repeat = file(&sidecar, Some(&ben_viewing()), filing())
        .await
        .unwrap();
    assert_eq!(repeat.outcome, "unchanged");
    assert_eq!(heard.lock().unwrap().len(), 21 + 19 + 1);
}

#[tokio::test]
async fn a_plugin_reads_back_what_it_filed_for_a_person() {
    let (sidecar, _) = registered(&[]).await;
    let mut request = Request::new(ReadFiledTicketsRequest {
        idempotency_keys: vec!["break-still-open".into()],
        ..Default::default()
    });
    request
        .metadata_mut()
        .insert(HEADER, ben_viewing().parse().unwrap());
    let reply = sidecar.filed_tickets(request).await.unwrap().into_inner();
    assert_eq!(reply.tickets.len(), 1);
    assert_eq!(reply.tickets[0].ticket_id, "TKT-for-local|ben", "stamped");
}

#[tokio::test]
async fn only_the_two_ticket_topics_are_sent_for_a_person_below_admin() {
    // W4.9's notes: the exception names its two topics, and every other
    // `config` command and query stays an admin's.
    let (sidecar, _) = registered(&["operations"]).await;
    let viewing = URL_SAFE_NO_PAD.decode(ben_viewing()).unwrap();
    let viewing = CallerAssertion::decode(viewing.as_slice()).unwrap();
    let topics = include_str!("../../../../deploy/topics.tsv");
    let mut checked = 0;
    for topic in topics
        .lines()
        .filter_map(|line| line.split('\t').next())
        .filter(|topic| {
            topic.starts_with("platform.config.command.")
                || topic.starts_with("platform.config.query.")
        })
    {
        let vouched = sidecar.vouched_for_configuration(topic, Some(&viewing), now());
        if topic == FILE_TICKET || topic == FILED_TICKETS {
            assert!(vouched.is_ok(), "{topic}");
        } else {
            assert_eq!(
                vouched.unwrap_err().code(),
                Code::PermissionDenied,
                "{topic}"
            );
            checked += 1;
        }
        assert_eq!(
            sidecar
                .vouched_for_configuration(topic, None, now())
                .unwrap_err()
                .code(),
            Code::PermissionDenied,
            "{topic} as the plugin itself"
        );
    }
    assert!(checked > 10, "only {checked} config topics were read");
}

/// Every case of the red-team corpus a plugin's filing can carry, through
/// the sidecar: a bound or a character refused here, naming the field, and
/// nothing filed; a text that only reads as instructions passed on, for the
/// dashboard to hold as suspect (W6.21).
#[tokio::test]
async fn every_corpus_case_a_plugin_can_file_is_refused_here_or_passed_on_as_it_pins() {
    let corpus: Value =
        serde_json::from_str(include_str!("../../../../deploy/prompt-attacks.json")).unwrap();
    let mut replayed = 0;
    for case in corpus["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let channel = case["channel"].as_str().unwrap();
        let expect = &case["expect"]["deployment"];
        let planted = case["planted"].as_str().unwrap_or_default();
        let (sidecar, heard) = registered(&["operations"]).await;
        let mut sent = filing();
        sent.idempotency_key = format!("case-{id}");
        let header = ben_viewing();
        let mut header = Some(header.as_str());
        match channel {
            "ticket-title" => sent.title = planted.into(),
            "ticket-seen" => sent.seen = planted.into(),
            "plugin-filing" => match expect["rule"].as_str() {
                Some("filer") => header = None,
                Some("concerns") => {
                    sent.seen = planted.into();
                    sent.concerns.as_mut().unwrap().instance = "custody-1".into();
                }
                Some("rate") => continue, // the rate's own test, above
                _ => sent.seen = planted.into(),
            },
            "ticket-reference" => {
                sent.references = vec![TicketReference {
                    kind: "account".into(),
                    value: planted.into(),
                    account_id: planted.into(),
                }]
            }
            // A note, or a call's arguments, never reach a sidecar.
            _ => continue,
        }
        replayed += 1;
        let filed = file(&sidecar, header, sent).await;
        match expect["outcome"].as_str().unwrap() {
            "refused" => {
                let refused = filed.expect_err(id);
                if let Some(field) = expect["field"].as_str() {
                    assert_eq!(paths(&refused), [field], "{id}");
                }
                assert!(
                    heard.lock().unwrap().is_empty(),
                    "{id} reached the dashboard"
                );
            }
            "suspect" => {
                filed.unwrap_or_else(|refused| panic!("{id} was refused: {refused}"));
                assert_eq!(heard.lock().unwrap().len(), 1, "{id}");
            }
            other => panic!("{id}: an outcome the corpus does not define: {other}"),
        }
    }
    assert!(replayed >= 10, "only {replayed} cases were replayed");
}
