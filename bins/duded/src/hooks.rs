//! The one `hook.event` route from the local transport to the supervisor.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use dudes_proto::rpc::hook::HookEventParams;
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_proto::{EventKind, HookReply};
use dudes_rpc::auth::{PeerRole, Requirement};
use dudes_rpc::{Dispatcher, dispatch::error};
use dudes_supervisor::Clock;
use dudes_types::AgentId;

use crate::Handle;

/// The stable overload response. No event was queued, so the error does not claim a timeout.
pub use dudes_proto::rpc::hook::OVERLOADED;

#[derive(Debug)]
struct Admission {
    /// Maximum outstanding `Job::Hook` values. This bounds job count; it does not claim that a
    /// serialized frame's byte length equals the heap occupied by its parsed `serde_json::Value`.
    cap: usize,
    per_agent_cap: Option<NonZeroUsize>,
    outstanding: Mutex<Outstanding>,
}

#[derive(Debug, Default)]
struct Outstanding {
    total: usize,
    by_agent: BTreeMap<AgentId, usize>,
}

impl Admission {
    fn new(cap: NonZeroUsize, per_agent_cap: Option<NonZeroUsize>) -> Arc<Self> {
        Arc::new(Self {
            cap: cap.get(),
            per_agent_cap,
            outstanding: Mutex::new(Outstanding::default()),
        })
    }

    fn acquire(self: &Arc<Self>, agent: AgentId) -> Option<HookPermit> {
        let mut outstanding = self
            .outstanding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if outstanding.total >= self.cap {
            return None;
        }
        let charged_agent = if let Some(cap) = self.per_agent_cap {
            let owned = outstanding.by_agent.entry(agent).or_default();
            if *owned >= cap.get() {
                return None;
            }
            *owned += 1;
            Some(agent)
        } else {
            None
        };
        outstanding.total += 1;
        Some(HookPermit {
            lease: Arc::new(ReplyLease {
                admission: Arc::clone(self),
                charged_agent,
                phase: Mutex::new(LeasePhase::Waiting),
            }),
        })
    }
}

/// One admitted hook job. It is deliberately not `Clone`: the queued job owns the slot until the
/// loop pumps or drops that job.
#[derive(Debug)]
pub struct HookPermit {
    lease: Arc<ReplyLease>,
}

#[derive(Debug)]
enum LeasePhase {
    Waiting,
    Expired,
    Completed,
}

#[derive(Debug)]
pub(crate) struct ReplyLease {
    admission: Arc<Admission>,
    charged_agent: Option<AgentId>,
    phase: Mutex<LeasePhase>,
}

impl ReplyLease {
    pub(crate) fn expire(&self) {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*phase, LeasePhase::Waiting) {
            *phase = LeasePhase::Expired;
        }
    }
}

impl HookPermit {
    #[cfg(feature = "testing")]
    pub(crate) fn isolated_for_test(agent: AgentId) -> Self {
        Admission::new(NonZeroUsize::MIN, Some(NonZeroUsize::MIN))
            .acquire(agent)
            .expect("the isolated hook admission is empty")
    }

    pub(crate) fn reply_lease(&self) -> Arc<ReplyLease> {
        Arc::clone(&self.lease)
    }

    pub(crate) fn reply_is_waiting(&self) -> bool {
        matches!(
            *self
                .lease
                .phase
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            LeasePhase::Waiting
        )
    }
}

impl Drop for HookPermit {
    fn drop(&mut self) {
        let mut phase = self
            .lease
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut outstanding = self
            .lease
            .admission
            .outstanding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        outstanding.total -= 1;
        if let Some(agent) = self.lease.charged_agent {
            let owned = outstanding
                .by_agent
                .get_mut(&agent)
                .expect("a retained control permit owns an agent charge");
            *owned -= 1;
            if *owned == 0 {
                outstanding.by_agent.remove(&agent);
            }
        }
        *phase = LeasePhase::Completed;
    }
}

