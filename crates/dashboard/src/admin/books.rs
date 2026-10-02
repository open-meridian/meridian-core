//! Each account's attributes in the book of record, set for a deployment
//! admin (W9.13, W9.14; contract v8).
//!
//! The base currency and the default method of relieving lots, each set with
//! a reason, journalled by the book in the account's partition; and the date
//! its opening balance stands for, which only the book's own W9.1 sets. The
//! dashboard reads every account's attributes and no account's data (the
//! deployment dashboard's ruling 21): nothing here shows a position, a lot or
//! a break.

use std::time::Duration;

use meridian_domain::v1::{
    set_account_attribute_request, AccountAttributeReply, AccountAttributes, AccountRecord,
    ListAccountAttributesReply, ListAccountAttributesRequest, LotReliefMethod,
    SetAccountAttributeRequest,
};
use prost::Message;

use crate::html::escape;

pub const SET_ACCOUNT_ATTRIBUTE: &str = "platform.book.command.set-account-attribute";
pub const LIST_ACCOUNT_ATTRIBUTES: &str = "platform.book.query.list-account-attributes";

/// How long the page waits for the book: it is one section of the page, and
/// the rest is drawn without it.
const WAIT: Duration = Duration::from_secs(2);

/// The methods a sale may relieve lots by, as the form offers them.
const METHODS: [(LotReliefMethod, &str); 5] = [
    (LotReliefMethod::FirstInFirstOut, "First in, first out"),
    (LotReliefMethod::LastInFirstOut, "Last in, first out"),
    (LotReliefMethod::HighestCost, "Highest cost"),
    (LotReliefMethod::LowestCost, "Lowest cost"),
    (LotReliefMethod::AverageCost, "Average cost"),
];

/// What the book said, or that it did not answer.
pub enum Books {
    Read(Vec<AccountAttributes>),
    NotAnswering(String),
}

/// Every account's attributes, read as the dashboard, a component reading
/// for a deployment admin.
pub async fn read(bus: &meridian_bus::Bus) -> Books {
    let mut found = Vec::new();
    let mut cursor = String::new();
    loop {
        let asked = bus
            .call(
                LIST_ACCOUNT_ATTRIBUTES,
                "meridian.v1.ListAccountAttributesRequest",
                ListAccountAttributesRequest {
                    page_size: 500,
                    cursor: cursor.clone(),
                    ..Default::default()
                }
                .encode_to_vec(),
                None,
                Some(WAIT),
            )
            .await;
        let page = match asked {
            Ok((_, bytes)) => match ListAccountAttributesReply::decode(&bytes[..]) {
                Ok(page) => page,
                Err(failed) => return Books::NotAnswering(failed.to_string()),
            },
            Err(failed) => return Books::NotAnswering(failed.to_string()),
        };
        found.extend(page.attributes);
        if page.next_cursor.is_empty() {
            return Books::Read(found);
        }
        cursor = page.next_cursor;
    }
}

fn method_named(method: i32) -> &'static str {
    METHODS
        .iter()
        .find(|(held, _)| *held as i32 == method)
        .map(|(_, name)| *name)
        .unwrap_or("")
}

/// The Books tab: one row per open account, its attributes, and an Edit.
pub fn section(accounts: &[AccountRecord], books: &Books, token: &str) -> (String, String) {
    let body = match books {
        Books::NotAnswering(why) => format!(
            "<p class=\"refused\">The book of record is not answering: {}</p>",
            escape(why)
        ),
        Books::Read(held) => {
            let mut sorted: Vec<&AccountRecord> = accounts.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name).then(a.account_id.cmp(&b.account_id)));
            let mut rows = String::new();
            for account in sorted {
                let attributes = held.iter().find(|a| a.account_id == account.account_id);
                let currency = attributes
                    .map(|a| a.base_currency_code.clone())
                    .unwrap_or_default();
                let method = attributes.map(|a| a.lot_relief_default).unwrap_or_default();
                let since = attributes
                    .and_then(|a| a.opening_balance.as_ref())
                    .map(|opening| opening.as_of_date.clone())
                    .unwrap_or_default();
                let fill = serde_json::json!({ "fields": {
                    "account_id": account.account_id,
                    "base_currency_code": currency,
                    "lot_relief_default": method.to_string(),
                    "reason": "",
                } });
                rows.push_str(&format!(
                    "<tr data-id=\"{id}\" data-name=\"{name}\"><td><span class=\"name\">{name}</span>\
                     <span class=\"id\">{id}</span></td><td>{currency}</td><td>{method}</td>\
                     <td>{since}</td><td class=\"actions\"><button type=\"button\" \
                     data-dialog-open=\"book\" data-title=\"{title}\" data-fill=\"{fill}\">Edit</button></td></tr>",
                    id = escape(&account.account_id),
                    name = escape(&account.name),
                    currency = escape(&currency),
                    method = escape(method_named(method)),
                    since = if since.is_empty() {
                        "<span class=\"hint\">Not yet</span>".to_string()
                    } else {
                        escape(&since)
                    },
                    title = escape(&format!("{}'s book", account.name)),
                    fill = escape(&fill.to_string()),
                ));
            }
            if rows.is_empty() {
                "<p class=\"empty\">No accounts yet.</p>".to_string()
            } else {
                format!(
                    "<table class=\"list books\" id=\"books-table\"><thead><tr><th>Account</th>\
                     <th>Base currency</th><th>Lot relief</th><th>In the book since</th><th></th>\
                     </tr></thead><tbody>{rows}</tbody></table>"
                )
            }
        }
    };
    let methods: String = std::iter::once("<option value=\"0\">Not set</option>".to_string())
        .chain(METHODS.iter().map(|(method, name)| {
            format!(
                "<option value=\"{}\">{}</option>",
                *method as i32,
                escape(name)
            )
        }))
        .collect();
    let dialog = format!(
        "<dialog id=\"book\" aria-labelledby=\"book-title\"><form method=\"post\" \
         action=\"/admin/accounts/book#books\">{token}\
         <div class=\"dialog-head\"><h2 id=\"book-title\" data-title-new=\"Book\">Book</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"account_id\">\
         <label>Base currency <input name=\"base_currency_code\" maxlength=\"3\" \
         pattern=\"[A-Z]{{3}}\" placeholder=\"USD\"></label>\
         <label>Lot relief, when a sale names none <select name=\"lot_relief_default\">{methods}</select></label>\
         <label>Reason <input name=\"reason\" required></label></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Save\">Save</button></div></form></dialog>"
    );
    (body, dialog)
}

