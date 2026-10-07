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
            "Keep activity records for",
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
    ];
    for n in 0..20 {
        records.known_plugins.push(known(
            &format!("plugin-{n:02}"),
            &[["custody", "operations", "dgm", "reporting"][n % 4]],
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
                // Kept as written: ops-1 was relaunched without ccm.
                entry(TWO_ROLES, "ccm", AccessLevel::Admin),
            ],
            built_in: false,
        },
    ];
    // Its settings record, as the conductor keeps one for every known plugin.
    records.plugin_settings.push(PluginSettingsRecord {
        plugin_instance_id: TWO_ROLES.into(),
        ..Default::default()
    });
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

fn records() -> AccessRecords {
    let mut records = settings_records();
    per_role(&mut records);
    // The holds a deployment admin set (contract v16, W6.25).
    records.holds = vec![
        meridian_domain::v1::Hold {
            role: String::new(),
            days: 400,
            write_once: false,
            updated_by: ADMIN.into(),
            updated_at_ns: 1_791_331_200_000_000_000,
        },
        meridian_domain::v1::Hold {
            role: "custody".into(),
            days: 2190,
            write_once: false,
            updated_by: ADMIN.into(),
            updated_at_ns: 1_791_417_600_000_000_000,
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
        ],
        declaration: Some(PluginDeclaration {
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
        }),
        stored: vec![
            StoredSpan {
                record_kind: "activity".into(),
                record_count: 48_210,
                first_received_ns: 1_554_076_800_000_000_000,
                last_received_ns: now - 600_000_000_000,
            },
            StoredSpan {
                record_kind: "responses".into(),
                record_count: 1_260,
                first_received_ns: now - 30 * 86_400_000_000_000,
                last_received_ns: now - 600_000_000_000,
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
            secrets_set: vec!["client_id".into()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn instrument_id(n: usize) -> String {
    format!("INS-{n:04}")
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
    record.updated_by = by.into();
    record.updated_at_ns = now;
    Ok(())
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
    bus.serve(
        crate::admin::instruments::LIST_INSTRUMENTS_TO_COMPLETE,
        |envelope| {
            let asked = ListInstrumentsToCompleteRequest::decode(&envelope.payload[..])
                .map_err(|e| e.to_string())?;
            let instruments = (1..=250)
                .map(instrument_id)
                .filter(|id| asked.instrument_id.is_empty() || *id == asked.instrument_id)
                .map(|id| InstrumentToComplete {
                    instrument: Some(InstrumentRecord {
                        identifiers: vec![Identifier {
                            scheme: "symbol".into(),
                            value: format!("F{}", &id[4..]),
                            ..Default::default()
                        }],
                        description: format!("Fund {}", &id[4..]),
                        instrument_id: id,
                        ..Default::default()
                    }),
                    complete: true,
                    complete_for_book: true,
                    ..Default::default()
                })
                .collect();
            Ok((
                "".into(),
                ListInstrumentsToCompleteReply {
                    instruments,
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        },
    );

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
            };
            let mut records = held.lock().unwrap();
            records.holds.retain(|h| h.role != hold.role);
            if hold.days > 0 {
                records.holds.push(hold.clone());
            }
            Ok(("".into(), hold.encode_to_vec()))
        });
    }

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
    println!("serving the settings pages on port {port}");
    axum::serve(listener, router(app)).await.unwrap();
}
