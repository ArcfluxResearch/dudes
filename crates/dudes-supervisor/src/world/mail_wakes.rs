use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::mail_wake::{MailWakeContext, MailWakeMail, MailWakeState};
use dudes_proto::{Entry, MailRef, MailTo, Record};
use dudes_types::{AgentId, Lsn, MailId, TurnId, UtcDay};

use super::World;

#[derive(Clone, Debug, Default)]
pub(super) struct Fold {
    mail: BTreeMap<MailId, (Option<AgentId>, MailWakeMail)>,
    pending: BTreeMap<AgentId, BTreeSet<MailId>>,
    states: BTreeMap<AgentId, MailWakeState>,
    by_day: BTreeMap<(AgentId, UtcDay), u8>,
    origins: BTreeSet<TurnId>,
    fenced: bool,
    accepted_lsn: Option<Lsn>,
}

impl Fold {
    pub(super) fn state(&self, agent: AgentId) -> Option<&MailWakeState> {
        self.states.get(&agent)
    }
    pub(super) fn count(&self, agent: AgentId, day: UtcDay) -> u8 {
        self.by_day.get(&(agent, day)).copied().unwrap_or_default()
    }
    pub(super) fn receipts(&self, agent: AgentId) -> Vec<MailWakeMail> {
        self.pending
            .get(&agent)
            .into_iter()
            .flatten()
            .map(|id| self.mail[id].1)
            .collect()
    }
    pub(super) fn valid(&self) -> bool {
        !self.fenced
    }
    pub(super) fn origin(&self, turn: TurnId) -> bool {
        self.origins.contains(&turn)
    }
    pub(super) fn accepted(&self, lsn: Lsn) -> bool {
        self.accepted_lsn == Some(lsn)
    }

