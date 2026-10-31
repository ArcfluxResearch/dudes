//! The mail a goal-free agent may be woken for, and the mail turns already issued.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::mail_turn::MAX_COVERED_MAIL;
use dudes_proto::{
    Entry, MailAct, MailFact, MailFrom, MailRef, MailTo, MailTurnAdmission, MailTurnDecision,
    Record, request_answer,
};
use dudes_types::{AgentId, Lsn, MailId, MailState, ThreadId, Timestamp, TurnId, UtcDay};

use super::World;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// Durable or notified: the recipient has not read it.
    Open,
    Acked,
    Bounced,
}

impl Standing {
    const fn of(state: &MailState) -> Self {
        match state {
            MailState::Durable | MailState::Notified { .. } => Self::Open,
            MailState::Acked => Self::Acked,
            MailState::Bounced { .. } => Self::Bounced,
        }
    }
}

#[derive(Clone, Debug)]
struct Row {
    lsn: Lsn,
    from: AgentId,
    to: AgentId,
    act: MailAct,
    standing: Standing,
}

#[derive(Clone, Debug)]
struct Thread {
    members: [AgentId; 2],
    mails: Vec<MailId>,
}

/// The newest accepted decision for an agent.
#[derive(Clone, Debug)]
pub struct Latest {
    /// Where the decision sits.
    pub lsn: Lsn,
    /// When it was journaled.
    pub at: Timestamp,
    /// What was decided.
    pub decision: MailTurnDecision,
}

/// One thread's wake-worthy mail for one agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The conversation.
    pub thread: ThreadId,
    /// The other member of the conversation.
    pub sender: AgentId,
    /// The wake-worthy mail not yet covered by a turn, oldest first.
    pub mail: Vec<MailRef>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Fold {
    mail: BTreeMap<MailId, Row>,
    threads: BTreeMap<ThreadId, Thread>,
    by_agent: BTreeMap<AgentId, BTreeSet<ThreadId>>,
    covered: BTreeSet<MailId>,
    latest: BTreeMap<AgentId, Latest>,
    by_thread: BTreeMap<(AgentId, ThreadId), Latest>,
    per_thread: BTreeMap<(AgentId, ThreadId), u32>,
    per_sender_day: BTreeMap<(AgentId, AgentId, UtcDay), u32>,
    per_day: BTreeMap<(AgentId, UtcDay), u32>,
    origins: BTreeSet<TurnId>,
    accepted_lsn: Option<Lsn>,
}

impl Fold {
    pub(super) fn accepted(&self, lsn: Lsn) -> bool {
        self.accepted_lsn == Some(lsn)
    }

    pub(super) fn origin(&self, turn: TurnId) -> bool {
        self.origins.contains(&turn)
    }

    pub(super) fn latest(&self, agent: AgentId) -> Option<&Latest> {
        self.latest.get(&agent)
    }

    pub(super) fn latest_for_thread(&self, agent: AgentId, thread: ThreadId) -> Option<&Latest> {
        self.by_thread.get(&(agent, thread))
    }

    pub(super) fn is_covered(&self, id: MailId) -> bool {
        self.covered.contains(&id)
    }

