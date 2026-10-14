use super::work_test_support as common;
use super::*;
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{
    AdoptionKey, Liveness, NativeBoundaryAcknowledgement, NativeBoundaryReceipt,
    OwnedTargetEvidence, OwnedTargetState, OwnedTargets, OwnedTargetsStatus, SpawnResult,
};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

type TestDaemon = Daemon<
    MockNativeHost,
    common::ScriptedRunner,
    common::TestClock,
    dudes_supervisor::testing::FakeRng,
>;

fn same_recorded_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
}

#[derive(Clone)]
struct MockNativeHost {
    inner: common::SharedHost,
    state: Arc<Mutex<MockNativeState>>,
}

#[derive(Default)]
struct MockNativeState {
    targets: BTreeMap<u32, MockNativeTarget>,
    acknowledgements: Vec<Acknowledgement>,
    answers: VecDeque<NativeBoundaryAcknowledgement>,
    next_key: Option<AdoptionKey>,
}

struct MockNativeTarget {
    key: AdoptionKey,
    cwd: PathBuf,
    receipt: NativeBoundaryReceipt,
    absent: bool,
    observed: bool,
}

#[derive(Clone)]
struct Acknowledgement {
    key: AdoptionKey,
    receipt: NativeBoundaryReceipt,
    answer: NativeBoundaryAcknowledgement,
}

impl MockNativeHost {
    fn new() -> Self {
        Self {
            inner: common::SharedHost::new(),
            state: Arc::new(Mutex::new(MockNativeState::default())),
        }
    }

    fn expose_absence(&self, key: &AdoptionKey) -> NativeBoundaryReceipt {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let target = state.targets.get_mut(&key.pid).expect("mock spawn target");
        assert!(same_recorded_key(&target.key, key));
        target.absent = true;
        self.inner
            .answer(key.pid, dudes_supervisor::host::ProbeAnswer::NoSuchPid);
        target.receipt.clone()
    }

    fn answer_next(&self, answer: NativeBoundaryAcknowledgement) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answers
            .push_back(answer);
    }

    fn reuse_next_key(&self, key: &AdoptionKey) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_key = Some(key.clone());
    }

    fn acknowledgements(&self) -> Vec<Acknowledgement> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .acknowledgements
            .clone()
    }
}

