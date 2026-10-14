//! Provider adapters: the pure layer between a vendor CLI and the supervisor.
//!
//! An adapter builds a [`launch::LaunchSpec`], reads the lines the child writes through a
//! [`stream::TurnReader`], derives the turn outcome and accounts usage. It never spawns, reads a clock or performs I/O; the host
//! that runs the spec and feeds the lines lives in the supervisor. Vendor payload types are
//! the `#[serde(default)]` structs of `dudes_proto::providers`; this crate adds none.
//!
//! [`registry::Adapters`] is the one place a daemon run's adapters live: built once from the
//! operator's `[providers]` table, and asked both by the hire that admits an agent and by the
//! launch that starts its turn, so the two cannot disagree about which providers exist.

pub mod adapter;
pub mod claude;
pub mod codex;
pub mod instructions;
pub mod launch;
pub mod mcp;
pub mod pi;
pub mod protocol;
pub mod registry;
pub mod stream;
