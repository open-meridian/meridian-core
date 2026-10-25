//! A prepared change waits for the person (contract v20; spec/an-agents-
//! prepared-change-waits-for-the-person, plans/an-agents-prepared-change-
//! waits-for-the-person, accepted 2026-10-10): W6.26 to W6.29 and W6.31.
//!
//! **Preparing** (W6.26). An agent through `/mcp` (`dashboard__prepare_call`)
//! or a person at a page (the reserved field `prepare`, or Prepare on
//! core's own pages) names an act, its arguments and a reason. The dashboard
//! checks it as the act's own check would -- a plugin's act by its check
//! tool (`checked_by`), called with the preparer's claims at the act's
//! level; one of core's thirteen by the page's own code, run with the act's
//! bus row held back ([`crate::mcp::Acting`]) -- and keeps it if it passes,
//! a `PC-` ID and its first step; nothing in the deployment changes.
//!
//! **Who it is shown to.** Everyone who may make the act -- by their access
//! to the plugin, its role and level and the accounts its input names
//! (`accounts`), or core's page's gate -- but one prepared through a
//! person's own delegation goes to that person alone until they share it,
//! and one a person prepared at a page goes to everyone else, the preparer
//! too only while the act's second pair of eyes is off.
//!
//! **Making** (W6.27). From the page it is made on (the frame opens it
//! with the prepared change in the claims, and the plugin reads it with
//! ReadPreparedCall), or in the Inbox for an act no page draws: the act made
//! as the person's own, stamped with the prepared change's ID, and the
//! prepared change settled `made` or `changed`; or discarded, shared,
//! withdrawn, or lapsed after the lapse period, with its delegation, or when
//! its act is set to may not. Each step is its own record (decisions/031);
//! the first who makes, changes or discards it settles it for all.
//!
//! **The reason** is another's words: bounded at 4,000 characters, held as
//! suspect by the ticket's rules, and marked as another's words to every
//! agent that reads it, its preparer's included.

pub mod handoff;
pub mod levels;
pub mod own;
pub mod pages;
pub mod store;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use meridian_access::{Access, AccessLevel};
use meridian_domain::v1::AccessRecords;
use meridian_pb::v1::{PreparedCall, PreparedCallState, ToolDeclaration};
use serde_json::{json, Value};

use crate::mcp::{Caller, Spec};
use crate::web::App;
use levels::{Applies, Level, Levels};
use store::{InMemory, Kept, LevelChange, LevelRow, PreparedNotice, PreparedStore, Step, StepKind};

/// The most a reason may be (W6.26).
pub const MOST_REASON: usize = 4_000;
/// How long a settled prepared change, its steps and notices are kept.
pub const KEPT_AFTER_SETTLING_NS: i64 = 90 * 24 * 3_600 * 1_000_000_000;
const DAY_NS: i64 = 24 * 3_600 * 1_000_000_000;

/// Core's acts that can be prepared (the plan's Q13; from contract v22 the
/// order path's two, Q44), by their names on the surface.
pub const CORE_PREPARABLE: [&str; 31] = [
    "dashboard__set_plugin_settings",
    "dashboard__allow_archive",
    "dashboard__withdraw_archive",
    "dashboard__set_hold",
    "dashboard__launch_plugin",
    "dashboard__stop_plugin",
    "dashboard__set_dataset_licence",
    "dashboard__set_dataset_entitlement",
    "dashboard__set_source_priority",
    "dashboard__complete_instruments",
    "dashboard__accept_offered_values",
    "dashboard__merge_instruments",
    "dashboard__ask_platform_for_instrument",
    "dashboard__set_account_attribute",
    "dashboard__set_trading_instruction",
    // Contract v24 (W6.20, W11.36, W3.28).
    "dashboard__set_local_record_orders",
    "dashboard__set_reports_usage",
    // Contract v24 (W3.26, W6.3, W6.26; approved 2026-10-15).
    "dashboard__apply_migration",
    "dashboard__close_account",
    // Contract v25 (W8.15, W6.20, W6.26).
    "dashboard__retire_plugin_version",
    "dashboard__set_licensed_schemes",
    // Contract v25 (W8.10 to W8.12, W5.19, W6.26).
    "dashboard__install_plugin",
    "dashboard__update_plugin",
    "dashboard__acknowledge_pull",
    "dashboard__set_reports_plugin_usage",
    // Contract v26 (W11.38): preparable by an agent, made by a person.
    "dashboard__set_agent_orders_demo_only",
    // Contract v26 (W6.35).
    "dashboard__set_deployment_time_zone",
    // Contract v26 (W12.6).
    "dashboard__set_model_routing",
    // Contract v27 (W6.36).
    "dashboard__set_fx_pivot_currencies",
    // Contract v28 (W6.37, W6.38).
    "dashboard__set_series_request_rule",
    "dashboard__set_stale_reference_actions_permitted",
];

/// v20's own tools, never prepared and never levelled but for the three on
/// a prepared change.
pub const OWN_TOOLS: [&str; 7] = [
    "dashboard__prepare_call",
    "dashboard__list_prepared",
    "dashboard__withdraw_prepared",
    "dashboard__read_prepared",
    "dashboard__make_prepared",
    "dashboard__discard_prepared",
    "dashboard__share_prepared",
];

/// A prepared change's ID as the dashboard mints it: `PC-` and a ULID, in
/// Crockford's base 32 (a short one, as the fixtures write, read too).
pub fn is_prepared_call_id(id: &str) -> bool {
    id.strip_prefix("PC-").is_some_and(|ulid| {
        (20..=26).contains(&ulid.len())
            && ulid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
    })
}

/// A refusal, as every tool and page here says one: the reason, each field
/// by its path, the words.
pub type Refusal = Value;

fn refusal(reason: &str, detail: impl Into<String>, paths: &[&str]) -> Refusal {
    crate::mcp::refused(
        reason,
        &detail.into(),
        paths.iter().map(|path| json!({"path": path})).collect(),
    )
}

/// The act a prepared change names, as the dashboard reaches it.
#[derive(Debug, Clone)]
pub enum Act {
    /// One of core's thirteen.
    Core(&'static Spec),
    /// A plugin's act, its check tool and the level a call opens at.
    Plugin {
        instance: String,
        declared: Box<ToolDeclaration>,
        check: Option<Box<ToolDeclaration>>,
        level: AccessLevel,
    },
}

impl Act {
    pub fn preparable(&self) -> bool {
        match self {
            Act::Core(_) => true,
            Act::Plugin { check, .. } => check.is_some(),
        }
    }

