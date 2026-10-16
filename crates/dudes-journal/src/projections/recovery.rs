//! Journal-owned recovery reads, certified against a canonical prefix before any absent lookup.

use std::sync::Arc;

use dudes_proto::record::Hex32;
use dudes_proto::recovery::{RecoveryAuthorized, RecoveryClaimRef};
use dudes_proto::recovery_state::RecoveryClaimState;
use dudes_proto::recovery_state::cache::{RecoveryCache, RecoveryCacheReader, RecoveryCacheRow};
use dudes_proto::{Entry, Record};
use dudes_types::Lsn;
use redb::{ReadableDatabase, ReadableTable};

use super::{
    LAST_APPLIED_CRC, LAST_APPLIED_LSN, META, PROJECTION_VERSION, Projections, RECOVERY,
    SCHEMA_VERSION,
};
use crate::error::{Error, RecoveryStoreError, Result};
use crate::reader::ValidatedPrefix;
use crate::writer::Writer;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct Certification {
    store: Arc<()>,
    writer: Arc<()>,
    origin: ValidatedPrefix,
    frontier: Lsn,
    cut: Lsn,
    crc: Option<u64>,
}

/// An owned historical receipt. Reading it never submits or releases work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReceipt {
    /// Exact original claim.
    pub claim: RecoveryClaimRef,
    /// Unchanged request and context intent.
    pub authorization: RecoveryAuthorized,
    /// Durable claim outcome at the checked cut.
    pub state: RecoveryClaimState,
}

/// Exclusive access to the actual writer and its canonically checked projection generation.
///
/// This library boundary is not wired into daemon commands yet. No recovery records are
/// registered by this module; ordinary-prefix checks cannot prove healthy recovery prefixes.
pub struct CheckedRecoveryStore<'a> {
    writer: &'a mut Writer,
    projections: &'a mut Projections,
}

impl<'a> CheckedRecoveryStore<'a> {
    pub(crate) fn open(writer: &'a mut Writer, projections: &'a mut Projections) -> Result<Self> {
        if let Err(error) = certain(writer) {
            projections.recovery_certification = None;
            return Err(error);
        }
        if projections.recovery_certification.is_some() {
            if let Err(error) = check(writer, projections) {
                projections.recovery_certification = None;
                return Err(error);
            }
        } else {
            let frontier = writer.next_lsn()?;
            let (origin, canonical) = writer.reader().canonical_recovery_projection()?;
            certain(writer)?;
            if frontier != writer.next_lsn()? {
                return refused(RecoveryStoreError::JournalAdvanced);
            }
            if authority_rows(projections)? != authority_rows(&canonical)? {
                return refused(RecoveryStoreError::CanonicalMismatch);
            }
            projections.recovery_certification = Some(Certification {
                store: Arc::clone(&projections.recovery_generation),
                writer: Arc::clone(&writer.recovery_generation),
                origin,
                frontier,
                cut: projections.last_applied,
                crc: projections.last_applied_crc,
            });
        }
        Ok(Self {
            writer,
            projections,
        })
    }

    /// Look up an original request at a certain journal cut, without appending or retrying it.
    pub fn receipt(&self, request: Hex32, digest: Hex32) -> Result<Option<RecoveryReceipt>> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        let table = txn.open_table(RECOVERY)?;
        let cache = RecoveryCache::load_receipt(
            &CacheReader(&table),
            self.projections.last_applied,
            request,
        )?;
        Ok(cache
            .state()
            .receipt(request, digest)?
            .map(|receipt| RecoveryReceipt {
                claim: receipt.claim,
                authorization: receipt.authorization.clone(),
                state: receipt.state.clone(),
            }))
    }

    /// Validate projection folds, durably append that same entry, and commit its rows together.
    ///
    /// A failure after append may leave the entry durable. It fences this writer and requires
    /// reopening/reconstruction before another checked receipt can be read. Existing daemon
    /// authority checks remain required when this boundary is wired into the append path.
    pub fn append(&mut self, entry: &Entry) -> Result<Lsn> {
        check(self.writer, self.projections)?;
        self.projections.validate_next_instruction(entry)?;
        let prepared = self.writer.prepare_append(entry, false)?;
        let lsn = prepared.lsn();
        let crc = prepared.crc();
        let mut txn = self.projections.db.begin_write()?;
        txn.set_durability(self.projections.durability)?;
        check_metadata_table(self.projections, &txn.open_table(META)?)?;
        let certification = self
            .projections
            .recovery_certification
            .take()
            .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
        let first = super::fold_batch(
            &txn,
            self.projections.last_applied,
            [(prepared.lsn(), prepared.crc(), prepared.entry())],
        )?;
        if first.applied != 1 {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        let written = self.writer.commit_prepared(prepared);
        let (written_lsn, written_crc) = match written {
            Ok(value) => value,
            Err(error) => {
                if self.writer.poisoned().is_some()
                    || self.writer.next_lsn()? != certification.frontier
                {
                    self.writer.recovery_fenced = true;
                }
                return Err(error);
            }
        };
        let result = (|| {
            certain(self.writer)?;
            if (written_lsn, written_crc) != (lsn, crc) {
                return refused(RecoveryStoreError::AppendMismatch);
            }
            let frontier = self.writer.next_lsn()?;
            let frames = written_interval(self.writer, lsn, crc, entry, frontier)?;
            let last = if frames.len() == 2 {
                let (at, witness, checkpoint) = &frames[1];
                super::fold_batch(&txn, first.last, [(*at, *witness, checkpoint)])?
            } else {
                first
            };
            txn.commit()?;
            self.projections.publish_batch(&last);
            self.projections.recovery_certification = Some(Certification {
                store: certification.store,
                writer: certification.writer,
                origin: certification.origin,
                frontier,
                cut: self.projections.last_applied,
                crc: self.projections.last_applied_crc,
            });
            Ok(lsn)
        })();
        if result.is_err() {
            self.writer.recovery_fenced = true;
        }
        result
    }
}

