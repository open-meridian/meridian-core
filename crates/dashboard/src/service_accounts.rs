//! A service account runs a plugin's autonomous work (contract v21;
//! spec/a-service-account-runs-a-plugins-autonomous-work, accepted
//! 2026-10-05; plans/a-service-account-runs-a-plugins-autonomous-work,
//! "Refreshed 2026-10-10", ruled 2026-10-11): W6.32 to W6.34 and W4.15.
//!
//! **A user that is not a person** (W6.32). A deployment admin creates one on
//! the deployment's Settings, Service accounts tab -- a name, its purpose,
//! its owner and its setting, **prepare only** (the default) or **act** --
//! changes, disables, re-enables or removes it. Its subject is its `SA-` ID,
//! by which a user group lists it in its logins and its grants reach it, as
//! a person's do. It never signs in and holds no password, key or token:
//! what vouches for it is this record, its grants and a role's "runs as".
//! It is neither the LDAP bind account ([`crate::directory`]) nor a
//! Kubernetes service account, which the words also name in core.
//!
//! **Runs as** (W6.33). A role's admin chooses, on the plugin's Access tab,
//! which service account the role's autonomous work runs as, among those
//! granted on that role, at a level no higher than its grant: the grant is
//! the permitted set (the plan's Q8).
//!
//! **The owner** (W6.34) sees what it did, within their own read access, is
//! told when it is disabled and when its prepared changes lapse, and may
//! disable it, nothing more (the plan's Q10).
//!
//! **Disabling or removing** refuses its next call ([`admission`]), lapses
//! its pending prepared changes, each its own record naming why, and tells
//! its owner. Removing keeps every record it left. No lifetime is fixed (the
//! plan's Q11).
//!
//! Every change is its own record, `ServiceAccountChange` (decisions/031):
//! its `from` and `to` say the part changed and its value, as written
//! (`setting: act`); a role's choice is scoped to `instance:role`.

pub mod admission;
pub mod pages;
pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use meridian_access::AccessLevel;
use meridian_domain::v1::AccessRecords;

use crate::web::App;
use store::{
    Change, InMemory, Notice, RunsAs, ServiceAccount, ServiceAccountStore, Setting, State,
};

/// The name's and the purpose's bounds, an account's (W6.3).
pub const NAME_MOST: usize = 200;
pub const PURPOSE_MOST: usize = 2_000;
/// A change's note (the dictionary's ServiceAccountChange.note).
pub const NOTE_MOST: usize = 2_000;
/// The most notices an owner's inbox shows.
pub const NOTICES_SHOWN: usize = 50;

/// A service account's ID as the dashboard mints it: `SA-` and a ULID, in
/// Crockford's base 32 (a short one, as the fixtures write, read too). Its
/// subject: what a user group's logins name it by.
pub fn is_service_account(subject: &str) -> bool {
    subject.strip_prefix("SA-").is_some_and(|ulid| {
        (20..=26).contains(&ulid.len())
            && ulid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
    })
}

/// What refused a change at a page: the field by its name, and the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub field: String,
    pub detail: String,
}

impl Refused {
    fn at(field: &str, detail: impl Into<String>) -> Refused {
        Refused {
            field: field.to_string(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            f.write_str(&self.detail)
        } else {
            write!(f, "{}: {}", self.field, self.detail)
        }
    }
}

/// Who makes a change at a page: their subject and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    pub subject: String,
    pub display_name: String,
    pub deployment_admin: bool,
}

/// What a deployment admin asks for, creating or changing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Asked {
    pub name: String,
    pub purpose: String,
    pub owner_subject: String,
    pub setting: Option<Setting>,
}

/// The service accounts and each role's "runs as".
pub struct ServiceAccounts {
    store: Arc<dyn ServiceAccountStore>,
    /// The accounts as last read or written, for code that cannot wait on
    /// the store (whom a prepared change is shown to).
    held: RwLock<BTreeMap<String, ServiceAccount>>,
}

impl Default for ServiceAccounts {
    fn default() -> Self {
        ServiceAccounts::keeping(Arc::new(InMemory::default()))
    }
}

