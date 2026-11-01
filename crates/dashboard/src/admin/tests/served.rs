//! A plugin's Settings pages, served for a real browser to drive
//! (`make e2e-settings-page`; e2e/settings-page/browser.py).
//!
//! The dashboard's own router and pages, with the kit the image carries, for
//! an instance shaped like SnapTrade's declarations: a key type deciding two
//! fields, two secrets every key needs and one more under a commercial key,
//! four numbers with defaults, two developer's switches, and two table
//! settings of at most 200 rows, "Plan-code links" and "Cash links". Behind
//! it a stand-in for the conductor that keeps the settings record as the
//! real one does -- each table's cells checked by the same code
//! (`setting_table::checked`), its rows stamped with who and when
//! (`setting_table::stamped`), a refusal said as the conductor says it -- and
//! answers the access records and the instrument list the pages read. What
//! the browser proves is therefore the page and the dashboard's handling of
//! what it posts, end to end, with only the store's persistence left out.
//!
//! And, from contract v16, the instance's raw records on its Summary --
//! two kinds, what storage and the archive hold of each, sixty moves paged
//! -- with Allow archive and Withdraw, and the holds on the deployment's
//! Settings, behind a stand-in conductor answering ReadMoves, AllowArchive,
//! WithdrawArchive and SetHold as the real one records them.
//!
//! And, from contract v18, the Data sources page: twenty-four datasets of
//! eight instances, with their licences, entitlements and counts, and the
//! priorities, behind a stand-in lake and conductor answering ListDatasets,
//! ListSourcePriorities, SetDatasetLicence, SetDatasetEntitlement and
//! SetSourcePriority, the last with the stale guard.
//!
//! Ignored in the ordinary run: it serves until it is stopped. The person
//! signed in is a deployment admin, administering every plugin, and the
//! session it starts is printed for the browser to present.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::setting_table;
use meridian_domain::v1::{
    AccessRecords, AccessRecordsRequest, ExternalAccountLink, Identifier, InstrumentRecord,
    InstrumentToComplete, ListInstrumentsToCompleteReply, ListInstrumentsToCompleteRequest,
    Permission, PluginSettingValue, PluginSettingsRecord, SetPluginSettingsRequest, SignInRecord,
    UserGroup,
};
use meridian_pb::v1::{
    AccessLevel, SettingChoice, SettingColumn, SettingColumnType, SettingCondition,
    SettingDeclaration, SettingType,
};
use prost::Message;

use crate::clock::Clock;
use crate::records::{RecordsCache, ACCESS_RECORDS};
use crate::session::Sessions;
use crate::web::{router, App};

const INSTANCE: &str = "snaptrade-1";
const ADMIN: &str = "https://directory.example.org|admin";
/// Contract v28 (W10.1): a person reading prices through plugin-03, whom
/// the professional test asks.
const READER: &str = "https://directory.example.org|ines";

fn setting(name: &str, kind: SettingType, label: &str, description: &str) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: kind as i32,
        label: label.into(),
        description: description.into(),
        ..Default::default()
    }
}

fn column(name: &str, kind: SettingColumnType, label: &str, required: bool) -> SettingColumn {
    SettingColumn {
        name: name.into(),
        r#type: kind as i32,
        label: label.into(),
        required,
        ..Default::default()
    }
}

/// SnapTrade's declarations, as its 0.11 declares them, its tables labelled
/// as 0.11.1 labels them.
fn declared() -> Vec<SettingDeclaration> {
    let commercial = || {
        Some(SettingCondition {
            setting: "key_type".into(),
            one_of: vec!["commercial".into()],
        })
    };
    let number =
        |name: &str, label: &str, default: &str, unit: &str, about: &str| SettingDeclaration {
            default_value: default.into(),
            unit: unit.into(),
            ..setting(name, SettingType::Integer, label, about)
        };
    vec![
        SettingDeclaration {
            required: true,
            default_value: "personal".into(),
            choices: vec![
                SettingChoice {
                    value: "personal".into(),
                    label: "Personal key".into(),
                    description: "Belongs to one SnapTrade user: you.".into(),
                },
                SettingChoice {
                    value: "commercial".into(),
                    label: "Commercial key".into(),
                    description: "Registers SnapTrade users of its own, each with a secret.".into(),
                },
            ],
            ..setting(
                "key_type",
                SettingType::Choice,
                "Key type",
                "Which kind of key SnapTrade issued. It decides the fields that follow.",
            )
        },
        SettingDeclaration {
            required: true,
            secret: true,
            ..setting(
                "client_id",
                SettingType::String,
                "Client ID",
                "From the SnapTrade dashboard's API keys page.",
            )
        },
        SettingDeclaration {
            required: true,
            secret: true,
            ..setting(
                "consumer_key",
                SettingType::String,
                "Consumer key",
                "Shown once, when SnapTrade issues the key.",
            )
        },
        SettingDeclaration {
            required: true,
            applies_when: commercial(),
            ..setting(
                "user_id",
                SettingType::String,
                "User ID",
                "The SnapTrade user whose brokerage connections this instance reads.",
            )
        },
        SettingDeclaration {
            required: true,
            secret: true,
            applies_when: commercial(),
            ..setting(
                "user_secret",
                SettingType::String,
                "User secret",
                "That user's secret, returned when the user was registered.",
            )
        },
        // An optional secret, set: the one a person may Remove.
        SettingDeclaration {
            secret: true,
            ..setting(
                "webhook_token",
                SettingType::String,
                "Webhook token",
                "Signs the notices SnapTrade sends when a connection changes.",
            )
        },
        number(
            "poll_seconds",
            "Read every",
            "300",
            "seconds",
            "How often to read SnapTrade. At least 60.",
        ),
        number(
            "stale_after_hours",
            "Stale after",
            "24",
            "hours",
            "How old SnapTrade's last sync of an account may be before it is reported stale.",
        ),
        number(
            "raw_retention_days",
            "Keep raw responses for",
            "30",
            "days",
            "How long SnapTrade's responses to each read are kept as received, for the Raw \
             responses tab. At least 1; older ones are removed.",
        ),
        number(
            "activity_retention_days",
            // A long label beside its "default" tag, both kept whole on a
            // phone, the label giving way (the dev deployment, 2026-10-17).
            "Keep each activity record SnapTrade reported for",
            "2555",
            "days",
            "How long the record of each activity SnapTrade reported is kept, from when it \
             was received. Never shorter than the history SnapTrade reported; at most 36500.",
        ),
        SettingDeclaration {
            columns: vec![
                column(
                    "account",
                    SettingColumnType::ExternalAccount,
                    "Account",
                    true,
                ),
                SettingColumn {
                    description: "As SnapTrade names it in the account's activity.".into(),
                    ..column("code", SettingColumnType::Text, "Plan code", true)
                },
                column(
                    "instrument",
                    SettingColumnType::Instrument,
                    "Instrument",
                    true,
                ),
            ],
            most_rows: 200,
            ..setting(
                "plan_code_links",
                SettingType::Table,
                "Plan-code links",
                "A plan's own fund codes, each linked on one account to the instrument it is \
                 (Fidelity's OQKR to VIGIX): its activity is then that instrument, naming who \
                 linked it.",
            )
        },
        SettingDeclaration {
            columns: vec![
                column(
                    "account",
                    SettingColumnType::ExternalAccount,
                    "Account",
                    false,
                ),
                SettingColumn {
                    description: "As SnapTrade names the position, such as FDIC99532.".into(),
                    ..column("symbol", SettingColumnType::Text, "Symbol", true)
                },
                SettingColumn {
                    description: "The ISO 4217 code of the cash it is, such as USD.".into(),
                    ..column("currency", SettingColumnType::Text, "Currency", true)
                },
            ],
            most_rows: 200,
            ..setting(
                "counted_as_cash",
                SettingType::Table,
                "Cash links",
                "Positions a custodian holds as cash that SnapTrade does not mark as a cash \
                 equivalent, such as a bank deposit as an IRA's core position: each is sent as \
                 cash in its currency, naming who listed it. A fund stays a fund.",
            )
        },
        SettingDeclaration {
            default_value: "false".into(),
            developer: true,
            ..setting(
                "synthetic",
                SettingType::Boolean,
                "Synthetic mode",
                "Serve built-in synthetic SnapTrade responses instead of calling SnapTrade.",
            )
        },
        SettingDeclaration {
            developer: true,
            ..setting(
                "personal_key",
                SettingType::Boolean,
                "Personal key (the old way)",
                "Replaced by Key type, and read only while Key type is unset.",
            )
        },
    ]
}

/// The external accounts the plugin links, which its tables' account
/// columns offer.
pub const ACCOUNTS: [&str; 4] = ["FID-401K-1", "FID-IRA-2", "SCHW-BRK-3", "VG-ROTH-4"];

/// The plugin holding custody and operations whose Access tab and rows the
/// browser checks (contract v15).
pub const TWO_ROLES: &str = "ops-1";

