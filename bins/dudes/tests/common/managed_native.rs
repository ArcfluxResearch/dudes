use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::SharedHost;

use dudes_journal::Reader;
use dudes_proto::{Entry, InstructionObservationOwner, Record};
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::LaunchSpec;
use dudes_providers::stream::{FinalReply, FinalReplyCompleteness, LineRead, TurnEnd, TurnReader};
use dudes_supervisor::Input;
use dudes_supervisor::host::{
    AdoptionKey, MAX_OWNED_TARGET_QUERY_ROWS, NativeBoundaryAcknowledgement, NativeBoundaryReceipt,
    OwnedTargetEvidence, OwnedTargetState, OwnedTargets, OwnedTargetsStatus, ProbeAnswer,
    ProcessHost, SpawnResult,
};
use dudes_types::{AgentId, Lsn};

#[derive(Debug)]
pub(super) struct ManagedNativeState {
    journal: PathBuf,
    targets: Vec<ManagedNativeTarget>,
    acknowledged: Vec<(AdoptionKey, AttemptBirth)>,
    children: Option<Arc<duded::ChildEnds>>,
    watches: Vec<ManagedChildWatch>,
}

#[derive(Debug)]
struct ManagedNativeTarget {
    key: AdoptionKey,
    cwd: PathBuf,
    birth: AttemptBirth,
    receipt: NativeBoundaryReceipt,
    reaped: bool,
    observed: bool,
}

struct ManagedChildWatch {
    key: AdoptionKey,
    watcher: duded::ChildTurn,
    result: Arc<Mutex<Option<TurnEnd>>>,
}

impl std::fmt::Debug for ManagedChildWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedChildWatch")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

struct ScriptedTurnReader(Arc<Mutex<Option<TurnEnd>>>);

impl std::fmt::Debug for ScriptedTurnReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedTurnReader").finish_non_exhaustive()
    }
}

impl TurnReader for ScriptedTurnReader {
    fn line(&mut self, _text: &str) -> LineRead {
        LineRead::default()
    }

    fn end(self: Box<Self>, _exit: dudes_proto::ExitStatus) -> TurnEnd {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("the exact child completion was scripted before EOF")
    }
}

#[derive(Clone, Copy, Debug)]
struct AttemptBirth {
    tail: Lsn,
    agent: AgentId,
}

fn same_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
}

fn entries(journal: &Path) -> Option<Vec<(Lsn, Entry)>> {
    Reader::new(journal)
        .iter_from(Lsn::ZERO)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn attempt_birth(journal: &Path, spec: &LaunchSpec) -> Option<AttemptBirth> {
    let tail = entries(journal)?.last()?.0;
    let mut mailboxes = spec.env.iter().filter(|var| var.name() == "DUDES_MAILBOX");
    let mailbox = Path::new(mailboxes.next()?.expose_value());
    if mailboxes.next().is_some() || mailbox.parent()?.file_name()? != "agents" {
        return None;
    }
    let agent = AgentId::parse(mailbox.file_name()?.to_str()?).ok()?;
    Some(AttemptBirth { tail, agent })
}

fn boundary_written(journal: &Path, key: &AdoptionKey, birth: AttemptBirth) -> bool {
    let Some(entries) = entries(journal) else {
        return false;
    };
    entries.iter().any(|(settled_lsn, settled)| {
        let Record::InstructionAttemptBoundarySettled {
            owner: owner @ InstructionObservationOwner::Managed { agent, turn, .. },
            started_lsn,
            proof: dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn },
            },
        } = &settled.record else { return false; };
        if *agent != birth.agent || *started_lsn > birth.tail || *spawn_lsn <= birth.tail
            || *settled_lsn <= *spawn_lsn {
            return false;
        }
        entries.iter().any(|(lsn, entry)| *lsn == *started_lsn
            && matches!(entry.record, Record::InstructionObservationStarted { owner: seen, .. } if seen == *owner))
            && entries.iter().any(|(lsn, entry)| *lsn == *spawn_lsn
                && matches!(&entry.record, Record::TurnSpawned { agent: spawned_agent, turn: spawned_turn, pid, start_time, boot, .. }
                    if spawned_agent == agent && spawned_turn == turn && *pid == key.pid
                        && *start_time == key.start_time && *boot == key.boot))
    })
}

impl SharedHost {
    pub(crate) fn with_native_evidence(journal: PathBuf) -> Self {
        let mut host = Self::new();
        host.3 = Some(Arc::new(Mutex::new(ManagedNativeState {
            journal,
            targets: Vec::new(),
            acknowledged: Vec::new(),
            children: None,
            watches: Vec::new(),
        })));
        host
    }

    pub(super) fn bind_child_ends(&self, children: Arc<duded::ChildEnds>) {
        if let Some(native) = &self.3 {
            let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
            assert!(
                native.targets.is_empty() && native.children.is_none(),
                "bind one child owner before any spawn"
            );
            native.children = Some(children);
        }
    }

