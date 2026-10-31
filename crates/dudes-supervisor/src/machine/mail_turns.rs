//! Mail turns: a turn for a goal-free agent that wake-worthy mail from one teammate started.

use super::{
    AgentId, Clock, Effect, Lsn, ManagedLaunch, Record, Rng, Supervisor, Then, Timestamp, TurnId,
};
use crate::input::{MailWakeRefreshFailure, MailWakeRequest};
use crate::world::MailTurnCandidate;
use dudes_policy::{
    AGENT_DAY_TURN_CAP, BudgetState, Control, MailTurnInput, MailTurnUse, SENDER_DAY_TURN_CAP,
    THREAD_TURN_CAP,
};
use dudes_proto::record::Hex32;
use dudes_proto::{
    BrowserNoteDestination, ExhaustedReason, MailRef, MailTurnAdmission, MailTurnDecision,
    MailTurnIssue, MailTurnTask,
};
use dudes_types::{BreakerRung, MailId, UtcDay};

/// How many mailbox passes may find a candidate's mail still unpublished before the beat alone
/// looks again.
const MAIL_TURN_PASS_RETRIES: u8 = 16;

/// One mailbox inspection that is outstanding for an agent.
#[derive(Debug)]
pub(super) struct MailTurnInspection {
    request: MailWakeRequest,
    key: String,
}

