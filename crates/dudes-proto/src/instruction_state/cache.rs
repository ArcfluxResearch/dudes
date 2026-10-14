//! Keyed persistence for the pure instruction fold. Snapshots and terminal evidence have
//! separate immutable cells; a fold loads only its relationships and affected pending cards.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use dudes_types::{AgentId, CardId, GoalId, Lsn, TurnId};
use serde::{Deserialize, Serialize};

use super::{
    End, InstructionAttempt, InstructionBaselineRef, InstructionBoundaryAbsenceProof,
    InstructionBoundarySettlement, InstructionFinished, InstructionNativeBoundaryReceipt,
    InstructionObservationOwner, InstructionSettlement, InstructionSnapshot, InstructionState,
    InstructionTicketAssociation, Issue, baseline_key, issue_identity, owner_agent,
};
use crate::record::Record;
use crate::rpc::pty::PtyId;

#[cfg(test)]
mod tests;

/// A malformed derived instruction cache. Rebuilding uses the canonical journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionCacheError {
    /// A cell cannot be decoded or encoded.
    Encoding,
    /// The structural key and typed value disagree.
    Identity,
    /// An index refers to an absent attempt or baseline.
    MissingCell,
}

impl fmt::Display for InstructionCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid instruction cache: {self:?}")
    }
}

impl std::error::Error for InstructionCacheError {}

/// One structural key and its encoded typed payload.
pub type InstructionCacheRow = (Vec<u8>, Vec<u8>);

/// Byte cells read at one committed projection cut.
pub trait InstructionCacheReader {
    /// Storage errors retain their origin; cache consistency errors are also reportable.
    type Error: From<InstructionCacheError>;

    /// Read one structural key.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Read the exact prefix in key order. An empty prefix requests a cold reconstruction.
    fn prefix(&self, prefix: &[u8]) -> Result<Vec<InstructionCacheRow>, Self::Error>;
}

/// One changed cell, or a removed relationship. Applied with the projection cursor and CRC.
pub struct InstructionCacheUpdate {
    /// Structural byte key.
    pub key: Vec<u8>,
    /// Canonical cell bytes, or removal.
    pub value: Option<Vec<u8>>,
}

/// A scoped fold and its original cells, isolated from unrelated terminal history.
pub struct InstructionCache {
    state: InstructionState,
    before: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl InstructionCache {
    /// Load the dependencies of one record and any board cards affected by an external fold.
    /// `through` is the projection's committed position, never a predicted append position.
    pub fn load<R: InstructionCacheReader>(
        reader: &R,
        through: Lsn,
        record: &Record,
        affected_cards: &[CardId],
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through);
        loader.record(record)?;
        for card in affected_cards {
            loader.card(*card)?;
        }
        Ok(loader.finish())
    }

    /// Explicit cold reconstruction for export and offline inspection.
    pub fn read_all<R: InstructionCacheReader>(
        reader: &R,
        through: Lsn,
    ) -> Result<InstructionState, R::Error> {
        let mut loader = Loader::new(reader, through);
        loader.prefix(&[])?;
        for attempt in loader.state.attempts.values() {
            if matches!(attempt.baseline, InstructionBaselineRef::Started { .. })
                && attempt.snapshot.is_none()
            {
                return Err(InstructionCacheError::MissingCell.into());
            }
        }
        Ok(loader.state)
    }

    /// The same pure state used by the in-memory lifecycle adapter.
    #[must_use]
    pub fn state(&self) -> &InstructionState {
        &self.state
    }

    /// Apply the pure fold after loading its complete scope.
    pub fn state_mut(&mut self) -> &mut InstructionState {
        &mut self.state
    }