    /// Its title, as the surface lists it.
    pub fn title(&self) -> &str {
        match self {
            Act::Core(spec) => spec.title,
            Act::Plugin { declared, .. } => &declared.title,
        }
    }

    pub fn instance(&self) -> &str {
        match self {
            Act::Core(_) => "",
            Act::Plugin { instance, .. } => instance,
        }
    }

    /// The page it is made on, on the plugin's host; None for the Inbox.
    pub fn made_on(&self) -> Option<&str> {
        match self {
            Act::Core(_) => None,
            Act::Plugin { declared, .. } => {
                Some(declared.made_on.as_str()).filter(|path| !path.is_empty())
            }
        }
    }
}

/// The prepared changes, the levels and a person's own settings.
pub struct Prepared {
    store: Arc<dyn PreparedStore>,
    minted: Mutex<(u64, u128)>,
    /// The levels as last read, for code that cannot wait on the store.
    levels: std::sync::RwLock<Levels>,
}

impl Default for Prepared {
    fn default() -> Self {
        Prepared::keeping(Arc::new(InMemory::default()))
    }
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The access groups a person's permissions name: what an access group's
/// level reaches (W6.29).
pub fn access_groups_of(records: &AccessRecords, access: &Access) -> BTreeSet<String> {
    records
        .permissions
        .iter()
        .filter(|permission| access.user_group_ids.contains(&permission.user_group_id))
        .map(|permission| permission.access_group_id.clone())
        .collect()
}

/// A person's access as the records hold them, from their latest sign-in.
pub fn access_of(records: &AccessRecords, subject: &str) -> Access {
    let groups = records
        .people
        .iter()
        .find(|p| p.subject == subject)
        .map(|p| p.directory_groups.clone())
        .unwrap_or_default();
    meridian_access::person_access(records, subject, &groups)
}

/// The values a JSON object names at each path (`account_id`,
/// `positions[].account_id`): the accounts an act's input touches.
pub fn values_at(arguments: &Value, paths: &[String]) -> Vec<String> {
    fn walk(value: &Value, parts: &[&str], out: &mut BTreeSet<String>) {
        let Some((first, rest)) = parts.split_first() else {
            if let Some(said) = value.as_str().filter(|said| !said.is_empty()) {
                out.insert(said.to_string());
            }
            return;
        };
        let (name, each) = match first.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (*first, false),
        };
        let Some(next) = value.get(name) else {
            return;
        };
        if each {
            for item in next.as_array().into_iter().flatten() {
                walk(item, rest, out);
            }
        } else {
            walk(next, rest, out);
        }
    }
    let mut out = BTreeSet::new();
    for path in paths {
        let parts: Vec<&str> = path.split('.').collect();
        walk(arguments, &parts, &mut out);
    }
    out.into_iter().collect()
}

/// A person at a page, as the core acts' code takes a caller: no delegation,
/// everything they hold.
pub fn person_caller(subject: &str, display_name: &str, directory_groups: &[String]) -> Caller {
    Caller {
        subject: subject.to_string(),
        display_name: display_name.to_string(),
        directory_groups: directory_groups.to_vec(),
        delegation_id: String::new(),
        client_name: String::new(),
        covers: crate::delegation::Covers::everything(),
        owner_subject: String::new(),
    }
}

/// The state's proto name, as the tools answer it.
pub fn state_name(state: i32) -> &'static str {
    PreparedCallState::try_from(state)
        .map(|state| state.as_str_name())
        .unwrap_or("PREPARED_CALL_STATE_UNSPECIFIED")
}

/// The state as a person reads it.
pub fn state_words(state: i32) -> &'static str {
    match PreparedCallState::try_from(state) {
        Ok(PreparedCallState::Prepared) => "Waiting",
        Ok(PreparedCallState::Shared) => "Waiting, shared",
        Ok(PreparedCallState::Made) => "Made",
        Ok(PreparedCallState::Changed) => "Changed, then made",
        Ok(PreparedCallState::Discarded) => "Discarded",
        Ok(PreparedCallState::Withdrawn) => "Withdrawn",
        Ok(PreparedCallState::Lapsed) => "Lapsed",
        _ => "Unknown",
    }
}

/// Whether two typed records say the same: the made change compared with
/// what was prepared (W6.27). Each field the prepared one names, read as
/// text, the same in the other; a field the person added beside them, as a
/// form's token, does not count.
pub fn same_record(prepared: &Value, made: &Value) -> bool {
    fn text(value: &Value) -> String {
        match value {
            Value::String(said) => said.clone(),
            other => other.to_string(),
        }
    }
    match (prepared, made) {
        (Value::Object(prepared), Value::Object(made)) => {
            prepared
                .iter()
                .all(|(name, value)| match (value, made.get(name)) {
                    (Value::Object(_) | Value::Array(_), Some(other)) => same_record(value, other),
                    (_, Some(other)) => text(value) == text(other),
                    (Value::Null, None) => true,
                    (Value::String(said), None) => said.is_empty(),
                    _ => false,
                })
        }
        (Value::Array(prepared), Value::Array(made)) => {
            prepared.len() == made.len()
                && prepared.iter().zip(made).all(|(a, b)| same_record(a, b))
        }
        (prepared, made) => text(prepared) == text(made),
    }
}

impl Prepared {
    pub fn keeping(store: Arc<dyn PreparedStore>) -> Self {
        Prepared {
            store,
            minted: Mutex::new((0, 0)),
            levels: std::sync::RwLock::new(Levels::default()),
        }
    }

    /// A ULID at the deployment's time, one more than the last within the
    /// same millisecond, so they sort as minted.
    pub fn ulid(&self, now_ns: i64) -> String {
        use rand::RngCore;
        let ms = (now_ns.max(0) / 1_000_000) as u64;
        let mut minted = self.minted.lock().expect("ulid lock poisoned");
        let random = if ms <= minted.0 {
            minted.1 + 1
        } else {
            let mut bytes = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            u128::from_be_bytes(bytes) >> 49
        };
        let ms = ms.max(minted.0);
        *minted = (ms, random);
        let value: u128 = ((ms as u128) << 80) | (random & ((1u128 << 80) - 1));
        (0..26)
            .rev()
            .map(|i| CROCKFORD[((value >> (i * 5)) & 31) as usize] as char)
            .collect()
    }

