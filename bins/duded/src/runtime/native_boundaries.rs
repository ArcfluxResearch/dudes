use std::collections::VecDeque;

use anyhow::Result;
use dudes_proto::{
    Entry, InstructionBootIdentity, InstructionBoundaryAbsenceProof,
    InstructionNativeBoundaryReceipt, InstructionObservationOwner, Record,
};
use dudes_supervisor::host::{
    AdoptionKey, MAX_OWNED_TARGET_QUERY_ROWS, NativeBoundaryReceipt, OwnedTargetState,
};
use dudes_supervisor::{Clock, Input, ProcessHost, Rng, Supervisor};
use dudes_types::{AgentId, HostBoot, Lsn, TurnId};

use super::{Ctx, Shared};
use crate::predicate::PredicateRunner;

pub(super) fn same_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
}

pub(crate) struct PendingNativeLaunch {
    owner: InstructionObservationOwner,
    key: AdoptionKey,
}

pub(crate) struct PendingNativeAcknowledgement {
    pub(super) owner: InstructionObservationOwner,
    recorded_lsn: Lsn,
    key: AdoptionKey,
    receipt: NativeBoundaryReceipt,
}

struct Settlement {
    started: Lsn,
    owner: InstructionObservationOwner,
    proof: InstructionBoundaryAbsenceProof,
    acknowledgement: Option<(AdoptionKey, NativeBoundaryReceipt)>,
    launch_failure: bool,
}

fn spawned_absence<H: ProcessHost>(
    host: &H,
    world: &dudes_supervisor::World,
) -> Option<Settlement> {
    if world.managed_native_spawn_receipts().len() > MAX_OWNED_TARGET_QUERY_ROWS {
        return None;
    }
    let targets = host.owned_targets(MAX_OWNED_TARGET_QUERY_ROWS);
    for target in targets.targets {
        if target.state != OwnedTargetState::NativeBoundaryAbsent {
            continue;
        }
        let (Some(key), Some(receipt)) = (target.key, target.native_absence_receipt) else {
            continue;
        };
        let mut matched = world
            .managed_native_spawn_receipts()
            .filter(|(_, spawn)| same_key(&key, &spawn.key));
        let Some((&started, spawn)) = matched.next() else {
            continue;
        };
        if matched.next().is_some() {
            continue;
        }
        return Some(Settlement {
            started,
            owner: spawn.owner,
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned {
                    lsn: spawn.spawn_lsn,
                },
            },
            acknowledgement: Some((key, receipt)),
            launch_failure: false,
        });
    }
    None
}

fn prior_boot<H: ProcessHost>(host: &H, world: &dudes_supervisor::World) -> Option<Settlement> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let HostBoot::Read(value) = host.boot_identity() else {
        return None;
    };
    let current = InstructionBootIdentity::LinuxKernelBootId { value };
    let mut examined = 0;
    for owner in world.instructions.unresolved_owners() {
        for (started, attempt) in world.instructions.unresolved_launch_attempts(owner) {
            if examined == MAX_OWNED_TARGET_QUERY_ROWS {
                return None;
            }
            examined += 1;
            let Some(previous) = attempt
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.boot.as_ref())
            else {
                continue;
            };
            if previous != &current {
                return Some(Settlement {
                    started: *started,
                    owner: attempt.owner,
                    proof: InstructionBoundaryAbsenceProof::PriorBootAbsent {
                        previous: previous.clone(),
                        current,
                    },
                    acknowledgement: None,
                    launch_failure: false,
                });
            }
        }
    }
    None
}

fn owner_agent(owner: InstructionObservationOwner) -> dudes_types::AgentId {
    match owner {
        InstructionObservationOwner::Managed { agent, .. }
        | InstructionObservationOwner::AttachedProcess { agent, .. }
        | InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
    }
}

