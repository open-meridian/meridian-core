//! The typed operations through the sidecar, against an in-memory bus with a
//! stand-in for each component that answers: the street store records, the
//! conductor holds the plugin's links.

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AccountRecord, AccountState, Accounts, ExternalAccountLink, ExternalAccountsEvent,
    LinkExternalAccountRequest, MissingInstrumentDetectedEvent, PluginConfiguration,
    PluginConfigurationChangedEvent, RecordHoldingReply, RecordHoldingRequest, SyncState,
    SyncStatusEvent,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    AssetClass, Decimal, ExternalAccount, HoldingSide, Identifier, LinkExternalAccountParams,
    Money, ReadAccountsForLinkingParams, RecordHoldingParams, RecordHoldingsStatementParams,
    ReportExternalAccountsParams, ReportMissingInstrumentParams, ReportSyncStatusParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccessLevel, CallerAssertion, CallerClaims, InterfaceDeclaration, PageDeclaration, Refusal,
    RefusalReason, RegisterRequest,
};
use prost::Message;
use tonic::{Code, Request};

use crate::front_door::Verifier;
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};

const RECORD_HOLDING: &str = "platform.street.command.record-holding";
const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.street.command.record-holding\tcommand\tcustody\tstreet\n\
         platform.reference.event.instrument-missing\tevent\tcustody\tinstrument\n\
         platform.custody.*.event.sync-status\tevent\tcustody\tdashboard\n\
         platform.custody.*.event.external-accounts\tevent\tcustody\tdashboard\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n\
         platform.street.command.record-statement\tcommand\tcustody\tstreet\n\
         platform.config.command.link-external-account\tcommand\tcustody\tconductor\n\
         platform.config.query.accounts\tquery\tcustody\tconductor\n\
         platform.reference.query.resolve-identifier\tquery\tcustody\tinstrument\n\
         platform.street.query.list-custodial-positions\tquery\toperations\tstreet\n\
         platform.street.query.list-statements\tquery\toperations\tstreet\n",
        "name\tkind\ncustody\trole\noperations\trole\nstreet\tcomponent\nsidecar\tcomponent\n\
         conductor\tcomponent\ninstrument\tcomponent\n",
    )
    .unwrap()
}

/// A registered sidecar for `snaptrade-1` on its own bus, a conductor that
/// links `ext-1` to `ACC-1` and `ext-out` to `ACC-OUT`, with ACC-1 and ACC-3
/// in the plugin's write scope, and a street store that keeps what it records.
async fn registered(roles: &[&str]) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
    registered_with(roles, None).await
}

async fn registered_with(
    roles: &[&str],
    verifier: Option<Arc<Verifier>>,
) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
    registered_at(roles, verifier, "v2").await
}

