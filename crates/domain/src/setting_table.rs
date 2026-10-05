//! A table setting (contract v14; W4.8, W6.11): rows of typed columns a
//! plugin declares, which an admin of the plugin enters in the dashboard's
//! Settings form, and which the plugin only reads.
//!
//! The value travels as JSON text: a list of rows, each an object of its
//! cells keyed by column name, every cell text, with [`CHANGED_BY`] and
//! [`CHANGED_AT`], which the conductor stamps on a row added or changed; an
//! unchanged row keeps its own. Here once, because the dashboard checks each
//! cell before it sends anything and the conductor checks every cell before
//! it stores anything, and the two must refuse alike, naming the same cell
//! (`plan_code_links[2].instrument`).
//!
//! What only one side can know is that side's: the dashboard checks an
//! instrument cell names a record the instrument store holds, and an
//! external account cell one the plugin reported; here, their shape.

use std::collections::{BTreeMap, BTreeSet};

use meridian_pb::v1::{SettingColumn, SettingColumnType, SettingDeclaration, SettingType};

/// Who added or last changed a row: the person the dashboard stamped.
pub const CHANGED_BY: &str = "changed_by";
/// When, RFC 3339 in UTC, by the deployment's clock.
pub const CHANGED_AT: &str = "changed_at";
/// The most rows any table holds, and a declaration's `most_rows` at most.
pub const MOST_ROWS: usize = 500;
/// The longest text cell.
pub const MOST_TEXT: usize = 500;
/// The longest identifier cell: an external account's or an instrument's.
pub const MOST_ID: usize = 200;
/// A decimal cell's most places and significant digits (decisions/023).
const MOST_PLACES: usize = 18;
const MOST_DIGITS: usize = 38;

/// One row's cells, by column name, without its stamps.
pub type Cells = BTreeMap<String, String>;

/// A row as held: its cells, and who changed it when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Cells,
    pub changed_by: String,
    pub changed_at: String,
}

/// A cell, a row or the table that does not read, by its path, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{}: {}", self.path, self.message)
    }
}

/// Whether a declaration is a table.
pub fn is_table(declaration: &SettingDeclaration) -> bool {
    declaration.r#type == SettingType::Table as i32
}

/// The most rows this table holds.
pub fn most_rows(declaration: &SettingDeclaration) -> usize {
    match usize::try_from(declaration.most_rows) {
        Ok(0) | Err(_) => MOST_ROWS,
        Ok(most) => most.min(MOST_ROWS),
    }
}

/// What a column's heading reads.
pub fn label(column: &SettingColumn) -> &str {
    if column.label.is_empty() {
        &column.name
    } else {
        &column.label
    }
}

/// Why a table's declaration cannot stand, or nothing: at registration, the
/// sidecar's refusal (W4.1). A declaration that is no table declares no
/// column.
pub fn declaration_refused(declaration: &SettingDeclaration) -> Option<String> {
    let name = &declaration.name;
    if !is_table(declaration) {
        return (!declaration.columns.is_empty())
            .then(|| format!("setting {name} declares columns, and only a table has columns"));
    }
    if declaration.secret {
        return Some(format!(
            "setting {name} is a table, and a table is never secret"
        ));
    }
    if declaration.columns.is_empty() {
        return Some(format!("setting {name} is a table and declares no column"));
    }
    if declaration.most_rows < 0 || declaration.most_rows as usize > MOST_ROWS {
        return Some(format!(
            "setting {name}'s most_rows is {}; 0 to {MOST_ROWS}",
            declaration.most_rows
        ));
    }
    let mut seen = BTreeSet::new();
    for (i, column) in declaration.columns.iter().enumerate() {
        let at = format!("setting {name}'s columns[{i}]");
        if column.name.is_empty() {
            return Some(format!("{at} has no name"));
        }
        if column.name == CHANGED_BY || column.name == CHANGED_AT {
            return Some(format!(
                "{at} is named {}, which the conductor stamps on each row",
                column.name
            ));
        }
        if !seen.insert(column.name.as_str()) {
            return Some(format!("{at} is named {} twice", column.name));
        }
        if column_type(column) == SettingColumnType::Choice && column.choices.is_empty() {
            return Some(format!("{at} is a choice and declares no options"));
        }
    }
    None
}

