use super::super::RecoveryAttempt;
use super::super::history::{RecoveryAttemptContext, RecoveryPriorControl, RecoveryWitnessContext};
use super::*;
use crate::record::{BrowserControl, Hex32, samples};
use crate::recovery::{RecoveryAnswerDelivery, RecoveryEpisodeId, RecoveryIssued, RecoveryRecap};
use crate::{AutomaticHold, Entry, Record, TurnOutcome};
use dudes_types::{AgentId, GoalId, ModelSelection, Provider, SessionId, Timestamp, TurnId};

fn hash(bytes: &[u8]) -> Hex32 {
    let mut output = [0; 32];
    for (index, byte) in bytes.iter().enumerate() {
        output[index % 32] ^= byte;
    }
    Hex32::new(output)
}

struct Fixture {
    history: RecoveryHistory,
    state: RecoveryState,
    facts: RecoveryFacts,
    required: RecoveryRequired,
}

impl Fixture {
    fn new() -> Self {
        let work = RecoveryWork {
            agent: AgentId::from_parts(61, 1),
            goal: GoalId::from_parts(61, 2),
            turn: TurnId::from_parts(61, 3),
        };
        let binding = RecoveryBinding {
            work,
            hired_lsn: Lsn(10),
            issue_lsn: Lsn(20),
            ticket: None,
            worktree: None,
            provider: Provider::Codex,
            model_selection: Some(ModelSelection::provider_default()),
        };
        let mut fixture = Self {
            history: RecoveryHistory::default(),
            state: RecoveryState::default(),
            facts: RecoveryFacts {
                through: Lsn(60),
                binding: binding.clone(),
                source_issue: RecoveryIssue {
                    lsn: Lsn(20),
                    issued: RecoveryIssued {
                        work,
                        task_digest: Hex32::new([1; 32]),
                        answers: Vec::new(),
                        model_selection: binding.model_selection.clone(),
                        session: None,
                    },
                },
                attempt: RecoveryAttempt {
                    spawn_lsn: Some(Lsn(30)),
                    end_lsn: Some(Lsn(40)),
                    confirmation_lsn: None,
                },
                control_lsn: Some(Lsn(50)),
                confirmed_session: None,
            },
            required: RecoveryRequired {
                binding,
                origin: RecoveryOrigin::Existing { lsn: Lsn(50) },
                reason: RecoveryReason::HeldOrPausedFailure,
                boundary: RecoveryBoundary::Paused { end_lsn: Lsn(40) },
                predecessor: None,
            },
        };
        fixture.install_sources();
        fixture
    }

    fn install_sources(&mut self) {
        let work = self.facts.binding.work;
        let mut hired = samples::record(RecordKind::AgentHired);
        if let Record::AgentHired {
            id,
            provider,
            worktree,
            ..
        } = &mut hired
        {
            *id = work.agent;
            *provider = Provider::Codex;
            *worktree = None;
        }
        self.observe(10, hired, RecoveryWitnessContext::default());
        let mut goal = samples::record(RecordKind::GoalSet);
        if let Record::GoalSet { agent, goal, .. } = &mut goal {
            *agent = work.agent;
            goal.id = work.goal;
        }
        self.observe(15, goal, RecoveryWitnessContext::default());
        self.observe(
            20,
            Record::TurnIssued {
                agent: work.agent,
                turn: work.turn,
                goal: work.goal,
                mode: Mode::Managed,
                lsn_of_decision: Lsn(19),
                task: "original task".into(),
                session: None,
                model_selection: self.facts.binding.model_selection.clone(),
                answers: Vec::new(),
            },
            RecoveryWitnessContext::default(),
        );
        self.facts.source_issue = self.history.at(Lsn(20)).unwrap().issue().unwrap();
        self.spawn(30, work, 20, None, false, None);
        self.end(
            40,
            work,
            20,
            Some(30),
            TurnOutcome::Failed {
                reason: "failure".into(),
            },
        );
        self.observe(
            50,
            Record::Pause {
                agent: work.agent,
                browser: None,
            },
            RecoveryWitnessContext {
                previous_control: Some(RecoveryPriorControl {
                    agent: work.agent,
                    lsn: None,
                }),
                ..RecoveryWitnessContext::default()
            },
        );
    }

