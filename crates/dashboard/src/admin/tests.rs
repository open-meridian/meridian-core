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
        // As first run writes them: deployment admin, and All plugins
        // (admin), so the admins configure every plugin (W7.6).
        permissions: vec![
            Permission {
                permission_id: "P-1".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: DEPLOYMENT_ADMIN.into(),
            },
            Permission {
                permission_id: "P-0".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
            },
        ],
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
    harness_holding(records, refuse_with, plugins, None)
}

/// The same, holding local accounts of its own.
fn harness_holding(
    records: AccessRecords,
    refuse_with: Option<&'static str>,
    plugins: Option<Arc<crate::plugins::Plugins>>,
    accounts: Option<Arc<dyn crate::accounts::Accounts>>,
) -> Harness {
    let bus = Arc::new(Bus::single(
        "dashboard-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
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
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(At(T0)),
        bus,
        oidc: None,
        directory: None,
        accounts,
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins,
        registry: None,
        custody: Arc::default(),
        health: Arc::default(),
        kit: None,
        bounds: Arc::default(),
        tickets: Arc::default(),
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
                    ..Default::default()
                },
                ExternalAccount {
                    external_account_id: "SNAP-2".into(),
                    name: "Roth IRA 5678".into(),
                    venue_account_type: "Roth IRA".into(),
                    ..Default::default()
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
            "<span class=\"hint note\" id=\"account-note-0\">Rollover, 2026.</span></td>\
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

    // One dialog for a new account and for each edit, holding all four,
    // bounded as the conductor bounds them; an Edit fills it with what the
    // account holds.
    assert_eq!(
        body.matches("<dialog id=\"account\" aria-labelledby=\"account-title\">")
            .count(),
        1
    );
    let dialog = body
        .split("<dialog id=\"account\" aria-labelledby=\"account-title\">")
        .nth(1)
        .unwrap();
    let dialog = dialog.split("</dialog>").next().unwrap();
    for field in [
        "name=\"account_id\" value=\"\" data-record-id",
        "name=\"custodian\" value=\"\" maxlength=\"200\"",
        "name=\"account_type\" value=\"\" maxlength=\"200\"",
        "name=\"owner\" value=\"\" maxlength=\"200\"",
        "name=\"note\" rows=\"3\" maxlength=\"2000\"></textarea>",
    ] {
        assert!(dialog.contains(field), "{field} is not in {dialog}");
    }
    let fill = fill_of(&body, "ACC-1");
    assert_eq!(fill["fields"]["account_id"], "ACC-1");
    assert_eq!(fill["fields"]["custodian"], "Fidelity");
    assert_eq!(fill["fields"]["account_type"], "Roth IRA");
    assert_eq!(fill["fields"]["owner"], "Fund <I>");
    assert_eq!(fill["fields"]["note"], "Rollover, 2026.");
    assert!(body.contains("data-dialog-open=\"account\" data-title=\"Edit Growth\""));
}

#[tokio::test]
async fn an_accounts_note_is_in_its_row_and_shown_whole_in_one_shared_bubble() {
    // The product owner, 2026-09-30: "show notes in a bubble when we hover
    // over the line". A note is free text an admin wrote, so it is escaped
    // wherever it is, and it reaches the bubble only as text.
    let written = "Rollover <script>alert(\"x\")</script> & 'Fund I'\nsecond line";
    let mut records = admin_records();
    records.accounts = vec![
        AccountRecord {
            account_id: "ACC-1".into(),
            name: "Growth".into(),
            state: AccountState::Open as i32,
            created_at_ns: T0,
            note: written.into(),
            ..Default::default()
        },
        AccountRecord {
            account_id: "ACC-2".into(),
            name: "Income".into(),
            state: AccountState::Open as i32,
            created_at_ns: T0,
            ..Default::default()
        },
        AccountRecord {
            account_id: "ACC-3".into(),
            name: "Reserve".into(),
            state: AccountState::Closed as i32,
            created_at_ns: T0,
            note: "Wound down.".into(),
            ..Default::default()
        },
    ];
    let h = harness(records, None);
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    let accounts = table(&body, "accounts");
    let row = |id: &str| {
        accounts
            .split(&format!("<tr data-id=\"{id}\""))
            .nth(1)
            .unwrap_or_else(|| panic!("no row {id}"))
            .split("</tr>")
            .next()
            .unwrap()
            .to_string()
    };

    // The note whole in its row, escaped, which is what shows without
    // script, and what a screen reader is given as its marker's description.
    let escaped = "Rollover &lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;Fund I&#39;\nsecond line";
    let growth = row("ACC-1");
    assert!(
        growth.contains(&format!(
            "<td><span class=\"name\">Growth</span><button type=\"button\" class=\"note-mark\" \
             aria-label=\"Note\" aria-describedby=\"account-note-0\"></button>\
             <span class=\"id\">ACC-1</span><span class=\"hint note\" id=\"account-note-0\">{escaped}</span></td>"
        )),
        "{growth}"
    );
    assert!(!body.contains("<script>alert"), "the note is never markup");
    assert!(
        !body.contains(written),
        "nowhere in the page unescaped, not even in a title"
    );

    // Only a row with a note has a marker; a closed account's note is shown
    // as an open one's is.
    let income = row("ACC-2");
    assert!(
        !income.contains("note-mark") && !income.contains("aria-describedby"),
        "{income}"
    );
    assert!(row("ACC-3").contains(
        "<button type=\"button\" class=\"note-mark\" aria-label=\"Note\" \
         aria-describedby=\"account-note-2\"></button>"
    ));
    assert_eq!(body.matches("class=\"note-mark\"").count(), 2);

    // One bubble for the table, empty until a row fills it, and hidden from
    // a screen reader, which has the note from the row.
    assert_eq!(body.matches("class=\"note-bubble\"").count(), 1);
    assert!(body.contains(
        "<div class=\"note-bubble\" id=\"accounts-note-bubble\" aria-hidden=\"true\" hidden></div>"
    ));

    // The page's script puts the note in the bubble as text, and never as
    // markup.
    let script = super::overview::NOTE_SCRIPT;
    assert!(body.contains(script.trim()));
    assert!(script.contains("bubble.textContent = note.textContent;"));
    for markup in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
    ] {
        assert!(!script.contains(markup), "{markup}");
    }
}

#[tokio::test]
async fn with_no_note_there_is_no_bubble() {
    let mut records = admin_records();
    records.accounts = vec![AccountRecord {
        account_id: "ACC-1".into(),
        name: "Growth".into(),
        state: AccountState::Open as i32,
        created_at_ns: T0,
        ..Default::default()
    }];
    let h = harness(records, None);
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    assert!(!body.contains("class=\"note-bubble\""), "no bubble element");
    assert!(!body.contains("class=\"note-mark\""));
}

/// What the Edit on the row `id` fills its dialog with.
fn fill_of(body: &str, id: &str) -> serde_json::Value {
    let row = body
        .split(&format!("<tr data-id=\"{id}\""))
        .nth(1)
        .unwrap_or_else(|| panic!("no row {id}"))
        .split("</tr>")
        .next()
        .unwrap();
    let fill = row
        .split("data-fill=\"")
        .nth(1)
        .unwrap_or_else(|| panic!("no Edit on {row}"))
        .split('"')
        .next()
        .unwrap();
    let json = fill
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&json).unwrap()
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
    assert_eq!(
        parse_entries("oms-1 admin").unwrap()[0].level,
        AccessLevel::Admin as i32
    );
    assert!(parse_entries("oms-1 owner")
        .unwrap_err()
        .contains("not read, write or admin"));
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
        updated_by: String::new(),
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
        .split("<div class=\"setting ")
        .next()
        .unwrap()
}

