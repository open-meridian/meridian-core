//! A time zone's offset from UTC at an instant: moved to
//! [`meridian_domain::zone`] (contract v26) so the book reads the deployment's
//! time zone for a held booking's `due_by` (W11.34) with the same code.

pub use meridian_domain::zone::*;
