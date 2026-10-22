use super::{AgentId, Clock, Lsn, ManagedLaunch, Record, Rng, Supervisor, Then, TurnId};
use dudes_policy::{BudgetState, Control, ConversationInput};
use dudes_proto::{BrowserNoteDestination, ConversationChargeScope, NoteAdmission};

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Whether the startup environment exposes a credential listed by the registered adapter.
    #[must_use]
    pub fn billing_credential_visible(&self, provider: dudes_types::Provider) -> bool {
        let Some(adapter) = self.adapters.get(provider) else {
            return false;
        };
        let names = adapter.billing_env_vars();
        self.config.daemon_env.iter().any(|var| {
            !var.expose_value().is_empty()
                && names
                    .iter()
                    .any(|name| var.name() == std::ffi::OsStr::new(name))
        })
    }

    /// Whether billed finite work lacks a ceiling in its agent and Floor scopes.
    #[must_use]
    pub fn conversation_ceiling_missing(&self, agent: AgentId) -> bool {
        self.world.agents.get(&agent).is_some_and(|row| {
            self.billing_credential_visible(row.provider)
                && !dudes_policy::budget::billing_ceiling_satisfied(
                    row.provider,
                    &row.budget,
                    None,
                    self.config.floor_usd_per_day,
                )
        })
    }

    pub(super) fn queue_conversation_answer(
        &mut self,
        agent: AgentId,
        text: &str,
        authority: dudes_proto::ConversationAnswerAuthority,
    ) -> bool {
        let Some(row) = self.world.agents.get(&agent) else {
            return false;
        };
        let Some(origin) = row.conversation_question else {
            return false;
        };
        let (Some(question), Some(question_lsn)) = (row.open_question.as_deref(), row.question_lsn)
        else {
            return true;
        };
        self.persist(
            Record::ConversationAnswerQueued {
                agent,
                question_lsn,
                origin,
                text: text.to_owned(),
                task: dudes_proto::conversation_answer_task(question, text),
                authority,
                charge_scope: ConversationChargeScope::AgentAndFloor,
            },
            Then::AnswerSaved { agent },
        );
        true
    }

    pub(super) fn conversation_input_for(&self, agent: AgentId) -> Option<ConversationInput> {
        if !self.browser_delivery_open || self.closing_active || self.closing_recovery {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        if row.retired || row.attached || row.goal.is_some() {
            return None;
        }
        let (note_lsn, note) = self.world.eligible_browser_note(
            agent,
            &BrowserNoteDestination::ManagedConversation {
                context_lsn: row.context_lsn,
            },
        )?;
        let spend = self.spend.get(&agent)?;
        Some(ConversationInput {
            note_lsn,
            context_lsn: row.context_lsn,
            text: note.task.as_ref().unwrap_or(&note.text).clone(),
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
                goal: dudes_proto::Spend::default(),
                goal_unknown_ends: 0,
                agent_today: spend.agent_today,
                agent_unknown_ends_today: spend.agent_unknown_ends_today,
                floor_today: spend.floor_today,
                floor_unknown_ends_today: spend.floor_unknown_ends_today,
            },
            billing_ceiling_missing: self.conversation_ceiling_missing(agent),
            agent_budget: row.budget,
            floor_usd_per_day: self.config.floor_usd_per_day,
            per_turn_floor: self.config.per_turn_floor,
            rung: row.rung,
        })
    }

    pub(super) fn decide_conversation(&mut self, agent: AgentId) {
        let Some(input) = self.conversation_input_for(agent) else {
            return;
        };
        let Some(input_digest) = conversation_digest(&input) else {
            return;
        };
        let decided = dudes_policy::admit_conversation(&input);
        let refusal = super::budget_refusal_values(
            agent,
            None,
            &input.budget_state,
            &dudes_policy::Ceilings {
                goal: dudes_proto::GoalBudget {
                    usd: None,
                    tokens: None,
                },
                agent: input.agent_budget,
                floor_usd_per_day: input.floor_usd_per_day,
            },
            input.per_turn_floor,
            &decided.gate_trace,
        );
        let then = match &decided.decision {
            NoteAdmission::Issue { task } => Then::ConversationIssue {
                agent,
                note_lsn: input.note_lsn,
                context_lsn: input.context_lsn,
                task: task.clone(),
            },
            _ => Then::Fold,
        };
        self.persist(
            Record::ConversationTurnDecided {
                agent,
                note_lsn: input.note_lsn,
                context_lsn: input.context_lsn,
                charge_scope: ConversationChargeScope::AgentAndFloor,
                input_digest,
                gate_trace: decided.gate_trace,
                decision: decided.decision,
            },
            then,
        );
        if let Some(record) = refusal {
            self.persist(record, Then::Fold);
        }
    }

    pub(super) fn issue_conversation(
        &mut self,
        agent: AgentId,
        note_lsn: Lsn,
        context_lsn: Lsn,
        task: &str,
        lsn_of_decision: Lsn,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if self.closing_active
            || self.closing_recovery
            || !self.browser_delivery_open
            || self.launch_held(agent, operation)
            || self
                .world
                .conversation_message(agent, note_lsn, context_lsn)
                .is_none()
        {
            return;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if row.open_turn.is_some() || row.paused || row.halted {
            return;
        }
        let turn = TurnId::from_ulid(self.rng.ulid());
        let session = self.announce_session(agent, false);
        let Some(selection) = self.model_selection(&row) else {
            return;
        };
        let launch = match self.launch(agent, &row, task, &session, &selection) {
            ManagedLaunch::Ready(launch) => launch,
            ManagedLaunch::Unavailable => return,
            ManagedLaunch::Refused(error) => {
                self.persist(
                    Record::AgentHeld {
                        agent,
                        provenance: dudes_types::Provenance::Observed,
                        cause: dudes_proto::AutomaticHold::LaunchRefused {
                            reason: error.to_string(),
                        },
                    },
                    Then::Fold,
                );
                return;
            }
        };
        self.persist(
            Record::ConversationTurnIssued {
                agent,
                turn,
                note_lsn,
                context_lsn,
                charge_scope: ConversationChargeScope::AgentAndFloor,
                task: task.to_owned(),
                session: session.id().cloned(),
                model_selection: selection,
                lsn_of_decision,
            },
            Then::Spawn {
                agent,
                turn,
                launch,
            },
        );
    }
}

pub(super) fn conversation_digest(input: &ConversationInput) -> Option<String> {
    let bytes = serde_json::to_vec(&("finite_conversation_v1", input)).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}
