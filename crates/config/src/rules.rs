//! What the configuration store refuses, checked against one snapshot.
//!
//! Each returns a sentence naming what was wrong, because the person reading
//! it is a deployment admin at a form, and "invalid request" tells them
//! nothing. Refusals are the spec's: deployment admin is built in; an access
//! entry names a tag its plugin carries; a permission to deployment admin
//! names no account group and every other names one.

use meridian_domain::v1::{
    AccessGroup, AccessLevel, AccountGroup, AccountState, DefineAccountRequest,
    GrantPermissionRequest, LinkExternalAccountRequest, UserGroup,
};

use crate::store::Snapshot;
use crate::DEPLOYMENT_ADMIN;

pub type Verdict = Result<(), String>;

fn required(value: &str, what: &str) -> Verdict {
    if value.trim().is_empty() {
        return Err(format!("{what} is required"));
    }
    Ok(())
}

fn exists<T>(items: &[T], id: &str, key: impl Fn(&T) -> &str, what: &str) -> Verdict {
    if items.iter().any(|item| key(item) == id) {
        Ok(())
    } else {
        Err(format!("there is no {what} {id}"))
    }
}

pub fn define_account(snapshot: &Snapshot, request: &DefineAccountRequest) -> Verdict {
    required(&request.name, "an account's name")?;
    if !request.account_id.is_empty() {
        exists(
            &snapshot.records.accounts,
            &request.account_id,
            |a| &a.account_id,
            "account",
        )?;
    }
    Ok(())
}

pub fn close_account(snapshot: &Snapshot, account_id: &str) -> Verdict {
    exists(
        &snapshot.records.accounts,
        account_id,
        |a| &a.account_id,
        "account",
    )
}

pub fn link(snapshot: &Snapshot, request: &LinkExternalAccountRequest) -> Verdict {
    required(&request.plugin_instance_id, "the plugin")?;
    required(&request.external_account_id, "the external account")?;
    exists(
        &snapshot.plugins,
        &request.plugin_instance_id,
        |p| &p.plugin_instance_id,
        "plugin that has reported, named",
    )?;
    if request.account_id.is_empty() {
        return Ok(());
    }
    let account = snapshot
        .records
        .accounts
        .iter()
        .find(|a| a.account_id == request.account_id)
        .ok_or_else(|| format!("there is no account {}", request.account_id))?;
    if account.state == AccountState::Closed as i32 {
        return Err(format!(
            "account {} is closed, and nothing new is recorded against a closed account",
            request.account_id
        ));
    }
    Ok(())
}

pub fn user_group(snapshot: &Snapshot, group: &UserGroup) -> Verdict {
    required(&group.name, "a user group's name")?;
    if !group.user_group_id.is_empty() {
        exists(
            &snapshot.records.user_groups,
            &group.user_group_id,
            |g| &g.user_group_id,
            "user group",
        )?;
    }
    Ok(())
}

pub fn account_group(snapshot: &Snapshot, group: &AccountGroup) -> Verdict {
    required(&group.name, "an account group's name")?;
    if !group.account_group_id.is_empty() {
        exists(
            &snapshot.records.account_groups,
            &group.account_group_id,
            |g| &g.account_group_id,
            "account group",
        )?;
    }
    for account in &group.account_ids {
        exists(
            &snapshot.records.accounts,
            account,
            |a| &a.account_id,
            "account",
        )?;
    }
    Ok(())
}

pub fn access_group(snapshot: &Snapshot, group: &AccessGroup) -> Verdict {
    if group.access_group_id == DEPLOYMENT_ADMIN || group.built_in {
        return Err("deployment admin is built in, and cannot be edited".into());
    }
    required(&group.name, "an access group's name")?;
    if !group.access_group_id.is_empty() {
        exists(
            &snapshot.records.access_groups,
            &group.access_group_id,
            |g| &g.access_group_id,
            "access group",
        )?;
    }
    for entry in &group.entries {
        let plugin = snapshot
            .plugins
            .iter()
            .find(|p| p.plugin_instance_id == entry.plugin_instance_id)
            .ok_or_else(|| {
                format!(
                    "no plugin {} has reported, so what it carries is unknown",
                    entry.plugin_instance_id
                )
            })?;
        if !plugin.carries(&entry.tag) {
            let carried: Vec<String> = plugin.roles.iter().chain(&plugin.tags).cloned().collect();
            return Err(format!(
                "plugin {} does not carry `{}`; it carries {}",
                entry.plugin_instance_id,
                entry.tag,
                carried.join(", ")
            ));
        }
        let known = [AccessLevel::Read as i32, AccessLevel::Write as i32];
        if !known.contains(&entry.level) {
            return Err(format!(
                "an access entry for {} names no level; it is read or write",
                entry.plugin_instance_id
            ));
        }
    }
    Ok(())
}

pub fn grant(snapshot: &Snapshot, request: &GrantPermissionRequest) -> Verdict {
    let records = &snapshot.records;
    exists(
        &records.user_groups,
        &request.user_group_id,
        |g| &g.user_group_id,
        "user group",
    )?;
    exists(
        &records.access_groups,
        &request.access_group_id,
        |g| &g.access_group_id,
        "access group",
    )?;

    if request.access_group_id == DEPLOYMENT_ADMIN {
        if !request.account_group_id.is_empty() {
            return Err(
                "a permission to deployment admin names no account group; it reaches every account"
                    .into(),
            );
        }
    } else {
        required(&request.account_group_id, "the account group")?;
        exists(
            &records.account_groups,
            &request.account_group_id,
            |g| &g.account_group_id,
            "account group",
        )?;
    }

    let duplicate = records.permissions.iter().any(|p| {
        p.user_group_id == request.user_group_id
            && p.account_group_id == request.account_group_id
            && p.access_group_id == request.access_group_id
    });
    if duplicate {
        return Err("that permission is already granted".into());
    }
    Ok(())
}
