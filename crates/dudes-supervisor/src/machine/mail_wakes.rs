use super::{
    AgentId, BreakerSignal, Clock, Effect, Entry, GoalId, Lsn, ManagedLaunch, Mode, Provenance,
    Record, Rng, SpendView, Supervisor, Then, Timestamp, TurnId,
};
use crate::input::{MailWakeRefreshFailure, MailWakeRequest};
use dudes_policy::{
    BudgetState, Ceilings, CompletedNoteState, Control, MailWakeFacts, MailWakeInput, MailWakeWork,
};
use dudes_proto::mail_wake::{MAX_WAKE_TARGETS, MailWakeProgress};
use dudes_proto::{
    MailWakeAdmission, MailWakeDecision, MailWakeDigest, MailWakeIssue, MailWakePlan,
    MailWakePurpose, MailWakeTarget, MailWakeTask,
};

#[derive(Clone, Debug)]
pub(super) enum MailRefreshPhase {
    Inspect,
    CheckIssue {
        root_lsn: Lsn,
        decision_lsn: Lsn,
        at: Timestamp,
        input_digest: MailWakeDigest,
    },
}

#[derive(Debug)]
pub(super) struct MailRefresh {
    request: MailWakeRequest,
    goal: GoalId,
    phase: MailRefreshPhase,
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Release only the volatile refresh abandoned by a failed runtime pump.
    /// Canonical debt and the persist ledger are unchanged; this emits no effect.
    pub fn abandon_mail_wake_refresh(&mut self, request: MailWakeRequest) {
        self.mail_refreshes
            .retain(|_, pending| pending.request != request);
    }

    /// Whether one mailbox callback still names an exact pending canonical target.
    #[must_use]
    pub fn mail_wake_notification_current(
        &self,
        agent: AgentId,
        root_lsn: Lsn,
        target: &MailWakeTarget,
    ) -> bool {
        self.mail_wake_can_reconcile(agent)
            && self
                .world
                .mail_wake_state(agent)
                .and_then(|state| state.preparation.as_ref())
                .is_some_and(|preparation| {
                    preparation.root_lsn == root_lsn
                        && self.world.completed_goal(agent)
                            == Some((preparation.goal, preparation.completion_lsn))
                        && preparation
                            .plan
                            .targets
                            .iter()
                            .zip(&preparation.progress)
                            .any(|(owned, progress)| {
                                owned == target && matches!(progress, MailWakeProgress::Pending)
                            })
                })
    }

    fn mail_wake_can_reconcile(&self, agent: AgentId) -> bool {
        self.world.mail_wake_history_valid()
            && !self.closing_active
            && !self.closing_recovery
            && !self.protection_pending(agent)
            && !self.reissue_pending(agent)
            && self.completed_note_input_for(agent).is_none()
            && self
                .world
                .agents
                .get(&agent)
                .is_some_and(|row| !row.retired && !row.attached && row.open_turn.is_none())
    }

    pub(super) fn request_mail_wake(&mut self, agent: AgentId) {
        self.request_mail_refresh(agent, MailRefreshPhase::Inspect);
    }

    pub(super) fn request_mail_refresh(&mut self, agent: AgentId, phase: MailRefreshPhase) {
        if !self.mail_wake_can_reconcile(agent)
            || self.mail_refreshes.contains_key(&agent)
            || self.ledger.values().any(|parked| match &parked.then {
                Then::MailWakeContinue { agent: owner }
                | Then::MailWakeCheckIssue { agent: owner, .. }
                | Then::Spawn { agent: owner, .. } => *owner == agent,
                _ => false,
            })
        {
            return;
        }
        let Some(state) = self.world.mail_wake_state(agent) else {
            return;
        };
        if state.preparation.is_none() && state.candidate.is_none() {
            return;
        }
        let Some((goal, _)) = self.world.completed_goal(agent) else {
            return;
        };
        let Some(next) = self.next_mail_request.checked_add(1) else {
            return;
        };
        self.next_mail_request = next;
        let request = MailWakeRequest(next);
        self.mail_refreshes.insert(
            agent,
            MailRefresh {
                request,
                goal,
                phase,
            },
        );
        self.pending.push(Effect::MailWakeRefresh {
            agent,
            goal,
            request,
        });
    }