/// The two daemon-owned inputs the hook route shares with the ordinary control surface.
pub struct Ingress<C> {
    handle: Handle,
    clock: Arc<C>,
}

impl<C> Ingress<C> {
    /// Bundle the loop handle and the daemon's single clock for dispatcher construction.
    #[must_use]
    pub fn new(handle: Handle, clock: Arc<C>) -> Self {
        Self { handle, clock }
    }

    /// The same loop handle ordinary RPC commands use.
    #[must_use]
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

/// Register `hook.event` on the daemon's production dispatcher.
pub fn register<C>(dispatcher: &mut Dispatcher, ingress: Ingress<C>, cap: NonZeroUsize)
where
    C: Clock + Send + Sync + 'static,
{
    let Ingress { handle, clock } = ingress;
    let share = NonZeroUsize::new(cap.get() / 2).unwrap_or(NonZeroUsize::MIN);
    let control = Admission::new(cap, Some(share));
    let other = Admission::new(cap, None);
    dispatcher.register(Method::HookEvent, Requirement::Agent, move |call| {
        let PeerRole::Agent { agent } = call.role else {
            return Err(error(
                ErrorCode::InternalError,
                "hook.event passed its Agent requirement without an agent role",
            ));
        };
        let params: HookEventParams = dudes_rpc::methods::params(&call)?;
        let event = dudes_providers::claude::translate(*agent, clock.now(), &params.raw)
            .map_err(|reason| invalid_payload(&reason))?;
        let admission = if matches!(
            &event.kind,
            EventKind::PreTool { .. } | EventKind::PromptSubmit
        ) {
            &control
        } else {
            &other
        };
        let permit = admission
            .acquire(*agent)
            .ok_or_else(|| error(ErrorCode::InternalError, OVERLOADED))?;
        let reply = if dudes_providers::claude::requires_reply(&event) {
            handle.hook(event.clone(), permit)?
        } else {
            handle.observe_hook(event.clone(), permit)?;
            HookReply::ALLOW
        };
        let reply = dudes_providers::claude::encode_reply(&reply, &event);
        dudes_rpc::methods::encode(Method::HookEvent, &reply)
    });
}

fn invalid_payload(reason: &dudes_providers::claude::HookTranslationError) -> RpcError {
    error(ErrorCode::InvalidParams, format!("hook.event: {reason}"))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use dudes_proto::Event;
    use dudes_supervisor::{Input, RuntimeSignal};
    use dudes_types::{AgentId, Provenance, Timestamp};

    use super::*;
    use crate::ports::Job;

    fn event() -> Event {
        Event {
            agent: AgentId::NIL,
            at: Timestamp::from_unix_millis(1_789_000_000_000),
            provenance: Provenance::Observed,
            kind: EventKind::PromptSubmit,
            raw: None,
        }
    }

    #[test]
    fn confirmed_control_answers_and_errors_survive_the_deadline_change() {
        for stopped in [None, Some(dudes_types::Lsn(9))] {
            for original in [
                Ok(HookReply::allow_with("exact existing context")),
                Ok(HookReply {
                    decision: dudes_proto::HookDecision::Deny {
                        reason: "the loop is paused".to_owned(),
                    },
                    context: None,
                }),
                Ok(HookReply {
                    decision: dudes_proto::HookDecision::Halt,
                    context: None,
                }),
                Err(crate::ports::journal_stopped_error(dudes_types::Lsn(7))),
            ] {
                let admission = Admission::new(NonZeroUsize::MIN, None);
                let (jobs, queued) = mpsc::channel();
                let handle = Handle::new(jobs);
                let permit = admission.acquire(AgentId::NIL).unwrap();
                let caller = handle.clone();
                let answer = std::thread::spawn(move || caller.hook(event(), permit));
                let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
                if let Some(lsn) = stopped {
                    handle.publish_journal_stopped(lsn);
                }
                let Job::Hook {
                    reply: Some(reply), ..
                } = &held
                else {
                    panic!("the actual writer reply channel")
                };
                reply
                    .send(original.clone().map(|reply| crate::ports::HookAnswer {
                        reply,
                        stopped_at: stopped,
                    }))
                    .unwrap();
                let received = answer.join().unwrap();
                drop(held);
                assert_eq!(received, original);
                assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
            }
        }
    }

    #[test]
    fn a_fenced_hook_timeout_denies_instead_of_failing_open() {
        let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
        let permit = admission.acquire(AgentId::NIL).unwrap();
        let (jobs, queued) = mpsc::channel();
        let handle = Handle::new(jobs);
        let caller = handle.clone();
        let reply = std::thread::spawn(move || caller.hook(event(), permit));
        let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(handle.fence_journal());
        let result = reply.join().unwrap().unwrap();
        assert_eq!(
            result,
            HookReply {
                decision: dudes_proto::HookDecision::Deny {
                    reason: "journal state unresolved; restart to replay before new work or writes"
                        .to_owned(),
                },
                context: None,
            }
        );
        drop(held);
    }

    #[test]
    fn a_late_successful_hook_reply_is_filtered_at_the_return_boundary() {
        for fenced in [false, true] {
            let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
            let permit = admission.acquire(AgentId::NIL).unwrap();
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs);
            let caller = handle.clone();
            let answer = std::thread::spawn(move || caller.hook(event(), permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            if fenced {
                handle.fence_journal();
            }
            let Job::Hook {
                reply: Some(reply), ..
            } = &held
            else {
                panic!("hook reply is retained")
            };
            reply
                .send(Ok(crate::ports::HookAnswer::from(HookReply::allow_with(
                    "healthy context",
                ))))
                .unwrap();
            let received = answer.join().unwrap().unwrap();
            if fenced {
                assert!(matches!(
                    received.decision,
                    dudes_proto::HookDecision::Deny { .. }
                ));
                assert_eq!(received.context, None);
            } else {
                assert_eq!(received, HookReply::allow_with("healthy context"));
            }
        }
    }

    #[test]
    fn journal_uncertainty_keeps_halt_precedence_without_context() {
        let admission = Admission::new(NonZeroUsize::MIN, None);
        let permit = admission.acquire(AgentId::NIL).unwrap();
        let (jobs, queued) = mpsc::channel();
        let handle = Handle::new(jobs);
        let caller = handle.clone();
        let answer = std::thread::spawn(move || caller.hook(event(), permit));
        let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.fence_journal();
        let Job::Hook {
            reply: Some(reply), ..
        } = &held
        else {
            panic!("the actual writer reply channel")
        };
        reply
            .send(Ok(HookReply {
                decision: dudes_proto::HookDecision::Halt,
                context: Some("old context".to_owned()),
            }
            .into()))
            .unwrap();
        assert_eq!(
            answer.join().unwrap().unwrap(),
            HookReply {
                decision: dudes_proto::HookDecision::Halt,
                context: None,
            }
        );
        drop(held);
    }

    #[test]
    fn poison_preserves_only_writer_proven_no_write_replies_across_the_return_boundary() {
        use crate::ports::HookAnswer;
        use dudes_types::Lsn;
        for (tag, context, accepted) in [
            (None, None, false),
            (None, Some("old context"), false),
            (Some(Lsn(9)), None, true),
            (Some(Lsn(8)), None, false),
        ] {
            let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
            let permit = admission.acquire(AgentId::NIL).unwrap();
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs);
            let caller = handle.clone();
            let answer = std::thread::spawn(move || caller.hook(event(), permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            handle.publish_journal_stopped(Lsn(9));
            let Job::Hook {
                reply: Some(reply), ..
            } = &held
            else {
                panic!("queued hook reply expected")
            };
            reply
                .send(Ok(HookAnswer {
                    reply: context.map_or(HookReply::ALLOW, HookReply::allow_with),
                    stopped_at: tag,
                }))
                .unwrap();
            let received = answer.join().unwrap();
            if accepted {
                assert_eq!(received.unwrap(), HookReply::ALLOW);
            } else {
                let error = received.unwrap_err();
                assert_eq!(error.code, dudes_proto::rpc::JOURNAL_STOPPED_CODE);
                assert_eq!(error.data.unwrap()["lsn"], serde_json::json!(Lsn(9)));
            }
        }
    }

    #[test]
    fn a_pure_closing_notice_survives_poison_but_never_bypasses_coherence() {
        use crate::ports::HookAnswer;
        use dudes_supervisor::testing::Harness;
        use dudes_types::Lsn;
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.set_goal(agent, &["/usr/bin/true"]);
        let announcement = Lsn(10_000);
        let effects = harness
            .supervisor
            .closing_announce()
            .unwrap()
            .iter()
            .map(dudes_supervisor::testing::Seen::of)
            .collect::<Vec<_>>();
        harness.trace.extend(effects.iter().cloned());
        let token = match effects.as_slice() {
            [dudes_supervisor::testing::Seen::Persist { token, .. }] => *token,
            _ => panic!("one closing announcement expected"),
        };
        harness.drive(harness.persisted(token, announcement));
        let pure = harness
            .supervisor
            .hook_reply_without_write(agent, &EventKind::PromptSubmit)
            .unwrap();
        assert_eq!(
            pure.context,
            Some(dudes_policy::closing_notice(announcement))
        );
        for (tag, coherence, accepted) in [
            (Some(Lsn(9)), false, true),
            (Some(Lsn(8)), false, false),
            (None, false, false),
            (Some(Lsn(9)), true, false),
        ] {
            let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
            let permit = admission.acquire(agent).unwrap();
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs);
            let caller = handle.clone();
            let mut event = event();
            event.agent = agent;
            let answer = std::thread::spawn(move || caller.hook(event, permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            handle.publish_journal_stopped(Lsn(9));
            if coherence {
                handle.fence_journal();
            }
            let Job::Hook {
                reply: Some(reply), ..
            } = &held
            else {
                panic!("queued hook reply expected")
            };
            reply
                .send(Ok(HookAnswer {
                    reply: pure.clone(),
                    stopped_at: tag,
                }))
                .unwrap();
            let received = answer.join().unwrap();
            if accepted {
                assert_eq!(received.unwrap(), pure);
            } else if coherence {
                let reply = received.unwrap();
                assert!(matches!(
                    reply.decision,
                    dudes_proto::HookDecision::Deny { .. }
                ));
                assert_eq!(reply.context, None);
            } else {
                assert_eq!(
                    received.unwrap_err().code,
                    dudes_proto::rpc::JOURNAL_STOPPED_CODE
                );
            }
        }
    }

    #[test]
    fn a_control_deadline_stays_denied_when_the_journal_stopped() {
        for kind in [
            EventKind::PreTool {
                name: "Bash".to_owned(),
                input: serde_json::json!({"command":"fixture"}),
            },
            EventKind::PromptSubmit,
        ] {
            let admission = Admission::new(NonZeroUsize::MIN, None);
            let permit = admission.acquire(AgentId::NIL).unwrap();
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs).with_hook_reply_timeout(Duration::from_millis(250));
            let caller = handle.clone();
            let original = Event { kind, ..event() };
            let sent = original.clone();
            let answer = std::thread::spawn(move || caller.hook(sent, permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            handle.publish_journal_stopped(dudes_types::Lsn(9));
            let result = answer.join().unwrap();
            let Job::Hook { event, .. } = &held else {
                panic!("the admitted event remains queued")
            };
            assert_eq!(event, &original);
            assert!(admission.acquire(AgentId::NIL).is_none());
            assert!(matches!(
                queued.recv_timeout(Duration::from_secs(1)).unwrap(),
                Job::Input(input) if matches!(*input, Input::Signal {
                    agent: AgentId::NIL,
                    signal: RuntimeSignal::HookTimeout,
                })
            ));
            assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
            drop(held);
            assert!(admission.acquire(AgentId::NIL).is_some());
            assert_eq!(
                result.unwrap(),
                HookReply {
                    decision: dudes_proto::HookDecision::Deny {
                        reason: "the daemon did not answer the control hook; retry, and restart duded if this persists".to_owned(),
                    },
                    context: None,
                }
            );
        }
    }

    #[test]
    fn stop_boundary_deadlines_keep_an_empty_allow() {
        for kind in [
            EventKind::TurnEnd { last_message: None },
            EventKind::SubagentEnd,
        ] {
            let admission = Admission::new(NonZeroUsize::MIN, None);
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs).with_hook_reply_timeout(Duration::from_millis(20));
            let permit = admission.acquire(AgentId::NIL).unwrap();
            let answer = std::thread::spawn(move || handle.hook(Event { kind, ..event() }, permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            let result = answer.join().unwrap().unwrap();
            drop(held);
            assert_eq!(
                result,
                HookReply {
                    decision: dudes_proto::HookDecision::Allow,
                    context: None,
                }
            );
        }
    }

    #[test]
    fn non_control_timeouts_and_control_disconnect_keep_the_typed_delivery_error() {
        use dudes_types::Lsn;
        for (kind, disconnect) in [
            (EventKind::TurnEnd { last_message: None }, false),
            (EventKind::SubagentEnd, false),
            (EventKind::PromptSubmit, true),
        ] {
            let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
            let permit = admission.acquire(AgentId::NIL).unwrap();
            let (jobs, queued) = mpsc::channel();
            let handle = Handle::new(jobs).with_hook_reply_timeout(Duration::from_millis(250));
            let caller = handle.clone();
            let answer = std::thread::spawn(move || caller.hook(Event { kind, ..event() }, permit));
            let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
            handle.publish_journal_stopped(Lsn(9));
            let held = (!disconnect).then_some(held);
            let error = answer.join().unwrap().unwrap_err();
            assert_eq!(error.code, dudes_proto::rpc::JOURNAL_STOPPED_CODE);
            assert_eq!(error.data.unwrap()["lsn"], serde_json::json!(Lsn(9)));
            drop(held);
        }
    }

    #[test]
    fn reply_lease_expires_at_timeout_and_keeps_its_physical_slot() {
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let admission = Admission::new(NonZeroUsize::new(1).unwrap(), None);
        let permit = admission.acquire(agent).expect("the free slot");
        let lease = permit.reply_lease();
        let event = Event {
            agent,
            at: Timestamp::from_unix_millis(1_789_000_000_000),
            provenance: Provenance::Observed,
            kind: EventKind::PromptSubmit,
            raw: None,
        };
        let (jobs, queued) = mpsc::channel();
        let handle = Handle::new(jobs);
        let caller = std::thread::spawn(move || handle.hook(event, permit));
        let held = queued
            .recv_timeout(Duration::from_secs(1))
            .expect("the retained hook");
        let Job::Hook { permit, .. } = &held else {
            panic!("the admitted hook");
        };
        assert!(permit.reply_is_waiting());
        assert_eq!(
            caller.join().unwrap().unwrap(),
            HookReply {
                decision: dudes_proto::HookDecision::Deny {
                    reason: "the daemon did not answer the control hook; retry, and restart duded if this persists".to_owned(),
                },
                context: None,
            }
        );
        assert!(!permit.reply_is_waiting());
        assert!(matches!(*lease.phase.lock().unwrap(), LeasePhase::Expired));
        assert!(
            admission.acquire(agent).is_none(),
            "the physical slot stays full"
        );
        assert!(matches!(
            queued.recv().unwrap(),
            Job::Input(input) if matches!(*input, Input::Signal {
                agent: owner,
                signal: RuntimeSignal::HookTimeout,
            } if owner == agent)
        ));
        assert!(matches!(
            queued.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        drop(held);
        lease.expire();
        assert!(matches!(
            *lease.phase.lock().unwrap(),
            LeasePhase::Completed
        ));
        let restored = admission
            .acquire(agent)
            .expect("completion restores the slot");
        assert!(restored.reply_is_waiting());
    }
}
