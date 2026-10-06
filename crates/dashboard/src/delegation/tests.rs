//! Delegations without HTTP: registration's rules, a code's and a refresh
//! token's single use, the bounds against a clock that moves, renewal, and
//! narrowing. The HTTP half is `web::oauth`'s.

use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccountGroup, AccountRecord, AccountState, Permission, UserGroup,
};

use super::*;

const T0: i64 = 1_790_380_800_000_000_000;
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const BACK: &str = "http://127.0.0.1:53682/callback";

fn cli() -> Registration {
    Registration {
        name: "meridian on ada-laptop".into(),
        redirect_uris: vec![BACK.into()],
        software_id: CLI_SOFTWARE_ID.into(),
    }
}

fn ada(at: i64) -> Person {
    Person {
        subject: "local|ada".into(),
        display_name: "Ada Park".into(),
        directory_groups: vec!["desk".into()],
        signed_in_at_ns: at,
    }
}

async fn registered(delegations: &Delegations) -> Client {
    delegations
        .register(cli(), T0)
        .await
        .expect("kept")
        .expect("accepted")
}

/// Registered, signed in, consented to, and the code: what a client holds
/// when the browser comes back.
async fn coded(delegations: &Delegations, covers: Covers, days: i64, at: i64) -> (Client, String) {
    let client = registered(delegations).await;
    let asked = check_asked(
        client.clone(),
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "st-1",
        Resource::Terminal,
    )
    .expect("a good request");
    let id = delegations.open(asked, at);
    let (_, confirm) = delegations.signed_in(&id, ada(at), at).expect("waiting");
    let (_, code) = delegations
        .decide(&id, &confirm, Some(Consent { covers, days }), at)
        .await
        .expect("kept")
        .expect("decided");
    (client, code.expect("a code for an allowed consent"))
}

async fn issued(delegations: &Delegations, at: i64) -> (Client, Issued) {
    let (client, code) = coded(delegations, Covers::everything(), 90, at).await;
    let (delegation, resource) = delegations
        .redeem(&code, VERIFIER, BACK, &client.client_id, None, at)
        .await
        .expect("kept")
        .expect("redeemed");
    let pair = delegations
        .issue(delegation, resource, at)
        .await
        .expect("issued");
    (client, pair)
}

async fn refresh(
    delegations: &Delegations,
    client: &Client,
    token: &str,
    at: i64,
) -> Result<Issued, Refusal> {
    let (delegation, held) = delegations
        .refreshing(token, &client.client_id, at)
        .await
        .expect("kept")?;
    if !delegations.spend(&held, at).await.expect("kept") {
        return Err(Refusal::Reused);
    }
    Ok(delegations
        .issue(delegation, Resource::Terminal, at)
        .await
        .expect("issued"))
}

async fn checked(delegations: &Delegations, token: &str, at: i64) -> Result<Delegation, Refusal> {
    delegations
        .check(token, Resource::Terminal, None, at)
        .await
        .expect("kept")
}

#[test]
fn registration_takes_https_and_this_machine_and_the_cli_only_by_literal_address() {
    let with = |uri: &str, software_id: &str| Registration {
        name: "a client".into(),
        redirect_uris: vec![uri.into()],
        software_id: software_id.into(),
    };
    for allowed in [
        "https://claude.ai/api/mcp/auth_callback",
        "http://127.0.0.1:8123/callback",
        "http://[::1]:8123/callback",
        "http://localhost:6274/oauth/callback",
    ] {
        assert!(check_registration(&with(allowed, "")).is_ok(), "{allowed}");
    }
    assert!(check_registration(&with("http://127.0.0.1:9/callback", CLI_SOFTWARE_ID)).is_ok());
    for (refused, software_id) in [
        ("http://localhost:6274/callback", CLI_SOFTWARE_ID),
        ("http://evil.example/callback", ""),
        ("myapp://callback", ""),
        ("https://claude.ai/callback#fragment", ""),
        ("https://user:pass@claude.ai/callback", ""),
        ("/relative", ""),
    ] {
        assert!(
            check_registration(&with(refused, software_id)).is_err(),
            "{refused} as {software_id:?}"
        );
    }
    let mut nameless = cli();
    nameless.name = " ".into();
    assert!(check_registration(&nameless).is_err());
    let mut many = cli();
    many.redirect_uris = vec![BACK.into(); 11];
    assert!(check_registration(&many).is_err());
}