/// Access per role (contract v15): SnapTrade holding custody, a plugin
/// holding custody and operations, a plugin holding none, and twenty more
/// holding one role each, so the Access editor's rows page; access groups
/// per role, one keeping an entry on a role its plugin no longer holds;
/// thirty desks granted on the two-role plugin's roles, so its Access tab
/// pages too.
fn per_role(records: &mut AccessRecords) {
    use meridian_domain::v1::{AccessEntry, AccessGroup, AccountGroup, KnownPluginRoles};
    let known = |id: &str, roles: &[&str]| KnownPluginRoles {
        plugin_instance_id: id.into(),
        roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    records.known_plugins = vec![
        known(INSTANCE, &["custody"]),
        known(TWO_ROLES, &["custody", "operations"]),
        known("reference-1", &[]),
        // Contract v22 (W11.21): a connectivity plugin and its ems.
        known("fix-1", &["connectivity"]),
        known("ems-1", &["ems"]),
    ];
    for n in 0..20 {
        records.known_plugins.push(known(
            &format!("plugin-{n:02}"),
            &[["custody", "operations", "data", "reporting"][n % 4]],
        ));
    }
    let entry = |plugin: &str, role: &str, level: AccessLevel| AccessEntry {
        plugin_instance_id: plugin.into(),
        level: level as i32,
        role: role.into(),
    };
    records.access_groups = vec![
        AccessGroup {
            access_group_id: "AX-RECON".into(),
            name: "Reconciliation".into(),
            entries: vec![
                entry(TWO_ROLES, "operations", AccessLevel::Write),
                entry(TWO_ROLES, "custody", AccessLevel::Read),
            ],
            built_in: false,
        },
        AccessGroup {
            access_group_id: "AX-CUSTODY-ADMINS".into(),
            name: "Custody admins".into(),
            entries: vec![
                entry(TWO_ROLES, "custody", AccessLevel::Admin),
                entry(INSTANCE, "custody", AccessLevel::Admin),
                // Kept as written: ops-1 was relaunched without connectivity.
                entry(TWO_ROLES, "connectivity", AccessLevel::Admin),
            ],
            built_in: false,
        },
    ];
    // Its settings record, as the conductor keeps one for every known plugin.
    records.plugin_settings.push(PluginSettingsRecord {
        plugin_instance_id: TWO_ROLES.into(),
        ..Default::default()
    });
    // And the connectivity plugin's and its ems's (contract v22).
    for instance in ["fix-1", "ems-1"] {
        records.plugin_settings.push(PluginSettingsRecord {
            plugin_instance_id: instance.into(),
            ..Default::default()
        });
    }
    records.account_groups.push(AccountGroup {
        account_group_id: "AG-DESK".into(),
        name: "The desk's accounts".into(),
        account_ids: vec![],
        built_in: false,
    });
    for n in 0..30 {
        let group = format!("UG-DESK-{n:02}");
        records.user_groups.push(UserGroup {
            user_group_id: group.clone(),
            name: format!("Desk {n:02}"),
            directory_groups: vec![format!("desk-{n:02}")],
            logins: vec![],
        });
        records.permissions.push(Permission {
            permission_id: format!("P-DESK-{n:02}"),
            user_group_id: group.clone(),
            account_group_id: if n % 3 == 0 {
                String::new()
            } else {
                "AG-DESK".into()
            },
            access_group_id: if n % 3 == 0 {
                "AX-CUSTODY-ADMINS"
            } else {
                "AX-RECON"
            }
            .into(),
        });
    }
}

/// The firm's accounts, which the Accounts tab lists one line each with
/// what the book holds of them, the rest behind Edit.
fn accounts() -> Vec<meridian_domain::v1::AccountRecord> {
    let named = [
        (
            "Credit Suisse 401(k)",
            "Fidelity",
            "401(k)",
            "Vincent",
            "Rolled over in 2026, pending the plan's letter.",
        ),
        (
            "Traditional IRA",
            "Fidelity",
            "Traditional IRA",
            "Vincent",
            "",
        ),
        (
            "Brokerage",
            "Charles Schwab",
            "Individual brokerage",
            "Household",
            "",
        ),
        (
            "Roth IRA",
            "Vanguard",
            "Roth IRA",
            "Vincent",
            "Contributions only; no conversions.",
        ),
        (
            "Joint taxable",
            "Fidelity",
            "Joint brokerage",
            "Household",
            "",
        ),
        ("Kalshi", "Kalshi", "Event contracts", "Vincent", ""),
    ];
    named
        .iter()
        .enumerate()
        .map(
            |(n, (name, custodian, kind, owner, note))| meridian_domain::v1::AccountRecord {
                account_id: ACCOUNTS
                    .get(n)
                    .map(|account| format!("ACC-{account}"))
                    .unwrap_or_else(|| format!("ACC-{n}")),
                name: (*name).into(),
                state: meridian_domain::v1::AccountState::Open as i32,
                created_at_ns: 1_790_380_800_000_000_000,
                custodian: (*custodian).into(),
                account_type: (*kind).into(),
                owner: (*owner).into(),
                note: (*note).into(),
            },
        )
        .collect()
}

fn records() -> AccessRecords {
    let mut records = settings_records();
    per_role(&mut records);
    // Contract v28 (W10.1, W10.13): Ines reads plugin-03's reporting role,
    // which kraken-1:daily, split professional and non-professional, is
    // entitled to.
    records.user_groups.push(UserGroup {
        user_group_id: "UG-PRICES".into(),
        name: "Prices".into(),
        directory_groups: vec![],
        logins: vec![READER.into()],
    });
    records
        .access_groups
        .push(meridian_domain::v1::AccessGroup {
            access_group_id: "AX-PRICES".into(),
            name: "Prices".into(),
            entries: vec![meridian_domain::v1::AccessEntry {
                plugin_instance_id: "plugin-03".into(),
                level: AccessLevel::Read as i32,
                role: "reporting".into(),
            }],
            built_in: false,
        });
    records.permissions.push(Permission {
        permission_id: "P-PRICES".into(),
        user_group_id: "UG-PRICES".into(),
        account_group_id: "AG-DESK".into(),
        access_group_id: "AX-PRICES".into(),
    });
    records.people.push(SignInRecord {
        subject: READER.into(),
        display_name: "Ines Reader".into(),
        directory_groups: vec![],
        signed_in_at_ns: 1_791_417_600_000_000_000,
    });
    records.accounts = accounts();
    // Contract v22 (W11.21, W11.22): fix-1 available and linked to ems-1,
    // which mapped every account on it.
    records.connectivity_links = vec![meridian_pb::v1::ConnectivityLink {
        connectivity_instance_id: "fix-1".into(),
        available: true,
        ems_instance_id: "ems-1".into(),
        users: vec![meridian_pb::v1::ConnectivityUser {
            user: Some(meridian_pb::v1::connectivity_user::User::Subject(
                ADMIN.into(),
            )),
        }],
    }];
    records.venue_account_maps = vec![meridian_pb::v1::VenueAccountMap {
        ems_instance_id: "ems-1".into(),
        // The first four accounts, two codes each: the last two mapped by
        // none, so the Close of one only granted, and of one attached to
        // nothing, are drawn too (approved 2026-10-14).
        mappings: records
            .accounts
            .iter()
            .take(4)
            .flat_map(|account| [account.clone(), account.clone()])
            .enumerate()
            .map(|(n, account)| meridian_pb::v1::VenueAccountMapping {
                connectivity_instance_id: "fix-1".into(),
                venue_account_code: format!("ACCT-{:04}", n + 1),
                account_id: account.account_id.clone(),
            })
            .collect(),
    }];
    // "Joint taxable", granted to SnapTrade and attached to nothing else.
    if let Some(joint) = records.accounts.get(4).map(|a| a.account_id.clone()) {
        match records
            .linkable_accounts
            .iter_mut()
            .find(|granted| granted.plugin_instance_id == INSTANCE)
        {
            Some(granted) => granted.account_ids.push(joint),
            None => records
                .linkable_accounts
                .push(meridian_domain::v1::LinkableAccounts {
                    plugin_instance_id: INSTANCE.into(),
                    account_ids: vec![joint],
                    ..Default::default()
                }),
        }
    }
    // The holds a deployment admin set (contract v16, W6.25).
    records.holds = vec![
        meridian_domain::v1::Hold {
            role: String::new(),
            days: 400,
            write_once: false,
            updated_by: ADMIN.into(),
            updated_at_ns: 1_791_331_200_000_000_000,
            ..Default::default()
        },
        meridian_domain::v1::Hold {
            role: "custody".into(),
            days: 2190,
            write_once: false,
            updated_by: ADMIN.into(),
            updated_at_ns: 1_791_417_600_000_000_000,
            ..Default::default()
        },
    ];
    records
}

/// SnapTrade's report as a v16 sidecar sends it: its figures, its
/// declaration with two kinds of raw record, and what its storage holds of
/// each (contract v16, W4.5, W4.8).
fn report(now: i64) -> meridian_domain::v1::PluginReport {
    use meridian_pb::v1::{
        plugin_figure, NotCarried, NotCarriedReason, PluginDeclaration, PluginFigure,
        RawRecordKind, StorageDeclaration, StoredSpan,
    };
    let figure = |label: &str, value: plugin_figure::Value| PluginFigure {
        label: label.into(),
        value: Some(value),
        ..Default::default()
    };
    meridian_domain::v1::PluginReport {
        plugin_instance_id: INSTANCE.into(),
        roles: vec!["custody".into()],
        registered: true,
        healthy: true,
        last_heartbeat_at_ns: now,
        contract_version: "v16".into(),
        reported_at_ns: now,
        declared_settings: declared(),
        figures: vec![
            figure("Connections", plugin_figure::Value::Count(3)),
            figure("Accounts reached", plugin_figure::Value::Count(7)),
            figure(
                "Last read",
                plugin_figure::Value::AtNs(now - 300_000_000_000),
            ),
            // Five figures, as alpaca 0.8.0's Summary has at phone width
            // (the build plan's O8, found 2026-10-14).
            figure(
                "Buying power",
                plugin_figure::Value::Text("USD 1,234,567,890.12".into()),
            ),
            figure("Orders working", plugin_figure::Value::Count(12_345)),
        ],
        declaration: Some(PluginDeclaration {
            algorithms: Default::default(),
            secret_settings: vec!["client_id".into(), "consumer_key".into()],
            not_carried: vec![NotCarried {
                role: "custody".into(),
                scheme: "snaptrade:position".into(),
                name: "open_pnl".into(),
                reason: NotCarriedReason::NoContractMeaning as i32,
            }],
            storage: Some(StorageDeclaration {
                retention_days: 2555,
                record_kinds: vec![
                    RawRecordKind {
                        name: "activity".into(),
                        label: "Reported activity".into(),
                        window_days: 2555,
                        archivable: true,
                    },
                    RawRecordKind {
                        name: "responses".into(),
                        label: "Raw responses".into(),
                        window_days: 30,
                        archivable: true,
                    },
                ],
            }),
            catalogue: None,
            ..Default::default()
        }),
        stored: vec![
            StoredSpan {
                record_kind: "activity".into(),
                record_count: 48_210,
                first_received_ns: 1_554_076_800_000_000_000,
                last_received_ns: now - 600_000_000_000,
                // What its archived months use of the archive.
                bytes: 3_435_973_837,
            },
            StoredSpan {
                record_kind: "responses".into(),
                record_count: 1_260,
                first_received_ns: now - 30 * 86_400_000_000_000,
                last_received_ns: now - 600_000_000_000,
                bytes: 0,
            },
        ],
        ..Default::default()
    }
}

/// Sixty months of an account's activity archived by its window, the
/// latest restored for Ada: what the moves' pager pages (W4.13, W6.9).
fn moves() -> Vec<meridian_domain::v1::MoveRecord> {
    use meridian_pb::v1::{MoveOutcome, RecordMoveRequest};
    let month = 30 * 86_400_000_000_000_i64;
    let mut moves: Vec<_> = (0..60_i64)
        .map(|n| meridian_domain::v1::MoveRecord {
            r#move: Some(RecordMoveRequest {
                record_kind: "activity".into(),
                unit: format!("activity/FID-401K-1/{:04}-{:02}", 2014 + n / 12, n % 12 + 1),
                record_count: 180 + n as u64,
                first_received_ns: 1_388_534_400_000_000_000 + n * month,
                last_received_ns: 1_388_534_400_000_000_000 + (n + 1) * month - 1,
                outcome: MoveOutcome::Archived as i32,
                rule: "activity_window_days 2555".into(),
            }),
            person: String::new(),
            at_ns: 1_791_504_000_000_000_000 + n * 1_000_000_000,
            ..Default::default()
        })
        .collect();
    moves.reverse();
    let mut restored = moves[0].clone();
    if let Some(moved) = restored.r#move.as_mut() {
        moved.outcome = MoveOutcome::Restored as i32;
        moved.rule.clear();
    }
    restored.person = ADMIN.into();
    restored.at_ns += 60_000_000_000;
    moves.insert(0, restored);
    moves
}

