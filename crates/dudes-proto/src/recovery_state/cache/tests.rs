use std::cell::RefCell;

use super::*;
use crate::record::{Entry, Record};
use crate::recovery::{
    RecoveryClaimRef, RecoveryContext, RecoveryOrigin, RecoveryRecap, RecoveryRequired,
};
use crate::recovery_state::{RecoveryFacts, RecoveryStateError};
use dudes_types::{AgentId, GoalId, Mode, ModelSelection, Provider, Timestamp};

#[derive(Clone, Default)]
struct Memory {
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    reads: RefCell<Vec<Vec<u8>>>,
}

impl RecoveryCacheReader for Memory {
    type Error = RecoveryCacheError;
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        self.reads.borrow_mut().push(key.to_vec());
        Ok(self.rows.get(key).cloned())
    }
    fn prefix(&self, prefix: &[u8]) -> Result<Vec<RecoveryCacheRow>, Self::Error> {
        Ok(self
            .rows
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }
}

impl Memory {
    fn apply(&mut self, updates: Vec<RecoveryCacheUpdate>) {
        for update in updates {
            if let Some(value) = update.value {
                self.rows.insert(update.key, value);
            } else {
                self.rows.remove(&update.key);
            }
        }
    }
}

fn facts() -> RecoveryFacts {
    let work = RecoveryWork {
        agent: AgentId::from_parts(21, 1),
        goal: GoalId::from_parts(21, 2),
        turn: TurnId::from_parts(21, 3),
    };
    RecoveryFacts {
        attempt: crate::recovery_state::RecoveryAttempt::default(),
        through: Lsn(100),
        binding: RecoveryBinding {
            work,
            issue_lsn: Lsn(20),
            hired_lsn: Lsn(10),
            ticket: None,
            worktree: None,
            provider: Provider::Codex,
            model_selection: Some(ModelSelection::provider_default()),
        },
        source_issue: RecoveryIssue {
            lsn: Lsn(20),
            issued: crate::recovery::RecoveryIssued {
                work,
                task_digest: Hex32::new([3; 32]),
                answers: vec![Lsn(15)],
                model_selection: Some(ModelSelection::provider_default()),
                session: None,
            },
        },
        control_lsn: Some(Lsn(90)),
        control: crate::recovery_state::history::RecoveryStandingControl::default(),
        confirmed_session: None,
    }
}

fn required(facts: &RecoveryFacts, missing_spawn: bool) -> RecoveryRequired {
    RecoveryRequired {
        binding: facts.binding.clone(),
        origin: if missing_spawn {
            RecoveryOrigin::ThisBarrier
        } else {
            RecoveryOrigin::Existing {
                lsn: facts
                    .control_lsn
                    .expect("held fixture has a control boundary"),
            }
        },
        reason: if missing_spawn {
            RecoveryReason::MissingSpawn
        } else {
            RecoveryReason::HeldOrPausedFailure
        },
        boundary: if missing_spawn {
            crate::recovery::RecoveryBoundary::MissingSpawn
        } else {
            crate::recovery::RecoveryBoundary::Paused { end_lsn: Lsn(80) }
        },
        predecessor: None,
    }
}

fn authorization(
    episode: &RecoveryEpisode,
    facts: &RecoveryFacts,
    request: u8,
) -> RecoveryAuthorized {
    RecoveryAuthorized {
        episode: episode.id,
        binding: episode.binding.clone(),
        request_id: Hex32::new([request; 32]),
        request_digest: Hex32::new([request + 1; 32]),
        attribution: crate::recovery::RecoveryAttribution::LocalOperator { pid: 42, uid: None },
        expected_control: facts.control_lsn,
        resulting_control: facts.control_lsn,
        resume_lsn: None,
        plan: if episode.reason == RecoveryReason::MissingSpawn {
            RecoveryPlan::RepeatIssued {
                issue: facts.source_issue.clone(),
                session: None,
            }
        } else {
            RecoveryPlan::New {
                turn: TurnId::from_parts(21, u128::from(request) + 10),
                context: RecoveryContext::Fresh {
                    session: None,
                    recap: RecoveryRecap {
                        text: "Earlier effects are unknown.".into(),
                        sources: vec![facts.source_issue.lsn],
                        delivered_answers: Vec::new(),
                    },
                },
            }
        },
    }
}

