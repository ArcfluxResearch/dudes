//! The one writer (invariant 1). `append` returns only after the bytes
//! are synced, which is what lets invariant 2 ("persist before spawn") rest on the return value
//! alone.
//!
//! An append that fails after its write began poisons the writer ([`Error::Poisoned`]): the
//! segment may hold a partial frame, and a frame appended after it would be unreachable at
//! replay while its lsn claimed otherwise. Reopening the journal repairs the tail.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dudes_proto::rpc::MAX_FRAME_BYTES;
use dudes_proto::rpc::journal::journal_appended;
use dudes_proto::{Entry, Record};
use dudes_types::{Lsn, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::frame::{self, MAX_PAYLOAD};
use crate::projections::PROJECTION_VERSION;
use crate::segment::{self, lsn_at, segment_path, would_cross};

/// [`PROJECTION_VERSION`] in the width [`Record::Checkpoint`] carries it.
///
/// The constant is a `u64` and the field is a `u32`. Narrowing it here, in a `const` block,
/// puts the check at build time: a `PROJECTION_VERSION` past `u32::MAX` stops the compile
/// instead of stamping a wrapped tag onto every checkpoint, and [`Writer::append`] — invariant
/// 2's persist-before-spawn path — gains no failure branch for a conversion that cannot fail.
const CHECKPOINT_PROJECTION_VERSION: u32 = {
    assert!(
        PROJECTION_VERSION <= 0xFFFF_FFFF,
        "PROJECTION_VERSION no longer fits the u32 `Record::Checkpoint` carries"
    );
    #[allow(clippy::cast_possible_truncation)] // proved by the assert above, at compile time
    let narrowed = PROJECTION_VERSION as u32;
    narrowed
};

/// What `append` waits for before it returns (`[daemon] fsync`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fsync {
    /// `File::sync_all`: data and metadata. On Apple targets std issues `fcntl(F_FULLFSYNC)`,
    /// so the bytes reach the platter, not the drive cache. The default.
    #[default]
    Full,
    /// `File::sync_data`: `fdatasync` where the platform has it; no weaker than `Full` on
    /// Apple targets, where std uses `F_FULLFSYNC` for both.
    Fdatasync,
    /// No sync. Dev configuration only: a record whose append returned may still be lost, so
    /// invariant 2 does not hold. The daemon's config loader refuses it outside dev.
    None,
}

/// What `open` needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalOptions {
    /// The sync policy.
    pub fsync: Fsync,
    /// Rotate before a frame that would end past this many bytes.
    pub rotate_at: u64,
    /// Emit a [`Record::Checkpoint`] after every this-many appends;
    /// `0` never emits one. Counted over [`Writer::append`] calls, not over frames — a
    /// checkpoint does not count towards the next one.
    pub checkpoint_every: u64,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            fsync: Fsync::Full,
            rotate_at: segment::DEFAULT_ROTATE_AT,
            checkpoint_every: 10_000,
        }
    }
}

/// Appends records to the current segment. Single-threaded by construction (`&mut self`);
/// owned by the daemon's writer task. Only ever appends to a segment it created in this
/// process or found empty on open, so a rotated segment is never modified.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    file: File,
    segment: u32,
    len: u64,
    options: JournalOptions,
    sync_count: u64,
    /// Appends since the last checkpoint was emitted; see [`Writer::emit_checkpoint`].
    since_checkpoint: u64,
    buf: Vec<u8>,
    /// The lsn of the append that failed mid-write, once one has.
    poisoned: Option<Lsn>,
    required_started: bool,
    pub(crate) recovery_generation: Arc<()>,
    pub(crate) recovery_fenced: bool,
    pub(crate) validated: Option<crate::reader::ValidatedPrefix>,
    pub(crate) validation_scratch: Option<PathBuf>,
    #[cfg(any(test, debug_assertions))]
    fault: Option<Fault>,
    /// Writes to let through before [`Writer::fault`] fires.
    #[cfg(any(test, debug_assertions))]
    skip_writes: usize,
}

/// A failure injected into the next append, at the point where the real one would occur.
/// Test-only: the seam is the two calls `append` makes after the frame is positioned, and
/// this stands in for the `File` returning an error from them.
#[cfg(any(test, debug_assertions))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// `write_all` fails after this many bytes of the frame reached the file.
    WriteShort(usize),
    /// The write completes, the sync fails.
    #[cfg(test)]
    Sync,
}

pub(crate) struct PreparedAppend<'a> {
    entry: &'a Entry,
    bytes: Vec<u8>,
    frontier: Lsn,
    lsn: Lsn,
    rotates: bool,
    required: bool,
}

impl PreparedAppend<'_> {
    pub(crate) const fn entry(&self) -> &Entry {
        self.entry
    }
    pub(crate) const fn lsn(&self) -> Lsn {
        self.lsn
    }
    pub(crate) fn crc(&self) -> u32 {
        let header: [u8; frame::HEADER_LEN] = self.bytes[..frame::HEADER_LEN]
            .try_into()
            .expect("stage writes the fixed header before the payload");
        frame::Header::parse(&header).crc
    }
}

