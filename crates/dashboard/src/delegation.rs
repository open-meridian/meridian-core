//! Delegations: a person's access lent to one named client (decisions/029;
//! spec/clients-act-on-a-persons-delegation; W6.14, W6.17, W6.18).
//!
//! A client that is not a browser -- the CLI first -- registers with the
//! dashboard, sends the person to its authorisation with a PKCE challenge
//! (RFC 7636), and the person signs in afresh, however this deployment signs
//! people in, and consents. The dashboard records the delegation and sends a
//! one-time code to the client's redirect address, which the client exchanges
//! with its verifier for an access token (ten minutes, one resource) and a
//! refresh token (single use). This module is that state, and nothing about
//! HTTP ([`crate::web`] serves it).
//!
//! **A delegation is the person's authority, lent.** It names a person and a
//! client and what it covers, and nothing the person may do: every request on
//! it reads it, and evaluates the person from the records as any session's is,
//! intersected with what it covers ([`narrow`]). Revoking it ends the client's
//! access at its next request.
//!
//! **Fingerprints only.** Every token is kept by its SHA-256 and never
//! itself, so no table and no log line can be presented as one; each is
//! prefixed (`mda_` access, `mdr_` refresh) so one found somewhere says what
//! it is.
//!
//! **Single use is what makes a theft visible.** A refresh token presented a
//! second time revokes the delegation, as does an authorisation code: the only
//! way to present either twice is for somebody else to hold it too.
//!
//! Requests and codes stay in this process's memory, as the terminal's do:
//! minutes long, so a restart during one costs a retry. Clients, delegations
//! and tokens are kept by a [`DelegationStore`], the dashboard's own tables in
//! the deployment's database, so they survive a restart or an upgrade.
//!
//! The bounds -- ten minutes, 90 days, a week's notice, a week's groups --
//! are constants here rather than configuration, as decisions/015's are.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use meridian_access::{AccessLevel, Held, Levels};
use meridian_domain::v1::AccessRecords;

use crate::clock::{HOUR_NS, MINUTE_NS, SECOND_NS};
use crate::session::token;
use crate::terminal::{hashed, same, unreserved, url_safe, verifies, Person, Unavailable};

mod store;
pub use store::{CallsOf, DelegationStore, InMemory, InPostgres};

pub const DAY_NS: i64 = 24 * HOUR_NS;
/// An access token's life (requirement 8).
pub const ACCESS_NS: i64 = 10 * MINUTE_NS;
/// An authorisation code's (requirement 10).
pub const CODE_NS: i64 = 60 * SECOND_NS;
/// How long an authorisation waits for somebody to sign in and consent.
pub const REQUEST_NS: i64 = 10 * MINUTE_NS;
/// The longest a delegation lives before the person consents again.
pub const MOST_NS: i64 = 90 * DAY_NS;
/// How far ahead of a lapse the person is told (requirement 6).
pub const NOTICE_NS: i64 = 7 * DAY_NS;
/// How old a delegation's directory groups may be where the firm signs
/// people in through its own provider (requirement 7; question 1, ruled 7
/// days). Past it the delegation is refused, not revoked, until the person
/// signs in again.
pub const GROUPS_BOUND_NS: i64 = 7 * DAY_NS;
/// A delegation's last use is written at most this often, to spare the
/// database a write on every request.
pub const USE_RECORDED_NS: i64 = MINUTE_NS;
/// Authorisations held at once, the oldest dropped first: opening one needs
/// no sign-in.
pub const MAX_REQUESTS: usize = 1_000;
/// Registrations held at once (registration is open, so bounded). At the
/// cap, the oldest nobody consented to goes first.
pub const MAX_CLIENTS: usize = 10_000;
/// A registration nobody consents to is removed after a day.
pub const UNCONSENTED_NS: i64 = DAY_NS;
/// A revoked or lapsed delegation stays listed, with why, this long.
pub const KEPT_NS: i64 = 30 * DAY_NS;
/// How long a call through the deployment's MCP surface stays recorded: the
/// longest a delegation lives (W6.20, Q8).
pub const CALLS_KEPT_NS: i64 = 90 * DAY_NS;

/// One call through the deployment's MCP surface, as recorded (W6.20,
/// requirement 21): who, through which delegation and client, which tool
/// and whose, at which level, how it came out and how long it took. Never
/// an argument or an answer, which may carry account data a deployment
/// admin, who reads this, reaches none of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub called_at_ns: i64,
    pub subject: String,
    pub delegation_id: String,
    pub client_name: String,
    /// `dashboard`, or a plugin's instance.
    pub owner: String,
    pub tool: String,
    /// The level a plugin's tool opened at; empty for core's.
    pub level: String,
    /// made, unchanged or refused.
    pub outcome: String,
    pub reason: String,
    pub duration_ms: i64,
}
/// What a person may choose a delegation to last, in days.
pub const DAYS: [i64; 3] = [7, 30, 90];

pub const ACCESS_PREFIX: &str = "mda_";
pub const REFRESH_PREFIX: &str = "mdr_";
pub const CLIENT_PREFIX: &str = "mdc_";

/// What the CLI says it is when it registers (`software_id`). Trusted for the
/// consent page's defaults and nothing else, since anybody could say it.
pub const CLI_SOFTWARE_ID: &str = "meridian-cli";

/// A token's one resource (requirement 8; RFC 8707): the CLI's surface, or
/// the deployment's MCP surface (W6.20, contract v12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// The CLI's surface, `/terminal/`.
    Terminal,
    /// The deployment's MCP surface, `/mcp` ([`crate::mcp`]).
    Mcp,
}