fn hex(bytes: &[u8]) -> Hex32 {
    Hex32::new(*blake3::hash(bytes).as_bytes())
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Release the inspection a failed runtime pump abandoned; nothing is written.
    pub fn abandon_mail_turn_inspection(&mut self, agent: AgentId, request: MailWakeRequest) {
        if self
            .mail_turn_inspections
            .get(&agent)
            .is_some_and(|pending| pending.request == request)
        {
            self.mail_turn_inspections.remove(&agent);
        }
    }

    fn mail_turn_open(&self, agent: AgentId) -> bool {
        !self.closing_active
            && !self.closing_recovery
            && self.browser_delivery_open
            && self.world.mail_wake_history_valid()
            && !self.instructions.recovering()
            && !self.instructions.pending_agents.contains(&agent)
            && !self.protection_pending(agent)
            && !self.reissue_pending(agent)
            && self.world.agents.get(&agent).is_some_and(|row| {
                row.goal.is_none()
                    && !row.retired
                    && !row.attached
                    && row.open_turn.is_none()
                    && self
                        .world
                        .eligible_browser_note(
                            agent,
                            &BrowserNoteDestination::ManagedConversation {
                                context_lsn: row.context_lsn,
                            },
                        )
                        .is_none()
            })
            && self
                .world
                .mail_wake_state(agent)
                .is_none_or(|state| state.preparation.is_none())
    }

    /// The admission input for the covered mail of `candidate`, or none while the agent lacks
    /// what a turn needs.
    fn mail_turn_input(
        &self,
        agent: AgentId,
        candidate: &MailTurnCandidate,
        covered: Vec<MailRef>,
        inspection_digest: String,
        day: UtcDay,
    ) -> Option<MailTurnInput> {
        let row = self.world.agents.get(&agent)?;
        let spend = self.spend.get(&agent)?;
        Some(MailTurnInput {
            command_set: row.task_command_set(),
            agent,
            thread: candidate.thread,
            covered,
            inspection_digest,
            identity: row.identity_line(agent)?,
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
            question_lsn: row.question_lsn,
            budget_state: BudgetState {
                goal: dudes_proto::Spend::default(),
                goal_unknown_ends: 0,
                agent_today: spend.agent_today,
                agent_unknown_ends_today: spend.agent_unknown_ends_today,
                floor_today: spend.floor_today,
                floor_unknown_ends_today: spend.floor_unknown_ends_today,
            },
            agent_budget: row.budget,
            floor_usd_per_day: self.config.floor_usd_per_day,
            billing_ceiling_missing: self.conversation_ceiling_missing(agent),
            per_turn_floor: self.config.per_turn_floor,
            rung: row.rung,
            used: self
                .world
                .mail_turn_used(agent, candidate.thread, candidate.sender, day),
        })
    }

    /// The digest a refusal is kept under. Spend and turn-end facts are left out unless the
    /// refusal is a budget one, so a capped request is not decided again after every turn.
    fn mail_turn_digest(
        input: &MailTurnInput,
        decision: &MailTurnAdmission,
        day: UtcDay,
    ) -> Option<Hex32> {
        let mut seen = input.clone();
        if !matches!(decision, MailTurnAdmission::Issue { .. }) {
            seen.inspection_digest.clear();
        }
        match decision {
            MailTurnAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit,
            } => {
                seen.budget_state = BudgetState::default();
                seen.rung = BreakerRung::Healthy;
                seen.last_turn_end = None;
                seen.used = MailTurnUse {
                    thread: u32::from(seen.used.thread >= THREAD_TURN_CAP),
                    sender_today: u32::from(seen.used.sender_today >= SENDER_DAY_TURN_CAP),
                    agent_today: u32::from(seen.used.agent_today >= AGENT_DAY_TURN_CAP),
                };
            }
            MailTurnAdmission::Issue { .. } | MailTurnAdmission::Exhausted { .. } => {}
            MailTurnAdmission::Hold { .. } => {
                seen.budget_state = BudgetState::default();
                seen.rung = BreakerRung::Healthy;
            }
            MailTurnAdmission::Wait { .. } => {
                seen.budget_state = BudgetState::default();
                seen.rung = BreakerRung::Healthy;
                seen.last_turn_end = None;
            }
        }
        let bytes = serde_json::to_vec(&("mail_turn_v2", seen, day)).ok()?;
        Some(hex(&bytes))
    }

    /// Ask the mailbox which wake-worthy mail is in the agent's inbox, unless the same inputs were
    /// already handled today.
    pub(super) fn request_mail_turn(&mut self, agent: AgentId) {
        if self.mail_turn_inspections.contains_key(&agent) {
            return;
        }
        if !self.mail_turn_open(agent) {
            self.mail_turn_waiting.remove(&agent);
            return;
        }
        if self.world.agents.get(&agent).is_some_and(|row| {
            matches!(
                row.repositories,
                dudes_proto::repository::HireRepositories::Scoped(_)
            )
        }) && self.hold_for_issue_baseline(agent)
        {
            return;
        }
        if self.ledger.values().any(|parked| match &parked.then {
            Then::MailTurnIssue { agent: owner, .. } | Then::Spawn { agent: owner, .. } => {
                *owner == agent
            }
            _ => false,
        }) {
            return;
        }
        let candidates = self.world.mail_turn_candidates(agent);
        let Some(first) = candidates.first() else {
            self.mail_turn_waiting.remove(&agent);
            return;
        };
        if !self.spend.contains_key(&agent) {
            self.mail_turn_waiting.entry(agent).or_insert(0);
            return;
        }
        let day = self.clock.now().utc_day();
        let Some(input) =
            self.mail_turn_input(agent, first, first.mail.clone(), String::new(), day)
        else {
            return;
        };
        let Ok(key) = serde_json::to_string(&(
            "mail_turn_inputs_v1",
            &input,
            day,
            candidates
                .iter()
                .map(|candidate| (candidate.thread, &candidate.mail))
                .collect::<Vec<_>>(),
        )) else {
            return;
        };
        let key = hex(key.as_bytes()).to_hex();
        if self.mail_turn_handled.get(&agent) == Some(&key) {
            return;
        }
        let Some(next) = self.next_mail_request.checked_add(1) else {
            return;
        };
        self.next_mail_request = next;
        let request = MailWakeRequest(next);
        let mail: Vec<MailId> = candidates
            .iter()
            .flat_map(|candidate| candidate.mail.iter().map(|mail| mail.id))
            .collect();
        self.mail_turn_inspections
            .insert(agent, MailTurnInspection { request, key });
        self.pending.push(Effect::MailTurnInspect {
            agent,
            request,
            mail,
        });
    }

    /// Whether some agent's wake-worthy mail was not yet in its inbox at the last inspection.
    #[must_use]
    pub fn mail_turn_waiting(&self) -> bool {
        !self.mail_turn_waiting.is_empty()
    }

    /// The mailbox finished a pass, so mail that was journaled but not yet published may be there.
    pub(super) fn mail_pass_finished(&mut self) {
        let agents: Vec<AgentId> = self.mail_turn_waiting.keys().copied().collect();
        for agent in agents {
            self.request_mail_turn(agent);
        }
    }

    /// The agents whose candidates the beat should look at again.
    pub(super) fn mail_turn_sweep(&mut self) {
        let agents: Vec<AgentId> = self
            .world
            .agents
            .iter()
            .filter(|(_, row)| row.goal.is_none() && !row.retired)
            .map(|(agent, _)| *agent)
            .collect();
        for agent in agents {
            self.request_mail_turn(agent);
        }
    }

    pub(super) fn mail_turn_inspected(
        &mut self,
        agent: AgentId,
        request: MailWakeRequest,
        result: Result<(Timestamp, Vec<MailId>), MailWakeRefreshFailure>,
    ) {
        if self
            .mail_turn_inspections
            .get(&agent)
            .is_none_or(|pending| pending.request != request)
        {
            return;
        }
        let pending = self
            .mail_turn_inspections
            .remove(&agent)
            .expect("matching mail turn request");
        let Ok((now, mut present)) = result else {
            return;
        };
        if !self.mail_turn_open(agent) {
            return;
        }
        present.sort();
        present.dedup();
        let candidates = self.world.mail_turn_candidates(agent);
        let complete = candidates.iter().all(|other| {
            other
                .mail
                .iter()
                .all(|mail| present.binary_search(&mail.id).is_ok())
        });
        self.track_mail_turn_presence(agent, complete);
        let Ok(report) = serde_json::to_vec(&("mail_turn_inspection_v1", agent, &present)) else {
            return;
        };
        let inspection_digest = hex(&report);
        let day = now.utc_day();
        if complete {
            self.mail_turn_handled.insert(agent, pending.key);
        }
        for candidate in candidates {
            let covered: Vec<MailRef> = candidate
                .mail
                .iter()
                .filter(|mail| present.binary_search(&mail.id).is_ok())
                .copied()
                .collect();
            if covered.is_empty() {
                continue;
            }
            let Some(input) = self.mail_turn_input(
                agent,
                &candidate,
                covered.clone(),
                inspection_digest.to_hex(),
                day,
            ) else {
                return;
            };
            let Some(decided) = dudes_policy::admit_mail_turn(&input) else {
                continue;
            };
            let issue = matches!(decided.decision, MailTurnAdmission::Issue { .. });
            let Some(input_digest) = Self::mail_turn_digest(&input, &decided.decision, day) else {
                return;
            };
            let candidate_level = Self::candidate_level_refusal(&input, &decided.decision);
            if !issue
                && self
                    .world
                    .mail_turn_latest_for_thread(agent, candidate.thread)
                    .is_some_and(|latest| latest.decision.input_digest == input_digest)
            {
                if candidate_level {
                    continue;
                }
                return;
            }
            let decision = MailTurnDecision {
                agent,
                thread: candidate.thread,
                covered,
                inspection_digest,
                input_digest,
                gate_trace: decided.gate_trace,
                decision: decided.decision,
            };
            if !decision.valid() {
                return;
            }
            let then = if issue {
                Then::MailTurnIssue { agent, at: now }
            } else {
                Then::Fold
            };
            self.persist_at(now, Record::MailTurnDecided(decision), then);
            if issue || !candidate_level {
                return;
            }
        }
    }

    fn track_mail_turn_presence(&mut self, agent: AgentId, complete: bool) {
        if complete {
            self.mail_turn_waiting.remove(&agent);
            return;
        }
        let retries = self.mail_turn_waiting.get(&agent).copied().unwrap_or(0);
        if retries < MAIL_TURN_PASS_RETRIES {
            self.mail_turn_waiting.insert(agent, retries + 1);
        } else {
            self.mail_turn_waiting.remove(&agent);
        }
    }

    /// Whether a refusal concerns only this thread or sender, so the next thread may still be
    /// admitted.
    fn candidate_level_refusal(input: &MailTurnInput, decision: &MailTurnAdmission) -> bool {
        matches!(
            decision,
            MailTurnAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            }
        ) && (input.used.thread >= THREAD_TURN_CAP
            || input.used.sender_today >= SENDER_DAY_TURN_CAP)
    }

    /// Whether the agent's current inputs still admit exactly this issue, checked again right
    /// before it is written.
    pub(super) fn delayed_mail_turn_admitted(&self, issue: &MailTurnIssue, at: Timestamp) -> bool {
        let Some(candidate) = self
            .world
            .mail_turn_candidates(issue.agent)
            .into_iter()
            .find(|candidate| candidate.thread == issue.thread)
        else {
            return false;
        };
        if !issue
            .covered
            .iter()
            .all(|mail| candidate.mail.contains(mail))
        {
            return false;
        }
        let Some(latest) = self.world.mail_turn_latest(issue.agent) else {
            return false;
        };
        let Some(input) = self.mail_turn_input(
            issue.agent,
            &candidate,
            issue.covered.clone(),
            latest.decision.inspection_digest.to_hex(),
            at.utc_day(),
        ) else {
            return false;
        };
        latest.lsn == issue.lsn_of_decision
            && matches!(dudes_policy::admit_mail_turn(&input),
                Some(decided) if decided.decision == MailTurnAdmission::Issue { task: issue.task.clone() })
    }

    /// Issue the decided turn. When nothing is issued the decision is forgotten as handled, so it
    /// is made again once whatever blocked the issue clears.
    pub(super) fn issue_mail_turn(
        &mut self,
        agent: AgentId,
        decision_lsn: Lsn,
        at: Timestamp,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if !self.try_issue_mail_turn(agent, decision_lsn, at, operation) {
            self.mail_turn_handled.remove(&agent);
        }
    }

    fn try_issue_mail_turn(
        &mut self,
        agent: AgentId,
        decision_lsn: Lsn,
        at: Timestamp,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        if !self.mail_turn_open(agent)
            || self.issue_in_flight(agent)
            || self.launch_held(agent, operation)
        {
            return false;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return false;
        };
        if row.paused || row.halted {
            return false;
        }
        let Some(latest) = self.world.mail_turn_latest(agent).cloned() else {
            return false;
        };
        let MailTurnAdmission::Issue { task } = &latest.decision.decision else {
            return false;
        };
        let Some(line) = row.identity_line(agent) else {
            return false;
        };
        if latest.lsn != decision_lsn
            || MailTurnTask::new(
                &line,
                u8::try_from(latest.decision.covered.len()).unwrap_or(0),
            )
            .as_ref()
                != Some(task)
            || !self
                .world
                .mail_turn_candidates(agent)
                .iter()
                .any(|candidate| {
                    candidate.thread == latest.decision.thread
                        && latest
                            .decision
                            .covered
                            .iter()
                            .all(|mail| candidate.mail.contains(mail))
                })
        {
            return false;
        }
        let turn = TurnId::from_ulid(self.rng.ulid());
        let session = self.announce_session(agent, false);
        let Some(selection) = self.model_selection(&row) else {
            return false;
        };
        let Some(repositories) = self.issue_repositories(agent) else {
            return false;
        };
        let launch = match self.launch(
            agent,
            &row,
            Some(&repositories),
            task.as_str(),
            &session,
            &selection,
        ) {
            ManagedLaunch::Ready(launch) => launch,
            ManagedLaunch::Unavailable => return false,
            ManagedLaunch::Refused(error) => {
                self.hold_agent(
                    agent,
                    dudes_types::Provenance::Observed,
                    dudes_proto::AutomaticHold::LaunchRefused {
                        reason: error.to_string(),
                    },
                );
                return false;
            }
        };
        let issue = MailTurnIssue {
            agent,
            turn,
            thread: latest.decision.thread,
            covered: latest.decision.covered.clone(),
            task: task.clone(),
            session: session.id().cloned(),
            model_selection: selection,
            lsn_of_decision: decision_lsn,
        };
        if !issue.valid() {
            return false;
        }
        let record = launch
            .repositories
            .record(dudes_proto::issue::ManagedIssuePayload::MailTurn(issue));
        self.persist_at(
            at,
            record,
            Then::Spawn {
                agent,
                turn,
                launch,
            },
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Input;
    use crate::input::SpendView;
    use crate::testing::{Harness, Seen};
    use dudes_proto::record::Hex32;
    use dudes_proto::{Entry, RecordKind};
    use dudes_types::ThreadId;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    struct Flow {
        h: Harness,
        a: AgentId,
        b: AgentId,
        next: u128,
    }

    impl Flow {
        fn new() -> Self {
            let mut h = Harness::new();
            let a = h.hire();
            let b = h.hire();
            let mut flow = Self { h, a, b, next: 1 };
            flow.spend();
            flow
        }

        fn spend(&mut self) {
            for agent in [self.a, self.b] {
                self.h.step(Input::Spend {
                    agent,
                    view: SpendView::default(),
                });
            }
        }

        fn mail(
            &mut self,
            from: &serde_json::Value,
            act: &str,
            members: &[AgentId],
        ) -> (MailId, Vec<Seen>) {
            let id = MailId::from_parts(5, self.next);
            self.next += 1;
            let record: Record = serde_json::from_value(serde_json::json!({
                "t": "mail", "id": id, "from": from,
                "to": {"kind": "agent", "agent": self.b}, "act": act, "subject": "s",
                "body_len": 0, "state": {"state": "durable"},
                "thread": ThreadId::from_parts(5, 100 + self.next), "thread_seq": 1,
                "members": members,
            }))
            .unwrap();
            let effects = self.h.journaled(Entry { at: at(10), record });
            (id, effects)
        }

        fn agent_mail(&mut self, act: &str, members: &[AgentId]) -> (MailId, Vec<Seen>) {
            let from = serde_json::json!({"kind": "agent", "agent": self.a});
            self.mail(&from, act, members)
        }
    }

    fn inspects(effects: &[Seen]) -> Vec<(crate::input::MailWakeRequest, Vec<MailId>)> {
        effects
            .iter()
            .filter_map(|seen| match seen {
                Seen::MailTurnInspect { request, mail, .. } => Some((*request, mail.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn only_a_direct_request_from_an_agent_asks_for_an_inspection() {
        let mut f = Flow::new();
        let (a, b) = (f.a, f.b);
        let operator = serde_json::json!({"kind": "human"});
        let (_, effects) = f.mail(&operator, "request", &[a, b]);
        assert!(inspects(&effects).is_empty(), "operator mail");
        let c = AgentId::from_parts(1, 77);
        let (_, effects) = f.agent_mail("request", &[a, b, c]);
        assert!(inspects(&effects).is_empty(), "a group thread");
        let (_, effects) = f.agent_mail("message", &[a, b]);
        assert!(inspects(&effects).is_empty(), "a plain message");
        let (_, effects) = f.agent_mail("card", &[a, b]);
        assert!(inspects(&effects).is_empty(), "a card");
        let (direct, effects) = f.agent_mail("request", &[a, b]);
        assert_eq!(
            inspects(&effects)
                .into_iter()
                .map(|(_, mail)| mail)
                .collect::<Vec<_>>(),
            vec![vec![direct]]
        );
    }

    #[test]
    fn a_decision_without_an_issue_is_decided_again_when_the_daemon_starts() {
        let mut f = Flow::new();
        let (a, b) = (f.a, f.b);
        f.h.unfsynced.insert(RecordKind::MailTurnIssued);
        let (mail, effects) = f.agent_mail("request", &[a, b]);
        let mut found = inspects(&effects);
        assert_eq!(found.len(), 1);
        let (request, _) = found.remove(0);
        let effects = f.h.step(Input::MailTurnInspected {
            agent: b,
            request,
            result: Ok((at(20), vec![mail])),
        });
        let token = effects
            .iter()
            .find_map(|seen| match seen {
                Seen::Persist {
                    token,
                    entry:
                        Entry {
                            record: Record::MailTurnDecided(_),
                            ..
                        },
                } => Some(*token),
                _ => None,
            })
            .expect("a decision");
        f.h.acknowledge(token);
        let decided = |h: &Harness| {
            h.journal
                .iter()
                .filter(|entry| {
                    matches!(&entry.record, Record::MailTurnDecided(d)
                    if matches!(d.decision, dudes_proto::MailTurnAdmission::Issue { .. }))
                })
                .count()
        };
        assert_eq!(decided(&f.h), 1);
        assert!(
            !f.h.journal
                .iter()
                .any(|entry| matches!(entry.record, Record::MailTurnIssued(_)))
        );

        f.h.restart();
        f.spend();
        let effects = f.h.step(Input::MailPassFinished);
        let mut found = inspects(&effects);
        assert_eq!(found.len(), 1, "the decision is made again: {effects:?}");
        let (request, found) = found.remove(0);
        assert_eq!(found, vec![mail]);
        let effects = f.h.step(Input::MailTurnInspected {
            agent: b,
            request,
            result: Ok((at(30), vec![mail])),
        });
        assert!(effects.iter().any(|seen| matches!(seen, Seen::Persist {
            entry: Entry { record: Record::MailTurnDecided(d), .. }, ..
        } if matches!(d.decision, dudes_proto::MailTurnAdmission::Issue { .. }))));
    }

    #[test]
    fn covered_mail_stays_covered_when_the_agent_later_gets_a_goal() {
        let mut f = Flow::new();
        let (a, b) = (f.a, f.b);
        let (mail, effects) = f.agent_mail("request", &[a, b]);
        let mut found = inspects(&effects);
        let (request, _) = found.remove(0);
        let effects = f.h.step(Input::MailTurnInspected {
            agent: b,
            request,
            result: Ok((at(20), vec![mail])),
        });
        let token = effects
            .iter()
            .find_map(|seen| match seen {
                Seen::Persist {
                    token,
                    entry:
                        Entry {
                            record: Record::MailTurnDecided(_),
                            ..
                        },
                } => Some(*token),
                _ => None,
            })
            .expect("a decision");
        f.h.acknowledge(token);
        assert!(
            f.h.journal
                .iter()
                .any(|entry| matches!(entry.record, Record::MailTurnIssued(_)))
        );
        let world = f.h.supervisor.world();
        assert!(world.mail_turn_covered(mail));
        assert!(world.mail_turn_candidates(b).is_empty());
        let turn = world.agents[&b].open_turn.expect("the mail turn is open");
        assert!(world.turns[&turn].goal.is_none());

        f.h.set_goal(b, &["true"]);
        let world = f.h.supervisor.world();
        assert!(world.mail_turn_covered(mail));
        assert!(world.mail_turn_candidates(b).is_empty());
    }

    #[test]
    fn a_chat_note_between_the_decision_and_the_issue_delays_the_turn_until_the_chat_turn_ends() {
        let mut f = Flow::new();
        let (a, b) = (f.a, f.b);
        let (mail, effects) = f.agent_mail("request", &[a, b]);
        let (first_request, _) = inspects(&effects).remove(0);
        let effects = f.h.step(Input::MailTurnInspected {
            agent: b,
            request: first_request,
            result: Ok((at(20), vec![mail])),
        });
        let token = effects
            .iter()
            .find_map(|seen| match seen {
                Seen::Persist {
                    token,
                    entry:
                        Entry {
                            record: Record::MailTurnDecided(_),
                            ..
                        },
                } => Some(*token),
                _ => None,
            })
            .expect("a decision");
        let context_lsn = f.h.supervisor.world().agents[&b].context_lsn;
        f.h.drive(Input::Command(crate::Command::ConversationMessageQueued {
            agent: b,
            text: "look at this first".into(),
            browser: dudes_proto::BrowserNoteAttribution {
                request_id_digest: Hex32::new([1; 32]),
                request_digest: Hex32::new([2; 32]),
                session_digest: Hex32::new([3; 32]),
            },
            context_lsn,
            context_reset_turn: None,
        }));
        let chat = f.h.open_turn(b).expect("the chat message issued a turn");
        f.h.acknowledge(token);
        assert!(
            !f.h.journal
                .iter()
                .any(|entry| matches!(entry.record, Record::MailTurnIssued(_))),
            "the chat turn holds the mail turn back"
        );
        assert!(
            !f.h.supervisor.mail_turn_handled.contains_key(&b),
            "an issue that did not happen leaves the decision to be made again"
        );
        f.h.drive(Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            turn: chat,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        });
        let effects: Vec<Seen> = f.h.trace.iter().rev().take(40).cloned().collect();
        let mut found = inspects(&effects);
        found.retain(|(request, _)| *request != first_request);
        assert_eq!(
            found.len(),
            1,
            "decided again with no new mail: {effects:?}"
        );
        let (request, covered) = found.remove(0);
        assert_eq!(covered, vec![mail]);
        let effects = f.h.step(Input::MailTurnInspected {
            agent: b,
            request,
            result: Ok((at(40), vec![mail])),
        });
        let token = effects
            .iter()
            .find_map(|seen| match seen {
                Seen::Persist {
                    token,
                    entry:
                        Entry {
                            record: Record::MailTurnDecided(_),
                            ..
                        },
                } => Some(*token),
                _ => None,
            })
            .expect("a second decision");
        f.h.acknowledge(token);
        assert!(
            f.h.journal
                .iter()
                .any(|entry| matches!(entry.record, Record::MailTurnIssued(_)))
        );
    }
}
