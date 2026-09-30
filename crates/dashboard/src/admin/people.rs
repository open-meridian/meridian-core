//! The people this dashboard can name, for choosing them into a user group
//! and for listing them (the product owner, 2026-09-30: "default by user ID
//! then login ID").
//!
//! A person is known by two identifiers, and ordered by them in that order:
//!
//! - **login ID**: the login exactly as sign-in presents it and every grant
//!   names it, `{issuer}|{subject}`: `local|ada` for an account this
//!   deployment holds, `ldap:{base}|{dn}` for a directory bound over LDAP,
//!   `{issuer URL}|{sub}` for OpenID Connect. It never changes for a person
//!   (the directory's `issuer`), so it is what a group holds.
//! - **user ID**: the part of the login a person is known by in their own
//!   directory, the most stable one held: a local account's name; the `sub`
//!   claim for OpenID Connect; for LDAP, the value of the entry's first
//!   relative name (`ada` of `uid=ada,ou=people,…`), since the whole
//!   distinguished name is the login.
//!
//! The display name is shown and searched when one is held. No email is: the
//! dashboard keeps none.
//!
//! Nobody is listed who has not been named somewhere this dashboard can see:
//! in a user group, holding a terminal session, or a local account. A person
//! from a directory who has never signed in is named by typing their login,
//! which the form still takes (design/naming-a-person-before-they-sign-in
//! owns how a directory's people are named before then).

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    pub login: String,
    pub user_id: String,
    pub name: String,
}

/// The user ID in a login (see the module's comment).
pub fn user_id(login: &str) -> String {
    let Some((issuer, subject)) = login.split_once('|') else {
        return login.to_string();
    };
    if issuer.starts_with("ldap:") {
        if let Some(value) = first_rdn_value(subject) {
            return value;
        }
    }
    subject.to_string()
}

/// The value of a distinguished name's first relative name, its escapes
/// undone (`cn=Park\, Ada,ou=people` is `Park, Ada`); none when it has no
/// `=` or no value.
fn first_rdn_value(dn: &str) -> Option<String> {
    let mut rdn = String::new();
    let mut escaped = false;
    for c in dn.chars() {
        if escaped {
            rdn.push('\\');
            rdn.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ',' || c == '+' {
            break;
        } else {
            rdn.push(c);
        }
    }
    let (_, value) = rdn.split_once('=')?;
    let mut out = String::new();
    let mut escaped = false;
    for c in value.trim().chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Everybody named in `sources`, one each by login, with the first display
/// name any source holds, ordered by user ID and then login ID.
pub fn gather<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<Person> {
    let mut by_login: BTreeMap<String, String> = BTreeMap::new();
    for (login, name) in sources {
        let login = login.trim();
        if login.is_empty() {
            continue;
        }
        let held = by_login.entry(login.to_string()).or_default();
        if held.is_empty() {
            *held = name.trim().to_string();
        }
    }
    let mut people: Vec<Person> = by_login
        .into_iter()
        .map(|(login, name)| Person {
            user_id: user_id(&login),
            login,
            name,
        })
        .collect();
    sort(&mut people);
    people
}

/// By user ID, ignoring case, then by login ID.
pub fn sort(people: &mut [Person]) {
    people.sort_by(|a, b| {
        a.user_id
            .to_lowercase()
            .cmp(&b.user_id.to_lowercase())
            .then_with(|| a.login.cmp(&b.login))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_id_is_the_part_of_the_login_a_person_is_known_by() {
        assert_eq!(user_id("local|ada"), "ada");
        assert_eq!(user_id("https://idp.example.org|8812"), "8812");
        assert_eq!(
            user_id("ldap:dc=firm,dc=internal|uid=ada.park,ou=people,dc=firm,dc=internal"),
            "ada.park"
        );
        assert_eq!(
            user_id("ldap:dc=firm|cn=Park\\, Ada,ou=people"),
            "Park, Ada"
        );
        assert_eq!(
            user_id("ldap:dc=firm|odd"),
            "odd",
            "no relative name: the subject"
        );
        assert_eq!(user_id("no-issuer"), "no-issuer");
    }

    #[test]
    fn people_are_one_each_by_login_ordered_by_user_id_then_login() {
        let people = gather([
            ("https://idp.example.org|bob", ""),
            ("local|ada", ""),
            ("local|Bob", "Bob Local"),
            ("local|ada", "Ada Park"),
            ("https://idp.example.org|ada", "Ada (directory)"),
            ("  ", "nobody"),
        ]);
        let order: Vec<(&str, &str, &str)> = people
            .iter()
            .map(|p| (p.user_id.as_str(), p.login.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("ada", "https://idp.example.org|ada", "Ada (directory)"),
                ("ada", "local|ada", "Ada Park"),
                ("bob", "https://idp.example.org|bob", ""),
                ("Bob", "local|Bob", "Bob Local"),
            ]
        );
    }
}
