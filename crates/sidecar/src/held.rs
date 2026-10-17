//! Registration held to the version (W4.1, contract v25;
//! plans/a-plugin-carries-its-own-documentation, Q10, and amendment 2).
//!
//! From v25 a version's declaration carries what registration sends --
//! settings, an interface, tools and whether it reads external accounts --
//! and the conductor tells the sidecar the declaration the launched version
//! was uploaded with (`PluginConfiguration.declaration`). A plugin declaring
//! v25 or later registers that declaration, and settings, an interface,
//! tools and a reading of external accounts equal to it; otherwise the first
//! difference is named. The per-kind window settings the SDK adds,
//! `<kind>_window_days` and `<kind>_past_window` (W6.11), are left out of
//! the comparison: the past window's default depends on whether the instance
//! has an archive, known only where it runs.

use meridian_pb::v1::{InterfaceDeclaration, PluginDeclaration, RegisterRequest};

/// The first way a registration differs from the declaration its version
/// was uploaded with, as words naming it; none when they agree.
pub fn difference(request: &RegisterRequest, uploaded: &PluginDeclaration) -> Option<String> {
    let kinds: Vec<&str> = uploaded
        .storage
        .iter()
        .flat_map(|storage| &storage.record_kinds)
        .map(|kind| kind.name.as_str())
        .collect();
    let reserved = |name: &str| {
        kinds.iter().any(|kind| {
            name.strip_prefix(kind)
                .is_some_and(|rest| rest == "_window_days" || rest == "_past_window")
        })
    };
    let settings: Vec<_> = request
        .settings
        .iter()
        .filter(|setting| !reserved(&setting.name))
        .cloned()
        .collect();
    first_in("setting", &settings, &uploaded.settings, |s| s.name.clone())
        .or_else(|| interface(request.interface.as_ref(), uploaded.interface.as_ref()))
        .or_else(|| first_in("tool", &request.tools, &uploaded.tools, |t| t.name.clone()))
        .or_else(|| {
            (request.reads_external_accounts != uploaded.reads_external_accounts).then(|| {
                if request.reads_external_accounts {
                    "it registers reading external accounts, which its version did not declare"
                        .to_string()
                } else {
                    "it does not register reading external accounts, which its version declared"
                        .to_string()
                }
            })
        })
        .or_else(|| match &request.declaration {
            None => Some("it registers no declaration".to_string()),
            Some(registered) => declaration(registered, uploaded),
        })
}

/// The first element of a list registered unlike the version's, named.
fn first_in<T: PartialEq>(
    what: &str,
    registered: &[T],
    declared: &[T],
    name: impl Fn(&T) -> String,
) -> Option<String> {
    let at = (0..registered.len().max(declared.len()))
        .find(|&i| registered.get(i) != declared.get(i))?;
    Some(match (registered.get(at), declared.get(at)) {
        (Some(held), None) => format!(
            "it registers the {what} {}, which its version did not declare",
            name(held)
        ),
        (None, Some(declared)) => format!(
            "it does not register the {what} {}, which its version declared",
            name(declared)
        ),
        (Some(held), Some(declared)) if name(held) != name(declared) => format!(
            "it registers the {what} {} where its version declared the {what} {}",
            name(held),
            name(declared)
        ),
        (Some(held), Some(_)) => format!(
            "it registers the {what} {} unlike its version's declaration of it",
            name(held)
        ),
        (None, None) => unreachable!("an index below the longer length"),
    })
}

fn interface(
    registered: Option<&InterfaceDeclaration>,
    declared: Option<&InterfaceDeclaration>,
) -> Option<String> {
    match (registered, declared) {
        (None, None) => None,
        (Some(_), None) => {
            Some("it registers an interface, which its version did not declare".into())
        }
        (None, Some(_)) => Some("it registers no interface, and its version declared one".into()),
        (Some(held), Some(declared)) => first_in("page", &held.pages, &declared.pages, |page| {
            page.path.clone()
        })
        .or_else(|| {
            (held.loopback_port != declared.loopback_port).then(|| {
                format!(
                    "it registers its interface on port {} where its version declared {}",
                    held.loopback_port, declared.loopback_port
                )
            })
        })
        .or_else(|| {
            (held != declared)
                .then(|| "it registers its interface unlike its version's declaration".into())
        }),
    }
}