    pub(super) fn spawn_with_native(
        &mut self,
        spec: &LaunchSpec,
        install: &ManagedInstall,
    ) -> SpawnResult {
        let birth = self.3.as_ref().map(|native| {
            let native = native.lock().unwrap_or_else(PoisonError::into_inner);
            attempt_birth(&native.journal, spec).expect("current managed attempt before spawn")
        });
        let spawned = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(spec, install);
        if let SpawnResult::Spawned(key) = &spawned
            && let Some(native) = &self.3
        {
            let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
            let children = native
                .children
                .as_ref()
                .expect("child owner bound before spawn");
            let result = Arc::new(Mutex::new(None));
            children.read_next_with(Some(Box::new(ScriptedTurnReader(Arc::clone(&result)))));
            let watcher = children.watch(Some(key));
            native.watches.push(ManagedChildWatch {
                key: key.clone(),
                watcher,
                result,
            });
            native.targets.push(ManagedNativeTarget {
                key: key.clone(),
                cwd: spec.cwd.clone(),
                birth: birth.expect("native attempt retained before spawn"),
                receipt: NativeBoundaryReceipt::fresh(),
                reaped: false,
                observed: false,
            });
        }
        spawned
    }

    pub(crate) fn mark_native_reaped(&self, key: &AdoptionKey) {
        let mut host = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let native = self.3.as_ref().expect("native evidence must be opted in");
        let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
        let mut matching = native
            .targets
            .iter_mut()
            .filter(|target| same_key(&target.key, key));
        let target = matching.next().expect("exact retained spawn");
        assert!(
            matching.next().is_none(),
            "full key selects one retained attempt"
        );
        target.reaped = true;
        host.answer(key.pid, ProbeAnswer::NoSuchPid);
    }

    pub(crate) fn finish_owned_child(&self, key: &AdoptionKey, input: Input) {
        let Input::ChildExited {
            exit,
            elapsed_ms,
            usage,
            model_usage,
            outcome,
            last_message,
            final_reply,
            ..
        } = input
        else {
            panic!("expected an exact child completion payload");
        };
        let watch = {
            let native = self
                .3
                .as_ref()
                .expect("owned completion requires native opt-in");
            let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
            let mut matching = native
                .watches
                .iter()
                .enumerate()
                .filter(|(_, watch)| same_key(&watch.key, key));
            let (index, _) = matching.next().expect("exact retained owned ChildTurn");
            assert!(
                matching.next().is_none(),
                "owned completion key is unambiguous"
            );
            native.watches.remove(index)
        };
        *watch.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(TurnEnd {
            final_observation: None,
            outcome,
            usage,
            model_usage,
            last_message,
            final_reply: final_reply.map(|reply| FinalReply {
                text: reply.text,
                completeness: match reply.completeness {
                    dudes_proto::ChatReplyCompleteness::Complete => {
                        FinalReplyCompleteness::Complete
                    }
                    dudes_proto::ChatReplyCompleteness::Truncated => {
                        FinalReplyCompleteness::Truncated
                    }
                },
            }),
            usage_refused: None,
            session: None,
            attribution_refused: 0,
            unreadable: 0,
        });
        watch.watcher.ended_with_elapsed(exit, elapsed_ms);
    }

    pub(super) fn native_owned_targets(&self, limit: usize) -> OwnedTargets {
        let Some(native) = &self.3 else {
            return OwnedTargets {
                targets: Vec::new(),
                status: OwnedTargetsStatus::UnknownUnsupported,
            };
        };
        let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
        let targets: Vec<_> = native
            .targets
            .iter_mut()
            .take(limit.min(MAX_OWNED_TARGET_QUERY_ROWS))
            .map(|target| {
                target.observed |= target.reaped;
                OwnedTargetEvidence {
                    key: Some(target.key.clone()),
                    cwd: target.cwd.clone(),
                    state: if target.reaped {
                        OwnedTargetState::NativeBoundaryAbsent
                    } else {
                        OwnedTargetState::Present
                    },
                    native_absence_receipt: target.reaped.then(|| target.receipt.clone()),
                }
            })
            .collect();
        let status = if targets.len() == native.targets.len() {
            OwnedTargetsStatus::Complete
        } else {
            OwnedTargetsStatus::Truncated {
                omitted: native.targets.len() - targets.len(),
            }
        };
        OwnedTargets { targets, status }
    }

    pub(super) fn acknowledge_native(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        let Some(native) = &self.3 else {
            return NativeBoundaryAcknowledgement::UnknownUnsupported;
        };
        let mut native = native.lock().unwrap_or_else(PoisonError::into_inner);
        let mut matching = native
            .targets
            .iter()
            .enumerate()
            .filter(|(_, target)| same_key(&target.key, key));
        let Some((index, target)) = matching.next() else {
            return NativeBoundaryAcknowledgement::UnknownIdentity;
        };
        if matching.next().is_some() || target.receipt != *receipt {
            return NativeBoundaryAcknowledgement::UnknownIdentity;
        }
        if !target.reaped
            || !target.observed
            || !boundary_written(&native.journal, key, target.birth)
        {
            return NativeBoundaryAcknowledgement::RefusedUnobserved;
        }
        let target = native.targets.remove(index);
        native.acknowledged.push((key.clone(), target.birth));
        NativeBoundaryAcknowledgement::Acknowledged
    }

    pub(crate) fn await_native_acknowledged(&self, key: &AdoptionKey) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let native = self.3.as_ref().expect("native evidence must be opted in");
                let native = native.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some((_, birth)) = native
                    .acknowledged
                    .iter()
                    .find(|(ack, _)| same_key(ack, key))
                {
                    assert!(
                        !native
                            .targets
                            .iter()
                            .any(|target| same_key(&target.key, key))
                    );
                    assert!(boundary_written(&native.journal, key, *birth));
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "exact native absence was not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
