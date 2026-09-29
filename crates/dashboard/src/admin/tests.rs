use std::sync::Mutex;

use axum::body::{to_bytes, Body};
use axum::http::header::COOKIE;
use axum::http::Request;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccountRecord, AccountState, ExternalAccount, ExternalAccountLink, ExternalAccountsEvent,
    Permission, PluginSettingValue, RedeemClaimCodeReply, SetPluginSettingsRequest, SyncState,
    SyncStatusEvent, UnlinkedExternalAccount, UnlinkedExternalAccountsEvent,
};
use meridian_pb::v1::{SettingDeclaration, SettingType};
use tower::ServiceExt;

use super::*;
use crate::clock::Clock;
use crate::records::RecordsCache;
use crate::session::Sessions;
use crate::web::{router, SESSION_COOKIE};

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

const T0: i64 = 1_790_380_800_000_000_000;
const ADA: &str = "https://directory.example.org|8812";

/// What the fake conductor was asked, and on whose behalf.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn admin_records() -> AccessRecords {
    AccessRecords {
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec![ADA.into()],
        }],
        permissions: vec![Permission {
            permission_id: "P-1".into(),
            user_group_id: "UG-1".into(),
            account_group_id: String::new(),
            access_group_id: DEPLOYMENT_ADMIN.into(),
        }],
        ..Default::default()
    }
}

struct Harness {
    app: Arc<App>,
    seen: Seen,
    session: String,
    form_token: String,
}

