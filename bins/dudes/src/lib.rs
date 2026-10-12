//! `dudes`: one binary that is the operator's door and the agent's door.
//!
//! # Which door it is depends on nothing the caller types
//!
//! With an operator credential readable beside the socket it is the operator's door; with an agent
//! token in its environment it is the agent's. Neither is a flag and neither is a subcommand, and
//! the daemon is what decides what either presentation *means*: which role it resolves to
//! (`dudes_rpc::auth::classify`) and which methods that role may call (each method's
//! `Requirement`). Neither is restated here: a CLI carrying its own copy of either would answer
//! differently from the daemon the day the daemon changed, and nothing would go red.
//!
//! # This crate is a client
//!
//! It links the wire types (`dudes-proto`), the transport, the vocabulary, a JSON encoder, the provider crate's
//! redacting environment-variable type and the Windows DACL/filesystem leaf. It journals nothing,
//! decides nothing, owns no child, folds no projection and runs no runtime — `cargo xtask
//! layering`'s `dudes` row is the enforcement, and this package's dependency list is what that row
//! reads. The three programs it starts are let go or replace it, so none is its child.
//!
//! # The two functions the door is made of
//!
//! [`credential::read_operator_credential`] opens the credential file and returns an
//! `OperatorCredential`; [`call::call_presenting`] takes that value and puts it on the envelope. Two
//! separately-named functions with a value between them, never one that opens a path and returns a
//! `Response`. `tests/door_shape.rs` checks it.
//!
//! **That shape also keeps the wire type path-free.** A grep counts path- and file-shaped tokens
//! in `crates/dudes-proto/src/rpc/cred.rs`, and this package adds none, because the wire type
//! never learns where it came from.
//!
//! # Nothing is interpolated into a shell
//!
//! Invariant 7. A goal's text, an agent's name and a predicate's argv are typed values carried as
//! parameters over the socket. Only the operator's editor selection may resolve a program on
//! the CLI's PATH. Quickstart forwards an allowlisted PATH for daemon-owned programs.
//!
//! The CLI starts programs from three item-scoped grants with an expiring `#[expect]`:
//! quickstart's daemon, an absolute `duded` path with no arguments; and an engine's own
//! login, from the absolute path in the daemon's `system.doctor` row; and the operator's selected
//! editor, with the recorded directory. `tests/no_interpreter.rs`
//! confines the command types to those files, lists each item's admitted calls, and keeps
//! every interpreter route absent, with planted controls that prove the scan fires. The programs
//! themselves may be scripts; what Dudes composes is typed argv and no shell string.
//!
//! # Why there is a library target
//!
//! `bins/duded`'s reason, one step over: an exit code is a property of a process, so the door
//! tests have to run the real binary as a subprocess through `env!("CARGO_BIN_EXE_dudes")` — and
//! everything *else* worth asserting is a property of a function, which a bin-only package would put
//! out of an integration test's reach.

pub mod args;
pub mod call;
mod checkout;
pub mod config;
pub mod credential;
mod editor;
pub mod env;
pub mod exit;
pub mod floor;
pub mod login;
mod mail;
mod quickstart;
pub mod render;
pub mod run;
pub mod setup;
pub mod workspace;

pub use call::call_presenting;
pub use credential::{CredentialError, read_operator_credential};
pub use env::Environment;
pub use run::run;