/// A text the page sent, as kept: trimmed.
fn trimmed(said: &str) -> String {
    said.trim().to_string()
}

impl ServiceAccounts {
    /// Over `store`, its held view empty until [`ServiceAccounts::accounts`]
    /// reads it -- which a starting dashboard does once, off the async
    /// runtime, since the store may block on its database.
    pub fn keeping(store: Arc<dyn ServiceAccountStore>) -> Self {
        ServiceAccounts {
            store,
            held: RwLock::new(BTreeMap::new()),
        }
    }

    /// Run store work off the async runtime.
    pub async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn ServiceAccountStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || work(store.as_ref()))
            .await
            .map_err(|joined| format!("the service accounts' store failed: {joined}"))?
            .map_err(|failed| {
                tracing::error!(%failed, "the service accounts could not be read or written");
                format!("the service accounts could not be read or written: {failed}")
            })
    }

    /// The store, for work already off the async runtime (a bus handler).
    pub fn store(&self) -> &dyn ServiceAccountStore {
        self.store.as_ref()
    }

    /// Every account, removed ones too, oldest first; the held view
    /// refreshed.
    pub async fn accounts(&self) -> Result<Vec<ServiceAccount>, String> {
        let all = self.blocking(|store| store.accounts()).await?;
        self.hold(&all);
        Ok(all)
    }

    fn hold(&self, all: &[ServiceAccount]) {
        if let Ok(mut held) = self.held.write() {
            *held = all
                .iter()
                .map(|account| (account.service_account_id.clone(), account.clone()))
                .collect();
        }
    }

    fn held_one(&self, account: &ServiceAccount) {
        if let Ok(mut held) = self.held.write() {
            held.insert(account.service_account_id.clone(), account.clone());
        }
    }

    /// One account as last read or written: what cannot wait on the store.
    pub fn held(&self, id: &str) -> Option<ServiceAccount> {
        self.held.read().ok().and_then(|held| held.get(id).cloned())
    }

    /// The owner of the service account `subject` names, as last read; None
    /// for anything that is not one.
    pub fn owner_of(&self, subject: &str) -> Option<String> {
        if !is_service_account(subject) {
            return None;
        }
        self.held(subject).map(|account| account.owner_subject)
    }

    pub async fn account(&self, id: &str) -> Result<Option<ServiceAccount>, String> {
        let id = id.to_string();
        let found = self.blocking(move |store| store.account(&id)).await?;
        if let Some(account) = &found {
            self.held_one(account);
        }
        Ok(found)
    }

    pub async fn runs_as(&self) -> Result<Vec<RunsAs>, String> {
        self.blocking(|store| store.runs_as()).await
    }

    pub async fn changes(&self, id: &str) -> Result<Vec<Change>, String> {
        let id = id.to_string();
        self.blocking(move |store| store.changes(&id)).await
    }

    pub async fn notices(&self, subject: &str) -> Result<Vec<Notice>, String> {
        let subject = subject.to_string();
        self.blocking(move |store| store.notices(&subject, NOTICES_SHOWN))
            .await
    }
}