impl Writer {
    /// Compare the owned projection with the actual journal before allowing recovery reads.
    ///
    /// The returned borrow excludes raw journal/projection mutation. Dropping it does not let a
    /// later raw append retain a truthful receipt: the next checked read verifies the writer cut.
    pub fn checked_recovery<'a>(
        &'a mut self,
        projections: &'a mut crate::Projections,
    ) -> Result<crate::projections::recovery::CheckedRecoveryStore<'a>> {
        crate::projections::recovery::CheckedRecoveryStore::open(self, projections)
    }

    /// A live reader that can reuse completed-contract validation from this writer's open.
    /// Replay checks the prefix's bytes before reuse and validates any changed journal again.
    #[must_use]
    pub fn reader(&self) -> crate::Reader {
        let mut reader = crate::Reader::new(&self.dir);
        reader.validated.clone_from(&self.validated);
        reader
            .validation_scratch
            .clone_from(&self.validation_scratch);
        reader
    }

    /// Start appending to `segment`, which is either a new file (created here, directory
    /// synced) or an existing empty one.
    pub(crate) fn start(dir: &Path, segment: u32, options: JournalOptions) -> Result<Self> {
        let path = segment_path(dir, segment);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        if len != 0 {
            // `open` only ever hands this function a fresh or empty segment.
            return Err(Error::SegmentNotEmpty { segment, len });
        }
        segment::sync_dir(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            file,
            segment,
            len: 0,
            options,
            sync_count: 0,
            since_checkpoint: 0,
            buf: Vec::new(),
            poisoned: None,
            required_started: false,
            recovery_generation: Arc::new(()),
            recovery_fenced: false,
            validated: None,
            validation_scratch: None,
            #[cfg(any(test, debug_assertions))]
            fault: None,
            #[cfg(any(test, debug_assertions))]
            skip_writes: 0,
        })
    }

    /// Append one entry — a record stamped with the time it entered the machine. Returns its lsn only after the frame is written and synced per [`Fsync`];
    /// until then the record does not exist. After a failure past the start of the
    /// write, this and every later call return [`Error::Poisoned`].
    ///
    /// The payload is the whole [`Entry`], so `at` is flattened beside `t`; the frame's `kind`
    /// tag still comes from the record. Public append paths take `Entry` and commit the same
    /// staged frame path. The writer has one additional producer: every
    /// [`JournalOptions::checkpoint_every`] appends this also writes a
    /// [`Record::Checkpoint`], after the caller's frame and before returning. See
    /// [`Writer::emit_checkpoint`] for what that record does and does not attest.
    pub fn append(&mut self, entry: &Entry) -> Result<Lsn> {
        self.append_witnessed(entry).map(|(lsn, _)| lsn)
    }

    /// [`Writer::append`], also returning the frame's `crc32c` — the witness the projection
    /// stores beside `last_applied_lsn`.
    ///
    /// The caller that folds a record it has just written needs the same checksum a reader
    /// would decode at that lsn, and only this function is in a position to hand it over
    /// without a second read of the file. It is not recomputed: the value returned is parsed
    /// back out of the header [`frame::encode`] put in the buffer that went to the file, by
    /// the same [`frame::Header::parse`] a reader uses. Agreement between the two paths is
    /// then a construction rather than a coincidence a test has to keep watching — and a
    /// second CRC pass over up to [`MAX_PAYLOAD`] bytes is not spent on invariant 2's
    /// persist-before-spawn path.
    pub fn append_witnessed(&mut self, entry: &Entry) -> Result<(Lsn, u32)> {
        self.append_framed(entry, false)
    }

    /// Append an entry in the required-record format, fsynced before returning its position.
    ///
    /// The first required append in this writer starts at offset zero of a fresh segment so an
    /// older recovery search cannot hide it behind a damaged compatible prefix.
    pub fn append_required(&mut self, entry: &Entry) -> Result<Lsn> {
        self.append_required_witnessed(entry).map(|(lsn, _)| lsn)
    }

    /// [`Writer::append_required`], also returning the checksum of the committed frame.
    pub fn append_required_witnessed(&mut self, entry: &Entry) -> Result<(Lsn, u32)> {
        self.append_framed(entry, true)
    }

    fn append_framed(&mut self, entry: &Entry, required: bool) -> Result<(Lsn, u32)> {
        let prepared = self.prepare_append(entry, required)?;
        self.commit_prepared(prepared)
    }

    pub(crate) fn prepare_append<'a>(
        &self,
        entry: &'a Entry,
        required: bool,
    ) -> Result<PreparedAppend<'a>> {
        if self.recovery_fenced {
            return Err(Error::RecoveryStore(
                crate::error::RecoveryStoreError::JournalUncertain,
            ));
        }
        if let Some(lsn) = self.poisoned {
            return Err(Error::Poisoned { lsn });
        }
        let required = required || entry.record.requires_required_frame();
        let staged = Self::stage(entry, required)?;
        let frame_len = staged.len();
        let rotates = would_cross(self.len, frame_len as u64, self.options.rotate_at)
            || (required && !self.required_started && self.len != 0);
        let candidate_lsn = if rotates {
            lsn_at(self.segment + 1, 0)?
        } else {
            lsn_at(self.segment, self.len)?
        };
        let notification_len = serde_json::to_vec(&journal_appended(candidate_lsn, entry))
            .map_err(|source| Error::Encode { source })?
            .len()
            + 1;
        if notification_len > MAX_FRAME_BYTES {
            return Err(Error::NotificationTooLarge {
                len: notification_len,
                max: MAX_FRAME_BYTES,
            });
        }

        Ok(PreparedAppend {
            entry,
            bytes: staged,
            frontier: self.next_lsn()?,
            lsn: candidate_lsn,
            rotates,
            required,
        })
    }

    pub(crate) fn commit_prepared(&mut self, prepared: PreparedAppend<'_>) -> Result<(Lsn, u32)> {
        if let Some(lsn) = self.poisoned {
            return Err(Error::Poisoned { lsn });
        }
        if self.next_lsn()? != prepared.frontier {
            return Err(Error::RecoveryStore(
                crate::error::RecoveryStoreError::JournalAdvanced,
            ));
        }
        let PreparedAppend {
            entry,
            bytes: staged,
            lsn: candidate_lsn,
            rotates,
            required,
            ..
        } = prepared;
        let frame_len = staged.len();
        self.buf = staged;
        if rotates {
            self.rotate()?;
        }
        let lsn = self.commit_staged(frame_len)?;
        self.required_started |= required;
        debug_assert_eq!(lsn, candidate_lsn, "preflight and commit use one lsn");
        // The witness is read back out of the bytes that were written, never hashed a second
        // time. `rotate` and `write_and_sync` do not touch `buf`, so these are the header
        // `stage` produced above and `write_all` sent to the file.
        let header: [u8; frame::HEADER_LEN] = self.buf[..frame::HEADER_LEN]
            .try_into()
            .expect("`encode` writes the fixed header before the payload");
        let crc = frame::Header::parse(&header).crc;
        self.since_checkpoint += 1;
        if self.options.checkpoint_every != 0
            && self.since_checkpoint >= self.options.checkpoint_every
        {
            self.since_checkpoint = 0;
            // A checkpoint that fails **never un-acknowledges the record above**: its bytes are
            // synced, and invariant 2 rests on this return value. An `Err` here would tell the
            // daemon a record was not persisted that the log already holds, and it would issue
            // it again.
            //
            // What that costs is different on each of the two arms `emit_checkpoint` can fail
            // on, so neither is described by the other:
            //
            // - **`commit_staged`** — the write or the sync failed past the start of the frame.
            //   The writer is poisoned at the checkpoint's own lsn, so the failure is reported:
            //   `poisoned()` returns it and the next `append` is `Error::Poisoned`.
            // - **`rotate`** — the next segment could not be created, so nothing was written and
            //   nothing is poisoned. The writer stays coherent (`rotate` mutates no field until
            //   both of its fallible calls have succeeded) and appends carry on, but this
            //   checkpoint is lost and `since_checkpoint` has already been reset, so the cadence
            //   slips by one interval. Replay does not start from checkpoints, so the slip does
            //   not change recovery today. It is still the writer's durability path, and an
            //   invisible error there is worse than the slip.
            //
            // Hence the log line rather than a discarded `Result`: it is the only report the
            // second arm has.
            if let Err(err) = self.emit_checkpoint(entry.at) {
                tracing::warn!(
                    target: "journal",
                    lsn = %lsn,
                    segment = self.segment,
                    error = %err,
                    "journal.checkpoint_failed"
                );
            }
        }
        Ok((lsn, crc))
    }

    /// Append a [`Record::Checkpoint`] naming **its own frame's** position,
    /// stamped with `at` — the `at` of the entry whose append triggered it, because this crate
    /// reads no clock (see `lib.rs`).
    ///
    /// Replay does not start from a `Checkpoint`; it is applied as a no-op. The checked append
    /// transport validates its physical tail without treating it as a recovery starting point.
    /// `dudes-supervisor`'s `world.rs` folds it
    /// in an arm that names the record; in this crate's four projection modules it reaches a
    /// wildcard — `agents::apply` and `agents::apply_flow`, `goals::apply`, `turns::apply` name
    /// it beside their wildcards, and `budget::apply` matches no record at all, returning before
    /// any record arm unless `turns::apply` handed it an ended turn.
    ///
    /// This is not checkpoint-based recovery: the record attests that the writer reached this position under this `PROJECTION_VERSION`,
    /// and **nothing about what any projection has applied**. Starting a replay from one, and
    /// archiving the segments before one, are both the follow-up: replay starting from the
    /// latest usable `Checkpoint` needs the `crc32c` that `Record::Checkpoint` is to gain,
    /// because `lsn` locates a record and the frame's `crc32c`
    /// says whether the record there differs from the one applied, and neither alone identifies
    /// it.
    ///
    /// **The pin, and what the change that lands a consumer does with it.** `tests/checkpoint.rs`
    /// asserts that checkpointing changes no projection, which is true only while every fold is
    /// inert. That proposition is false by design when checkpoint-based recovery exists, so the
    /// pin has no true form to be updated into: **the implementing change deletes it** and puts the
    /// consumer's own properties in its place. It is not repaired, and it is not to be weakened
    /// until it passes.
    ///
    /// The position it names is the position it is written at, so its own encoded length
    /// decides where it goes: encode against the position it would take, and if that frame
    /// would cross `rotate_at`, rotate and encode again against the segment it is actually
    /// going into. At most one repeat — [`Writer::rotate`] leaves `len` at `0`, and
    /// `would_cross(0, ..)` is false for a frame of any size.
    fn emit_checkpoint(&mut self, at: Timestamp) -> Result<()> {
        loop {
            let entry = Entry {
                at,
                record: Record::Checkpoint {
                    projection_version: CHECKPOINT_PROJECTION_VERSION,
                    segment: self.segment,
                    offset: self.len,
                },
            };
            let staged = Self::stage(&entry, false)?;
            let frame_len = staged.len();
            if would_cross(self.len, frame_len as u64, self.options.rotate_at) {
                self.rotate()?;
                continue;
            }
            self.buf = staged;
            self.commit_staged(frame_len)?;
            return Ok(());
        }
    }

    /// Encode `entry` into a local whole frame, ready to be admitted and written. Touches no
    /// writer state or file, so an oversized payload or delivery notification is refused before
    /// mutation begins.
    ///
    /// Separate from [`Writer::commit_staged`] because the record whose bytes these are is not
    /// always the caller's: a `Checkpoint` names the position it is about to occupy, so the
    /// emitter has to encode, ask whether that position survives the rotation check, and encode
    /// again when it does not.
    fn stage(entry: &Entry, required: bool) -> Result<Vec<u8>> {
        dudes_proto::recovery::validate_record_shape(&entry.record).map_err(|reason| {
            Error::RecoveryAdmission {
                kind: entry.record.kind(),
                reason,
            }
        })?;
        dudes_proto::validate_instruction_record(&entry.record).map_err(|reason| {
            Error::Instruction {
                lsn: Lsn::ZERO,
                reason: reason.into(),
            }
        })?;
        let payload = serde_json::to_vec(entry).map_err(|source| Error::Encode { source })?;
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge {
                len: payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        let mut staged = Vec::with_capacity(frame::HEADER_LEN + payload.len());
        let encoded = if required {
            frame::encode_required(entry.record.kind(), &payload, &mut staged)
        } else {
            frame::encode(entry.record.kind().tag(), &payload, &mut staged)
        };
        encoded.map_err(|_| Error::PayloadTooLarge {
            len: payload.len(),
            max: MAX_PAYLOAD,
        })?;
        Ok(staged)
    }

    /// Write the frame [`Writer::stage`] left in `self.buf` at the current position and advance
    /// past it; returns the lsn it took. A failure past the start of the write poisons the
    /// writer at that lsn, because the segment may hold a partial frame.
    ///
    /// The caller decides rotation before calling this: `self.segment` and `self.len` are read
    /// as they stand.
    fn commit_staged(&mut self, frame_len: usize) -> Result<Lsn> {
        let lsn = lsn_at(self.segment, self.len)?;
        if let Err(e) = self.write_and_sync() {
            self.poisoned = Some(lsn);
            return Err(e);
        }
        self.len += frame_len as u64;
        Ok(lsn)
    }

    /// The write itself: bytes, then the sync policy. A failure anywhere here leaves the
    /// segment in an unknown state, which is why `append` poisons on it.
    fn write_and_sync(&mut self) -> Result<()> {
        #[cfg(any(test, debug_assertions))]
        if self.fault.is_some() {
            if self.skip_writes > 0 {
                self.skip_writes -= 1;
            } else if let Some(fault) = self.fault.take() {
                return self.inject(fault);
            }
        }
        self.file.write_all(&self.buf)?;
        self.sync()
    }

    /// Stand in for the `File` failing inside [`Writer::write_and_sync`].
    #[cfg(any(test, debug_assertions))]
    fn inject(&mut self, fault: Fault) -> Result<()> {
        let partial = match fault {
            Fault::WriteShort(n) => n.min(self.buf.len()),
            #[cfg(test)]
            Fault::Sync => self.buf.len(),
        };
        self.file.write_all(&self.buf[..partial])?;
        Err(Error::Io(std::io::Error::other(format!(
            "injected {fault:?}"
        ))))
    }

    /// Make the next write fail at `fault` — the caller's own frame.
    #[cfg(test)]
    pub(crate) fn fail_next(&mut self, fault: Fault) {
        self.fault = Some(fault);
        self.skip_writes = 0;
    }

    /// Make the write *after* the next one fail at `fault`. An append that emits a checkpoint
    /// writes twice, so this is how the **checkpoint's** write is the one that fails while the
    /// caller's frame goes down intact.
    #[cfg(test)]
    pub(crate) fn fail_second_write(&mut self, fault: Fault) {
        self.fault = Some(fault);
        self.skip_writes = 1;
    }

    /// Inject a short write at the existing append boundary in a debug test build.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn fail_write_after(&mut self, frames_to_skip: usize, bytes: usize) {
        self.fault = Some(Fault::WriteShort(bytes));
        self.skip_writes = frames_to_skip;
    }

    /// The lsn of the append that poisoned this writer, if one has.
    #[must_use]
    pub fn poisoned(&self) -> Option<Lsn> {
        self.poisoned
    }

    /// The segment being appended to.
    #[must_use]
    pub fn segment(&self) -> u32 {
        self.segment
    }

    /// The lsn the next append will get.
    pub fn next_lsn(&self) -> Result<Lsn> {
        lsn_at(self.segment, self.len)
    }

    /// How many times `append` has synced. `Full`/`Fdatasync`: one per append, plus one for
    /// each [`Record::Checkpoint`] it emitted; `None`: zero. A test probe, not part of the
    /// contract.
    #[doc(hidden)]
    #[must_use]
    pub fn sync_count(&self) -> u64 {
        self.sync_count
    }

    /// The one place the sync policy is applied; `append` returns only after this does.
    fn sync(&mut self) -> Result<()> {
        match self.options.fsync {
            Fsync::Full => self.file.sync_all()?,
            Fsync::Fdatasync => self.file.sync_data()?,
            Fsync::None => return Ok(()),
        }
        self.sync_count += 1;
        Ok(())
    }

    /// Open the next segment; the current one is complete and never touched again.
    fn rotate(&mut self) -> Result<()> {
        let next = self.segment + 1;
        let path = segment_path(&self.dir, next);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        segment::sync_dir(&self.dir)?;
        self.file = file;
        self.segment = next;
        self.len = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dudes_proto::rpc::MAX_FRAME_BYTES;
    use dudes_proto::rpc::journal::journal_appended;
    use dudes_proto::{Record, RecordKind};
    use dudes_types::{AgentId, Timestamp};

    use super::*;
    use crate::reader::scan;

    /// A stamp far enough from the epoch to have the width a real one has.
    const AT_BASE: i64 = 1_787_394_600_000;

    /// One entry per `i`, with a *distinct* `at`: a writer that dropped `at` or stamped a
    /// constant would still round-trip if every sample shared one.
    fn record(i: u64) -> Entry {
        Entry {
            at: Timestamp::from_unix_millis(AT_BASE + i64::try_from(i).unwrap()),
            record: Record::MemoryAppend {
                agent: AgentId::from_parts(i, 7),
                text: "x".repeat(200),
                body: None,
                provenance: None,
            },
        }
    }

    /// A `MemoryAppend` whose complete typed `journal.appended` frame, newline included, is
    /// exactly `bytes` long at `lsn`. Its text is unescaped ASCII, so each added byte moves the
    /// frame boundary by exactly one; the assertion keeps that fixture arithmetic honest.
    fn record_with_notification_len(lsn: Lsn, bytes: usize) -> Entry {
        let mut entry = Entry {
            at: Timestamp::from_unix_millis(AT_BASE),
            record: Record::MemoryAppend {
                agent: AgentId::NIL,
                text: String::new(),
                body: None,
                provenance: None,
            },
        };
        let overhead = serde_json::to_vec(&journal_appended(lsn, &entry))
            .unwrap()
            .len()
            + 1;
        let Record::MemoryAppend { text, .. } = &mut entry.record else {
            unreachable!("the fixture constructs MemoryAppend")
        };
        text.push_str(&"x".repeat(bytes - overhead));
        assert_eq!(
            serde_json::to_vec(&journal_appended(lsn, &entry))
                .unwrap()
                .len()
                + 1,
            bytes,
            "the fixture's complete notification arithmetic"
        );
        entry
    }

    /// A writer that never checkpoints: every test that predates the emission asserts about
    /// frames the caller wrote, and a checkpoint among them would be a second thing to explain.
    /// The tests that *are* about checkpoints use [`checkpointing`].
    fn writer(dir: &Path, fsync: Fsync, rotate_at: u64) -> Writer {
        Writer::start(
            dir,
            1,
            JournalOptions {
                fsync,
                rotate_at,
                checkpoint_every: 0,
            },
        )
        .unwrap()
    }

    /// A writer that emits a checkpoint every `checkpoint_every` appends.
    fn checkpointing(dir: &Path, checkpoint_every: u64, rotate_at: u64) -> Writer {
        checkpointing_with(dir, Fsync::None, checkpoint_every, rotate_at)
    }

    /// [`checkpointing`] under a named sync policy, for the one test that is about the policy.
    fn checkpointing_with(
        dir: &Path,
        fsync: Fsync,
        checkpoint_every: u64,
        rotate_at: u64,
    ) -> Writer {
        Writer::start(
            dir,
            1,
            JournalOptions {
                fsync,
                rotate_at,
                checkpoint_every,
            },
        )
        .unwrap()
    }

    /// Invariant 2: `append` syncs once per record under `Full` and `Fdatasync`,
    /// and the sync happens inside `append`, before the lsn is returned — `sync_count` equals
    /// the number of appends at every point. `None` never syncs.
    #[test]
    fn append_syncs_before_returning() {
        for (fsync, per_append) in [(Fsync::Full, 1), (Fsync::Fdatasync, 1), (Fsync::None, 0)] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = writer(dir.path(), fsync, segment::DEFAULT_ROTATE_AT);
            for i in 1..=5 {
                w.append(&record(i)).unwrap();
                assert_eq!(w.sync_count(), i * per_append, "{fsync:?}");
            }
        }
    }

    /// The other way `emit_checkpoint` can fail: [`Writer::rotate`] cannot create the next
    /// segment. Nothing was written, so nothing is poisoned — and the writer stays usable, which
    /// is the whole difference from the `commit_staged` arm.
    ///
    /// This is what the log line in `append_witnessed` is for. Without it the failure has no
    /// report at all: the caller is handed `Ok`, `poisoned()` stays `None`, and the checkpoint
    /// is simply missing. The cadence slips by one interval too, because `since_checkpoint` was
    /// reset before the attempt — asserted here so that changing it is a decision rather than an
    /// accident.
    #[test]
    fn a_checkpoint_that_cannot_rotate_is_lost_without_poisoning_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        // Squat the segment `rotate` would create, so `create_new` fails with `AlreadyExists`.
        fs::write(segment_path(dir.path(), 2), b"").unwrap();
        let caller = (frame::HEADER_LEN + serde_json::to_vec(&record(1)).unwrap().len()) as u64;
        let mut w = checkpointing(dir.path(), 2, 2 * caller);
        w.append(&record(1)).unwrap();
        // The second append fills segment 1 exactly, so the checkpoint after it must rotate.
        let second = w.append(&record(2)).unwrap();
        assert_eq!(
            second.segment(),
            1,
            "the caller's frames both fit segment 1"
        );
        assert_eq!(
            w.poisoned(),
            None,
            "nothing was written, so nothing is torn"
        );

        // Where the failure *does* surface without the log line, and why that is too late: the
        // caller's own next append needs the same segment, and gets the bare `AlreadyExists`
        // that the checkpoint met first — reported against a record that had nothing to do
        // with it.
        let err = w.append(&record(3)).unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists),
            "{err:?}"
        );
        drop(w);
        let bytes = fs::read(segment_path(dir.path(), 1)).unwrap();
        let outcome = scan(&bytes);
        assert!(outcome.failure.is_none(), "{outcome:?}");
        assert_eq!(outcome.frames, 2, "two caller frames, no checkpoint");

        // Control: the identical writer with a segment 2 it *can* create does emit the
        // checkpoint, so the absence above is the failed rotation and not the cadence.
        let ok = tempfile::tempdir().unwrap();
        let mut w = checkpointing(ok.path(), 2, 2 * caller);
        w.append(&record(1)).unwrap();
        w.append(&record(2)).unwrap();
        drop(w);
        let bytes = fs::read(segment_path(ok.path(), 2)).unwrap();
        assert_eq!(
            scan(&bytes).frames,
            1,
            "the checkpoint rotated into segment 2"
        );
    }

    /// A checkpoint's own frame is synced under the writer's policy, like any other frame.
    ///
    /// The checkpoint goes down through the same `write_and_sync` as a caller's record, so it
    /// gets the same durability — which is what [`Writer::sync_count`]'s doc claims and what
    /// nothing else here measures: every other checkpoint test runs under [`Fsync::None`], and
    /// the sync test above runs with the emission off, so a checkpoint written unsynced would
    /// leave both of them green.
    ///
    /// The count is the assertion. At one checkpoint per two appends, six appends sync six
    /// caller frames and three checkpoints — nine, not six, and not twelve.
    #[test]
    fn a_checkpoint_is_synced_like_any_other_frame() {
        for fsync in [Fsync::Full, Fsync::Fdatasync] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = checkpointing_with(dir.path(), fsync, 2, segment::DEFAULT_ROTATE_AT);
            for i in 1..=6 {
                w.append(&record(i)).unwrap();
                assert_eq!(
                    w.sync_count(),
                    i + i / 2,
                    "after {i} appends under {fsync:?}"
                );
            }
            assert_eq!(w.sync_count(), 9, "{fsync:?}");

            // Control: the same six appends with the emission off sync six times, so the three
            // extra syncs above are the checkpoints' and not something six appends do anyway.
            let off = tempfile::tempdir().unwrap();
            let mut w = checkpointing_with(off.path(), fsync, 0, segment::DEFAULT_ROTATE_AT);
            for i in 1..=6 {
                w.append(&record(i)).unwrap();
            }
            assert_eq!(w.sync_count(), 6, "{fsync:?}");
        }
    }

    /// Journal format: lsns are `(segment << 40) | offset`, strictly increasing, and the
    /// offset is the frame's first byte.
    #[test]
    fn lsns_are_positions() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, segment::DEFAULT_ROTATE_AT);
        let a = w.append(&record(1)).unwrap();
        let b = w.append(&record(2)).unwrap();
        assert_eq!(a, Lsn::from_parts(1, 0).unwrap());
        assert_eq!(b.segment(), 1);
        assert_eq!(
            b.offset(),
            fs::metadata(segment_path(dir.path(), 1)).unwrap().len() / 2
        );
        assert!(a < b);
        assert_eq!(w.next_lsn().unwrap().offset(), b.offset() * 2);
    }

    /// Segments: rotation happens before a frame that would cross `rotate_at`, a frame is
    /// never split, and every segment scans clean end to end.
    #[test]
    fn rotation_keeps_frames_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, 1024);
        let mut lsns = Vec::new();
        for i in 1..=20 {
            lsns.push(w.append(&record(i)).unwrap());
        }
        let segments = segment::list_segments(dir.path()).unwrap();
        assert!(segments.len() > 1, "{segments:?}");
        assert_eq!(w.segment(), *segments.last().unwrap());
        for no in &segments {
            let bytes = fs::read(segment_path(dir.path(), *no)).unwrap();
            assert!(bytes.len() <= 1024, "segment {no} is {} bytes", bytes.len());
            let outcome = scan(&bytes);
            assert!(outcome.failure.is_none(), "segment {no}: {outcome:?}");
            assert_eq!(outcome.good_end, bytes.len() as u64);
        }
        assert!(lsns.windows(2).all(|p| p[0] < p[1]));
        assert_eq!(
            lsns.iter().filter(|l| l.offset() == 0).count(),
            segments.len(),
            "every segment starts with a frame at offset 0"
        );
    }

    /// A frame larger than `rotate_at` still goes into an empty segment whole.
    #[test]
    fn oversize_frame_is_not_split() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, 64);
        let a = w.append(&record(1)).unwrap();
        let b = w.append(&record(2)).unwrap();
        assert_eq!((a.segment(), a.offset()), (1, 0));
        assert_eq!((b.segment(), b.offset()), (2, 0));
    }

    /// The writer only appends to a segment it created or found empty ("never modified
    /// after rotation"); handing it a non-empty one is refused by name.
    #[test]
    fn refuses_a_non_empty_segment() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(segment_path(dir.path(), 1), b"data").unwrap();
        let err = Writer::start(dir.path(), 1, JournalOptions::default()).unwrap_err();
        assert!(
            matches!(err, Error::SegmentNotEmpty { segment: 1, len: 4 }),
            "{err:?}"
        );
    }

    /// Invariant 1: a write that fails part-way poisons the writer — the failed lsn is
    /// returned by every later append, `next_lsn` does not move, nothing more is written, and
    /// reopening repairs the partial frame as the torn tail at exactly that lsn.
    #[test]
    fn short_write_poisons_and_reopen_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, segment::DEFAULT_ROTATE_AT);
        let first = w.append(&record(1)).unwrap();
        let expected = w.next_lsn().unwrap();
        w.fail_next(Fault::WriteShort(5));
        let err = w.append(&record(2)).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err:?}");
        assert_eq!(w.poisoned(), Some(expected));
        for _ in 0..2 {
            let err = w.append(&record(3)).unwrap_err();
            assert!(
                matches!(err, Error::Poisoned { lsn } if lsn == expected),
                "{err:?}"
            );
        }
        assert_eq!(w.next_lsn().unwrap(), expected);
        let len = fs::metadata(segment_path(dir.path(), 1)).unwrap().len();
        assert_eq!(len, expected.offset() + 5, "five bytes, then nothing more");
        drop(w);
        let (_, report) = crate::open(dir.path(), JournalOptions::default()).unwrap();
        assert_eq!(
            report.torn.map(|t| (t.lsn, t.dropped_bytes)),
            Some((expected, 5))
        );
        assert_eq!(report.frames, 1);
        assert!(report.end > first);
    }

    /// A sync failure poisons too: the frame may be whole on disk but its append returned an
    /// error, so reopen finds it intact and replays it — the same outcome as a crash between
    /// write and return, which the caller must already tolerate.
    #[test]
    fn sync_failure_poisons() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::Full, segment::DEFAULT_ROTATE_AT);
        w.append(&record(1)).unwrap();
        let expected = w.next_lsn().unwrap();
        w.fail_next(Fault::Sync);
        assert!(w.append(&record(2)).is_err());
        assert_eq!(w.sync_count(), 1, "the failed sync is not counted");
        let err = w.append(&record(3)).unwrap_err();
        assert!(
            matches!(err, Error::Poisoned { lsn } if lsn == expected),
            "{err:?}"
        );
        drop(w);
        let (_, report) = crate::open(dir.path(), JournalOptions::default()).unwrap();
        assert_eq!(report.torn, None);
        assert_eq!(report.frames, 2);
    }

    /// A record that serialises past [`MAX_PAYLOAD`] is refused by name, and nothing is written.
    #[test]
    fn oversize_payload_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, segment::DEFAULT_ROTATE_AT);
        let huge = Entry {
            at: Timestamp::from_unix_millis(AT_BASE),
            record: Record::MemoryAppend {
                agent: AgentId::NIL,
                text: "x".repeat(MAX_PAYLOAD),
                body: None,
                provenance: None,
            },
        };
        let err = w.append(&huge).unwrap_err();
        assert!(
            matches!(err, Error::PayloadTooLarge { len, max } if len > max && max == MAX_PAYLOAD),
            "{err:?}"
        );
        assert_eq!(w.next_lsn().unwrap(), Lsn::from_parts(1, 0).unwrap());
        assert_eq!(w.poisoned(), None, "refused before the write began");
        assert_eq!(fs::metadata(segment_path(dir.path(), 1)).unwrap().len(), 0);
    }

    /// Durable admission uses the exact post-rotation `journal.appended` frame.
    /// Cap+1 is refused before segment creation or writer-state movement; the same record shape
    /// one byte smaller is accepted at the cap and reads back from the candidate lsn.
    #[test]
    fn notification_admission_is_exact_and_precedes_rotation_or_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::Full, 1);
        w.append(&record(1)).unwrap();

        let candidate = Lsn::from_parts(2, 0).unwrap();
        let refused = record_with_notification_len(candidate, MAX_FRAME_BYTES + 1);
        let accepted = record_with_notification_len(candidate, MAX_FRAME_BYTES);
        let before_segment = w.segment();
        let before_lsn = w.next_lsn().unwrap();
        let before_syncs = w.sync_count();
        let before_bytes = fs::read(segment_path(dir.path(), 1)).unwrap();
        let before_segments = segment::list_segments(dir.path()).unwrap();

        let err = w
            .append(&refused)
            .expect_err("a cap+1 notification is not made durable");
        assert!(
            matches!(
                err,
                Error::NotificationTooLarge { len, max }
                    if len == MAX_FRAME_BYTES + 1 && max == MAX_FRAME_BYTES
            ),
            "{err:?}"
        );
        assert_eq!(w.segment(), before_segment, "rotation did not begin");
        assert_eq!(w.next_lsn().unwrap(), before_lsn, "lsn did not advance");
        assert_eq!(w.sync_count(), before_syncs, "nothing was synced");
        assert_eq!(w.poisoned(), None, "the writer remains usable");
        assert_eq!(
            fs::read(segment_path(dir.path(), 1)).unwrap(),
            before_bytes,
            "the current segment did not change"
        );
        assert_eq!(
            segment::list_segments(dir.path()).unwrap(),
            before_segments,
            "the post-rotation segment was not created"
        );

        let lsn = w
            .append(&accepted)
            .expect("the same notification shape at the cap is admitted");
        assert_eq!(lsn, candidate);
        assert_eq!(w.sync_count(), before_syncs + 1);
        drop(w);
        let entries = read_back(dir.path());
        assert_eq!(entries.last(), Some(&(candidate, accepted)));
    }

    /// the frame payload is the `Entry`, with `at` a millisecond
    /// **integer** flattened beside `t` — not a nested object, not a string. Read off the
    /// segment bytes, because what the daemon writes is what a later reader must parse.
    #[test]
    fn at_is_a_millisecond_integer_beside_t() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), Fsync::None, segment::DEFAULT_ROTATE_AT);
        w.append(&Entry {
            at: Timestamp::from_unix_millis(AT_BASE),
            record: Record::Pause {
                agent: AgentId::NIL,
                browser: None,
            },
        })
        .unwrap();
        drop(w);
        let bytes = fs::read(segment_path(dir.path(), 1)).unwrap();
        let payload = frame::decode(&bytes).unwrap().payload;
        let v: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(v["at"].as_i64(), Some(AT_BASE), "{v}");
        assert_eq!(v["t"], "pause", "{v}");
        // Control 1: flattened, so `as_i64` is not reading some other level of the payload.
        assert!(v.get("record").is_none(), "{v}");
        // Control 2: `as_i64` is not vacuously satisfied. Quote that same stamp in that same
        // payload and it no longer decodes — so the assertion above tells an integer from a
        // string rather than accepting either.
        let text = std::str::from_utf8(payload).unwrap();
        let quoted = text.replace(
            &format!("\"at\":{AT_BASE}"),
            &format!("\"at\":\"{AT_BASE}\""),
        );
        assert_ne!(quoted, text, "the control changed nothing: {text}");
        assert!(serde_json::from_str::<Entry>(&quoted).is_err(), "{quoted}");
    }

    /// `[daemon] fsync` defaults to `full`, segments rotate at 256 MiB, and a checkpoint is
    /// written every 10,000 appends.
    #[test]
    fn defaults_are_full_sync_256_mib_and_a_checkpoint_every_ten_thousand() {
        let options = JournalOptions::default();
        assert_eq!(options.fsync, Fsync::Full);
        assert_eq!(options.rotate_at, segment::DEFAULT_ROTATE_AT);
        assert_eq!(segment::DEFAULT_ROTATE_AT, 268_435_456);
        assert_eq!(options.checkpoint_every, 10_000);
        assert_eq!(Fsync::default(), Fsync::Full);
    }

    /// Every entry the journal holds, in order, with the lsn each was read at.
    fn read_back(dir: &Path) -> Vec<(Lsn, Entry)> {
        let (reader, _) = crate::reader::Reader::open(dir).unwrap();
        reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// A `Checkpoint` after every `checkpoint_every` appends, and after no
    /// other append. The assertion is the whole sequence of kinds rather than a count, because
    /// *where* the checkpoints fall is the property — a count passes for a writer that emitted
    /// two in a row and none later.
    ///
    /// A checkpoint does not count towards the next one: seven appends at N = 3 put the second
    /// checkpoint after the seventh *caller* record, at frame index 7, not at index 6.
    #[test]
    fn a_checkpoint_lands_every_nth_append_and_nowhere_else() {
        use RecordKind::{Checkpoint, MemoryAppend};

        let dir = tempfile::tempdir().unwrap();
        let mut w = checkpointing(dir.path(), 3, segment::DEFAULT_ROTATE_AT);
        for i in 1..=7 {
            w.append(&record(i)).unwrap();
        }
        drop(w);
        let kinds: Vec<RecordKind> = read_back(dir.path())
            .iter()
            .map(|(_, e)| e.record.kind())
            .collect();
        assert_eq!(
            kinds,
            [
                MemoryAppend,
                MemoryAppend,
                MemoryAppend,
                Checkpoint,
                MemoryAppend,
                MemoryAppend,
                MemoryAppend,
                Checkpoint,
                MemoryAppend,
            ]
        );

        // Control: the same seven appends with `checkpoint_every: 0` yield seven frames and no
        // `Checkpoint`. Without it, "the kinds are these nine" could hold for a read-back that
        // reports `Checkpoint` for something else, or for a writer that emits one regardless.
        let off = tempfile::tempdir().unwrap();
        let mut w = checkpointing(off.path(), 0, segment::DEFAULT_ROTATE_AT);
        for i in 1..=7 {
            w.append(&record(i)).unwrap();
        }
        drop(w);
        let kinds: Vec<RecordKind> = read_back(off.path())
            .iter()
            .map(|(_, e)| e.record.kind())
            .collect();
        assert_eq!(kinds, [MemoryAppend; 7]);
    }

    /// What the record carries: its **own** frame's `(segment, offset)`, this
    /// build's `PROJECTION_VERSION`, and the `at` of the entry whose append triggered it
    /// — not a clock of the writer's own, which this crate does not have.
    ///
    /// `projection_version` is widened to `u64` and compared against the constant rather than
    /// narrowed the way the writer narrows it: comparing the two narrowings would hold however
    /// wrong the narrowing was.
    #[test]
    fn a_checkpoint_names_its_own_frame_this_build_and_the_at_that_triggered_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = checkpointing(dir.path(), 3, segment::DEFAULT_ROTATE_AT);
        for i in 1..=3 {
            w.append(&record(i)).unwrap();
        }
        drop(w);
        let entries = read_back(dir.path());
        let (lsn, entry) = &entries[3];
        // Control: the first frame *is* at offset 0, so `lsn.offset() > 0` below is a fact
        // about where the checkpoint landed and not one about frames in general.
        assert_eq!(entries[0].0.offset(), 0);
        assert!(lsn.offset() > 0, "{lsn}");
        let Record::Checkpoint {
            projection_version,
            segment,
            offset,
        } = entry.record
        else {
            panic!("frame 3 is not a checkpoint: {:?}", entry.record)
        };
        assert_eq!((segment, offset), (lsn.segment(), lsn.offset()));
        assert_eq!(u64::from(projection_version), PROJECTION_VERSION);
        // `record(i)` stamps a distinct `at` per `i`, so a writer that reused the *first*
        // entry's stamp, or a constant, fails here — the `assert_ne!` is what says so.
        assert_eq!(entry.at, record(3).at);
        assert_ne!(record(3).at, record(1).at);
    }

    /// A checkpoint that fails to write poisons the writer **without** un-acknowledging the
    /// record whose append triggered it: those bytes are synced, and invariant 2 rests on
    /// `append`'s return value. Reporting `Err` there would tell the daemon a record was not
    /// persisted that the log already holds, and it would issue it again.
    #[test]
    fn a_failed_checkpoint_poisons_without_un_acknowledging_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = checkpointing(dir.path(), 1, segment::DEFAULT_ROTATE_AT);
        let expected = w.next_lsn().unwrap();
        w.fail_second_write(Fault::WriteShort(5));
        let lsn = w
            .append(&record(1))
            .expect("the caller's frame was written and synced");
        assert_eq!(lsn, expected);
        // Where the checkpoint's frame started: just past the caller's, whose length is derived
        // from the same encoding the writer used rather than read back off the writer.
        let caller_frame =
            (frame::HEADER_LEN + serde_json::to_vec(&record(1)).unwrap().len()) as u64;
        let checkpoint_lsn = Lsn::from_parts(lsn.segment(), lsn.offset() + caller_frame).unwrap();
        assert_eq!(w.poisoned(), Some(checkpoint_lsn));
        let err = w.append(&record(2)).unwrap_err();
        assert!(
            matches!(err, Error::Poisoned { lsn } if lsn == checkpoint_lsn),
            "{err:?}"
        );
        drop(w);
        // The caller's frame survives as a whole frame; the checkpoint's five bytes are the
        // torn tail `open` repairs.
        let (_, report) = crate::open(dir.path(), JournalOptions::default()).unwrap();
        assert_eq!(report.frames, 1);
        assert_eq!(
            report.torn.map(|t| (t.lsn, t.dropped_bytes)),
            Some((checkpoint_lsn, 5))
        );

        // Control: the same writer with the fault on the **caller's** write instead. `Ok`
        // above is then not the harness's default answer, and the two failures are told apart
        // by which lsn the writer is poisoned at.
        let dir = tempfile::tempdir().unwrap();
        let mut w = checkpointing(dir.path(), 1, segment::DEFAULT_ROTATE_AT);
        let expected = w.next_lsn().unwrap();
        w.fail_next(Fault::WriteShort(5));
        assert!(w.append(&record(1)).is_err());
        assert_eq!(w.poisoned(), Some(expected));
    }

    /// The record names the position it occupies, so its own encoded length decides where it
    /// goes — and a checkpoint that would cross `rotate_at` has to be built again against the
    /// segment it actually lands in, not the one it was first encoded for.
    ///
    /// `rotate_at` is two caller frames **exactly**, so both appends fit segment 1 and the
    /// checkpoint after them is necessarily the frame that crosses. The assertion is that the
    /// record agrees with its own lsn *after* the rotation, which a writer that encoded once
    /// and rotated afterwards would fail: it would still claim segment 1 at the old offset.
    #[test]
    fn a_checkpoint_that_rotates_names_the_segment_it_lands_in() {
        let dir = tempfile::tempdir().unwrap();
        let caller = (frame::HEADER_LEN + serde_json::to_vec(&record(1)).unwrap().len()) as u64;
        let mut w = checkpointing(dir.path(), 2, 2 * caller);
        w.append(&record(1)).unwrap();
        w.append(&record(2)).unwrap();
        drop(w);
        let entries = read_back(dir.path());
        let (lsn, entry) = &entries[2];
        assert_eq!(lsn.segment(), 2, "the checkpoint rotated: {lsn}");
        assert_eq!(lsn.offset(), 0);
        let Record::Checkpoint {
            segment, offset, ..
        } = entry.record
        else {
            panic!("frame 2 is not a checkpoint: {:?}", entry.record)
        };
        assert_eq!((segment, offset), (lsn.segment(), lsn.offset()));
        // Control: the two caller frames really are in segment 1, so the rotation above is the
        // checkpoint's own and not something every frame in this test does.
        assert_eq!(
            entries[..2]
                .iter()
                .map(|(l, _)| l.segment())
                .collect::<Vec<_>>(),
            [1, 1]
        );
    }
}
