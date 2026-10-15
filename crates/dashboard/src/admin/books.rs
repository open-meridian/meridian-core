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
    set_account_attribute_request, AccountAttributeReply, AccountAttributes,
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

pub(super) fn method_named(method: i32) -> &'static str {
    METHODS
        .iter()
        .find(|(held, _)| *held as i32 == method)
        .map(|(_, name)| *name)
        .unwrap_or("")
}

impl Books {
    /// An account's attributes in the book, where it answered and holds any.
    pub fn of(&self, account_id: &str) -> Option<&AccountAttributes> {
        match self {
            Books::Read(held) => held.iter().find(|a| a.account_id == account_id),
            Books::NotAnswering(_) => None,
        }
    }
}

/// What an account's row and its Edit say of it in the book: its base
/// currency, its lot relief by name and as the form's value, and the date
/// its opening balance stands for, empty where none.
pub(super) fn in_book(books: &Books, account_id: &str) -> (String, i32, String) {
    let attributes = books.of(account_id);
    (
        attributes
            .map(|a| a.base_currency_code.clone())
            .unwrap_or_default(),
        attributes.map(|a| a.lot_relief_default).unwrap_or_default(),
        attributes
            .and_then(|a| a.opening_balance.as_ref())
            .map(|opening| opening.as_of_date.clone())
            .unwrap_or_default(),
    )
}

/// Whether an account's releases wait for compliance (W9.13, W11.6;
/// contract v22): as the book holds it, and true for an account it holds no
/// attributes for, as a new account waits.
pub(super) fn waits(books: &Books, account_id: &str) -> bool {
    books.of(account_id).is_none_or(|a| a.waits_for_compliance)
}

/// The form's word for it, which the select posts.
pub(super) fn waits_word(waits: bool) -> &'static str {
    if waits {
        WAITS
    } else {
        DOES_NOT_WAIT
    }
}

/// An account's mark (contract v24, W9.13): an error account, an
/// allocation account, or neither, by the form's word.
pub(super) fn mark_word(books: &Books, account_id: &str) -> &'static str {
    match books.of(account_id) {
        Some(a) if a.error_account => ERROR_ACCOUNT,
        Some(a) if a.allocation_account => ALLOCATION_ACCOUNT,
        _ => NEITHER,
    }
}

const NEITHER: &str = "neither";
const ERROR_ACCOUNT: &str = "error";
const ALLOCATION_ACCOUNT: &str = "allocation";

const WAITS: &str = "wait";
const DOES_NOT_WAIT: &str = "no-wait";

/// The Accounts tab's Edit, its "In the book" section (W9.13; the product
/// owner, 2026-10-10: one book per deployment, so its attributes sit with the
/// account they belong to): a form of its own, posted to the book with a
/// reason, each attribute that changed its own act, as the Books tab's was.
/// Shown only for an account that exists, by the dialog's script
/// (`data-edit-only`); the date the opening balance stands for is the book's
/// own (W9.1), shown and never posted.
pub fn book_form(token: &str) -> String {
    let methods: String = std::iter::once("<option value=\"0\">Not set</option>".to_string())
        .chain(METHODS.iter().map(|(method, name)| {
            format!(
                "<option value=\"{}\">{}</option>",
                *method as i32,
                escape(name)
            )
        }))
        .collect();
    format!(
        "<form method=\"post\" action=\"/admin/accounts/book#accounts\" class=\"dialog-part\" \
         data-edit-only>{token}<div class=\"dialog-body\"><h3>In the book \
         <span class=\"hint\">since <output name=\"since\"></output></span></h3>\
         <input type=\"hidden\" name=\"account_id\" data-record-id>\
         <div class=\"pair\"><label>Base currency <input name=\"base_currency_code\" maxlength=\"3\" \
         pattern=\"[A-Z]{{3}}\" placeholder=\"USD\"></label>\
         <label>Lot relief, when a sale names none <select name=\"lot_relief_default\">{methods}</select></label></div>\
         <div class=\"pair\"><label>Releases <select name=\"waits_for_compliance\">\
         <option value=\"{WAITS}\">Wait for compliance</option>\
         <option value=\"{DOES_NOT_WAIT}\">Do not wait</option></select></label>\
         <label>Marked <select name=\"account_mark\"><option value=\"{NEITHER}\">Neither</option>\
         <option value=\"{ERROR_ACCOUNT}\">An error account</option>\
         <option value=\"{ALLOCATION_ACCOUNT}\">An allocation account</option></select></label></div>\
         <div class=\"pair save\"><label>Why <input name=\"note\" required></label>\
         <button type=\"submit\" class=\"primary\">Save</button></div></div></form>"
    )
}