fn settings_records() -> AccessRecords {
    AccessRecords {
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec![ADMIN.into()],
        }],
        permissions: vec![
            Permission {
                permission_id: "P-1".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
            },
            Permission {
                permission_id: "P-0".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
            },
        ],
        people: vec![SignInRecord {
            subject: ADMIN.into(),
            display_name: "Ada Admin".into(),
            ..Default::default()
        }],
        links: ACCOUNTS
            .iter()
            .map(|account| ExternalAccountLink {
                plugin_instance_id: INSTANCE.into(),
                external_account_id: (*account).into(),
                account_id: format!("ACC-{account}"),
            })
            .collect(),
        plugin_settings: vec![PluginSettingsRecord {
            plugin_instance_id: INSTANCE.into(),
            declared_settings: declared(),
            values: vec![
                PluginSettingValue {
                    name: "key_type".into(),
                    value: "personal".into(),
                },
                // One link held: the page the product owner saw.
                PluginSettingValue {
                    name: "plan_code_links".into(),
                    value: setting_table::written(&[setting_table::Row {
                        cells: [
                            ("account".to_string(), ACCOUNTS[0].to_string()),
                            ("code".to_string(), "OQKR".to_string()),
                            ("instrument".to_string(), instrument_id(1)),
                        ]
                        .into(),
                        changed_by: ADMIN.into(),
                        changed_at: setting_table::rfc3339(1_790_380_800_000_000_000),
                    }]),
                },
            ],
            secrets_set: vec!["client_id".into(), "webhook_token".into()],
            // Who set each secret, and when, as the conductor's records say.
            changes: ["client_id", "webhook_token"]
                .iter()
                .map(|name| meridian_domain::v1::SettingLastChange {
                    name: (*name).into(),
                    changed_by: ADMIN.into(),
                    changed_at_ns: 1_791_331_200_000_000_000,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn instrument_id(n: usize) -> String {
    format!("INS-{n:04}")
}

/// The first sixty records lack their class and currency: what the
/// Instruments page lists to complete (W3.11), paged and searched.
fn incomplete(id: &str) -> bool {
    id[4..].parse::<usize>().is_ok_and(|n| n <= 60)
}

/// The record the picker brings SPY in as: provisional, the platform not
/// having answered yet (ruling 786).
pub const BROUGHT_IN: &str = "LCL-01JSPY00000000000000000001";

/// A record as the stand-in instrument store answers it: a fund with its
/// symbol, complete from the sixty-first; INS-0007 a binary event contract
/// whose old ticker the platform ended (contract v19), offered as an end.
fn instrument_listed(id: String) -> InstrumentToComplete {
    use meridian_domain::v1::{instrument_value, InstrumentField, InstrumentValue, OfferedValue};
    let lacking = incomplete(&id);
    let n = &id[4..];
    let mut record = InstrumentRecord {
        identifiers: vec![Identifier {
            scheme: "symbol".into(),
            value: format!("F{n}"),
            ..Default::default()
        }],
        description: format!("Fund {n}"),
        instrument_id: id.clone(),
        version: 2,
        ..Default::default()
    };
    if !lacking {
        record.asset_class = meridian_domain::v1::AssetClass::Fund as i32;
        record.currency = "USD".into();
    }
    if id == "INS-0007" {
        record.description = "Fed cuts rates in November, YES".into();
        record.identifiers.push(Identifier {
            scheme: "ticker".into(),
            value: "KXFED-OLD".into(),
            valid_from_ns: 1_790_000_000_000_000_000,
            valid_until_ns: 1_791_000_000_000_000_000,
            ..Default::default()
        });
        record.offers.push(OfferedValue {
            value: Some(InstrumentValue {
                value: Some(instrument_value::Value::Identifier(Identifier {
                    scheme: "symbol".into(),
                    value: format!("F{n}"),
                    valid_until_ns: 1_791_500_000_000_000_000,
                    ..Default::default()
                })),
                source: "the platform, record INS-9 version 3".into(),
            }),
            ..Default::default()
        });
        record.offers.push(OfferedValue {
            value: Some(InstrumentValue {
                value: Some(instrument_value::Value::AssetClass(
                    meridian_domain::v1::AssetClass::EventContract as i32,
                )),
                source: "stated by kalshi-1".into(),
            }),
            ..Default::default()
        });
    }
    InstrumentToComplete {
        instrument: Some(record),
        lacks: if lacking {
            vec![
                InstrumentField::AssetClass as i32,
                InstrumentField::Currency as i32,
            ]
        } else {
            Vec::new()
        },
        complete: !lacking,
        complete_for_book: !lacking,
        ..Default::default()
    }
}

/// Twelve conflicts, every other one a plugin's resolve (contract v19).
fn conflicts() -> Vec<meridian_domain::v1::InstrumentConflict> {
    (1..=12)
        .map(|n| meridian_domain::v1::InstrumentConflict {
            identifiers: vec![Identifier {
                scheme: "figi".into(),
                value: format!("BBG00000{n:04}"),
                ..Default::default()
            }],
            instrument_ids: vec![instrument_id(100 + n), instrument_id(200 + n)],
            reported_by: if n % 2 == 0 {
                "coinbase-1".into()
            } else {
                String::new()
            },
            reason: if n % 2 == 0 {
                meridian_domain::v1::MissReason::Ambiguous as i32
            } else {
                0
            },
            first_seen_ns: n as i64,
            last_seen_ns: n as i64,
        })
        .collect()
}

/// The stand-in conductor's change to the settings record: as the real one
/// makes it, each table checked and its rows stamped, or its refusal.
fn set(
    held: &mut AccessRecords,
    asked: SetPluginSettingsRequest,
    by: &str,
    now: i64,
) -> Result<(), String> {
    let record = held
        .plugin_settings
        .iter_mut()
        .find(|record| record.plugin_instance_id == asked.plugin_instance_id)
        .ok_or("no such plugin")?;
    for value in &asked.values {
        let declaration = record
            .declared_settings
            .iter()
            .find(|declaration| declaration.name == value.name)
            .ok_or_else(|| format!("{} is not declared", value.name))?
            .clone();
        if declaration.secret {
            if !record.secrets_set.contains(&value.name) {
                record.secrets_set.push(value.name.clone());
            }
            continue;
        }
        let written = if setting_table::is_table(&declaration) {
            let given: Vec<setting_table::Cells> = setting_table::parse(&value.value)?
                .into_iter()
                .map(|row| row.cells)
                .collect();
            let rows = setting_table::checked(&declaration, &given).map_err(|problems| {
                problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            })?;
            let standing = record
                .values
                .iter()
                .find(|held| held.name == value.name)
                .and_then(|held| setting_table::parse(&held.value).ok())
                .unwrap_or_default();
            setting_table::written(&setting_table::stamped(&rows, &standing, by, now))
        } else {
            value.value.clone()
        };
        record.values.retain(|held| held.name != value.name);
        record.values.push(PluginSettingValue {
            name: value.name.clone(),
            value: written,
        });
    }
    for name in &asked.cleared {
        record.values.retain(|held| &held.name != name);
        record.secrets_set.retain(|held| held != name);
    }
    for name in asked
        .values
        .iter()
        .map(|value| &value.name)
        .chain(&asked.cleared)
    {
        record.changes.retain(|change| &change.name != name);
        record.changes.push(meridian_domain::v1::SettingLastChange {
            name: name.clone(),
            changed_by: by.into(),
            changed_at_ns: now,
            ..Default::default()
        });
    }
    record.updated_by = by.into();
    record.updated_at_ns = now;
    Ok(())
}

/// The datasets the Data sources page lists (contract v18): eight `data`
/// instances of three datasets each, long names among them, the first two
/// entitled to three readers each, one licence set and one person's terms.
fn lake_listing() -> meridian_domain::v1::ListDatasetsReply {
    use meridian_domain::v1::{DatasetEntitlement, DatasetRef, ListDatasetsReply};
    use meridian_pb::v1::{DatasetDeclaration, DatasetLicence};
    let vendors = [
        ("coinbase-1", "Coinbase", ""),
        ("kraken-1", "Kraken", ""),
        ("alpaca-1", "IEX", "Alpaca"),
        ("tradier-1", "Consolidated tape", "Tradier"),
        (
            "fed-h10-1",
            "Board of Governors of the Federal Reserve System",
            "",
        ),
        ("databento-1", "Nasdaq, NYSE and Cboe summary", "Databento"),
        ("massive-1", "Massive", ""),
        ("tiingo-1", "Tiingo", ""),
    ];
    let mut reply = ListDatasetsReply::default();
    for (n, (instance, vendor, aggregator)) in vendors.iter().enumerate() {
        for (k, key) in ["daily", "live", "fx_and_rates_by_release"]
            .iter()
            .enumerate()
        {
            let dataset = format!("{instance}:{key}");
            reply.datasets.push(DatasetRef {
                dataset: dataset.clone(),
                instance: instance.to_string(),
                vendor: vendor.to_string(),
                aggregator: aggregator.to_string(),
                declaration: Some(DatasetDeclaration {
                    key: key.to_string(),
                    vendor: vendor.to_string(),
                    aggregator: aggregator.to_string(),
                    // Contract v29: one serving the factor kind too, its
                    // Serves cell the widest.
                    data_types: if n == 6 {
                        vec![
                            "meridian.v1.Price".into(),
                            "meridian.v1.Bar".into(),
                            "meridian.v1.Factor".into(),
                        ]
                    } else {
                        vec!["meridian.v1.Price".into(), "meridian.v1.Bar".into()]
                    },
                    modes: vec![1, 2],
                    cadence: 86_400,
                    licence_default: Some(DatasetLicence {
                        kept: k != 1,
                        retention_days: if k == 2 { 3650 } else { 0 },
                        personal_use: n == 2,
                        // Contract 28, amended 2026-10-31 (ruling 896): the
                        // source behind the vendor, one long enough to be cut.
                        underlying_source: match n {
                            5 => "Nasdaq, NYSE and Cboe summary feeds, consolidated by the aggregator"
                                .into(),
                            6 => "US equity SIPs (CTA, UTP)".into(),
                            _ => String::new(),
                        },
                        ..Default::default()
                    }),
                    day_time_zone: "America/New_York".into(),
                    // Ruling 906: each class, and not stated.
                    price_source_class: (n % 5) as i32,
                    ..Default::default()
                }),
                unconverted_count: (n * 3 + k) as u64 * 7,
                miss_count: (n + k) as u64,
                // Contract v23: closed days, and some outside coverage, so
                // the browser sees both in the one cell.
                closed_day_count: (n * 2 + k) as u64 * 11,
                outside_coverage_count: if k == 1 { (n + 1) as u64 * 3 } else { 0 },
                // Contract v29 (619 (2)): the rows kept by effective state,
                // a large dataset among them.
                valid_count: (n * 3 + k) as u64 * 104_729,
                suspicious_count: (n + k) as u64 * 13,
                invalid_count: if k == 1 { (n + 1) as u64 * 2 } else { 0 },
                ..Default::default()
            });
            if n < 2 || (n == 2 && k == 0) {
                for reader in ["sample-reporting-1", "board-1", "compliance-1"] {
                    reply.entitlements.push(DatasetEntitlement {
                        dataset: dataset.clone(),
                        instance: reader.into(),
                        allowed: true,
                        fields: if reader == "board-1" {
                            vec!["meridian.v1.Price.price".into()]
                        } else {
                            vec![]
                        },
                        updated_by: ADMIN.into(),
                        updated_at_ns: 1_791_417_600_000_000_000
                            + (n * 3 + k) as i64 * 60_000_000_000,
                        note: "The book's valuation reads it.".into(),
                        ..Default::default()
                    });
                }
            }
        }
    }
    // Contract v28 (W10.1, W10.13): one dataset licensed on meters, its
    // display terms split professional and non-professional on the
    // standard test, plugin-03 entitled to it.
    reply.licences.push(DatasetLicence {
        dataset: "kraken-1:daily".into(),
        kept: true,
        display: true,
        meters: vec![
            meridian_pb::v1::LicenceMeter::Display as i32,
            meridian_pb::v1::LicenceMeter::NonDisplay as i32,
        ],
        display_terms: Some(meridian_pb::v1::DisplayTerms {
            professional_split: true,
            professional_definition: meridian_pb::v1::ProfessionalDefinition::Standard as i32,
            ..Default::default()
        }),
        underlying_source: "Kraken".into(),
        delay_label: "Real time".into(),
        updated_by: ADMIN.into(),
        updated_at_ns: 1_791_417_600_000_000_000,
        note: "Licensed per person and per plugin.".into(),
        ..Default::default()
    });
    reply.entitlements.push(DatasetEntitlement {
        dataset: "kraken-1:daily".into(),
        instance: "plugin-03".into(),
        allowed: true,
        updated_by: ADMIN.into(),
        updated_at_ns: 1_791_417_600_000_000_000,
        note: "The desk's prices.".into(),
        ..Default::default()
    });
    reply.licences.push(DatasetLicence {
        dataset: "coinbase-1:daily".into(),
        kept: true,
        retention_days: 2555,
        derived_use_terms: meridian_pb::v1::DerivedUse::NotRestricted as i32,
        display: true,
        updated_by: ADMIN.into(),
        updated_at_ns: 1_791_417_600_000_000_000,
        note: "Seven years, as the firm keeps its records.".into(),
        ..Default::default()
    });
    reply
}

/// A connectivity plugin's link in the stand-in conductor's records, made
/// withdrawn and unlinked where none was kept.
fn connectivity_link<'a>(
    records: &'a mut AccessRecords,
    instance: &str,
) -> &'a mut meridian_pb::v1::ConnectivityLink {
    if !records
        .connectivity_links
        .iter()
        .any(|link| link.connectivity_instance_id == instance)
    {
        records
            .connectivity_links
            .push(meridian_pb::v1::ConnectivityLink {
                connectivity_instance_id: instance.into(),
                ..Default::default()
            });
    }
    records
        .connectivity_links
        .iter_mut()
        .find(|link| link.connectivity_instance_id == instance)
        .unwrap()
}

/// The platform's ID of the US dollar's cash instrument, which the
/// stand-in store's US dollar calendar names, and the deployment's record
/// kept under it beside its ISO 4217 code (contract v23, W3.21; from v24
/// requirement 19).
const PLATFORM_USD: &str = "INS-01JUSD0000000000000000001";
/// Contract v29 (676): a municipal revenue bond the platform minted, its
/// terms with every group a page draws -- calls, a sinking fund schedule
/// long enough to page, the municipal group -- and one complete field still
/// to come.
pub(crate) const SERVED_BOND: &str = "INS-01JC29MUNIREVENUE00000001";

fn served_bond() -> InstrumentRecord {
    use meridian_domain::v1::{
        Bond, CouponType, DayCount, DebtSecurityType, FederalTaxStatus, IssuerType, MunicipalTerms,
        RedemptionKind, RedemptionStyle, RedemptionWindow, SinkingFundPayment,
    };
    let decimal = |text: &str| {
        Some(
            text.parse::<meridian_domain::exact::Exact>()
                .unwrap()
                .to_wire(),
        )
    };
    InstrumentRecord {
        instrument_id: SERVED_BOND.into(),
        identifiers: vec![Identifier {
            scheme: "figi".into(),
            value: "BBG01MUNIREV1".into(),
            ..Default::default()
        }],
        asset_class: meridian_domain::v1::AssetClass::Debt as i32,
        instrument_type: meridian_domain::v1::InstrumentType::Bond as i32,
        currency: "USD".into(),
        description: "Commonwealth Water Authority revenue 5% 2041-06-01".into(),
        version: 3,
        template_id: "TPL-bond".into(),
        template_version: "1".into(),
        missing_fields: vec!["payment_calendar_id".into()],
        bond: Some(Bond {
            security_type: DebtSecurityType::Rev as i32,
            issuer_company_id: "ENT-01JC29WATERAUTHORITY0001".into(),
            issuer_type: IssuerType::Municipal as i32,
            country_of_issue: "US".into(),
            state_or_province_of_issue: "MA".into(),
            issue_date: "2021-06-15".into(),
            dated_date: "2021-06-15".into(),
            first_coupon_date: "2021-12-01".into(),
            maturity_date: "2041-06-01".into(),
            coupon_rate: decimal("5"),
            coupon_type: CouponType::Fixed as i32,
            coupon_frequency: 2,
            day_count: DayCount::Thirty360Sia as i32,
            minimum_denomination: decimal("5000"),
            denomination_increment: decimal("5000"),
            original_issue_amount: decimal("185000000"),
            calls: vec![RedemptionWindow {
                kind: RedemptionKind::Par as i32,
                from_date: "2031-06-01".into(),
                price: decimal("100"),
                notice_days_min: 20,
                notice_days_max: 60,
                style: RedemptionStyle::Continuous as i32,
                ..Default::default()
            }],
            sinking_funds: (2032..=2041)
                .map(|year| SinkingFundPayment {
                    date: format!("{year}-06-01"),
                    face_amount: decimal("15000000"),
                    price: None,
                })
                .collect(),
            municipal: Some(MunicipalTerms {
                federal_tax_status: FederalTaxStatus::Exempt as i32,
                state_tax_exempt: true,
                insurer_company_id: "ENT-01JC29ASSUREDGUARANTY01".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The calendars the stand-in instrument store holds: three seed calendars,
/// each with its subject (contract v23), and the US equities rule, and
/// thirty more currencies so the tab pages; filtered by kind and by text on
/// the name and the subject's codes, as the store answers.
fn held_calendars(
    asked: &meridian_domain::v1::ListCalendarsRequest,
) -> meridian_domain::v1::ListCalendarsReply {
    use meridian_domain::v1::{CalendarKind, HeldCalendar, HeldDateRule, SettlementSystem};
    let mut calendars = vec![
        HeldCalendar {
            calendar_id: "CAL-01JAE000000000000000USD1".into(),
            name: "US dollar".into(),
            kind: CalendarKind::Currency as i32,
            version: 3,
            coverage_to_date: "2027-12-31".into(),
            refreshed_at_ns: 1_791_417_600_000_000_000,
            instrument_id: PLATFORM_USD.into(),
            ..Default::default()
        },
        HeldCalendar {
            calendar_id: "CAL-01JAE000000000000000XNYS".into(),
            name: "US equity venues".into(),
            kind: CalendarKind::Trading as i32,
            version: 7,
            coverage_to_date: "2027-12-31".into(),
            refreshed_at_ns: 1_791_417_600_000_000_000,
            venue_ids: vec![
                "VEN-01JAE00000000000000XNYS".into(),
                "VEN-01JAE00000000000000XNAS".into(),
            ],
            ..Default::default()
        },
        HeldCalendar {
            calendar_id: "CAL-01JAE0000000000000000USSS".into(),
            name: "US securities settlement".into(),
            kind: CalendarKind::SecuritiesSettlement as i32,
            version: 4,
            coverage_to_date: "2028-12-31".into(),
            refreshed_at_ns: 1_791_417_600_000_000_000,
            settlement_system: Some(SettlementSystem {
                name: "DTC".into(),
                country_code: "US".into(),
            }),
            ..Default::default()
        },
    ];
    for n in 0..30 {
        calendars.push(HeldCalendar {
            calendar_id: format!("CAL-01JAE00000000000000CUR{n:02}"),
            name: format!("Currency {n:02} settlement days, as its central bank publishes them"),
            kind: CalendarKind::Currency as i32,
            version: 1,
            coverage_to_date: "2027-12-31".into(),
            refreshed_at_ns: 1_791_417_600_000_000_000,
            ..Default::default()
        });
    }
    let query = asked.query.trim().to_lowercase();
    calendars.retain(|calendar| {
        let codes = [
            calendar.name.clone(),
            if calendar.instrument_id == PLATFORM_USD {
                "USD".into()
            } else {
                String::new()
            },
            calendar
                .venue_ids
                .iter()
                .map(|id| id[id.len() - 4..].to_string())
                .collect::<Vec<_>>()
                .join(" "),
            calendar
                .settlement_system
                .as_ref()
                .map(|system| format!("{} {}", system.name, system.country_code))
                .unwrap_or_default(),
        ];
        (asked.kind == 0 || calendar.kind == asked.kind)
            && (query.is_empty()
                || codes
                    .iter()
                    .any(|code| code.to_lowercase().contains(&query)))
    });
    meridian_domain::v1::ListCalendarsReply {
        calendars,
        next_cursor: String::new(),
        rules: vec![HeldDateRule {
            kind: meridian_pb::v1::DateRuleKind::Settlement as i32,
            subject: "US/equity".into(),
            version: 2,
            refreshed_at_ns: 1_791_417_600_000_000_000,
        }],
    }
}

/// A calendar's year as the stand-in store answers it: the Federal
/// Reserve's holidays in 2026.
fn calendar_year(
    asked: &meridian_domain::v1::ReadCalendarRequest,
) -> meridian_domain::v1::ReadCalendarReply {
    use meridian_domain::v1::{
        CalendarCoverage, CalendarDay, CalendarDayKind, CalendarSource, CalendarWeekend, Weekday,
    };
    let days = [
        ("2026-01-01", "New Year's Day"),
        ("2026-01-19", "Birthday of Martin Luther King, Jr."),
        ("2026-02-16", "Washington's Birthday"),
        ("2026-05-25", "Memorial Day"),
        ("2026-06-19", "Juneteenth National Independence Day"),
        ("2026-09-07", "Labor Day"),
        ("2026-10-12", "Columbus Day"),
        ("2026-11-11", "Veterans Day"),
        ("2026-11-26", "Thanksgiving Day"),
        ("2026-12-25", "Christmas Day"),
    ]
    .into_iter()
    .filter(|(date, _)| *date >= asked.from_date.as_str() && *date <= asked.to_date.as_str())
    .map(|(date, name)| CalendarDay {
        date: date.into(),
        kind: CalendarDayKind::Closed as i32,
        name: name.into(),
        source: "fed-holidays".into(),
    })
    .collect();
    meridian_domain::v1::ReadCalendarReply {
        found: true,
        calendar_id: asked.calendar_id.clone(),
        version: 3,
        days,
        weekends: vec![CalendarWeekend {
            weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
            valid_from_date: "2016-01-01".into(),
        }],
        coverage: Some(CalendarCoverage {
            from_date: "2026-01-01".into(),
            to_date: "2027-12-31".into(),
        }),
        sources: vec![CalendarSource {
            source_id: "fed-holidays".into(),
            name: "Federal Reserve holidays".into(),
            address: "https://www.federalreserve.gov/aboutthefed/k8.htm".into(),
            read_at_ns: 1_760_140_800_000_000_000,
        }],
        trading_sessions: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "serves until stopped: make e2e-settings-page runs it for the browser"]
async fn serve_a_plugins_settings_pages_for_a_browser() {
    let clock = meridian_clock::SystemClock;
    let bus = Arc::new(Bus::single(
        "dashboard-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let held = Arc::new(Mutex::new(records()));
    {
        let held = Arc::clone(&held);
        bus.serve(ACCESS_RECORDS, move |envelope| {
            AccessRecordsRequest::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
            Ok(("".into(), held.lock().unwrap().encode_to_vec()))
        });
    }
    {
        let held = Arc::clone(&held);
        bus.serve(
            "platform.config.command.set-plugin-settings",
            move |envelope| {
                let asked = SetPluginSettingsRequest::decode(&envelope.payload[..])
                    .map_err(|e| e.to_string())?;
                let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
                let mut records = held.lock().unwrap();
                set(
                    &mut records,
                    asked,
                    &by,
                    meridian_clock::SystemClock.now_ns(),
                )?;
                let record = records.plugin_settings[0].clone();
                Ok(("".into(), record.encode_to_vec()))
            },
        );
    }
    {
        // The stand-in conductor's access groups: kept as asked, the Access
        // editor's rows a group's entries per role (contract v15).
        let held = Arc::clone(&held);
        bus.serve(
            "platform.config.command.define-access-group",
            move |envelope| {
                let asked =
                    meridian_domain::v1::DefineAccessGroupRequest::decode(&envelope.payload[..])
                        .map_err(|e| e.to_string())?;
                let mut group = asked.access_group.unwrap_or_default();
                if group.access_group_id.is_empty() {
                    group.access_group_id = format!("AX-{}", group.name.to_uppercase());
                }
                let mut records = held.lock().unwrap();
                records
                    .access_groups
                    .retain(|g| g.access_group_id != group.access_group_id);
                records.access_groups.push(group.clone());
                Ok(("".into(), group.encode_to_vec()))
            },
        );
    }
    {
        // The book's attributes of each account (W9.13): held as set, each
        // change answered as the book answers it.
        use meridian_domain::v1::{
            set_account_attribute_request::Attribute, AccountAttributeReply, AccountAttributes,
            ListAccountAttributesReply, LotReliefMethod, OpeningBalance,
            SetAccountAttributeRequest,
        };
        let book: Arc<Mutex<Vec<AccountAttributes>>> = Arc::new(Mutex::new(
            accounts()
                .iter()
                .take(4)
                .enumerate()
                .map(|(n, account)| AccountAttributes {
                    account_id: account.account_id.clone(),
                    base_currency_code: "USD".into(),
                    lot_relief_default: if n == 2 {
                        LotReliefMethod::HighestCost as i32
                    } else {
                        LotReliefMethod::FirstInFirstOut as i32
                    },
                    opening_balance: (n < 2).then(|| OpeningBalance {
                        as_of_date: "2026-09-08".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
        ));
        {
            let book = Arc::clone(&book);
            bus.serve(crate::admin::books::LIST_ACCOUNT_ATTRIBUTES, move |_| {
                Ok((
                    "".into(),
                    ListAccountAttributesReply {
                        attributes: book.lock().unwrap().clone(),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
            });
        }
        bus.serve(
            crate::admin::books::SET_ACCOUNT_ATTRIBUTE,
            move |envelope| {
                let asked = SetAccountAttributeRequest::decode(&envelope.payload[..])
                    .map_err(|e| e.to_string())?;
                let mut held = book.lock().unwrap();
                if !held.iter().any(|a| a.account_id == asked.account_id) {
                    held.push(AccountAttributes {
                        account_id: asked.account_id.clone(),
                        ..Default::default()
                    });
                }
                let attributes = held
                    .iter_mut()
                    .find(|a| a.account_id == asked.account_id)
                    .unwrap();
                match asked.attribute {
                    Some(Attribute::BaseCurrencyCode(code)) => attributes.base_currency_code = code,
                    Some(Attribute::LotReliefDefault(method)) => {
                        attributes.lot_relief_default = method
                    }
                    _ => {}
                }
                Ok((
                    "".into(),
                    AccountAttributeReply {
                        attributes: Some(attributes.clone()),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
            },
        );
    }
    // Contract v24 (W3.11, W3.28, W11.36, W11.37): the provisional records,
    // the usage reports sent, and the escape hatch's setting.
    bus.serve(crate::admin::instruments::LIST_USAGE_REPORTS, |_| {
        Ok((
            "".into(),
            meridian_domain::v1::ListUsageReportsReply {
                reports_usage: true,
                licensed_schemes: vec!["cusip".into()],
                reports: vec![meridian_domain::v1::SentUsageReport {
                    sent_at_ns: 1_791_417_600_000_000_000,
                    received: true,
                    report: Some(meridian_domain::v1::IdentifierUsageReport {
                        period_start: "2026-10-05".into(),
                        period_end: "2026-10-11".into(),
                        licensed_schemes: vec!["cusip".into()],
                        scheme_counts: vec![meridian_domain::v1::SchemeUsageCount {
                            scheme: "cusip".into(),
                            instruments_carrying: 412,
                            instruments_only_by: 37,
                            misses_only_by: 4,
                        }],
                    }),
                }],
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::admin::instruments::SET_REPORTS_USAGE, |envelope| {
        let asked = meridian_domain::v1::SetReportsUsageRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        Ok((
            "".into(),
            meridian_domain::v1::ReportsUsageSetting {
                reports_usage: asked.reports_usage,
                licensed_schemes: Vec::new(),
            }
            .encode_to_vec(),
        ))
    });
    {
        let local = Arc::new(Mutex::new(meridian_domain::v1::LocalRecordOrdersSetting {
            allows_local_record_orders: true,
            open_order_count: 3,
        }));
        let reading = Arc::clone(&local);
        bus.serve(crate::admin::trading::READ_LOCAL_RECORD_ORDERS, move |_| {
            Ok(("".into(), reading.lock().unwrap().encode_to_vec()))
        });
        bus.serve(
            crate::admin::trading::SET_LOCAL_RECORD_ORDERS,
            move |envelope| {
                let asked =
                    meridian_domain::v1::SetLocalRecordOrdersRequest::decode(&envelope.payload[..])
                        .map_err(|e| e.to_string())?;
                let mut held = local.lock().unwrap();
                held.allows_local_record_orders = asked.allows_local_record_orders;
                Ok(("".into(), held.encode_to_vec()))
            },
        );
    }
    // Contract v24 (W3.26): the migration's lines made, answered with the
    // counts after.
    bus.serve(
        crate::admin::instruments::APPLY_INSTRUMENT_MIGRATION,
        |envelope| {
            let asked =
                meridian_domain::v1::ApplyInstrumentMigrationRequest::decode(&envelope.payload[..])
                    .map_err(|e| e.to_string())?;
            let mut counts = provisional_listed().migration_counts.unwrap_or_default();
            counts.held_back += asked.held_back_ids.len() as i64;
            Ok(("".into(), counts.encode_to_vec()))
        },
    );
    // Contract v28 (W3.33; rulings 727, 774, 778): the conductor's
    // candidates for the picker's one search -- SPY, which the deployment
    // does not hold, on its US composite with no venue -- and the store's
    // resolve bringing one in, kept and listed after. As on the dev
    // deployment (ruling 786), the platform has not answered yet: the record
    // is provisional under an `LCL-` ID, its name only offered.
    let brought: Arc<Mutex<Vec<InstrumentRecord>>> = Arc::new(Mutex::new(Vec::new()));
    bus.serve(
        crate::admin::instruments::ASK_PLATFORM_FOR_INSTRUMENT_CANDIDATES,
        |envelope| {
            let asked =
                meridian_domain::v1::InstrumentCandidatesRequest::decode(&envelope.payload[..])
                    .map_err(|e| e.to_string())?;
            let query = asked.query.trim().to_uppercase();
            let candidates = if !query.is_empty() && "SPY".starts_with(&query) {
                vec![meridian_domain::v1::InstrumentCandidate {
                    ticker: "SPY".into(),
                    country: "US".into(),
                    name: "SPDR S&P 500 ETF Trust".into(),
                    asset_class: meridian_domain::v1::AssetClass::Fund as i32,
                    currency: "USD".into(),
                    composite_figi: "BBG000BDTBL9".into(),
                    share_class_figi: "BBG001S72SM3".into(),
                    identifiers: vec![
                        // Its ticker on the platform's "US composite" venue,
                        // which has no MIC (779), as the platform answers it.
                        Identifier {
                            scheme: "ticker".into(),
                            value: "SPY".into(),
                            source: "VEN-01JUSCOMPOSITE00000000001".into(),
                            ..Default::default()
                        },
                        Identifier {
                            scheme: "share_class_figi".into(),
                            value: "BBG001S72SM3".into(),
                            ..Default::default()
                        },
                        Identifier {
                            scheme: "composite_figi".into(),
                            value: "BBG000BDTBL9".into(),
                            source: "US".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }]
            } else {
                Vec::new()
            };
            Ok((
                "".into(),
                meridian_domain::v1::AskPlatformForInstrumentCandidatesReply {
                    reachable: true,
                    answer: Some(meridian_domain::v1::InstrumentCandidatesReply {
                        candidates,
                        sources: Vec::new(),
                    }),
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        },
    );
    {
        let brought = Arc::clone(&brought);
        bus.serve(
            crate::admin::instruments::RESOLVE_IDENTIFIER,
            move |envelope| {
                let asked =
                    meridian_domain::v1::ResolveIdentifierRequest::decode(&envelope.payload[..])
                        .map_err(|e| e.to_string())?;
                use meridian_domain::v1::{instrument_value, InstrumentValue, OfferedValue};
                let mut held = brought.lock().unwrap();
                let id = BROUGHT_IN.to_string();
                if !held.iter().any(|r| r.instrument_id == id) {
                    let offered = |value: instrument_value::Value| OfferedValue {
                        value: Some(InstrumentValue {
                            source: "the instrument picker for Ada Admin".into(),
                            value: Some(value),
                        }),
                        offered_at_ns: 1,
                        ..Default::default()
                    };
                    held.push(InstrumentRecord {
                        instrument_id: id.clone(),
                        identifiers: asked.identifiers.clone(),
                        offers: vec![
                            offered(instrument_value::Value::Description(
                                asked.stated_description.clone(),
                            )),
                            offered(instrument_value::Value::Currency(
                                asked.stated_currency.clone(),
                            )),
                        ],
                        version: 1,
                        ..Default::default()
                    });
                }
                Ok((
                    "".into(),
                    meridian_domain::v1::ResolveIdentifierReply {
                        found: true,
                        instrument_id: id,
                        minted: true,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
            },
        );
    }
    // The lake's latest close for whatever a source is read for (W10.10).
    bus.serve(crate::admin::sources_view::QUERY_LAKE, |envelope| {
        let asked = meridian_domain::v1::QueryLakeRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        let dataset = asked
            .sources
            .and_then(|s| s.named.first().cloned())
            .unwrap_or_default();
        let price = meridian_domain::v1::Price {
            meta: Some(meridian_domain::v1::ObservationMeta {
                valid_from_ns: 1_793_000_000_000_000_000,
                source: Some(meridian_domain::v1::Source {
                    dataset,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            kind: meridian_domain::v1::PriceKind::Close as i32,
            price: Some(meridian_domain::v1::Money {
                amount: Some(
                    "687.12"
                        .parse::<meridian_domain::exact::Exact>()
                        .unwrap()
                        .to_wire(),
                ),
                currency_code: "USD".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok((
            "".into(),
            meridian_domain::v1::QueryLakeReply {
                prices: vec![price],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let listing_brought = Arc::clone(&brought);
    bus.serve(
        crate::admin::instruments::LIST_INSTRUMENTS_TO_COMPLETE,
        move |envelope| {
            let asked = ListInstrumentsToCompleteRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            // Contract v24 (W3.11): the provisional records, each with why,
            // and the migration's counts.
            if asked.provisional {
                return Ok(("".into(), provisional_listed().encode_to_vec()));
            }
            // The picker's search (contract v20, W3.11): by the record's ID,
            // complete ones included, as the store answers it.
            let search = asked.search.trim().to_lowercase();
            let wanted = |id: &String| {
                if !asked.instrument_id.is_empty() {
                    *id == asked.instrument_id
                } else if !search.is_empty() {
                    id.to_lowercase().contains(&search)
                } else {
                    asked.include_complete || incomplete(id)
                }
            };
            let mut instruments: Vec<InstrumentToComplete> = (1..=250)
                .map(instrument_id)
                .filter(wanted)
                .map(instrument_listed)
                .collect();
            // The US dollar's record, kept under the platform's `INS-` ID
            // (contract v24, requirement 19), found by it.
            if (!search.is_empty() && PLATFORM_USD.to_lowercase().contains(&search))
                || asked.instrument_id == PLATFORM_USD
            {
                instruments.push(InstrumentToComplete {
                    instrument: Some(InstrumentRecord {
                        instrument_id: PLATFORM_USD.into(),
                        identifiers: vec![Identifier {
                            scheme: "iso4217".into(),
                            value: "USD".into(),
                            ..Default::default()
                        }],
                        asset_class: meridian_domain::v1::AssetClass::Cash as i32,
                        currency: "USD".into(),
                        description: "US dollar".into(),
                        version: 1,
                        ..Default::default()
                    }),
                    complete_for_book: true,
                    complete: true,
                    ..Default::default()
                });
            }
            // Contract v29: the municipal bond, by its ID.
            if asked.instrument_id == SERVED_BOND {
                instruments.push(InstrumentToComplete {
                    instrument: Some(served_bond()),
                    complete_for_book: true,
                    complete: true,
                    ..Default::default()
                });
            }
            // A record the picker brought in (contract v28), found by its
            // ID, its description or an identifier.
            for record in listing_brought.lock().unwrap().iter() {
                let found = asked.instrument_id == record.instrument_id
                    || (!search.is_empty()
                        && (record.instrument_id.to_lowercase() == search
                            || crate::admin::instruments::name_of(record)
                                .to_lowercase()
                                .contains(&search)
                            || record
                                .identifiers
                                .iter()
                                .any(|i| i.value.to_lowercase() == search)));
                if found {
                    instruments.push(InstrumentToComplete {
                        instrument: Some(record.clone()),
                        complete_for_book: true,
                        complete: true,
                        ..Default::default()
                    });
                }
            }
            Ok((
                "".into(),
                ListInstrumentsToCompleteReply {
                    instruments,
                    conflicts: if asked.instrument_id.is_empty() {
                        conflicts()
                    } else {
                        Vec::new()
                    },
                    incomplete_for_book: 60,
                    incomplete: 60,
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        },
    );
    {
        // The stand-in conductor's links (W11.21, contract v22): kept as
        // asked.
        let held = Arc::clone(&held);
        bus.serve(
            crate::admin::connectivity::SET_CONNECTIVITY_AVAILABLE,
            move |envelope| {
                let asked = meridian_domain::v1::SetConnectivityAvailableRequest::decode(
                    &envelope.payload[..],
                )
                .map_err(|e| e.to_string())?;
                let mut records = held.lock().unwrap();
                let link = connectivity_link(&mut records, &asked.connectivity_instance_id);
                link.available = asked.available;
                Ok(("".into(), link.encode_to_vec()))
            },
        );
    }
    {
        let held = Arc::clone(&held);
        bus.serve(
            crate::admin::connectivity::LINK_CONNECTIVITY,
            move |envelope| {
                let asked =
                    meridian_domain::v1::LinkConnectivityRequest::decode(&envelope.payload[..])
                        .map_err(|e| e.to_string())?;
                let mut records = held.lock().unwrap();
                let link = connectivity_link(&mut records, &asked.connectivity_instance_id);
                link.ems_instance_id = asked.ems_instance_id;
                link.users = asked.users;
                Ok(("".into(), link.encode_to_vec()))
            },
        );
    }
    // The instrument store's calendars (W3.19, W3.21, contract v22).
    bus.serve(crate::admin::calendars::LIST_CALENDARS, |envelope| {
        let asked = meridian_domain::v1::ListCalendarsRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        Ok(("".into(), held_calendars(&asked).encode_to_vec()))
    });
    // Contract v25 (D9): a calendar's venue read by its ID, shown by its MIC.
    bus.serve(crate::admin::calendars::RESOLVE_VENUE, |envelope| {
        let asked = meridian_domain::v1::ResolveVenueRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        let (mic, name) = match asked.venue_id.as_str() {
            "VEN-01JAE00000000000000XNYS" => ("XNYS", "New York Stock Exchange"),
            "VEN-01JAE00000000000000XNAS" => ("XNAS", "Nasdaq"),
            _ => ("", ""),
        };
        Ok((
            "".into(),
            meridian_domain::v1::ResolveVenueReply {
                found: !mic.is_empty(),
                venue: (!mic.is_empty()).then(|| meridian_domain::v1::VenueRecord {
                    venue_id: asked.venue_id.clone(),
                    name: name.into(),
                    identifiers: vec![meridian_domain::v1::Identifier {
                        scheme: "iso10383".into(),
                        value: mic.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                miss_reason: 0,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::admin::calendars::READ_CALENDAR, |envelope| {
        let asked = meridian_domain::v1::ReadCalendarRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        Ok(("".into(), calendar_year(&asked).encode_to_vec()))
    });
    // Contract v29: the companies the bond's terms name, as the store holds
    // them; the insurer it holds no record of.
    bus.serve(crate::admin::instruments::RESOLVE_COMPANY, |envelope| {
        let asked = meridian_domain::v1::ResolveCompanyRequest::decode(&envelope.payload[..])
            .map_err(|e| e.to_string())?;
        if asked.company_id != "ENT-01JC29WATERAUTHORITY0001" {
            return Err(meridian_bus::refusal_naming(
                meridian_pb::v1::RefusalReason::Unspecified as i32,
                &["company_id".to_string()],
                "the store holds no such company".to_string(),
            ));
        }
        Ok((
            "meridian.v1.CompanyRecord".into(),
            meridian_domain::v1::CompanyRecord {
                company_id: asked.company_id,
                name: "Commonwealth Water Authority".into(),
                version: 1,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::admin::instruments::READ_INSTRUMENT_HISTORY, |_| {
        Ok((
            "".into(),
            meridian_domain::v1::ReadInstrumentHistoryReply::default().encode_to_vec(),
        ))
    });

    // The stand-in conductor's archive and moves (contract v16): ReadMoves
    // answered from the moves above, an archive allowed or withdrawn as
    // asked, each hold kept as set.
    let archive: Arc<Mutex<Option<meridian_domain::v1::PluginArchive>>> = Arc::default();
    {
        let archive = Arc::clone(&archive);
        bus.serve(crate::archive::READ_MOVES, move |envelope| {
            let asked = meridian_domain::v1::ReadMovesRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            let all = moves();
            let from = asked
                .cursor
                .strip_prefix("before-")
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            let page: Vec<_> = all.iter().skip(from).take(50).cloned().collect();
            let next = if from + 50 < all.len() {
                format!("before-{}", from + 50)
            } else {
                String::new()
            };
            let reply = meridian_domain::v1::ReadMovesReply {
                moves: page,
                next_cursor: next,
                archived: vec![meridian_pb::v1::StoredSpan {
                    record_kind: "activity".into(),
                    record_count: (0..60).map(|n| 180 + n).sum(),
                    first_received_ns: 1_388_534_400_000_000_000,
                    last_received_ns: 1_388_534_400_000_000_000 + 60 * 30 * 86_400_000_000_000 - 1,
                    bytes: 0,
                }],
                archive: archive.lock().unwrap().clone(),
            };
            Ok(("".into(), reply.encode_to_vec()))
        });
    }
    {
        let archive = Arc::clone(&archive);
        bus.serve(crate::archive::ALLOW_ARCHIVE, move |envelope| {
            let asked = meridian_domain::v1::AllowArchiveRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
            let allowed = meridian_domain::v1::PluginArchive {
                instance_id: asked.instance_id,
                allowed: true,
                most_bytes: asked.most_bytes,
                updated_by: by,
                updated_at_ns: meridian_clock::SystemClock.now_ns(),
                ..Default::default()
            };
            *archive.lock().unwrap() = Some(allowed.clone());
            Ok(("".into(), allowed.encode_to_vec()))
        });
    }
    {
        let archive = Arc::clone(&archive);
        bus.serve(crate::archive::WITHDRAW_ARCHIVE, move |_| {
            let mut held = archive.lock().unwrap();
            let withdrawn = meridian_domain::v1::PluginArchive {
                allowed: false,
                ..held.clone().unwrap_or_default()
            };
            *held = Some(withdrawn.clone());
            Ok(("".into(), withdrawn.encode_to_vec()))
        });
    }
    {
        let held = Arc::clone(&held);
        bus.serve("platform.config.command.set-hold", move |envelope| {
            let asked = meridian_domain::v1::SetHoldRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            let hold = meridian_domain::v1::Hold {
                role: asked.role,
                days: asked.days,
                write_once: asked.write_once,
                updated_by: envelope.meta.clone().unwrap_or_default().acting_for_subject,
                updated_at_ns: meridian_clock::SystemClock.now_ns(),
                ..Default::default()
            };
            let mut records = held.lock().unwrap();
            records.holds.retain(|h| h.role != hold.role);
            if hold.days > 0 {
                records.holds.push(hold.clone());
            }
            Ok(("".into(), hold.encode_to_vec()))
        });
    }

    // The stand-in lake and conductor behind the Data sources page (contract
    // v18): the listing kept, each licence and entitlement replacing its
    // own, each priority replaced whole against what was read.
    let lake = Arc::new(Mutex::new((
        lake_listing(),
        vec![
            meridian_domain::v1::SourcePriority {
                data_type: "meridian.v1.Price".into(),
                kind: 1,
                datasets: vec![
                    "coinbase-1:daily".into(),
                    "kraken-1:daily".into(),
                    "alpaca-1:daily".into(),
                ],
                updated_by: ADMIN.into(),
                updated_at_ns: 1_791_417_600_000_000_000,
                note: "The exchanges' own closes first.".into(),
                ..Default::default()
            },
            meridian_domain::v1::SourcePriority {
                data_type: "meridian.v1.Bar".into(),
                datasets: vec!["databento-1:daily".into(), "massive-1:daily".into()],
                updated_by: ADMIN.into(),
                updated_at_ns: 1_791_417_600_000_000_000,
                ..Default::default()
            },
        ],
    )));
    {
        let lake = Arc::clone(&lake);
        bus.serve(crate::admin::data_sources::LIST_DATASETS, move |_| {
            Ok(("".into(), lake.lock().unwrap().0.encode_to_vec()))
        });
    }
    {
        let lake = Arc::clone(&lake);
        bus.serve(
            crate::admin::data_sources::LIST_SOURCE_PRIORITIES,
            move |_| {
                let priorities = lake.lock().unwrap().1.clone();
                Ok((
                    "".into(),
                    meridian_domain::v1::ListSourcePrioritiesReply { priorities }.encode_to_vec(),
                ))
            },
        );
    }
    {
        let lake = Arc::clone(&lake);
        bus.serve(
            crate::admin::data_sources::SET_DATASET_LICENCE,
            move |envelope| {
                let asked =
                    meridian_domain::v1::SetDatasetLicenceRequest::decode(&envelope.payload[..])
                        .map_err(|e| e.to_string())?;
                let licence = meridian_pb::v1::DatasetLicence {
                    dataset: asked.dataset.clone(),
                    updated_by: envelope.meta.clone().unwrap_or_default().acting_for_subject,
                    updated_at_ns: meridian_clock::SystemClock.now_ns(),
                    note: asked.note,
                    ..asked.licence.unwrap_or_default()
                };
                let mut held = lake.lock().unwrap();
                held.0.licences.retain(|l| l.dataset != licence.dataset);
                held.0.licences.push(licence.clone());
                Ok(("".into(), licence.encode_to_vec()))
            },
        );
    }
    {
        let lake = Arc::clone(&lake);
        bus.serve(
            crate::admin::data_sources::SET_DATASET_ENTITLEMENT,
            move |envelope| {
                let asked = meridian_domain::v1::SetDatasetEntitlementRequest::decode(
                    &envelope.payload[..],
                )
                .map_err(|e| e.to_string())?;
                let entitlement = meridian_domain::v1::DatasetEntitlement {
                    dataset: asked.dataset,
                    instance: asked.instance,
                    allowed: asked.allowed,
                    fields: asked.fields,
                    updated_by: envelope.meta.clone().unwrap_or_default().acting_for_subject,
                    updated_at_ns: meridian_clock::SystemClock.now_ns(),
                    note: asked.note,
                    ..Default::default()
                };
                let mut held = lake.lock().unwrap();
                held.0.entitlements.retain(|e| {
                    !(e.dataset == entitlement.dataset && e.instance == entitlement.instance)
                });
                held.0.entitlements.push(entitlement.clone());
                Ok(("".into(), entitlement.encode_to_vec()))
            },
        );
    }
    {
        let lake = Arc::clone(&lake);
        bus.serve(crate::admin::data_sources::SET_SOURCE_PRIORITY, move |envelope| {
            let asked = meridian_domain::v1::SetSourcePriorityRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            let mut held = lake.lock().unwrap();
            let standing = held
                .1
                .iter()
                .find(|p| p.data_type == asked.data_type && p.kind == asked.kind)
                .map_or(0, |p| p.updated_at_ns);
            if standing != asked.against_updated_at_ns {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::RecordChanged as i32,
                    &["against_updated_at_ns".to_string()],
                    "the priority changed since it was read; read it again, and nothing was changed",
                ));
            }
            let priority = meridian_domain::v1::SourcePriority {
                data_type: asked.data_type,
                kind: asked.kind,
                datasets: asked.datasets,
                updated_by: envelope.meta.clone().unwrap_or_default().acting_for_subject,
                updated_at_ns: meridian_clock::SystemClock.now_ns(),
                note: asked.note,
                ..Default::default()
            };
            held.1
                .retain(|p| !(p.data_type == priority.data_type && p.kind == priority.kind));
            held.1.push(priority.clone());
            Ok(("".into(), priority.encode_to_vec()))
        });
    }

    // Contract v25: the catalogue holding SnapTrade's version, launched as
    // INSTANCE, and its documentation as a vendor writes it (the SnapTrade
    // draft's shape: headings, lists, a table, outside links), for the
    // Documentation tab, the versions list and a version's page.
    bus.serve(crate::catalogue::PLUGIN_CATALOGUE, |_| {
        let version = |v: &str| meridian_domain::v1::PluginVersion {
            metadata: Some(meridian_domain::v1::PluginMetadata {
                name: "snaptrade".into(),
                version: v.into(),
                roles: vec!["custody".into()],
                declaration: Some(documented_declaration()),
                ..Default::default()
            }),
            uploaded_by: ADMIN.into(),
            uploaded_at_ns: 1_790_380_800_000_000_000,
            source_registry: "local".into(),
            ..Default::default()
        };
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue {
                next_cursor: String::new(),
                versions: (0..14)
                    .map(|n| version(&format!("0.{n}.0")))
                    .chain(registry_versions())
                    .collect(),
                launches: vec![
                    meridian_domain::v1::PluginLaunch {
                        instance_id: INSTANCE.into(),
                        name: "snaptrade".into(),
                        version: "0.13.0".into(),
                        roles: vec!["custody".into()],
                        state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                        ..Default::default()
                    },
                    // Contract v25: an instance of a listed version, pulled
                    // since, with an update that widens its roles.
                    meridian_domain::v1::PluginLaunch {
                        instance_id: "fx-1".into(),
                        name: "open-meridian/fx".into(),
                        version: "1.4.0".into(),
                        roles: vec!["custody".into()],
                        state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                        ..Default::default()
                    },
                    // Ruling 831: the connectivity plugin on the home, its
                    // card saying where it trades.
                    meridian_domain::v1::PluginLaunch {
                        instance_id: "fix-1".into(),
                        name: "a FIX venue".into(),
                        version: "0.2.0".into(),
                        roles: vec!["connectivity".into()],
                        state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                        ..Default::default()
                    },
                ],
            }
            .encode_to_vec(),
        ))
    });
    // Contract v26 (W6.35): the deployment's time zone, set once, and the
    // operations plugin's affirmation deadline, for the Time zone tab.
    bus.serve(crate::admin::time_zone::READ_DEPLOYMENT_TIME_ZONE, |_| {
        Ok((
            "meridian.v1.DeploymentTimeZoneSetting".into(),
            meridian_domain::v1::DeploymentTimeZoneSetting {
                deployment_time_zone: "America/Chicago".into(),
                changed_at_ns: 1_792_300_000_000_000_000,
                changed_by: ADMIN.into(),
                affirmation_deadline: 21 * 60,
                affirmation_deadline_instance_id: "operations-1".into(),
            }
            .encode_to_vec(),
        ))
    });
    // Contract v27 (W6.36): the FX pivot currencies, the US dollar's record
    // first, set by the admin, for the FX pivot currencies tab; never set in
    // the development deployment's pass, which the tab says is the default
    // (ruling 512).
    let never_set = std::env::var("MERIDIAN_SETTINGS_DEVELOPMENT").is_ok_and(|v| v == "1");
    bus.serve(
        crate::admin::fx_pivots::READ_FX_PIVOT_CURRENCIES,
        move |_| {
            let setting = if never_set {
                meridian_domain::v1::FxPivotCurrenciesSetting::default()
            } else {
                meridian_domain::v1::FxPivotCurrenciesSetting {
                    fx_pivot_currencies: vec![PLATFORM_USD.into()],
                    changed_at_ns: 1_792_300_000_000_000_000,
                    changed_by: ADMIN.into(),
                }
            };
            Ok((
                "meridian.v1.FxPivotCurrenciesSetting".into(),
                setting.encode_to_vec(),
            ))
        },
    );
    // Contract v26 (W12.6): the routing table, one model id routed with a
    // long reason, and the measures the analytics component serves.
    bus.serve(crate::admin::model_routing::LIST_MODEL_ROUTING, |_| {
        Ok((
            "meridian.v1.ListModelRoutingReply".into(),
            meridian_domain::v1::ListModelRoutingReply {
                routes: vec![
                    meridian_domain::v1::ModelRouting {
                        model_id: "cash".into(),
                        implementation: "built-in".into(),
                        ..Default::default()
                    },
                    meridian_domain::v1::ModelRouting {
                        model_id: "quoted_price".into(),
                        implementation: "sample-pricing-with-a-long-instance-name-1".into(),
                        changed_at_ns: 1_792_300_000_000_000_000,
                        changed_by: ADMIN.into(),
                        reason: "Prove the pricing seam against the built-in model, as the \
                                 sample plugin answers it"
                            .into(),
                    },
                    meridian_domain::v1::ModelRouting {
                        model_id: "binary_event".into(),
                        implementation: "built-in".into(),
                        ..Default::default()
                    },
                ]
                .into_iter()
                // Contract v29 (stage E): v28's and v29's models, built-in.
                .chain(
                    crate::admin::model_routing::MODEL_IDS[3..]
                        .iter()
                        .map(|model| meridian_domain::v1::ModelRouting {
                            model_id: model.to_string(),
                            implementation: "built-in".into(),
                            ..Default::default()
                        }),
                )
                .collect(),
                version: 2,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(
        crate::admin::model_routing::LIST_MEASURE_DEFINITIONS,
        |_| {
            Ok((
                "meridian.v1.ListMeasureDefinitionsReply".into(),
                meridian_domain::v1::ListMeasureDefinitionsReply {
                    definitions: [
                        "market_value",
                        "exposure",
                        "concentration",
                        "pnl",
                        "historical_var",
                        "expected_shortfall",
                        "resolution_payoff",
                        "implied_distribution",
                    ]
                    .iter()
                    .map(|d| meridian_domain::v1::MeasureDefinition {
                        definition: d.to_string(),
                        version: 1,
                        ..Default::default()
                    })
                    .collect(),
                }
                .encode_to_vec(),
            ))
        },
    );
    // Contract v25 (W8.10 to W8.12, W5.19): the catalogue's copy Browse and
    // a listing's page read, filtered as the conductor filters it, and the
    // usage reports sent.
    bus.serve(crate::registry::REGISTRY_PLUGINS, |envelope| {
        let asked =
            meridian_domain::v1::ListRegistryPluginsRequest::decode(&envelope.payload[..]).unwrap();
        // The starter packs' members (v28), answered when one is named, so
        // Browse's own list stays as it was.
        let members = pack_listings()
            .into_iter()
            .filter(|l| !asked.listing.is_empty() && l.listing == asked.listing);
        let listings = registry_listings()
            .into_iter()
            .filter(|l| asked.listing.is_empty() || l.listing == asked.listing)
            .filter(|l| asked.search.is_empty() || l.listing.contains(&asked.search))
            .chain(members)
            .collect();
        Ok((
            "meridian.v1.ListRegistryPluginsReply".into(),
            meridian_domain::v1::ListRegistryPluginsReply {
                listings,
                next_cursor: String::new(),
                catalogue_pulled_at_ns: 1_792_200_000_000_000_000,
            }
            .encode_to_vec(),
        ))
    });
    // Contract v27's wire, built in v28 (W8.18, W8.19): two starter packs --
    // Crypto installed at revision 1 of 2, a member added since, one
    // launched, one failed; Multi-asset's sixteen not installed, one
    // unavailable and one not permitted here -- and their acts answered as
    // the conductor answers them.
    bus.serve(crate::registry::packs::REGISTRY_PACKS, |envelope| {
        let asked =
            meridian_domain::v1::ListRegistryPacksRequest::decode(&envelope.payload[..]).unwrap();
        let packs = served_packs()
            .into_iter()
            .filter(|p| asked.pack.is_empty() || p.pack == asked.pack)
            .filter(|p| {
                asked.search.is_empty()
                    || p.revision.as_ref().is_some_and(|r| {
                        r.title
                            .to_lowercase()
                            .contains(&asked.search.to_lowercase())
                    })
            })
            .collect();
        Ok((
            "meridian.v1.ListRegistryPacksReply".into(),
            meridian_domain::v1::ListRegistryPacksReply {
                packs,
                next_cursor: String::new(),
                catalogue_pulled_at_ns: 1_792_200_000_000_000_000,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::registry::packs::READ_PACK, |envelope| {
        let asked = meridian_domain::v1::ReadPackRequest::decode(&envelope.payload[..]).unwrap();
        match served_pack_status(&asked.pack) {
            Some(status) => Ok(("meridian.v1.PackStatus".into(), status.encode_to_vec())),
            None => Err(format!(
                "no starter pack {} is in the catalogue this deployment holds",
                asked.pack
            )),
        }
    });
    bus.serve(crate::registry::packs::INSTALL_PACK, |envelope| {
        let asked = meridian_domain::v1::InstallPackRequest::decode(&envelope.payload[..]).unwrap();
        Ok((
            "meridian.v1.PackInstall".into(),
            meridian_domain::v1::PackInstall {
                pack: asked.pack,
                revision: asked.revision,
                members: asked
                    .members
                    .iter()
                    .map(|listing| meridian_domain::v1::PackMemberStatus {
                        listing: listing.clone(),
                        state: meridian_domain::v1::PackMemberState::Installing as i32,
                        ..Default::default()
                    })
                    .collect(),
            }
            .encode_to_vec(),
        ))
    });
    for topic in [
        crate::registry::packs::UPDATE_PACK,
        crate::registry::packs::REMOVE_PACK,
    ] {
        bus.serve(topic, |_| {
            Ok((
                "meridian.v1.PackStatus".into(),
                served_pack_status("open-meridian/crypto")
                    .unwrap_or_default()
                    .encode_to_vec(),
            ))
        });
    }
    // Contract v28 (W6.37, W3.31): the series request rule, at its default,
    // and a request for series the rule refuses.
    bus.serve(crate::admin::series_rule::READ_SERIES_REQUEST_RULE, |_| {
        Ok((
            "meridian.v1.SeriesRequestRule".into(),
            meridian_domain::v1::SeriesRequestRule {
                orders: true,
                standing_wants: false,
                daily_limit: 500,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(
        crate::admin::series_rule::REQUEST_SERIES_RECORDS,
        |envelope| {
            let asked =
                meridian_domain::v1::RequestSeriesRecordsRequest::decode(&envelope.payload[..])
                    .map_err(|e| e.to_string())?;
            Ok((
                "meridian.v1.RequestSeriesRecordsReply".into(),
                meridian_domain::v1::RequestSeriesRecordsReply {
                    answers: asked
                        .series
                        .iter()
                        .map(|want| meridian_domain::v1::SeriesAnswer {
                            series_key: want.series_key.clone(),
                            refused_by_rule: want.cause
                                == meridian_domain::v1::SeriesRequestCause::Standing as i32,
                            pending_review: want.cause
                                != meridian_domain::v1::SeriesRequestCause::Standing as i32,
                            ..Default::default()
                        })
                        .collect(),
                }
                .encode_to_vec(),
            ))
        },
    );
    // Contract v28 (W10.9): the Age tab's rows, as reports and core's hops
    // would leave them in the monitor.
    {
        let now = meridian_clock::SystemClock.now_ns();
        for (n, dataset) in ["alpaca-1:quotes", "tradier-1:options", "coinbase-1:trades"]
            .iter()
            .enumerate()
        {
            // A plugin's edge, under its instance; the lake's own hop, under
            // the lake (ruling 606).
            for (instance, hop) in [
                (
                    dataset.split(':').next().unwrap_or_default(),
                    "AGE_HOP_EDGE",
                ),
                ("lake-1", "AGE_HOP_LAKE"),
            ] {
                let mut ages = meridian_domain::latency::Histogram::default();
                for k in 1..=40_i64 {
                    ages.record((n as i64 + 1) * k * 250_000);
                }
                meridian_domain::latency::monitor().merge(
                    meridian_domain::latency::Key {
                        instance: instance.to_string(),
                        scope: meridian_domain::latency::Scope::Dataset {
                            dataset: dataset.to_string(),
                            hop: hop.to_string(),
                        },
                    },
                    now,
                    &ages,
                    meridian_domain::latency::Said::default(),
                );
            }
        }
        // Contract v28 (W4.16; rulings 427 and 606): the instance's session
        // over its ceiling for three minutes, an episode open on its
        // Summary, and its notices, opened and an earlier one closed, in the
        // Inbox.
        let minute = 60_000_000_000_i64;
        let start = (now / minute - 4) * minute;
        for (i, ms) in [40_i64, 2_300, 2_400, 2_350].iter().enumerate() {
            let mut rtt = meridian_domain::latency::Histogram::default();
            for _ in 0..12 {
                rtt.record(ms * 1_000_000);
            }
            meridian_domain::latency::monitor().merge(
                meridian_domain::latency::Key {
                    instance: INSTANCE.into(),
                    scope: meridian_domain::latency::Scope::Session {
                        session: "snaptrade-api".into(),
                        metric: "LATENCY_METRIC_RTT".into(),
                    },
                },
                start + i as i64 * minute + 1,
                &rtt,
                meridian_domain::latency::Said {
                    ceiling_ns: 250_000_000,
                    alive_at_ns: now,
                    ..Default::default()
                },
            );
        }
        let judged = meridian_domain::latency::monitor().judge(now);
        let store = Arc::new(crate::latency::notices::InMemory::default());
        let mut notices: Vec<crate::latency::notices::Notice> = judged
            .iter()
            .enumerate()
            .map(|(n, t)| crate::latency::notices::Notice {
                notice_id: format!("01JLAT{n:020}"),
                subject: ADMIN.into(),
                plugin_instance_id: t.key.instance.clone(),
                kind: t.kind.into(),
                about: crate::latency::notices::about(t),
                changed_ns: now,
            })
            .collect();
        notices.push(crate::latency::notices::Notice {
            notice_id: "01JLAT0000000000000000000".into(),
            subject: ADMIN.into(),
            plugin_instance_id: "lake-1".into(),
            kind: "latency_recovered".into(),
            about: "alpaca-1:quotes at lake: p99 3.2 ms".into(),
            changed_ns: now - 3_600_000_000_000,
        });
        crate::latency::notices::NoticeStore::notify(store.as_ref(), &notices).unwrap();
        crate::latency::notices::keep_in(store);
    }
    bus.serve(crate::registry::LIST_PLUGIN_USAGE_REPORTS, |_| {
        let count =
            |listing: &str, version: &str, registry: &str| meridian_domain::v1::PluginUsageCount {
                listing: listing.into(),
                version: version.into(),
                registry: registry.into(),
                instances: 1,
                people_read: 4,
                people_write: 2,
                people_admin: 1,
                daily_operations: vec![120, 98, 143, 110, 0, 0, 131],
                daily_requests: vec![40, 31, 52, 38, 0, 0, 44],
            };
        let reports = (0..3)
            .map(|week| meridian_domain::v1::SentPluginUsageReport {
                sent_at_ns: 1_792_200_000_000_000_000 - week * 604_800_000_000_000,
                received: week != 1,
                report: Some(meridian_domain::v1::PluginUsageReport {
                    period_start: ["2026-10-05", "2026-09-28", "2026-09-21"][week as usize].into(),
                    period_end: ["2026-10-11", "2026-10-04", "2026-09-27"][week as usize].into(),
                    counts: vec![
                        count("open-meridian/fx", "1.4.0", "community"),
                        count("acme/rates", "2.0.0", "verified"),
                    ],
                }),
            })
            .collect();
        Ok((
            "meridian.v1.ListPluginUsageReportsReply".into(),
            meridian_domain::v1::ListPluginUsageReportsReply {
                reports,
                next_cursor: String::new(),
                reports_plugin_usage: true,
                catalogue_pulled_at_ns: 1_792_200_000_000_000_000,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::documentation::READ, |_| {
        Ok((
            "meridian.v1.ReadPluginDocumentationReply".into(),
            meridian_domain::v1::ReadPluginDocumentationReply {
                declaration: Some(documented_declaration()),
                documentation: documentation_texts(),
                uploaded_by: ADMIN.into(),
                uploaded_at_ns: 1_790_380_800_000_000_000,
            }
            .encode_to_vec(),
        ))
    });

    let kit = crate::kit::Kit::at(
        std::env::var("MERIDIAN_UI_DIR").unwrap_or_else(|_| crate::kit::IN_IMAGE.into()),
    )
    .expect("the kit, which the browser's pages load");
    crate::html::use_kit(kit.base());
    if std::env::var("MERIDIAN_SETTINGS_DEVELOPMENT").is_ok_and(|v| v == "1") {
        crate::html::mark_development();
    }
    let port: u16 = std::env::var("MERIDIAN_SETTINGS_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8080);
    let plugins = crate::plugins::Plugins::new(
        &format!("http://settings-page:{port}"),
        "http://{instance}.sidecars.invalid:9",
        crate::signing::Signer::holding(
            "dashboard-test",
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
        ),
    )
    .unwrap()
    .resolving(
        &format!("{INSTANCE}.sidecars.invalid"),
        SocketAddr::from(([127, 0, 0, 1], 9)),
    );
    let cache = Arc::new(RecordsCache::default());
    cache.store(held.lock().unwrap().clone(), clock.now_ns());
    let sessions = Arc::new(Sessions::default());
    let session = sessions.start(ADMIN, "Ada Admin", vec![], clock.now_ns());
    // SnapTrade's report, heard afresh every half minute as its sidecar
    // sends it, so its Summary is never stale.
    let health: Arc<crate::health::Health> = Arc::default();
    health.hear(INSTANCE, report(clock.now_ns()));
    {
        let health = Arc::clone(&health);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                health.hear(INSTANCE, report(meridian_clock::SystemClock.now_ns()));
            }
        });
    }
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: Arc::clone(&cache),
        sessions,
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(clock),
        bus: Arc::clone(&bus),
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: false,
        plugins: Some(Arc::new(plugins)),
        registry: None,
        custody: Arc::default(),
        health,
        kit: Some(Arc::new(kit)),
        bounds: Arc::default(),
        tickets: Arc::default(),
        prepared: Arc::default(),
        service_accounts: Arc::new(service_accounts()),
    });
    tokio::spawn(crate::records::refresh_forever(
        bus,
        cache,
        Arc::new(meridian_clock::SystemClock),
    ));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("the port");
    println!("SESSION={session}");
    // Contract v28 (W10.1): Ines's own, whom the professional test asks.
    println!(
        "READER_SESSION={}",
        app.sessions.start(
            READER,
            "Ines Reader",
            vec![],
            meridian_clock::SystemClock.now_ns()
        )
    );
    println!("serving the settings pages on port {port}");
    axum::serve(listener, router(app)).await.unwrap();
}

/// One service account already on the Service accounts tab (contract v21),
/// so each pass reads a row: the admin's, set to act, last refused once.
fn service_accounts() -> crate::service_accounts::ServiceAccounts {
    use crate::service_accounts::store::{
        InMemory, ServiceAccount, ServiceAccountStore as _, Setting, State,
    };
    let store = InMemory::default();
    store
        .put(
            &ServiceAccount {
                service_account_id: "SA-01JAD000000000000000000001".into(),
                name: "Reconciliation sweep".into(),
                purpose: "Closes breaks whose difference has gone at the next statement".into(),
                owner_subject: ADMIN.into(),
                setting: Setting::Act,
                state: State::Active,
                created_at_ns: 1_790_380_800_000_000_000,
                last_used_ns: Some(1_790_384_400_000_000_000),
                last_refusal: "ops-1__close_books: at may not for Reconciliation sweep".into(),
                last_refusal_ns: Some(1_790_384_400_000_000_000),
            },
            &[],
            &[],
        )
        .expect("kept");
    crate::service_accounts::ServiceAccounts::keeping(std::sync::Arc::new(store))
}

/// The migration's lines (contract v24, W3.26): a dozen exact ones, two
/// records answered by one INS- ID (a duplicate line) and one held back.
/// How the stand-in draws a record: its number, description, template and
/// what it lacks.
type DrawRecord<'a> = &'a dyn Fn(usize, &str, &str, &[&str]) -> InstrumentRecord;

fn migration_lines(record: DrawRecord<'_>) -> Vec<InstrumentToComplete> {
    use meridian_domain::v1::{MigrationSection, ProvisionalReason};
    let line = |n: usize, description: &str, section: MigrationSection, answered: String| {
        InstrumentToComplete {
            instrument: Some(record(n, description, "TPL-equity", &[])),
            provisional_reason: if section == MigrationSection::HeldBack {
                ProvisionalReason::HeldBack
            } else {
                ProvisionalReason::Incomplete
            } as i32,
            migration_section: section as i32,
            answered_instrument_id: answered,
            ..Default::default()
        }
    };
    let mut lines: Vec<InstrumentToComplete> = (0..12)
        .map(|k| {
            line(
                10 + k,
                &format!("Kept since v10, line {}", k + 1),
                if k % 3 == 0 {
                    MigrationSection::Minted
                } else {
                    MigrationSection::Matched
                },
                format!("INS-01JB2N7QEXACT00000000{k:04}"),
            )
        })
        .collect();
    lines.push(line(
        30,
        "US dollar, a v20 statement",
        MigrationSection::AlreadyHeld,
        "INS-01JUSD0000000000000000000001".into(),
    ));
    lines.push(line(
        31,
        "USD cash, another custodian",
        MigrationSection::Matched,
        "INS-01JUSD0000000000000000000001".into(),
    ));
    lines.push(line(
        32,
        "Grayscale Bitcoin Trust",
        MigrationSection::HeldBack,
        "INS-01JB2N7QGBTC000000000000001".into(),
    ));
    lines
}

/// The provisional records the stand-in instrument store lists (contract
/// v24, W3.11): one per reason, with the migration's counts.
fn provisional_listed() -> ListInstrumentsToCompleteReply {
    use meridian_domain::v1::{InstrumentValueSource, MigrationCounts, ProvisionalReason};
    let at = 1_791_331_200_000_000_000;
    let record = |n: usize, description: &str, template: &str, missing: &[&str]| InstrumentRecord {
        instrument_id: format!("LCL-01JB2N7Q00000000000000{n:04}"),
        description: description.into(),
        provisional: true,
        template_id: template.into(),
        template_version: if template.is_empty() {
            String::new()
        } else {
            "1".into()
        },
        missing_fields: missing.iter().map(|m| m.to_string()).collect(),
        sources: vec![InstrumentValueSource {
            recorded_at_ns: at - (n as i64) * 86_400_000_000_000,
            ..Default::default()
        }],
        version: 1,
        ..Default::default()
    };
    let listed = |record: InstrumentRecord, reason: ProvisionalReason| InstrumentToComplete {
        instrument: Some(record),
        provisional_reason: reason as i32,
        ..Default::default()
    };
    ListInstrumentsToCompleteReply {
        instruments: vec![
            listed(
                record(
                    1,
                    "Municipal bond, Commonwealth of Massachusetts 5% 2041",
                    "TPL-debt-base",
                    &["identifiers"],
                ),
                ProvisionalReason::Incomplete,
            ),
            listed(
                record(2, "ZZ Top Holdings Inc. common stock", "TPL-equity", &[]),
                ProvisionalReason::AwaitingStaff,
            ),
            listed(
                record(3, "A position with no class stated", "", &[]),
                ProvisionalReason::ClassNeeded,
            ),
            listed(
                record(
                    4,
                    "Sweep fund",
                    "TPL-money-market-fund",
                    &["money_market_fund.nav"],
                ),
                ProvisionalReason::Unreachable,
            ),
        ]
        .into_iter()
        .chain(migration_lines(&record))
        .collect(),
        migration_counts: Some(MigrationCounts {
            already_held: 3,
            matched: 118,
            minted: 9,
            near_match: 1,
            awaiting_staff: 2,
            kept_local: 14,
            unreachable: 0,
            held_back: 1,
            remaining: 0,
        }),
        ..Default::default()
    }
}

/// SnapTrade's declaration as its v25 upload carries it.
fn documented_declaration() -> meridian_pb::v1::PluginDeclaration {
    meridian_pb::v1::PluginDeclaration {
        secret_settings: vec!["consumer_key".into()],
        settings: declared(),
        interface: Some(meridian_pb::v1::InterfaceDeclaration {
            loopback_port: 8000,
            title: "SnapTrade".into(),
            pages: vec![
                meridian_pb::v1::PageDeclaration {
                    path: "/admin/connections".into(),
                    title: "Connections".into(),
                    levels: vec![meridian_pb::v1::AccessLevel::Admin as i32],
                    roles: vec!["custody".into()],
                },
                meridian_pb::v1::PageDeclaration {
                    path: "/statements".into(),
                    title: "Statements".into(),
                    levels: vec![
                        meridian_pb::v1::AccessLevel::Write as i32,
                        meridian_pb::v1::AccessLevel::Read as i32,
                    ],
                    roles: vec!["custody".into()],
                },
            ],
        }),
        tools: (0..9)
            .map(|n| meridian_pb::v1::ToolDeclaration {
                name: format!("read_connection_{n}"),
                title: format!("Read connection {n}"),
                description: "Each brokerage connection, its state and when it last read.".into(),
                reads: n % 3 != 0,
                levels: vec![meridian_pb::v1::AccessLevel::Admin as i32],
                roles: vec!["custody".into()],
                ..Default::default()
            })
            .collect(),
        reads_external_accounts: true,
        storage: Some(meridian_pb::v1::StorageDeclaration {
            retention_days: 2555,
            record_kinds: vec![meridian_pb::v1::RawRecordKind {
                name: "activity".into(),
                label: "Reported activity".into(),
                window_days: 2555,
                archivable: true,
            }],
        }),
        ..Default::default()
    }
}

/// SnapTrade's sections, the draft's shape and length.
fn documentation_texts() -> Vec<meridian_domain::v1::DocumentationText> {
    let text = |section: &str, markdown: &str| meridian_domain::v1::DocumentationText {
        section: section.into(),
        markdown: markdown.into(),
    };
    vec![
        text(
            "overview",
            "# SnapTrade\n\nReads the holdings, balances and activity of brokerage accounts \
             through [SnapTrade](https://snaptrade.com/), for people who keep their investments \
             at several brokers.\n\n## What it reads\n\n- Each connection a person makes \
             through SnapTrade's portal\n- The positions, cash and activity of each account it \
             reaches\n- **Never** a password: SnapTrade holds the broker sign-in\n\n## What \
             it records\n\nA statement per account per read, as the custody role records it, \
             and its raw responses for 30 days. See [Setup](setup.md#before-you-start).",
        ),
        text(
            "setup",
            "## Before you start\n\nYou need a SnapTrade client ID and consumer key, from \
             [the dashboard](https://dashboard.snaptrade.com/).\n\n## Enter the keys\n\n1. \
             Open **Settings**.\n2. Enter the client ID and the consumer key.\n3. Save.\n\n\
             | Setting | Where it comes from |\n|---|---|\n| Client ID | SnapTrade's dashboard |\n\
             | Consumer key | SnapTrade's dashboard, shown once |\n\n## Link the accounts\n\n\
             Each account SnapTrade reaches is listed on **Account links**; link each to one of \
             this deployment's accounts.",
        ),
        text(
            "troubleshooting",
            "## A connection needs sign-in\n\nThe broker asked the person to sign in again. \
             Open **Connections** and choose *Reconnect*.\n\n## Nothing arrives\n\nCheck the \
             keys on **Settings**; a wrong consumer key reads as `401` in the health line.",
        ),
        text(
            "terms",
            "SnapTrade's [terms](https://snaptrade.com/terms) and \
             [privacy policy](https://snaptrade.com/privacy) apply to the data it reads. The \
             firm carries its own due diligence on SnapTrade as a vendor.",
        ),
        text(
            "pages",
            "- **Connections**: each connection and its state.\n- **Account links**: link each \
             account it reaches.\n- **Statements**: what it recorded, per account.",
        ),
    ]
}

/// Contract v25: the versions installed from a registry -- one installed
/// online from the community registry, one carried in by a bundle from the
/// verified one, its key status's age shown.
fn registry_versions() -> Vec<meridian_domain::v1::PluginVersion> {
    let version = |name: &str, v: &str, registry: &str, key_status_issued_at_ns: i64| {
        meridian_domain::v1::PluginVersion {
            metadata: Some(meridian_domain::v1::PluginMetadata {
                name: name.into(),
                version: v.into(),
                roles: vec!["custody".into()],
                declaration: Some(documented_declaration()),
                ..Default::default()
            }),
            image_digest: format!("sha256:{}", "ab".repeat(32)),
            uploaded_by: ADMIN.into(),
            uploaded_at_ns: 1_792_100_000_000_000_000,
            source_registry: registry.into(),
            listing: name.into(),
            key_status_issued_at_ns,
            ..Default::default()
        }
    };
    vec![
        version("open-meridian/fx", "1.4.0", "community", 0),
        version("acme/rates", "2.0.0", "verified", 1_792_000_000_000_000_000),
    ]
}

/// The starter packs' members as the catalogue's copy holds them (v28):
/// each a listing of one version, its roles and its datasets' terms.
fn pack_listings() -> Vec<meridian_domain::v1::PluginListing> {
    use meridian_pb::v1::{AccessBasis, Catalogue, DatasetDeclaration, DatasetLicence};
    let member = |listing: &str, title: &str, version: &str, roles: &[&str], basis: AccessBasis| {
        let mut declaration = documented_declaration();
        declaration.interface = Some(meridian_pb::v1::InterfaceDeclaration {
            title: title.into(),
            ..Default::default()
        });
        if basis != AccessBasis::Unspecified {
            declaration.catalogue = Some(Catalogue {
                datasets: vec![DatasetDeclaration {
                    key: "daily".into(),
                    licence_default: Some(DatasetLicence {
                        access_basis: basis as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            });
        }
        meridian_domain::v1::PluginListing {
            listing: listing.into(),
            versions: vec![meridian_domain::v1::ListedVersion {
                listing: listing.into(),
                version: version.into(),
                registry: "community".into(),
                metadata: Some(meridian_domain::v1::PluginMetadata {
                    name: listing.into(),
                    version: version.into(),
                    roles: roles.iter().map(|r| r.to_string()).collect(),
                    declaration: Some(declaration),
                    ..Default::default()
                }),
                schema_version: "v27".into(),
                ..Default::default()
            }],
            publisher: "Open Meridian".into(),
            ..Default::default()
        }
    };
    PACK_MEMBERS
        .iter()
        .map(|(listing, title, version, roles, _, _)| {
            let basis = if roles.contains(&"data") {
                if listing.ends_with("tiingo") || listing.ends_with("truefx") {
                    AccessBasis::Licensed
                } else {
                    AccessBasis::PublicDomain
                }
            } else {
                AccessBasis::Unspecified
            };
            member(listing, title, version, roles, basis)
        })
        .collect()
}

/// Each member: listing, title, version, roles, group, selection.
#[allow(clippy::type_complexity)]
const PACK_MEMBERS: [(&str, &str, &str, &[&str], &str, i32); 16] = [
    (
        "open-meridian/tiingo",
        "Tiingo",
        "0.6.2",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/us-treasury",
        "US Treasury",
        "0.2.0",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/ny-fed",
        "New York Fed",
        "0.1.0",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/plugin-ecb",
        "European Central Bank",
        "0.3.1",
        &["data"],
        "Data",
        2,
    ),
    ("open-meridian/fred", "FRED", "0.1.0", &["data"], "Data", 2),
    (
        "open-meridian/fed-board",
        "Federal Reserve Board",
        "0.2.0",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/truefx",
        "TrueFX",
        "0.4.0",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/sec-edgar",
        "SEC EDGAR",
        "0.1.0",
        &["data"],
        "Data",
        2,
    ),
    (
        "open-meridian/alpaca",
        "Alpaca",
        "0.9.0",
        &["custody", "connectivity"],
        "Brokers",
        3,
    ),
    (
        "open-meridian/oanda",
        "OANDA",
        "0.8.1",
        &["custody", "connectivity"],
        "Brokers",
        3,
    ),
    (
        "open-meridian/snaptrade",
        "SnapTrade",
        "0.11.1",
        &["custody"],
        "Brokers",
        3,
    ),
    (
        "open-meridian/tradier",
        "Tradier",
        "0.5.0",
        &["custody", "connectivity"],
        "Brokers",
        3,
    ),
    (
        "open-meridian/coinbase",
        "Coinbase",
        "0.7.1",
        &["custody", "connectivity"],
        "Venues",
        3,
    ),
    (
        "open-meridian/kalshi",
        "Kalshi",
        "0.4.0",
        &["custody", "connectivity"],
        "Venues",
        3,
    ),
    (
        "open-meridian/kraken",
        "Kraken",
        "0.6.0",
        &["custody", "connectivity"],
        "Venues",
        3,
    ),
    (
        "open-meridian/polymarket-us",
        "Polymarket US",
        "0.2.0",
        &["custody", "connectivity"],
        "Venues",
        3,
    ),
];

/// The two packs the stand-in's catalogue carries.
fn served_packs() -> Vec<meridian_domain::v1::PluginPack> {
    let member = |listing: &str| {
        let (_, _, _, _, group, selection) = PACK_MEMBERS
            .iter()
            .find(|m| m.0 == listing)
            .copied()
            .expect("a member");
        meridian_domain::v1::PackMember {
            listing: listing.into(),
            registry: "community".into(),
            selection,
            group: group.into(),
            minimum_version: "0.1.0".into(),
        }
    };
    let pack = |name: &str,
                revision: u64,
                title: &str,
                about: &str,
                classes: &[&str],
                members: Vec<meridian_domain::v1::PackMember>| {
        meridian_domain::v1::PluginPack {
            pack: name.into(),
            curator: "Open Meridian".into(),
            revision: Some(meridian_domain::v1::PackRevision {
                revision,
                title: title.into(),
                description: about.into(),
                asset_classes: classes.iter().map(|c| c.to_string()).collect(),
                members,
                note: "Reviewed.".into(),
                published_at_ns: 1_792_100_000_000_000_000,
            }),
            withdrawn_at_ns: 0,
            sequence: 212,
        }
    };
    vec![
        pack(
            "open-meridian/crypto",
            2,
            "Crypto",
            "Rates for discounting, and the venues where you hold crypto accounts.",
            &["crypto_asset", "cash"],
            [
                "open-meridian/us-treasury",
                "open-meridian/ny-fed",
                "open-meridian/fred",
                "open-meridian/coinbase",
                "open-meridian/kraken",
            ]
            .iter()
            .map(|m| member(m))
            .collect(),
        ),
        pack(
            "open-meridian/multi-asset",
            1,
            "Multi-asset",
            "Every launch pack's members: market data, rates, filings, brokers and venues.",
            &["equity", "cash", "crypto_asset", "event_contract"],
            PACK_MEMBERS.iter().map(|m| member(m.0)).collect(),
        ),
    ]
}

/// What the stand-in conductor answers for a pack's page.
fn served_pack_status(pack: &str) -> Option<meridian_domain::v1::PackStatus> {
    use meridian_domain::v1::{PackMemberState as S, PackMemberStatus};
    let held = served_packs().into_iter().find(|p| p.pack == pack)?;
    let version = |listing: &str| {
        PACK_MEMBERS
            .iter()
            .find(|m| m.0 == listing)
            .map(|m| m.2.to_string())
            .unwrap_or_default()
    };
    let status = |listing: &str, state: S| PackMemberStatus {
        listing: listing.into(),
        group: PACK_MEMBERS
            .iter()
            .find(|m| m.0 == listing)
            .map(|m| m.4.to_string())
            .unwrap_or_default(),
        latest_version: version(listing),
        state: state as i32,
        ..Default::default()
    };
    if pack == "open-meridian/crypto" {
        return Some(meridian_domain::v1::PackStatus {
            pack: pack.into(),
            installed_revision: 1,
            latest_revision: 2,
            withdrawn: false,
            members: vec![
                PackMemberStatus {
                    installed_version: "0.2.0".into(),
                    setup: "Launch to connect".into(),
                    ..status("open-meridian/us-treasury", S::Installed)
                },
                PackMemberStatus {
                    installed_version: "0.1.0".into(),
                    instance_ids: vec!["ny-fed-1".into()],
                    ..status("open-meridian/ny-fed", S::Launched)
                },
                PackMemberStatus {
                    added_by_revision: 2,
                    ..status("open-meridian/fred", S::NotInstalled)
                },
                PackMemberStatus {
                    installed_version: "0.7.0".into(),
                    instance_ids: vec![INSTANCE.into()],
                    reason: "Installed 0.7.0; 0.7.1 offered".into(),
                    ..status("open-meridian/coinbase", S::Launched)
                },
                PackMemberStatus {
                    reason: "verification failed: the provenance: none of the image's referrers is its provenance".into(),
                    ..status("open-meridian/kraken", S::Failed)
                },
            ],
            ..Default::default()
        });
    }
    Some(meridian_domain::v1::PackStatus {
        pack: pack.into(),
        installed_revision: 0,
        latest_revision: held.revision.as_ref().map_or(0, |r| r.revision),
        withdrawn: false,
        members: held
            .revision
            .unwrap_or_default()
            .members
            .iter()
            .map(|m| match m.listing.as_str() {
                "open-meridian/sec-edgar" => PackMemberStatus {
                    latest_version: String::new(),
                    reason: "none listed at or above 0.1.0".into(),
                    ..status(&m.listing, S::Unavailable)
                },
                "open-meridian/polymarket-us" => PackMemberStatus {
                    reason: "the floor: open-meridian/polymarket-us 0.2.0 declares v99, which this deployment's sidecar does not serve".into(),
                    ..status(&m.listing, S::NotPermitted)
                },
                "open-meridian/snaptrade" => PackMemberStatus {
                    installed_version: "0.11.1".into(),
                    setup: "Launch to connect".into(),
                    ..status(&m.listing, S::Installed)
                },
                _ => status(&m.listing, S::NotInstalled),
            })
            .collect(),
        ..Default::default()
    })
}

/// Contract v25: the catalogue's copy -- open-meridian/fx with its pulled
/// 1.4.0 and a 1.5.0 asking a role more, acme/rates verified, and enough
/// other listings that Browse pages.
fn registry_listings() -> Vec<meridian_domain::v1::PluginListing> {
    let listed = |listing: &str, version: &str, registry: &str, roles: &[&str]| {
        meridian_domain::v1::ListedVersion {
            listing: listing.into(),
            version: version.into(),
            registry: registry.into(),
            image_digest: format!("sha256:{}", "cd".repeat(32)),
            metadata: Some(meridian_domain::v1::PluginMetadata {
                name: listing.rsplit('/').next().unwrap_or_default().into(),
                version: version.into(),
                roles: roles.iter().map(|r| r.to_string()).collect(),
                declaration: Some(documented_declaration()),
                ..Default::default()
            }),
            approved_roles: if registry == "verified" {
                roles.iter().map(|r| r.to_string()).collect()
            } else {
                Vec::new()
            },
            schema_version: "v25".into(),
            build: Some(meridian_domain::v1::PluginBuild {
                repository: format!("https://github.com/{listing}"),
                commit: "c".repeat(40),
                tag: format!("v{version}"),
                signer_fingerprint: "SHA256:release-key".into(),
                ..Default::default()
            }),
            listed_at_ns: 1_792_000_000_000_000_000,
            ..Default::default()
        }
    };
    let listing = |name: &str, versions: Vec<meridian_domain::v1::ListedVersion>, stars: u64| {
        meridian_domain::v1::PluginListing {
            listing: name.into(),
            repository: format!("https://github.com/{name}"),
            versions,
            regions: vec!["US".into()],
            asset_classes: vec!["fx".into()],
            support_contact: "support@example.com".into(),
            stars,
            installs: stars * 3,
            instances: stars * 4,
            counts_shown: stars >= 3,
            reviewed: false,
            terms_url: "https://open-meridian.com/terms#plugins-and-registries".into(),
            publisher: "Open Meridian".into(),
            ..Default::default()
        }
    };
    let mut pulled = listed("open-meridian/fx", "1.4.0", "community", &["custody"]);
    pulled.pull = Some(meridian_domain::v1::PluginPull {
        listing: "open-meridian/fx".into(),
        version: "1.4.0".into(),
        kind: meridian_domain::v1::PullReasonKind::TermsBreach as i32,
        reason: "It sent account numbers to a host it did not declare.".into(),
        pulled_at_ns: 1_792_150_000_000_000_000,
        ..Default::default()
    });
    let mut listings = vec![
        listing(
            "open-meridian/fx",
            vec![
                listed("open-meridian/fx", "1.3.0", "community", &["custody"]),
                pulled,
                listed(
                    "open-meridian/fx",
                    "1.5.0",
                    "community",
                    &["custody", "operations"],
                ),
            ],
            12,
        ),
        listing(
            "acme/rates",
            vec![
                listed("acme/rates", "2.0.0", "verified", &["custody"]),
                listed("acme/rates", "2.1.0", "verified", &["custody"]),
            ],
            2,
        ),
    ];
    for n in 0..30 {
        let name = format!("vendor-{n:02}/plugin-{n:02}");
        listings.push(listing(
            &name,
            vec![listed(&name, "0.1.0", "community", &["custody"])],
            n,
        ));
    }
    listings
}
