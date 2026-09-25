//! Signing people in against the firm's LDAP, bound directly.
//!
//! Decision 018: the deployment runs no identity server. Where a firm has a
//! provider we federate to it; where they have LDAP we come here; where they
//! have neither the deployment holds the accounts itself.
//!
//! What this owes the access model is small and is the whole of it:
//! authenticate a person, and say who they are and which groups they are in.
//! Permission is decided elsewhere, by joining those groups to an account
//! group and an access group.
//!
//! Two properties are load-bearing, and both are ways LDAP authentication is
//! commonly got wrong rather than incidental details.
//!
//! **A password is never empty.** An LDAP simple bind with an empty password
//! is an *unauthenticated* bind, and a conforming server answers it with
//! success. A sign-in form that passes one through authenticates anybody who
//! types a username and nothing else. This refuses before it binds.
//!
//! **A username never reaches a filter unescaped.** `*` alone would match
//! every person in the tree, and a crafted value closes the filter and opens
//! another. Values are escaped by RFC 4515.

use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};

/// Where the firm's directory is, and how a person is found in it.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    /// In order. The first that answers is used, so a firm may name a replica
    /// and have a sign-in survive one server being down.
    pub servers: Vec<String>,
    /// Upgrade an `ldap://` connection to TLS before anything is sent. Off,
    /// a plain `ldap://` server receives the bind and every person's password
    /// as they were typed; an `ldaps://` one is encrypted either way.
    pub start_tls: bool,
    /// Where the search starts.
    pub base_dn: String,
    /// Who this deployment searches as. It reads people and their groups and
    /// needs nothing else; it is never used to authenticate anybody.
    pub bind_dn: String,
    pub bind_password: String,
    /// How a person is found from what they typed. `{}` is the escaped name.
    pub user_filter: String,
    /// Where the person's groups are read from. `memberOf` on most
    /// directories, and it is an attribute rather than a second search
    /// because that is what the e2e's tree and every managed directory we
    /// have met present.
    pub group_attribute: String,
}

/// Who signed in, in the only terms the access model wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// The person's distinguished name, which is what does not change when
    /// they are renamed or marry or move team. Half of a login, the other
    /// half being the issuer.
    pub subject: String,
    pub name: String,
    pub email: String,
    pub groups: Vec<String>,
}

/// Why a sign-in did not happen.
///
/// `Refused` is what a person is shown and says nothing about which half was
/// wrong, because a message distinguishing an unknown name from a wrong
/// password is a way to enumerate a firm's staff. The rest are the
/// deployment's own problem and are said plainly, since nobody signing in can
/// do anything about them and an operator needs to know.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The name or the password. Which, deliberately unsaid.
    Refused,
    /// No server answered.
    Unreachable(String),
    /// This deployment's own bind failed: a wrong service account, usually.
    NotOurs(String),
    /// The directory answered, and not in a way this understands.
    Confused(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused => write!(out, "that name and password were not accepted"),
            Failure::Unreachable(detail) => write!(out, "no directory server answered: {detail}"),
            Failure::NotOurs(detail) => {
                write!(
                    out,
                    "this deployment could not sign in to the directory: {detail}"
                )
            }
            Failure::Confused(detail) => write!(out, "the directory answered oddly: {detail}"),
        }
    }
}

/// Escape a value for an LDAP filter, RFC 4515.
///
/// Its own function with its own test, because the failure is silent: an
/// unescaped `*` matches every person in the tree, and the search that
/// follows finds many and refuses -- which reads as a directory problem
/// rather than as the injection it is.
fn escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\\' => out.push_str("\\5c"),
            '\0' => out.push_str("\\00"),
            '/' => out.push_str("\\2f"),
            other => out.push(other),
        }
    }
    out
}

impl Directory {
    /// What this directory is called in a login.
    ///
    /// A login is `{issuer}|{subject}`, and both halves are written into every
    /// permission ever granted, so this must never change for a directory
    /// that stays the same. The base is what identifies it: a firm may add a
    /// replica or move a server, and neither makes their people different
    /// people. The server URL would.
    ///
    /// **Not yet ruled.** [[design/naming-a-person-before-they-sign-in]] owns
    /// the question, and changing this later invalidates every grant.
    pub fn issuer(&self) -> String {
        format!("ldap:{}", self.base_dn)
    }

