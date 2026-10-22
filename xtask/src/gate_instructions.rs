use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use dudes_proto::instruction_state::cache::{
    InstructionCache, InstructionCacheReader, InstructionCacheRow,
};
use dudes_proto::instruction_state::{InstructionAttempt, InstructionState};
use dudes_proto::{
    Entry, InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionEndBoundary,
    InstructionGapKind, InstructionIssueAssociation, InstructionNativeBoundaryReceipt,
    InstructionObservationOwner, InstructionPathContext, InstructionSampleUsage,
    InstructionSettlement, InstructionSnapshotSource, InstructionUnknownReason, Record,
    TurnOutcome,
};
use dudes_types::{AgentId, GoalId, Lsn, Mode, Provenance, Timestamp, TurnId};

const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Default)]
struct Rows(BTreeMap<Vec<u8>, Vec<u8>>);

impl InstructionCacheReader for Rows {
    type Error = anyhow::Error;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }

    fn prefix(&self, prefix: &[u8]) -> Result<Vec<InstructionCacheRow>> {
        Ok(self
            .0
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }
}

struct Journal(BTreeMap<Lsn, Record>);

struct Export {
    through: Lsn,
    rows: Rows,
}

#[derive(Clone, Copy)]
struct Issue {
    agent: AgentId,
    turn: TurnId,
    goal: GoalId,
    lsn: Lsn,
}

type CompletedAttempt = (Lsn, Lsn, InstructionIssueAssociation, bool);

pub(crate) fn run(
    crash_journal: &Path,
    crash_export: &Path,
    control_journal: &Path,
    control_export: &Path,
) -> Result<()> {
    validate(
        &read_document(crash_journal)?,
        &read_document(crash_export)?,
        true,
    )
    .context("crashed run's instruction evidence")?;
    validate(
        &read_document(control_journal)?,
        &read_document(control_export)?,
        false,
    )
    .context("control run's instruction evidence")?;
    println!(
        "gate: exact instruction histories and projection cells validated (crash 18, control 14)"
    );
    Ok(())
}

fn read_document(path: &Path) -> Result<String> {
    let mut text = String::new();
    File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        text.len() as u64 <= MAX_DOCUMENT_BYTES,
        "evidence document exceeds the gate's bound"
    );
    Ok(text)
}

fn journal(text: &str) -> Result<Journal> {
    let mut records = BTreeMap::new();
    let mut previous = Lsn::ZERO;
    for (line, text) in text.lines().enumerate() {
        let mut columns = text.splitn(3, ' ');
        let position = Lsn(columns
            .next()
            .context("journal position missing")?
            .parse()?);
        let kind = columns.next().context("journal kind missing")?;
        let record: Record =
            serde_json::from_str(columns.next().context("journal record missing")?)
                .with_context(|| format!("journal record on line {}", line + 1))?;
        ensure!(
            position > previous,
            "journal positions are not strictly ordered"
        );
        ensure!(
            serde_json::to_string(&record.kind())?.trim_matches('"') == kind,
            "journal kind disagrees with its typed record"
        );
        ensure!(
            !matches!(record, Record::TicketInstructionReviewed { .. }),
            "instruction reviews are outside this gate transcript"
        );
        records.insert(position, record);
        previous = position;
    }
    ensure!(!records.is_empty(), "journal is empty");
    Ok(Journal(records))
}

fn export(text: &str) -> Result<Export> {
    let mut through = None;
    let mut rows = Rows::default();
    for line in text.lines() {
        if let Some(position) = line.strip_prefix("lsn ") {
            ensure!(through.is_none(), "duplicate export cursor");
            through = Some(Lsn(position.parse()?));
        } else if let Some(cell) = line.strip_prefix("instructions ") {
            let (key, value) = cell
                .split_once(' ')
                .context("instruction export cell missing")?;
            let key = hex_key(key)?;
            let value: serde_json::Value = serde_json::from_str(value)?;
            ensure!(
                rows.0.insert(key, serde_json::to_vec(&value)?).is_none(),
                "duplicate instruction export key"
            );
        }
    }
    Ok(Export {
        through: through.context("export cursor missing")?,
        rows,
    })
}

