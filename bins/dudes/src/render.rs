//! Output for a human at a terminal, and output for a script.
//!
//! # Two modes, one source
//!
//! `--json` writes command results as the contract's own types. The daemon's value is deserialised
//! into `dudes_proto` and re-serialised. A caller parses `HireResult`, `WhyResult`, `PingResult` or
//! `JournalAppendedParams` against `schemas/rpc.schema.json` and never scrapes a line of prose.
//! Recovery submission has one client-only exception. A missing or malformed reply produces
//! `{"state":"reply_unavailable","request_id":"..."}` rather than a daemon result. The retained ID
//! supports a separate receipt read, and the CLI does not resend the submission.
//!
//! The human mode is prose and is not a contract. Nothing should parse it, which is why the
//! machine-readable mode exists at all.
//!
//! # Everything writes through `io::Write`, not through `println!`
//!
//! The workspace lints `clippy::print_stdout` and `clippy::print_stderr` to `warn`, and a CLI that
//! answered them with an `#[allow]` would be suppressing the lint rather than the reason for it.
//! Writing through a handle is what the lint is *for*: it makes the destination an argument, so
//! every function here is testable in-process against a `Vec<u8>` and the binary is the only thing
//! that ever names a real stream.
//!
//! # Text from elsewhere is escaped
//!
//! Every name, role, reason, id or path the human mode prints from the daemon or a file goes
//! through [`Escaped`], so a terminal shows its control characters instead of obeying them.
//! `--json` writes the same text exactly, and a script reads it as data.
//!
//! # A secret is never rendered
//!
//! Nothing in this module takes an `OperatorCredential` or a `HookToken`. That is structural rather
//! than careful: both types are redacted in `Debug`, neither has `Display`, and neither reaches any
//! signature here. `tests/token.rs` asserts the outcome over a real process, with the socket path as
//! the control that proves the capture would have seen a leak.

use std::io::{self, Write};
use std::path::Path;

use chrono::TimeZone;
use dudes_proto::decision::AutomaticHold;
use dudes_proto::decision::LoopDecision;
use dudes_proto::rpc::agent::{HireResult, McpResult, WhyResult};
use dudes_proto::rpc::board::CardCreateResult;
use dudes_proto::rpc::floor::{EnrollmentReloadResult, SessionIssueResult};
use dudes_proto::rpc::goal::GoalSetResult;
use dudes_proto::rpc::journal::JournalAppendedParams;
use dudes_proto::rpc::system::PingResult;
use dudes_proto::rpc::{Empty, RpcError};
use dudes_proto::{AgentView, CardView};
pub use dudes_types::Escaped;
use dudes_types::{Lsn, Provider};
use serde::Serialize;

use crate::config;
use crate::floor::FloorUrl;

pub mod ticket;

/// Explain the selected context without promising restored history or exactly-once effects.
pub fn recovery_choice_warning(
    out: &mut dyn Write,
    choice: dudes_proto::rpc::recovery::RecoveryChoice,
) -> io::Result<()> {
    use dudes_proto::rpc::recovery::RecoveryChoice;
    writeln!(
        out,
        "{}",
        match choice {
            RecoveryChoice::TryRecorded =>
                "Try recorded conversation. A matching identifier does not prove provider history was restored.",
            RecoveryChoice::FreshWithRecap =>
                "Fresh context does not restore old conversation history. Its bounded recap may be shortened or omitted.",
            RecoveryChoice::RepeatIssuedFresh =>
                "Repeat the original issued task in fresh context. A child may have executed before its Spawn was recorded; prior effects may repeat.",
        }
    )
}

/// Render one inspected opportunity or its fixed unavailable reason.
pub fn recovery_inspection(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::recovery::RecoveryInspectResult,
) -> io::Result<()> {
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectResult,
    };
    if machine {
        return json(out, result);
    }
    match result {
        RecoveryInspectResult::Unavailable { reason } => {
            writeln!(out, "Recovery unavailable: {reason:?}.")
        }
        RecoveryInspectResult::Available {
            through,
            expected,
            reason,
            choices,
            prior_effects_unknown,
            waiting,
        } => {
            writeln!(
                out,
                "Recovery available for agent {}, goal {}, turn {}.",
                expected.work.agent, expected.work.goal, expected.work.turn
            )?;
            writeln!(
                out,
                "Checked through {through}; issue {}; source {}; reason {reason:?}.",
                expected.issue_lsn, expected.source_lsn
            )?;
            writeln!(
                out,
                "Admission waits: question={:?}, budget={:?}, hops={:?}, instruction review={:?}, instruction comparison={:?}.",
                waiting.open_question,
                waiting.budget,
                waiting.hops,
                waiting.instruction_review,
                waiting.instruction_comparison
            )?;
            match choices {
                RecoveryAllowedChoices::FreshOnly => {
                    recovery_choice_warning(out, RecoveryChoice::FreshWithRecap)?;
                }
                RecoveryAllowedChoices::RecordedOrFresh => {
                    recovery_choice_warning(out, RecoveryChoice::TryRecorded)?;
                    recovery_choice_warning(out, RecoveryChoice::FreshWithRecap)?;
                }
                RecoveryAllowedChoices::RepeatIssuedFresh => {
                    recovery_choice_warning(out, RecoveryChoice::RepeatIssuedFresh)?;
                }
            }
            if *prior_effects_unknown {
                writeln!(out, "Prior effects are unknown and may repeat.")?;
            }
            writeln!(
                out,
                "Save and review `dudes --json recovery inspect {}` before submitting a choice.",
                expected.work.agent
            )
        }
    }
}

fn recovery_recorded(
    out: &mut dyn Write,
    receipt: &dudes_proto::rpc::recovery::RecoveryReceiptView,
) -> io::Result<()> {
    use dudes_proto::rpc::recovery::RecoveryReceiptState;
    writeln!(
        out,
        "Recovery authorization recorded at {}. Authorization does not promise that work launched.",
        receipt.claim.authorization_lsn
    )?;
    match &receipt.status {
        RecoveryReceiptState::Pending => {
            writeln!(out, "Pending: the reserved turn has not been issued.")?;
        }
        RecoveryReceiptState::IssueBound { turn, issue_lsn } => writeln!(
            out,
            "Issue-bound: turn {turn}, issue {issue_lsn}; no consuming physical receipt is recorded."
        )?,
        RecoveryReceiptState::Consumed {
            turn,
            issue_lsn,
            evidence_lsn,
        } => writeln!(
            out,
            "Consumed: turn {turn}, issue {issue_lsn}, physical receipt {evidence_lsn}."
        )?,
        RecoveryReceiptState::Invalidated {
            invalidation_lsn,
            reason,
        } => writeln!(out, "Invalidated at {invalidation_lsn}: {reason:?}.")?,
        RecoveryReceiptState::Superseded { barrier_lsn, .. } => writeln!(
            out,
            "Superseded by a new recovery episode at {barrier_lsn}; the owed issue is retained."
        )?,
    }
    recovery_choice_warning(out, receipt.choice)?;
    if receipt.prior_effects_unknown {
        writeln!(out, "Prior effects are unknown and may repeat.")?;
    }
    Ok(())
}

/// Render a typed daemon submission result.
pub fn recovery_submitted(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::recovery::RecoverySubmitResult,
) -> io::Result<()> {
    use dudes_proto::rpc::recovery::RecoverySubmitResult;
    if machine {
        return json(out, result);
    }
    match result {
        RecoverySubmitResult::Accepted { receipt } => recovery_recorded(out, receipt),
        RecoverySubmitResult::Refused { reason, prefix } => {
            writeln!(out, "Recovery refused: {reason:?}.")?;
            if let Some(required) = prefix.required {
                writeln!(out, "Required barrier already acknowledged at {required}.")?;
            }
            if let Some(resume) = prefix.resume {
                writeln!(out, "Resume already acknowledged at {resume}.")?;
            }
            Ok(())
        }
        RecoverySubmitResult::Unknown { .. } => writeln!(
            out,
            "The daemon could not certify whether recovery was authorized. Read the retained request's receipt."
        ),
    }
}

