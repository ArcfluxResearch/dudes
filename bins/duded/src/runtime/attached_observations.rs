use std::collections::VecDeque;

use anyhow::{Context as _, Result};
use dudes_proto::{
    Entry, InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionEndBoundary,
    InstructionNativeBoundaryReceipt, InstructionObservationOwner, InstructionSettlement, Record,
};
use dudes_supervisor::host::{AdoptionKey, NativeBoundaryReceipt};
use dudes_supervisor::{Clock, Input, ProcessHost, Rng, Supervisor};
use dudes_types::{AgentId, Lsn, ModelSelection};

use super::{Ctx, Shared};
use crate::predicate::PredicateRunner;
use dudes_proto::rpc::pty::PtyId;

pub(crate) struct AttachedBoundary {
    pty: PtyId,
    owner: InstructionObservationOwner,
    started: Lsn,
    key: Option<AdoptionKey>,
    token: Option<NativeBoundaryReceipt>,
    native_acknowledged: bool,
    settlement: Option<(Lsn, Lsn)>,
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    fn append_instruction_boundary<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        record: Record,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<Lsn> {
        let entry = Entry {
            at: self.clock.now(),
            record,
        };
        let lsn = self.append(sup.world(), &entry)?;
        sup.handle(Input::InstructionJournaled { lsn, entry });
        anyhow::ensure!(
            sup.world().last_applied_lsn == Some(lsn),
            "instruction boundary was not folded"
        );
        anyhow::ensure!(
            sup.effects().is_empty(),
            "instruction boundary emitted unrelated effects"
        );
        self.tokens.mark_folded();
        self.publish_folded_floor(sup.world(), waiting)?;
        Ok(lsn)
    }

