//! Segment files: `journal/NNNNNN.log`, numbered from 1, rotated at [`DEFAULT_ROTATE_AT`] or on
//! daemon start. `lsn = (segment << 40) | byte offset`
//! of the frame's first byte, so segment 0 offset 0 (`Lsn::ZERO`) is "before any record".

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use dudes_types::Lsn;

use crate::error::{Error, Result};

/// Rotate when the next frame would end past this many bytes: 256 MiB.
pub const DEFAULT_ROTATE_AT: u64 = 256 * 1024 * 1024;
/// The first segment number.
pub const FIRST_SEGMENT: u32 = 1;
/// Digits in a segment file name.
const DIGITS: usize = 6;
/// File extension.
const EXT: &str = "log";

/// `dir/NNNNNN.log` for segment `no`.
#[must_use]
pub fn segment_path(dir: &Path, no: u32) -> PathBuf {
    dir.join(format!("{no:0DIGITS$}.{EXT}"))
}

/// The segment number of a file name, or `None` for a name this crate did not produce.
#[must_use]
pub fn parse_segment_name(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let stem = name.strip_suffix(".log")?;
    if stem.len() != DIGITS || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok().filter(|no| *no >= FIRST_SEGMENT)
}

/// The segment numbers present in `dir`, ascending. Names this crate did not produce are
/// ignored; a gap between two present segments is [`Error::SegmentGap`] (a gap before the
/// first is archival, not loss).
pub fn list_segments(dir: &Path) -> Result<Vec<u32>> {
    let mut numbers = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(no) = parse_segment_name(&entry.file_name()) {
            numbers.push(no);
        }
    }
    numbers.sort_unstable();
    for pair in numbers.windows(2) {
        if pair[1] != pair[0] + 1 {
            return Err(Error::SegmentGap {
                after: pair[0],
                found: pair[1],
            });
        }
    }
    Ok(numbers)
}

/// True when a frame of `frame_len` bytes appended at `len` would end past `rotate_at`. A
/// segment that is still empty never rotates: a frame is never split, whatever its size.
#[must_use]
pub fn would_cross(len: u64, frame_len: u64, rotate_at: u64) -> bool {
    len > 0 && len.saturating_add(frame_len) > rotate_at
}

/// The lsn of the frame starting at `offset` in `segment`.
pub fn lsn_at(segment: u32, offset: u64) -> Result<Lsn> {
    Lsn::from_parts(segment, offset).ok_or(Error::LsnRange { segment, offset })
}

