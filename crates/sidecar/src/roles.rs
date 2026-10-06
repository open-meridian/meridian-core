//! The roles a page, tool or setting serves (W4.1, contract v15;
//! decisions/033; the spec's requirements 11, 12 and 21).
//!
//! A declaration picks among the roles a deployment admin approved at launch,
//! for people: it decides what is shown, never what is admitted, which the
//! sidecar decides by the generated grants and the person's grant on the role
//! holding each act (W4.9). So a declaration naming a role the plugin was not
//! launched with is refused; on a plugin holding exactly one role a
//! declaration naming none serves it; on a plugin holding none, declarations
//! name none. On a plugin holding several, one naming none is refused from a
//! plugin built at v15 or later, and from one built earlier serves every role
//! the plugin holds -- safe, since every act is still the sidecar's to admit.
//!
//! What is admitted is written back with its roles filled, so the report
//! carries each declaration's roles as served (W4.8) and the dashboard draws
//! the tab row, lists the tools and gates the Settings form by role.

/// The first contract whose plugins name the roles each declaration serves.
pub const ROLES_FROM: u32 = 15;

/// The roles a declaration serves, filled where it names none and may; or
/// the end of a sentence saying why it is refused ("serves oms, which ...").
pub fn served(named: &[String], launched: &[String], built_at: u32) -> Result<Vec<String>, String> {
    let holds = if launched.is_empty() {
        "it holds no role".to_string()
    } else {
        format!("it holds {}", launched.join(" and "))
    };
    if let Some(stranger) = named.iter().find(|role| !launched.contains(role)) {
        return Err(format!(
            "serves {stranger}, which this plugin was not launched with: {holds}"
        ));
    }
    if !named.is_empty() {
        let mut roles: Vec<String> = Vec::new();
        for role in named {
            if !roles.contains(role) {
                roles.push(role.clone());
            }
        }
        return Ok(roles);
    }
    if launched.len() > 1 && built_at >= ROLES_FROM {
        return Err(format!(
            "names no role, and {holds}: on a plugin holding several roles each page, tool and \
             setting names the roles it serves"
        ));
    }
    Ok(launched.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn one_role_fills_a_declaration_naming_none() {
        assert_eq!(
            served(&[], &roles(&["custody"]), 15),
            Ok(roles(&["custody"]))
        );
        assert_eq!(
            served(&[], &[], 15),
            Ok(vec![]),
            "a role-less plugin names none"
        );
    }

    #[test]
    fn a_role_the_plugin_was_not_launched_with_is_refused_naming_its_roles() {
        let refused = served(&roles(&["oms"]), &roles(&["custody", "operations"]), 15).unwrap_err();
        assert!(
            refused.contains("oms") && refused.contains("custody and operations"),
            "{refused}"
        );
        let on_none = served(&roles(&["custody"]), &[], 15).unwrap_err();
        assert!(on_none.contains("no role"), "{on_none}");
    }

    #[test]
    fn several_roles_need_naming_from_v15_and_serve_all_from_before() {
        let both = roles(&["custody", "operations"]);
        assert!(served(&[], &both, 15)
            .unwrap_err()
            .contains("names no role"));
        assert_eq!(
            served(&[], &both, 14),
            Ok(both.clone()),
            "built at v14: every role"
        );
        assert_eq!(
            served(&roles(&["operations", "operations"]), &both, 15),
            Ok(roles(&["operations"]))
        );
    }
}
