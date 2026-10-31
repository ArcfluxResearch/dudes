//! Pure admission input for a goal-free agent's mail turn.

use dudes_proto::{GateStep, MailRef, MailTurnAdmission, Spend};
use dudes_types::{AgentId, BreakerRung, Lsn, Provenance, ThreadId};
use serde::Serialize;

use crate::{BudgetState, Control};

/// Most mail turns one goal-free agent gets for one thread.
pub const THREAD_TURN_CAP: u32 = 2;
/// Most mail turns one goal-free agent gets for one sender in one UTC day.
pub const SENDER_DAY_TURN_CAP: u32 = 4;
/// Most mail turns and mail wakes together one agent gets in one UTC day.
pub const AGENT_DAY_TURN_CAP: u32 = 20;

/// What the replayed issues already used of each cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MailTurnUse {
    /// Mail turns issued for this agent and thread.
    pub thread: u32,
    /// Mail turns issued for this agent and the thread's sender today.
    pub sender_today: u32,
    /// Mail turns and mail wakes issued for this agent today.
    pub agent_today: u32,
}

/// Pure admission state for the oldest thread with wake-worthy mail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MailTurnInput {
    /// Commands selected from the immutable recorded hire before hashing this input.
    #[serde(skip_serializing_if = "dudes_proto::TaskCommandSet::is_legacy")]
    pub command_set: dudes_proto::TaskCommandSet,
    /// The idle chat agent.
    pub agent: AgentId,
    /// The conversation being answered.
    pub thread: ThreadId,
    /// The wake-worthy mail this turn would cover, oldest first.
    pub covered: Vec<MailRef>,
    /// Digest of what the mailbox inspection reported.
    pub inspection_digest: String,
    /// The identity line the task carries.
    pub identity: dudes_proto::IdentityLine,
    /// Current control flags.
    pub control: Control,
    /// Most recent terminal provenance.
    pub last_turn_end: Option<Provenance>,
    /// Unanswered structured question.
    pub open_question: Option<String>,
    /// The standing question's position, present exactly when a question is open.
    pub question_lsn: Option<Lsn>,
    /// Fresh spending against agent and Floor.
    pub budget_state: BudgetState,
    /// Agent's configured daily limits.
    pub agent_budget: dudes_proto::AgentBudget,
    /// Floor's configured daily dollar limit.
    pub floor_usd_per_day: Option<dudes_types::Usd>,
    /// A visible billing credential lacks a ceiling applicable to this engine.
    pub billing_ceiling_missing: bool,
    /// Conservative reserve per unknown end.
    pub per_turn_floor: Spend,
    /// Current breaker rung.
    pub rung: BreakerRung,
    /// What the replayed issues already used of each cap.
    pub used: MailTurnUse,
}

/// A mail turn verdict whose type cannot complete a goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MailTurnDecided {
    /// Issue the turn, or the refusal to retain.
    pub decision: MailTurnAdmission,
    /// Consulted gates in their fixed order.
    pub gate_trace: Vec<GateStep>,
}

#[cfg(test)]
mod tests {
    use dudes_proto::{ExhaustedReason, Gate, HoldReason, MailTurnTask};
    use dudes_types::MailId;

    use super::*;
    use crate::admit_mail_turn;

    fn input() -> MailTurnInput {
        MailTurnInput {
            command_set: dudes_proto::TaskCommandSet::Legacy,
            agent: AgentId::NIL,
            thread: ThreadId::from_parts(1, 2),
            covered: vec![MailRef {
                id: MailId::from_parts(1, 3),
                lsn: Lsn(10),
            }],
            inspection_digest: "0".repeat(64),
            identity: dudes_proto::IdentityLine::new(AgentId::NIL, "Kim", "backend engineer"),
            control: Control::default(),
            last_turn_end: None,
            open_question: None,
            question_lsn: None,
            budget_state: BudgetState::default(),
            agent_budget: dudes_proto::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            floor_usd_per_day: None,
            billing_ceiling_missing: false,
            per_turn_floor: Spend::DEFAULT_PER_TURN_FLOOR,
            rung: BreakerRung::Healthy,
            used: MailTurnUse::default(),
        }
    }

    fn fired(decided: &MailTurnDecided) -> Gate {
        decided
            .gate_trace
            .iter()
            .find(|step| step.fired)
            .expect("one gate fires")
            .gate
    }

    #[test]
    fn an_idle_agent_with_room_is_issued_the_exact_task() {
        let value = input();
        let decided = admit_mail_turn(&value).unwrap();
        assert_eq!(
            decided.decision,
            MailTurnAdmission::Issue {
                task: MailTurnTask::new(&value.identity, 1).unwrap()
            }
        );
        assert_eq!(decided.gate_trace.len(), 7);
        assert_eq!(fired(&decided), Gate::Work);
    }

    #[test]
    fn gates_refuse_in_order_and_each_cap_is_a_budget_refusal() {
        let mut value = input();
        value.control.paused = true;
        value.used.thread = 2;
        let decided = admit_mail_turn(&value).unwrap();
        assert_eq!(
            decided.decision,
            MailTurnAdmission::Hold {
                reason: HoldReason::Paused
            }
        );
        value.control.paused = false;
        value.open_question = Some("which one?".into());
        value.question_lsn = Some(Lsn(4));
        let decided = admit_mail_turn(&value).unwrap();
        assert_eq!(
            decided.decision,
            MailTurnAdmission::Wait {
                question_lsn: Lsn(4)
            }
        );
        value.question_lsn = None;
        assert_eq!(admit_mail_turn(&value), None);
        value.open_question = None;
        for (used, note) in [
            (
                MailTurnUse {
                    thread: 2,
                    ..MailTurnUse::default()
                },
                "mail turn thread limit reached: 2 of 2",
            ),
            (
                MailTurnUse {
                    sender_today: 4,
                    ..MailTurnUse::default()
                },
                "mail turn sender day limit reached: 4 of 4",
            ),
            (
                MailTurnUse {
                    agent_today: 20,
                    ..MailTurnUse::default()
                },
                "mail turn and mail wake day limit reached: 20 of 20",
            ),
        ] {
            value.used = used;
            let decided = admit_mail_turn(&value).unwrap();
            assert_eq!(
                decided.decision,
                MailTurnAdmission::Exhausted {
                    reason: ExhaustedReason::MailWakeLimit
                }
            );
            assert_eq!(fired(&decided), Gate::Budget);
            assert_eq!(
                decided.gate_trace.last().unwrap().note.as_deref(),
                Some(note)
            );
        }
        value.used = MailTurnUse {
            thread: 1,
            sender_today: 3,
            agent_today: 19,
        };
        assert!(matches!(
            admit_mail_turn(&value).unwrap().decision,
            MailTurnAdmission::Issue { .. }
        ));
    }

    #[test]
    fn a_visible_billing_credential_without_a_ceiling_exhausts_the_turn() {
        let mut value = input();
        value.billing_ceiling_missing = true;
        assert_eq!(
            admit_mail_turn(&value).unwrap().decision,
            MailTurnAdmission::Exhausted {
                reason: ExhaustedReason::CeilingRequired
            }
        );
    }
}