#[tokio::test]
async fn a_loopback_redirect_matches_at_any_port_and_nothing_else_does() {
    let delegations = Delegations::default();
    let client = registered(&delegations).await;
    assert!(client.redirects_to(BACK));
    assert!(client.redirects_to("http://127.0.0.1:61000/callback"));
    assert!(!client.redirects_to("http://127.0.0.1:61000/elsewhere"));
    assert!(!client.redirects_to("http://localhost:53682/callback"));
    assert!(!client.redirects_to("https://127.0.0.1:53682/callback"));
}

#[test]
fn a_request_holds_to_the_code_flow_and_s256() {
    let client = Client {
        client_id: "mdc_x".into(),
        name: "x".into(),
        redirect_uris: vec![BACK.into()],
        software_id: String::new(),
        registered_at_ns: T0,
        consented: false,
    };
    let ask = |response_type, challenge, method, state| {
        check_asked(
            client.clone(),
            BACK,
            response_type,
            challenge,
            method,
            state,
            Resource::Terminal,
        )
    };
    assert!(ask("code", CHALLENGE, "S256", "st-1").is_ok());
    assert!(
        ask("code", CHALLENGE, "S256", "").is_ok(),
        "state is optional"
    );
    assert_eq!(
        ask("token", CHALLENGE, "S256", "s").unwrap_err().0,
        "unsupported_response_type"
    );
    assert_eq!(
        ask("code", CHALLENGE, "plain", "s").unwrap_err().0,
        "invalid_request"
    );
    assert_eq!(
        ask("code", "short", "S256", "s").unwrap_err().0,
        "invalid_request"
    );
    assert_eq!(
        ask("code", CHALLENGE, "S256", "a b").unwrap_err().0,
        "invalid_request"
    );
}

#[test]
fn a_resource_is_named_by_an_absolute_address_whose_path_is_the_terminals_or_the_mcp_surfaces() {
    assert_eq!(
        Resource::indicated("https://dash.firm.example/terminal"),
        Some((Resource::Terminal, "https://dash.firm.example".into()))
    );
    assert_eq!(
        Resource::indicated("http://127.0.0.1:8443/terminal/"),
        Some((Resource::Terminal, "http://127.0.0.1:8443".into()))
    );
    // Contract v12: the deployment's MCP surface is a resource too.
    assert_eq!(
        Resource::indicated("https://dash.firm.example/mcp"),
        Some((Resource::Mcp, "https://dash.firm.example".into()))
    );
    for not in [
        "/terminal",
        "https://dash.firm.example/mcpx",
        "https://d/terminal?x=1",
        "",
    ] {
        assert_eq!(Resource::indicated(not), None, "{not}");
    }
}

#[tokio::test]
async fn a_consent_makes_a_delegation_whose_tokens_act_until_the_access_token_lapses() {
    let delegations = Delegations::default();
    let (_, pair) = issued(&delegations, T0).await;
    assert!(pair.access_token.starts_with(ACCESS_PREFIX));
    assert!(pair.refresh_token.starts_with(REFRESH_PREFIX));
    assert_eq!(pair.delegation.expires_at_ns, T0 + 90 * DAY_NS);
    assert_eq!(pair.delegation.client_name, "meridian on ada-laptop");

    let delegation = checked(&delegations, &pair.access_token, T0 + ACCESS_NS)
        .await
        .expect("exactly ten minutes is still in");
    assert_eq!(delegation.subject, "local|ada");
    assert_eq!(delegation.last_used_at_ns, Some(T0 + ACCESS_NS));
    assert_eq!(
        checked(&delegations, &pair.access_token, T0 + ACCESS_NS + 1).await,
        Err(Refusal::Expired)
    );
    assert_eq!(
        checked(&delegations, &pair.refresh_token, T0).await,
        Err(Refusal::Unknown),
        "a refresh token is not an access token"
    );
    assert_eq!(
        delegations
            .check(&pair.access_token, Resource::Terminal, None, T0)
            .await
            .unwrap()
            .map(|d| d.id),
        Ok(pair.delegation.id.clone())
    );
}

