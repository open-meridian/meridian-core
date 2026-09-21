//! What the hook does with a call, as pure functions over Zitadel's JSON.
//!
//! Tried against Zitadel v4.17.3 on 2026-09-21; the payload shapes, and the
//! reasons for each rule below, are in meridian-design's
//! reference/zitadel-group-trial.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};

/// The metadata key the group list is kept under on a Zitadel user.
pub const METADATA_KEY: &str = "groups";

/// Which SAML attributes carry what. Defaults are the common names; a firm's
/// directory that uses claim URIs names them in the chart.
#[derive(Debug, Clone)]
pub struct SamlAttributes {
    pub groups: String,
    pub username: String,
    pub given_name: String,
    pub family_name: String,
    pub email: String,
}

impl Default for SamlAttributes {
    fn default() -> Self {
        Self {
            groups: "groups".into(),
            username: "uid".into(),
            given_name: "givenName".into(),
            family_name: "sn".into(),
            email: "email".into(),
        }
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    }
}

/// The groups a brokered sign-in presented. LDAP's `memberOf` values are
/// distinguished names and are kept whole, because two groups in different
/// branches of a directory can share a common name. A sign-in presenting no
/// group attribute presents no groups -- the empty list, never "unchanged".
pub fn groups_presented(info: &Value, saml: &SamlAttributes) -> Vec<String> {
    let mut groups = if let Some(ldap) = info.get("ldap") {
        strings(ldap.pointer("/attributes/memberOf"))
    } else {
        strings(
            info.pointer("/rawInformation/attributes")
                .and_then(|a| a.get(&saml.groups)),
        )
    };
    groups.sort();
    groups.dedup();
    groups
}