fn owed_retry_observation(
    world: &dudes_supervisor::World,
    agent: AgentId,
    turn: Option<TurnId>,
) -> Option<(Lsn, InstructionObservationOwner)> {
    let turn = turn?;
    let row = world.turns.get(&turn)?;
    if row.agent != agent || !row.is_issued_not_spawned() {
        return None;
    }
    let issue_lsn = world.instructions.issue_lsn(turn)?;
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: dudes_proto::InstructionIssueAssociation::Issued { issue_lsn },
    };
    let (started, attempt) = world.instructions.current_for_turn(turn)?;
    (attempt.owner == owner
        && attempt.finished.is_none()
        && attempt.spawn_lsn.is_none()
        && matches!(attempt.baseline, dudes_proto::InstructionBaselineRef::Started { lsn }
            if lsn == started)
        && attempt.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.source == dudes_proto::InstructionSnapshotSource::Fresh
        }))
    .then_some((started, owner))
}

fn historical_owed_observation(
    world: &dudes_supervisor::World,
    retry: Option<(Lsn, InstructionObservationOwner)>,
    started: Lsn,
    attempt: &dudes_proto::instruction_state::InstructionAttempt,
) -> bool {
    let Some((
        fresh_started,
        InstructionObservationOwner::Managed {
            agent,
            turn,
            issue: dudes_proto::InstructionIssueAssociation::Issued { issue_lsn },
        },
    )) = retry
    else {
        return false;
    };
    if world.instructions.issue_lsn(turn) != Some(issue_lsn) {
        return false;
    }
    let same_issue = match attempt.owner {
        InstructionObservationOwner::Managed {
            agent: prior_agent,
            turn: prior_turn,
            issue,
        } if prior_agent == agent && prior_turn == turn => match issue {
            dudes_proto::InstructionIssueAssociation::Prospective => {
                started < issue_lsn
                    && matches!(attempt.baseline, dudes_proto::InstructionBaselineRef::Started { lsn }
                            if lsn == started)
            }
            dudes_proto::InstructionIssueAssociation::Issued {
                issue_lsn: prior_issue,
            } => prior_issue == issue_lsn,
        },
        _ => false,
    };
    started < fresh_started
        && same_issue
        && attempt.finished.is_some()
        && attempt.spawn_lsn.is_none()
}