    /// Run store work off the async runtime.
    pub async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn PreparedStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || work(store.as_ref()))
            .await
            .map_err(|joined| format!("the prepared changes' store failed: {joined}"))?
            .map_err(|failed| {
                tracing::error!(%failed, "the prepared changes could not be read or written");
                format!("the prepared changes could not be read or written: {failed}")
            })
    }

    /// The store, for work already off the async runtime (a bus handler).
    pub fn store(&self) -> &dyn PreparedStore {
        self.store.as_ref()
    }

    /// The levels as last read ([`Prepared::levels`]): for code that cannot
    /// wait on the store, after its caller read them.
    pub fn levels_now(&self) -> Levels {
        self.levels
            .read()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    pub async fn levels(&self) -> Result<Levels, String> {
        let read = Levels {
            rows: self.blocking(|store| store.levels()).await?,
        };
        if let Ok(mut held) = self.levels.write() {
            *held = read.clone();
        }
        Ok(read)
    }

    /// The lapse period in days (W6.28).
    pub async fn lapse_days(&self) -> u32 {
        self.blocking(|store| store.setting(levels::LAPSE_DAYS))
            .await
            .ok()
            .flatten()
            .and_then(|said| said.parse().ok())
            .unwrap_or(levels::LAPSE_DEFAULT)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Kept>, String> {
        let id = id.to_string();
        self.blocking(move |store| store.get(&id)).await
    }

    pub async fn steps(&self, id: &str) -> Result<Vec<Step>, String> {
        let id = id.to_string();
        self.blocking(move |store| store.steps(&id)).await
    }

    /// Set a level and gate at a scope, or clear it back to the act's
    /// default, its record with it; nothing when nothing changes.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_level(
        &self,
        act: &str,
        scope: &str,
        to: Option<(Level, bool)>,
        by: &Caller,
        note: &str,
        now_ns: i64,
    ) -> Result<bool, String> {
        let levels = self.levels().await?;
        let from = levels
            .at(act, scope)
            .map(|row| levels::said(Level::from_code(row.level), row.second_pair_of_eyes))
            .unwrap_or_default();
        let to_said = to
            .map(|(level, eyes)| levels::said(level, eyes))
            .unwrap_or_default();
        if from == to_said {
            return Ok(false);
        }
        let row = to.map(|(level, eyes)| LevelRow {
            act: act.to_string(),
            scope: scope.to_string(),
            level: level as i16,
            second_pair_of_eyes: eyes,
        });
        let change = LevelChange {
            act: act.to_string(),
            scope: scope.to_string(),
            from,
            to: if to_said.is_empty() {
                format!("cleared: {}", levels::said(levels::default_for(act), false))
            } else {
                to_said
            },
            by: by.subject.clone(),
            delegation: by.delegation_id.clone(),
            client: by.client_name.clone(),
            at_ns: now_ns,
            note: note.to_string(),
        };
        let (act_owned, scope_owned) = (act.to_string(), scope.to_string());
        self.blocking(move |store| {
            store.set_level(&act_owned, &scope_owned, row.as_ref(), &change)
        })
        .await?;
        Ok(true)
    }

    pub async fn set_lapse_days(
        &self,
        days: u32,
        by: &Caller,
        note: &str,
        now_ns: i64,
    ) -> Result<bool, String> {
        let from = self.lapse_days().await;
        if from == days {
            return Ok(false);
        }
        let change = LevelChange {
            act: levels::LAPSE_DAYS.into(),
            scope: "deployment".into(),
            from: from.to_string(),
            to: days.to_string(),
            by: by.subject.clone(),
            delegation: by.delegation_id.clone(),
            client: by.client_name.clone(),
            at_ns: now_ns,
            note: note.to_string(),
        };
        self.blocking(move |store| {
            store.set_setting(levels::LAPSE_DAYS, &days.to_string(), &change)
        })
        .await?;
        Ok(true)
    }

    /// Every person who may make `kept`'s act now, by the records (W6.26).
    fn may_make_act(
        app: &App,
        records: &AccessRecords,
        act: &Act,
        kept: &Kept,
        subject: &str,
    ) -> bool {
        let access = access_of(records, subject);
        match act {
            Act::Core(spec) => {
                spec.area.open_to(&access)
                    && match instance_named(kept) {
                        Some(instance) => {
                            crate::mcp::plugin_area::gate_holds(spec, &access, &instance)
                        }
                        None => true,
                    }
            }
            Act::Plugin {
                instance, declared, ..
            } => {
                let held = access.plugin(instance);
                let Some(level) =
                    crate::mcp::level_by_role(&declared.levels, &declared.roles, &held)
                else {
                    return false;
                };
                let accounts = held.union().accounts;
                kept.accounts.iter().all(|account| match level {
                    AccessLevel::Write => accounts.write.contains(account),
                    AccessLevel::Read => accounts.read.contains(account),
                    _ => true,
                }) && {
                    let _ = app;
                    true
                }
            }
        }
    }

    /// Whom a prepared change is shown to now: the person it was prepared
    /// through until shared, else everyone who may make it, its preparer
    /// left out where the gate is on and they prepared it at a page.
    pub fn shown_to(
        app: &App,
        records: &AccessRecords,
        act: &Act,
        kept: &Kept,
        eyes: bool,
    ) -> Vec<String> {
        // One a service account prepared goes to everyone who may make it
        // at once, there being no person whose own it is, and its owner is
        // always shown it (contract v21, the plan's Q7).
        if let Some(owner) = app.service_accounts.owner_of(&kept.preparer_subject) {
            let mut shown: Vec<String> = records
                .people
                .iter()
                .map(|person| person.subject.clone())
                .filter(|subject| Self::may_make_act(app, records, act, kept, subject))
                .collect();
            if !shown.contains(&owner) {
                shown.push(owner);
            }
            return shown;
        }
        let through = !kept.through_delegation.is_empty();
        if through && kept.state != PreparedCallState::Shared as i32 {
            return vec![kept.preparer_subject.clone()];
        }
        let mut shown: Vec<String> = records
            .people
            .iter()
            .map(|person| person.subject.clone())
            .filter(|subject| Self::may_make_act(app, records, act, kept, subject))
            .filter(|subject| !(eyes && !through && *subject == kept.preparer_subject))
            .collect();
        if through && !shown.contains(&kept.preparer_subject) {
            shown.push(kept.preparer_subject.clone());
        }
        shown
    }

    /// Whether `subject` may make it now (W6.27): shown to them, pending,
    /// they may make the act, and under the gate they neither prepared it nor
    /// stand behind the agent that did.
    pub fn may_make(
        app: &App,
        records: &AccessRecords,
        act: &Act,
        kept: &Kept,
        eyes: bool,
        subject: &str,
    ) -> Result<(), String> {
        if !store::PENDING.contains(&kept.state) {
            return Err(format!(
                "{} is {}: it is settled, and nothing more is made of it",
                kept.prepared_call_id,
                state_words(kept.state).to_lowercase()
            ));
        }
        if !Self::shown_to(app, records, act, kept, eyes)
            .iter()
            .any(|shown| shown == subject)
        {
            return Err(format!(
                "{} is not shown to {subject}: it goes to whoever prepared it until they share it, \
                 and to those who may make its act",
                kept.prepared_call_id
            ));
        }
        if !Self::may_make_act(app, records, act, kept, subject) {
            return Err(format!(
                "{subject} may not make {} now: it needs what its page needs, on the accounts it names",
                kept.tool_name
            ));
        }
        if eyes && kept.preparer_subject == subject {
            return Err(format!(
                "{} needs a second pair of eyes: it is made by someone other than whoever prepared \
                 it, and other than the person behind the agent that did",
                kept.tool_name
            ));
        }
        // A service account's owner counts as the person behind it (contract
        // v21, the plan's Q6): under the gate they never make what it
        // prepared; with it off they may, as anyone who may make the act.
        if eyes
            && app
                .service_accounts
                .owner_of(&kept.preparer_subject)
                .as_deref()
                == Some(subject)
        {
            return Err(format!(
                "{} needs a second pair of eyes: it is made by someone other than the service \
                 account that prepared it, and other than its owner",
                kept.tool_name
            ));
        }
        Ok(())
    }

    /// A notice to each of `to` naming the prepared change and the step.
    pub fn notices(
        &self,
        to: &[String],
        kept: &Kept,
        kind: StepKind,
        by: &Caller,
        now_ns: i64,
    ) -> Vec<PreparedNotice> {
        to.iter()
            .filter(|subject| **subject != by.subject || kind == StepKind::Prepared)
            .map(|subject| PreparedNotice {
                notice_id: self.ulid(now_ns),
                subject: subject.clone(),
                prepared_call_id: kept.prepared_call_id.clone(),
                kind: kind.said().to_string(),
                by_subject: by.subject.clone(),
                by_name: by.display_name.clone(),
                through_delegation: by.delegation_id.clone(),
                client_name: by.client_name.clone(),
                changed_ns: now_ns,
                read: false,
            })
            .collect()
    }

    /// Move it from pending to `to`, its step and notices with it: false
    /// when it had settled already.
    #[allow(clippy::too_many_arguments)]
    pub async fn settle(
        &self,
        app: &App,
        kept: &Kept,
        to: PreparedCallState,
        kind: StepKind,
        by: &Caller,
        note: &str,
        now_ns: i64,
    ) -> Result<bool, String> {
        let records = app
            .records
            .current(now_ns)
            .map_err(|stale| stale.to_string())?;
        let mut audience = match resolve_kept(app, kept, &records).await {
            Some(act) => {
                let eyes = self.eyes(app, &act, kept).await;
                Self::shown_to(app, &records, &act, kept, eyes)
            }
            None => Vec::new(),
        };
        if !audience.contains(&kept.preparer_subject) {
            audience.push(kept.preparer_subject.clone());
        }
        let notices = self.notices(&audience, kept, kind, by, now_ns);
        let step = Step {
            prepared_call_id: kept.prepared_call_id.clone(),
            kind,
            by_subject: by.subject.clone(),
            through_delegation: by.delegation_id.clone(),
            client_name: by.client_name.clone(),
            at_ns: now_ns,
            note: note.to_string(),
        };
        let id = kept.prepared_call_id.clone();
        self.blocking(move |store| {
            store.transition(&id, &store::PENDING, to as i32, &step, &notices)
        })
        .await
    }

    /// The act's gate as it stands now, for a prepared change's preparer.
    pub async fn eyes(&self, app: &App, act: &Act, kept: &Kept) -> bool {
        let Ok(levels) = self.levels().await else {
            return false;
        };
        let groups = app
            .records
            .current(app.clock.now_ns())
            .map(|records| access_groups_of(&records, &access_of(&records, &kept.preparer_subject)))
            .unwrap_or_default();
        levels
            .applies(&kept.tool_name, &groups, act.preparable(), false)
            .second_pair_of_eyes
    }

    /// Lapse what is past the lapse period, prepared through a delegation
    /// that ended, or naming an act set to may not (W6.28), each its own
    /// record. Returns how many lapsed.
    pub async fn sweep(&self, app: &App, now_ns: i64) -> usize {
        let days = i64::from(self.lapse_days().await);
        let Ok(pending) = self.blocking(|store| store.pending()).await else {
            return 0;
        };
        let levels = self.levels().await.unwrap_or_default();
        let lapser = Caller {
            subject: String::new(),
            display_name: "the dashboard".into(),
            directory_groups: Vec::new(),
            delegation_id: String::new(),
            client_name: String::new(),
            covers: crate::delegation::Covers::default(),
            owner_subject: String::new(),
        };
        let mut lapsed = 0;
        for kept in pending {
            let why = if now_ns - kept.prepared_at_ns > days * DAY_NS {
                Some(format!("past the lapse period of {days} days"))
            } else if levels.own(&kept.tool_name).0 == Level::MayNot
                || levels
                    .ceiling(&kept.tool_name)
                    .is_some_and(|(level, _)| level == Level::MayNot)
            {
                Some("its act was set to may not".to_string())
            } else if !kept.through_delegation.is_empty()
                && !app
                    .delegations
                    .delegation(&kept.through_delegation)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|delegation| delegation.live(now_ns))
            {
                Some("the delegation it was prepared through ended".to_string())
            } else {
                None
            };
            let Some(why) = why else { continue };
            match self
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
                    tracing::info!(
                        prepared_call = kept.prepared_call_id,
                        why,
                        "a prepared change lapsed"
                    );
                }
                Ok(false) => {}
                Err(failed) => tracing::warn!(%failed, "a prepared change could not be lapsed"),
            }
        }
        let before = now_ns - KEPT_AFTER_SETTLING_NS;
        if let Err(failed) = self.blocking(move |store| store.sweep(before)).await {
            tracing::warn!(%failed, "settled prepared changes could not be swept");
        }
        lapsed
    }
}