fn first(attributes: &Value, name: &str) -> String {
    strings(attributes.get(name))
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// After a brokered sign-in (a response execution on
/// `RetrieveIdentityProviderIntent`): write the complete group list onto the
/// user Zitadel is about to create or update, replacing whatever was there,
/// and for SAML supply the profile Zitadel does not map.
pub fn on_intent(mut body: Value, saml: &SamlAttributes) -> Value {
    let Some(response) = body.get_mut("response") else {
        return json!({});
    };
    let info = response
        .get("idpInformation")
        .cloned()
        .unwrap_or(Value::Null);
    let groups = groups_presented(&info, saml);
    let encoded = STANDARD.encode(serde_json::to_vec(&groups).expect("strings serialise"));
    let attributes = info.pointer("/rawInformation/attributes").cloned();
    let is_saml = info.get("saml").is_some();

    for action in ["createUser", "updateUser", "addHumanUser"] {
        let Some(user) = response.get_mut(action).filter(|u| u.is_object()) else {
            continue;
        };
        let metadata = user
            .as_object_mut()
            .expect("checked an object")
            .entry("metadata")
            .or_insert_with(|| json!([]));
        if let Some(entries) = metadata.as_array_mut() {
            entries.retain(|entry| entry.get("key").and_then(Value::as_str) != Some(METADATA_KEY));
            entries.push(json!({"key": METADATA_KEY, "value": encoded}));
        }
        if let (true, Some(attributes)) = (is_saml, &attributes) {
            map_saml_profile(user, attributes, saml);
        }
    }
    body.get("response").cloned().unwrap_or_else(|| json!({}))
}

/// Zitadel maps no profile from SAML, and without a name and an email it
/// will not create the user at all.
fn map_saml_profile(user: &mut Value, attributes: &Value, saml: &SamlAttributes) {
    let username = first(attributes, &saml.username);
    let given = first(attributes, &saml.given_name);
    let family = first(attributes, &saml.family_name);
    let email = first(attributes, &saml.email);
    user["username"] = json!(username);
    user["human"]["profile"]["givenName"] = json!(given);
    user["human"]["profile"]["familyName"] = json!(family);
    user["human"]["profile"]["displayName"] = json!(format!("{given} {family}").trim());
    user["human"]["email"] = json!({"email": email, "isVerified": true});
    if let Some(links) = user
        .pointer_mut("/human/idpLinks")
        .and_then(Value::as_array_mut)
    {
        for link in links {
            link["userName"] = json!(username);
        }
    }
}

/// When a token is issued (`preaccesstoken`, `preuserinfo`): the `groups`
/// claim, from the metadata a brokered sign-in wrote, or from the person's
/// roles on the dashboard's own Zitadel project when they were made in
/// Zitadel itself, which has no groups. Roles on any other project are not
/// groups.
pub fn on_token(body: &Value, project_id: &str) -> Value {
    let mut groups: Vec<String> = Vec::new();
    for entry in body
        .get("user_metadata")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if entry.get("key").and_then(Value::as_str) != Some(METADATA_KEY) {
            continue;
        }
        let decoded = entry
            .get("value")
            .and_then(Value::as_str)
            .and_then(|v| STANDARD.decode(v).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok());
        groups.extend(decoded.unwrap_or_default());
    }
    for grant in body
        .get("user_grants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if grant.get("project_id").and_then(Value::as_str) == Some(project_id) {
            groups.extend(strings(grant.get("roles")));
        }
    }
    groups.sort();
    groups.dedup();
    json!({"append_claims": [{"key": "groups", "value": groups}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(response: &Value, action: &str) -> Vec<String> {
        let entry = response[action]["metadata"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == METADATA_KEY)
            .unwrap();
        serde_json::from_slice(&STANDARD.decode(entry["value"].as_str().unwrap()).unwrap()).unwrap()
    }

    /// The shape Zitadel sent for bob's LDAP sign-in in the trial.
    fn ldap(member_of: Value, existing: Value) -> Value {
        json!({"response": {
            "idpInformation": {"ldap": {"attributes": {"uid": ["bob"], "memberOf": member_of}}},
            "updateUser": {"userId": "391736511978012678", "username": "bob", "metadata": existing}
        }})
    }

    #[test]
    fn ldap_groups_are_written_whole_and_replace_what_was_there() {
        let body = ldap(
            json!([
                "cn=ldap-group-b,ou=groups,dc=example,dc=org",
                "cn=ldap-group-a,ou=groups,dc=example,dc=org"
            ]),
            json!([{"key": "groups", "value": STANDARD.encode(br#"["stale"]"#)}, {"key": "other", "value": "eA=="}]),
        );
        let out = on_intent(body, &SamlAttributes::default());
        assert_eq!(
            decoded(&out, "updateUser"),
            [
                "cn=ldap-group-a,ou=groups,dc=example,dc=org",
                "cn=ldap-group-b,ou=groups,dc=example,dc=org"
            ]
        );
        assert_eq!(
            out["updateUser"]["metadata"].as_array().unwrap().len(),
            2,
            "other metadata kept"
        );
    }

    #[test]
    fn a_person_in_no_group_gets_an_empty_list_not_their_old_one() {
        let body = ldap(
            json!(null),
            json!([{"key": "groups", "value": STANDARD.encode(br#"["stale"]"#)}]),
        );
        let out = on_intent(body, &SamlAttributes::default());
        assert!(decoded(&out, "updateUser").is_empty());
    }

    #[test]
    fn a_saml_sign_in_writes_groups_and_the_profile_zitadel_does_not_map() {
        let body = json!({"response": {
            "idpInformation": {"saml": {}, "rawInformation": {"attributes": {
                "groups": ["saml-group-a"], "uid": ["frank"], "givenName": ["Frank"],
                "sn": ["Saml"], "email": ["frank@saml.example.org"]}}},
            "createUser": {"human": {"idpLinks": [{"idpId": "1"}]}}
        }});
        let out = on_intent(body, &SamlAttributes::default());
        assert_eq!(decoded(&out, "createUser"), ["saml-group-a"]);
        let user = &out["createUser"];
        assert_eq!(user["username"], "frank");
        assert_eq!(user["human"]["profile"]["displayName"], "Frank Saml");
        assert_eq!(user["human"]["email"]["email"], "frank@saml.example.org");
        assert_eq!(user["human"]["idpLinks"][0]["userName"], "frank");
    }

    #[test]
    fn the_token_carries_metadata_groups_and_this_projects_roles_only() {
        let body = json!({
            "user_metadata": [{"key": "groups", "value": STANDARD.encode(br#"["cn=a,dc=x"]"#)}],
            "user_grants": [
                {"project_id": "P-DASH", "roles": ["traders"]},
                {"project_id": "P-OTHER", "roles": ["root"]}
            ]
        });
        assert_eq!(
            on_token(&body, "P-DASH"),
            json!({"append_claims": [{"key": "groups", "value": ["cn=a,dc=x", "traders"]}]})
        );
    }

    #[test]
    fn a_token_for_someone_with_nothing_carries_an_empty_list() {
        assert_eq!(
            on_token(&json!({}), "P-DASH"),
            json!({"append_claims": [{"key": "groups", "value": []}]})
        );
    }
}
