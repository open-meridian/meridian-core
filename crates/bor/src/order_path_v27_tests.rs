//! The order path's v27 rules (sdk-contract/the-placement-type-tca-and-fx-netting-contract),
//! held over the bus with the in-memory store as the order path's are: the
//! placement type (W11.8; the gaps spec's B1 to B9), an RFQ placement's
//! quotes and the pick (W11.40, W11.41; slice C's buy side).

use super::*;

fn placement(order_id: &str, kind: PlacementType) -> CreatePlacementRequest {
    CreatePlacementRequest {
        order_id: order_id.into(),
        connectivity_instance_id: ME.into(),
        venue_account_code: "SNAP-ACCT-9F2".into(),
        quantity: d("4"),
        limit_price: usd("227.50"),
        placement_type: kind as i32,
        ..Default::default()
    }
}

async fn create(
    w: &Wired,
    request: &CreatePlacementRequest,
    from: &As,
) -> Result<Placement, String> {
    send_from(
        &w.bus,
        CREATE_PLACEMENT,
        "meridian.v1.CreatePlacementRequest",
        request,
        from,
    )
    .await
    .map(|reply| {
        PlacementReply::decode(&reply[..])
            .unwrap()
            .placement
            .unwrap()
    })
}

fn fields(refused: &str) -> Vec<String> {
    let (status, fields) = status_of(refused).unwrap_or_else(|| panic!("{refused}"));
    assert_eq!(
        status,
        meridian_bus::RefusalStatus::InvalidArgument,
        "{refused}"
    );
    fields
}

fn strategy() -> Option<StrategyInstruction> {
    Some(StrategyInstruction {
        kind: StrategyKind::Vwap as i32,
        ..Default::default()
    })
}

/// B3 and B5: a type and its fields agree, or the placement is refused
/// INVALID_ARGUMENT naming the field; unspecified is checked as before v27.
#[tokio::test(flavor = "multi_thread")]
async fn a_placements_type_and_its_fields_agree_or_it_is_refused_naming_the_field() {
    let w = wired();
    let order = released(&w, "40").await;
    let id = order.order_id.as_str();
    let trader = person(TRADER);

    let algo = placement(id, PlacementType::Algo);
    assert_eq!(
        fields(&create(&w, &algo, &trader).await.unwrap_err()),
        vec!["strategy"]
    );
    for kind in [PlacementType::Dma, PlacementType::Care] {
        let with_strategy = CreatePlacementRequest {
            strategy: strategy(),
            ..placement(id, kind)
        };
        assert_eq!(
            fields(&create(&w, &with_strategy, &trader).await.unwrap_err()),
            vec!["strategy"]
        );
    }
    let dma_with_contact = CreatePlacementRequest {
        counterparty_contact: "J. Smith, desk line".into(),
        ..placement(id, PlacementType::Dma)
    };
    assert_eq!(
        fields(&create(&w, &dma_with_contact, &trader).await.unwrap_err()),
        vec!["counterparty_contact"]
    );
    let rfq_nowhere = CreatePlacementRequest {
        connectivity_instance_id: String::new(),
        ..placement(id, PlacementType::Rfq)
    };
    assert_eq!(
        fields(&create(&w, &rfq_nowhere, &trader).await.unwrap_err()),
        vec!["connectivity_instance_id"]
    );
    let manual_at_a_venue = CreatePlacementRequest {
        destination: "VEN-01J8XQ4M7K000000000BROKR".into(),
        ..placement(id, PlacementType::Manual)
    };
    assert_eq!(
        fields(&create(&w, &manual_at_a_venue, &trader).await.unwrap_err()),
        vec!["connectivity_instance_id"]
    );

    // An algo with its strategy, a care and a DMA placement are placed, each
    // keeping its type; an unspecified one is kept unspecified.
    let algo = create(
        &w,
        &CreatePlacementRequest {
            strategy: strategy(),
            ..placement(id, PlacementType::Algo)
        },
        &trader,
    )
    .await
    .unwrap();
    assert_eq!(algo.placement_type, PlacementType::Algo as i32);
    let care = create(&w, &placement(id, PlacementType::Care), &trader)
        .await
        .unwrap();
    assert_eq!(care.placement_type, PlacementType::Care as i32);
    let old = create(&w, &placement(id, PlacementType::Unspecified), &trader)
        .await
        .unwrap();
    assert_eq!(old.placement_type, PlacementType::Unspecified as i32);
}

