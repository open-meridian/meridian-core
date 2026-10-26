//! Companies: the platform's minimal company records, kept by the
//! deployment (contract v28; W1.32, W3.32; FDP Q6; rulings 594 and 612).
//!
//! A `data` plugin naming a filer, a holder, an issuer or a rating agency by
//! a CIK or an LEI asks `ResolveCompany`, as the lake and the dashboard do;
//! the store answers the company record it holds, by `company_id` or by
//! `identifiers` as of a date. A company is never provisional: the
//! deployment mints no company record of its own. At a miss by identifiers
//! the store asks the conductor (`AskPlatformForCompany`, ruling 612), which
//! alone dials the platform (`MatchOrMintCompany`, W1.32), within the same
//! bounded ask a resolve takes. Only identifiers in a scheme on the licence
//! list the store read are sent (W3.25), with no name and a fresh request
//! key; a company matched or minted is kept under its `ENT-` ID and answers
//! the resolve. Queued for the platform's staff, or the platform not
//! reached, the resolve is a miss and nothing is kept: the data plugin holds
//! its rows and the next miss asks again with a new key, the platform
//! keeping no asker. A miss by `company_id` alone has nothing to look for
//! and is not asked about.

use std::collections::BTreeMap;
use std::sync::Arc;

use meridian_bus::Bus;
use meridian_domain::v1::{
    CompanyRecord, Identifier, MatchOrMintCompanyReply, MatchOrMintCompanyRequest,
    ResolveCompanyRequest,
};
use prost::Message;

use crate::store::{Asked as Held, Result, Store};

/// W3.32: a company resolved by its record or its open identifiers.
pub const RESOLVE_COMPANY: &str = "platform.reference.query.resolve-company";
/// W3.32 (ruling 612): the store asks the conductor, which alone dials the
/// platform.
pub const ASK_PLATFORM_FOR_COMPANY: &str = "platform.reference.query.ask-platform-for-company";

/// The platform's company IDs begin with this (ruling 594; decisions/034:
/// never reused).
pub const PLATFORM_PREFIX: &str = "ENT-";

/// The schemes a company is named by (W1.32): a CIK and an LEI.
pub const COMPANY_SCHEMES: [&str; 2] = ["cik", "lei"];

/// What the platform said of a company the store asked about.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
    Answered(MatchOrMintCompanyReply),
    /// Not reached within the bounded ask, or refused by the conductor: a
    /// miss, asked again at the next.
    Unreachable(String),
}

/// How the store asks the platform about a company: over the bus, through
/// the conductor, in a deployment; a fake in a test.
pub trait AskCompany: Send + Sync {
    fn ask(&self, request: MatchOrMintCompanyRequest) -> Asked;
}

/// A resolve answered: the record, and whether this resolve kept it, having
/// asked the platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub company: CompanyRecord,
    pub kept: bool,
}

/// A resolve refused or missed, naming each field by its path, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub fields: Vec<String>,
    pub words: String,
}

fn refused(field: impl Into<String>, words: impl Into<String>) -> Refused {
    let field = field.into();
    Refused {
        words: format!("{field}: {}", words.into()),
        fields: vec![field],
    }
}

/// Whether an identifier held on a record names it at `as_of_ns` (0 for
/// now): from, inclusive, until, exclusive, 0 while it is on the record.
fn holds_at(held: &Identifier, as_of_ns: i64) -> bool {
    if as_of_ns <= 0 {
        return held.valid_until_ns == 0;
    }
    held.valid_from_ns <= as_of_ns && (held.valid_until_ns == 0 || as_of_ns < held.valid_until_ns)
}

/// The identifier as a resolve names it: its scheme and its value, trimmed.
fn named(identifier: &Identifier) -> (String, String) {
    (
        identifier.scheme.trim().to_string(),
        identifier.value.trim().to_string(),
    )
}

