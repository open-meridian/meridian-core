//! The runtime's integration tests, as one binary.
//!
//! Each `tests/` file used to be its own binary, and each linked the whole
//! runtime and everything under it again: five copies in the gate's target
//! cache and five more in the image its test stage runs (ruling 354, keeping
//! the tests stage at least 2 GB under its 15 GB budget on CI). One binary
//! links it once; a module here is what a file was, and a target picks its
//! module by name:
//!
//! - `wiring`, which stands up its own world in-process: Dockerfile.rust's
//!   `test` stage, `run-tests -p meridian-runtime --test it -- wiring::`;
//! - `grants`, `waiting` and `preflight`, against Postgres: `make test-store`;
//! - `end_to_end` and `outage`, against a running platform: `make demo`.
//!
//! A filter names the module with its `::`, so it matches only that module's
//! tests, and a module added here is named by a target or runs nowhere, as a
//! new `tests/` file was (tools/check_test_targets.py).

mod end_to_end;
mod grants;
mod outage;
mod preflight;
mod waiting;
mod wiring;
