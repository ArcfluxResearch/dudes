use super::{
    AgentId, BrowserNoteCancelDisposition, Clock, Effect, GoalId, LaunchSpec, LoopDecision, Lsn,
    ManagedInstall, Record, Rng, Supervisor, Then, TurnId, budget_refusal_values,
};

use crate::world::BrowserNoteState;
use dudes_policy::{
    BrowserNoteInput, BudgetState, Ceilings, CompletedNoteInput, CompletedNoteState, Control,
};
use dudes_proto::{BrowserNoteAttribution, BrowserNoteDestination, ChatReason, NoteAdmission};

pub(super) enum BrowserLaunchPhase {
    Preparing {
        generation: u64,
        prior_intent: Option<Lsn>,
        install: ManagedInstall,
        mcp_grant: Option<Lsn>,
    },
    IntentPending {
        generation: u64,
        prior_intent: Option<Lsn>,
        spec: Box<LaunchSpec>,
        install: ManagedInstall,
        mcp_grant: Option<Lsn>,
    },
    DispatchReady {
        generation: u64,
        prior_intent: Option<Lsn>,
    },
    DispatchClaimed {
        prior_intent: Option<Lsn>,
    },
    ReplayRefused {
        prior_intent: Lsn,
    },
    CancelPending {
        previous: Option<Box<BrowserLaunchPhase>>,
    },
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// An ungranted browser attempt must settle before the first desired-policy edit.
    #[must_use]
    pub fn has_ungranted_browser_launch(&self, agent: AgentId) -> bool {
        let historical = self
            .world
            .issued_not_spawned()
            .filter(|(turn, _)| {
                self.world
                    .browser_issue(*turn)
                    .is_some_and(|issue| !issue.dispatch_intents.is_empty())
            })
            .map(|(turn, _)| turn);
        self.browser_launches
            .keys()
            .copied()
            .chain(historical)
            .any(|turn| {
                let Some(issue) = self.world.browser_issue(turn) else {
                    return false;
                };
                if issue.agent != agent || issue.cancellation.is_some() || issue.end_lsn.is_some() {
                    return false;
                }
                let Some(attempt) = self.mcp_attempts.get(&turn) else {
                    return true;
                };
                if attempt.grant.is_some() {
                    return false;
                }
                let Some(key) = &attempt.key else {
                    return true;
                };
                self.world.turns.get(&turn).is_none_or(|row| {
                    row.pid != Some(key.pid)
                        || row.start_time != Some(key.start_time)
                        || row.boot != key.boot
                        || row.spawn_lsn.is_none()
                })
            })
    }

    /// Whether browser-note delivery is still open: true until the daemon begins to stop.
    #[must_use]
    pub const fn browser_delivery_open(&self) -> bool {
        self.browser_delivery_open
    }

    /// Stop new browser-note delivery for the rest of the run, as the daemon begins to stop.
    pub fn close_browser_delivery(&mut self) {
        self.browser_delivery_open = false;
    }

    /// Classify one cancellation against all historical dispatch attempts and the live claim.
    #[must_use]
    pub fn browser_note_cancel_disposition(
        &self,
        agent: AgentId,
        note_lsn: Lsn,
    ) -> BrowserNoteCancelDisposition {
        let refused = |reason| BrowserNoteCancelDisposition::Refused { reason };
        let Some(note) = self
            .world
            .browser_note(note_lsn)
            .filter(|note| note.agent == agent)
        else {
            return refused(ChatReason::NoteUnknown);
        };
        if self.browser_context.get(&agent) == Some(&note_lsn) {
            return refused(ChatReason::DeliveryStarted);
        }
        match note.state {
            BrowserNoteState::Queued => BrowserNoteCancelDisposition::Queued,
            BrowserNoteState::Reserved { turn } => {
                if matches!(
                    note.destination,
                    BrowserNoteDestination::ManagedConversation { .. }
                ) {
                    return refused(ChatReason::DeliveryStarted);
                }
                if matches!(
                    self.browser_launches.get(&turn),
                    Some(BrowserLaunchPhase::DispatchClaimed { .. })
                ) {
                    return refused(ChatReason::DeliveryStarted);
                }
                if matches!(
                    self.browser_launches.get(&turn),
                    Some(BrowserLaunchPhase::CancelPending { .. })
                ) {
                    return refused(ChatReason::NoteCancelled);
                }
                let Some(issue) = self.world.browser_issue(turn) else {
                    return refused(ChatReason::NoteUnknown);
                };
                if issue.cancellation.is_some() {
                    return refused(ChatReason::NoteCancelled);
                }
                if issue.spawn_lsn.is_some() || issue.end_lsn.is_some() {
                    return refused(ChatReason::DeliveryStarted);
                }
                let prior = self.prior_browser_dispatch(turn);
                match prior {
                    Some(dispatch_intent_lsn) => BrowserNoteCancelDisposition::AmbiguousReissue {
                        turn,
                        dispatch_intent_lsn,
                    },
                    None => BrowserNoteCancelDisposition::CleanIssue { turn },
                }
            }
            BrowserNoteState::Cancelled | BrowserNoteState::ReissueCancelled { .. } => {
                refused(ChatReason::NoteCancelled)
            }
            _ => refused(ChatReason::DeliveryStarted),
        }
    }