    fn observe(&mut self, at: u64, record: Record, context: RecoveryWitnessContext) {
        self.history
            .observe_with_context(
                Lsn(at),
                &Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record,
                },
                context,
                hash,
            )
            .unwrap();
    }

    fn spawn(
        &mut self,
        at: u64,
        work: RecoveryWork,
        issue: u64,
        session: Option<SessionId>,
        reissued: bool,
        confirmed: Option<u64>,
    ) {
        self.observe(
            at,
            Record::TurnSpawned {
                agent: work.agent,
                turn: work.turn,
                pid: 42,
                start_time: 10,
                boot: None,
                session,
                reissued,
                mcp_grant: None,
            },
            RecoveryWitnessContext {
                attempt: Some(RecoveryAttemptContext {
                    issue_lsn: Lsn(issue),
                    previous_spawn_lsn: None,
                    previous_end_lsn: None,
                    confirmed_session_lsn: confirmed.map(Lsn),
                }),
                ..RecoveryWitnessContext::default()
            },
        );
    }

    fn end(
        &mut self,
        at: u64,
        work: RecoveryWork,
        issue: u64,
        spawned: Option<u64>,
        outcome: TurnOutcome,
    ) {
        let mut record = samples::record(RecordKind::TurnEnded);
        if let Record::TurnEnded {
            agent,
            turn,
            mode,
            provenance,
            outcome: actual,
            ..
        } = &mut record
        {
            *agent = work.agent;
            *turn = work.turn;
            *mode = Mode::Managed;
            *provenance = Provenance::Observed;
            *actual = Some(outcome);
        }
        self.observe(
            at,
            record,
            RecoveryWitnessContext {
                attempt: Some(RecoveryAttemptContext {
                    issue_lsn: Lsn(issue),
                    previous_spawn_lsn: spawned.map(Lsn),
                    previous_end_lsn: None,
                    confirmed_session_lsn: None,
                }),
                ..RecoveryWitnessContext::default()
            },
        );
    }

    fn authorize(&mut self) -> RecoveryAuthorized {
        self.state
            .apply_transition(
                Lsn(70),
                RecoveryTransition::Required(&self.required),
                &self.facts,
                &self.history,
            )
            .unwrap();
        self.observe(
            80,
            Record::Resume {
                agent: self.facts.binding.work.agent,
                browser: None,
            },
            RecoveryWitnessContext {
                previous_control: Some(RecoveryPriorControl {
                    agent: self.facts.binding.work.agent,
                    lsn: Some(Lsn(50)),
                }),
                ..RecoveryWitnessContext::default()
            },
        );
        self.facts.through = Lsn(80);
        self.facts.control_lsn = Some(Lsn(80));
        RecoveryAuthorized {
            episode: RecoveryEpisodeId {
                work: self.facts.binding.work,
                origin: Lsn(50),
            },
            binding: self.facts.binding.clone(),
            request_id: Hex32::new([7; 32]),
            request_digest: Hex32::new([8; 32]),
            attribution: RecoveryAttribution::LocalOperator {
                pid: 42,
                uid: Some(501),
            },
            expected_control: Some(Lsn(50)),
            resulting_control: Some(Lsn(80)),
            resume_lsn: Some(Lsn(80)),
            plan: RecoveryPlan::New {
                turn: TurnId::from_parts(61, 4),
                context: RecoveryContext::Fresh {
                    session: None,
                    recap: RecoveryRecap {
                        text: "Recorded role and goal; earlier effects unknown.".into(),
                        sources: vec![Lsn(10), Lsn(15)],
                        delivered_answers: Vec::new(),
                    },
                },
            },
        }
    }

    fn passing_result(&self) -> Record {
        let mut result = samples::record(RecordKind::PredicateResult);
        if let Record::PredicateResult {
            goal,
            provenance,
            pass,
            ..
        } = &mut result
        {
            *goal = self.facts.binding.work.goal;
            *provenance = Provenance::Observed;
            *pass = true;
        }
        result
    }

    fn check(&self, transition: RecoveryTransition<'_>) -> Result<(), RecoveryStateError> {
        self.state
            .validate_transition(transition, &self.facts, &self.history)
    }
}