#[test]
fn the_settings_tab_names_who_last_changed_them_from_the_form_or_the_plugins_page() {
    let mut records = with_settings();
    records.people.push(meridian_domain::v1::SignInRecord {
        subject: ADA.into(),
        display_name: "Ada Park".into(),
        ..Default::default()
    });
    let mut record = snaptrade();
    assert_eq!(
        last_changed(&records, &record),
        "",
        "nothing said while nobody set one"
    );
    record.updated_by = ADA.into();
    record.updated_at_ns = T0;
    let said = last_changed(&records, &record);
    assert!(
        said.contains("Last changed by Ada Park, 2026-09-26 00:00 UTC"),
        "{said}"
    );
    record.updated_by = "local|<grace>".into();
    let unknown = last_changed(&records, &record);
    assert!(
        unknown.contains("local|&lt;grace&gt;"),
        "escaped, by subject: {unknown}"
    );
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
    assert!(
        user.contains("<span class=\"badge info applies\">Only when Key is Commercial key</span>")
    );

    // A default greyed in the empty field, never its value; the unit beside.
    let poll = setting(&form, "poll_seconds");
    assert!(poll.contains("Optional") && poll.contains("Read every"));
    assert!(poll.contains(
        "<input type=\"number\" step=\"1\" name=\"value.poll_seconds\" value=\"900\" placeholder=\"300\" \
         id=\"setting-poll_seconds\" aria-describedby=\"setting-poll_seconds-about\">"
    ));
    assert!(poll.contains("<span class=\"unit\">seconds</span>"));
    assert!(poll.contains(
        "<span class=\"badge\" title=\"Left empty, the plugin uses 300 seconds.\">default 300 seconds</span>"
    ));
    let stale = setting(&form, "stale_after_hours");
    assert!(stale.contains("value=\"\" placeholder=\"24\""), "{stale}");
    assert!(stale.contains("<span class=\"unit\">hours</span>"));
    assert!(
        form.contains("What poll_seconds is &lt;for&gt;."),
        "escaped"
    );

    // A developer's setting only on a development deployment, under
    // "Developer".
    assert!(!form.contains("data-setting=\"synthetic\""));
    assert!(!form.contains("<details"));
    let developing = settings::form(&record, "", true);
    let synthetic = setting(&developing, "synthetic");
    assert!(synthetic.contains("name=\"value.synthetic\""));
    let details = developing
        .split("<details class=\"developer\">")
        .nth(1)
        .unwrap();
    assert!(details.starts_with("<summary>Developer "), "{details}");
    assert!(details.contains("data-setting=\"synthetic\""));
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
    assert!(key.contains(
        "<span class=\"badge\" title=\"Not set, the plugin uses Personal key.\">default Personal key</span>"
    ));
}

/// The part of `body` between `open` and the first `close` after it.
fn between<'a>(body: &'a str, open: &str, close: &str) -> &'a str {
    body.split(open)
        .nth(1)
        .unwrap_or_else(|| panic!("no {open} in {body}"))
        .split(close)
        .next()
        .unwrap()
}