    /// Only changed cells. Unchanged snapshots and terminal evidence are never rewritten.
    pub fn updates(&self) -> Result<Vec<InstructionCacheUpdate>, InstructionCacheError> {
        let after = cells(&self.state)?;
        let mut updates = Vec::new();
        for (key, value) in &after {
            if self.before.get(key) != Some(value) {
                updates.push(InstructionCacheUpdate {
                    key: key.clone(),
                    value: Some(value.clone()),
                });
            }
        }
        for key in self.before.keys() {
            if !after.contains_key(key) {
                updates.push(InstructionCacheUpdate {
                    key: key.clone(),
                    value: None,
                });
            }
        }
        Ok(updates)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AttemptMetadata {
    baseline: InstructionBaselineRef,
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    reviewed_lsn: Option<Lsn>,
    spawn_lsn: Option<Lsn>,
    boundary_settled: Option<InstructionBoundarySettlement>,
}

impl From<&InstructionAttempt> for AttemptMetadata {
    fn from(attempt: &InstructionAttempt) -> Self {
        Self {
            baseline: attempt.baseline,
            owner: attempt.owner,
            ticket: attempt.ticket,
            reviewed_lsn: attempt.reviewed_lsn,
            spawn_lsn: attempt.spawn_lsn,
            boundary_settled: attempt.boundary_settled.clone(),
        }
    }
}

impl AttemptMetadata {
    fn into_attempt(self) -> InstructionAttempt {
        InstructionAttempt {
            baseline: self.baseline,
            owner: self.owner,
            ticket: self.ticket,
            snapshot: None,
            finished: None,
            reviewed_lsn: self.reviewed_lsn,
            spawn_lsn: self.spawn_lsn,
            boundary_settled: self.boundary_settled,
        }
    }
}

#[derive(Clone, Copy)]
enum Key {
    Attempt(Lsn),
    Snapshot(Lsn),
    Finished(Lsn),
    Issue(TurnId),
    End(TurnId),
    Attachment(Lsn),
    AttachmentEnd(Lsn),
    CurrentTurn(TurnId),
    CurrentAttachment(Lsn),
    Unfinished(Lsn),
    Unresolved(AgentId, Lsn),
    Assignment(CardId),
    Association(GoalId),
    TicketHistory(GoalId),
    NativeReceipt(Lsn),
    PtyAttachment(PtyId),
    CloseRequest(Lsn),
    LatestAttachment(AgentId),
    CardGoal(CardId),
    CardOutstanding(CardId, Lsn),
    GoalOutstanding(GoalId, Lsn),
    NoHostHold(Lsn),
    NoHostClaim(Lsn),
    ObservedTurn(TurnId),
    AttachmentAttempt(Lsn),
}

impl Key {
    fn bytes(self) -> Vec<u8> {
        match self {
            Self::Attempt(id) => key(1, &id.0.to_be_bytes()),
            Self::Snapshot(id) => key(2, &id.0.to_be_bytes()),
            Self::Finished(id) => key(3, &id.0.to_be_bytes()),
            Self::Issue(id) => key(4, &id.to_bytes()),
            Self::End(id) => key(5, &id.to_bytes()),
            Self::Attachment(id) => key(6, &id.0.to_be_bytes()),
            Self::AttachmentEnd(id) => key(7, &id.0.to_be_bytes()),
            Self::CurrentTurn(id) => key(8, &id.to_bytes()),
            Self::CurrentAttachment(id) => key(9, &id.0.to_be_bytes()),
            Self::Unfinished(id) => key(10, &id.0.to_be_bytes()),
            Self::Unresolved(agent, id) => member_key(11, agent.to_bytes(), id),
            Self::Assignment(id) => key(12, &id.to_bytes()),
            Self::Association(id) => key(13, &id.to_bytes()),
            Self::TicketHistory(id) => key(14, &id.to_bytes()),
            Self::NativeReceipt(id) => key(15, &id.0.to_be_bytes()),
            Self::PtyAttachment(id) => key(16, &id.0.to_be_bytes()),
            Self::CloseRequest(id) => key(17, &id.0.to_be_bytes()),
            Self::LatestAttachment(id) => key(18, &id.to_bytes()),
            Self::CardGoal(id) => key(19, &id.to_bytes()),
            Self::CardOutstanding(card, id) => member_key(20, card.to_bytes(), id),
            Self::GoalOutstanding(goal, id) => member_key(21, goal.to_bytes(), id),
            Self::NoHostHold(id) => key(22, &id.0.to_be_bytes()),
            Self::NoHostClaim(id) => key(23, &id.0.to_be_bytes()),
            Self::ObservedTurn(id) => key(24, &id.to_bytes()),
            Self::AttachmentAttempt(id) => key(25, &id.0.to_be_bytes()),
        }
    }
}

fn key(tag: u8, id: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + id.len());
    key.push(tag);
    key.extend_from_slice(id);
    key
}

fn member_key(tag: u8, id: [u8; 16], lsn: Lsn) -> Vec<u8> {
    let mut key = key(tag, &id);
    key.extend_from_slice(&lsn.0.to_be_bytes());
    key
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Cell {
    Attempt {
        lsn: Lsn,
        value: AttemptMetadata,
    },
    Snapshot {
        lsn: Lsn,
        value: InstructionSnapshot,
    },
    Finished {
        lsn: Lsn,
        value: InstructionFinished,
    },
    Issue {
        turn: TurnId,
        value: Issue,
    },
    End {
        turn: TurnId,
        value: End,
    },
    Attachment {
        lsn: Lsn,
        agent: AgentId,
    },
    AttachmentEnd {
        attachment: Lsn,
        end: Lsn,
    },
    CurrentTurn {
        turn: TurnId,
        started: Lsn,
    },
    CurrentAttachment {
        attachment: Lsn,
        started: Lsn,
    },
    Unfinished {
        lsn: Lsn,
    },
    Unresolved {
        agent: AgentId,
        lsn: Lsn,
    },
    Assignment {
        card: CardId,
        lsn: Lsn,
    },
    Association {
        value: InstructionTicketAssociation,
    },
    TicketHistory {
        value: InstructionTicketAssociation,
    },
    NativeReceipt {
        lsn: Lsn,
        started: Lsn,
        receipt: InstructionNativeBoundaryReceipt,
    },
    PtyAttachment {
        pty: PtyId,
        attachment: Lsn,
    },
    CloseRequest {
        request: Lsn,
        attachment: Lsn,
    },
    LatestAttachment {
        agent: AgentId,
        attachment: Lsn,
    },
    CardGoal {
        card: CardId,
        goal: GoalId,
    },
    CardOutstanding {
        card: CardId,
        lsn: Lsn,
    },
    GoalOutstanding {
        goal: GoalId,
        lsn: Lsn,
    },
    NoHostHold {
        lsn: Lsn,
        agent: AgentId,
    },
    NoHostClaim {
        lsn: Lsn,
        started: Lsn,
    },
    ObservedTurn {
        turn: TurnId,
    },
    AttachmentAttempt {
        attachment: Lsn,
        started: Lsn,
    },
}

impl Cell {
    fn key(&self) -> Key {
        match self {
            Self::Attempt { lsn, .. } => Key::Attempt(*lsn),
            Self::Snapshot { lsn, .. } => Key::Snapshot(*lsn),
            Self::Finished { lsn, .. } => Key::Finished(*lsn),
            Self::Issue { turn, .. } => Key::Issue(*turn),
            Self::End { turn, .. } => Key::End(*turn),
            Self::Attachment { lsn, .. } => Key::Attachment(*lsn),
            Self::AttachmentEnd { attachment, .. } => Key::AttachmentEnd(*attachment),
            Self::CurrentTurn { turn, .. } => Key::CurrentTurn(*turn),
            Self::CurrentAttachment { attachment, .. } => Key::CurrentAttachment(*attachment),
            Self::Unfinished { lsn } => Key::Unfinished(*lsn),
            Self::Unresolved { agent, lsn } => Key::Unresolved(*agent, *lsn),
            Self::Assignment { card, .. } => Key::Assignment(*card),
            Self::Association { value } => Key::Association(value.goal),
            Self::TicketHistory { value } => Key::TicketHistory(value.goal),
            Self::NativeReceipt { lsn, .. } => Key::NativeReceipt(*lsn),
            Self::PtyAttachment { pty, .. } => Key::PtyAttachment(*pty),
            Self::CloseRequest { request, .. } => Key::CloseRequest(*request),
            Self::LatestAttachment { agent, .. } => Key::LatestAttachment(*agent),
            Self::CardGoal { card, .. } => Key::CardGoal(*card),
            Self::CardOutstanding { card, lsn } => Key::CardOutstanding(*card, *lsn),
            Self::GoalOutstanding { goal, lsn } => Key::GoalOutstanding(*goal, *lsn),
            Self::NoHostHold { lsn, .. } => Key::NoHostHold(*lsn),
            Self::NoHostClaim { lsn, .. } => Key::NoHostClaim(*lsn),
            Self::ObservedTurn { turn } => Key::ObservedTurn(*turn),
            Self::AttachmentAttempt { attachment, .. } => Key::AttachmentAttempt(*attachment),
        }
    }

    fn insert(self, state: &mut InstructionState) -> Result<(), InstructionCacheError> {
        match self {
            Self::Attempt { lsn, value } => {
                if baseline_key(value.baseline) != lsn {
                    return Err(InstructionCacheError::Identity);
                }
                state.attempts.insert(lsn, value.into_attempt());
            }
            Self::Snapshot { lsn, value } => {
                attempt_mut(state, lsn)?.snapshot = Some(value);
            }
            Self::Finished { lsn, value } => {
                attempt_mut(state, lsn)?.finished = Some(value);
            }
            Self::Issue { turn, value } => {
                state.issues.insert(turn, value);
            }
            Self::End { turn, value } => {
                state.ends.insert(turn, value);
            }
            Self::Attachment { lsn, agent } => {
                state.attachments.insert(lsn, agent);
            }
            Self::AttachmentEnd { attachment, end } => {
                state.attachment_ends.insert(attachment, end);
            }
            Self::CurrentTurn { turn, started } => {
                state.current_turns.insert(turn, started);
            }
            Self::CurrentAttachment {
                attachment,
                started,
            } => {
                state.current_attachments.insert(attachment, started);
            }
            Self::Unfinished { lsn } => {
                state.unfinished.insert(lsn);
            }
            Self::Unresolved { agent, lsn } => {
                state.unresolved.entry(agent).or_default().insert(lsn);
            }
            Self::Assignment { card, lsn } => {
                state.assignments.insert(card, lsn);
            }
            Self::Association { value } => {
                state.associations.insert(value.goal, value);
            }
            Self::TicketHistory { value } => {
                state.ticket_history.insert(value.goal, value);
            }
            Self::NativeReceipt {
                lsn,
                started,
                receipt,
            } => {
                state.native_receipts.insert(lsn, (started, receipt));
            }
            Self::PtyAttachment { pty, attachment } => {
                state.pty_attachments.insert(pty, attachment);
            }
            Self::CloseRequest {
                request,
                attachment,
            } => {
                state.close_requests.insert(request, attachment);
            }
            Self::LatestAttachment { agent, attachment } => {
                state.latest_attachments.insert(agent, attachment);
            }
            Self::CardGoal { card, goal } => {
                state.card_goals.insert(card, goal);
            }
            Self::CardOutstanding { card, lsn } => {
                state.by_card.entry(card).or_default().insert(lsn);
            }
            Self::GoalOutstanding { goal, lsn } => {
                state.by_goal.entry(goal).or_default().insert(lsn);
            }
            Self::NoHostHold { lsn, agent } => {
                state.no_host_holds.insert(lsn, agent);
            }
            Self::NoHostClaim { lsn, started } => {
                state.no_host_claims.insert(lsn, started);
            }
            Self::ObservedTurn { turn } => {
                state.observed_turns.insert(turn);
            }
            Self::AttachmentAttempt {
                attachment,
                started,
            } => {
                state.attachment_attempts.insert(attachment, started);
            }
        }
        Ok(())
    }
}

fn attempt_mut(
    state: &mut InstructionState,
    lsn: Lsn,
) -> Result<&mut InstructionAttempt, InstructionCacheError> {
    state
        .attempts
        .get_mut(&lsn)
        .ok_or(InstructionCacheError::MissingCell)
}

fn attempt_cells(lsn: Lsn, attempt: &InstructionAttempt) -> [Option<Cell>; 3] {
    [
        Some(Cell::Attempt {
            lsn,
            value: attempt.into(),
        }),
        attempt.snapshot.as_ref().map(|value| Cell::Snapshot {
            lsn,
            value: value.clone(),
        }),
        attempt.finished.as_ref().map(|value| Cell::Finished {
            lsn,
            value: value.clone(),
        }),
    ]
}

fn cells(state: &InstructionState) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, InstructionCacheError> {
    let mut cells = BTreeMap::new();
    let mut add = |cell: Cell| -> Result<(), InstructionCacheError> {
        cells.insert(
            cell.key().bytes(),
            serde_json::to_vec(&cell).map_err(|_| InstructionCacheError::Encoding)?,
        );
        Ok(())
    };
    for (&lsn, value) in &state.attempts {
        for cell in attempt_cells(lsn, value).into_iter().flatten() {
            add(cell)?;
        }
    }
    for (&turn, &value) in &state.issues {
        add(Cell::Issue { turn, value })?;
    }
    for (&turn, &value) in &state.ends {
        add(Cell::End { turn, value })?;
    }
    for (&lsn, &agent) in &state.attachments {
        add(Cell::Attachment { lsn, agent })?;
    }
    for (&attachment, &end) in &state.attachment_ends {
        add(Cell::AttachmentEnd { attachment, end })?;
    }
    for (&turn, &started) in &state.current_turns {
        add(Cell::CurrentTurn { turn, started })?;
    }
    for (&attachment, &started) in &state.current_attachments {
        add(Cell::CurrentAttachment {
            attachment,
            started,
        })?;
    }
    for &lsn in &state.unfinished {
        add(Cell::Unfinished { lsn })?;
    }
    for (&agent, keys) in &state.unresolved {
        for &lsn in keys {
            add(Cell::Unresolved { agent, lsn })?;
        }
    }
    for (&card, &lsn) in &state.assignments {
        add(Cell::Assignment { card, lsn })?;
    }
    for &value in state.associations.values() {
        add(Cell::Association { value })?;
    }
    for &value in state.ticket_history.values() {
        add(Cell::TicketHistory { value })?;
    }
    for (&lsn, &(started, receipt)) in &state.native_receipts {
        add(Cell::NativeReceipt {
            lsn,
            started,
            receipt,
        })?;
    }
    for (&pty, &attachment) in &state.pty_attachments {
        add(Cell::PtyAttachment { pty, attachment })?;
    }
    for (&request, &attachment) in &state.close_requests {
        add(Cell::CloseRequest {
            request,
            attachment,
        })?;
    }
    for (&agent, &attachment) in &state.latest_attachments {
        add(Cell::LatestAttachment { agent, attachment })?;
    }
    for (&card, &goal) in &state.card_goals {
        add(Cell::CardGoal { card, goal })?;
    }
    for (&card, keys) in &state.by_card {
        for &lsn in keys {
            add(Cell::CardOutstanding { card, lsn })?;
        }
    }
    for (&goal, keys) in &state.by_goal {
        for &lsn in keys {
            add(Cell::GoalOutstanding { goal, lsn })?;
        }
    }
    for (&lsn, &agent) in &state.no_host_holds {
        add(Cell::NoHostHold { lsn, agent })?;
    }
    for (&lsn, &started) in &state.no_host_claims {
        add(Cell::NoHostClaim { lsn, started })?;
    }
    for &turn in &state.observed_turns {
        add(Cell::ObservedTurn { turn })?;
    }
    for (&attachment, &started) in &state.attachment_attempts {
        add(Cell::AttachmentAttempt {
            attachment,
            started,
        })?;
    }
    Ok(cells)
}

struct Loader<'a, R> {
    reader: &'a R,
    state: InstructionState,
    before: BTreeMap<Vec<u8>, Vec<u8>>,
    loaded: BTreeSet<Vec<u8>>,
    prefixes: BTreeSet<Vec<u8>>,
    attempts: BTreeSet<Lsn>,
}

impl<'a, R: InstructionCacheReader> Loader<'a, R> {
    fn new(reader: &'a R, through: Lsn) -> Self {
        Self {
            reader,
            state: InstructionState {
                last_applied: through,
                ..InstructionState::default()
            },
            before: BTreeMap::new(),
            loaded: BTreeSet::new(),
            prefixes: BTreeSet::new(),
            attempts: BTreeSet::new(),
        }
    }

