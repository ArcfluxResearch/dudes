use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::mail_wake::{MailRef, MailWakeMail};
use dudes_proto::{
    Entry, MAX_MESSAGES, MAX_MESSAGES_BYTES, MAX_THREADS, MailAct, MailFrom, MailTo, MessageAct,
    MessageState, MessageView, Record, ReplyStatus, ThreadView,
};
use dudes_types::{AgentId, Lsn, MailId, MailState, ThreadId, Timestamp};
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};

use super::{RowTable, get_row, put_row};
use crate::error::{Error, Result};

/// Internal mail metadata and delivery state. Browser readers receive only [`MessageView`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MailRow {
    /// The item, repeated so the canonical export exposes a key/row mismatch.
    id: MailId,
    from: MailFrom,
    at: Timestamp,
    lsn: Lsn,
    /// The recipient fixed on the original `Mail` record.
    to: MailTo,
    /// The highest-lsn delivery state folded for this item.
    state: MailState,
    #[serde(default)]
    state_lsn: Lsn,
    #[serde(default)]
    thread: Option<ThreadId>,
    #[serde(default)]
    members: Vec<AgentId>,
    act: MailAct,
    subject: Option<String>,
    thread_seq: u32,
}

impl MailRow {
    pub(super) fn wake_fact(&self) -> Option<(AgentId, MailWakeMail)> {
        let MailTo::Agent { agent } = self.to else {
            return None;
        };
        Some((
            agent,
            MailWakeMail::from_state(
                MailRef {
                    id: self.id,
                    lsn: self.lsn,
                },
                &self.state,
                if self.state_lsn == Lsn::ZERO {
                    self.lsn
                } else {
                    self.state_lsn
                },
            ),
        ))
    }
}

/// Apply one entry to the mail table. An unknown state change is corrupt ordering at the producer
/// edge, but projections follow the other row folds: warn and skip rather than inventing a row
/// whose recipient the state-only record cannot supply.
pub(super) fn apply(
    table: &mut RowTable<'_>,
    pending: &mut Table<'_, &'static [u8; 32], &'static [u8]>,
    messages: &mut Table<'_, &'static str, &'static [u8]>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    match &entry.record {
        Record::Mail {
            id,
            from,
            to,
            state,
            thread,
            members,
            act,
            subject,
            subject_admission,
            thread_seq,
            ..
        } => {
            if get_row::<MailRow>(table, &id.to_bytes())?.is_some() {
                return Ok(());
            }
            let row = MailRow {
                id: *id,
                from: *from,
                at: entry.at,
                lsn,
                to: *to,
                state: state.clone(),
                state_lsn: lsn,
                thread: *thread,
                members: members.clone(),
                act: *act,
                subject: (*subject_admission == Some(1))
                    .then(|| dudes_proto::messages::mail_subject_caption(subject)),
                thread_seq: *thread_seq,
            };
            put_row(table, &id.to_bytes(), &row)?;
            index(pending, &row)?;
            update_messages(messages, &row)
        }
        Record::MailState { id, state } => {
            let key = id.to_bytes();
            let Some(mut row) = get_row::<MailRow>(table, &key)? else {
                tracing::warn!(
                    target: "journal",
                    lsn = %lsn,
                    mail = %id,
                    kind = ?entry.record.kind(),
                    "projection.unknown_mail"
                );
                return Ok(());
            };
            row.state = state.clone();
            row.state_lsn = lsn;
            put_row(table, &key, &row)?;
            index(pending, &row)?;
            update_messages(messages, &row)
        }
        _ => Ok(()),
    }
}

pub(super) fn recipient_key(agent: AgentId, mail: [u8; 16]) -> [u8; 32] {
    let mut key = [0; 32];
    key[..16].copy_from_slice(&agent.to_bytes());
    key[16..].copy_from_slice(&mail);
    key
}