fn hex_key(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len().is_multiple_of(2),
        "invalid structural key length"
    );
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let byte = std::str::from_utf8(pair)?;
            u8::from_str_radix(byte, 16).context("invalid structural key")
        })
        .collect()
}

fn derive_rows(journal: &Journal) -> Result<Rows> {
    let mut rows = Rows::default();
    let mut through = Lsn::ZERO;
    let unused_outer_timestamp = Timestamp::from_unix_millis(0);
    for (&lsn, record) in &journal.0 {
        let mut cache = InstructionCache::load(&rows, through, record, &[])?;
        let entry = Entry {
            at: unused_outer_timestamp,
            record: record.clone(),
        };
        cache.state_mut().apply(lsn, &entry, None, |_| {
            unreachable!("review records were rejected before instruction folding")
        })?;
        for update in cache.updates()? {
            match update.value {
                Some(value) => {
                    rows.0.insert(update.key, value);
                }
                None => {
                    rows.0.remove(&update.key);
                }
            }
        }
        through = lsn;
    }
    Ok(rows)
}

fn normalized(rows: &Rows) -> Result<BTreeMap<Vec<u8>, serde_json::Value>> {
    rows.0
        .iter()
        .map(|(key, bytes)| Ok((key.clone(), serde_json::from_slice(bytes)?)))
        .collect()
}

fn validate(journal_text: &str, export_text: &str, crashed: bool) -> Result<()> {
    let journal = journal(journal_text)?;
    let export = export(export_text)?;
    ensure!(
        journal.0.last_key_value().map(|(lsn, _)| *lsn) == Some(export.through),
        "export cursor is not the complete journal cut"
    );
    let state = InstructionCache::read_all(&export.rows, export.through)?;
    let expected = derive_rows(&journal)?;
    ensure!(
        normalized(&export.rows)? == normalized(&expected)?,
        "instruction export differs from the exact typed journal fold"
    );
    inventory(&export.rows, crashed)?;
    record_inventory(&journal, crashed)?;
    history(&journal, &state, crashed)
}

fn record_inventory(journal: &Journal, crashed: bool) -> Result<()> {
    let mut counts = BTreeMap::new();
    for record in journal.0.values() {
        let category = match record {
            Record::InstructionObservationStarted { .. } => "started",
            Record::InstructionObservationFinished { .. } => "finished",
            Record::InstructionAttemptBoundarySettled { .. } => "settled",
            Record::TurnIssued { .. } => "issued",
            Record::TurnSpawned { .. } => "spawned",
            Record::TurnEnded { .. } => "ended",
            _ => continue,
        };
        *counts.entry(category).or_insert(0_usize) += 1;
    }
    let attempts = if crashed { 3 } else { 2 };
    ensure!(
        counts
            == BTreeMap::from([
                ("started", attempts),
                ("finished", attempts),
                ("settled", 2),
                ("issued", 2),
                ("spawned", 2),
                ("ended", 2)
            ]),
        "extra or missing instruction/turn records in gate history"
    );
    Ok(())
}

fn inventory(rows: &Rows, crashed: bool) -> Result<()> {
    let mut actual = BTreeMap::new();
    for value in normalized(rows)?.values() {
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .context("cell category missing")?;
        *actual.entry(kind.to_owned()).or_insert(0_usize) += 1;
    }
    let attempts = if crashed { 3 } else { 2 };
    let mut expected = BTreeMap::from([
        ("attempt".to_owned(), attempts),
        ("snapshot".to_owned(), attempts),
        ("finished".to_owned(), attempts),
        ("issue".to_owned(), 2),
        ("end".to_owned(), 2),
        ("native_receipt".to_owned(), 2),
        ("observed_turn".to_owned(), 2),
    ]);
    if crashed {
        expected.insert("unresolved".to_owned(), 1);
    }
    ensure!(
        actual == expected,
        "instruction cell inventory is outside the exact gate history"
    );
    Ok(())
}