#[test]
fn typed_boundary_requires_the_latest_attempt_and_exact_reason() {
    let mut fixture = Fixture::new();
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Ok(())
    );
    fixture.required.reason = RecoveryReason::Vanished;
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
    fixture.required.reason = RecoveryReason::HeldOrPausedFailure;
    fixture.facts.attempt.spawn_lsn = Some(Lsn(55));
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
    fixture.facts.attempt.spawn_lsn = Some(Lsn(30));
    fixture.facts.attempt.end_lsn = Some(Lsn(56));
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn typed_vanished_requires_an_observed_end_of_the_latest_recorded_child() {
    let mut fixture = Fixture::new();
    let work = fixture.facts.binding.work;
    fixture.end(55, work, 20, Some(30), TurnOutcome::Vanished);
    fixture.facts.attempt.end_lsn = Some(Lsn(55));
    fixture.required.origin = RecoveryOrigin::ThisBarrier;
    fixture.required.reason = RecoveryReason::Vanished;
    fixture.required.boundary = RecoveryBoundary::Vanished {
        spawn_lsn: Lsn(30),
        end_lsn: Lsn(55),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Ok(())
    );
    fixture.required.boundary = RecoveryBoundary::Vanished {
        spawn_lsn: Lsn(30),
        end_lsn: Lsn(40),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn typed_missing_spawn_refuses_a_terminal_or_any_later_attempt() {
    let mut fixture = Fixture::new();
    fixture.required.origin = RecoveryOrigin::ThisBarrier;
    fixture.required.reason = RecoveryReason::MissingSpawn;
    fixture.required.boundary = RecoveryBoundary::MissingSpawn;
    fixture.facts.attempt = RecoveryAttempt::default();
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Ok(())
    );
    fixture.facts.attempt.end_lsn = Some(Lsn(40));
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
    fixture.facts.attempt.end_lsn = None;
    fixture.facts.attempt.spawn_lsn = Some(Lsn(30));
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn typed_unconfirmed_context_joins_the_actual_requested_confirmation() {
    let mut fixture = Fixture::new();
    let work = fixture.facts.binding.work;
    let session = SessionId::new("previous-confirmed");
    fixture.observe(
        25,
        Record::SessionLearned {
            agent: work.agent,
            turn: TurnId::from_parts(61, 8),
            session: session.clone(),
        },
        RecoveryWitnessContext::default(),
    );
    fixture.spawn(51, work, 20, Some(session), false, Some(25));
    fixture.end(55, work, 20, Some(51), TurnOutcome::NoResult);
    fixture.facts.attempt = RecoveryAttempt {
        spawn_lsn: Some(Lsn(51)),
        end_lsn: Some(Lsn(55)),
        confirmation_lsn: None,
    };
    fixture.required.origin = RecoveryOrigin::ThisBarrier;
    fixture.required.reason = RecoveryReason::UnconfirmedContext;
    fixture.required.boundary = RecoveryBoundary::Unconfirmed {
        spawn_lsn: Lsn(51),
        end_lsn: Lsn(55),
        requested_confirmation_lsn: Lsn(25),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Ok(())
    );
    fixture.facts.attempt.confirmation_lsn = Some(Lsn(54));
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Context)
    );
    fixture.facts.attempt.confirmation_lsn = None;
    fixture.required.boundary = RecoveryBoundary::Unconfirmed {
        spawn_lsn: Lsn(30),
        end_lsn: Lsn(55),
        requested_confirmation_lsn: Lsn(25),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn typed_holds_join_exact_cause_and_named_failure_sources() {
    let mut fixture = Fixture::new();
    let work = fixture.facts.binding.work;
    let first = RecoveryWork {
        turn: TurnId::from_parts(61, 9),
        ..work
    };
    fixture.observe(
        17,
        Record::TurnIssued {
            agent: work.agent,
            goal: work.goal,
            turn: first.turn,
            mode: Mode::Managed,
            lsn_of_decision: Lsn(16),
            task: "first attempt".into(),
            session: None,
            model_selection: fixture.facts.binding.model_selection.clone(),
            answers: Vec::new(),
        },
        RecoveryWitnessContext::default(),
    );
    fixture.end(19, first, 17, None, TurnOutcome::NoResult);
    fixture.observe(
        55,
        Record::AgentHeld {
            agent: work.agent,
            provenance: Provenance::Observed,
            cause: AutomaticHold::RepeatedFastFailure {
                first: first.turn,
                latest: work.turn,
            },
        },
        RecoveryWitnessContext {
            previous_control: Some(RecoveryPriorControl {
                agent: work.agent,
                lsn: Some(Lsn(50)),
            }),
            ..RecoveryWitnessContext::default()
        },
    );
    fixture.facts.control_lsn = Some(Lsn(55));
    fixture.required.origin = RecoveryOrigin::Existing { lsn: Lsn(55) };
    fixture.required.boundary = RecoveryBoundary::Held {
        end_lsn: Lsn(40),
        first: Some(crate::recovery::RecoveryFailure {
            issue_lsn: Lsn(17),
            end_lsn: Lsn(19),
        }),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Ok(())
    );
    fixture.required.boundary = RecoveryBoundary::Held {
        end_lsn: Lsn(40),
        first: None,
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Required(&fixture.required)),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn typed_own_resume_joins_previous_control_and_recorded_attribution() {
    let mut fixture = Fixture::new();
    let mut authorization = fixture.authorize();
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Ok(())
    );
    authorization.expected_control = None;
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Control)
    );
    authorization.expected_control = Some(Lsn(50));
    authorization.attribution = RecoveryAttribution::BrowserSession {
        session_digest: Hex32::new([3; 32]),
    };
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Control)
    );
    authorization.attribution = RecoveryAttribution::LocalOperator {
        pid: 42,
        uid: Some(501),
    };
    fixture.facts.control_lsn = Some(Lsn(81));
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Control)
    );
}

