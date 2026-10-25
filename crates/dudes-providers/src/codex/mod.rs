//! Codex CLI in managed mode: `codex exec --json`, one event object per line.

pub mod app_server;
pub mod config;
pub mod login;
pub mod managed;
pub mod model_list;
pub mod startup;
mod steps;
pub mod stream;
pub mod version;

pub use managed::CodexAdapter;

/// A value Codex reported, cut to 64 characters and escaped, for a refusal a person reads.
fn reported(value: &str) -> String {
    const SHOWN: usize = 64;
    let mut shown: String = value
        .chars()
        .take(SHOWN)
        .flat_map(char::escape_debug)
        .collect();
    if value.chars().nth(SHOWN).is_some() {
        shown.push('…');
    }
    shown
}
