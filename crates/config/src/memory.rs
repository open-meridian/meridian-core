//! The configuration store in memory, for tests and for a deployment with no
//! database configured. Its atomic operations hold one lock across the check
//! and the write, which is what makes them atomic.

use std::sync::Mutex;

use meridian_domain::v1::{
    AccessGroup, AccountGroup, AccountRecord, ExternalAccountLink, Permission, SignInRecord,
    UserGroup,
};

use crate::store::{KnownPlugin, Result, Snapshot, Store, Withdrawal};
use crate::DEPLOYMENT_ADMIN;

pub struct MemoryStore {
    state: Mutex<Snapshot>,
}

impl MemoryStore {
    /// Starts as a fresh deployment does: deployment admin exists, and nobody
    /// holds it.
    pub fn new() -> Self {
        let mut snapshot = Snapshot::default();
        snapshot
            .records
            .access_groups
            .push(crate::deployment_admin());
        Self {
            state: Mutex::new(snapshot),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn upsert<T: Clone>(items: &mut Vec<T>, item: &T, same: impl Fn(&T) -> bool) {
    match items.iter_mut().find(|existing| same(existing)) {
        Some(existing) => *existing = item.clone(),
        None => items.push(item.clone()),
    }
}

impl Store for MemoryStore {
    fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.state.lock().expect("store lock poisoned").clone())
    }

    fn put_account(&self, account: &AccountRecord) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.accounts, account, |a| {
            a.account_id == account.account_id
        });
        Ok(())
    }

    fn put_user_group(&self, group: &UserGroup) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.user_groups, group, |g| {
            g.user_group_id == group.user_group_id
        });
        Ok(())
    }

    fn put_account_group(&self, group: &AccountGroup) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.account_groups, group, |g| {
            g.account_group_id == group.account_group_id
        });
        Ok(())
    }

    fn put_access_group(&self, group: &AccessGroup) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.access_groups, group, |g| {
            g.access_group_id == group.access_group_id
        });
        Ok(())
    }

    fn put_link(&self, link: &ExternalAccountLink) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        state.links.retain(|l| {
            !(l.plugin_instance_id == link.plugin_instance_id
                && l.external_account_id == link.external_account_id)
        });
        if !link.account_id.is_empty() {
            state.links.push(link.clone());
        }
        Ok(())
    }

    fn add_permission(&self, permission: &Permission) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        state.records.permissions.push(permission.clone());
        Ok(())
    }

    fn withdraw_permission(&self, permission_id: &str) -> Result<Withdrawal> {
        let mut state = self.state.lock().expect("store lock poisoned");
        let permissions = &mut state.records.permissions;
        let Some(at) = permissions
            .iter()
            .position(|p| p.permission_id == permission_id)
        else {
            return Ok(Withdrawal::Unknown);
        };
        let admins = permissions
            .iter()
            .filter(|p| p.access_group_id == DEPLOYMENT_ADMIN)
            .count();
        if permissions[at].access_group_id == DEPLOYMENT_ADMIN && admins == 1 {
            return Ok(Withdrawal::LastAdmin);
        }
        permissions.remove(at);
        Ok(Withdrawal::Withdrawn)
    }

    fn install_first_admin(&self, group: &UserGroup, permission: &Permission) -> Result<bool> {
        let mut state = self.state.lock().expect("store lock poisoned");
        if state
            .records
            .permissions
            .iter()
            .any(|p| p.access_group_id == DEPLOYMENT_ADMIN)
        {
            return Ok(false);
        }
        state.records.user_groups.push(group.clone());
        state.records.permissions.push(permission.clone());
        Ok(true)
    }

    fn record_sign_in(&self, record: &SignInRecord) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.records.people, record, |p| {
            p.subject == record.subject
        });
        Ok(())
    }

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()> {
        let mut state = self.state.lock().expect("store lock poisoned");
        upsert(&mut state.plugins, plugin, |p| {
            p.plugin_instance_id == plugin.plugin_instance_id
        });
        Ok(())
    }
}
