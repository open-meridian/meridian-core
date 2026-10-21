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

/// v26's model ids, each at version 1 (the first release's §1).
pub const MODELS: [&str; 3] = ["cash", "quoted_price", "binary_event"];

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

/// Every definition the component serves, in the order it lists them:
/// v26's at version 1, and from contract v27 `execution_cost` version 1 and
/// `pnl` version 2.
pub const DEFINITIONS: [Definition; 10] = [
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