/// W3.32: answer the company record the request names; at a miss by
/// identifiers, ask the platform through the conductor and keep its answer.
pub fn resolve(
    store: &dyn Store,
    ask: &dyn AskCompany,
    request: &ResolveCompanyRequest,
    now_ns: i64,
) -> Result<std::result::Result<Resolved, Refused>> {
    use meridian_pb::bounds::RESOLVE_COMPANY_REQUEST_IDENTIFIERS_COUNT as COUNT;
    let company_id = request.company_id.trim();
    if !COUNT.admits(request.identifiers.len()) {
        return Ok(Err(refused(
            "identifiers",
            format!("at most {} identifiers", COUNT.most),
        )));
    }
    let identifiers: Vec<(String, String)> = request
        .identifiers
        .iter()
        .map(named)
        .filter(|(scheme, value)| !scheme.is_empty() && !value.is_empty())
        .collect();
    if company_id.is_empty() && identifiers.is_empty() {
        return Ok(Err(refused(
            "identifiers",
            "a company is named by its company_id or by a cik or an lei",
        )));
    }
    if !company_id.is_empty() {
        return Ok(match store.company(company_id)? {
            Some(company) => Ok(Resolved {
                company,
                kept: false,
            }),
            None => Err(refused(
                "company_id",
                format!("{company_id:?} is no company this deployment holds"),
            )),
        });
    }
    // Each company an identifier names at the date asked, by ID: one is the
    // answer, two are a contradiction no resolve settles.
    let mut found: BTreeMap<String, CompanyRecord> = BTreeMap::new();
    for (scheme, value) in &identifiers {
        for company in store.companies_with(scheme, value)? {
            let holds = company.identifiers.iter().any(|held| {
                held.scheme == *scheme && held.value == *value && holds_at(held, request.as_of_ns)
            });
            if holds {
                found.insert(company.company_id.clone(), company);
            }
        }
    }
    if found.len() > 1 {
        return Ok(Err(refused(
            "identifiers",
            format!(
                "the identifiers name different companies: {}",
                found.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        )));
    }
    if let Some(company) = found.into_values().next() {
        return Ok(Ok(Resolved {
            company,
            kept: false,
        }));
    }
    // A miss: what to look for, in schemes on the licence list alone.
    let list = crate::one_id::licence(store);
    let asking: Vec<Held> = identifiers
        .iter()
        .filter(|(scheme, _)| COMPANY_SCHEMES.contains(&scheme.as_str()))
        .map(|(scheme, value)| Held {
            scheme: scheme.clone(),
            value: value.clone(),
            source: String::new(),
        })
        .collect();
    let sendable = crate::one_id::sendable(&asking, &list);
    if sendable.is_empty() {
        return Ok(Err(refused(
            "identifiers",
            "no company this deployment holds, and none of its identifiers is one the licence \
             list lets it ask the platform by",
        )));
    }
    match ask.ask(MatchOrMintCompanyRequest {
        identifiers: sendable,
        name: String::new(),
        request_key: crate::one_id::request_key(),
    }) {
        Asked::Answered(MatchOrMintCompanyReply {
            company: Some(company),
            ..
        }) if company.company_id.starts_with(PLATFORM_PREFIX) => {
            let kept = store.keep_company(&company, now_ns)?;
            // The latest version held answers: a platform's answer older
            // than what was kept meanwhile never steps a reader back.
            let company = store.company(&company.company_id)?.unwrap_or(company);
            Ok(Ok(Resolved { company, kept }))
        }
        Asked::Answered(MatchOrMintCompanyReply {
            company: Some(other),
            ..
        }) => {
            tracing::warn!(
                company = other.company_id,
                "the platform answered a company under an ID that is not its own; nothing kept"
            );
            Ok(Err(refused(
                "identifiers",
                "no company this deployment holds; the platform's answer was not a company \
                 record it could keep",
            )))
        }
        Asked::Answered(reply) => Ok(Err(refused(
            "identifiers",
            if reply.pending_review {
                "no company this deployment holds; the platform queued it for its staff, and \
                 the next resolve asks again"
            } else {
                "no company this deployment holds, nor the platform"
            },
        ))),
        Asked::Unreachable(why) => {
            tracing::info!(why, "a company miss while the platform cannot be reached");
            Ok(Err(refused(
                "identifiers",
                "no company this deployment holds; the platform could not be reached, and the \
                 next resolve asks again",
            )))
        }
    }
}

/// The conductor asked over the bus, within the bounded ask (W3.32).
pub struct BusAskCompany {
    pub bus: Arc<Bus>,
    pub within: std::time::Duration,
}

impl AskCompany for BusAskCompany {
    fn ask(&self, request: MatchOrMintCompanyRequest) -> Asked {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Asked::Unreachable("no runtime".into());
        };
        let asked = handle
            .block_on(self.bus.call(
                ASK_PLATFORM_FOR_COMPANY,
                "meridian.v1.MatchOrMintCompanyRequest",
                request.encode_to_vec(),
                None,
                Some(self.within),
            ))
            .map_err(|failed| failed.to_string())
            .and_then(|(_, bytes)| {
                MatchOrMintCompanyReply::decode(&bytes[..]).map_err(|f| f.to_string())
            });
        match asked {
            Ok(reply) => Asked::Answered(reply),
            Err(failed) => Asked::Unreachable(failed),
        }
    }
}

/// Register the resolve on the bus (W3.32).
pub fn serve(
    bus: &Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn meridian_clock::Clock>,
    ask: Arc<dyn AskCompany>,
) {
    bus.serve(RESOLVE_COMPANY, move |envelope| {
        if envelope.payload_type != "meridian.v1.ResolveCompanyRequest" {
            return Err(format!(
                "expected meridian.v1.ResolveCompanyRequest, got {}",
                envelope.payload_type
            ));
        }
        let request = ResolveCompanyRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable meridian.v1.ResolveCompanyRequest: {failed}"))?;
        let resolved = resolve(store.as_ref(), ask.as_ref(), &request, clock.now_ns())
            .map_err(|failed| failed.to_string())?
            .map_err(|refusal| {
                meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    &refusal.fields,
                    refusal.words,
                )
            })?;
        if resolved.kept {
            tracing::info!(
                company = resolved.company.company_id,
                version = resolved.company.version,
                "a company record kept from the platform's answer"
            );
        }
        Ok((
            "meridian.v1.CompanyRecord".to_string(),
            resolved.company.encode_to_vec(),
        ))
    });
}

#[cfg(test)]
mod tests;