impl Resource {
    /// As a request names it, the path of the resource's address.
    pub fn path(self) -> &'static str {
        match self {
            Resource::Terminal => "/terminal",
            Resource::Mcp => "/mcp",
        }
    }

    /// As it is kept beside a token.
    pub fn name(self) -> &'static str {
        match self {
            Resource::Terminal => "terminal",
            Resource::Mcp => "mcp",
        }
    }

    pub fn named(name: &str) -> Option<Resource> {
        match name {
            "terminal" => Some(Resource::Terminal),
            "mcp" => Some(Resource::Mcp),
            _ => None,
        }
    }

    /// The resource an indicator names: an absolute address whose path is
    /// one this dashboard issues tokens for. Its origin is the caller's to
    /// check, since only the caller knows its own address.
    pub fn indicated(indicator: &str) -> Option<(Resource, String)> {
        let url = reqwest::Url::parse(indicator).ok()?;
        if !matches!(url.scheme(), "https" | "http")
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return None;
        }
        let resource = match url.path().trim_end_matches('/') {
            "/terminal" => Resource::Terminal,
            "/mcp" => Resource::Mcp,
            _ => return None,
        };
        Some((resource, url.origin().ascii_serialization()))
    }
}

/// What a delegation covers: everything the person holds, as that changes,
/// or a narrowed part named at consent (requirement 3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Covers {
    pub everything: bool,
    /// The deployment admin's capabilities, only if named.
    pub deployment_admin: bool,
    /// Plugin instance, role and level (`admin`, `write` or `read`): rows of
    /// plugin, role and level from contract v15 (W6.17, decisions/033), the
    /// role empty for a plugin holding none.
    pub plugins: BTreeSet<(String, String, String)>,
    /// Rows recorded before v15, naming a plugin and a level, that the
    /// one-time rewrite could not name a role for -- a plugin holding
    /// several roles, or none known: kept as recorded, covering nothing, and
    /// flagged on Connected clients until the person consents again
    /// ([`rewrite_rows`]; the plan's Q4).
    pub unmatched: BTreeSet<(String, String)>,
    /// The account groups whose accounts it reaches.
    pub account_groups: BTreeSet<String>,
    /// The tools that change something its consent page listed (contract
    /// v18; the MCP spec, ruled 2026-10-09): a tool a release or a plugin
    /// adds to one of its rows later reaches it only after fresh consent.
    /// None for one covering everything, which follows the person's grants,
    /// and for one narrowed before v18 until it is first evaluated, when it
    /// is filled with what it reached then ([`Delegations::backfill_acting`]).
    pub acting: Option<BTreeSet<String>>,
}

impl Covers {
    pub fn everything() -> Covers {
        Covers {
            everything: true,
            ..Covers::default()
        }
    }

    fn covers(&self, instance: &str, role: &str, level: &str) -> bool {
        self.plugins
            .contains(&(instance.to_string(), role.to_string(), level.to_string()))
    }

    /// The one level a person picks on a plugin's role (the product owner,
    /// 2026-10-04, kernel/the-consent-page-at-scale ruling 1; per role from
    /// v15): the highest it covers there. Manage includes Open and View, as
    /// holding it does.
    pub fn level_on(&self, instance: &str, role: &str) -> Option<AccessLevel> {
        LEVELS_DOWN
            .into_iter()
            .find(|level| self.covers(instance, role, meridian_access::level_name(*level)))
    }

    /// The plugin instances and roles it names, in order.
    pub fn rows(&self) -> BTreeSet<(&str, &str)> {
        self.plugins
            .iter()
            .map(|(instance, role, _)| (instance.as_str(), role.as_str()))
            .collect()
    }

    /// Its rows by the level picked on each, Manage first: "ops-1" for a
    /// role-less plugin's, "ops-1 custody" for a role's.
    pub fn by_level(&self) -> Vec<(AccessLevel, Vec<String>)> {
        let rows = self.rows();
        LEVELS_DOWN
            .into_iter()
            .map(|level| {
                let at: Vec<String> = rows
                    .iter()
                    .filter(|(instance, role)| self.level_on(instance, role) == Some(level))
                    .map(|(instance, role)| row_named(instance, role))
                    .collect();
                (level, at)
            })
            .filter(|(_, at)| !at.is_empty())
            .collect()
    }

    /// For a person to read, and a refusal to name: what it covers, in a
    /// line, each plugin with the one level picked on it, as the consent page
    /// picks one, however many it names.
    pub fn said(&self, names: &BTreeMap<String, String>) -> String {
        if self.everything {
            return "Everything you hold".into();
        }
        let mut parts = Vec::new();
        if self.deployment_admin {
            parts.push("deployment admin".to_string());
        }
        let plugins: Vec<String> = self
            .rows()
            .into_iter()
            .filter_map(|(instance, role)| {
                let level = self.level_on(instance, role)?;
                Some(format!(
                    "{} ({})",
                    row_named(instance, role),
                    meridian_access::button(level)
                ))
            })
            .collect();
        if plugins.len() <= NAMES_SAID {
            parts.extend(plugins);
        } else {
            parts.push(format!(
                "{} and {} more plugins",
                plugins[..NAMES_SAID].join(", "),
                plugins.len() - NAMES_SAID
            ));
        }
        if !self.account_groups.is_empty() {
            let groups: Vec<&str> = self
                .account_groups
                .iter()
                .map(|id| names.get(id).map(String::as_str).unwrap_or(id))
                .collect();
            parts.push(format!("accounts in {}", listed(&groups)));
        }
        if parts.is_empty() {
            "Nothing".into()
        } else {
            parts.join("; ")
        }
    }
}

/// A row of a delegation, as a person reads it: the plugin, and its role
/// where it names one.
pub fn row_named(instance: &str, role: &str) -> String {
    if role.is_empty() {
        instance.to_string()
    } else {
        format!("{instance} {role}")
    }
}

/// Rows recorded before contract v15 -- a plugin and a level -- rewritten
/// once to name the plugin's one role where the records say it holds exactly
/// one, or none where it holds none (W6.17; the plan's design): None when
/// nothing changes, rows already naming a role being skipped, so it is
/// idempotent. A row on a plugin holding several roles, or one the records
/// do not list, is kept unmatched, covering nothing.
pub fn rewrite_rows(covers: &Covers, records: &AccessRecords) -> Option<Covers> {
    if covers.unmatched.is_empty() {
        return None;
    }
    let mut rewritten = covers.clone();
    rewritten.unmatched.clear();
    for (instance, level) in &covers.unmatched {
        match meridian_access::known_roles(records, instance) {
            Some([one]) => {
                rewritten
                    .plugins
                    .insert((instance.clone(), one.clone(), level.clone()));
            }
            Some([]) => {
                rewritten
                    .plugins
                    .insert((instance.clone(), String::new(), level.clone()));
            }
            _ => {
                rewritten
                    .unmatched
                    .insert((instance.clone(), level.clone()));
            }
        }
    }
    (rewritten != *covers).then_some(rewritten)
}