/// Render a client-side missing reply separately from a daemon-produced unknown result.
pub fn recovery_reply_unavailable(
    out: &mut dyn Write,
    machine: bool,
    request_id: dudes_proto::record::Hex32,
) -> io::Result<()> {
    if machine {
        return json(
            out,
            &serde_json::json!({"state":"reply_unavailable","request_id":request_id}),
        );
    }
    writeln!(
        out,
        "Recovery reply unavailable. Authorization was not confirmed. Read the retained request's receipt."
    )
}

/// Render a historical receipt without treating absence as a rollback of earlier steps.
pub fn recovery_receipt(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::recovery::RecoveryReceiptResult,
) -> io::Result<()> {
    use dudes_proto::rpc::recovery::RecoveryReceiptResult;
    if machine {
        return json(out, result);
    }
    match result {
        RecoveryReceiptResult::Found { receipt } => recovery_recorded(out, receipt),
        RecoveryReceiptResult::Absent { checked_through } => writeln!(
            out,
            "No authorization at checked cut {checked_through}. A Required or Resume prefix may still have been recorded."
        ),
        RecoveryReceiptResult::Unavailable { reason } => writeln!(
            out,
            "Recovery receipt unavailable: {reason:?}. Neither presence nor absence is certified."
        ),
    }
}

/// Render the durable memory append receipt.
pub fn memory_appended(
    out: &mut dyn Write,
    machine: bool,
    result: dudes_proto::rpc::memory::MemoryAppendResult,
) -> io::Result<()> {
    if machine {
        json(out, &result)
    } else {
        writeln!(out, "memory appended at {}", position(result.lsn))
    }
}

/// Render a memory page without interpreting terminal controls in its text.
pub fn memory_read(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::memory::MemoryReadResult,
) -> io::Result<()> {
    if machine {
        json(out, result)
    } else {
        if result.lsn == Lsn::ZERO {
            writeln!(out, "no memory records in this page")?;
        } else {
            writeln!(out, "memory through {}", position(result.lsn))?;
        }
        writeln!(out, "{}", Escaped(&result.text))
    }
}

/// Render labelled search hits and live continuation without interpreting terminal controls.
pub fn memory_search(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::memory::MemorySearchResult,
) -> io::Result<()> {
    use dudes_proto::rpc::memory::MemorySearchHit;
    if machine {
        return json(out, result);
    }
    if result.hits.is_empty() {
        writeln!(out, "no matches in this page")?;
    }
    for hit in &result.hits {
        match hit {
            MemorySearchHit::Raw {
                lsn,
                match_offset,
                text,
                text_offset,
                source_bytes,
                truncated,
                read_since_lsn,
            } => {
                writeln!(
                    out,
                    "raw memory at {}: match byte {match_offset}; text starts at byte {text_offset}; {source_bytes} source bytes{}",
                    position(*lsn),
                    if *truncated { "; excerpt" } else { "" }
                )?;
                writeln!(out, "{}", Escaped(text))?;
                if *source_bytes > dudes_rpc::methods::memory::MAX_MEMORY_READ_TEXT_BYTES {
                    writeln!(out, "too large for a whole-entry memory read")?;
                } else {
                    writeln!(out, "raw read cursor: {read_since_lsn}")?;
                }
            }
            MemorySearchHit::Summary {
                lsn,
                match_offset,
                text,
                covered_from_lsn,
                covered_to_lsn,
                read_since_lsn,
                ..
            } => {
                writeln!(
                    out,
                    "summary at {}: match byte {match_offset}; covers {covered_from_lsn} through {covered_to_lsn}",
                    position(*lsn)
                )?;
                writeln!(out, "{}", Escaped(text))?;
                if let Some(cursor) = read_since_lsn {
                    writeln!(
                        out,
                        "read original range after {cursor}; the summary position is not a raw read cursor"
                    )?;
                } else {
                    writeln!(
                        out,
                        "read original range from the start; the summary position is not a raw read cursor"
                    )?;
                }
            }
        }
    }
    if let Some(cursor) = result.next_since_lsn {
        writeln!(
            out,
            "more candidates; continue search with --since-lsn {cursor}"
        )?;
    } else {
        writeln!(out, "search exhausted at this call's state")?;
    }
    Ok(())
}

/// Render the bounded mailbox observations with terminal-safe filename labels.
///
/// # Errors
/// Returns the output write error.
pub fn mailbox_report(
    out: &mut dyn Write,
    report: &dudes_proto::rpc::system::MailboxReport,
) -> io::Result<()> {
    if let Some(detail) = &report.detail {
        writeln!(out, "{}", Escaped(detail))?;
    }
    for row in &report.diagnostics {
        writeln!(
            out,
            "mailbox {} {}: {:?}",
            row.agent,
            Escaped(&row.filename),
            row.problem
        )?;
    }
    if report.truncated {
        writeln!(out, "mailbox diagnostics truncated")?;
    }
    Ok(())
}

/// Write `value` as one line of JSON.
///
/// # Errors
///
/// Whatever writing returns, and a serialisation failure as [`io::Error`].
pub fn json<T: Serialize>(out: &mut dyn Write, value: &T) -> io::Result<()> {
    let line = serde_json::to_string(value).map_err(io::Error::other)?;
    writeln!(out, "{line}")
}

/// `system.ping`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn ping(out: &mut dyn Write, machine: bool, result: &PingResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(
        out,
        "duded {} on {}",
        Escaped(&result.version),
        Escaped(&result.socket.display().to_string())
    )?;
    if result.lsn == Lsn::ZERO {
        return writeln!(out, "journal has no records yet");
    }
    writeln!(out, "journal at {}", position(result.lsn))
}

fn position(lsn: Lsn) -> String {
    format!("segment {} · byte {}", lsn.segment(), lsn.offset())
}

/// Render operator diagnostics and any stopped-journal recovery guidance.
///
/// # Errors
///
/// Whatever writing or serialization returns.
pub fn doctor(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::system::DoctorJournalResult,
) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    ping(out, false, &result.doctor.ping)?;
    if let Some(stopped) = result.journal_stopped {
        writeln!(out, "{stopped}")?;
    }
    crate::setup::report_engines(out, &result.doctor.providers)
}

pub(crate) fn doctor_json(out: &mut dyn Write, value: &serde_json::Value) -> io::Result<()> {
    let text = serde_json::to_string(value).map_err(io::Error::other)?;
    for character in text.chars() {
        let raw = character.to_string();
        if Escaped(&raw).to_string() == raw {
            write!(out, "{character}")?;
        } else {
            write!(out, "\\u{:04x}", u32::from(character))?;
        }
    }
    writeln!(out)
}

/// `agent.hire`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn hired(out: &mut dyn Write, machine: bool, result: &HireResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(out, "hired {}", result.agent)
}

/// `worktree.list`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn worktrees(
    out: &mut dyn Write,
    machine: bool,
    entries: &[dudes_proto::rpc::worktree::WorktreeEntry],
) -> io::Result<()> {
    if machine {
        return json(out, &entries);
    }
    if entries.is_empty() {
        return writeln!(out, "no worktrees; hire with `dudes hire --worktree`");
    }
    for entry in entries {
        writeln!(
            out,
            "{}  {:?}  {}  {}  {}",
            entry.worktree,
            entry.state,
            entry
                .agent
                .map_or_else(|| "-".to_owned(), |agent| agent.to_string()),
            Escaped(&entry.branch),
            Escaped(&entry.path.display().to_string()),
        )?;
    }
    Ok(())
}