/// Make the directory entries as durable as the bytes (a new or truncated segment). A no-op
/// where the platform has no directory fsync.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "one signature spans both cfg arms: the unix arm fsyncs and can fail, this one \
                  has no directory fsync to call and cannot. Three call sites on the durability \
                  path write `sync_dir(..)?`; returning `()` here would move the cfg out of this \
                  function and into every one of them"
    )
)]
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::record::{Record, RecordKind, samples};

    use super::*;

    /// Journal format: segments are `NNNNNN.log`, six digits from 000001, and only that.
    #[test]
    fn names_round_trip_and_reject_strangers() {
        let dir = Path::new("/j");
        assert_eq!(segment_path(dir, 1), Path::new("/j/000001.log"));
        assert_eq!(segment_path(dir, 123_456), Path::new("/j/123456.log"));
        assert_eq!(parse_segment_name(OsStr::new("000001.log")), Some(1));
        assert_eq!(parse_segment_name(OsStr::new("000000.log")), None);
        assert_eq!(parse_segment_name(OsStr::new("1.log")), None);
        assert_eq!(parse_segment_name(OsStr::new("000001.log.bak")), None);
        assert_eq!(parse_segment_name(OsStr::new("LOCK")), None);
        assert_eq!(parse_segment_name(OsStr::new("00000a.log")), None);
    }

    /// Segments are contiguous: a hole between present files is missing records, reported
    /// before anything is replayed; files before the first present segment may be archived.
    #[test]
    fn listing_is_sorted_and_gap_free() {
        let dir = tempfile::tempdir().unwrap();
        for no in [3u32, 2, 4] {
            fs::write(segment_path(dir.path(), no), b"").unwrap();
        }
        fs::write(dir.path().join("notes.txt"), b"").unwrap();
        assert_eq!(list_segments(dir.path()).unwrap(), vec![2, 3, 4]);
        fs::remove_file(segment_path(dir.path(), 3)).unwrap();
        assert!(matches!(
            list_segments(dir.path()),
            Err(Error::SegmentGap { after: 2, found: 4 })
        ));
    }

    /// Rotation: a frame is never split — rotate before a frame that would cross, never
    /// on an empty segment.
    #[test]
    fn rotation_never_splits_a_frame() {
        assert!(!would_cross(0, 10_000, 4096));
        assert!(!would_cross(100, 3996, 4096));
        assert!(would_cross(100, 3997, 4096));
        assert!(would_cross(1, u64::MAX, 4096));
    }

    /// `lsn = (segment << 40) | offset`; positions outside the fields are refused, not wrapped.
    #[test]
    fn lsn_composition() {
        assert_eq!(lsn_at(1, 512).unwrap(), Lsn((1 << 40) | 512));
        assert!(matches!(lsn_at(1, 1 << 40), Err(Error::LsnRange { .. })));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "an exhaustive reference classifier binds every referenced field explicitly"
    )]
    fn lsns_of(record: &Record) -> Vec<Lsn> {
        match record {
            Record::McpLaunchGranted { policy_lsn, .. } => vec![*policy_lsn],
            Record::TurnSpawned { mcp_grant, .. } => mcp_grant.iter().copied().collect(),
            Record::MailWakeTurnDecided(decision) => {
                let mut refs = vec![decision.completion_lsn];
                match &decision.purpose {
                    dudes_proto::MailWakePurpose::FreshPreparation { plan } => {
                        refs.extend([plan.trigger.lsn, plan.frontier]);
                        refs.extend(plan.targets.iter().map(|target| target.mail.lsn));
                    }
                    dudes_proto::MailWakePurpose::ExistingPreparation { root_lsn } => {
                        refs.push(*root_lsn);
                    }
                    dudes_proto::MailWakePurpose::UnpreparedCandidate { trigger, frontier } => {
                        refs.extend([trigger.lsn, *frontier]);
                    }
                }
                if let dudes_proto::MailWakeAdmission::Wait { question_lsn } = decision.decision {
                    refs.push(question_lsn);
                }
                refs
            }
            Record::MailWakeOnlyTurnIssued(issue) => {
                vec![issue.completion_lsn, issue.root_lsn, issue.lsn_of_decision]
            }
            Record::TurnIssued {
                agent: _,
                turn: _,
                goal: _,
                task: _,
                session: _,
                mode: _,
                model_selection: _,
                lsn_of_decision,
                answers,
            } => std::iter::once(*lsn_of_decision)
                .chain(answers.iter().copied())
                .collect(),
            Record::BrowserNoteCancelled {
                agent: _,
                note_lsn,
                browser: _,
            }
            | Record::BrowserNoteIssuedCancelled {
                agent: _,
                note_lsn,
                turn: _,
                browser: _,
            }
            | Record::BrowserNoteDispatchIntent {
                agent: _,
                note_lsn,
                turn: _,
            }
            | Record::BrowserNoteContextEmitted { agent: _, note_lsn } => vec![*note_lsn],
            Record::BrowserNoteReissueCancelled {
                agent: _,
                note_lsn,
                turn: _,
                dispatch_intent_lsn,
                browser: _,
            } => vec![*note_lsn, *dispatch_intent_lsn],
            Record::NoteTurnDecided {
                agent: _,
                goal: _,
                note_lsn,
                completion_lsn,
                input_digest: _,
                gate_trace: _,
                decision: _,
            }
            | Record::TaughtNoteTurnDecided {
                agent: _,
                goal: _,
                note_lsn,
                completion_lsn,
                input_digest: _,
                gate_trace: _,
                decision: _,
            } => vec![*note_lsn, *completion_lsn],
            Record::NoteOnlyTurnIssued {
                agent: _,
                turn: _,
                goal: _,
                task: _,
                session: _,
                mode: _,
                model_selection: _,
                lsn_of_decision,
                answers,
                note_lsn,
                completion_lsn,
            }
            | Record::TaughtNoteOnlyTurnIssued {
                agent: _,
                turn: _,
                goal: _,
                task: _,
                session: _,
                mode: _,
                model_selection: _,
                lsn_of_decision,
                answers,
                note_lsn,
                completion_lsn,
            } => std::iter::once(*lsn_of_decision)
                .chain(answers.iter().copied())
                .chain([*note_lsn, *completion_lsn])
                .collect(),
            Record::TurnIssuedWithBrowserNote {
                agent: _,
                turn: _,
                goal: _,
                task: _,
                session: _,
                mode: _,
                model_selection: _,
                lsn_of_decision,
                answers,
                note_lsn,
            } => std::iter::once(*lsn_of_decision)
                .chain(answers.iter().copied())
                .chain([*note_lsn])
                .collect(),
            // A browser answer names the `HumanQuestion` it closes; a local or older answer
            // names nothing.
            Record::HumanAnswer {
                agent: _,
                text: _,
                provenance: _,
                browser_answer,
            } => browser_answer
                .iter()
                .map(|attribution| attribution.question_lsn)
                .collect(),
            // A browser pause or resume names the control identity its page presented; a local or
            // older one, or a browser one presented with none, names nothing.
            Record::Pause { agent: _, browser } | Record::Resume { agent: _, browser } => browser
                .iter()
                .filter_map(|attribution| attribution.control_lsn)
                .collect(),
            Record::MemoryCondense {
                agent: _,
                from_lsn,
                to_lsn,
                summary_ref: _,
                model_usage: _,
            } => vec![*from_lsn, *to_lsn],
            // The completion carries the position of the request whose bytes it reports on, so it
            // belongs here rather than in the ignored arm.
            //
            // **This classifier protects nothing today, and saying so is the point.** `lsns_of` is
            // declared inside `#[cfg(test)]` and has exactly two references — this declaration and
            // the sweep below over `dudes-proto`'s sample corpus. No retention, pruning or
            // archival pass exists in this crate to consume it, so nothing here keeps a
            // `PtyWriteCompleted`'s request reachable. What the arm buys is that the constraint is
            // recorded *where it will be read* if such a pass is written: the match has no
            // wildcard, so its author inherits a correct classification of which records carry a
            // cross-record position instead of re-deriving it, and `request_lsn` is already in the
            // set rather than discovered after a sweep has dropped the segment holding it.
            Record::PtyWriteCompleted {
                request_lsn,
                outcome: _,
            }
            | Record::PtyCloseCompleted {
                request_lsn,
                outcome: _,
            } => vec![*request_lsn],
            // Both name the `Steer` whose note they settle, so an archival pass that dropped the
            // segment holding it would leave a consumption or a clear pointing at nothing. Same
            // constraint as `request_lsn` above, recorded in the same place.
            Record::SteerConsumed {
                agent: _,
                steer_lsn,
            }
            | Record::SteerCleared {
                agent: _,
                steer_lsn,
            } => vec![*steer_lsn],
            Record::ConversationTurnDecided {
                note_lsn,
                context_lsn,
                ..
            } => vec![*note_lsn, *context_lsn],
            Record::ConversationTurnIssued {
                note_lsn,
                context_lsn,
                lsn_of_decision,
                ..
            } => vec![*note_lsn, *context_lsn, *lsn_of_decision],
            Record::ConversationMessageQueued { context_lsn, .. }
            | Record::ConversationContextResetQueued { context_lsn, .. } => vec![*context_lsn],
            Record::ConversationDispatchUncertain {
                dispatch_intent_lsn,
                ..
            } => vec![*dispatch_intent_lsn],
            Record::ConversationQuestion { origin, .. } => vec![origin.context_lsn],
            Record::ConversationAnswerQueued {
                origin,
                question_lsn,
                ..
            } => vec![origin.context_lsn, *question_lsn],
            Record::BrowserNoteQueued {
                agent: _,
                text: _,
                browser: _,
                destination: _,
            }
            | Record::AgentRenamed { .. }
            | Record::AgentLookChanged { .. }
            | Record::AgentMcpServersChanged { .. }
            | Record::AgentHired { .. }
            | Record::AgentHeld { .. }
            | Record::AgentRetired { .. }
            | Record::GoalSet { .. }
            | Record::GoalCleared { .. }
            | Record::GoalReassigned { .. }
            | Record::SessionLearned { .. }
            | Record::LiveActivityObserved { .. }
            | Record::TurnEnded { .. }
            | Record::PredicateResult { .. }
            | Record::LoopDecided { .. }
            | Record::BudgetDecision { .. }
            | Record::MemoryCondenseModelAdmitted { .. }
            | Record::BreakerBeat { .. }
            | Record::CardCreated { .. }
            | Record::CardTransition { .. }
            | Record::Claim { .. }
            | Record::Mail { .. }
            | Record::MailState { .. }
            | Record::HumanQuestion { .. }
            | Record::Attached { .. }
            | Record::Detached { .. }
            | Record::Steer { .. }
            | Record::Halt { .. }
            | Record::MemoryAppend { .. }
            | Record::HookTokenMinted { .. }
            | Record::ClosingTime { .. }
            | Record::Checkpoint { .. }
            | Record::KillDeferred { .. }
            | Record::ConditionedEffectSuppressed { .. }
            | Record::MemoryAppendQuarantined { .. }
            | Record::PtyWriteRequested { .. }
            | Record::PtyCloseRequested { .. }
            | Record::PtySpawned { .. }
            | Record::WorktreeLeased { .. }
            | Record::WorktreeRemoved { .. }
            | Record::TicketCreated { .. }
            | Record::TicketEdited { .. }
            | Record::TicketAssigned { .. }
            | Record::TicketStarted { .. }
            | Record::TicketRemoved { .. } => Vec::new(),
        }
    }

    #[test]
    fn no_sample_lsn_is_below_the_first_segment() {
        const EXPECTED_SAMPLE_REFERENCE_COUNT: usize = 46;

        let mut swept = 0_usize;
        let mut lsns = 0_usize;
        for &kind in RecordKind::KNOWN {
            for lsn in lsns_of(&samples::record(kind)) {
                lsns += 1;
                assert!(
                    lsn.segment() >= FIRST_SEGMENT,
                    "the {kind:?} sample carries {lsn}: segment {} is below FIRST_SEGMENT \
                     ({FIRST_SEGMENT}), and an lsn that references a frame names a segment \
                     that exists",
                    lsn.segment(),
                );
            }
            swept += 1;
        }
        assert_eq!(
            swept,
            RecordKind::KNOWN.len(),
            "a kind was skipped rather than swept"
        );
        assert_eq!(
            lsns, EXPECTED_SAMPLE_REFERENCE_COUNT,
            "the sweep examined a different number of lsns than the corpus carries"
        );

        let question_lsn = Lsn::from_parts(1, 256).unwrap();
        let mut attributed = samples::record(RecordKind::HumanAnswer);
        if let Record::HumanAnswer { browser_answer, .. } = &mut attributed {
            *browser_answer = Some(dudes_proto::record::BrowserAnswer {
                request_id: dudes_proto::record::Hex32::new([1; 32]),
                request_digest: dudes_proto::record::Hex32::new([2; 32]),
                question_lsn,
                session_digest: dudes_proto::record::Hex32::new([3; 32]),
            });
        }
        assert_eq!(
            lsns_of(&attributed),
            vec![question_lsn],
            "a browser answer names the question it closes"
        );

        let first = Record::Pause {
            agent: dudes_types::AgentId::NIL,
            browser: Some(dudes_proto::record::BrowserControl {
                request_id: dudes_proto::record::Hex32::new([1; 32]),
                request_digest: dudes_proto::record::Hex32::new([2; 32]),
                control_lsn: None,
                session_digest: dudes_proto::record::Hex32::new([3; 32]),
            }),
        };
        assert!(
            lsns_of(&first).is_empty(),
            "a pause that presented no control identity names nothing"
        );
    }
}