impl ProcessHost for MockNativeHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let SpawnResult::Spawned(key) = self.inner.spawn(spec, install) else {
            panic!("the incumbent mock host must spawn")
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = state.next_key.take().unwrap_or(key);
        state.targets.insert(
            key.pid,
            MockNativeTarget {
                key: key.clone(),
                cwd: spec.cwd.clone(),
                receipt: NativeBoundaryReceipt::fresh(),
                absent: false,
                observed: false,
            },
        );
        SpawnResult::Spawned(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.targets.get(&key.pid) {
            Some(target) if same_recorded_key(&target.key, key) => {
                if target.absent {
                    Liveness::Gone
                } else {
                    Liveness::Alive
                }
            }
            Some(_) => Liveness::Unknown,
            None => self.inner.alive(key),
        }
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let count = state.targets.len();
        let targets: Vec<_> = state
            .targets
            .values_mut()
            .take(limit.min(dudes_supervisor::host::MAX_OWNED_TARGET_QUERY_ROWS))
            .map(|target| {
                target.observed |= target.absent;
                OwnedTargetEvidence {
                    key: Some(target.key.clone()),
                    cwd: target.cwd.clone(),
                    state: if target.absent {
                        OwnedTargetState::NativeBoundaryAbsent
                    } else {
                        OwnedTargetState::Present
                    },
                    native_absence_receipt: target.absent.then(|| target.receipt.clone()),
                }
            })
            .collect();
        let status = if targets.len() == count {
            OwnedTargetsStatus::Complete
        } else {
            OwnedTargetsStatus::Truncated {
                omitted: count - targets.len(),
            }
        };
        OwnedTargets { targets, status }
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let matches = state.targets.get(&key.pid).is_some_and(|target| {
            same_recorded_key(&target.key, key) && target.receipt == *receipt
        });
        let observed = state
            .targets
            .get(&key.pid)
            .is_some_and(|target| target.absent && target.observed);
        let answer = if !matches {
            NativeBoundaryAcknowledgement::UnknownIdentity
        } else if !observed {
            NativeBoundaryAcknowledgement::RefusedUnobserved
        } else {
            state
                .answers
                .pop_front()
                .unwrap_or(NativeBoundaryAcknowledgement::RefusedUnobserved)
        };
        state.acknowledgements.push(Acknowledgement {
            key: key.clone(),
            receipt: receipt.clone(),
            answer,
        });
        if answer == NativeBoundaryAcknowledgement::Acknowledged {
            state.targets.remove(&key.pid);
        }
        answer
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn boot_identity(&self) -> dudes_types::HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> std::io::Result<()> {
        self.inner.write_hooks(install)
    }
}

struct Owned {
    daemon: TestDaemon,
    fixture: common::Fixture,
    host: MockNativeHost,
}

struct Attempt {
    started: Lsn,
    owner: dudes_proto::InstructionObservationOwner,
    turn: TurnId,
    key: AdoptionKey,
    spawn_lsn: Lsn,
}

impl Owned {
    fn new() -> Self {
        let fixture = common::Fixture::new();
        let host = MockNativeHost::new();
        let mut seams = common::seams(
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: "/usr/bin/true".into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect();
        let daemon = crate::boot(&fixture.settings(), seams).expect("native mock fixture boot");
        Self {
            daemon,
            fixture,
            host,
        }
    }

    fn spawn(&mut self, cwd: &Path) -> Attempt {
        let answer = self
            .daemon
            .ask(Ask::Hire(Box::new(common::hire_params(cwd))))
            .expect("hire before mock spawn");
        let Answered::Hired(result) = answer else {
            panic!("expected a hired agent")
        };
        let effect = prepared(&mut self.daemon, result.agent);
        let Effect::Spawn { turn, .. } = &effect else {
            unreachable!()
        };
        let turn = *turn;
        let mut queue = VecDeque::new();
        execute(&mut self.daemon, &effect, &mut queue);
        for _ in 0..64 {
            let Some(input) = queue.pop_front() else {
                break;
            };
            self.daemon.sup.handle(input);
            self.daemon
                .ctx
                .retire_recorded_native_launches(self.daemon.sup.world());
            for effect in self.daemon.sup.take_effects() {
                execute(&mut self.daemon, &effect, &mut queue);
            }
        }
        assert!(queue.is_empty(), "spawn folding exceeded its bound");
        let (started, receipt) = self
            .daemon
            .sup
            .world()
            .managed_native_spawn_receipts()
            .find(|(_, receipt)| matches!(receipt.owner,
                dudes_proto::InstructionObservationOwner::Managed { turn: owned, .. } if owned == turn))
            .expect("actual folded Starting and TurnSpawned receipts");
        let attempt = Attempt {
            started: *started,
            owner: receipt.owner,
            turn,
            key: receipt.key.clone(),
            spawn_lsn: receipt.spawn_lsn,
        };
        assert!(self.fixture.entries().iter().any(|(lsn, entry)| {
            *lsn == attempt.started
                && matches!(entry.record, Record::InstructionObservationStarted { owner, .. }
                    if owner == attempt.owner)
        }));
        assert!(self.fixture.entries().iter().any(|(lsn, entry)| {
            *lsn == attempt.spawn_lsn
                && matches!(entry.record, Record::TurnSpawned { turn: owned, .. } if owned == turn)
        }));
        attempt
    }

    fn settle(&mut self, queue: &mut VecDeque<Input>) -> anyhow::Result<bool> {
        self.daemon
            .ctx
            .settle_one_native_boundary(&self.daemon.sup, queue)
    }

    fn fold(&mut self, input: Input, attempt: &Attempt) {
        let Input::InstructionJournaled { lsn, entry } = &input else {
            panic!("settlement must use the instruction journal acknowledgement")
        };
        assert!(
            matches!(entry.record, Record::InstructionAttemptBoundarySettled {
            owner, started_lsn,
            proof: dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn },
            },
        } if owner == attempt.owner && started_lsn == attempt.started && spawn_lsn == attempt.spawn_lsn)
        );
        assert!(
            self.fixture
                .entries()
                .iter()
                .any(|(written, stamped)| { written == lsn && stamped.record == entry.record })
        );
        self.daemon.sup.handle(input);
        self.daemon
            .ctx
            .retire_recorded_native_launches(self.daemon.sup.world());
        assert!(self.daemon.sup.take_effects().is_empty());
        assert!(
            self.daemon
                .sup
                .world()
                .instructions
                .attempt(attempt.started)
                .unwrap()
                .boundary_settled
                .is_some()
        );
    }

    fn assert_pending(&self, attempt: &Attempt) {
        let pending = self
            .daemon
            .ctx
            .native_acknowledgements
            .get(&attempt.started)
            .expect("exact started receipt remains available for acknowledgement retry");
        assert_eq!(pending.owner, attempt.owner);
    }

    fn settlements(&self) -> usize {
        self.fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::InstructionAttemptBoundarySettled { .. }))
            .count()
    }
}

fn execute(daemon: &mut TestDaemon, effect: &Effect, queue: &mut VecDeque<Input>) {
    daemon
        .ctx
        .execute(
            effect,
            queue,
            &mut PumpReport::Discard,
            &mut daemon.sup,
            &mut daemon.closing,
        )
        .expect("real persistence executor");
}