fn column_type(column: &SettingColumn) -> SettingColumnType {
    SettingColumnType::try_from(column.r#type).unwrap_or(SettingColumnType::Unspecified)
}

/// The rows a table's value holds, as stored: each row's cells and stamps.
/// A value that does not read is said, never guessed at.
pub fn parse(value: &str) -> Result<Vec<Row>, String> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let listed: serde_json::Value =
        serde_json::from_str(value).map_err(|_| "the table is not JSON".to_string())?;
    let rows = listed
        .as_array()
        .ok_or_else(|| "the table is not a list of rows".to_string())?;
    rows.iter()
        .enumerate()
        .map(|(n, row)| {
            let object = row
                .as_object()
                .ok_or_else(|| format!("row {} is not an object of cells", n + 1))?;
            let mut cells = Cells::new();
            let (mut changed_by, mut changed_at) = (String::new(), String::new());
            for (key, value) in object {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("row {}'s {key} is not text", n + 1))?
                    .to_string();
                match key.as_str() {
                    CHANGED_BY => changed_by = text,
                    CHANGED_AT => changed_at = text,
                    _ => {
                        cells.insert(key.clone(), text);
                    }
                }
            }
            Ok(Row {
                cells,
                changed_by,
                changed_at,
            })
        })
        .collect()
}

/// The rows as JSON text, each with its stamps, in the order given.
pub fn written(rows: &[Row]) -> String {
    let listed: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut object = serde_json::Map::new();
            for (key, value) in &row.cells {
                object.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            object.insert(CHANGED_BY.into(), row.changed_by.clone().into());
            object.insert(CHANGED_AT.into(), row.changed_at.clone().into());
            serde_json::Value::Object(object)
        })
        .collect();
    serde_json::Value::Array(listed).to_string()
}

/// Cells only, as JSON text: what the dashboard sends, which the conductor
/// stamps.
pub fn cells_written(rows: &[Cells]) -> String {
    let listed: Vec<serde_json::Value> = rows
        .iter()
        .map(|cells| {
            serde_json::Value::Object(
                cells
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            )
        })
        .collect();
    serde_json::Value::Array(listed).to_string()
}

/// Every cell of `rows` checked against the declaration's columns: the rows
/// as they will be kept -- each cell trimmed and written as its type writes
/// it, a row wholly blank dropped -- or every cell that does not read, by
/// its path (`name[n].column`, `n` the row's place as given). A cell naming
/// no column, stamps included, is refused: only the conductor stamps.
pub fn checked(
    declaration: &SettingDeclaration,
    rows: &[Cells],
) -> Result<Vec<Cells>, Vec<Problem>> {
    let name = &declaration.name;
    let mut kept = Vec::new();
    let mut problems = Vec::new();
    for (n, given) in rows.iter().enumerate() {
        let at = format!("{name}[{n}]");
        for key in given.keys() {
            if !declaration.columns.iter().any(|column| &column.name == key) {
                problems.push(Problem {
                    path: format!("{at}.{key}"),
                    message: format!("{name} has no column {key}"),
                });
            }
        }
        if given.values().all(|value| value.trim().is_empty()) {
            continue;
        }
        let mut cells = Cells::new();
        for column in &declaration.columns {
            let path = format!("{at}.{}", column.name);
            let typed = given
                .get(&column.name)
                .map(|v| v.trim())
                .unwrap_or_default();
            if typed.is_empty() {
                if column.required {
                    problems.push(Problem {
                        path,
                        message: format!("give the {}", label(column)),
                    });
                }
                continue;
            }
            match cell(column, typed) {
                Ok(written) => {
                    cells.insert(column.name.clone(), written);
                }
                Err(message) => problems.push(Problem { path, message }),
            }
        }
        kept.push(cells);
    }
    let most = most_rows(declaration);
    if kept.len() > most {
        problems.push(Problem {
            path: name.clone(),
            message: format!("at most {most} rows"),
        });
    }
    if problems.is_empty() {
        Ok(kept)
    } else {
        Err(problems)
    }
}