/// What the form asks the book to set: each attribute that differs from what
/// it holds, each its own act with the reason given (W9.13).
#[cfg(test)]
pub fn requests(
    account_id: &str,
    base_currency_code: &str,
    lot_relief_default: &str,
    waits_for_compliance: &str,
    reason: &str,
    held: Option<&AccountAttributes>,
) -> Result<Vec<SetAccountAttributeRequest>, String> {
    requests_marked(
        account_id,
        base_currency_code,
        lot_relief_default,
        waits_for_compliance,
        "",
        reason,
        held,
    )
}

/// [`requests`], with the account's mark (contract v24, W9.13): an error
/// account or an allocation account, each change its own act; one mark
/// cleared before the other is set.
pub fn requests_marked(
    account_id: &str,
    base_currency_code: &str,
    lot_relief_default: &str,
    waits_for_compliance: &str,
    account_mark: &str,
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
    // Whether releases wait for compliance (contract v22): a new account's
    // is true, so only a change from what it holds is asked.
    let waits_now = held.is_none_or(|a| a.waits_for_compliance);
    match waits_for_compliance {
        "" => {}
        WAITS | DOES_NOT_WAIT => {
            let wanted = waits_for_compliance == WAITS;
            if wanted != waits_now {
                asked.push(set_account_attribute_request::Attribute::WaitsForCompliance(wanted));
            }
        }
        other => {
            return Err(format!(
                "{other:?} is neither waiting for compliance nor not"
            ))
        }
    }
    let (error_now, allocation_now) = held
        .map(|a| (a.error_account, a.allocation_account))
        .unwrap_or((false, false));
    let wanted = match account_mark {
        "" => None,
        NEITHER => Some((false, false)),
        ERROR_ACCOUNT => Some((true, false)),
        ALLOCATION_ACCOUNT => Some((false, true)),
        other => return Err(format!("{other:?} is no mark an account takes")),
    };
    if let Some((error, allocation)) = wanted {
        // A mark cleared first, so the account never holds both.
        if error_now && !error {
            asked.push(set_account_attribute_request::Attribute::ErrorAccount(
                false,
            ));
        }
        if allocation_now && !allocation {
            asked.push(set_account_attribute_request::Attribute::AllocationAccount(
                false,
            ));
        }
        if error && !error_now {
            asked.push(set_account_attribute_request::Attribute::ErrorAccount(true));
        }
        if allocation && !allocation_now {
            asked.push(set_account_attribute_request::Attribute::AllocationAccount(
                true,
            ));
        }
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

/// One attribute's change as dashboard__set_account_attribute's arguments
/// (W6.20, contract v22): what a dialog prepares.
pub fn arguments(request: &SetAccountAttributeRequest) -> serde_json::Value {
    let mut arguments = serde_json::json!({
        "account_id": request.account_id,
        "note": request.reason,
    });
    match &request.attribute {
        Some(set_account_attribute_request::Attribute::BaseCurrencyCode(code)) => {
            arguments["base_currency_code"] = code.clone().into();
        }
        Some(set_account_attribute_request::Attribute::LotReliefDefault(method)) => {
            arguments["lot_relief_default"] = method_value(*method).into();
        }
        Some(set_account_attribute_request::Attribute::WaitsForCompliance(waits)) => {
            arguments["waits_for_compliance"] = (*waits).into();
        }
        // Contract v24 (W9.13): the error and allocation account marks.
        Some(set_account_attribute_request::Attribute::ErrorAccount(marked)) => {
            arguments["error_account"] = (*marked).into();
        }
        Some(set_account_attribute_request::Attribute::AllocationAccount(marked)) => {
            arguments["allocation_account"] = (*marked).into();
        }
        None => {}
    }
    arguments
}

/// A lot relief method by the tool's word for it.
pub fn method_value(method: i32) -> &'static str {
    match LotReliefMethod::try_from(method) {
        Ok(LotReliefMethod::FirstInFirstOut) => "first_in_first_out",
        Ok(LotReliefMethod::LastInFirstOut) => "last_in_first_out",
        Ok(LotReliefMethod::HighestCost) => "highest_cost",
        Ok(LotReliefMethod::LowestCost) => "lowest_cost",
        Ok(LotReliefMethod::AverageCost) => "average_cost",
        _ => "",
    }
}

/// The method a tool's word names.
pub fn method_of(word: &str) -> Option<LotReliefMethod> {
    METHODS
        .iter()
        .map(|(method, _)| *method)
        .find(|method| method_value(*method as i32) == word)
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
            "",
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
        let both = requests("ACC-1", "USD", "2", "wait", "why", None).unwrap();
        assert_eq!(both.len(), 2, "a new account waits already");
        let off = requests("ACC-1", "", "0", "no-wait", "why", None).unwrap();
        assert_eq!(
            off[0].attribute,
            Some(set_account_attribute_request::Attribute::WaitsForCompliance(false))
        );
        assert_eq!(arguments(&off[0])["waits_for_compliance"], false);
        assert!(requests(
            "ACC-1",
            "USD",
            "1",
            "",
            "why",
            Some(&held("USD", LotReliefMethod::FirstInFirstOut))
        )
        .is_err());
        assert!(requests("", "USD", "0", "", "why", None).is_err());
        assert_eq!(
            method_of("highest_cost"),
            Some(LotReliefMethod::HighestCost)
        );
    }

    #[test]
    fn an_accounts_book_is_read_by_its_id_and_its_form_posts_with_a_reason() {
        let mut attributes = held("USD", LotReliefMethod::HighestCost);
        attributes.opening_balance = Some(meridian_domain::v1::OpeningBalance {
            as_of_date: "2026-09-08".into(),
            ..Default::default()
        });
        let books = Books::Read(vec![attributes]);
        assert_eq!(
            in_book(&books, "ACC-1"),
            (
                "USD".into(),
                LotReliefMethod::HighestCost as i32,
                "2026-09-08".into()
            )
        );
        assert_eq!(in_book(&books, "ACC-2"), (String::new(), 0, String::new()));
        let silent = Books::NotAnswering("no handler".into());
        assert!(silent.of("ACC-1").is_none());
        let form = book_form("");
        assert!(form.contains("action=\"/admin/accounts/book#accounts\""));
        assert!(form.contains("name=\"note\" required") && form.contains("data-edit-only"));
        assert!(form.contains("name=\"waits_for_compliance\""));
        assert!(waits(&books, "ACC-2"), "a new account waits");
        assert!(
            form.contains("<output name=\"since\">"),
            "shown, never posted"
        );
    }
}

#[cfg(test)]
mod mark_tests {
    use super::*;

    #[test]
    fn a_mark_is_set_and_cleared_each_its_own_act_never_both() {
        let held = AccountAttributes {
            account_id: "ACC-1".into(),
            error_account: true,
            ..Default::default()
        };
        let asked =
            requests_marked("ACC-1", "", "0", "", "allocation", "why", Some(&held)).unwrap();
        assert_eq!(
            asked
                .iter()
                .map(|r| r.attribute.clone().unwrap())
                .collect::<Vec<_>>(),
            vec![
                set_account_attribute_request::Attribute::ErrorAccount(false),
                set_account_attribute_request::Attribute::AllocationAccount(true),
            ]
        );
        assert!(requests_marked("ACC-1", "", "0", "", "error", "why", Some(&held)).is_err());
        assert!(requests_marked("ACC-1", "", "0", "", "other", "why", None).is_err());
        assert_eq!(arguments(&asked[1])["allocation_account"], true);
    }
}