    pub(super) fn cancel_browser_note(
        &mut self,
        agent: AgentId,
        note_lsn: Lsn,
        browser: BrowserNoteAttribution,
    ) {
        let record = match self.browser_note_cancel_disposition(agent, note_lsn) {
            BrowserNoteCancelDisposition::Queued => Record::BrowserNoteCancelled {
                agent,
                note_lsn,
                browser,
            },
            BrowserNoteCancelDisposition::CleanIssue { turn } => {
                self.claim_browser_cancel(turn);
                Record::BrowserNoteIssuedCancelled {
                    agent,
                    note_lsn,
                    turn,
                    browser,
                }
            }
            BrowserNoteCancelDisposition::AmbiguousReissue {
                turn,
                dispatch_intent_lsn,
            } => {
                self.claim_browser_cancel(turn);
                Record::BrowserNoteReissueCancelled {
                    agent,
                    note_lsn,
                    turn,
                    dispatch_intent_lsn,
                    browser,
                }
            }
            BrowserNoteCancelDisposition::Refused { .. } => return,
        };
        self.persist(record, Then::Fold);
    }

    pub(super) fn browser_launch_allowed(
        &self,
        agent: AgentId,
        turn: TurnId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        if self
            .world
            .browser_issue(turn)
            .is_some_and(|issue| issue.goal.is_none())
            && self.conversation_ceiling_missing(agent)
        {
            return false;
        }
        self.browser_delivery_open
            && !self.world.browser_issue_cancelled(turn)
            && !matches!(
                self.browser_launches.get(&turn),
                Some(
                    BrowserLaunchPhase::CancelPending { .. }
                        | BrowserLaunchPhase::ReplayRefused { .. }
                )
            )
            && !self.launch_held(agent, operation)
            && self
                .world
                .turns
                .get(&turn)
                .is_some_and(|row| !row.has_ended())
            && self
                .world
                .agents
                .get(&agent)
                .is_some_and(|row| !row.retired && !row.halted && !row.paused && !row.attached)
    }

    pub(super) fn prepare_browser_launch(
        &mut self,
        turn: TurnId,
        spec: LaunchSpec,
        install: ManagedInstall,
        mcp_grant: Option<Lsn>,
    ) {
        let Some(generation) = self.browser_generation.checked_add(1) else {
            return;
        };
        self.browser_generation = generation;
        let prior_intent = self
            .world
            .browser_issue(turn)
            .and_then(|issue| issue.dispatch_intents.last().copied());
        self.browser_launches.insert(
            turn,
            BrowserLaunchPhase::Preparing {
                generation,
                prior_intent,
                install,
                mcp_grant,
            },
        );
        self.pending.push(Effect::PrepareBrowserLaunch {
            turn,
            generation,
            spec,
        });
    }

