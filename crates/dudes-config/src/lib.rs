//! The edit code for the operator's config file that both the CLI and the daemon need.
//!
//! [`config`] reads and edits the settings keys and refuses an edit that breaks the file's own
//! start rules. [`workspace`] enrolls and removes projects. Both keep the file's other bytes and
//! comments as they were and write through a temporary file renamed over the original.

pub mod config;
pub mod workspace;
