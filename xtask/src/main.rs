//! `cargo xtask` — repository automation.
//!
//! Commands:
//! - `layering [--strict]` — CI gate for invariant 9 (crate layering); `--strict` fails on a
//!   rule whose member crate is missing. See [`layering`].
//! - `lints` — pin each unsafe-code lint override and its source grant. See [`lints`].
//! - `env-pairs` — reject raw environment name/value representations in workspace source types.
//!   See [`env_pairs`].
//! - `schemas [--check]` — regenerate `schemas/*.json` from `dudes-proto`; `--check` diffs instead of
//!   writing. See [`schemas`].
//! - `naming` — public text carries no ticket ids, decision-record numbers or private paths. See [`naming`].
//! - `journal --state <dir>` — the journal as text, one line per record. See [`gate`].
//! - `export --state <dir> [--from-journal]` — the projection export. See [`gate`].
//! - `gate` — run the end-to-end gate transcript's three arms. See [`gate`].
//! - `blake3-pure [--target <triple>]` — the shipped hash stays on `blake3`'s assembly path; the
//!   portable implementation is a cross-check tool and never a resolved feature. See [`blake3_pure`].
//! - `dist [--target <triple>]` — the release layout: release binaries and the built Floor in one directory. See
//!   [`dist`].
//! - `release-notes --draft --from <tag>` / `release-notes --version <v> ...` — a draft changelog
//!   section, or the full GitHub Release body. See [`release_notes`].
//!
//! Commands shell out to `$CARGO` with typed argument lists; nothing is interpolated into a shell.
//!
//! Every command that reads the repository first resolves the workspace at run time through
//! [`workspace::locate`] and prints it, so the transcript states which checkout it describes. That
//! call also refuses to run a binary built from a different checkout — see [`workspace`] for why a
//! shared `CARGO_TARGET_DIR` makes that reachable through plain `cargo xtask`.

// A CLI reports on stdout/stderr by design.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod blake3_pure;
mod dist;
mod env_pairs;
mod gate;
mod gate_instructions;
mod layering;
mod lints;
mod metadata;
mod naming;
mod release_notes;
mod schemas;
mod smoke;
mod workspace;

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "layering" => {
            let strict = rest.iter().any(|a| a == "--strict");
            announce()?;
            layering::run(strict)
        }
        "lints" => {
            let workspace = announce()?;
            lints::run(&workspace)
        }
        "env-pairs" => {
            let workspace = announce()?;
            env_pairs::run(&workspace)
        }
        "schemas" => {
            let check = rest.iter().any(|a| a == "--check");
            let workspace = announce()?;
            schemas::run(&workspace, check)
        }
        "naming" => {
            let workspace = announce()?;
            naming::run(&workspace)
        }
        // The two readers below print a document a caller compares, so their banner goes to
        // stderr: stdout is the artefact.
        "journal" => {
            let state = required_path(&rest, "--state")?;
            announce_on_stderr()?;
            gate::journal(&state)
        }
        "export" => {
            let state = required_path(&rest, "--state")?;
            let source = if rest.iter().any(|a| a == "--from-journal") {
                gate::Source::Journal
            } else {
                gate::Source::Store
            };
            announce_on_stderr()?;
            gate::export(&state, source)
        }
        "gate" => {
            let workspace = announce()?;
            gate::run(&workspace)
        }
        "gate-instructions" => {
            let crash_journal = required_path(&rest, "--crash-journal")?;
            let crash_export = required_path(&rest, "--crash-export")?;
            let control_journal = required_path(&rest, "--control-journal")?;
            let control_export = required_path(&rest, "--control-export")?;
            announce_on_stderr()?;
            gate_instructions::run(
                &crash_journal,
                &crash_export,
                &control_journal,
                &control_export,
            )
        }
        "blake3-pure" => {
            // Arguments first: a usage error should not depend on finding a workspace.
            let target = blake3_pure::target(&rest)?;
            announce()?;
            blake3_pure::run(target.as_deref())
        }
        "dist" => {
            let target = dist::target(&rest)?;
            let workspace = announce()?;
            dist::run(&workspace, target.as_deref())
        }
        // The body is the artefact on stdout, so the banner goes to stderr.
        "release-notes" => {
            let mode = release_notes::parse(&rest)?;
            let workspace = announce_on_stderr()?;
            release_notes::run(&workspace, mode)
        }
        "smoke" => {
            let workspace = announce()?;
            smoke::run(&workspace)
        }
        "" | "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown command `{other}`")
        }
    }
    .with_context(|| format!("`cargo xtask {command}` failed"))
}

/// Resolve the workspace, refuse a binary from another checkout, and say which tree this run is
/// about. Printed unconditionally: a gate's output is only evidence if it names what it measured.
fn announce() -> Result<workspace::Workspace> {
    let workspace = workspace::locate()?;
    println!("xtask: workspace {workspace}");
    Ok(workspace)
}

/// [`announce`] for a command whose stdout is a machine-readable list: the same resolution and the
/// same refusal, with the banner on stderr so a caller can splice the output into a command without
/// having to filter this line back out of it.
fn announce_on_stderr() -> Result<workspace::Workspace> {
    let workspace = workspace::locate()?;
    eprintln!("xtask: workspace {workspace}");
    Ok(workspace)
}

/// The value of a required `--flag <path>` argument.
///
/// Its own function rather than an inline scan, because "the flag was given with no value" and
/// "the flag was not given" are different mistakes and a caller of a gate script needs to be told
/// which one it made.
fn required_path(args: &[String], flag: &str) -> Result<std::path::PathBuf> {
    let at = args
        .iter()
        .position(|arg| arg == flag)
        .with_context(|| format!("`{flag} <dir>` is required"))?;
    let value = args
        .get(at + 1)
        .with_context(|| format!("`{flag}` needs a value"))?;
    Ok(std::path::PathBuf::from(value))
}

fn print_usage() {
    println!(
        "usage: cargo xtask <command>\n\n\
         commands:\n  \
         layering [--strict] check crate layering (invariant 9); --strict fails on skipped rules\n  \
         lints              check unsafe lint overrides and source grants\n  \
         env-pairs          reject raw environment pairs in workspace source types\n  \
         schemas [--check]  regenerate schemas/ from dudes-proto (or diff with --check)\n  \
         naming             check that public text has no ticket ids, decision-record numbers or private paths\n  \
         journal --state <dir>\n                     \
         print the journal under <dir>, one line per record\n  \
         export --state <dir> [--from-journal]\n                     \
         print the projection export of <dir>, from its store or from a replay\n  \
         gate               run the end-to-end gate transcript: arm 1, arm 2 and the control\n  \
         gate-instructions --crash-journal <file> --crash-export <file>\n                     \
         --control-journal <file> --control-export <file>\n                     \
         validate the exact instruction histories retained by the gate\n  \
         blake3-pure [--target <triple>]\n                     \
         check that blake3's `pure` feature is not enabled in the resolved graph\n  \
         dist [--target <triple>]\n                     \
         build the release binaries and lay them out with floor/dist in\n                     \
         target/dist/dudes-<version>/\n  \
         release-notes --draft --from <tag>\n                     \
         print a draft CHANGELOG.md section from the pull requests since <tag>\n  \
         release-notes --version <v> [--from <tag>] --repo <o/r> --run-url <url> --sums <file> --maintainer <login>\n                     \
         [--authors <file>] [--preview]  print the GitHub Release body\n  \
         smoke              verify release install, quickstart, browser hire, the newcomer journey and cleanup"
    );
}