fn index(table: &mut Table<'_, &'static [u8; 32], &'static [u8]>, row: &MailRow) -> Result<()> {
    if let Some((agent, mail)) = row.wake_fact() {
        let key = recipient_key(agent, row.id.to_bytes());
        if mail.count.is_some() {
            table.insert(&key, b"null".as_slice())?;
        } else {
            table.remove(&key)?;
        }
    }
    Ok(())
}

/// One canonical cut and its complete bounded mail window and thread graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailView {
    /// The last applied journal position read with both tables.
    pub lsn: Lsn,
    /// The final bounded window in original creation order.
    pub messages: Vec<MessageView>,
    /// Fixed membership for exactly the threads referenced by the window.
    pub threads: Vec<ThreadView>,
}

pub(super) fn view(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    window: &impl ReadableTable<&'static str, &'static [u8]>,
    retired: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    lsn: Lsn,
) -> Result<MailView> {
    let mut facts = messages(window)?;
    let mut ids = BTreeSet::new();
    let mut previous = Lsn::ZERO;
    if facts.len() > MAX_MESSAGES {
        return Err(Error::MailViewCorrupt { mail: None });
    }
    let mut retired_agents = BTreeSet::new();
    for fact in &facts {
        let corrupt = || Error::MailViewCorrupt {
            mail: Some(fact.id),
        };
        if fact.lsn <= previous
            || fact.lsn > lsn
            || !ids.insert(fact.id)
            || get_row::<MailRow>(table, &fact.id.to_bytes())?.as_ref() != Some(fact)
        {
            return Err(corrupt());
        }
        previous = fact.lsn;
        let (MailFrom::Agent { agent: from }, MailTo::Agent { agent: to }) = (fact.from, fact.to)
        else {
            return Err(corrupt());
        };
        if retired.get(&to.to_bytes())?.is_some() {
            retired_agents.insert(to);
        }
        if fact.thread.is_some() {
            if !(2..=8).contains(&fact.members.len())
                || fact.members.windows(2).any(|pair| pair[0] >= pair[1])
                || from == to
                || !fact.members.contains(&from)
                || !fact.members.contains(&to)
            {
                return Err(corrupt());
            }
        } else if !fact.members.is_empty() {
            return Err(corrupt());
        }
    }
    loop {
        let messages = derive_messages(&facts, &retired_agents);
        let threads = thread_graph(&facts)?;
        if threads.len() <= MAX_THREADS
            && encoded_len(&messages)? + encoded_len(&threads)? <= MAX_MESSAGES_BYTES
        {
            return Ok(MailView {
                lsn,
                messages,
                threads,
            });
        }
        facts.remove(0);
    }
}

fn thread_graph(facts: &[MailRow]) -> Result<Vec<ThreadView>> {
    let mut threads = BTreeMap::new();
    for fact in facts {
        if let Some(id) = fact.thread
            && threads
                .insert(id, fact.members.clone())
                .is_some_and(|members| members != fact.members)
        {
            return Err(Error::MailViewCorrupt {
                mail: Some(fact.id),
            });
        }
    }
    Ok(threads
        .into_iter()
        .map(|(id, members)| ThreadView { id, members })
        .collect())
}

fn derive_messages(facts: &[MailRow], retired: &BTreeSet<AgentId>) -> Vec<MessageView> {
    facts
        .iter()
        .filter_map(|fact| {
            let (MailFrom::Agent { agent: from }, MailTo::Agent { agent: to }) =
                (fact.from, fact.to)
            else {
                return None;
            };
            let reply_status = if fact.act != MailAct::Request {
                ReplyStatus::NotExpected
            } else if answered(facts, fact) {
                ReplyStatus::Answered
            } else if matches!(fact.state, MailState::Bounced { .. }) || retired.contains(&to) {
                ReplyStatus::Undeliverable
            } else if fact.thread.is_some() && fact.thread_seq == 12 {
                ReplyStatus::ThreadFull
            } else {
                ReplyStatus::Awaiting
            };
            Some(MessageView {
                id: fact.id,
                from,
                to,
                at: fact.at,
                lsn: fact.lsn.0.to_string(),
                state: message_state(&fact.state),
                thread: fact.thread,
                subject: fact.subject.clone(),
                act: match fact.act {
                    MailAct::Message => Some(MessageAct::Message),
                    MailAct::Request => Some(MessageAct::Request),
                    _ => None,
                },
                reply_status,
            })
        })
        .collect()
}

