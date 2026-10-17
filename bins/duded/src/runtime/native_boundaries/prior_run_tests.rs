use std::cell::Cell;

use super::*;
use dudes_proto::record::{RecordKind, samples};
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{KillOutcome, Liveness, SpawnResult};
use dudes_supervisor::testing::FakeHost;
use dudes_types::Timestamp;

const BOOT: &str = "test-boot";
const NAMESPACE: &str = "pid:[4026531836]";

struct Observations {
    inner: FakeHost,
    namespace: Option<String>,
    leader: ProbeAnswer,
    group: NativeGroupPresence,
    leader_queries: Cell<usize>,
    group_queries: Cell<usize>,
}

impl Default for Observations {
    fn default() -> Self {
        let inner = FakeHost {
            host_boot: if cfg!(target_os = "linux") {
                HostBoot::Read(BOOT.into())
            } else {
                HostBoot::NotApplicable
            },
            ..FakeHost::default()
        };
        Self {
            inner,
            namespace: Some(NAMESPACE.into()),
            leader: ProbeAnswer::NoSuchPid,
            group: NativeGroupPresence::Absent,
            leader_queries: Cell::new(0),
            group_queries: Cell::new(0),
        }
    }
}

impl ProcessHost for Observations {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> std::io::Result<()> {
        self.inner.write_hooks(install)
    }

    fn pid_namespace(&self) -> Option<String> {
        self.namespace.clone()
    }

    fn probe_native_leader(&self, _key: &AdoptionKey) -> ProbeAnswer {
        self.leader_queries.set(self.leader_queries.get() + 1);
        self.leader
    }

    fn native_group_presence(&self, _pid: u32) -> NativeGroupPresence {
        self.group_queries.set(self.group_queries.get() + 1);
        self.group
    }
}

fn fold(world: &mut dudes_supervisor::World, lsn: u64, record: Record) {
    world
        .apply_checked(
            Lsn(lsn),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record,
            },
        )
        .unwrap();
}

fn inherited_world(count: u32) -> dudes_supervisor::World {
    let mut world = dudes_supervisor::World::default();
    fold(&mut world, 1, samples::record(RecordKind::AgentHired));
    for index in 0..count {
        let turn = TurnId::from_parts(1, u128::from(index) + 1);
        let owner = InstructionObservationOwner::Managed {
            agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            turn,
            issue: dudes_proto::InstructionIssueAssociation::Prospective,
        };
        let mut started = samples::record(RecordKind::InstructionObservationStarted);
        let boot = cfg!(target_os = "linux").then(|| BOOT.to_owned());
        if let Record::InstructionObservationStarted {
            owner: actual,
            ticket,
            snapshot,
        } = &mut started
        {
            *actual = owner;
            *ticket = None;
            snapshot.boot = boot
                .clone()
                .map(|value| InstructionBootIdentity::LinuxKernelBootId { value });
            snapshot.pid_namespace = cfg!(target_os = "linux").then(|| NAMESPACE.to_owned());
        }
        let position = 2 + 3 * u64::from(index);
        fold(&mut world, position, started);
        let mut issued = samples::record(RecordKind::TurnIssued);
        if let Record::TurnIssued { turn: actual, .. } = &mut issued {
            *actual = turn;
        }
        fold(&mut world, position + 1, issued);
        let mut spawned = samples::record(RecordKind::TurnSpawned);
        if let Record::TurnSpawned {
            turn: actual,
            pid,
            start_time,
            boot: actual_boot,
            ..
        } = &mut spawned
        {
            *actual = turn;
            *pid = 1_000 + index;
            *start_time = 99;
            *actual_boot = boot;
        }
        fold(&mut world, position + 2, spawned);
    }
    assert_eq!(world.managed_native_spawn_receipts().len(), count as usize);
    world
}

fn candidates(world: &dudes_supervisor::World) -> InheritedNativeBoundaries {
    let mut inherited = InheritedNativeBoundaries::default();
    inherited.capture(world);
    inherited
}