/// What `worktree remove` prints while the tree still exists: the command to run, with every
/// argument single-quoted so a path with a space or a quote reads back exactly.
///
/// # Errors
///
/// Whatever writing returns.
pub fn worktree_remove_command(
    err: &mut dyn Write,
    origin: &std::path::Path,
    path: &std::path::Path,
) -> io::Result<()> {
    fn quoted(text: &std::path::Path) -> String {
        format!(
            "'{}'",
            Escaped(&text.display().to_string())
                .to_string()
                .replace('\'', "'\\''")
        )
    }
    writeln!(
        err,
        "the tree still exists. Removing it deletes ignored files such as a .env, a local database or build output.\n\
         Check what is in it with:\n  git -C {} status --ignored\n\
         Review the tree for uncommitted work (?? untracked, M modified, !! ignored files that removal deletes), commit or copy out what you want to keep, then remove it with your own git and run this command again:\n  git -c core.fsmonitor=false -c core.hooksPath=/dev/null -C {} worktree remove -- {}",
        quoted(path),
        quoted(origin),
        quoted(path)
    )
}

/// What `worktree remove` prints for a lease whose removal is already recorded.
///
/// # Errors
///
/// Whatever writing returns.
pub fn worktree_already_removed(
    out: &mut dyn Write,
    machine: bool,
    worktree: dudes_types::WorktreeId,
) -> io::Result<()> {
    if machine {
        return json(
            out,
            &serde_json::json!({"worktree": worktree, "already_removed": true}),
        );
    }
    writeln!(out, "worktree {worktree} was already removed")
}

/// The note a removal prints.
///
/// # Errors
///
/// Whatever writing returns.
pub fn worktree_removed(
    out: &mut dyn Write,
    machine: bool,
    worktree: dudes_types::WorktreeId,
    branch: &str,
) -> io::Result<()> {
    if machine {
        return json(
            out,
            &serde_json::json!({"worktree": worktree, "branch": branch}),
        );
    }
    writeln!(
        out,
        "removed worktree {worktree}; branch {} is kept",
        Escaped(branch)
    )
}

/// `goal.set`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn goal_set(out: &mut dyn Write, machine: bool, result: &GoalSetResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(out, "goal {}", result.goal)
}

/// The line `dudes goal` prints before a call whose check the daemon runs first. The daemon sees
/// the connection close on every platform, so Ctrl-C cancels everywhere.
#[must_use]
pub fn goal_check_runs_first(timeout_s: u64) -> String {
    format!(
        "Running the completion check once before the first turn, to see whether it already \
         passes (at most {timeout_s} s). Ctrl-C before the check finishes cancels without \
         setting the goal."
    )
}

/// What `dudes goal` says for a refusal that its check run or its admission gave, with the
/// remedy this CLI offers; `None` for any other error, which renders as it always has.
#[must_use]
pub fn goal_refusal(error: &RpcError) -> Option<String> {
    use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData};
    if error.code == ErrorCode::NotFound.code() {
        return Some(format!("Refused: {}.", error.message));
    }
    let gate = error
        .data
        .clone()
        .and_then(|data| serde_json::from_value::<RefusedData>(data).ok())?
        .gate;
    let hint = match gate {
        RefusalGate::CheckAlreadyPasses => {
            " Use a check that fails until the goal is met, or add --check-may-already-pass if \
             Done should mean only that the check still passes after the agent's work."
        }
        RefusalGate::CheckDidNotFinish => " Raise its timeout, or add --check-may-already-pass.",
        RefusalGate::OpenTurn => " Try again when it ends, or add --check-may-already-pass.",
        RefusalGate::CheckCannotRun => "",
        _ => return None,
    };
    Some(format!("Refused: {}.{hint}", error.message))
}

/// Render the replacement state without claiming that a turn or check ran.
pub fn goal_replaced(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::goal::GoalReplaceResult,
) -> io::Result<()> {
    use dudes_proto::rpc::goal::GoalReplaceResult;
    if machine {
        return json(out, result);
    }
    match result {
        GoalReplaceResult::Changed {
            goal,
            completion_check,
            ..
        } => writeln!(
            out,
            "Goal {goal} recorded with check {}. No automatic resume was performed.",
            Escaped(&completion_check.label)
        ),
        GoalReplaceResult::Pending { .. } => {
            writeln!(out, "Replacement pending. Retry the same request file.")
        }
        GoalReplaceResult::Refused { reason, .. } => writeln!(
            out,
            "Replacement refused: {}. No replacement was appended.",
            serde_json::to_value(reason)
                .map_err(io::Error::other)?
                .as_str()
                .unwrap_or("unknown")
        ),
    }
}

/// Explain the existing targeted loop control after its acknowledgement.
pub fn loop_control(
    out: &mut dyn Write,
    machine: bool,
    resume: bool,
    result: Empty,
) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    if resume {
        writeln!(
            out,
            "Resume recorded. Pause, halt and automatic hold are cleared."
        )
    } else {
        writeln!(
            out,
            "Pause recorded. Running turns and checks may still be unsettled; already-issued replay work is not canceled."
        )
    }
}

/// Explain a halt after its acknowledgement, without claiming completed teardown.
///
/// # Errors
///
/// Whatever writing returns.
pub fn halted(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(
        out,
        "Halt recorded. Owned child teardown may still be in progress. Use resume to clear the halt."
    )
}

/// Explain a retirement after its acknowledgement, without claiming completed teardown.
///
/// # Errors
///
/// Whatever writing returns.
pub fn retired(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(
        out,
        "Retirement recorded. Owned child teardown may still be in progress."
    )
}

/// Render the newly created card id.
///
/// # Errors
///
/// Whatever writing returns.
pub fn card_created(
    out: &mut dyn Write,
    machine: bool,
    result: &CardCreateResult,
) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(out, "card {}", result.card)
}

/// Render complete current cards, including the sequence needed for a later move.
///
/// # Errors
///
/// Whatever writing returns.
pub fn cards(out: &mut dyn Write, machine: bool, cards: &[CardView]) -> io::Result<()> {
    if machine {
        return json(out, &cards);
    }
    for card in cards {
        writeln!(
            out,
            "{}  seq {}  {:?}  {}  {}",
            card.id,
            card.seq,
            card.status,
            Escaped(&card.department),
            Escaped(&card.title)
        )?;
    }
    Ok(())
}

/// `agent.list`: one line per agent, or one line saying there is none.
///
/// # Errors
///
/// Whatever writing returns.
pub fn agents(out: &mut dyn Write, machine: bool, agents: &[AgentView]) -> io::Result<()> {
    if machine {
        return json(out, &agents);
    }
    if agents.is_empty() {
        return writeln!(
            out,
            "no agents are hired; hire one with `dudes hire` or on the Floor"
        );
    }
    for agent in agents {
        let standing = match (agent.halted, agent.paused) {
            (true, _) => "  halted",
            (false, true) => "  paused",
            (false, false) => "",
        };
        let status = if !agent.halted
            && !agent.paused
            && agent.mode != dudes_types::Mode::Attached
            && agent.automatic_hold.is_some()
        {
            "Hold".to_owned()
        } else {
            format!("{:?}", agent.status)
        };
        writeln!(
            out,
            "{}  {}  {}  {:?}  {status}{standing}",
            agent.id,
            Escaped(&agent.name),
            Escaped(&agent.role),
            agent.provider
        )?;
        if let Some(cause) = &agent.automatic_hold {
            writeln!(out, "  held: {}", Escaped(&cause.to_string()))?;
            if let Some(hint) = missing_engine_hint(agent.provider, cause, &agent.id.to_string()) {
                writeln!(out, "  {hint}")?;
            }
        }
    }
    Ok(())
}

/// The engines a newcomer may hire and the program the daemon looks for.
const ENGINES: [(Provider, &str, &str); 3] = [
    (Provider::Claude, "Claude Code", "claude"),
    (Provider::Codex, "Codex", "codex"),
    (Provider::Pi, "Pi", "pi"),
];

/// What to do about a launch that failed with a not-found error. The recorded reason ends in the
/// error kind and names no engine, and other not-found errors share it, so the program is only
/// the usual cause.
fn missing_engine_hint(provider: Provider, cause: &AutomaticHold, agent: &str) -> Option<String> {
    let AutomaticHold::LaunchFailed { reason } = cause else {
        return None;
    };
    if !reason.ends_with("(NotFound)") {
        return None;
    }
    let (_, name, program) = ENGINES.into_iter().find(|(known, ..)| *known == provider)?;
    Some(format!(
        "Dudes could not start {name}, usually because `{program}` is not installed. Install it and sign in, then run `dudes resume {agent}`, or hire an agent with an engine you have."
    ))
}

