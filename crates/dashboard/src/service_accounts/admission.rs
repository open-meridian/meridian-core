//! A plugin's call as its role's service account, admitted through the act's
//! door (W4.15, contract v21; the plan's Q4 and Q14, ruled 2026-10-11): the
//! row PluginCallsAsServiceAccount on `platform.config.command.call-as-
//! service-account`, which a sidecar sends as itself, for the instance it
//! serves and no other, with no person stamped.
//!
//! **What is checked**, in order, refusing at the first that fails, each
//! refusal `permission_denied`, `failed_precondition` or `invalid_argument`
//! naming the field (`role`, `tool_name`, `arguments_json`, `reason`) with
//! no refusal code of its own:
//!
//! 1. the role is one the instance was launched with, and the tool is one of
//!    its acts on that role by the tools its sidecar reported (W4.8) -- never
//!    a read, another role's act or a core tool;
//! 2. the role's admin chose a service account for it (W6.33), and it is
//!    active;
//! 3. the account holds `write` on the role, and the "runs as" level is
//!    `write`;
//! 4. the arguments a JSON object (`arguments_json`); then the level that
//!    applies to the act for the account -- the strictest of the ceiling,
//!    the plugin admin's level, each access group's reaching the account
//!    (it joins user groups by its subject, as a person does) --
//!    and the act's second pair of eyes (W6.29), with the account's own
//!    setting the strictest of them: at "may not" it is refused by name; an
//!    act with no check tool cannot be prepared, so it is refused an account
//!    set to prepare only;
//! 5. a reason, 1 to 4,000 characters, untrusted text as a prepared change's
//!    is (W6.26).
//!
//! **Made** where it may make the act: the assertion a person's call gets
//! (W6.20), addressed to this instance, single use, at most 60 seconds, at
//! `write` with the account's accounts on the role and that role's entry
//! alone, naming the tool, its subject the account's, marked a service
//! account and naming its owner; sent to the act's route as CallPluginTool
//! sends a tool's. The answer carries the act's own answer.
//!
//! **Prepared** where it is held to preparing: the act's check tool called
//! with the account's claims and, on a pass, the prepared change kept, its
//! preparer the service account, shown to everyone who may make it and to
//! its owner (W6.26). The check's refusal is answered by path, nothing kept.
//!
//! **Recorded** (decisions/031): the call record names the account, the
//! tool, the outcome and its reason, never the arguments (W6.20), and the
//! account's last use and last refusal show on its row.

use std::sync::Arc;
use std::time::Instant;

use meridian_access::{AccessLevel, Levels, RoleSession};
use meridian_domain::v1::Envelope;
use meridian_pb::v1::{CallAsServiceAccountReply, CallAsServiceAccountRequest, ToolDeclaration};
use prost::Message;
use serde_json::Value;

use super::store::{ServiceAccount, Setting, State};
use crate::prepared::levels::Level;
use crate::web::App;

/// PluginCallsAsServiceAccount (W4.15): the sixth named exception to a
/// `config` command being sent only for a plugin's admin (W4.9).
pub const CALL_AS_SERVICE_ACCOUNT: &str = "platform.config.command.call-as-service-account";

/// The most a reason may be, as a prepared change's (W6.26).
pub const REASON_MOST: usize = 4_000;

/// A refusal as the sidecar answers its plugin: the status, the fields by
/// path, and the words, which begin with the first field's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: &'static str,
    pub fields: Vec<String>,
    pub words: String,
}

impl Refusal {
    fn at(status: &'static str, field: &str, words: impl std::fmt::Display) -> Refusal {
        Refusal {
            status,
            fields: vec![field.to_string()],
            words: format!("{field}: {words}"),
        }
    }

    /// As the bus carries it to the sidecar: `<status>: [fields=a;b] words`.
    pub fn said(&self) -> String {
        if self.fields.is_empty() {
            format!("{}: {}", self.status, self.words)
        } else {
            format!(
                "{}: [fields={}] {}",
                self.status,
                self.fields.join(";"),
                self.words
            )
        }
    }
}

const INVALID: &str = "invalid_argument";
const DENIED: &str = "permission_denied";
const PRECONDITION: &str = "failed_precondition";
const UNAVAILABLE: &str = "unavailable";

/// The service account a call is made as, as the code that calls a plugin
/// takes a caller: its subject, its name, everything its grants give, and
/// its owner beside it.
pub fn caller_of(account: &ServiceAccount) -> crate::mcp::Caller {
    crate::mcp::Caller {
        subject: account.service_account_id.clone(),
        display_name: account.name.clone(),
        directory_groups: Vec::new(),
        delegation_id: String::new(),
        client_name: String::new(),
        covers: crate::delegation::Covers::everything(),
        owner_subject: account.owner_subject.clone(),
    }
}

