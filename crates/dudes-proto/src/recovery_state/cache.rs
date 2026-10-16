//! Scoped projection cells for recovery claims and immutable source witnesses.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use dudes_types::{Lsn, TurnId};
use serde::{Deserialize, Serialize};

use super::history::{RecoveryHistory, RecoveryHistoryError, RecoveryWitness, RecoveryWitnessRef};
use super::{RecoveryClaimState, RecoveryEpisode, RecoveryEvent, RecoveryState};
use crate::record::Hex32;
use crate::recovery::{
    MAX_RECOVERY_RECAP_BYTES, RecoveryAuthorized, RecoveryBinding, RecoveryContext,
    RecoveryEpisodeId, RecoveryIssue, RecoveryPlan, RecoveryReason, RecoveryRequired, RecoveryWork,
};

#[cfg(test)]
mod tests;

/// A malformed projection, independent of journal or request contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryCacheError {
    /// A typed cell cannot be decoded or encoded.
    Encoding,
    /// Structural key, value or stored relationship disagrees.
    Identity,
    /// A required referenced cell is absent.
    MissingCell,
    /// A cell names a zero or future position.
    Position,
    /// Historical evidence has an invalid shape or conflicts at its position.
    History(RecoveryHistoryError),
}

impl fmt::Display for RecoveryCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid recovery cache: {self:?}")
    }
}
impl std::error::Error for RecoveryCacheError {}
impl From<RecoveryHistoryError> for RecoveryCacheError {
    fn from(value: RecoveryHistoryError) -> Self {
        Self::History(value)
    }
}

/// One structural key and its encoded typed value.
pub type RecoveryCacheRow = (Vec<u8>, Vec<u8>);

/// Read deterministic cells from one committed projection cut.
///
/// This codec interface does not certify completeness or truthful absence of optional roots.
/// Production recovery authority requires the journal-owned checked-store boundary.
pub trait RecoveryCacheReader {
    /// Storage errors preserve their origin as well as cache errors.
    type Error: From<RecoveryCacheError>;
    /// Read one exact key.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
    /// Read structural keys in prefix order; empty prefix is explicit cold reconstruction.
    fn prefix(&self, prefix: &[u8]) -> Result<Vec<RecoveryCacheRow>, Self::Error>;
}

/// A changed cell or removal, to commit with the caller's existing cursor and checksum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCacheUpdate {
    /// Structural byte key.
    pub key: Vec<u8>,
    /// Canonical typed bytes, or removal.
    pub value: Option<Vec<u8>>,
}

/// One event's dependency closure. It is not a global view of unloaded requests or history.
pub struct RecoveryCache {
    state: RecoveryState,
    history: RecoveryHistory,
    before: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[derive(Clone, Copy)]
enum Dependencies<'a> {
    Required(&'a RecoveryRequired),
    Authorized(&'a RecoveryAuthorized),
    Claim(Lsn),
}

impl<'a> From<RecoveryEvent<'a>> for Dependencies<'a> {
    fn from(event: RecoveryEvent<'a>) -> Self {
        match event {
            RecoveryEvent::Required(record) => Self::Required(record),
            RecoveryEvent::Authorized(record) => Self::Authorized(record),
            RecoveryEvent::Issued { claim, .. }
            | RecoveryEvent::Consumed { claim, .. }
            | RecoveryEvent::Invalidated { claim, .. } => Self::Claim(claim.authorization_lsn),
        }
    }
}

impl<'a> From<super::transition::RecoveryTransition<'a>> for Dependencies<'a> {
    fn from(transition: super::transition::RecoveryTransition<'a>) -> Self {
        use super::transition::RecoveryTransition as T;
        match transition {
            T::Required(record) => Self::Required(record),
            T::Authorized(record) => Self::Authorized(record),
            T::Issued { claim, .. } | T::Consumed { claim, .. } => {
                Self::Claim(claim.authorization_lsn)
            }
            T::Invalidated(record) => Self::Claim(record.claim.authorization_lsn),
        }
    }
}