fn prepared(daemon: &mut TestDaemon, agent: AgentId) -> Effect {
    let params = common::goal_params(agent, &["/usr/bin/true"]);
    let mut queue = VecDeque::from([Input::Command(Command::GoalSet {
        agent,
        text: params.text,
        predicate: params.predicate,
        budget: params.budget,
        hop_cap: params.hop_cap,
        browser_goal: None,
        goal_replacement: None,
        completion_check: None,
        baseline: None,
    })]);
    for _ in 0..64 {
        let input = queue.pop_front().unwrap_or_else(|| {
            match daemon.jobs.recv_timeout(Duration::from_secs(5)) {
                Ok(Job::Input(input)) => *input,
                _ => panic!("bounded preparation awaits the actual worker input"),
            }
        });
        daemon.sup.handle(input);
        for effect in daemon.sup.take_effects() {
            if matches!(effect, Effect::Spawn { .. }) {
                return effect;
            }
            execute(daemon, &effect, &mut queue);
        }
    }
    panic!("bounded launch preparation did not produce Spawn")
}

#[test]
fn an_issued_identity_never_exempts_the_runtime_owned_pre_receipt_window() {
    let mut owned = Owned::new();
    let Answered::Hired(hired) = owned
        .daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            owned.fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("the launch needs its recorded owner");
    };
    let effect = prepared(&mut owned.daemon, hired.agent);
    let Effect::Spawn { turn, .. } = &effect else {
        panic!("the sampled launch must be prepared");
    };
    let turn = *turn;
    let mut queue = VecDeque::new();
    execute(&mut owned.daemon, &effect, &mut queue);
    assert!(owned.daemon.sup.world().turns[&turn].is_issued_not_spawned());
    assert_eq!(owned.daemon.ctx.pending_native_launches.len(), 1);
    assert!(!owned.fixture.records().iter().any(|record| {
        matches!(record, Record::TurnSpawned { turn: captured, .. } if *captured == turn)
    }));
    assert!(
        matches!(owned.daemon.ctx.admit_prior_attempt(&owned.daemon.sup, hired.agent, Some(turn)),
            Err(dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { agent })
                if agent == hired.agent)
    );
    assert!(
        matches!(owned.daemon.ctx.admit_prior_attempt(&owned.daemon.sup, hired.agent, None),
            Err(dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { agent })
                if agent == hired.agent)
    );
    assert_eq!(owned.daemon.ctx.pending_native_launches.len(), 1);
    assert_eq!(owned.host.inner.spawned().len(), 1);
}

