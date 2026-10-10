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
    let with_ems = contract.grants_for(&roles(&["custody", "ems"])).unwrap();
    assert_eq!(with_ems, custody);

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