/// As `registered_with`, by a plugin built against `version`.
async fn registered_at(
    roles: &[&str],
    verifier: Option<Arc<Verifier>>,
    version: &str,
) -> (Sidecar, Arc<Bus>, Arc<Mutex<Vec<RecordHoldingRequest>>>) {
    let bus = Arc::new(Bus::single(
        "snaptrade-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&recorded);
    bus.serve(RECORD_HOLDING, move |envelope| {
        let row = RecordHoldingRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        keeping.lock().unwrap().push(row);
        Ok((
            "meridian.v1.RecordHoldingReply".into(),
            RecordHoldingReply {
                holding_id: "H-1".into(),
                resolved: true,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "snaptrade-1".into(),
                links: vec![
                    ExternalAccountLink {
                        plugin_instance_id: "snaptrade-1".into(),
                        external_account_id: "ext-1".into(),
                        account_id: "ACC-1".into(),
                    },
                    // Linked, and nobody may write it through this plugin.
                    ExternalAccountLink {
                        plugin_instance_id: "snaptrade-1".into(),
                        external_account_id: "ext-out".into(),
                        account_id: "ACC-OUT".into(),
                    },
                    // Another plugin's link to the same external name, which
                    // must not be this one's.
                    ExternalAccountLink {
                        plugin_instance_id: "other-1".into(),
                        external_account_id: "ext-2".into(),
                        account_id: "ACC-9".into(),
                    },
                ],
                read_account_ids: vec!["ACC-1".into(), "ACC-3".into(), "ACC-R".into()],
                write_account_ids: vec!["ACC-1".into(), "ACC-3".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let roles = roles.iter().map(|r| r.to_string()).collect();
    let mut sidecar = Sidecar::under(
        &contract(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("snaptrade-1", roles),
    );
    if let Some(verifier) = verifier {
        sidecar = sidecar.with_verifier(verifier);
    }
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: version.into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, bus, recorded)
}

/// The next delivery, or a failure rather than a wait that never ends: a
/// message that was not sent is the thing these tests exist to notice.
async fn delivered(listening: &mut meridian_bus::Subscription) -> meridian_bus::Delivery {
    tokio::time::timeout(std::time::Duration::from_secs(2), listening.recv())
        .await
        .expect("nothing was delivered within two seconds")
        .expect("the subscription closed")
}

/// A number as a plugin puts it on the wire, from the decimal a person reads.
fn wire(text: &str) -> Decimal {
    let exact: Exact = text.parse().unwrap();
    let domain = exact.to_wire();
    Decimal {
        high: domain.high,
        low: domain.low,
        scale: domain.scale,
    }
}

fn holding(external: &str) -> RecordHoldingParams {
    RecordHoldingParams {
        statement_id: "S-1".into(),
        instrument_id: String::new(),
        unresolved_identifiers: vec![Identifier {
            scheme: "symbol".into(),
            value: "AAPL".into(),
            source: "snaptrade".into(),
        }],
        quantity: Some(wire("12.5")),
        market_value: Some(Money {
            amount: Some(wire("20000.00")),
            currency_code: "USD".into(),
            instrument_id: String::new(),
        }),
        external_account_id: external.into(),
        side: HoldingSide::Long as i32,
        settle_date_quantity: None,
        currency_assumed: false,
        also_counted_in_cash: false,
        acting_for: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_holding_is_recorded_against_the_account_its_external_account_is_linked_to() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let result = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect("recorded")
        .into_inner();
    assert_eq!(result.holding_id, "H-1");
    assert!(result.resolved);

    let rows = recorded.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].account_id, "ACC-1", "stamped from the link");
    let quantity = Exact::from_wire(rows[0].quantity.as_ref().unwrap()).unwrap();
    assert_eq!(quantity.to_string(), "12.5");
    let value = rows[0].market_value.as_ref().unwrap();
    assert_eq!(
        Exact::from_wire(value.amount.as_ref().unwrap())
            .unwrap()
            .to_string(),
        "20000.00"
    );
    assert_eq!(value.currency_code, "USD");
    assert_eq!(rows[0].unresolved_identifiers[0].value, "AAPL");
}

#[tokio::test]
async fn the_smallest_and_largest_holdings_reach_the_street_store_as_they_were_sent() {
    // spec/quantities-carry-their-own-scale, requirement 7: Alpaca's ninth
    // decimal, a hundred billion units, and those at eighteen decimals.
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let sent = [
        "0.000000001",
        "100000000000",
        "100000000000.000000000000000001",
    ];
    for quantity in sent {
        let mut row = holding("ext-1");
        row.quantity = Some(wire(quantity));
        sidecar
            .record_holding(Request::new(row))
            .await
            .expect("recorded");
    }
    let rows = recorded.lock().unwrap();
    let arrived: Vec<String> = rows
        .iter()
        .map(|row| {
            Exact::from_wire(row.quantity.as_ref().unwrap())
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(arrived, sent);
}

#[tokio::test]
async fn a_number_the_wire_does_not_carry_is_refused_naming_its_field() {
    // Sent raw, past the SDK that would have refused it first. Refused before
    // the link is asked about, so nothing reaches the street store.
    let (sidecar, _, recorded) = registered(&["custody"]).await;

    let mut nineteenth = holding("ext-1");
    nineteenth.quantity = Some(Decimal {
        high: 0,
        low: 1,
        scale: 19,
    });
    let refused = sidecar
        .record_holding(Request::new(nineteenth))
        .await
        .expect_err("a nineteenth decimal place");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused
            .message()
            .starts_with("quantity has 19 decimal places"),
        "{}",
        refused.message()
    );

    // 10^38: a thirty-ninth digit, in an amount.
    let mut too_wide = holding("ext-1");
    too_wide.market_value = Some(Money {
        amount: Some(Decimal {
            high: 5_421_010_862_427_522_170,
            low: 687_399_551_400_673_280,
            scale: 0,
        }),
        currency_code: "USD".into(),
        instrument_id: String::new(),
    });
    let refused = sidecar
        .record_holding(Request::new(too_wide))
        .await
        .expect_err("a thirty-ninth digit");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused
            .message()
            .starts_with("market_value has more than 38 digits"),
        "{}",
        refused.message()
    );

    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unlinked_external_account_is_refused_and_nothing_recorded() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    // ext-2 is linked, but for another plugin.
    for external in ["ext-2", "never-linked"] {
        let refused = sidecar
            .record_holding(Request::new(holding(external)))
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), Code::FailedPrecondition, "{external}");
        assert!(
            refused.message().contains("not linked"),
            "{}",
            refused.message()
        );
        assert_eq!(
            reason(&refused),
            Some(RefusalReason::ExternalAccountNotLinked),
            "{external}: the code a plugin matches, beside the status"
        );
    }
    assert!(recorded.lock().unwrap().is_empty());
}

/// The reason code a refusal carries beside its status, if any
/// (spec/typed-sidecar-operations, section 7).
fn reason(refused: &tonic::Status) -> Option<RefusalReason> {
    let carried = refused.metadata().get_bin(crate::REFUSAL_METADATA)?;
    let bytes = carried.to_bytes().expect("the code is base64 on the wire");
    let refusal = Refusal::decode(bytes.as_ref()).expect("a Refusal");
    RefusalReason::try_from(refusal.reason).ok()
}

#[tokio::test]
async fn a_plugin_cannot_set_what_the_sidecar_stamps() {
    // Bytes for publisher_instance_id (field 5) appended to the params: the
    // plugin-facing message has no such field, so they are not carried, and
    // the sidecar's own value is what reaches the bus.
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe(INSTRUMENT_MISSING);

    let mut bytes = ReportMissingInstrumentParams {
        source: "snaptrade".into(),
        asset_class: AssetClass::Equity as i32,
        ..Default::default()
    }
    .encode_to_vec();
    bytes.extend(
        MissingInstrumentDetectedEvent {
            publisher_instance_id: "somebody-else".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let params = ReportMissingInstrumentParams::decode(bytes.as_slice()).unwrap();

    let published = sidecar
        .report_missing_instrument(Request::new(params))
        .await
        .expect("published")
        .into_inner();
    assert!(!published.message_id.is_empty());

    let delivery = delivered(&mut listening).await;
    let event = MissingInstrumentDetectedEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.publisher_instance_id, "snaptrade-1");
    assert_eq!(event.source, "snaptrade");
    assert_eq!(event.asset_class, AssetClass::Equity as i32);
}

#[tokio::test]
async fn an_asset_class_the_contract_does_not_define_is_refused_naming_the_field() {
    // sdk-contract/asset-class-is-an-enum. Proto3 carries any number, so a
    // plugin that built its params by hand could otherwise put a class nobody
    // ruled on the bus. The SDK refuses it before sending; this is the door.
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe(INSTRUMENT_MISSING);

    let refused = sidecar
        .report_missing_instrument(Request::new(ReportMissingInstrumentParams {
            source: "snaptrade".into(),
            asset_class: 99,
            ..Default::default()
        }))
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "asset_class is 99, which the contract does not define"
    );

    // No class at all is not refused here: a publisher may not know it.
    sidecar
        .report_missing_instrument(Request::new(ReportMissingInstrumentParams {
            source: "snaptrade".into(),
            ..Default::default()
        }))
        .await
        .expect("published without a class");
    let delivery = delivered(&mut listening).await;
    let event = MissingInstrumentDetectedEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.asset_class, AssetClass::Unspecified as i32);
}

#[tokio::test]
async fn a_sync_status_is_published_as_this_instance_and_its_account() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe("platform.custody.snaptrade-1.event.sync-status");
    sidecar
        .report_sync_status(Request::new(ReportSyncStatusParams {
            source: "snaptrade".into(),
            connection_healthy: true,
            external_account_id: "ext-1".into(),
            ..Default::default()
        }))
        .await
        .expect("published");
    let delivery = delivered(&mut listening).await;
    let event = SyncStatusEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.account_id, "ACC-1");
}

#[tokio::test]
async fn an_unlinked_accounts_sync_status_is_published_and_its_holdings_are_still_refused() {
    // Ruled 2026-09-28: a sync status describes the connection, not data
    // recorded against the account, so it reaches the dashboard before a
    // link, with no account. Nothing is recorded against the account, and a
    // holding from it is refused exactly as before.
    let (sidecar, bus, recorded) = registered(&["custody"]).await;
    let mut listening = bus.subscribe("platform.custody.snaptrade-1.event.sync-status");
    sidecar
        .report_sync_status(Request::new(ReportSyncStatusParams {
            source: "snaptrade".into(),
            external_account_id: "ext-nobody-linked".into(),
            state: SyncState::HoldingsUnavailable as i32,
            ..Default::default()
        }))
        .await
        .expect("published though nobody linked it");
    let event =
        SyncStatusEvent::decode(&delivered(&mut listening).await.envelope.payload[..]).unwrap();
    assert_eq!(event.account_id, "", "attributed to no account");
    assert_eq!(event.external_account_id, "ext-nobody-linked");
    assert_eq!(event.state, SyncState::HoldingsUnavailable as i32);
    assert!(
        sidecar.unlinked_now().is_none(),
        "a sync status refused nothing, so nothing is counted"
    );

    let refused = sidecar
        .record_holding(Request::new(holding("ext-nobody-linked")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_sync_status_carries_why_and_how_fresh_as_the_plugin_said() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe("platform.custody.snaptrade-1.event.sync-status");
    sidecar
        .report_sync_status(Request::new(ReportSyncStatusParams {
            source: "snaptrade".into(),
            external_account_id: "ext-1".into(),
            state: SyncState::NeedsSignIn as i32,
            holdings_as_of_ns: 1_757_289_600_000_000_000,
            history_as_of_ns: 1_757_203_200_000_000_000,
            ..Default::default()
        }))
        .await
        .expect("published");
    let event =
        SyncStatusEvent::decode(&delivered(&mut listening).await.envelope.payload[..]).unwrap();
    assert_eq!(event.state, SyncState::NeedsSignIn as i32);
    assert_eq!(event.holdings_as_of_ns, 1_757_289_600_000_000_000);
    assert_eq!(event.history_as_of_ns, 1_757_203_200_000_000_000);
}

#[tokio::test]
async fn the_accounts_a_connection_reaches_are_published_as_this_instance_unlinked_or_not() {
    // W2.8: before anything is recorded, so nothing is refused for want of a
    // link. The instance is the topic's, which is the one a link names.
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let mut listening = bus.subscribe("platform.custody.snaptrade-1.event.external-accounts");
    sidecar
        .report_external_accounts(Request::new(ReportExternalAccountsParams {
            accounts: vec![
                ExternalAccount {
                    external_account_id: "ext-1".into(),
                    name: "Individual Brokerage 1234".into(),
                    venue_account_type: "Individual".into(),
                    ..Default::default()
                },
                ExternalAccount {
                    external_account_id: "ext-nobody-linked".into(),
                    name: "Roth IRA 5678".into(),
                    venue_account_type: "Roth IRA".into(),
                    ..Default::default()
                },
            ],
        }))
        .await
        .expect("published, though one of them has no link");
    let event =
        ExternalAccountsEvent::decode(&delivered(&mut listening).await.envelope.payload[..])
            .unwrap();
    let reported: Vec<&str> = event
        .accounts
        .iter()
        .map(|account| account.external_account_id.as_str())
        .collect();
    assert_eq!(reported, ["ext-1", "ext-nobody-linked"]);
    assert_eq!(event.accounts[1].venue_account_type, "Roth IRA");
    assert!(
        sidecar.unlinked_now().is_none(),
        "reporting an account is not bringing data for it"
    );
}

#[tokio::test]
async fn a_settle_date_quantity_past_the_wire_is_refused_naming_it() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let mut too_fine = holding("ext-1");
    too_fine.settle_date_quantity = Some(Decimal {
        high: 0,
        low: 1,
        scale: 19,
    });
    let refused = sidecar
        .record_holding(Request::new(too_fine))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused
            .message()
            .starts_with("settle_date_quantity has 19 decimal places"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_holding_with_no_market_value_reaches_the_street_store_without_one() {
    // SnapTrade and Kalshi report none; unset is carried as unset, never as
    // a zero in some currency.
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let mut unvalued = holding("ext-1");
    unvalued.market_value = None;
    unvalued.side = HoldingSide::Short as i32;
    unvalued.quantity = Some(wire("-15.25"));
    sidecar
        .record_holding(Request::new(unvalued))
        .await
        .expect("recorded");
    let rows = recorded.lock().unwrap();
    assert!(rows[0].market_value.is_none());
    assert_eq!(rows[0].side, HoldingSide::Short as i32);
}

#[tokio::test]
async fn an_operation_no_role_grants_is_refused_naming_the_grant() {
    let (sidecar, _, recorded) = registered(&[]).await;
    let refused = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect_err("no grant");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains(RECORD_HOLDING),
        "{}",
        refused.message()
    );
    assert!(
        refused.message().contains("no role"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nothing_is_served_before_registration() {
    let bus = Arc::new(Bus::single(
        "snaptrade-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let sidecar = Sidecar::under(
        &contract(),
        bus,
        "DEP-test",
        Identity::new("snaptrade-1", vec!["custody".into()]),
    );
    let refused = sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect_err("not registered");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert!(
        refused.message().contains("not registered"),
        "{}",
        refused.message()
    );
    assert_eq!(
        reason(&refused),
        None,
        "the same status as an unlinked account, told apart by carrying no code"
    );
}

#[tokio::test]
async fn a_link_made_later_is_used_once_the_conductor_says_so() {
    let (sidecar, bus, recorded) = registered(&["custody"]).await;
    // Read once, with ext-3 unlinked.
    assert!(sidecar
        .record_holding(Request::new(holding("ext-3")))
        .await
        .is_err());

    // The conductor now links it, and announces the change.
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "snaptrade-1".into(),
                links: vec![ExternalAccountLink {
                    plugin_instance_id: "snaptrade-1".into(),
                    external_account_id: "ext-3".into(),
                    account_id: "ACC-3".into(),
                }],
                write_account_ids: vec!["ACC-3".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.publish(
        crate::configuration::PLUGIN_CONFIGURATION_CHANGED,
        "meridian.v1.PluginConfigurationChangedEvent",
        PluginConfigurationChangedEvent {
            plugin_instance_id: "snaptrade-1".into(),
            changed_at_ns: 1,
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();

    for _ in 0..50 {
        if sidecar
            .record_holding(Request::new(holding("ext-3")))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows = recorded.lock().unwrap();
    assert_eq!(rows.last().map(|r| r.account_id.as_str()), Some("ACC-3"));
}

// ── Write scope, and a person (W4.9) ────────────────────────────────────────

#[tokio::test]
async fn a_linked_account_nobody_may_write_through_the_plugin_is_refused() {
    // The plugin acting as itself: a link says whose row it is, not that
    // anybody may write it through this plugin (requirement 20).
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let refused = sidecar
        .record_holding(Request::new(holding("ext-out")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused
            .message()
            .contains("outside this plugin's write scope"),
        "{}",
        refused.message()
    );
    assert_eq!(reason(&refused), None, "linked, so not this code");
    assert!(recorded.lock().unwrap().is_empty());
    assert_eq!(
        sidecar.report(0).refused_grants,
        1,
        "and the plugin report counts it"
    );
}

#[tokio::test]
async fn the_configuration_is_read_again_after_30_seconds_and_trusted_for_10_minutes() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    const T0: i64 = 1_790_000_000_000_000_000;
    const SECOND: i64 = 1_000_000_000;
    let scope = |c: meridian_domain::v1::PluginConfiguration| c.write_account_ids;
    assert!(scope(sidecar.configuration(T0).await.unwrap()).contains(&"ACC-1".to_string()));

    // The conductor now says ACC-1 is out, and then stops answering.
    let answers = Arc::new(Mutex::new(0));
    let counting = Arc::clone(&answers);
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        let mut answered = counting.lock().unwrap();
        *answered += 1;
        if *answered > 1 {
            return Err("the conductor is down".into());
        }
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    assert!(
        scope(sidecar.configuration(T0 + 29 * SECOND).await.unwrap())
            .contains(&"ACC-1".to_string()),
        "inside 30 seconds, as last read"
    );
    assert_eq!(*answers.lock().unwrap(), 0);
    assert!(
        scope(sidecar.configuration(T0 + 31 * SECOND).await.unwrap()).is_empty(),
        "past it, read again"
    );
    let read_at = T0 + 31 * SECOND;
    assert!(
        sidecar
            .configuration(read_at + 9 * 60 * SECOND)
            .await
            .is_ok(),
        "the conductor down: as last read, within 10 minutes"
    );
    let refused = sidecar
        .configuration(read_at + 11 * 60 * SECOND)
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        Code::Aborted,
        "and refused past them, with the conductor's reason"
    );
}

const KEY_ID: &str = "dashboard-2026-09-0a1b2c3d";

fn now() -> i64 {
    use meridian_clock::Clock as _;
    meridian_clock::SystemClock.now_ns()
}

/// What a person holds on the plugin: the accounts they may read and the
/// accounts they may write through it (decisions/026).
#[derive(Default)]
struct Held {
    read: Vec<String>,
    write: Vec<String>,
}

/// What the dashboard would have signed for a person holding `access`, in a
/// session opened by Open.
fn assertion(key: &SigningKey, access: Held) -> CallerAssertion {
    signed(key, access, false)
}

/// The same, or when `admin`, a deployment admin's session opened by Manage,
/// which carries no account.
fn signed(key: &SigningKey, access: Held, admin: bool) -> CallerAssertion {
    match admin {
        true => claimed(key, Held::default(), AccessLevel::Admin, true),
        false => claimed(key, access, AccessLevel::Write, false),
    }
}

/// What the dashboard would have signed for a session at `level`.
fn claimed(
    key: &SigningKey,
    access: Held,
    level: AccessLevel,
    deployment_admin: bool,
) -> CallerAssertion {
    let issued = now();
    let claims = CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: "snaptrade-1".into(),
        read_account_ids: access.read,
        write_account_ids: access.write,
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-1".into(),
        deployment_admin,
        level: level as i32,
        ..CallerClaims::default()
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: KEY_ID.into(),
    }
}

fn writing(accounts: &[&str]) -> Held {
    Held {
        read: accounts.iter().map(|a| a.to_string()).collect(),
        write: accounts.iter().map(|a| a.to_string()).collect(),
    }
}

fn reading(accounts: &[&str]) -> Held {
    Held {
        read: accounts.iter().map(|a| a.to_string()).collect(),
        write: vec![],
    }
}

/// A sidecar holding the dashboard's key, and a street store that keeps whom
/// each row was recorded for.
async fn for_people() -> (Sidecar, SigningKey, Arc<Mutex<Vec<String>>>) {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(
        "snaptrade-1",
        KEY_ID,
        key.verifying_key(),
    ));
    let (sidecar, bus, _) = registered_with(&["custody"], Some(verifier)).await;
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&subjects);
    bus.serve(RECORD_HOLDING, move |envelope| {
        let meta = envelope.meta.clone().unwrap_or_default();
        keeping.lock().unwrap().push(meta.acting_for_subject);
        Ok((
            "meridian.v1.RecordHoldingReply".into(),
            RecordHoldingReply {
                holding_id: "H-1".into(),
                resolved: true,
            }
            .encode_to_vec(),
        ))
    });
    (sidecar, key, subjects)
}

fn for_person(external: &str, assertion: CallerAssertion) -> RecordHoldingParams {
    RecordHoldingParams {
        acting_for: Some(assertion),
        ..holding(external)
    }
}

#[tokio::test]
async fn a_command_sent_for_a_person_who_may_write_the_account_is_stamped_with_them() {
    let (sidecar, key, subjects) = for_people().await;
    sidecar
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&key, writing(&["ACC-1"])),
        )))
        .await
        .expect("admitted");
    sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect("and the plugin as itself");
    assert_eq!(
        *subjects.lock().unwrap(),
        vec!["local|ada".to_string(), String::new()]
    );
}

/// W4.9 (contract v9): a person through a client -- the CLI, their agent --
/// is stamped with the delegation the assertion names, beside them, and from
/// v10 the client's name beside the delegation; a
/// browser's assertion names none, and the plugin as itself is stamped with
/// nobody.
#[tokio::test]
async fn a_person_through_a_client_is_stamped_with_their_delegation() {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(
        "snaptrade-1",
        KEY_ID,
        key.verifying_key(),
    ));
    let (sidecar, bus, _) = registered_with(&["custody"], Some(verifier)).await;
    let stamped = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&stamped);
    bus.serve(RECORD_HOLDING, move |envelope| {
        let meta = envelope.meta.clone().unwrap_or_default();
        keeping.lock().unwrap().push((
            meta.acting_for_subject,
            meta.acting_through_delegation,
            meta.acting_through_client,
        ));
        Ok((
            "meridian.v1.RecordHoldingReply".into(),
            RecordHoldingReply {
                holding_id: "H-1".into(),
                resolved: true,
            }
            .encode_to_vec(),
        ))
    });
    let through = |delegation: &str, client: &str| {
        let issued = now();
        let claims = CallerClaims {
            subject: "local|ada".into(),
            display_name: "Ada".into(),
            audience_instance_id: "snaptrade-1".into(),
            read_account_ids: vec!["ACC-1".into()],
            write_account_ids: vec!["ACC-1".into()],
            issued_at_ns: issued,
            expires_at_ns: issued + 60_000_000_000,
            assertion_id: format!("a-{delegation}"),
            level: AccessLevel::Write as i32,
            delegation_id: delegation.into(),
            client_name: client.into(),
            ..CallerClaims::default()
        }
        .encode_to_vec();
        CallerAssertion {
            signature: key.sign(&claims).to_bytes().to_vec(),
            claims,
            key_id: KEY_ID.into(),
        }
    };
    for assertion in [through("DLG-1", "meridian on ada-laptop"), through("", "")] {
        sidecar
            .record_holding(Request::new(for_person("ext-1", assertion)))
            .await
            .expect("admitted");
    }
    sidecar
        .record_holding(Request::new(holding("ext-1")))
        .await
        .expect("and the plugin as itself");
    assert_eq!(
        *stamped.lock().unwrap(),
        vec![
            (
                "local|ada".to_string(),
                "DLG-1".to_string(),
                "meridian on ada-laptop".to_string()
            ),
            ("local|ada".to_string(), String::new(), String::new()),
            (String::new(), String::new(), String::new()),
        ]
    );
}

#[tokio::test]
async fn a_person_is_refused_an_account_they_may_only_read_or_not_reach_at_all() {
    let (sidecar, key, subjects) = for_people().await;
    for access in [reading(&["ACC-1"]), writing(&["ACC-3"])] {
        let refused = sidecar
            .record_holding(Request::new(for_person("ext-1", assertion(&key, access))))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        assert!(
            refused.message().contains("may not write account ACC-1"),
            "{}",
            refused.message()
        );
    }
    // Nor does a person widen the plugin: ACC-OUT is outside its scope,
    // whatever they were vouched to write.
    let refused = sidecar
        .record_holding(Request::new(for_person(
            "ext-out",
            assertion(&key, writing(&["ACC-OUT"])),
        )))
        .await
        .unwrap_err();
    assert!(
        refused.message().contains("write scope"),
        "{}",
        refused.message()
    );
    assert!(subjects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_assertion_the_dashboard_did_not_sign_vouches_for_nobody() {
    let (sidecar, _, subjects) = for_people().await;
    let stranger = SigningKey::generate(&mut rand::rngs::OsRng);
    let refused = sidecar
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&stranger, writing(&["ACC-1"])),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unauthenticated);

    // Nor does a sidecar given no keys vouch for anybody.
    let (keyless, _, _) = registered(&["custody"]).await;
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let refused = keyless
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&key, writing(&["ACC-1"])),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unauthenticated);
    assert!(subjects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_assertion_the_page_was_opened_with_vouches_for_its_commands_too() {
    // Its id was recorded at the front door; a command carrying it is the
    // plugin acting on that request, not a replay.
    let (sidecar, key, subjects) = for_people().await;
    let once = assertion(&key, writing(&["ACC-1"]));
    for _ in 0..2 {
        sidecar
            .record_holding(Request::new(for_person("ext-1", once.clone())))
            .await
            .expect("admitted");
    }
    assert_eq!(subjects.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_command_naming_no_account_is_sent_for_somebody_who_may_write_something() {
    let (sidecar, key, _) = for_people().await;
    sidecar
        .bus
        .serve("platform.street.command.record-statement", |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            assert_eq!(meta.acting_for_subject, "local|ada");
            Ok((
                "meridian.v1.RecordHoldingsStatementReply".into(),
                meridian_domain::v1::RecordHoldingsStatementReply {
                    statement_id: "S-1".into(),
                    already_recorded: false,
                }
                .encode_to_vec(),
            ))
        });
    let statement = |access| RecordHoldingsStatementParams {
        source: "snaptrade".into(),
        expected_rows: 1,
        acting_for: Some(assertion(&key, access)),
        ..Default::default()
    };
    let refused = sidecar
        .record_holdings_statement(Request::new(statement(reading(&["ACC-1"]))))
        .await
        .unwrap_err();
    assert!(
        refused.message().contains("may write nothing"),
        "{}",
        refused.message()
    );
    sidecar
        .record_holdings_statement(Request::new(statement(writing(&["ACC-1"]))))
        .await
        .expect("admitted");
}

// ── The deployment's configuration, for a deployment admin (W6.4) ───────────

const LINK: &str = "platform.config.command.link-external-account";
const ACCOUNTS: &str = "platform.config.query.accounts";

/// What the conductor heard: each link, and whom it was for.
type Heard = Arc<Mutex<Vec<(LinkExternalAccountRequest, String)>>>;

/// A sidecar holding the dashboard's key, whose plugin reported `reported`,
/// and a conductor that links as asked -- making ACC-NEW for a new account's
/// name -- and answers the accounts it holds, keeping whom each was for.
async fn linking(reported: &[&str]) -> (Sidecar, SigningKey, Heard, Arc<Mutex<Vec<String>>>) {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(
        "snaptrade-1",
        KEY_ID,
        key.verifying_key(),
    ));
    let (sidecar, bus, _) = registered_with(&["custody"], Some(verifier)).await;
    let heard: Heard = Arc::default();
    let keeping = Arc::clone(&heard);
    bus.serve(LINK, move |envelope| {
        let request = LinkExternalAccountRequest::decode(&envelope.payload[..]).unwrap();
        let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
        keeping.lock().unwrap().push((request.clone(), by));
        let account = if request.new_account_name.is_empty() {
            request.account_id
        } else {
            "ACC-NEW".into()
        };
        Ok((
            "meridian.v1.ExternalAccountLink".into(),
            ExternalAccountLink {
                plugin_instance_id: request.plugin_instance_id,
                external_account_id: request.external_account_id,
                account_id: account,
            }
            .encode_to_vec(),
        ))
    });
    let readers = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&readers);
    bus.serve(ACCOUNTS, move |envelope| {
        keeping
            .lock()
            .unwrap()
            .push(envelope.meta.clone().unwrap_or_default().acting_for_subject);
        Ok((
            "meridian.v1.Accounts".into(),
            Accounts {
                accounts: vec![AccountRecord {
                    account_id: "ACC-1".into(),
                    name: "Growth".into(),
                    state: AccountState::Open as i32,
                    created_at_ns: 1,
                    custodian: "Fidelity".into(),
                    account_type: "Roth IRA".into(),
                    owner: "Fund I".into(),
                    note: "Rollover, 2026.".into(),
                }],
            }
            .encode_to_vec(),
        ))
    });
    if !reported.is_empty() {
        sidecar
            .report_external_accounts(Request::new(ReportExternalAccountsParams {
                accounts: reported
                    .iter()
                    .map(|id| ExternalAccount {
                        external_account_id: id.to_string(),
                        ..Default::default()
                    })
                    .collect(),
            }))
            .await
            .expect("reported");
    }
    (sidecar, key, heard, readers)
}

fn link(
    external: &str,
    account: &str,
    new_name: &str,
    by: Option<CallerAssertion>,
) -> LinkExternalAccountParams {
    LinkExternalAccountParams {
        external_account_id: external.into(),
        account_id: account.into(),
        new_account_name: new_name.into(),
        acting_for: by,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_link_for_a_deployment_admin_is_stamped_with_them_and_this_plugin() {
    // Linking ext-new to ACC-9, an account nothing may write through this
    // plugin yet: the link is what grants it (W4.11), so the write scope is
    // not what admits it.
    let (sidecar, key, heard, _) = linking(&["ext-new", "st-2"]).await;
    let admin = || Some(signed(&key, Held::default(), true));
    let linked = sidecar
        .link_external_account(Request::new(link("ext-new", "ACC-9", "", admin())))
        .await
        .expect("admitted")
        .into_inner();
    assert_eq!(linked.account_id, "ACC-9");
    let created = sidecar
        .link_external_account(Request::new(LinkExternalAccountParams {
            new_account_custodian: "Fidelity".into(),
            new_account_type: "Roth IRA".into(),
            new_account_owner: "Fund I".into(),
            new_account_note: "Linked from SnapTrade.".into(),
            ..link("st-2", "", "Fidelity Brokerage", admin())
        }))
        .await
        .expect("admitted")
        .into_inner();
    assert_eq!(
        created.account_id, "ACC-NEW",
        "the account the conductor made"
    );
    sidecar
        .link_external_account(Request::new(link("ext-new", "", "", admin())))
        .await
        .expect("an unlink, admitted");

    let heard = heard.lock().unwrap();
    assert_eq!(heard.len(), 3);
    for (request, by) in heard.iter() {
        assert_eq!(
            request.plugin_instance_id, "snaptrade-1",
            "stamped by the sidecar"
        );
        assert_eq!(by, "local|ada", "recorded as hers");
    }
    assert_eq!(heard[1].0.new_account_name, "Fidelity Brokerage");
    assert_eq!(
        (
            heard[1].0.new_account_custodian.as_str(),
            heard[1].0.new_account_type.as_str(),
            heard[1].0.new_account_owner.as_str(),
            heard[1].0.new_account_note.as_str(),
        ),
        ("Fidelity", "Roth IRA", "Fund I", "Linked from SnapTrade."),
        "the new account's description reaches the conductor as sent (W6.4)"
    );
    assert_eq!(
        (
            heard[2].0.account_id.as_str(),
            heard[2].0.new_account_name.as_str()
        ),
        ("", "")
    );
}

#[tokio::test]
async fn a_link_is_refused_without_a_deployment_admins_assertion() {
    let (sidecar, key, heard, _) = linking(&["ext-new"]).await;
    for (by, said) in [
        (None, "carries no assertion"),
        // A person who writes the account, in a session opened by Open.
        (
            Some(signed(&key, writing(&["ACC-1"]), false)),
            "Open (write)",
        ),
        // A deployment admin, in a session opened by View.
        (
            Some(claimed(&key, reading(&["ACC-1"]), AccessLevel::Read, true)),
            "View (read)",
        ),
        // One naming no level holds nothing.
        (
            Some(claimed(
                &key,
                Held::default(),
                AccessLevel::Unspecified,
                true,
            )),
            "level-less",
        ),
    ] {
        let refused = sidecar
            .link_external_account(Request::new(link("ext-new", "ACC-1", "", by)))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        assert!(refused.message().contains(said), "{}", refused.message());
        assert!(refused.message().contains("admin of the plugin"));
    }
    // One the dashboard did not sign is not an admin's however it reads.
    let forged = signed(
        &SigningKey::generate(&mut rand::rngs::OsRng),
        Held::default(),
        true,
    );
    let refused = sidecar
        .link_external_account(Request::new(link("ext-new", "ACC-1", "", Some(forged))))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unauthenticated);
    assert!(
        heard.lock().unwrap().is_empty(),
        "nothing reaches the conductor"
    );
    assert_eq!(
        sidecar.report(0).refused_grants,
        4,
        "the report counts each"
    );
}

#[tokio::test]
async fn a_link_is_refused_for_an_external_account_this_plugin_did_not_report() {
    // W2.8: ext-new is reported; ext-2 is another plugin's name, and
    // never-reported nobody's. ext-1 is linked and unreported: it may be
    // unlinked, and linked to nothing else.
    let (sidecar, key, heard, _) = linking(&["ext-new"]).await;
    let admin = || Some(signed(&key, Held::default(), true));
    for (external, account, new_name) in [
        ("ext-2", "ACC-1", ""),
        ("never-reported", "", "A new one"),
        ("never-reported", "", ""),
        ("ext-1", "ACC-3", ""),
    ] {
        let refused = sidecar
            .link_external_account(Request::new(link(external, account, new_name, admin())))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied, "{external}");
        assert!(
            refused.message().contains("not one this plugin reported"),
            "{}",
            refused.message()
        );
    }
    assert!(heard.lock().unwrap().is_empty());
    sidecar
        .link_external_account(Request::new(link("ext-1", "", "", admin())))
        .await
        .expect("its own link, removed");

    // Reported again without it, ext-new is no longer one to link.
    sidecar
        .report_external_accounts(Request::new(ReportExternalAccountsParams {
            accounts: vec![ExternalAccount {
                external_account_id: "st-9".into(),
                ..Default::default()
            }],
        }))
        .await
        .unwrap();
    let refused = sidecar
        .link_external_account(Request::new(link("ext-new", "ACC-1", "", admin())))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn the_deployments_accounts_are_read_only_for_a_deployment_admin() {
    let (sidecar, key, _, readers) = linking(&[]).await;
    let read = sidecar
        .read_accounts_for_linking(Request::new(ReadAccountsForLinkingParams {
            acting_for: Some(signed(&key, Held::default(), true)),
        }))
        .await
        .expect("read for her")
        .into_inner();
    assert_eq!(read.accounts.len(), 1);
    assert_eq!(
        (
            read.accounts[0].account_id.as_str(),
            read.accounts[0].name.as_str()
        ),
        ("ACC-1", "Growth")
    );
    assert_eq!(
        (
            read.accounts[0].custodian.as_str(),
            read.accounts[0].account_type.as_str(),
            read.accounts[0].owner.as_str(),
            read.accounts[0].note.as_str(),
        ),
        ("Fidelity", "Roth IRA", "Fund I", "Rollover, 2026."),
        "each account's custodian, type, owner and note reach the plugin (W6.4)"
    );
    for by in [None, Some(signed(&key, reading(&["ACC-1"]), false))] {
        let refused = sidecar
            .read_accounts_for_linking(Request::new(ReadAccountsForLinkingParams {
                acting_for: by,
            }))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
    }
    assert_eq!(
        *readers.lock().unwrap(),
        vec!["local|ada".to_string()],
        "asked once, for her"
    );
}

#[tokio::test]
async fn the_report_carries_the_interface_the_plugin_declared() {
    // W4.8: its pages, each with the levels it serves, in its order, for the
    // plugin area's tab rows (W6.9).
    let bus = Arc::new(Bus::single(
        "snaptrade-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let sidecar = Sidecar::under(
        &contract(),
        bus,
        "DEP-test",
        Identity::new("snaptrade-1", vec!["custody".into()]),
    );
    assert!(
        sidecar.report(0).declared_interface.is_none(),
        "not registered, none"
    );
    let declared = InterfaceDeclaration {
        loopback_port: 8000,
        title: "SnapTrade".into(),
        pages: vec![
            PageDeclaration {
                path: "/admin/connections".into(),
                title: "Connections".into(),
                levels: vec![AccessLevel::Admin as i32],
                roles: vec![],
            },
            PageDeclaration {
                path: "/statements".into(),
                title: "Statements".into(),
                levels: vec![AccessLevel::Write as i32, AccessLevel::Read as i32],
                roles: vec![],
            },
        ],
    };
    sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            interface: Some(declared.clone()),
            ..Default::default()
        }))
        .await
        .unwrap();
    // Each page with the role it serves filled, on a plugin holding one.
    let mut served = declared.clone();
    for page in &mut served.pages {
        page.roles = vec!["custody".into()];
    }
    assert_eq!(sidecar.report(0).declared_interface, Some(served));
    sidecar
        .leave(Request::new(meridian_pb::v1::LeaveRequest::default()))
        .await
        .unwrap();
    assert!(
        sidecar.report(0).declared_interface.is_none(),
        "gone when it leaves"
    );
}

