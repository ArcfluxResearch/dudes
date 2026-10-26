use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::{Entry, MailFrom, MailTo, Record};
use dudes_types::{AgentId, MailId, ThreadId, Timestamp, UtcDay};
use serde::Deserialize;

use super::Row;

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum Address {
    Agent { agent: AgentId },
    Agents { agents: Vec<AgentId> },
    Department { department: String },
    Everyone { everyone: bool },
}

#[derive(Clone, Copy)]
pub(super) enum Route<'a> {
    To(&'a Address),
    Reply(MailId),
}

impl<'a> Route<'a> {
    pub(super) fn parse(
        address: Option<&'a Address>,
        reply: Option<MailId>,
    ) -> Result<Self, &'static str> {
        match (address, reply) {
            (Some(address), None) => Ok(Self::To(address)),
            (None, Some(parent)) => Ok(Self::Reply(parent)),
            _ => Err("exactly one route is required"),
        }
    }
}

#[derive(Clone)]
pub(super) struct SendPlan {
    pub thread: ThreadId,
    pub sequence: u32,
    pub members: Vec<AgentId>,
    pub recipients: Vec<AgentId>,
    pub receipt: Option<Row>,
}

struct Thread {
    members: Vec<AgentId>,
    sequence: u32,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum LogicalSend {
    Legacy(MailId),
    Thread(ThreadId, u32),
}

#[derive(Default)]
pub(super) struct MailIndex {
    threads: BTreeMap<ThreadId, Thread>,
    departments: BTreeMap<AgentId, String>,
    live: BTreeSet<AgentId>,
    recipients: BTreeSet<AgentId>,
    logical: BTreeSet<(AgentId, LogicalSend)>,
    daily: BTreeMap<(AgentId, UtcDay), u32>,
}

impl MailIndex {
    pub(super) fn apply(&mut self, entry: &Entry) {
        match &entry.record {
            Record::AgentHired { id, department, .. } => {
                self.departments.insert(*id, department.clone());
                self.live.insert(*id);
                self.recipients.insert(*id);
            }
            Record::AgentRetired { id, .. } => {
                self.live.remove(id);
            }
            Record::Mail {
                id,
                from: MailFrom::Agent { agent },
                thread,
                thread_seq,
                members,
                to,
                ..
            } => {
                if let MailTo::Agent { agent } = to {
                    self.recipients.insert(*agent);
                }
                let first = if let Some(thread) = thread {
                    self.threads
                        .entry(*thread)
                        .and_modify(|row| {
                            row.sequence = row.sequence.max(*thread_seq);
                        })
                        .or_insert_with(|| Thread {
                            members: members.clone(),
                            sequence: *thread_seq,
                        });
                    self.logical
                        .insert((*agent, LogicalSend::Thread(*thread, *thread_seq)))
                } else {
                    self.logical.insert((*agent, LogicalSend::Legacy(*id)))
                };
                if first {
                    *self.daily.entry((*agent, entry.at.utc_day())).or_default() += 1;
                }
            }
            Record::Mail {
                to: MailTo::Agent { agent },
                ..
            } => {
                self.recipients.insert(*agent);
            }
            _ => {}
        }
    }

    fn resolve(&self, sender: AgentId, address: &Address) -> Result<Vec<AgentId>, &'static str> {
        let mut recipients: BTreeSet<_> = match address {
            Address::Agent { agent } => BTreeSet::from([*agent]),
            Address::Agents { agents } => {
                let supplied: BTreeSet<_> = agents.iter().copied().collect();
                if supplied.is_empty() || supplied.len() > 7 {
                    return Err("explicit recipient limit");
                }
                supplied
            }
            Address::Department { department } => {
                let wanted = department.trim_matches(|c: char| c.is_ascii_whitespace());
                self.live
                    .iter()
                    .filter(|id| {
                        self.departments.get(id).is_some_and(|name| {
                            name.trim_matches(|c: char| c.is_ascii_whitespace()) == wanted
                        })
                    })
                    .copied()
                    .collect()
            }
            Address::Everyone { everyone: true } => self.live.clone(),
            Address::Everyone { everyone: false } => return Err("invalid everyone address"),
        };
        recipients.remove(&sender);
        if recipients.is_empty() || recipients.len() > 7 {
            return Err("thread membership limit");
        }
        Ok(recipients.into_iter().collect())
    }

    pub(super) fn admit_day(&self, sender: AgentId, now: Timestamp) -> Result<(), &'static str> {
        if self
            .daily
            .get(&(sender, now.utc_day()))
            .copied()
            .unwrap_or(0)
            >= 32
        {
            return Err("daily sender limit");
        }
        Ok(())
    }

    pub(super) fn plan(
        &self,
        sender: AgentId,
        outbox: MailId,
        route: Route<'_>,
        rows: &BTreeMap<MailId, Row>,
        mint: &mut impl FnMut() -> ThreadId,
    ) -> Result<SendPlan, &'static str> {
        let receipt = self
            .recipients
            .iter()
            .find_map(|recipient| rows.get(&dudes_journal::mail_recipient_id(outbox, *recipient)));
        if let Some(row) = receipt {
            if row.item.from != (MailFrom::Agent { agent: sender }) {
                return Err("receipt collision");
            }
            let Some(thread) = row.item.thread else {
                return Err("legacy receipt collision");
            };
            let members = row.item.members.clone();
            let recipients: Vec<_> = members.iter().copied().filter(|id| *id != sender).collect();
            match route {
                Route::To(address @ (Address::Agent { .. } | Address::Agents { .. })) => {
                    if self.resolve(sender, address)? != recipients {
                        return Err("routing changed");
                    }
                }
                Route::Reply(parent)
                    if rows.get(&parent).and_then(|row| row.item.thread) != Some(thread) =>
                {
                    return Err("reply routing changed");
                }
                Route::Reply(_) | Route::To(_) => {}
            }
            return Ok(SendPlan {
                thread,
                sequence: row.item.thread_seq,
                members,
                recipients,
                receipt: Some(row.clone()),
            });
        }
        let recipients = match route {
            Route::Reply(parent) => {
                let Some(thread) = rows.get(&parent).and_then(|row| row.item.thread) else {
                    return Err("reply not found");
                };
                let row = &self.threads[&thread];
                if !row.members.contains(&sender) {
                    return Err("reply not found");
                }
                let sequence = row.sequence.saturating_add(1);
                if sequence > 12 {
                    return Err("thread message limit");
                }
                return Ok(SendPlan {
                    thread,
                    sequence,
                    members: row.members.clone(),
                    recipients: row
                        .members
                        .iter()
                        .copied()
                        .filter(|id| *id != sender)
                        .collect(),
                    receipt: None,
                });
            }
            Route::To(address) => self.resolve(sender, address)?,
        };
        let mut members = recipients.clone();
        members.push(sender);
        members.sort_unstable();
        let thread = mint();
        if self.threads.contains_key(&thread) {
            return Err("thread id collision");
        }
        Ok(SendPlan {
            thread,
            sequence: 1,
            members,
            recipients,
            receipt: None,
        })
    }
}