    fn context(&self, world: &World, agent: AgentId, record: &Record) -> MailWakeContext {
        let owner = world.agents.get(&agent);
        let completion = world
            .completed_goal(agent)
            .filter(|(goal, _)| world.goals.get(goal).is_some_and(|row| row.agent == agent));
        let mails = if matches!(record, Record::MailWakeTurnDecided(decision) if matches!(decision.purpose, dudes_proto::MailWakePurpose::FreshPreparation { .. }))
        {
            self.receipts(agent)
        } else if let Record::MailState { id, .. } = record {
            self.mail
                .get(id)
                .filter(|(recipient, _)| *recipient == Some(agent))
                .map(|(_, mail)| *mail)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        MailWakeContext {
            completion,
            retired: owner.is_none_or(|row| row.retired),
            attached: owner.is_some_and(|row| row.attached),
            open_turn: owner.and_then(|row| row.open_turn),
            turn_exists: matches!(record, Record::MailWakeOnlyTurnIssued(issue) if world.turns.contains_key(&issue.turn)),
            receipt_exists: matches!(record, Record::Mail { id, .. } if self.mail.contains_key(id)),
            question_lsn: owner.and_then(|row| row.question_lsn),
            mails,
        }
    }

    pub(super) fn affected(&self, record: &Record) -> Vec<AgentId> {
        if let Some((agent, _)) = record.goal_install() {
            return vec![agent];
        }
        match record {
            Record::Mail {
                to: MailTo::Agent { agent },
                ..
            }
            | Record::GoalCleared { agent, .. }
            | Record::BreakerBeat { agent, .. } => vec![*agent],
            Record::AgentRetired { id, .. } => vec![*id],
            Record::GoalReassigned { from, to, .. } => {
                if from == to {
                    vec![*from]
                } else {
                    vec![*from, *to]
                }
            }
            Record::MailWakeTurnDecided(decision) => vec![decision.agent],
            Record::MailWakeOnlyTurnIssued(issue) => vec![issue.agent],
            Record::MailState { id, .. } => self
                .mail
                .get(id)
                .and_then(|(recipient, _)| *recipient)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn apply(&mut self, world: &World, lsn: Lsn, entry: &Entry) {
        self.accepted_lsn = None;
        if self.fenced {
            return;
        }
        for agent in self.affected(&entry.record) {
            let context = self.context(world, agent, &entry.record);
            let state = self.states.get(&agent).cloned().unwrap_or_default();
            let key = (agent, entry.at.utc_day());
            let count = self.by_day.get(&key).copied().unwrap_or_default();
            let applied = match state.apply(agent, &context, count, lsn, entry) {
                Ok(applied) => applied,
                Err(reason) => {
                    self.fenced = true;
                    tracing::error!(%lsn, %agent, %reason, "invalid canonical mail-wake prefix");
                    return;
                }
            };
            if lsn > state.last_lsn
                && matches!(
                    entry.record,
                    Record::MailWakeTurnDecided(_) | Record::MailWakeOnlyTurnIssued(_)
                )
            {
                self.accepted_lsn = Some(lsn);
            }
            self.states.insert(agent, applied.state);
            if applied.issued {
                let Record::MailWakeOnlyTurnIssued(issue) = &entry.record else {
                    unreachable!();
                };
                self.by_day.insert(key, count + 1);
                self.origins.insert(issue.turn);
            }
        }
        match &entry.record {
            Record::Mail { id, to, state, .. } => {
                let recipient = if let MailTo::Agent { agent } = to {
                    Some(*agent)
                } else {
                    None
                };
                self.mail.entry(*id).or_insert_with(|| {
                    (
                        recipient,
                        MailWakeMail::from_state(MailRef { id: *id, lsn }, state, lsn),
                    )
                });
                self.index(*id);
            }
            Record::MailState { id, state } => {
                if let Some((_, mail)) = self.mail.get_mut(id) {
                    *mail = MailWakeMail::from_state(mail.reference, state, lsn);
                }
                self.index(*id);
            }
            _ => {}
        }
    }

    fn index(&mut self, id: MailId) {
        let Some((Some(agent), mail)) = self.mail.get(&id) else {
            return;
        };
        if mail.count.is_some() {
            self.pending.entry(*agent).or_default().insert(id);
        } else if let Some(pending) = self.pending.get_mut(agent) {
            pending.remove(&id);
            if pending.is_empty() {
                self.pending.remove(agent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Liveness, ProbeAnswer, ProcessHost, SpawnResult};
    use crate::testing::{Harness, Seen, journal_with_issued_turn};
    use crate::{Command, Input};
    use dudes_proto::record::Hex32;
    use dudes_proto::{
        AutomaticHold, Evidence, ExitStatus, Gate, GateStep, InstructionBaselineRef,
        InstructionComparison, InstructionCoverageGap, InstructionGapKind,
        InstructionObservationOwner, InstructionPathContext, InstructionSampleUsage,
        InstructionSettlement, InstructionSnapshot, InstructionSnapshotSource,
        InstructionUnknownReason, LoopDecision, MailWakeAdmission, MailWakeDecision,
        MailWakeDigest, MailWakeDigestDomain, MailWakeIssue, MailWakePlan, MailWakePurpose,
        MailWakeTarget, MailWakeTask, TurnOutcome,
    };
    use dudes_providers::claude::ClaudeAdapter;
    use dudes_providers::claude::config::ClaudeConfig;
    use dudes_providers::stream::ScopedUsage;
    use dudes_types::{
        AgentStatus, MailState, Mode, ModelSelection, Provenance, SessionId, Timestamp, Tokens,
        Usage, Usd,
    };
    use std::ffi::OsStr;

    #[test]
    fn recipient_index_keeps_only_accepted_nonterminal_canonical_receipts() {
        let agent = AgentId::from_parts(1, 1);
        let other = AgentId::from_parts(1, 2);
        let id = MailId::from_parts(2, 1);
        let mut world = World::default();
        let mail = |to, state| {
            serde_json::from_value(serde_json::json!({
                "t": "mail", "id": id, "from": {"kind": "human"},
                "to": {"kind": "agent", "agent": to}, "act": "message",
                "subject": "index", "body_len": 0, "state": state
            }))
            .unwrap()
        };
        let entries = [
            mail(agent, MailState::Notified { n: 5 }),
            mail(other, MailState::Durable),
            Record::MailState {
                id: MailId::from_parts(2, 99),
                state: MailState::Durable,
            },
        ];
        for (index, record) in entries.into_iter().enumerate() {
            world.apply(
                Lsn(index as u64 + 1),
                &Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record,
                },
            );
        }
        assert_eq!(
            world.mail_wakes.receipts(agent),
            vec![MailWakeMail {
                reference: MailRef { id, lsn: Lsn(1) },
                count: Some(5),
                terminal_lsn: None,
            }]
        );
        assert!(world.mail_wakes.receipts(other).is_empty());
        world.apply(
            Lsn(4),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::MailState {
                    id,
                    state: MailState::Acked,
                },
            },
        );
        world.apply(
            Lsn(5),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: mail(other, MailState::Durable),
            },
        );
        assert!(world.mail_wakes.receipts(agent).is_empty());
        assert!(world.mail_wakes.receipts(other).is_empty());
        assert_eq!(world.mail_wakes.mail[&id].0, Some(agent));
        assert_eq!(world.mail_wakes.mail[&id].1.count, None);
        assert_eq!(world.mail_wakes.mail.len(), 1);
    }

    struct Prefix {
        entries: Vec<Entry>,
        agent: AgentId,
        goal: dudes_types::GoalId,
        completion: Lsn,
        turn: TurnId,
        mail: MailId,
    }

    impl Prefix {
        fn new(session: Option<SessionId>) -> Self {
            let mut prefix = Self::completed(session);
            prefix.wake(1, wake_at());
            prefix
        }

        fn completed(session: Option<SessionId>) -> Self {
            let (entries, agent, ordinary) = journal_with_issued_turn(session.clone(), "closed");
            let Record::GoalSet { goal, .. } = &entries[1].record else {
                unreachable!()
            };
            let mut prefix = Self {
                goal: goal.id,
                entries,
                agent,
                completion: Lsn::ZERO,
                turn: ordinary,
                mail: MailId::from_parts(2, 1),
            };
            if let Some(session) = session {
                prefix.append(
                    Record::SessionLearned {
                        agent,
                        turn: ordinary,
                        session,
                    },
                    Timestamp::UNIX_EPOCH,
                );
            }
            prefix.append(prefix.end(ordinary), Timestamp::UNIX_EPOCH);
            prefix.append(
                Record::PredicateResult {
                    goal: prefix.goal,
                    turn: Some(ordinary),
                    provenance: Provenance::Observed,
                    pass: true,
                    evidence: Evidence::Command {
                        argv: vec!["npm".into(), "test".into()],
                        program: Some("npm".into()),
                        exit: Some(0),
                        output_tail: String::new(),
                    },
                },
                Timestamp::UNIX_EPOCH,
            );
            prefix.completion = prefix.append(
                Record::LoopDecided {
                    agent,
                    input_digest: "ordinary-done".into(),
                    gate_trace: Vec::new(),
                    decision: LoopDecision::Done,
                },
                Timestamp::UNIX_EPOCH,
            );
            prefix
        }

        fn append(&mut self, record: Record, at: Timestamp) -> Lsn {
            self.entries.push(Entry { at, record });
            Lsn(u64::try_from(self.entries.len()).unwrap())
        }

        fn end(&self, turn: TurnId) -> Record {
            Record::TurnEnded {
                final_reply: None,
                agent: self.agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                elapsed_ms: Some(6_000),
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: Some(Usage::ZERO),
                session_usage: None,
                model_usage: BTreeMap::new(),
                last_message: None,
                session: None,
                outcome: Some(TurnOutcome::Completed),
            }
        }

        fn wake(&mut self, index: u64, at: Timestamp) {
            self.turn = TurnId::from_parts(2, u128::from(index));
            self.mail = MailId::from_parts(2, u128::from(index));
            let mail = self.append(serde_json::from_value(serde_json::json!({
                "t": "mail", "id": self.mail, "from": {"kind":"human"},
                    "to":{"kind":"agent","agent":self.agent}, "act":"message",
                    "subject":"private subject", "body":"private body", "body_len":12, "state":MailState::Durable
            })).unwrap(), Timestamp::UNIX_EPOCH);
            let task = MailWakeTask::new(1).unwrap();
            let receipt = MailRef {
                id: self.mail,
                lsn: mail,
            };
            let decision = |purpose, domain| {
                Record::MailWakeTurnDecided(MailWakeDecision {
                    agent: self.agent,
                    goal: self.goal,
                    completion_lsn: self.completion,
                    purpose,
                    input_digest: MailWakeDigest {
                        domain,
                        value: Hex32::new([1; 32]),
                    },
                    gate_trace: Gate::ORDER
                        .into_iter()
                        .map(|gate| GateStep {
                            gate,
                            fired: gate == Gate::Work,
                            note: None,
                        })
                        .collect(),
                    decision: MailWakeAdmission::Issue { task: task.clone() },
                })
            };
            let root = decision(
                MailWakePurpose::FreshPreparation {
                    plan: MailWakePlan {
                        trigger: receipt,
                        frontier: mail,
                        unread_count: 1,
                        task: task.clone(),
                        targets: vec![MailWakeTarget {
                            mail: receipt,
                            original_count: 0,
                            target_count: 1,
                        }],
                    },
                },
                MailWakeDigestDomain::Fresh,
            );
            let root_lsn = self.append(root, Timestamp::UNIX_EPOCH);
            self.append(
                Record::MailState {
                    id: self.mail,
                    state: MailState::Notified { n: 1 },
                },
                Timestamp::UNIX_EPOCH,
            );
            let lsn_of_decision = self.append(
                Record::MailWakeTurnDecided(MailWakeDecision {
                    agent: self.agent,
                    goal: self.goal,
                    completion_lsn: self.completion,
                    purpose: MailWakePurpose::ExistingPreparation { root_lsn },
                    input_digest: MailWakeDigest {
                        domain: MailWakeDigestDomain::Resume,
                        value: Hex32::new([2; 32]),
                    },
                    gate_trace: Gate::ORDER
                        .into_iter()
                        .map(|gate| GateStep {
                            gate,
                            fired: gate == Gate::Work,
                            note: None,
                        })
                        .collect(),
                    decision: MailWakeAdmission::Issue { task: task.clone() },
                }),
                at,
            );
            let session = self.entries.iter().find_map(|entry| match &entry.record {
                Record::SessionLearned { session, .. } => Some(session.clone()),
                _ => None,
            });
            self.append(
                Record::MailWakeOnlyTurnIssued(MailWakeIssue {
                    agent: self.agent,
                    goal: self.goal,
                    turn: self.turn,
                    completion_lsn: self.completion,
                    root_lsn,
                    lsn_of_decision,
                    task,
                    session,
                    mode: Mode::Managed,
                    model_selection: ModelSelection::named("claude-haiku-4-5"),
                    answers: Vec::new(),
                }),
                at,
            );
        }

        fn harness(&self) -> Harness {
            replay(&self.entries)
        }
    }

    fn replay(entries: &[Entry]) -> Harness {
        replay_into(Harness::new(), entries)
    }

    fn replay_into(mut h: Harness, entries: &[Entry]) -> Harness {
        for (position, entry) in entries.iter().enumerate() {
            assert!(
                h.step(Input::Replayed {
                    lsn: Lsn(u64::try_from(position).unwrap() + 1),
                    entry: entry.clone()
                })
                .is_empty()
            );
        }
        h.journal = entries.to_vec();
        h
    }

    fn acknowledge(h: &mut Harness, seen: Seen) -> Vec<Seen> {
        let Seen::Persist { token, entry } = seen else {
            panic!("expected persistence")
        };
        h.journal.push(entry);
        h.step(Input::Persisted {
            token,
            lsn: Lsn(u64::try_from(h.journal.len()).unwrap()),
        })
    }

    fn sample(h: &mut Harness, seen: Seen) -> Vec<Seen> {
        let Seen::SampleInstructions {
            request,
            owner,
            goal,
            baseline,
        } = seen
        else {
            panic!("expected an explicit instruction sample")
        };
        let InstructionObservationOwner::Managed { agent, turn, .. } = owner else {
            panic!("mail wakes have a managed instruction owner")
        };
        assert!(
            goal.and_then(|goal| h
                .supervisor
                .world()
                .instructions
                .association_for_goal(goal, dudes_types::Seq::ZERO))
                .is_none()
        );
        if let Some(baseline) = &baseline {
            let (_, attempt) = h
                .supervisor
                .world()
                .instructions
                .current_for_turn(turn)
                .unwrap();
            assert_eq!(attempt.owner, owner);
            assert_eq!(attempt.snapshot.as_ref(), Some(baseline.as_ref()));
            assert!(
                h.supervisor
                    .world()
                    .instructions
                    .managed_end_lsn(agent, turn)
                    .is_some()
            );
        }
        let gap = InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::DeclarationMissing,
            context: InstructionPathContext::Unresolved,
            agent: Some(agent),
            path: None,
        };
        let snapshot = InstructionSnapshot {
            sampled_at: wake_at(),
            source: InstructionSnapshotSource::Fresh,
            execution_folder: None,
            participants: Vec::new(),
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: vec![
                InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::PathUnresolved,
                    ..gap.clone()
                },
                gap.clone(),
            ],
            usage: InstructionSampleUsage::default(),
            boot: None,
        };
        assert!(snapshot.validate().is_ok());
        h.step(Input::InstructionSampled {
            request,
            snapshot: Box::new(snapshot),
            ticket: None,
            comparison: baseline.map(|_| {
                Box::new(InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![gap],
                    usage: InstructionSampleUsage::default(),
                })
            }),
        })
    }