#[tokio::test]
async fn a_refresh_token_is_spent_by_its_first_use_and_a_second_revokes_the_delegation() {
    let delegations = Delegations::default();
    let (client, first) = issued(&delegations, T0).await;
    let later = T0 + 30 * MINUTE_NS;
    let second = refresh(&delegations, &client, &first.refresh_token, later)
        .await
        .expect("refreshed");
    assert_eq!(
        second.delegation.id, first.delegation.id,
        "the same delegation"
    );
    assert!(checked(&delegations, &second.access_token, later)
        .await
        .is_ok());

    // The first refresh token again: somebody else holds it.
    assert_eq!(
        refresh(&delegations, &client, &first.refresh_token, later)
            .await
            .err(),
        Some(Refusal::Reused)
    );
    assert_eq!(
        checked(&delegations, &second.access_token, later).await,
        Err(Refusal::Revoked),
        "the owner's own pair is ended too"
    );
    let revoked = delegations
        .delegation(&first.delegation.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revoked.revoked.as_ref().unwrap().by, "dashboard");
    assert!(
        revoked.last_refusal.is_some(),
        "and the refusal is recorded"
    );
}

#[tokio::test]
async fn a_refresh_token_is_its_clients_alone() {
    let delegations = Delegations::default();
    let (client, pair) = issued(&delegations, T0).await;
    let other = registered(&delegations).await;
    assert_eq!(
        refresh(&delegations, &other, &pair.refresh_token, T0)
            .await
            .err(),
        Some(Refusal::Unknown)
    );
    assert!(
        refresh(&delegations, &client, &pair.refresh_token, T0)
            .await
            .is_ok(),
        "and presenting it as another client spent nothing"
    );
}

#[tokio::test]
async fn a_code_is_spent_by_its_first_presentation_and_a_second_revokes_what_it_granted() {
    let delegations = Delegations::default();
    let (client, code) = coded(&delegations, Covers::everything(), 30, T0).await;
    let (delegation, _) = delegations
        .redeem(&code, VERIFIER, BACK, &client.client_id, None, T0)
        .await
        .unwrap()
        .expect("first");
    let again = delegations
        .redeem(&code, VERIFIER, BACK, &client.client_id, None, T0)
        .await
        .unwrap();
    assert_eq!(again.unwrap_err().0, Refusal::Reused);
    assert!(delegations
        .delegation(&delegation.id)
        .await
        .unwrap()
        .unwrap()
        .revoked
        .is_some());
}

#[tokio::test]
async fn a_code_needs_its_verifier_its_redirect_its_client_and_its_minute() {
    let delegations = Delegations::default();
    for (verifier, back, late) in [
        ("x".repeat(43), BACK, 0),
        (VERIFIER.to_string(), "http://127.0.0.1:1/callback", 0),
        (VERIFIER.to_string(), BACK, CODE_NS + 1),
    ] {
        let (client, code) = coded(&delegations, Covers::everything(), 30, T0).await;
        let refused = delegations
            .redeem(&code, &verifier, back, &client.client_id, None, T0 + late)
            .await
            .unwrap();
        assert_eq!(
            refused.unwrap_err().0,
            Refusal::Unknown,
            "{verifier} {back} {late}"
        );
    }
    let (_, code) = coded(&delegations, Covers::everything(), 30, T0).await;
    let refused = delegations
        .redeem(&code, VERIFIER, BACK, "mdc_somebody-else", None, T0)
        .await
        .unwrap();
    assert_eq!(refused.unwrap_err().0, Refusal::Unknown);
}