struct Harness {
    memory: Memory,
    full: RecoveryState,
    history: RecoveryHistory,
    through: Lsn,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            memory: Memory::default(),
            full: RecoveryState::default(),
            history: RecoveryHistory::default(),
            through: Lsn(100),
        }
    }
}

impl Harness {
    fn step(
        &mut self,
        at: u64,
        event: RecoveryEvent<'_>,
        facts: &RecoveryFacts,
    ) -> Vec<RecoveryCacheUpdate> {
        let mut cache = RecoveryCache::load(&self.memory, self.through, event, &[]).unwrap();
        self.full.apply(Lsn(at), event, facts).unwrap();
        cache.state_mut().apply(Lsn(at), event, facts).unwrap();
        let updates = cache.updates().unwrap();
        self.memory.apply(updates.clone());
        self.through = Lsn(at);
        let (cold, history) = RecoveryCache::read_all(&self.memory, self.through).unwrap();
        assert_eq!(cold, self.full);
        assert_eq!(history, self.history);
        updates
    }

    fn witness(&mut self, at: u64, entry: &Entry) {
        let hash = |_: &[u8]| Hex32::new([9; 32]);
        let mut cache = RecoveryCache::load_history(&self.memory, self.through, Lsn(at)).unwrap();
        self.history.observe(Lsn(at), entry, hash).unwrap();
        cache.history_mut().observe(Lsn(at), entry, hash).unwrap();
        self.memory.apply(cache.updates().unwrap());
        self.through = Lsn(at);
        let (cold, history) = RecoveryCache::read_all(&self.memory, self.through).unwrap();
        assert_eq!(cold, self.full);
        assert_eq!(history, self.history);
    }
}

fn claimed() -> (Harness, RecoveryFacts, RecoveryAuthorized, RecoveryClaimRef) {
    let mut h = Harness::default();
    let mut facts = facts();
    let barrier = required(&facts, false);
    h.step(200, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let auth = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 7);
    h.step(300, RecoveryEvent::Authorized(&auth), &facts);
    facts.through = h.through;
    let claim = RecoveryClaimRef {
        episode: auth.episode,
        authorization_lsn: Lsn(300),
    };
    (h, facts, auth, claim)
}

