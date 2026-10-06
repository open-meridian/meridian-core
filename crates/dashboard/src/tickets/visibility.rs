//! Who sees a ticket and who works it: one function each over the access
//! fold, computed per read and used by the pages, every tool and the counts
//! (requirements 8 to 10 and 16; W6.22, W6.23).
//!
//! **Seen by:** a plugin's ticket naming no account, those holding any level
//! on that plugin; naming accounts, those whose read set through that plugin
//! holds every one of them; core's or the platform's naming none, the
//! deployment admin; core's naming accounts, those who read each of them
//! through some plugin; and the filer always -- through a delegation, while
//! it still reaches what the ticket concerns, since a delegation cuts
//! tickets exactly as it cuts a page. `/mcp`'s caller's access is already
//! narrowed, so the same function serves both. **Per role** (contract v15,
//! the plan's Q7): a plugin's ticket concerns the plugin, never one of its
//! roles, so "any level on that plugin" is any level on any of its roles,
//! and its read set the union over their roles ([`Access::held`]); nothing
//! else moves.
//!
//! **Worked by** (at the page only, never through a delegation): a plugin's
//! ticket by a person holding write on any of its roles with every account
//! it names in the union of those roles' write accounts, one naming none
//! also by an admin of any of its roles (ruled 2026-10-05); core's and the platform's by
//! the deployment admin, one naming accounts only by a deployment admin who
//! also reads every one of them (ruled 2026-10-04); and the filer may close
//! what they filed as withdrawn. A worker is always someone who may see it.

use meridian_access::{Access, AccessLevel};

use super::Ticket;

/// Who is reading: the person, what they reach now -- narrowed to a
/// delegation's cover when they read through one -- and whether they do.
#[derive(Debug, Clone)]
pub struct Reader {
    pub subject: String,
    pub access: Access,
    pub through_delegation: bool,
}

/// The accounts a ticket names: its references that carry one, the found
/// in its text among them.
pub fn accounts(ticket: &Ticket) -> Vec<&str> {
    let mut named: Vec<&str> = ticket
        .references
        .iter()
        .map(|r| r.account_id.as_str())
        .filter(|a| !a.is_empty())
        .collect();
    named.sort_unstable();
    named.dedup();
    named
}

/// Whether the access reaches what the ticket concerns at all.
fn reaches(access: &Access, ticket: &Ticket) -> bool {
    if ticket.concerns.kind == "plugin" {
        access.held(&ticket.concerns.instance).holds_any()
    } else {
        access.deployment_admin || access.plugins.values().any(|held| held.holds_any())
    }
}

/// Whether `access` may read every account the ticket names through what it
/// concerns (each through some plugin, for core's and the platform's).
fn reads_every_account(access: &Access, ticket: &Ticket, named: &[&str]) -> bool {
    if ticket.concerns.kind == "plugin" {
        let read = &access.held(&ticket.concerns.instance).accounts.read;
        named.iter().all(|account| read.contains(*account))
    } else {
        named.iter().all(|account| {
            access
                .plugins
                .values()
                .any(|held| held.union().accounts.read.contains(*account))
        })
    }
}

pub fn may_see(reader: &Reader, ticket: &Ticket) -> bool {
    if ticket.filed_by.subject == reader.subject
        && (!reader.through_delegation || reaches(&reader.access, ticket))
    {
        return true;
    }
    let named = accounts(ticket);
    let access = &reader.access;
    if ticket.concerns.kind == "plugin" {
        if named.is_empty() {
            access.held(&ticket.concerns.instance).holds_any()
        } else {
            reads_every_account(access, ticket, &named)
        }
    } else if named.is_empty() {
        access.deployment_admin
    } else {
        reads_every_account(access, ticket, &named)
    }
}