/// Each owner's name, by subject, for `accounts`: as they last signed in,
/// or as the account this deployment holds for them names them, or else
/// their subject.
pub async fn owner_names(
    app: &App,
    records: &AccessRecords,
    accounts: &[ServiceAccount],
) -> BTreeMap<String, String> {
    let local: Vec<(String, String)> = match app.accounts.clone() {
        Some(held) => tokio::task::spawn_blocking(move || held.people())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    accounts
        .iter()
        .map(|account| {
            let subject = &account.owner_subject;
            let signed = crate::tickets::display_name(records, subject);
            let name = if &signed != subject {
                signed
            } else {
                local
                    .iter()
                    .find(|(name, _)| &meridian_access::local_login(name) == subject)
                    .map(|(_, display)| display.clone())
                    .filter(|display| !display.is_empty())
                    .unwrap_or(signed)
            };
            (subject.clone(), name)
        })
        .collect()
}

/// A person the deployment knows (W6.1): one who has signed in, one a user
/// group names by login, or an account it holds itself. Never a service
/// account: an owner is a person.
pub async fn known_person(app: &App, records: &AccessRecords, subject: &str) -> bool {
    if subject.is_empty() || is_service_account(subject) {
        return false;
    }
    if records.people.iter().any(|p| p.subject == subject)
        || records
            .user_groups
            .iter()
            .any(|g| g.logins.iter().any(|login| login == subject))
    {
        return true;
    }
    let Some(accounts) = app.accounts.clone() else {
        return false;
    };
    let local = tokio::task::spawn_blocking(move || accounts.people())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    local
        .iter()
        .any(|(name, _)| meridian_access::local_login(name) == subject)
}

fn bounded(field: &str, said: &str, least: usize, most: usize) -> Result<(), Refused> {
    if let Some(found) = meridian_domain::text::refused(said) {
        return Err(Refused::at(field, found.to_string()));
    }
    let length = meridian_domain::text::characters(said);
    if length < least {
        return Err(Refused::at(
            field,
            format!("is empty; it holds 1 to {most} characters"),
        ));
    }
    if length > most {
        return Err(Refused::at(
            field,
            format!("is {length} characters; it holds at most {most}"),
        ));
    }
    Ok(())
}

/// The account a change at a page names, or why not.
async fn found(app: &App, id: &str) -> Result<ServiceAccount, Refused> {
    app.service_accounts
        .account(id)
        .await
        .map_err(|why| Refused::at("", why))?
        .ok_or_else(|| Refused::at("service_account_id", format!("no service account {id}")))
}

fn change(id: &str, from: String, to: String, by: &By, at_ns: i64, note: &str) -> Change {
    Change {
        service_account_id: id.to_string(),
        scope: String::new(),
        from,
        to,
        by: by.subject.clone(),
        at_ns,
        note: note.to_string(),
    }
}

/// W6.32: a deployment admin creates one, active, granted nothing, its
/// setting prepare only unless they chose act; one record.
pub async fn create(
    app: &App,
    by: &By,
    asked: &Asked,
    note: &str,
) -> Result<ServiceAccount, Refused> {
    if !by.deployment_admin {
        return Err(Refused::at(
            "",
            "creating a service account is a deployment admin's",
        ));
    }
    let name = trimmed(&asked.name);
    let purpose = trimmed(&asked.purpose);
    bounded("name", &name, 1, NAME_MOST)?;
    bounded("purpose", &purpose, 0, PURPOSE_MOST)?;
    bounded("note", note, 0, NOTE_MOST)?;
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refused::at("", stale.to_string()))?;
    let owner = trimmed(&asked.owner_subject);
    if !known_person(app, &records, &owner).await {
        return Err(Refused::at(
            "owner_subject",
            format!(
                "{owner:?} is no person this deployment knows: an owner is a person who has signed \
                 in, a user group names, or an account it holds"
            ),
        ));
    }
    let account = ServiceAccount {
        service_account_id: format!("SA-{}", app.prepared.ulid(now)),
        name,
        purpose,
        owner_subject: owner,
        setting: asked.setting.unwrap_or_default(),
        state: State::Active,
        created_at_ns: now,
        ..ServiceAccount::default()
    };
    let record = change(
        &account.service_account_id,
        String::new(),
        format!(
            "created: name {:?}, owner {}, setting {}, state active",
            account.name,
            account.owner_subject,
            account.setting.value()
        ),
        by,
        now,
        note,
    );
    let (kept, records_of) = (account.clone(), vec![record]);
    app.service_accounts
        .blocking(move |store| store.put(&kept, &records_of, &[]))
        .await
        .map_err(|why| Refused::at("", why))?;
    app.service_accounts.held_one(&account);
    tracing::info!(
        service_account = account.service_account_id,
        owner = account.owner_subject,
        setting = account.setting.value(),
        by = by.subject,
        "a service account created"
    );
    Ok(account)
}