fn completed_owed_observation(
    world: &dudes_supervisor::World,
    started: Lsn,
    attempt: &dudes_proto::instruction_state::InstructionAttempt,
) -> bool {
    if attempt.finished.is_none() || attempt.spawn_lsn.is_some() {
        return false;
    }
    let InstructionObservationOwner::Managed { agent, turn, .. } = attempt.owner else {
        return false;
    };
    let Some(row) = world
        .turns
        .get(&turn)
        .filter(|row| row.agent == agent && row.has_ended())
    else {
        return false;
    };
    let (Some(spawn), Some(end)) = (
        row.spawn_lsn,
        world.instructions.managed_end_lsn(agent, turn),
    ) else {
        return false;
    };
    let Some((_, later)) = world
        .instructions
        .later_managed_attempt_for_spawn(agent, turn, started, spawn)
    else {
        return false;
    };
    let ended = later.finished.as_ref().is_some_and(|finished| {
        let boundary = match &finished.settlement {
            dudes_proto::InstructionSettlement::Compared { end, .. } => Some(*end),
            dudes_proto::InstructionSettlement::Unknown { end, .. } => *end,
            dudes_proto::InstructionSettlement::NoIssuedTurn
            | dudes_proto::InstructionSettlement::NoProcess { .. } => None,
        };
        boundary == Some(dudes_proto::InstructionEndBoundary::ManagedTurn { turn, end_lsn: end })
    });
    end > spawn
        && ended
        && later.boundary_settled.is_some()
        && attempt
            .ticket
            .is_none_or(|ticket| !world.instructions.pending_for_goal(ticket.goal))
        && later
            .ticket
            .is_none_or(|ticket| !world.instructions.pending_for_goal(ticket.goal))
        && row
            .goal
            .is_none_or(|goal| !world.instructions.pending_for_goal(goal))
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    fn native_admission_capacity_exceeded(&self, world: &dudes_supervisor::World) -> bool {
        world.unmatched_attached_turn_end_count() >= MAX_OWNED_TARGET_QUERY_ROWS
            || self.attached_instruction_boundaries.len() >= MAX_OWNED_TARGET_QUERY_ROWS
            || self.pending_attached_calls.len() >= MAX_OWNED_TARGET_QUERY_ROWS
            || self.pending_native_launches.len() >= MAX_OWNED_TARGET_QUERY_ROWS
            || self.native_acknowledgements.len() >= MAX_OWNED_TARGET_QUERY_ROWS
            || self.cleanup_ptys.len() > MAX_OWNED_TARGET_QUERY_ROWS
    }

    pub(super) fn admit_prior_attempt<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: dudes_types::AgentId,
        issued_turn: Option<TurnId>,
    ) -> Result<(), dudes_supervisor::SharedFolderRefusal> {
        let refused = || dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { agent };
        let world = sup.world();
        if self.native_admission_capacity_exceeded(world) {
            return Err(dudes_supervisor::SharedFolderRefusal::ChildLimit);
        }
        if self
            .pending_native_launches
            .values()
            .any(|pending| owner_agent(pending.owner) == agent)
            || self
                .pending_attached_calls
                .values()
                .any(|owner| owner_agent(*owner) == agent)
            || self.owns_attached_boundary(agent)
            || self.live_ptys.contains_key(&agent)
            || self
                .cleanup_ptys
                .values()
                .any(|cleanup| cleanup.agent == agent)
            || self
                .confirmed_launch_failures
                .values()
                .any(|owner| owner_agent(*owner) == agent)
            || self
                .native_acknowledgements
                .values()
                .any(|pending| owner_agent(pending.owner) == agent)
        {
            return Err(refused());
        }
        let mut native = world.managed_native_spawn_receipts();
        let mut attached = world.attached_native_spawn_receipts();
        if native.len().saturating_add(attached.len()) >= MAX_OWNED_TARGET_QUERY_ROWS {
            return Err(dudes_supervisor::SharedFolderRefusal::ChildLimit);
        }
        if native.any(|(_, spawn)| owner_agent(spawn.owner) == agent)
            || attached.any(|(_, spawn)| owner_agent(spawn.owner) == agent)
        {
            return Err(refused());
        }
        let current = world
            .agents
            .get(&agent)
            .and_then(|row| row.open_turn)
            .and_then(|turn| world.instructions.current_for_turn(turn))
            .map(|(started, _)| started)
            .or_else(|| {
                world
                    .instructions
                    .attachment_lsn(agent)
                    .and_then(|attachment| world.instructions.current_for_attachment(attachment))
                    .map(|(started, _)| started)
            });
        let owed_retry = owed_retry_observation(world, agent, issued_turn);
        for (count, (started, attempt)) in world
            .instructions
            .unresolved_launch_attempts(agent)
            .enumerate()
        {
            let historical_owed = historical_owed_observation(world, owed_retry, *started, attempt);
            let completed_owed = completed_owed_observation(world, *started, attempt);
            if count == MAX_OWNED_TARGET_QUERY_ROWS
                || (!historical_owed
                    && !completed_owed
                    && (Some(*started) != current
                        || attempt.finished.is_some()
                        || attempt.spawn_lsn.is_some()))
            {
                return Err(refused());
            }
        }
        let mut examined = 0;
        self.children.visit_owned(|turn, _, _| {
            examined += 1;
            if examined > MAX_OWNED_TARGET_QUERY_ROWS
                || world.turns.get(&turn).is_none_or(|row| row.agent == agent)
            {
                Err(refused())
            } else {
                Ok(())
            }
        })?;
        let incomplete = self.children.visit_uncertain(|turn, _| {
            examined += 1;
            if examined > MAX_OWNED_TARGET_QUERY_ROWS
                || world.turns.get(&turn).is_none_or(|row| row.agent == agent)
            {
                Err(refused())
            } else {
                Ok(())
            }
        })?;
        if incomplete {
            return Err(refused());
        }
        Ok(())
    }

    pub(super) fn remember_native_launch(
        &mut self,
        world: &dudes_supervisor::World,
        turn: dudes_types::TurnId,
        key: &AdoptionKey,
    ) {
        if let Some((started, attempt)) = world.instructions.current_for_turn(turn) {
            self.pending_native_launches.insert(
                started,
                PendingNativeLaunch {
                    owner: attempt.owner,
                    key: key.clone(),
                },
            );
        }
    }

    pub(super) fn retire_recorded_native_launches(&mut self, world: &dudes_supervisor::World) {
        self.pending_attached_calls.retain(|started, owner| {
            world
                .instructions
                .attempt(*started)
                .is_none_or(|attempt| attempt.owner != *owner || attempt.finished.is_none())
        });
        self.pending_native_launches.retain(|started, pending| {
            world
                .managed_native_spawn_receipt(*started)
                .is_none_or(|spawn| {
                    spawn.owner != pending.owner || !same_key(&spawn.key, &pending.key)
                })
        });
    }

    fn launch_failure_absence(&self, world: &dudes_supervisor::World) -> Option<Settlement> {
        for (&started, &owner) in &self.confirmed_launch_failures {
            let InstructionObservationOwner::Managed { agent, turn, .. } = owner else {
                continue;
            };
            let Some(attempt) = world.instructions.attempt(started) else {
                continue;
            };
            if attempt.owner != owner || attempt.boundary_settled.is_some() {
                continue;
            }
            let Some(ended) = world.instructions.managed_end_lsn(agent, turn) else {
                continue;
            };
            return Some(Settlement {
                started,
                owner,
                proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn: ended },
                },
                acknowledgement: None,
                launch_failure: true,
            });
        }
        None
    }

    pub(super) fn settle_one_native_boundary<R: Rng>(
        &mut self,
        sup: &Supervisor<Shared<C>, R>,
        queue: &mut VecDeque<Input>,
    ) -> Result<bool> {
        let acknowledged = self
            .native_acknowledgements
            .iter()
            .find_map(|(&started, pending)| {
                if !sup
                    .world()
                    .instructions
                    .attempt(started)
                    .is_some_and(|attempt| {
                        attempt.owner == pending.owner
                            && attempt
                                .boundary_settled
                                .as_ref()
                                .is_some_and(|settled| settled.lsn == pending.recorded_lsn)
                    })
                {
                    return None;
                }
                (self
                    .host
                    .acknowledge_native_boundary_absence(&pending.key, &pending.receipt)
                    == dudes_supervisor::host::NativeBoundaryAcknowledgement::Acknowledged)
                    .then_some(started)
            });
        if let Some(started) = acknowledged {
            self.native_acknowledgements.remove(&started);
            return Ok(true);
        }
        let world = sup.world();
        let Some(settlement) = self
            .launch_failure_absence(world)
            .or_else(|| spawned_absence(&self.host, world))
            .or_else(|| prior_boot(&self.host, world))
            .filter(|settlement| {
                !self
                    .native_acknowledgements
                    .contains_key(&settlement.started)
            })
        else {
            return Ok(false);
        };
        if settlement.acknowledgement.is_some()
            && self.native_acknowledgements.len() == MAX_OWNED_TARGET_QUERY_ROWS
        {
            return Ok(false);
        }
        let entry = Entry {
            at: self.clock.now(),
            record: Record::InstructionAttemptBoundarySettled {
                owner: settlement.owner,
                started_lsn: settlement.started,
                proof: settlement.proof,
            },
        };
        let lsn = self.append(world, &entry)?;
        if let Some((key, receipt)) = settlement.acknowledgement {
            self.native_acknowledgements.insert(
                settlement.started,
                PendingNativeAcknowledgement {
                    owner: settlement.owner,
                    recorded_lsn: lsn,
                    key,
                    receipt,
                },
            );
        }
        if settlement.launch_failure {
            self.confirmed_launch_failures.remove(&settlement.started);
        }
        queue.push_back(Input::InstructionJournaled { lsn, entry });
        Ok(true)
    }
}