#[test]
fn the_form_fits_one_screen_without_losing_what_it_says() {
    // The product owner, 2026-09-30: "let's compact ... a bit and make the
    // form short enough to display in one page".
    let record = snaptrade();
    let form = settings::form(&record, "", true);

    // Long fields across the form: the choice that decides which follow,
    // and every secret. Short ones two to a row: numbers and an on/off.
    for (name, size) in [
        ("key_type", "wide"),
        ("snaptrade_client_id", "wide"),
        ("snaptrade_consumer_key", "wide"),
        ("user_secret", "wide"),
        ("poll_seconds", "short"),
        ("stale_after_hours", "short"),
        ("synthetic", "short"),
    ] {
        assert!(
            form.contains(&format!(
                "<div class=\"setting {size}\" data-setting=\"{name}\""
            )),
            "{name} is {size}"
        );
    }
    let fields = between(&form, "<div class=\"fields\">", "<details");
    assert!(!fields.contains("data-setting=\"synthetic\""));

    // A choice's options on one line, each its label alone: what each
    // means is in the field's hint.
    let key = setting(&form, "key_type");
    let options = between(key, "<div class=\"options\">", "</div>");
    assert_eq!(options.matches("<label class=\"option\">").count(), 2);
    assert!(!options.contains("hint"), "{options}");
    assert!(key.contains("<fieldset class=\"choice\" aria-describedby=\"setting-key_type-about\">"));
    assert!(key.contains("Personal key: Belongs to one user.\nCommercial key: Registers users"));

    // Each hint one small line under its field, which the field and its
    // marker are described by, and no other paragraph in the form.
    let client = setting(&form, "snaptrade_client_id");
    assert!(client.contains(
        "<label class=\"setting-label\" for=\"setting-snaptrade_client_id\">Client ID</label>\
         <button type=\"button\" class=\"note-mark\" aria-label=\"About Client ID\" \
         aria-describedby=\"setting-snaptrade_client_id-about\"></button>"
    ));
    assert!(client.contains(
        "id=\"setting-snaptrade_client_id\" autocomplete=\"new-password\" \
         placeholder=\"Type a new value to replace it\" aria-describedby=\"setting-snaptrade_client_id-about\">"
    ));
    assert!(client.contains(
        "<p class=\"hint about\" id=\"setting-snaptrade_client_id-about\">What snaptrade_client_id is \
         &lt;for&gt;.\nA secret: never shown again once set.</p>"
    ));
    // Clearing a secret sits beside its field.
    assert!(client.contains(
        "aria-describedby=\"setting-snaptrade_client_id-about\"><label class=\"check\">\
         <input type=\"checkbox\" name=\"clear.snaptrade_client_id\"> Clear it</label></span>"
    ));
    let paragraphs = form.matches("<p").count();
    assert_eq!(paragraphs, form.matches("<p class=\"hint about\"").count());
    assert_eq!(paragraphs, 7, "a hint for each field, each described");
    // Required or optional, set or not, the default, the unit and when it
    // applies stay in view, small.
    let user = setting(&form, "user_secret");
    assert!(user.contains(
        "<span class=\"badge warn\">Required</span> <span class=\"badge\">not set</span> \
         <span class=\"badge info applies\">Only when Key is Commercial key</span>"
    ));
    let stale = setting(&form, "stale_after_hours");
    assert!(stale.contains("<span class=\"badge\">Optional</span>"));
    assert!(stale.contains(">default 24 hours</span>"));
    assert!(stale.contains("<span class=\"unit\">hours</span>"));

    // The hints' one bubble, as an account's note: filled as text, and
    // hidden from a screen reader, which has the hint from the field.
    assert_eq!(form.matches("class=\"note-bubble").count(), 1);
    assert!(form.contains(
        "<div class=\"note-bubble hints\" id=\"settings-hint-bubble\" aria-hidden=\"true\" hidden></div>"
    ));
    let script = between(&form, "<script>", "</script>");
    assert!(script.contains("form.classList.add(\"js\");"));
    assert!(script.contains("bubble.textContent = about.textContent;"));
    assert!(!script.contains("innerHTML"));
    for shown_by in ["\"focusin\"", "\"mouseover\"", "\"click\"", "\"Escape\""] {
        assert!(script.contains(shown_by), "{shown_by}");
    }
    // A field that does not apply still collapses by the script alone.
    assert!(script.contains("holder.hidden = oneOf.indexOf("));

    // A developer's settings under a closed "Developer", and Save last.
    assert!(form.contains(
        "<details class=\"developer\"><summary>Developer \
         <span class=\"summary-note\">1 setting for whoever develops the plugin</span></summary>"
    ));
    assert!(form.contains(
        "</details><div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save settings</button>\
         </div></form>"
    ));

    // Open while a developer's setting is required and missing.
    let mut needed = snaptrade();
    needed
        .declared_settings
        .iter_mut()
        .find(|declaration| declaration.name == "synthetic")
        .unwrap()
        .required = true;
    assert!(settings::form(&needed, "", true).contains("<details class=\"developer\" open>"));
}

/// What a browser posts of a form as it stands: each named input's value, a
/// radio or a box only when checked, and a select's option selected, or its
/// first.
fn submitted(form: &str) -> Fields {
    let attribute = |tag: &str, name: &str| -> Option<String> {
        tag.split(&format!(" {name}=\""))
            .nth(1)
            .map(|rest| rest.split('"').next().unwrap().to_string())
    };
    let mut posted = Fields::new();
    for tag in form
        .split("<input")
        .skip(1)
        .map(|rest| rest.split('>').next().unwrap())
    {
        let Some(name) = attribute(tag, "name") else {
            continue;
        };
        let kind = attribute(tag, "type").unwrap_or_default();
        let ticked = tag.contains(" checked");
        let value = match kind.as_str() {
            "radio" | "checkbox" if !ticked => continue,
            "checkbox" => attribute(tag, "value").unwrap_or_else(|| "on".into()),
            _ => attribute(tag, "value").unwrap_or_default(),
        };
        posted.insert(name, value);
    }
    for select in form.split("<select").skip(1) {
        let (open, rest) = select.split_once('>').unwrap();
        let options = rest.split("</select>").next().unwrap();
        let chosen = options
            .split("<option")
            .skip(1)
            .find(|option| option.contains(" selected"))
            .or_else(|| options.split("<option").nth(1))
            .unwrap();
        posted.insert(
            attribute(open, "name").unwrap(),
            attribute(chosen, "value").unwrap(),
        );
    }
    posted
}

#[test]
fn the_compact_form_posts_what_the_form_always_did() {
    let record = snaptrade();
    for development in [false, true] {
        let form = settings::form(&record, "", development);
        let posted = submitted(&form);
        let mut names: Vec<&str> = posted.keys().map(String::as_str).collect();
        names.sort_unstable();
        // Every field the form had, a hidden one too: the user secret under
        // a personal key, and a developer's setting in its closed section.
        let mut expected = vec![
            "secret.snaptrade_client_id",
            "secret.snaptrade_consumer_key",
            "secret.user_secret",
            "value.key_type",
            "value.poll_seconds",
            "value.stale_after_hours",
        ];
        if development {
            expected.push("value.synthetic");
        }
        expected.sort_unstable();
        assert_eq!(names, expected, "development: {development}");
        assert_eq!(posted["value.key_type"], "personal");
        assert_eq!(posted["value.poll_seconds"], "900");
        assert_eq!(posted["value.stale_after_hours"], "");
        // Saved as it stands, it asks for nothing.
        assert_eq!(
            settings::request(&record, &posted, development),
            None,
            "development: {development}"
        );
    }

    // A developer's setting changed under "Developer" is saved on a
    // development deployment, and nowhere else.
    let mut posted = submitted(&settings::form(&record, "", true));
    posted.insert("value.synthetic".into(), "true".into());
    let saved = settings::request(&record, &posted, true).unwrap();
    assert_eq!(saved.values.len(), 1);
    assert_eq!(
        (
            saved.values[0].name.as_str(),
            saved.values[0].value.as_str()
        ),
        ("synthetic", "true")
    );
    assert_eq!(settings::request(&record, &posted, false), None);

    // Clear it, beside its field, clears the secret that is set.
    let mut posted = submitted(&settings::form(&record, "", false));
    posted.insert("clear.snaptrade_client_id".into(), "on".into());
    let cleared = settings::request(&record, &posted, false).unwrap();
    assert_eq!(cleared.cleared, ["snaptrade_client_id"]);
    assert!(cleared.values.is_empty());
}