/// W6.32: a deployment admin changes its name, purpose, owner or setting,
/// each part changed its own record; nothing when nothing changes. A removed
/// one is changed no more.
pub async fn change_parts(
    app: &App,
    by: &By,
    id: &str,
    asked: &Asked,
    note: &str,
) -> Result<(ServiceAccount, usize), Refused> {
    if !by.deployment_admin {
        return Err(Refused::at(
            "",
            "changing a service account is a deployment admin's",
        ));
    }
    let mut account = found(app, id).await?;
    if account.state == State::Removed {
        return Err(Refused::at(
            "service_account_id",
            format!("{id} is removed: it is changed no more, and every record it left is kept"),
        ));
    }
    let name = trimmed(&asked.name);
    let purpose = trimmed(&asked.purpose);
    bounded("name", &name, 1, NAME_MOST)?;
    bounded("purpose", &purpose, 0, PURPOSE_MOST)?;
    bounded("note", note, 0, NOTE_MOST)?;
    let now = app.clock.now_ns();
    let owner = trimmed(&asked.owner_subject);
    if owner != account.owner_subject {
        let records = app
            .records
            .current(now)
            .map_err(|stale| Refused::at("", stale.to_string()))?;
        if !known_person(app, &records, &owner).await {
            return Err(Refused::at(
                "owner_subject",
                format!("{owner:?} is no person this deployment knows"),
            ));
        }
    }
    let mut changes = Vec::new();
    if name != account.name {
        changes.push(change(
            id,
            format!("name: {}", account.name),
            format!("name: {name}"),
            by,
            now,
            note,
        ));
        account.name = name;
    }
    if purpose != account.purpose {
        changes.push(change(
            id,
            format!("purpose: {}", account.purpose),
            format!("purpose: {purpose}"),
            by,
            now,
            note,
        ));
        account.purpose = purpose;
    }
    if owner != account.owner_subject {
        changes.push(change(
            id,
            format!("owner: {}", account.owner_subject),
            format!("owner: {owner}"),
            by,
            now,
            note,
        ));
        account.owner_subject = owner;
    }
    if let Some(setting) = asked.setting.filter(|setting| *setting != account.setting) {
        changes.push(change(
            id,
            format!("setting: {}", account.setting.value()),
            format!("setting: {}", setting.value()),
            by,
            now,
            note,
        ));
        account.setting = setting;
    }
    let count = changes.len();
    if count > 0 {
        let kept = account.clone();
        app.service_accounts
            .blocking(move |store| store.put(&kept, &changes, &[]))
            .await
            .map_err(|why| Refused::at("", why))?;
        app.service_accounts.held_one(&account);
    }
    Ok((account, count))
}

/// W6.32, W6.34: its state moved -- disabled by a deployment admin or its
/// owner, re-enabled or removed by a deployment admin -- its record, its
/// owner told when someone else disabled or removed it, and on disabling or
/// removing its pending prepared changes lapsed, each its own record, its
/// owner told of each. Returns how many lapsed.
pub async fn set_state(
    app: &App,
    by: &By,
    id: &str,
    to: State,
    note: &str,
) -> Result<(ServiceAccount, usize), Refused> {
    bounded("note", note, 0, NOTE_MOST)?;
    let mut account = found(app, id).await?;
    let owner = account.owner_subject == by.subject;
    let allowed = match to {
        State::Disabled => by.deployment_admin || owner,
        State::Active | State::Removed => by.deployment_admin,
    };
    if !allowed {
        return Err(Refused::at(
            "",
            if owner {
                "its owner may disable it and nothing more: re-enabling, changing and removing \
                 it are a deployment admin's"
                    .to_string()
            } else {
                format!(
                    "{} {id} is a deployment admin's, or for disabling, its owner's",
                    match to {
                        State::Active => "re-enabling",
                        State::Disabled => "disabling",
                        State::Removed => "removing",
                    }
                )
            },
        ));
    }
    let from = account.state;
    let moves = matches!(
        (from, to),
        (State::Active, State::Disabled)
            | (State::Disabled, State::Active)
            | (State::Active, State::Removed)
            | (State::Disabled, State::Removed)
    );
    if !moves {
        return Err(Refused::at(
            "state",
            format!(
                "{id} is {}, and is not {} from there",
                from.value(),
                match to {
                    State::Active => "re-enabled",
                    State::Disabled => "disabled",
                    State::Removed => "removed",
                }
            ),
        ));
    }
    let now = app.clock.now_ns();
    account.state = to;
    let record = change(
        id,
        format!("state: {}", from.value()),
        format!("state: {}", to.value()),
        by,
        now,
        note,
    );
    let notices = if to != State::Active && !owner {
        vec![Notice {
            notice_id: app.prepared.ulid(now),
            subject: account.owner_subject.clone(),
            service_account_id: id.to_string(),
            kind: to.value().to_string(),
            prepared_call_id: String::new(),
            by_subject: by.subject.clone(),
            by_name: by.display_name.clone(),
            changed_ns: now,
        }]
    } else {
        Vec::new()
    };
    let kept = account.clone();
    app.service_accounts
        .blocking(move |store| store.put(&kept, &[record], &notices))
        .await
        .map_err(|why| Refused::at("", why))?;
    app.service_accounts.held_one(&account);
    tracing::info!(
        service_account = id,
        from = from.value(),
        to = to.value(),
        by = by.subject,
        "a service account's state changed"
    );
    let lapsed = if to == State::Active {
        0
    } else {
        lapse(app, &account, by, now).await
    };
    Ok((account, lapsed))
}