/// A dashboard whose records are `records`, with Ada signed in, and a fake
/// conductor answering define-account and redeem-claim-code.
fn harness(records: AccessRecords, refuse_with: Option<&'static str>) -> Harness {
    let bus = Arc::new(Bus::single("dashboard-1", Arc::new(MemoryBackend::new())));
    let seen: Seen = Arc::default();
    for topic in [
        "platform.config.command.define-account",
        "platform.config.command.redeem-claim-code",
    ] {
        let seen = Arc::clone(&seen);
        bus.serve(topic, move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            seen.lock()
                .unwrap()
                .push((meta.topic.clone(), meta.acting_for_subject.clone()));
            if let Some(sentence) = refuse_with {
                return Err(sentence.to_string());
            }
            if meta.topic.ends_with("redeem-claim-code") {
                return Ok((
                    "".into(),
                    RedeemClaimCodeReply {
                        redeemed: true,
                        refusal_reason: String::new(),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ));
            }
            Ok((
                "".into(),
                AccountRecord {
                    account_id: "ACC-1".into(),
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        });
    }
    let cache = Arc::new(RecordsCache::default());
    cache.store(records, T0);
    let sessions = Arc::new(Sessions::default());
    let session = sessions.start(ADA, "Ada", vec![], T0);
    let form_token = sessions.find(&session, T0).unwrap().form_token;
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions,
        terminals: Arc::new(crate::terminal::Terminals::default()),
        clock: Arc::new(At(T0)),
        bus,
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins: None,
        registry: None,
        custody: Arc::default(),
    });
    Harness {
        app,
        seen,
        session,
        form_token,
    }
}

async fn send(h: &Harness, request: Request<Body>) -> (StatusCode, String) {
    let response = router(Arc::clone(&h.app)).oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn get(h: &Harness, path: &str, signed_in: bool) -> Request<Body> {
    let mut request = Request::get(path);
    if signed_in {
        request = request.header(COOKIE, format!("__Host-{SESSION_COOKIE}={}", h.session));
    }
    request.body(Body::empty()).unwrap()
}

fn post(h: &Harness, path: &str, form: &str) -> Request<Body> {
    Request::post(path)
        .header(COOKIE, format!("__Host-{SESSION_COOKIE}={}", h.session))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

#[tokio::test]
async fn the_admin_pages_are_for_deployment_admins_only() {
    let h = harness(AccessRecords::default(), None);
    assert_eq!(
        send(&h, get(&h, "/admin", false)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&h, get(&h, "/admin", true)).await.0,
        StatusCode::FORBIDDEN
    );
    let token = h.form_token.clone();
    let (status, _) = send(
        &h,
        post(
            &h,
            "/admin/accounts",
            &format!("form_token={token}&name=Growth"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        h.seen.lock().unwrap().is_empty(),
        "a refused person's command is never sent"
    );

    let admin = harness(admin_records(), None);
    assert_eq!(
        send(&admin, get(&admin, "/admin", true)).await.0,
        StatusCode::OK
    );
}

/// The table in `body` whose class list includes `class`.
fn table<'a>(body: &'a str, class: &str) -> &'a str {
    body.split(&format!("<table class=\"list {class}\">"))
        .nth(1)
        .and_then(|rest| rest.split("</table>").next())
        .unwrap_or_else(|| panic!("no {class} table in the page"))
}

#[tokio::test]
async fn reported_accounts_wait_beside_the_link_action_until_one_is_made() {
    // W2.8 and W6.4: SNAP-1 is linked, SNAP-2 is not, and st-9902 was only
    // ever refused. The two without a link wait, each with its own link.
    let mut records = admin_records();
    records.accounts = vec![AccountRecord {
        account_id: "ACC-1".into(),
        name: "Growth".into(),
        state: AccountState::Open as i32,
        created_at_ns: T0,
    }];
    records.links = vec![ExternalAccountLink {
        plugin_instance_id: "snaptrade-1".into(),
        external_account_id: "SNAP-1".into(),
        account_id: "ACC-1".into(),
    }];
    let h = harness(records, None);
    h.app.custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: vec![
                ExternalAccount {
                    external_account_id: "SNAP-1".into(),
                    name: "Individual Brokerage 1234".into(),
                    venue_account_type: "Individual".into(),
                },
                ExternalAccount {
                    external_account_id: "SNAP-2".into(),
                    name: "Roth IRA 5678".into(),
                    venue_account_type: "Roth IRA".into(),
                },
            ],
        },
    );
    h.app.custody.hear_refused(UnlinkedExternalAccountsEvent {
        plugin_instance_id: "snaptrade-1".into(),
        accounts: vec![UnlinkedExternalAccount {
            external_account_id: "st-9902".into(),
            refused_rows: 12,
            ..Default::default()
        }],
    });

    let (status, body) = send(&h, get(&h, "/admin", true)).await;
    assert_eq!(status, StatusCode::OK);
    let waiting = table(&body, "unlinked");
    assert!(waiting.contains("data-id=\"SNAP-2\""), "{waiting}");
    assert!(waiting.contains("Roth IRA 5678") && waiting.contains(">Roth IRA<"));
    assert!(waiting.contains("data-id=\"st-9902\"") && waiting.contains("12 rows refused"));
    assert!(
        !waiting.contains("data-id=\"SNAP-1\""),
        "a linked account is not waiting"
    );
    // Its own link, which posts what W6.4 always took.
    assert!(body.contains("<input type=\"hidden\" name=\"external_account_id\" value=\"SNAP-2\">"));
    assert!(table(&body, "linked").contains("data-account=\"ACC-1\""));
}

#[tokio::test]
async fn an_unlinked_accounts_sync_state_is_shown_beside_it() {
    // Ruled 2026-09-28: sync status describes the connection, not recorded
    // data, so it arrives before a link and says whether one is worth making.
    let h = harness(admin_records(), None);
    h.app.custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: vec![ExternalAccount {
                external_account_id: "SNAP-9".into(),
                name: "Hidden Brokerage".into(),
                venue_account_type: "Individual".into(),
            }],
        },
    );
    h.app.custody.hear_sync(
        "snaptrade-1",
        SyncStatusEvent {
            external_account_id: "SNAP-9".into(),
            account_id: String::new(),
            state: SyncState::HoldingsUnavailable as i32,
            ..Default::default()
        },
    );

    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    let waiting = table(&body, "unlinked");
    assert!(waiting.contains("data-id=\"SNAP-9\""));
    assert!(waiting.contains("Holdings unavailable"), "{waiting}");
    assert!(waiting.contains("Connect the account another way"));
    assert!(table(&body, "sync").contains("not linked"));
}