#[tokio::test]
async fn each_section_adds_with_a_short_button_and_its_dialog_says_what_is_made() {
    // The product owner, 2026-09-30: "maybe replace 'New access group', 'New
    // user group', and 'New account group' to '+ Add' as well and just use
    // Access / User / Account?"
    let h = framing(a_firm());
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    for (section, heading, dialog, what, title) in [
        (
            "permissions",
            "Permissions",
            "new-permission",
            "a permission",
            "Grant a permission",
        ),
        (
            "user-groups",
            "User",
            "user-group",
            "a user group",
            "New user group",
        ),
        (
            "account-groups",
            "Account",
            "account-group",
            "an account group",
            "New account group",
        ),
        (
            "access-groups",
            "Access",
            "access-group",
            "an access group",
            "New access group",
        ),
        (
            "accounts",
            "Accounts",
            "account",
            "an account",
            "New account",
        ),
    ] {
        let part = between(
            &body,
            &format!("<section class=\"admin-section\" id=\"{section}\">"),
            "</section>",
        );
        assert!(part.contains(&format!("<h2>{heading}</h2>")), "{section}");
        assert!(
            part.contains(&format!(
                "<button type=\"button\" class=\"primary\" data-dialog-open=\"{dialog}\" \
                 aria-label=\"Add {what}\">+ Add</button>"
            )),
            "{section}"
        );
        // The dialog is named by its heading, which says it in full.
        assert!(
            part.contains(&format!(
                "<dialog id=\"{dialog}\" aria-labelledby=\"{dialog}-title\">"
            )),
            "{section}"
        );
        assert!(
            part.contains(&format!(
                "<h2 id=\"{dialog}-title\" data-title-new=\"{title}\">{title}</h2>"
            )),
            "{section}"
        );
        assert!(!part.contains(&format!(">{title}</button>")), "{section}");
    }
    // The tabs still say which groups.
    let tabs = between(&body, "<nav class=\"tabs\">", "</nav>");
    for tab in ["User groups", "Account groups", "Access groups"] {
        assert!(tabs.contains(&format!(">{tab}</a>")), "{tab}");
    }
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
    assert_eq!(names, ["Overview", "Settings", "Access"]);
    assert_eq!(here, "Overview");
    assert_eq!(tabs[0].0, VIEW);
    assert!(body.contains("id=\"health\""));
    for (part, tab) in [("id=\"settings\"", "settings"), ("id=\"access\"", "access")] {
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
            built_in: false,
        });
    records
        .access_groups
        .push(meridian_domain::v1::AccessGroup {
            access_group_id: "AG-1".into(),
            name: "Custody readers".into(),
            entries: vec![meridian_domain::v1::AccessEntry {
                plugin_instance_id: "snaptrade-1".into(),
                level: meridian_access::AccessLevel::Read as i32,
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
                    ..Default::default()
                })
                .collect(),
        },
    );

    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let health = body.split("id=\"health\"").nth(1).unwrap();
    assert!(health.contains("Not healthy"), "{health}");
    assert!(health.contains("required setting snaptrade_consumer_key is not set"));
    // The count leads to the plugin's pages under Manage, where it links
    // them (W6.4, W6.10), and to nothing of the dashboard's.
    assert!(
        health.contains(
            "5 external accounts not linked. <a href=\"/plugins/snaptrade-1?level=admin\">Link them \
             on the plugin's pages, under Manage</a>."
        ),
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
    // Ada's link to All plugins (admin) is a row of its own, at admin, on
    // no account; and she is offered the way to grant, being a deployment
    // admin.
    assert!(
        access.contains("through All plugins (admin)") && access.contains(">admin<"),
        "{access}"
    );
    assert!(body.contains("A deployment admin holds nothing on it by being one."));
    assert!(body.contains("href=\"/admin#permissions\""));

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
            pages: pages
                .iter()
                .map(|(path, title)| meridian_pb::v1::PageDeclaration {
                    path: path.to_string(),
                    title: title.to_string(),
                    levels: vec![meridian_access::AccessLevel::Admin as i32],
                })
                .collect(),
        }),
        ..Default::default()
    }
}