/// Whether a prepared change's contents are withheld from `subject`: it is
/// a service account's, `subject` is its owner, and the owner may not read
/// every account it names (contract v21, the plan's Q18): owning an
/// automation never opens accounts the owner may not read.
pub fn contents_withheld(app: &App, records: &AccessRecords, kept: &Kept, subject: &str) -> bool {
    if app
        .service_accounts
        .owner_of(&kept.preparer_subject)
        .as_deref()
        != Some(subject)
    {
        return false;
    }
    let reads = access_of(records, subject)
        .plugin(&kept.instance)
        .union()
        .accounts
        .read;
    !kept.accounts.iter().all(|account| reads.contains(account))
}

/// What a person outside the accounts it names reads in place of its
/// contents.
pub const WITHHELD_CONTENTS: &str = "[withheld: it names accounts outside your own read access]";

/// The plugin instance one of core's plugin-area acts names, read from its
/// arguments; None for an act naming none.
pub fn instance_named(kept: &Kept) -> Option<String> {
    let arguments: Value = serde_json::from_str(&kept.arguments_json).ok()?;
    ["plugin_instance_id", "instance_id"]
        .iter()
        .find_map(|name| arguments.get(*name).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|instance| !instance.is_empty())
}

/// The act a kept prepared change names, as it stands now: core's, or the
/// running plugin's declaration of it with its check tool. None when the
/// plugin no longer offers it.
/// The most acts the claims name (meridian.v1.CallerClaims.second_pair_of_eyes).
pub const MOST_EYES_IN_CLAIMS: usize = 200;