    pub(super) fn on_browser_launch_ready(
        &mut self,
        turn: TurnId,
        generation: u64,
        prepared: Result<Box<LaunchSpec>, String>,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        let Some(row) = self.world.turns.get(&turn) else {
            return;
        };
        let agent = row.agent;
        if !self.browser_launch_allowed(agent, turn, operation) {
            if !self.browser_turn_cancelled(turn)
                && !matches!(
                    self.browser_launches.get(&turn),
                    Some(BrowserLaunchPhase::ReplayRefused { .. })
                )
            {
                self.spawning.remove(&turn);
                self.reissued.remove(&turn);
                self.browser_launches.remove(&turn);
            }
            return;
        }
        if !matches!(self.browser_launches.get(&turn), Some(BrowserLaunchPhase::Preparing { generation: current, .. }) if *current == generation)
        {
            return;
        }
        let spec = match prepared {
            Ok(spec)
                if spec
                    .argv
                    .first()
                    .is_some_and(|program| std::path::Path::new(program).is_absolute()) =>
            {
                spec
            }
            Ok(_) => {
                self.on_spawn_failed(
                    turn,
                    "managed program preparation did not resolve an absolute program".to_owned(),
                );
                return;
            }
            Err(reason) => {
                self.on_spawn_failed(turn, reason);
                return;
            }
        };
        let Some(BrowserLaunchPhase::Preparing {
            install,
            prior_intent,
            mcp_grant,
            ..
        }) = self.browser_launches.remove(&turn)
        else {
            return;
        };
        self.browser_launches.insert(
            turn,
            BrowserLaunchPhase::IntentPending {
                generation,
                prior_intent,
                spec,
                install,
                mcp_grant,
            },
        );
        if let Some(dispatch_intent_lsn) = prior_intent
            && self.world.browser_issue(turn).is_some_and(|issue| {
                issue.goal.is_none() && !issue.uncertain_dispatches.contains(&dispatch_intent_lsn)
            })
        {
            self.persist(
                Record::ConversationDispatchUncertain {
                    agent,
                    turn,
                    dispatch_intent_lsn,
                    charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
                },
                Then::ConversationRetryReserved { turn, generation },
            );
        } else {
            self.record_browser_dispatch(turn, generation, operation);
        }
    }

    pub(super) fn record_browser_dispatch(
        &mut self,
        turn: TurnId,
        generation: u64,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        let Some(issue) = self.world.browser_issue(turn) else {
            return;
        };
        let agent = issue.agent;
        let note_lsn = issue.note_lsn;
        if !self.browser_launch_allowed(agent, turn, operation) {
            if !self.browser_turn_cancelled(turn) {
                self.browser_launches.remove(&turn);
                self.spawning.remove(&turn);
                self.reissued.remove(&turn);
            }
            return;
        }
        if !matches!(self.browser_launches.get(&turn), Some(BrowserLaunchPhase::IntentPending { generation: current, .. }) if *current == generation)
        {
            return;
        }
        self.persist(
            Record::BrowserNoteDispatchIntent {
                agent,
                note_lsn,
                turn,
            },
            Then::BrowserDispatch { turn, generation },
        );
    }

