//! Contract v22's admission at the sidecar (W4.9, W4.11, W11.20 to W11.22),
//! against the contract compiled into this runtime: a service account's
//! algorithm checked and stamped; a command naming its record by ID alone
//! stamped with the write scope it is held to; a venue fact's account from
//! the venue account map; the venue's catalogue read as the plugin itself;
//! a code mapped twice refused before the conductor.

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    OrderReply, PluginConfiguration, ReadVenueCatalogueReply, VenueCatalogue,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    Decimal, MapVenueAccountParams, ProposeOrderParams, ReadVenueCatalogueParams,
    RecordOrderParams, ReleaseOrderParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccessLevel, AlgorithmDeclaration, CallerAssertion, CallerClaims, MessageMeta,
    PluginDeclaration, RegisterRequest, RoleAccess, SubjectKind, VenueAccountMap,
    VenueAccountMapping,
};
use prost::Message;
use tonic::{Code, Request};

use crate::front_door::Verifier;
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};

const KEY_ID: &str = "dashboard-2026-10-0a1b2c3d";
const PROPOSE_ORDER: &str = "platform.book.command.propose-order";
const RECORD_ORDER: &str = "platform.book.command.record-order";
const RELEASE_ORDER: &str = "platform.book.command.release-order";
const CREATE_PLACEMENT: &str = "platform.book.command.create-placement";

fn now() -> i64 {
    use meridian_clock::Clock as _;
    meridian_clock::SystemClock.now_ns()
}

/// What the stand-ins heard: each command's envelope stamp and its body.
type Heard = Arc<Mutex<Vec<(MessageMeta, Vec<u8>)>>>;

struct Plugin {
    sidecar: Sidecar,
    key: SigningKey,
    heard: Heard,
    bus: Arc<Bus>,
}