#[tokio::test]
async fn a_sync_state_is_shown_with_what_to_do_about_it() {
    let h = harness(admin_records(), None);
    for (external, state) in [
        ("SNAP-1", SyncState::NeedsSignIn),
        ("SNAP-2", SyncState::Disabled),
        ("SNAP-3", SyncState::Stale),
        ("SNAP-4", SyncState::DelayedByDesign),
        ("SNAP-5", SyncState::Current),
        ("SNAP-6", SyncState::HoldingsUnavailable),
    ] {
        h.app.custody.hear_sync(
            "snaptrade-1",
            SyncStatusEvent {
                external_account_id: external.into(),
                account_id: "ACC-1".into(),
                state: state as i32,
                holdings_as_of_ns: 1_757_289_600_000_000_000,
                ..Default::default()
            },
        );
    }

    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    let sync = table(&body, "sync");
    for said in [
        "Needs sign-in",
        "Sign in again at the venue",
        "Disabled",
        "Re-enable the connection",
        "Stale",
        "Wait: the connection is serving what it last read",
        "Delayed by design",
        "Expected: this venue reports late",
        "Current",
        "Holdings unavailable",
        "Connect the account another way, or through another venue",
        "2025-09-08 00:00 UTC",
    ] {
        assert!(sync.contains(said), "{said} is not shown: {sync}");
    }
}