#[test]
fn both_positive_leader_observations_require_an_empty_group() {
    let world = inherited_world(1);
    for (leader, expected) in [
        (ProbeAnswer::NoSuchPid, PriorRunLeader::NoSuchProcess),
        (
            ProbeAnswer::StartValue(100),
            PriorRunLeader::StartTimeDiffers,
        ),
    ] {
        let host = Observations {
            leader,
            ..Observations::default()
        };
        let settlement = candidates(&world).settlement(&host, &world).unwrap();
        assert_eq!(settlement.started, Lsn(2));
        assert_eq!(
            settlement.owner,
            world.instructions.attempt(Lsn(2)).unwrap().owner
        );
        assert_eq!(
            settlement.proof,
            InstructionBoundaryAbsenceProof::PriorRunGroupAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(4) },
                pid: 1_000,
                start_time: 99,
                boot: cfg!(target_os = "linux").then(|| BOOT.to_owned()),
                leader: expected,
            }
        );
        assert!(settlement.acknowledgement.is_none());
        assert_eq!(host.group_queries.get(), 1);
    }
}

#[test]
fn a_live_or_unreadable_leader_stops_before_the_group_query() {
    let world = inherited_world(1);
    for leader in [ProbeAnswer::StartValue(99), ProbeAnswer::CouldNotAnswer] {
        let host = Observations {
            leader,
            ..Observations::default()
        };
        assert!(candidates(&world).settlement(&host, &world).is_none());
        assert_eq!(host.leader_queries.get(), 1);
        assert_eq!(host.group_queries.get(), 0);
    }
}

#[test]
fn a_reused_pid_group_member_permission_or_query_error_is_not_absence() {
    let world = inherited_world(1);
    for leader in [ProbeAnswer::NoSuchPid, ProbeAnswer::StartValue(100)] {
        for group in [
            NativeGroupPresence::Present,
            NativeGroupPresence::Unsignallable,
            NativeGroupPresence::Unknown,
        ] {
            let host = Observations {
                leader,
                group,
                ..Observations::default()
            };
            assert!(candidates(&world).settlement(&host, &world).is_none());
            assert_eq!(host.group_queries.get(), 1);
        }
    }
}

#[test]
fn the_boot_snapshot_never_refreshes_to_include_current_run_launches() {
    let empty = inherited_world(0);
    let current = inherited_world(1);
    let mut inherited = candidates(&empty);
    inherited.capture(&current);
    let host = Observations::default();
    assert!(inherited.settlement(&host, &current).is_none());
    assert_eq!(host.leader_queries.get(), 0);
    assert_eq!(current.managed_native_spawn_receipts().len(), 1);
}

#[test]
fn reaching_the_cap_disables_every_candidate_without_dropping_receipts() {
    for count in [511, 512, 513] {
        let world = inherited_world(count);
        let inherited = candidates(&world);
        let host = Observations::default();
        if count < 512 {
            let InheritedNativeBoundaries::Captured(values) = &inherited else {
                panic!("below-cap capture")
            };
            assert_eq!(values.len(), 511);
            assert!(inherited.settlement(&host, &world).is_some());
        } else {
            assert!(matches!(inherited, InheritedNativeBoundaries::Disabled));
            assert!(inherited.settlement(&host, &world).is_none());
            assert_eq!(host.leader_queries.get(), 0);
        }
        assert_eq!(world.managed_native_spawn_receipts().len(), count as usize);
    }
}

