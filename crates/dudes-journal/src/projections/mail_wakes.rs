use dudes_proto::mail_wake::{MailWakeContext, MailWakeState};
use dudes_proto::{Entry, MailTo, Record};
use dudes_types::{AgentId, GoalStatus, Lsn, Mode, TurnId, UtcDay};
use redb::{ReadableTable, Table};

use super::mail::MailRow;
use super::{AgentRow, GoalRow, RowTable, TurnRow, get_row, put_row};
use crate::error::{Error, Result};

pub(super) struct Tables<'txn> {
    pub states: RowTable<'txn>,
    pub by_day: Table<'txn, &'static [u8; 20], &'static [u8]>,
}

pub(super) struct Facts<'a, 'txn> {
    pub agents: &'a RowTable<'txn>,
    pub goals: &'a RowTable<'txn>,
    pub turns: &'a RowTable<'txn>,
    pub mail: &'a RowTable<'txn>,
    pub pending: &'a Table<'txn, &'static [u8; 32], &'static [u8]>,
    pub completions: &'a RowTable<'txn>,
}

impl Facts<'_, '_> {
    fn context(&self, agent: AgentId, record: &Record) -> Result<MailWakeContext> {
        let owner = get_row::<AgentRow>(self.agents, &agent.to_bytes())?;
        let completion = if let Some(goal) = owner.as_ref().and_then(|row| row.goal) {
            let held = get_row::<GoalRow>(self.goals, &goal.to_bytes())?;
            if held.is_some_and(|row| row.agent == agent && row.status == GoalStatus::Achieved) {
                get_row::<Lsn>(self.completions, &goal.to_bytes())?.map(|lsn| (goal, lsn))
            } else {
                None
            }
        } else {
            None
        };
        let mut mails = Vec::new();
        if matches!(record, Record::MailWakeTurnDecided(decision) if matches!(decision.purpose, dudes_proto::MailWakePurpose::FreshPreparation { .. }))
        {
            let start = super::mail::recipient_key(agent, [0; 16]);
            let end = super::mail::recipient_key(agent, [u8::MAX; 16]);
            for entry in self.pending.range::<&[u8; 32]>(&start..=&end)? {
                let (key, _) = entry?;
                let mut id = [0; 16];
                id.copy_from_slice(&key.value()[16..]);
                if let Some(row) = get_row::<MailRow>(self.mail, &id)?
                    && let Some((recipient, mail)) = row.wake_fact()
                    && recipient == agent
                    && mail.count.is_some()
                {
                    mails.push(mail);
                }
            }
        } else if let Record::MailState { id, .. } = record
            && let Some(row) = get_row::<MailRow>(self.mail, &id.to_bytes())?
            && let Some((recipient, mail)) = row.wake_fact()
            && recipient == agent
        {
            mails.push(mail);
        }
        let proposed_turn = if let Record::MailWakeOnlyTurnIssued(issue) = record {
            Some(issue.turn)
        } else {
            None
        };
        let receipt_exists = if let Record::Mail { id, .. } = record {
            get_row::<MailRow>(self.mail, &id.to_bytes())?.is_some()
        } else {
            false
        };
        Ok(MailWakeContext {
            completion,
            retired: owner.is_none(),
            attached: owner.as_ref().is_some_and(|row| row.mode == Mode::Attached),
            open_turn: owner.as_ref().and_then(|row| row.open_turn),
            turn_exists: self.turn_exists(proposed_turn)?,
            receipt_exists,
            question_lsn: owner.and_then(|row| row.question_lsn),
            mails,
        })
    }