#[tokio::test]
async fn a_change_is_sent_on_the_admins_behalf() {
    let h = harness(admin_records(), None);
    let form = format!("form_token={}&account_id=&name=Growth", h.form_token);
    let (status, _) = send(&h, post(&h, "/admin/accounts", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        h.seen.lock().unwrap().as_slice(),
        [(
            "platform.config.command.define-account".to_string(),
            ADA.to_string()
        )]
    );
}

#[tokio::test]
async fn a_form_without_the_sessions_token_is_refused_and_sends_nothing() {
    let h = harness(admin_records(), None);
    let (status, _) = send(
        &h,
        post(&h, "/admin/accounts", "form_token=forged&name=Growth"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_stores_refusal_is_shown_as_it_is() {
    let h = harness(
        admin_records(),
        Some("deployment admin is built in, and cannot be edited"),
    );
    let form = format!("form_token={}&name=Growth", h.form_token);
    let (status, body) = send(&h, post(&h, "/admin/accounts", &form)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("deployment admin is built in, and cannot be edited"),
        "{body}"
    );
}

#[tokio::test]
async fn the_claim_page_is_open_only_while_nobody_administers() {
    let open = harness(AccessRecords::default(), None);
    assert_eq!(
        send(&open, get(&open, "/claim", true)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&open, get(&open, "/claim", false)).await.0,
        StatusCode::UNAUTHORIZED
    );

    let claimed = harness(admin_records(), None);
    assert_eq!(
        send(&claimed, get(&claimed, "/claim", true)).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn a_claim_is_redeemed_on_the_signed_in_persons_behalf() {
    let h = harness(AccessRecords::default(), None);
    let form = format!("form_token={}&code=7KQ2-MX4P-9RTD", h.form_token);
    let (status, _) = send(&h, post(&h, "/claim", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        h.seen.lock().unwrap().as_slice(),
        [(
            "platform.config.command.redeem-claim-code".to_string(),
            ADA.to_string()
        )]
    );
}

#[test]
fn a_directory_group_is_a_whole_line_so_a_distinguished_name_survives() {
    let fields: Fields = [(
        "directory_groups".to_string(),
        "cn=traders,ou=groups,dc=firm,dc=com\r\n\n  ops  \n".to_string(),
    )]
    .into();
    assert_eq!(
        lines(&fields, "directory_groups"),
        ["cn=traders,ou=groups,dc=firm,dc=com", "ops"]
    );
}

#[test]
fn entries_are_plugin_tag_and_level_one_per_line() {
    let parsed = parse_entries("oms-1 oms write\n\n snaptrade-1 custody read ").unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].level, AccessLevel::Write as i32);
    assert_eq!(parsed[1].plugin_instance_id, "snaptrade-1");
    assert!(parse_entries("oms-1 oms admin")
        .unwrap_err()
        .contains("not read or write"));
    assert!(parse_entries("oms-1 write").is_err());
}

// ── A plugin instance's settings (W6.11) ────────────────────────────────────

/// Obviously not a real credential, and long enough to find in a page.
const SECRET: &str = "sk-test-not-a-real-key-7f3a";

fn declared(name: &str, kind: SettingType, required: bool, secret: bool) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: kind as i32,
        required,
        secret,
        description: format!("What {name} is <for>."),
    }
}

/// SnapTrade's shape: two secrets, one of them set, a number and a switch.
fn snaptrade() -> PluginSettingsRecord {
    PluginSettingsRecord {
        plugin_instance_id: "snaptrade-1".into(),
        values: vec![PluginSettingValue {
            name: "poll_seconds".into(),
            value: "900".into(),
        }],
        secrets_set: vec!["snaptrade_client_id".into()],
        updated_at_ns: T0,
        declared_settings: vec![
            declared("snaptrade_client_id", SettingType::String, true, true),
            declared("snaptrade_consumer_key", SettingType::String, true, true),
            declared("poll_seconds", SettingType::Integer, false, false),
            declared("synthetic", SettingType::Boolean, false, false),
        ],
    }
}

fn with_settings() -> AccessRecords {
    let mut records = admin_records();
    records.plugin_settings = vec![snaptrade()];
    records
}

type Asked = Arc<Mutex<Vec<(SetPluginSettingsRequest, String)>>>;

/// A conductor answering set-plugin-settings, or refusing with `refuse_with`.
fn conductor_setting(h: &Harness, refuse_with: Option<&'static str>) -> Asked {
    let asked: Asked = Arc::default();
    let keeping = Arc::clone(&asked);
    h.app.bus.serve(
        "platform.config.command.set-plugin-settings",
        move |envelope| {
            let request = SetPluginSettingsRequest::decode(&envelope.payload[..]).unwrap();
            let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
            keeping.lock().unwrap().push((request, by));
            match refuse_with {
                Some(sentence) => Err(sentence.to_string()),
                None => Ok(("".into(), snaptrade().encode_to_vec())),
            }
        },
    );
    asked
}

const SETTINGS: &str = "/admin/plugins/snaptrade-1/settings";

#[tokio::test]
async fn a_plugins_settings_form_is_built_from_what_it_declared_for_admins_alone() {
    let h = harness(with_settings(), None);
    let (status, body) = send(&h, get(&h, SETTINGS, true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A secret is a password field, always empty, saying whether it is set.
    let client = body
        .split("data-setting=\"snaptrade_client_id\"")
        .nth(1)
        .unwrap()
        .split("</div>")
        .next()
        .unwrap();
    assert!(client.contains("type=\"password\" name=\"secret.snaptrade_client_id\" value=\"\""));
    assert!(client.contains(">set<") && client.contains("required"));
    assert!(client.contains("name=\"clear.snaptrade_client_id\""));
    let consumer = body
        .split("data-setting=\"snaptrade_consumer_key\"")
        .nth(1)
        .unwrap()
        .split("</div>")
        .next()
        .unwrap();
    assert!(consumer.contains(">not set<") && !consumer.contains("clear."));

    // What is not secret, as it stands, in a field its type takes.
    assert!(body
        .contains("<input type=\"number\" step=\"1\" name=\"value.poll_seconds\" value=\"900\">"));
    assert!(body.contains("<select name=\"value.synthetic\"><option value=\"\" selected>"));
    assert!(
        body.contains("What poll_seconds is &lt;for&gt;."),
        "escaped"
    );
    assert!(
        body.contains(&format!("value=\"{}\"", h.form_token)),
        "the form token"
    );

    // And the overview lists it, with the required secret it still needs.
    let (_, overview) = send(&h, get(&h, "/admin", true)).await;
    let listed = table(&overview, "settings");
    assert!(listed.contains("data-id=\"snaptrade-1\""));
    assert!(listed.contains("needs snaptrade_consumer_key"), "{listed}");
    assert!(listed.contains(&format!("href=\"{SETTINGS}\"")));

    let unknown = send(&h, get(&h, "/admin/plugins/ghost-1/settings", true)).await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);

    let mut not_admin = with_settings();
    not_admin.permissions.clear();
    let nobody = harness(not_admin, None);
    assert_eq!(
        send(&nobody, get(&nobody, SETTINGS, true)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&nobody, get(&nobody, SETTINGS, false)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_secret_typed_in_goes_to_the_conductor_and_never_back_into_a_page() {
    let h = harness(with_settings(), None);
    let asked = conductor_setting(&h, None);
    let form = format!(
        "form_token={}&secret.snaptrade_client_id=&secret.snaptrade_consumer_key={SECRET}\
         &value.poll_seconds=900&value.synthetic=true",
        h.form_token
    );
    let response = router(Arc::clone(&h.app))
        .oneshot(post(&h, SETTINGS, &form))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        format!("{SETTINGS}?saved=1").as_str()
    );

    let (request, by) = asked.lock().unwrap()[0].clone();
    assert_eq!(by, ADA, "on her behalf");
    assert_eq!(request.plugin_instance_id, "snaptrade-1");
    // Only what changed: the set secret left empty is left alone, and the
    // number is what it was.
    let sent: Vec<(&str, &str)> = request
        .values
        .iter()
        .map(|v| (v.name.as_str(), v.value.as_str()))
        .collect();
    assert_eq!(
        sent,
        [("snaptrade_consumer_key", SECRET), ("synthetic", "true")]
    );
    assert!(request.cleared.is_empty());

    let (_, page) = send(&h, get(&h, &format!("{SETTINGS}?saved=1"), true)).await;
    assert!(page.contains("Saved."));
    assert!(!page.contains(SECRET), "never shown");

    // Refused, the page says the conductor's sentence and not what was typed.
    let refused = harness(with_settings(), None);
    conductor_setting(
        &refused,
        Some("setting snaptrade_consumer_key could not be stored"),
    );
    let form = format!(
        "form_token={}&secret.snaptrade_consumer_key={SECRET}",
        refused.form_token
    );
    let (status, body) = send(&refused, post(&refused, SETTINGS, &form)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be stored"), "{body}");
    assert!(!body.contains(SECRET));
}

#[tokio::test]
async fn a_settings_form_without_the_sessions_token_sends_nothing() {
    let h = harness(with_settings(), None);
    let asked = conductor_setting(&h, None);
    let form = format!("form_token=forged&secret.snaptrade_consumer_key={SECRET}");
    let (status, body) = send(&h, post(&h, SETTINGS, &form)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.contains(SECRET));
    assert!(asked.lock().unwrap().is_empty());

    // And a form that changes nothing sends nothing, and says so.
    let form = format!("form_token={}&value.poll_seconds=900", h.form_token);
    let response = router(Arc::clone(&h.app))
        .oneshot(post(&h, SETTINGS, &form))
        .await
        .unwrap();
    assert_eq!(
        response.headers()["location"],
        format!("{SETTINGS}?saved=none").as_str()
    );
    assert!(asked.lock().unwrap().is_empty());
}

#[test]
fn a_form_asks_for_what_changed_and_clears_only_what_it_was_told_to() {
    let record = snaptrade();
    let form = |pairs: &[(&str, &str)]| -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert!(settings::request(&record, &form(&[("value.poll_seconds", "900")])).is_none());

    let emptied = settings::request(&record, &form(&[("value.poll_seconds", "")])).unwrap();
    assert_eq!(emptied.cleared, ["poll_seconds"]);

    let cleared = settings::request(
        &record,
        &form(&[
            ("value.poll_seconds", "900"),
            ("clear.snaptrade_client_id", "on"),
            ("clear.snaptrade_consumer_key", "on"),
        ]),
    )
    .unwrap();
    assert_eq!(
        cleared.cleared,
        ["snaptrade_client_id"],
        "a secret that is not set has nothing to clear"
    );

    let replaced = settings::request(
        &record,
        &form(&[
            ("value.poll_seconds", "900"),
            ("secret.snaptrade_client_id", " new-key "),
            ("clear.snaptrade_client_id", "on"),
            ("value.undeclared", "x"),
        ]),
    )
    .unwrap();
    assert!(replaced.cleared.is_empty(), "typed in, it is replaced");
    assert_eq!(replaced.values.len(), 1);
    assert_eq!(replaced.values[0].value, "new-key");
}