fn another_required_episode(h: &mut Harness) -> RecoveryFacts {
    let mut facts = facts();
    facts.binding.work = RecoveryWork {
        agent: AgentId::from_parts(77, 1),
        goal: GoalId::from_parts(77, 2),
        turn: TurnId::from_parts(77, 3),
    };
    facts.binding.hired_lsn = Lsn(310);
    facts.binding.issue_lsn = Lsn(320);
    facts.source_issue.lsn = Lsn(320);
    facts.source_issue.issued.work = facts.binding.work;
    facts.source_issue.issued.answers = vec![Lsn(315)];
    facts.control_lsn = Some(Lsn(330));
    facts.through = Lsn(350);
    h.through = facts.through;
    let barrier = required(&facts, false);
    h.step(400, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    facts
}

#[test]
fn scoped_folding_and_cold_reconstruction_agree_through_two_repeat_supersessions() {
    let (mut h, mut facts, auth, claim) = claimed();
    let mut issued = facts.source_issue.issued.clone();
    let RecoveryPlan::New { turn, .. } = auth.plan else {
        unreachable!()
    };
    issued.work.turn = turn;
    h.step(
        400,
        RecoveryEvent::Issued {
            claim,
            issued: &issued,
        },
        &facts,
    );
    facts.through = h.through;
    facts.binding.work = issued.work;
    facts.binding.issue_lsn = Lsn(400);
    facts.source_issue = RecoveryIssue {
        lsn: Lsn(400),
        issued,
    };
    let mut barrier = required(&facts, true);
    barrier.predecessor = Some(claim);
    h.step(500, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let first_repeat = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 8);
    h.step(600, RecoveryEvent::Authorized(&first_repeat), &facts);
    facts.through = h.through;
    let repeat_claim = RecoveryClaimRef {
        episode: first_repeat.episode,
        authorization_lsn: Lsn(600),
    };
    barrier.predecessor = Some(repeat_claim);
    h.step(700, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let second_repeat = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 9);
    h.step(800, RecoveryEvent::Authorized(&second_repeat), &facts);
    facts.through = h.through;
    let final_claim = RecoveryClaimRef {
        episode: second_repeat.episode,
        authorization_lsn: Lsn(800),
    };
    h.step(
        900,
        RecoveryEvent::Consumed {
            claim: final_claim,
            issue: &facts.source_issue,
            session: None,
        },
        &facts,
    );
    assert_eq!(
        h.full.current(facts.binding.work).unwrap().id.origin,
        Lsn(700)
    );
    assert_eq!(h.full.planned_turns[&turn], Lsn(300));
    assert_eq!(
        h.full.current(claim.episode.work).unwrap().id,
        claim.episode
    );
    assert_eq!(
        h.full
            .receipt(second_repeat.request_id, second_repeat.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Consumed {
            issue: facts.source_issue.clone(),
            evidence_lsn: Lsn(900)
        }
    );
    h.memory.reads.borrow_mut().clear();
    let cache = RecoveryCache::load_receipt(&h.memory, h.through, auth.request_id).unwrap();
    assert_eq!(
        cache
            .state()
            .receipt(auth.request_id, auth.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Superseded {
            issue: facts.source_issue.clone(),
            successor: first_repeat.episode,
            barrier_lsn: Lsn(500)
        }
    );
    assert!(
        !h.memory
            .reads
            .borrow()
            .contains(&Key::Authorization(Lsn(600)).bytes())
    );
    assert!(cache.updates().unwrap().is_empty());
}

#[test]
fn authorization_intent_and_episode_intent_are_not_rewritten_by_claim_updates() {
    let (mut h, mut facts, auth, claim) = claimed();
    let authorization_bytes = h.memory.rows[&Key::Authorization(Lsn(300)).bytes()].clone();
    let episode_bytes = h.memory.rows[&Key::Episode(auth.episode).bytes()].clone();
    facts.through = Lsn(350);
    h.through = facts.through;
    let updates = h.step(
        400,
        RecoveryEvent::Invalidated {
            claim,
            evidence: crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
                lsn: Lsn(350),
            },
        },
        &facts,
    );
    assert!(
        !updates
            .iter()
            .any(|row| row.key == Key::Authorization(Lsn(300)).bytes()
                || row.key == Key::Episode(auth.episode).bytes())
    );
    assert_eq!(
        h.memory.rows[&Key::Authorization(Lsn(300)).bytes()],
        authorization_bytes
    );
    assert_eq!(
        h.memory.rows[&Key::Episode(auth.episode).bytes()],
        episode_bytes
    );
    assert_eq!(h.full.planned_turns.len(), 1);
    assert_eq!(
        h.full
            .receipt(auth.request_id, auth.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Invalidated {
            invalidation_lsn: Lsn(400),
            evidence: crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
                lsn: Lsn(350)
            }
        }
    );
}

#[test]
fn missing_required_claim_link_intent_and_reverse_indexes_refuse_without_storage_changes() {
    let (h, facts, auth, _) = claimed();
    let RecoveryPlan::New { turn, .. } = auth.plan else {
        unreachable!()
    };
    for key in [
        Key::Cut,
        Key::Episode(auth.episode),
        Key::EpisodeClaim(auth.episode),
        Key::Authorization(Lsn(300)),
        Key::Claim(Lsn(300)),
        Key::Request(auth.request_id),
        Key::Planned(turn),
    ] {
        let mut memory = h.memory.clone();
        memory.rows.remove(&key.bytes());
        let before = memory.rows.clone();
        assert!(
            RecoveryCache::read_all(&memory, h.through).is_err(),
            "{key:?}"
        );
        let mut other = auth.clone();
        other.request_id = Hex32::new([99; 32]);
        assert!(
            RecoveryCache::load(&memory, h.through, RecoveryEvent::Authorized(&other), &[])
                .is_err(),
            "{key:?}"
        );
        assert_eq!(memory.rows, before);
        assert_eq!(facts.through, Lsn(300));
    }
}

#[test]
fn an_absent_new_request_is_distinct_from_a_dangling_stored_request() {
    let (mut h, _, auth, _) = claimed();
    let missing = Hex32::new([99; 32]);
    let cache = RecoveryCache::load_receipt(&h.memory, h.through, missing).unwrap();
    assert_eq!(
        cache.state().receipt(missing, Hex32::new([0; 32])),
        Ok(None)
    );
    assert!(cache.updates().unwrap().is_empty());
    h.memory.rows.remove(&Key::Claim(Lsn(300)).bytes());
    assert!(matches!(
        RecoveryCache::load_receipt(&h.memory, h.through, auth.request_id),
        Err(RecoveryCacheError::MissingCell)
    ));
}

#[test]
fn a_missing_unclaimed_episode_link_cannot_be_read_as_no_authorization() {
    let mut h = Harness::default();
    let mut facts = facts();
    let barrier = required(&facts, false);
    h.step(200, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let auth = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 7);
    h.memory
        .rows
        .remove(&Key::EpisodeClaim(auth.episode).bytes());
    assert!(matches!(
        RecoveryCache::load(&h.memory, h.through, RecoveryEvent::Authorized(&auth), &[]),
        Err(RecoveryCacheError::MissingCell)
    ));
}

#[test]
fn key_value_mismatch_future_cut_and_wrong_cell_kind_refuse() {
    let (h, _, auth, _) = claimed();
    for cell in [
        Cell::Current {
            work: auth.episode.work,
            episode: RecoveryEpisodeId {
                origin: Lsn(91),
                ..auth.episode
            },
        },
        Cell::Request {
            request: auth.request_id,
            authorization: Lsn(301),
        },
        Cell::Planned {
            turn: TurnId::from_parts(99, 99),
            authorization: Lsn(300),
        },
    ] {
        let mut memory = h.memory.clone();
        memory
            .rows
            .insert(cell.key().bytes(), encoded(&cell).unwrap());
        assert!(RecoveryCache::read_all(&memory, h.through).is_err());
    }
    let mut memory = h.memory.clone();
    memory.rows.insert(
        Key::Claim(Lsn(300)).bytes(),
        encoded(&Cell::Cut { lsn: Lsn(300) }).unwrap(),
    );
    assert!(matches!(
        RecoveryCache::read_all(&memory, h.through),
        Err(RecoveryCacheError::Identity)
    ));
}

#[test]
fn explicitly_requested_source_witnesses_do_not_default_when_absent_or_misowned() {
    let mut h = Harness::default();
    let facts = facts();
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TurnIssued {
            agent: facts.binding.work.agent,
            goal: facts.binding.work.goal,
            turn: facts.binding.work.turn,
            task: "literal issued task".into(),
            session: None,
            mode: Mode::Managed,
            model_selection: None,
            lsn_of_decision: Lsn(110),
            answers: Vec::new(),
        },
    };
    h.witness(120, &entry);
    let reference = RecoveryWitnessRef {
        lsn: Lsn(120),
        kind: crate::RecordKind::TurnIssued,
        owner: super::super::history::RecoveryWitnessOwner::Work(facts.binding.work),
    };
    let unboxed = h.history.get(reference).unwrap();
    let expected = serde_json::json!({ "kind": "witness", "value": unboxed });
    let stored: serde_json::Value =
        serde_json::from_slice(&h.memory.rows[&Key::Witness(reference.lsn).bytes()]).unwrap();
    assert_eq!(stored, expected);
    let barrier = required(&facts, false);
    let cache = RecoveryCache::load(
        &h.memory,
        h.through,
        RecoveryEvent::Required(&barrier),
        &[reference],
    )
    .unwrap();
    assert_eq!(
        cache
            .history()
            .get(reference)
            .unwrap()
            .issue()
            .unwrap()
            .issued
            .task_digest,
        Hex32::new([9; 32])
    );
    assert!(cache.updates().unwrap().is_empty());
    assert!(matches!(
        RecoveryCache::load(
            &h.memory,
            h.through,
            RecoveryEvent::Required(&barrier),
            &[RecoveryWitnessRef {
                lsn: Lsn(121),
                ..reference
            }]
        ),
        Err(RecoveryCacheError::MissingCell)
    ));
    assert!(matches!(
        RecoveryCache::load(
            &h.memory,
            h.through,
            RecoveryEvent::Required(&barrier),
            &[RecoveryWitnessRef {
                kind: crate::RecordKind::SessionLearned,
                ..reference
            }]
        ),
        Err(RecoveryCacheError::History(RecoveryHistoryError::Identity))
    ));
}

#[test]
fn malformed_history_cell_owner_does_not_enter_cold_state() {
    let mut h = Harness::default();
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::Pause {
            agent: facts().binding.work.agent,
            browser: None,
        },
    };
    h.witness(120, &entry);
    let mut cell: Cell =
        serde_json::from_slice(&h.memory.rows[&Key::Witness(Lsn(120)).bytes()]).unwrap();
    let Cell::Witness { value } = &mut cell else {
        unreachable!()
    };
    value.reference.owner = super::super::history::RecoveryWitnessOwner::Worktree(
        dudes_types::WorktreeId::from_bytes([1; 16]),
    );
    h.memory
        .rows
        .insert(Key::Witness(Lsn(120)).bytes(), encoded(&cell).unwrap());
    assert!(matches!(
        RecoveryCache::read_all(&h.memory, h.through),
        Err(RecoveryCacheError::History(RecoveryHistoryError::Identity))
    ));
}