    fn assert_observation(h: &Harness, record: &Record) {
        match record {
            Record::InstructionObservationStarted {
                owner,
                snapshot,
                ticket,
            } => {
                assert_eq!(*ticket, None);
                assert!(!snapshot.gaps.is_empty());
                assert!(snapshot.validate().is_ok());
                let InstructionObservationOwner::Managed { agent, turn, issue } = owner else {
                    panic!("expected a managed mail observation")
                };
                assert!(h.supervisor.world().agents.contains_key(agent));
                match issue {
                    dudes_proto::InstructionIssueAssociation::Prospective => {
                        assert_eq!(h.supervisor.world().instructions.issue_lsn(*turn), None);
                    }
                    dudes_proto::InstructionIssueAssociation::Issued { issue_lsn } => {
                        assert_eq!(
                            h.supervisor.world().instructions.issue_lsn(*turn),
                            Some(*issue_lsn)
                        );
                    }
                }
            }
            Record::InstructionObservationFinished {
                owner,
                baseline,
                ticket,
                settlement,
            } => {
                assert_eq!(*ticket, None);
                let InstructionObservationOwner::Managed { agent, turn, .. } = owner else {
                    panic!("expected a managed mail observation")
                };
                match baseline {
                    InstructionBaselineRef::Started { lsn } => {
                        assert_eq!(
                            h.supervisor
                                .world()
                                .instructions
                                .attempt(*lsn)
                                .unwrap()
                                .owner,
                            *owner
                        );
                    }
                    InstructionBaselineRef::MissingIssueBoundary {
                        turn: issued,
                        issue_lsn,
                    } => {
                        assert_eq!(issued, turn);
                        assert_eq!(
                            h.supervisor.world().instructions.issue_lsn(*turn),
                            Some(*issue_lsn)
                        );
                    }
                    InstructionBaselineRef::MissingTurnBoundary { .. } => {
                        panic!("unexpected mail-wake baseline")
                    }
                }
                match settlement {
                    InstructionSettlement::Unknown {
                        reason,
                        recovery,
                        comparison,
                        ..
                    } => {
                        assert_eq!(*reason, InstructionUnknownReason::UnrecordedLaunchBoundary);
                        assert!(*recovery);
                        assert!(
                            comparison
                                .gaps
                                .iter()
                                .any(|gap| gap.reason
                                    == InstructionGapKind::UnrecordedLaunchBoundary)
                        );
                    }
                    InstructionSettlement::Compared {
                        end, comparison, ..
                    } => {
                        assert!(
                            matches!(end, dudes_proto::InstructionEndBoundary::ManagedTurn {
                            turn: ended, end_lsn,
                        } if ended == turn && h.supervisor.world().instructions.managed_end_lsn(*agent, *turn) == Some(*end_lsn))
                        );
                        assert!(!comparison.gaps.is_empty());
                    }
                    _ => panic!("these mail fixtures never manufacture no-process proof"),
                }
            }
            _ => panic!("expected a named instruction persistence"),
        }
    }

    fn instruction_work(h: &mut Harness, mut effects: Vec<Seen>) -> Vec<Seen> {
        for _ in 0..16 {
            let [effect] = effects.as_slice() else {
                return effects;
            };
            match effect {
                Seen::SampleInstructions { .. } => {
                    effects = sample(h, one(effects));
                }
                Seen::Persist { entry, .. } => {
                    if !matches!(
                        entry.record,
                        Record::InstructionObservationStarted { .. }
                            | Record::InstructionObservationFinished { .. }
                    ) {
                        return effects;
                    }
                    assert_observation(h, &entry.record);
                    effects = acknowledge(h, one(effects));
                }
                _ => return effects,
            }
        }
        panic!("bounded explicit instruction acknowledgements did not finish")
    }

    fn acknowledge_observations(h: &mut Harness, seen: Seen) -> Vec<Seen> {
        let effects = acknowledge(h, seen);
        instruction_work(h, effects)
    }

    fn wake_at() -> Timestamp {
        Timestamp::from_unix_millis(86_400_001)
    }

    fn mail_record(prefix: &Prefix, state: &MailState) -> Record {
        serde_json::from_value(serde_json::json!({
            "t":"mail", "id":prefix.mail, "from":{"kind":"human"},
            "to":{"kind":"agent","agent":prefix.agent}, "act":"message",
            "subject":"private subject", "body":"private body", "body_len":12, "state":state
        }))
        .unwrap()
    }

    fn mail_journaled(h: &mut Harness, record: Record) -> Vec<Seen> {
        let entry = Entry {
            at: wake_at(),
            record,
        };
        h.journal.push(entry.clone());
        h.step(Input::Journaled {
            lsn: Lsn(u64::try_from(h.journal.len()).unwrap()),
            entry,
        })
    }

    fn refresh(h: &mut Harness, seen: &Seen) -> Vec<Seen> {
        let Seen::MailWakeRefresh { agent, request, .. } = seen else {
            panic!("expected the exact current mailbox refresh")
        };
        h.step(Input::MailWakeReady {
            agent: *agent,
            request: *request,
            result: Ok((wake_at(), crate::input::SpendView::default(), Lsn::ZERO)),
        })
    }