/// One cell, written as its type writes it, or why it does not read. Never
/// repeats the value.
fn cell(column: &SettingColumn, typed: &str) -> Result<String, String> {
    match column_type(column) {
        SettingColumnType::Integer => typed
            .parse::<i64>()
            .map(|number| number.to_string())
            .map_err(|_| "a whole number".to_string()),
        SettingColumnType::Decimal => decimal(typed),
        SettingColumnType::Date => date(typed),
        SettingColumnType::Choice => {
            if column.choices.iter().any(|choice| choice.value == typed) {
                Ok(typed.to_string())
            } else {
                let options: Vec<&str> = column.choices.iter().map(|c| c.value.as_str()).collect();
                Err(format!("one of {}", options.join(", ")))
            }
        }
        SettingColumnType::ExternalAccount | SettingColumnType::Instrument => {
            if typed.chars().count() > MOST_ID || typed.chars().any(char::is_whitespace) {
                Err(format!(
                    "an identifier, at most {MOST_ID} characters, no spaces"
                ))
            } else {
                Ok(typed.to_string())
            }
        }
        SettingColumnType::Text | SettingColumnType::Unspecified => {
            if typed.chars().count() > MOST_TEXT {
                Err(format!("at most {MOST_TEXT} characters"))
            } else {
                Ok(typed.to_string())
            }
        }
    }
}

fn decimal(typed: &str) -> Result<String, String> {
    let said = "an exact decimal, such as 12.5, at most 18 places".to_string();
    let unsigned = typed.strip_prefix('-').unwrap_or(typed);
    let (whole, places) = match unsigned.split_once('.') {
        Some((whole, places)) => (whole, places),
        None => (unsigned, ""),
    };
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty()
        || !digits(whole)
        || !digits(places)
        || (unsigned.contains('.') && places.is_empty())
        || places.len() > MOST_PLACES
        || whole.trim_start_matches('0').len() + places.len() > MOST_DIGITS
    {
        return Err(said);
    }
    Ok(typed.to_string())
}

fn date(typed: &str) -> Result<String, String> {
    let said = Err("a date, YYYY-MM-DD".to_string());
    let parts: Vec<&str> = typed.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        return said;
    }
    let (year, month, day): (i64, u32, u32) = (
        parts[0].parse().unwrap_or(0),
        parts[1].parse().unwrap_or(0),
        parts[2].parse().unwrap_or(0),
    );
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return said,
    };
    if day == 0 || day > days {
        return said;
    }
    Ok(typed.to_string())
}

/// The rows to hold: each new row whose cells a row standing holds keeps
/// that row's stamps; any other is stamped with `by` and `at_ns`. A row the
/// update repeats is matched once, so two rows of the same cells stay two.
pub fn stamped(new: &[Cells], standing: &[Row], by: &str, at_ns: i64) -> Vec<Row> {
    let mut left: Vec<&Row> = standing.iter().collect();
    new.iter()
        .map(
            |cells| match left.iter().position(|row| &row.cells == cells) {
                Some(at) => left.remove(at).clone(),
                None => Row {
                    cells: cells.clone(),
                    changed_by: by.to_string(),
                    changed_at: rfc3339(at_ns),
                },
            },
        )
        .collect()
}