#[test]
fn a_rejected_transition_changes_neither_loaded_state_nor_stored_cells() {
    let (h, facts, auth, _) = claimed();
    let mut other = auth.clone();
    other.request_id = Hex32::new([99; 32]);
    let mut cache =
        RecoveryCache::load(&h.memory, h.through, RecoveryEvent::Authorized(&other), &[]).unwrap();
    let before = cache.state().clone();
    assert_eq!(
        cache
            .state_mut()
            .apply(Lsn(400), RecoveryEvent::Authorized(&other), &facts),
        Err(RecoveryStateError::Claimed)
    );
    assert_eq!(cache.state(), &before);
    assert!(cache.updates().unwrap().is_empty());
}

#[test]
fn unrelated_terminal_history_does_not_expand_receipt_reads_or_updates() {
    let (mut h, _, original, _) = claimed();
    h.memory.reads.borrow_mut().clear();
    let before = RecoveryCache::load_receipt(&h.memory, h.through, original.request_id).unwrap();
    assert!(before.updates().unwrap().is_empty());
    let expected_keys = h.memory.reads.borrow().clone();

    let mut facts = another_required_episode(&mut h);
    let other = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 37);
    h.step(500, RecoveryEvent::Authorized(&other), &facts);
    facts.through = Lsn(550);
    h.through = facts.through;
    h.step(
        600,
        RecoveryEvent::Invalidated {
            claim: RecoveryClaimRef {
                episode: other.episode,
                authorization_lsn: Lsn(500),
            },
            evidence: crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
                lsn: Lsn(550),
            },
        },
        &facts,
    );

    h.memory.reads.borrow_mut().clear();
    let after = RecoveryCache::load_receipt(&h.memory, h.through, original.request_id).unwrap();
    assert_eq!(*h.memory.reads.borrow(), expected_keys);
    assert!(after.updates().unwrap().is_empty());
    assert_eq!(
        after
            .state()
            .receipt(original.request_id, original.request_digest)
            .unwrap(),
        before
            .state()
            .receipt(original.request_id, original.request_digest)
            .unwrap()
    );
}

