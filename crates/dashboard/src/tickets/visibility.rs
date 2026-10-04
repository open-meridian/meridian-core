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
//! narrowed, so the same function serves both. At access per role (v14),
//! "any level on that plugin" becomes "any of its roles" and nothing else
//! moves.
//!
//! **Worked by** (at the page only, never through a delegation): a plugin's
//! ticket by a person holding write on that plugin and on every account it
//! names, one naming none also by its admins; core's and the platform's by
//! the deployment admin; and the filer may close what they filed as
//! withdrawn. A worker is always someone who may see it.

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
                .any(|held| held.accounts.read.contains(*account))
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
        named.is_empty() && access.deployment_admin
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
            .any(|held| held.accounts.read.contains(account)),
    }
}