/// What the form asks the book to set: each attribute that differs from what
/// it holds, each its own act with the reason given (W9.13).
pub fn requests(
    account_id: &str,
    base_currency_code: &str,
    lot_relief_default: &str,
    reason: &str,
    held: Option<&AccountAttributes>,
) -> Result<Vec<SetAccountAttributeRequest>, String> {
    if account_id.is_empty() {
        return Err("the form names no account".into());
    }
    let method: i32 = lot_relief_default
        .parse()
        .map_err(|_| format!("{lot_relief_default:?} is no method of relieving lots"))?;
    let mut asked = Vec::new();
    let currency = base_currency_code.trim();
    if !currency.is_empty() && held.map(|a| a.base_currency_code.as_str()) != Some(currency) {
        asked.push(set_account_attribute_request::Attribute::BaseCurrencyCode(
            currency.to_string(),
        ));
    }
    if method != 0 && held.map(|a| a.lot_relief_default) != Some(method) {
        asked.push(set_account_attribute_request::Attribute::LotReliefDefault(
            method,
        ));
    }
    if asked.is_empty() {
        return Err("nothing changed: the book holds these already".into());
    }
    Ok(asked
        .into_iter()
        .map(|attribute| SetAccountAttributeRequest {
            account_id: account_id.to_string(),
            attribute: Some(attribute),
            reason: reason.to_string(),
        })
        .collect())
}

/// Send one, for the person signed in, who the book records as its actor.
pub async fn set(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: SetAccountAttributeRequest,
) -> Result<(), String> {
    let (_, bytes) = bus
        .call_for(
            SET_ACCOUNT_ATTRIBUTE,
            "meridian.v1.SetAccountAttributeRequest",
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            subject,
        )
        .await
        .map_err(|failed| match failed {
            meridian_bus::BusError::HandlerFailed { detail, .. } => {
                match meridian_bus::read_refusal(&detail) {
                    Some((_, words)) => words.to_string(),
                    None => detail,
                }
            }
            other => other.to_string(),
        })?;
    AccountAttributeReply::decode(&bytes[..])
        .map(|_| ())
        .map_err(|failed| format!("the book's answer did not read: {failed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(currency: &str, method: LotReliefMethod) -> AccountAttributes {
        AccountAttributes {
            account_id: "ACC-1".into(),
            base_currency_code: currency.into(),
            lot_relief_default: method as i32,
            ..Default::default()
        }
    }

    #[test]
    fn only_what_changed_is_asked_for_each_its_own_act() {
        let asked = requests(
            "ACC-1",
            "EUR",
            "1",
            "the fund reports in euros",
            Some(&held("USD", LotReliefMethod::FirstInFirstOut)),
        )
        .unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].attribute,
            Some(set_account_attribute_request::Attribute::BaseCurrencyCode(
                "EUR".into()
            ))
        );
        let both = requests("ACC-1", "USD", "2", "why", None).unwrap();
        assert_eq!(both.len(), 2);
        assert!(requests(
            "ACC-1",
            "USD",
            "1",
            "why",
            Some(&held("USD", LotReliefMethod::FirstInFirstOut))
        )
        .is_err());
        assert!(requests("", "USD", "0", "why", None).is_err());
    }

    #[test]
    fn a_book_not_answering_is_said_and_no_account_data_is_shown() {
        let accounts = vec![AccountRecord {
            account_id: "ACC-1".into(),
            name: "Main".into(),
            ..Default::default()
        }];
        let (body, _) = section(&accounts, &Books::NotAnswering("no handler".into()), "");
        assert!(body.contains("not answering"));
        let mut attributes = held("USD", LotReliefMethod::HighestCost);
        attributes.opening_balance = Some(meridian_domain::v1::OpeningBalance {
            as_of_date: "2026-09-08".into(),
            ..Default::default()
        });
        let (body, dialog) = section(&accounts, &Books::Read(vec![attributes]), "");
        assert!(body.contains("<td>USD</td>") && body.contains("Highest cost"));
        assert!(body.contains("2026-09-08"));
        assert!(dialog.contains("name=\"reason\" required"));
    }
}