/// B5: a manual placement names no connectivity instance -- so the sidecar's
/// routing verdict does not apply -- and keeps its trade details as evidence.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_placement_names_no_connection_and_keeps_its_trade_details() {
    let w = wired();
    let order = released(&w, "10").await;
    let by_phone = CreatePlacementRequest {
        order_id: order.order_id.clone(),
        quantity: d("10"),
        limit_price: usd("227.50"),
        destination: "VEN-01J8XQ4M7K000000000BROKR".into(),
        placement_type: PlacementType::Manual as i32,
        counterparty_contact: "J. Smith, +41 44 000 00 00".into(),
        agreed_at_ns: NOW - 60_000_000_000,
        agreed_precision: TimePrecision::Second as i32,
        ..Default::default()
    };
    let no_route = As {
        allowed: false,
        ..person(TRADER)
    };
    let missing_counterparty = CreatePlacementRequest {
        destination: String::new(),
        ..by_phone.clone()
    };
    assert_eq!(
        fields(
            &create(&w, &missing_counterparty, &no_route)
                .await
                .unwrap_err()
        ),
        vec!["destination"]
    );
    let placed = create(&w, &by_phone, &no_route).await.unwrap();
    assert_eq!(placed.placement_type, PlacementType::Manual as i32);
    assert!(placed.connectivity_instance_id.is_empty());
    assert_eq!(placed.counterparty_contact, "J. Smith, +41 44 000 00 00");
    assert_eq!(placed.agreed_at_ns, NOW - 60_000_000_000);
    assert_eq!(placed.agreed_precision, TimePrecision::Second as i32);
    assert_eq!(state(&order_now(&w, &order.order_id)), OrderState::Working);
}

/// B3: an order of type request for quote takes only RFQ placements; an RFQ
/// placement is allowed on any order.
#[tokio::test(flavor = "multi_thread")]
async fn an_rfq_order_takes_only_rfq_placements() {
    let w = wired();
    waits(&w, ACC, false).await;
    let rfq_order = ProposeOrderRequest {
        order_type: OrderType::RequestForQuote as i32,
        limit_price: None,
        ..buy("10")
    };
    let order = propose(&w, &rfq_order).await.unwrap();
    release(&w, &order.order_id).await.unwrap();
    let dma = CreatePlacementRequest {
        limit_price: None,
        ..placement(&order.order_id, PlacementType::Dma)
    };
    assert_eq!(
        fields(&create(&w, &dma, &person(TRADER)).await.unwrap_err()),
        vec!["placement_type"]
    );
    let rfq = CreatePlacementRequest {
        limit_price: None,
        ..placement(&order.order_id, PlacementType::Rfq)
    };
    create(&w, &rfq, &person(TRADER)).await.unwrap();
    // On a limit order, an RFQ placement too.
    let limit = released(&w, "5").await;
    create(
        &w,
        &placement(&limit.order_id, PlacementType::Rfq),
        &person(TRADER),
    )
    .await
    .unwrap();
}

fn quote(placement_id: &str, dealer: &str, price: &str) -> ReportPlacementQuoteRequest {
    ReportPlacementQuoteRequest {
        placement_id: placement_id.into(),
        dealer_id: dealer.into(),
        dealer_quote_id: format!("Q-{dealer}"),
        side: Side::Sell as i32,
        price: usd(price),
        quantity: d("4"),
        received_at_ns: NOW,
        valid_until_ns: NOW + 30_000_000_000,
        outcome: PlacementQuoteOutcome::Open as i32,
        raw_record: Some(meridian_pb::v1::RawRecordRef {
            instance_id: ME.into(),
            key: format!("quotes/{dealer}"),
        }),
        ..Default::default()
    }
}