/// Whether a person, at the page with their whole access, may work the
/// ticket: assign it, set its due date, resolve, close, reopen, release.
pub fn may_work(subject: &str, access: &Access, ticket: &Ticket) -> bool {
    let reader = Reader {
        subject: subject.to_string(),
        access: access.clone(),
        through_delegation: false,
    };
    if !may_see(&reader, ticket) {
        return false;
    }
    let named = accounts(ticket);
    if ticket.concerns.kind == "plugin" {
        let held = access.held(&ticket.concerns.instance);
        let writes = held.data == Some(AccessLevel::Write)
            && named
                .iter()
                .all(|account| held.accounts.write.contains(*account));
        writes || (named.is_empty() && held.admin)
    } else {
        access.deployment_admin && (named.is_empty() || reads_every_account(access, ticket, &named))
    }
}

/// Who works a ticket, as the page and a refusal say it to a person who
/// may not.
pub fn who_works(ticket: &Ticket) -> &'static str {
    if ticket.concerns.kind == "plugin" {
        "a person holding write on any of its roles and, through those, every account it \
         names, or, naming none, by an admin of any of its roles"
    } else {
        "a deployment admin who also reads every account it names"
    }
}

/// Whether a person may close what they filed as withdrawn: the filer, who
/// always sees it.
pub fn may_withdraw(subject: &str, ticket: &Ticket) -> bool {
    ticket.filed_by.subject == subject
}

/// Whether a reader may name `account` on a ticket concerning `ticket`'s
/// subject: one they may read through the plugin it concerns, or through
/// any plugin for core's and the platform's.
pub fn may_name(access: &Access, concerns_plugin: Option<&str>, account: &str) -> bool {
    match concerns_plugin {
        Some(instance) => access.held(instance).accounts.read.contains(account),
        None => access
            .plugins
            .values()
            .any(|held| held.union().accounts.read.contains(account)),
    }
}

#[cfg(test)]
mod per_role {
    //! Tickets on a plugin holding several roles (contract v15, the plan's
    //! Q7 and the ruling of 2026-10-05 on who works one).

    use meridian_access::{Access, AccessLevel, Held, Levels, PluginHeld};

    use super::*;
    use crate::tickets::{Reference, Subject, Ticket};

    fn set(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// Write on operations (ACC-2), read on custody (ACC-1).
    fn tam() -> Access {
        let mut plugin = PluginHeld::default();
        plugin.roles.insert(
            "operations".into(),
            Held {
                admin: false,
                data: Some(AccessLevel::Write),
                accounts: Levels {
                    read: set(&["ACC-2"]),
                    write: set(&["ACC-2"]),
                },
            },
        );
        plugin.roles.insert(
            "custody".into(),
            Held {
                admin: false,
                data: Some(AccessLevel::Read),
                accounts: Levels {
                    read: set(&["ACC-1"]),
                    write: Default::default(),
                },
            },
        );
        let mut access = Access::default();
        access.plugins.insert("ops-1".into(), plugin);
        access
    }

    fn ticket(accounts: &[&str]) -> Ticket {
        Ticket {
            concerns: Subject {
                kind: "plugin".into(),
                instance: "ops-1".into(),
                ..Default::default()
            },
            references: accounts
                .iter()
                .map(|account| Reference {
                    kind: "account".into(),
                    value: account.to_string(),
                    account_id: account.to_string(),
                    found: true,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_plugins_ticket_is_seen_through_any_role_and_worked_by_write_on_one() {
        let reader = Reader {
            subject: "local|tam".into(),
            access: tam(),
            through_delegation: false,
        };
        assert!(may_see(&reader, &ticket(&[])), "any level on any role");
        assert!(may_see(&reader, &ticket(&["ACC-1", "ACC-2"])), "the union of the read sets");
        assert!(may_work("local|tam", &tam(), &ticket(&["ACC-2"])), "write on operations");
        assert!(
            !may_work("local|tam", &tam(), &ticket(&["ACC-1"])),
            "custody's account is read only"
        );
        assert!(may_work("local|tam", &tam(), &ticket(&[])), "naming none, write on any role");
        let mut reader_only = tam();
        reader_only.plugins.get_mut("ops-1").unwrap().roles.remove("operations");
        assert!(!may_work("local|tam", &reader_only, &ticket(&[])), "read alone works none");
    }
}