/// The rows rewrite, once the records are first read at v15 (W6.17): tried
/// every five seconds until they are, then done once, each delegation
/// rewritten logged. Spawned by the dashboard at start.
pub async fn rewrite_once_records_are_read(
    delegations: Arc<Delegations>,
    records: Arc<crate::records::RecordsCache>,
    clock: Arc<dyn crate::clock::Clock>,
) {
    loop {
        if let Ok(read) = records.current(clock.now_ns()) {
            match rewrite_recorded_rows(&delegations, &read).await {
                Ok(0) => {}
                Ok(rewritten) => tracing::info!(
                    rewritten,
                    "delegations' rows rewritten to name each plugin's role (contract v15)"
                ),
                Err(unavailable) => {
                    tracing::warn!(%unavailable, "delegations' rows were not rewritten yet");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// The rows rewrite: every delegation recorded with rows from before v15,
/// once the records are first read at v15 (W6.17), each rewrite logged.
/// Returns how many delegations it rewrote.
pub async fn rewrite_recorded_rows(
    delegations: &Delegations,
    records: &AccessRecords,
) -> Result<usize, Unavailable> {
    let mut rewritten = 0;
    for delegation in delegations.narrowed().await? {
        let Some(covers) = rewrite_rows(&delegation.covers, records) else {
            continue;
        };
        delegations.rewrite(&delegation.id, &covers).await?;
        tracing::info!(
            delegation = delegation.id,
            subject = delegation.subject,
            left = covers.unmatched.len(),
            "a delegation's rows rewritten to name each plugin's role (contract v15)"
        );
        rewritten += 1;
    }
    Ok(rewritten)
}

/// The levels from the highest down: Manage, Open, View.
pub const LEVELS_DOWN: [AccessLevel; 3] =
    [AccessLevel::Admin, AccessLevel::Write, AccessLevel::Read];

/// How many names a line says before "and N more".
pub const NAMES_SAID: usize = 3;

/// Names for a person to read, the first few and how many more: "a", "a and
/// b", "a, b and c", "a, b, c and 297 more".
pub fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        _ if names.len() <= NAMES_SAID => format!(
            "{} and {}",
            names[..names.len() - 1].join(", "),
            names[names.len() - 1]
        ),
        _ => format!(
            "{} and {} more",
            names[..NAMES_SAID].join(", "),
            names.len() - NAMES_SAID
        ),
    }
}

/// A person's access cut to what a delegation covers: taken per request,
/// never copied, so a narrowed delegation never grows and an everything one
/// follows the person (question 2). Nothing here widens anybody: every level
/// and account kept is one the person holds now. Per role from v15: a row
/// covers its plugin's role at its level, and nothing of another role.
pub fn narrow(
    access: meridian_access::Access,
    covers: &Covers,
    records: &AccessRecords,
) -> meridian_access::Access {
    if covers.everything {
        return access;
    }
    let accounts = meridian_access::accounts_in_groups(records, &covers.account_groups);
    let mut plugins: meridian_access::PluginLevels = BTreeMap::new();
    for (instance, role) in covers.rows() {
        let Some(held) = access.plugin(instance).roles.get(role).cloned() else {
            continue;
        };
        let admin = held.admin && covers.covers(instance, role, "admin");
        let data = match held.data {
            Some(AccessLevel::Write) if covers.covers(instance, role, "write") => {
                Some(AccessLevel::Write)
            }
            Some(AccessLevel::Write | AccessLevel::Read)
                if covers.covers(instance, role, "read") =>
            {
                Some(AccessLevel::Read)
            }
            _ => None,
        };
        let mut reached = Levels::default();
        if let Some(level) = data {
            reached.read = held
                .accounts
                .read
                .intersection(&accounts)
                .cloned()
                .collect();
            if level == AccessLevel::Write {
                reached.write = held
                    .accounts
                    .write
                    .intersection(&accounts)
                    .cloned()
                    .collect();
            }
        }
        if admin || data.is_some() {
            plugins
                .entry(instance.to_string())
                .or_default()
                .roles
                .insert(
                    role.to_string(),
                    Held {
                        admin,
                        data,
                        accounts: reached,
                    },
                );
        }
    }
    meridian_access::Access {
        deployment_admin: access.deployment_admin && covers.deployment_admin,
        // Every plugin's admin, those launched later included, is not a list;
        // a narrowed delegation names its plugins.
        all_plugins_admin: false,
        user_group_ids: access.user_group_ids,
        plugins,
        known_roles: access.known_roles,
    }
}

/// A registered client. Public: it holds no secret (requirement 13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub client_id: String,
    /// Its own choice, and shown as that.
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub software_id: String,
    pub registered_at_ns: i64,
    /// Whether anybody has consented to it, which keeps it past a day.
    pub consented: bool,
}

impl Client {
    pub fn is_cli(&self) -> bool {
        self.software_id == CLI_SOFTWARE_ID
    }

    /// Whether `asked` is one of its redirect addresses. A loopback address
    /// by IP matches at any port (RFC 8252, section 7.3): the CLI listens on
    /// whichever port is free when it connects.
    pub fn redirects_to(&self, asked: &str) -> bool {
        self.redirect_uris
            .iter()
            .any(|registered| registered == asked || same_but_port(registered, asked))
    }
}

fn same_but_port(registered: &str, asked: &str) -> bool {
    let (Ok(mut registered), Ok(mut asked)) =
        (reqwest::Url::parse(registered), reqwest::Url::parse(asked))
    else {
        return false;
    };
    if !(loopback_ip(&registered) && loopback_ip(&asked)) {
        return false;
    }
    let _ = registered.set_port(None);
    let _ = asked.set_port(None);
    registered == asked
}

/// Plain HTTP to this machine by literal address: `127.0.0.0/8` or `[::1]`.
fn loopback_ip(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && match url.host_str() {
            Some("[::1]") => true,
            Some(host) => host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback()),
            None => false,
        }
}

/// What a client registers with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub software_id: String,
}

