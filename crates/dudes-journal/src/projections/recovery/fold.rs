//! Strict recovery transitions prepared against the transaction's preceding rows.

use dudes_proto::recovery::RecoveryWork;
use dudes_proto::recovery_state::cache::{RecoveryCache, RecoveryCacheUpdate};
use dudes_proto::recovery_state::history::{RecoveryHistory, RecoveryWitnessContext};
use dudes_proto::recovery_state::transition::RecoveryTransition;
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryStateError};
use dudes_proto::{Entry, Record};
use dudes_types::{Lsn, Mode};
use redb::ReadableTable;

use super::{CacheReader, FactTables, attempt_at, facts_at};
use crate::error::{Error, Result};

pub(in crate::projections) fn prepare_record<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    lsn: Lsn,
    entry: &Entry,
    context: RecoveryWitnessContext,
) -> Result<Vec<RecoveryCacheUpdate>>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let result = (|| match &entry.record {
        Record::RecoveryRequired(record) => {
            apply(tables, through, lsn, RecoveryTransition::Required(record))
        }
        Record::RecoveryAuthorized(record) => {
            apply(tables, through, lsn, RecoveryTransition::Authorized(record))
        }
        Record::RecoveryInvalidated(record) => apply(
            tables,
            through,
            lsn,
            RecoveryTransition::Invalidated(record),
        ),
        Record::TurnIssued { turn, .. }
        | Record::TurnIssuedWithBrowserNote { turn, .. }
        | Record::ConversationTurnIssued { turn, .. }
        | Record::NoteOnlyTurnIssued { turn, .. }
        | Record::TaughtNoteOnlyTurnIssued { turn, .. }
        | Record::MailWakeOnlyTurnIssued(dudes_proto::MailWakeIssue { turn, .. }) => {
            let cache = RecoveryCache::load_planned(&CacheReader(tables.recovery), through, *turn)?;
            if let Some(claim) = cache.state().planned_claim(*turn) {
                if !matches!(
                    &entry.record,
                    Record::TurnIssued {
                        mode: Mode::Managed,
                        ..
                    } | Record::TurnIssuedWithBrowserNote {
                        mode: Mode::Managed,
                        ..
                    }
                ) {
                    return Err(RecoveryStateError::Binding.into());
                }
                let history = source(lsn, entry, context)?;
                let issue = history
                    .at(lsn)
                    .map_err(|_| RecoveryStateError::Binding)?
                    .issue()
                    .ok_or(RecoveryStateError::Binding)?;
                apply(
                    tables,
                    through,
                    lsn,
                    RecoveryTransition::Issued {
                        claim,
                        issue: &issue,
                    },
                )
            } else {
                Ok(Vec::new())
            }
        }
        Record::TurnSpawned { turn, .. } | Record::TurnEnded { turn, .. } => {
            let row =
                super::super::get_row::<super::super::TurnRow>(tables.turns, &turn.to_bytes())?;
            if let Some(row) = row.filter(|row| row.mode == Mode::Managed && row.goal.is_some()) {
                let work = RecoveryWork {
                    agent: row.agent,
                    goal: row.goal.ok_or(RecoveryStateError::Binding)?,
                    turn: *turn,
                };
                let cache = RecoveryCache::load_work(&CacheReader(tables.recovery), through, work)?;
                if let Some(claim) = cache.state().work_claim(work)? {
                    let receipt = cache.state().claim_receipt(claim)?;
                    if let RecoveryClaimState::IssueBound(issue) = receipt.state {
                        let history = source(lsn, entry, context)?;
                        let evidence = history.at(lsn).map_err(|_| RecoveryStateError::Binding)?;
                        apply(
                            tables,
                            through,
                            lsn,
                            RecoveryTransition::Consumed {
                                claim,
                                issue,
                                evidence,
                            },
                        )
                    } else {
                        Ok(Vec::new())
                    }
                } else {
                    Ok(Vec::new())
                }
            } else {
                Ok(Vec::new())
            }
        }
        _ => Ok(Vec::new()),
    })();
    result.map_err(|error| match error {
        Error::RecoveryState(reason) => Error::RecoveryRecord { lsn, reason },
        other => other,
    })
}

fn source(lsn: Lsn, entry: &Entry, context: RecoveryWitnessContext) -> Result<RecoveryHistory> {
    let mut history = RecoveryHistory::default();
    history
        .observe_with_context(lsn, entry, context, super::super::instructions::hash)
        .map_err(dudes_proto::recovery_state::cache::RecoveryCacheError::from)?;
    Ok(history)
}

fn apply<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    lsn: Lsn,
    transition: RecoveryTransition<'_>,
) -> Result<Vec<RecoveryCacheUpdate>>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let mut cache =
        RecoveryCache::load_transition(&CacheReader(tables.recovery), through, transition)?;
    match transition {
        RecoveryTransition::Invalidated(record) => {
            let positions = match record.evidence {
                dudes_proto::recovery::RecoveryInvalidationEvidence::RecordedChange { lsn, .. } => vec![lsn],
                dudes_proto::recovery::RecoveryInvalidationEvidence::BrowserAuthorityUnavailable { .. } => Vec::new(),
            };
            let sources =
                RecoveryCache::load_sources(&CacheReader(tables.recovery), through, &positions)?;
            cache
                .state_mut()
                .apply_invalidation(lsn, record, through, sources.history())?;
        }
        RecoveryTransition::Consumed {
            claim,
            issue,
            evidence,
        } => {
            let facts = attempt_at(tables, through, issue)?;
            cache
                .state_mut()
                .apply_consumed(lsn, claim, issue, evidence, &facts)?;
        }
        _ => {
            let binding = transition.binding(cache.state())?;
            let facts = facts_at(tables, through, &binding)?;
            let sources = RecoveryCache::load_sources(
                &CacheReader(tables.recovery),
                through,
                &transition.sources(&facts),
            )?;
            cache
                .state_mut()
                .apply_transition(lsn, transition, &facts, sources.history())?;
            super::validate_new_turn(tables.turns, transition)?;
        }
    }
    Ok(cache.updates()?)
}
