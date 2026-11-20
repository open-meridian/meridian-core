//! Simulation models of the failover protocols, for the architecture review
//! after v30 (item 102; the simulation plan of 2026-11-19, layer A).
//!
//! Each model is today's design written small, as turmoil hosts against
//! fakes of the database, the stream and the bus, with the choices the
//! defects of checkpoint 5 turned on kept as named variants beside it, so a
//! checker that finds nothing on today's design is shown to find the defect
//! where the design still had it. No production code is run here and none is
//! changed: that is layer B, behind the plan's seams.
//!
//! - [`f2`]: scenario 2, the F2 window -- a fence landing between a member's
//!   `refusing` check and the publish of a write it hands on.
//! - [`groups`]: scenario 4, a writer-group raise and lower over the lake's
//!   one write-ahead stream, each group keeping its own place in it.
//! - [`moves`]: scenario 5, a partition's move between two databases with a
//!   crash at each step.
//! - [`timeline`]: scenario 6, a database timeline change during a shard's
//!   takeover: the epoch fence and the write-ahead replay.
//!
//! Every model runs a fast tier of 32 seeds with the unit tests, and a
//! longer sweep (`SIM_SEEDS`, ignored by default) run by hand. Scenario 0 --
//! the same seed is the same run -- is a test of each.
//!
//! Everything here is compiled for this crate's tests only.

#![cfg(test)]

mod world;

mod f2;
mod groups;
mod moves;
mod timeline;
