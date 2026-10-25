//! v26's standard measure definitions, version 1 (the one-API spec's
//! requirements 14 to 17 and Q15; the v26 plan's question 226; rulings 301
//! to 303), shipped with the component and versioned with it.
//!
//! Each fixes every parameter that changes its number. The parameters are
//! the SDK reference's (`meridian.analytics.reference.DEFINITIONS`) word for
//! word, and historical VaR's and the implied distribution's are design's
//! fixture's (fixtures/analytics/list-measure-definitions.yaml).

/// The breakdown a figure's component is named under (ruling 302): exposure
/// answers each bucket's `gross` and `net`, pnl its `unrealized` and
/// `realized`.
pub const COMPONENT: &str = "component";

/// How realized P&L's period is labelled wherever it is shown (ruling
/// 303): pnl version 2's, v27's (ruling 320).
pub const REALIZED_SINCE: &str = "since the account opened";

/// The implementation every model id has (requirement 19).
pub const BUILT_IN: &str = "built-in";

/// v26's model ids, each at version 1 (the first release's §1); from
/// contract v28 (ruling 565) the option models and the calibrations.
pub const MODELS: [&str; 8] = [
    "cash",
    "quoted_price",
    "binary_event",
    "american_binomial",
    "black_scholes",
    "black_76",
    "svi",
    "ssvi",
];

/// The breakdowns the linear measures allow (requirement 17).
pub const BY: [&str; 6] = [
    "account",
    "asset_class",
    "currency",
    "instrument",
    "sleeve",
    "country",
];

/// A measure's version-1 definition, as `ListMeasureDefinitions` serves it.
#[derive(Debug, Clone, Copy)]
pub struct Definition {
    pub definition: &'static str,
    pub version: i32,
    pub parameters: &'static [(&'static str, &'static str)],
    pub breakdowns: &'static [&'static str],
    pub scale: u32,
    pub description: &'static str,
}

impl Definition {
    pub fn parameter(&self, name: &str) -> Option<&'static str> {
        self.parameters
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    }

    /// Whether the definition is over an event group.
    pub fn is_event_measure(&self) -> bool {
        matches!(
            self.definition,
            "resolution_payoff" | "implied_distribution"
        )
    }
}

const HISTORICAL: &[(&str, &str)] = &[
    ("confidence", "0.99"),
    ("horizon_days", "1"),
    ("window_days", "500"),
    ("return_type", "relative"),
    ("missing_days", "carry_forward_then_not_evaluated_after_5"),
    ("quantile", "order_statistic_lower"),
    ("daily_cut", "16:00 America/New_York"),
    ("event_contracts", "excluded_with_resolution_worst_case"),
];

/// The option models' parameters (contract v28; W12.1, W12.3; ruling 565).
const OPTION_MODEL: &[(&str, &str)] = &[
    (
        "models",
        "american_binomial_black_scholes_black_76_by_the_package_binding",
    ),
    ("steps", "200"),
    ("time", "calendar_minutes_to_settlement_over_525600"),
    (
        "settlement_time",
        "am_at_the_session_open_pm_at_1600_in_the_venue_zone",
    ),
    ("forward", "put_call_parity_at_the_least_abs_call_less_put"),
    // Ruling 608 as 611 named it: the curve, and how it is read at each
    // expiry; a later version overrides either (requirement 14).
    ("risk_free_rate", "ust_cmt"),
    ("rate_interpolation", "cubic_spline"),
];

/// The surface's parameters (contract v28; W12.1).
const SURFACE: &[(&str, &str)] = &[
    ("slice", "raw_svi_out_of_the_money_mids"),
    ("across", "ssvi_power_law"),
    ("weights", "inverse_iv_bid_ask_width"),
    ("checks", "gatheral_jacquier_butterfly_and_calendar"),
    ("steps", "200"),
    ("risk_free_rate", "ust_cmt"),
    ("rate_interpolation", "cubic_spline"),
];

/// A positioning basis's parameters (ruling 591: one version per basis).
const POSITIONING_LONG_CALLS_SHORT_PUTS: &[(&str, &str)] = &[
    ("positioning_basis", "convention_long_calls_short_puts"),
    ("greeks", "gex_dex_vex_chex"),
    ("gex", "gamma_times_spot_squared_over_100_per_1pct_move"),
    ("expiry_window", "the_job_chains"),
    ("open_interest_date", "the_latest_on_or_before_the_as_of"),
    ("steps", "200"),
    ("risk_free_rate", "ust_cmt"),
    ("rate_interpolation", "cubic_spline"),
];
const POSITIONING_SHORT_ALL: &[(&str, &str)] = &[
    ("positioning_basis", "convention_short_all"),
    ("greeks", "gex_dex_vex_chex"),
    ("gex", "gamma_times_spot_squared_over_100_per_1pct_move"),
    ("expiry_window", "the_job_chains"),
    ("open_interest_date", "the_latest_on_or_before_the_as_of"),
    ("steps", "200"),
    ("risk_free_rate", "ust_cmt"),
    ("rate_interpolation", "cubic_spline"),
];