/// A moment as RFC 3339 in UTC, to the microsecond, by integer arithmetic on
/// the days since 1970 (Howard Hinnant's civil-from-days).
pub fn rfc3339(at_ns: i64) -> String {
    let seconds = at_ns.div_euclid(1_000_000_000);
    let micros = at_ns.rem_euclid(1_000_000_000) / 1_000;
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_pb::v1::SettingChoice;

    fn column(name: &str, kind: SettingColumnType, required: bool) -> SettingColumn {
        SettingColumn {
            name: name.into(),
            r#type: kind as i32,
            required,
            ..Default::default()
        }
    }

    fn plan_codes() -> SettingDeclaration {
        SettingDeclaration {
            name: "plan_code_links".into(),
            r#type: SettingType::Table as i32,
            most_rows: 2,
            columns: vec![
                column("account", SettingColumnType::ExternalAccount, true),
                column("code", SettingColumnType::Text, true),
                column("instrument", SettingColumnType::Instrument, true),
                column("weight", SettingColumnType::Decimal, false),
                column("from", SettingColumnType::Date, false),
                SettingColumn {
                    choices: vec![SettingChoice {
                        value: "a".into(),
                        ..Default::default()
                    }],
                    ..column("kind", SettingColumnType::Choice, false)
                },
            ],
            ..Default::default()
        }
    }

    fn cells(pairs: &[(&str, &str)]) -> Cells {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn each_cell_is_checked_and_named_by_its_path() {
        let given = vec![
            cells(&[
                ("account", " st-1 "),
                ("code", "OQKR"),
                ("instrument", "INS-7"),
            ]),
            cells(&[("account", ""), ("code", ""), ("instrument", "")]),
            cells(&[
                ("account", "st-2"),
                ("code", ""),
                ("instrument", "has space"),
                ("weight", "1.2.3"),
                ("from", "2026-02-30"),
                ("kind", "b"),
                ("changed_by", "me"),
            ]),
        ];
        let problems = checked(&plan_codes(), &given).unwrap_err();
        let paths: Vec<&str> = problems.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "plan_code_links[2].changed_by",
                "plan_code_links[2].code",
                "plan_code_links[2].instrument",
                "plan_code_links[2].weight",
                "plan_code_links[2].from",
                "plan_code_links[2].kind",
            ]
        );
        let kept = checked(&plan_codes(), &given[..2]).unwrap();
        assert_eq!(
            kept,
            [cells(&[
                ("account", "st-1"),
                ("code", "OQKR"),
                ("instrument", "INS-7")
            ])]
        );
        let three = vec![given[0].clone(), given[0].clone(), given[0].clone()];
        let too_many = checked(&plan_codes(), &three).unwrap_err();
        assert_eq!(too_many[0].path, "plan_code_links");
    }

    #[test]
    fn an_unchanged_row_keeps_its_stamps_and_any_other_is_stamped() {
        let standing = vec![Row {
            cells: cells(&[("code", "OQKR")]),
            changed_by: "local|ben".into(),
            changed_at: "2026-10-01T00:00:00.000000Z".into(),
        }];
        let held = stamped(
            &[cells(&[("code", "NEW")]), cells(&[("code", "OQKR")])],
            &standing,
            "local|ada",
            1_790_380_800_000_000_000,
        );
        assert_eq!(held[0].changed_by, "local|ada");
        assert_eq!(held[0].changed_at, "2026-09-26T00:00:00.000000Z");
        assert_eq!(held[1], standing[0]);
        assert_eq!(
            parse(&written(&held)).unwrap(),
            held,
            "written and read back whole"
        );
    }

    #[test]
    fn a_table_declaration_is_held_to_its_shape() {
        assert_eq!(declaration_refused(&plan_codes()), None);
        let mut bad = plan_codes();
        bad.columns
            .push(column("changed_at", SettingColumnType::Text, false));
        assert!(declaration_refused(&bad).unwrap().contains("stamps"));
        let mut bad = plan_codes();
        bad.columns.clear();
        assert!(declaration_refused(&bad).unwrap().contains("no column"));
        let mut bad = plan_codes();
        bad.secret = true;
        assert!(declaration_refused(&bad).unwrap().contains("never secret"));
        let mut bad = plan_codes();
        bad.columns[5].choices.clear();
        assert!(declaration_refused(&bad).unwrap().contains("no options"));
    }
}