fn issues(journal: &Journal) -> Result<Vec<Issue>> {
    let mut issued = Vec::new();
    for (&lsn, record) in &journal.0 {
        if let Record::TurnIssued {
            agent,
            turn,
            goal,
            mode,
            ..
        } = record
        {
            ensure!(*mode == Mode::Managed, "gate issue is not managed");
            ensure!(
                !issued.iter().any(|old: &Issue| old.turn == *turn),
                "duplicate canonical turn issue"
            );
            issued.push(Issue {
                agent: *agent,
                turn: *turn,
                goal: *goal,
                lsn,
            });
        }
    }
    ensure!(
        issued.len() == 2,
        "gate requires exactly two immutable issued turns"
    );
    ensure!(
        issued[0].agent == issued[1].agent && issued[0].goal == issued[1].goal,
        "issued turns do not retain the same agent and goal"
    );
    Ok(issued)
}

fn history(journal: &Journal, state: &InstructionState, crashed: bool) -> Result<()> {
    let issues = issues(journal)?;
    let mut completed = Vec::new();
    let mut unknown = Vec::new();
    for (&start, attempt) in state.attempts() {
        let InstructionObservationOwner::Managed { agent, turn, issue } = attempt.owner else {
            anyhow::bail!("gate attempt has a non-managed owner");
        };
        let canonical = issues
            .iter()
            .find(|issued| issued.agent == agent && issued.turn == turn)
            .context("attempt has no exact canonical issue")?;
        snapshot(journal, start, attempt, agent)?;
        ensure!(
            state.issue_lsn(turn) == Some(canonical.lsn),
            "cache issue linkage differs"
        );
        match &attempt
            .finished
            .as_ref()
            .context("attempt has no terminal evidence")?
            .settlement
        {
            InstructionSettlement::Compared { .. } => {
                let reissued = completed_attempt(journal, state, start, attempt, *canonical)?;
                completed.push((canonical.lsn, start, issue, reissued));
            }
            InstructionSettlement::Unknown { .. } => {
                old_unknown(journal, state, start, attempt, *canonical)?;
                unknown.push((start, attempt));
            }
            _ => anyhow::bail!("unsupported gate attempt settlement"),
        }
    }
    completed.sort_by_key(|row| row.0);
    ensure!(
        completed.len() == 2,
        "gate requires exactly two completed physical attempts"
    );
    ensure!(
        completed[0].0 == issues[0].lsn && completed[1].0 == issues[1].lsn,
        "completed attempts do not cover each exact issue"
    );
    ensure!(
        completed[0].2 == InstructionIssueAssociation::Prospective && !completed[0].3,
        "first physical attempt changed provenance"
    );
    if crashed {
        crash_history(state, &issues, &completed, &unknown)?;
    } else {
        ensure!(
            unknown.is_empty() && state.unresolved_owners().next().is_none(),
            "control contains uncertainty"
        );
        ensure!(
            completed[1].2 == InstructionIssueAssociation::Prospective && !completed[1].3,
            "control's second attempt changed provenance"
        );
    }
    Ok(())
}

fn crash_history(
    state: &InstructionState,
    issues: &[Issue],
    completed: &[CompletedAttempt],
    unknown: &[(Lsn, &InstructionAttempt)],
) -> Result<()> {
    ensure!(
        unknown.len() == 1,
        "crash requires one exact old unknown attempt"
    );
    let (old_start, old) = unknown[0];
    ensure!(
        matches!(old.owner, InstructionObservationOwner::Managed { turn, .. } if turn == issues[1].turn),
        "old unknown does not belong to the reissued immutable turn"
    );
    ensure!(
        completed[1].2
            == InstructionIssueAssociation::Issued {
                issue_lsn: issues[1].lsn
            }
            && completed[1].3,
        "retry did not retain its exact canonical issue and reissue provenance"
    );
    ensure!(
        old.finished.as_ref().unwrap().lsn < completed[1].1 && old_start < issues[1].lsn,
        "retry did not follow the old prospective recovery evidence"
    );
    let unresolved: Vec<_> = state
        .unresolved_launch_attempts(issues[1].agent)
        .map(|(lsn, _)| *lsn)
        .collect();
    ensure!(
        unresolved == [old_start],
        "unresolved cache does not retain the exact old baseline"
    );
    ensure!(
        state.unresolved_owners().collect::<Vec<_>>() == [issues[1].agent],
        "extra unresolved owner"
    );
    Ok(())
}

