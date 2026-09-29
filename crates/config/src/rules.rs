//! What the configuration store refuses, checked against one snapshot.
//!
//! Each returns a sentence naming what was wrong, because the person reading
//! it is a deployment admin at a form, and "invalid request" tells them
//! nothing. Refusals are the spec's: deployment admin is built in; an access
//! entry names a plugin that has reported and a level, and no tag
//! (decisions/026); a permission to deployment admin
//! names no account group and every other names one; a setting is one its
//! plugin declared, in a form its declared type reads.

use std::collections::BTreeSet;

use meridian_domain::v1::{
    AccessGroup, AccessLevel, AccountGroup, AccountState, DefineAccountRequest,
    GrantPermissionRequest, LinkExternalAccountRequest, SetPluginSettingsRequest, UserGroup,
};
use meridian_pb::v1::{SettingDeclaration, SettingType};

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

/// A link names an existing account, or a new account's name for the
/// conductor to create and link in one step, or neither to remove it; never
/// both (W6.4).
pub fn link(snapshot: &Snapshot, request: &LinkExternalAccountRequest) -> Verdict {
    required(&request.plugin_instance_id, "the plugin")?;
    required(&request.external_account_id, "the external account")?;
    exists(
        &snapshot.plugins,
        &request.plugin_instance_id,
        |p| &p.plugin_instance_id,
        "plugin that has reported, named",
    )?;
    if !request.new_account_name.is_empty() {
        if !request.account_id.is_empty() {
            return Err(
                "a link names an existing account or a new account's name, not both".into(),
            );
        }
        return required(&request.new_account_name, "a new account's name");
    }
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
        if !snapshot
            .plugins
            .iter()
            .any(|p| p.plugin_instance_id == entry.plugin_instance_id)
        {
            return Err(format!(
                "no plugin {} has reported, so the deployment does not know it",
                entry.plugin_instance_id
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

/// An access entry names a plugin and a level, and nothing else: a plugin
/// declares no tags, and a person's access to it is `read` or `write`, the
/// same for every plugin (decisions/026).
///
/// Read from the request's bytes, because the field an entry named a tag in,
/// `AccessEntry` field 2, is reserved, and decoding drops a field it does not
/// know without a word. A sender built before the revision would otherwise
/// have its tag ignored rather than refused, and whoever wrote the entry
/// would not learn that the grant is now to the whole plugin.
pub fn names_no_tag(define_access_group: &[u8]) -> Verdict {
    const GROUP: u32 = 1; // DefineAccessGroupRequest.access_group
    const ENTRIES: u32 = 3; // AccessGroup.entries
    const RETIRED_TAG: u32 = 2; // AccessEntry, reserved "tag"
    let mut named = false;
    let read = wire::each(define_access_group, |number, group| {
        let Some(group) = group.filter(|_| number == GROUP) else {
            return Some(());
        };
        wire::each(group, |number, entry| {
            let Some(entry) = entry.filter(|_| number == ENTRIES) else {
                return Some(());
            };
            wire::each(entry, |number, _| {
                named |= number == RETIRED_TAG;
                Some(())
            })
        })
    });
    match (read, named) {
        (None, _) => Err("an access group that does not decode".into()),
        (Some(()), true) => Err(
            "an access entry names a plugin and a level, `read` or `write`, and no tag: \
             a plugin declares no tags (decisions/026)"
                .into(),
        ),
        (Some(()), false) => Ok(()),
    }
}

/// Just enough of the protobuf wire format to see which fields a message
/// carries, including ones its generated type no longer has.
mod wire {
    /// Calls `field` with every field's number, and a length-delimited
    /// field's bytes; `None` when the bytes are not a message or `field` says
    /// stop.
    pub fn each(
        mut bytes: &[u8],
        mut field: impl FnMut(u32, Option<&[u8]>) -> Option<()>,
    ) -> Option<()> {
        while !bytes.is_empty() {
            let key = prost::encoding::decode_varint(&mut bytes).ok()?;
            let number = u32::try_from(key >> 3).ok()?;
            let value = match key & 7 {
                0 => {
                    prost::encoding::decode_varint(&mut bytes).ok()?;
                    None
                }
                1 => {
                    bytes = bytes.get(8..)?;
                    None
                }
                2 => {
                    let length =
                        usize::try_from(prost::encoding::decode_varint(&mut bytes).ok()?).ok()?;
                    let value = bytes.get(..length)?;
                    bytes = &bytes[length..];
                    Some(value)
                }
                5 => {
                    bytes = bytes.get(4..)?;
                    None
                }
                _ => return None,
            };
            field(number, value)?;
        }
        Some(())
    }
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

/// One setting as checked: what the plugin declared of it, and the value to
/// hold, or `None` to clear it.
pub type CheckedSetting<'a> = (&'a SettingDeclaration, Option<String>);

/// A value as its declared type reads it, written the one way the SDK reads
/// it back: an integer in decimal, a boolean as `true` or `false`, a choice as
/// one of its options' values, and text as given. A refusal names the setting
/// and the type, or a choice's options, and never the value, which may be a
/// secret typed into the wrong field.
pub fn setting_value(declaration: &SettingDeclaration, value: &str) -> Result<String, String> {
    let name = &declaration.name;
    if value.is_empty() {
        return Err(format!("setting {name} is empty; clear it instead"));
    }
    if declaration.r#type == SettingType::Integer as i32 {
        return value
            .trim()
            .parse::<i64>()
            .map(|number| number.to_string())
            .map_err(|_| format!("setting {name} is a whole number"));
    }
    if declaration.r#type == SettingType::Boolean as i32 {
        return match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok("true".into()),
            "false" | "no" | "off" | "0" => Ok("false".into()),
            _ => Err(format!("setting {name} is true or false")),
        };
    }
    if declaration.r#type == SettingType::Choice as i32 {
        let value = value.trim();
        if declaration
            .choices
            .iter()
            .any(|choice| choice.value == value)
        {
            return Ok(value.to_string());
        }
        let options: Vec<&str> = declaration
            .choices
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        return Err(if options.is_empty() {
            format!("setting {name} is a choice, and the plugin declares no options for it")
        } else {
            format!("setting {name} is one of {}", options.join(", "))
        });
    }
    Ok(value.to_string())
}