/// Render the public roster as JSON or escaped human text.
///
/// # Errors
/// Whatever writing returns.
pub fn team(
    out: &mut dyn Write,
    machine: bool,
    rows: &[dudes_proto::rpc::team::RosterEntry],
) -> io::Result<()> {
    if machine {
        return json(out, &rows);
    }
    if rows.is_empty() {
        return writeln!(out, "no agents are hired");
    }
    for row in rows {
        let status = match row.status {
            dudes_proto::rpc::team::RosterStatus::Idle => "idle",
            dudes_proto::rpc::team::RosterStatus::Working => "working",
            dudes_proto::rpc::team::RosterStatus::Done => "done",
        };
        let card = row
            .held_card
            .map_or_else(|| "none".to_owned(), |id| id.to_string());
        writeln!(
            out,
            "{}  {}  {}  {}  {status}  {card}",
            row.id,
            Escaped(&row.name),
            Escaped(&row.role),
            Escaped(&row.department)
        )?;
    }
    Ok(())
}

/// Report a best-effort pause over the current roster, including each daemon refusal.
pub fn pause_all(
    out: &mut dyn Write,
    paused: usize,
    refusals: &[(String, String)],
) -> io::Result<()> {
    writeln!(out, "Paused {paused} agents.")?;
    for (name, reason) in refusals {
        writeln!(out, "Refused {}: {}", Escaped(name), Escaped(reason))?;
    }
    Ok(())
}

/// `system.shutdown`, answered and awaited.
///
/// # Errors
///
/// Whatever writing returns.
pub fn stopped(
    out: &mut dyn Write,
    machine: bool,
    result: &dudes_proto::rpc::system::ShutdownResult,
) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(out, "the daemon stopped")
}

/// Render the exact final closing census.
///
/// # Errors
/// Whatever writing returns.
pub fn closing_completed(
    out: &mut dyn Write,
    machine: bool,
    report: &dudes_proto::rpc::closing::ClosingReport,
) -> io::Result<()> {
    if machine {
        return json(out, report);
    }
    writeln!(out, "the daemon stopped")?;
    writeln!(
        out,
        "closing time acknowledgements: {} of {}",
        report.acknowledged.len(),
        report.participants.len()
    )?;
    if report.settled.is_empty() {
        return Ok(());
    }
    let settled = report
        .settled
        .iter()
        .map(|settled| format!("{} ({})", settled.agent, settled.reason.describe()))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        out,
        "closing time settled without acknowledgement: {settled}"
    )
}

/// Render an own acknowledgement without claiming a save outcome.
///
/// # Errors
/// Whatever writing returns.
pub fn closing_acknowledged(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(out, "closing time acknowledged")
}

/// Render a durable claim or status change.
///
/// # Errors
///
/// Whatever writing returns.
pub fn card_updated(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(out, "card updated")
}

/// `agent.ask`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn asked(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(out, "question recorded")
}

/// `agent.why`: the decision, then the gates in the order they were consulted.
///
/// The gate trace is printed whole rather than summarised to the gate that fired, because invariant
/// 5 fixes the order — control → provenance → human → done → budget → hops → work — and which
/// gates were consulted before this one is the question `why` exists to answer.
///
/// # Errors
///
/// Whatever writing returns.
pub fn why(out: &mut dyn Write, machine: bool, result: &WhyResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(
        out,
        "journal {} · input {}",
        position(result.lsn),
        Escaped(&result.input_digest)
    )?;
    writeln!(out, "decision: {:?}", result.decision)?;
    for step in &result.gate_trace {
        let fired = if step.fired { "→" } else { " " };
        match &step.note {
            Some(note) => writeln!(out, "  {fired} {:?}: {}", step.gate, Escaped(note))?,
            None => writeln!(out, "  {fired} {:?}", step.gate)?,
        }
    }
    if matches!(result.decision, LoopDecision::Wait { .. }) {
        writeln!(
            out,
            "answer it in the Floor's Questions view: `dudes floor open` prints the link, and floor.answer must be on"
        )?;
    }
    Ok(())
}

/// `floor.session.issue`, as the one URL to open.
///
/// # The bearer is written once, on stdout, in either mode
///
/// A secret is barred from a query, an argv or a log, with exactly one
/// disclosure allowed: the one fragment-bearing bootstrap URL may be printed once by the requesting
/// CLI. So the human line prints [`FloorUrl::printed_once`] once and the prose beside it names
/// only [`FloorUrl::page`], which carries no bearer; `--json` writes the contract's own
/// `SessionIssueResult`, whose `bootstrap` field is that same single disclosure in the shape a
/// script parses. Neither mode writes it to stderr, and this binary links no subscriber
/// (`src/main.rs`), so those two streams are the whole of its sinks.
///
/// # Errors
///
/// Whatever writing returns.
pub fn floor_url(
    out: &mut dyn Write,
    machine: bool,
    result: &SessionIssueResult,
    urls: &FloorUrl,
) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(out, "{}", urls.printed_once)?;
    writeln!(
        out,
        "Paste this link into any browser within {} seconds. It works once; run dudes floor open \
         for a new one.",
        result.expires_in_s
    )
}

/// `floor.enrollment.reload`: the file's hash, the change, and whether it was applied.
///
/// # Errors
///
/// Whatever writing returns.
pub fn reload(
    out: &mut dyn Write,
    machine: bool,
    result: &EnrollmentReloadResult,
) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    let list = |ids: &[String]| {
        if ids.is_empty() {
            "none".to_owned()
        } else {
            Escaped(&ids.join(", ")).to_string()
        }
    };
    let changed: Vec<String> = result
        .changed
        .iter()
        .map(|entry| format!("{} ({})", entry.id, entry.fields.join(", ")))
        .collect();
    writeln!(
        out,
        "file     {}",
        Escaped(
            result
                .path
                .as_deref()
                .unwrap_or("(a path that is not UTF-8)")
        )
    )?;
    writeln!(out, "sha256   {}", Escaped(&result.sha256))?;
    writeln!(out, "added    {}", list(&result.added))?;
    writeln!(out, "removed  {}", list(&result.removed))?;
    writeln!(out, "changed  {}", list(&changed))?;
    if !result.switches.is_empty() {
        writeln!(out, "switches {}", list(&result.switches))?;
    }
    if !result.not_applied.is_empty() {
        writeln!(
            out,
            "needs a daemon restart: {}",
            Escaped(&result.not_applied.join(", "))
        )?;
    }
    writeln!(out, "running  {}", list(&result.workspaces))?;
    match result.refused {
        Some(reason) => writeln!(
            out,
            "declined ({}); nothing was applied",
            serde_json::to_value(reason)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
        None if result.applied => writeln!(out, "applied"),
        None if result.added.is_empty()
            && result.removed.is_empty()
            && result.changed.is_empty()
            && result.switches.is_empty()
            && result.not_applied.is_empty() =>
        {
            writeln!(
                out,
                "nothing to apply; the file and the running daemon agree"
            )
        }
        None => writeln!(
            out,
            "not applied; run `dudes project reload --apply {}` to apply this change",
            Escaped(&result.sha256)
        ),
    }
}

/// `dudes project list`: each id the file enrolls or the daemon runs, and where it is, or one
/// line naming the file when there is none.
///
/// # Errors
///
/// Whatever writing returns.
pub fn workspaces(
    out: &mut dyn Write,
    machine: bool,
    config: &std::path::Path,
    file: &[String],
    running: Option<&[String]>,
) -> io::Result<()> {
    if machine {
        return json(
            out,
            &serde_json::json!({ "file": file, "running": running }),
        );
    }
    let mut ids: Vec<&String> = file.iter().chain(running.unwrap_or_default()).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return writeln!(
            out,
            "no project is enrolled in {}; add one with `dudes project add`",
            Escaped(&config.display().to_string())
        );
    }
    for id in ids {
        let in_file = file.contains(id);
        let place = match running.map(|running| running.contains(id)) {
            None => "in the file; no daemon is running",
            Some(true) if in_file => "in the file and running",
            Some(true) => "running only: the file no longer enrolls it",
            Some(false) => "in the file only: the daemon has not adopted it",
        };
        writeln!(out, "{}  {place}", Escaped(id))?;
    }
    Ok(())
}