/// Its pending prepared changes lapsed, each its own record naming why, its
/// owner told of each but where they did it themselves (W6.28, W6.34).
pub async fn lapse(app: &App, account: &ServiceAccount, by: &By, now_ns: i64) -> usize {
    use crate::prepared::store::StepKind;
    use meridian_pb::v1::PreparedCallState;
    let Ok(pending) = app.prepared.blocking(|store| store.pending()).await else {
        return 0;
    };
    let why = format!(
        "the service account that prepared it, {}, was {}",
        account.name,
        account.state.value()
    );
    let lapser = crate::mcp::Caller {
        subject: by.subject.clone(),
        display_name: by.display_name.clone(),
        directory_groups: Vec::new(),
        delegation_id: String::new(),
        client_name: String::new(),
        covers: crate::delegation::Covers::default(),
        owner_subject: String::new(),
    };
    let mut lapsed = 0;
    let mut notices = Vec::new();
    for kept in pending
        .into_iter()
        .filter(|kept| kept.preparer_subject == account.service_account_id)
    {
        match app
            .prepared
            .settle(
                app,
                &kept,
                PreparedCallState::Lapsed,
                StepKind::Lapsed,
                &lapser,
                &why,
                now_ns,
            )
            .await
        {
            Ok(true) => {
                lapsed += 1;
                if account.owner_subject != by.subject {
                    notices.push(Notice {
                        notice_id: app.prepared.ulid(now_ns),
                        subject: account.owner_subject.clone(),
                        service_account_id: account.service_account_id.clone(),
                        kind: "lapsed".into(),
                        prepared_call_id: kept.prepared_call_id.clone(),
                        by_subject: by.subject.clone(),
                        by_name: by.display_name.clone(),
                        changed_ns: now_ns,
                    });
                }
            }
            Ok(false) => {}
            Err(failed) => {
                tracing::warn!(%failed, "a service account's prepared change did not lapse")
            }
        }
    }
    if !notices.is_empty() {
        if let Err(failed) = app
            .service_accounts
            .blocking(move |store| store.notify(&notices))
            .await
        {
            tracing::warn!(%failed, "a service account's owner was not told of its lapses");
        }
    }
    lapsed
}

/// The level a role's grant gives an account on one role of an instance:
/// its data level there, `write` or `read`, or None (W6.33, the plan's Q8).
pub fn granted_on(
    records: &AccessRecords,
    id: &str,
    instance: &str,
    role: &str,
) -> Option<AccessLevel> {
    meridian_access::person_access(records, id, &[])
        .plugin(instance)
        .roles
        .get(role)
        .and_then(|held| held.data)
}

/// The roles of an instance, as the records know them: none for one holding
/// no role, which is granted as a whole.
pub fn roles_of(records: &AccessRecords, instance: &str) -> Vec<String> {
    meridian_access::known_roles(records, instance)
        .map(<[String]>::to_vec)
        .unwrap_or_default()
}