    pub(super) fn issues_for_thread(&self, agent: AgentId, thread: ThreadId) -> u32 {
        self.per_thread
            .get(&(agent, thread))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn issues_for_sender_day(
        &self,
        agent: AgentId,
        sender: AgentId,
        day: UtcDay,
    ) -> u32 {
        self.per_sender_day
            .get(&(agent, sender, day))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn issues_on_day(&self, agent: AgentId, day: UtcDay) -> u32 {
        self.per_day.get(&(agent, day)).copied().unwrap_or_default()
    }

    /// Every thread with wake-worthy mail for `agent`, the thread holding the oldest first.
    pub(super) fn candidates(&self, agent: AgentId) -> Vec<Candidate> {
        let mut found = Vec::new();
        for thread_id in self.by_agent.get(&agent).into_iter().flatten() {
            let thread = &self.threads[thread_id];
            let sender = if thread.members[0] == agent {
                thread.members[1]
            } else {
                thread.members[0]
            };
            let rows: Vec<&Row> = thread.mails.iter().map(|id| &self.mail[id]).collect();
            let facts: Vec<MailFact> = rows
                .iter()
                .map(|row| MailFact {
                    from: row.from,
                    to: row.to,
                    act: row.act,
                    bounced: row.standing == Standing::Bounced,
                })
                .collect();
            let mail: Vec<MailRef> = rows
                .iter()
                .enumerate()
                .filter(|(index, row)| {
                    row.to == agent
                        && row.standing == Standing::Open
                        && !self.covered.contains(&thread.mails[*index])
                        && (row.act == MailAct::Request
                            || (0..*index).any(|asked| {
                                rows[asked].from == agent
                                    && request_answer(&facts, asked) == Some(*index)
                            }))
                })
                .map(|(index, row)| MailRef {
                    id: thread.mails[index],
                    lsn: row.lsn,
                })
                .take(MAX_COVERED_MAIL)
                .collect();
            if !mail.is_empty() {
                found.push(Candidate {
                    thread: *thread_id,
                    sender,
                    mail,
                });
            }
        }
        found.sort_by_key(|candidate| candidate.mail[0].lsn);
        found
    }

    pub(super) fn apply(&mut self, world: &World, lsn: Lsn, entry: &Entry) {
        self.accepted_lsn = None;
        match &entry.record {
            Record::Mail {
                id,
                from: MailFrom::Agent { agent: from },
                to: MailTo::Agent { agent: to },
                act,
                thread: Some(thread),
                members,
                state,
                ..
            } if members.len() == 2
                && from != to
                && members.contains(from)
                && members.contains(to) =>
            {
                if self.mail.contains_key(id) {
                    return;
                }
                self.mail.insert(
                    *id,
                    Row {
                        lsn,
                        from: *from,
                        to: *to,
                        act: *act,
                        standing: Standing::of(state),
                    },
                );
                let row = self.threads.entry(*thread).or_insert_with(|| Thread {
                    members: [members[0], members[1]],
                    mails: Vec::new(),
                });
                row.mails.push(*id);
                for member in members {
                    self.by_agent.entry(*member).or_default().insert(*thread);
                }
            }
            Record::MailState { id, state } => {
                if let Some(row) = self.mail.get_mut(id) {
                    row.standing = Standing::of(state);
                }
            }
            Record::MailTurnDecided(decision) => {
                if self.decision_current(world, decision) {
                    let latest = Latest {
                        lsn,
                        at: entry.at,
                        decision: decision.clone(),
                    };
                    self.by_thread
                        .insert((decision.agent, decision.thread), latest.clone());
                    self.latest.insert(decision.agent, latest);
                    self.accepted_lsn = Some(lsn);
                }
            }
            _ => {
                if let Some(issue) = entry.record.mail_turn_issue() {
                    self.apply_issue(world, lsn, entry.at, issue);
                }
            }
        }
    }

    fn apply_issue(
        &mut self,
        world: &World,
        lsn: Lsn,
        at: dudes_types::Timestamp,
        issue: &dudes_proto::MailTurnIssue,
    ) {
        let current = self.issue_current(world, issue);
        if !current {
            return;
        }
        let Some(thread) = self.threads.get(&issue.thread) else {
            return;
        };
        let sender = if thread.members[0] == issue.agent {
            thread.members[1]
        } else {
            thread.members[0]
        };
        let day = at.utc_day();
        self.covered
            .extend(issue.covered.iter().map(|mail| mail.id));
        *self
            .per_thread
            .entry((issue.agent, issue.thread))
            .or_default() += 1;
        *self
            .per_sender_day
            .entry((issue.agent, sender, day))
            .or_default() += 1;
        *self.per_day.entry((issue.agent, day)).or_default() += 1;
        self.origins.insert(issue.turn);
        self.accepted_lsn = Some(lsn);
    }

    fn idle_goal_free(world: &World, agent: AgentId) -> bool {
        world.agents.get(&agent).is_some_and(|row| {
            row.goal.is_none() && !row.retired && !row.attached && row.open_turn.is_none()
        })
    }

    fn decision_current(&self, world: &World, decision: &MailTurnDecision) -> bool {
        if !world
            .agents
            .get(&decision.agent)
            .is_some_and(|row| row.goal.is_none() && !row.retired)
        {
            return false;
        }
        let candidates = self.candidates(decision.agent);
        candidates
            .iter()
            .find(|candidate| candidate.thread == decision.thread)
            .is_some_and(|candidate| {
                decision
                    .covered
                    .iter()
                    .all(|mail| candidate.mail.contains(mail))
            })
    }

    fn issue_current(&self, world: &World, issue: &dudes_proto::MailTurnIssue) -> bool {
        Self::idle_goal_free(world, issue.agent)
            && !world.turns.contains_key(&issue.turn)
            && issue
                .covered
                .iter()
                .all(|mail| !self.covered.contains(&mail.id))
            && self.latest.get(&issue.agent).is_some_and(|latest| {
                latest.lsn == issue.lsn_of_decision
                    && latest.decision.thread == issue.thread
                    && latest.decision.covered == issue.covered
                    && matches!(&latest.decision.decision,
                        MailTurnAdmission::Issue { task } if *task == issue.task)
            })
    }
}