    fn finish(self) -> InstructionCache {
        InstructionCache {
            state: self.state,
            before: self.before,
        }
    }

    fn insert(&mut self, key: Vec<u8>, bytes: Vec<u8>) -> Result<(), R::Error> {
        if self.loaded.contains(&key) {
            return Ok(());
        }
        let cell: Cell =
            serde_json::from_slice(&bytes).map_err(|_| InstructionCacheError::Encoding)?;
        if cell.key().bytes() != key {
            return Err(InstructionCacheError::Identity.into());
        }
        cell.insert(&mut self.state)?;
        self.loaded.insert(key.clone());
        self.before.insert(key, bytes);
        Ok(())
    }

    fn get(&mut self, key: Key) -> Result<(), R::Error> {
        let key = key.bytes();
        if !self.loaded.contains(&key) {
            if let Some(bytes) = self.reader.get(&key)? {
                self.insert(key, bytes)?;
            } else {
                self.loaded.insert(key);
            }
        }
        Ok(())
    }

    fn prefix(&mut self, prefix: &[u8]) -> Result<(), R::Error> {
        if self.prefixes.insert(prefix.to_vec()) {
            for (key, bytes) in self.reader.prefix(prefix)? {
                if !key.starts_with(prefix) {
                    return Err(InstructionCacheError::Identity.into());
                }
                self.insert(key, bytes)?;
            }
        }
        Ok(())
    }