    fn live_wake(prefix: &mut Prefix, h: &mut Harness, index: u64) -> Seen {
        let calls = h.host.spawned.len();
        prefix.mail = MailId::from_parts(2, u128::from(index));
        let arrival = one(mail_journaled(h, mail_record(prefix, &MailState::Durable)));
        let root = one(refresh(h, &arrival));
        assert!(
            matches!(root.record(), Some(Record::MailWakeTurnDecided(value))
            if matches!(value.purpose, MailWakePurpose::FreshPreparation { .. }))
        );
        let ready = one(acknowledge(h, root));
        let notification = one(refresh(h, &ready));
        let Seen::MailWakeNotify { target, .. } = notification else {
            panic!("expected the canonical mailbox target")
        };
        assert_eq!(target.mail.id, prefix.mail);
        assert_eq!((target.original_count, target.target_count), (0, 1));
        let notified = one(mail_journaled(
            h,
            Record::MailState {
                id: prefix.mail,
                state: MailState::Notified { n: 1 },
            },
        ));
        let decision = one(refresh(h, &notified));
        assert!(
            matches!(decision.record(), Some(Record::MailWakeTurnDecided(value))
            if matches!(value.purpose, MailWakePurpose::ExistingPreparation { .. }))
        );
        let checking = one(acknowledge(h, decision));
        let sampled = refresh(h, &checking);
        assert!(matches!(
            sampled.as_slice(),
            [Seen::SampleInstructions { baseline: None, .. }]
        ));
        let issue = one(instruction_work(h, sampled));
        let Some(Record::MailWakeOnlyTurnIssued(carrier)) = issue.record() else {
            panic!("expected the actual post-baseline mail issue")
        };
        assert_eq!(carrier.goal, prefix.goal);
        assert_eq!(carrier.completion_lsn, prefix.completion);
        assert_eq!(carrier.task, MailWakeTask::new(1).unwrap());
        prefix.turn = carrier.turn;
        let armed = one(acknowledge(h, issue));
        assert!(
            matches!(armed, Seen::Spawn { .. })
                || matches!(armed.record(), Some(Record::HookTokenMinted { .. }))
        );
        assert_eq!(h.host.spawned.len(), calls);
        prefix.entries = h.journal.clone();
        armed
    }

    fn live(prefix: &mut Prefix) -> (Harness, Seen) {
        use crate::testing::{FakeClock, FakeHost};
        let harness = Harness::with_adapter(
            FakeClock::frozen(86_400_001),
            FakeHost::wall_clock_anchored(),
            || {
                Box::new(ClaudeAdapter::new(ClaudeConfig {
                    model: Some("claude-haiku-4-5".into()),
                    ..ClaudeConfig::default()
                }))
            },
        );
        let mut h = replay_into(harness, &prefix.entries);
        assert!(h.step(Input::Started).is_empty());
        let mint = live_wake(prefix, &mut h, 1);
        (h, mint)
    }

    fn canonical_control(h: &mut Harness, record: Record) {
        let entry = Entry {
            at: wake_at(),
            record,
        };
        h.journal.push(entry.clone());
        assert!(
            h.step(Input::Replayed {
                lsn: Lsn(u64::try_from(h.journal.len()).unwrap()),
                entry,
            })
            .is_empty()
        );
    }

    fn ready_spawn(h: &mut Harness, armed: Seen) -> Seen {
        if matches!(armed.record(), Some(Record::HookTokenMinted { .. })) {
            one(acknowledge_observations(h, armed))
        } else {
            assert!(matches!(armed, Seen::Spawn { .. }));
            armed
        }
    }