    pub(super) fn mail_event_recipients(&self, record: &Record) -> Vec<AgentId> {
        if record.goal_install().is_some() {
            return self.world.mail_wake_recipients(record);
        }
        match record {
            Record::Mail { .. }
            | Record::MailState { .. }
            | Record::GoalCleared { .. }
            | Record::GoalReassigned { .. }
            | Record::AgentRetired { .. } => self.world.mail_wake_recipients(record),
            Record::BreakerBeat { signals, .. }
                if signals.contains(&BreakerSignal::UnackedMail) =>
            {
                self.world.mail_wake_recipients(record)
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn mail_journaled(&mut self, lsn: Lsn, entry: &Entry) {
        if !matches!(entry.record, Record::Mail { .. } | Record::MailState { .. })
            || self
                .world
                .last_applied_lsn
                .is_some_and(|previous| lsn <= previous)
        {
            return;
        }
        let agents = self.mail_event_recipients(&entry.record);
        self.apply_entry(lsn, entry);
        for agent in agents {
            self.request_mail_wake(agent);
        }
    }

    fn mail_wake_input_for(
        &self,
        agent: AgentId,
        at: Timestamp,
        spend: SpendView,
    ) -> Option<(MailWakeInput, MailWakePurpose)> {
        if !self.mail_wake_can_reconcile(agent) {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        let (goal_id, completion_lsn) = self.world.completed_goal(agent)?;
        let goal = self.world.goals.get(&goal_id)?;
        let state = self.world.mail_wake_state(agent)?;
        let (work, purpose) = if let Some(preparation) = &state.preparation {
            (
                MailWakeWork::Resume {
                    preparation: preparation.clone(),
                },
                MailWakePurpose::ExistingPreparation {
                    root_lsn: preparation.root_lsn,
                },
            )
        } else {
            let candidate = state.candidate.as_ref()?;
            let receipts = self.world.mail_wake_receipts(agent);
            let unread_count = u8::try_from(receipts.len().min(MAX_WAKE_TARGETS))
                .ok()
                .filter(|count| *count >= 1)?;
            let mut targets: Vec<_> = receipts
                .iter()
                .filter_map(|mail| {
                    let count = mail.count?;
                    (count < 5 && mail.reference.lsn <= candidate.frontier).then_some(
                        MailWakeTarget {
                            mail: mail.reference,
                            original_count: count,
                            target_count: count + 1,
                        },
                    )
                })
                .collect();
            targets.sort_by_key(|target| (target.mail.lsn, target.mail.id));
            targets.truncate(MAX_WAKE_TARGETS);
            let plan = MailWakePlan {
                trigger: candidate.trigger,
                frontier: candidate.frontier,
                unread_count,
                task: MailWakeTask::new(unread_count)?,
                targets,
            };
            (
                MailWakeWork::Fresh {
                    candidate: candidate.clone(),
                    plan: plan.clone(),
                },
                MailWakePurpose::FreshPreparation { plan },
            )
        };
        let input = MailWakeInput::new(
            MailWakeFacts {
                agent,
                completion: Some((goal_id, completion_lsn)),
                retired: row.retired,
                open_turn: row.open_turn,
                browser_note_priority: false,
                question_lsn: row.question_lsn,
                wakes_today: self.world.mail_wake_count(agent, at.utc_day()),
                fifth_crossing: state.fifth_crossing,
                fifth_consumed: state.fifth_consumed,
            },
            work,
            CompletedNoteState {
                control: Control {
                    halted: row.halted,
                    paused: row.paused,
                    attached: row.attached,
                    automatic_hold: row
                        .automatic_hold
                        .as_ref()
                        .map(dudes_proto::AutomaticHold::hold_reason),
                },
                last_turn_end: row.last_turn_end,
                open_question: row.open_question.clone(),
                budget_state: BudgetState {
                    goal: spend.goal,
                    goal_unknown_ends: spend.goal_unknown_ends,
                    agent_today: spend.agent_today,
                    agent_unknown_ends_today: spend.agent_unknown_ends_today,
                    floor_today: spend.floor_today,
                    floor_unknown_ends_today: spend.floor_unknown_ends_today,
                },
                ceilings: Ceilings {
                    goal: goal.goal.budget,
                    agent: row.budget,
                    floor_usd_per_day: self.config.floor_usd_per_day,
                },
                per_turn_floor: self.config.per_turn_floor,
                rung: row.rung,
                turns_on_goal: goal.turns_on_goal,
                hop_cap: goal.goal.hop_cap,
            },
        )?;
        Some((input, purpose))
    }

    pub(super) fn mail_wake_ready(
        &mut self,
        agent: AgentId,
        request: MailWakeRequest,
        result: Result<(Timestamp, SpendView, Lsn), MailWakeRefreshFailure>,
    ) {
        if self
            .mail_refreshes
            .get(&agent)
            .is_none_or(|pending| pending.request != request)
        {
            return;
        }
        let pending = self
            .mail_refreshes
            .remove(&agent)
            .expect("matching mail request");
        let Ok((now, spend, _)) = result else {
            return;
        };
        if self
            .world
            .completed_goal(agent)
            .is_none_or(|(goal, _)| goal != pending.goal)
        {
            return;
        }
        let Some((input, purpose)) = self.mail_wake_input_for(agent, now, spend) else {
            return;
        };
        let Ok(digest) = crate::host::mail_wake_input_digest(&input, now.utc_day()) else {
            return;
        };
        let decided = dudes_policy::admit_mail_wake(&input);
        if let MailRefreshPhase::CheckIssue { root_lsn, decision_lsn, at, input_digest } = pending.phase
            && now.utc_day() == at.utc_day() && digest == input_digest
            && decided.as_ref().is_some_and(|value| matches!(&value.decision, MailWakeAdmission::Issue { task } if task == input.task()))
            && self.world.mail_wake_state(agent).is_some_and(|state| state.ready()
                && state.preparation.as_ref().is_some_and(|preparation| preparation.root_lsn == root_lsn)
                && state.latest.as_ref().is_some_and(|latest| latest.lsn == decision_lsn && latest.at == at
                    && latest.root_lsn == Some(root_lsn) && latest.input_digest == input_digest
                    && matches!(latest.decision, MailWakeAdmission::Issue { .. }))) {
            self.issue_mail_wake(agent, root_lsn, decision_lsn, at);
            return;
        }
        let Some(decided) = decided else {
            self.notify_next_mail_target(agent);
            return;
        };
        self.record_mail_admission(&input, purpose, now, digest, decided);
    }

    fn record_mail_admission(
        &mut self,
        input: &MailWakeInput,
        mut purpose: MailWakePurpose,
        at: Timestamp,
        input_digest: MailWakeDigest,
        decided: dudes_policy::MailWakeDecided,
    ) {
        let agent = input.agent();
        let issue = matches!(decided.decision, MailWakeAdmission::Issue { .. });
        if !issue
            && self
                .world
                .mail_wake_state(agent)
                .and_then(|state| state.latest.as_ref())
                .is_some_and(|latest| latest.input_digest == input_digest)
        {
            return;
        }
        if !issue && let MailWakePurpose::FreshPreparation { plan } = purpose {
            purpose = MailWakePurpose::UnpreparedCandidate {
                trigger: plan.trigger,
                frontier: plan.frontier,
            };
        }
        let then = if issue {
            match purpose {
                MailWakePurpose::ExistingPreparation { root_lsn } => Then::MailWakeCheckIssue {
                    agent,
                    root_lsn,
                    at,
                    input_digest,
                },
                _ => Then::MailWakeContinue { agent },
            }
        } else {
            Then::Fold
        };
        let decision = MailWakeDecision {
            agent,
            goal: input.goal(),
            completion_lsn: input.completion_lsn(),
            purpose,
            input_digest,
            gate_trace: decided.gate_trace,
            decision: decided.decision,
        };
        if !decision.valid() {
            return;
        }
        self.persist_at(at, Record::MailWakeTurnDecided(decision), then);
    }

    fn notify_next_mail_target(&mut self, agent: AgentId) {
        let Some(preparation) = self
            .world
            .mail_wake_state(agent)
            .and_then(|state| state.preparation.as_ref())
        else {
            return;
        };
        if let Some((target, _)) = preparation
            .plan
            .targets
            .iter()
            .zip(&preparation.progress)
            .find(|(_, progress)| matches!(progress, MailWakeProgress::Pending))
        {
            self.pending.push(Effect::MailWakeNotify {
                agent,
                root_lsn: preparation.root_lsn,
                target: target.clone(),
            });
        }
    }

    fn issue_mail_wake(&mut self, agent: AgentId, root_lsn: Lsn, decision_lsn: Lsn, at: Timestamp) {
        if !self.mail_wake_can_reconcile(agent) || self.launch_held(agent) {
            return;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if row.halted || row.paused {
            return;
        }
        let Some(preparation) = self
            .world
            .mail_wake_state(agent)
            .and_then(|state| state.preparation.as_ref())
            .cloned()
        else {
            return;
        };
        let session = self.announce_session(agent, false);
        let Some(selection) = self.model_selection(&row) else {
            return;
        };
        let launch = match self.launch(
            agent,
            &row,
            preparation.plan.task.as_str(),
            &session,
            &selection,
        ) {
            ManagedLaunch::Ready(launch) => launch,
            ManagedLaunch::Unavailable => return,
            ManagedLaunch::Refused(error) => {
                self.hold_agent(
                    agent,
                    Provenance::Observed,
                    dudes_proto::AutomaticHold::LaunchRefused {
                        reason: error.to_string(),
                    },
                );
                return;
            }
        };
        let turn = TurnId::from_ulid(self.rng.ulid());
        let issue = MailWakeIssue {
            agent,
            turn,
            goal: preparation.goal,
            completion_lsn: preparation.completion_lsn,
            root_lsn,
            lsn_of_decision: decision_lsn,
            task: preparation.plan.task,
            session: session.id().cloned(),
            mode: Mode::Managed,
            model_selection: selection,
            answers: Vec::new(),
        };
        if !issue.valid() {
            return;
        }
        self.persist_at(
            at,
            Record::MailWakeOnlyTurnIssued(issue),
            Then::Spawn {
                agent,
                turn,
                launch,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Input;
    use crate::host::{ProcessHost as _, SpawnResult};
    use crate::testing::{Harness, Seen, journal_with_issued_turn};
    use dudes_proto::Evidence;
    use dudes_proto::{ExitStatus, LoopDecision, Spend, TurnOutcome};
    use dudes_types::{BreakerRung, MailId, MailState, Usage, Usd};
    use std::collections::BTreeMap;

    struct Flow {
        h: Harness,
        agent: AgentId,
        goal: GoalId,
        mail: MailId,
    }
    fn at(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    impl Flow {
        fn completed() -> Self {
            let (mut entries, agent, turn) =
                journal_with_issued_turn(None, "completed private goal");
            for entry in &mut entries {
                entry.at = Timestamp::UNIX_EPOCH;
            }
            let Record::GoalSet { goal, .. } = &entries[1].record else {
                panic!("goal fixture");
            };
            let goal = goal.id;
            entries.push(Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::TurnEnded {
                    final_reply: None,
                    agent,
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
                },
            });
            entries.push(Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::PredicateResult {
                    goal,
                    turn: Some(turn),
                    provenance: Provenance::Observed,
                    pass: true,
                    evidence: Evidence::Command {
                        argv: vec!["true".into()],
                        program: Some("true".into()),
                        exit: Some(0),
                        output_tail: String::new(),
                    },
                },
            });
            entries.push(Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::LoopDecided {
                    agent,
                    input_digest: "ordinary-done".into(),
                    gate_trace: Vec::new(),
                    decision: LoopDecision::Done,
                },
            });
            let mut h = Harness::default();
            for (entry, position) in entries.iter().zip(1u64..) {
                assert!(
                    h.step(Input::Replayed {
                        lsn: Lsn(position),
                        entry: entry.clone()
                    })
                    .is_empty()
                );
            }
            h.journal = entries;
            Self {
                h,
                agent,
                goal,
                mail: MailId::from_parts(3, 1),
            }
        }
        fn restarted(&self, entries: &[Entry]) -> Self {
            let mut restarted = Self {
                h: Harness::default(),
                agent: self.agent,
                goal: self.goal,
                mail: self.mail,
            };
            for (entry, position) in entries.iter().zip(1u64..) {
                assert!(
                    restarted
                        .h
                        .step(Input::Replayed {
                            lsn: Lsn(position),
                            entry: entry.clone()
                        })
                        .is_empty()
                );
            }
            restarted.h.journal = entries.to_vec();
            restarted
        }
        fn arrival(&mut self, count: u8) -> Vec<Seen> {
            let state = if count == 0 {
                MailState::Durable
            } else {
                MailState::Notified { n: count }
            };
            let record = serde_json::from_value(serde_json::json!({
                "t":"mail", "id":self.mail, "from":{"kind":"human"},
                "to":{"kind":"agent","agent":self.agent}, "act":"message", "subject":"private", "body_len":0, "state":state
            })).unwrap();
            self.journal(record)
        }
        fn journal(&mut self, record: Record) -> Vec<Seen> {
            let entry = Entry { at: at(1), record };
            self.h.journal.push(entry.clone());
            self.h.step(Input::Journaled {
                lsn: Lsn(u64::try_from(self.h.journal.len()).unwrap()),
                entry,
            })
        }
        fn refresh(&mut self, effects: &[Seen], stamp: Timestamp, spend: SpendView) -> Vec<Seen> {
            let Some(Seen::MailWakeRefresh { agent, request, .. }) = effects
                .iter()
                .find(|effect| matches!(effect, Seen::MailWakeRefresh { .. }))
            else {
                panic!("expected current mail refresh");
            };
            self.h.step(Input::MailWakeReady {
                agent: *agent,
                request: *request,
                result: Ok((stamp, spend, Lsn::ZERO)),
            })
        }
        fn ack(&mut self, effects: &[Seen]) -> Vec<Seen> {
            let Some(Seen::Persist { token, entry }) = effects
                .iter()
                .find(|effect| matches!(effect, Seen::Persist { .. }))
            else {
                panic!("expected persistence");
            };
            self.h.journal.push(entry.clone());
            self.h.step(Input::Persisted {
                token: *token,
                lsn: Lsn(u64::try_from(self.h.journal.len()).unwrap()),
            })
        }
        fn root(&mut self, count: u8) -> Vec<Seen> {
            let arrival = self.arrival(count);
            let root = self.refresh(&arrival, at(1), SpendView::default());
            assert!(matches!(
                root.as_slice(),
                [Seen::Persist {
                    entry: Entry {
                        record: Record::MailWakeTurnDecided(MailWakeDecision {
                            purpose: MailWakePurpose::FreshPreparation { .. },
                            ..
                        }),
                        ..
                    },
                    ..
                }]
            ));
            assert!(
                self.h
                    .supervisor
                    .world
                    .mail_wake_state(self.agent)
                    .unwrap()
                    .preparation
                    .is_none()
            );
            self.ack(&root)
        }
        fn advance(&mut self, refresh: &[Seen]) -> Vec<Seen> {
            let next = self.refresh(refresh, at(1), SpendView::default());
            let [
                Seen::MailWakeNotify {
                    root_lsn, target, ..
                },
            ] = next.as_slice()
            else {
                panic!("one exact advance");
            };
            assert!(
                self.h
                    .supervisor
                    .mail_wake_notification_current(self.agent, *root_lsn, target)
            );
            self.journal(Record::MailState {
                id: target.mail.id,
                state: MailState::Notified {
                    n: target.target_count,
                },
            })
        }
        fn spawn(&mut self, effects: Vec<Seen>, issue: &MailWakeIssue) {
            assert!(self.h.host.spawned.is_empty());
            let launch = if matches!(
                effects.as_slice(),
                [Seen::Persist {
                    entry: Entry {
                        record: Record::HookTokenMinted { .. },
                        ..
                    },
                    ..
                }]
            ) {
                self.ack(&effects)
            } else {
                effects
            };
            let [
                Seen::Spawn {
                    turn,
                    spec,
                    install,
                    ..
                },
            ] = launch.as_slice()
            else {
                panic!("one acknowledged mail launch");
            };
            assert_eq!(*turn, issue.turn);
            assert_eq!(
                spec.argv
                    .iter()
                    .filter(|arg| *arg == issue.task.as_str())
                    .count(),
                1
            );
            let SpawnResult::Spawned(key) = self.h.host.spawn(spec, install) else {
                panic!("fake host launches the issued task");
            };
            let spawned = self.h.step(Input::Spawned {
                turn: *turn,
                key,
                mcp_grant: None,
            });
            assert!(matches!(
                spawned.as_slice(),
                [Seen::Persist { entry: Entry { record: Record::TurnSpawned { agent, turn, .. }, .. }, .. }]
                    if *agent == self.agent && *turn == issue.turn
            ));
            assert!(self.ack(&spawned).is_empty());
            assert_eq!(self.h.host.spawned.len(), 1);
            assert_eq!(
                self.h
                    .journal
                    .iter()
                    .filter(|entry| matches!(
                        entry.record, Record::TurnSpawned { turn, .. } if turn == issue.turn
                    ))
                    .count(),
                1
            );
        }
        fn assert_fifth_accounting(&self, issues: u8) {
            let states: Vec<_> = self
                .h
                .journal
                .iter()
                .filter_map(|entry| match &entry.record {
                    Record::MailState { id, state } if *id == self.mail => Some(state),
                    _ => None,
                })
                .collect();
            assert_eq!(states, [&MailState::Notified { n: 5 }]);
            assert_eq!(
                self.h
                    .journal
                    .iter()
                    .filter(|entry| matches!(
                        entry.record,
                        Record::MailWakeTurnDecided(MailWakeDecision {
                            purpose: MailWakePurpose::FreshPreparation { .. },
                            ..
                        })
                    ))
                    .count(),
                1
            );
            assert_eq!(
                self.h
                    .journal
                    .iter()
                    .filter(|entry| matches!(entry.record, Record::MailWakeOnlyTurnIssued(_)))
                    .count(),
                usize::from(issues)
            );
            assert_eq!(
                self.h.supervisor.world.goals[&self.goal].turns_on_goal,
                1 + u32::from(issues)
            );
            assert_eq!(
                self.h
                    .supervisor
                    .world
                    .mail_wake_count(self.agent, at(1).utc_day()),
                issues
            );
            let completions: Vec<_> = self
                .h
                .journal
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    matches!(
                        entry.record,
                        Record::LoopDecided {
                            decision: LoopDecision::Done,
                            ..
                        }
                    )
                    .then_some(Lsn(u64::try_from(index + 1).unwrap()))
                })
                .collect();
            assert_eq!(completions.len(), 1);
            assert_eq!(
                self.h.supervisor.world.completed_goal(self.agent),
                Some((self.goal, completions[0]))
            );
        }
        fn final_decision(&mut self) -> Vec<Seen> {
            let root = self.root(0);
            let advanced = self.advance(&root);
            let decision = self.refresh(&advanced, at(1), SpendView::default());
            assert!(matches!(
                decision.as_slice(),
                [Seen::Persist {
                    entry: Entry {
                        record: Record::MailWakeTurnDecided(MailWakeDecision {
                            purpose: MailWakePurpose::ExistingPreparation { .. },
                            ..
                        }),
                        ..
                    },
                    ..
                }]
            ));
            assert!(
                self.h.supervisor.world.agents[&self.agent]
                    .open_turn
                    .is_none()
            );
            decision
        }
    }

    #[test]
    fn preparation_advance_and_same_stamp_issue_are_acknowledged_before_spawn() {
        let mut f = Flow::completed();
        let final_decision = f.final_decision();
        let check = f.ack(&final_decision);
        let issue = f.refresh(&check, at(5_000), SpendView::default());
        let [
            Seen::Persist {
                entry:
                    Entry {
                        at: stamp,
                        record: Record::MailWakeOnlyTurnIssued(carrier),
                    },
                ..
            },
        ] = issue.as_slice()
        else {
            panic!("freshly checked mail carrier");
        };
        assert_eq!(*stamp, at(1));
        assert_eq!(carrier.task, MailWakeTask::new(1).unwrap());
        assert_eq!(carrier.goal, f.goal);
        assert!(carrier.answers.is_empty());
        assert_eq!(
            f.h.supervisor
                .world
                .mail_wake_count(f.agent, stamp.utc_day()),
            0
        );
        let turn = carrier.turn;
        let after = f.ack(&issue);
        assert_eq!(f.h.supervisor.world.agents[&f.agent].open_turn, Some(turn));
        assert_eq!(f.h.supervisor.world.goals[&f.goal].turns_on_goal, 2);
        assert_eq!(
            f.h.supervisor
                .world
                .mail_wake_count(f.agent, stamp.utc_day()),
            1
        );
        let launch = if after
            .iter()
            .any(|seen| matches!(seen, Seen::Persist { .. }))
        {
            f.ack(&after)
        } else {
            after
        };
        assert!(
            launch
                .iter()
                .any(|seen| matches!(seen, Seen::Spawn { turn: spawned, .. } if *spawned == turn))
        );
        assert_eq!(
            f.h.journal
                .iter()
                .filter(|entry| matches!(
                    entry.record,
                    Record::LoopDecided {
                        decision: LoopDecision::Done,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn forty_pending_mails_still_prepare_one_plan_of_the_oldest_thirty_two() {
        let mut f = Flow::completed();
        f.mail = MailId::from_parts(3, 1);
        let first = f.arrival(0);
        for number in 2..=40u128 {
            f.mail = MailId::from_parts(3, number);
            f.arrival(0);
        }
        let root = f.refresh(&first, at(1), SpendView::default());
        let [
            Seen::Persist {
                entry:
                    Entry {
                        record:
                            Record::MailWakeTurnDecided(MailWakeDecision {
                                purpose: MailWakePurpose::FreshPreparation { plan },
                                ..
                            }),
                        ..
                    },
                ..
            },
        ] = root.as_slice()
        else {
            panic!("a fresh preparation despite forty pending mails");
        };
        assert_eq!(plan.unread_count, 32);
        assert_eq!(plan.task, MailWakeTask::new(32).unwrap());
        let expected: Vec<_> = (1..=32u128).map(|n| MailId::from_parts(3, n)).collect();
        let planned: Vec<_> = plan.targets.iter().map(|target| target.mail.id).collect();
        assert_eq!(planned, expected);
        f.ack(&root);
        assert!(
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .preparation
                .is_some()
        );
    }

    fn mail_wake_record(seen: &Seen) -> bool {
        matches!(
            seen,
            Seen::Persist {
                entry: Entry {
                    record: Record::MailWakeTurnDecided(_) | Record::MailWakeOnlyTurnIssued(_),
                    ..
                },
                ..
            }
        )
    }

    #[test]
    fn a_queued_note_takes_the_pass_and_the_owed_mail_waits_until_its_turn_ends() {
        let mut f = Flow::completed();
        let queued =
            f.h.step(Input::Command(crate::input::Command::BrowserNoteQueued {
                agent: f.agent,
                text: "queued note".into(),
                browser: dudes_proto::BrowserNoteAttribution {
                    request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
                    request_digest: dudes_proto::record::Hex32::new([2; 32]),
                    session_digest: dudes_proto::record::Hex32::new([3; 32]),
                },
                destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal: f.goal },
            }));
        let mut seen = queued;
        let mut persisted = 0;
        let mut note_turn = None;
        while let Some(Seen::Persist { entry, .. }) = seen
            .iter()
            .find(|effect| matches!(effect, Seen::Persist { .. }))
        {
            if let Record::TaughtNoteOnlyTurnIssued { turn, .. } = &entry.record {
                note_turn = Some(*turn);
            }
            persisted += 1;
            let next = f.ack(&seen);
            let mail = f.arrival(0);
            assert!(
                !mail.iter().any(mail_wake_record),
                "no mail wake beside a queued note"
            );
            assert!(
                !mail
                    .iter()
                    .any(|effect| matches!(effect, Seen::MailWakeRefresh { .. })),
                "no mail refresh while the note owns the pass"
            );
            f.mail = MailId::from_parts(3, u128::try_from(persisted).unwrap() + 100);
            seen = next;
        }
        assert!(persisted >= 2, "the note decision and its issue");
        let turn = note_turn.expect("exactly one note issue");
        assert!(
            f.h.journal.iter().all(|entry| !matches!(
                entry.record,
                Record::MailWakeTurnDecided(_) | Record::MailWakeOnlyTurnIssued(_)
            )),
            "no mail-wake record while the note is in flight"
        );
        assert_eq!(
            f.h.journal
                .iter()
                .filter(|entry| matches!(entry.record, Record::TaughtNoteOnlyTurnIssued { .. }))
                .count(),
            1
        );
        let mut effects = f.h.step(Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(Usage::ZERO)),
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        });
        let mut refresh = None;
        for _ in 0..8 {
            if let Some(found) = effects
                .iter()
                .find(|effect| matches!(effect, Seen::MailWakeRefresh { .. }))
            {
                refresh = Some(found.clone());
                break;
            }
            if !effects
                .iter()
                .any(|effect| matches!(effect, Seen::Persist { .. }))
            {
                break;
            }
            effects = f.ack(&effects);
        }
        let refresh = refresh.expect("the owed mail is reconsidered after the note turn ends");
        let decision = f.refresh(&[refresh], at(1), SpendView::default());
        assert!(
            decision.iter().any(|effect| matches!(
                effect,
                Seen::Persist {
                    entry: Entry {
                        record: Record::MailWakeTurnDecided(_),
                        ..
                    },
                    ..
                }
            )),
            "a mail decision follows the note turn"
        );
    }

    #[test]
    fn restart_at_each_committed_stage_reuses_progress_or_the_issued_turn() {
        let mut f = Flow::completed();
        let final_decision = f.final_decision();
        let check = f.ack(&final_decision);
        let issued = f.refresh(&check, at(1), SpendView::default());
        let Some(Seen::Persist {
            entry:
                Entry {
                    record: Record::MailWakeOnlyTurnIssued(carrier),
                    ..
                },
            ..
        }) = issued.first()
        else {
            panic!("carrier");
        };
        let carrier = carrier.clone();
        f.ack(&issued);
        let history = f.h.journal.clone();
        for cut in 8..=11 {
            let mut restarted = f.restarted(&history[..cut]);
            let ready = restarted.h.step(Input::Started);
            if cut == 11 {
                assert!(
                    !ready
                        .iter()
                        .any(|seen| matches!(seen, Seen::MailWakeRefresh { .. }))
                );
                assert_eq!(
                    restarted.h.supervisor.world.turns[&carrier.turn].task,
                    carrier.task.as_str()
                );
                assert_eq!(
                    restarted.h.supervisor.world.turns[&carrier.turn].model_selection,
                    Some(carrier.model_selection.clone())
                );
                let launch = if ready
                    .iter()
                    .any(|seen| matches!(seen, Seen::Persist { .. }))
                {
                    restarted.ack(&ready)
                } else {
                    ready
                };
                assert!(launch.iter().any(|seen| matches!(seen, Seen::Spawn { turn, spec, .. }
                    if *turn == carrier.turn && spec.argv.iter().any(|arg| arg == carrier.task.as_str()))));
                assert_eq!(
                    restarted
                        .h
                        .journal
                        .iter()
                        .filter(|entry| matches!(entry.record, Record::MailWakeOnlyTurnIssued(_)))
                        .count(),
                    1
                );
            } else {
                let continuation = restarted.refresh(&ready, at(2), SpendView::default());
                if cut == 8 {
                    let [Seen::MailWakeNotify { target, .. }] = continuation.as_slice() else {
                        panic!("missing first advance");
                    };
                    assert_eq!((target.original_count, target.target_count), (0, 1));
                } else {
                    assert!(matches!(
                        continuation.as_slice(),
                        [Seen::Persist {
                            entry: Entry {
                                record: Record::MailWakeTurnDecided(MailWakeDecision {
                                    decision: MailWakeAdmission::Issue { .. },
                                    ..
                                }),
                                ..
                            },
                            ..
                        }]
                    ));
                }
                assert_eq!(
                    restarted
                        .h
                        .supervisor
                        .world
                        .mail_wake_count(f.agent, at(2).utc_day()),
                    0
                );
            }
        }
    }

    #[test]
    fn postack_midnight_and_backward_day_require_a_new_decision() {
        for current in [86_400_001, -1] {
            let mut f = Flow::completed();
            let decision = f.final_decision();
            let check = f.ack(&decision);
            let changed = f.refresh(&check, at(current), SpendView::default());
            let [Seen::Persist { entry, .. }] = changed.as_slice() else {
                panic!("new current admission");
            };
            assert_eq!(entry.at, at(current));
            assert!(matches!(entry.record, Record::MailWakeTurnDecided(_)));
            assert_eq!(
                f.h.supervisor
                    .world
                    .mail_wake_count(f.agent, at(current).utc_day()),
                0
            );
        }
    }

    #[test]
    fn postack_changed_budget_and_control_record_current_refusal() {
        for control in [false, true] {
            let mut f = Flow::completed();
            let decision = f.final_decision();
            let check = f.ack(&decision);
            if control {
                let entry = Entry {
                    at: at(2),
                    record: Record::Pause {
                        agent: f.agent,
                        browser: None,
                    },
                };
                f.h.journal.push(entry.clone());
                f.h.step(Input::Replayed {
                    lsn: Lsn(u64::try_from(f.h.journal.len()).unwrap()),
                    entry,
                });
            }
            let mut spend = SpendView::default();
            if !control {
                spend.goal.usd = Usd::from_dollars(6);
            }
            let refused = f.refresh(&check, at(3), spend);
            let [
                Seen::Persist {
                    entry:
                        Entry {
                            record: Record::MailWakeTurnDecided(value),
                            ..
                        },
                    ..
                },
            ] = refused.as_slice()
            else {
                panic!("current refusal");
            };
            assert!(if control {
                matches!(value.decision, MailWakeAdmission::Hold { .. })
            } else {
                matches!(value.decision, MailWakeAdmission::Exhausted { .. })
            });
            assert!(
                f.h.supervisor
                    .world
                    .mail_wake_state(f.agent)
                    .unwrap()
                    .preparation
                    .is_some()
            );
        }
    }

    #[test]
    fn refresh_failure_abandonment_and_stale_responses_do_not_authorize_or_clear_new_owner() {
        let mut f = Flow::completed();
        let first = f.arrival(0);
        let [Seen::MailWakeRefresh { request: old, .. }] = first.as_slice() else {
            panic!("refresh");
        };
        let failed = f.h.step(Input::MailWakeReady {
            agent: f.agent,
            request: *old,
            result: Err(MailWakeRefreshFailure::SpendUnavailable),
        });
        assert!(failed.is_empty());
        assert!(f.h.supervisor.mail_refreshes.is_empty());
        assert!(
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .candidate
                .is_some()
        );
        f.h.supervisor.request_mail_wake(f.agent);
        let newer = f.h.supervisor.mail_refreshes[&f.agent].request;
        assert_ne!(newer, *old);
        f.h.supervisor.abandon_mail_wake_refresh(*old);
        assert!(
            f.h.step(Input::MailWakeReady {
                agent: f.agent,
                request: *old,
                result: Ok((at(1), SpendView::default(), Lsn::ZERO))
            })
            .is_empty()
        );
        assert_eq!(f.h.supervisor.mail_refreshes[&f.agent].request, newer);
        f.h.supervisor.abandon_mail_wake_refresh(newer);
        assert!(f.h.supervisor.mail_refreshes.is_empty());
        assert!(
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .candidate
                .is_some()
        );
        assert_eq!(f.h.journal.len(), 7);
    }

    fn assert_consuming_fifth_beat(effects: &[Seen]) {
        assert!(matches!(
            effects,
            [Seen::Persist { entry: Entry { record: Record::BreakerBeat {
                rung_before: BreakerRung::Healthy,
                rung_after: BreakerRung::Steer,
                signals,
                ..
            }, .. }, .. }] if signals == &[BreakerSignal::UnackedMail]
        ));
    }

    #[test]
    fn consuming_fifth_beat_resumes_without_another_arrival() {
        let mut f = Flow::completed();
        let completion = f.h.supervisor.world.completed_goal(f.agent);
        let root = f.root(4);
        let preparation =
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .preparation
                .clone()
                .unwrap();
        let advanced = f.advance(&root);
        assert!(f.refresh(&advanced, at(1), SpendView::default()).is_empty());
        let beat = f.h.step(Input::Tick);
        assert_consuming_fifth_beat(&beat);
        f.assert_fifth_accounting(0);
        assert!(f.h.host.spawned.is_empty());
        let consumed = f.ack(&beat);
        assert_eq!(
            f.h.supervisor.world.agents[&f.agent].rung,
            BreakerRung::Steer
        );
        let decided = f.refresh(&consumed, at(1), SpendView::default());
        assert!(matches!(
            decided.as_slice(),
            [Seen::Persist { entry: Entry { record: Record::MailWakeTurnDecided(MailWakeDecision {
                purpose: MailWakePurpose::ExistingPreparation { root_lsn },
                decision: MailWakeAdmission::Issue { .. }, ..
            }), .. }, .. }] if *root_lsn == preparation.root_lsn
        ));
        f.assert_fifth_accounting(0);
        assert!(
            f.h.trace.iter().all(|seen| !matches!(
                seen,
                Seen::Spawn { .. }
                    | Seen::Persist {
                        entry: Entry {
                            record: Record::MailWakeOnlyTurnIssued(_),
                            ..
                        },
                        ..
                    }
            )),
            "the decision acknowledgement precedes issue and spawn"
        );
        let checked = f.ack(&decided);
        let issued = f.refresh(&checked, at(1), SpendView::default());
        let [
            Seen::Persist {
                entry:
                    Entry {
                        record: Record::MailWakeOnlyTurnIssued(issue),
                        ..
                    },
                ..
            },
        ] = issued.as_slice()
        else {
            panic!("the consuming beat reaches an issue without a later input");
        };
        let issue = issue.clone();
        assert_eq!(issue.root_lsn, preparation.root_lsn);
        assert_eq!(issue.task, preparation.plan.task);
        assert_eq!(
            issue.task.as_str(),
            format!(
                "[1 unread mail]{}{}",
                dudes_proto::MEMORY_INSTRUCTION,
                dudes_proto::MAIL_INSTRUCTION
            )
        );
        assert_eq!(
            issue.lsn_of_decision,
            Lsn(u64::try_from(f.h.journal.len()).unwrap())
        );
        assert_eq!(Some((issue.goal, issue.completion_lsn)), completion);
        assert!(issue.answers.is_empty());
        assert!(f.h.supervisor.world.agents[&f.agent].open_turn.is_none());
        f.assert_fifth_accounting(0);
        assert!(
            f.h.trace
                .iter()
                .all(|seen| !matches!(seen, Seen::Spawn { .. })),
            "the issue acknowledgement precedes spawn"
        );
        let launch = f.ack(&issued);
        f.spawn(launch, &issue);
        f.assert_fifth_accounting(1);
        assert_eq!(
            f.h.supervisor.world.agents[&f.agent].open_turn,
            Some(issue.turn)
        );
    }

    fn assert_fifth_restart_refusals(f: &Flow) {
        for paused in [true, false] {
            let mut history = f.h.journal.clone();
            if paused {
                history.push(Entry {
                    at: at(1),
                    record: Record::Pause {
                        agent: f.agent,
                        browser: None,
                    },
                });
            }
            let mut restarted = f.restarted(&history);
            let started = restarted.h.step(Input::Started);
            let mut spend = SpendView::default();
            if !paused {
                spend.goal.usd = Usd::from_dollars(6);
            }
            let refused = restarted.refresh(&started, at(1), spend);
            let [
                Seen::Persist {
                    entry:
                        Entry {
                            record: Record::MailWakeTurnDecided(decision),
                            ..
                        },
                    ..
                },
            ] = refused.as_slice()
            else {
                panic!("current controls still refuse a charged preparation");
            };
            assert!(if paused {
                matches!(decision.decision, MailWakeAdmission::Hold { .. })
            } else {
                matches!(decision.decision, MailWakeAdmission::Exhausted { .. })
            });
            assert!(restarted.ack(&refused).is_empty());
            assert_eq!(
                restarted
                    .h
                    .supervisor
                    .world
                    .mail_wake_state(f.agent)
                    .unwrap()
                    .preparation,
                f.h.supervisor
                    .world
                    .mail_wake_state(f.agent)
                    .unwrap()
                    .preparation
            );
            assert!(restarted.h.host.spawned.is_empty());
            restarted.assert_fifth_accounting(0);
        }
    }

    #[test]
    fn fifth_notification_survives_restarts_before_beat_ack_and_spawn() {
        let mut f = Flow::completed();
        let root = f.root(4);
        let preparation =
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .preparation
                .clone()
                .unwrap();
        let advanced = f.advance(&root);
        assert!(f.refresh(&advanced, at(1), SpendView::default()).is_empty());
        for _ in 0..2 {
            f = f.restarted(&f.h.journal);
            let started = f.h.step(Input::Started);
            assert!(f.refresh(&started, at(1), SpendView::default()).is_empty());
            f.assert_fifth_accounting(0);
            assert!(f.h.host.spawned.is_empty());
            let owed =
                f.h.supervisor
                    .world
                    .mail_wake_state(f.agent)
                    .unwrap()
                    .preparation
                    .as_ref()
                    .unwrap();
            assert_eq!(owed.root_lsn, preparation.root_lsn);
            assert_eq!(owed.plan, preparation.plan);
        }
        let beat = f.h.step(Input::Tick);
        assert_consuming_fifth_beat(&beat);
        let [Seen::Persist { entry, .. }] = beat.as_slice() else {
            unreachable!()
        };
        f.h.journal.push(entry.clone());
        f = f.restarted(&f.h.journal);
        f.assert_fifth_accounting(0);
        assert_eq!(
            f.h.supervisor.world.agents[&f.agent].rung,
            BreakerRung::Steer
        );
        let state = f.h.supervisor.world.mail_wake_state(f.agent).unwrap();
        assert!(state.fifth_consumed > state.fifth_crossing);
        assert_eq!(
            state.fifth_consumed,
            Lsn(u64::try_from(f.h.journal.len()).unwrap())
        );
        let started = f.h.step(Input::Started);
        let decided = f.refresh(&started, at(1), SpendView::default());
        assert!(
            matches!(
                decided.as_slice(),
                [Seen::Persist { entry: Entry { record: Record::MailWakeTurnDecided(MailWakeDecision {
                    purpose: MailWakePurpose::ExistingPreparation { root_lsn },
                    decision: MailWakeAdmission::Issue { .. }, ..
                }), .. }, .. }] if *root_lsn == preparation.root_lsn
            ),
            "a charged target at five still owes its original issue after restart"
        );
        assert_fifth_restart_refusals(&f);
        let checked = f.ack(&decided);
        let issued = f.refresh(&checked, at(1), SpendView::default());
        let [
            Seen::Persist {
                entry:
                    Entry {
                        record: Record::MailWakeOnlyTurnIssued(issue),
                        ..
                    },
                ..
            },
        ] = issued.as_slice()
        else {
            panic!("the recovered preparation reaches its issue");
        };
        let issue = issue.clone();
        assert_eq!(issue.root_lsn, preparation.root_lsn);
        assert_eq!(issue.task, preparation.plan.task);
        f.ack(&issued);
        for restart in 0..2 {
            assert!(f.h.host.spawned.is_empty());
            f = f.restarted(&f.h.journal);
            let launch = f.h.step(Input::Started);
            f.assert_fifth_accounting(1);
            assert_eq!(
                f.h.supervisor.world.turns[&issue.turn].task,
                issue.task.as_str()
            );
            assert_eq!(
                f.h.supervisor.world.agents[&f.agent].open_turn,
                Some(issue.turn)
            );
            if restart == 1 {
                f.spawn(launch, &issue);
            }
        }
        f.assert_fifth_accounting(1);
    }

    #[test]
    fn terminal_retirement_preserves_a_later_arrival_for_reconciliation() {
        let mut f = Flow::completed();
        let root = f.root(0);
        f.h.supervisor.abandon_mail_wake_refresh(match &root[0] {
            Seen::MailWakeRefresh { request, .. } => *request,
            _ => panic!("refresh"),
        });
        let first = f.mail;
        f.mail = MailId::from_parts(3, 2);
        let later = f.arrival(0);
        let terminal = f.journal(Record::MailState {
            id: first,
            state: MailState::Acked,
        });
        assert!(terminal.is_empty());
        let next = f.refresh(&later, at(2), SpendView::default());
        let [
            Seen::Persist {
                entry:
                    Entry {
                        record: Record::MailWakeTurnDecided(value),
                        ..
                    },
                ..
            },
        ] = next.as_slice()
        else {
            panic!("next preparation");
        };
        let MailWakePurpose::FreshPreparation { plan } = &value.purpose else {
            panic!("fresh later frontier");
        };
        assert_eq!(plan.trigger.id, f.mail);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].mail.id, f.mail);
    }

    #[test]
    fn pending_progress_still_records_earlier_gate_refusal_and_suppresses_unchanged_repeat() {
        let mut f = Flow::completed();
        let root = f.root(0);
        let spend = SpendView {
            agent_today: Spend {
                usd: Usd::from_dollars(11),
                ..Spend::default()
            },
            ..SpendView::default()
        };
        let refused = f.refresh(&root, at(1), spend);
        let again = f.ack(&refused);
        assert!(again.is_empty());
        f.h.supervisor.request_mail_wake(f.agent);
        let request = f.h.supervisor.mail_refreshes[&f.agent].request;
        assert!(
            f.h.step(Input::MailWakeReady {
                agent: f.agent,
                request,
                result: Ok((at(2), spend, Lsn::ZERO))
            })
            .is_empty()
        );
        assert!(
            f.h.supervisor
                .world
                .mail_wake_state(f.agent)
                .unwrap()
                .preparation
                .as_ref()
                .unwrap()
                .progress
                .iter()
                .all(|progress| matches!(progress, MailWakeProgress::Pending))
        );
    }
}