// ── A session's level (W4.9, W6.9; 2026-09-30) ────────────────────────────

#[tokio::test]
async fn a_plugin_admin_links_to_an_existing_account_and_never_names_a_new_one() {
    let (sidecar, key, heard, _) = linking(&["ext-new", "st-2"]).await;
    let manage = || Some(claimed(&key, Held::default(), AccessLevel::Admin, false));
    sidecar
        .link_external_account(Request::new(link("ext-new", "ACC-9", "", manage())))
        .await
        .expect("any existing account, whatever they may read");
    let refused = sidecar
        .link_external_account(Request::new(link(
            "st-2",
            "",
            "Fidelity Brokerage",
            manage(),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused
            .message()
            .contains("only a deployment admin names a new one"),
        "{}",
        refused.message()
    );
    assert_eq!(heard.lock().unwrap().len(), 1, "the refusal reached nobody");
    sidecar
        .read_accounts_for_linking(Request::new(ReadAccountsForLinkingParams {
            acting_for: manage(),
        }))
        .await
        .expect("and reads every account's identity to offer");
}

#[tokio::test]
async fn a_command_is_sent_for_a_person_only_in_a_session_opened_by_open() {
    let (sidecar, key, subjects) = for_people().await;
    for (level, said) in [
        (AccessLevel::Admin, "Manage (admin)"),
        (AccessLevel::Read, "View (read)"),
        (AccessLevel::Unspecified, "level-less"),
    ] {
        // Carrying a write set does not make it Open: the level does.
        let refused = sidecar
            .record_holding(Request::new(for_person(
                "ext-1",
                claimed(&key, writing(&["ACC-1"]), level, false),
            )))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        assert!(refused.message().contains(said), "{}", refused.message());
    }
    assert!(subjects.lock().unwrap().is_empty());
    sidecar
        .record_holding(Request::new(for_person(
            "ext-1",
            claimed(&key, writing(&["ACC-1"]), AccessLevel::Write, false),
        )))
        .await
        .expect("admitted under Open");
}

#[tokio::test]
async fn a_page_serving_no_level_is_refused_at_registration_naming_it() {
    let bus = Arc::new(Bus::single(
        "snaptrade-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let sidecar = Sidecar::under(
        &contract(),
        bus,
        "DEP-test",
        Identity::new("snaptrade-1", vec!["custody".into()]),
    );
    for levels in [vec![], vec![AccessLevel::Unspecified as i32], vec![9]] {
        let reply = sidecar
            .register(Request::new(RegisterRequest {
                schema_version: "v5".into(),
                interface: Some(InterfaceDeclaration {
                    loopback_port: 8000,
                    title: "SnapTrade".into(),
                    pages: vec![PageDeclaration {
                        path: "/statements".into(),
                        title: "Statements".into(),
                        levels,
                        roles: vec![],
                    }],
                }),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!reply.admitted);
        assert!(
            reply.refusal_reason.contains("/statements (Statements)"),
            "{}",
            reply.refusal_reason
        );
    }
}

// ── Contract v7: a statement's account and figures, reads within a scope ────

const RECORD_STATEMENT: &str = "platform.street.command.record-statement";

/// A street store that keeps each statement it is asked to open.
fn opening(bus: &Bus) -> Arc<Mutex<Vec<meridian_domain::v1::RecordHoldingsStatementRequest>>> {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&opened);
    bus.serve(RECORD_STATEMENT, move |envelope| {
        keeping.lock().unwrap().push(
            meridian_domain::v1::RecordHoldingsStatementRequest::decode(&envelope.payload[..])
                .unwrap(),
        );
        Ok((
            "meridian.v1.RecordHoldingsStatementReply".into(),
            meridian_domain::v1::RecordHoldingsStatementReply {
                statement_id: "S-1".into(),
                already_recorded: false,
            }
            .encode_to_vec(),
        ))
    });
    opened
}

fn statement(external: &str) -> RecordHoldingsStatementParams {
    RecordHoldingsStatementParams {
        source: "snaptrade".into(),
        external_account_id: external.into(),
        institution: "Interactive Brokers".into(),
        expected_rows: 1,
        figures: vec![meridian_pb::plugin::v1::StatementFigures {
            segment: String::new(),
            buying_power: Some(Money {
                amount: Some(wire("25000.00")),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_statement_from_a_v7_plugin_is_recorded_against_its_external_accounts_link() {
    let (sidecar, bus, _) = registered_at(&["custody"], None, "v7").await;
    let opened = opening(&bus);
    sidecar
        .record_holdings_statement(Request::new(statement("ext-1")))
        .await
        .expect("recorded");
    {
        let opened = opened.lock().unwrap();
        assert_eq!(opened[0].account_id, "ACC-1", "stamped from the link");
        assert_eq!(opened[0].institution, "Interactive Brokers");
    }

    let unlinked = sidecar
        .record_holdings_statement(Request::new(statement("ext-9")))
        .await
        .unwrap_err();
    assert_eq!(unlinked.code(), Code::FailedPrecondition);
    assert_eq!(
        reason(&unlinked),
        Some(RefusalReason::ExternalAccountNotLinked)
    );

    let none = sidecar
        .record_holdings_statement(Request::new(statement("")))
        .await
        .unwrap_err();
    assert_eq!(none.code(), Code::InvalidArgument);
    assert!(none.message().contains("external_account_id is required"));
}

#[tokio::test]
async fn a_statement_from_a_plugin_before_v7_is_admitted_with_no_account() {
    let (sidecar, bus, _) = registered_at(&["custody"], None, "v6").await;
    let opened = opening(&bus);
    sidecar
        .record_holdings_statement(Request::new(RecordHoldingsStatementParams {
            source: "snaptrade".into(),
            expected_rows: 1,
            buying_power: Some(Money {
                amount: Some(wire("25000.00")),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            ..Default::default()
        }))
        .await
        .expect("admitted, as before v7");
    assert_eq!(
        opened.lock().unwrap()[0].account_id,
        "",
        "its rows will give it one"
    );
}

#[tokio::test]
async fn a_statements_figures_that_cannot_stand_are_refused_naming_the_field() {
    let (sidecar, bus, _) = registered_at(&["custody"], None, "v7").await;
    let opened = opening(&bus);
    let mut both = statement("ext-1");
    both.buying_power = Some(Money {
        amount: Some(wire("1")),
        currency_code: "USD".into(),
        instrument_id: String::new(),
    });
    let refused = sidecar
        .record_holdings_statement(Request::new(both))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "buying_power is read from a plugin before v7; send it in figures"
    );

    let mut far = statement("ext-1");
    far.figures[0].collateral = vec![meridian_pb::plugin::v1::ReportedCollateral {
        direction: meridian_pb::plugin::v1::CollateralDirection::Posted as i32,
        instrument_id: "INS-1".into(),
        quantity: Some(wire("1")),
        haircut: Some(Decimal {
            high: 0,
            low: 1,
            scale: 19,
        }),
        ..Default::default()
    }];
    let refused = sidecar
        .record_holdings_statement(Request::new(far))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused
            .message()
            .starts_with("figures[0].collateral[0].haircut"),
        "{}",
        refused.message()
    );
    assert!(
        opened.lock().unwrap().is_empty(),
        "nothing reached the street store"
    );
}

#[tokio::test]
async fn a_number_inside_a_lot_is_refused_naming_its_path() {
    let (sidecar, _, recorded) = registered(&["custody"]).await;
    let mut row = holding("ext-1");
    row.lots = vec![meridian_pb::plugin::v1::ReportedLot {
        quantity: Some(Decimal {
            high: 0,
            low: 1,
            scale: 19,
        }),
        ..Default::default()
    }];
    let refused = sidecar.record_holding(Request::new(row)).await.unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused.message().starts_with("lots[0].quantity"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_read_is_stamped_with_the_scope_marked_and_one_outside_it_refused_first() {
    let (sidecar, bus, _) = registered(&["operations"]).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&seen);
    bus.serve(
        "platform.street.query.list-custodial-positions",
        move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            keeping
                .lock()
                .unwrap()
                .push((meta.account_scope_applies, meta.account_scope));
            Ok((
                "meridian.v1.ListCustodialPositionsReply".into(),
                meridian_domain::v1::ListCustodialPositionsReply::default().encode_to_vec(),
            ))
        },
    );

    sidecar
        .list_custodial_positions(Request::new(
            meridian_pb::plugin::v1::ListCustodialPositionsParams::default(),
        ))
        .await
        .expect("the whole scope");
    let refused = sidecar
        .list_custodial_positions(Request::new(
            meridian_pb::plugin::v1::ListCustodialPositionsParams {
                account_id: "ACC-9".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "ACC-9 is not in this plugin's read scope"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the refused read never left");
    assert!(seen[0].0, "marked as applying");
    assert_eq!(seen[0].1, ["ACC-1", "ACC-3", "ACC-R"]);
}

#[tokio::test]
async fn an_unscoped_read_is_stamped_too_and_answered_whatever_the_scope() {
    let (sidecar, bus, _) = registered(&["custody"]).await;
    let marked = Arc::new(Mutex::new(None));
    let keeping = Arc::clone(&marked);
    bus.serve(
        "platform.reference.query.resolve-identifier",
        move |envelope| {
            *keeping.lock().unwrap() = Some(
                envelope
                    .meta
                    .clone()
                    .unwrap_or_default()
                    .account_scope_applies,
            );
            Ok((
                "meridian.v1.ResolveIdentifierReply".into(),
                meridian_domain::v1::ResolveIdentifierReply::default().encode_to_vec(),
            ))
        },
    );
    sidecar
        .resolve_identifier(Request::new(
            meridian_pb::plugin::v1::ResolveIdentifierParams::default(),
        ))
        .await
        .expect("answered");
    assert_eq!(*marked.lock().unwrap(), Some(true));
}

// ── Contract v8: a component's refusal carries its code ─────────────────────

/// An `operations` plugin's sidecar, its contract the book's command, and a
/// book that refuses every break for want of an opening balance, with its
/// code (W9.4).
async fn before_the_book_holds_the_account() -> Sidecar {
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.book.command.record-break\tcommand\toperations\tbor\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\noperations\trole\nbor\tcomponent\nsidecar\tcomponent\n\
         conductor\tcomponent\n",
    )
    .unwrap();
    let bus = Arc::new(Bus::single(
        "operations-sample-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve("platform.book.command.record-break", |_| {
        Err(meridian_bus::refusal(
            RefusalReason::NoOpeningBalance as i32,
            "account ACC-1 has no opening balance; it enters the book once, with one (W9.1)",
        ))
    });
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "operations-sample-1".into(),
                read_account_ids: vec!["ACC-1".into()],
                write_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract,
        bus,
        "DEP-test",
        Identity::new("operations-sample-1", vec!["operations".into()]),
    );
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v10".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    sidecar
}

#[tokio::test]
async fn a_components_refusal_reaches_the_plugin_with_its_code_beside_aborted() {
    let sidecar = before_the_book_holds_the_account().await;
    let refused = sidecar
        .record_break(Request::new(meridian_pb::plugin::v1::RecordBreakParams {
            account_id: "ACC-1".into(),
            business_date: "2026-09-09".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Aborted);
    assert_eq!(reason(&refused), Some(RefusalReason::NoOpeningBalance));
    // The words alone: the code rides beside them, not in them.
    assert_eq!(
        refused.message(),
        "account ACC-1 has no opening balance; it enters the book once, with one (W9.1)"
    );
}

/// Contract v9: a refusal naming what the command left out carries each
/// field beside its code, so a plugin shows a person what to complete.
#[tokio::test]
async fn an_incomplete_command_reaches_the_plugin_with_each_missing_field() {
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.book.command.record-break\tcommand\toperations\tbor\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\noperations\trole\nbor\tcomponent\nsidecar\tcomponent\n\
         conductor\tcomponent\n",
    )
    .unwrap();
    let bus = Arc::new(Bus::single(
        "operations-sample-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve("platform.book.command.record-break", |_| {
        Err(meridian_bus::refusal_naming(
            RefusalReason::Incomplete as i32,
            &[
                "positions[0].settled_quantity".into(),
                "positions[0].lots".into(),
            ],
            "the opening balance is incomplete",
        ))
    });
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "operations-sample-1".into(),
                read_account_ids: vec!["ACC-1".into()],
                write_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract,
        bus,
        "DEP-test",
        Identity::new("operations-sample-1", vec!["operations".into()]),
    );
    sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v10".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let refused = sidecar
        .record_break(Request::new(meridian_pb::plugin::v1::RecordBreakParams {
            account_id: "ACC-1".into(),
            business_date: "2026-09-09".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Aborted);
    let carried = refused
        .metadata()
        .get_bin(crate::REFUSAL_METADATA)
        .expect("a refusal");
    let refusal = Refusal::decode(carried.to_bytes().unwrap().as_ref()).unwrap();
    assert_eq!(refusal.reason, RefusalReason::Incomplete as i32);
    assert_eq!(
        refusal.fields,
        vec!["positions[0].settled_quantity", "positions[0].lots"]
    );
    assert_eq!(refused.message(), "the opening balance is incomplete");
}

/// Contract v10: the book could not check a command because the instrument
/// store did not answer, so the refusal is `unavailable`, the status a caller
/// tries again, with its code beside it.
#[tokio::test]
async fn a_command_the_book_could_not_check_is_unavailable_to_try_again() {
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.book.command.record-break\tcommand\toperations\tbor\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\noperations\trole\nbor\tcomponent\nsidecar\tcomponent\n\
         conductor\tcomponent\n",
    )
    .unwrap();
    let bus = Arc::new(Bus::single(
        "operations-sample-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve("platform.book.command.record-break", |_| {
        Err(meridian_bus::refusal(
            RefusalReason::ReferenceUnavailable as i32,
            "the instrument store did not answer; try again",
        ))
    });
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "operations-sample-1".into(),
                read_account_ids: vec!["ACC-1".into()],
                write_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract,
        bus,
        "DEP-test",
        Identity::new("operations-sample-1", vec!["operations".into()]),
    );
    sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v10".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let refused = sidecar
        .record_break(Request::new(meridian_pb::plugin::v1::RecordBreakParams {
            account_id: "ACC-1".into(),
            business_date: "2026-09-09".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unavailable);
    let carried = refused
        .metadata()
        .get_bin(crate::REFUSAL_METADATA)
        .expect("a refusal");
    let refusal = Refusal::decode(carried.to_bytes().unwrap().as_ref()).unwrap();
    assert_eq!(refusal.reason, RefusalReason::ReferenceUnavailable as i32);
    assert!(refusal.fields.is_empty());
    assert_eq!(
        refused.message(),
        "the instrument store did not answer; try again"
    );
}

// ── By the role holding the act (contract v15, decisions/033; W4.9) ───────

/// A sidecar for a plugin holding `roles`, the dashboard's key in hand, its
/// street store keeping what it records, and its conductor taking a link.
async fn holding_roles(
    roles: &[&str],
    version: &str,
) -> (
    Sidecar,
    SigningKey,
    Arc<Mutex<Vec<RecordHoldingRequest>>>,
    Heard,
) {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifier = Arc::new(Verifier::holding(
        "snaptrade-1",
        KEY_ID,
        key.verifying_key(),
    ));
    let (sidecar, bus, recorded) = registered_at(roles, Some(verifier), version).await;
    let heard: Heard = Arc::default();
    let keeping = Arc::clone(&heard);
    bus.serve(LINK, move |envelope| {
        let request = LinkExternalAccountRequest::decode(&envelope.payload[..]).unwrap();
        let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
        keeping.lock().unwrap().push((request.clone(), by));
        Ok((
            "meridian.v1.ExternalAccountLink".into(),
            ExternalAccountLink {
                plugin_instance_id: request.plugin_instance_id,
                external_account_id: request.external_account_id,
                account_id: request.account_id,
            }
            .encode_to_vec(),
        ))
    });
    sidecar
        .report_external_accounts(Request::new(ReportExternalAccountsParams {
            accounts: vec![ExternalAccount {
                external_account_id: "ext-new".into(),
                ..Default::default()
            }],
        }))
        .await
        .expect("reported");
    (sidecar, key, recorded, heard)
}

/// What the dashboard signs from v15: the session's level and account sets,
/// the union, and each role's level and accounts as positions in the read
/// set.
fn by_role(
    key: &SigningKey,
    level: AccessLevel,
    read: &[&str],
    write: &[&str],
    roles: Vec<meridian_pb::v1::RoleAccess>,
) -> CallerAssertion {
    let issued = now();
    let claims = CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: "snaptrade-1".into(),
        read_account_ids: read.iter().map(|a| a.to_string()).collect(),
        write_account_ids: write.iter().map(|a| a.to_string()).collect(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-roles".into(),
        level: level as i32,
        roles,
        ..CallerClaims::default()
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: KEY_ID.into(),
    }
}

fn role(
    name: &str,
    level: AccessLevel,
    read: &[u32],
    write: &[u32],
) -> meridian_pb::v1::RoleAccess {
    meridian_pb::v1::RoleAccess {
        role: name.into(),
        level: level as i32,
        read_positions: read.to_vec(),
        write_positions: write.to_vec(),
    }
}

#[tokio::test]
async fn a_command_is_admitted_by_write_on_the_role_whose_grants_hold_it() {
    let (sidecar, key, recorded, _) = holding_roles(&["custody", "operations"], "v15").await;
    // Write on operations, read on custody: the holding is custody's.
    let operations_writer = by_role(
        &key,
        AccessLevel::Write,
        &["ACC-1"],
        &["ACC-1"],
        vec![
            role("custody", AccessLevel::Read, &[0], &[]),
            role("operations", AccessLevel::Write, &[0], &[0]),
        ],
    );
    let refused = sidecar
        .record_holding(Request::new(for_person("ext-1", operations_writer)))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "RecordHolding is custody's, and Ada holds read on custody"
    );
    assert!(
        recorded.lock().unwrap().is_empty(),
        "nothing reached the street"
    );

    // Write on custody: admitted.
    let custody_writer = by_role(
        &key,
        AccessLevel::Write,
        &["ACC-1"],
        &["ACC-1"],
        vec![
            role("custody", AccessLevel::Write, &[0], &[0]),
            role("operations", AccessLevel::Read, &[0], &[]),
        ],
    );
    sidecar
        .record_holding(Request::new(for_person("ext-1", custody_writer)))
        .await
        .expect("admitted on custody");
    assert_eq!(recorded.lock().unwrap().len(), 1);

    // Write on custody, but not on the account the row is for.
    let elsewhere = by_role(
        &key,
        AccessLevel::Write,
        &["ACC-1", "ACC-3"],
        &["ACC-1", "ACC-3"],
        vec![role("custody", AccessLevel::Write, &[1], &[1])],
    );
    let refused = sidecar
        .record_holding(Request::new(for_person("ext-1", elsewhere)))
        .await
        .unwrap_err();
    assert!(
        refused
            .message()
            .contains("write on custody without account ACC-1"),
        "{}",
        refused.message()
    );

    // Nothing on custody at all.
    let none = by_role(
        &key,
        AccessLevel::Write,
        &["ACC-1"],
        &["ACC-1"],
        vec![role("operations", AccessLevel::Write, &[0], &[0])],
    );
    let refused = sidecar
        .record_holding(Request::new(for_person("ext-1", none)))
        .await
        .unwrap_err();
    assert_eq!(
        refused.message(),
        "RecordHolding is custody's, and Ada holds nothing on custody"
    );
}

#[tokio::test]
async fn claims_with_no_per_role_entry_are_today_s_on_one_role_and_refused_on_two() {
    // A dashboard older than the sidecar signs no roles.
    let (one, key, recorded, _) = holding_roles(&["custody"], "v14").await;
    one.record_holding(Request::new(for_person(
        "ext-1",
        assertion(&key, writing(&["ACC-1"])),
    )))
    .await
    .expect("one role: the claims' sets are that role's");
    assert_eq!(recorded.lock().unwrap().len(), 1);

    let (two, key, recorded, _) = holding_roles(&["custody", "operations"], "v14").await;
    let refused = two
        .record_holding(Request::new(for_person(
            "ext-1",
            assertion(&key, writing(&["ACC-1"])),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("carry no level per role"),
        "{}",
        refused.message()
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_link_is_admitted_by_admin_on_the_role_holding_it_and_refused_naming_it() {
    let (sidecar, key, _, heard) = holding_roles(&["custody", "operations"], "v15").await;
    let operations_admin = by_role(
        &key,
        AccessLevel::Admin,
        &[],
        &[],
        vec![role("operations", AccessLevel::Admin, &[], &[])],
    );
    let refused = sidecar
        .link_external_account(Request::new(link(
            "ext-new",
            "ACC-1",
            "",
            Some(operations_admin),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "LinkExternalAccount is custody's, and Ada holds nothing on custody"
    );
    assert!(heard.lock().unwrap().is_empty());

    let custody_admin = by_role(
        &key,
        AccessLevel::Admin,
        &[],
        &[],
        vec![role("custody", AccessLevel::Admin, &[], &[])],
    );
    sidecar
        .link_external_account(Request::new(link(
            "ext-new",
            "ACC-1",
            "",
            Some(custody_admin),
        )))
        .await
        .expect("an admin of custody links");
    assert_eq!(heard.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_role_less_page_on_two_roles_is_refused_from_v15_and_serves_both_from_before() {
    let page = |roles: Vec<String>| InterfaceDeclaration {
        loopback_port: 8000,
        title: "Ops".into(),
        pages: vec![PageDeclaration {
            path: "/balances".into(),
            title: "Balances".into(),
            levels: vec![AccessLevel::Write as i32],
            roles,
        }],
    };
    let sidecar = |version: &str| {
        let _ = version;
        Sidecar::under(
            &contract(),
            Arc::new(Bus::single(
                "ops-1",
                Arc::new(MemoryBackend::new()),
                Arc::new(meridian_clock::SystemClock),
            )),
            "DEP-test",
            Identity::new("ops-1", vec!["custody".into(), "operations".into()]),
        )
    };
    async fn register(
        sc: &Sidecar,
        version: &str,
        interface: InterfaceDeclaration,
    ) -> Result<tonic::Response<meridian_pb::v1::RegisterReply>, tonic::Status> {
        sc.register(Request::new(RegisterRequest {
            schema_version: version.into(),
            interface: Some(interface),
            ..Default::default()
        }))
        .await
    }
    let at_v15 = sidecar("v15");
    let reply = register(&at_v15, "v15", page(vec![]))
        .await
        .unwrap()
        .into_inner();
    assert!(!reply.admitted);
    assert!(
        reply.refusal_reason.contains("/balances")
            && reply.refusal_reason.contains("names no role"),
        "{}",
        reply.refusal_reason
    );
    let stranger = sidecar("v15");
    let reply = register(&stranger, "v15", page(vec!["oms".into()]))
        .await
        .unwrap()
        .into_inner();
    assert!(!reply.admitted);
    assert!(
        reply.refusal_reason.contains("oms"),
        "{}",
        reply.refusal_reason
    );

    let at_v14 = sidecar("v14");
    let reply = register(&at_v14, "v14", page(vec![]))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    assert_eq!(
        at_v14.registration().unwrap().interface.unwrap().pages[0].roles,
        ["custody", "operations"],
        "built before v15, it serves every role"
    );
}
