use std::sync::Mutex;

use axum::body::{to_bytes, Body};
use axum::http::header::COOKIE;
use axum::http::Request;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccountRecord, AccountState, DefineAccountRequest, ExternalAccount, ExternalAccountLink,
    ExternalAccountsEvent, Permission, PluginSettingValue, RedeemClaimCodeReply,
    SetPluginSettingsRequest, SyncState, SyncStatusEvent, UnlinkedExternalAccount,
    UnlinkedExternalAccountsEvent,
};
use meridian_pb::v1::{SettingChoice, SettingCondition, SettingDeclaration, SettingType};
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
    /// Each payload the fake conductor was sent, in order.
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    session: String,
    form_token: String,
}

/// A dashboard whose records are `records`, with Ada signed in, and a fake
/// conductor answering define-account and redeem-claim-code.
fn harness(records: AccessRecords, refuse_with: Option<&'static str>) -> Harness {
    harness_serving(records, refuse_with, None)
}

/// The same, serving plugins' pages below a name with a domain, so a
/// plugin's admin pages are framed.
fn framing(records: AccessRecords) -> Harness {
    let plugins = crate::plugins::Plugins::new(
        "https://meridian.example",
        "http://{instance}.sidecars.invalid:9292",
        crate::signing::Signer::holding(
            "dashboard-test",
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
        ),
    )
    .unwrap();
    harness_serving(records, None, Some(Arc::new(plugins)))
}