#[cfg(test)]
mod owed_retry_tests {
    use super::*;
    use dudes_supervisor::testing::Harness;

    #[test]
    fn an_owed_retry_requires_its_distinct_fresh_unspawned_owner() {
        let mut harness = Harness::new();
        harness.launch = false;
        let (agent, _) = harness.hire_with_goal(&["true"]);
        let turn = harness.supervisor.world().agents[&agent].open_turn.unwrap();
        let first = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap()
            .0;
        harness.drive(Input::InstructionLaunchCleanupUnconfirmed { turn });
        let historical = harness
            .supervisor
            .world()
            .instructions
            .attempt(first)
            .unwrap()
            .clone();
        assert!(historical.finished.is_some());
        assert!(historical.boundary_settled.is_none());
        assert!(owed_retry_observation(harness.supervisor.world(), agent, Some(turn)).is_none());

        harness.restart();
        let world = harness.supervisor.world();
        let (fresh, current) = world.instructions.current_for_turn(turn).unwrap();
        let issue_lsn = world.instructions.issue_lsn(turn).unwrap();
        let issued_owner = InstructionObservationOwner::Managed {
            agent,
            turn,
            issue: dudes_proto::InstructionIssueAssociation::Issued { issue_lsn },
        };
        assert!(fresh > first);
        assert_eq!(
            owed_retry_observation(world, agent, Some(turn)),
            Some((fresh, issued_owner))
        );
        assert!(current.finished.is_none() && current.spawn_lsn.is_none());
        assert_eq!(world.instructions.attempt(first), Some(&historical));
        assert!(
            world
                .instructions
                .unresolved_launch_attempts(agent)
                .any(|(lsn, _)| *lsn == first)
        );
        assert!(historical_owed_observation(
            world,
            owed_retry_observation(world, agent, Some(turn)),
            first,
            &historical
        ));
        assert!(!historical_owed_observation(
            world,
            Some((
                fresh,
                InstructionObservationOwner::Managed {
                    agent,
                    turn: TurnId::from_parts(99, 2),
                    issue: dudes_proto::InstructionIssueAssociation::Issued { issue_lsn },
                }
            )),
            first,
            &historical
        ));
        assert!(!historical_owed_observation(
            world,
            Some((fresh, current.owner)),
            fresh,
            current
        ));
        assert!(!historical_owed_observation(
            world,
            Some((
                fresh,
                InstructionObservationOwner::Managed {
                    agent,
                    turn,
                    issue: dudes_proto::InstructionIssueAssociation::Issued {
                        issue_lsn: Lsn(issue_lsn.0 + 1),
                    },
                }
            )),
            first,
            &historical
        ));
        assert!(owed_retry_observation(world, agent, None).is_none());
        assert!(owed_retry_observation(world, AgentId::from_parts(99, 1), Some(turn)).is_none());
        assert!(owed_retry_observation(world, agent, Some(TurnId::from_parts(99, 2))).is_none());
    }

