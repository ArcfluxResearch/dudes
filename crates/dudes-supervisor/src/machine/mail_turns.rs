//! Mail turns: a turn for a goal-free agent that wake-worthy mail from one teammate started.

use super::{
    AgentId, Clock, Effect, Lsn, ManagedLaunch, Record, Rng, Supervisor, Then, Timestamp, TurnId,
};
use crate::input::{MailWakeRefreshFailure, MailWakeRequest};
use crate::world::MailTurnCandidate;
use dudes_policy::{BudgetState, Control, MailTurnInput};
use dudes_proto::record::Hex32;
use dudes_proto::{
    BrowserNoteDestination, MailRef, MailTurnAdmission, MailTurnDecision, MailTurnIssue,
    MailTurnTask,
};
use dudes_types::{MailId, UtcDay};

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

    fn mail_turn_digest(input: &MailTurnInput, day: UtcDay) -> Option<Hex32> {
        let bytes = serde_json::to_vec(&("mail_turn_v1", input, day)).ok()?;
        Some(hex(&bytes))
    }

    /// Ask the mailbox which wake-worthy mail is in the agent's inbox, unless the same inputs were
    /// already handled today.
    pub(super) fn request_mail_turn(&mut self, agent: AgentId) {
        if self.mail_turn_inspections.contains_key(&agent) || !self.mail_turn_open(agent) {
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
            return;
        };
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
        let Some((candidate, covered)) = self
            .world
            .mail_turn_candidates(agent)
            .into_iter()
            .find_map(|candidate| {
                let covered: Vec<MailRef> = candidate
                    .mail
                    .iter()
                    .filter(|mail| present.binary_search(&mail.id).is_ok())
                    .copied()
                    .collect();
                (!covered.is_empty()).then_some((candidate, covered))
            })
        else {
            return;
        };
        let complete = self
            .world
            .mail_turn_candidates(agent)
            .iter()
            .all(|other| {
                other
                    .mail
                    .iter()
                    .all(|mail| present.binary_search(&mail.id).is_ok())
            });
        let Ok(report) = serde_json::to_vec(&("mail_turn_inspection_v1", agent, &present)) else {
            return;
        };
        let inspection_digest = hex(&report);
        let day = now.utc_day();
        let Some(input) = self.mail_turn_input(
            agent,
            &candidate,
            covered.clone(),
            inspection_digest.to_hex(),
            day,
        ) else {
            return;
        };
        let Some(input_digest) = Self::mail_turn_digest(&input, day) else {
            return;
        };
        let Some(decided) = dudes_policy::admit_mail_turn(&input) else {
            return;
        };
        if complete {
            self.mail_turn_handled.insert(agent, pending.key);
        }
        let issue = matches!(decided.decision, MailTurnAdmission::Issue { .. });
        if !issue
            && self
                .world
                .mail_turn_latest(agent)
                .is_some_and(|latest| latest.decision.input_digest == input_digest)
        {
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

    pub(super) fn issue_mail_turn(
        &mut self,
        agent: AgentId,
        decision_lsn: Lsn,
        at: Timestamp,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if !self.mail_turn_open(agent) || self.launch_held(agent, operation) {
            return;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if row.paused || row.halted {
            return;
        }
        let Some(latest) = self.world.mail_turn_latest(agent).cloned() else {
            return;
        };
        let MailTurnAdmission::Issue { task } = &latest.decision.decision else {
            return;
        };
        let Some(line) = row.identity_line(agent) else {
            return;
        };
        if latest.lsn != decision_lsn
            || MailTurnTask::new(&line, u8::try_from(latest.decision.covered.len()).unwrap_or(0))
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
            return;
        }
        let turn = TurnId::from_ulid(self.rng.ulid());
        let session = self.announce_session(agent, false);
        let Some(selection) = self.model_selection(&row) else {
            return;
        };
        let Some(repositories) = self.issue_repositories(agent) else {
            return;
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
            ManagedLaunch::Unavailable => return,
            ManagedLaunch::Refused(error) => {
                self.hold_agent(
                    agent,
                    dudes_types::Provenance::Observed,
                    dudes_proto::AutomaticHold::LaunchRefused {
                        reason: error.to_string(),
                    },
                );
                return;
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
            return;
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
    }
}