fn snapshot(
    journal: &Journal,
    start: Lsn,
    attempt: &InstructionAttempt,
    agent: AgentId,
) -> Result<()> {
    ensure!(
        attempt.baseline == InstructionBaselineRef::Started { lsn: start }
            && attempt.ticket.is_none()
            && attempt.reviewed_lsn.is_none(),
        "attempt identity or review changed"
    );
    let snapshot = attempt
        .snapshot
        .as_ref()
        .context("starting snapshot missing")?;
    snapshot.validate()?;
    ensure!(
        snapshot.source == InstructionSnapshotSource::Fresh,
        "starting sample is not fresh"
    );
    let cwd = journal
        .0
        .values()
        .find_map(|record| match record {
            Record::AgentHired { id, cwd, .. } if *id == agent => Some(cwd),
            _ => None,
        })
        .context("attempt owner was not hired")?;
    ensure!(
        snapshot
            .execution_folder
            .as_ref()
            .context("execution folder missing")?
            .to_path()?
            .as_path()
            == cwd,
        "sample execution folder differs from the recorded gate folder"
    );
    ensure!(
        matches!(journal.0.get(&start), Some(Record::InstructionObservationStarted { owner, ticket: None, snapshot: seen })
        if *owner == attempt.owner && seen == snapshot),
        "snapshot/start relationship changed"
    );
    Ok(())
}

fn finished_record(journal: &Journal, attempt: &InstructionAttempt) -> Result<()> {
    let finished = attempt.finished.as_ref().context("finish missing")?;
    ensure!(
        matches!(journal.0.get(&finished.lsn), Some(Record::InstructionObservationFinished {
        owner, ticket: None, baseline, settlement }) if *owner == attempt.owner
        && *baseline == attempt.baseline && *settlement == finished.settlement),
        "terminal record linkage changed"
    );
    Ok(())
}

fn completed_attempt(
    journal: &Journal,
    state: &InstructionState,
    start: Lsn,
    attempt: &InstructionAttempt,
    issue: Issue,
) -> Result<bool> {
    finished_record(journal, attempt)?;
    let spawn = attempt
        .spawn_lsn
        .context("completed attempt has no spawn")?;
    ensure!(
        matches!(state.later_managed_attempt_for_spawn(issue.agent, issue.turn, Lsn::ZERO, spawn),
        Some((indexed_start, _)) if indexed_start == start),
        "native receipt index does not name the exact completed attempt"
    );
    let Some(Record::TurnSpawned {
        agent,
        turn,
        reissued,
        ..
    }) = journal.0.get(&spawn)
    else {
        anyhow::bail!("actual spawn receipt missing");
    };
    ensure!(
        *agent == issue.agent && *turn == issue.turn && issue.lsn < spawn && start < spawn,
        "spawn does not belong to the exact issued attempt"
    );
    let InstructionSettlement::Compared {
        end:
            InstructionEndBoundary::ManagedTurn {
                turn: ended,
                end_lsn,
            },
        recovery: false,
        comparison,
        ..
    } = &attempt.finished.as_ref().unwrap().settlement
    else {
        anyhow::bail!("completed attempt lacks an ordinary managed comparison");
    };
    comparison.validate()?;
    ensure!(
        comparison.changes.is_empty()
            && *ended == issue.turn
            && state.managed_end_lsn(issue.agent, issue.turn) == Some(*end_lsn),
        "managed ending linkage changed"
    );
    ensure!(
        matches!(journal.0.get(end_lsn), Some(Record::TurnEnded { agent, turn,
        mode: Mode::Managed, provenance: Provenance::Observed, outcome: Some(TurnOutcome::Completed), .. })
        if *agent == issue.agent && *turn == issue.turn),
        "actual observed managed end missing"
    );
    let boundary = attempt
        .boundary_settled
        .as_ref()
        .context("native boundary settlement missing")?;
    let proof = InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
        receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn },
    };
    ensure!(
        boundary.proof == proof
            && matches!(journal.0.get(&boundary.lsn), Some(Record::InstructionAttemptBoundarySettled {
        owner, started_lsn, proof: seen }) if *owner == attempt.owner && *started_lsn == start && *seen == proof),
        "actual native settlement is not linked to the exact attempt and spawn"
    );
    Ok(*reissued)
}