/// The plugin's acts, by their tool names, whose second pair of eyes is on
/// for `caller` now (W6.29, contract v20; CallerClaims.second_pair_of_eyes):
/// named in every assertion the dashboard mints for the plugin, so its pages
/// draw Prepare in place of Save. For showing only; the dashboard refuses a
/// direct make whatever a page draws.
pub async fn eyes_for_claims(app: &App, instance: &str, caller: &Caller) -> Vec<String> {
    let acts: Vec<ToolDeclaration> = app
        .health
        .view()
        .get(instance)
        .map(|report| {
            report
                .declared_tools
                .iter()
                .filter(|tool| !tool.reads)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if acts.is_empty() {
        return Vec::new();
    }
    let levels = app.prepared.levels().await.unwrap_or_default();
    let groups = app
        .records
        .current(app.clock.now_ns())
        .map(|records| access_groups_of(&records, &caller.access(&records)))
        .unwrap_or_default();
    acts.iter()
        .filter(|tool| {
            levels
                .applies(
                    &format!("{instance}__{}", tool.name),
                    &groups,
                    !tool.checked_by.is_empty(),
                    false,
                )
                .second_pair_of_eyes
        })
        .map(|tool| tool.name.clone())
        .take(MOST_EYES_IN_CLAIMS)
        .collect()
}

/// A prepared change's act by its title where the dashboard knows it, else
/// by its tool's name.
pub fn act_title(app: &App, kept: &Kept) -> String {
    let title = if kept.instance.is_empty() {
        crate::mcp::core_spec(&kept.tool_name).map(|spec| spec.title.to_string())
    } else {
        kept.tool_name.split_once("__").and_then(|(_, name)| {
            app.health.view().get(&kept.instance).and_then(|report| {
                report
                    .declared_tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .map(|tool| tool.title.clone())
            })
        })
    };
    title
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| kept.tool_name.clone())
}

pub async fn resolve_kept(app: &App, kept: &Kept, _records: &AccessRecords) -> Option<Act> {
    if kept.instance.is_empty() {
        return crate::mcp::core_spec(&kept.tool_name).map(Act::Core);
    }
    let reports = app.health.view();
    let report = reports.get(&kept.instance)?;
    let (_, name) = kept.tool_name.split_once("__")?;
    let declared = report
        .declared_tools
        .iter()
        .find(|tool| tool.name == name)?;
    let check = (!declared.checked_by.is_empty())
        .then(|| {
            report
                .declared_tools
                .iter()
                .find(|tool| tool.name == declared.checked_by && tool.reads)
        })
        .flatten();
    let level = [AccessLevel::Write, AccessLevel::Admin, AccessLevel::Read]
        .into_iter()
        .find(|level| declared.levels.contains(&(*level as i32)))
        .unwrap_or(AccessLevel::Write);
    Some(Act::Plugin {
        instance: kept.instance.clone(),
        declared: Box::new(declared.clone()),
        check: check.map(|check| Box::new(check.clone())),
        level,
    })
}

/// The act a caller names, if they may reach it: core's thirteen by their
/// page's gate, a plugin's from what `/mcp` lists them. Refused naming why.
pub async fn resolve(app: &App, caller: &Caller, tool_name: &str) -> Result<Act, Refusal> {
    if OWN_TOOLS.contains(&tool_name) {
        return Err(refusal(
            "not_preparable",
            format!("{tool_name} is one of the prepared change's own tools, and is never prepared"),
            &["tool"],
        ));
    }
    let tools = crate::mcp::reachable(app, caller)
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?;
    let Some(tool) = tools.into_iter().find(|tool| tool.name == tool_name) else {
        return Err(refusal(
            "not_listed",
            format!("{tool_name} is no act this delegation may make or prepare"),
            &["tool"],
        ));
    };
    match tool.owner {
        crate::mcp::Owner::Dashboard(spec) => {
            if CORE_PREPARABLE.contains(&tool_name) {
                Ok(Act::Core(spec))
            } else {
                Err(refusal(
                    "not_preparable",
                    format!(
                        "{tool_name} cannot be prepared: core's configuration and records acts can, \
                         tickets' acts and reads cannot"
                    ),
                    &["tool"],
                ))
            }
        }
        crate::mcp::Owner::Plugin {
            instance,
            declared,
            level,
            ..
        } => {
            if declared.reads {
                return Err(refusal(
                    "not_preparable",
                    format!("{tool_name} only reads: there is nothing to prepare"),
                    &["tool"],
                ));
            }
            let check = app.health.view().get(&instance).and_then(|report| {
                report
                    .declared_tools
                    .iter()
                    .find(|tool| tool.name == declared.checked_by && tool.reads)
                    .cloned()
            });
            if check.is_none() {
                return Err(refusal(
                    "not_preparable",
                    format!(
                        "{tool_name} names no check tool, so it cannot be prepared until its plugin \
                         names one"
                    ),
                    &["tool"],
                ));
            }
            Ok(Act::Plugin {
                instance,
                declared,
                check: check.map(Box::new),
                level,
            })
        }
    }
}

/// The level and gate that apply to `caller` on `tool_name` now.
pub async fn applies(app: &App, caller: &Caller, tool_name: &str, preparable: bool) -> Applies {
    let levels = app.prepared.levels().await.unwrap_or_default();
    let groups = app
        .records
        .current(app.clock.now_ns())
        .map(|records| access_groups_of(&records, &caller.access(&records)))
        .unwrap_or_default();
    let prepare_only_row =
        !caller.delegation_id.is_empty() && caller.covers.prepare_only.contains(tool_name);
    levels.applies(tool_name, &groups, preparable, prepare_only_row)
}

/// What a check answered: passed, with its answer kept, or refused.
pub async fn check(
    app: &App,
    caller: &Caller,
    act: &Act,
    arguments: &Value,
) -> Result<String, Refusal> {
    match act {
        Act::Core(spec) => {
            let answered = crate::mcp::Acting::checking(crate::mcp::run_core(
                app,
                caller,
                spec,
                arguments.clone(),
            ))
            .await;
            let outcome = answered.get("outcome").and_then(Value::as_str);
            let reason = answered.get("reason").and_then(Value::as_str);
            match (outcome, reason) {
                (Some("refused"), Some(crate::mcp::CHECKED)) => {
                    Ok("passed the page's own checks".into())
                }
                (Some("unchanged"), _) => Err(refusal(
                    "unchanged",
                    answered.get("detail").and_then(Value::as_str).unwrap_or(
                        "nothing named differs from what is set, so nothing would change",
                    ),
                    &[],
                )),
                _ => Err(answered),
            }
        }
        Act::Plugin {
            instance,
            check,
            level,
            ..
        } => {
            let Some(check) = check else {
                return Err(refusal("not_preparable", "no check tool", &["tool"]));
            };
            let answered = crate::mcp::plugin_call_as(
                app,
                caller,
                instance,
                check,
                *level,
                arguments.clone(),
                "",
            )
            .await;
            match answered.get("outcome").and_then(Value::as_str) {
                Some("made") | Some("unchanged") => Ok(answered
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("passed the plugin's check")
                    .chars()
                    .take(1000)
                    .collect()),
                _ => Err(answered),
            }
        }
    }
}

/// W6.26: prepare `tool_name` with `arguments` for `caller` -- a person at a
/// page when the caller names no delegation -- or the refusal, nothing kept.
pub async fn prepare(
    app: &App,
    caller: &Caller,
    tool_name: &str,
    arguments: Value,
    reason: &str,
) -> Result<Kept, Refusal> {
    if reason.chars().count() > MOST_REASON {
        return Err(refusal(
            "invalid_argument",
            format!("reason: at most {MOST_REASON} characters"),
            &["reason"],
        ));
    }
    if !arguments.is_object() {
        return Err(refusal(
            "invalid_argument",
            "arguments: a JSON object",
            &["arguments"],
        ));
    }
    let act = resolve(app, caller, tool_name).await?;
    if !caller.delegation_id.is_empty() {
        let applies = applies(app, caller, tool_name, act.preparable()).await;
        if applies.level == Level::MayNot {
            return Err(refusal(
                "may_not",
                format!(
                    "agents may not reach {tool_name} in this deployment ({}, set by {})",
                    applies.level.words(),
                    applies.from
                ),
                &["tool"],
            ));
        }
    }
    let checked = check(app, caller, &act, &arguments).await?;
    keep(app, caller, tool_name, &act, arguments, reason, checked).await
}

/// A checked change kept for `caller` -- a person, a person through a
/// delegation, or from contract v21 a service account (W4.15) -- its first
/// step and its notices to everyone it is shown to; nothing in the
/// deployment changes.
pub async fn keep(
    app: &App,
    caller: &Caller,
    tool_name: &str,
    act: &Act,
    arguments: Value,
    reason: &str,
    checked: String,
) -> Result<Kept, Refusal> {
    let now = app.clock.now_ns();
    let act = act.clone();
    let accounts = match &act {
        Act::Plugin { declared, .. } => values_at(&arguments, &declared.accounts),
        Act::Core(_) => Vec::new(),
    };
    let suspect = !crate::tickets::quarantine::matched(reason).is_empty()
        || crate::tickets::quarantine::credential_shape(reason).is_some();
    let kept = Kept {
        prepared_call_id: format!("PC-{}", app.prepared.ulid(now)),
        tool_name: tool_name.to_string(),
        instance: act.instance().to_string(),
        arguments_json: arguments.to_string(),
        preparer_subject: caller.subject.clone(),
        preparer_name: caller.display_name.clone(),
        through_delegation: caller.delegation_id.clone(),
        client_name: caller.client_name.clone(),
        reason: reason.to_string(),
        reason_suspect: suspect,
        state: PreparedCallState::Prepared as i32,
        accounts,
        checked,
        prepared_at_ns: now,
        settled_at_ns: None,
    };
    let records = app
        .records
        .current(now)
        .map_err(|stale| refusal("unavailable", stale.to_string(), &[]))?;
    let eyes = app.prepared.eyes(app, &act, &kept).await;
    let audience = Prepared::shown_to(app, &records, &act, &kept, eyes);
    let notices = app
        .prepared
        .notices(&audience, &kept, StepKind::Prepared, caller, now);
    let step = Step {
        prepared_call_id: kept.prepared_call_id.clone(),
        kind: StepKind::Prepared,
        by_subject: caller.subject.clone(),
        through_delegation: caller.delegation_id.clone(),
        client_name: caller.client_name.clone(),
        at_ns: now,
        note: String::new(),
    };
    let held = kept.clone();
    app.prepared
        .blocking(move |store| store.insert(&held, &step, &notices))
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?;
    tracing::info!(
        prepared_call = kept.prepared_call_id,
        tool = tool_name,
        by = caller.subject,
        delegation = caller.delegation_id,
        "a change prepared"
    );
    Ok(kept)
}

/// W6.27: make a prepared change as `caller`, with `arguments` where they
/// changed it first: the act made with the prepared change's ID stamped, and
/// the prepared change settled `made` or `changed`. Refused, settling
/// nothing, where they may not make it or the act refuses.
pub async fn make(
    app: &App,
    caller: &Caller,
    id: &str,
    changed: Option<Value>,
    note: &str,
) -> Result<(Value, Kept), Refusal> {
    let now = app.clock.now_ns();
    let kept = app
        .prepared
        .get(id)
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?
        .ok_or_else(|| {
            refusal(
                "not_found",
                format!("no prepared change {id}"),
                &["prepared_call_id"],
            )
        })?;
    let records = app
        .records
        .current(now)
        .map_err(|stale| refusal("unavailable", stale.to_string(), &[]))?;
    let act = resolve_kept(app, &kept, &records).await.ok_or_else(|| {
        refusal(
            "not_found",
            format!(
                "{} is no longer offered, so {id} cannot be made",
                kept.tool_name
            ),
            &[],
        )
    })?;
    let eyes = app.prepared.eyes(app, &act, &kept).await;
    Prepared::may_make(app, &records, &act, &kept, eyes, &caller.subject)
        .map_err(|why| refusal("may_not_make", why, &["prepared_call_id"]))?;
    if !caller.delegation_id.is_empty() {
        if kept.through_delegation == caller.delegation_id
            || (!kept.through_delegation.is_empty()
                && kept.preparer_subject == caller.subject
                && eyes)
        {
            return Err(refusal(
                "may_not_make",
                "an agent never makes a change it prepared",
                &["prepared_call_id"],
            ));
        }
        // The safeguard (approved 2026-10-10): an agent makes it only where
        // it could make the act itself.
        let applies = applies(app, caller, &kept.tool_name, act.preparable()).await;
        if applies.level != Level::MayMake {
            return Err(refusal(
                "prepare_only",
                format!(
                    "{} is at {} for this delegation (set by {}), so an agent may not make it, \
                     prepared or not",
                    kept.tool_name,
                    applies.level.words(),
                    applies.from
                ),
                &["prepared_call_id"],
            ));
        }
    }
    let prepared: Value = serde_json::from_str(&kept.arguments_json).unwrap_or_else(|_| json!({}));
    let arguments = changed.unwrap_or_else(|| prepared.clone());
    let is_changed = !same_record(&prepared, &arguments) || !same_record(&arguments, &prepared);
    // The note the act carries: the person's, or the reason as prepared,
    // marked as the preparer's words (W6.27).
    let mut arguments = arguments;
    if let Act::Core(_) = act {
        let note_said = if note.trim().is_empty() {
            format!(
                "[prepared by {}{}: {}]",
                kept.preparer_name,
                if kept.client_name.is_empty() {
                    String::new()
                } else {
                    format!(" through {}", kept.client_name)
                },
                kept.reason
            )
        } else {
            note.to_string()
        };
        if let Some(object) = arguments.as_object_mut() {
            if object.contains_key("note") || !note_said.is_empty() {
                object.insert(
                    "note".into(),
                    Value::String(note_said.chars().take(2000).collect()),
                );
            }
        }
    }
    let answered = match &act {
        Act::Core(spec) => {
            crate::mcp::Acting::making(
                id,
                crate::mcp::run_core(app, caller, spec, arguments.clone()),
            )
            .await
        }
        Act::Plugin {
            instance,
            declared,
            level,
            ..
        } => {
            crate::mcp::plugin_call_as(
                app,
                caller,
                instance,
                declared,
                *level,
                arguments.clone(),
                id,
            )
            .await
        }
    };
    if answered.get("outcome").and_then(Value::as_str) == Some("refused") {
        // Shown with what differs where the act refuses what was prepared.
        let step = Step {
            prepared_call_id: id.to_string(),
            kind: StepKind::ShownWithWhatDiffers,
            by_subject: caller.subject.clone(),
            through_delegation: caller.delegation_id.clone(),
            client_name: caller.client_name.clone(),
            at_ns: now,
            note: answered
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(1000)
                .collect(),
        };
        let _ = app
            .prepared
            .blocking(move |store| store.note_step(&step))
            .await;
        return Err(answered);
    }
    let (state, kind) = if is_changed {
        (PreparedCallState::Changed, StepKind::Changed)
    } else {
        (PreparedCallState::Made, StepKind::Made)
    };
    let settled = app
        .prepared
        .settle(app, &kept, state, kind, caller, note, now)
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?;
    if !settled {
        tracing::warn!(
            prepared_call = id,
            "a prepared change was made after another settled it"
        );
    }
    Ok((answered, kept))
}

/// Settle a prepared change on a plugin's answer to the person's request
/// from its page (W6.27): made or changed. Nothing on a refusal.
pub async fn settle_from_page(app: &App, who: &Caller, id: &str, made: &Value) {
    let Ok(Some(kept)) = app.prepared.get(id).await else {
        return;
    };
    let prepared: Value = serde_json::from_str(&kept.arguments_json).unwrap_or_else(|_| json!({}));
    let (state, kind) = if same_record(&prepared, made) {
        (PreparedCallState::Made, StepKind::Made)
    } else {
        (PreparedCallState::Changed, StepKind::Changed)
    };
    let now = app.clock.now_ns();
    match app
        .prepared
        .settle(app, &kept, state, kind, who, "", now)
        .await
    {
        Ok(true) => tracing::info!(
            prepared_call = id,
            by = who.subject,
            "a prepared change made on its page"
        ),
        Ok(false) => {}
        Err(failed) => {
            tracing::warn!(%failed, "a prepared change made on its page was not settled")
        }
    }
}

/// Discard, share or withdraw (W6.27, W6.28).
pub async fn decide(
    app: &App,
    caller: &Caller,
    id: &str,
    kind: StepKind,
    note: &str,
) -> Result<Kept, Refusal> {
    let now = app.clock.now_ns();
    let kept = app
        .prepared
        .get(id)
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?
        .ok_or_else(|| {
            refusal(
                "not_found",
                format!("no prepared change {id}"),
                &["prepared_call_id"],
            )
        })?;
    if !store::PENDING.contains(&kept.state) {
        return Err(refusal(
            "settled",
            format!("{id} is {}", state_words(kept.state).to_lowercase()),
            &["prepared_call_id"],
        ));
    }
    let records = app
        .records
        .current(now)
        .map_err(|stale| refusal("unavailable", stale.to_string(), &[]))?;
    let act = resolve_kept(app, &kept, &records).await;
    let own = kept.preparer_subject == caller.subject;
    let (to, allowed) = match kind {
        StepKind::Withdrawn => (
            PreparedCallState::Withdrawn,
            kept.preparer_subject == caller.subject
                && kept.through_delegation == caller.delegation_id,
        ),
        StepKind::Shared => (
            PreparedCallState::Shared,
            own && kept.state == PreparedCallState::Prepared as i32,
        ),
        StepKind::Discarded => (PreparedCallState::Discarded, {
            let eyes = match &act {
                Some(act) => app.prepared.eyes(app, act, &kept).await,
                None => false,
            };
            own || act
                .as_ref()
                .map(|act| Prepared::shown_to(app, &records, act, &kept, eyes))
                .unwrap_or_default()
                .contains(&caller.subject)
        }),
        _ => (PreparedCallState::Prepared, false),
    };
    if !allowed {
        return Err(refusal(
            "may_not",
            match kind {
                StepKind::Withdrawn => format!("{id} was not prepared through this delegation"),
                StepKind::Shared => {
                    format!("{id} is not this person's own pending prepared change")
                }
                _ => format!("{id} is not shown to {}", caller.subject),
            },
            &["prepared_call_id"],
        ));
    }
    let moved = app
        .prepared
        .settle(app, &kept, to, kind, caller, note, now)
        .await
        .map_err(|why| refusal("unavailable", why, &[]))?;
    if !moved {
        return Err(refusal(
            "settled",
            format!("{id} was settled by someone else first"),
            &["prepared_call_id"],
        ));
    }
    Ok(Kept {
        state: to as i32,
        ..kept
    })
}

/// A prepared change as a plugin is shown it (ReadPreparedCall, W4.14).
pub fn as_shown(kept: &Kept) -> PreparedCall {
    PreparedCall {
        prepared_call_id: kept.prepared_call_id.clone(),
        tool_name: kept
            .tool_name
            .split_once("__")
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| kept.tool_name.clone()),
        arguments_json: kept.arguments_json.clone(),
        prepared_by_subject: kept.preparer_subject.clone(),
        prepared_by_display_name: kept.preparer_name.clone(),
        prepared_through_client: kept.client_name.clone(),
        reason: if kept.reason_suspect {
            "[withheld: held as suspect until a person releases it]".into()
        } else {
            kept.reason.clone()
        },
        state: kept.state,
        prepared_at_ns: kept.prepared_at_ns,
    }
}