fn refused<T>(reason: RecoveryStoreError) -> Result<T> {
    Err(Error::RecoveryStore(reason))
}

fn certain(writer: &Writer) -> Result<()> {
    if writer.poisoned().is_some() || writer.recovery_fenced {
        return refused(RecoveryStoreError::JournalUncertain);
    }
    Ok(())
}

fn check(writer: &Writer, projections: &Projections) -> Result<()> {
    certain(writer)?;
    let certification = projections
        .recovery_certification
        .as_ref()
        .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
    if !Arc::ptr_eq(&certification.store, &projections.recovery_generation)
        || !Arc::ptr_eq(&certification.writer, &writer.recovery_generation)
    {
        return refused(RecoveryStoreError::Uncertified);
    }
    if writer.next_lsn()? != certification.frontier {
        return refused(RecoveryStoreError::JournalAdvanced);
    }
    let txn = projections.db.begin_read()?;
    check_metadata(projections, &txn)
}

fn check_metadata(projections: &Projections, txn: &redb::ReadTransaction) -> Result<()> {
    check_metadata_table(projections, &txn.open_table(META)?)
}

fn check_metadata_table(
    projections: &Projections,
    meta: &impl ReadableTable<&'static str, u64>,
) -> Result<()> {
    let certification = projections
        .recovery_certification
        .as_ref()
        .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
    if meta.get(SCHEMA_VERSION)?.map(|value| value.value()) != Some(PROJECTION_VERSION)
        || meta.get(LAST_APPLIED_LSN)?.map(|value| value.value()) != Some(certification.cut.0)
        || meta.get(LAST_APPLIED_CRC)?.map(|value| value.value()) != certification.crc
        || projections.last_applied != certification.cut
        || projections.last_applied_crc != certification.crc
    {
        return refused(RecoveryStoreError::Metadata);
    }
    Ok(())
}

fn written_interval(
    writer: &Writer,
    lsn: Lsn,
    crc: u32,
    entry: &Entry,
    end: Lsn,
) -> Result<Vec<(Lsn, u32, Entry)>> {
    let mut frames = Vec::with_capacity(2);
    for frame in writer.reader().witnessed_from(lsn)? {
        let frame = frame?;
        if frame.0 >= end {
            break;
        }
        if frames.len() == 2 {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        frames.push(frame);
    }
    if frames
        .first()
        .is_none_or(|(at, witness, recorded)| *at != lsn || *witness != crc || recorded != entry)
        || frames
            .get(1)
            .is_some_and(|(_, _, recorded)| !matches!(recorded.record, Record::Checkpoint { .. }))
    {
        return refused(RecoveryStoreError::AppendMismatch);
    }
    Ok(frames)
}

struct CacheReader<'a, T>(&'a T);

impl<T: ReadableTable<&'static [u8], &'static [u8]>> RecoveryCacheReader for CacheReader<'_, T> {
    type Error = Error;
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|value| value.value().to_vec()))
    }
    fn prefix(&self, prefix: &[u8]) -> Result<Vec<RecoveryCacheRow>> {
        let mut rows = Vec::new();
        for row in self.0.range(prefix..)? {
            let (key, value) = row?;
            if !key.value().starts_with(prefix) {
                break;
            }
            rows.push((key.value().to_vec(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

pub(super) fn observe(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    through: Lsn,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let mut cache = RecoveryCache::load_history(&CacheReader(table), through, lsn)?;
    cache
        .history_mut()
        .observe(lsn, entry, super::instructions::hash)
        .map_err(dudes_proto::recovery_state::cache::RecoveryCacheError::from)?;
    for update in cache.updates()? {
        match update.value {
            Some(value) => {
                table.insert(update.key.as_slice(), value.as_slice())?;
            }
            None => {
                table.remove(update.key.as_slice())?;
            }
        }
    }
    Ok(())
}

fn authority_rows(projections: &Projections) -> Result<Vec<String>> {
    let txn = projections.db.begin_read()?;
    let meta = txn.open_table(META)?;
    let schema = meta.get(SCHEMA_VERSION)?.map(|value| value.value());
    let cut = meta.get(LAST_APPLIED_LSN)?.map(|value| value.value());
    let crc = meta.get(LAST_APPLIED_CRC)?.map(|value| value.value());
    if schema != Some(PROJECTION_VERSION)
        || cut != Some(projections.last_applied.0)
        || crc != projections.last_applied_crc
    {
        return refused(RecoveryStoreError::Metadata);
    }
    super::instructions::read(
        &txn.open_table(super::INSTRUCTIONS)?,
        projections.last_applied,
    )?;
    RecoveryCache::read_all(
        &CacheReader(&txn.open_table(RECOVERY)?),
        projections.last_applied,
    )?;
    let mut rows = vec![
        format!("schema {PROJECTION_VERSION}"),
        projections.export_at(&txn)?,
    ];
    // The public export deliberately presents card views; admission also needs the full row.
    super::export_rows::<16, super::board::BoardRow>(
        &mut rows,
        "board_authority",
        &txn.open_table(super::BOARD)?,
    )?;
    Ok(rows)
}

pub(super) fn export_cells(
    rows: &mut Vec<String>,
    name: &str,
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<()> {
    for row in table.iter()? {
        let (key, value) = row?;
        let value: serde_json::Value =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&value).map_err(|source| Error::Row { source })?;
        rows.push(format!("{name} {} {json}", super::hex(key.value())));
    }
    Ok(())
}