    fn owner(&mut self, owner: InstructionObservationOwner) -> Result<(), R::Error> {
        match owner {
            InstructionObservationOwner::Managed { turn, .. }
            | InstructionObservationOwner::AttachedTurn { turn, .. } => {
                self.get(Key::Issue(turn))?;
                self.get(Key::End(turn))?;
                self.get(Key::CurrentTurn(turn))?;
                self.get(Key::ObservedTurn(turn))?;
            }
            InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } => {
                self.get(Key::Attachment(attachment_lsn))?;
                self.get(Key::AttachmentEnd(attachment_lsn))?;
                self.get(Key::CurrentAttachment(attachment_lsn))?;
                self.get(Key::AttachmentAttempt(attachment_lsn))?;
            }
        }
        Ok(())
    }

    fn ticket(&mut self, ticket: Option<InstructionTicketAssociation>) -> Result<(), R::Error> {
        if let Some(ticket) = ticket {
            self.get(Key::TicketHistory(ticket.goal))?;
        }
        Ok(())
    }

    fn attempt(&mut self, lsn: Lsn, required: bool) -> Result<(), R::Error> {
        if !self.attempts.insert(lsn) {
            return Ok(());
        }
        self.get(Key::Attempt(lsn))?;
        let Some(attempt) = self.state.attempts.get(&lsn) else {
            return if required {
                Err(InstructionCacheError::MissingCell.into())
            } else {
                Ok(())
            };
        };
        let owner = attempt.owner;
        let ticket = attempt.ticket;
        let started = matches!(attempt.baseline, InstructionBaselineRef::Started { .. });
        self.get(Key::Snapshot(lsn))?;
        self.get(Key::Finished(lsn))?;
        if started && self.state.attempts[&lsn].snapshot.is_none() {
            return Err(InstructionCacheError::MissingCell.into());
        }
        self.get(Key::Unfinished(lsn))?;
        self.get(Key::Unresolved(owner_agent(owner), lsn))?;
        self.owner(owner)?;
        self.ticket(ticket)?;
        if let Some(ticket) = ticket {
            self.get(Key::CardOutstanding(ticket.card, lsn))?;
            self.get(Key::GoalOutstanding(ticket.goal, lsn))?;
        }
        Ok(())
    }

