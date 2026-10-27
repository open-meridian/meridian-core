//! The analytics component (W12; spec/the-analytics-engine-answers-behind-
//! one-api; spec/the-analytics-engines-first-release): a coordinator that
//! compiles a job, resolves its inputs on the client's behalf, splits it
//! into units and answers -- inline or later -- and a pool of stateless
//! workers taking the units from a queue group, every unit a pure node
//! named by its inputs' hash.
//!
//! It holds nothing it cannot lose ("Stateless compute, owned storage";
//! Q12): finished answers and price batches waiting are kept in memory,
//! bounded, and a restart loses them; a client runs its job again. Nothing
//! here is a record, and nothing here journals.
//!
//! Its own process and image, under core's licence; the engine
//! (`meridian-analytics-engine`) is our own Rust and nothing else computes a
//! figure (ruled 2026-10-12).

pub mod chains;
pub mod resolve;
pub mod service;
pub mod wire;
pub mod worker;

#[cfg(test)]
mod tests;

/// The component's topics and the reads it asks (deploy/topics.tsv).
pub mod topics {
    pub const RUN_JOB: &str = "platform.analytics.query.run-job";
    pub const LIST_MEASURE_DEFINITIONS: &str = "platform.analytics.query.list-measure-definitions";
    pub const PRICE_BATCH: &str = "platform.analytics.event.price-batch";
    pub const LIST_PRICE_BATCHES: &str = "platform.analytics.query.list-price-batches";
    pub const ANSWER_PRICE_BATCH: &str = "platform.analytics.command.answer-price-batch";
    pub const JOB_FINISHED: &str = "platform.analytics.event.job-finished";
    pub const LIST_FINISHED_JOBS: &str = "platform.analytics.query.list-finished-jobs";
    pub const READ_JOB_ANSWER: &str = "platform.analytics.query.read-job-answer";
    pub const LIST_MODEL_ROUTING: &str = "platform.analytics.query.list-model-routing";

    pub const LIST_POSITIONS: &str = "platform.book.query.list-positions";
    pub const LIST_ACCOUNT_ATTRIBUTES: &str = "platform.book.query.list-account-attributes";
    pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";
    pub const READ_TEMPLATE: &str = "platform.reference.query.read-template";
    pub const LIST_EVENT_GROUPS: &str = "platform.reference.query.list-event-groups";
    pub const LIST_PRICES: &str = "platform.lake.query.list-prices";
    pub const LIST_OUTCOMES: &str = "platform.lake.query.list-outcomes";
    /// Contract v27 (W6.36; ruling 483): the deployment's pivot order.
    pub const READ_FX_PIVOT_CURRENCIES: &str = "platform.config.query.read-fx-pivot-currencies";
    /// Contract v27 (W11.17; ruling 510): an `execution_cost` job's orders,
    /// their placements and executions, and an order's history.
    pub const LIST_ORDERS: &str = "platform.book.query.list-orders";
    pub const ORDER_HISTORY: &str = "platform.book.query.order-history";
    /// Contract v27 (W10.11): the execution costs a job records.
    pub const RECORD_EXECUTION_COSTS: &str = "platform.lake.command.record-execution-costs";
    /// Contract v28 (W12.1, W12.2, W12.8): a job's chains, their series,
    /// quotes, statistics, dividends and sessions, and its saved scenario.
    pub const LIST_SERIES: &str = "platform.lake.query.list-series";
    pub const LIST_QUOTES: &str = "platform.lake.query.list-quotes";
    pub const LIST_STATISTICS: &str = "platform.lake.query.list-statistics";
    pub const LIST_CORPORATE_ACTIONS: &str = "platform.lake.query.list-corporate-actions";
    pub const LIST_SCENARIOS: &str = "platform.lake.query.list-scenarios";
    pub const LIST_RATES: &str = "platform.lake.query.list-rates";
    /// Contract v28's fourth amendment (W3.1, W12.2; rulings 608 and 611):
    /// the default risk-free curve's record, by its symbol, read-only.
    pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";
    pub const READ_CALENDAR: &str = "platform.reference.query.read-calendar";
    /// Ruling 767: the datasets' declarations a read's answer leaves unset,
    /// from the lake's listing, once per job.
    pub const LIST_DATASETS: &str = "platform.lake.query.list-datasets";

    /// The component's own topic, coordinator to worker: a unit, asked of
    /// the worker pool's queue group and answered by one worker. A
    /// component topic, not a contract row (topics.md, "Analytics"): no
    /// plugin holds it, and the broker grants it to the component's own
    /// credential alone. Its name awaits the product owner's approval.
    pub const COMPUTE_UNIT: &str = "platform.analytics.internal.compute-unit";
}

/// The component's instance, every copy of it: the queue group its copies
/// share, and the publisher a store reads `on_behalf_of_instance_id` from
/// (W12.2).
pub const INSTANCE: &str = "analytics";

/// The component's version, which every answer names (provenance's
/// `engine_version`, and inside the hashed inputs).
pub fn engine_version(image_version: &str) -> String {
    if image_version.is_empty() {
        env!("CARGO_PKG_VERSION").to_string()
    } else {
        image_version.to_string()
    }
}