    pub(super) fn start_attached_boundary<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        agent: AgentId,
        cwd: &std::path::Path,
        queue: &VecDeque<Input>,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<(InstructionObservationOwner, Lsn)> {
        anyhow::ensure!(
            !queue.iter().any(|input| matches!(
                input,
                Input::Persisted { .. }
                    | Input::Journaled { .. }
                    | Input::InstructionJournaled { .. }
                    | Input::BoardChanged { .. }
            )),
            "attached baseline cannot pass an earlier acknowledgment"
        );
        let attachment_lsn = sup
            .world()
            .instructions
            .attachment_lsn(agent)
            .context("attached owner has no durable attachment")?;
        anyhow::ensure!(
            sup.world()
                .instructions
                .attempt_for_attachment(attachment_lsn)
                .is_none(),
            "attachment already has a physical starting boundary"
        );
        let owner = InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn,
        };
        let goal = sup.world().agents.get(&agent).and_then(|row| row.goal);
        let (snapshot, ticket) = self.sample_instruction_boundary(
            sup,
            owner,
            goal,
            &ModelSelection::provider_default(),
            None,
            cwd,
        )?;
        snapshot.validate()?;
        let started = self.append_instruction_boundary(
            sup,
            Record::InstructionObservationStarted {
                owner,
                ticket,
                snapshot,
            },
            waiting,
        )?;
        Ok((owner, started))
    }

    pub(super) fn remember_attached_boundary(
        &mut self,
        pty: PtyId,
        owner: InstructionObservationOwner,
        started: Lsn,
        key: Option<AdoptionKey>,
    ) {
        let token = self.pty_host.track_native_close(pty).ok();
        self.attached_instruction_boundaries.insert(
            started,
            AttachedBoundary {
                pty,
                owner,
                started,
                key,
                token,
                native_acknowledged: false,
                settlement: None,
            },
        );
    }

    pub(super) fn owns_attached_boundary(&self, agent: AgentId) -> bool {
        self.attached_instruction_boundaries.values().any(|boundary|
            matches!(boundary.owner, InstructionObservationOwner::AttachedProcess { agent: owner, .. } if owner == agent))
    }

    pub(super) fn retry_attached_acknowledgements(&mut self, world: &dudes_supervisor::World) {
        let mut retired = Vec::new();
        for (&started, boundary) in self
            .attached_instruction_boundaries
            .iter_mut()
            .take(dudes_supervisor::host::MAX_OWNED_TARGET_QUERY_ROWS)
        {
            let Some((settled_lsn, spawn_lsn)) = boundary.settlement else {
                continue;
            };
            let Some(attempt) = world.instructions.attempt(started) else {
                continue;
            };
            if attempt.owner != boundary.owner
                || attempt.spawn_lsn != Some(spawn_lsn)
                || attempt.boundary_settled.as_ref().is_none_or(|settled| {
                    settled.lsn != settled_lsn
                        || settled.proof
                            != InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                                receipt: InstructionNativeBoundaryReceipt::Spawned {
                                    lsn: spawn_lsn,
                                },
                            }
                })
            {
                continue;
            }
            if !boundary.native_acknowledged
                && boundary.key.is_some()
                && let Some(token) = &boundary.token
                && self.pty_host.acknowledge_native_close(boundary.pty, token)
            {
                boundary.native_acknowledged = true;
            }
            if boundary.native_acknowledged && attempt.finished.is_some() {
                retired.push(started);
            }
        }
        for started in retired {
            self.attached_instruction_boundaries.remove(&started);
        }
    }

    pub(super) fn settle_one_attached_boundary<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        queried: &mut std::collections::BTreeSet<Lsn>,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<bool> {
        let candidates: Vec<_> = self
            .attached_instruction_boundaries
            .iter()
            .take(dudes_supervisor::host::MAX_OWNED_TARGET_QUERY_ROWS)
            .filter(|(started, boundary)| {
                boundary.settlement.is_none() && !queried.contains(started)
            })
            .map(|(&started, boundary)| {
                (
                    started,
                    boundary.owner,
                    boundary.pty,
                    boundary.key.clone(),
                    boundary.token.clone(),
                )
            })
            .collect();
        for (started, owner, pty, key, token) in candidates {
            queried.insert(started);
            let (Some(key), Some(token)) = (key, token) else {
                continue;
            };
            if self.pty_host.native_close_absence(pty, &token).is_err() {
                continue;
            }
            let Some(spawn_lsn) = sup
                .world()
                .attached_native_spawn_receipt(started)
                .filter(|spawn| {
                    spawn.owner == owner
                        && spawn.pty == pty
                        && super::native_boundaries::same_key(&key, &spawn.key)
                })
                .map(|spawn| spawn.spawn_lsn)
            else {
                continue;
            };
            let settled_lsn = self.append_instruction_boundary(
                sup,
                Record::InstructionAttemptBoundarySettled {
                    owner,
                    started_lsn: started,
                    proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                        receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn },
                    },
                },
                waiting,
            )?;
            if let Some(boundary) = self.attached_instruction_boundaries.get_mut(&started) {
                boundary.settlement = Some((settled_lsn, spawn_lsn));
            }
            self.retry_attached_acknowledgements(sup.world());
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn finish_attached_boundary<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        agent: AgentId,
        pty: PtyId,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<()> {
        let mut matched = self
            .attached_instruction_boundaries
            .values()
            .filter(|boundary| {
                boundary.pty == pty
                    && matches!(boundary.owner,
                InstructionObservationOwner::AttachedProcess { agent: owner, .. } if owner == agent)
            });
        let Some(boundary) = matched.next() else {
            return Ok(());
        };
        anyhow::ensure!(
            matched.next().is_none(),
            "PTY close has ambiguous starting identities"
        );
        let InstructionObservationOwner::AttachedProcess {
            agent: owner,
            attachment_lsn: _,
        } = boundary.owner
        else {
            anyhow::bail!("PTY boundary has a different owner kind");
        };
        anyhow::ensure!(owner == agent, "PTY boundary has a different agent");
        let (owner, started, key, token, acknowledged) = (
            boundary.owner,
            boundary.started,
            boundary.key.clone(),
            boundary.token.clone(),
            boundary.native_acknowledged,
        );
        if !acknowledged
            && let Some(token) = &token
            && self.pty_host.native_close_absence(pty, token).is_ok()
        {
            let spawn = sup
                .world()
                .attached_native_spawn_receipt(started)
                .filter(|spawn| spawn.owner == owner && spawn.pty == pty)
                .filter(|spawn| {
                    key.as_ref()
                        .is_some_and(|key| super::native_boundaries::same_key(key, &spawn.key))
                });
            if let Some(spawn) = spawn {
                let receipt_lsn = spawn.spawn_lsn;
                let settled_lsn = self.append_instruction_boundary(
                    sup,
                    Record::InstructionAttemptBoundarySettled {
                        owner,
                        started_lsn: started,
                        proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                            receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: receipt_lsn },
                        },
                    },
                    waiting,
                )?;
                if let Some(boundary) = self.attached_instruction_boundaries.get_mut(&started) {
                    boundary.settlement = Some((settled_lsn, receipt_lsn));
                }
            }
        }
        self.retry_attached_acknowledgements(sup.world());
        self.compare_closed_attachment(sup, pty, owner, started, key.is_none(), waiting)?;
        if self
            .attached_instruction_boundaries
            .get(&started)
            .is_some_and(|boundary| boundary.native_acknowledged)
        {
            self.attached_instruction_boundaries.remove(&started);
        }
        Ok(())
    }
    fn compare_closed_attachment<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        pty: PtyId,
        owner: InstructionObservationOwner,
        started: Lsn,
        keyless: bool,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<()> {
        let InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn,
        } = owner
        else {
            anyhow::bail!("PTY boundary has a different owner kind");
        };
        let Some((actual_started, attempt)) = sup
            .world()
            .instructions
            .attempt_for_attachment(attachment_lsn)
        else {
            anyhow::bail!("attached starting evidence is unavailable");
        };
        anyhow::ensure!(
            actual_started == started && attempt.owner == owner,
            "attached starting identity changed"
        );
        if attempt.finished.is_none() {
            let snapshot = attempt
                .snapshot
                .clone()
                .context("attached baseline has no sample")?;
            let ticket = attempt.ticket;
            let end_lsn = sup.world().instructions.attachment_end_lsn(attachment_lsn);
            if end_lsn.is_none()
                && keyless
                && attempt.spawn_lsn.is_none()
                && attempt.boundary_settled.is_none()
                && sup.world().attached_native_spawn_receipt(started).is_none()
            {
                self.finish_keyless_attachment(sup, owner, ticket, started, snapshot, waiting)?;
                return Ok(());
            }
            let end_lsn = end_lsn.context("attached close has no durable ending boundary")?;
            let cwd = snapshot
                .execution_folder
                .as_ref()
                .and_then(|path| path.to_path().ok())
                .or_else(|| self.pty_host.receipt(pty).map(|(_, cwd)| cwd.to_path_buf()))
                .or_else(|| sup.world().agents.get(&agent).map(|row| row.cwd.clone()))
                .context("attached comparison path is unavailable")?;
            let (endpoint, _) = self.sample_instruction_boundary(
                sup,
                owner,
                ticket.map(|ticket| ticket.goal),
                &ModelSelection::provider_default(),
                Some(&snapshot),
                &cwd,
            )?;
            let comparison = crate::instruction_sample::compare(&snapshot, &endpoint);
            self.append_instruction_boundary(
                sup,
                Record::InstructionObservationFinished {
                    owner,
                    ticket,
                    baseline: InstructionBaselineRef::Started { lsn: started },
                    settlement: InstructionSettlement::Compared {
                        end: InstructionEndBoundary::AttachedProcess {
                            attachment_lsn,
                            end_lsn,
                        },
                        sampled_at: endpoint.sampled_at,
                        recovery: false,
                        comparison,
                    },
                },
                waiting,
            )?;
        }
        Ok(())
    }

    fn finish_keyless_attachment<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        owner: InstructionObservationOwner,
        ticket: Option<dudes_proto::InstructionTicketAssociation>,
        started: Lsn,
        snapshot: dudes_proto::InstructionSnapshot,
        waiting: &mut crate::closing::Waiting,
    ) -> Result<()> {
        let InstructionObservationOwner::AttachedProcess { agent, .. } = owner else {
            anyhow::bail!("keyless close has a different owner kind");
        };
        let mut comparison = dudes_proto::InstructionComparison::default();
        let _ = comparison.try_push_gap(dudes_proto::InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: dudes_proto::InstructionGapKind::UnrecordedLaunchBoundary,
            context: dudes_proto::InstructionPathContext::Unresolved,
            agent: Some(agent),
            path: None,
        });
        for gap in snapshot.gaps {
            let _ = comparison.try_push_gap(gap);
        }
        self.append_instruction_boundary(
            sup,
            Record::InstructionObservationFinished {
                owner,
                ticket,
                baseline: InstructionBaselineRef::Started { lsn: started },
                settlement: InstructionSettlement::Unknown {
                    reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
                    end: None,
                    sampled_at: None,
                    recovery: false,
                    comparison,
                },
            },
            waiting,
        )?;
        Ok(())
    }
}