    fn turn_exists(&self, turn: Option<TurnId>) -> Result<bool> {
        turn.map(|turn| get_row::<TurnRow>(self.turns, &turn.to_bytes()).map(|row| row.is_some()))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn affected(&self, record: &Record) -> Result<Vec<AgentId>> {
        if let Some((agent, _)) = record.goal_install() {
            return Ok(vec![agent]);
        }
        Ok(match record {
            Record::Mail {
                to: MailTo::Agent { agent },
                ..
            }
            | Record::GoalCleared { agent, .. }
            | Record::BreakerBeat { agent, .. } => vec![*agent],
            Record::AgentRetired { id, .. } => vec![*id],
            Record::GoalReassigned { from, to, .. } => {
                if from == to {
                    vec![*from]
                } else {
                    vec![*from, *to]
                }
            }
            Record::MailWakeTurnDecided(decision) => vec![decision.agent],
            Record::MailWakeOnlyTurnIssued(issue) => vec![issue.agent],
            Record::MailState { id, .. } => get_row::<MailRow>(self.mail, &id.to_bytes())?
                .and_then(|row| row.wake_fact().map(|value| value.0))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        })
    }
}

pub(super) fn apply(
    tables: &mut Tables<'_>,
    facts: &Facts<'_, '_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    for agent in facts.affected(&entry.record)? {
        let context = facts.context(agent, &entry.record)?;
        let previous = get_row::<MailWakeState>(&tables.states, &agent.to_bytes())?;
        let had_row = previous.is_some();
        let row = previous.unwrap_or_default();
        let key = day_key(agent, entry.at.utc_day());
        let count = tables
            .by_day
            .get(&key)?
            .map(|value| {
                serde_json::from_slice::<u8>(value.value()).map_err(|source| Error::Row { source })
            })
            .transpose()?
            .unwrap_or_default();
        if count > 20 {
            return Err(Error::MailWake {
                lsn,
                reason: dudes_proto::mail_wake::MailWakeError::Limit,
            });
        }
        let applied = row
            .apply(agent, &context, count, lsn, entry)
            .map_err(|reason| Error::MailWake { lsn, reason })?;
        let empty = MailWakeState {
            last_lsn: applied.state.last_lsn,
            ..MailWakeState::default()
        };
        let retain_row = had_row || applied.state != empty;
        if retain_row {
            put_row(&mut tables.states, &agent.to_bytes(), &applied.state)?;
        }
        if applied.issued {
            let bytes = serde_json::to_vec(&(count + 1)).map_err(|source| Error::Row { source })?;
            tables.by_day.insert(&key, bytes.as_slice())?;
        }
    }
    Ok(())
}

fn day_key(agent: AgentId, day: UtcDay) -> [u8; 20] {
    let mut key = [0u8; 20];
    key[..16].copy_from_slice(&agent.to_bytes());
    key[16..]
        .copy_from_slice(&(day.days_since_epoch().cast_unsigned() ^ 0x8000_0000).to_be_bytes());
    key
}

pub(super) fn normal_carrier(entry: &Entry) -> Option<Entry> {
    let Record::MailWakeOnlyTurnIssued(issue) = &entry.record else {
        return None;
    };
    Some(Entry {
        at: entry.at,
        record: Record::TurnIssued {
            agent: issue.agent,
            turn: issue.turn,
            goal: issue.goal,
            task: issue.task.as_str().to_owned(),
            session: issue.session.clone(),
            mode: issue.mode,
            model_selection: Some(issue.model_selection.clone()),
            lsn_of_decision: issue.lsn_of_decision,
            answers: Vec::new(),
        },
    })
}

#[cfg(test)]
mod tests {
    use dudes_proto::record::Hex32;
    use dudes_proto::{
        ExhaustedReason, Gate, GateStep, MailRef, MailWakeAdmission, MailWakeDecision,
        MailWakeDigest, MailWakeDigestDomain, MailWakeIssue, MailWakePlan, MailWakePurpose,
        MailWakeTarget, MailWakeTask,
    };
    use dudes_types::{GoalId, MailId, ModelSelection, SessionId, Timestamp};

    use super::*;

    fn maximal_fresh_decision(
        agent: AgentId,
        goal: GoalId,
        task: &MailWakeTask,
        gate_trace: Vec<GateStep>,
    ) -> MailWakeDecision {
        let targets: Vec<_> = (0..32_u64)
            .map(|index| MailWakeTarget {
                mail: MailRef {
                    id: MailId::from_parts(1, u128::from(index)),
                    lsn: Lsn(u64::MAX - 100 + index),
                },
                original_count: 4,
                target_count: 5,
            })
            .collect();
        MailWakeDecision {
            agent,
            goal,
            completion_lsn: Lsn(u64::MAX - 200),
            purpose: MailWakePurpose::FreshPreparation {
                plan: MailWakePlan {
                    trigger: targets[0].mail,
                    frontier: Lsn(u64::MAX - 1),
                    unread_count: 32,
                    task: task.clone(),
                    targets,
                },
            },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Fresh,
                value: Hex32::new([255; 32]),
            },
            gate_trace,
            decision: MailWakeAdmission::Issue { task: task.clone() },
        }
    }

