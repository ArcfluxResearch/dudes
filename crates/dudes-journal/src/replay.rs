//! `replay(reader, &mut projections)`: apply every entry after `last_applied_lsn`.
//! Idempotent: a second run applies nothing.
//! Starting from a `Checkpoint` is not implemented yet; the record is applied as a no-op.
//!
//! A projection ahead of the reader's view — [`Reader::locate`] exposes no frame at
//! `last_applied_lsn` and its segment was not archived — is [`Error::ProjectionAhead`], never a
//! silent reset. Bytes or segments may have been removed after the projection was written, or a
//! fixed [`Reader::open`] torn-tail snapshot may hide bytes completed after it opened. Reopening
//! can expose the latter; if the position stays missing, the rows describe records that are gone
//! and only the human can accept that loss. The check happens on that one position before
//! anything is applied: a later segment being intact neither expands a fixed snapshot nor makes
//! an earlier truncation acceptable, and an archived segment followed by an empty current one is
//! the normal restart, not loss.
//!
//! A store that has applied **nothing** is checked against the log's beginning instead, because
//! `Lsn::ZERO` locates to an answer about the *position* rather than about the log: it is before
//! every segment, so it reads `Archived` whenever the log has a segment at all, and `Missing`
//! when it has none. A fresh store — what every `PROJECTION_VERSION` bump produces — over a log
//! whose first segment was archived would rebuild from a *suffix* and report `Ok`; that is
//! [`Error::RebuildFromSuffix`], because invariant 1 makes the projection whatever `dudes replay`
//! rebuilds from the journal and a rebuild missing its prefix is not one.
//!
//! **Only *partial* archival is refused, and the limit is a real one.** Remove the last segment
//! too and [`crate::open`] mints [`FIRST_SEGMENT`] on the empty directory, so the first present
//! segment *is* the first and a fresh store rebuilds to an empty projection: measured
//! `Ok(applied: 0)` against a whole-log rebuild holding one agent row. Telling total archival
//! from a first boot needs a *record* — archival journaled before it happens, in a segment that
//! survives it — not a segment number, and that is a decision of its own rather than a condition
//! to widen here.
//!
//! `locate` answers about a *position*, and an lsn is a position, so a segment rewritten in
//! place presents the same one for different bytes. What separates them is the frame's
//! `crc32c`, stored beside the cursor in the same commit and
//! compared here: a frame that is there but is not the one applied is
//! [`Error::ProjectionDiverged`] — the same recovery as `ProjectionAhead` under a different
//! name, because *truncated* and *rewritten* are different incidents.

use dudes_types::Lsn;

use crate::error::{Error, Result};
use crate::projections::{BATCH, Projections};
use crate::reader::{Location, Reader};
use crate::segment::{FIRST_SEGMENT, list_segments};

/// What a replay did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Records applied in this run.
    pub applied: u64,
    /// `last_applied_lsn` afterwards.
    pub last_applied: Lsn,
}