    fn card(&mut self, card: CardId) -> Result<(), R::Error> {
        self.prefix(&key(20, &card.to_bytes()))?;
        let keys = self.state.by_card.get(&card).cloned().unwrap_or_default();
        for lsn in keys {
            self.attempt(lsn, true)?;
        }
        Ok(())
    }

    fn assignment(&mut self, card: CardId) -> Result<(), R::Error> {
        self.get(Key::Assignment(card))?;
        self.get(Key::CardGoal(card))?;
        if let Some(&goal) = self.state.card_goals.get(&card) {
            self.get(Key::Association(goal))?;
        }
        Ok(())
    }

    fn current_turn(&mut self, turn: TurnId, card: bool) -> Result<(), R::Error> {
        self.get(Key::CurrentTurn(turn))?;
        if let Some(&lsn) = self.state.current_turns.get(&turn) {
            self.attempt(lsn, true)?;
            if card && let Some(ticket) = self.state.attempts[&lsn].ticket {
                self.card(ticket.card)?;
            }
        }
        Ok(())
    }

    fn current_attachment(&mut self, attachment: Lsn, card: bool) -> Result<(), R::Error> {
        self.get(Key::CurrentAttachment(attachment))?;
        if let Some(&lsn) = self.state.current_attachments.get(&attachment) {
            self.attempt(lsn, true)?;
            if card && let Some(ticket) = self.state.attempts[&lsn].ticket {
                self.card(ticket.card)?;
            }
        }
        Ok(())
    }

