//! One process, three surfaces, one bus.
//!
//! Every part here is tested in its own crate against its own harness, and none
//! of that says they were wired together. This is the test that would have
//! failed for as long as the street store existed and no process ran it: it
//! compiled, passed 55 tests, and was reachable from nothing.
//!
//! So what is under test is the assembly, and it is driven the way a plugin
//! drives it — through the sidecar's gRPC surface, against the contract the
//! deployment actually ships. Calling the bus directly would prove the handlers
//! registered and skip the half that decides whether a connector gets in.

use std::sync::Arc;

use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    DiagnosticBundle, DiagnosticBundleReceipt, ExternalAccountLink, ListCustodialPositionsReply,
    ListCustodialPositionsRequest, PluginConfiguration, RedeemClaimCodeReply,
};
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::plugin::v1::{
    Decimal, ExternalAccount, HoldingSide, Identifier, LinkExternalAccountParams, Money,
    RecordHoldingParams, RecordHoldingsStatementParams, ReportExternalAccountsParams,
    ResolveIdentifierParams,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    AccountScopeDelivery, CallerAssertion, CallerClaims, LinkedExternalAccount, Refusal,
    RefusalReason, RegisterRequest, WatchAccountScopeRequest,
};
use meridian_sidecar::front_door::Verifier;
use meridian_sidecar::{Contract, Identity, Sidecar};
use prost::Message;
use tokio_stream::StreamExt;
use tonic::Request;

const NOW: i64 = 1_757_376_000_000_000_000;

/// The wiring `main` does, minus the platform client and the two Postgres
/// stores, which need a network and a database.
fn runtime() -> (Arc<Bus>, Sidecar) {
    let bus = Arc::new(Bus::single(
        "runtime-test",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));

    meridian_street::service::serve(
        bus.clone(),
        Arc::new(meridian_street::MemoryStore::new()),
        bus.clock(),
    );
    meridian_instrument::service::serve_queries(
        &bus,
        Arc::new(meridian_instrument::MemoryStore::new()),
        bus.clock(),
    );
    // The conductor's part, stood in for: this plugin's external account
    // `ext-1` is linked to ACC-1, which somebody may write through it.
    bus.serve("platform.config.query.plugin-configuration", |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "custody-snaptrade-1".into(),
                links: vec![ExternalAccountLink {
                    plugin_instance_id: "custody-snaptrade-1".into(),
                    external_account_id: "ext-1".into(),
                    account_id: "ACC-1".into(),
                }],
                read_account_ids: vec!["ACC-1".into()],
                write_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });

    // Its grants are the contract's, compiled in: no table to load.
    let sidecar = Sidecar::new(
        bus.clone(),
        "DEP-test",
        Identity::new("custody-snaptrade-1", vec!["custody".to_string()]),
    );

    (bus, sidecar)
}

/// A plugin announcing its arrival. It says nothing about who it is: the
/// sidecar was launched knowing that, and the reply is where the plugin finds
/// out.
async fn admitted(sidecar: &Sidecar, expected_role: &str) {
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v2".into(),
            ..Default::default()
        }))
        .await
        .expect("register is served")
        .into_inner();
    assert!(
        reply.admitted,
        "the contract refuses the {expected_role} role: {}",
        reply.refusal_reason
    );
    assert_eq!(reply.roles, vec![expected_role.to_string()]);
}