/// The registered declaration against the uploaded one, naming the first
/// field that differs.
fn declaration(registered: &PluginDeclaration, uploaded: &PluginDeclaration) -> Option<String> {
    let differs: [(&str, bool); 11] = [
        (
            "secret_settings",
            registered.secret_settings != uploaded.secret_settings,
        ),
        (
            "not_carried",
            registered.not_carried != uploaded.not_carried,
        ),
        ("storage", registered.storage != uploaded.storage),
        ("catalogue", registered.catalogue != uploaded.catalogue),
        ("algorithms", registered.algorithms != uploaded.algorithms),
        ("settings", registered.settings != uploaded.settings),
        ("interface", registered.interface != uploaded.interface),
        ("tools", registered.tools != uploaded.tools),
        (
            "reads_external_accounts",
            registered.reads_external_accounts != uploaded.reads_external_accounts,
        ),
        (
            "documentation",
            registered.documentation != uploaded.documentation,
        ),
        (
            "affirmation_deadline",
            registered.affirmation_deadline != uploaded.affirmation_deadline,
        ),
    ];
    let (field, _) = differs.iter().find(|(_, differs)| *differs)?;
    Some(format!("it registers a declaration whose {field} differs"))
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::{
        DocumentationSection, PageDeclaration, RawRecordKind, SettingDeclaration,
        StorageDeclaration, ToolDeclaration,
    };

    use super::*;

    fn setting(name: &str) -> SettingDeclaration {
        SettingDeclaration {
            name: name.into(),
            label: name.into(),
            ..Default::default()
        }
    }

    fn uploaded() -> PluginDeclaration {
        PluginDeclaration {
            secret_settings: vec!["api_key".into()],
            storage: Some(StorageDeclaration {
                retention_days: 30,
                record_kinds: vec![RawRecordKind {
                    name: "responses".into(),
                    label: "Raw responses".into(),
                    window_days: 30,
                    archivable: true,
                }],
            }),
            settings: vec![setting("api_key"), setting("region")],
            interface: Some(InterfaceDeclaration {
                loopback_port: 8000,
                title: "SnapTrade".into(),
                pages: vec![PageDeclaration {
                    path: "/admin/connections".into(),
                    title: "Connections".into(),
                    ..Default::default()
                }],
            }),
            tools: vec![ToolDeclaration {
                name: "read_connections".into(),
                ..Default::default()
            }],
            reads_external_accounts: true,
            documentation: vec![DocumentationSection {
                name: "overview".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                bytes: 12,
            }],
            ..Default::default()
        }
    }

    /// What the SDK registers from the same declaration: its settings with
    /// the per-kind window settings added (W6.11).
    fn registered() -> RegisterRequest {
        let declaration = uploaded();
        let mut settings = declaration.settings.clone();
        settings.push(setting("responses_window_days"));
        settings.push(setting("responses_past_window"));
        RegisterRequest {
            schema_version: "v25".into(),
            settings,
            interface: declaration.interface.clone(),
            tools: declaration.tools.clone(),
            reads_external_accounts: true,
            declaration: Some(declaration),
        }
    }

    #[test]
    fn a_registration_equal_to_its_version_is_held_without_the_window_settings() {
        assert_eq!(difference(&registered(), &uploaded()), None);
    }

    #[test]
    fn the_first_difference_is_named() {
        type Change = fn(&mut RegisterRequest);
        let cases: [(Change, &str); 9] = [
            (
                |r| r.settings.insert(1, setting("extra")),
                "registers the setting extra where its version declared the setting region",
            ),
            (
                |r| {
                    r.settings.remove(1);
                },
                "does not register the setting region, which its version declared",
            ),
            (
                |r| r.settings[0].label = "Key".into(),
                "registers the setting api_key unlike its version's declaration of it",
            ),
            (
                |r| {
                    r.interface.as_mut().unwrap().pages.push(PageDeclaration {
                        path: "/admin/more".into(),
                        ..Default::default()
                    })
                },
                "registers the page /admin/more, which its version did not declare",
            ),
            (
                |r| r.interface.as_mut().unwrap().loopback_port = 9000,
                "on port 9000 where its version declared 8000",
            ),
            (
                |r| r.interface = None,
                "registers no interface, and its version declared one",
            ),
            (
                |r| r.tools.clear(),
                "does not register the tool read_connections, which its version declared",
            ),
            (
                |r| r.reads_external_accounts = false,
                "does not register reading external accounts",
            ),
            (
                |r| r.declaration.as_mut().unwrap().documentation.clear(),
                "it registers a declaration whose documentation differs",
            ),
        ];
        for (change, says) in cases {
            let mut request = registered();
            change(&mut request);
            let named = difference(&request, &uploaded()).expect(says);
            assert!(named.contains(says), "{says}: {named}");
        }
        let mut none = registered();
        none.declaration = None;
        assert_eq!(
            difference(&none, &uploaded()).as_deref(),
            Some("it registers no declaration")
        );
    }

    /// A setting named like a window setting for a kind the version does
    /// not store is compared like any other.
    #[test]
    fn only_a_stored_kinds_window_settings_are_left_out() {
        let mut request = registered();
        request.settings.push(setting("quotes_window_days"));
        let named = difference(&request, &uploaded()).unwrap();
        assert!(named.contains("the setting quotes_window_days"), "{named}");
    }
}