/// Show the owned settings and their start rules. Other settings and replayed ceilings are not
/// validated here.
///
/// # Errors
///
/// Whatever writing returns.
pub fn config(out: &mut dyn Write, path: &Path, settings: &config::Settings) -> io::Result<()> {
    let absent = if settings.exists() {
        ""
    } else {
        " (absent, so every key has its default)"
    };
    writeln!(
        out,
        "{:<30}{}{absent}",
        "file",
        Escaped(&path.display().to_string())
    )?;
    for key in config::Key::ALL {
        writeln!(out, "{:<30}{}", key.name(), settings.shown(key))?;
    }
    writeln!(out, "{:<30}always on, with no setting", "chat")?;
    let problems = settings.problems();
    if !problems.is_empty() {
        let problems: Vec<String> = problems.iter().map(ToString::to_string).collect();
        writeln!(
            out,
            "the daemon will refuse to start with this file: {}",
            problems.join("; ")
        )?;
    }
    writeln!(
        out,
        "checked only the keys listed above; other file settings and journaled ceilings can also refuse startup"
    )
}

/// One `journal.appended` notification, with its time shown in `zone`.
///
/// # Errors
///
/// Whatever writing returns.
pub fn appended<Tz: TimeZone>(
    out: &mut dyn Write,
    machine: bool,
    params: &JournalAppendedParams,
    zone: &Tz,
) -> io::Result<()>
where
    Tz::Offset: std::fmt::Display,
{
    if machine {
        return json(out, params);
    }
    writeln!(
        out,
        "{:<37}  {}  {:?}",
        position(params.lsn),
        zoned(params.entry.at, zone),
        params.entry.record.kind()
    )
}

/// A moment as `2026-10-11 09:14:02 +02:00` in `zone`.
fn zoned<Tz: TimeZone>(at: dudes_types::Timestamp, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    chrono::DateTime::from_timestamp_millis(at.unix_millis()).map_or_else(
        || at.to_string(),
        |utc| {
            utc.with_timezone(zone)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        },
    )
}

/// A refusal or a daemon error, on the error stream.
///
/// In `--json` mode this is the contract's [`RpcError`], so a script reads `error.code` — the value
/// a script branches on — rather than matching a message. The human form leads with
/// the code for the same reason: it is the part that is stable.
///
/// # Errors
///
/// Whatever writing returns.
pub fn rpc_error(err: &mut dyn Write, machine: bool, error: &RpcError) -> io::Result<()> {
    if machine {
        return json(err, error);
    }
    writeln!(err, "dudes: {} ({})", Escaped(&error.message), error.code)
}

/// A local failure: bad arguments, an unreachable daemon, an unreadable credential.
///
/// Never rendered as JSON. The machine-readable mode is a promise about the *contract's* types, and
/// a local failure has no contract type — inventing one would put a shape on the wire's page that
/// the schema does not describe.
///
/// # Errors
///
/// Whatever writing returns.
pub fn local_error(err: &mut dyn Write, message: &str) -> io::Result<()> {
    writeln!(err, "dudes: {}", Escaped(message))
}

/// A warning that does not change the exit code, on the error stream.
///
/// # Errors
///
/// Whatever writing returns.
pub fn warning(err: &mut dyn Write, message: &str) -> io::Result<()> {
    writeln!(err, "dudes: warning: {}", Escaped(message))
}

/// A completed display rename.
///
/// # Errors
///
/// Returns any output write failure.
pub fn renamed(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        json(out, &result)
    } else {
        writeln!(out, "Name changed.")
    }
}

/// A completed role and appearance edit.
///
/// # Errors
///
/// Returns any output write failure.
pub fn look_changed(out: &mut dyn Write, machine: bool, result: Empty) -> io::Result<()> {
    if machine {
        json(out, &result)
    } else {
        writeln!(out, "Role and appearance changed.")
    }
}