fn answered(facts: &[MailRow], request: &MailRow) -> bool {
    let Some(thread) = request.thread else {
        return false;
    };
    let mut ids = Vec::new();
    let mut mail = Vec::new();
    for fact in facts.iter().filter(|fact| fact.thread == Some(thread)) {
        if let (MailFrom::Agent { agent: from }, MailTo::Agent { agent: to }) = (fact.from, fact.to)
        {
            ids.push(fact.id);
            mail.push(dudes_proto::MailFact {
                from,
                to,
                act: fact.act,
                bounced: matches!(fact.state, MailState::Bounced { .. }),
            });
        }
    }
    ids.iter()
        .position(|id| *id == request.id)
        .is_some_and(|at| dudes_proto::request_answer(&mail, at).is_some())
}

fn encoded_len(value: &impl Serialize) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|source| Error::Row { source })
}

fn message_state(state: &MailState) -> MessageState {
    match state {
        MailState::Durable => MessageState::Durable,
        MailState::Notified { .. } => MessageState::Notified,
        MailState::Acked => MessageState::Acked,
        MailState::Bounced { .. } => MessageState::Bounced,
    }
}

const WINDOW: &str = "window";

fn messages(table: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Vec<MailRow>> {
    table
        .get(WINDOW)?
        .map(|value| serde_json::from_slice(value.value()).map_err(|source| Error::Row { source }))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn update_messages(
    table: &mut Table<'_, &'static str, &'static [u8]>,
    row: &MailRow,
) -> Result<()> {
    if !matches!(
        (row.from, row.to),
        (MailFrom::Agent { .. }, MailTo::Agent { .. })
    ) {
        return Ok(());
    }
    let mut window = messages(table)?;
    window.retain(|fact| fact.id != row.id);
    let at = window
        .binary_search_by_key(&row.lsn, |fact| fact.lsn)
        .unwrap_or_else(|at| at);
    window.insert(at, row.clone());
    if window.len() > MAX_MESSAGES {
        window.remove(0);
    }
    let bytes = serde_json::to_vec(&window).map_err(|source| Error::Row { source })?;
    table.insert(WINDOW, bytes.as_slice())?;
    Ok(())
}

/// Count agent-addressed mail whose latest state remains unread.
pub(super) fn unread_by_agent(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    agent: AgentId,
) -> Result<u32> {
    let mut unread = 0u64;
    for entry in table.iter()? {
        let (_, value) = entry?;
        let row: MailRow =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        if row.to == (MailTo::Agent { agent })
            && matches!(row.state, MailState::Durable | MailState::Notified { .. })
        {
            // A redb table has a `u64` row count and this iterator yields each key once, so the
            // accumulator cannot overflow before the table itself does.
            unread += 1;
        }
    }
    bounded_unread_count(agent, unread)
}

/// Narrow an exact table count to the public view without disguising range as row corruption.
fn bounded_unread_count(agent: AgentId, count: u64) -> Result<u32> {
    u32::try_from(count).map_err(|_| Error::UnreadCountRange { agent, count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_count_range_is_not_row_corruption() {
        let agent = AgentId::from_parts(1, 1);
        assert_eq!(
            bounded_unread_count(agent, u64::from(u32::MAX)).unwrap(),
            u32::MAX
        );
        let error = bounded_unread_count(agent, u64::from(u32::MAX) + 1).unwrap_err();
        assert!(
            matches!(
                error,
                Error::UnreadCountRange {
                    agent: found,
                    count,
                } if found == agent && count == u64::from(u32::MAX) + 1
            ),
            "the first unrepresentable exact count has its own diagnosis: {error:?}"
        );
    }
}