#[test]
fn a_nonzero_contract_cut_cannot_stand_in_for_missing_episode_intent() {
    let mut memory = Memory::default();
    let cut = Cell::Cut { lsn: Lsn(200) };
    memory
        .rows
        .insert(cut.key().bytes(), encoded(&cut).unwrap());
    assert!(matches!(
        RecoveryCache::read_all(&memory, Lsn(200)),
        Err(RecoveryCacheError::MissingCell)
    ));
}

#[test]
fn corrupted_stored_authorization_intent_is_not_deserialized_into_authority() {
    struct AuthorizationCorruption {
        name: &'static str,
        edit: fn(&mut RecoveryAuthorized),
        expected: RecoveryCacheError,
    }

    let (h, _, auth, _) = claimed();
    let cases = [
        AuthorizationCorruption {
            name: "resulting_control",
            edit: |value| value.resulting_control = None,
            expected: RecoveryCacheError::Identity,
        },
        AuthorizationCorruption {
            name: "resume",
            edit: |value| value.resume_lsn = Some(Lsn(250)),
            expected: RecoveryCacheError::Identity,
        },
        AuthorizationCorruption {
            name: "provider",
            edit: |value| value.binding.provider = Provider::Claude,
            expected: RecoveryCacheError::Identity,
        },
        AuthorizationCorruption {
            name: "original_turn",
            edit: |value| {
                let RecoveryPlan::New { turn, .. } = &mut value.plan else {
                    unreachable!()
                };
                *turn = value.episode.work.turn;
            },
            expected: RecoveryCacheError::MissingCell,
        },
        AuthorizationCorruption {
            name: "future_recap_source",
            edit: |value| {
                let RecoveryPlan::New {
                    context: RecoveryContext::Fresh { recap, .. },
                    ..
                } = &mut value.plan
                else {
                    unreachable!()
                };
                recap.sources = vec![Lsn(301)];
            },
            expected: RecoveryCacheError::Identity,
        },
    ];
    for AuthorizationCorruption {
        name,
        edit,
        expected,
    } in cases
    {
        let mut memory = h.memory.clone();
        let mut changed = auth.clone();
        edit(&mut changed);
        if expected == RecoveryCacheError::MissingCell {
            let RecoveryPlan::New { turn, .. } = &changed.plan else {
                unreachable!()
            };
            assert!(!memory.rows.contains_key(&Key::Planned(*turn).bytes()));
        }
        let cell = Cell::Authorization {
            lsn: Lsn(300),
            value: Box::new(changed),
        };
        memory
            .rows
            .insert(cell.key().bytes(), encoded(&cell).unwrap());
        let before = memory.rows.clone();
        assert_eq!(
            RecoveryCache::load_receipt(&memory, h.through, auth.request_id).err(),
            Some(expected),
            "{name}: hot refusal"
        );
        assert_eq!(
            RecoveryCache::read_all(&memory, h.through).err(),
            Some(expected),
            "{name}: cold refusal"
        );
        assert_eq!(memory.rows, before);
    }
}