#[test]
fn same_boot_is_required_before_any_native_query() {
    let world = inherited_world(1);
    let mut host = Observations::default();
    host.inner.host_boot = HostBoot::Read("another-boot".into());
    assert!(candidates(&world).settlement(&host, &world).is_none());
    assert_eq!(host.leader_queries.get(), 0);
    host.inner.host_boot = HostBoot::Unreadable("permission denied".into());
    assert!(candidates(&world).settlement(&host, &world).is_none());
    assert_eq!(host.leader_queries.get(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_requires_the_recorded_and_current_pid_namespaces() {
    let world = inherited_world(1);
    let mut host = Observations::default();
    for namespace in [None, Some("pid:[999]".into())] {
        host.namespace = namespace;
        assert!(candidates(&world).settlement(&host, &world).is_none());
        assert_eq!(host.leader_queries.get(), 0);
    }
    let mut snapshot = world
        .instructions
        .attempt(Lsn(2))
        .unwrap()
        .snapshot
        .clone()
        .unwrap();
    let key = world
        .managed_native_spawn_receipt(Lsn(2))
        .unwrap()
        .key
        .clone();
    host.namespace = Some(NAMESPACE.into());
    snapshot.pid_namespace = None;
    assert!(!same_prior_run_scope(&host, &key, Some(&snapshot)));
    assert_eq!(snapshot.pid_namespace, None);
    snapshot.pid_namespace = Some("pid:[999]".into());
    assert!(!same_prior_run_scope(&host, &key, Some(&snapshot)));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_missing_boot_evidence_cannot_qualify_through_matching_namespaces() {
    let world = inherited_world(1);
    let mut host = Observations::default();
    let spawn = world.managed_native_spawn_receipt(Lsn(2)).unwrap();
    let mut key = spawn.key.clone();
    let mut snapshot = world
        .instructions
        .attempt(Lsn(2))
        .unwrap()
        .snapshot
        .clone()
        .unwrap();
    host.inner.host_boot = HostBoot::NotApplicable;
    assert!(!same_prior_run_scope(&host, &key, Some(&snapshot)));
    host.inner.host_boot = HostBoot::Read(BOOT.into());
    key.boot = None;
    assert!(!same_prior_run_scope(&host, &key, Some(&snapshot)));
    key.boot = spawn.key.boot.clone();
    snapshot.boot = None;
    assert!(!same_prior_run_scope(&host, &key, Some(&snapshot)));
    assert_eq!(host.leader_queries.get(), 0);
}

#[test]
fn a_valid_settlement_needs_no_turn_end_and_replays_to_retired_receipts() {
    let mut world = inherited_world(1);
    let mut inherited = candidates(&world);
    let settlement = inherited
        .settlement(&Observations::default(), &world)
        .unwrap();
    let record = Record::InstructionAttemptBoundarySettled {
        owner: settlement.owner,
        started_lsn: settlement.started,
        proof: settlement.proof,
    };
    fold(&mut world, 5, record.clone());
    fold(&mut world, 6, record);
    inherited.settled(settlement.started);
    assert_eq!(world.managed_native_spawn_receipts().len(), 0);
    let attempt = world.instructions.attempt(Lsn(2)).unwrap();
    assert_eq!(attempt.boundary_settled.as_ref().unwrap().lsn, Lsn(5));
    assert!(
        world
            .instructions
            .managed_end_lsn(
                AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
                TurnId::from_parts(1, 1)
            )
            .is_none()
    );
    assert!(
        inherited
            .settlement(&Observations::default(), &world)
            .is_none()
    );
}

#[test]
fn an_unspawned_prior_attempt_cannot_gain_a_candidate_when_reissued() {
    let mut world = inherited_world(0);
    let mut started = samples::record(RecordKind::InstructionObservationStarted);
    if let Record::InstructionObservationStarted { ticket, .. } = &mut started {
        *ticket = None;
    }
    fold(&mut world, 2, started);
    fold(&mut world, 3, samples::record(RecordKind::TurnIssued));
    let mut inherited = candidates(&world);
    let mut spawned = samples::record(RecordKind::TurnSpawned);
    if let Record::TurnSpawned { reissued, .. } = &mut spawned {
        *reissued = true;
    }
    fold(&mut world, 4, spawned);
    inherited.capture(&world);
    let host = Observations::default();
    assert!(inherited.settlement(&host, &world).is_none());
    assert_eq!(world.managed_native_spawn_receipts().len(), 1);
    assert_eq!(host.leader_queries.get(), 0);
}