    fn assert_unknown(h: &Harness, prefix: &Prefix, baseline: InstructionBaselineRef) {
        let boundary = match baseline {
            InstructionBaselineRef::Started { lsn } => lsn,
            InstructionBaselineRef::MissingIssueBoundary { issue_lsn, .. } => issue_lsn,
            InstructionBaselineRef::MissingTurnBoundary { .. } => {
                panic!("expected the exact unconfirmed issue interval")
            }
        };
        let attempt = h.supervisor.world().instructions.attempt(boundary).unwrap();
        assert_eq!(attempt.baseline, baseline);
        assert!(matches!(attempt.owner,
            InstructionObservationOwner::Managed { agent, turn, .. }
                if agent == prefix.agent && turn == prefix.turn));
        assert!(matches!(
            attempt.finished.as_ref().map(|finish| &finish.settlement),
            Some(InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                recovery: true,
                ..
            })
        ));
        assert!(attempt.boundary_settled.is_none());
        assert!(
            h.supervisor
                .world()
                .instructions
                .unresolved_launch_attempts(prefix.agent)
                .any(|(_, unresolved)| unresolved.baseline == baseline)
        );
        assert!(!h.journal.iter().any(|entry| matches!(
            entry.record,
            Record::InstructionObservationFinished {
                settlement: InstructionSettlement::NoProcess { .. },
                ..
            }
        )));
        assert!(h.host.spawned.is_empty());
        assert_eq!(h.open_turn(prefix.agent), Some(prefix.turn));
        assert_eq!(
            h.supervisor.world().turns[&prefix.turn].task,
            MailWakeTask::new(1).unwrap().as_str()
        );
        assert_eq!(
            h.supervisor.world().turns[&prefix.turn].model_selection,
            Some(ModelSelection::named("claude-haiku-4-5"))
        );
        assert_eq!(
            h.journal
                .iter()
                .filter(|entry| matches!(&entry.record,
            Record::MailWakeOnlyTurnIssued(issue) if issue.turn == prefix.turn))
                .count(),
            1
        );
        assert_completed(h, prefix);
    }

    fn one(mut effects: Vec<Seen>) -> Seen {
        assert_eq!(effects.len(), 1, "{effects:?}");
        effects.pop().unwrap()
    }

    fn board_changed(h: &mut Harness, card: dudes_types::CardId) -> Vec<Seen> {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::CardTransition {
                card,
                from: dudes_types::CardStatus::Doing,
                to: dudes_types::CardStatus::Done,
                by: dudes_proto::Actor::System,
                expected_seq: dudes_types::Seq(1),
                reason: None,
            },
        };
        h.journal.push(entry.clone());
        h.step(Input::BoardChanged {
            lsn: Lsn(u64::try_from(h.journal.len()).unwrap()),
            entry,
        })
    }

    fn start(h: &mut Harness) -> Seen {
        let effects = h.step(Input::Started);
        let mint = one(instruction_work(h, effects));
        assert!(matches!(
            mint.record(),
            Some(Record::HookTokenMinted { .. })
        ));
        one(acknowledge_observations(h, mint))
    }

    fn launch(h: &mut Harness, spawn: Seen) {
        let Seen::Spawn {
            turn,
            spec,
            install,
            ..
        } = spawn
        else {
            panic!("expected launch")
        };
        assert!(h.supervisor.claim_browser_dispatch(turn, None));
        let SpawnResult::Spawned(key) = h.host.spawn(&spec, &install) else {
            panic!("fake launch failed")
        };
        let spawned = one(h.step(Input::Spawned {
            turn,
            key,
            mcp_grant: None,
        }));
        assert!(matches!(
            spawned.record(),
            Some(Record::TurnSpawned {
                reissued: false,
                ..
            })
        ));
        assert!(acknowledge(h, spawned).is_empty());
    }

    fn exit(turn: TurnId, code: i32, usage: Usage) -> Input {
        Input::ChildExited {
            final_reply: None,
            turn,
            elapsed_ms: Some(1_000),
            exit: ExitStatus {
                code: Some(code),
                signal: None,
            },
            usage: Some(ScopedUsage::Turn(usage)),
            model_usage: BTreeMap::new(),
            outcome: if code == 0 {
                TurnOutcome::Completed
            } else {
                TurnOutcome::Failed {
                    reason: "owned failure".into(),
                }
            },
            last_message: None,
        }
    }

    fn assert_completed(h: &Harness, prefix: &Prefix) {
        assert_eq!(
            h.supervisor.world().completed_goal(prefix.agent),
            Some((prefix.goal, prefix.completion))
        );
        assert!(
            h.supervisor.world().goals[&prefix.goal]
                .predicate
                .as_ref()
                .unwrap()
                .pass
        );
        assert_eq!(h.journal.iter().filter(|entry| matches!(entry.record, Record::LoopDecided { agent, decision: LoopDecision::Done, .. } if agent == prefix.agent)).count(), 1);
        assert_eq!(
            h.supervisor
                .world()
                .mail_wakes
                .by_day
                .values()
                .copied()
                .sum::<u8>(),
            1
        );
        assert_eq!(h.supervisor.world().goals[&prefix.goal].turns_on_goal, 2);
    }

    #[test]
    fn mail_wake_issue_installs_one_original_turn_and_saturates_its_hop() {
        let prefix = Prefix::new(Some(SessionId::new("recorded-session")));
        let issue_lsn = Lsn(u64::try_from(prefix.entries.len()).unwrap());
        let mut world = World::default();
        for (position, entry) in prefix.entries[..prefix.entries.len() - 1]
            .iter()
            .enumerate()
        {
            world.apply(Lsn(u64::try_from(position).unwrap() + 1), entry);
        }
        assert_eq!(world.goals[&prefix.goal].turns_on_goal, 1);
        assert!(world.mail_wakes.by_day.is_empty());
        world.goals.get_mut(&prefix.goal).unwrap().turns_on_goal = u32::MAX;
        let entry = prefix.entries.last().unwrap();
        world.apply(issue_lsn, entry);
        let row = &world.turns[&prefix.turn];
        assert_eq!(row.session, Some(SessionId::new("recorded-session")));
        assert_eq!(
            row.model_selection,
            Some(ModelSelection::named("claude-haiku-4-5"))
        );
        assert_eq!(row.task, MailWakeTask::new(1).unwrap().as_str());
        assert!(row.answers.is_empty());
        assert_eq!(world.agents[&prefix.agent].open_turn, Some(prefix.turn));
        assert_eq!(world.goals[&prefix.goal].turns_on_goal, u32::MAX);
        assert_eq!(
            world.completed_goal(prefix.agent),
            Some((prefix.goal, prefix.completion))
        );
        assert_eq!(
            world.mail_wakes.by_day[&(prefix.agent, entry.at.utc_day())],
            1
        );
        assert_eq!(
            prefix.harness().supervisor.status_of(prefix.agent),
            AgentStatus::Running
        );
        world.apply(issue_lsn, entry);
        assert_eq!(
            world.mail_wakes.by_day[&(prefix.agent, entry.at.utc_day())],
            1
        );
        assert_eq!(world.goals[&prefix.goal].turns_on_goal, u32::MAX);
    }

    #[test]
    fn mail_wake_replay_preserves_task_model_and_session_selection_without_host_authority() {
        for session in [None, Some(SessionId::new("recorded-session"))] {
            let prefix = Prefix::new(session.clone());
            let mut h = prefix.harness();
            assert_eq!(h.supervisor.world().turns[&prefix.turn].session, session);
            let spawn = start(&mut h);
            let Seen::Spawn {
                turn,
                spec,
                install,
                ..
            } = &spawn
            else {
                panic!("expected launch")
            };
            assert_eq!(*turn, prefix.turn);
            assert_eq!(
                spec.argv.last().unwrap(),
                OsStr::new(MailWakeTask::new(1).unwrap().as_str())
            );
            assert!(install.files.iter().any(|(_, contents)| {
                serde_json::from_str::<serde_json::Value>(contents)
                    .is_ok_and(|value| value["model"] == "claude-haiku-4-5")
            }));
            assert!(spec.argv.iter().any(|part| part == "--session-id"));
            assert!(!spec.argv.iter().any(|part| part == "--resume"));
            assert!(h.supervisor.world().browser_issue(prefix.turn).is_none());
            let baseline = InstructionBaselineRef::MissingIssueBoundary {
                turn: prefix.turn,
                issue_lsn: h
                    .supervisor
                    .world()
                    .instructions
                    .issue_lsn(prefix.turn)
                    .unwrap(),
            };
            assert_eq!(h.spawns_of(prefix.turn).len(), 1);
            assert_unknown(&h, &prefix, baseline);
            let mut restarted = replay(&h.journal);
            let candidate = start(&mut restarted);
            assert!(matches!(candidate, Seen::Spawn { turn, .. } if turn == prefix.turn));
            assert_eq!(
                restarted.supervisor.world().turns[&prefix.turn].task,
                MailWakeTask::new(1).unwrap().as_str()
            );
            assert_unknown(&restarted, &prefix, baseline);
        }
    }

    #[test]
    fn mail_wake_halt_and_automatic_hold_release_the_same_owed_issue_once() {
        for control in [
            Record::Halt {
                agent: Prefix::new(None).agent,
                text: None,
            },
            Record::AgentHeld {
                agent: Prefix::new(None).agent,
                provenance: Provenance::Observed,
                cause: AutomaticHold::LaunchFailed {
                    reason: "owned failure".into(),
                },
            },
        ] {
            let mut prefix = Prefix::completed(None);
            let (mut h, mint) = live(&mut prefix);
            canonical_control(&mut h, control);
            assert!(h.host.spawned.is_empty());
            assert_eq!(h.open_turn(prefix.agent), Some(prefix.turn));
            let resumed = one(h.step(Input::Command(Command::Resume {
                agent: prefix.agent,
                browser: None,
            })));
            assert!(acknowledge_observations(&mut h, resumed).is_empty());
            let repeated_resume = one(h.step(Input::Command(Command::Resume {
                agent: prefix.agent,
                browser: None,
            })));
            assert!(acknowledge(&mut h, repeated_resume).is_empty());
            let spawn = ready_spawn(&mut h, mint);
            launch(&mut h, spawn);
            assert_eq!(h.spawns_of(prefix.turn).len(), 1);
            assert_completed(&h, &prefix);
        }
        let mut prefix = Prefix::completed(None);
        let (mut paused, mint) = live(&mut prefix);
        canonical_control(
            &mut paused,
            Record::Pause {
                agent: prefix.agent,
                browser: None,
            },
        );
        let spawn = ready_spawn(&mut paused, mint);
        launch(&mut paused, spawn);
        assert_eq!(paused.host.spawned.len(), 1);
        assert!(paused.supervisor.world().agents[&prefix.agent].paused);
    }

    #[test]
    fn mail_wake_resume_cannot_erase_a_replayed_unconfirmed_issue_boundary() {
        for held in [false, true] {
            let mut prefix = Prefix::new(None);
            prefix.append(
                if held {
                    Record::AgentHeld {
                        agent: prefix.agent,
                        provenance: Provenance::Observed,
                        cause: AutomaticHold::LaunchFailed {
                            reason: "owned failure".into(),
                        },
                    }
                } else {
                    Record::Halt {
                        agent: prefix.agent,
                        text: None,
                    }
                },
                wake_at(),
            );
            let mut h = prefix.harness();
            let baseline = InstructionBaselineRef::MissingIssueBoundary {
                turn: prefix.turn,
                issue_lsn: h
                    .supervisor
                    .world()
                    .instructions
                    .issue_lsn(prefix.turn)
                    .unwrap(),
            };
            let started = h.step(Input::Started);
            assert!(instruction_work(&mut h, started).is_empty());
            assert_unknown(&h, &prefix, baseline);
            let resumed = one(h.step(Input::Command(Command::Resume {
                agent: prefix.agent,
                browser: None,
            })));
            let mint = one(acknowledge_observations(&mut h, resumed));
            assert!(matches!(
                mint.record(),
                Some(Record::HookTokenMinted { .. })
            ));
            let repeated = one(h.step(Input::Command(Command::Resume {
                agent: prefix.agent,
                browser: None,
            })));
            assert!(acknowledge(&mut h, repeated).is_empty());
            let candidate = ready_spawn(&mut h, mint);
            assert!(matches!(candidate, Seen::Spawn { turn, .. } if turn == prefix.turn));
            assert_eq!(h.spawns_of(prefix.turn).len(), 1);
            assert_unknown(&h, &prefix, baseline);
        }
    }

    #[test]
    fn mail_wake_end_preserves_completion_and_usage_without_judging_again() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        let spawn = ready_spawn(&mut h, mint);
        launch(&mut h, spawn);
        let usage = Usage {
            input: Tokens::new(17),
            usd: Some(Usd::from_micros(41)),
            ..Usage::ZERO
        };
        let ended = one(h.step(exit(prefix.turn, 0, usage)));
        assert!(
            matches!(ended.record(), Some(Record::TurnEnded { turn, provenance: Provenance::Observed, usage: Some(observed), .. }) if *turn == prefix.turn && *observed == usage)
        );
        assert!(acknowledge_observations(&mut h, ended).is_empty());
        assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Done);
        assert_eq!(h.open_turn(prefix.agent), None);
        assert_completed(&h, &prefix);
        assert!(
            h.trace
                .iter()
                .all(|seen| !matches!(seen, Seen::RunPredicate { goal } if *goal == prefix.goal))
        );
        assert!(h.step(exit(prefix.turn, 0, Usage::ZERO)).is_empty());
        let mut restart = replay(&h.journal);
        assert!(restart.step(Input::Started).is_empty());
        assert_completed(&restart, &prefix);

        let mut ordinary = Harness::new();
        let (agent, goal) = ordinary.hire_with_goal(&["/usr/bin/true"]);
        let turn = ordinary.open_turn(agent).unwrap();
        let ended = one(ordinary.step(exit(turn, 0, Usage::ZERO)));
        assert!(
            matches!(one(acknowledge_observations(&mut ordinary, ended)), Seen::RunPredicate { goal: judged } if judged == goal)
        );
    }

    #[test]
    fn mail_wake_end_before_spawn_ack_and_late_observations_cannot_reopen_it() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        let spawn = ready_spawn(&mut h, mint);
        let Seen::Spawn { spec, install, .. } = spawn else {
            panic!("expected launch")
        };
        let SpawnResult::Spawned(key) = h.host.spawn(&spec, &install) else {
            unreachable!()
        };
        let ended = one(h.step(exit(prefix.turn, 0, Usage::ZERO)));
        assert!(
            h.step(Input::Spawned {
                turn: prefix.turn,
                key: key.clone(),
                mcp_grant: None
            })
            .is_empty()
        );
        assert!(
            h.step(Input::Liveness {
                turn: prefix.turn,
                liveness: Liveness::Alive
            })
            .is_empty()
        );
        assert!(acknowledge_observations(&mut h, ended).is_empty());
        assert!(
            h.step(Input::Spawned {
                turn: prefix.turn,
                key,
                mcp_grant: None
            })
            .is_empty()
        );
        assert!(
            h.step(Input::Liveness {
                turn: prefix.turn,
                liveness: Liveness::Gone
            })
            .is_empty()
        );
        assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Done);
        assert_eq!(h.records().filter(|record| matches!(record, Record::TurnEnded { turn, .. } if *turn == prefix.turn)).count(), 1);
        assert_completed(&h, &prefix);
    }

    #[test]
    fn mail_wake_reaper_retains_unknown_adopts_alive_and_ends_gone() {
        for liveness in [Liveness::Unknown, Liveness::Alive, Liveness::Gone] {
            let mut prefix = Prefix::new(None);
            prefix.append(
                Record::TurnSpawned {
                    agent: prefix.agent,
                    turn: prefix.turn,
                    pid: 1000,
                    start_time: 900,
                    boot: None,
                    session: None,
                    reissued: true,

                    mcp_grant: None,
                },
                Timestamp::UNIX_EPOCH,
            );
            let mut h = prefix.harness();
            let effects = h.step(Input::Started);
            assert!(
                matches!(one(instruction_work(&mut h, effects)), Seen::Probe { turn } if turn == prefix.turn)
            );
            assert_unknown(
                &h,
                &prefix,
                InstructionBaselineRef::MissingIssueBoundary {
                    turn: prefix.turn,
                    issue_lsn: h
                        .supervisor
                        .world()
                        .instructions
                        .issue_lsn(prefix.turn)
                        .unwrap(),
                },
            );
            let answer = match liveness {
                Liveness::Unknown => ProbeAnswer::CouldNotAnswer,
                Liveness::Alive => ProbeAnswer::StartValue(900),
                Liveness::Gone => ProbeAnswer::NoSuchPid,
            };
            h.host.answers.insert(1000, answer);
            let observed = h.host.alive(&h.key_of(prefix.turn).unwrap());
            assert_eq!(observed, liveness);
            let result = h.step(Input::Liveness {
                turn: prefix.turn,
                liveness: observed,
            });
            if liveness == Liveness::Gone {
                let ended = one(result);
                assert!(matches!(
                    ended.record(),
                    Some(Record::TurnEnded {
                        provenance: Provenance::Observed,
                        outcome: Some(TurnOutcome::Vanished),
                        ..
                    })
                ));
                assert!(acknowledge_observations(&mut h, ended).is_empty());
                assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Done);
            } else {
                assert!(result.is_empty());
                assert_eq!(h.open_turn(prefix.agent), Some(prefix.turn));
                assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Running);
                assert_eq!(
                    h.supervisor.retirement_has_unknown_liveness(prefix.agent),
                    liveness == Liveness::Unknown
                );
            }
            assert_completed(&h, &prefix);
        }
    }

    #[test]
    fn mail_wake_confirmed_spawn_failure_keeps_charge_and_observed_hold() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        let spawn = ready_spawn(&mut h, mint);
        let Seen::Spawn {
            turn,
            spec,
            install,
            ..
        } = spawn
        else {
            panic!("expected launch")
        };
        assert_eq!(turn, prefix.turn);
        h.host.fail_spawn = Some(std::io::ErrorKind::PermissionDenied);
        let SpawnResult::NotLaunched { reason } = h.host.spawn(&spec, &install) else {
            panic!("expected confirmed failure")
        };
        let ended = one(h.step(Input::SpawnFailed {
            turn: prefix.turn,
            reason: reason.to_string(),
        }));
        let held = one(acknowledge_observations(&mut h, ended));
        assert!(matches!(
            held.record(),
            Some(Record::AgentHeld {
                provenance: Provenance::Observed,
                cause: AutomaticHold::LaunchFailed { .. },
                ..
            })
        ));
        assert!(acknowledge(&mut h, held).is_empty());
        assert!(matches!(
            h.supervisor.world().agents[&prefix.agent].automatic_hold,
            Some(AutomaticHold::LaunchFailed { .. })
        ));
        assert_completed(&h, &prefix);
        let mut restart = replay(&h.journal);
        assert!(restart.step(Input::Started).is_empty());
        assert_completed(&restart, &prefix);
    }

    #[test]
    fn mail_wake_mint_and_spawn_crash_cuts_keep_the_same_issue_and_launch_uncertainty() {
        for mint_durable in [false, true] {
            let mut prefix = Prefix::completed(None);
            let (mut current, mint) = live(&mut prefix);
            let baseline = current
                .supervisor
                .world()
                .instructions
                .current_for_turn(prefix.turn)
                .unwrap()
                .1
                .baseline;
            assert_eq!(current.spawns_of(prefix.turn).len(), 0);
            assert_eq!(current.open_turn(prefix.agent), Some(prefix.turn));
            if mint_durable {
                let candidate = ready_spawn(&mut current, mint);
                assert!(matches!(candidate, Seen::Spawn { turn, .. } if turn == prefix.turn));
            }
            assert!(current.host.spawned.is_empty());
            let mut restarted = replay(&current.journal);
            let candidate = start(&mut restarted);
            assert!(matches!(candidate, Seen::Spawn { turn, .. } if turn == prefix.turn));
            assert_eq!(restarted.spawns_of(prefix.turn).len(), 1);
            assert_eq!(
                restarted
                    .supervisor
                    .world()
                    .issued_not_spawned()
                    .map(|(turn, _)| turn)
                    .collect::<Vec<_>>(),
                vec![prefix.turn]
            );
            assert_unknown(&restarted, &prefix, baseline);
            let mut before_spawn_ack = replay(&restarted.journal);
            let candidate = start(&mut before_spawn_ack);
            assert!(matches!(candidate, Seen::Spawn { turn, .. } if turn == prefix.turn));
            assert_eq!(before_spawn_ack.spawns_of(prefix.turn).len(), 1);
            assert_unknown(&before_spawn_ack, &prefix, baseline);
        }
    }

    #[test]
    fn mail_wake_browser_cancellation_and_intent_cannot_change_its_origin() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        let browser = dudes_proto::BrowserNoteAttribution {
            request_id_digest: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            session_digest: Hex32::new([3; 32]),
        };
        canonical_control(
            &mut h,
            Record::BrowserNoteIssuedCancelled {
                agent: prefix.agent,
                note_lsn: Lsn(1),
                turn: prefix.turn,
                browser,
            },
        );
        canonical_control(
            &mut h,
            Record::BrowserNoteDispatchIntent {
                agent: prefix.agent,
                note_lsn: Lsn(1),
                turn: prefix.turn,
            },
        );
        assert!(h.supervisor.world().mail_wake_origin(prefix.turn));
        assert!(h.supervisor.world().browser_issue(prefix.turn).is_none());
        assert!(!h.supervisor.world().browser_issue_cancelled(prefix.turn));
        let spawn = ready_spawn(&mut h, mint);
        launch(&mut h, spawn);
        let ended = one(h.step(exit(prefix.turn, 0, Usage::ZERO)));
        assert!(acknowledge_observations(&mut h, ended).is_empty());
        assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Done);
        assert_completed(&h, &prefix);
    }

    #[test]
    fn mail_wake_day_accounting_keeps_the_full_older_bucket_after_clock_reversal() {
        let mut prefix = Prefix::new(None);
        let day = prefix.entries.last().unwrap().at;
        for index in 2..=21 {
            prefix.append(prefix.end(prefix.turn), day);
            prefix.append(
                Record::MailState {
                    id: prefix.mail,
                    state: MailState::Acked,
                },
                day,
            );
            let at = if index == 21 {
                Timestamp::from_unix_millis(172_800_001)
            } else {
                day
            };
            prefix.wake(index, at);
        }
        let mut h = prefix.harness();
        assert!(h.supervisor.world().mail_wake_history_valid());
        assert_eq!(
            h.supervisor.world().mail_wakes.by_day[&(prefix.agent, day.utc_day())],
            20
        );
        assert_eq!(
            h.supervisor.world().mail_wakes.by_day[&(
                prefix.agent,
                Timestamp::from_unix_millis(172_800_001).utc_day()
            )],
            1
        );
        assert_eq!(h.supervisor.world().goals[&prefix.goal].turns_on_goal, 22);
        prefix.append(prefix.end(prefix.turn), day);
        prefix.append(
            Record::MailState {
                id: prefix.mail,
                state: MailState::Acked,
            },
            day,
        );
        prefix.wake(22, day);
        h = prefix.harness();
        assert!(!h.supervisor.world().mail_wake_history_valid());
        assert!(!h.supervisor.world().turns.contains_key(&prefix.turn));
        assert_eq!(
            h.supervisor.world().mail_wakes.by_day[&(prefix.agent, day.utc_day())],
            20
        );
        assert_eq!(h.supervisor.world().goals[&prefix.goal].turns_on_goal, 22);
        assert!(h.step(Input::Started).is_empty());
    }

    #[test]
    fn mail_wake_origin_survives_goal_clearing_before_its_end() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        canonical_control(
            &mut h,
            Record::GoalCleared {
                agent: prefix.agent,
                goal: prefix.goal,
            },
        );
        let spawn = ready_spawn(&mut h, mint);
        launch(&mut h, spawn);
        let ended = one(h.step(exit(prefix.turn, 0, Usage::ZERO)));
        assert!(acknowledge_observations(&mut h, ended).is_empty());
        assert_eq!(
            h.supervisor.world().completed_goal_lsn(prefix.goal),
            Some(prefix.completion)
        );
        assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Idle);
        assert_eq!(h.supervisor.world().agents[&prefix.agent].goal, None);
        assert_eq!(h.supervisor.world().goals[&prefix.goal].turns_on_goal, 2);
        assert_eq!(
            h.supervisor
                .world()
                .mail_wakes
                .by_day
                .values()
                .copied()
                .sum::<u8>(),
            1
        );
        assert!(
            h.trace
                .iter()
                .all(|seen| !matches!(seen, Seen::RunPredicate { goal } if *goal == prefix.goal))
        );
    }

    #[test]
    fn mail_wake_adapter_refusal_ends_once_with_an_observed_hold() {
        use crate::testing::{FakeClock, FakeHost};
        use dudes_providers::claude::config::SettingSource;

        let prefix = Prefix::new(None);
        let harness = Harness::with_adapter(
            FakeClock::frozen(0),
            FakeHost::wall_clock_anchored(),
            || {
                Box::new(ClaudeAdapter::new(ClaudeConfig {
                    setting_sources: Some(vec![SettingSource::Project]),
                    ..ClaudeConfig::default()
                }))
            },
        );
        let mut h = replay_into(harness, &prefix.entries);
        let effects = h.step(Input::Started);
        let ended = one(instruction_work(&mut h, effects));
        assert!(
            matches!(ended.record(), Some(Record::TurnEnded { turn, outcome: Some(TurnOutcome::LaunchRefused { .. }), provenance: Provenance::Observed, .. }) if *turn == prefix.turn)
        );
        let held = one(acknowledge_observations(&mut h, ended));
        assert!(matches!(
            held.record(),
            Some(Record::AgentHeld {
                provenance: Provenance::Observed,
                cause: AutomaticHold::LaunchRefused { .. },
                ..
            })
        ));
        assert!(acknowledge(&mut h, held).is_empty());
        assert_eq!(h.supervisor.status_of(prefix.agent), AgentStatus::Done);
        assert!(h.supervisor.world().turns[&prefix.turn].has_ended());
        assert_eq!(h.spawns_of(prefix.turn).len(), 0);
        assert!(h.host.spawned.is_empty());
        assert!(
            h.supervisor
                .world()
                .instructions
                .unresolved_launch_attempts(prefix.agent)
                .any(|(_, attempt)| matches!(
                    attempt.finished.as_ref().map(|finish| &finish.settlement),
                    Some(InstructionSettlement::Unknown {
                        reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                        ..
                    })
                ))
        );
        assert!(!h.records().any(|record| matches!(
            record,
            Record::InstructionObservationFinished {
                settlement: InstructionSettlement::NoProcess { .. },
                ..
            }
        )));
        assert_completed(&h, &prefix);
    }

    #[test]
    fn mail_wake_two_rapid_failures_protect_before_any_work_on_live_end_and_restart() {
        let mut prefix = Prefix::completed(None);
        let (mut h, mint) = live(&mut prefix);
        let first = prefix.turn;
        let spawn = ready_spawn(&mut h, mint);
        launch(&mut h, spawn);
        let end = one(h.step(exit(first, 1, Usage::ZERO)));
        assert!(acknowledge_observations(&mut h, end).is_empty());
        assert_eq!(
            h.supervisor.world().agents[&prefix.agent].fast_failures,
            Some((first, first))
        );
        prefix.entries = h.journal.clone();
        let at = prefix
            .entries
            .iter()
            .find(|entry| matches!(entry.record, Record::MailWakeOnlyTurnIssued(_)))
            .unwrap()
            .at;
        assert!(
            mail_journaled(
                &mut h,
                Record::MailState {
                    id: prefix.mail,
                    state: MailState::Acked,
                },
            )
            .is_empty()
        );
        assert_eq!(at, wake_at());
        let armed = live_wake(&mut prefix, &mut h, 2);
        let spawn = ready_spawn(&mut h, armed);
        launch(&mut h, spawn);
        let end = one(h.step(exit(prefix.turn, 1, Usage::ZERO)));
        let beat = one(acknowledge_observations(&mut h, end));
        assert!(
            matches!(beat.record(), Some(Record::BreakerBeat { signals, .. }) if signals == &[dudes_types::BreakerSignal::RapidFailure])
        );
        let mut restarted = replay(&h.journal);
        let recovered = one(restarted.step(Input::Started));
        assert!(
            matches!(recovered.record(), Some(Record::BreakerBeat { signals, .. }) if signals == &[dudes_types::BreakerSignal::RapidFailure])
        );
        for (harness, effect) in [(&mut h, beat), (&mut restarted, recovered)] {
            let held = one(acknowledge(harness, effect));
            assert!(
                matches!(held.record(), Some(Record::AgentHeld { provenance: Provenance::Inferred, cause: AutomaticHold::RepeatedFastFailure { first: observed_first, latest }, .. }) if *observed_first == first && *latest == prefix.turn)
            );
            assert!(acknowledge(harness, held).is_empty());
            assert_eq!(
                harness.supervisor.world().completed_goal_lsn(prefix.goal),
                Some(prefix.completion)
            );
            assert_eq!(
                harness.supervisor.world().goals[&prefix.goal].turns_on_goal,
                3
            );
            assert_eq!(
                harness
                    .supervisor
                    .world()
                    .mail_wakes
                    .by_day
                    .values()
                    .copied()
                    .sum::<u8>(),
                2
            );
            assert!(
                harness.trace.iter().all(
                    |seen| !matches!(seen, Seen::RunPredicate { goal } if *goal == prefix.goal)
                )
            );
        }
    }

    #[test]
    fn mail_wake_completed_board_goal_stays_out_of_predicate_enumeration() {
        let mut prefix = Prefix::completed(None);
        let card = dudes_types::CardId::from_parts(3, 1);
        let Record::GoalSet { goal, .. } = &mut prefix.entries[1].record else {
            unreachable!()
        };
        goal.predicate = dudes_proto::Predicate::BoardState {
            card,
            status: dudes_types::CardStatus::Done,
        };
        for entry in &mut prefix.entries {
            if let Record::PredicateResult { evidence, .. } = &mut entry.record {
                *evidence = Evidence::Card {
                    card,
                    status: dudes_types::CardStatus::Done,
                    by: dudes_proto::Actor::System,
                };
            }
        }
        let (mut h, mint) = live(&mut prefix);
        let spawn = ready_spawn(&mut h, mint);
        launch(&mut h, spawn);
        assert!(board_changed(&mut h, card).is_empty());
        let end = one(h.step(exit(prefix.turn, 0, Usage::ZERO)));
        assert!(acknowledge_observations(&mut h, end).is_empty());
        assert!(board_changed(&mut h, card).is_empty());
        assert_completed(&h, &prefix);
        let mut restart = replay(&h.journal);
        assert!(restart.step(Input::Started).is_empty());
        assert!(board_changed(&mut restart, card).is_empty());
        assert_eq!(restart.supervisor.world().held_board_goals(None).count(), 0);

        let mut unfinished =
            prefix.entries[..usize::try_from(prefix.completion.0).unwrap() - 1].to_vec();
        let Record::PredicateResult { pass, .. } = &mut unfinished.last_mut().unwrap().record
        else {
            unreachable!()
        };
        *pass = false;
        let mut ordinary = replay(&unfinished);
        assert!(
            matches!(one(board_changed(&mut ordinary, card)), Seen::RunPredicate { goal } if goal == prefix.goal)
        );
    }

    #[test]
    fn mail_wake_invalid_issue_join_never_moves_status_or_reaches_launch() {
        let mut prefix = Prefix::new(None);
        let valid = prefix.harness();
        assert_eq!(
            valid.supervisor.status_of(prefix.agent),
            AgentStatus::Running
        );
        let Record::MailWakeOnlyTurnIssued(issue) = &mut prefix.entries.last_mut().unwrap().record
        else {
            unreachable!()
        };
        issue.task = MailWakeTask::new(2).unwrap();
        let mut invalid = prefix.harness();
        assert!(!invalid.supervisor.world().mail_wake_history_valid());
        assert!(!invalid.supervisor.world().turns.contains_key(&prefix.turn));
        assert_eq!(
            invalid.supervisor.status_of(prefix.agent),
            AgentStatus::Done
        );
        assert_eq!(
            invalid.supervisor.world().goals[&prefix.goal].turns_on_goal,
            1
        );
        assert!(invalid.supervisor.world().mail_wakes.by_day.is_empty());
        assert!(invalid.step(Input::Started).is_empty());
    }

    #[test]
    fn current_goal_replacement_retires_unissued_wake_and_preserves_issued_goal() {
        let mut prefix = Prefix::new(None);
        let issue = prefix.entries.pop().unwrap();
        let Record::GoalSet { goal, .. } = &prefix.entries[1].record else {
            unreachable!()
        };
        let mut replacement = goal.clone();
        replacement.id = dudes_types::GoalId::from_parts(3, 1);
        let mut world = World::default();
        for (index, entry) in prefix.entries.iter().enumerate() {
            world.apply(Lsn(u64::try_from(index).unwrap() + 1), entry);
        }
        let prepared = world.mail_wake_state(prefix.agent).unwrap();
        assert!(prepared.ready());
        let frontier = prepared.preparation.as_ref().unwrap().plan.frontier;
        let change = Entry {
            at: issue.at,
            record: Record::GoalSet {
                agent: prefix.agent,
                goal: replacement.clone(),
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        };
        let changed_lsn = Lsn(u64::try_from(prefix.entries.len()).unwrap() + 1);
        world.apply(changed_lsn, &change);
        assert_eq!(world.agents[&prefix.agent].goal, Some(replacement.id));
        assert_eq!(world.agents[&prefix.agent].context_lsn, changed_lsn);
        let retired = world.mail_wake_state(prefix.agent).unwrap();
        assert!(retired.preparation.is_none());
        assert_eq!(retired.covered_frontier, frontier);
        world.apply(Lsn(changed_lsn.0 + 1), &issue);
        assert!(!world.mail_wake_history_valid());
        assert!(!world.turns.contains_key(&prefix.turn));
        assert_eq!(world.goals[&prefix.goal].turns_on_goal, 1);
        assert_eq!(world.goals[&replacement.id].turns_on_goal, 0);

        prefix.entries.push(issue);
        let mut issued = prefix.harness();
        let original_context = issued.supervisor.world().agents[&prefix.agent].context_lsn;
        assert_eq!(
            issued.supervisor.world().turns[&prefix.turn].goal,
            Some(prefix.goal)
        );
        assert!(
            issued
                .step(Input::Replayed {
                    lsn: Lsn(changed_lsn.0 + 1),
                    entry: change,
                })
                .is_empty()
        );
        let world = issued.supervisor.world();
        assert!(world.mail_wake_history_valid());
        assert_eq!(world.turns[&prefix.turn].goal, Some(prefix.goal));
        assert_eq!(world.agents[&prefix.agent].goal, Some(replacement.id));
        assert!(world.agents[&prefix.agent].context_lsn > original_context);
        assert_eq!(world.goals[&prefix.goal].turns_on_goal, 2);
        assert_eq!(world.goals[&replacement.id].turns_on_goal, 0);
        assert_eq!(
            world.mail_wake_count(prefix.agent, prefix.entries.last().unwrap().at.utc_day()),
            1
        );
    }
}