/// What a person reads as the account that prepared a change (W6.24): the
/// account and its owner.
pub fn preparer_name(account: &ServiceAccount, owner_name: &str) -> String {
    format!("{}, a service account owned by {owner_name}", account.name)
}

/// What a call opens at the act's route: `write`, the account's accounts on
/// the role, and that role's entry alone (W4.15). No admin, whatever the
/// account's grants.
pub(crate) fn opening_on(
    records: &meridian_domain::v1::AccessRecords,
    account: &ServiceAccount,
    instance: &str,
    role: &str,
) -> Option<crate::plugins::Opening> {
    let held = meridian_access::person_access(records, &account.service_account_id, &[])
        .plugin(instance)
        .roles
        .get(role)
        .cloned()?;
    if held.data != Some(AccessLevel::Write) {
        return None;
    }
    let accounts: Levels = held.accounts;
    Some(crate::plugins::Opening {
        access: accounts.clone(),
        roles: vec![RoleSession {
            role: role.to_string(),
            level: AccessLevel::Write,
            accounts,
        }],
        deployment_admin: false,
        level: AccessLevel::Write,
        // The connectivity plugins it may route through (contract v22): by
        // its SA- ID, and the user groups whose logins list it.
        connectivity_instance_ids: crate::admin::connectivity::routes(
            records,
            instance,
            &account.service_account_id,
            &meridian_access::user_groups_of(records, &account.service_account_id, &[])
                .into_iter()
                .map(|group| group.user_group_id.clone())
                .collect(),
        ),
    })
}

/// One act's route called with the account's claims (W4.15): the act made,
/// or its check run.
async fn call_route(
    app: &App,
    account: &ServiceAccount,
    instance: &str,
    role: &str,
    tool: &ToolDeclaration,
    arguments: Value,
    algorithm_id: &str,
) -> Value {
    let Some(plugins) = app.plugins.as_deref() else {
        return crate::mcp::refused(
            "unavailable",
            "this dashboard serves no plugin pages: it needs its own address",
            Vec::new(),
        );
    };
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => return crate::mcp::refused("unavailable", &stale.to_string(), Vec::new()),
    };
    let Some(opened) = opening_on(&records, account, instance, role) else {
        return crate::mcp::refused(
            "not_listed",
            &format!("{} no longer holds write on {role}", account.name),
            Vec::new(),
        );
    };
    plugins
        .call_tool(
            instance,
            crate::plugins::ToolCaller {
                subject: &account.service_account_id,
                display_name: &account.name,
                delegation_id: "",
                client_name: "",
                prepared_call_id: "",
                own_settings: Vec::new(),
                second_pair_of_eyes: Vec::new(),
                owner_subject: &account.owner_subject,
                algorithm_id,
            },
            opened,
            tool,
            arguments,
            now,
        )
        .await
}

/// What a call came to, for its record.
struct Called {
    account: Option<ServiceAccount>,
    tool: String,
    outcome: Result<CallAsServiceAccountReply, Refusal>,
}