    fn unchanged_control_hash(bytes: &[u8]) -> dudes_proto::record::Hex32 {
        dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
    }

    fn unchanged_control_apply(
        world: &mut dudes_supervisor::World,
        position: u64,
        record: Record,
        facts: Option<dudes_proto::instruction_state::InstructionReviewFacts>,
    ) {
        assert_eq!(world.last_applied_lsn.map_or(0, |lsn| lsn.0), position - 1);
        let entry = Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        };
        dudes_proto::validate_instruction_record(&entry.record).unwrap();
        world.validate_record_authority(&entry).unwrap();
        world
            .instructions
            .validate_next(&entry, facts, unchanged_control_hash)
            .unwrap();
        world.apply(Lsn(position), &entry);
        assert_eq!(world.last_applied_lsn, Some(Lsn(position)));
    }

    fn unchanged_control_synthetic_snapshot(agent: AgentId) -> dudes_proto::InstructionSnapshot {
        use dudes_proto::{
            InstructionDeclarationMode, InstructionDiscoveryPlan, InstructionParticipant,
            InstructionSampleUsage, InstructionSnapshot, InstructionSnapshotSource,
            NativeInstructionPath,
        };

        InstructionSnapshot {
            sampled_at: dudes_types::Timestamp::UNIX_EPOCH,
            source: InstructionSnapshotSource::Fresh,
            boot: None,
            execution_folder: Some(NativeInstructionPath::Utf8 {
                value: "/work/repo".into(),
            }),
            participants: vec![InstructionParticipant {
                agent,
                provider: dudes_types::Provider::Claude,
                mode: InstructionDeclarationMode::ClaudeBare,
                declaration_digest: dudes_proto::record::Hex32::new([1; 32]),
                discovery: InstructionDiscoveryPlan {
                    roots: Vec::new(),
                    rules: Vec::new(),
                    configuration_digest: None,
                    gaps: Vec::new(),
                },
            }],
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: Vec::new(),
            usage: InstructionSampleUsage::default(),
        }
    }

    struct UnchangedControlFixture {
        world: dudes_supervisor::World,
        agent: AgentId,
        card: dudes_types::CardId,
        goal: dudes_types::GoalId,
        turn: TurnId,
        owner: InstructionObservationOwner,
    }

    fn unchanged_control_setup() -> UnchangedControlFixture {
        use dudes_proto::record::samples;
        use dudes_proto::{InstructionIssueAssociation, RecordKind};
        use dudes_types::Seq;

        let mut world = dudes_supervisor::World::default();
        let hired = samples::record(RecordKind::AgentHired);
        let Record::AgentHired { id: agent, .. } = &hired else {
            unreachable!()
        };
        let agent = *agent;
        unchanged_control_apply(&mut world, 1, hired, None);
        let created = samples::record(RecordKind::TicketCreated);
        let Record::TicketCreated { card, .. } = &created else {
            unreachable!()
        };
        let card = *card;
        unchanged_control_apply(&mut world, 2, created, None);
        let mut assigned = samples::record(RecordKind::TicketAssigned);
        let Record::TicketAssigned { expected_seq, .. } = &mut assigned else {
            unreachable!()
        };
        *expected_seq = Seq::ZERO;
        unchanged_control_apply(&mut world, 3, assigned, None);
        let mut started = samples::record(RecordKind::TicketStarted);
        let Record::TicketStarted {
            expected_seq, goal, ..
        } = &mut started
        else {
            unreachable!()
        };
        *expected_seq = Seq(1);
        let goal = goal.id;
        unchanged_control_apply(&mut world, 4, started, None);
        unchanged_control_apply(
            &mut world,
            5,
            samples::record(RecordKind::LoopDecided),
            None,
        );
        let mut issue = samples::record(RecordKind::TurnIssued);
        let Record::TurnIssued {
            turn,
            task,
            lsn_of_decision,
            answers,
            ..
        } = &mut issue
        else {
            unreachable!()
        };
        let turn = *turn;
        *task = "next".into();
        *lsn_of_decision = Lsn(5);
        answers.clear();
        unchanged_control_apply(&mut world, 6, issue, None);
        let owner = InstructionObservationOwner::Managed {
            agent,
            turn,
            issue: InstructionIssueAssociation::Issued { issue_lsn: Lsn(6) },
        };
        UnchangedControlFixture {
            world,
            agent,
            card,
            goal,
            turn,
            owner,
        }
    }

    fn unchanged_control_attempts(
        fixture: &mut UnchangedControlFixture,
    ) -> (
        dudes_proto::InstructionTicketAssociation,
        dudes_proto::InstructionTicketAssociation,
    ) {
        use dudes_proto::{
            InstructionBaselineRef, InstructionComparison, InstructionCoverageGap,
            InstructionGapKind, InstructionPathContext, InstructionSettlement,
            InstructionUnknownReason,
        };
        use dudes_types::Seq;

        let (world, agent, goal, owner) = (
            &mut fixture.world,
            fixture.agent,
            fixture.goal,
            fixture.owner,
        );
        let older_ticket = world
            .instructions
            .association_for_goal(goal, Seq(2))
            .unwrap();
        unchanged_control_apply(
            world,
            7,
            Record::InstructionObservationStarted {
                owner,
                ticket: Some(older_ticket),
                snapshot: unchanged_control_synthetic_snapshot(agent),
            },
            None,
        );
        unchanged_control_apply(
            world,
            8,
            Record::InstructionObservationFinished {
                owner,
                ticket: Some(older_ticket),
                baseline: InstructionBaselineRef::Started { lsn: Lsn(7) },
                settlement: InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    end: None,
                    sampled_at: None,
                    recovery: true,
                    comparison: InstructionComparison {
                        gaps: vec![InstructionCoverageGap {
                            reason: InstructionGapKind::UnrecordedLaunchBoundary,
                            context: InstructionPathContext::SharedProject,
                            agent: Some(agent),
                            path: None,
                        }],
                        ..InstructionComparison::default()
                    },
                },
            },
            None,
        );
        let later_ticket = world
            .instructions
            .association_for_goal(goal, Seq(4))
            .unwrap();
        unchanged_control_apply(
            world,
            9,
            Record::InstructionObservationStarted {
                owner,
                ticket: Some(later_ticket),
                snapshot: unchanged_control_synthetic_snapshot(agent),
            },
            None,
        );
        (older_ticket, later_ticket)
    }

    fn unchanged_control_finish(
        fixture: &mut UnchangedControlFixture,
        later_ticket: dudes_proto::InstructionTicketAssociation,
    ) -> dudes_proto::InstructionBoundaryAbsenceProof {
        use dudes_proto::record::samples;
        use dudes_proto::{
            InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionComparison,
            InstructionEndBoundary, InstructionNativeBoundaryReceipt, InstructionSettlement,
            RecordKind,
        };
        use dudes_types::Provenance;

        let (world, turn, owner) = (&mut fixture.world, fixture.turn, fixture.owner);
        unchanged_control_apply(world, 10, samples::record(RecordKind::TurnSpawned), None);
        let ended = samples::record(RecordKind::TurnEnded);
        assert!(matches!(
            ended,
            Record::TurnEnded {
                provenance: Provenance::Observed,
                ..
            }
        ));
        unchanged_control_apply(world, 11, ended, None);
        unchanged_control_apply(
            world,
            12,
            Record::InstructionObservationFinished {
                owner,
                ticket: Some(later_ticket),
                baseline: InstructionBaselineRef::Started { lsn: Lsn(9) },
                settlement: InstructionSettlement::Compared {
                    end: InstructionEndBoundary::ManagedTurn {
                        turn,
                        end_lsn: Lsn(11),
                    },
                    sampled_at: dudes_types::Timestamp::UNIX_EPOCH,
                    recovery: false,
                    comparison: InstructionComparison::default(),
                },
            },
            None,
        );
        let proof = InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
            receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(10) },
        };
        unchanged_control_apply(
            world,
            13,
            Record::InstructionAttemptBoundarySettled {
                owner,
                started_lsn: Lsn(9),
                proof: proof.clone(),
            },
            None,
        );
        proof
    }

    fn unchanged_control_review(fixture: &mut UnchangedControlFixture) {
        use dudes_proto::instruction_state::InstructionReviewFacts;
        use dudes_proto::{
            Actor, InstructionAcceptCompletion, InstructionBaselineRef, InstructionGoalClear,
            InstructionReviewAction,
        };
        use dudes_types::Seq;

        let (world, agent, card, goal) = (
            &mut fixture.world,
            fixture.agent,
            fixture.card,
            fixture.goal,
        );
        let review = world
            .instructions
            .review(card, unchanged_control_hash)
            .unwrap()
            .unwrap();
        assert!(!review.pending_comparison);
        assert_eq!(review.intervals.len(), 1);
        assert_eq!(
            review.intervals[0].baseline,
            InstructionBaselineRef::Started { lsn: Lsn(7) }
        );
        assert!(world.instructions.pending_for_goal(goal));
        assert_eq!(world.agents[&agent].goal, Some(goal));
        assert_eq!(world.goals[&goal].agent, agent);
        assert!(!world.goals[&goal].cleared);
        assert_eq!(world.completed_goal_lsn(goal), None);
        assert!(!completed_owed_observation(
            world,
            Lsn(7),
            world.instructions.attempt(Lsn(7)).unwrap(),
        ));
        unchanged_control_apply(
            world,
            14,
            Record::TicketInstructionReviewed {
                card,
                by: Actor::Peer {
                    pid: 77,
                    uid: Some(501),
                },
                expected_seq: Seq(7),
                watermark: review.watermark,
                action: InstructionReviewAction::Accept {
                    completion: InstructionAcceptCompletion::Continue,
                },
            },
            Some(InstructionReviewFacts {
                card,
                seq: Seq(7),
                goal: Some(goal),
                completed_lsn: None,
                clear_goal: Some(InstructionGoalClear { agent, goal }),
            }),
        );
    }

    fn unchanged_control_assertions(
        fixture: &UnchangedControlFixture,
        older_ticket: dudes_proto::InstructionTicketAssociation,
        later_ticket: dudes_proto::InstructionTicketAssociation,
        proof: &dudes_proto::InstructionBoundaryAbsenceProof,
    ) {
        use dudes_proto::{InstructionComparison, InstructionSettlement};

        let (world, agent, card, goal, turn, owner) = (
            &fixture.world,
            fixture.agent,
            fixture.card,
            fixture.goal,
            fixture.turn,
            fixture.owner,
        );
        let older = world.instructions.attempt(Lsn(7)).unwrap();
        let (later_started, later) = world
            .instructions
            .later_managed_attempt_for_spawn(agent, turn, Lsn(7), Lsn(10))
            .unwrap();
        assert_eq!(older.owner, owner);
        assert_eq!(older.ticket, Some(older_ticket));
        assert_eq!(older.spawn_lsn, None);
        assert_eq!(older.reviewed_lsn, Some(Lsn(14)));
        assert_eq!(older.boundary_settled, None);
        assert_eq!(later_started, Lsn(9));
        assert_eq!(later.owner, owner);
        assert_eq!(later.ticket, Some(later_ticket));
        assert_eq!(later.spawn_lsn, Some(Lsn(10)));
        assert_eq!(later.reviewed_lsn, None);
        assert_eq!(&later.boundary_settled.as_ref().unwrap().proof, proof);
        assert!(matches!(
            &later.finished.as_ref().unwrap().settlement,
            InstructionSettlement::Compared { comparison, .. }
                if *comparison == InstructionComparison::default()
        ));
        assert_eq!(world.turns[&turn].spawn_lsn, Some(Lsn(10)));
        assert!(world.turns[&turn].has_ended());
        assert_eq!(
            world.instructions.managed_end_lsn(agent, turn),
            Some(Lsn(11))
        );
        assert!(world.managed_native_spawn_receipt(Lsn(9)).is_none());
        assert!(world.instructions.pending_cards().next().is_none());
        assert!(!world.instructions.pending_for_goal(goal));
        assert!(!world.instructions.observation_pending(card));
        assert_eq!(world.instructions.ticket_reason(card), None);
        assert_eq!(
            world
                .instructions
                .watermark(card, unchanged_control_hash)
                .unwrap(),
            None
        );
        assert_eq!(
            world
                .instructions
                .review(card, unchanged_control_hash)
                .unwrap(),
            None
        );
        assert!(
            world
                .instructions
                .unresolved_launch_attempts(agent)
                .any(|(lsn, _)| *lsn == Lsn(7))
        );
        assert!(completed_owed_observation(world, Lsn(7), older));
    }

    #[test]
    fn a_synthetic_unchanged_retry_needs_no_review_receipt() {
        let mut fixture = unchanged_control_setup();
        let (older_ticket, later_ticket) = unchanged_control_attempts(&mut fixture);
        let proof = unchanged_control_finish(&mut fixture, later_ticket);
        unchanged_control_review(&mut fixture);
        unchanged_control_assertions(&fixture, older_ticket, later_ticket, &proof);
    }
}