/// A prepared change as a tool answers it, the reason marked as another's
/// words (contract v18's rule for others' text).
pub fn as_json(kept: &Kept, steps: &[Step]) -> Value {
    let arguments: Value = serde_json::from_str(&kept.arguments_json).unwrap_or(Value::Null);
    let mut arguments = arguments;
    crate::mcp::others_json(&mut arguments);
    json!({
        "prepared_call_id": kept.prepared_call_id,
        "tool": kept.tool_name,
        "arguments": arguments,
        "prepared_by": kept.preparer_subject,
        "prepared_by_display_name": kept.preparer_name,
        "prepared_through_client": kept.client_name,
        "reason": if kept.reason_suspect {
            "[withheld: held as suspect until a person releases it]".to_string()
        } else {
            crate::mcp::others_words(&kept.reason)
        },
        "state": state_name(kept.state),
        "checked": kept.checked,
        "accounts": kept.accounts,
        "prepared_at_ns": kept.prepared_at_ns,
        "settled_at_ns": kept.settled_at_ns,
        "history": steps.iter().map(|step| json!({
            "step": step.kind.said(),
            "by": step.by_subject,
            "through_delegation": step.through_delegation,
            "client_name": step.client_name,
            "at_ns": step.at_ns,
            "note": crate::mcp::others_words(&step.note),
        })).collect::<Vec<_>>(),
    })
}

