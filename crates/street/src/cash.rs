//! Each amount the street answers names its cash instrument (contract v18;
//! decisions/023 as amended, ruling 1 of 2026-10-09).
//!
//! A custodian states a fiat amount by its ISO 4217 code; the street keeps it
//! so, and resolves each code once through the instrument store (W3.1, by
//! `iso4217`) to the currency's cash instrument, keeping the resolution as
//! its own record. A command naming a code not yet resolved has it resolved
//! before it is recorded; a code the instrument store could not answer for is
//! resolved by the next sweep, and the amount answered by its code meanwhile.
//! The codes held before v18 are resolved once at start, each record saying it
//! was filled in then (decisions/031).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::money::{self, Resolution};
use meridian_domain::v1::{Identifier, ResolveIdentifierReply, ResolveIdentifierRequest};
use prost::Message;

use crate::store::Store;

/// W3.1, asked (contract v18, approved 2026-10-10).
pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";

const WAIT: Duration = Duration::from_secs(2);

/// How often the codes held are swept for one not resolved.
pub const SWEEP_EVERY: Duration = Duration::from_secs(300);

/// The resolutions the store keeps, learned by this process.
pub fn load(store: &dyn Store) {
    match store.cash_instruments() {
        Ok(kept) => {
            for resolution in kept {
                money::learn(&resolution.code, &resolution.instrument_id);
            }
        }
        Err(failed) => tracing::warn!(%failed, "the cash instruments kept could not be read"),
    }
}

async fn resolve(bus: &Bus, code: &str, as_of_ns: i64) -> Option<String> {
    let asked = bus
        .call(
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            ResolveIdentifierRequest {
                identifiers: vec![Identifier {
                    scheme: money::ISO4217.into(),
                    value: code.to_string(),
                    source: String::new(),
                }],
                as_of_ns,
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            Some(WAIT),
        )
        .await;
    match asked {
        Ok((_, payload)) => ResolveIdentifierReply::decode(&payload[..])
            .ok()
            .filter(|reply| reply.found && !reply.instrument_id.is_empty())
            .map(|reply| reply.instrument_id),
        Err(failed) => {
            tracing::warn!(code, %failed, "a currency's code was not resolved; the sweep tries again");
            None
        }
    }
}

/// Each code not yet resolved, resolved and kept; `backfilled` for codes held
/// before this was asked.
pub async fn ensure(
    bus: &Bus,
    store: &Arc<dyn Store>,
    codes: &[String],
    now_ns: i64,
    backfilled: bool,
) {
    for code in codes {
        if money::known(code).is_some() {
            continue;
        }
        let Some(instrument) = resolve(bus, code, now_ns).await else {
            continue;
        };
        let resolution = Resolution {
            code: code.clone(),
            instrument_id: instrument.clone(),
            resolved_at_ns: now_ns,
            backfilled,
        };
        let keeping = Arc::clone(store);
        match tokio::task::spawn_blocking(move || keeping.keep_cash_instrument(&resolution)).await {
            Ok(Ok(())) => {
                money::learn(code, &instrument);
                tracing::info!(code, instrument, backfilled, "{}", money::RESOLVED_BY);
            }
            Ok(Err(failed)) => tracing::warn!(code, %failed, "a resolution was not kept"),
            Err(failed) => tracing::warn!(code, %failed, "a resolution was not kept"),
        }
    }
}

/// [`ensure`], from a handler's blocking thread.
pub fn ensure_blocking(bus: &Bus, store: &Arc<dyn Store>, codes: &[String], now_ns: i64) {
    if codes.iter().all(|code| money::known(code).is_some()) {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(ensure(bus, store, codes, now_ns, false));
    }
}

/// The codes held, each resolved: at start, every one held before is
/// backfilled; every five minutes after, any the instrument store did not
/// answer for.
pub async fn sweep_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    let mut first = true;
    loop {
        let reading = Arc::clone(&store);
        let codes = tokio::task::spawn_blocking(move || {
            load(reading.as_ref());
            reading.currency_codes()
        })
        .await;
        match codes {
            Ok(Ok(codes)) => ensure(&bus, &store, &codes, bus.clock().now_ns(), first).await,
            Ok(Err(failed)) => tracing::warn!(%failed, "the codes held could not be read"),
            Err(failed) => tracing::warn!(%failed, "the codes held could not be read"),
        }
        first = false;
        tokio::time::sleep(every).await;
    }
}