/// W6.11. Every setting named is one the plugin declared when it last
/// registered, named once, and its value is one its type reads. Checked
/// whole before anything is written, so a refusal changes nothing.
pub fn plugin_settings<'a>(
    snapshot: &'a Snapshot,
    request: &SetPluginSettingsRequest,
) -> Result<Vec<CheckedSetting<'a>>, String> {
    let plugin = &request.plugin_instance_id;
    required(plugin, "the plugin")?;
    if !snapshot
        .plugins
        .iter()
        .any(|known| &known.plugin_instance_id == plugin)
    {
        return Err(format!(
            "no plugin {plugin} has reported, so what it needs is unknown"
        ));
    }
    let declared = snapshot
        .declared_settings
        .get(plugin)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let find = |name: &str| {
        declared
            .iter()
            .find(|declaration| declaration.name == name)
            .ok_or_else(|| format!("plugin {plugin} declares no setting named {name}"))
    };

    let mut named = BTreeSet::new();
    let mut checked = Vec::new();
    for given in &request.values {
        if !named.insert(given.name.as_str()) {
            return Err(format!("setting {} is given twice", given.name));
        }
        let declaration = find(&given.name)?;
        checked.push((declaration, Some(setting_value(declaration, &given.value)?)));
    }
    for name in &request.cleared {
        if !named.insert(name.as_str()) {
            return Err(format!(
                "setting {name} is both given and cleared, or cleared twice"
            ));
        }
        checked.push((find(name)?, None));
    }
    Ok(checked)
}