#[tokio::test]
async fn declining_spends_the_request_and_records_nothing() {
    let delegations = Delegations::default();
    let client = registered(&delegations).await;
    let asked = check_asked(
        client,
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "s",
        Resource::Terminal,
    )
    .unwrap();
    let id = delegations.open(asked, T0);
    let (_, confirm) = delegations.signed_in(&id, ada(T0), T0).unwrap();
    let (_, code) = delegations
        .decide(&id, &confirm, None, T0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, None);
    assert!(delegations
        .of_person("local|ada", T0)
        .await
        .unwrap()
        .is_empty());
    assert!(delegations
        .decide(&id, &confirm, None, T0)
        .await
        .unwrap()
        .is_err());
}

#[tokio::test]
async fn a_consent_needs_its_own_sign_in_and_its_ten_minutes() {
    let delegations = Delegations::default();
    let client = registered(&delegations).await;
    let asked = check_asked(
        client,
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "s",
        Resource::Terminal,
    )
    .unwrap();
    let id = delegations.open(asked.clone(), T0);
    let (_, confirm) = delegations.signed_in(&id, ada(T0), T0).unwrap();
    assert!(delegations.consenting(&id, "guessed", T0).is_none());
    assert!(
        delegations.signed_in(&id, ada(T0), T0).is_none(),
        "signed in to once"
    );
    assert!(delegations
        .consenting(&id, &confirm, T0 + REQUEST_NS + 1)
        .is_none());
    let late = delegations.open(asked, T0);
    assert!(delegations
        .signed_in(&late, ada(T0), T0 + REQUEST_NS + 1)
        .is_none());
}

#[tokio::test]
async fn a_delegation_lapses_at_its_end_and_refreshing_cannot_keep_it() {
    let delegations = Delegations::default();
    let (client, code) = coded(&delegations, Covers::everything(), 7, T0).await;
    let (delegation, resource) = delegations
        .redeem(&code, VERIFIER, BACK, &client.client_id, None, T0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delegation.expires_at_ns, T0 + 7 * DAY_NS);
    assert!(
        delegation.noticed(T0),
        "a week's delegation is within its week's notice"
    );
    let mut pair = delegations.issue(delegation, resource, T0).await.unwrap();
    let mut now = T0;
    // Refreshed every nine minutes: it never lapses by its access tokens,
    // only by its own end.
    while now + 9 * MINUTE_NS <= T0 + 7 * DAY_NS {
        now += 9 * MINUTE_NS;
        pair = refresh(&delegations, &client, &pair.refresh_token, now)
            .await
            .expect("refreshed within the delegation");
    }
    assert!(
        pair.delegation.expires_at_ns - now < ACCESS_NS,
        "an access token never outlives its delegation"
    );
    let past = T0 + 7 * DAY_NS + 1;
    assert_eq!(
        checked(&delegations, &pair.access_token, past).await,
        Err(Refusal::Expired)
    );
    let (lapsed, _) = delegations
        .refreshing(&pair.refresh_token, &client.client_id, past)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lapsed.refusal(past, None), Some(Refusal::Lapsed));
}

#[tokio::test]
async fn groups_older_than_the_bound_are_refused_until_the_person_signs_in_again() {
    let delegations = Delegations::default();
    let (_, pair) = issued(&delegations, T0).await;
    let bound = Some(GROUPS_BOUND_NS);
    let delegation = pair.delegation.clone();
    assert_eq!(delegation.refusal(T0 + GROUPS_BOUND_NS, bound), None);
    assert_eq!(
        delegation.refusal(T0 + GROUPS_BOUND_NS + 1, bound),
        Some(Refusal::Groups)
    );
    assert_eq!(
        delegation.refusal(T0 + GROUPS_BOUND_NS + 1, None),
        None,
        "LDAP and local accounts are read afresh instead"
    );
    let later = T0 + 8 * DAY_NS;
    delegations
        .signed_in_afresh("local|ada", vec!["desk".into(), "risk".into()], later)
        .await
        .unwrap();
    let fresh = delegations
        .delegation(&delegation.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh.refusal(later, bound),
        None,
        "not revoked, only refused"
    );
    assert_eq!(
        fresh.directory_groups,
        vec!["desk".to_string(), "risk".to_string()]
    );
}