    fn record(&mut self, record: &Record) -> Result<(), R::Error> {
        if let Some((_, turn)) = issue_identity(record) {
            self.get(Key::Issue(turn))?;
            self.current_turn(turn, false)?;
        }
        match record {
            Record::InstructionObservationStarted { owner, ticket, .. } => {
                self.owner(*owner)?;
                self.ticket(*ticket)?;
                if let Some(ticket) = ticket {
                    self.card(ticket.card)?;
                }
            }
            Record::InstructionObservationFinished {
                owner,
                ticket,
                baseline,
                settlement,
            } => {
                self.attempt(baseline_key(*baseline), false)?;
                self.owner(*owner)?;
                self.ticket(*ticket)?;
                if let Some(ticket) = ticket {
                    self.card(ticket.card)?;
                }
                if let InstructionSettlement::NoProcess { proof_lsn, .. } = settlement {
                    self.get(Key::NoHostHold(*proof_lsn))?;
                    self.get(Key::NoHostClaim(*proof_lsn))?;
                    self.get(Key::NativeReceipt(*proof_lsn))?;
                }
            }
            Record::TicketInstructionReviewed { card, .. }
            | Record::TicketAssigned { card, .. }
            | Record::TicketRemoved { card, .. } => {
                self.assignment(*card)?;
                self.card(*card)?;
            }
            Record::InstructionAttemptBoundarySettled {
                started_lsn, proof, ..
            } => {
                self.attempt(*started_lsn, false)?;
                if let InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt } =
                    proof
                {
                    let (InstructionNativeBoundaryReceipt::Spawned { lsn }
                    | InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn }) = receipt;
                    self.get(Key::NativeReceipt(*lsn))?;
                }
            }
            Record::TurnEnded {
                turn, provenance, ..
            } if provenance.is_observed() => {
                self.get(Key::End(*turn))?;
                self.current_turn(*turn, true)?;
            }
            Record::Attached { agent, .. } => {
                self.get(Key::LatestAttachment(*agent))?;
            }
            Record::TurnSpawned { turn, .. } => {
                self.current_turn(*turn, false)?;
            }
            Record::PtySpawned { agent, pty, .. } => {
                self.get(Key::LatestAttachment(*agent))?;
                self.get(Key::PtyAttachment(*pty))?;
                if let Some(&attachment) = self.state.latest_attachments.get(agent) {
                    self.current_attachment(attachment, false)?;
                }
            }
            Record::PtyCloseRequested { pty, .. } => {
                self.get(Key::PtyAttachment(*pty))?;
            }
            Record::PtyCloseCompleted {
                request_lsn,
                outcome:
                    crate::record::PtyCloseOutcome::Closed
                    | crate::record::PtyCloseOutcome::AlreadyClosed,
            } => {
                self.get(Key::CloseRequest(*request_lsn))?;
                if let Some(&attachment) = self.state.close_requests.get(request_lsn) {
                    self.get(Key::AttachmentEnd(attachment))?;
                    self.current_attachment(attachment, true)?;
                }
            }
            Record::TicketStarted { card, goal, .. } => {
                self.assignment(*card)?;
                self.get(Key::Association(goal.id))?;
                self.get(Key::TicketHistory(goal.id))?;
                self.card(*card)?;
            }
            Record::CardTransition { card, .. } | Record::TicketEdited { card, .. } => {
                self.card(*card)?;
            }
            _ => {}
        }
        Ok(())
    }
}