/// Check a registration (RFC 7591), before anything is kept.
pub fn check_registration(asked: &Registration) -> Result<(), String> {
    let name = asked.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(
            "client_name must be 1 to 100 characters, none of them control characters".into(),
        );
    }
    if asked.redirect_uris.is_empty() || asked.redirect_uris.len() > 10 {
        return Err("redirect_uris must name 1 to 10 addresses".into());
    }
    if asked.software_id.len() > 100 || asked.software_id.chars().any(char::is_control) {
        return Err("software_id is too long or holds control characters".into());
    }
    let cli = asked.software_id == CLI_SOFTWARE_ID;
    for uri in &asked.redirect_uris {
        redirect_allowed(uri, cli)?;
    }
    Ok(())
}

/// Question 5, ruled: HTTPS anywhere; HTTP to the loopback by literal
/// address, as W6.13 ruled for the CLI; and HTTP to `localhost` for other
/// clients, which the MCP clients people use register. No custom schemes, no
/// fragment, nobody's credentials in the address.
fn redirect_allowed(uri: &str, cli: bool) -> Result<(), String> {
    let refused = |why: &str| {
        Err(format!(
            "{uri} is not a redirect address this accepts: {why}"
        ))
    };
    let Ok(url) = reqwest::Url::parse(uri) else {
        return refused("it is not an absolute address");
    };
    if url.fragment().is_some() {
        return refused("it has a fragment");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return refused("it carries credentials");
    }
    if url.scheme() == "https" && url.host_str().is_some() {
        return Ok(());
    }
    if url.scheme() != "http" {
        return refused("it is neither https nor http");
    }
    if loopback_ip(&url) || (!cli && url.host_str() == Some("localhost")) {
        return Ok(());
    }
    refused(if cli {
        "the CLI's is http://127.0.0.1 or http://[::1], never localhost"
    } else {
        "plain HTTP goes only to this machine"
    })
}

/// When, by whom and why a delegation was revoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revoked {
    pub at_ns: i64,
    /// The subject who revoked it, or `client` for the client itself, or
    /// `dashboard` for the deployment.
    pub by: String,
    pub why: String,
}

/// A delegation, as the deployment records it (requirement 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    pub id: String,
    pub subject: String,
    pub display_name: String,
    pub client_id: String,
    /// The client's registered name, read with the delegation.
    pub client_name: String,
    pub covers: Covers,
    pub made_at_ns: i64,
    pub renewed_at_ns: i64,
    pub expires_at_ns: i64,
    pub revoked: Option<Revoked>,
    pub last_used_at_ns: Option<i64>,
    pub last_refusal: Option<(i64, String)>,
    pub directory_groups: Vec<String>,
    pub groups_read_at_ns: i64,
    /// When a delegation narrowed before v18 had its consented tools filled
    /// in from what it reached then (decisions/031); None for one whose
    /// consent page recorded them.
    pub acting_backfilled_at_ns: Option<i64>,
}

impl Delegation {
    pub fn live(&self, now_ns: i64) -> bool {
        self.revoked.is_none() && now_ns <= self.expires_at_ns
    }

    /// Whether the person should be told it lapses soon, or has.
    pub fn noticed(&self, now_ns: i64) -> bool {
        self.revoked.is_none()
            && now_ns + NOTICE_NS >= self.expires_at_ns
            && now_ns <= self.expires_at_ns + NOTICE_NS
    }

    /// Why a request on it is refused now, if it is.
    pub fn refusal(&self, now_ns: i64, groups_bound: Option<i64>) -> Option<Refusal> {
        if self.revoked.is_some() {
            return Some(Refusal::Revoked);
        }
        if now_ns > self.expires_at_ns {
            return Some(Refusal::Lapsed);
        }
        if groups_bound.is_some_and(|bound| now_ns - self.groups_read_at_ns > bound) {
            return Some(Refusal::Groups);
        }
        None
    }
}

/// What makes a delegation, or renews the standing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub subject: String,
    pub display_name: String,
    pub client_id: String,
    pub covers: Covers,
    pub directory_groups: Vec<String>,
    pub expires_at_ns: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Access,
    Refresh,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Access => "access",
            Kind::Refresh => "refresh",
        }
    }
}

/// A token as it is kept: its fingerprint, never itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub fingerprint: String,
    pub kind: Kind,
    pub resource: String,
    pub delegation_id: String,
    pub client_id: String,
    pub expires_at_ns: i64,
    pub spent: bool,
}

/// Why a token, a code or a request on a delegation was refused. Said to
/// the client, which says it to the person or reports what stopped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Never issued here, for another resource or client, or gone.
    Unknown,
    /// An access token past its ten minutes: refresh.
    Expired,
    /// Revoked: by the person, a deployment admin, the client, or a token
    /// presented twice.
    Revoked,
    /// Past its 90 days, or the days the person chose.
    Lapsed,
    /// Its directory groups are older than this deployment's bound: the
    /// person signs in again.
    Groups,
    /// A refresh token or a code presented a second time: the delegation is
    /// revoked.
    Reused,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::Unknown => "unknown",
            Refusal::Expired => "expired",
            Refusal::Revoked => "revoked",
            Refusal::Lapsed => "lapsed",
            Refusal::Groups => "groups",
            Refusal::Reused => "reused",
        }
    }

    /// A sentence a person can act on.
    pub fn sentence(self) -> &'static str {
        match self {
            Refusal::Unknown => "this deployment does not know that token",
            Refusal::Expired => "the access token has expired; refresh it",
            Refusal::Revoked => "the delegation was revoked; connect again",
            Refusal::Lapsed => "the delegation has lapsed; connect again to renew it",
            Refusal::Groups => {
                "the delegation's directory groups are older than this deployment allows; \
                 sign in to the dashboard, or connect again"
            }
            Refusal::Reused => {
                "a token was presented twice, so the delegation it belonged to is revoked; \
                 connect again"
            }
        }
    }
}

/// What a token answer hands a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    /// The only copies there will be.
    pub access_token: String,
    pub refresh_token: String,
    pub delegation: Delegation,
}