/// Answer the row on the bus, from now on.
pub fn serve(app: Arc<App>) {
    let answering = Arc::clone(&app);
    app.bus
        .serve(CALL_AS_SERVICE_ACCOUNT, move |envelope: Envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            let instance = meta.publisher_instance_id;
            if !crate::plugins::is_instance(&instance) {
                return Err(format!(
                    "{DENIED}: this is answered for a plugin instance's sidecar"
                ));
            }
            if !meta.acting_for_subject.is_empty() {
                return Err(format!(
                "{DENIED}: a service account's call is sent by the plugin as itself: nothing it \
                 sends here names a person"
            ));
            }
            let asked = CallAsServiceAccountRequest::decode(&envelope.payload[..])
                .map_err(|failed| format!("{INVALID}: the call did not read: {failed}"))?;
            // Off the async runtime already: the bus runs each handler on the
            // blocking pool, where waiting on the plugin is allowed.
            let reply = tokio::runtime::Handle::current()
                .block_on(admit(&answering, &instance, asked))
                .map_err(|refusal| refusal.said())?;
            Ok((
                "meridian.v1.CallAsServiceAccountReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
}

/// W4.15: the call admitted, made or prepared, or refused naming the field;
/// recorded either way once the account is known.
pub async fn admit(
    app: &App,
    instance: &str,
    asked: CallAsServiceAccountRequest,
) -> Result<CallAsServiceAccountReply, Refusal> {
    let started = Instant::now();
    let called_at_ns = app.clock.now_ns();
    let called = decided(app, instance, &asked).await;
    if let Some(account) = &called.account {
        let (outcome, reason) = match &called.outcome {
            Ok(reply) => (reply.outcome.clone(), String::new()),
            Err(refusal) => ("refused".to_string(), refusal.words.clone()),
        };
        let record = crate::delegation::ToolCall {
            called_at_ns,
            subject: account.service_account_id.clone(),
            delegation_id: String::new(),
            client_name: String::new(),
            owner: instance.to_string(),
            tool: called.tool.clone(),
            level: "write".into(),
            outcome: outcome.clone(),
            reason: reason.chars().take(200).collect(),
            duration_ms: started.elapsed().as_millis() as i64,
        };
        if let Err(failed) = app.delegations.record_call(record).await {
            tracing::warn!(%failed, "a service account's call was not recorded");
        }
        let id = account.service_account_id.clone();
        let refusal = (outcome == "refused")
            .then(|| format!("{}: {}", called.tool, reason))
            .map(|said| said.chars().take(1000).collect::<String>());
        if let Err(failed) = app
            .service_accounts
            .blocking(move |store| store.used(&id, called_at_ns, refusal.as_deref()))
            .await
        {
            tracing::warn!(%failed, "a service account's last use was not kept");
        }
        tracing::info!(
            instance,
            service_account = account.service_account_id,
            owner = account.owner_subject,
            tool = called.tool,
            outcome,
            "a service account's call"
        );
    }
    called.outcome
}

async fn decided(app: &App, instance: &str, asked: &CallAsServiceAccountRequest) -> Called {
    let mut called = Called {
        account: None,
        tool: format!("{instance}__{}", asked.tool_name),
        outcome: Err(Refusal::at(INVALID, "role", "unread")),
    };
    called.outcome = decide(app, instance, asked, &mut called.account).await;
    called
}

async fn decide(
    app: &App,
    instance: &str,
    asked: &CallAsServiceAccountRequest,
    account_found: &mut Option<ServiceAccount>,
) -> Result<CallAsServiceAccountReply, Refusal> {
    let now = app.clock.now_ns();
    let records = app
        .records
        .current(now)
        .map_err(|stale| Refusal::at(UNAVAILABLE, "role", stale))?;
    let role = asked.role.as_str();

    // 1. The role, and the tool among its acts.
    let reports = app.health.view();
    let report = reports.get(instance);
    let launched = report
        .map(|report| report.roles.clone())
        .filter(|roles| !roles.is_empty())
        .unwrap_or_else(|| super::roles_of(&records, instance));
    if !launched.iter().any(|held| held == role) {
        return Err(Refusal::at(
            INVALID,
            "role",
            format!("{role:?} is not a role {instance} was launched with"),
        ));
    }
    let declared = report.and_then(|report| {
        report
            .declared_tools
            .iter()
            .find(|tool| tool.name == asked.tool_name)
            .cloned()
    });
    let Some(declared) = declared.filter(|tool| {
        !tool.reads && (tool.roles.is_empty() || tool.roles.iter().any(|held| held == role))
    }) else {
        return Err(Refusal::at(
            INVALID,
            "tool_name",
            format!(
                "{:?} is not one of {instance}'s acts on {role}: a service account reaches its \
                 plugin's own acts on the role it runs for -- never a read, another role's act, \
                 or a core tool",
                asked.tool_name
            ),
        ));
    };
    let check = (!declared.checked_by.is_empty())
        .then(|| {
            report.and_then(|report| {
                report
                    .declared_tools
                    .iter()
                    .find(|tool| tool.name == declared.checked_by && tool.reads)
                    .cloned()
            })
        })
        .flatten();

    // 2. What the role runs as, and its state.
    let chosen = app
        .service_accounts
        .runs_as()
        .await
        .map_err(|why| Refusal::at(UNAVAILABLE, "role", why))?
        .into_iter()
        .find(|held| held.instance == instance && held.role == role);
    let Some(chosen) = chosen else {
        return Err(Refusal::at(
            PRECONDITION,
            "role",
            format!(
                "{role} runs as no service account; its admin chooses one on the plugin's Access \
                 tab"
            ),
        ));
    };
    let account = app
        .service_accounts
        .account(&chosen.service_account_id)
        .await
        .map_err(|why| Refusal::at(UNAVAILABLE, "role", why))?
        .ok_or_else(|| {
            Refusal::at(
                DENIED,
                "role",
                format!(
                    "{role} runs as {}, which this deployment no longer holds",
                    chosen.service_account_id
                ),
            )
        })?;
    *account_found = Some(account.clone());
    if account.state != State::Active {
        return Err(Refusal::at(
            DENIED,
            "role",
            format!(
                "{role} runs as {} ({}), which is {}: its calls are refused until a deployment \
                 admin re-enables it or the role's admin chooses another",
                account.name,
                account.service_account_id,
                account.state.value()
            ),
        ));
    }

    // 3. Its grant on the role, and the level it runs at.
    let grant = super::granted_on(&records, &account.service_account_id, instance, role);
    if grant != Some(AccessLevel::Write) {
        return Err(Refusal::at(
            DENIED,
            "role",
            format!(
                "{} holds {} on {role}, and an act needs write: a deployment admin grants it",
                account.name,
                grant.map(meridian_access::level_name).unwrap_or("nothing")
            ),
        ));
    }
    if chosen.level != "write" {
        return Err(Refusal::at(
            DENIED,
            "role",
            format!(
                "{role} runs as {} at {}, and an act needs write: its admin chooses the level",
                account.name, chosen.level
            ),
        ));
    }

    // 4. The level that applies, the gate, and the account's setting.
    let arguments: Value = serde_json::from_str(&asked.arguments_json).unwrap_or(Value::Null);
    if !arguments.is_object() {
        return Err(Refusal::at(
            INVALID,
            "arguments_json",
            "is not a JSON object: the act's typed record of inputs, an empty object when it \
             takes none",
        ));
    }
    let act = format!("{instance}__{}", declared.name);
    let caller = caller_of(&account);
    let preparable = check.is_some();
    let applies = crate::prepared::applies(app, &caller, &act, preparable).await;
    if applies.level == Level::MayNot {
        return Err(Refusal::at(
            DENIED,
            "tool_name",
            format!(
                "{act} is at may not for {} in this deployment (set by {}): no agent reaches it",
                account.name, applies.from
            ),
        ));
    }
    let makes = applies.level == Level::MayMake
        && !applies.second_pair_of_eyes
        && account.setting == Setting::Act;
    if !makes && !preparable {
        return Err(Refusal::at(
            PRECONDITION,
            "tool_name",
            format!(
                "{act} names no check tool, so it cannot be prepared, and {} is set to prepare \
                 only: set to act, at may make, it is made",
                account.name
            ),
        ));
    }

    // 5. The reason.
    let reason = asked.reason.as_str();
    if let Some(found) = meridian_domain::text::refused(reason) {
        return Err(Refusal::at(INVALID, "reason", found));
    }
    let length = meridian_domain::text::characters(reason);
    if length == 0 || length > REASON_MOST {
        return Err(Refusal::at(
            INVALID,
            "reason",
            format!("is {length} characters; a reason holds 1 to {REASON_MOST}"),
        ));
    }

    if makes {
        let answered = call_route(
            app,
            &account,
            instance,
            role,
            &declared,
            arguments,
            &asked.algorithm_id,
        )
        .await;
        return Ok(CallAsServiceAccountReply {
            outcome: "made".into(),
            prepared_call_id: String::new(),
            answer_json: answered.to_string(),
        });
    }

    // Prepared: the act's check tool with the account's claims, then kept.
    let Some(check) = check else {
        return Err(Refusal::at(PRECONDITION, "tool_name", "no check tool"));
    };
    let checked = call_route(
        app,
        &account,
        instance,
        role,
        &check,
        arguments.clone(),
        &asked.algorithm_id,
    )
    .await;
    let passed = matches!(
        checked.get("outcome").and_then(Value::as_str),
        Some("made") | Some("unchanged")
    );
    if !passed {
        let fields: Vec<String> = checked
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|field| field.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let detail = checked
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("the act's check refused it");
        return Err(Refusal {
            status: PRECONDITION,
            words: match fields.first() {
                Some(first) if !detail.starts_with(first.as_str()) => format!("{first}: {detail}"),
                _ => detail.to_string(),
            },
            fields,
        });
    }
    let owner_name = super::owner_names(app, &records, std::slice::from_ref(&account))
        .await
        .remove(&account.owner_subject)
        .unwrap_or_else(|| account.owner_subject.clone());
    let act_held = crate::prepared::Act::Plugin {
        instance: instance.to_string(),
        declared: Box::new(declared),
        check: Some(Box::new(check)),
        level: AccessLevel::Write,
    };
    let said = checked
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("passed the plugin's check")
        .chars()
        .take(1000)
        .collect::<String>();
    let mut preparer = caller;
    preparer.display_name = preparer_name(&account, &owner_name);
    let kept = crate::prepared::keep(app, &preparer, &act, &act_held, arguments, reason, said)
        .await
        .map_err(|refused| {
            Refusal::at(
                UNAVAILABLE,
                "tool_name",
                refused
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("the prepared change was not kept"),
            )
        })?;
    Ok(CallAsServiceAccountReply {
        outcome: "prepared".into(),
        prepared_call_id: kept.prepared_call_id,
        answer_json: String::new(),
    })
}