    #[test]
    fn maximal_escaped_wake_envelopes_fit_journal_and_exact_rpc_notification() {
        let agent = AgentId::from_parts(1, 1);
        let goal = GoalId::from_parts(1, 2);
        let task = MailWakeTask::new(32).unwrap();
        let trace = |through: Gate| {
            Gate::ORDER
                .into_iter()
                .take_while(|gate| *gate <= through)
                .map(|gate| GateStep {
                    gate,
                    fired: gate == through,
                    note: Some("\u{0001}".repeat(512)),
                })
                .collect()
        };
        let root = maximal_fresh_decision(agent, goal, &task, trace(Gate::Work));
        assert!(root.valid());
        let mut resume = root.clone();
        resume.purpose = MailWakePurpose::ExistingPreparation {
            root_lsn: Lsn(u64::MAX - 150),
        };
        resume.input_digest.domain = MailWakeDigestDomain::Resume;
        assert!(resume.valid());
        let mut refusal = resume.clone();
        refusal.gate_trace = trace(Gate::Budget);
        refusal.decision = MailWakeAdmission::Exhausted {
            reason: ExhaustedReason::MailWakeLimit,
        };
        assert!(refusal.valid());
        let mut candidate_refusal = refusal.clone();
        candidate_refusal.input_digest.domain = MailWakeDigestDomain::Fresh;
        candidate_refusal.purpose = MailWakePurpose::UnpreparedCandidate {
            trigger: MailRef {
                id: MailId::from_parts(1, 1),
                lsn: Lsn(u64::MAX - 100),
            },
            frontier: Lsn(u64::MAX - 1),
        };
        assert!(candidate_refusal.valid());
        let issue = MailWakeIssue {
            agent,
            goal,
            turn: TurnId::from_parts(1, 3),
            completion_lsn: Lsn(u64::MAX - 200),
            root_lsn: Lsn(u64::MAX - 150),
            lsn_of_decision: Lsn(u64::MAX - 1),
            task,
            session: Some(SessionId::new("\u{0001}".repeat(65_536))),
            mode: Mode::Managed,
            model_selection: ModelSelection::named("\u{0001}".repeat(65_536)),
            answers: Vec::new(),
        };
        assert!(issue.valid());
        let entries = [
            Record::MailWakeTurnDecided(root),
            Record::MailWakeTurnDecided(resume),
            Record::MailWakeTurnDecided(refusal),
            Record::MailWakeOnlyTurnIssued(issue),
            Record::MailWakeTurnDecided(candidate_refusal),
        ]
        .map(|record| Entry {
            at: Timestamp::from_unix_millis(i64::MIN),
            record,
        });
        let mut lengths = Vec::new();
        for entry in &entries {
            let payload = serde_json::to_vec(entry).unwrap();
            let notification = serde_json::to_vec(&dudes_proto::rpc::journal::journal_appended(
                Lsn(u64::MAX),
                entry,
            ))
            .unwrap();
            assert!(payload.len() <= crate::frame::MAX_PAYLOAD);
            assert!(notification.len() < dudes_proto::rpc::MAX_FRAME_BYTES);
            let mut framed = Vec::new();
            crate::frame::encode_required(entry.record.kind(), &payload, &mut framed).unwrap();
            assert_eq!(crate::frame::decode(&framed).unwrap().payload, payload);
            assert_eq!(serde_json::from_slice::<Entry>(&payload).unwrap(), *entry);
            lengths.push((payload.len(), notification.len() + 1));
        }
        assert_eq!(
            lengths,
            [
                (26677, 26770),
                (22651, 22744),
                (15996, 16089),
                (787_249, 787_342),
                (16068, 16161)
            ]
        );
    }
}