/// Desired server names and whether they were newly recorded for a later launch.
///
/// # Errors
/// Returns any output write failure.
pub fn mcp_servers(out: &mut dyn Write, machine: bool, result: &McpResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    let names = if result.mcp_servers.is_empty() {
        "none".to_owned()
    } else {
        result
            .mcp_servers
            .iter()
            .map(dudes_types::McpServerName::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if result.changed {
        writeln!(
            out,
            "MCP servers recorded for the next managed launch: {names}."
        )
    } else {
        writeln!(
            out,
            "Desired MCP servers for the next managed launch: {names}."
        )
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::decision::{Gate, GateStep, LoopDecision};
    use dudes_proto::rpc::ErrorCode;
    use dudes_types::{AgentId, Lsn};

    use super::*;

    #[test]
    fn the_removal_command_turns_off_hooks_and_the_monitor_and_quotes_every_path() {
        let printed = text(|out| {
            worktree_remove_command(
                out,
                std::path::Path::new("/repos/it's here"),
                std::path::Path::new("/trees/a b"),
            )
        });
        assert!(
            printed.contains(
                "git -c core.fsmonitor=false -c core.hooksPath=/dev/null -C '/repos/it'\\''s here' \
                 worktree remove -- '/trees/a b'"
            ),
            "{printed}"
        );
    }

    #[test]
    fn the_removal_text_warns_that_ignored_files_are_deleted_before_the_command() {
        let printed = text(|out| {
            worktree_remove_command(
                out,
                std::path::Path::new("/repos/main"),
                std::path::Path::new("/trees/a b"),
            )
        });
        assert!(
            printed.starts_with(
                "the tree still exists. Removing it deletes ignored files such as a .env, a local database or build output.\n\
                 Check what is in it with:\n  git -C '/trees/a b' status --ignored\n\
                 Review the tree for uncommitted work (?? untracked, M modified, !! ignored files that removal deletes), commit or copy out what you want to keep, then remove it with your own git and run this command again:\n  git -c "
            ),
            "{printed}"
        );
    }

    #[test]
    fn a_removal_that_was_already_recorded_says_so() {
        let id: dudes_types::WorktreeId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        assert_eq!(
            text(|out| worktree_already_removed(out, false, id)),
            "worktree 0123456789abcdef0123456789abcdef was already removed\n"
        );
        assert!(
            text(|out| worktree_already_removed(out, true, id))
                .contains("\"already_removed\":true")
        );
    }

    fn text(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut buffer = Vec::new();
        f(&mut buffer).expect("writing to a vector");
        String::from_utf8(buffer).expect("the renderer writes UTF-8")
    }

    fn an_agent() -> AgentId {
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("an agent id")
    }

    #[test]
    fn doctor_json_escapes_line_separators_as_valid_json() {
        let value = serde_json::json!({"detail": "left\u{2028}middle\u{2029}right"});
        let line = text(|out| doctor_json(out, &value));
        assert_eq!(line, "{\"detail\":\"left\\u2028middle\\u2029right\"}\n");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            value
        );
    }

    /// The machine mode emits the contract's own type, and it reads back as that type. That round
    /// trip is the whole promise of `--json`: a caller can deserialise it without scraping.
    #[test]
    fn the_machine_mode_round_trips_through_the_contracts_type() {
        let result = HireResult { agent: an_agent() };
        let line = text(|out| hired(out, true, &result));
        let back: HireResult = serde_json::from_str(&line).expect("it is the contract's type");
        assert_eq!(back, result);
    }

    /// **The one admitted disclosure still happens in full.**
    ///
    /// `FloorUrl`'s `Debug` redacts the bearer, and this is the assertion that the redaction did
    /// not spread to the one place the bearer is *supposed* to go. The human line writes
    /// `printed_once` through `Display`, never `Debug`, so the two sit side by side: a fix that
    /// redacted the print instead of the formatting turns this red, and a `Debug` that leaked turns
    /// `floor::tests::debug_redacts_the_bearer_and_keeps_the_page_legible` red. Neither test alone
    /// pins the pair.
    #[test]
    fn the_floor_url_line_still_emits_the_whole_fragment_bearing_url() {
        let result: SessionIssueResult = serde_json::from_value(serde_json::json!({
            "base_url": "http://127.0.0.1:53211",
            "bootstrap": "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFg",
            "expires_in_s": 60,
        }))
        .expect("the contract accepts this result");
        let urls = crate::floor::urls(&result).expect("a loopback origin");

        let printed = text(|out| floor_url(out, false, &result, &urls));
        assert!(
            printed.contains(&urls.printed_once),
            "the admitted disclosure was lost: {printed}"
        );
        assert!(
            printed.contains(&result.bootstrap),
            "the bearer is not in the printed URL: {printed}"
        );
        assert!(
            !printed.contains("<redacted>"),
            "the print path went through Debug: {printed}"
        );
    }

    /// The two modes differ, and the human one is not JSON. Both arms matter: a renderer that
    /// ignored the flag would pass either alone.
    #[test]
    fn the_human_mode_is_not_the_machine_mode() {
        let result = HireResult { agent: an_agent() };
        let human = text(|out| hired(out, false, &result));
        let machine = text(|out| hired(out, true, &result));
        assert_ne!(human, machine);
        assert!(
            serde_json::from_str::<serde_json::Value>(&human).is_err(),
            "the human form is prose, not a second wire format: {human}"
        );
        assert!(
            human.contains(&an_agent().to_string()),
            "and it still names the agent: {human}"
        );
    }

    /// The error renderer carries the code in both modes, because the code is what a caller
    /// branches on. The JSON arm is asserted through the contract's type rather than by substring.
    #[test]
    fn an_error_carries_its_code_in_both_modes() {
        let error = RpcError {
            code: ErrorCode::Unauthorized.code(),
            message: "this method requires an operator".to_owned(),
            data: None,
        };
        let machine = text(|out| rpc_error(out, true, &error));
        let back: RpcError = serde_json::from_str(&machine).expect("the contract's type");
        assert_eq!(back.code, ErrorCode::Unauthorized.code());

        let human = text(|out| rpc_error(out, false, &error));
        assert!(human.contains("-32001"), "{human}");
        assert!(human.contains("requires an operator"), "{human}");
    }

    /// `why` prints every gate in the trace, not only the one that fired — which is the question the
    /// command exists to answer. The control is the count: a renderer that printed only the firing
    /// step would produce one line and fail here.
    #[test]
    fn why_prints_the_whole_gate_trace() {
        let result = WhyResult {
            lsn: Lsn(11),
            input_digest: "abc123".to_owned(),
            decision: LoopDecision::Done,
            gate_trace: vec![
                GateStep {
                    gate: Gate::Control,
                    fired: false,
                    note: None,
                },
                GateStep {
                    gate: Gate::Done,
                    fired: true,
                    note: Some("the predicate passed".to_owned()),
                },
            ],
        };
        let human = text(|out| why(out, false, &result));
        assert!(
            human.starts_with("journal segment 0 · byte 11 · input abc123\n"),
            "{human}"
        );
        assert!(human.contains("abc123"), "{human}");
        assert!(human.contains("Control"), "{human}");
        assert!(human.contains("Done"), "{human}");
        assert!(human.contains("the predicate passed"), "{human}");
        assert_eq!(
            human.lines().filter(|line| line.starts_with("  ")).count(),
            2,
            "one line per gate consulted: {human}"
        );
    }
    #[test]
    fn agents_show_the_automatic_hold_reason() {
        let agent: AgentView = serde_json::from_value(serde_json::json!({
            "id": an_agent(), "name": "Hector", "role": "infra", "department": "infra",
            "provider": "claude", "status": "idle", "mode": "managed",
            "automatic_hold": { "kind": "launch_failed", "reason": "managed process could not launch (NotFound)" }
        })).expect("agent view");
        let human = text(|out| agents(out, false, &[agent]));
        assert!(human.contains("Claude  Hold"), "{human}");
        assert!(human.contains("held: launch failed: managed process could not launch (NotFound); fix the cause and resume"), "{human}");
        assert_eq!(
            human,
            format!(
                "{id}  Hector  infra  Claude  Hold\n  held: launch failed: managed process could not launch (NotFound); fix the cause and resume\n  Dudes could not start Claude Code, usually because `claude` is not installed. Install it and sign in, then run `dudes resume {id}`, or hire an agent with an engine you have.\n",
                id = an_agent()
            )
        );
    }

    #[test]
    fn a_not_found_launch_names_the_usual_program() {
        for (provider, name, program) in [("codex", "Codex", "codex"), ("pi", "Pi", "pi")] {
            let agent: AgentView = serde_json::from_value(serde_json::json!({
                "id": an_agent(), "name": "Hector", "role": "infra", "department": "infra",
                "provider": provider, "status": "idle", "mode": "managed",
                "automatic_hold": { "kind": "launch_failed", "reason": "managed process could not launch (NotFound)" }
            })).expect("agent view");
            let human = text(|out| agents(out, false, &[agent]));
            assert!(
                human.contains(&format!(
                    "Dudes could not start {name}, usually because `{program}` is not installed. Install it"
                )),
                "{human}"
            );
        }
    }

    #[test]
    fn other_launch_failures_get_no_engine_hint() {
        let agent: AgentView = serde_json::from_value(serde_json::json!({
            "id": an_agent(), "name": "Hector", "role": "infra", "department": "infra",
            "provider": "claude", "status": "idle", "mode": "managed",
            "automatic_hold": { "kind": "launch_failed", "reason": "managed process could not launch (PermissionDenied)" }
        })).expect("agent view");
        assert!(!text(|out| agents(out, false, &[agent])).contains("Dudes could not start"));
    }

    /// OSC 52 (writes the clipboard), a CSI and its one-byte C1 form, DEL, a newline that starts a
    /// forged `dudes agents` row, a right-to-left override and an isolate.
    const HOSTILE: &str = "\u{1b}]52;c;aGk=\u{7}\u{1b}[2J\u{9b}1m\u{7f}\n01ARZ3NDEKTSV4RRFFQ69G5FAV  Fake  admin  Claude  Idle\u{202e}gpj.exe\u{2066}é";
    const SHOWN: &str = r"\u{1b}]52;c;aGk=\u{7}\u{1b}[2J\u{9b}1m\u{7f}\u{a}01ARZ3NDEKTSV4RRFFQ69G5FAV  Fake  admin  Claude  Idle\u{202e}gpj.exe\u{2066}é";

    fn hostile_agent() -> AgentView {
        serde_json::from_value(serde_json::json!({
            "id": an_agent(), "name": HOSTILE, "role": HOSTILE, "department": "infra",
            "provider": "claude", "status": "idle", "mode": "managed",
            "automatic_hold": { "kind": "launch_failed", "reason": HOSTILE }
        }))
        .expect("agent view")
    }

    #[test]
    fn agents_escape_the_name_role_and_hold_reason() {
        assert_eq!(
            text(|out| agents(out, false, &[hostile_agent()])),
            format!(
                "{}  {SHOWN}  {SHOWN}  Claude  Hold\n  held: launch failed: {SHOWN}; fix the cause and resume\n",
                an_agent()
            )
        );
    }

    #[test]
    fn the_machine_mode_keeps_hostile_text_exact() {
        let line = text(|out| agents(out, true, &[hostile_agent()]));
        let back: Vec<AgentView> = serde_json::from_str(&line).expect("the contract's type");
        assert_eq!(back[0].name, HOSTILE);
        assert_eq!(back[0].role, HOSTILE);

        let error = RpcError {
            code: ErrorCode::InvalidParams.code(),
            message: HOSTILE.to_owned(),
            data: None,
        };
        let line = text(|out| rpc_error(out, true, &error));
        let back: RpcError = serde_json::from_str(&line).expect("the contract's type");
        assert_eq!(back.message, HOSTILE);
    }

    #[test]
    fn cards_escape_the_title_and_department() {
        let card: CardView = serde_json::from_value(serde_json::json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "title": HOSTILE, "department": HOSTILE,
            "status": "todo", "status_by": { "kind": "system" }, "seq": 3
        }))
        .expect("card view");
        assert_eq!(
            text(|out| cards(out, false, &[card])),
            format!("01ARZ3NDEKTSV4RRFFQ69G5FAV  seq 3  Todo  {SHOWN}  {SHOWN}\n")
        );
    }

    /// The decision is printed through `Debug`, which already escapes each of these characters.
    #[test]
    fn why_escapes_the_digest_the_notes_and_the_decision_text() {
        let result = WhyResult {
            lsn: Lsn(11),
            input_digest: HOSTILE.to_owned(),
            decision: LoopDecision::Wait {
                question: HOSTILE.to_owned(),
            },
            gate_trace: vec![GateStep {
                gate: Gate::Human,
                fired: true,
                note: Some(HOSTILE.to_owned()),
            }],
        };
        assert_eq!(
            text(|out| why(out, false, &result)),
            format!(
                "journal segment 0 · byte 11 · input {SHOWN}\n\
                 decision: Wait {{ question: \"{}\" }}\n\
                 \x20 → Human: {SHOWN}\n\
                 answer it in the Floor's Questions view: `dudes floor open` prints the link, and floor.answer must be on\n",
                r"\u{1b}]52;c;aGk=\u{7}\u{1b}[2J\u{9b}1m\u{7f}\n01ARZ3NDEKTSV4RRFFQ69G5FAV  Fake  admin  Claude  Idle\u{202e}gpj.exe\u{2066}é"
            )
        );
    }

    #[test]
    fn pause_all_escapes_each_refused_name_and_reason() {
        assert_eq!(
            text(|out| pause_all(out, 1, &[(HOSTILE.to_owned(), HOSTILE.to_owned())])),
            format!("Paused 1 agents.\nRefused {SHOWN}: {SHOWN}\n")
        );
    }

    #[test]
    fn ping_escapes_the_version_and_the_socket() {
        let result = PingResult {
            version: HOSTILE.to_owned(),
            socket: HOSTILE.into(),
            lsn: Lsn(3),
        };
        assert_eq!(
            text(|out| ping(out, false, &result)),
            format!("duded {SHOWN} on {SHOWN}\njournal at segment 0 · byte 3\n")
        );
    }

    #[test]
    fn ping_names_the_journal_position_by_segment_and_byte_and_its_json_keeps_the_raw_lsn() {
        let result = PingResult {
            version: "0.63.0".to_owned(),
            socket: "/run/dudes/duded.sock".into(),
            lsn: Lsn::from_parts(2, 15176).expect("a position"),
        };
        assert_eq!(
            text(|out| ping(out, false, &result)),
            "duded 0.63.0 on /run/dudes/duded.sock\njournal at segment 2 · byte 15176\n"
        );
        assert_eq!(
            text(|out| ping(out, true, &result)),
            "{\"version\":\"0.63.0\",\"socket\":\"/run/dudes/duded.sock\",\"lsn\":2199023270728}\n"
        );
    }

    #[test]
    fn ping_on_an_empty_journal_says_it_has_no_records_and_its_json_keeps_the_zero() {
        let result = PingResult {
            version: "0.63.0".to_owned(),
            socket: "/run/dudes/duded.sock".into(),
            lsn: Lsn::ZERO,
        };
        assert_eq!(
            text(|out| ping(out, false, &result)),
            "duded 0.63.0 on /run/dudes/duded.sock\njournal has no records yet\n"
        );
        assert_eq!(
            text(|out| ping(out, true, &result)),
            "{\"version\":\"0.63.0\",\"socket\":\"/run/dudes/duded.sock\",\"lsn\":0}\n"
        );
    }

    #[test]
    fn memory_positions_are_readable_and_json_keeps_raw_positions() {
        use dudes_proto::rpc::memory::{MemoryAppendResult, MemoryReadResult};
        let lsn = Lsn::from_parts(1, 2048).unwrap();
        assert_eq!(
            text(|out| memory_appended(out, false, MemoryAppendResult { lsn })),
            "memory appended at segment 1 · byte 2048\n"
        );
        assert_eq!(
            text(|out| memory_appended(out, true, MemoryAppendResult { lsn })),
            "{\"lsn\":1099511629824}\n"
        );
        let read = MemoryReadResult {
            text: "notes".into(),
            lsn,
        };
        assert_eq!(
            text(|out| memory_read(out, false, &read)),
            "memory through segment 1 · byte 2048\nnotes\n"
        );
        assert_eq!(
            text(|out| memory_read(out, true, &read)),
            "{\"text\":\"notes\",\"lsn\":1099511629824}\n"
        );
        assert_eq!(
            text(|out| memory_read(
                out,
                false,
                &MemoryReadResult {
                    text: String::new(),
                    lsn: Lsn::ZERO
                }
            )),
            "no memory records in this page\n\n"
        );
    }

    #[test]
    fn memory_search_labels_raw_summary_and_continuation_and_escapes_controls() {
        use dudes_proto::rpc::memory::{MemorySearchHit, MemorySearchResult};
        let result = MemorySearchResult {
            hits: vec![
                MemorySearchHit::Raw {
                    lsn: Lsn(7),
                    match_offset: 4,
                    text: format!("\u{1b}[2Jneedle{}", "x".repeat(1014)),
                    text_offset: 0,
                    source_bytes: 65_537,
                    truncated: true,
                    read_since_lsn: Lsn(6),
                },
                MemorySearchHit::Summary {
                    lsn: Lsn(20),
                    match_offset: 0,
                    text: "summary\u{1}needle".into(),
                    text_offset: 0,
                    source_bytes: 14,
                    truncated: false,
                    read_since_lsn: Some(Lsn(2)),
                    covered_from_lsn: Lsn(3),
                    covered_to_lsn: Lsn(10),
                },
            ],
            next_since_lsn: Some(Lsn(20)),
        };
        let human = text(|out| memory_search(out, false, &result));
        assert!(human.contains("raw memory at segment 0 · byte 7"));
        assert!(human.contains("summary at segment 0 · byte 20"));
        assert!(human.contains("too large for a whole-entry memory read"));
        assert!(human.contains("read original range after 0:2"));
        assert!(human.contains("continue search with --since-lsn 0:20"));
        assert!(human.contains("\\u{1b}[2Jneedle"));
        assert!(!human.contains('\u{1b}') && !human.contains('\u{1}'));
        let machine = text(|out| memory_search(out, true, &result));
        assert_eq!(
            serde_json::from_str::<MemorySearchResult>(&machine).unwrap(),
            result
        );
    }

    #[test]
    fn tail_time_column_stays_aligned_across_the_full_position_range() {
        let wire = r#"{"lsn":1,"entry":{"at":1791271167492,"t":"human_question","agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","text":"which branch?"}}"#;
        let mut params: JournalAppendedParams = serde_json::from_str(wire).unwrap();
        for lsn in [Lsn(1), Lsn(u64::MAX)] {
            params.lsn = lsn;
            let rendered = text(|out| appended(out, false, &params, &chrono::Utc));
            let (prefix, _) = rendered.split_once("2026-10-06").unwrap();
            assert_eq!(prefix.chars().count(), 39, "{rendered}");
        }
    }

    #[test]
    fn tail_names_the_position_and_the_local_time_and_its_json_keeps_the_raw_fields() {
        let wire = r#"{"lsn":2199023270728,"entry":{"at":1791271167492,"t":"human_question","agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","text":"which branch?"}}"#;
        let params: JournalAppendedParams = serde_json::from_str(wire).expect("the contract");
        let zone = chrono::FixedOffset::east_opt(2 * 3600).expect("an offset");
        assert_eq!(
            text(|out| appended(out, false, &params, &zone)),
            "segment 2 · byte 15176                 2026-10-06 09:19:27 +02:00  HumanQuestion\n"
        );
        assert_eq!(
            text(|out| appended(out, true, &params, &zone)),
            format!("{wire}\n")
        );
    }

    #[test]
    fn errors_escape_the_daemons_message_and_local_detail() {
        let error = RpcError {
            code: ErrorCode::Unauthorized.code(),
            message: HOSTILE.to_owned(),
            data: None,
        };
        assert_eq!(
            text(|out| rpc_error(out, false, &error)),
            format!("dudes: {SHOWN} (-32001)\n")
        );
        assert_eq!(
            text(|out| local_error(out, HOSTILE)),
            format!("dudes: {SHOWN}\n")
        );
        assert_eq!(
            text(|out| warning(out, HOSTILE)),
            format!("dudes: warning: {SHOWN}\n")
        );
    }

    #[test]
    fn escaped_writes_each_terminal_control_as_its_escape_and_keeps_every_neighbour() {
        assert_eq!(
            Escaped("\u{0}\u{1f}\u{7f}\u{80}\u{9f}\u{61c}\u{200e}\u{200f}\u{2028}\u{2029}\u{202a}\u{202e}\u{2066}\u{2069}")
                .to_string(),
            r"\u{0}\u{1f}\u{7f}\u{80}\u{9f}\u{61c}\u{200e}\u{200f}\u{2028}\u{2029}\u{202a}\u{202e}\u{2066}\u{2069}"
        );
        let kept =
            " ~\u{a0}\u{61b}\u{61d}\u{200d}\u{2010}\u{2027}\u{202f}\u{2065}\u{206a}é日本\\u{1b}";
        assert_eq!(Escaped(kept).to_string(), kept);
    }

    #[test]
    fn reload_and_workspaces_escape_every_id_path_and_field() {
        let result = EnrollmentReloadResult {
            applied: false,
            refused: None,
            sha256: HOSTILE.to_owned(),
            path: Some(HOSTILE.to_owned()),
            workspaces: vec![HOSTILE.to_owned()],
            added: vec![HOSTILE.to_owned()],
            removed: vec![HOSTILE.to_owned()],
            changed: vec![dudes_proto::rpc::floor::ChangedWorkspace {
                id: HOSTILE.to_owned(),
                fields: vec![HOSTILE.to_owned()],
            }],
            switches: vec![],
            not_applied: vec![HOSTILE.to_owned()],
        };
        assert_eq!(
            text(|out| reload(out, false, &result)),
            format!(
                "file     {SHOWN}\n\
                 sha256   {SHOWN}\n\
                 added    {SHOWN}\n\
                 removed  {SHOWN}\n\
                 changed  {SHOWN} ({SHOWN})\n\
                 needs a daemon restart: {SHOWN}\n\
                 running  {SHOWN}\n\
                 not applied; run `dudes project reload --apply {SHOWN}` to apply this change\n"
            )
        );

        let ids = [HOSTILE.to_owned()];
        assert_eq!(
            text(|out| workspaces(out, false, Path::new("/config.toml"), &ids, Some(&ids))),
            format!("{SHOWN}  in the file and running\n")
        );
    }

    #[test]
    fn reload_says_so_when_there_is_nothing_to_apply() {
        let mut result = EnrollmentReloadResult {
            applied: false,
            refused: None,
            sha256: "abc".to_owned(),
            path: Some("/config.toml".to_owned()),
            workspaces: vec!["web-app".to_owned()],
            added: vec![],
            removed: vec![],
            changed: vec![],
            switches: vec![],
            not_applied: vec![],
        };
        let said = text(|out| reload(out, false, &result));
        assert!(
            said.ends_with("nothing to apply; the file and the running daemon agree\n"),
            "{said}"
        );
        assert!(!said.contains("not applied"), "{said}");

        result.not_applied = vec!["floor.answer".to_owned()];
        let said = text(|out| reload(out, false, &result));
        assert!(
            said.ends_with(
                "not applied; run `dudes project reload --apply abc` to apply this change\n"
            ),
            "a restart-only difference is still a difference: {said}"
        );
    }

    #[test]
    fn why_points_a_waiting_agent_to_the_floor_and_only_then() {
        let said = |decision| {
            text(|out| {
                why(
                    out,
                    false,
                    &WhyResult {
                        lsn: Lsn(1),
                        input_digest: "d".to_owned(),
                        decision,
                        gate_trace: vec![],
                    },
                )
            })
        };
        assert!(said(LoopDecision::Wait { question: "Which?".to_owned() })
            .ends_with("answer it in the Floor's Questions view: `dudes floor open` prints the link, and floor.answer must be on\n"));
        assert!(!said(LoopDecision::Done).contains("Floor"));
    }

    #[test]
    fn the_operator_file_path_is_escaped() {
        assert_eq!(
            text(|out| workspaces(out, false, Path::new(HOSTILE), &[], None)),
            format!("no project is enrolled in {SHOWN}; add one with `dudes project add`\n")
        );

        let absent = tempfile::tempdir().expect("a directory with no operator file");
        let settings = config::Settings::read(&absent.path().join("config.toml"))
            .expect("an absent file holds every default");
        let shown = text(|out| config(out, Path::new(HOSTILE), &settings));
        assert_eq!(
            shown.lines().next(),
            Some(
                format!(
                    "{:<30}{SHOWN} (absent, so every key has its default)",
                    "file"
                )
                .as_str()
            ),
            "{shown}"
        );
    }

    /// Each refusal a goal's check run or its admission gives reads as the terminal's sentence,
    /// with the remedy this CLI has; any other error keeps the daemon's words.
    #[test]
    fn goal_refusals_name_their_remedy() {
        use dudes_proto::rpc::{RefusalGate, RefusedData};
        let refused = |gate: Option<RefusalGate>, code: ErrorCode, message: &str| {
            goal_refusal(&RpcError {
                code: code.code(),
                message: message.to_owned(),
                data: gate.map(|gate| serde_json::to_value(RefusedData { gate }).unwrap()),
            })
        };
        assert_eq!(
            goal_check_runs_first(600),
            "Running the completion check once before the first turn, to see whether it already \
             passes (at most 600 s). Ctrl-C before the check finishes cancels without setting the \
             goal."
        );
        for (gate, message, shown) in [
            (
                RefusalGate::CheckAlreadyPasses,
                "the completion check already passes (`test -f hello.txt` exited 0 in /repo), so \
                 it cannot show when this goal is done",
                "Refused: the completion check already passes (`test -f hello.txt` exited 0 in \
                 /repo), so it cannot show when this goal is done. Use a check that fails until \
                 the goal is met, or add --check-may-already-pass if Done should mean only that \
                 the check still passes after the agent's work.",
            ),
            (
                RefusalGate::CheckDidNotFinish,
                "the completion check did not finish (it timed out after 5 s, or a signal ended \
                 it), so it cannot show whether the check already passes",
                "Refused: the completion check did not finish (it timed out after 5 s, or a \
                 signal ended it), so it cannot show whether the check already passes. Raise its \
                 timeout, or add --check-may-already-pass.",
            ),
            (
                RefusalGate::OpenTurn,
                "the agent has a turn or a check running in its directory, so the completion \
                 check cannot run there now",
                "Refused: the agent has a turn or a check running in its directory, so the \
                 completion check cannot run there now. Try again when it ends, or add \
                 --check-may-already-pass.",
            ),
            (
                RefusalGate::CheckCannotRun,
                "the completion check could not run, because /repo does not exist",
                "Refused: the completion check could not run, because /repo does not exist.",
            ),
        ] {
            assert_eq!(
                refused(Some(gate), ErrorCode::Refused, message).as_deref(),
                Some(shown)
            );
        }
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_eq!(
            refused(
                None,
                ErrorCode::NotFound,
                &format!("there is no live agent {agent}")
            ),
            Some(format!("Refused: there is no live agent {agent}."))
        );
        assert_eq!(
            refused(
                None,
                ErrorCode::InvalidParams,
                "goal.set: the completion check cannot run: not found"
            ),
            None
        );
        assert_eq!(
            refused(Some(RefusalGate::Budget), ErrorCode::Refused, "over budget"),
            None
        );
    }
}