/// A registered sidecar for `instance` holding `roles`, declaring the
/// algorithm `drift`, its conductor saying ACC-1 and ACC-3 are its write
/// scope and holding `map`, and a book that answers each order command.
async fn plugin(instance: &str, roles: &[&str], map: Option<VenueAccountMap>) -> Plugin {
    let bus = Arc::new(Bus::single(
        instance,
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let heard: Heard = Arc::new(Mutex::new(Vec::new()));
    for topic in [PROPOSE_ORDER, RECORD_ORDER, RELEASE_ORDER, CREATE_PLACEMENT] {
        let keeping = Arc::clone(&heard);
        bus.serve(topic, move |envelope| {
            keeping
                .lock()
                .unwrap()
                .push((envelope.meta.clone().unwrap_or_default(), envelope.payload));
            Ok((
                "meridian.v1.OrderReply".into(),
                OrderReply::default().encode_to_vec(),
            ))
        });
    }
    let keeping = Arc::clone(&heard);
    bus.serve("platform.config.query.venue-catalogue", move |envelope| {
        keeping
            .lock()
            .unwrap()
            .push((envelope.meta.clone().unwrap_or_default(), envelope.payload));
        Ok((
            "meridian.v1.ReadVenueCatalogueReply".into(),
            ReadVenueCatalogueReply {
                catalogue: Some(VenueCatalogue {
                    connectivity_instance_id: "fix-1".into(),
                    venue_account_code: "ACCT-7".into(),
                    ..Default::default()
                }),
            }
            .encode_to_vec(),
        ))
    });
    let instance_id = instance.to_string();
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: instance_id.clone(),
                read_account_ids: vec!["ACC-1".into(), "ACC-3".into()],
                write_account_ids: vec!["ACC-1".into(), "ACC-3".into()],
                venue_account_map: map.clone(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(instance, KEY_ID, key.verifying_key()));
    let sidecar = Sidecar::under(
        Contract::embedded(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new(instance, roles.iter().map(|r| r.to_string()).collect()),
    )
    .with_verifier(verifier);
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v22".into(),
            declaration: Some(PluginDeclaration {
                algorithms: vec![AlgorithmDeclaration {
                    algorithm_id: "drift".into(),
                    description: "Rebalances a sleeve past its drift band".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    Plugin {
        sidecar,
        key,
        heard,
        bus,
    }
}

/// The dashboard's assertion for `subject` on `instance`: write on `role`
/// over `writes` (or admin, with none), a service account's where `owner`
/// names its owner, carrying `algorithm`.
fn vouched(
    key: &SigningKey,
    instance: &str,
    role: &str,
    level: AccessLevel,
    writes: &[&str],
    owner: Option<&str>,
    algorithm: &str,
) -> CallerAssertion {
    routed(key, instance, role, level, writes, owner, algorithm, &[])
}

/// [`vouched`], the claims naming the connectivity plugins the caller may
/// route through from `instance` (contract v22).
#[allow(clippy::too_many_arguments)]
fn routed(
    key: &SigningKey,
    instance: &str,
    role: &str,
    level: AccessLevel,
    writes: &[&str],
    owner: Option<&str>,
    algorithm: &str,
    routes: &[&str],
) -> CallerAssertion {
    let issued = now();
    let reads: Vec<String> = writes.iter().map(|w| w.to_string()).collect();
    let claims = CallerClaims {
        subject: if owner.is_some() { "SA-7" } else { "local|ada" }.into(),
        display_name: "Ada".into(),
        audience_instance_id: instance.into(),
        read_account_ids: reads.clone(),
        write_account_ids: reads.clone(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: format!("a-{issued}"),
        level: level as i32,
        roles: vec![RoleAccess {
            role: role.into(),
            level: level as i32,
            read_positions: (0..reads.len() as u32).collect(),
            write_positions: if level == AccessLevel::Write {
                (0..reads.len() as u32).collect()
            } else {
                Vec::new()
            },
        }],
        subject_kind: if owner.is_some() {
            SubjectKind::ServiceAccount as i32
        } else {
            SubjectKind::Unspecified as i32
        },
        owner_subject: owner.unwrap_or_default().into(),
        algorithm_id: algorithm.into(),
        connectivity_instance_ids: routes.iter().map(|r| r.to_string()).collect(),
        ..CallerClaims::default()
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: KEY_ID.into(),
    }
}

fn one() -> Option<Decimal> {
    Some(Decimal {
        high: 0,
        low: 1,
        scale: 0,
    })
}

fn proposal(acting_for: CallerAssertion) -> ProposeOrderParams {
    ProposeOrderParams {
        account_id: "ACC-1".into(),
        instrument_id: "INS-1".into(),
        quantity: one(),
        acting_for: Some(acting_for),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_service_accounts_algorithm_is_one_its_version_declares_and_is_stamped() {
    let p = plugin("pm-1", &["portfolio"], None).await;
    let as_account = |algorithm: &str| {
        vouched(
            &p.key,
            "pm-1",
            "portfolio",
            AccessLevel::Write,
            &["ACC-1"],
            Some("local|ada"),
            algorithm,
        )
    };
    p.sidecar
        .propose_order(Request::new(proposal(as_account("drift"))))
        .await
        .expect("a declared algorithm is admitted");
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert_eq!(meta.algorithm_id, "drift");
    assert_eq!(meta.acting_for_kind, SubjectKind::ServiceAccount as i32);
    assert_eq!(meta.owner_subject, "local|ada");

    let refused = p
        .sidecar
        .propose_order(Request::new(proposal(as_account("momentum"))))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(refused.message().contains("algorithm_id"), "{refused:?}");
    assert!(
        refused.message().contains("drift"),
        "it names what is declared"
    );

    // A person's act carries none, whatever the claims say.
    let person = vouched(
        &p.key,
        "pm-1",
        "portfolio",
        AccessLevel::Write,
        &["ACC-1"],
        None,
        "momentum",
    );
    p.sidecar
        .propose_order(Request::new(proposal(person)))
        .await
        .unwrap();
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert!(meta.algorithm_id.is_empty());
}

#[tokio::test]
async fn a_call_as_a_service_account_names_only_a_declared_algorithm() {
    let p = plugin("pm-1", &["portfolio"], None).await;
    let refused = p
        .sidecar
        .call_act_as_service_account(Request::new(meridian_pb::v1::CallAsServiceAccountRequest {
            role: "portfolio".into(),
            tool_name: "rebalance".into(),
            arguments_json: "{}".into(),
            reason: "the sleeve drifted".into(),
            algorithm_id: "momentum".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(refused.message().starts_with("algorithm_id"), "{refused:?}");
}

#[tokio::test]
async fn a_command_naming_its_record_by_id_alone_carries_the_write_scope_it_is_held_to() {
    let p = plugin("oms-1", &["oms"], None).await;
    // As itself: the instance's write scope, marked as applying.
    p.sidecar
        .release_order(Request::new(ReleaseOrderParams {
            order_id: "ORD-1".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert!(meta.account_scope_applies);
    assert_eq!(meta.account_scope, vec!["ACC-1", "ACC-3"]);

    // For a person, narrowed to what they may write through it.
    let ada = vouched(
        &p.key,
        "oms-1",
        "oms",
        AccessLevel::Write,
        &["ACC-3", "ACC-9"],
        None,
        "",
    );
    p.sidecar
        .release_order(Request::new(ReleaseOrderParams {
            order_id: "ORD-1".into(),
            acting_for: Some(ada),
            ..Default::default()
        }))
        .await
        .unwrap();
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert!(meta.account_scope_applies);
    assert_eq!(meta.account_scope, vec!["ACC-3"]);
    assert_eq!(meta.acting_for_subject, "local|ada");

    // A command naming its account carries no scope: its account is checked
    // here.
    p.sidecar
        .propose_order(Request::new(ProposeOrderParams {
            account_id: "ACC-1".into(),
            quantity: one(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert!(!meta.account_scope_applies);
}

#[test]
fn every_book_command_the_operations_send_with_no_account_is_one_named_by_id_alone() {
    // The generated half hands `account = None` to these; the list here is
    // what stamps their scope. A new one generated and not listed would go
    // unscoped.
    let generated = include_str!("../../operations.rs");
    let mut unlisted = Vec::new();
    for block in generated.split("async fn ").skip(1) {
        let Some(at) = block.find("self.command_typed(\"platform.book.command.") else {
            continue;
        };
        if !block.contains("let account = None;") {
            continue;
        }
        let call = &block[at..];
        let payload = call.split('"').nth(3).unwrap_or_default();
        if !crate::typed::BY_ID_ALONE.contains(&payload) {
            unlisted.push(payload.to_string());
        }
    }
    assert!(unlisted.is_empty(), "not listed: {unlisted:?}");
}

#[tokio::test]
async fn a_connectivity_plugins_venue_fact_takes_the_account_its_ems_mapped_or_none() {
    let map = VenueAccountMap {
        ems_instance_id: "ems-1".into(),
        mappings: vec![
            VenueAccountMapping {
                connectivity_instance_id: "fix-1".into(),
                venue_account_code: "ACCT-7".into(),
                account_id: "ACC-1".into(),
            },
            // Another plugin's code of the same spelling is not this one's.
            VenueAccountMapping {
                connectivity_instance_id: "fix-2".into(),
                venue_account_code: "ACCT-8".into(),
                account_id: "ACC-3".into(),
            },
        ],
    };
    let p = plugin("fix-1", &["connectivity"], Some(map)).await;
    let order = |code: &str| RecordOrderParams {
        venue_account_code: code.into(),
        // Whatever the plugin names, the map decides.
        account_id: "ACC-3".into(),
        quantity: one(),
        venue_order_id: "V-1".into(),
        ..Default::default()
    };
    p.sidecar
        .record_order(Request::new(order("ACCT-7")))
        .await
        .unwrap();
    let (_, body) = p.heard.lock().unwrap().last().cloned().unwrap();
    let sent = meridian_domain::v1::RecordOrderRequest::decode(&body[..]).unwrap();
    assert_eq!(sent.account_id, "ACC-1");

    // A code nobody mapped on this plugin is sent with no account, never
    // refused: the book keeps the fact unplaced.
    p.sidecar
        .record_order(Request::new(order("ACCT-8")))
        .await
        .unwrap();
    let (_, body) = p.heard.lock().unwrap().last().cloned().unwrap();
    let sent = meridian_domain::v1::RecordOrderRequest::decode(&body[..]).unwrap();
    assert!(sent.account_id.is_empty());
}

#[tokio::test]
async fn the_venue_catalogue_is_read_as_the_plugin_itself() {
    let p = plugin("ems-1", &["ems"], None).await;
    let reply = p
        .sidecar
        .read_venue_catalogue(Request::new(ReadVenueCatalogueParams {
            connectivity_instance_id: "fix-1".into(),
            venue_account_code: "ACCT-7".into(),
            ..Default::default()
        }))
        .await
        .expect("an ems reads the catalogue as itself")
        .into_inner();
    assert_eq!(reply.catalogue.unwrap().venue_account_code, "ACCT-7");
    let (meta, _) = p.heard.lock().unwrap().last().cloned().unwrap();
    assert!(meta.acting_for_subject.is_empty(), "sent for nobody");
}

#[tokio::test]
async fn a_code_its_map_holds_for_another_account_is_refused_before_the_conductor() {
    let map = VenueAccountMap {
        ems_instance_id: "ems-1".into(),
        mappings: vec![VenueAccountMapping {
            connectivity_instance_id: "fix-1".into(),
            venue_account_code: "ACCT-7".into(),
            account_id: "ACC-1".into(),
        }],
    };
    let p = plugin("ems-1", &["ems"], Some(map)).await;
    let asked = p.bus.subscribe("platform.config.command.map-venue-account");
    let admin = vouched(&p.key, "ems-1", "ems", AccessLevel::Admin, &[], None, "");
    let refused = p
        .sidecar
        .map_venue_account(Request::new(MapVenueAccountParams {
            connectivity_instance_id: "fix-1".into(),
            venue_account_code: "ACCT-7".into(),
            account_id: "ACC-3".into(),
            acting_for: Some(admin),
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::AlreadyExists);
    assert!(refused.message().starts_with("venue_account_code"));
    drop(asked);
}

#[test]
fn a_placement_reaches_a_connectivity_plugin_only_at_the_instance_it_names() {
    // matrix/directed.tsv (contract v22): directed to the connectivity role
    // alone; every other role hearing the row hears it by its scope.
    const PLACEMENT: &str = "platform.book.event.placement-updated";
    let directed = vec![("connectivity", "fix-1".to_string())];
    let here = |roles: &[&str], instance: &str| {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        crate::receive::directed_here(&directed, PLACEMENT, &roles, instance)
    };
    assert!(here(&["connectivity"], "fix-1"));
    assert!(!here(&["connectivity"], "fix-2"));
    assert!(here(&["connectivity", "ems"], "fix-2"), "heard as an ems");
    assert!(here(&["oms"], "oms-1"));
    // Nothing directed is heard as before.
    assert!(crate::receive::directed_here(
        &[],
        PLACEMENT,
        &["connectivity".to_string()],
        "fix-2"
    ));
}

#[tokio::test]
async fn a_plugin_holding_custody_and_connectivity_takes_the_map_never_its_links() {
    // Q42: custody does not send a venue's order, so only the map stamps it,
    // whatever account the plugin names and whether or not it names a code.
    let map = VenueAccountMap {
        ems_instance_id: "ems-1".into(),
        mappings: vec![VenueAccountMapping {
            connectivity_instance_id: "snap-1".into(),
            venue_account_code: "SNAP-A".into(),
            account_id: "ACC-1".into(),
        }],
    };
    let p = plugin("snap-1", &["custody", "connectivity"], Some(map)).await;
    for (code, expected) in [("SNAP-A", "ACC-1"), ("", "")] {
        p.sidecar
            .record_order(Request::new(RecordOrderParams {
                venue_account_code: code.into(),
                account_id: "ACC-3".into(),
                quantity: one(),
                venue_order_id: "V-1".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        let (_, body) = p.heard.lock().unwrap().last().cloned().unwrap();
        let sent = meridian_domain::v1::RecordOrderRequest::decode(&body[..]).unwrap();
        assert_eq!(sent.account_id, expected, "code {code:?}");
    }
}

#[tokio::test]
async fn a_placement_says_whether_its_connectivity_plugin_is_one_the_caller_may_route_through() {
    use meridian_pb::plugin::v1::CreatePlacementParams;
    // An ems's placement: allowed where the claims name the plugin, not
    // where they do not, and never for the plugin as itself.
    let p = plugin("ems-1", &["ems"], None).await;
    let place = |connectivity: &str, acting_for: Option<CallerAssertion>| CreatePlacementParams {
        order_id: "ORD-1".into(),
        connectivity_instance_id: connectivity.into(),
        quantity: one(),
        acting_for,
        ..Default::default()
    };
    let allowed = |p: &Plugin| {
        p.heard
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .0
            .connectivity_allowed
    };
    let ada = |routes: &[&str]| {
        routed(
            &p.key,
            "ems-1",
            "ems",
            AccessLevel::Write,
            &["ACC-1"],
            None,
            "",
            routes,
        )
    };
    p.sidecar
        .create_placement(Request::new(place("fix-1", Some(ada(&["fix-1", "fix-3"])))))
        .await
        .unwrap();
    assert!(allowed(&p));
    p.sidecar
        .create_placement(Request::new(place("fix-2", Some(ada(&["fix-1"])))))
        .await
        .unwrap();
    assert!(!allowed(&p), "a plugin the claims do not name");
    p.sidecar
        .create_placement(Request::new(place("fix-1", None)))
        .await
        .unwrap();
    assert!(
        !allowed(&p),
        "the plugin as itself names nobody who may route"
    );

    // A connectivity plugin's own direct order: allowed where the claims
    // name its own instance.
    let c = plugin("fix-1", &["connectivity"], None).await;
    let own = |routes: &[&str]| {
        routed(
            &c.key,
            "fix-1",
            "connectivity",
            AccessLevel::Write,
            &["ACC-1"],
            None,
            "",
            routes,
        )
    };
    c.sidecar
        .create_placement(Request::new(place("fix-1", Some(own(&["fix-1"])))))
        .await
        .unwrap();
    assert!(allowed(&c));
    c.sidecar
        .create_placement(Request::new(place("fix-1", Some(own(&[])))))
        .await
        .unwrap();
    assert!(!allowed(&c), "withdrawn, or not among its users");
}

// ── The order path's rules the sidecar holds (contract v22) ─────────────────

const REQUEST_ORDER_CHANGE: &str = "platform.book.command.request-order-change";
const LIST_ORDERS: &str = "platform.book.query.list-orders";

fn quantity_change() -> meridian_pb::plugin::v1::RequestOrderChangeParams {
    meridian_pb::plugin::v1::RequestOrderChangeParams {
        order_id: "ORD-1".into(),
        change: Some(meridian_pb::plugin::v1::OrderChange {
            kind: meridian_domain::v1::OrderChangeKind::Quantity as i32,
            quantity: Some(Decimal {
                high: 0,
                low: 12,
                scale: 0,
            }),
            ..Default::default()
        }),
        reason: "more".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_ems_changes_no_ordered_quantity_and_the_book_hears_nothing_of_it() {
    let desk = plugin("desk-1", &["ems"], None).await;
    let heard = Arc::clone(&desk.heard);
    let keeping = Arc::clone(&heard);
    desk.bus.serve(REQUEST_ORDER_CHANGE, move |envelope| {
        keeping
            .lock()
            .unwrap()
            .push((envelope.meta.clone().unwrap_or_default(), envelope.payload));
        Ok((
            "meridian.v1.OrderReply".into(),
            OrderReply::default().encode_to_vec(),
        ))
    });
    let refused = desk
        .sidecar
        .request_order_change(Request::new(quantity_change()))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Aborted);
    let refusal = meridian_pb::v1::Refusal::decode(
        refused
            .metadata()
            .get_bin(crate::typed::REFUSAL_METADATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    assert_eq!(
        refusal.reason,
        meridian_pb::v1::RefusalReason::OrderState as i32
    );
    assert!(heard.lock().unwrap().is_empty());

    // An oms's goes to the book, which reads whether it is an alignment.
    let oms = plugin("oms-1", &["oms"], None).await;
    let heard = Arc::clone(&oms.heard);
    let keeping = Arc::clone(&heard);
    oms.bus.serve(REQUEST_ORDER_CHANGE, move |envelope| {
        keeping
            .lock()
            .unwrap()
            .push((envelope.meta.clone().unwrap_or_default(), envelope.payload));
        Ok((
            "meridian.v1.OrderReply".into(),
            OrderReply::default().encode_to_vec(),
        ))
    });
    oms.sidecar
        .request_order_change(Request::new(quantity_change()))
        .await
        .unwrap();
    assert_eq!(heard.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_book_refusal_with_a_status_reaches_the_plugin_as_that_status_naming_its_fields() {
    let desk = plugin("pm-1", &["portfolio"], None).await;
    let scoped = plugin("oms-9", &["oms"], None).await;
    // The plugins' book stand-ins answer before the ones `plugin` serves.
    desk.bus.serve(PROPOSE_ORDER, |_| {
        Err(meridian_bus::refusal_with_status(
            meridian_bus::RefusalStatus::InvalidArgument,
            &["quantity".into(), "cash_quantity".into()],
            "an order states exactly one",
        ))
    });
    scoped.bus.serve(RELEASE_ORDER, |_| {
        Err(meridian_bus::refusal_with_status(
            meridian_bus::RefusalStatus::PermissionDenied,
            &[],
            "outside the write scope",
        ))
    });
    let refused = desk
        .sidecar
        .propose_order(Request::new(ProposeOrderParams {
            account_id: "ACC-1".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    let refusal = meridian_pb::v1::Refusal::decode(
        refused
            .metadata()
            .get_bin(crate::typed::REFUSAL_METADATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    assert_eq!(refusal.fields, vec!["quantity", "cash_quantity"]);
    assert_eq!(refusal.reason, 0);
    let denied = scoped
        .sidecar
        .release_order(Request::new(ReleaseOrderParams {
            order_id: "ORD-1".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn a_connectivity_plugin_reads_only_the_orders_behind_its_own_placements() {
    let venue = plugin("fix-1", &["connectivity"], None).await;
    let placement = |id: &str, instance: &str| meridian_domain::v1::Placement {
        placement_id: id.into(),
        connectivity_instance_id: instance.into(),
        ..Default::default()
    };
    let reply = meridian_domain::v1::ListOrdersReply {
        orders: vec![
            meridian_domain::v1::Order {
                order_id: "ORD-mine".into(),
                placements: vec![
                    placement("PLC-mine", "fix-1"),
                    placement("PLC-theirs", "fix-2"),
                ],
                ..Default::default()
            },
            meridian_domain::v1::Order {
                order_id: "ORD-theirs".into(),
                placements: vec![placement("PLC-other", "fix-2")],
                ..Default::default()
            },
        ],
        placements: vec![
            placement("PLC-mine", "fix-1"),
            placement("PLC-other", "fix-2"),
        ],
        executions: vec![
            meridian_domain::v1::Execution {
                execution_id: "EXE-mine".into(),
                placement_id: "PLC-mine".into(),
                ..Default::default()
            },
            meridian_domain::v1::Execution {
                execution_id: "EXE-theirs".into(),
                placement_id: "PLC-other".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    venue.bus.serve(LIST_ORDERS, move |_| {
        Ok(("meridian.v1.ListOrdersReply".into(), reply.encode_to_vec()))
    });
    let read = venue
        .sidecar
        .list_orders(Request::new(
            meridian_pb::plugin::v1::ListOrdersParams::default(),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.orders.len(), 1);
    assert_eq!(read.orders[0].order_id, "ORD-mine");
    assert_eq!(read.orders[0].placements.len(), 1);
    assert_eq!(read.placements.len(), 1);
    assert_eq!(read.executions.len(), 1);
    assert_eq!(read.executions[0].execution_id, "EXE-mine");
}

#[tokio::test]
async fn a_venue_fact_kept_with_no_account_is_counted_on_the_plugins_admin_view() {
    let venue = plugin("fix-1", &["connectivity"], None).await;
    venue
        .bus
        .serve("platform.book.command.report-execution", |envelope| {
            let asked =
                meridian_domain::v1::ReportExecutionRequest::decode(&envelope.payload[..]).unwrap();
            assert!(asked.account_id.is_empty(), "no ems mapped the code");
            Ok((
                "meridian.v1.FactReply".into(),
                meridian_domain::v1::FactReply {
                    unplaced: Some(meridian_domain::v1::UnplacedFact {
                        unplaced_id: "UNP-1".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        });
    let figure = |sidecar: &Sidecar| {
        sidecar
            .report(now())
            .figures
            .into_iter()
            .find(|figure| figure.label == crate::report::UNPLACED_LABEL)
    };
    assert!(figure(&venue.sidecar).is_none());
    venue
        .sidecar
        .report_execution(Request::new(
            meridian_pb::plugin::v1::ReportExecutionParams {
                placement_id: "PLC-1".into(),
                venue_account_code: "NOBODY-MAPPED".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let counted = figure(&venue.sidecar).expect("counted");
    assert_eq!(
        counted.value,
        Some(meridian_pb::v1::plugin_figure::Value::Count(1))
    );
}

#[tokio::test]
async fn the_unplaced_count_is_read_from_the_book_and_survives_a_restart() {
    // A sidecar started afresh counts what the book keeps for its plugin:
    // naming no account, not placed since, its raw record its plugin's --
    // every page of the book of no account, read as a core reader.
    let venue = plugin("fix-1", &["connectivity"], None).await;
    let fact =
        |id: &str, account: &str, instance: &str, placed: bool| meridian_domain::v1::UnplacedFact {
            unplaced_id: id.into(),
            account_id: account.into(),
            raw_record: Some(meridian_pb::v1::RawRecordRef {
                instance_id: instance.into(),
                key: format!("orders/{id}"),
            }),
            placed_by: placed.then(meridian_domain::v1::JournalRef::default),
            ..Default::default()
        };
    let pages = [
        meridian_domain::v1::ListOrdersReply {
            unplaced_facts: vec![
                fact("UNP-1", "", "fix-1", false),
                fact("UNP-2", "", "fix-2", false),
            ],
            next_cursor: "after-UNP-2".into(),
            ..Default::default()
        },
        meridian_domain::v1::ListOrdersReply {
            unplaced_facts: vec![
                fact("UNP-3", "", "fix-1", false),
                fact("UNP-4", "", "fix-1", true),
                fact("UNP-5", "ACC-1", "fix-1", false),
            ],
            ..Default::default()
        },
    ];
    venue.bus.serve(LIST_ORDERS, move |envelope| {
        let asked = meridian_domain::v1::ListOrdersRequest::decode(&envelope.payload[..]).unwrap();
        assert_eq!(
            asked.account_id, "~unplaced",
            "the book of no account alone"
        );
        let page = if asked.cursor.is_empty() {
            &pages[0]
        } else {
            &pages[1]
        };
        Ok(("meridian.v1.ListOrdersReply".into(), page.encode_to_vec()))
    });
    let report = venue.sidecar.report_now(now()).await;
    let counted = report
        .figures
        .into_iter()
        .find(|figure| figure.label == crate::report::UNPLACED_LABEL)
        .expect("counted from the book");
    assert_eq!(
        counted.value,
        Some(meridian_pb::v1::plugin_figure::Value::Count(2))
    );
}