impl RecoveryCache {
    /// Load one global planned-turn reservation and its existing claim dependencies.
    /// Absence is authoritative only at the journal-owned checked boundary.
    pub fn load_planned<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        turn: TurnId,
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.planned(turn, false)?;
        Ok(loader.finish())
    }

    /// Load only current work and its original planned-turn reservation from a certified store.
    /// Absence is authoritative only at the journal-owned checked boundary.
    pub fn load_work<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        work: RecoveryWork,
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.current(work)?;
        loader.planned(work.turn, false)?;
        Ok(loader.finish())
    }

    /// Load only required actual source positions from a caller-owned certified prefix.
    ///
    /// This codec validates present cells; it cannot certify the underlying store.
    pub fn load_sources<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        sources: &[Lsn],
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        for lsn in sources {
            loader.read(Key::Witness(*lsn), true)?;
        }
        Ok(loader.finish())
    }

    /// Load one contract transition and explicitly requested immutable witness references.
    ///
    /// The caller supplies a complete store certified by the journal gateway; this raw loader
    /// validates only the dependency closure it reads.
    pub fn load<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        event: RecoveryEvent<'_>,
        witnesses: &[RecoveryWitnessRef],
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.dependencies(event.into())?;
        for reference in witnesses {
            loader.witness(*reference)?;
        }
        Ok(loader.finish())
    }

    /// Load only the typed transition's claim dependencies, without validating its evidence.
    /// The caller still needs a certified complete store and strict historical interpretation.
    pub fn load_transition<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        transition: super::transition::RecoveryTransition<'_>,
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.dependencies(transition.into())?;
        Ok(loader.finish())
    }

    /// Load an actual source position before retaining its immutable witness.
    pub fn load_history<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        actual_lsn: Lsn,
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.read(Key::Witness(actual_lsn), false)?;
        Ok(loader.finish())
    }

    /// Load only a historical receipt and its fixed relationships, without following current work.
    ///
    /// An absent request is truthful only when the caller owns a checked complete store.
    pub fn load_receipt<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
        request: Hex32,
    ) -> Result<Self, R::Error> {
        let mut loader = Loader::new(reader, through)?;
        loader.request(request, false)?;
        Ok(loader.finish())
    }

    /// Reconstruct and validate all cells without synthesizing missing reverse indexes.
    pub fn read_all<R: RecoveryCacheReader>(
        reader: &R,
        through: Lsn,
    ) -> Result<(RecoveryState, RecoveryHistory), R::Error> {
        let mut loader = Loader::new(reader, through)?;
        let mut rows = BTreeMap::new();
        for (key, value) in reader.prefix(&[])? {
            if rows.insert(key, value).is_some() {
                return Err(RecoveryCacheError::Identity.into());
            }
        }
        for (key, value) in rows {
            loader.decode(&key, &value)?;
        }
        loader.validate_all()?;
        Ok((loader.state, loader.history))
    }

    /// Scoped state for the transition used by `load`.
    #[must_use]
    pub const fn state(&self) -> &RecoveryState {
        &self.state
    }
    /// Apply the already-loaded transition using the existing pure reducer.
    pub fn state_mut(&mut self) -> &mut RecoveryState {
        &mut self.state
    }
    /// Explicitly loaded immutable source evidence.
    #[must_use]
    pub const fn history(&self) -> &RecoveryHistory {
        &self.history
    }
    /// Retain source evidence after `load_history` supplied its actual-position scope.
    pub fn history_mut(&mut self) -> &mut RecoveryHistory {
        &mut self.history
    }

    /// Only logical changes; immutable intent and unrelated terminal history are not rewritten.
    pub fn updates(&self) -> Result<Vec<RecoveryCacheUpdate>, RecoveryCacheError> {
        let after = cells(&self.state, &self.history)?;
        let mut updates = Vec::new();
        for (key, value) in &after {
            if self.before.get(key) != Some(value) {
                updates.push(RecoveryCacheUpdate {
                    key: key.clone(),
                    value: Some(value.clone()),
                });
            }
        }
        for key in self.before.keys() {
            if !after.contains_key(key) {
                updates.push(RecoveryCacheUpdate {
                    key: key.clone(),
                    value: None,
                });
            }
        }
        Ok(updates)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Cut,
    Episode(RecoveryEpisodeId),
    EpisodeClaim(RecoveryEpisodeId),
    Current(RecoveryWork),
    Authorization(Lsn),
    Claim(Lsn),
    Request(Hex32),
    Planned(TurnId),
    Witness(Lsn),
}