/// W6.9, sdk-contract/a-plugin-has-admins: the admin view keeps the tabs
/// every plugin has and frames none of the plugin's pages, which are in its
/// area; it links there.
#[tokio::test]
async fn the_view_keeps_the_tabs_every_plugin_has_and_links_to_the_plugins_area() {
    let h = framing(with_settings());
    h.app.health.hear(
        "snaptrade-1",
        declaring(&[
            ("/admin/connections", "Connections"),
            ("/admin/accounts", "Account links"),
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
        ]
    );
    assert_eq!(here, "Overview");
    assert!(!body.contains("<iframe"), "none of the plugin's pages");
    assert!(
        body.contains(
            "<a class=\"button\" href=\"/plugins/snaptrade-1?level=admin\" data-area>Its pages</a>"
        ),
        "{body}"
    );
    // The flag for its unlinked accounts leads there too, under Manage.
    let (_, asked) = send(&h, get(&h, &format!("{VIEW}?tab=connections"), true)).await;
    assert_eq!(tabs_of(&asked).1, "Overview", "no tab of the plugin's own");

    // The breadcrumb: the settings home, its plugins, then this one.
    let head = body.split("</header>").next().unwrap();
    assert!(
        head.contains(
            "<a href=\"/admin\">Settings</a><span class=\"sep\" aria-hidden=\"true\">/</span>\
         <a href=\"/admin#plugins\">Plugins</a><span class=\"sep\" aria-hidden=\"true\">/</span>\
         <span class=\"here\" aria-current=\"page\" title=\"snaptrade-1\">snaptrade-1</span>"
        ),
        "{head}"
    );
}

/// Ada in a user group granted SnapTrade's `admin` alone, and not a
/// deployment admin.
fn plugin_admin_alone() -> AccessRecords {
    let mut records = with_settings();
    records.permissions = vec![Permission {
        permission_id: "P-9".into(),
        user_group_id: "UG-1".into(),
        account_group_id: String::new(),
        access_group_id: "AG-ADMIN".into(),
    }];
    records.access_groups = vec![meridian_domain::v1::AccessGroup {
        access_group_id: "AG-ADMIN".into(),
        name: "SnapTrade admins".into(),
        entries: vec![meridian_domain::v1::AccessEntry {
            plugin_instance_id: "snaptrade-1".into(),
            level: meridian_access::AccessLevel::Admin as i32,
        }],
        built_in: false,
    }];
    records
}

#[tokio::test]
async fn a_plugin_admin_reaches_its_tabs_and_settings_and_nothing_else_of_the_portal() {
    let h = harness(plugin_admin_alone(), None);
    let (status, body) = send(&h, get(&h, VIEW, true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tabs_of(&body).0.len(), 3, "Overview, Settings and Access");
    let (status, settings) = send(&h, get(&h, &format!("{VIEW}?tab=settings"), true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        settings.contains(&format!("action=\"{SETTINGS}\"")),
        "{settings}"
    );
    // Access she reads, and changes nothing on: only a deployment admin grants.
    let (_, access) = send(&h, get(&h, &format!("{VIEW}?tab=access"), true)).await;
    assert!(
        access.contains("A deployment admin grants access."),
        "{access}"
    );
    assert!(!access.contains("href=\"/admin#permissions\""));
    // Her way back is Home, and there is no way to the deployment's settings.
    let head = body.split("</header>").next().unwrap();
    assert!(
        head.contains("<a href=\"/\">Home</a>") && !head.contains("href=\"/admin\""),
        "{head}"
    );
    for page in ["/admin", "/admin/plugins/another-1"] {
        let (status, _) = send(&h, get(&h, page, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    }
    // Her settings reach the conductor as hers.
    let asked = conductor_setting(&h, None);
    let (status, _) = send(
        &h,
        post(
            &h,
            SETTINGS,
            &format!("form_token={}&value.poll_seconds=600", h.form_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_deployment_admin_whose_link_is_withdrawn_reaches_no_plugins_settings() {
    let mut records = with_settings();
    records
        .permissions
        .retain(|p| p.access_group_id == DEPLOYMENT_ADMIN);
    let h = harness(records, None);
    // What is theirs on it: its health and granting.
    let (status, body) = send(&h, get(&h, VIEW, true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<String> = tabs_of(&body).0.into_iter().map(|(_, name)| name).collect();
    assert_eq!(names, ["Overview", "Access"]);
    assert!(!body.contains("data-area"), "no pages of its to open");
    let (_, access) = send(&h, get(&h, &format!("{VIEW}?tab=access"), true)).await;
    assert!(access.contains("href=\"/admin#permissions\""));
    // And not its settings.
    let (status, _) = send(&h, get(&h, SETTINGS, true)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let asked = conductor_setting(&h, None);
    let (status, _) = send(
        &h,
        post(
            &h,
            SETTINGS,
            &format!("form_token={}&value.poll_seconds=600", h.form_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(asked.lock().unwrap().is_empty());
}

/// The product owner, 2026-09-30: "let's put 'Its sidecar has stopped
/// reporting.' in note when hovering the silent button (similar concept for
/// other 'notes')". A health badge is a button described by its detail,
/// which is a line under it without script and, with it, the bubble's.
#[tokio::test]
async fn a_health_badge_carries_its_detail_as_its_note() {
    let h = harness(with_settings(), None);
    // Its sidecar last said so 91 seconds ago: three reports missed.
    h.app.health.hear(
        "snaptrade-1",
        meridian_domain::v1::PluginReport {
            reported_at_ns: T0 - 91_000_000_000,
            ..declaring(&[])
        },
    );
    let silent = "<button type=\"button\" class=\"badge warn\" data-note \
                  aria-describedby=\"plugin-health-0\">Silent</button>\
                  <span class=\"hint noted\" id=\"plugin-health-0\">Its sidecar has stopped reporting.</span>";
    let (_, overview) = send(&h, get(&h, "/admin", true)).await;
    let listed = table(&overview, "plugins");
    assert!(listed.contains(silent), "{listed}");
    // The note is not also a line of its own once the script runs, and the
    // one bubble is drawn by the chrome's script, as text.
    assert!(overview.contains("html[data-script] .noted{display:none}"));
    assert!(overview.contains("bubble.textContent = text;"));

    // The same in the view's Health panel, where the detail was a paragraph.
    let (_, view) = send(&h, get(&h, VIEW, true)).await;
    let health = view.split("id=\"health\"").nth(1).unwrap();
    let health = health.split("</section>").next().unwrap();
    assert!(
        health.contains(
            "<h2>Health</h2><button type=\"button\" class=\"badge warn\" data-note \
             aria-describedby=\"health-note\">Silent</button></div>\
             <span class=\"hint noted\" id=\"health-note\">Its sidecar has stopped reporting.</span>"
        ),
        "{health}"
    );
    assert!(!health.contains("<p>Its sidecar"), "{health}");

    // Healthy, with nothing to say, the badge is only a badge.
    h.app.health.hear("snaptrade-1", declaring(&[]));
    let (_, view) = send(&h, get(&h, VIEW, true)).await;
    assert!(view.contains("<span class=\"badge good\">Healthy</span>"));
    assert!(
        !view.contains("id=\"health-note\"")
            && !view.contains("<button type=\"button\" class=\"badge"),
        "no note to show"
    );
}

#[tokio::test]
async fn a_connections_state_carries_what_the_plugin_said_of_it_as_its_note() {
    let h = harness(with_settings(), None);
    h.app.custody.hear_sync(
        "snaptrade-1",
        SyncStatusEvent {
            external_account_id: "SNAP-1".into(),
            account_id: "ACC-1".into(),
            state: SyncState::NeedsSignIn as i32,
            status_detail: "the daily <sign-in> has lapsed".into(),
            ..Default::default()
        },
    );
    let (_, body) = send(&h, get(&h, VIEW, true)).await;
    let sync = table(&body, "sync");
    assert!(
        sync.contains(
            "<td><button type=\"button\" class=\"pill warn\" data-note aria-describedby=\"sync-note-0\">\
             Needs sign-in</button><span class=\"hint noted\" id=\"sync-note-0\">the daily &lt;sign-in&gt; \
             has lapsed</span></td>"
        ),
        "{sync}"
    );
    assert!(!sync.contains("<th>Detail</th>"), "the note, not a column");
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
async fn the_plugins_tab_names_the_instance_apart_and_offers_a_search() {
    let h = harness(admin_records(), None);
    h.app.custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: vec![ExternalAccount {
                external_account_id: "SNAP-1".into(),
                name: "Individual Brokerage 1234".into(),
                venue_account_type: "Individual".into(),
                ..Default::default()
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

// ── Built for a hundred and more (the product owner, 2026-09-30) ────────────

const ACCOUNTS: usize = 500;
const PEOPLE: usize = 200;
const GROUPS: usize = 30;

/// A deployment of a realistic size: 500 accounts (every tenth closed), 200
/// people across 30 user groups, 30 account groups of 50 accounts, and 30
/// access groups, each over four of 33 plugins.
fn a_firm() -> AccessRecords {
    let mut records = admin_records();
    records.accounts = (0..ACCOUNTS)
        .map(|i| AccountRecord {
            account_id: format!("ACC-{i:04}"),
            name: format!("Account {:04}", ACCOUNTS - 1 - i),
            custodian: if i % 2 == 0 { "Fidelity" } else { "Schwab" }.into(),
            state: if i % 10 == 0 {
                AccountState::Closed
            } else {
                AccountState::Open
            } as i32,
            created_at_ns: T0,
            ..Default::default()
        })
        .collect();
    // People from a directory, and one whose subject is a distinguished name.
    let login = |i: usize| match i {
        0 => "ldap:dc=firm,dc=internal|uid=zed,ou=people,dc=firm,dc=internal".to_string(),
        _ => format!("https://idp.example.org|user-{:03}", PEOPLE - i),
    };
    records.user_groups.extend((0..GROUPS).map(|g| UserGroup {
        user_group_id: format!("UG-{:02}", g + 2),
        name: format!("Desk {g:02}"),
        directory_groups: vec![format!("cn=desk-{g},ou=groups,dc=firm")],
        logins: (0..PEOPLE).filter(|i| i % GROUPS == g).map(login).collect(),
    }));
    records.account_groups = (0..GROUPS)
        .map(|g| meridian_domain::v1::AccountGroup {
            account_group_id: format!("AcG-{g:02}"),
            name: format!("Book {g:02}"),
            account_ids: (0..50).map(|k| format!("ACC-{:04}", g * 10 + k)).collect(),
            built_in: false,
        })
        .collect();
    records.access_groups = (0..GROUPS)
        .map(|g| meridian_domain::v1::AccessGroup {
            access_group_id: format!("AG-{g:02}"),
            name: format!("Access {g:02}"),
            entries: (0..4)
                .map(|k| meridian_domain::v1::AccessEntry {
                    plugin_instance_id: format!("plugin-{:02}", (g + k) % 40),
                    level: if k == 0 {
                        AccessLevel::Write
                    } else {
                        AccessLevel::Read
                    } as i32,
                })
                .collect(),
            built_in: false,
        })
        .collect();
    records
}

#[tokio::test]
async fn every_option_is_in_the_page_once_however_many_records_there_are() {
    let h = harness(a_firm(), None);
    let (status, body) = send(&h, get(&h, "/admin", true)).await;
    assert_eq!(status, StatusCode::OK);
    // One dialog per kind of record, whatever the number of records.
    for kind in ["user-group", "account-group", "access-group", "account"] {
        assert_eq!(
            body.matches(&format!(
                "<dialog id=\"{kind}\" aria-labelledby=\"{kind}-title\">"
            ))
            .count(),
            1,
            "{kind}"
        );
    }
    assert!(!body.contains("<dialog id=\"edit-"), "no dialog per record");
    // So each option is there once: every account, every person named, every plugin.
    assert_eq!(body.matches("name=\"account_ids\"").count(), ACCOUNTS);
    // Admins' Ada and the 200.
    assert_eq!(body.matches("name=\"login\"").count(), PEOPLE + 1);
    assert_eq!(body.matches("name=\"plugin\"").count(), 33);
    assert_eq!(body.matches("<select name=\"level.").count(), 33);
    assert!(
        body.len() < 1 << 20,
        "the page is {} bytes for {ACCOUNTS} accounts and {PEOPLE} people",
        body.len()
    );
    // A closed account is an option only while a group holds it.
    let closed = body
        .split("value=\"ACC-0010\"")
        .next()
        .unwrap()
        .rsplit("<div class=\"picker-option\"")
        .next()
        .unwrap();
    assert!(
        closed.starts_with(" data-also=\"Fidelity  \" data-only-when-chosen>"),
        "{closed}"
    );
    // Each Edit carries its record.
    let fill = fill_of(&body, "AcG-03");
    assert_eq!(fill["fields"]["name"], "Book 03");
    assert_eq!(fill["checked"]["account_ids"].as_array().unwrap().len(), 50);
}

#[tokio::test]
async fn people_are_listed_by_user_id_then_login_id_and_found_by_either_or_their_name() {
    let accounts = crate::accounts::InMemory::default();
    for (name, display) in [("mo", "Mo Local"), ("ada", "")] {
        crate::accounts::Accounts::put(
            &accounts,
            &crate::accounts::LocalAccount {
                name: name.into(),
                display_name: display.into(),
                password_hash: "x".into(),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let h = harness_holding(a_firm(), None, None, Some(Arc::new(accounts)));
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    let picker = body
        .split("id=\"user-group-people\"")
        .nth(1)
        .unwrap()
        .split("</fieldset>")
        .next()
        .unwrap();
    let logins: Vec<&str> = picker
        .split("name=\"login\" value=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap())
        .collect();
    assert_eq!(
        logins.len(),
        PEOPLE + 3,
        "the groups' people, and two local accounts"
    );
    // By user ID: 8812 (Ada's directory login), then ada (local), mo, the
    // directory's user-001.. and zed, the user ID of a distinguished name.
    assert_eq!(logins[0], ADA);
    assert_eq!(logins[1], "local|ada");
    assert_eq!(logins[2], "local|mo");
    assert_eq!(logins[3], "https://idp.example.org|user-001");
    assert_eq!(
        logins.last().unwrap(),
        &"ldap:dc=firm,dc=internal|uid=zed,ou=people,dc=firm,dc=internal"
    );
    // Each shows its user ID and name, its login small beside, all searched.
    assert!(picker.contains(
        "value=\"local|mo\"> <span class=\"option-label\">mo (Mo Local)</span> \
         <span class=\"id\">local|mo</span>"
    ));
    assert!(picker.contains(
        "<span class=\"option-label\">zed</span> \
         <span class=\"id\">ldap:dc=firm,dc=internal|uid=zed,ou=people,dc=firm,dc=internal</span>"
    ));
    // A group's row names its people by user ID, the first few, and how
    // many more, the rest there for the search.
    let row = body
        .split("<tr data-id=\"UG-02\"")
        .nth(1)
        .unwrap()
        .split("</tr>")
        .next()
        .unwrap();
    assert!(
        row.contains("<td>user-020, user-050, user-080<span class=\"more\"> and 4 more</span>"),
        "{row}"
    );
    assert!(row.contains("https://idp.example.org|user-170") && row.contains("uid=zed"));
}

#[tokio::test]
async fn every_list_on_the_settings_page_is_searchable_sortable_and_says_when_none_match() {
    let h = harness(a_firm(), None);
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    for (id, noun) in [
        ("permissions-table", "permissions"),
        ("user-groups-table", "user groups"),
        ("account-groups-table", "account groups"),
        ("access-groups-table", "access groups"),
        ("accounts-table", "accounts"),
    ] {
        assert!(
            body.contains(&format!("data-filter=\"{id}\" hidden")),
            "{id}"
        );
        assert!(
            body.contains(&format!("aria-label=\"Search {noun}\"")),
            "{id}"
        );
        assert!(
            body.contains(&format!("data-filter-count=\"{id}\"")),
            "{id}"
        );
        assert!(
            body.contains(&format!("id=\"{id}\" data-sortable>")),
            "{id}"
        );
        assert!(
            body.contains(&format!(
                "<p class=\"empty\" data-filter-none=\"{id}\" hidden>No {noun} match that search.</p>"
            )),
            "{id}"
        );
    }
    // In the order a person reads them: accounts by name.
    let accounts = table(&body, "accounts");
    let first = accounts.split("<tr data-id=\"").nth(1).unwrap();
    assert!(
        first.starts_with("ACC-0499\""),
        "Account 0000 first: {}",
        &first[..40]
    );
}

#[test]
fn logins_are_taken_whole_when_chosen_and_split_only_between_logins_when_typed() {
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    };
    let dn = "ldap:dc=firm,dc=internal|uid=ada,ou=people,dc=firm,dc=internal";
    assert_eq!(
        logins_of(&pairs(&[
            ("login", dn),
            ("login", "local|bob"),
            (
                "logins",
                "local|cy, local|di\nldap:dc=firm|uid=ed,ou=people\n\nlocal|bob"
            ),
        ])),
        [
            dn,
            "local|bob",
            "local|cy",
            "local|di",
            "ldap:dc=firm|uid=ed,ou=people",
        ]
    );
    // As the form before the picker sent them.
    assert_eq!(
        logins_of(&pairs(&[("logins", "local|nobody-e2e")])),
        ["local|nobody-e2e"]
    );
}

#[test]
fn an_access_group_gives_each_plugin_chosen_exactly_one_level() {
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    };
    let entries = entries_of(&pairs(&[
        ("plugin", "oms-1"),
        ("level.oms-1", "write"),
        ("plugin", "snaptrade-1"),
        ("level.snaptrade-1", "read"),
        ("level.unchosen-1", "write"),
    ]))
    .unwrap();
    let got: Vec<(&str, i32)> = entries
        .iter()
        .map(|e| (e.plugin_instance_id.as_str(), e.level))
        .collect();
    assert_eq!(
        got,
        [
            ("oms-1", AccessLevel::Write as i32),
            ("snaptrade-1", AccessLevel::Read as i32)
        ],
        "a level beside a plugin not chosen gives nothing"
    );
    // The lines the form took before, still.
    assert_eq!(
        entries_of(&pairs(&[("entries", "oms-1 read")]))
            .unwrap()
            .len(),
        1
    );
    // One level each: never twice, however it is sent.
    for twice in [
        pairs(&[
            ("plugin", "oms-1"),
            ("level.oms-1", "read"),
            ("entries", "oms-1 write"),
        ]),
        pairs(&[("entries", "oms-1 read\noms-1 write")]),
        pairs(&[
            ("plugin", "oms-1"),
            ("plugin", "oms-1"),
            ("level.oms-1", "read"),
        ]),
    ] {
        let refused = entries_of(&twice).unwrap_err();
        assert!(
            refused.contains("named twice") && refused.contains("write includes read"),
            "{refused}"
        );
    }
    assert!(entries_of(&pairs(&[("plugin", "oms-1")]))
        .unwrap_err()
        .contains("no level"));
    assert!(
        entries_of(&pairs(&[("plugin", "oms-1"), ("level.oms-1", "owner")]))
            .unwrap_err()
            .contains("not admin, read or write")
    );
    // Admin, alone or beside one data level (W6.7).
    let levels = |choice: &str| -> Vec<i32> {
        entries_of(&pairs(&[("plugin", "oms-1"), ("level.oms-1", choice)]))
            .unwrap()
            .into_iter()
            .map(|entry| entry.level)
            .collect()
    };
    assert_eq!(levels("admin"), [AccessLevel::Admin as i32]);
    assert_eq!(
        levels("admin-read"),
        [AccessLevel::Admin as i32, AccessLevel::Read as i32]
    );
    assert_eq!(
        levels("admin-write"),
        [AccessLevel::Admin as i32, AccessLevel::Write as i32]
    );
}

/// The define commands the page sends, as the fake conductor received them.
fn conductor_defining(h: &Harness, topic: &'static str) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let kept: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let keeping = Arc::clone(&kept);
    h.app.bus.serve(topic, move |envelope| {
        keeping.lock().unwrap().push(envelope.payload.clone());
        Ok(("".into(), Vec::new()))
    });
    kept
}

#[tokio::test]
async fn the_access_group_form_sends_one_level_per_plugin_and_refuses_two() {
    let h = harness(admin_records(), None);
    let sent = conductor_defining(&h, "platform.config.command.define-access-group");
    let form = format!(
        "form_token={}&access_group_id=&name=Traders&plugin=oms-1&level.oms-1=write\
         &plugin=snaptrade-1&level.snaptrade-1=read&level.other-1=write",
        h.form_token
    );
    let (status, _) = send(&h, post(&h, "/admin/access-groups", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let request = DefineAccessGroupRequest::decode(&sent.lock().unwrap()[0][..]).unwrap();
    let group = request.access_group.unwrap();
    assert_eq!(group.name, "Traders");
    let levels: Vec<(String, i32)> = group
        .entries
        .into_iter()
        .map(|e| (e.plugin_instance_id, e.level))
        .collect();
    assert_eq!(
        levels,
        [
            ("oms-1".to_string(), AccessLevel::Write as i32),
            ("snaptrade-1".to_string(), AccessLevel::Read as i32)
        ]
    );

    let twice = format!(
        "form_token={}&name=Traders&plugin=oms-1&level.oms-1=read&entries=oms-1+write",
        h.form_token
    );
    let (status, body) = send(&h, post(&h, "/admin/access-groups", &twice)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("named twice"), "{body}");
    assert_eq!(sent.lock().unwrap().len(), 1, "nothing more was sent");
}

#[tokio::test]
async fn the_user_group_form_sends_the_people_chosen_and_those_typed_in() {
    let h = harness(admin_records(), None);
    let sent = conductor_defining(&h, "platform.config.command.define-user-group");
    let dn = "ldap:dc=firm,dc=internal|uid=ada,ou=people,dc=firm,dc=internal";
    let form = format!(
        "form_token={}&user_group_id=UG-9&name=Ops&login={}&login=local%7Cbob\
         &logins=local%7Ccy%0Alocal%7Cbob&directory_groups=cn%3Dops%2Cou%3Dgroups",
        h.form_token,
        dn.replace('|', "%7C")
            .replace(',', "%2C")
            .replace('=', "%3D")
            .replace(':', "%3A")
    );
    let (status, _) = send(&h, post(&h, "/admin/user-groups", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let request = DefineUserGroupRequest::decode(&sent.lock().unwrap()[0][..]).unwrap();
    let group = request.user_group.unwrap();
    assert_eq!(group.user_group_id, "UG-9");
    assert_eq!(group.logins, [dn, "local|bob", "local|cy"]);
    assert_eq!(group.directory_groups, ["cn=ops,ou=groups"]);
}

#[tokio::test]
async fn each_group_dialog_is_a_picker_that_is_a_plain_list_without_script() {
    let h = framing(a_firm());
    let (_, body) = send(&h, get(&h, "/admin", true)).await;
    for (dialog, picker, name) in [
        ("user-group", "user-group-people", "login"),
        ("account-group", "account-group-accounts", "account_ids"),
        ("access-group", "access-group-plugins", "plugin"),
    ] {
        let inside = body
            .split(&format!(
                "<dialog id=\"{dialog}\" aria-labelledby=\"{dialog}-title\">"
            ))
            .nth(1)
            .unwrap()
            .split("</dialog>")
            .next()
            .unwrap();
        assert!(
            inside.contains(&format!(
                "<fieldset class=\"checks picker\" id=\"{picker}\" data-picker>"
            )),
            "{dialog}"
        );
        assert!(
            inside.contains("<div class=\"picker-tools\" hidden>"),
            "{dialog}"
        );
        assert!(
            inside.contains(&format!("type=\"checkbox\" name=\"{name}\"")),
            "{dialog}"
        );
        assert!(
            inside.contains("data-record-id>"),
            "{dialog}: its id is cleared for a new one"
        );
        assert!(
            !inside.contains(" checked"),
            "{dialog}: a new one chooses nothing"
        );
    }
    // An access entry's level: one choice, admin, a data level, or admin
    // beside one, never read and write both.
    assert!(body.contains(
        "<select name=\"level.plugin-00\" aria-label=\"Level on plugin-00\"><option value=\"read\">Read</option>\
         <option value=\"write\">Write (includes read)</option>\
         <option value=\"admin\">Admin (configures it, no account)</option>\
         <option value=\"admin-read\">Admin and read</option>\
         <option value=\"admin-write\">Admin and write</option></select>"
    ));
    let fill = fill_of(&body, "AG-00");
    assert_eq!(fill["fields"]["level.plugin-00"], "write");
    assert_eq!(fill["fields"]["level.plugin-01"], "read");
    assert_eq!(fill["checked"]["plugin"].as_array().unwrap().len(), 4);
    // The page's script: the pickers, and a dialog filled from its Edit.
    for held in [
        "Array.prototype.forEach.call(document.querySelectorAll(\"[data-picker]\"), picker);",
        "form.reset();",
        "function (el) { el.value = \"\"; }",
        "if (ticked[el.name]) el.checked = ticked[el.name][el.value] === true;",
        "if (p.refresh) p.refresh();",
    ] {
        assert!(body.contains(held), "{held}");
    }
}
