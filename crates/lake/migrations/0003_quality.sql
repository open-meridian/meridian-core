-- Quality states (contract v28, W10.4, W10.6, W10.12; rulings 421 to 424
-- and 619; sdk-contract/lake-observations-carry-quality-states-contract).
--
-- A row's state as the edge judged it travels in its body's envelope
-- (ObservationMeta.quality, .quality_reasons), recorded as sent. A signal
-- plugin's escalation of a row version is its own record here, beside the
-- row by its dataset and sequence (decisions/031), never a rewrite: a read
-- answers the highest of the row's own state and every assessment recorded
-- by its cut-off. States only escalate; the lake refuses a lower one. The
-- kind is new in v28 and owes no backfill.

CREATE TABLE IF NOT EXISTS lake_quality_assessment (
    assessment_id bigserial  PRIMARY KEY,
    dataset       text       NOT NULL,
    sequence      bigint     NOT NULL,
    row_key       text       NOT NULL,
    version       bigint     NOT NULL CHECK (version > 0),
    -- QualityState: 2 suspicious, 3 invalid.
    state         smallint   NOT NULL CHECK (state IN (2, 3)),
    -- QualityReason values, in the order given.
    reasons       smallint[] NOT NULL,
    by_instance   text       NOT NULL,
    at_ns         bigint     NOT NULL
);
CREATE INDEX IF NOT EXISTS lake_quality_assessment_by_row
    ON lake_quality_assessment (dataset, sequence);
