use super::*;
use crate::record::{Hex32, samples};
use crate::recovery::RecoveryWork;
use crate::{Entry, Goal, Record};
use dudes_types::{ModelSelection, Timestamp};

fn hash(bytes: &[u8]) -> Hex32 {
    let mut digest = [0u8; 32];
    for (index, byte) in bytes.iter().enumerate() {
        digest[index % 32] ^= byte;
    }
    Hex32::new(digest)
}

struct Fixture {
    history: RecoveryHistory,
    goal: Goal,
    binding: RecoveryBinding,
    issue: RecoveryIssue,
}

impl Fixture {
    fn new() -> Self {
        let agent = AgentId::from_parts(41, 1);
        let work = RecoveryWork {
            agent,
            goal: GoalId::from_parts(41, 2),
            turn: TurnId::from_parts(41, 3),
        };
        let mut hired = samples::record(RecordKind::AgentHired);
        let Record::AgentHired {
            id,
            provider,
            worktree,
            ..
        } = &mut hired
        else {
            unreachable!()
        };
        *id = agent;
        *provider = Provider::Codex;
        *worktree = None;
        let mut installed = samples::record(RecordKind::GoalSet);
        let Record::GoalSet {
            agent: owner, goal, ..
        } = &mut installed
        else {
            unreachable!()
        };
        *owner = agent;
        goal.id = work.goal;
        let goal = goal.clone();
        let issued = Record::TurnIssued {
            agent,
            turn: work.turn,
            goal: work.goal,
            mode: Mode::Managed,
            lsn_of_decision: Lsn(25),
            task: "original immutable task".into(),
            session: None,
            model_selection: Some(ModelSelection::provider_default()),
            answers: Vec::new(),
        };
        let mut history = RecoveryHistory::default();
        for (at, record) in [(10, hired), (20, installed), (30, issued)] {
            history
                .observe(
                    Lsn(at),
                    &Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record,
                    },
                    hash,
                )
                .unwrap();
        }
        let issue = history.at(Lsn(30)).unwrap().issue().unwrap();
        Self {
            history,
            goal,
            binding: RecoveryBinding {
                work,
                issue_lsn: Lsn(30),
                hired_lsn: Lsn(10),
                ticket: None,
                worktree: None,
                provider: Provider::Codex,
                model_selection: Some(ModelSelection::provider_default()),
            },
            issue,
        }
    }

    fn current(&self) -> RecoveryCurrent<'_> {
        RecoveryCurrent {
            through: Lsn(100),
            agent: RecoveryAgentCurrent {
                agent: self.binding.work.agent,
                hired_lsn: Lsn(10),
                provider: Provider::Codex,
                worktree: None,
                mode: Mode::Managed,
                retired: false,
                control: RecoveryStandingControl::default(),
                goal: Some(self.binding.work.goal),
                control_lsn: None,
                session: None,
                session_lsn: None,
            },
            goal: RecoveryGoalCurrent {
                goal: self.binding.work.goal,
                agent: self.binding.work.agent,
                installed_lsn: Lsn(20),
                last_turn: Some(self.binding.work.turn),
                cleared: false,
                done: false,
                predicate: &self.goal.predicate,
                result_lsn: None,
                card_change_lsn: None,
                retired_lsn: None,
            },
            issue: self.issue.clone(),
            mode: Mode::Managed,
            attempt: crate::recovery_state::RecoveryAttempt::default(),
            ticket: None,
            lease: None,
        }
    }
}

#[test]
fn actual_sources_bind_the_original_issue_without_dispatch_authority() {
    let fixture = Fixture::new();
    let facts = fixture
        .current()
        .bind(&fixture.binding, &fixture.history)
        .unwrap();
    assert_eq!(facts.through, Lsn(100));
    assert_eq!(facts.binding, fixture.binding);
    assert_eq!(facts.source_issue, fixture.issue);
    assert_eq!(facts.control_lsn, None);
    assert_eq!(facts.confirmed_session, None);
}

#[test]
fn changed_current_values_or_required_sources_refuse_without_defaults() {
    let fixture = Fixture::new();
    for change in 0..10 {
        let mut current = fixture.current();
        let expected = match change {
            0 => {
                current.agent.hired_lsn = Lsn(11);
                RecoveryStateError::Binding
            }
            1 => {
                current.goal.installed_lsn = Lsn(21);
                RecoveryStateError::Binding
            }
            2 => {
                current.issue.issued.task_digest = Hex32::new([0; 32]);
                RecoveryStateError::Binding
            }
            3 => {
                current.issue.issued.answers.push(Lsn(15));
                RecoveryStateError::Binding
            }
            4 => {
                current.agent.control_lsn = Some(Lsn(20));
                RecoveryStateError::Control
            }
            5 => {
                current.agent.session = Some(SessionId::new("unproven"));
                RecoveryStateError::Context
            }
            6 => {
                current.agent.retired = true;
                RecoveryStateError::Binding
            }
            7 => {
                current.goal.done = true;
                RecoveryStateError::Binding
            }
            8 => {
                current.mode = Mode::Attached;
                RecoveryStateError::Binding
            }
            9 => {
                current.through = Lsn(29);
                RecoveryStateError::Position
            }
            _ => unreachable!(),
        };
        assert_eq!(
            current.bind(&fixture.binding, &fixture.history).err(),
            Some(expected),
            "case {change}"
        );
    }
    for missing in [Lsn(10), Lsn(20), Lsn(30)] {
        let mut history = fixture.history.clone();
        history.witnesses.remove(&missing);
        assert_eq!(
            fixture.current().bind(&fixture.binding, &history).err(),
            Some(RecoveryStateError::Binding)
        );
    }
}