#[test]
fn typed_browser_resume_requires_same_session_request_and_control() {
    let mut fixture = Fixture::new();
    let mut authorization = fixture.authorize();
    let digest = Hex32::new([3; 32]);
    authorization.attribution = RecoveryAttribution::BrowserSession {
        session_digest: digest,
    };
    let browser = BrowserControl {
        request_id: authorization.request_id,
        request_digest: authorization.request_digest,
        control_lsn: Some(Lsn(80)),
        session_digest: digest,
    };
    fixture.observe(
        81,
        Record::Resume {
            agent: fixture.facts.binding.work.agent,
            browser: Some(browser),
        },
        RecoveryWitnessContext {
            previous_control: Some(RecoveryPriorControl {
                agent: fixture.facts.binding.work.agent,
                lsn: Some(Lsn(80)),
            }),
            ..RecoveryWitnessContext::default()
        },
    );
    fixture.facts.through = Lsn(81);
    fixture.facts.control_lsn = Some(Lsn(81));
    authorization.expected_control = Some(Lsn(80));
    authorization.resulting_control = Some(Lsn(81));
    authorization.resume_lsn = Some(Lsn(81));
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Ok(())
    );
    authorization.request_digest = Hex32::new([4; 32]);
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Control)
    );
}

#[test]
fn typed_recap_requires_a_same_goal_carrier_and_its_durable_spawn() {
    let mut fixture = Fixture::new();
    let mut authorization = fixture.authorize();
    let work = fixture.facts.binding.work;
    fixture.observe(
        16,
        Record::HumanAnswer {
            agent: work.agent,
            text: "delivered answer".into(),
            provenance: Some(crate::PeerProvenance::NoRecordedAgent),
            browser_answer: None,
        },
        RecoveryWitnessContext::default(),
    );
    let carried = RecoveryWork {
        turn: TurnId::from_parts(61, 9),
        ..work
    };
    fixture.observe(
        17,
        Record::TurnIssued {
            agent: work.agent,
            goal: work.goal,
            turn: carried.turn,
            mode: Mode::Managed,
            lsn_of_decision: Lsn(16),
            task: "verbatim answer task".into(),
            session: None,
            model_selection: fixture.facts.binding.model_selection.clone(),
            answers: vec![Lsn(16)],
        },
        RecoveryWitnessContext::default(),
    );
    fixture.spawn(18, carried, 17, None, false, None);
    let RecoveryPlan::New {
        context: RecoveryContext::Fresh { recap, .. },
        ..
    } = &mut authorization.plan
    else {
        unreachable!()
    };
    recap.sources.push(Lsn(16));
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Context)
    );
    let RecoveryPlan::New {
        context: RecoveryContext::Fresh { recap, .. },
        ..
    } = &mut authorization.plan
    else {
        unreachable!()
    };
    recap.delivered_answers.push(RecoveryAnswerDelivery {
        answer_lsn: Lsn(16),
        issue_lsn: Lsn(17),
        spawn_lsn: Lsn(18),
    });
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Ok(())
    );
    let RecoveryPlan::New {
        context: RecoveryContext::Fresh { recap, .. },
        ..
    } = &mut authorization.plan
    else {
        unreachable!()
    };
    recap.delivered_answers[0].spawn_lsn = Lsn(30);
    assert_eq!(
        fixture.check(RecoveryTransition::Authorized(&authorization)),
        Err(RecoveryStateError::Context)
    );
}

