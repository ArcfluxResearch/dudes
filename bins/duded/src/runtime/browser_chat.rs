use super::{
    BTreeMap, Clock, Command, Ctx, Daemon, Input, PredicateRunner, ProcessHost, PumpFailure,
    Result, Rng, Supervisor,
};
use crate::floor_chat::{ChatAction, ChatOutcome, ChatRefusal, ChatRequest};
use dudes_proto::{BrowserNoteAttribution, BrowserNoteDestination, ChatReason};
use dudes_supervisor::machine::BrowserNoteCancelDisposition;
use dudes_supervisor::world::BrowserNoteState;

enum OwnedLaunchFailure {
    RepositoryDeferred,
    Preparation(std::io::Error),
}

impl From<std::io::Error> for OwnedLaunchFailure {
    fn from(error: std::io::Error) -> Self {
        Self::Preparation(error)
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// The owned preparation control used by real daemon boundary tests.
    #[doc(hidden)]
    #[must_use]
    pub fn browser_launch_pause(&self) -> crate::BrowserLaunchPause {
        self.ctx.browser_launches.pause()
    }

    pub(super) fn pump_runtime_input(&mut self, input: Input) -> Result<()> {
        self.pump(input)
    }

    /// The folder a chat file for `agent` goes under. An Attached agent's engine is not launched by
    /// Dudes, so it could not be given the folder.
    pub(super) fn chat_file_dir(
        &self,
        agent: dudes_types::AgentId,
    ) -> Result<std::path::PathBuf, &'static str> {
        match self.sup.world().agents.get(&agent) {
            None => Err("agent_absent"),
            Some(row) if row.retired => Err("agent_retired"),
            Some(row) if row.attached => Err("attached"),
            Some(_) => Ok(self.ctx.agents_root.join(agent.to_string())),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the writer checks switch, durable retry, fence, world, and append permit in one visible order"
    )]
    pub(super) fn browser_chat(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &ChatRequest,
    ) -> Result<ChatOutcome, ChatRefusal> {
        let refused = |reason| {
            Ok(ChatOutcome::Refused {
                request_id: request.request_wire.clone(),
                reason,
            })
        };
        if !session.is_valid() {
            return Err(ChatRefusal::Forbidden);
        }
        if let Some(indexed) = self.ctx.chat_index.get(request.request_id_digest) {
            if indexed.request_digest != request.request_digest {
                return Err(ChatRefusal::Conflict);
            }
            if self.ctx.switches.load().control.pause_resume() {
                self.lift_floor_halt_after_note(session, request)?;
            }
            return Ok(indexed.outcome(request.request_wire.clone()));
        }
        // The second step of stop and send lifts a halt, so it needs the control switch. A resend
        // of a note that is already durable was answered above.
        if request.follows_stop() && !self.ctx.switches.load().control.pause_resume() {
            return refused(ChatReason::ActionOff);
        }
        if self.ctx.journal_writes_stopped() {
            return Err(ChatRefusal::UnavailableNoEffect);
        }
        let browser = BrowserNoteAttribution {
            request_id_digest: request.request_id_digest,
            request_digest: request.request_digest,
            session_digest: session.digest(),
        };
        let command = match &request.action {
            ChatAction::Send {
                text,
                context_reset_turn,
                after_stop,
            } => {
                let row = match self.sup.world().agents.get(&request.agent) {
                    None => return refused(ChatReason::AgentAbsent),
                    Some(row) if row.retired => return refused(ChatReason::AgentRetired),
                    Some(row) => row,
                };
                // The second step of stop and send is the only send that reaches a halted agent,
                // and only the Floor's own halt, still the one its first step returned.
                if let Some(stop) = after_stop {
                    if row.control_lsn != Some(*stop) {
                        return refused(ChatReason::StaleControl);
                    }
                    if !(row.halted && row.halted_by_floor) {
                        return refused(ChatReason::Halted);
                    }
                    if row.open_turn.is_some() {
                        return refused(ChatReason::TurnOpen);
                    }
                } else if row.halted {
                    return refused(ChatReason::Halted);
                }
                let loss = self.sup.world().conversation_context_loss(request.agent);
                if !row.attached && row.goal.is_none() {
                    match (loss, context_reset_turn) {
                        (Some(turn_id), None) => {
                            return Ok(ChatOutcome::ContextLost {
                                request_id: request.request_wire.clone(),
                                agent: request.agent,
                                turn_id,
                            });
                        }
                        (Some(failed_turn), Some(reset)) if failed_turn == *reset => {}
                        (None, None) => {}
                        _ => return refused(ChatReason::ContextChanged),
                    }
                } else if context_reset_turn.is_some() {
                    return refused(ChatReason::ContextChanged);
                }
                let Some(adapter) = self.sup.adapters().get(row.provider) else {
                    return refused(ChatReason::UnsupportedDelivery);
                };
                let destination = if row.attached {
                    if adapter.tier() != dudes_types::Tier::HookGoverned
                        || self.sup.config().hook_command.is_none()
                    {
                        return refused(ChatReason::UnsupportedDelivery);
                    }
                    BrowserNoteDestination::AttachedContext
                } else {
                    match row.goal.filter(|goal| {
                        self.sup
                            .world()
                            .goals
                            .get(goal)
                            .is_some_and(|row| !row.cleared)
                    }) {
                        Some(goal) => BrowserNoteDestination::ManagedGoal { goal },
                        None => BrowserNoteDestination::ManagedConversation {
                            context_lsn: row.context_lsn,
                        },
                    }
                };
                let goal = match destination {
                    BrowserNoteDestination::ManagedGoal { goal } => Some(goal),
                    BrowserNoteDestination::AttachedContext
                    | BrowserNoteDestination::ManagedConversation { .. } => None,
                };
                let view = self
                    .ctx
                    .projections
                    .chat()
                    .map_err(|_| ChatRefusal::UnavailableNoEffect)?;
                if !crate::floor_chat::pending_fits(&view, request.agent, goal, text)
                    .map_err(|_| ChatRefusal::UnavailableNoEffect)?
                {
                    return refused(ChatReason::QueueFull);
                }
                if let BrowserNoteDestination::ManagedConversation { context_lsn } = destination {
                    Command::ConversationMessageQueued {
                        agent: request.agent,
                        text: text.clone(),
                        browser,
                        context_lsn,
                        context_reset_turn: *context_reset_turn,
                    }
                } else {
                    Command::BrowserNoteQueued {
                        agent: request.agent,
                        text: text.clone(),
                        browser,
                        destination,
                    }
                }
            }
            ChatAction::Cancel { note_lsn } => {
                if let BrowserNoteCancelDisposition::Refused { reason } = self
                    .sup
                    .browser_note_cancel_disposition(request.agent, *note_lsn)
                {
                    return refused(reason);
                }
                Command::BrowserNoteCancel {
                    agent: request.agent,
                    note_lsn: *note_lsn,
                    browser,
                }
            }
        };
        let pumped = self.pump_browser(session, command);
        if let Some(indexed) = self.ctx.chat_index.get(request.request_id_digest)
            && indexed.request_digest == request.request_digest
        {
            self.lift_floor_halt_after_note(session, request)?;
            return Ok(indexed.outcome(request.request_wire.clone()));
        }
        if matches!(pumped, Err(PumpFailure::Forbidden | PumpFailure::NoEffect)) {
            self.sup.browser_append_refused(request.request_id_digest);
        }
        Err(match pumped {
            Err(PumpFailure::Forbidden) => ChatRefusal::Forbidden,
            Err(PumpFailure::NoEffect) => ChatRefusal::UnavailableNoEffect,
            Ok(()) | Err(PumpFailure::Uncertain) => ChatRefusal::Unavailable,
        })
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// The second append of stop and send: lift the Floor's halt once its note is durable.
    ///
    /// The resume carries an identity derived from the note's own request, so a resend finds it in
    /// the control index and appends nothing. It is appended only while the halt is still the
    /// Floor's and the control identity is still the one the stop returned, so a halt an operator
    /// made after a crash is never lifted by a resent request.
    fn lift_floor_halt_after_note(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &ChatRequest,
    ) -> Result<(), ChatRefusal> {
        let ChatAction::Send {
            after_stop: Some(stop),
            ..
        } = &request.action
        else {
            return Ok(());
        };
        let derive = |domain: &[u8], source: dudes_proto::record::Hex32| {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(domain);
            hasher.update(source.bytes());
            dudes_proto::record::Hex32::new(hasher.finalize().into())
        };
        let request_id = derive(
            b"dudes.chat.send.stop.resume.id.v1\0",
            request.request_id_digest,
        );
        if self.ctx.control_index.get(request_id).is_some() {
            return Ok(());
        }
        let still_floor_halt = self
            .sup
            .world()
            .agents
            .get(&request.agent)
            .is_some_and(|row| {
                !row.retired && row.halted && row.halted_by_floor && row.control_lsn == Some(*stop)
            });
        if !still_floor_halt {
            return Ok(());
        }
        let pumped = self.pump_browser(
            session,
            Command::Resume {
                agent: request.agent,
                browser: Some(dudes_proto::record::BrowserControl {
                    request_id,
                    request_digest: derive(
                        b"dudes.chat.send.stop.resume.digest.v1\0",
                        request.request_digest,
                    ),
                    control_lsn: Some(*stop),
                    session_digest: session.digest(),
                }),
            },
        );
        if self.ctx.control_index.get(request_id).is_some() {
            return Ok(());
        }
        Err(match pumped {
            Err(PumpFailure::Forbidden) => ChatRefusal::Forbidden,
            Err(PumpFailure::NoEffect) => ChatRefusal::UnavailableNoEffect,
            Ok(()) | Err(PumpFailure::Uncertain) => ChatRefusal::Unavailable,
        })
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn spawn_turn<R: Rng>(
        &mut self,
        turn: dudes_types::TurnId,
        spec: &dudes_providers::launch::LaunchSpec,
        install: &dudes_providers::adapter::ManagedInstall,
        dispatch: (Option<dudes_types::Lsn>, Option<u64>),
        queue: &mut std::collections::VecDeque<Input>,
        sup: &mut Supervisor<super::Shared<C>, R>,
    ) -> Result<()> {
        let row = sup
            .world()
            .turns
            .get(&turn)
            .ok_or_else(|| anyhow::anyhow!("managed launch turn is unavailable"))?;
        let agent = row.agent;
        let owner = sup
            .world()
            .agents
            .get(&agent)
            .ok_or_else(|| anyhow::anyhow!("managed launch owner is unavailable"))?;
        if let Err(refusal) = self
            .admit_prior_attempt(sup, agent, Some(turn))
            .and_then(|()| {
                self.admit_shared_folder(
                    sup,
                    Some(agent),
                    owner.provider,
                    owner.mcp.isolation,
                    dudes_types::Mode::Managed,
                    &spec.cwd,
                )
            })
        {
            queue.push_back(Input::LaunchAdmissionRefused {
                agent,
                turn: Some(turn),
                refusal,
                dispatch_token: None,
            });
            return Ok(());
        }
        self.spawn_owned(turn, spec, install, dispatch, queue, sup)
    }

    pub(super) fn dispatch_browser_intent<R: Rng>(
        &mut self,
        effect: &dudes_supervisor::Effect,
        queue: &mut std::collections::VecDeque<Input>,
        sup: &mut Supervisor<super::Shared<C>, R>,
        closing: &mut crate::closing::Waiting,
    ) -> Result<()> {
        let dudes_supervisor::Effect::Persist { token, entry } = effect else {
            anyhow::bail!("browser dispatch requires a persist effect");
        };
        let dudes_proto::Record::BrowserNoteDispatchIntent { agent, turn, .. } = &entry.record
        else {
            anyhow::bail!("browser dispatch requires its exact intent record");
        };
        if queue.iter().any(|input| {
            matches!(
                input,
                Input::Persisted { .. } | Input::Journaled { .. } | Input::Replayed { .. }
            )
        }) {
            queue.push_back(Input::BrowserDispatchAdmissionReady {
                token: *token,
                turn: *turn,
            });
            return Ok(());
        }
        if !self.recovery_dispatch_allowed(sup, *turn) {
            return Ok(());
        }
        let spec = self.with_recovery_facts(sup, &[], |sup, operation| {
            sup.browser_dispatch_spec(*token, *turn, operation).cloned()
        });
        let Some(spec) = spec else { return Ok(()) };
        let owner = sup
            .world()
            .agents
            .get(agent)
            .ok_or_else(|| anyhow::anyhow!("browser dispatch owner is unavailable"))?;
        if let Err(refusal) = self
            .admit_prior_attempt(sup, *agent, Some(*turn))
            .and_then(|()| {
                self.admit_shared_folder(
                    sup,
                    Some(*agent),
                    owner.provider,
                    owner.mcp.isolation,
                    dudes_types::Mode::Managed,
                    &spec.cwd,
                )
            })
        {
            queue.push_back(Input::LaunchAdmissionRefused {
                agent: *agent,
                turn: Some(*turn),
                refusal,
                dispatch_token: Some(*token),
            });
            return Ok(());
        }
        let append = self.append(sup.world(), entry)?;
        let through = append.through();
        anyhow::ensure!(
            self.writer.poisoned().is_none(),
            "browser dispatch journal stopped after its append"
        );
        let input = Input::Persisted {
            token: *token,
            append,
        };
        let works = super::recovery::input_works(&input);
        let handled = self
            .with_recovery_facts(sup, &works, |sup, operation| sup.handle(input, operation))
            .inspect_err(|_| self.fence_journal())?;
        self.require_handled_through(sup.world(), &handled, through)?;
        self.tokens.mark_folded();
        self.publish_folded_floor(sup.world(), closing)?;
        let effects = sup.take_effects();
        let mut spawned = false;
        for effect in &effects {
            match effect {
                dudes_supervisor::Effect::Spawn {
                    turn: issued,
                    spec,
                    install,
                    mcp_grant,
                    browser_generation,
                } => {
                    anyhow::ensure!(
                        !spawned && *issued == *turn,
                        "browser intent yielded a different or repeated turn"
                    );
                    spawned = true;
                    self.tokens
                        .publish_completed_folds(|| sup.world().token_hash_owners());
                    self.spawn_owned(*issued, spec, install, (*mcp_grant, *browser_generation), queue, sup)?;
                }
                dudes_supervisor::Effect::Persist { entry, .. }
                    if matches!(entry.record, dudes_proto::Record::RecoveryInvalidated(_)) =>
                {
                    self.execute(effect, queue, &mut super::PumpReport::Discard, sup, closing)?;
                }
                _ => anyhow::bail!("browser intent acknowledgment yielded an unrelated effect"),
            }
        }
        Ok(())
    }

    fn prepare_owned_launch<R: Rng>(
        &mut self,
        turn: dudes_types::TurnId,
        spec: &dudes_providers::launch::LaunchSpec,
        mcp_grant: Option<dudes_types::Lsn>,
        already_resolved: bool,
        sup: &Supervisor<super::Shared<C>, R>,
    ) -> Result<dudes_providers::launch::LaunchSpec, OwnedLaunchFailure> {
        let issued = sup
            .world()
            .turns
            .get(&turn)
            .ok_or_else(|| std::io::Error::other("managed turn identity is unavailable"))?;
        let owner = sup
            .world()
            .agents
            .get(&issued.agent)
            .ok_or_else(|| std::io::Error::other("managed turn owner is unavailable"))?;
        if !self.repository_turn_allowed(sup.world(), turn) {
            return Err(OwnedLaunchFailure::RepositoryDeferred);
        }
        let authority = issued
            .repositories
            .as_ref()
            .filter(|authority| authority.matches_hire(owner.mcp.hire_lsn, &owner.repositories))
            .ok_or_else(|| std::io::Error::other("managed repository authority is unavailable"))?;
        let repository = authority
            .singleton()
            .ok_or_else(|| std::io::Error::other("managed repository authority is unsupported"))?;
        if spec.cwd != repository.cwd() {
            return Err(std::io::Error::other(
                "managed launch differs from its recorded repository",
            )
            .into());
        }
        if already_resolved {
            Ok(spec.clone())
        } else {
            crate::program::resolve_launch(spec.clone())
        }
        .and_then(|mut spec| {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let outbox = {
                let (agent_id, agent) = sup
                    .world()
                    .turns
                    .get(&turn)
                    .and_then(|row| {
                        sup.world()
                            .agents
                            .get(&row.agent)
                            .map(|agent| (row.agent, agent))
                    })
                    .ok_or_else(|| std::io::Error::other("managed turn identity is unavailable"))?;
                if agent.provider == dudes_types::Provider::Codex {
                    Some(
                        self.mailbox
                            .as_ref()
                            .ok_or_else(|| std::io::Error::other("owned mailbox is unavailable"))?
                            .admit_outbox(agent_id)?,
                    )
                } else {
                    None
                }
            };
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let outbox = None;
            self.children.prepare_launch(&mut spec, outbox)?;
            self.children
                .read_next_for(super::turn_reader(sup, turn, &spec), &spec, mcp_grant);
            Ok(spec)
        })
        .map_err(Into::into)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the final authority check and actual host-call outcome stay in one visible boundary"
    )]
    fn spawn_owned<R: Rng>(
        &mut self,
        turn: dudes_types::TurnId,
        spec: &dudes_providers::launch::LaunchSpec,
        install: &dudes_providers::adapter::ManagedInstall,
        dispatch: (Option<dudes_types::Lsn>, Option<u64>),
        queue: &mut std::collections::VecDeque<Input>,
        sup: &mut Supervisor<super::Shared<C>, R>,
    ) -> Result<()> {
        let (mcp_grant, generation) = dispatch;
        if !self.repository_turn_allowed(sup.world(), turn) {
            if let Some(claim) = sup.prepare_host_dispatch(turn) {
                queue.push_back(Input::RepositoryLaunchDeferred { claim });
            }
            return Ok(());
        }
        if !self.recovery_dispatch_allowed(sup, turn) {
            return Ok(());
        }
        if !self.with_recovery_facts(sup, &[], |sup, operation| {
            sup.claim_browser_dispatch(turn, generation, operation)
        }) {
            return Ok(());
        }
        let Some(claim) = sup.prepare_host_dispatch(turn) else {
            return Ok(());
        };
        let prepared =
            match self.prepare_owned_launch(turn, spec, mcp_grant, generation.is_some(), sup) {
                Err(OwnedLaunchFailure::RepositoryDeferred) => {
                    self.children.discard_unlaunched();
                    queue.push_back(Input::RepositoryLaunchDeferred { claim });
                    return Ok(());
                }
                Err(OwnedLaunchFailure::Preparation(error)) => Err(error),
                Ok(spec) => Ok(spec),
            };
        let cwd = prepared.as_ref().ok().map(|spec| spec.cwd.clone());
        let actual_host_call = prepared.is_ok();
        match match prepared {
            Ok(spec) => {
                #[cfg(test)]
                if let Some(before) = self.before_recovery_dispatch.take() {
                    before();
                }
                if !self.repository_turn_allowed(sup.world(), turn) {
                    queue.push_back(Input::RepositoryLaunchDeferred { claim });
                    self.children.discard_unlaunched();
                    return Ok(());
                }
                if !self.recovery_dispatch_allowed(sup, turn) || !sup.enter_host_dispatch(claim) {
                    self.children.discard_unlaunched();
                    return Ok(());
                }
                let holder = dudes_supervisor::Holder::Turn(turn);
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if let Some(mailbox) = &mut self.mailbox
                    && let Some(row) = sup.world().turns.get(&turn)
                {
                    mailbox.begin_subject_context(row.agent, holder, self.children.redactor());
                }
                self.children.redactor().hold_env(holder, &spec.env);
                self.host.spawn(&spec, install)
            }
            Err(reason) => super::SpawnResult::NotLaunched { reason },
        } {
            super::SpawnResult::Spawned(key) => {
                self.remember_native_launch(sup.world(), turn, &key);
                self.children.spawned_in(
                    turn,
                    &key,
                    mcp_grant,
                    cwd.as_deref()
                        .expect("a spawned child had a prepared execution folder"),
                );
                if let Some(row) = self.projections.turn(turn)?
                    && row.mode == dudes_types::Mode::Managed
                    && row.is_open()
                    && let Some(agent) = self.projections.agent(row.agent)?
                    && agent.mode == dudes_types::Mode::Managed
                    && agent.open_turn == Some(turn)
                {
                    let output = self.children.output();
                    output.prepare_reissued(agent.id, turn);
                    output.register(agent.id, turn, &key);
                }
                queue.push_back(Input::Spawned {
                    turn,
                    key,
                    mcp_grant,
                });
            }
            super::SpawnResult::NotLaunched { reason } => {
                self.children
                    .redactor()
                    .release(dudes_supervisor::Holder::Turn(turn));
                if actual_host_call
                    && self.confirmed_launch_failures.len()
                        < dudes_supervisor::host::MAX_OWNED_TARGET_QUERY_ROWS
                    && let Some((started, attempt)) =
                        sup.world().instructions.current_for_turn(turn)
                {
                    self.confirmed_launch_failures
                        .insert(started, attempt.owner);
                }
                tracing::error!(error = %reason, ?turn, "the host did not launch the turn");
                let reason = format!("managed process could not launch ({:?})", reason.kind());
                if !actual_host_call
                    && let Some((started_lsn, _)) = sup.world().instructions.current_for_turn(turn)
                {
                    queue.push_back(Input::LaunchPreparationFailed {
                        turn,
                        started_lsn,
                        reason,
                    });
                } else {
                    queue.push_back(Input::SpawnFailed { turn, reason });
                }
            }
            super::SpawnResult::CleanupUnconfirmed { reason } => {
                self.children.retain_uncertain(
                    turn,
                    cwd.as_deref()
                        .expect("unconfirmed cleanup followed a prepared launch"),
                );
                queue.push_back(Input::InstructionLaunchCleanupUnconfirmed { turn });
                tracing::error!(error = %reason, ?turn, "the host could not confirm cleanup after a failed launch");
            }
        }
        Ok(())
    }

    pub(crate) fn publish_chat<S: Clock, R: Rng>(&mut self, sup: &Supervisor<S, R>) -> Result<()> {
        let Some(streams) = self.floor_streams.clone() else {
            return Ok(());
        };
        if streams.is_ended() {
            return Ok(());
        }
        if !self.pending_floor.is_empty() {
            streams.end();
            return Ok(());
        }
        let mut overlays = BTreeMap::new();
        for (lsn, note) in sup.world().browser_notes() {
            if !matches!(
                note.state,
                BrowserNoteState::Queued | BrowserNoteState::Reserved { .. }
            ) {
                continue;
            }
            let disposition = sup.browser_note_cancel_disposition(note.agent, *lsn);
            let uncertain = match note.state {
                BrowserNoteState::Reserved { turn } => sup.browser_delivery_uncertain(turn),
                _ => false,
            };
            let delivery_started = matches!(
                disposition,
                BrowserNoteCancelDisposition::Refused {
                    reason: ChatReason::DeliveryStarted
                }
            );
            let reason = if uncertain {
                Some(ChatReason::DeliveryUncertain)
            } else if delivery_started {
                Some(ChatReason::DeliveryStarted)
            } else {
                pending_reason(sup, note)
            };
            overlays.insert(
                *lsn,
                dudes_journal::ChatOverlay {
                    uncertain,
                    delivery_started,
                    reason,
                },
            );
        }
        let chat = Some(self.projections.chat_with_overlays(&overlays)?);
        let snapshot = self.floor_snapshot()?;
        streams.publish_closing_with_chat_output(snapshot, self.floor_closing, chat, || {})?;
        Ok(())
    }
}

pub(super) fn attribution_session(
    record: &dudes_proto::Record,
) -> Option<dudes_proto::record::Hex32> {
    use dudes_proto::Record;
    match record {
        Record::ConversationAnswerQueued {
            authority: dudes_proto::ConversationAnswerAuthority::Browser { browser },
            ..
        } => Some(browser.session_digest),
        Record::ConversationContextResetQueued { browser, .. }
        | Record::ConversationMessageQueued { browser, .. }
        | Record::BrowserNoteQueued { browser, .. }
        | Record::BrowserNoteCancelled { browser, .. }
        | Record::BrowserNoteIssuedCancelled { browser, .. }
        | Record::BrowserNoteReissueCancelled { browser, .. } => Some(browser.session_digest),
        Record::AgentHired {
            browser_onboarding: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::GoalSet {
            browser_onboarding: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::GoalSet {
            browser_goal: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::HumanAnswer {
            browser_answer: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::Pause {
            browser: Some(meta),
            ..
        }
        | Record::Resume {
            browser: Some(meta),
            ..
        }
        | Record::BrowserHalt { browser: meta, .. } => Some(meta.session_digest),
        Record::AgentRenamed {
            browser_rename: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::AgentLookChanged {
            browser_look: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::AgentRetired {
            browser_retire: Some(meta),
            ..
        } => Some(meta.session_digest),
        Record::AgentModelChanged {
            by: dudes_proto::Actor::BrowserSession { session_digest },
            ..
        } => Some(*session_digest),
        record => ticket_session(record),
    }
}

/// The Floor session that authored a ticket record, when one did. The operator's own tickets, and
/// every move the daemon makes, name no session.
pub(super) fn ticket_session(record: &dudes_proto::Record) -> Option<dudes_proto::record::Hex32> {
    use dudes_proto::{Actor, Record};
    let by = record
        .ticket_created()
        .map(|created| created.by)
        .or_else(|| record.ticket_edited().map(|edited| edited.by))
        .or(match record {
            Record::TicketAssigned { by, .. }
            | Record::TicketInstructionReviewed { by, .. }
            | Record::TicketRemoved { by, .. }
            | Record::CardTransition { by, .. } => Some(*by),
            _ => None,
        })?;
    match by {
        Actor::BrowserSession { session_digest } => Some(session_digest),
        _ => None,
    }
}

fn pending_reason<S: Clock, R: Rng>(
    sup: &Supervisor<S, R>,
    note: &dudes_supervisor::world::BrowserNoteRow,
) -> Option<ChatReason> {
    let row = sup.world().agents.get(&note.agent)?;
    if row.retired {
        return Some(ChatReason::AgentRetired);
    }
    match note.destination {
        BrowserNoteDestination::ManagedConversation { context_lsn }
            if note.state == BrowserNoteState::Queued
                && (row.goal.is_some() || row.context_lsn != context_lsn) =>
        {
            return Some(if row.goal.is_some() {
                ChatReason::GoalChanged
            } else {
                ChatReason::ContextChanged
            });
        }
        BrowserNoteDestination::ManagedGoal { goal } if row.goal != Some(goal) => {
            return Some(ChatReason::GoalChanged);
        }
        BrowserNoteDestination::ManagedGoal { .. }
        | BrowserNoteDestination::ManagedConversation { .. }
            if row.attached =>
        {
            return Some(ChatReason::ModeChanged);
        }
        BrowserNoteDestination::AttachedContext if !row.attached => {
            return Some(ChatReason::ModeChanged);
        }
        _ => {}
    }
    if row.halted {
        Some(ChatReason::Halted)
    } else if row.paused {
        Some(ChatReason::Paused)
    } else if row.automatic_hold.is_some() {
        Some(ChatReason::Hold)
    } else if row.last_turn_end == Some(dudes_types::Provenance::Inferred) {
        Some(ChatReason::Provenance)
    } else if row.open_question.is_some() {
        Some(ChatReason::OpenQuestion)
    } else if note.destination == BrowserNoteDestination::AttachedContext {
        None
    } else if matches!(
        note.destination,
        BrowserNoteDestination::ManagedConversation { .. }
    ) && sup.conversation_ceiling_missing(note.agent)
    {
        Some(ChatReason::CeilingRequired)
    } else {
        sup.browser_note_admission_reason(note.agent)
    }
}