/// Bring `projections` up to the end of the journal, [`BATCH`] entries per transaction.
///
/// Fails with [`Error::ProjectionAhead`] when the reader exposes no frame at
/// `last_applied_lsn` although one was applied ([`Location::Missing`]): bytes were truncated or
/// segments removed after the projection was written, or a fixed [`Reader::open`] snapshot hides
/// bytes completed after it opened. A segment archived *at or before* the position is fine —
/// replay continues from the first record the log still has past that point.
///
/// Fails with [`Error::ProjectionDiverged`] when a frame *is* there and is not the one applied.
///
/// Fails with [`Error::RebuildFromSuffix`] when nothing has been applied and the journal's first
/// present segment is not [`FIRST_SEGMENT`]: there is no cursor to locate, and rebuilding from
/// what is left would drop the archived prefix without saying so.
///
/// The completed-note and mail-wake contracts are validated by replaying the journal from its
/// first record, so they are checked only when the first present segment is [`FIRST_SEGMENT`]. A
/// journal with an archived prefix is not validated this way, because a suffix alone cannot
/// supply the goal and mail history those records join to and would be refused for lacking it.
/// A reader from [`Reader::open`] or [`crate::Writer::reader`] reuses its successful validation
/// if a byte scan confirms the same prefix. Replay stops at the end of that scan.
pub fn replay(reader: &Reader, projections: &mut Projections) -> Result<ReplayReport> {
    let reader = reader.validate_completed_contracts()?;
    let mut applied = 0;
    let mut batch = Vec::with_capacity(BATCH);
    let last_applied = projections.last_applied_lsn();
    if last_applied == Lsn::ZERO {
        // A cursor of `Lsn::ZERO` cannot be located, and the `locate` arm below is skipped
        // rather than consulted because both answers it could give are about the *position*
        // instead of about this log. `Lsn::ZERO` sits before every segment: whenever the log has
        // a segment, `Archived` is the only answer it can get, which refuses nothing; and when
        // the log has none, `locate` returns `Missing` (`Reader::locate`'s `let else` on an
        // empty segment list), which that arm would read as `ProjectionAhead` and refuse the
        // first boot.
        //
        // What has to be checked for a store that has applied nothing is the **log's** own
        // beginning: a first present segment past `FIRST_SEGMENT` means replay would start at a
        // suffix and report `Ok`, which is the silent partial rebuild invariant 1 forbids. A
        // journal with no segment at all is not a suffix — it is the first boot, and there is
        // nothing there to be missing.
        //
        // The limit this cannot reach: total archival is indistinguishable from a first boot by
        // segment number alone, because `open` mints `FIRST_SEGMENT` on an empty directory
        // before the daemon replays. Closing it needs a record, not a wider condition here.
        if let Some(&first) = list_segments(reader.dir())?.first()
            && first != FIRST_SEGMENT
        {
            return Err(Error::RebuildFromSuffix {
                first_present: first,
            });
        }
    } else {
        match reader.locate(last_applied)? {
            Location::Missing => return Err(Error::ProjectionAhead { last_applied }),
            // Archived: the frame is gone, so there is nothing to witness, and a projection the
            // log still agrees with is not refused for that.
            Location::Archived => {}
            // Three states, not two: no witness stored, one that agrees, one that
            // does not. `crc32c` legitimately takes the value `0`, so the stored key is read as
            // `Option<u64>` and never as `unwrap_or(0)` — flattening the two would either stop
            // witnessing a store whose checksum is `0` or refuse an untouched journal for a
            // store that has no key at all.
            Location::Frame { crc } => {
                if let Some(expected) = projections.last_applied_crc()
                    && expected != u64::from(crc)
                {
                    return Err(Error::ProjectionDiverged {
                        last_applied,
                        expected,
                        found: crc,
                    });
                }
            }
        }
    }
    // `iter_from` is inclusive; the entry at `last_applied_lsn` itself is skipped by `apply`.
    // The witnessed stream carries each frame's checksum so `apply_batch` can store the one
    // that belongs to the cursor it moves, in the same commit.
    for item in reader.witnessed_from(last_applied)? {
        batch.push(item?);
        if batch.len() == BATCH {
            applied += projections.apply_batch(batch.iter().map(|(l, c, e)| (*l, *c, e)))?;
            batch.clear();
        }
    }
    applied += projections.apply_batch(batch.iter().map(|(l, c, e)| (*l, *c, e)))?;
    Ok(ReplayReport {
        applied,
        last_applied: projections.last_applied_lsn(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use dudes_proto::{Entry, Record};
    use dudes_types::{AgentId, Timestamp};

    use super::*;
    use crate::writer::{Fsync, JournalOptions};

    fn options() -> JournalOptions {
        JournalOptions {
            fsync: Fsync::None,
            rotate_at: 2048,
            checkpoint_every: 0,
        }
    }

    /// Stamp a record so it can be appended. These tests are about lsn order, which `at` never
    /// influences (`lsn` orders, `at` labels), so one stamp serves them all.
    fn stamped(record: Record) -> Entry {
        Entry {
            at: Timestamp::from_unix_millis(1_787_394_600_000),
            record,
        }
    }

    fn pause() -> Entry {
        stamped(Record::Pause {
            agent: AgentId::NIL,
            browser: None,
        })
    }

    /// replay is idempotent and resumes from `last_applied_lsn` — a
    /// second run applies nothing, and records appended later are applied exactly once,
    /// across batch and segment boundaries.
    #[test]
    fn replay_is_idempotent_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        let pause = pause();
        let n = u64::try_from(BATCH).unwrap() + 17;
        for _ in 0..n {
            w.append(&pause).unwrap();
        }
        let reader = Reader::new(&journal);
        let mut p = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        let first = replay(&reader, &mut p).unwrap();
        assert_eq!(first.applied, n);
        assert_eq!(
            first.last_applied,
            w.next_lsn().unwrap().min(first.last_applied)
        );
        assert!(first.last_applied < w.next_lsn().unwrap());
        let again = replay(&reader, &mut p).unwrap();
        assert_eq!(
            again,
            ReplayReport {
                applied: 0,
                last_applied: first.last_applied
            }
        );
        let lsn = w.append(&pause).unwrap();
        let third = replay(&reader, &mut p).unwrap();
        assert_eq!(
            third,
            ReplayReport {
                applied: 1,
                last_applied: lsn
            }
        );
        assert_eq!(p.last_applied_lsn(), lsn);
    }

    /// Invariant 1: a projection ahead of the log is refused by name, rows untouched — after
    /// the last segments are deleted, or the last one truncated. Archiving segments *before*
    /// the projection's position is not ahead: replay continues from what is left.
    #[test]
    fn projection_ahead_of_the_log_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let pause = pause();
        let mut lsns = Vec::new();
        for _ in 0..3 {
            let (mut w, _) = crate::open(&journal, options()).unwrap();
            for _ in 0..5 {
                lsns.push(w.append(&pause).unwrap());
            }
        }
        assert_eq!(
            crate::segment::list_segments(&journal).unwrap(),
            vec![1, 2, 3]
        );
        let reader = Reader::new(&journal);
        let mut p = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        let end = replay(&reader, &mut p).unwrap().last_applied;
        assert_eq!(end, *lsns.last().unwrap());

        // Archival before the position: fine.
        std::fs::remove_file(crate::segment::segment_path(&journal, 1)).unwrap();
        assert_eq!(replay(&reader, &mut p).unwrap().applied, 0);

        // The last segment truncated to a frame boundary before the position: ahead.
        let last = crate::segment::segment_path(&journal, 3);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&last)
            .unwrap()
            .set_len(end.offset())
            .unwrap();
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(err, Error::ProjectionAhead { last_applied } if last_applied == end),
            "{err:?}"
        );
        assert_eq!(p.last_applied_lsn(), end, "nothing reset");

        // The last segments deleted: ahead.
        std::fs::remove_file(&last).unwrap();
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(err, Error::ProjectionAhead { last_applied } if last_applied == end),
            "{err:?}"
        );
        assert_eq!(p.last_applied_lsn(), end);
    }

    /// after a schema-version reset, replay from the first segment
    /// rebuilds exactly the rows the pre-reset projection had.
    #[test]
    fn schema_reset_then_replay_rebuilds_the_same_rows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        let agent = AgentId::from_parts(1, 1);
        w.append(&stamped(Record::AgentHired {
            character: None,
            accent: None,
            id: agent,
            name: "Kim".into(),
            role: "engineer".into(),
            department: "eng".into(),
            provider: dudes_types::Provider::Claude,
            cwd: "/work".into(),
            budget: dudes_proto::AgentBudget {
                usd_per_day: Some(dudes_types::Usd::from_dollars(1)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }))
        .unwrap();
        for _ in 0..40 {
            w.append(&stamped(Record::Pause {
                agent,
                browser: None,
            }))
            .unwrap();
        }
        let last = w
            .append(&stamped(Record::Halt { agent, text: None }))
            .unwrap();
        let reader = Reader::new(&journal);
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        replay(&reader, &mut p).unwrap();
        let before = (p.agents().unwrap(), p.turns().unwrap());
        assert_eq!(before.0.len(), 1);
        drop(p);
        crate::projections::tests::write_schema_version(&path, crate::PROJECTION_VERSION + 1);
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO, "reset");
        assert_eq!(p.agents().unwrap().len(), 0);
        let report = replay(&reader, &mut p).unwrap();
        assert_eq!(report.applied, 42);
        assert_eq!(report.last_applied, last);
        assert_eq!((p.agents().unwrap(), p.turns().unwrap()), before);
    }

    #[test]
    fn a_version_19_store_replays_recorded_appearance() {
        const LATCHED: u64 = 19;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let mut hire = dudes_proto::record::samples::record(dudes_proto::RecordKind::AgentHired);
        let Record::AgentHired {
            character, accent, ..
        } = &mut hire
        else {
            panic!("hire sample");
        };
        *character = Some(dudes_types::CastCharacter::H);
        *accent = Some(dudes_types::AccentColour::Sky);
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        let lsn = writer.append(&stamped(hire)).unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let (frame_lsn, crc, mut old_fold_input) = reader
            .witnessed_from(Lsn::ZERO)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let Record::AgentHired {
            character, accent, ..
        } = &mut old_fold_input.record
        else {
            panic!("recorded hire");
        };
        *character = None;
        *accent = None;
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        stale.apply(frame_lsn, crc, &old_fold_input).unwrap();
        assert_eq!(stale.last_applied_lsn(), lsn);
        assert_eq!(stale.agents().unwrap()[0].character, None);
        assert_eq!(stale.agents().unwrap()[0].accent, None);
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(
            replay(&reader, &mut rebuilt).unwrap(),
            ReplayReport {
                applied: 1,
                last_applied: lsn
            }
        );
        assert_eq!(
            rebuilt.agents().unwrap()[0].character,
            Some(dudes_types::CastCharacter::H)
        );
        assert_eq!(
            rebuilt.agents().unwrap()[0].accent,
            Some(dudes_types::AccentColour::Sky)
        );
    }

    /// A v39 store folded every result as one that ran. The bump makes the refold read the
    /// recorded evidence, so a check that could not run says so after the upgrade.
    #[test]
    fn a_version_39_store_rebuilds_a_result_whose_check_did_not_run() {
        const LATCHED: u64 = 39;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let mut result =
            dudes_proto::record::samples::record(dudes_proto::RecordKind::PredicateResult);
        let Record::PredicateResult {
            goal,
            pass,
            evidence,
            ..
        } = &mut result
        else {
            panic!("result sample");
        };
        let goal = *goal;
        *pass = false;
        *evidence = dudes_proto::Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: None,
            exit: None,
            output_tail: "the predicate did not run: `npm` is not on the daemon's PATH".into(),
        };
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        for kind in [
            dudes_proto::RecordKind::AgentHired,
            dudes_proto::RecordKind::GoalSet,
        ] {
            writer
                .append(&stamped(dudes_proto::record::samples::record(kind)))
                .unwrap();
        }
        let last = writer.append(&stamped(result)).unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        for item in reader.witnessed_from(Lsn::ZERO).unwrap() {
            let (lsn, crc, mut old_fold_input) = item.unwrap();
            if let Record::PredicateResult { evidence, .. } = &mut old_fold_input.record {
                *evidence = dudes_proto::Evidence::Command {
                    argv: vec!["npm".into(), "test".into()],
                    program: Some("/opt/toolchain/bin/npm".into()),
                    exit: None,
                    output_tail: String::new(),
                };
            }
            stale.apply(lsn, crc, &old_fold_input).unwrap();
        }
        let stale_result = stale.goal(goal).unwrap().unwrap().last_result.unwrap();
        assert!(!stale_result.not_run, "control: the old fold kept it false");
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(replay(&reader, &mut rebuilt).unwrap().last_applied, last);
        let result = rebuilt.goal(goal).unwrap().unwrap().last_result.unwrap();
        assert!(result.not_run && !result.pass, "{result:?}");
    }

    /// A v40 store folded every goal without the baseline its `GoalSet` recorded. The bump makes
    /// the refold read it, so a goal whose check failed before its first turn says so after the
    /// upgrade.
    #[test]
    fn a_version_40_store_rebuilds_a_goals_baseline() {
        const LATCHED: u64 = 40;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let baseline = dudes_proto::CheckBaseline::CommandFailed {
            program: "/usr/bin/test".into(),
            exit: 1,
        };
        let mut set = dudes_proto::record::samples::record(dudes_proto::RecordKind::GoalSet);
        let Record::GoalSet { goal, .. } = &mut set else {
            panic!("goal sample");
        };
        goal.baseline = Some(baseline.clone());
        let id = goal.id;
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        writer
            .append(&stamped(dudes_proto::record::samples::record(
                dudes_proto::RecordKind::AgentHired,
            )))
            .unwrap();
        let last = writer.append(&stamped(set)).unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        for item in reader.witnessed_from(Lsn::ZERO).unwrap() {
            let (lsn, crc, mut old_fold_input) = item.unwrap();
            if let Record::GoalSet { goal, .. } = &mut old_fold_input.record {
                goal.baseline = None;
            }
            stale.apply(lsn, crc, &old_fold_input).unwrap();
        }
        assert_eq!(
            stale.goal(id).unwrap().unwrap().baseline,
            None,
            "control: the old fold kept none"
        );
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(replay(&reader, &mut rebuilt).unwrap().last_applied, last);
        assert_eq!(rebuilt.goal(id).unwrap().unwrap().baseline, Some(baseline));
    }

    #[test]
    fn a_version_26_cursor_past_a_turn_rebuilds_the_trace_window() {
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        for kind in [
            dudes_proto::RecordKind::AgentHired,
            dudes_proto::RecordKind::TurnIssued,
            dudes_proto::RecordKind::TurnSpawned,
            dudes_proto::RecordKind::TurnEnded,
        ] {
            writer
                .append(&stamped(dudes_proto::record::samples::record(kind)))
                .unwrap();
        }
        drop(writer);
        let reader = Reader::new(&journal);
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        replay(&reader, &mut stale).unwrap();
        let expected = stale.traces().unwrap();
        assert_eq!(expected.len(), 1);
        drop(stale);
        crate::projections::tests::write_schema_version(&path, 26);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert!(rebuilt.traces().unwrap().is_empty());
        assert_eq!(replay(&reader, &mut rebuilt).unwrap().applied, 4);
        assert_eq!(rebuilt.traces().unwrap(), expected);
    }

    #[test]
    fn a_version_20_store_rebuilds_renamed_display_text() {
        const LATCHED: u64 = 20;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let mut hire = dudes_proto::record::samples::record(dudes_proto::RecordKind::AgentHired);
        let Record::AgentHired { id, name, .. } = &mut hire else {
            panic!("hire sample");
        };
        let agent = *id;
        *name = "Before".to_owned();
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        writer.append(&stamped(hire)).unwrap();
        let last = writer
            .append(&stamped(Record::AgentRenamed {
                id: agent,
                name: "After".to_owned(),
                browser_rename: None,
            }))
            .unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        for item in reader.witnessed_from(Lsn::ZERO).unwrap() {
            let (lsn, crc, mut old_fold_input) = item.unwrap();
            if let Record::AgentRenamed { name, .. } = &mut old_fold_input.record {
                *name = "Before".to_owned();
            }
            stale.apply(lsn, crc, &old_fold_input).unwrap();
        }
        assert_eq!(stale.last_applied_lsn(), last);
        assert_eq!(stale.agent(agent).unwrap().unwrap().name, "Before");
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(
            replay(&reader, &mut rebuilt).unwrap(),
            ReplayReport {
                applied: 2,
                last_applied: last
            }
        );
        assert_eq!(rebuilt.agent(agent).unwrap().unwrap().name, "After");
        assert!(rebuilt.activity().unwrap().iter().any(|row| matches!(
            &row.event, dudes_proto::ActivityEvent::AgentRenamed { agent: renamed }
            if renamed.id == agent && renamed.name.as_deref() == Some("After")
        )));
    }

    #[test]
    fn a_version_21_store_rebuilds_the_retired_archive() {
        const LATCHED: u64 = 21;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let hire = dudes_proto::record::samples::record(dudes_proto::RecordKind::AgentHired);
        let Record::AgentHired { id: agent, .. } = hire else {
            panic!("hire sample");
        };
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        writer.append(&stamped(hire)).unwrap();
        writer
            .append(&stamped(Record::AgentRenamed {
                id: agent,
                name: "Final name".to_owned(),
                browser_rename: None,
            }))
            .unwrap();
        let last = writer
            .append(&stamped(Record::AgentRetired {
                id: agent,
                reason: "not in the archive".to_owned(),
                browser_retire: None,
            }))
            .unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        replay(&reader, &mut stale).unwrap();
        let expected = stale.retired_agents().unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].name, "Final name");
        assert_eq!(
            expected[0].hired_at,
            Timestamp::from_unix_millis(1_787_394_600_000)
        );
        assert_eq!(stale.last_applied_lsn(), last);
        drop(stale);
        {
            let db = redb::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new("retired_agents"))
                .unwrap();
            txn.commit().unwrap();
        }
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert!(rebuilt.retired_agents().unwrap().is_empty());
        assert_eq!(
            replay(&reader, &mut rebuilt).unwrap(),
            ReplayReport {
                applied: 3,
                last_applied: last
            }
        );
        assert_eq!(rebuilt.retired_agents().unwrap(), expected);
        assert!(rebuilt.agents().unwrap().is_empty());
    }

    #[test]
    fn a_version_17_store_replays_recorded_workspace_metadata() {
        const LATCHED: u64 = 17;
        assert_eq!(crate::PROJECTION_VERSION, 45);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let workspace = dudes_proto::record::RecordedWorkspace {
            id: "alpha".into(),
            label: "Workbench".into(),
        };
        let mut hire = dudes_proto::record::samples::record(dudes_proto::RecordKind::AgentHired);
        let Record::AgentHired {
            workspace: field, ..
        } = &mut hire
        else {
            panic!("hire sample");
        };
        *field = Some(workspace.clone());
        let hire = stamped(hire);
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        let lsn = writer.append(&hire).unwrap();
        drop(writer);
        let reader = Reader::new(&journal);
        let (frame_lsn, crc, mut old_fold_input) = reader
            .witnessed_from(Lsn::ZERO)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let Record::AgentHired {
            workspace: field, ..
        } = &mut old_fold_input.record
        else {
            panic!("recorded hire");
        };
        *field = None;
        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        stale.apply(frame_lsn, crc, &old_fold_input).unwrap();
        assert_eq!(stale.agents().unwrap()[0].workspace, None);
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(
            replay(&reader, &mut rebuilt).unwrap(),
            ReplayReport {
                applied: 1,
                last_applied: lsn
            }
        );
        assert_eq!(rebuilt.agents().unwrap()[0].workspace, Some(workspace));
    }

    /// A change to the `AgentHired` fold: a version-9 cache whose
    /// cursor is already past a hire cannot acquire the two recorded limits by ordinary replay,
    /// because that replay skips the hire. Opening this build must therefore reset the cache,
    /// after which replay of the same journal frame restores both values.
    ///
    /// The old-fold fixture carries the journal frame's real lsn and crc but clears the two fields
    /// before applying it. That is the rollback/re-upgrade case: the add-only record accepts the
    /// fields, while an older projection row has nowhere to put them and advances its cursor with
    /// both absent. A made-up witness would fail earlier as `ProjectionDiverged` and would not test
    /// the version reset at all.
    #[test]
    fn a_version_9_store_replays_recorded_run_limits() {
        const LATCHED: u64 = 9;
        assert_eq!(
            crate::PROJECTION_VERSION,
            45,
            "this fold change owns the 9 -> 10 bump; moving the version requires re-deriving the \
             latched store and current build together"
        );

        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let path = dir.path().join("state.redb");
        let agent = AgentId::from_parts(1, 1);
        let (mut writer, _) = crate::open(&journal, options()).unwrap();
        let hired = stamped(Record::AgentHired {
            character: None,
            accent: None,
            id: agent,
            name: "Kim".into(),
            role: "engineer".into(),
            department: "eng".into(),
            provider: dudes_types::Provider::Claude,
            cwd: "/work".into(),
            budget: dudes_proto::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: Some(7),
            max_budget: Some(dudes_types::Usd::from_micros(2_500_000)),
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        });
        let lsn = writer.append(&hired).unwrap();
        drop(writer);

        let reader = Reader::new(&journal);
        let (frame_lsn, crc, mut old_fold_input) = reader
            .witnessed_from(Lsn::ZERO)
            .unwrap()
            .next()
            .expect("the hire frame exists")
            .unwrap();
        assert_eq!(frame_lsn, lsn, "control: this is the appended hire");
        if let Record::AgentHired {
            max_turns,
            max_budget,
            ..
        } = &mut old_fold_input.record
        {
            *max_turns = None;
            *max_budget = None;
        } else {
            panic!("control: the journal frame is AgentHired");
        }

        let mut stale = Projections::open_with(&path, Fsync::None).unwrap();
        assert!(stale.apply(frame_lsn, crc, &old_fold_input).unwrap());
        assert_eq!(
            stale
                .agents()
                .unwrap()
                .iter()
                .map(|row| (row.max_turns, row.max_budget))
                .collect::<Vec<_>>(),
            [(None, None)],
            "control: the version-9 fold advanced past the hire without either limit"
        );
        drop(stale);
        crate::projections::tests::write_schema_version(&path, LATCHED);

        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            rebuilt.last_applied_lsn(),
            Lsn::ZERO,
            "version mismatch resets"
        );
        assert!(
            rebuilt.agents().unwrap().is_empty(),
            "the stale row is dropped"
        );
        assert_eq!(
            replay(&reader, &mut rebuilt).unwrap(),
            ReplayReport {
                applied: 1,
                last_applied: lsn,
            }
        );
        assert_eq!(
            rebuilt
                .agents()
                .unwrap()
                .iter()
                .map(|row| (row.max_turns, row.max_budget))
                .collect::<Vec<_>>(),
            [(Some(7), Some(dudes_types::Usd::from_micros(2_500_000)))],
            "replay of the journal, rather than a cache default, restores both recorded limits"
        );
    }

    /// Five `Pause` records in segment 1, replayed into a fresh store; the writer is dropped.
    fn five_replayed(dir: &Path) -> (PathBuf, Reader, Projections, Vec<Lsn>) {
        let journal = dir.join("journal");
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        let lsns: Vec<_> = (0..5).map(|_| w.append(&pause()).unwrap()).collect();
        drop(w);
        let reader = Reader::new(&journal);
        let mut p = Projections::open_with(&dir.join("state.redb"), Fsync::None).unwrap();
        assert_eq!(replay(&reader, &mut p).unwrap().last_applied, lsns[4]);
        (journal, reader, p, lsns)
    }

    /// Invariant 1: a projection never describes records the log lacks, even when later
    /// segments are intact. Segment 1 truncated to a boundary below `last_applied_lsn` with
    /// segment 2 whole is `ProjectionAhead`, not a silent skip to segment 2.
    #[test]
    fn truncated_earlier_segment_is_ahead_even_with_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, reader, mut p, lsns) = five_replayed(dir.path());
        // A restart rotates: five more records land in segment 2.
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        assert_eq!(w.segment(), 2);
        for _ in 0..5 {
            w.append(&pause()).unwrap();
        }
        drop(w);
        std::fs::OpenOptions::new()
            .write(true)
            .open(crate::segment::segment_path(&journal, 1))
            .unwrap()
            .set_len(lsns[2].offset())
            .unwrap();
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(err, Error::ProjectionAhead { last_applied } if last_applied == lsns[4]),
            "{err:?}"
        );
        assert_eq!(p.last_applied_lsn(), lsns[4], "nothing reset");
    }

    /// Invariant 1: a projection the log still agrees with is never
    /// discarded — the segment holding `last_applied_lsn` archived after a restart left an
    /// empty current segment is the normal case, not loss: replay applies nothing and carries
    /// on from the next append.
    ///
    /// an archived frame cannot be witnessed and is not refused for that. The
    /// store does hold a witness here, so the archived arm is skipping a comparison it could
    /// have made rather than passing because there was nothing to compare.
    #[test]
    fn archived_segment_with_an_empty_current_one_is_not_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, reader, mut p, lsns) = five_replayed(dir.path());
        assert!(p.last_applied_crc().is_some(), "a witness was stored");
        // Restart without appending: segment 2 exists and is empty. Then archive segment 1.
        let (w, _) = crate::open(&journal, options()).unwrap();
        assert_eq!(w.segment(), 2);
        drop(w);
        std::fs::remove_file(crate::segment::segment_path(&journal, 1)).unwrap();
        assert_eq!(crate::segment::list_segments(&journal).unwrap(), vec![2]);
        let report = replay(&reader, &mut p).unwrap();
        assert_eq!((report.applied, report.last_applied), (0, lsns[4]));
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        let next = w.append(&pause()).unwrap();
        let report = replay(&reader, &mut p).unwrap();
        assert_eq!((report.applied, report.last_applied), (1, next));
    }

    /// Invariant 1: the projection is whatever `dudes replay` rebuilds from the journal, so a
    /// rebuild that would start at a **suffix** is refused by name instead of reported as `Ok`.
    /// A store that has applied nothing is the ordinary case — every `PROJECTION_VERSION` bump
    /// makes one — and its cursor cannot be located: `Lsn::ZERO` sits before every segment, so
    /// whenever the log has a segment the `ProjectionAhead` check reads `Archived` for it and
    /// **cannot** go red for this input. That is why the arm below is not covered by
    /// [`projection_ahead_of_the_log_is_refused`].
    ///
    /// Three arms over one journal — `AgentHired` and a `Pause` in segment 1, a `Halt` in
    /// segment 2 — differing only in the store's cursor and in whether segment 1 is present:
    ///
    /// | segment 1 | cursor | expected |
    /// |---|---|---|
    /// | present | `Lsn::ZERO` (fresh) | `Ok`, one agent row |
    /// | archived | `Lsn::ZERO` (fresh) | `RebuildFromSuffix { first_present: 2 }` |
    /// | archived | inside segment 1 | `Ok`, unchanged — `locate`'s archived arm |
    ///
    /// The controls are what license the subject. The first says the prefix carries state the
    /// suffix does not; the third says the refusal is keyed to the *empty* cursor and not to
    /// archival, which is the normal restart and stays `Ok`.
    ///
    /// What the first control does **not** show, because these record shapes cannot: that a
    /// suffix rebuild *disagrees* with a whole-log one. Every fold here is monotone in the agent
    /// row, so the suffix's `agents() == {}` is a state the whole-log rebuild passes through on
    /// its way — a sub-state, consistent with merely lagging. A genuine disagreement needs a
    /// record whose fold is not a no-op for an agent that was never hired, and that is a
    /// different fixture rather than a bigger one.
    #[test]
    fn a_fresh_store_over_a_front_archived_log_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let agent = AgentId::from_parts(1, 1);
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        w.append(&stamped(Record::AgentHired {
            character: None,
            accent: None,
            id: agent,
            name: "Kim".into(),
            role: "engineer".into(),
            department: "eng".into(),
            provider: dudes_types::Provider::Claude,
            cwd: "/work".into(),
            budget: dudes_proto::AgentBudget {
                usd_per_day: Some(dudes_types::Usd::from_dollars(1)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }))
        .unwrap();
        w.append(&stamped(Record::Pause {
            agent,
            browser: None,
        }))
        .unwrap();
        drop(w);
        let reader = Reader::new(&journal);

        // A store that applied segment 1 while it was the whole log: the third arm's cursor.
        let mut resumed =
            Projections::open_with(&dir.path().join("resumed.redb"), Fsync::None).unwrap();
        assert_eq!(replay(&reader, &mut resumed).unwrap().applied, 2);
        assert_eq!(resumed.last_applied_lsn().segment(), FIRST_SEGMENT);

        // A restart rotates: the halt lands in segment 2.
        let (mut w, _) = crate::open(&journal, options()).unwrap();
        assert_eq!(w.segment(), 2);
        w.append(&stamped(Record::Halt { agent, text: None }))
            .unwrap();
        drop(w);
        assert_eq!(
            crate::segment::list_segments(&journal).unwrap(),
            vec![FIRST_SEGMENT, 2]
        );

        // Control, the pass side: the whole log into a fresh store.
        let mut whole =
            Projections::open_with(&dir.path().join("whole.redb"), Fsync::None).unwrap();
        assert_eq!(replay(&reader, &mut whole).unwrap().applied, 3);
        let rows = (whole.agents().unwrap(), whole.turns().unwrap());
        assert_eq!(rows.0.len(), 1, "the prefix is what hires the agent");

        // Archive segment 1.
        std::fs::remove_file(crate::segment::segment_path(&journal, FIRST_SEGMENT)).unwrap();
        assert_eq!(crate::segment::list_segments(&journal).unwrap(), vec![2]);
        assert!(
            reader
                .iter_from(Lsn::ZERO)
                .unwrap()
                .all(|item| !matches!(item.unwrap().1.record, Record::AgentHired { .. })),
            "the control the subject rests on: the hire is only in the archived segment, so the \
             prefix carries state the suffix does not"
        );

        // Subject: a fresh store over the front-archived log.
        let mut fresh =
            Projections::open_with(&dir.path().join("fresh.redb"), Fsync::None).unwrap();
        let err = replay(&reader, &mut fresh).unwrap_err();
        assert!(
            matches!(err, Error::RebuildFromSuffix { first_present } if first_present == 2),
            "{err:?}"
        );
        assert_eq!(fresh.last_applied_lsn(), Lsn::ZERO, "nothing applied");
        assert!(fresh.agents().unwrap().is_empty(), "no partial rebuild");

        // Control, the unchanged side: the same log, a cursor inside the archived segment.
        let report = replay(&reader, &mut resumed).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            (resumed.agents().unwrap(), resumed.turns().unwrap()),
            rows,
            "resuming across the archived segment still reaches the whole-log rows"
        );
    }

    /// The carve-out the zero-cursor arm makes for a journal with **no segment at all**: the
    /// first boot, which is not a suffix because there is nothing there to be missing. It is the
    /// one input on which `locate` would answer `Missing` for `Lsn::ZERO` and the
    /// `ProjectionAhead` arm would refuse, so skipping that arm is what keeps a first boot
    /// bootable.
    ///
    /// It gets a test of its own because no other arm reaches it: a mutant that refuses a
    /// segment-less journal survives the rest of the workspace's tests. It looks dead at the daemon's own call site — `open` mints
    /// [`FIRST_SEGMENT`] before replay runs, so the list is never empty there — and is live for a
    /// caller that replays a directory nobody has written, which is the offline replay
    /// [`Projections::in_memory`] is held open for.
    #[test]
    fn a_journal_with_no_segment_is_the_first_boot_not_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        std::fs::create_dir_all(&journal).unwrap();
        assert!(crate::segment::list_segments(&journal).unwrap().is_empty());
        let mut p = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        let report = replay(&Reader::new(&journal), &mut p).unwrap();
        assert_eq!((report.applied, report.last_applied), (0, Lsn::ZERO));
    }

    /// One frame, encoded exactly as the writer would encode it, so a segment can be rebuilt
    /// with a **different intact frame** at a position the projection has already applied.
    fn frame_bytes(entry: &Entry) -> Vec<u8> {
        let payload = serde_json::to_vec(entry).unwrap();
        let mut out = Vec::new();
        crate::frame::encode(entry.record.kind().tag(), &payload, &mut out).unwrap();
        out
    }

    /// an lsn is a position, so a segment rewritten in place presents
    /// the same one for different bytes and `ProjectionAhead` stays quiet. The frame's `crc32c`
    /// is what separates them, and the two are different incidents with different names.
    ///
    /// Three arms from one projection state — `last_applied_lsn` at `lsns[4]`, the witness the
    /// first replay stored — differing only in segment 1's bytes at that offset. The controls
    /// are what license the subject: truncation and untouched bytes give two answers different
    /// from the subject's and from each other, so the check is wired to this position and *can*
    /// go red. Before the witness it could not go red for the subject's input.
    #[test]
    fn a_different_intact_frame_at_the_cursor_is_diverged_not_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let (journal, reader, p, lsns) = five_replayed(dir.path());
        let end = lsns[4];
        let segment = crate::segment::segment_path(&journal, 1);
        let original = std::fs::read(&segment).unwrap();
        let applied_crc = match reader.locate(end).unwrap() {
            Location::Frame { crc } => crc,
            other => panic!("{other:?}"),
        };
        drop(p);
        // Each arm is a *restart* meeting the segment as it now stands: the store is reopened
        // from the file, so the witness under test is the one that was committed and not one
        // an open handle is still holding.
        let reopen = || Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            reopen().last_applied_crc(),
            Some(u64::from(applied_crc)),
            "the store witnesses the frame it applied"
        );

        // Subject: a `Halt` where a `Pause` was applied. Same offset, intact frame, different
        // record — and, before this check, a silent `Ok`.
        let halt = stamped(Record::Halt {
            agent: AgentId::NIL,
            text: None,
        });
        let mut rewritten = original[..usize::try_from(end.offset()).unwrap()].to_vec();
        rewritten.extend_from_slice(&frame_bytes(&halt));
        std::fs::write(&segment, &rewritten).unwrap();
        let found = match reader.locate(end).unwrap() {
            Location::Frame { crc } => crc,
            other => panic!("{other:?}"),
        };
        assert_ne!(
            found, applied_crc,
            "the control the subject rests on: the two frames differ in checksum"
        );
        let mut p = reopen();
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(
                err,
                Error::ProjectionDiverged { last_applied, expected, found: got }
                    if last_applied == end
                        && expected == u64::from(applied_crc)
                        && got == found
            ),
            "{err:?}"
        );
        assert_eq!(p.last_applied_lsn(), end, "nothing reset");
        drop(p);

        // Control A, the fail side: truncated at the very same offset. Nothing is there at all,
        // which is the older name.
        std::fs::write(
            &segment,
            &original[..usize::try_from(end.offset()).unwrap()],
        )
        .unwrap();
        let mut p = reopen();
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(err, Error::ProjectionAhead { last_applied } if last_applied == end),
            "{err:?}"
        );
        assert_eq!(p.last_applied_lsn(), end, "nothing reset");
        drop(p);

        // Control B, the pass side: the original bytes back. The witness agrees, so replay
        // proceeds and applies nothing.
        std::fs::write(&segment, &original).unwrap();
        let mut p = reopen();
        let report = replay(&reader, &mut p).unwrap();
        assert_eq!((report.applied, report.last_applied), (0, end));
    }

    /// the stored key has **three** states and the type keeps them apart. `None` —
    /// a store no build has witnessed — skips the comparison; `Some(0)` is a witness whose
    /// value happens to be zero and is compared like any other. Reading the cell as
    /// `unwrap_or(0)` answers one of the two wrongly whichever way the branch is then written:
    /// *skip when zero* stops witnessing one store in 2³², and *compare whatever was read*
    /// refuses a journal nobody touched.
    ///
    /// **Both arms use a journal whose own checksum is non-zero, and that is the choice that
    /// makes them discriminate.** Enumerate the four state pairs against the three
    /// implementations — correct, skip-when-zero, compare-always:
    ///
    /// | stored | journal crc | correct | skip-when-zero | compare-always |
    /// |---|---|---|---|---|
    /// | `Some(0)` | non-zero | `Err` | **`Ok`** | `Err` |
    /// | `None` | non-zero | `Ok` | `Ok` | **`Err`** |
    /// | `Some(0)` | zero | `Ok` | `Ok` | `Ok` |
    /// | `None` | zero | `Ok` | `Ok` | `Ok` |
    ///
    /// The two rows needing a frame whose real checksum is zero agree across all three, so
    /// they separate nothing and forging one buys no coverage. Forging one is not hard — a
    /// `crc32c` is affine over GF(2), so a preimage is a 32×32 linear solve, not a search —
    /// and it is still not worth writing, which is a different claim from "it cannot be done".
    /// The two rows above are the whole of the discrimination.
    ///
    /// The **write** side of a zero witness is a separate question and a separate test:
    /// [`crate::projections::tests::a_zero_witness_is_committed_not_skipped_as_absent`].
    #[test]
    fn an_absent_witness_is_skipped_and_a_stored_zero_is_compared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let (_journal, reader, p, lsns) = five_replayed(dir.path());
        let end = lsns[4];
        let applied_crc = match reader.locate(end).unwrap() {
            Location::Frame { crc } => crc,
            other => panic!("{other:?}"),
        };
        // The control both arms below rest on: the journal's own checksum is not zero, so
        // `Some(0)` is a *different* witness and `None` is a *missing* one.
        assert_ne!(applied_crc, 0);
        drop(p);

        // Absent: the key removed, as a build older than the witness would have left it.
        crate::projections::tests::write_last_applied_crc(&path, None);
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!((p.last_applied_lsn(), p.last_applied_crc()), (end, None));
        let report = replay(&reader, &mut p).unwrap();
        assert_eq!((report.applied, report.last_applied), (0, end));
        drop(p);

        // Present and zero: compared, and refused, against a journal whose checksum is not zero.
        crate::projections::tests::write_last_applied_crc(&path, Some(0));
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(p.last_applied_crc(), Some(0));
        let err = replay(&reader, &mut p).unwrap_err();
        assert!(
            matches!(
                err,
                Error::ProjectionDiverged { last_applied, expected: 0, found }
                    if last_applied == end && found == applied_crc
            ),
            "{err:?}"
        );
    }
}