impl Key {
    fn bytes(self) -> Vec<u8> {
        let (tag, value) = match self {
            Self::Cut => (0, Vec::new()),
            Self::Episode(id) => (1, episode_bytes(id)),
            Self::EpisodeClaim(id) => (2, episode_bytes(id)),
            Self::Current(work) => (3, work_bytes(work)),
            Self::Authorization(lsn) => (4, lsn.0.to_be_bytes().to_vec()),
            Self::Claim(lsn) => (5, lsn.0.to_be_bytes().to_vec()),
            Self::Request(id) => (6, id.bytes().to_vec()),
            Self::Planned(turn) => (7, turn.to_bytes().to_vec()),
            Self::Witness(lsn) => (8, lsn.0.to_be_bytes().to_vec()),
        };
        let mut key = vec![tag];
        key.extend(value);
        key
    }
}

fn work_bytes(work: RecoveryWork) -> Vec<u8> {
    [
        work.agent.to_bytes(),
        work.goal.to_bytes(),
        work.turn.to_bytes(),
    ]
    .concat()
}
fn episode_bytes(id: RecoveryEpisodeId) -> Vec<u8> {
    let mut value = work_bytes(id.work);
    value.extend(id.origin.0.to_be_bytes());
    value
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodeIntent {
    id: RecoveryEpisodeId,
    barrier_lsn: Lsn,
    binding: RecoveryBinding,
    reason: RecoveryReason,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Cell {
    Cut {
        lsn: Lsn,
    },
    Episode {
        value: EpisodeIntent,
    },
    EpisodeClaim {
        episode: RecoveryEpisodeId,
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        authorization: Option<Lsn>,
    },
    Current {
        work: RecoveryWork,
        episode: RecoveryEpisodeId,
    },
    Authorization {
        lsn: Lsn,
        value: Box<RecoveryAuthorized>,
    },
    Claim {
        authorization: Lsn,
        value: RecoveryClaimState,
    },
    Request {
        request: Hex32,
        authorization: Lsn,
    },
    Planned {
        turn: TurnId,
        authorization: Lsn,
    },
    Witness {
        value: Box<RecoveryWitness>,
    },
}

impl Cell {
    fn key(&self) -> Key {
        match self {
            Self::Cut { .. } => Key::Cut,
            Self::Episode { value } => Key::Episode(value.id),
            Self::EpisodeClaim { episode, .. } => Key::EpisodeClaim(*episode),
            Self::Current { work, .. } => Key::Current(*work),
            Self::Authorization { lsn, .. } => Key::Authorization(*lsn),
            Self::Claim { authorization, .. } => Key::Claim(*authorization),
            Self::Request { request, .. } => Key::Request(*request),
            Self::Planned { turn, .. } => Key::Planned(*turn),
            Self::Witness { value } => Key::Witness(value.reference().lsn),
        }
    }

    fn validate(&self, through: Lsn) -> Result<(), RecoveryCacheError> {
        let prior = |lsn: Lsn| lsn != Lsn::ZERO && lsn <= through;
        let valid = match self {
            Self::Cut { lsn } => prior(*lsn),
            Self::Episode { value } => {
                valid_episode_intent(value)
                    && prior(value.id.origin)
                    && prior(value.barrier_lsn)
                    && value.id.origin <= value.barrier_lsn
            }
            Self::EpisodeClaim {
                episode,
                authorization,
            } => prior(episode.origin) && authorization.is_none_or(prior),
            Self::Current { work, episode } => *work == episode.work && prior(episode.origin),
            Self::Authorization { lsn, value } => {
                prior(*lsn) && value.episode.work == value.binding.work
            }
            Self::Claim { authorization, .. }
            | Self::Request { authorization, .. }
            | Self::Planned { authorization, .. } => prior(*authorization),
            Self::Witness { value } => {
                value.validate()?;
                prior(value.reference().lsn)
            }
        };
        if !valid {
            return Err(RecoveryCacheError::Identity);
        }
        Ok(())
    }
}

fn encoded(cell: &Cell) -> Result<Vec<u8>, RecoveryCacheError> {
    serde_json::to_vec(cell).map_err(|_| RecoveryCacheError::Encoding)
}

fn cells(
    state: &RecoveryState,
    history: &RecoveryHistory,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, RecoveryCacheError> {
    let mut rows = BTreeMap::new();
    let mut push = |cell: Cell| -> Result<(), RecoveryCacheError> {
        rows.insert(cell.key().bytes(), encoded(&cell)?);
        Ok(())
    };
    if state.last_applied != Lsn::ZERO {
        push(Cell::Cut {
            lsn: state.last_applied,
        })?;
    }
    for episode in state.episodes.values() {
        push(Cell::Episode {
            value: EpisodeIntent {
                id: episode.id,
                barrier_lsn: episode.barrier_lsn,
                binding: episode.binding.clone(),
                reason: episode.reason,
            },
        })?;
        push(Cell::EpisodeClaim {
            episode: episode.id,
            authorization: episode.authorization_lsn,
        })?;
    }
    for (work, episode) in &state.current {
        push(Cell::Current {
            work: *work,
            episode: *episode,
        })?;
    }
    for (lsn, value) in &state.authorizations {
        push(Cell::Authorization {
            lsn: *lsn,
            value: Box::new(value.clone()),
        })?;
    }
    for (authorization, value) in &state.claims {
        push(Cell::Claim {
            authorization: *authorization,
            value: value.clone(),
        })?;
    }
    for (request, authorization) in &state.requests {
        push(Cell::Request {
            request: *request,
            authorization: *authorization,
        })?;
    }
    for (turn, authorization) in &state.planned_turns {
        push(Cell::Planned {
            turn: *turn,
            authorization: *authorization,
        })?;
    }
    for value in history.witnesses.values() {
        push(Cell::Witness {
            value: Box::new(value.clone()),
        })?;
    }
    Ok(rows)
}

struct Loader<'a, R> {
    reader: &'a R,
    through: Lsn,
    state: RecoveryState,
    history: RecoveryHistory,
    before: BTreeMap<Vec<u8>, Vec<u8>>,
    loaded: BTreeMap<Key, Option<Cell>>,
    expanded_authorizations: BTreeSet<Lsn>,
    expanded_episodes: BTreeSet<RecoveryEpisodeId>,
}

impl<'a, R: RecoveryCacheReader> Loader<'a, R> {
    fn new(reader: &'a R, through: Lsn) -> Result<Self, R::Error> {
        let mut loader = Self {
            reader,
            through,
            state: RecoveryState::default(),
            history: RecoveryHistory::default(),
            before: BTreeMap::new(),
            loaded: BTreeMap::new(),
            expanded_authorizations: BTreeSet::new(),
            expanded_episodes: BTreeSet::new(),
        };
        loader.read(Key::Cut, false)?;
        Ok(loader)
    }

    fn finish(self) -> RecoveryCache {
        RecoveryCache {
            state: self.state,
            history: self.history,
            before: self.before,
        }
    }

    fn read(&mut self, key: Key, required: bool) -> Result<Option<Cell>, R::Error> {
        if let Some(value) = self.loaded.get(&key) {
            if required && value.is_none() {
                return Err(RecoveryCacheError::MissingCell.into());
            }
            return Ok(value.clone());
        }
        let value = self.reader.get(&key.bytes())?;
        if let Some(bytes) = value {
            return self.decode(&key.bytes(), &bytes).map(Some);
        }
        self.loaded.insert(key, None);
        if required {
            return Err(RecoveryCacheError::MissingCell.into());
        }
        Ok(None)
    }

    fn decode(&mut self, key: &[u8], bytes: &[u8]) -> Result<Cell, R::Error> {
        let cell: Cell = serde_json::from_slice(bytes).map_err(|_| RecoveryCacheError::Encoding)?;
        if cell.key().bytes() != key {
            return Err(RecoveryCacheError::Identity.into());
        }
        cell.validate(self.through)?;
        if !matches!(cell, Cell::Cut { .. } | Cell::Witness { .. })
            && self.state.last_applied == Lsn::ZERO
        {
            return Err(RecoveryCacheError::MissingCell.into());
        }
        let position = match &cell {
            Cell::Episode { value } => Some(value.barrier_lsn),
            Cell::EpisodeClaim {
                episode,
                authorization,
            } => Some(authorization.unwrap_or(episode.origin)),
            Cell::Current { episode, .. } => Some(episode.origin),
            Cell::Authorization { lsn, .. } => Some(*lsn),
            Cell::Claim { authorization, .. }
            | Cell::Request { authorization, .. }
            | Cell::Planned { authorization, .. } => Some(*authorization),
            Cell::Cut { .. } | Cell::Witness { .. } => None,
        };
        if position.is_some_and(|lsn| lsn > self.state.last_applied) {
            return Err(RecoveryCacheError::Position.into());
        }
        self.insert(&cell)?;
        self.before.insert(key.to_vec(), encoded(&cell)?);
        self.loaded.insert(cell.key(), Some(cell.clone()));
        Ok(cell)
    }

    fn insert(&mut self, cell: &Cell) -> Result<(), R::Error> {
        match cell {
            Cell::Cut { lsn } => self.state.last_applied = *lsn,
            Cell::Episode { value } => {
                self.state.episodes.insert(
                    value.id,
                    RecoveryEpisode {
                        id: value.id,
                        barrier_lsn: value.barrier_lsn,
                        binding: value.binding.clone(),
                        reason: value.reason,
                        authorization_lsn: None,
                    },
                );
            }
            Cell::EpisodeClaim {
                episode,
                authorization,
            } => {
                self.state
                    .episodes
                    .get_mut(episode)
                    .ok_or(RecoveryCacheError::MissingCell)?
                    .authorization_lsn = *authorization;
            }
            Cell::Current { work, episode } => {
                self.state.current.insert(*work, *episode);
            }
            Cell::Authorization { lsn, value } => {
                self.state
                    .authorizations
                    .insert(*lsn, value.as_ref().clone());
            }
            Cell::Claim {
                authorization,
                value,
            } => {
                self.state.claims.insert(*authorization, value.clone());
            }
            Cell::Request {
                request,
                authorization,
            } => {
                self.state.requests.insert(*request, *authorization);
            }
            Cell::Planned {
                turn,
                authorization,
            } => {
                self.state.planned_turns.insert(*turn, *authorization);
            }
            Cell::Witness { value } => self
                .history
                .insert(value.as_ref().clone())
                .map_err(RecoveryCacheError::from)?,
        }
        Ok(())
    }

    fn dependencies(&mut self, scope: Dependencies<'_>) -> Result<(), R::Error> {
        match scope {
            Dependencies::Required(value) => {
                if self.current(value.binding.work)?.is_none() {
                    self.planned(value.binding.work.turn, false)?;
                }
                if let Some(previous) = value.predecessor {
                    self.authorization(previous.authorization_lsn, true)?;
                    self.current(previous.episode.work)?;
                }
            }
            Dependencies::Authorized(value) => {
                self.request(value.request_id, false)?;
                self.current(value.episode.work)?;
                if let RecoveryPlan::New { turn, .. } = value.plan {
                    self.planned(turn, false)?;
                }
            }
            Dependencies::Claim(authorization) => {
                self.authorization(authorization, true)?;
            }
        }
        Ok(())
    }

    fn witness(&mut self, reference: RecoveryWitnessRef) -> Result<(), R::Error> {
        self.read(Key::Witness(reference.lsn), true)?;
        self.history
            .get(reference)
            .map_err(RecoveryCacheError::from)?;
        Ok(())
    }

    fn current(&mut self, work: RecoveryWork) -> Result<Option<RecoveryEpisodeId>, R::Error> {
        let Some(Cell::Current { episode, .. }) = self.read(Key::Current(work), false)? else {
            return Ok(None);
        };
        self.episode(episode, true)?;
        Ok(Some(episode))
    }

    fn episode(&mut self, id: RecoveryEpisodeId, follow_claim: bool) -> Result<(), R::Error> {
        self.read(Key::Episode(id), true)?;
        self.read(Key::EpisodeClaim(id), true)?;
        if follow_claim
            && self.expanded_episodes.insert(id)
            && let Some(authorization) = self.state.episodes[&id].authorization_lsn
        {
            self.authorization(authorization, true)?;
            if self.state.authorizations[&authorization].episode != id {
                return Err(RecoveryCacheError::Identity.into());
            }
        }
        Ok(())
    }

    fn authorization(&mut self, lsn: Lsn, required: bool) -> Result<(), R::Error> {
        let Some(Cell::Authorization { value, .. }) =
            self.read(Key::Authorization(lsn), required)?
        else {
            return Ok(());
        };
        if !self.expanded_authorizations.insert(lsn) {
            return Ok(());
        }
        self.episode(value.episode, false)?;
        self.read(Key::Claim(lsn), true)?;
        if self.state.episodes[&value.episode].authorization_lsn != Some(lsn)
            || self.state.episodes[&value.episode].binding != value.binding
        {
            return Err(RecoveryCacheError::Identity.into());
        }
        self.request(value.request_id, true)?;
        if self.state.requests[&value.request_id] != lsn {
            return Err(RecoveryCacheError::Identity.into());
        }
        if let RecoveryPlan::New { turn, .. } = value.plan {
            self.planned(turn, true)?;
            if self.state.planned_turns[&turn] != lsn {
                return Err(RecoveryCacheError::Identity.into());
            }
        }
        validate_authorization_intent(lsn, &value, &self.state.episodes[&value.episode])?;
        let claim = self.state.claims[&lsn].clone();
        validate_claim(lsn, &value, &claim, self.state.last_applied)?;
        if let RecoveryClaimState::Superseded {
            issue,
            successor,
            barrier_lsn,
        } = claim
        {
            self.episode(successor, false)?;
            let next = &self.state.episodes[&successor];
            let mut expected_binding = value.binding.clone();
            expected_binding.work = issue.issued.work;
            expected_binding.issue_lsn = issue.lsn;
            if next.barrier_lsn != barrier_lsn
                || next.binding != expected_binding
                || next.id.work != issue.issued.work
                || next.reason != RecoveryReason::MissingSpawn
            {
                return Err(RecoveryCacheError::Identity.into());
            }
        }
        Ok(())
    }

    fn request(&mut self, request: Hex32, required: bool) -> Result<(), R::Error> {
        let Some(Cell::Request { authorization, .. }) =
            self.read(Key::Request(request), required)?
        else {
            return Ok(());
        };
        self.authorization(authorization, true)?;
        if self.state.authorizations[&authorization].request_id != request {
            return Err(RecoveryCacheError::Identity.into());
        }
        Ok(())
    }

    fn planned(&mut self, turn: TurnId, required: bool) -> Result<(), R::Error> {
        let Some(Cell::Planned { authorization, .. }) = self.read(Key::Planned(turn), required)?
        else {
            return Ok(());
        };
        self.authorization(authorization, true)?;
        if !matches!(self.state.authorizations[&authorization].plan, RecoveryPlan::New { turn: original, .. } if original == turn)
        {
            return Err(RecoveryCacheError::Identity.into());
        }
        Ok(())
    }

    fn validate_all(&mut self) -> Result<(), R::Error> {
        if self.state.last_applied != Lsn::ZERO && self.state.episodes.is_empty() {
            return Err(RecoveryCacheError::MissingCell.into());
        }
        let mut latest = BTreeMap::new();
        for episode in self.state.episodes.values() {
            let current = latest
                .entry(episode.id.work)
                .or_insert((episode.barrier_lsn, episode.id));
            if episode.barrier_lsn > current.0 {
                *current = (episode.barrier_lsn, episode.id);
            }
        }
        for id in self.state.episodes.keys().copied().collect::<Vec<_>>() {
            self.episode(id, true)?;
            let current = self
                .current(id.work)?
                .ok_or(RecoveryCacheError::MissingCell)?;
            if current != latest[&id.work].1 {
                return Err(RecoveryCacheError::Identity.into());
            }
        }
        for work in self.state.current.keys().copied().collect::<Vec<_>>() {
            self.current(work)?;
        }
        for lsn in self
            .state
            .authorizations
            .keys()
            .chain(self.state.claims.keys())
            .copied()
            .collect::<Vec<_>>()
        {
            self.authorization(lsn, true)?;
        }
        for request in self.state.requests.keys().copied().collect::<Vec<_>>() {
            self.request(request, true)?;
        }
        for turn in self.state.planned_turns.keys().copied().collect::<Vec<_>>() {
            self.planned(turn, true)?;
        }
        Ok(())
    }
}

fn valid_episode_intent(value: &EpisodeIntent) -> bool {
    let binding = &value.binding;
    value.id.work == binding.work
        && binding.hired_lsn != Lsn::ZERO
        && binding.hired_lsn < binding.issue_lsn
        && binding.issue_lsn < value.id.origin
        && (if value.reason == RecoveryReason::HeldOrPausedFailure {
            value.id.origin < value.barrier_lsn
        } else {
            value.id.origin == value.barrier_lsn
        })
        && binding.ticket.is_none_or(|ticket| {
            ticket.goal == binding.work.goal
                && ticket.claimant == binding.work.agent
                && ticket.round != 0
        })
}

fn validate_authorization_intent(
    lsn: Lsn,
    value: &RecoveryAuthorized,
    episode: &RecoveryEpisode,
) -> Result<(), RecoveryCacheError> {
    let prior = |at: Lsn| at != Lsn::ZERO && at < lsn;
    if episode.barrier_lsn >= lsn
        || [
            value.expected_control,
            value.resulting_control,
            value.resume_lsn,
        ]
        .into_iter()
        .flatten()
        .any(|at| !prior(at))
    {
        return Err(RecoveryCacheError::Position);
    }
    let valid_control = match value.resume_lsn {
        Some(resume) => {
            value.resulting_control == Some(resume)
                && resume > episode.barrier_lsn
                && value.expected_control.is_none_or(|old| old < resume)
        }
        None => value.expected_control == value.resulting_control,
    };
    let valid_plan = match &value.plan {
        RecoveryPlan::New { turn, context } => {
            episode.reason != RecoveryReason::MissingSpawn
                && *turn != episode.id.work.turn
                && match context {
                    RecoveryContext::TryRecorded { .. } => {
                        episode.reason != RecoveryReason::UnconfirmedContext
                    }
                    RecoveryContext::Fresh { recap, .. } => {
                        recap.text.len() <= MAX_RECOVERY_RECAP_BYTES
                            && super::ordered_positions(&recap.sources)
                            && recap.sources.iter().all(|at| prior(*at))
                    }
                }
        }
        RecoveryPlan::RepeatIssued { issue, .. } => {
            episode.reason == RecoveryReason::MissingSpawn
                && issue.lsn == value.binding.issue_lsn
                && issue.issued.work == value.binding.work
                && issue.issued.model_selection == value.binding.model_selection
                && super::ordered_positions(&issue.issued.answers)
                && issue.issued.answers.iter().all(|at| *at < issue.lsn)
        }
    };
    if !valid_control || !valid_plan {
        return Err(RecoveryCacheError::Identity);
    }
    Ok(())
}

fn validate_claim(
    lsn: Lsn,
    authorization: &RecoveryAuthorized,
    claim: &RecoveryClaimState,
    through: Lsn,
) -> Result<(), RecoveryCacheError> {
    let after = |at: Lsn| at > lsn && at <= through;
    match claim {
        RecoveryClaimState::Pending => {
            if !matches!(authorization.plan, RecoveryPlan::New { .. }) {
                return Err(RecoveryCacheError::Identity);
            }
        }
        RecoveryClaimState::IssueBound(issue) => {
            validate_issue(lsn, authorization, issue, through)?;
        }
        RecoveryClaimState::Consumed {
            issue,
            evidence_lsn,
        } => {
            validate_issue(lsn, authorization, issue, through)?;
            if !after(*evidence_lsn) || *evidence_lsn <= issue.lsn {
                return Err(RecoveryCacheError::Position);
            }
        }
        RecoveryClaimState::Invalidated {
            invalidation_lsn,
            evidence,
        } => {
            if !matches!(authorization.plan, RecoveryPlan::New { .. }) || !after(*invalidation_lsn)
            {
                return Err(RecoveryCacheError::Identity);
            }
            match evidence {
                crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                    lsn: at, ..
                } if !after(*at) || *invalidation_lsn <= *at => {
                    return Err(RecoveryCacheError::Position);
                }
                crate::recovery::RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
                    session_digest,
                } if !matches!(authorization.attribution, crate::recovery::RecoveryAttribution::BrowserSession { session_digest: original } if original == *session_digest) =>
                {
                    return Err(RecoveryCacheError::Identity);
                }
                _ => {}
            }
        }
        RecoveryClaimState::Superseded {
            issue,
            successor,
            barrier_lsn,
        } => {
            validate_issue(lsn, authorization, issue, through)?;
            if !after(*barrier_lsn)
                || *barrier_lsn <= issue.lsn
                || successor.work != issue.issued.work
            {
                return Err(RecoveryCacheError::Identity);
            }
        }
    }
    Ok(())
}

fn validate_issue(
    authorization_lsn: Lsn,
    authorization: &RecoveryAuthorized,
    issue: &RecoveryIssue,
    through: Lsn,
) -> Result<(), RecoveryCacheError> {
    if issue.lsn == Lsn::ZERO || issue.lsn > through {
        return Err(RecoveryCacheError::Position);
    }
    let valid = match &authorization.plan {
        RecoveryPlan::New { turn, .. } => {
            issue.lsn > authorization_lsn
                && issue.issued.work.agent == authorization.episode.work.agent
                && issue.issued.work.goal == authorization.episode.work.goal
                && issue.issued.work.turn == *turn
                && issue.issued.model_selection == authorization.binding.model_selection
                && issue.issued.session.as_ref() == super::planned_session(&authorization.plan)
        }
        RecoveryPlan::RepeatIssued {
            issue: original, ..
        } => issue == original && issue.lsn < authorization_lsn,
    };
    if !valid
        || !super::ordered_positions(&issue.issued.answers)
        || issue
            .issued
            .answers
            .iter()
            .any(|answer| *answer >= issue.lsn)
    {
        return Err(RecoveryCacheError::Identity);
    }
    Ok(())
}