/// Every definition the component serves, in the order it lists them:
/// v26's at version 1, from contract v27 `execution_cost` version 1 and
/// `pnl` version 2, and from contract v28 the options measures (W12.1).
pub const DEFINITIONS: [Definition; 23] = [
    Definition {
        definition: "market_value",
        version: 1,
        parameters: &[("valuation", "quantity_times_value_per_unit")],
        breakdowns: &BY,
        scale: 2,
        description: "Each position's quantity times its model's value per unit, net",
    },
    Definition {
        definition: "exposure",
        version: 1,
        parameters: &[
            ("valuation", "quantity_times_value_per_unit"),
            ("components", "gross_absolute_and_net_signed"),
        ],
        breakdowns: &BY,
        scale: 2,
        description: "Each bucket's market value gross (without its sign) and net",
    },
    Definition {
        definition: "concentration",
        version: 1,
        parameters: &[(
            "weight",
            "instrument_market_value_over_account_market_value",
        )],
        breakdowns: &["instrument"],
        scale: 2,
        description: "Each instrument's weight in its account, as a numerator and a denominator",
    },
    Definition {
        definition: "pnl",
        version: 1,
        parameters: &[("unrealized", "market_value_less_open_lots_cost")],
        breakdowns: &BY,
        scale: 2,
        description: "Unrealized P&L: each bucket's market value less its open lots' cost",
    },
    Definition {
        definition: "historical_var",
        version: 1,
        parameters: HISTORICAL,
        breakdowns: &["account", "asset_class", "currency", "sleeve"],
        scale: 2,
        description: "Historical value at risk by full revaluation over the lake's history",
    },
    Definition {
        definition: "expected_shortfall",
        version: 1,
        parameters: HISTORICAL,
        breakdowns: &["account", "asset_class", "currency", "sleeve"],
        scale: 2,
        description: "The mean loss beyond historical value at risk, by full revaluation",
    },
    Definition {
        definition: "resolution_payoff",
        version: 1,
        parameters: &[("none_of_the_named", "mutually_exclusive_not_exhaustive")],
        breakdowns: &["account"],
        scale: 2,
        description: "Each account's P&L if each member of an event group resolves YES",
    },
    Definition {
        definition: "implied_distribution",
        version: 1,
        parameters: &[
            ("price", "mid_or_last_past_spread"),
            ("spread_threshold", "0.10"),
            ("normalisation", "price_over_sum_of_prices"),
        ],
        breakdowns: &[],
        scale: 18,
        description: "Each outcome's price over the sum of the group's prices, as a numerator \
                      and a denominator",
    },
    // Contract v27 (W12.1; the gaps spec's C1 to C10): each execution's cost
    // against each benchmark, recorded as the lake's ExecutionCost rows.
    Definition {
        definition: "execution_cost",
        version: 1,
        parameters: &[
            (
                "benchmarks",
                "decision_arrival_placement_arrival_mid_at_fill",
            ),
            ("sign", "side_adjusted_positive_cost"),
            ("units", "bps"),
            ("staleness_bound_ns", "5000000000"),
        ],
        breakdowns: &["account"],
        scale: 4,
        description: "Each execution's cost against each benchmark in bps, side-adjusted, \
                      positive a cost; each account's notional-weighted mean per benchmark",
    },
    // Contract v27 (rulings 320 and 396): realized P&L beside unrealized,
    // served now the book records each closed lot's proceeds (Lot.reliefs).
    Definition {
        definition: "pnl",
        version: 2,
        parameters: &[
            ("unrealized", "market_value_less_open_lots_cost"),
            (
                "realized",
                "closed_lots_proceeds_less_cost_since_the_account_opened",
            ),
        ],
        breakdowns: &BY,
        scale: 2,
        description: "Each bucket's unrealized P&L (open lots) and realized P&L since the \
                      account opened (closed lots)",
    },
    // Contract v28 (W12.1; ruling 565; OPT 2.2 and G14).
    Definition {
        definition: "implied_volatility",
        version: 1,
        parameters: OPTION_MODEL,
        breakdowns: &["series", "quote_side"],
        scale: 6,
        description: "Each series' implied volatility at bid, mid and ask; none below intrinsic",
    },
    Definition {
        definition: "implied_forward",
        version: 1,
        parameters: OPTION_MODEL,
        breakdowns: &["expiry"],
        scale: 6,
        description: "Each expiration's forward from put-call parity, and the borrow it implies",
    },
    Definition {
        definition: "greeks",
        version: 1,
        parameters: &[
            ("models", "american_binomial_black_scholes_black_76_by_the_package_binding"),
            ("steps", "200"),
            ("node", "the_implied_volatility_at_mid"),
            ("risk_free_rate", "ust_cmt"),
            ("rate_interpolation", "cubic_spline"),
            (
                "units",
                "delta_gamma_per_1_vega_vanna_per_vol_point_theta_charm_per_calendar_day_rho_per_1pct",
            ),
        ],
        breakdowns: &["series", "greek"],
        scale: 6,
        description: "Each series' delta, gamma, theta, vega, rho, vanna and charm, from the node \
                      its implied volatility came from",
    },
    Definition {
        definition: "surface_fit",
        version: 1,
        parameters: SURFACE,
        breakdowns: &["expiry", "parameter"],
        scale: 6,
        description: "Raw SVI per expiration and SSVI across them, no-arbitrage checked: the \
                      parameters, RMSE in vol points and the share inside the bid-ask",
    },
    Definition {
        definition: "surface_residual",
        version: 1,
        parameters: SURFACE,
        breakdowns: &["series"],
        scale: 4,
        description: "Each series' residual to the fitted surface in vol points; rich or cheap \
                      only outside the IV bid-ask",
    },
    Definition {
        definition: "term_and_skew",
        version: 1,
        parameters: SURFACE,
        breakdowns: &["expiry"],
        scale: 6,
        description: "ATM IV, constant-maturity 30, 60 and 90 days, the 25-delta risk reversal \
                      and butterfly, and both expected moves",
    },
    Definition {
        definition: "volatility_index",
        version: 1,
        parameters: &[
            ("method", "cboe_published_method"),
            ("target_days", "30"),
            ("time", "calendar_minutes_to_settlement_over_525600"),
            ("risk_free_rate", "ust_cmt"),
            ("rate_interpolation", "cubic_spline"),
        ],
        breakdowns: &[],
        scale: 4,
        description: "Cboe's published volatility index method on the listed strip; no option \
                      model",
    },
    Definition {
        definition: "scenario_grid",
        version: 1,
        parameters: &[
            ("surface_dynamics", "sticky_strike"),
            ("expiration", "intrinsic"),
            ("models", "american_binomial_black_scholes_black_76_by_the_package_binding"),
            ("steps", "200"),
            ("risk_free_rate", "ust_cmt"),
            ("rate_interpolation", "cubic_spline"),
        ],
        breakdowns: &["instrument", "account"],
        scale: 2,
        description: "Each position's value at each point of a scenario set, on the fitted \
                      surface",
    },
    Definition {
        definition: "positioning_exposure",
        version: 1,
        parameters: POSITIONING_LONG_CALLS_SHORT_PUTS,
        breakdowns: &["expiry", "greek", "series"],
        scale: 2,
        description: "GEX, DEX, VEX and CHEX by series and expiry; long calls and short puts",
    },
    Definition {
        definition: "positioning_exposure",
        version: 2,
        parameters: POSITIONING_SHORT_ALL,
        breakdowns: &["expiry", "greek", "series"],
        scale: 2,
        description: "GEX, DEX, VEX and CHEX by series and expiry; every series short",
    },
    Definition {
        definition: "positioning_levels",
        version: 1,
        parameters: &[
            ("positioning_basis", "convention_long_calls_short_puts"),
            ("spot_grid", "plus_or_minus_15pct_in_0.5pct_steps"),
            ("liquidity", "the_underlying_volume_times_spot"),
            ("risk_free_rate", "ust_cmt"),
            ("rate_interpolation", "cubic_spline"),
        ],
        breakdowns: &[],
        scale: 4,
        description: "The zero-gamma crossings, the gamma concentration strikes, gamma to \
                      liquidity and the 0DTE share",
    },
    Definition {
        definition: "max_pain",
        version: 1,
        parameters: &[("open_interest", "the_latest_statistic_on_or_before_the_as_of")],
        breakdowns: &["expiry"],
        scale: 4,
        description: "Each expiration's strike at which the holders' payout is least, exact",
    },
    Definition {
        definition: "open_interest_change",
        version: 1,
        parameters: &[("dates", "the_latest_trading_date_less_the_one_before")],
        breakdowns: &["series"],
        scale: 0,
        description: "Each series' open interest less the trading date's before, exact",
    },
];

/// The definition by name and version, where the component serves it.
pub fn definition(name: &str, version: i32) -> Option<&'static Definition> {
    DEFINITIONS
        .iter()
        .find(|d| d.definition == name && d.version == version)
}

/// The definition an inputs document names, as the engine computes it.
pub fn computed(name: &str, version: i32) -> Option<&'static Definition> {
    definition(name, version)
}

/// The definitions as the wire carries them.
pub fn wire() -> Vec<meridian_domain::v1::MeasureDefinition> {
    DEFINITIONS
        .iter()
        .map(|d| meridian_domain::v1::MeasureDefinition {
            definition: d.definition.to_string(),
            version: d.version,
            parameters: d
                .parameters
                .iter()
                .map(|(name, value)| meridian_domain::v1::MeasureParameter {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            breakdowns: d.breakdowns.iter().map(|b| b.to_string()).collect(),
            scale: d.scale as i32,
            description: d.description.to_string(),
        })
        .collect()
}