/// What an authorisation asked for, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    pub client: Client,
    pub redirect_uri: String,
    pub challenge: String,
    /// Possibly empty; sent back as it came.
    pub state: String,
    pub resource: Resource,
}

/// Check an authorisation's parameters, the client and redirect address
/// already found. A code challenge, S256 only (requirement 12).
pub fn check_asked(
    client: Client,
    redirect_uri: &str,
    response_type: &str,
    challenge: &str,
    method: &str,
    state: &str,
    resource: Resource,
) -> Result<Asked, (&'static str, String)> {
    if response_type != "code" {
        return Err((
            "unsupported_response_type",
            "response_type must be code: this deployment issues nothing by any other flow".into(),
        ));
    }
    if method != "S256" {
        return Err((
            "invalid_request",
            "code_challenge_method must be S256".into(),
        ));
    }
    if challenge.len() != 43 || !challenge.bytes().all(url_safe) {
        return Err((
            "invalid_request",
            "code_challenge must be a SHA-256 in base64url".into(),
        ));
    }
    if state.len() > 512
        || !state
            .bytes()
            .all(|b| unreserved(b) || b"+/=%:".contains(&b))
    {
        return Err((
            "invalid_request",
            "state must be at most 512 URL-safe characters".into(),
        ));
    }
    Ok(Asked {
        client,
        redirect_uri: redirect_uri.to_string(),
        challenge: challenge.to_string(),
        state: state.to_string(),
        resource,
    })
}

/// The person's answer on the consent page, checked against what they hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consent {
    pub covers: Covers,
    pub days: i64,
}

/// A fingerprint, as tokens are kept.
pub fn fingerprint(token: &str) -> String {
    hashed(token)
}

/// What the in-memory half of a code's redemption decided.
enum Redeemed {
    Refused(&'static str),
    /// Presented before: the delegation its first use granted.
    Twice(String),
    Granted(String, Resource),
}

struct Waiting {
    asked: Asked,
    opened_at_ns: i64,
    signed_in: Option<(Person, String)>,
}

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    resource: Resource,
    delegation_id: String,
    issued_at_ns: i64,
    redeemed: bool,
}

#[derive(Default)]
struct Pending {
    waiting: HashMap<String, Waiting>,
    by_provider_state: HashMap<String, String>,
    codes: HashMap<String, Code>,
}

/// The delegations, and the authorisations in flight.
pub struct Delegations {
    pending: Mutex<Pending>,
    store: Arc<dyn DelegationStore>,
}

impl Default for Delegations {
    /// Kept in memory: for tests, and a dashboard given no database.
    fn default() -> Self {
        Self::keeping(Arc::new(InMemory::default()))
    }
}

impl Delegations {
    pub fn keeping(store: Arc<dyn DelegationStore>) -> Self {
        Self {
            pending: Mutex::default(),
            store,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().expect("delegation lock poisoned")
    }

    /// Ask the store off the async runtime: it may be the blocking Postgres
    /// client, which panics when used on it.
    async fn stored<T: Send + 'static>(
        &self,
        ask: impl FnOnce(&dyn DelegationStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, Unavailable> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || ask(store.as_ref()))
            .await
            .map_err(|failed| Unavailable(failed.to_string()))?
            .map_err(Unavailable)
    }

    // ── Registration ──────────────────────────────────────────────────────

    /// RFC 7591: a public client, its name and its redirect addresses, kept;
    /// or why not. Grants nothing.
    pub async fn register(
        &self,
        asked: Registration,
        now_ns: i64,
    ) -> Result<Result<Client, String>, Unavailable> {
        if let Err(why) = check_registration(&asked) {
            return Ok(Err(why));
        }
        let client = Client {
            client_id: format!("{CLIENT_PREFIX}{}", token()),
            name: asked.name.trim().to_string(),
            redirect_uris: asked.redirect_uris,
            software_id: asked.software_id,
            registered_at_ns: now_ns,
            consented: false,
        };
        let kept = client.clone();
        let room = self
            .stored(move |store| store.register(&kept, MAX_CLIENTS))
            .await?;
        Ok(if room {
            Ok(client)
        } else {
            Err("this deployment holds as many clients as it will; try again later".into())
        })
    }

    pub async fn client(&self, client_id: &str) -> Result<Option<Client>, Unavailable> {
        let client_id = client_id.to_string();
        self.stored(move |store| store.client(&client_id)).await
    }

    // ── Authorisation ─────────────────────────────────────────────────────

    /// Hold a checked authorisation, and return the id its sign-in carries.
    pub fn open(&self, asked: Asked, now_ns: i64) -> String {
        let mut pending = self.lock();
        pending
            .waiting
            .retain(|_, w| now_ns - w.opened_at_ns <= REQUEST_NS);
        while pending.waiting.len() >= MAX_REQUESTS {
            let oldest = pending
                .waiting
                .iter()
                .min_by_key(|(_, w)| w.opened_at_ns)
                .map(|(id, _)| id.clone())
                .expect("a full map has an oldest");
            pending.waiting.remove(&oldest);
        }
        let id = token();
        pending.waiting.insert(
            id.clone(),
            Waiting {
                asked,
                opened_at_ns: now_ns,
                signed_in: None,
            },
        );
        id
    }

    /// Note that a provider sign-in with this state is for this
    /// authorisation.
    pub fn through_provider(&self, provider_state: &str, id: &str) {
        self.lock()
            .by_provider_state
            .insert(provider_state.to_string(), id.to_string());
    }

    /// The authorisation a provider sign-in was for, if it was for one.
    /// Taken, so a state is matched once.
    pub fn for_provider_state(&self, provider_state: &str) -> Option<String> {
        self.lock().by_provider_state.remove(provider_state)
    }

    /// Somebody signed in to this authorisation: what it asked for, and the
    /// token their consent must carry; or nothing when it has gone.
    pub fn signed_in(&self, id: &str, person: Person, now_ns: i64) -> Option<(Asked, String)> {
        let mut pending = self.lock();
        let waiting = pending.waiting.get_mut(id)?;
        if now_ns - waiting.opened_at_ns > REQUEST_NS || waiting.signed_in.is_some() {
            return None;
        }
        let confirm = token();
        waiting.signed_in = Some((person, confirm.clone()));
        Some((waiting.asked.clone(), confirm))
    }