    /// Bind, find the person, bind as them, and read their groups.
    pub async fn authenticate(&self, name: &str, password: &str) -> Result<Person, Failure> {
        // Before anything reaches the network. An empty password is an
        // unauthenticated bind and the server will say yes.
        if name.trim().is_empty() || password.is_empty() {
            return Err(Failure::Refused);
        }

        let (mut ldap, connection) = self.connect().await?;
        ldap3::drive!(connection);

        ldap.simple_bind(&self.bind_dn, &self.bind_password)
            .await
            .map_err(|failed| Failure::NotOurs(failed.to_string()))?
            .success()
            .map_err(|failed| Failure::NotOurs(failed.to_string()))?;

        let filter = self.user_filter.replace("{}", &escaped(name));
        let attributes = ["dn", "cn", "mail", self.group_attribute.as_str()];
        let (entries, _) = ldap
            .search(&self.base_dn, Scope::Subtree, &filter, attributes)
            .await
            .map_err(|failed| Failure::Confused(failed.to_string()))?
            .success()
            .map_err(|failed| Failure::Confused(failed.to_string()))?;

        // Exactly one. None is an unknown person; several means the filter
        // does not identify anybody, and binding as whichever came back first
        // would be picking a person at random.
        let entry = match entries.len() {
            1 => SearchEntry::construct(entries.into_iter().next().expect("one entry")),
            0 => return Err(Failure::Refused),
            several => {
                return Err(Failure::Confused(format!(
                    "{several} people match {filter}; the user filter does not identify one"
                )))
            }
        };

        // The password, checked by the directory and never by us. A bind that
        // fails here is the wrong password, which is the person's business;
        // anything else is ours.
        let bound = ldap
            .simple_bind(&entry.dn, password)
            .await
            .map_err(|failed| Failure::Confused(failed.to_string()))?;
        if bound.success().is_err() {
            return Err(Failure::Refused);
        }

        let _ = ldap.unbind().await;

        let one = |attribute: &str| {
            entry
                .attrs
                .get(attribute)
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_default()
        };
        Ok(Person {
            subject: entry.dn.clone(),
            name: one("cn"),
            email: one("mail"),
            groups: entry
                .attrs
                .get(&self.group_attribute)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// Whether this deployment can use this directory at all: a server
    /// answers, the service account binds, and the base is there to search.
    ///
    /// What first run tests a firm's answer with (W7.4), so a wrong address
    /// or password is a finding on the wizard's page rather than every
    /// sign-in failing after the configuration has been applied. It signs
    /// nobody in and reads one entry, the base's own.
    pub async fn check(&self) -> Result<(), Failure> {
        // The same reason as a person's: an empty password is an
        // unauthenticated bind, the server says yes, and a check that passed
        // on it would pass an answer that can read nothing.
        if self.bind_dn.trim().is_empty() || self.bind_password.is_empty() {
            return Err(Failure::NotOurs(
                "a bind DN and its password are both needed; an empty password \
                 is an anonymous bind"
                    .into(),
            ));
        }

        let (mut ldap, connection) = self.connect().await?;
        ldap3::drive!(connection);

        ldap.simple_bind(&self.bind_dn, &self.bind_password)
            .await
            .map_err(|failed| Failure::NotOurs(failed.to_string()))?
            .success()
            .map_err(|failed| Failure::NotOurs(failed.to_string()))?;

        let (entries, _) = ldap
            .search(&self.base_dn, Scope::Base, "(objectClass=*)", ["dn"])
            .await
            .map_err(|failed| Failure::Confused(failed.to_string()))?
            .success()
            .map_err(|failed| {
                Failure::Confused(format!(
                    "the base {} could not be read: {failed}",
                    self.base_dn
                ))
            })?;
        let _ = ldap.unbind().await;

        match entries.len() {
            1 => Ok(()),
            _ => Err(Failure::Confused(format!(
                "the base {} is not an entry this account can read",
                self.base_dn
            ))),
        }
    }

    /// The first server that answers.
    async fn connect(&self) -> Result<(ldap3::Ldap, ldap3::LdapConnAsync), Failure> {
        let mut refusals = Vec::new();
        let settings = LdapConnSettings::new().set_starttls(self.start_tls);
        for server in &self.servers {
            match LdapConnAsync::with_settings(settings.clone(), server).await {
                Ok((connection, ldap)) => return Ok((ldap, connection)),
                Err(failed) => refusals.push(format!("{server}: {failed}")),
            }
        }
        Err(Failure::Unreachable(if refusals.is_empty() {
            "no servers are configured".to_string()
        } else {
            refusals.join("; ")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_value_cannot_close_the_filter_it_is_in() {
        assert_eq!(escaped("alice"), "alice");
        // The one that matches everybody.
        assert_eq!(escaped("*"), "\\2a");
        // And the one that ends the filter and starts another.
        assert_eq!(escaped("alice)(uid=*"), "alice\\29\\28uid=\\2a");
        assert_eq!(escaped("back\\slash"), "back\\5cslash");
    }

    #[tokio::test]
    async fn an_empty_password_is_refused_before_anything_is_dialled() {
        // No server is configured, so reaching the network at all would fail
        // with Unreachable. Refused proves this never got that far -- which
        // is the point, because the server would have said yes.
        let directory = Directory {
            base_dn: "dc=example,dc=org".into(),
            ..Default::default()
        };
        assert_eq!(
            directory.authenticate("alice", "").await,
            Err(Failure::Refused)
        );
        assert_eq!(directory.authenticate("", "").await, Err(Failure::Refused));
    }
}
