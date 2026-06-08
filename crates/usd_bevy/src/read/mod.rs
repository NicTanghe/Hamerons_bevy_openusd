//! openusd-backed readers: typed decode of the composed stage into the
//! structs the Bevy projection consumes. Replaces the former `usd_schema`
//! reader crate; everything here reads through openusd only.

pub mod lux;
pub mod util;