/// The pending prepared changes shown to `subject` now, newest first.
pub async fn waiting_for(app: &App, subject: &str) -> Vec<Kept> {
    let now = app.clock.now_ns();
    let Ok(records) = app.records.current(now) else {
        return Vec::new();
    };
    let Ok(pending) = app.prepared.blocking(|store| store.pending()).await else {
        return Vec::new();
    };
    let mut shown = Vec::new();
    for kept in pending {
        let Some(act) = resolve_kept(app, &kept, &records).await else {
            continue;
        };
        let eyes = app.prepared.eyes(app, &act, &kept).await;
        if Prepared::shown_to(app, &records, &act, &kept, eyes)
            .iter()
            .any(|shown| shown == subject)
        {
            shown.push(kept);
        }
    }
    shown.reverse();
    shown
}

/// Answer ReadPreparedCall on the bus, from now on (W4.14): for the instance
/// the envelope names and the person the sidecar stamped, only a prepared
/// change of that instance's, shown to the person and theirs to make now;
/// otherwise unset, never saying which.
pub fn serve(app: Arc<App>) {
    let reading = Arc::clone(&app);
    app.bus
        .serve(crate::prepared::own::PREPARED_CALL, move |envelope| {
            use prost::Message;
            let meta = envelope.meta.clone().unwrap_or_default();
            let instance = meta.publisher_instance_id;
            // Never a service account's (contract v21): a prepared change's
            // read is the deployment's configuration, which it does not reach.
            if !crate::plugins::is_instance(&instance)
                || meta.acting_for_subject.is_empty()
                || meta.acting_for_kind == meridian_pb::v1::SubjectKind::ServiceAccount as i32
            {
                return Err(
                    "permission_denied: a prepared change is read only for a person a plugin acts for"
                        .into(),
                );
            }
            let asked = meridian_pb::v1::ReadPreparedCallRequest::decode(&envelope.payload[..])
                .map_err(|failed| format!("invalid_argument: the read did not read: {failed}"))?;
            let reply = tokio::runtime::Handle::current().block_on(async {
                let kept = reading.prepared.get(&asked.prepared_call_id).await.ok().flatten();
                let now = reading.clock.now_ns();
                let records = reading.records.current(now).ok();
                let found = match (kept, records) {
                    (Some(kept), Some(records)) if kept.instance == instance => {
                        match resolve_kept(&reading, &kept, &records).await {
                            Some(act) => {
                                let eyes = reading.prepared.eyes(&reading, &act, &kept).await;
                                Prepared::may_make(
                                    &reading,
                                    &records,
                                    &act,
                                    &kept,
                                    eyes,
                                    &meta.acting_for_subject,
                                )
                                .ok()
                                .map(|_| as_shown(&kept))
                            }
                            None => None,
                        }
                    }
                    _ => None,
                };
                meridian_pb::v1::ReadPreparedCallReply {
                    prepared_call: found,
                }
            });
            Ok((
                "meridian.v1.ReadPreparedCallReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    own::serve(app);
}

#[cfg(test)]
mod tests;