fn harness_serving(
    records: AccessRecords,
    refuse_with: Option<&'static str>,
    plugins: Option<Arc<crate::plugins::Plugins>>,
) -> Harness {
    let bus = Arc::new(Bus::single("dashboard-1", Arc::new(MemoryBackend::new())));
    let seen: Seen = Arc::default();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    for topic in [
        "platform.config.command.define-account",
        "platform.config.command.redeem-claim-code",
    ] {
        let seen = Arc::clone(&seen);
        let sent = Arc::clone(&sent);
        bus.serve(topic, move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            seen.lock()
                .unwrap()
                .push((meta.topic.clone(), meta.acting_for_subject.clone()));
            sent.lock().unwrap().push(envelope.payload.clone());
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
        plugins,
        registry: None,
        custody: Arc::default(),
        health: Arc::default(),
        kit: None,
    });
    Harness {
        app,
        seen,
        sent,
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
    body.split(&format!("<table class=\"list {class}\""))
        .nth(1)
        .and_then(|rest| rest.split_once('>').map(|(_, inside)| inside))
        .and_then(|rest| rest.split("</table>").next())
        .unwrap_or_else(|| panic!("no {class} table in the page"))
}

#[tokio::test]
async fn the_dashboard_lists_and_links_no_external_accounts_and_counts_them() {
    // The product owner, 2026-09-28: each plugin links its own external
    // accounts on its admin pages (W6.4). SNAP-1 is linked, SNAP-2 is not, and
    // st-9902 was only ever refused: the dashboard counts the two on the
    // plugin's line and offers no link of its own.
    let mut records = admin_records();
    records.accounts = vec![AccountRecord {
        account_id: "ACC-1".into(),
        name: "Growth".into(),
        state: AccountState::Open as i32,
        created_at_ns: T0,
        ..Default::default()
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
    assert!(!body.contains("external-accounts") && !body.contains("External accounts"));
    assert!(
        !body.contains("name=\"external_account_id\""),
        "no link of its own"
    );
    assert!(!body.contains("/admin/links"));
    let listed = table(&body, "plugins");
    assert!(
        listed.contains(&format!(
            "<a href=\"{VIEW}\">2 external accounts not linked</a>"
        )),
        "{listed}"
    );

    // Nor does it take a link any more: nothing reaches the conductor.
    let form = format!(
        "form_token={}&plugin_instance_id=snaptrade-1&external_account_id=SNAP-2&account_id=ACC-1",
        h.form_token
    );
    let (status, _) = send(&h, post(&h, "/admin/links", &form)).await;
    assert!(!status.is_success() && !status.is_redirection(), "{status}");
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unlinked_accounts_sync_state_is_on_the_plugins_overview() {
    // Ruled 2026-09-28: sync status describes the connection, not recorded
    // data, so it arrives before a link and says whether one is worth making.
    let h = harness(with_settings(), None);
    h.app.custody.hear_sync(
        "snaptrade-1",
        SyncStatusEvent {
            external_account_id: "SNAP-9".into(),
            account_id: String::new(),
            state: SyncState::HoldingsUnavailable as i32,
            ..Default::default()
        },
    );

    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let sync = table(&body, "sync");
    assert!(sync.contains("data-id=\"SNAP-9\""));
    assert!(sync.contains("Holdings unavailable"), "{sync}");
    assert!(sync.contains("Connect the account another way"));
    assert!(sync.contains("not linked"));
    let (_, admin) = send(&h, get(&h, "/admin", true)).await;
    assert!(
        !admin.contains("class=\"list sync\""),
        "and not on the admin portal"
    );
}

#[tokio::test]
async fn a_sync_state_is_shown_with_what_to_do_about_it() {
    let h = harness(with_settings(), None);
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
    // Another plugin's connection is not this one's.
    h.app.custody.hear_sync(
        "other-1",
        SyncStatusEvent {
            external_account_id: "OTHER-1".into(),
            state: SyncState::Stale as i32,
            ..Default::default()
        },
    );

    let (_, body) = send(&h, get(&h, VIEW, true)).await;
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
    assert!(!sync.contains("OTHER-1"));
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
async fn an_accounts_custodian_type_owner_and_note_are_sent_as_the_form_gives_them() {
    // W6.3: all four from the define and edit form; an empty one is sent
    // empty, which clears it.
    let h = harness(admin_records(), None);
    let form = format!(
        "form_token={}&account_id=ACC-1&name=Growth&custodian=+Fidelity+\
         &account_type=Roth+IRA&owner=Fund+I&note=",
        h.form_token
    );
    let (status, _) = send(&h, post(&h, "/admin/accounts", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let sent = h.sent.lock().unwrap();
    let request = DefineAccountRequest::decode(&sent[0][..]).unwrap();
    assert_eq!(
        request,
        DefineAccountRequest {
            account_id: "ACC-1".into(),
            name: "Growth".into(),
            custodian: "Fidelity".into(),
            account_type: "Roth IRA".into(),
            owner: "Fund I".into(),
            note: String::new(),
        }
    );
}

#[tokio::test]
async fn the_accounts_tab_shows_each_accounts_attributes_and_offers_a_search() {
    let mut records = admin_records();
    records.accounts = vec![
        AccountRecord {
            account_id: "ACC-1".into(),
            name: "Growth".into(),
            state: AccountState::Open as i32,
            created_at_ns: T0,
            custodian: "Fidelity".into(),
            account_type: "Roth IRA".into(),
            owner: "Fund <I>".into(),
            note: "Rollover, 2026.".into(),
        },
        AccountRecord {
            account_id: "ACC-2".into(),
            name: "Income".into(),
            state: AccountState::Open as i32,
            created_at_ns: T0,
            ..Default::default()
        },
    ];
    let h = harness(records, None);
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    assert!(
        body.contains("data-filter=\"accounts-table\"") && body.contains("id=\"accounts-table\""),
        "a search box narrowing the accounts' table, on the generic filter"
    );
    let accounts = table(&body, "accounts");
    assert!(
        accounts
            .contains("<th>Name</th><th>Custodian</th><th>Type</th><th>Owner</th><th>State</th>"),
        "{accounts}"
    );
    assert!(
        accounts.contains(
            "<span class=\"hint\">Rollover, 2026.</span></td>\
             <td>Fidelity</td><td>Roth IRA</td><td>Fund &lt;I&gt;</td>"
        ),
        "the note under the name, then its custodian, type and owner, escaped: {accounts}"
    );
    let income = accounts.split("data-id=\"ACC-2\"").nth(1).unwrap();
    assert!(
        income.contains("<td></td><td></td><td></td>")
            && !income.split("</tr>").next().unwrap().contains("hint"),
        "an account with none shows none: {income}"
    );

    // Its edit dialog holds all four, bounded as the conductor bounds them.
    let dialog = body.split("<dialog id=\"edit-ACC-1\">").nth(1).unwrap();
    let dialog = dialog.split("</dialog>").next().unwrap();
    for field in [
        "name=\"custodian\" value=\"Fidelity\" maxlength=\"200\"",
        "name=\"account_type\" value=\"Roth IRA\" maxlength=\"200\"",
        "name=\"owner\" value=\"Fund &lt;I&gt;\" maxlength=\"200\"",
        "name=\"note\" rows=\"3\" maxlength=\"2000\">Rollover, 2026.</textarea>",
    ] {
        assert!(dialog.contains(field), "{field} is not in {dialog}");
    }
    let new = body.split("<dialog id=\"new-account\">").nth(1).unwrap();
    assert!(
        new.contains("name=\"custodian\" value=\"\"") && new.contains("name=\"note\""),
        "and so does a new account's"
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
fn entries_are_plugin_and_level_one_per_line() {
    let parsed = parse_entries("oms-1 write\n\n snaptrade-1 read ").unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].plugin_instance_id, "oms-1");
    assert_eq!(parsed[0].level, AccessLevel::Write as i32);
    assert_eq!(parsed[1].plugin_instance_id, "snaptrade-1");
    assert_eq!(parsed[1].level, AccessLevel::Read as i32);
    assert!(parse_entries("oms-1 admin")
        .unwrap_err()
        .contains("not read or write"));
    assert!(parse_entries("oms-1").is_err());
    // decisions/026: a plugin declares no tags, so an entry naming one is
    // refused and says why, rather than read as something else.
    let tagged = parse_entries("snaptrade-1 custody read").unwrap_err();
    assert!(
        tagged.contains("names a tag") && tagged.contains("decisions/026"),
        "{tagged}"
    );
}

// ── A plugin instance's admin view and settings (W6.9, W6.10, W6.11) ───────

/// Obviously not a real credential, and long enough to find in a page.
const SECRET: &str = "sk-test-not-a-real-key-7f3a";

fn declared(name: &str, kind: SettingType, required: bool, secret: bool) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: kind as i32,
        required,
        secret,
        description: format!("What {name} is <for>."),
        ..Default::default()
    }
}

/// SnapTrade's shape: a personal or commercial key, which decides whether a
/// user secret is needed; two secrets every key needs, one of them set; how
/// often to read, and when a reading is stale, each with a default and a
/// unit; and a developer's switch.
fn snaptrade() -> PluginSettingsRecord {
    let choice = |value: &str, label: &str, description: &str| SettingChoice {
        value: value.into(),
        label: label.into(),
        description: description.into(),
    };
    PluginSettingsRecord {
        plugin_instance_id: "snaptrade-1".into(),
        values: vec![
            PluginSettingValue {
                name: "key_type".into(),
                value: "personal".into(),
            },
            PluginSettingValue {
                name: "poll_seconds".into(),
                value: "900".into(),
            },
        ],
        secrets_set: vec!["snaptrade_client_id".into()],
        updated_at_ns: T0,
        declared_settings: vec![
            SettingDeclaration {
                label: "Client ID".into(),
                ..declared("snaptrade_client_id", SettingType::String, true, true)
            },
            SettingDeclaration {
                label: "Consumer key".into(),
                ..declared("snaptrade_consumer_key", SettingType::String, true, true)
            },
            SettingDeclaration {
                label: "User secret".into(),
                applies_when: Some(SettingCondition {
                    setting: "key_type".into(),
                    one_of: vec!["commercial".into()],
                }),
                ..declared("user_secret", SettingType::String, true, true)
            },
            SettingDeclaration {
                label: "Read every".into(),
                default_value: "300".into(),
                unit: "seconds".into(),
                ..declared("poll_seconds", SettingType::Integer, false, false)
            },
            SettingDeclaration {
                label: "Stale after".into(),
                default_value: "24".into(),
                unit: "hours".into(),
                ..declared("stale_after_hours", SettingType::Integer, false, false)
            },
            SettingDeclaration {
                label: "Serve built-in data".into(),
                developer: true,
                ..declared("synthetic", SettingType::Boolean, false, false)
            },
            // Declared last, asked for first: it decides which fields follow.
            SettingDeclaration {
                label: "Key".into(),
                choices: vec![
                    choice("personal", "Personal key", "Belongs to one user."),
                    choice(
                        "commercial",
                        "Commercial key",
                        "Registers users of its own.",
                    ),
                ],
                ..declared("key_type", SettingType::Choice, true, false)
            },
        ],
    }
}

fn with_settings() -> AccessRecords {
    let mut records = admin_records();
    records.plugin_settings = vec![snaptrade()];
    records
}

/// The part of `body` for one setting.
fn setting<'a>(body: &'a str, name: &str) -> &'a str {
    body.split(&format!("data-setting=\"{name}\""))
        .nth(1)
        .unwrap_or_else(|| panic!("no setting {name} in {body}"))
        .split("<div class=\"setting\"")
        .next()
        .unwrap()
}

#[test]
fn the_form_says_what_to_fill_in() {
    let record = snaptrade();
    let form = settings::form(
        &record,
        "<input type=\"hidden\" name=\"form_token\">",
        false,
    );

    // The choice first, as radio buttons, since it decides what follows.
    let first = form.split("data-setting=\"").nth(1).unwrap();
    assert!(first.starts_with("key_type\""), "{form}");
    let key = setting(&form, "key_type");
    assert!(key.contains("Key") && key.contains("Required"), "{key}");
    assert!(
        key.contains("<input type=\"radio\" name=\"value.key_type\" value=\"personal\" checked>")
    );
    assert!(key.contains("<input type=\"radio\" name=\"value.key_type\" value=\"commercial\">"));
    assert!(key.contains("Personal key") && key.contains("Belongs to one user."));
    assert!(
        !key.contains("value=\"\""),
        "a required choice has no unset option"
    );

    // Each field under its label, marked required or optional.
    let client = setting(&form, "snaptrade_client_id");
    assert!(client.contains("Client ID") && client.contains("Required"));
    assert!(client.contains("type=\"password\" name=\"secret.snaptrade_client_id\" value=\"\""));
    assert!(client.contains(">set<") && client.contains("name=\"clear.snaptrade_client_id\""));
    let consumer = setting(&form, "snaptrade_consumer_key");
    assert!(consumer.contains(">not set<") && !consumer.contains("clear."));

    // Applies only to a commercial key: said, and marked for the script.
    let user = setting(&form, "user_secret");
    assert!(user.contains("data-applies-setting=\"key_type\""), "{user}");
    assert!(user.contains("data-applies-one-of=\"[&quot;commercial&quot;]\""));
    assert!(user.contains("Only when Key is Commercial key."));

    // A default greyed in the empty field, never its value; the unit beside.
    let poll = setting(&form, "poll_seconds");
    assert!(poll.contains("Optional") && poll.contains("Read every"));
    assert!(poll.contains(
        "<input type=\"number\" step=\"1\" name=\"value.poll_seconds\" value=\"900\" placeholder=\"300\">"
    ));
    assert!(poll.contains("<span class=\"unit\">seconds</span>"));
    assert!(poll.contains("Left empty, the plugin uses 300 seconds."));
    let stale = setting(&form, "stale_after_hours");
    assert!(stale.contains("value=\"\" placeholder=\"24\""), "{stale}");
    assert!(stale.contains("<span class=\"unit\">hours</span>"));
    assert!(
        form.contains("What poll_seconds is &lt;for&gt;."),
        "escaped"
    );

    // A developer's setting only on a development deployment.
    assert!(!form.contains("data-setting=\"synthetic\""));
    let developing = settings::form(&record, "", true);
    let synthetic = setting(&developing, "synthetic");
    assert!(synthetic.contains("Developer") && synthetic.contains("name=\"value.synthetic\""));
}

#[test]
fn what_is_missing_is_what_is_required_of_the_settings_that_apply() {
    let names = |record: &PluginSettingsRecord| -> Vec<String> {
        settings::missing(record, false)
            .into_iter()
            .map(|d| d.name.clone())
            .collect()
    };
    // Personal: the user secret does not apply, so it is not missing.
    let personal = snaptrade();
    assert_eq!(names(&personal), ["snaptrade_consumer_key"]);

    let mut commercial = snaptrade();
    commercial.values[0].value = "commercial".into();
    assert_eq!(
        names(&commercial),
        ["snaptrade_consumer_key", "user_secret"]
    );

    // Nothing chosen: the choice is missing, and what hangs on it waits.
    let mut unchosen = snaptrade();
    unchosen.values.remove(0);
    assert_eq!(names(&unchosen), ["snaptrade_consumer_key", "key_type"]);
}

/// SnapTrade 0.2.0 upgraded from 0.1.0: its key's type a required choice,
/// "personal" by default, and nothing saved for it.
fn defaulted_and_unsaved() -> PluginSettingsRecord {
    let mut record = snaptrade();
    record.values.retain(|held| held.name != "key_type");
    let key = record
        .declared_settings
        .iter_mut()
        .find(|declaration| declaration.name == "key_type")
        .unwrap();
    key.default_value = "personal".into();
    record
}

#[test]
fn a_required_setting_declaring_a_default_is_not_missing_and_its_option_is_shown_chosen() {
    let record = defaulted_and_unsaved();
    let names: Vec<&str> = settings::missing(&record, false)
        .into_iter()
        .map(|d| d.name.as_str())
        .collect();
    // The default is the value the plugin uses, so the choice is not
    // missing, and a personal key's user secret does not apply.
    assert_eq!(names, ["snaptrade_consumer_key"]);

    // The form shows the default's option chosen, since a required choice
    // has no unset option to show instead.
    let form = settings::form(&record, "", false);
    let key = setting(&form, "key_type");
    assert!(
        key.contains(r#"<input type="radio" name="value.key_type" value="personal" checked>"#),
        "{key}"
    );
    assert!(key.contains(r#"<input type="radio" name="value.key_type" value="commercial">"#));

    // Saving the form as shown saves the default; until then it is unsaved.
    let posted: Fields = [("value.key_type".to_string(), "personal".to_string())]
        .into_iter()
        .collect();
    let saved = settings::request(&record, &posted, false).unwrap();
    assert_eq!(saved.values.len(), 1);
    assert_eq!(
        (
            saved.values[0].name.as_str(),
            saved.values[0].value.as_str()
        ),
        ("key_type", "personal")
    );

    // Without a default, nothing is chosen and the choice is missing.
    let mut undefaulted = snaptrade();
    undefaulted.values.retain(|held| held.name != "key_type");
    let form = settings::form(&undefaulted, "", false);
    assert!(!setting(&form, "key_type").contains("checked"));

    // An optional choice keeps its unset option chosen, saying what the
    // plugin uses then.
    let mut optional = defaulted_and_unsaved();
    optional
        .declared_settings
        .iter_mut()
        .find(|declaration| declaration.name == "key_type")
        .unwrap()
        .required = false;
    let form = settings::form(&optional, "", false);
    let key = setting(&form, "key_type");
    assert!(
        key.contains(r#"<input type="radio" name="value.key_type" value="" checked>"#),
        "{key}"
    );
    assert!(key.contains("The plugin uses Personal key."));
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

const VIEW: &str = "/admin/plugins/snaptrade-1";
const SETTINGS: &str = "/admin/plugins/snaptrade-1/settings";

/// The tabs a view's page offers, as (href, name), and the one it is on.
fn tabs_of(body: &str) -> (Vec<(String, String)>, String) {
    let nav = body
        .split("<nav class=\"tabs view-tabs\"")
        .nth(1)
        .and_then(|rest| rest.split("</nav>").next())
        .expect("the view's tabs");
    let mut tabs = Vec::new();
    let mut here = String::new();
    for link in nav.split("<a href=\"").skip(1) {
        let href = link.split('"').next().unwrap().replace("&amp;", "&");
        let name = link.split('>').nth(1).unwrap().split('<').next().unwrap();
        if link.contains("aria-current=\"page\"") {
            here = name.to_string();
        }
        tabs.push((href, name.to_string()));
    }
    (tabs, here)
}

#[tokio::test]
async fn a_plugins_admin_view_is_for_admins_alone_and_holds_its_settings_form() {
    let h = harness(with_settings(), None);
    let (status, body) = send(&h, get(&h, VIEW, true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Tabs, each a link of its own; the view opens on the first.
    let (tabs, here) = tabs_of(&body);
    let names: Vec<&str> = tabs.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["Overview", "Settings", "Access", "Admin page"]);
    assert_eq!(here, "Overview");
    assert_eq!(tabs[0].0, VIEW);
    assert_eq!(tabs[3].0, format!("{VIEW}?tab=admin"));
    assert!(body.contains("id=\"health\""));
    for (part, tab) in [
        ("id=\"settings\"", "settings"),
        ("id=\"access\"", "access"),
        ("id=\"admin-page\"", "admin"),
    ] {
        assert!(!body.contains(part), "{part} is only on its own tab");
        let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab={tab}"), true)).await;
        assert!(page.contains(part), "{part} is not on ?tab={tab}");
        assert!(!page.contains("id=\"health\""));
    }
    let (_, body) = send(&h, get(&h, &format!("{VIEW}?tab=settings"), true)).await;
    assert_eq!(tabs_of(&body).1, "Settings");
    assert!(body.contains(&format!("action=\"{SETTINGS}\"")));
    assert!(
        body.contains(&format!("value=\"{}\"", h.form_token)),
        "the form token"
    );
    // This harness serves no plugin pages, and the view says so rather than
    // framing something that is not there.
    let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab=admin"), true)).await;
    assert!(page.contains("cannot frame this one"));
    // A tab that is not one is the first.
    let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab=secret"), true)).await;
    assert_eq!(tabs_of(&page).1, "Overview");

    // The old address of the form is the view's.
    let old = router(Arc::clone(&h.app))
        .oneshot(get(&h, SETTINGS, true))
        .await
        .unwrap();
    assert_eq!(old.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        old.headers()["location"],
        format!("{VIEW}?tab=settings").as_str()
    );

    // The overview's Plugins tab lists it, with the secret it still needs,
    // leading to the view.
    let (_, overview) = send(&h, get(&h, "/admin", true)).await;
    let listed = table(&overview, "plugins");
    assert!(listed.contains("data-id=\"snaptrade-1\""));
    assert!(listed.contains("needs Consumer key"), "{listed}");
    assert!(listed.contains(&format!("href=\"{VIEW}\"")));

    let unknown = send(&h, get(&h, "/admin/plugins/ghost-1", true)).await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);

    let mut not_admin = with_settings();
    not_admin.permissions.clear();
    let nobody = harness(not_admin, None);
    for path in [VIEW, SETTINGS] {
        assert_eq!(
            send(&nobody, get(&nobody, path, true)).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(&nobody, get(&nobody, path, false)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn the_plugins_tab_and_view_do_not_ask_for_a_required_setting_its_default_fills() {
    let mut records = admin_records();
    records.plugin_settings = vec![defaulted_and_unsaved()];
    let h = harness(records, None);
    let (_, overview) = send(&h, get(&h, "/admin", true)).await;
    let listed = table(&overview, "plugins");
    assert!(listed.contains("needs Consumer key"), "{listed}");
    assert!(
        !listed.contains("Key,") && !listed.contains(", Key"),
        "{listed}"
    );
    let (_, view) = send(&h, get(&h, VIEW, true)).await;
    assert!(view.contains("Needs Consumer key:"), "{view}");
}

#[tokio::test]
async fn the_view_shows_the_plugins_health_who_has_access_and_its_unlinked_accounts() {
    let mut records = with_settings();
    records.user_groups.push(UserGroup {
        user_group_id: "UG-2".into(),
        name: "Operations".into(),
        ..Default::default()
    });
    records
        .account_groups
        .push(meridian_domain::v1::AccountGroup {
            account_group_id: "AcG-1".into(),
            name: "Growth accounts".into(),
            account_ids: vec![],
        });
    records
        .access_groups
        .push(meridian_domain::v1::AccessGroup {
            access_group_id: "AG-1".into(),
            name: "Custody readers".into(),
            entries: vec![meridian_domain::v1::AccessEntry {
                plugin_instance_id: "snaptrade-1".into(),
                level: meridian_domain::v1::AccessLevel::Read as i32,
            }],
            built_in: false,
        });
    records.permissions.push(Permission {
        permission_id: "P-2".into(),
        user_group_id: "UG-2".into(),
        account_group_id: "AcG-1".into(),
        access_group_id: "AG-1".into(),
    });
    let h = harness(records, None);
    h.app.health.hear(
        "snaptrade-1",
        meridian_domain::v1::PluginReport {
            plugin_instance_id: "snaptrade-1".into(),
            registered: true,
            healthy: false,
            health_detail: "required setting snaptrade_consumer_key is not set".into(),
            contract_version: "v2".into(),
            reported_at_ns: T0,
            ..Default::default()
        },
    );
    h.app.custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: (1..=5)
                .map(|n| ExternalAccount {
                    external_account_id: format!("SNAP-{n}"),
                    name: format!("Brokerage {n}"),
                    venue_account_type: "Individual".into(),
                })
                .collect(),
        },
    );

    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let health = body.split("id=\"health\"").nth(1).unwrap();
    assert!(health.contains("Not healthy"), "{health}");
    assert!(health.contains("required setting snaptrade_consumer_key is not set"));
    // The count leads to the plugin's own admin pages, where it links them
    // (W6.4, W6.10), and to nothing of the dashboard's.
    assert!(
        health.contains(&format!(
            "5 external accounts not linked. <a href=\"{VIEW}?tab=admin\">Link them on the \
             plugin's admin pages</a>."
        )),
        "{health}"
    );
    assert!(!health.contains("/admin#external-accounts"));
    assert!(health.contains(&format!(
        "<a href=\"{VIEW}?tab=settings\">fill in its settings</a>"
    )));
    let (_, body) = send(&h, get(&h, &format!("{VIEW}?tab=access"), true)).await;
    let access = table(&body, "access");
    assert!(access.contains("Operations") && access.contains("Custody readers"));
    assert!(access.contains("Growth accounts") && access.contains(">read<"));
    assert!(access.contains("<th>Level</th>") && !access.contains("Tag"));
    assert!(body.contains("Deployment admins open it too"));

    // The same flag on the overview, leading to the view (W6.10).
    let (_, overview) = send(&h, get(&h, "/admin", true)).await;
    let listed = table(&overview, "plugins");
    assert!(listed.contains("Not healthy"));
    assert!(
        listed.contains(&format!(
            "<a href=\"{VIEW}\">5 external accounts not linked</a>"
        )),
        "{listed}"
    );
}

fn declaring(pages: &[(&str, &str)]) -> meridian_domain::v1::PluginReport {
    meridian_domain::v1::PluginReport {
        plugin_instance_id: "snaptrade-1".into(),
        registered: true,
        healthy: true,
        reported_at_ns: T0,
        declared_interface: Some(meridian_pb::v1::InterfaceDeclaration {
            loopback_port: 8000,
            title: "SnapTrade".into(),
            admin_pages: pages
                .iter()
                .map(|(path, title)| meridian_pb::v1::PageDeclaration {
                    path: path.to_string(),
                    title: title.to_string(),
                })
                .collect(),
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_plugins_declared_admin_pages_are_tabs_in_its_order_each_framing_its_path() {
    // W6.9, the product owner, 2026-09-29: Overview, Settings, Access, then
    // one tab per admin page the plugin declared (W4.8), in its order.
    let h = framing(with_settings());
    h.app.health.hear(
        "snaptrade-1",
        declaring(&[
            ("/admin/connections", "Connections"),
            ("/admin/accounts", "Accounts"),
            ("/admin/holdings", "Holdings"),
        ]),
    );
    let (status, body) = send(&h, get(&h, VIEW, true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (tabs, here) = tabs_of(&body);
    assert_eq!(
        tabs,
        [
            (VIEW.to_string(), "Overview".to_string()),
            (format!("{VIEW}?tab=settings"), "Settings".into()),
            (format!("{VIEW}?tab=access"), "Access".into()),
            (format!("{VIEW}?tab=connections"), "Connections".into()),
            (format!("{VIEW}?tab=accounts"), "Accounts".into()),
            (format!("{VIEW}?tab=holdings"), "Holdings".into()),
        ]
    );
    assert_eq!(here, "Overview");
    assert!(
        !body.contains("<iframe"),
        "a page is framed only on its own tab"
    );

    let (_, accounts) = send(&h, get(&h, &format!("{VIEW}?tab=accounts"), true)).await;
    assert_eq!(tabs_of(&accounts).1, "Accounts");
    let frame = accounts.split("<iframe").nth(1).expect("the page, framed");
    let frame = frame.split("</iframe>").next().unwrap();
    assert!(
        frame.contains(
            "src=\"/plugins/snaptrade-1/enter?path=%2Fadmin%2Faccounts&amp;om-scheme=default\
             &amp;om-mode=system&amp;om-direction=green-up&amp;om-framed=1\""
        ),
        "seamless from its first paint: {frame}"
    );
    assert!(frame.contains("data-origin=\"https://snaptrade-1.plugins.meridian.example\""));
    assert!(
        frame.contains("class=\"admin-frame\"") && frame.contains(" data-seamless "),
        "{frame}"
    );
    assert_eq!(accounts.matches("<iframe").count(), 1, "one page at a time");
    // Seamless (the product owner, 2026-09-29): straight under the tab row,
    // with no panel, heading, path or hint of the dashboard's around it.
    let under_tabs = accounts.split("</nav>").last().unwrap();
    assert!(
        under_tabs.starts_with("<div class=\"tab-body\" data-current=\"accounts\"><iframe "),
        "{under_tabs}"
    );
    let body = under_tabs.split("</iframe>").next().unwrap();
    for gone in [
        "class=\"panel",
        "<h2>",
        "<code>/admin/accounts</code>",
        "class=\"hint\"",
    ] {
        assert!(!body.contains(gone), "{gone} around the frame: {body}");
    }

    // A window of its own, where no frame could hold the page, is a page on
    // its own.
    let on_localhost = crate::plugins::Plugins::new(
        "http://localhost:8080",
        "http://{instance}.sidecars.invalid:9292",
        crate::signing::Signer::holding(
            "dashboard-test",
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
        ),
    )
    .unwrap();
    let linked = harness_serving(with_settings(), None, Some(Arc::new(on_localhost)));
    linked
        .app
        .health
        .hear("snaptrade-1", declaring(&[("/admin/accounts", "Accounts")]));
    let (_, page) = send(&linked, get(&linked, &format!("{VIEW}?tab=accounts"), true)).await;
    let panel = page
        .split("id=\"admin-page\"")
        .nth(1)
        .expect("the panel saying why");
    assert!(
        panel.contains("&amp;om-framed=0\" target=\"_blank\"") && !page.contains("<iframe"),
        "{panel}"
    );
}

#[tokio::test]
async fn a_page_that_is_not_one_on_the_plugins_host_is_no_tab() {
    let h = framing(with_settings());
    h.app.health.hear(
        "snaptrade-1",
        declaring(&[
            ("//evil.example/x", "Elsewhere"),
            ("https://evil.example/", "Absolute"),
            ("/.meridian/ui/0.1.0/", "The kit"),
            ("/admin/accounts", ""),
            ("/admin/accounts", "Twice"),
        ]),
    );
    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let names: Vec<String> = tabs_of(&body).0.into_iter().map(|(_, name)| name).collect();
    // Untitled, it is called by its path; the second of a path is dropped.
    assert_eq!(names, ["Overview", "Settings", "Access", "/admin/accounts"]);
    assert_eq!(
        tabs_of(&body).0[3].0,
        format!("{VIEW}?tab=admin-accounts"),
        "named in the query by its path, made a word"
    );
    for asked in ["%2F%2Fevil.example%2Fx", "https%3A%2F%2Fevil.example%2F"] {
        let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab={asked}"), true)).await;
        assert_eq!(tabs_of(&page).1, "Overview", "{asked} is framed nowhere");
        assert!(!page.contains("<iframe"));
    }

    // Declaring none, the plugin's /admin is the one page tab.
    h.app.health.hear("snaptrade-1", declaring(&[]));
    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let (tabs, _) = tabs_of(&body);
    assert_eq!(
        tabs.last().unwrap(),
        &(format!("{VIEW}?tab=admin"), "Admin page".to_string())
    );
    let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab=admin"), true)).await;
    assert!(page.contains("src=\"/plugins/snaptrade-1/enter?path=%2Fadmin&amp;"));
}

#[tokio::test]
async fn a_secret_typed_in_goes_to_the_conductor_and_never_back_into_a_page() {
    let h = harness(with_settings(), None);
    let asked = conductor_setting(&h, None);
    let form = format!(
        "form_token={}&value.key_type=commercial&secret.snaptrade_client_id=\
         &secret.snaptrade_consumer_key={SECRET}&value.poll_seconds=900&value.stale_after_hours=",
        h.form_token
    );
    let response = router(Arc::clone(&h.app))
        .oneshot(post(&h, SETTINGS, &form))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        format!("{VIEW}?tab=settings&saved=1").as_str()
    );

    let (request, by) = asked.lock().unwrap()[0].clone();
    assert_eq!(by, ADA, "on her behalf");
    assert_eq!(request.plugin_instance_id, "snaptrade-1");
    // Only what changed: the set secret left empty is left alone, the number
    // is what it was, and the empty field with a default stores nothing.
    let sent: Vec<(&str, &str)> = request
        .values
        .iter()
        .map(|v| (v.name.as_str(), v.value.as_str()))
        .collect();
    assert_eq!(
        sent,
        [
            ("snaptrade_consumer_key", SECRET),
            ("key_type", "commercial")
        ]
    );
    assert!(request.cleared.is_empty());

    let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab=settings&saved=1"), true)).await;
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
        format!("{VIEW}?tab=settings&saved=none").as_str()
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
    let asked = |pairs: &[(&str, &str)]| settings::request(&record, &form(pairs), false);
    assert!(asked(&[("value.poll_seconds", "900")]).is_none());
    // Not posted is not emptied: a field the form did not carry is left.
    assert!(asked(&[]).is_none());

    let emptied = asked(&[("value.poll_seconds", "")]).unwrap();
    assert_eq!(emptied.cleared, ["poll_seconds"]);

    let cleared = asked(&[
        ("value.poll_seconds", "900"),
        ("clear.snaptrade_client_id", "on"),
        ("clear.snaptrade_consumer_key", "on"),
    ])
    .unwrap();
    assert_eq!(
        cleared.cleared,
        ["snaptrade_client_id"],
        "a secret that is not set has nothing to clear"
    );

    let replaced = asked(&[
        ("value.poll_seconds", "900"),
        ("secret.snaptrade_client_id", " new-key "),
        ("clear.snaptrade_client_id", "on"),
        ("value.undeclared", "x"),
    ])
    .unwrap();
    assert!(replaced.cleared.is_empty(), "typed in, it is replaced");
    assert_eq!(replaced.values.len(), 1);
    assert_eq!(replaced.values[0].value, "new-key");

    // A developer's setting is not this deployment's to change, whatever is
    // posted; a development deployment's is.
    assert!(asked(&[("value.synthetic", "true")]).is_none());
    let developing =
        settings::request(&record, &form(&[("value.synthetic", "true")]), true).unwrap();
    assert_eq!(developing.values[0].name, "synthetic");
}

#[tokio::test]
async fn a_plugin_page_titled_as_one_of_the_views_own_does_not_take_its_tab() {
    let h = framing(with_settings());
    h.app.health.hear(
        "snaptrade-1",
        declaring(&[
            ("/admin/settings", "Settings"),
            ("/admin/ladder", "Cash ladder"),
            ("/admin/other", "Cash  ladder!"),
        ]),
    );
    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let hrefs: Vec<String> = tabs_of(&body).0.into_iter().map(|(href, _)| href).collect();
    assert_eq!(
        hrefs[3..],
        [
            format!("{VIEW}?tab=settings-2"),
            format!("{VIEW}?tab=cash-ladder"),
            format!("{VIEW}?tab=cash-ladder-2"),
        ]
    );
    let (_, page) = send(&h, get(&h, &format!("{VIEW}?tab=settings"), true)).await;
    assert!(
        page.contains(&format!("action=\"{SETTINGS}\"")),
        "?tab=settings is still the view's own"
    );
}

#[tokio::test]
async fn the_plugins_tab_names_the_instance_apart_and_offers_a_search() {
    let h = harness(admin_records(), None);
    h.app.custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: vec![ExternalAccount {
                external_account_id: "SNAP-1".into(),
                name: "Individual Brokerage 1234".into(),
                venue_account_type: "Individual".into(),
            }],
        },
    );
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    assert!(
        body.contains("<th>Plugin</th><th>Instance</th><th>Health</th>"),
        "the instance its own column"
    );
    assert!(
        body.contains("data-filter=\"plugins-table\"") && body.contains("id=\"plugins-table\""),
        "a search box narrowing the plugins' table"
    );
    assert!(
        table(&body, "plugins").contains("<td><code>snaptrade-1</code></td>"),
        "the instance in its own cell"
    );
}