async fn report_quote(
    w: &Wired,
    request: &ReportPlacementQuoteRequest,
) -> Result<Placement, String> {
    send_from(
        &w.bus,
        REPORT_PLACEMENT_QUOTE,
        "meridian.v1.ReportPlacementQuoteRequest",
        request,
        &venue(),
    )
    .await
    .map(|reply| {
        PlacementReply::decode(&reply[..])
            .unwrap()
            .placement
            .unwrap()
    })
}

async fn pick(
    w: &Wired,
    placement_id: &str,
    quote_id: &str,
    from: &As,
) -> Result<Placement, String> {
    send_from(
        &w.bus,
        PICK_PLACEMENT_QUOTE,
        "meridian.v1.PickPlacementQuoteRequest",
        &PickPlacementQuoteRequest {
            placement_id: placement_id.into(),
            quote_id: quote_id.into(),
            reason: "the second-best quote, for the dealer's size".into(),
            idempotency_key: String::new(),
        },
        from,
    )
    .await
    .map(|reply| {
        PlacementReply::decode(&reply[..])
            .unwrap()
            .placement
            .unwrap()
    })
}

fn outcome_of(placement: &Placement, quote_id: &str) -> PlacementQuoteOutcome {
    let quote = placement
        .quotes
        .iter()
        .find(|q| q.quote_id == quote_id)
        .unwrap();
    PlacementQuoteOutcome::try_from(quote.outcome).unwrap()
}