    /// The person behind a consent form, if it is still waiting and the form
    /// is theirs. Nothing is spent.
    pub fn consenting(&self, id: &str, confirm: &str, now_ns: i64) -> Option<(Asked, Person)> {
        let pending = self.lock();
        let waiting = pending.waiting.get(id)?;
        let (person, expected) = waiting.signed_in.as_ref()?;
        (now_ns - waiting.opened_at_ns <= REQUEST_NS
            && same(expected.as_bytes(), confirm.as_bytes()))
        .then(|| (waiting.asked.clone(), person.clone()))
    }

    /// The person's answer. Declined: where to say so. Allowed: the
    /// delegation recorded (or the standing one renewed), and a code for the
    /// client. Either way the authorisation is spent.
    pub async fn decide(
        &self,
        id: &str,
        confirm: &str,
        consent: Option<Consent>,
        now_ns: i64,
    ) -> Result<Result<(Asked, Option<String>), &'static str>, Unavailable> {
        let (asked, person) = {
            let mut pending = self.lock();
            let Some(waiting) = pending.waiting.get(id) else {
                return Ok(Err("this authorisation has expired or was already used"));
            };
            let Some((_, expected)) = &waiting.signed_in else {
                return Ok(Err("nobody has signed in to this authorisation"));
            };
            if !same(expected.as_bytes(), confirm.as_bytes()) {
                return Ok(Err("this answer did not come from this sign-in"));
            }
            let waiting = pending.waiting.remove(id).expect("found above");
            if now_ns - waiting.opened_at_ns > REQUEST_NS {
                return Ok(Err("this authorisation has expired or was already used"));
            }
            let (person, _) = waiting.signed_in.expect("checked above");
            (waiting.asked, person)
        };
        let Some(consent) = consent else {
            return Ok(Ok((asked, None)));
        };
        let days = consent.days.clamp(1, MOST_NS / DAY_NS);
        let grant = Grant {
            subject: person.subject.clone(),
            display_name: person.display_name.clone(),
            client_id: asked.client.client_id.clone(),
            covers: consent.covers,
            directory_groups: person.directory_groups.clone(),
            expires_at_ns: now_ns + days * DAY_NS,
        };
        let delegation = self
            .stored(move |store| store.grant(&grant, now_ns))
            .await?;
        let code = token();
        self.lock().codes.insert(
            code.clone(),
            Code {
                client_id: asked.client.client_id.clone(),
                redirect_uri: asked.redirect_uri.clone(),
                challenge: asked.challenge.clone(),
                resource: asked.resource,
                delegation_id: delegation.id,
                issued_at_ns: now_ns,
                redeemed: false,
            },
        );
        Ok(Ok((asked, Some(code))))
    }

    /// Trade a code for the delegation it granted, before anything is
    /// issued on it. Spent by its first presentation, whatever the answer; a
    /// second revokes the delegation. Which check failed is for the log;
    /// every refusal of a code is `invalid_grant`.
    pub async fn redeem(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        client_id: &str,
        resource: Option<Resource>,
        now_ns: i64,
    ) -> Result<Result<(Delegation, Resource), (Refusal, &'static str)>, Unavailable> {
        let redeemed = {
            let mut pending = self.lock();
            match pending.codes.get_mut(code) {
                None => Redeemed::Refused("unknown code"),
                // Somebody else holds it too: what it bought is theirs as
                // well, and ends.
                Some(held) if held.redeemed => Redeemed::Twice(held.delegation_id.clone()),
                Some(held) => {
                    held.redeemed = true;
                    if now_ns - held.issued_at_ns > CODE_NS {
                        Redeemed::Refused("code expired")
                    } else if held.client_id != client_id {
                        Redeemed::Refused("code issued to another client")
                    } else if held.redirect_uri != redirect_uri {
                        Redeemed::Refused(
                            "redirect_uri differs from the one the code was issued to",
                        )
                    } else if resource.is_some_and(|r| r != held.resource) {
                        Redeemed::Refused("resource differs from the one asked for")
                    } else if !verifies(verifier, &held.challenge) {
                        Redeemed::Refused("verifier does not match the challenge")
                    } else {
                        Redeemed::Granted(held.delegation_id.clone(), held.resource)
                    }
                }
            }
        };
        let (id, resource) = match redeemed {
            Redeemed::Granted(id, resource) => (id, resource),
            Redeemed::Refused(why) => return Ok(Err((Refusal::Unknown, why))),
            Redeemed::Twice(id) => {
                self.stored(move |store| {
                    store.revoke(
                        &id,
                        "dashboard",
                        "its authorisation code was used twice",
                        now_ns,
                    )
                })
                .await?;
                return Ok(Err((
                    Refusal::Reused,
                    "code used twice; its delegation is revoked",
                )));
            }
        };
        let delegation = self.stored(move |store| store.delegation(&id)).await?;
        Ok(match delegation {
            None => Err((Refusal::Unknown, "the code's delegation is gone")),
            Some(delegation) => Ok((delegation, resource)),
        })
    }

    /// The delegation a refresh token is for, before it is spent: so a
    /// refusal that asks the person to sign in again leaves the token usable
    /// after they have. A token spent already revokes its delegation.
    pub async fn refreshing(
        &self,
        refresh_token: &str,
        client_id: &str,
        now_ns: i64,
    ) -> Result<Result<(Delegation, Token), Refusal>, Unavailable> {
        if !refresh_token.starts_with(REFRESH_PREFIX) {
            return Ok(Err(Refusal::Unknown));
        }
        let key = fingerprint(refresh_token);
        let Some(token) = self.stored(move |store| store.token(&key)).await? else {
            return Ok(Err(Refusal::Unknown));
        };
        if token.kind != Kind::Refresh || token.client_id != client_id {
            return Ok(Err(Refusal::Unknown));
        }
        if token.spent {
            let id = token.delegation_id.clone();
            self.stored(move |store| {
                store.revoke(&id, "dashboard", "a refresh token was used twice", now_ns)
            })
            .await?;
            return Ok(Err(Refusal::Reused));
        }
        let id = token.delegation_id.clone();
        Ok(
            match self.stored(move |store| store.delegation(&id)).await? {
                None => Err(Refusal::Unknown),
                Some(delegation) => Ok((delegation, token)),
            },
        )
    }

    /// Spend a refresh token. False when somebody spent it first, in which
    /// case its delegation is revoked: two presentations at once are two
    /// holders.
    pub async fn spend(&self, token: &Token, now_ns: i64) -> Result<bool, Unavailable> {
        let key = token.fingerprint.clone();
        let spent = self.stored(move |store| store.spend(&key, now_ns)).await?;
        if !spent {
            let id = token.delegation_id.clone();
            self.stored(move |store| {
                store.revoke(&id, "dashboard", "a refresh token was used twice", now_ns)
            })
            .await?;
        }
        Ok(spent)
    }

    /// A new pair on a delegation: an access token for `resource`, ten
    /// minutes, and a refresh token living as long as the delegation.
    pub async fn issue(
        &self,
        delegation: Delegation,
        resource: Resource,
        now_ns: i64,
    ) -> Result<Issued, Unavailable> {
        let access_token = format!("{ACCESS_PREFIX}{}", token());
        let refresh_token = format!("{REFRESH_PREFIX}{}", token());
        let pair = [
            Token {
                fingerprint: fingerprint(&access_token),
                kind: Kind::Access,
                resource: resource.name().to_string(),
                delegation_id: delegation.id.clone(),
                client_id: delegation.client_id.clone(),
                expires_at_ns: (now_ns + ACCESS_NS).min(delegation.expires_at_ns),
                spent: false,
            },
            Token {
                fingerprint: fingerprint(&refresh_token),
                kind: Kind::Refresh,
                resource: resource.name().to_string(),
                delegation_id: delegation.id.clone(),
                client_id: delegation.client_id.clone(),
                expires_at_ns: delegation.expires_at_ns,
                spent: false,
            },
        ];
        self.stored(move |store| pair.iter().try_for_each(|token| store.issue(token)))
            .await?;
        Ok(Issued {
            access_token,
            refresh_token,
            delegation,
        })
    }

    // ── A request on a delegation (W6.18) ─────────────────────────────────

    /// The delegation an access token presented at `resource` is on, if it
    /// may act now; or why not, recorded on the delegation as its last
    /// refusal where it has one. `groups_bound` is the provider branch's.
    pub async fn check(
        &self,
        access_token: &str,
        resource: Resource,
        groups_bound: Option<i64>,
        now_ns: i64,
    ) -> Result<Result<Delegation, Refusal>, Unavailable> {
        if !access_token.starts_with(ACCESS_PREFIX) {
            return Ok(Err(Refusal::Unknown));
        }
        let key = fingerprint(access_token);
        let Some(token) = self.stored(move |store| store.token(&key)).await? else {
            return Ok(Err(Refusal::Unknown));
        };
        if token.kind != Kind::Access || token.resource != resource.name() {
            return Ok(Err(Refusal::Unknown));
        }
        if now_ns > token.expires_at_ns {
            return Ok(Err(Refusal::Expired));
        }
        let id = token.delegation_id.clone();
        let Some(delegation) = self.stored(move |store| store.delegation(&id)).await? else {
            return Ok(Err(Refusal::Unknown));
        };
        if let Some(refusal) = delegation.refusal(now_ns, groups_bound) {
            self.refused(&delegation.id, refusal.sentence(), now_ns)
                .await?;
            return Ok(Err(refusal));
        }
        let mut delegation = delegation;
        if delegation
            .last_used_at_ns
            .is_none_or(|at| now_ns - at >= USE_RECORDED_NS)
        {
            let id = delegation.id.clone();
            self.stored(move |store| store.used(&id, now_ns)).await?;
            delegation.last_used_at_ns = Some(now_ns);
        }
        Ok(Ok(delegation))
    }

    /// A delegation by id, whatever its state: what a plugin host's session
    /// opened from a client's link (W6.15) holds of it.
    pub async fn delegation(&self, id: &str) -> Result<Option<Delegation>, Unavailable> {
        let id = id.to_string();
        self.stored(move |store| store.delegation(&id)).await
    }

    /// Every delegation narrowed at consent, revoked or not: what the rows
    /// rewrite reads (W6.17).
    pub async fn narrowed(&self) -> Result<Vec<Delegation>, Unavailable> {
        self.stored(|store| store.narrowed()).await
    }

    /// A delegation's rows as rewritten once to name each plugin's role.
    pub async fn rewrite(&self, id: &str, covers: &Covers) -> Result<(), Unavailable> {
        let (id, covers) = (id.to_string(), covers.clone());
        self.stored(move |store| store.rewrite(&id, &covers)).await
    }

    /// A delegation narrowed before v18, its consented tools filled in once
    /// from the tools that change something it reaches now, said to be
    /// filled at `now_ns` (contract v18; decisions/031).
    pub async fn backfill_acting(
        &self,
        id: &str,
        acting: Vec<String>,
        now_ns: i64,
    ) -> Result<(), Unavailable> {
        let id = id.to_string();
        self.stored(move |store| store.backfill_acting(&id, &acting, now_ns))
            .await
    }

    /// Record why a request on a delegation was refused, for the person and
    /// the admin to read: at most [`MOST_REFUSAL`] characters of it
    /// (contract v18; v17's security review, Nit 4), so a megabyte of
    /// unknown keys is not kept and shown.
    pub async fn refused(&self, id: &str, why: &str, now_ns: i64) -> Result<(), Unavailable> {
        let (id, why) = (id.to_string(), capped(why));
        self.stored(move |store| store.refused(&id, &why, now_ns))
            .await
    }

    /// The person's directory groups, read again for a delegation (LDAP:
    /// whenever a token is issued on it).
    pub async fn groups_read(
        &self,
        id: &str,
        groups: Vec<String>,
        now_ns: i64,
    ) -> Result<(), Unavailable> {
        let id = id.to_string();
        self.stored(move |store| store.groups_read(&id, &groups, now_ns))
            .await
    }

    /// The person signed in afresh, in a browser, a terminal or a consent:
    /// their groups now, on every delegation they hold (requirement 7).
    pub async fn signed_in_afresh(
        &self,
        subject: &str,
        groups: Vec<String>,
        now_ns: i64,
    ) -> Result<(), Unavailable> {
        let subject = subject.to_string();
        self.stored(move |store| store.signed_in(&subject, &groups, now_ns))
            .await
    }

    // ── Listing and revoking (W6.14) ──────────────────────────────────────

    pub async fn revoke(
        &self,
        id: &str,
        by: &str,
        why: &str,
        now_ns: i64,
    ) -> Result<bool, Unavailable> {
        let (id, by, why) = (id.to_string(), by.to_string(), why.to_string());
        self.stored(move |store| store.revoke(&id, &by, &why, now_ns))
            .await
    }

    pub async fn revoke_person(
        &self,
        subject: &str,
        by: &str,
        why: &str,
        now_ns: i64,
    ) -> Result<usize, Unavailable> {
        let (subject, by, why) = (subject.to_string(), by.to_string(), why.to_string());
        self.stored(move |store| store.revoke_person(&subject, &by, &why, now_ns))
            .await
    }

    /// RFC 7009: a client revokes the delegation a token of its own is on.
    /// Nothing is said either way, as the RFC has it: a token unknown here,
    /// or another client's, revokes nothing.
    pub async fn revoke_by_token(
        &self,
        presented: &str,
        client_id: &str,
        now_ns: i64,
    ) -> Result<(), Unavailable> {
        let key = fingerprint(presented);
        let Some(token) = self.stored(move |store| store.token(&key)).await? else {
            return Ok(());
        };
        if token.client_id != client_id {
            return Ok(());
        }
        self.revoke(
            &token.delegation_id,
            "client",
            "revoked by its client",
            now_ns,
        )
        .await
        .map(|_| ())
    }

    /// A person's delegations, live first then newest.
    pub async fn of_person(
        &self,
        subject: &str,
        now_ns: i64,
    ) -> Result<Vec<Delegation>, Unavailable> {
        let subject = subject.to_string();
        self.stored(move |store| store.of_person(&subject, now_ns))
            .await
    }

    /// The standing delegation a person holds to a client, if any.
    pub async fn standing(
        &self,
        subject: &str,
        client_id: &str,
        now_ns: i64,
    ) -> Result<Option<Delegation>, Unavailable> {
        Ok(self
            .of_person(subject, now_ns)
            .await?
            .into_iter()
            .find(|d| d.client_id == client_id && d.live(now_ns)))
    }

    /// Who holds a live delegation, with a name and how many, by subject.
    pub async fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, Unavailable> {
        self.stored(move |store| store.holders(now_ns)).await
    }

    /// Forget everything past its bound.
    /// Record a call through the deployment's MCP surface.
    pub async fn record_call(&self, call: ToolCall) -> Result<(), Unavailable> {
        self.stored(move |store| store.record_call(&call)).await
    }

    /// The latest calls on a delegation, or of a person, newest first.
    pub async fn calls(&self, of: CallsOf, limit: usize) -> Result<Vec<ToolCall>, Unavailable> {
        self.stored(move |store| store.calls(&of, limit)).await
    }

    pub async fn sweep(&self, now_ns: i64) -> Result<(), Unavailable> {
        {
            let mut pending = self.lock();
            pending
                .waiting
                .retain(|_, w| now_ns - w.opened_at_ns <= REQUEST_NS);
            let waiting: std::collections::HashSet<String> =
                pending.waiting.keys().cloned().collect();
            pending
                .by_provider_state
                .retain(|_, id| waiting.contains(id));
            // A redeemed code is kept a little past its minute, so its second
            // presentation still finds what its first one bought.
            pending
                .codes
                .retain(|_, c| now_ns - c.issued_at_ns <= 10 * CODE_NS);
        }
        self.stored(move |store| store.sweep(now_ns)).await
    }
}