#[test]
fn a_durable_unfolded_settlement_cannot_acknowledge_or_lose_its_retry_receipt() {
    let mut owned = Owned::new();
    let cwd = owned.fixture.state.path().to_path_buf();
    let attempt = owned.spawn(&cwd);
    let receipt = owned.host.expose_absence(&attempt.key);
    let mut queue = VecDeque::new();
    assert!(owned.settle(&mut queue).unwrap());
    assert_eq!(owned.settlements(), 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(owned.daemon.ctx.native_acknowledgements.len(), 1);
    assert!(
        owned.host.acknowledgements().is_empty(),
        "durability preceded the World fold"
    );
    owned.assert_pending(&attempt);
    assert!(
        owned
            .daemon
            .sup
            .world()
            .managed_native_spawn_receipt(attempt.started)
            .is_some()
    );
    let _ = owned.settle(&mut queue).unwrap();
    assert!(
        owned.host.acknowledgements().is_empty(),
        "withheld fold cannot authorize a retry acknowledgement"
    );
    assert_eq!(
        owned.settlements(),
        1,
        "unfolded retry cannot append a duplicate frame"
    );
    owned.assert_pending(&attempt);
    owned.fold(queue.pop_front().unwrap(), &attempt);
    let _ = owned.settle(&mut queue).unwrap();
    let refused = owned.host.acknowledgements();
    assert!(!refused.is_empty());
    assert!(
        refused
            .iter()
            .all(|ack| same_recorded_key(&ack.key, &attempt.key)
                && ack.receipt == receipt
                && ack.answer == NativeBoundaryAcknowledgement::RefusedUnobserved)
    );
    owned.assert_pending(&attempt);
    owned
        .host
        .answer_next(NativeBoundaryAcknowledgement::Acknowledged);
    assert!(owned.settle(&mut queue).unwrap());
    assert!(
        !owned
            .daemon
            .ctx
            .native_acknowledgements
            .contains_key(&attempt.started)
    );
    let calls = owned.host.acknowledgements();
    let acknowledged = calls.last().unwrap();
    assert!(same_recorded_key(&acknowledged.key, &attempt.key));
    assert_eq!(acknowledged.receipt, receipt);
    assert_eq!(
        acknowledged.answer,
        NativeBoundaryAcknowledgement::Acknowledged
    );
    assert_eq!(owned.settlements(), 1);
    assert!(owned.host.owned_targets(512).targets.is_empty());
    assert_eq!(
        owned
            .daemon
            .sup
            .world()
            .instructions
            .attempt(attempt.started)
            .unwrap()
            .spawn_lsn,
        Some(attempt.spawn_lsn)
    );
    assert!(
        owned.daemon.sup.world().turns[&attempt.turn]
            .adoption_key()
            .is_some()
    );
}

#[test]
fn failed_settlement_append_never_acknowledges_and_the_same_query_receipt_is_retryable() {
    let mut owned = Owned::new();
    let cwd = owned.fixture.state.path().to_path_buf();
    let attempt = owned.spawn(&cwd);
    let receipt = owned.host.expose_absence(&attempt.key);
    owned
        .daemon
        .ctx
        .append_failure
        .fail_nth(NonZeroUsize::new(1).unwrap());
    let mut queue = VecDeque::new();
    assert!(owned.settle(&mut queue).is_err());
    assert!(queue.is_empty());
    assert_eq!(owned.settlements(), 0);
    assert!(owned.host.acknowledgements().is_empty());
    assert!(
        owned
            .daemon
            .sup
            .world()
            .managed_native_spawn_receipt(attempt.started)
            .is_some()
    );
    let targets = owned.host.owned_targets(512);
    assert_eq!(targets.targets.len(), 1);
    assert_eq!(
        targets.targets[0].native_absence_receipt.as_ref(),
        Some(&receipt)
    );
    assert!(same_recorded_key(
        targets.targets[0].key.as_ref().unwrap(),
        &attempt.key
    ));
    assert!(owned.settle(&mut queue).unwrap());
    assert!(owned.host.acknowledgements().is_empty());
    owned.assert_pending(&attempt);
    owned.fold(queue.pop_front().unwrap(), &attempt);
    owned
        .host
        .answer_next(NativeBoundaryAcknowledgement::Acknowledged);
    assert!(owned.settle(&mut queue).unwrap());
    assert_eq!(owned.settlements(), 1);
    assert!(
        !owned
            .daemon
            .ctx
            .native_acknowledgements
            .contains_key(&attempt.started)
    );
}

#[test]
fn a_same_key_successor_receipt_cannot_be_retired_by_the_old_folded_acknowledgement() {
    let mut owned = Owned::new();
    let first_cwd = owned.fixture.state.path().join("first");
    let second_cwd = owned.fixture.state.path().join("second");
    std::fs::create_dir(&first_cwd).unwrap();
    std::fs::create_dir(&second_cwd).unwrap();
    let first = owned.spawn(&first_cwd);
    let old_receipt = owned.host.expose_absence(&first.key);
    let mut queue = VecDeque::new();
    assert!(owned.settle(&mut queue).unwrap());
    assert!(owned.host.acknowledgements().is_empty());
    owned.fold(queue.pop_front().unwrap(), &first);
    owned.assert_pending(&first);
    owned.host.reuse_next_key(&first.key);
    let second = owned.spawn(&second_cwd);
    assert_ne!(first.started, second.started);
    assert_ne!(first.owner, second.owner);
    assert!(same_recorded_key(&first.key, &second.key));
    let fresh_receipt = owned.host.expose_absence(&second.key);
    assert_ne!(old_receipt, fresh_receipt);
    assert!(owned.settle(&mut queue).unwrap());
    owned.assert_pending(&first);
    owned.assert_pending(&second);
    assert!(
        !owned
            .host
            .acknowledgements()
            .iter()
            .any(|ack| ack.receipt == fresh_receipt)
    );
    owned.fold(queue.pop_front().unwrap(), &second);
    owned
        .host
        .answer_next(NativeBoundaryAcknowledgement::Acknowledged);
    assert!(owned.settle(&mut queue).unwrap());
    owned.assert_pending(&first);
    assert_eq!(owned.daemon.ctx.native_acknowledgements.len(), 1);
    assert!(
        !owned
            .daemon
            .ctx
            .native_acknowledgements
            .contains_key(&second.started)
    );
    let calls = owned.host.acknowledgements();
    assert!(calls.iter().any(|ack| ack.receipt == old_receipt
        && ack.answer == NativeBoundaryAcknowledgement::UnknownIdentity));
    let successful: Vec<_> = calls
        .iter()
        .filter(|ack| ack.answer == NativeBoundaryAcknowledgement::Acknowledged)
        .collect();
    assert_eq!(successful.len(), 1);
    assert_eq!(successful[0].receipt, fresh_receipt);
    assert!(same_recorded_key(&successful[0].key, &second.key));
    assert_eq!(owned.settlements(), 2);
}