#[tokio::test]
async fn consenting_again_renews_the_standing_delegation_and_ends_its_old_tokens() {
    let delegations = Delegations::default();
    let (client, first) = issued(&delegations, T0).await;
    let later = T0 + 80 * DAY_NS;
    let asked = check_asked(
        client.clone(),
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "s",
        Resource::Terminal,
    )
    .unwrap();
    let id = delegations.open(asked, later);
    let (_, confirm) = delegations.signed_in(&id, ada(later), later).unwrap();
    let (_, code) = delegations
        .decide(
            &id,
            &confirm,
            Some(Consent {
                covers: Covers::everything(),
                days: 90,
            }),
            later,
        )
        .await
        .unwrap()
        .unwrap();
    let (renewed, _) = delegations
        .redeem(
            &code.unwrap(),
            VERIFIER,
            BACK,
            &client.client_id,
            None,
            later,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renewed.id, first.delegation.id, "renewed, not another");
    assert_eq!(renewed.expires_at_ns, later + 90 * DAY_NS);
    assert_eq!(renewed.made_at_ns, T0);
    assert_eq!(
        refresh(&delegations, &client, &first.refresh_token, later)
            .await
            .err(),
        Some(Refusal::Unknown),
        "the old refresh token is gone, not spent, so it revokes nothing"
    );
    assert!(delegations
        .delegation(&renewed.id)
        .await
        .unwrap()
        .unwrap()
        .revoked
        .is_none());
    assert_eq!(
        delegations
            .of_person("local|ada", later)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_client_revokes_only_a_delegation_of_its_own() {
    let delegations = Delegations::default();
    let (client, pair) = issued(&delegations, T0).await;
    delegations
        .revoke_by_token(&pair.refresh_token, "mdc_another", T0)
        .await
        .unwrap();
    assert!(checked(&delegations, &pair.access_token, T0).await.is_ok());
    delegations
        .revoke_by_token(&pair.refresh_token, &client.client_id, T0)
        .await
        .unwrap();
    assert_eq!(
        checked(&delegations, &pair.access_token, T0).await,
        Err(Refusal::Revoked)
    );
    delegations
        .revoke_by_token("mdr_never-issued", &client.client_id, T0)
        .await
        .expect("an unknown token revokes nothing, and is not an error");
}

#[tokio::test]
async fn no_token_is_kept_only_its_fingerprint() {
    let store = Arc::new(InMemory::default());
    let delegations = Delegations::keeping(store.clone());
    let (_, pair) = issued(&delegations, T0).await;
    let kept = store.fingerprints();
    assert_eq!(kept.len(), 2);
    for token in [&pair.access_token, &pair.refresh_token] {
        assert!(!kept.iter().any(|k| k.contains(token.as_str())));
        assert!(kept.contains(&fingerprint(token)));
    }
}

#[tokio::test]
async fn the_sweep_forgets_registrations_nobody_consented_to_and_old_delegations() {
    let delegations = Delegations::default();
    let unconsented = registered(&delegations).await;
    let (consented, pair) = issued(&delegations, T0).await;
    delegations
        .revoke(&pair.delegation.id, "local|ada", "done with it", T0)
        .await
        .unwrap();
    delegations.sweep(T0 + DAY_NS + 1).await.unwrap();
    assert!(delegations
        .client(&unconsented.client_id)
        .await
        .unwrap()
        .is_none());
    assert!(delegations
        .client(&consented.client_id)
        .await
        .unwrap()
        .is_some());
    assert!(
        delegations
            .delegation(&pair.delegation.id)
            .await
            .unwrap()
            .is_some(),
        "a revoked delegation stays listed, with why"
    );
    delegations.sweep(T0 + KEPT_NS + 1).await.unwrap();
    assert!(delegations
        .delegation(&pair.delegation.id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn registrations_are_capped_making_room_from_the_oldest_unconsented() {
    let store = InMemory::default();
    let client = |n: i64, consented| Client {
        client_id: format!("mdc_{n}"),
        name: format!("client {n}"),
        redirect_uris: vec![BACK.into()],
        software_id: String::new(),
        registered_at_ns: T0 + n,
        consented,
    };
    assert!(store.register(&client(1, true), 2).unwrap());
    assert!(store.register(&client(2, false), 2).unwrap());
    assert!(
        store.register(&client(3, false), 2).unwrap(),
        "room made from 2"
    );
    assert!(store.client("mdc_2").unwrap().is_none());
    assert!(store.client("mdc_1").unwrap().is_some());
    assert!(
        store.register(&client(4, true), 2).unwrap(),
        "room made from 3"
    );
    assert!(
        !store.register(&client(5, false), 2).unwrap(),
        "nothing unconsented is left to make room from"
    );
}

// ── Narrowing ────────────────────────────────────────────────────────────

fn records() -> AccessRecords {
    let account = |id: &str| AccountRecord {
        account_id: id.into(),
        name: id.into(),
        state: AccountState::Open as i32,
        ..Default::default()
    };
    AccessRecords {
        accounts: vec![account("ACC-1"), account("ACC-2")],
        account_groups: vec![
            AccountGroup {
                account_group_id: "AG-1".into(),
                name: "Desk one".into(),
                account_ids: vec!["ACC-1".into()],
                ..Default::default()
            },
            AccountGroup {
                account_group_id: "AG-2".into(),
                name: "Desk two".into(),
                account_ids: vec!["ACC-2".into()],
                ..Default::default()
            },
        ],
        access_groups: vec![AccessGroup {
            access_group_id: "AX-1".into(),
            name: "Traders".into(),
            entries: vec![
                AccessEntry {
                    plugin_instance_id: "oms-1".into(),
                    level: AccessLevel::Write as i32,
                    role: String::new(),
                },
                AccessEntry {
                    plugin_instance_id: "oms-1".into(),
                    level: AccessLevel::Admin as i32,
                    role: String::new(),
                },
            ],
            ..Default::default()
        }],
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Desk".into(),
            directory_groups: vec![],
            logins: vec!["local|ada".into()],
        }],
        permissions: vec![
            Permission {
                permission_id: "P-1".into(),
                user_group_id: "UG-1".into(),
                account_group_id: "AG-1".into(),
                access_group_id: "AX-1".into(),
            },
            Permission {
                permission_id: "P-2".into(),
                user_group_id: "UG-1".into(),
                account_group_id: "AG-2".into(),
                access_group_id: "AX-1".into(),
            },
            Permission {
                permission_id: "P-3".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
            },
            Permission {
                permission_id: "P-4".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
            },
        ],
        ..Default::default()
    }
}

#[test]
fn everything_follows_the_person_and_a_narrowed_delegation_is_an_intersection() {
    let records = records();
    let access = meridian_access::person_access(&records, "local|ada", &[]);
    assert_eq!(
        narrow(access.clone(), &Covers::everything(), &records),
        access
    );

    let narrowed = narrow(
        access.clone(),
        &Covers {
            plugins: BTreeSet::from([("oms-1".into(), String::new(), "read".into())]),
            account_groups: BTreeSet::from(["AG-1".into()]),
            ..Covers::default()
        },
        &records,
    );
    assert!(!narrowed.deployment_admin, "only if named");
    assert!(!narrowed.all_plugins_admin);
    let held = narrowed.held("oms-1");
    assert_eq!(
        held.levels(),
        vec![AccessLevel::Read],
        "read, cut from write"
    );
    assert_eq!(held.accounts.read, BTreeSet::from(["ACC-1".to_string()]));
    assert!(held.accounts.write.is_empty());
    assert!(!narrowed.held("anything-else").holds_any());

    let admin_only = narrow(
        access,
        &Covers {
            deployment_admin: true,
            plugins: BTreeSet::from([
                ("oms-1".into(), String::new(), "admin".into()),
                ("new-1".into(), String::new(), "admin".into()),
            ]),
            ..Covers::default()
        },
        &records,
    );
    assert!(admin_only.deployment_admin);
    assert_eq!(admin_only.held("oms-1").levels(), vec![AccessLevel::Admin]);
    assert_eq!(
        admin_only.held("new-1").levels(),
        vec![AccessLevel::Admin],
        "All plugins (admin) reaches a named plugin it covers"
    );
    assert!(!admin_only.held("other-1").holds_any(), "and no other");
}

#[test]
fn a_narrowed_delegation_never_reaches_more_than_the_person_holds_now() {
    let mut records = records();
    let covers = Covers {
        deployment_admin: true,
        plugins: BTreeSet::from([("oms-1".into(), String::new(), "write".into())]),
        account_groups: BTreeSet::from(["AG-1".into(), "AG-2".into()]),
        ..Covers::default()
    };
    // The permission on AG-2 and deployment admin withdrawn.
    records
        .permissions
        .retain(|p| p.permission_id != "P-2" && p.permission_id != "P-3");
    let access = meridian_access::person_access(&records, "local|ada", &[]);
    let narrowed = narrow(access, &covers, &records);
    assert!(!narrowed.deployment_admin);
    assert_eq!(
        narrowed.held("oms-1").accounts.write,
        BTreeSet::from(["ACC-1".to_string()])
    );
}

#[test]
fn what_a_delegation_covers_is_said_in_a_line() {
    let names = BTreeMap::from([("AG-1".to_string(), "Desk one".to_string())]);
    assert_eq!(Covers::everything().said(&names), "Everything you hold");
    assert_eq!(Covers::default().said(&names), "Nothing");
    let some = Covers {
        deployment_admin: true,
        plugins: BTreeSet::from([
            ("oms-1".into(), String::new(), "write".into()),
            ("oms-1".into(), String::new(), "read".into()),
        ]),
        account_groups: BTreeSet::from(["AG-1".into()]),
        ..Covers::default()
    };
    // One level per plugin, the highest covered (kernel/the-consent-page-at-scale).
    assert_eq!(
        some.said(&names),
        "deployment admin; oms-1 (Open); accounts in Desk one"
    );
}

#[test]
fn one_level_is_said_per_plugin_and_a_long_list_is_cut_short() {
    let mut covers = Covers::default();
    for i in 0..5 {
        covers
            .plugins
            .insert((format!("p-{i}"), String::new(), "admin".into()));
        covers
            .plugins
            .insert((format!("p-{i}"), String::new(), "read".into()));
    }
    covers
        .plugins
        .insert(("q-1".into(), String::new(), "write".into()));
    covers
        .plugins
        .insert(("q-1".into(), String::new(), "read".into()));
    covers
        .plugins
        .insert(("r-1".into(), String::new(), "read".into()));
    covers.account_groups = (0..300).map(|i| format!("AG-{i:03}")).collect();
    assert_eq!(covers.level_on("p-0", ""), Some(AccessLevel::Admin));
    assert_eq!(covers.level_on("q-1", ""), Some(AccessLevel::Write));
    assert_eq!(covers.level_on("r-1", ""), Some(AccessLevel::Read));
    assert_eq!(covers.level_on("s-1", ""), None);
    assert_eq!(
        covers.said(&BTreeMap::new()),
        "p-0 (Manage), p-1 (Manage), p-2 (Manage) and 4 more plugins; \
         accounts in AG-000, AG-001, AG-002 and 297 more"
    );
    assert_eq!(listed(&[]), "");
    assert_eq!(listed(&["a"]), "a");
    assert_eq!(listed(&["a", "b"]), "a and b");
    assert_eq!(listed(&["a", "b", "c"]), "a, b and c");
    assert_eq!(listed(&["a", "b", "c", "d"]), "a, b, c and 1 more");
}

// ── Rows per role (contract v15, W6.17) ───────────────────────────────────

/// A plugin holding custody and operations: Ada writes operations on
/// Desk one and reads custody on Desk two.
fn two_role_records() -> AccessRecords {
    let mut records = records();
    records.known_plugins = vec![
        meridian_domain::v1::KnownPluginRoles {
            plugin_instance_id: "ops-1".into(),
            roles: vec!["custody".into(), "operations".into()],
        },
        meridian_domain::v1::KnownPluginRoles {
            plugin_instance_id: "oms-1".into(),
            roles: vec!["oms".into()],
        },
    ];
    records.access_groups.push(AccessGroup {
        access_group_id: "AX-OPS".into(),
        name: "Reconciliation".into(),
        entries: vec![AccessEntry {
            plugin_instance_id: "ops-1".into(),
            level: AccessLevel::Write as i32,
            role: "operations".into(),
        }],
        ..Default::default()
    });
    records.access_groups.push(AccessGroup {
        access_group_id: "AX-CUS".into(),
        name: "Custody readers".into(),
        entries: vec![AccessEntry {
            plugin_instance_id: "ops-1".into(),
            level: AccessLevel::Read as i32,
            role: "custody".into(),
        }],
        ..Default::default()
    });
    records.permissions.push(Permission {
        permission_id: "P-5".into(),
        user_group_id: "UG-1".into(),
        account_group_id: "AG-1".into(),
        access_group_id: "AX-OPS".into(),
    });
    records.permissions.push(Permission {
        permission_id: "P-6".into(),
        user_group_id: "UG-1".into(),
        account_group_id: "AG-2".into(),
        access_group_id: "AX-CUS".into(),
    });
    records
}

#[test]
fn a_narrowed_delegation_covers_a_role_at_its_level_and_nothing_of_another() {
    let records = two_role_records();
    let access = meridian_access::person_access(&records, "local|ada", &[]);
    let covers = Covers {
        plugins: BTreeSet::from([
            ("ops-1".into(), "operations".into(), "write".into()),
            ("ops-1".into(), "operations".into(), "read".into()),
        ]),
        account_groups: BTreeSet::from(["AG-1".into(), "AG-2".into()]),
        ..Covers::default()
    };
    let narrowed = narrow(access, &covers, &records);
    let ops = narrowed.plugin("ops-1");
    assert_eq!(ops.roles.keys().collect::<Vec<_>>(), ["operations"]);
    assert_eq!(ops.roles["operations"].data, Some(AccessLevel::Write));
    assert!(
        ops.roles["operations"].accounts.write.contains("ACC-1"),
        "operations write on Desk one"
    );
    assert!(
        !narrowed.held("ops-1").accounts.read.contains("ACC-2"),
        "custody's read is not covered"
    );
    // Everything follows the person per role.
    let all = narrow(
        meridian_access::person_access(&records, "local|ada", &[]),
        &Covers::everything(),
        &records,
    );
    assert!(all.plugin("ops-1").roles.contains_key("custody"));
}

#[test]
fn rows_recorded_before_v15_are_rewritten_once_to_name_the_plugins_one_role() {
    let records = two_role_records();
    let covers = Covers {
        unmatched: BTreeSet::from([
            ("oms-1".into(), "write".into()),
            ("ops-1".into(), "read".into()),
            ("gone-1".into(), "admin".into()),
        ]),
        ..Covers::default()
    };
    let rewritten = rewrite_rows(&covers, &records).expect("one row rewritten");
    assert_eq!(
        rewritten.plugins,
        BTreeSet::from([("oms-1".into(), "oms".into(), "write".into())])
    );
    assert_eq!(
        rewritten.unmatched,
        BTreeSet::from([
            ("gone-1".into(), "admin".into()),
            ("ops-1".into(), "read".into())
        ]),
        "a plugin holding several roles, or none known: kept, covering nothing"
    );
    assert_eq!(rewrite_rows(&rewritten, &records), None, "idempotent");
    // An unmatched row covers nothing.
    let access = meridian_access::person_access(&records, "local|ada", &[]);
    let narrowed = narrow(access, &rewritten, &records);
    assert!(!narrowed.held("ops-1").holds_any());
    assert_eq!(
        rewritten.said(&BTreeMap::new()),
        "oms-1 oms (Open)",
        "a row names its role"
    );
}

#[test]
fn the_rows_as_kept_read_back_whichever_shape_they_were_written_in() {
    let (rows, unmatched) = store::rows_of(&[
        "ops-1:custody:read".into(),
        "tool-1::write".into(),
        "oms-1:admin".into(),
    ]);
    assert_eq!(
        rows,
        BTreeSet::from([
            ("ops-1".into(), "custody".into(), "read".into()),
            ("tool-1".into(), String::new(), "write".into())
        ])
    );
    assert_eq!(
        unmatched,
        BTreeSet::from([("oms-1".into(), "admin".into())])
    );
}