#[test]
fn recovery_original_json_refuses_unknown_duplicate_and_absent_nested_intent() {
    let mut fixture = Fixture::new();
    let authorization = fixture.authorize();
    let original = serde_json::to_string(&authorization).unwrap();
    assert_eq!(
        serde_json::from_str::<RecoveryAuthorized>(&original).unwrap(),
        authorization
    );
    for replacement in [
        "{\"unexpected\":true}",
        "{\"selector\":null,\"selector\":null}",
    ] {
        let invalid = original.replacen(
            "\"model_selection\":{}",
            &format!("\"model_selection\":{replacement}"),
            1,
        );
        assert_ne!(invalid, original);
        assert!(serde_json::from_str::<RecoveryAuthorized>(&invalid).is_err());
    }
    let invalid = original.replacen("\"ticket\":null,", "", 1);
    assert!(serde_json::from_str::<RecoveryAuthorized>(&invalid).is_err());
    let invalid = original.replacen(
        "\"attribution\":",
        "\"unexpected\":true,\"attribution\":",
        1,
    );
    assert!(serde_json::from_str::<RecoveryAuthorized>(&invalid).is_err());
}

#[test]
fn browser_loss_is_a_tagged_durable_observation_of_only_the_originating_session() {
    use crate::recovery::{RecoveryInvalidated, RecoveryInvalidationEvidence};
    let mut fixture = Fixture::new();
    let mut authorization = fixture.authorize();
    let digest = Hex32::new([3; 32]);
    authorization.attribution = RecoveryAttribution::BrowserSession {
        session_digest: digest,
    };
    authorization.expected_control = fixture.facts.control_lsn;
    authorization.resulting_control = fixture.facts.control_lsn;
    authorization.resume_lsn = None;
    fixture
        .state
        .apply_transition(
            Lsn(90),
            RecoveryTransition::Authorized(&authorization),
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    let claim = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: Lsn(90),
    };
    let mut closed = RecoveryInvalidated {
        claim,
        evidence: RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
            session_digest: digest,
        },
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(90), &fixture.history),
        Ok(())
    );
    closed.evidence = RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
        session_digest: Hex32::new([4; 32]),
    };
    let before = fixture.state.clone();
    assert_eq!(
        fixture
            .state
            .apply_invalidation(Lsn(100), &closed, Lsn(90), &fixture.history),
        Err(RecoveryStateError::Binding)
    );
    assert_eq!(fixture.state, before);
    closed.evidence = RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
        session_digest: digest,
    };
    fixture
        .state
        .apply_invalidation(Lsn(100), &closed, Lsn(90), &fixture.history)
        .unwrap();
    let receipt = fixture
        .state
        .receipt(authorization.request_id, authorization.request_digest)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.claim, claim);
    assert_eq!(
        receipt.state,
        &super::super::RecoveryClaimState::Invalidated {
            invalidation_lsn: Lsn(100),
            evidence: closed.evidence
        }
    );
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(100), &fixture.history),
        Err(RecoveryStateError::Lifecycle)
    );
}

#[test]
fn invalidation_requires_the_actual_later_change_even_if_the_active_binding_is_gone() {
    use crate::recovery::{
        RecoveryInvalidated, RecoveryInvalidationEvidence, RecoveryInvalidationReason,
    };
    let mut fixture = Fixture::new();
    let authorization = fixture.authorize();
    fixture
        .state
        .apply_transition(
            Lsn(90),
            RecoveryTransition::Authorized(&authorization),
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    let claim = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: Lsn(90),
    };
    fixture.observe(
        95,
        Record::GoalCleared {
            agent: authorization.binding.work.agent,
            goal: authorization.binding.work.goal,
        },
        RecoveryWitnessContext::default(),
    );
    let mut closed = RecoveryInvalidated {
        claim,
        evidence: RecoveryInvalidationEvidence::RecordedChange {
            reason: RecoveryInvalidationReason::GoalChanged,
            lsn: Lsn(95),
        },
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(95), &fixture.history),
        Ok(())
    );
    closed.evidence = RecoveryInvalidationEvidence::RecordedChange {
        reason: RecoveryInvalidationReason::ControlChanged,
        lsn: Lsn(95),
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(95), &fixture.history),
        Err(RecoveryStateError::Binding)
    );
    closed.evidence = RecoveryInvalidationEvidence::RecordedChange {
        reason: RecoveryInvalidationReason::GoalChanged,
        lsn: Lsn(100),
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(95), &fixture.history),
        Err(RecoveryStateError::Position)
    );
    closed.evidence = RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
        session_digest: Hex32::new([3; 32]),
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(95), &fixture.history),
        Err(RecoveryStateError::Binding)
    );
}