/// A connector's whole path, through the sidecar, against one runtime.
#[tokio::test]
async fn a_connector_records_a_statement_and_a_dashboard_reads_the_position() {
    let (bus, sidecar) = runtime();
    admitted(&sidecar, "custody").await;

    // The reference side answers on the same process. Nothing is loaded, so the
    // answer is a miss — which is the honest one, and still proves the handler
    // is registered rather than absent.
    let resolved = sidecar
        .resolve_identifier(Request::new(ResolveIdentifierParams {
            as_of_ns: NOW,
            ..Default::default()
        }))
        .await
        .expect("resolve is served")
        .into_inner();
    assert!(!resolved.found);

    // The street store side, on that same bus: open a statement promising one
    // row, through the typed operations a plugin has (decisions/013).
    let opened = sidecar
        .record_holdings_statement(Request::new(RecordHoldingsStatementParams {
            source: "snaptrade".into(),
            external_statement_id: "st-1".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 1,
            ..Default::default()
        }))
        .await
        .expect("the statement is opened")
        .into_inner();
    assert!(!opened.already_recorded);

    let recorded = sidecar
        .record_holding(Request::new(RecordHoldingParams {
            statement_id: opened.statement_id,
            instrument_id: "INS-1".into(),
            // 12.5, and 2812.50 USD: the integer and the scale it was stated at.
            quantity: Some(Decimal {
                high: 0,
                low: 125,
                scale: 1,
            }),
            market_value: Some(Money {
                amount: Some(Decimal {
                    high: 0,
                    low: 281_250,
                    scale: 2,
                }),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            external_account_id: "ext-1".into(),
            side: HoldingSide::Long as i32,
            ..Default::default()
        }))
        .await
        .expect("the row is recorded against the linked account")
        .into_inner();
    assert!(recorded.resolved);

    // The dashboard reading what the connector wrote. One store behind one
    // bus, rather than two of each. The dashboard is a component and asks
    // the bus itself, as it does in a deployment (decisions/020).
    let (_, reply) = bus
        .call(
            meridian_street::service::LIST_CUSTODIAL_POSITIONS,
            "meridian.v1.ListCustodialPositionsRequest",
            ListCustodialPositionsRequest {
                account_id: "ACC-1".into(),
                include_unresolved: true,
                page_size: 100,
                cursor: String::new(),
                since: None,
            }
            .encode_to_vec(),
            None,
            None,
        )
        .await
        .expect("the street store answers");
    let listed = ListCustodialPositionsReply::decode(&reply[..]).expect("the reply decodes");

    assert_eq!(listed.positions.len(), 1);
    let position = &listed.positions[0];
    let read = |wire: Option<&meridian_pb::v1::Decimal>| {
        Exact::from_wire(wire.expect("a number"))
            .unwrap()
            .to_string()
    };
    assert_eq!(read(position.quantity.as_ref()), "12.5");
    let value = position.market_value.as_ref().expect("a market value");
    assert_eq!(read(value.amount.as_ref()), "2812.50");
    assert_eq!(value.currency_code, "USD");
}

/// W3.7 as a plugin sees it (contract v10). A set nothing matches is answered
/// with a record the deployment mints, said to be minted, and the same record
/// every time after; and a holding recorded against it is a resolved row.
#[tokio::test]
async fn a_connector_resolving_a_set_nothing_matches_is_answered_a_minted_record() {
    let (bus, sidecar) = runtime();
    admitted(&sidecar, "custody").await;

    let unknown = ResolveIdentifierParams {
        identifiers: vec![Identifier {
            scheme: "symbol".into(),
            value: "ZZTOP".into(),
            source: "snaptrade".into(),
            ..Default::default()
        }],
        as_of_ns: NOW,
        stated_currency: "USD".into(),
        ..Default::default()
    };
    let resolved = sidecar
        .resolve_identifier(Request::new(unknown.clone()))
        .await
        .expect("resolve is served")
        .into_inner();

    assert!(resolved.found);
    assert!(
        resolved.minted,
        "the plugin's mirror dropped the minted flag"
    );
    assert!(resolved.instrument_id.starts_with("LCL-"));

    let again = sidecar
        .resolve_identifier(Request::new(unknown))
        .await
        .expect("resolve is served")
        .into_inner();
    assert_eq!(again.instrument_id, resolved.instrument_id);
    assert!(!again.minted, "matched the second time, not minted");

    let opened = sidecar
        .record_holdings_statement(Request::new(RecordHoldingsStatementParams {
            source: "snaptrade".into(),
            external_statement_id: "st-placeholder".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 1,
            ..Default::default()
        }))
        .await
        .expect("the statement is opened")
        .into_inner();
    let recorded = sidecar
        .record_holding(Request::new(RecordHoldingParams {
            statement_id: opened.statement_id,
            instrument_id: resolved.instrument_id.clone(),
            quantity: Some(Decimal {
                high: 0,
                low: 5,
                scale: 0,
            }),
            market_value: Some(Money {
                amount: Some(Decimal::default()),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            external_account_id: "ext-1".into(),
            side: HoldingSide::Long as i32,
            ..Default::default()
        }))
        .await
        .expect("the row is recorded")
        .into_inner();
    assert!(recorded.resolved, "a minted record's row is a resolved row");

    let (_, reply) = bus
        .call(
            meridian_street::service::LIST_CUSTODIAL_POSITIONS,
            "meridian.v1.ListCustodialPositionsRequest",
            ListCustodialPositionsRequest {
                account_id: "ACC-1".into(),
                include_unresolved: true,
                page_size: 100,
                cursor: String::new(),
                since: None,
            }
            .encode_to_vec(),
            None,
            None,
        )
        .await
        .expect("the street store answers");
    let listed = ListCustodialPositionsReply::decode(&reply[..]).expect("the reply decodes");
    assert_eq!(listed.positions.len(), 1);
    assert_eq!(listed.positions[0].instrument_id, resolved.instrument_id);
    assert!(listed.unresolved.is_empty());
}

// ── W4.11, W6.4: a link reaches the plugin that made it ─────────────────────

/// Nothing upstream: this conductor is asked for configuration alone.
struct NoPlatform;

impl meridian_config::Upstream for NoPlatform {
    fn honour_claim_code(&self, _: &str, _: i32) -> Result<RedeemClaimCodeReply, String> {
        Err("no platform in this test".into())
    }

    fn submit_diagnostic_bundle(
        &self,
        _: &DiagnosticBundle,
    ) -> Result<DiagnosticBundleReceipt, String> {
        Err("no platform in this test".into())
    }
}

const DASHBOARD_KEY: &str = "dashboard-2026-09";

/// The stream's next delivery, or a failure rather than a wait that never
/// ends.
async fn next<S>(scope: &mut S) -> AccountScopeDelivery
where
    S: tokio_stream::Stream<Item = Result<AccountScopeDelivery, tonic::Status>> + Unpin,
{
    tokio::time::timeout(std::time::Duration::from_secs(5), scope.next())
        .await
        .expect("a delivery within five seconds")
        .expect("the stream is open")
        .expect("a delivery, not a refusal")
}

/// What the dashboard signs for a deployment admin opening the plugin's page
/// by Manage, which the page hands back to act for them (W4.9, W6.9).
fn deployment_admin(key: &SigningKey) -> CallerAssertion {
    let issued = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let claims = CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: "custody-snaptrade-1".into(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: "a-link-1".into(),
        deployment_admin: true,
        level: meridian_pb::v1::AccessLevel::Admin as i32,
        ..Default::default()
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: DASHBOARD_KEY.into(),
    }
}

/// The conductor's configuration store as it runs, on the bus the plugin's
/// sidecar is on: a link made through the plugin's own operation, acting for
/// a deployment admin, reaches that plugin's scope stream with the account's
/// name, and a row for the external account is refused with its code until
/// then. No stand-in answers for the conductor here.
#[tokio::test]
async fn a_link_made_through_the_operation_reaches_the_plugins_scope_stream() {
    // One bus, as `runtime` has: the memory bus answers a call from its own
    // handlers, and every envelope on it names the plugin, as its sidecar's
    // would, which is whom the conductor answers for.
    let bus = Arc::new(Bus::single(
        "custody-snaptrade-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let store = Arc::new(meridian_config::MemoryStore::new());
    meridian_config::serve(
        Arc::clone(&bus),
        store.clone(),
        bus.clock(),
        Arc::new(NoPlatform),
        Arc::new(meridian_config::SettingsKey::holding(&[7u8; 32])),
    );
    // The plugin has reported to the conductor, as its sidecar does (W4.8).
    meridian_config::Store::record_plugin(
        store.as_ref(),
        &meridian_config::KnownPlugin {
            plugin_instance_id: "custody-snaptrade-1".into(),
            roles: vec!["custody".into()],
            last_reported_at_ns: NOW,
        },
    )
    .unwrap();

    let key = SigningKey::from_bytes(&[9u8; 32]);
    let sidecar = Sidecar::new(
        bus,
        "DEP-test",
        Identity::new("custody-snaptrade-1", vec!["custody".to_string()]),
    )
    .with_verifier(Arc::new(Verifier::holding(
        "custody-snaptrade-1",
        DASHBOARD_KEY,
        key.verifying_key(),
    )));
    admitted(&sidecar, "custody").await;

    // Seeded on start: the stream opens with the scope and no links.
    let mut scope = sidecar
        .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
        .await
        .expect("the scope is served")
        .into_inner();
    let first = next(&mut scope).await;
    assert!(first.links.is_empty());

    // The connection reaches ext-7 (W2.8); a row for it is refused, by code.
    sidecar
        .report_external_accounts(Request::new(ReportExternalAccountsParams {
            accounts: vec![ExternalAccount {
                external_account_id: "ext-7".into(),
                name: "Individual Brokerage 1234".into(),
                venue_account_type: "Individual".into(),
                ..Default::default()
            }],
        }))
        .await
        .expect("reported");
    let refused = sidecar
        .record_holding(Request::new(RecordHoldingParams {
            statement_id: "S-1".into(),
            instrument_id: "INS-1".into(),
            quantity: Some(Decimal {
                high: 0,
                low: 1,
                scale: 0,
            }),
            external_account_id: "ext-7".into(),
            side: HoldingSide::Long as i32,
            ..Default::default()
        }))
        .await
        .expect_err("nothing links ext-7");
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    let carried = refused
        .metadata()
        .get_bin(meridian_sidecar::REFUSAL_METADATA)
        .expect("the refusal carries its code")
        .to_bytes()
        .unwrap();
    assert_eq!(
        Refusal::decode(carried.as_ref()).unwrap().reason,
        RefusalReason::ExternalAccountNotLinked as i32
    );

    // A deployment admin links it to a new account from the plugin's page.
    let linked = sidecar
        .link_external_account(Request::new(LinkExternalAccountParams {
            external_account_id: "ext-7".into(),
            new_account_name: "Individual Brokerage".into(),
            acting_for: Some(deployment_admin(&key)),
            ..Default::default()
        }))
        .await
        .expect("linked by the conductor")
        .into_inner();
    assert!(!linked.account_id.is_empty());

    // The conductor's announcement reaches the stream: the link, named, and
    // the account it grants in both scopes.
    let changed = next(&mut scope).await;
    assert_eq!(
        changed.links,
        vec![LinkedExternalAccount {
            external_account_id: "ext-7".into(),
            account_id: linked.account_id.clone(),
            account_name: "Individual Brokerage".into(),
        }]
    );
    assert!(changed.read_account_ids.contains(&linked.account_id));
    assert!(changed.write_account_ids.contains(&linked.account_id));

    // A plugin started now, on a stream of its own, has the same at once.
    let mut restarted = sidecar
        .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(next(&mut restarted).await.links, changed.links);
}

/// The contract is what grants, so a revision of it that took away what a
/// role's work needs is a plugin admitted and then refused on its first useful
/// call. Held here against the contract this runtime was built with.
#[test]
fn the_contract_admits_each_role_to_exactly_its_own_work() {
    let contract = Contract::embedded();
    let roles = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

    let custody = contract.grants_for(&roles(&["custody"])).unwrap();
    assert!(custody.may_publish(meridian_street::service::RECORD_STATEMENT));
    assert!(custody.may_publish(meridian_street::service::RECORD_HOLDING));
    assert!(custody.may_publish("platform.custody.custody-snaptrade-1.event.sync-status"));
    // A connector states what it holds; it does not announce that a position
    // moved. That is the street store's to say.
    assert!(!custody.may_publish(meridian_street::service::CUSTODIAL_POSITION_UPDATED));

    // The dashboard is a component that reaches no account's data (ruling
    // 21): from contract v15 it neither reads nor hears the street's
    // positions, and nothing it does writes to the street store.
    let dashboard = contract.component("dashboard");
    assert!(!dashboard.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));
    assert!(!dashboard.may_subscribe(meridian_street::service::CUSTODIAL_POSITION_UPDATED));
    assert!(!dashboard.may_publish(meridian_street::service::RECORD_HOLDING));

    // The book (W9, contract v8): operations writes it; portfolio, reporting,
    // compliance and oms read and hear it and write nothing; oms hears no
    // figures (open point 12); none but operations reads the street.
    let operations = contract.grants_for(&roles(&["operations"])).unwrap();
    assert!(operations.may_publish(meridian_bor::service::RECORD_OPENING_BALANCE));
    assert!(operations.may_publish(meridian_bor::service::RESOLVE_BREAK));
    assert!(operations.may_publish(meridian_bor::service::CLOSE_BREAKS_AS_CLEARED));
    assert!(operations.may_publish(meridian_bor::service::RECORD_ENCUMBRANCES));
    assert!(operations.may_subscribe(meridian_bor::service::BREAK_CHANGED));
    let oms = contract.grants_for(&roles(&["oms"])).unwrap();
    assert!(oms.may_publish(meridian_bor::service::LIST_POSITIONS));
    assert!(oms.may_subscribe(meridian_bor::service::POSITION_CHANGED));
    assert!(!oms.may_subscribe(meridian_bor::service::ACCOUNT_FIGURES_RECORDED));
    assert!(!oms.may_publish(meridian_bor::service::RECORD_BREAK));
    let portfolio = contract.grants_for(&roles(&["portfolio"])).unwrap();
    assert!(!portfolio.may_publish(meridian_bor::service::RESOLVE_BREAK));
    assert!(!portfolio.may_publish(meridian_street::service::LIST_CUSTODIAL_POSITIONS));
    let book = contract.component("bor");
    assert!(book.may_publish(meridian_bor::service::POSITION_CHANGED));
    assert!(book.may_subscribe(meridian_bor::service::RECORD_OPENING_BALANCE));
    // The attributes are set from the dashboard, for a deployment admin.
    assert!(dashboard.may_publish(meridian_bor::service::SET_ACCOUNT_ATTRIBUTE));
    assert!(!operations.may_publish(meridian_bor::service::SET_ACCOUNT_ATTRIBUTE));

    // Several roles hold the union; a role the contract gives nothing holds
    // nothing, and adds nothing to another.
    let with_confirmation = contract
        .grants_for(&roles(&["custody", "confirmation"]))
        .unwrap();
    assert_eq!(with_confirmation, custody);

    // The lake's 1b (contract v19): ems reads and hears trades and quotes,
    // and the datasets it may read; reporting hears neither (the plan's Q9
    // and Q37), and resolves an identifier, read-only.
    let ems = contract.grants_for(&roles(&["ems"])).unwrap();
    assert!(ems.may_publish(meridian_lake::service::LIST_TRADES));
    assert!(ems.may_publish(meridian_lake::service::LIST_QUOTES));
    assert!(ems.may_publish(meridian_lake::service::LIST_DATASETS));
    assert!(ems.may_subscribe("platform.lake.{dataset}.event.trades-recorded"));
    assert!(!ems.may_publish(meridian_lake::service::LIST_PRICES));
    // portfolio and oms hear neither (ruled 2026-10-10: portfolio shapes
    // orders and oms aggregates them; signal and ems take high-resolution
    // data).
    for role in ["portfolio", "oms"] {
        let reader = contract.grants_for(&roles(&[role])).unwrap();
        assert!(
            !reader.may_publish(meridian_lake::service::LIST_TRADES),
            "{role}"
        );
        assert!(
            !reader.may_subscribe("platform.lake.{dataset}.event.trades-recorded"),
            "{role}"
        );
    }
    let reporting = contract.grants_for(&roles(&["reporting"])).unwrap();
    assert!(reporting.may_publish("platform.reference.query.resolve-identifier"));
    assert!(!reporting.may_publish(meridian_lake::service::LIST_TRADES));
    assert!(!reporting.may_subscribe("platform.lake.{dataset}.event.trades-recorded"));
    let data = contract.grants_for(&roles(&["data"])).unwrap();
    assert!(data.may_publish(meridian_lake::service::RECORD_TRADES));
    assert!(data.may_publish(meridian_lake::service::RECORD_QUOTES));

    // Denial is by refusal for a name that is not a role, never by granting it
    // nothing and letting it register.
    assert!(contract.grants_for(&roles(&["not-a-role"])).is_err());
    assert!(contract.grants_for(&roles(&["street"])).is_err());
}

// ── W5.20: components say what they run, inward ─────────────────────────────

#[tokio::test]
async fn a_components_report_reaches_the_one_holding_the_key() {
    // The street store holds no key, so what it runs reaches the platform only by
    // way of the instrument store. This is that path, without a platform: the street store
    // publishes, and what the instrument store would send carries it.
    use meridian_runtime::{collect_inward, report_inward_forever, COMPONENT_REPORT_TOPIC};

    let bus = Arc::new(Bus::single(
        "instrument-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let heard = collect_inward(Arc::clone(&bus));

    let publishing = Arc::clone(&bus);
    tokio::spawn(async move { report_inward_forever(publishing, "street", 2).await });

    // The first report goes out immediately; the interval is for the ones
    // after it, which is what makes a restart visible promptly.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(report) = heard.lock().unwrap().get("street") {
            assert_eq!(report.schema_version, 2);
            assert_eq!(report.health, "COMPONENT_HEALTH_SERVING");
            // The build's own, never a variable's: unset here, as the chart
            // leaves it, a component said `0.1.0` whatever it ran.
            assert_eq!(report.version, meridian_runtime::VERSION);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing arrived on {COMPONENT_REPORT_TOPIC}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The dashboard's assertion for an `ems`'s admin on its page at Manage, the
/// `ems` role's admin (W4.9, W11.22).
fn ems_admin(key: &SigningKey, instance: &str) -> CallerAssertion {
    let issued = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let claims = CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: instance.into(),
        issued_at_ns: issued,
        expires_at_ns: issued + 60_000_000_000,
        assertion_id: format!("a-map-{issued}"),
        level: meridian_pb::v1::AccessLevel::Admin as i32,
        roles: vec![meridian_pb::v1::RoleAccess {
            role: "ems".into(),
            level: meridian_pb::v1::AccessLevel::Admin as i32,
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec();
    CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: DASHBOARD_KEY.into(),
    }
}

/// Contract v22 (W11.21, W11.22, W4.11): the conductor's configuration store
/// as it runs. A deployment admin makes a connectivity plugin available and
/// the ems's admin links it; the ems maps an account through its own
/// operation, acting for its admin; the ems's scope stream carries its
/// plugin and its map; and the connectivity plugin, started on the same
/// store, has its link and the mapped account as its scope, and a venue's
/// order it records takes that account from the map. No stand-in answers for
/// the conductor.
#[tokio::test]
async fn a_connectivity_plugin_linked_to_its_ems_reaches_the_accounts_the_ems_mapped() {
    let store = Arc::new(meridian_config::MemoryStore::new());
    let serve = |instance: &str| {
        let bus = Arc::new(Bus::single(
            instance,
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
        meridian_config::serve(
            Arc::clone(&bus),
            store.clone(),
            bus.clock(),
            Arc::new(NoPlatform),
            Arc::new(meridian_config::SettingsKey::holding(&[7u8; 32])),
        );
        bus
    };
    for (instance, role) in [("ems-1", "ems"), ("fix-1", "connectivity")] {
        meridian_config::Store::record_plugin(
            store.as_ref(),
            &meridian_config::KnownPlugin {
                plugin_instance_id: instance.into(),
                roles: vec![role.into()],
                last_reported_at_ns: NOW,
            },
        )
        .unwrap();
    }
    let ems_bus = serve("ems-1");
    // An account the deployment admin granted the ems (W6.30).
    let (_, defined) = ems_bus
        .call_for(
            "platform.config.command.define-account",
            "meridian.v1.DefineAccountRequest",
            meridian_domain::v1::DefineAccountRequest {
                name: "Desk one".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            None,
            "local|ada",
        )
        .await
        .unwrap();
    let account = meridian_domain::v1::AccountRecord::decode(&defined[..]).unwrap();
    ems_bus
        .call_for(
            "platform.config.command.set-linkable-accounts",
            "meridian.v1.SetLinkableAccountsRequest",
            meridian_domain::v1::SetLinkableAccountsRequest {
                plugin_instance_id: "ems-1".into(),
                grant_account_ids: vec![account.account_id.clone()],
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            None,
            "local|ada",
        )
        .await
        .unwrap();
    // At the page: available, then linked with its users.
    for (topic, kind, payload) in [
        (
            "platform.config.command.set-connectivity-available",
            "meridian.v1.SetConnectivityAvailableRequest",
            meridian_domain::v1::SetConnectivityAvailableRequest {
                connectivity_instance_id: "fix-1".into(),
                available: true,
                note: String::new(),
            }
            .encode_to_vec(),
        ),
        (
            "platform.config.command.link-connectivity",
            "meridian.v1.LinkConnectivityRequest",
            meridian_domain::v1::LinkConnectivityRequest {
                connectivity_instance_id: "fix-1".into(),
                ems_instance_id: "ems-1".into(),
                users: vec![meridian_pb::v1::ConnectivityUser {
                    user: Some(meridian_pb::v1::connectivity_user::User::Subject(
                        "local|ada".into(),
                    )),
                }],
                note: String::new(),
            }
            .encode_to_vec(),
        ),
    ] {
        ems_bus
            .call_for(topic, kind, payload, None, None, "local|ada")
            .await
            .unwrap();
    }

    let key = SigningKey::from_bytes(&[9u8; 32]);
    let ems = Sidecar::new(
        ems_bus,
        "DEP-test",
        Identity::new("ems-1", vec!["ems".to_string()]),
    )
    .with_verifier(Arc::new(Verifier::holding(
        "ems-1",
        DASHBOARD_KEY,
        key.verifying_key(),
    )));
    admitted(&ems, "ems").await;
    let mut scope = ems
        .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
        .await
        .unwrap()
        .into_inner();
    let first = next(&mut scope).await;
    assert_eq!(first.connectivity_links.len(), 1);
    assert_eq!(
        first.connectivity_links[0].connectivity_instance_id,
        "fix-1"
    );

    // The ems maps the account, acting for its admin.
    ems.map_venue_account(Request::new(
        meridian_pb::plugin::v1::MapVenueAccountParams {
            connectivity_instance_id: "fix-1".into(),
            venue_account_code: "ACCT-7".into(),
            account_id: account.account_id.clone(),
            acting_for: Some(ems_admin(&key, "ems-1")),
        },
    ))
    .await
    .expect("mapped by the conductor");
    let changed = next(&mut scope).await;
    let map = changed.venue_account_map.unwrap();
    assert_eq!(map.mappings.len(), 1);
    assert_eq!(map.mappings[0].account_id, account.account_id);

    // The connectivity plugin, on the same store: its link, and the mapped
    // account its scope.
    let fix_bus = serve("fix-1");
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let keeping = Arc::clone(&recorded);
    fix_bus.serve("platform.book.command.record-order", move |envelope| {
        let order = meridian_domain::v1::RecordOrderRequest::decode(&envelope.payload[..]).unwrap();
        keeping.lock().unwrap().push(order.account_id);
        Ok((
            "meridian.v1.OrderReply".into(),
            meridian_domain::v1::OrderReply::default().encode_to_vec(),
        ))
    });
    let fix = Sidecar::new(
        fix_bus,
        "DEP-test",
        Identity::new("fix-1", vec!["connectivity".to_string()]),
    );
    admitted(&fix, "connectivity").await;
    let mut scope = fix
        .watch_account_scope(Request::new(WatchAccountScopeRequest {}))
        .await
        .unwrap()
        .into_inner();
    let told = next(&mut scope).await;
    assert_eq!(told.connectivity_link.unwrap().ems_instance_id, "ems-1");
    assert_eq!(told.read_account_ids, vec![account.account_id.clone()]);

    fix.record_order(Request::new(meridian_pb::plugin::v1::RecordOrderParams {
        venue_account_code: "ACCT-7".into(),
        venue_order_id: "V-1".into(),
        quantity: Some(Decimal {
            high: 0,
            low: 5,
            scale: 0,
        }),
        ..Default::default()
    }))
    .await
    .expect("the venue's order is sent");
    assert_eq!(recorded.lock().unwrap().as_slice(), [account.account_id]);
}

/// Contract v22 (W3.16 to W3.20): a plugin works out a date through its
/// sidecar from what the instrument store keeps -- nothing held, then the
/// conductor's pull kept and announced, then the date with its basis.
#[tokio::test]
async fn a_plugin_asks_for_a_date_and_is_answered_from_the_calendars_pulled() {
    use meridian_pb::plugin::v1::{BusinessDayInputs, ComputeDateParams, ReadCalendarParams};
    let bus = Arc::new(Bus::single(
        "portfolio-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let mut applied = bus.subscribe("platform.reference.event.calendar-applied");
    let mut missing = bus.subscribe("platform.reference.event.calendar-missing");
    tokio::spawn(
        meridian_instrument::InstrumentService::new(
            Arc::clone(&bus),
            Arc::new(meridian_instrument::MemoryStore::new()),
            bus.clock(),
        )
        .start(),
    );
    bus.serve("platform.config.query.plugin-configuration", |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::new(
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("portfolio-1", vec!["portfolio".to_string()]),
    );
    admitted(&sidecar, "portfolio").await;
    const USD: &str = "CAL-01JAE000000000000000USD1";
    let ask = || {
        sidecar.compute_date(Request::new(ComputeDateParams {
            inputs: Some(
                meridian_pb::plugin::v1::compute_date_params::Inputs::BusinessDay(
                    BusinessDayInputs {
                        date: "2026-10-12".into(),
                        calendars: vec![USD.into()],
                    },
                ),
            ),
            as_of_ns: 0,
        }))
    };
    let first = ask().await.unwrap().into_inner();
    assert_eq!(
        first.outcome,
        meridian_pb::plugin::v1::DateOutcome::CalendarNotHeld as i32
    );
    let reported = tokio::time::timeout(std::time::Duration::from_secs(5), missing.recv())
        .await
        .unwrap()
        .unwrap();
    let reported =
        meridian_domain::v1::MissingCalendarDetectedEvent::decode(&reported.envelope.payload[..])
            .unwrap();
    assert_eq!(reported.subject.unwrap().calendar_id, USD);

    // The conductor's pull, as it hands it on.
    bus.publish(
        "platform.reference.event.calendar-pulled",
        "meridian.v1.PullCalendarReply",
        meridian_domain::v1::PullCalendarReply {
            calendars: vec![meridian_domain::v1::CalendarRecord {
                calendar_id: USD.into(),
                name: "US dollar".into(),
                weekends: vec![meridian_domain::v1::CalendarWeekend {
                    weekdays: vec![
                        meridian_domain::v1::Weekday::Saturday as i32,
                        meridian_domain::v1::Weekday::Sunday as i32,
                    ],
                    valid_from_date: "2026-01-01".into(),
                }],
                days: vec![meridian_domain::v1::CalendarDay {
                    date: "2026-10-12".into(),
                    kind: meridian_domain::v1::CalendarDayKind::Closed as i32,
                    name: "Columbus Day".into(),
                    source: "fed-holidays".into(),
                }],
                coverage: Some(meridian_domain::v1::CalendarCoverage {
                    from_date: "2026-01-01".into(),
                    to_date: "2027-12-31".into(),
                }),
                version: 1,
                ..Default::default()
            }],
            rules: Vec::new(),
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), applied.recv())
        .await
        .expect("the version kept is announced")
        .unwrap();

    let answer = ask().await.unwrap().into_inner();
    assert_eq!(
        answer.outcome,
        meridian_pb::plugin::v1::DateOutcome::Computed as i32
    );
    assert!(!answer.business_day, "Columbus Day is closed");
    assert_eq!(answer.basis.unwrap().calendars[0].version, 1);
    let read = sidecar
        .read_calendar(Request::new(ReadCalendarParams {
            calendar_id: USD.into(),
            from_date: "2026-10-01".into(),
            to_date: "2026-10-31".into(),
            as_of_ns: 0,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(read.found);
    assert_eq!(read.days[0].name, "Columbus Day");
}