#[test]
fn an_omitted_nullable_claim_link_is_not_decoded_as_unclaimed() {
    let (mut h, _, auth, _) = claimed();
    let key = Key::EpisodeClaim(auth.episode).bytes();
    let mut cell: serde_json::Value = serde_json::from_slice(&h.memory.rows[&key]).unwrap();
    cell.as_object_mut().unwrap().remove("authorization");
    h.memory
        .rows
        .insert(key, serde_json::to_vec(&cell).unwrap());
    assert!(matches!(
        RecoveryCache::load_receipt(&h.memory, h.through, auth.request_id),
        Err(RecoveryCacheError::Encoding)
    ));
}

#[test]
fn cold_validation_refuses_a_missing_current_root() {
    let mut h = Harness::default();
    let mut facts = facts();
    let barrier = required(&facts, false);
    h.step(200, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let before_state = h.full.clone();
    assert_eq!(
        h.full
            .apply(Lsn(300), RecoveryEvent::Required(&barrier), &facts),
        Err(RecoveryStateError::EpisodeAlias)
    );
    assert_eq!(h.full, before_state);

    h.memory
        .rows
        .remove(&Key::Current(facts.binding.work).bytes());
    let before_rows = h.memory.rows.clone();
    assert!(matches!(
        RecoveryCache::read_all(&h.memory, h.through),
        Err(RecoveryCacheError::MissingCell)
    ));
    assert_eq!(h.memory.rows, before_rows);
}

#[test]
fn cold_validation_refuses_a_missing_planned_root() {
    let (mut h, _, original, _) = claimed();
    let facts = another_required_episode(&mut h);
    let mut other = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 37);
    let RecoveryPlan::New { turn: reserved, .. } = original.plan else {
        unreachable!()
    };
    let RecoveryPlan::New { turn, .. } = &mut other.plan else {
        unreachable!()
    };
    *turn = reserved;
    let before_state = h.full.clone();
    assert_eq!(
        h.full
            .apply(Lsn(500), RecoveryEvent::Authorized(&other), &facts),
        Err(RecoveryStateError::PlannedTurn)
    );
    assert_eq!(h.full, before_state);

    h.memory.rows.remove(&Key::Planned(reserved).bytes());
    let before_rows = h.memory.rows.clone();
    assert!(matches!(
        RecoveryCache::read_all(&h.memory, h.through),
        Err(RecoveryCacheError::MissingCell)
    ));
    assert_eq!(h.memory.rows, before_rows);
}