/// A delegation covering everything a person holds, to the CLI on a computer,
/// for the terminal's paths, as `meridian connect` makes one: its access
/// token. For other modules' tests of what a terminal path does.
#[cfg(test)]
pub(crate) async fn connected_for_tests(
    delegations: &Delegations,
    subject: &str,
    now_ns: i64,
) -> String {
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const BACK: &str = "http://127.0.0.1:53682/callback";
    let client = delegations
        .register(
            Registration {
                name: format!("meridian for {subject}"),
                redirect_uris: vec![BACK.into()],
                software_id: CLI_SOFTWARE_ID.into(),
            },
            now_ns,
        )
        .await
        .expect("kept")
        .expect("registered");
    let asked = check_asked(
        client.clone(),
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "st",
        Resource::Terminal,
    )
    .expect("asked");
    let id = delegations.open(asked, now_ns);
    let person = Person {
        subject: subject.into(),
        display_name: subject.into(),
        directory_groups: vec![],
        signed_in_at_ns: now_ns,
    };
    let (_, confirm) = delegations
        .signed_in(&id, person, now_ns)
        .expect("signed in");
    let (_, code) = delegations
        .decide(
            &id,
            &confirm,
            Some(Consent {
                covers: Covers::everything(),
                days: 30,
            }),
            now_ns,
        )
        .await
        .expect("kept")
        .expect("decided");
    let (delegation, resource) = delegations
        .redeem(
            &code.expect("a code"),
            VERIFIER,
            BACK,
            &client.client_id,
            None,
            now_ns,
        )
        .await
        .expect("kept")
        .expect("redeemed");
    delegations
        .issue(delegation, resource, now_ns)
        .await
        .expect("issued")
        .access_token
}

/// The most of a refusal's words a delegation keeps (contract v18; v17's
/// security review, Nit 4).
pub const MOST_REFUSAL: usize = 1_000;

/// A refusal's words as kept: whole within [`MOST_REFUSAL`] characters,
/// otherwise cut there and said to be cut.
pub fn capped(why: &str) -> String {
    if why.chars().count() <= MOST_REFUSAL {
        return why.to_string();
    }
    let kept: String = why.chars().take(MOST_REFUSAL).collect();
    format!("{kept}... (cut at {MOST_REFUSAL} characters)")
}

#[cfg(test)]
mod tests;