/// The active and disabled accounts granted on a role, for its choice: the
/// permitted set (the plan's Q8).
pub fn granted_accounts(
    app: &App,
    records: &AccessRecords,
    instance: &str,
    role: &str,
) -> Vec<(ServiceAccount, AccessLevel)> {
    let held: Vec<ServiceAccount> = app
        .service_accounts
        .held
        .read()
        .map(|held| held.values().cloned().collect())
        .unwrap_or_default();
    held.into_iter()
        .filter(|account| account.state != State::Removed)
        .filter_map(|account| {
            granted_on(records, &account.service_account_id, instance, role)
                .map(|level| (account, level))
        })
        .collect()
}

/// W6.33: a role's admin chooses what the role's autonomous work runs as, at
/// a level no higher than the account's grant, or clears the choice; its
/// record. Nothing when nothing changes.
#[allow(clippy::too_many_arguments)]
pub async fn choose(
    app: &App,
    by: &By,
    administers: bool,
    instance: &str,
    role: &str,
    to: Option<(&str, AccessLevel)>,
    note: &str,
) -> Result<bool, Refused> {
    if !administers {
        return Err(Refused::at(
            "role",
            format!("what {role} runs as is chosen by an admin of {role} on {instance}"),
        ));
    }
    bounded("note", note, 0, NOTE_MOST)?;
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refused::at("", stale.to_string()))?;
    let roles = roles_of(&records, instance);
    if !(roles.iter().any(|held| held == role) || (roles.is_empty() && role.is_empty())) {
        return Err(Refused::at(
            "role",
            format!("{role:?} is not a role {instance} holds"),
        ));
    }
    let current = app
        .service_accounts
        .runs_as()
        .await
        .map_err(|why| Refused::at("", why))?
        .into_iter()
        .find(|chosen| chosen.instance == instance && chosen.role == role);
    let chosen = match to {
        None => None,
        Some((id, level)) => {
            let account = found(app, id).await?;
            if account.state == State::Removed {
                return Err(Refused::at(
                    "service_account_id",
                    format!("{id} is removed, and is chosen no more"),
                ));
            }
            let Some(grant) = granted_on(&records, id, instance, role) else {
                return Err(Refused::at(
                    "service_account_id",
                    format!(
                        "{} ({id}) is not granted on {role}: a role runs as one of the accounts a \
                         deployment admin granted on it",
                        account.name
                    ),
                ));
            };
            if level == AccessLevel::Write && grant != AccessLevel::Write {
                return Err(Refused::at(
                    "level",
                    format!(
                        "{} holds read on {role}, so it runs at read at most",
                        account.name
                    ),
                ));
            }
            if !matches!(level, AccessLevel::Write | AccessLevel::Read) {
                return Err(Refused::at(
                    "level",
                    "a role runs as an account at write or read",
                ));
            }
            Some(RunsAs {
                instance: instance.to_string(),
                role: role.to_string(),
                service_account_id: id.to_string(),
                level: meridian_access::level_name(level).to_string(),
            })
        }
    };
    let said = |held: Option<&RunsAs>| {
        held.map(|chosen| format!("{} at {}", chosen.service_account_id, chosen.level))
            .unwrap_or_default()
    };
    let (from, to_said) = (said(current.as_ref()), said(chosen.as_ref()));
    if from == to_said {
        return Ok(false);
    }
    let record = Change {
        service_account_id: chosen
            .as_ref()
            .or(current.as_ref())
            .map(|held| held.service_account_id.clone())
            .unwrap_or_default(),
        scope: format!("{instance}:{role}"),
        from,
        to: to_said,
        by: by.subject.clone(),
        at_ns: now,
        note: note.to_string(),
    };
    let (instance_owned, role_owned) = (instance.to_string(), role.to_string());
    app.service_accounts
        .blocking(move |store| {
            store.set_runs_as(&instance_owned, &role_owned, chosen.as_ref(), &record)
        })
        .await
        .map_err(|why| Refused::at("", why))?;
    tracing::info!(instance, role, by = by.subject, "a role's runs as chosen");
    Ok(true)
}

/// The roles each account runs for: `instance:role`, by account.
pub fn runs_for(chosen: &[RunsAs]) -> BTreeMap<String, BTreeSet<String>> {
    let mut by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for held in chosen {
        by.entry(held.service_account_id.clone())
            .or_default()
            .insert(if held.role.is_empty() {
                held.instance.clone()
            } else {
                format!("{}:{}", held.instance, held.role)
            });
    }
    by
}