#[test]
fn raw_pass_and_a_question_do_not_supply_qualified_completion_invalidation() {
    use crate::recovery::{
        RecoveryInvalidated, RecoveryInvalidationEvidence, RecoveryInvalidationReason,
    };
    let mut fixture = Fixture::new();
    let authorization = fixture.authorize();
    fixture
        .state
        .apply_transition(
            Lsn(90),
            RecoveryTransition::Authorized(&authorization),
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    let claim = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: Lsn(90),
    };
    let result = fixture.passing_result();
    fixture.observe(95, result.clone(), RecoveryWitnessContext::default());
    let mut closed = RecoveryInvalidated {
        claim,
        evidence: RecoveryInvalidationEvidence::RecordedChange {
            reason: RecoveryInvalidationReason::GoalCompleted,
            lsn: Lsn(95),
        },
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(95), &fixture.history),
        Err(RecoveryStateError::Binding)
    );
    let predicate = crate::Predicate::Command {
        argv: vec!["check".into()],
        timeout_s: 1,
    };
    let target = super::super::history::qualified_completion(
        authorization.binding.work.agent,
        authorization.binding.work.goal,
        &predicate,
        &result,
    )
    .unwrap();
    fixture.observe(
        96,
        result,
        RecoveryWitnessContext {
            qualified_completion: Some(target),
            ..RecoveryWitnessContext::default()
        },
    );
    closed.evidence = RecoveryInvalidationEvidence::RecordedChange {
        reason: RecoveryInvalidationReason::GoalCompleted,
        lsn: Lsn(96),
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(96), &fixture.history),
        Ok(())
    );
    fixture.observe(
        97,
        Record::HumanQuestion {
            agent: authorization.binding.work.agent,
            text: "wait for input".into(),
        },
        RecoveryWitnessContext::default(),
    );
    closed.evidence = RecoveryInvalidationEvidence::RecordedChange {
        reason: RecoveryInvalidationReason::GoalCompleted,
        lsn: Lsn(97),
    };
    assert_eq!(
        fixture
            .state
            .validate_invalidation(&closed, Lsn(97), &fixture.history),
        Err(RecoveryStateError::Binding)
    );
    assert!(matches!(
        fixture
            .state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap()
            .state,
        super::super::RecoveryClaimState::Pending
    ));
}

#[test]
fn issued_work_consumes_only_its_own_physical_attempt_and_keeps_the_original_receipt() {
    let mut fixture = Fixture::new();
    let authorization = fixture.authorize();
    fixture
        .state
        .apply_transition(
            Lsn(90),
            RecoveryTransition::Authorized(&authorization),
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    let claim = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: Lsn(90),
    };
    fixture.facts.through = Lsn(90);
    let mut issue = fixture.facts.source_issue.clone();
    issue.lsn = Lsn(100);
    issue.issued.work.turn = TurnId::from_parts(61, 4);
    fixture
        .state
        .apply_transition(
            Lsn(100),
            RecoveryTransition::Issued {
                claim,
                issue: &issue,
            },
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    fixture.facts.through = Lsn(100);
    fixture.facts.binding.work = issue.issued.work;
    fixture.facts.binding.issue_lsn = issue.lsn;
    fixture.facts.source_issue = issue.clone();
    fixture.facts.attempt = RecoveryAttempt::default();
    fixture.spawn(110, issue.issued.work, 100, None, false, None);
    let source = fixture.history.at(Lsn(110)).unwrap();
    assert_eq!(
        fixture.check(RecoveryTransition::Consumed {
            claim,
            issue: &issue,
            evidence: source
        }),
        Ok(())
    );
    fixture.spawn(111, issue.issued.work, 20, None, false, None);
    assert_eq!(
        fixture.check(RecoveryTransition::Consumed {
            claim,
            issue: &issue,
            evidence: fixture.history.at(Lsn(111)).unwrap()
        }),
        Err(RecoveryStateError::Binding)
    );
    let source = fixture.history.at(Lsn(110)).unwrap().clone();
    fixture
        .state
        .apply_transition(
            Lsn(110),
            RecoveryTransition::Consumed {
                claim,
                issue: &issue,
                evidence: &source,
            },
            &fixture.facts,
            &fixture.history,
        )
        .unwrap();
    let receipt = fixture
        .state
        .receipt(authorization.request_id, authorization.request_digest)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.authorization, &authorization);
    assert_eq!(
        receipt.state,
        &super::super::RecoveryClaimState::Consumed {
            issue,
            evidence_lsn: Lsn(110)
        }
    );
}