    /// The exact launch authorized by a still-parked, not-appended dispatch intent.
    #[must_use]
    pub fn browser_dispatch_spec(
        &self,
        token: crate::PersistToken,
        turn: TurnId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<&LaunchSpec> {
        let parked = self.ledger.get(&token)?;
        let Then::BrowserDispatch {
            turn: issued,
            generation,
        } = parked.then
        else {
            return None;
        };
        if issued != turn
            || !matches!(&parked.entry.record, Record::BrowserNoteDispatchIntent { turn: issued, .. } if *issued == turn)
        {
            return None;
        }
        let BrowserLaunchPhase::IntentPending {
            generation: prepared,
            spec,
            ..
        } = self.browser_launches.get(&turn)?
        else {
            return None;
        };
        let agent = self.world.turns.get(&turn)?.agent;
        (generation == *prepared && self.browser_launch_allowed(agent, turn, operation))
            .then_some(spec.as_ref())
    }

    pub(super) fn retry_browser_dispatch_admission(
        &mut self,
        token: crate::PersistToken,
        turn: TurnId,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if self.browser_dispatch_spec(token, turn, operation).is_some() {
            let entry = self.ledger[&token].entry.clone();
            self.pending.push(Effect::Persist { token, entry });
        }
    }

    pub(super) fn dispatch_browser_launch(
        &mut self,
        turn: TurnId,
        generation: u64,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        let Some(agent) = self.world.turns.get(&turn).map(|row| row.agent) else {
            return;
        };
        if !self.browser_launch_allowed(agent, turn, operation) {
            if !self.browser_turn_cancelled(turn)
                && !matches!(
                    self.browser_launches.get(&turn),
                    Some(BrowserLaunchPhase::ReplayRefused { .. })
                )
            {
                self.browser_launches.remove(&turn);
                self.spawning.remove(&turn);
                self.reissued.remove(&turn);
            }
            return;
        }
        if !matches!(self.browser_launches.get(&turn), Some(BrowserLaunchPhase::IntentPending { generation: current, .. }) if *current == generation)
        {
            return;
        }
        let Some(BrowserLaunchPhase::IntentPending {
            generation,
            prior_intent,
            spec,
            install,
            mcp_grant,
        }) = self.browser_launches.remove(&turn)
        else {
            return;
        };
        self.browser_launches.insert(
            turn,
            BrowserLaunchPhase::DispatchReady {
                generation,
                prior_intent,
            },
        );
        self.emit_spawn(turn, *spec, install, mcp_grant);
    }

    /// Claim actual host entry on the serialized executor, immediately before its host call.
    /// Ordinary turns retain their existing launch route.
    #[must_use]
    pub fn claim_browser_dispatch(
        &mut self,
        turn: TurnId,
        generation: Option<u64>,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        let Some(issue) = self.world.browser_issue(turn) else {
            return true;
        };
        if !self.browser_launch_allowed(issue.agent, turn, operation) {
            return false;
        }
        if !matches!(self.browser_launches.get(&turn), Some(BrowserLaunchPhase::DispatchReady { generation: current, .. }) if Some(*current) == generation)
        {
            return false;
        }
        let Some(BrowserLaunchPhase::DispatchReady { prior_intent, .. }) =
            self.browser_launches.remove(&turn)
        else {
            return false;
        };
        self.browser_launches
            .insert(turn, BrowserLaunchPhase::DispatchClaimed { prior_intent });
        true
    }

    /// Whether an unresolved dispatch predates this run's current preparation.
    #[must_use]
    pub fn browser_delivery_uncertain(&self, turn: TurnId) -> bool {
        self.prior_browser_dispatch(turn).is_some()
    }

    fn prior_browser_dispatch(&self, turn: TurnId) -> Option<Lsn> {
        match self.browser_launches.get(&turn) {
            Some(
                BrowserLaunchPhase::Preparing { prior_intent, .. }
                | BrowserLaunchPhase::IntentPending { prior_intent, .. }
                | BrowserLaunchPhase::DispatchReady { prior_intent, .. }
                | BrowserLaunchPhase::DispatchClaimed { prior_intent },
            ) => *prior_intent,
            Some(BrowserLaunchPhase::ReplayRefused { prior_intent }) => Some(*prior_intent),
            _ => self
                .world
                .browser_issue(turn)
                .filter(|issue| issue.spawn_lsn.is_none() && issue.end_lsn.is_none())
                .and_then(|issue| issue.dispatch_intents.last().copied()),
        }
    }

    pub(super) fn retain_failed_browser_replay(&mut self, turn: TurnId) -> bool {
        let Some(prior_intent) = self.prior_browser_dispatch(turn) else {
            return false;
        };
        self.browser_launches
            .insert(turn, BrowserLaunchPhase::ReplayRefused { prior_intent });
        self.spawning.remove(&turn);
        self.reissued.remove(&turn);
        self.reissue_confirmations.remove(&turn);
        tracing::warn!(%turn, %prior_intent, "a failed replay cannot settle an earlier browser dispatch");
        true
    }

    pub(super) fn retry_refused_browser_replays(
        &mut self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if !self.browser_delivery_open
            || self.launch_held(agent, operation)
            || self
                .world
                .agents
                .get(&agent)
                .is_none_or(|row| row.retired || row.halted || row.paused || row.attached)
        {
            return;
        }
        let refused: Vec<_> = self
            .browser_launches
            .iter()
            .filter_map(|(turn, phase)| {
                (matches!(phase, BrowserLaunchPhase::ReplayRefused { .. })
                    && self
                        .world
                        .turns
                        .get(turn)
                        .is_some_and(|row| row.agent == agent))
                .then_some(*turn)
            })
            .collect();
        for turn in refused {
            self.browser_launches.remove(&turn);
        }
    }

    pub(super) fn eligible_ordinary_note(&self, agent: AgentId) -> Option<BrowserNoteInput> {
        if !self.browser_delivery_open || self.closing_active || self.closing_recovery {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        if row.attached || row.retired {
            return None;
        }
        let goal = row.goal?;
        let (lsn, note) = self
            .world
            .eligible_browser_note(agent, &BrowserNoteDestination::ManagedGoal { goal })?;
        BrowserNoteInput::new(lsn, goal, note.text.clone())
    }

    pub(super) fn completed_note_input_for(&self, agent: AgentId) -> Option<CompletedNoteInput> {
        if !self.browser_delivery_open || self.closing_active || self.closing_recovery {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        if row.retired || row.attached {
            return None;
        }
        let (goal_id, completion_lsn) = self.world.completed_goal(agent)?;
        let goal = self.world.goals.get(&goal_id)?;
        let note = self.eligible_ordinary_note(agent)?;
        let spend = self.spend.get(&agent).copied().unwrap_or_default();
        CompletedNoteInput::new(
            goal_id,
            completion_lsn,
            note,
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
        )
    }

    pub(super) fn decide_completed_note(&mut self, agent: AgentId) {
        let Some(input) = self.completed_note_input_for(agent) else {
            return;
        };
        let input_digest = match crate::host::taught_note_input_digest(&input) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(%agent,%error,"taught_note_digest_refused");
                return;
            }
        };
        let decided = dudes_policy::admit_completed_note(&input);
        if !dudes_proto::valid_taught_decision(&decided.decision, &decided.gate_trace) {
            tracing::error!(%agent,"taught_note_decision_refused");
            return;
        }
        let refusal = budget_refusal_values(
            agent,
            Some(input.goal()),
            &input.state.budget_state,
            &input.state.ceilings,
            input.state.per_turn_floor,
            &decided.gate_trace,
        );
        let then = match &decided.decision {
            NoteAdmission::Issue { task } => Then::BrowserIssue {
                agent,
                task: task.clone(),
                answers: Vec::new(),
                note_lsn: input.note_lsn(),
                completion_lsn: Some(input.completion_lsn()),
            },
            _ => refusal.map_or(Then::Fold, |audit| Then::TaughtBudgetRefused {
                audit: Box::new(audit),
            }),
        };
        self.persist(
            Record::TaughtNoteTurnDecided {
                agent,
                goal: input.goal(),
                note_lsn: input.note_lsn(),
                completion_lsn: input.completion_lsn(),
                input_digest,
                gate_trace: decided.gate_trace,
                decision: decided.decision,
            },
            then,
        );
    }

    pub(super) fn answer_saved(
        &mut self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if self.world.completed_goal(agent).is_none() {
            self.decide(agent, operation);
        }
    }

    pub(super) fn completed_command_ended(
        &mut self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if !self.protect_failures(agent) {
            self.decide(agent, operation);
        }
    }

    pub(super) fn after_turn_end(
        &self,
        turn: TurnId,
        agent: AgentId,
        goal: Option<GoalId>,
    ) -> Then {
        if self
            .world
            .browser_issue(turn)
            .is_some_and(|issue| issue.note_only)
            || self.world.mail_wake_origin(turn)
        {
            Then::CompletedCommandEnded { agent }
        } else {
            goal.map_or(Then::NoteEnded { agent }, |goal| Then::Judge {
                agent,
                goal,
            })
        }
    }

    pub(super) fn browser_turn_cancelled(&self, turn: TurnId) -> bool {
        self.world.browser_issue_cancelled(turn)
            || matches!(
                self.browser_launches.get(&turn),
                Some(BrowserLaunchPhase::CancelPending { .. })
            )
    }

    /// Fixed current admission refusal for a pending browser note.
    #[must_use]
    pub fn browser_note_admission_reason(&self, agent: AgentId) -> Option<ChatReason> {
        if let Some(input) = self.completed_note_input_for(agent) {
            return match dudes_policy::admit_completed_note(&input).decision {
                NoteAdmission::Issue { .. } => None,
                NoteAdmission::Wait { .. } => Some(ChatReason::OpenQuestion),
                NoteAdmission::Hold { reason } => Some(hold_reason(reason)),
                NoteAdmission::Exhausted { reason } => Some(exhausted_reason(reason)),
            };
        }
        if let Some(input) = self.conversation_input_for(agent) {
            return match dudes_policy::admit_conversation(&input).decision {
                NoteAdmission::Issue { .. } => None,
                NoteAdmission::Wait { .. } => Some(ChatReason::OpenQuestion),
                NoteAdmission::Hold { reason } => Some(hold_reason(reason)),
                NoteAdmission::Exhausted { reason } => Some(exhausted_reason(reason)),
            };
        }
        self.loop_input_for(agent)
            .and_then(|input| match dudes_policy::decide(&input).decision {
                LoopDecision::Continue { .. } | LoopDecision::Done => None,
                LoopDecision::Wait { .. } => Some(ChatReason::OpenQuestion),
                LoopDecision::Hold { reason } => Some(hold_reason(reason)),
                LoopDecision::Exhausted { reason } => Some(exhausted_reason(reason)),
            })
    }

    fn claim_browser_cancel(&mut self, turn: TurnId) {
        let previous = self.browser_launches.remove(&turn).map(Box::new);
        self.browser_launches
            .insert(turn, BrowserLaunchPhase::CancelPending { previous });
    }

    /// Release only an attributed append that the runtime positively refused before writing.
    pub fn browser_append_refused(&mut self, request_id: dudes_proto::record::Hex32) {
        let tokens: Vec<_> = self
            .ledger
            .iter()
            .filter_map(|(token, parked)| {
                let (Record::ConversationContextResetQueued { browser, .. }
                | Record::ConversationMessageQueued { browser, .. }
                | Record::BrowserNoteQueued { browser, .. }
                | Record::BrowserNoteCancelled { browser, .. }
                | Record::BrowserNoteIssuedCancelled { browser, .. }
                | Record::BrowserNoteReissueCancelled { browser, .. }) = &parked.entry.record
                else {
                    return None;
                };
                (browser.request_id_digest == request_id).then_some(*token)
            })
            .collect();
        for token in tokens {
            let Some(parked) = self.ledger.remove(&token) else {
                continue;
            };
            let (Record::BrowserNoteIssuedCancelled { turn, .. }
            | Record::BrowserNoteReissueCancelled { turn, .. }) = parked.entry.record
            else {
                continue;
            };
            if matches!(
                self.browser_launches.get(&turn),
                Some(BrowserLaunchPhase::CancelPending { .. })
            ) && let Some(BrowserLaunchPhase::CancelPending { previous }) =
                self.browser_launches.remove(&turn)
                && let Some(previous) = previous
            {
                self.browser_launches.insert(turn, *previous);
            }
        }
    }
}

fn hold_reason(reason: dudes_proto::HoldReason) -> ChatReason {
    match reason {
        dudes_proto::HoldReason::Paused => ChatReason::Paused,
        dudes_proto::HoldReason::Halted => ChatReason::Halted,
        dudes_proto::HoldReason::InferredTurnEnd => ChatReason::Provenance,
        dudes_proto::HoldReason::Attached => ChatReason::ModeChanged,
        _ => ChatReason::Hold,
    }
}

fn exhausted_reason(reason: dudes_proto::ExhaustedReason) -> ChatReason {
    match reason {
        dudes_proto::ExhaustedReason::CeilingRequired => ChatReason::CeilingRequired,
        dudes_proto::ExhaustedReason::Hops => ChatReason::Hops,
        dudes_proto::ExhaustedReason::Breaker => ChatReason::Hold,
        _ => ChatReason::Budget,
    }
}