fn old_unknown(
    journal: &Journal,
    state: &InstructionState,
    start: Lsn,
    attempt: &InstructionAttempt,
    issue: Issue,
) -> Result<()> {
    finished_record(journal, attempt)?;
    ensure!(
        matches!(
            attempt.owner,
            InstructionObservationOwner::Managed {
                issue: InstructionIssueAssociation::Prospective,
                ..
            }
        ) && start < issue.lsn,
        "old unknown was not the original prospective baseline"
    );
    ensure!(
        attempt.spawn_lsn.is_none() && attempt.boundary_settled.is_none(),
        "old attempt gained native evidence"
    );
    let InstructionSettlement::Unknown {
        reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
        end: None,
        sampled_at: None,
        recovery: true,
        comparison,
    } = &attempt.finished.as_ref().unwrap().settlement
    else {
        anyhow::bail!("old attempt lost its unsampled recovery uncertainty");
    };
    ensure!(
        comparison.changes.is_empty() && comparison.usage == InstructionSampleUsage::default(),
        "old unknown claims sampled work or changes"
    );
    let [gap] = comparison.gaps.as_slice() else {
        anyhow::bail!("old unknown has unmatched coverage gaps")
    };
    ensure!(
        gap.reason == InstructionGapKind::UnrecordedLaunchBoundary
            && gap.context == InstructionPathContext::Unresolved
            && gap.agent == Some(issue.agent)
            && gap.path.is_none(),
        "old unknown gap changed scope"
    );
    ensure!(state.attempt(start).is_some(), "old baseline missing");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::{
        InstructionComparison, InstructionCoverageGap, InstructionSnapshot, NativeInstructionPath,
    };
    use serde_json::json;
    use std::fmt::Write as _;

    struct Fixture {
        journal: Journal,
        agent: AgentId,
        goal: GoalId,
    }

    impl Fixture {
        fn new() -> Self {
            let agent = AgentId::from_parts(1, 1);
            let mut fixture = Self {
                journal: Journal(BTreeMap::new()),
                agent,
                goal: GoalId::from_parts(1, 2),
            };
            fixture.push(serde_json::from_value(json!({
                "t":"agent_hired", "id":agent, "name":"builder", "role":"builder", "department":"test",
                "provider":"claude", "cwd":"/gate/work", "budget":{}, "trusted":false,
            })).unwrap());
            fixture
        }

        fn push(&mut self, record: Record) -> Lsn {
            let lsn = Lsn(self.journal.0.len() as u64 + 1);
            self.journal.0.insert(lsn, record);
            lsn
        }

        fn start(&mut self, turn: TurnId, issue: InstructionIssueAssociation) -> Lsn {
            let snapshot = InstructionSnapshot {
                sampled_at: Timestamp::from_unix_millis(10),
                source: InstructionSnapshotSource::Fresh,
                pid_namespace: None,
                boot: None,
                execution_folder: Some(NativeInstructionPath::Utf8 {
                    value: "/gate/work".into(),
                }),
                participants: Vec::new(),
                inputs: Vec::new(),
                inventoried: Vec::new(),
                gaps: vec![InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::ParticipantCoverageUnknown,
                    context: InstructionPathContext::Unresolved,
                    agent: None,
                    path: None,
                }],
                usage: InstructionSampleUsage::default(),
            };
            self.push(Record::InstructionObservationStarted {
                owner: InstructionObservationOwner::Managed {
                    agent: self.agent,
                    turn,
                    issue,
                },
                ticket: None,
                snapshot,
            })
        }

        fn issue(&mut self, turn: TurnId) -> Lsn {
            self.push(serde_json::from_value(json!({ "t":"turn_issued", "agent":self.agent, "turn":turn,
                "goal":self.goal, "task":"gate work", "mode":"managed", "model_selection":{}, "lsn_of_decision":0,
            })).unwrap())
        }

        fn spawn(&mut self, turn: TurnId, reissued: bool) -> Lsn {
            self.push(Record::TurnSpawned {
                agent: self.agent,
                turn,
                pid: 10,
                start_time: 20,
                boot: None,
                session: None,
                reissued,
                mcp_grant: None,
            })
        }

        fn end(&mut self, turn: TurnId) -> Lsn {
            self.push(serde_json::from_value(json!({ "t":"turn_ended", "agent":self.agent, "turn":turn,
                "mode":"managed", "provenance":"observed", "exit":{"code":0}, "outcome":{"kind":"completed"},
            })).unwrap())
        }

        fn owner(&self, started: Lsn) -> InstructionObservationOwner {
            let Record::InstructionObservationStarted { owner, .. } = self.journal.0[&started]
            else {
                unreachable!()
            };
            owner
        }

        fn native(&mut self, started: Lsn, spawn: Lsn) {
            self.push(Record::InstructionAttemptBoundarySettled {
                owner: self.owner(started),
                started_lsn: started,
                proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn },
                },
            });
        }

        fn finish(&mut self, started: Lsn, settlement: InstructionSettlement) {
            self.push(Record::InstructionObservationFinished {
                owner: self.owner(started),
                ticket: None,
                baseline: InstructionBaselineRef::Started { lsn: started },
                settlement,
            });
        }

        fn complete(
            &mut self,
            started: Lsn,
            turn: TurnId,
            reissued: bool,
            native_before_end: bool,
        ) {
            let spawn = self.spawn(turn, reissued);
            if native_before_end {
                self.native(started, spawn);
            }
            let end_lsn = self.end(turn);
            if !native_before_end {
                self.native(started, spawn);
            }
            self.finish(
                started,
                InstructionSettlement::Compared {
                    end: InstructionEndBoundary::ManagedTurn { turn, end_lsn },
                    sampled_at: Timestamp::from_unix_millis(30),
                    recovery: false,
                    comparison: InstructionComparison::default(),
                },
            );
        }

        fn journal_text(&self) -> String {
            let mut text = String::new();
            for (lsn, record) in &self.journal.0 {
                writeln!(
                    text,
                    "{} {} {}",
                    lsn.0,
                    serde_json::to_string(&record.kind())
                        .unwrap()
                        .trim_matches('"'),
                    serde_json::to_string(record).unwrap()
                )
                .unwrap();
            }
            text
        }

        fn documents(&self) -> (String, String) {
            let journal = self.journal_text();
            let rows = derive_rows(&self.journal).unwrap();
            let export = export_text(&rows, *self.journal.0.last_key_value().unwrap().0);
            (journal, export)
        }
    }

    fn fixture(crashed: bool, native_before_end: bool) -> Fixture {
        let mut fixture = Fixture::new();
        let first = TurnId::from_parts(1, 3);
        let second = TurnId::from_parts(1, 4);
        let started = fixture.start(first, InstructionIssueAssociation::Prospective);
        fixture.issue(first);
        fixture.complete(started, first, false, native_before_end);
        let original = fixture.start(second, InstructionIssueAssociation::Prospective);
        let issue_lsn = fixture.issue(second);
        let started = if crashed {
            fixture.finish(
                original,
                InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    end: None,
                    sampled_at: None,
                    recovery: true,
                    comparison: InstructionComparison {
                        changes: Vec::new(),
                        usage: InstructionSampleUsage::default(),
                        gaps: vec![InstructionCoverageGap {
                            origin: dudes_proto::InstructionGapOrigin::Observed,
                            reason: InstructionGapKind::UnrecordedLaunchBoundary,
                            context: InstructionPathContext::Unresolved,
                            agent: Some(fixture.agent),
                            path: None,
                        }],
                    },
                },
            );
            fixture.start(second, InstructionIssueAssociation::Issued { issue_lsn })
        } else {
            original
        };
        fixture.complete(started, second, crashed, native_before_end);
        fixture
    }

    fn export_text(rows: &Rows, through: Lsn) -> String {
        let mut export = format!("lsn {}\ncrc -\n", through.0);
        for (key, value) in normalized(rows).unwrap() {
            let mut hex = String::new();
            for byte in key {
                write!(hex, "{byte:02x}").unwrap();
            }
            writeln!(export, "instructions {hex} {value}").unwrap();
        }
        export
    }

    #[test]
    fn exact_histories_accept_both_native_and_logical_end_orders() {
        for crashed in [false, true] {
            for native_first in [false, true] {
                let (journal, export) = fixture(crashed, native_first).documents();
                validate(&journal, &export, crashed).unwrap();
            }
        }
    }

    #[test]
    fn missing_and_extra_structural_cells_are_rejected() {
        let fixture = fixture(true, false);
        let (journal, export) = fixture.documents();
        let mut parsed = super::export(&export).unwrap();
        let key = parsed.rows.0.keys().next().unwrap().clone();
        let value = parsed.rows.0.remove(&key).unwrap();
        assert!(validate(&journal, &export_text(&parsed.rows, parsed.through), true).is_err());
        parsed.rows.0.insert(key.clone(), value.clone());
        let mut extra = key;
        extra.push(0);
        parsed.rows.0.insert(extra, value);
        assert!(validate(&journal, &export_text(&parsed.rows, parsed.through), true).is_err());
    }

    #[test]
    fn identical_duplicate_export_keys_are_rejected() {
        let (journal, mut export) = fixture(true, false).documents();
        let duplicate = export
            .lines()
            .find(|line| line.starts_with("instructions "))
            .unwrap()
            .to_owned();
        writeln!(export, "{duplicate}").unwrap();
        assert!(validate(&journal, &export, true).is_err());
    }

    #[test]
    fn a_typed_native_receipt_with_wrong_baseline_link_is_rejected() {
        let (journal, export) = fixture(true, false).documents();
        let mut parsed = super::export(&export).unwrap();
        let mut values = normalized(&parsed.rows).unwrap();
        let starts: Vec<_> = super::journal(&journal)
            .unwrap()
            .0
            .iter()
            .filter_map(|(lsn, record)| {
                matches!(record, Record::InstructionObservationStarted { .. }).then_some(*lsn)
            })
            .collect();
        let cell = values
            .values_mut()
            .find(|value| value["kind"] == "native_receipt")
            .unwrap();
        assert!(cell["value"]["started"].is_u64());
        let replacement = starts
            .iter()
            .find(|lsn| serde_json::to_value(lsn.0).unwrap() != cell["value"]["started"])
            .unwrap();
        cell["value"]["started"] = json!(replacement.0);
        parsed.rows.0 = values
            .into_iter()
            .map(|(key, value)| (key, serde_json::to_vec(&value).unwrap()))
            .collect();
        InstructionCache::read_all(&parsed.rows, parsed.through).unwrap();
        assert!(validate(&journal, &export_text(&parsed.rows, parsed.through), true).is_err());
    }

    #[test]
    fn independent_history_rejects_swapped_native_receipt_baselines() {
        let fixture = fixture(true, false);
        let rows = derive_rows(&fixture.journal).unwrap();
        let mut values = normalized(&rows).unwrap();
        let keys: Vec<_> = values
            .iter()
            .filter_map(|(key, value)| (value["kind"] == "native_receipt").then_some(key.clone()))
            .collect();
        let [first, second] = keys.as_slice() else {
            panic!("two exact native receipts")
        };
        let first_start = values[first]["value"]["started"].clone();
        let second_start = values[second]["value"]["started"].clone();
        assert!(first_start.is_u64() && second_start.is_u64());
        assert_ne!(first_start, second_start);
        values.get_mut(first).unwrap()["value"]["started"] = second_start;
        values.get_mut(second).unwrap()["value"]["started"] = first_start;
        let rows = Rows(
            values
                .into_iter()
                .map(|(key, value)| (key, serde_json::to_vec(&value).unwrap()))
                .collect(),
        );
        let through = *fixture.journal.0.last_key_value().unwrap().0;
        let state = InstructionCache::read_all(&rows, through).unwrap();
        assert!(
            history(&fixture.journal, &state, true).is_err(),
            "independent provenance checks must reject a swapped native receipt index"
        );
    }

    #[test]
    fn unknown_attempt_cannot_claim_an_ending_sample() {
        let mut fixture = fixture(true, false);
        let record = fixture
            .journal
            .0
            .values_mut()
            .find(|record| {
                matches!(
                    record,
                    Record::InstructionObservationFinished {
                        settlement: InstructionSettlement::Unknown { .. },
                        ..
                    }
                )
            })
            .unwrap();
        let Record::InstructionObservationFinished {
            settlement: InstructionSettlement::Unknown { sampled_at, .. },
            ..
        } = record
        else {
            unreachable!()
        };
        *sampled_at = Some(Timestamp::from_unix_millis(40));
        let journal = fixture.journal_text();
        let rejected = derive_rows(&fixture.journal).and_then(|rows| {
            let through = *fixture.journal.0.last_key_value().unwrap().0;
            validate(&journal, &export_text(&rows, through), true)
        });
        assert!(rejected.is_err());
    }
}