#[test]
fn cold_validation_refuses_a_missing_request_root() {
    let (mut h, _, original, _) = claimed();
    let facts = another_required_episode(&mut h);
    let mut other = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 37);
    other.request_id = original.request_id;
    let before_state = h.full.clone();
    assert_eq!(
        h.full
            .apply(Lsn(500), RecoveryEvent::Authorized(&other), &facts),
        Err(RecoveryStateError::DuplicateRequest)
    );
    assert_eq!(h.full, before_state);

    h.memory
        .rows
        .remove(&Key::Request(original.request_id).bytes());
    let before_rows = h.memory.rows.clone();
    assert!(matches!(
        RecoveryCache::read_all(&h.memory, h.through),
        Err(RecoveryCacheError::MissingCell)
    ));
    assert_eq!(h.memory.rows, before_rows);
}

#[test]
fn browser_unavailability_keeps_its_tag_through_scoped_and_cold_receipts() {
    let mut h = Harness::default();
    let mut facts = facts();
    let barrier = required(&facts, false);
    h.step(200, RecoveryEvent::Required(&barrier), &facts);
    facts.through = h.through;
    let mut auth = authorization(h.full.current(facts.binding.work).unwrap(), &facts, 7);
    let digest = Hex32::new([11; 32]);
    auth.attribution = crate::recovery::RecoveryAttribution::BrowserSession {
        session_digest: digest,
    };
    h.step(300, RecoveryEvent::Authorized(&auth), &facts);
    facts.through = h.through;
    let claim = RecoveryClaimRef {
        episode: auth.episode,
        authorization_lsn: Lsn(300),
    };
    let evidence = crate::recovery::RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
        session_digest: digest,
    };
    h.step(400, RecoveryEvent::Invalidated { claim, evidence }, &facts);
    let before = h.memory.rows.clone();
    let scoped = RecoveryCache::load_receipt(&h.memory, h.through, auth.request_id).unwrap();
    let receipt = scoped
        .state()
        .receipt(auth.request_id, auth.request_digest)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.claim, claim);
    assert_eq!(
        receipt.state,
        &RecoveryClaimState::Invalidated {
            invalidation_lsn: Lsn(400),
            evidence
        }
    );
    assert!(scoped.updates().unwrap().is_empty());
    assert_eq!(h.memory.rows, before);
    assert_eq!(
        RecoveryCache::read_all(&h.memory, h.through).unwrap().0,
        h.full
    );
}