/// W11.40 and W11.41: the dealers' quotes on the firm's RFQ placement,
/// minted `PQT-` IDs, each outcome moving only forward; the `ems` picks one,
/// not necessarily the best; one pick at a time; a picked quote that expires
/// clears the pick.
#[tokio::test(flavor = "multi_thread")]
async fn an_rfq_placements_quotes_are_recorded_and_one_is_picked_at_a_time() {
    let w = wired();
    let order = released(&w, "4").await;
    let trader = person(TRADER);
    let rfq = create(&w, &placement(&order.order_id, PlacementType::Rfq), &trader)
        .await
        .unwrap();
    let dma = create(&w, &placement(&order.order_id, PlacementType::Dma), &trader)
        .await
        .unwrap();
    let mut heard = w.bus.subscribe_every(PLACEMENT_UPDATED);

    // A quote on a placement that is not RFQ: the order's state refuses it.
    let refused = report_quote(&w, &quote(&dma.placement_id, "VEN-A", "227.40"))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));
    // A quote missing its price and side, named.
    let bare = ReportPlacementQuoteRequest {
        price: None,
        side: Side::Unspecified as i32,
        ..quote(&rfq.placement_id, "VEN-A", "0")
    };
    assert_eq!(
        fields(&report_quote(&w, &bare).await.unwrap_err()),
        vec!["side", "price"]
    );

    let best = report_quote(&w, &quote(&rfq.placement_id, "VEN-A", "227.40"))
        .await
        .unwrap();
    let a = best.quotes[0].quote_id.clone();
    assert!(a.starts_with("PQT-"), "{a}");
    let both = report_quote(&w, &quote(&rfq.placement_id, "VEN-B", "227.45"))
        .await
        .unwrap();
    assert_eq!(both.quotes.len(), 2);
    let b = both.quotes[1].quote_id.clone();
    assert_eq!(outcome_of(&both, &b), PlacementQuoteOutcome::Open);
    let event =
        PlacementUpdatedEvent::decode(&next(&mut heard).await.envelope.payload[..]).unwrap();
    assert_eq!(event.entry.unwrap().kind, "placement-quote-recorded");

    // The venue never reports a pick.
    let picked_by_venue = ReportPlacementQuoteRequest {
        quote_id: a.clone(),
        outcome: PlacementQuoteOutcome::Picked as i32,
        ..Default::default()
    };
    let picked_by_venue = ReportPlacementQuoteRequest {
        placement_id: rfq.placement_id.clone(),
        ..picked_by_venue
    };
    assert_eq!(
        fields(&report_quote(&w, &picked_by_venue).await.unwrap_err()),
        vec!["outcome"]
    );

    // The trader picks the second-best quote: recorded, never refused.
    let after = pick(&w, &rfq.placement_id, &b, &trader).await.unwrap();
    assert_eq!(after.picked_quote_id, b);
    assert_eq!(outcome_of(&after, &b), PlacementQuoteOutcome::Picked);
    let entry = w.store.journal(ACC).unwrap().last().unwrap().meta.clone();
    assert_eq!(entry.kind, "placement-quote-picked");
    assert_eq!(entry.reason, "the second-best quote, for the dealer's size");
    // A second pick while one stands.
    let second = pick(&w, &rfq.placement_id, &a, &trader).await.unwrap_err();
    assert_eq!(code(&second), Some(RefusalReason::OrderState));

    // The picked quote expires before the venue accepts: the pick cleared,
    // and an outcome backwards refused.
    let expired = ReportPlacementQuoteRequest {
        placement_id: rfq.placement_id.clone(),
        quote_id: b.clone(),
        outcome: PlacementQuoteOutcome::Expired as i32,
        ..Default::default()
    };
    let after = report_quote(&w, &expired).await.unwrap();
    assert!(after.picked_quote_id.is_empty());
    assert_eq!(outcome_of(&after, &b), PlacementQuoteOutcome::Expired);
    let reopened = ReportPlacementQuoteRequest {
        outcome: PlacementQuoteOutcome::Open as i32,
        ..expired.clone()
    };
    assert_eq!(
        code(&report_quote(&w, &reopened).await.unwrap_err()),
        Some(RefusalReason::OrderState)
    );
    let refused = pick(&w, &rfq.placement_id, &b, &trader).await.unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::OrderState));

    // Another is picked, then accepted at the venue.
    pick(&w, &rfq.placement_id, &a, &trader).await.unwrap();
    let accepted = report_quote(
        &w,
        &ReportPlacementQuoteRequest {
            outcome: PlacementQuoteOutcome::Accepted as i32,
            quote_id: a.clone(),
            ..expired
        },
    )
    .await
    .unwrap();
    assert_eq!(accepted.picked_quote_id, a);
    assert_eq!(outcome_of(&accepted, &a), PlacementQuoteOutcome::Accepted);
    // The journal replays to the same quotes.
    let replayed = order_now(&w, &order.order_id);
    let kept = replayed
        .placements
        .iter()
        .find(|p| p.placement_id == rfq.placement_id)
        .unwrap();
    let outcomes = |quotes: &[PlacementQuote]| {
        quotes
            .iter()
            .map(|q| (q.quote_id.clone(), q.outcome, q.dealer_quote_id.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(outcomes(&kept.quotes), outcomes(&accepted.quotes));
    assert_eq!(kept.picked_quote_id, a);
}

/// W11.41: an agent's pick on a connection not stamped demo is refused and
/// recorded while the guard is on; a quote past its valid-until time is not
/// picked; a placement another `ems` made is not this one's.
#[tokio::test(flavor = "multi_thread")]
async fn a_pick_is_held_to_the_agents_guard_and_to_the_quotes_valid_until_time() {
    let w = wired();
    let order = released(&w, "4").await;
    let rfq = create(
        &w,
        &placement(&order.order_id, PlacementType::Rfq),
        &person(TRADER),
    )
    .await
    .unwrap();
    let short = ReportPlacementQuoteRequest {
        valid_until_ns: NOW - 1,
        ..quote(&rfq.placement_id, "VEN-A", "227.40")
    };
    let stale = report_quote(&w, &short).await.unwrap().quotes[0]
        .quote_id
        .clone();
    let fresh = report_quote(&w, &quote(&rfq.placement_id, "VEN-B", "227.41"))
        .await
        .unwrap()
        .quotes[1]
        .quote_id
        .clone();
    let past = pick(&w, &rfq.placement_id, &stale, &person(TRADER))
        .await
        .unwrap_err();
    assert_eq!(code(&past), Some(RefusalReason::OrderState));

    let before = w.store.journal(ACC).unwrap().len();
    let agent = As {
        delegation: Some("DLG-agent-1"),
        ..person(TRADER)
    };
    let refused = pick(&w, &rfq.placement_id, &fresh, &agent)
        .await
        .unwrap_err();
    assert_eq!(code(&refused), Some(RefusalReason::AgentLiveOrder));
    let journal = w.store.journal(ACC).unwrap();
    assert_eq!(journal.len(), before + 1, "the refusal is recorded");
    assert_eq!(
        journal.last().unwrap().meta.kind,
        "placement-quote-pick-refused"
    );
    let demo = As {
        demo: true,
        ..agent
    };
    let picked = pick(&w, &rfq.placement_id, &fresh, &demo).await.unwrap();
    assert_eq!(picked.picked_quote_id, fresh);
}

/// W9.10 (rulings 320 and 396): a sale relieves the lot it closes, the
/// relief recording what the quantity relieved was sold for and the trade
/// date; the journal replays to the same reliefs.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_lot_records_its_proceeds_on_each_relief() {
    let w = wired();
    let bought = released(&w, "10").await;
    let placement = place(&w, &bought.order_id, "10", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    execute(&w, &fill(&placement.placement_id, "10", "BUY:10"))
        .await
        .unwrap();

    let sale = ProposeOrderRequest {
        side: Side::Sell as i32,
        ..buy("4")
    };
    let order = propose(&w, &sale).await.unwrap();
    release(&w, &order.order_id).await.unwrap();
    let placement = place(&w, &order.order_id, "4", &person(TRADER))
        .await
        .unwrap()
        .placement
        .unwrap();
    let mut sold = fill(&placement.placement_id, "4", "SELL:4");
    sold.price = usd("230.15");
    sold.trade_date = "2026-10-14".into();
    sold.computed_settlement_date = "2026-10-15".into();
    execute(&w, &sold).await.unwrap();

    let lots = |w: &Wired| {
        w.store
            .book(ACC)
            .unwrap()
            .positions
            .values()
            .find(|held| held.instrument_id == AAPL && held.side == HoldingSide::Long as i32)
            .unwrap()
            .lots
            .values()
            .map(|state| state.lot.clone())
            .collect::<Vec<_>>()
    };
    let held = lots(&w);
    assert_eq!(held.len(), 1);
    let lot = &held[0];
    assert_eq!(read(&lot.open_quantity), "6");
    assert_eq!(lot.reliefs.len(), 1);
    let relief = &lot.reliefs[0];
    assert_eq!(read(&relief.quantity), "4");
    assert_eq!(read(&relief.proceeds.as_ref().unwrap().amount), "920.60");
    assert_eq!(relief.trade_date, "2026-10-14");
    assert_eq!(relief.relieved_by, lot.relieved_by.last().cloned());
    // A replay of the whole journal folds the same reliefs.
    let partition = lot.relieved_by.last().unwrap().partition.clone();
    let replayed = crate::Book::replay(ACC, &partition, &w.store.journal(ACC).unwrap()).unwrap();
    let again = replayed
        .positions
        .values()
        .find(|held| held.instrument_id == AAPL && held.side == HoldingSide::Long as i32)
        .unwrap()
        .lots
        .values()
        .next()
        .unwrap()
        .lot
        .clone();
    assert_eq!(again.reliefs, lot.reliefs);
}