/// An account's row, as the Service accounts tab and
/// `dashboard__list_service_accounts` say it.
pub fn row_json(
    owners: &BTreeMap<String, String>,
    account: &ServiceAccount,
    runs_for: &BTreeMap<String, BTreeSet<String>>,
) -> serde_json::Value {
    serde_json::json!({
        "service_account_id": account.service_account_id,
        "name": crate::mcp::others_words(&account.name),
        "purpose": crate::mcp::others_words(&account.purpose),
        "owner_subject": account.owner_subject,
        "owner": owners
            .get(&account.owner_subject)
            .cloned()
            .unwrap_or_else(|| account.owner_subject.clone()),
        "setting": account.setting.value(),
        "state": account.state.value(),
        "runs_for": runs_for
            .get(&account.service_account_id)
            .map(|roles| roles.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        "last_used_ns": account.last_used_ns,
        "last_refusal": crate::mcp::others_words(&account.last_refusal),
        "last_refusal_ns": account.last_refusal_ns,
    })
}

/// A notice to an owner as a tool answers it (W6.24, W6.34): what happened,
/// the account, the prepared change where one lapsed, by whom, when. Names,
/// never quotes.
pub fn notice_json(notice: &Notice) -> serde_json::Value {
    serde_json::json!({
        "notice_id": notice.notice_id,
        "kind": notice.kind,
        "service_account_id": notice.service_account_id,
        "prepared_call_id": notice.prepared_call_id,
        "by": notice.by_name,
        "changed_ns": notice.changed_ns,
    })
}

/// The owner's notices as the Inbox draws them, above the tickets': one
/// line each. Empty where there are none.
pub fn notices_rows(app: &App, notices: &[Notice]) -> String {
    use crate::html::escape;
    if notices.is_empty() {
        return String::new();
    }
    let rows: String = notices
        .iter()
        .map(|notice| {
            let name = app
                .service_accounts
                .held(&notice.service_account_id)
                .map(|account| account.name)
                .unwrap_or_else(|| notice.service_account_id.clone());
            let what = match notice.kind.as_str() {
                "disabled" => "Disabled",
                "removed" => "Removed",
                "lapsed" => "Lapsed",
                _ => "Changed",
            };
            let change = if notice.prepared_call_id.is_empty() {
                String::new()
            } else {
                format!(
                    " <a href=\"/prepared/{id}\"><code>{id}</code></a>",
                    id = escape(&notice.prepared_call_id)
                )
            };
            format!(
                "<tr data-kind=\"{kind}\" data-service-account=\"{id}\"><td><span class=\"badge warn\">{what}</span></td>\
                 <td title=\"{id}\"><a href=\"/service-accounts/{id}\">{name}</a>{change}</td>\
                 <td class=\"wide\">{by}</td><td>{when}</td></tr>",
                kind = escape(&notice.kind),
                id = escape(&notice.service_account_id),
                name = escape(&name),
                by = escape(&notice.by_name),
                when = crate::custody::utc(notice.changed_ns),
            )
        })
        .collect();
    format!(
        "<h2>Service accounts you own</h2><table class=\"list one-line service-account-notices\">\
         <thead><tr><th>What</th><th>Service account</th><th class=\"wide\">By</th><th>When</th></tr></thead>\
         <tbody>{rows}</tbody></table>"
    )
}

/// Lapse what any account no longer active left pending: the sweep's half
/// (W6.28), for a change prepared in the moment it was disabled.
pub async fn sweep(app: &App, now_ns: i64) -> usize {
    let Ok(all) = app.service_accounts.accounts().await else {
        return 0;
    };
    let mut lapsed = 0;
    for account in all.iter().filter(|account| account.state != State::Active) {
        let by = By {
            subject: String::new(),
            display_name: "the dashboard".into(),
            deployment_admin: false,
        };
        lapsed += lapse(app, account, &by, now_ns).await;
    }
    lapsed
}

#[cfg(test)]
mod tests;
