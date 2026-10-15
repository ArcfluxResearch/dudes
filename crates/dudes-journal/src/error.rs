//! Every way the journal can fail, named: every error that can reach the journal is a named
//! variant. A torn tail is not an error — it is reported in [`crate::OpenReport`], and
//! dropped by the writer's open path — so there is no `Torn` variant here.

use std::io;
use std::path::PathBuf;

use dudes_types::{AgentId, Lsn, MailId};

use crate::frame::FrameError;

/// The journal's error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A keyed instruction projection cell is malformed or missing its referenced attempt.
    #[error("projection instructions: {0}")]
    InstructionCache(#[from] dudes_proto::instruction_state::cache::InstructionCacheError),
    /// An instruction payload or its canonical prefix relationships are invalid.
    #[error("journal instruction record at {lsn}: {reason}")]
    Instruction {
        /// Offending position, or zero during position-independent staging.
        lsn: Lsn,
        /// Fixed contract or history failure.
        reason: dudes_proto::instruction_state::InstructionStateError,
    },
    /// An instruction record was refused against the committed prefix before append.
    #[error("journal instruction record {kind:?} refused before append: {reason}")]
    InstructionAdmission {
        /// The refused record's kind.
        kind: dudes_proto::RecordKind,
        /// Fixed contract or history failure.
        reason: dudes_proto::instruction_state::InstructionStateError,
    },
    /// An instruction payload is not carried by its exact required-v2 header.
    #[error(
        "journal instruction header at {lsn} has kind {kind} version {version}, expected kind {expected_kind} version 2"
    )]
    InstructionFrame {
        /// Offending intact frame.
        lsn: Lsn,
        /// Actual header kind.
        kind: u16,
        /// Actual header version.
        version: u16,
        /// Exact typed payload kind.
        expected_kind: u16,
    },
    /// A file or directory operation failed.
    #[error("journal i/o: {0}")]
    Io(#[from] io::Error),
    /// Temporary completed-contract validation storage failed. Check the named paths and use a
    /// writable scratch directory outside the journal with enough free space, then retry.
    /// Deleting `state.redb` does not repair this temporary-storage failure.
    #[error(
        "journal validation scratch at {} failed to {step}: {source}; use a writable directory outside the journal with enough free space and retry",
        directory.display()
    )]
    ValidationScratch {
        /// The selected temporary-storage directory.
        directory: PathBuf,
        /// The storage operation that failed.
        step: &'static str,
        /// The original filesystem or projection-storage failure.
        #[source]
        source: Box<Error>,
    },
    /// A frame that is not the torn tail failed to decode: the bytes on disk are not what the
    /// writer produced. Never repaired automatically.
    #[error("journal corrupt at {lsn}: {reason}")]
    Corrupt {
        /// Where the bad frame starts.
        lsn: Lsn,
        /// What the codec objected to.
        reason: FrameError,
    },
    /// An intact frame carries a header version this build does not read.
    #[error(
        "journal version skew at {lsn}: frame version {found}, this build reads versions 1 and {expected}"
    )]
    VersionSkew {
        /// Where the frame starts.
        lsn: Lsn,
        /// The version in the header.
        found: u16,
        /// The newest supported header version.
        expected: u16,
    },
    /// An intact required frame names a record kind this build cannot interpret.
    #[error("journal required record kind {kind} at {lsn} is not understood")]
    RequiredKind {
        /// Where the unsupported frame starts.
        lsn: Lsn,
        /// The unrecognized kind tag.
        kind: u16,
    },
    /// Required mail authority failed its full canonical prefix join.
    #[error("journal mail-wake record at {lsn}: {reason}")]
    MailWake {
        /// Offending canonical position.
        lsn: Lsn,
        /// Fixed failed relationship, without mail content.
        reason: dudes_proto::mail_wake::MailWakeError,
    },
    /// A mail authority payload is not carried by its exact required-v2 header.
    #[error(
        "journal mail-wake header at {lsn} has kind {kind} version {version}, expected kind {expected_kind} version 2"
    )]
    MailWakeFrame {
        /// Offending intact frame.
        lsn: Lsn,
        /// Header kind.
        kind: u16,
        /// Header version.
        version: u16,
        /// Typed payload's exact required kind.
        expected_kind: u16,
    },
    /// A taught-note payload is not carried by its exact required-v2 header.
    #[error(
        "journal taught-note header at {lsn} has kind {kind} version {version}, expected kind {expected_kind} version 2"
    )]
    TaughtNoteFrame {
        /// Offending intact frame.
        lsn: Lsn,
        /// Header kind.
        kind: u16,
        /// Header version.
        version: u16,
        /// Typed payload's exact required kind.
        expected_kind: u16,
    },
    /// A frame of a known kind whose payload is not an `Entry` — including a `ver = 1` payload
    /// with no `at`. Hard: the daemon must
    /// not drop state silently.
    #[error("journal record at {lsn} does not decode: {source}")]
    Decode {
        /// Where the frame starts.
        lsn: Lsn,
        /// The serde error.
        #[source]
        source: serde_json::Error,
    },
    /// An entry or its typed delivery notification did not serialise. Not expected for any
    /// `Entry`; named so it cannot be mistaken for an I/O failure.
    #[error("journal record does not serialise: {source}")]
    Encode {
        /// The serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A record serialised to more than [`crate::frame::MAX_PAYLOAD`] bytes; refused on write.
    #[error("journal payload of {len} bytes exceeds the {max}-byte frame limit")]
    PayloadTooLarge {
        /// The serialised size.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The exact newline-delimited `journal.appended` notification would not fit the RPC frame,
    /// so making this entry durable would create a record no subscriber can receive. Refused
    /// before rotation, file write, lsn advance, or writer mutation.
    #[error("journal.appended notification of {len} bytes exceeds the {max}-byte delivery limit")]
    NotificationTooLarge {
        /// Complete serialized notification size, including the terminating newline.
        len: usize,
        /// The RPC frame admission ceiling.
        max: usize,
    },
    /// An append failed after its write began, so the segment may hold a partial frame at
    /// `lsn`, or a whole one whose sync failed. The writer refuses every later append: a new
    /// frame after a partial one would be unreachable, and its lsn a lie. Recovery is to
    /// reopen the journal — open repairs the partial frame as a torn tail or replays the whole
    /// one, the same two outcomes as a crash between write and return.
    #[error("journal writer poisoned: the append at {lsn} failed after its write began")]
    Poisoned {
        /// The lsn of the append that failed.
        lsn: Lsn,
    },
    /// The writer was handed a segment that already holds bytes. `open` only ever starts the
    /// writer on a fresh or empty segment, so a rotated segment is never modified.
    #[error(
        "journal segment {segment} is not empty ({len} bytes); the writer only starts on a fresh segment"
    )]
    SegmentNotEmpty {
        /// The segment number.
        segment: u32,
        /// Its length.
        len: u64,
    },
    /// A position does not fit the 40-bit offset / 24-bit segment of an `Lsn`.
    #[error("journal position segment {segment} offset {offset} does not fit an lsn")]
    LsnRange {
        /// Segment number.
        segment: u32,
        /// Byte offset.
        offset: u64,
    },
    /// There is nothing at this path, or what is there is not a directory. The read path
    /// ([`crate::Reader::open`], a one-shot offline `dudes replay`) never creates one: a mistyped
    /// path is a question to answer, not an empty journal to invent. Only [`crate::open`], the
    /// writer, creates the directory.
    ///
    /// Strictly *absent*: a path the OS refused to describe (`EACCES` on a parent, `ELOOP`,
    /// `EIO`) is [`Error::Io`], because telling an operator with a permissions problem that there
    /// is no journal invites them to create one over the journal that is there.
    #[error("no journal directory at {}", dir.display())]
    NoJournal {
        /// The path that does not exist.
        dir: PathBuf,
    },
    /// The path is a directory, but it contains no journal segments. The read path refuses to
    /// invent a journal there; [`crate::open`] is the writer path that initializes one.
    #[error("directory at {} is not a journal: it contains no segments", dir.display())]
    NotAJournal {
        /// The existing directory that contains no journal segments.
        dir: PathBuf,
    },
    /// Segment files are not contiguous: records between them are missing.
    #[error("journal segment gap: segment {found} follows segment {after}")]
    SegmentGap {
        /// The last segment before the gap.
        after: u32,
        /// The first segment after it.
        found: u32,
    },
    /// The projection store (`state.redb`) failed. Deleting the file and replaying is always a
    /// valid recovery.
    #[error("projection store: {0}")]
    Store(#[from] redb::Error),
    /// A projection row did not (de)serialise: the store holds bytes this build did not write.
    #[error("projection row: {source}")]
    Row {
        /// The serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A bounded Floor row disagrees with its canonical journal-derived metadata.
    #[error("projected mail view is inconsistent at {mail:?}")]
    MailViewCorrupt {
        /// The inconsistent item, or no item for a malformed complete window.
        mail: Option<MailId>,
    },
    /// The exact latest-state unread set is larger than the public `u32` count can represent.
    #[error("unread mail count {count} for agent {agent} does not fit u32")]
    UnreadCountRange {
        /// Whose unread set was counted.
        agent: AgentId,
        /// Exact number of matching projection rows.
        count: u64,
    },
    /// The reader exposes no frame at the projection's `last_applied_lsn` and its segment was
    /// not archived. Bytes or segments may have been removed after the projection was written,
    /// so its rows describe records the log no longer holds; or a fixed [`crate::Reader::open`]
    /// torn-tail snapshot may hide bytes completed after it opened. Never reset automatically:
    /// reopen the reader to refresh that snapshot, and if the position stays missing the human
    /// decides whether to accept the loss by deleting `state.redb` and rebuilding from what the
    /// log still has.
    #[error(
        "projection ahead of the reader view: last applied {last_applied}, but the reader exposes no frame there"
    )]
    ProjectionAhead {
        /// The projection's `last_applied_lsn`.
        last_applied: Lsn,
    },
    /// The journal holds *a* frame at the projection's `last_applied_lsn`, but not the one the
    /// projection applied: the checksums differ. A segment restored
    /// from a backup, re-created and re-appended to, or copied from another data directory
    /// presents the same position for different bytes, and [`Error::ProjectionAhead`] stays
    /// quiet because the position is intact. Same recovery as `ProjectionAhead` and a different
    /// sentence, because *truncated* and *rewritten* are different incidents: nothing is reset,
    /// and deleting `state.redb` rebuilds from the log as it now stands.
    ///
    /// Two separate things, and the checksum keeps them apart. The **primitive**: a 32-bit
    /// error-detecting checksum distinguishes an *unchosen* difference and never a chosen one,
    /// so this says the record here is not the one applied, not which record it is. The **threat model**: whoever can rewrite a segment can rewrite `state.redb`
    /// beside it, so this detects an accident — a restore, a re-created segment, a copied data
    /// directory — and is not a security control.
    #[error(
        "projection diverged from the journal: last applied {last_applied} with crc32c \
         {expected:#010x}, but the journal holds a frame there with crc32c {found:#010x}"
    )]
    ProjectionDiverged {
        /// The projection's `last_applied_lsn`.
        last_applied: Lsn,
        /// The checksum the projection stored beside it, as the `meta` table holds it — a
        /// `u64` cell carrying a `u32` checksum (read as `Option<u64>`, never
        /// `unwrap_or(0)`).
        expected: u64,
        /// The `crc32c` of the frame the journal actually holds at that position.
        found: u32,
    },
    /// A projection that has applied nothing was replayed over a journal whose own first
    /// segment is gone, so the rebuild would start at a **suffix**: the archived records are in
    /// no projection and in no log, and the rows would silently describe part of the history.
    ///
    /// Invariant 1 is what this protects. A projection is whatever `dudes replay` rebuilds from
    /// the journal, and a rebuild that drops the prefix is not a
    /// rebuild — the daemon would have no way to know it happened, because an empty cursor has
    /// nothing to compare and [`Error::ProjectionAhead`] stays quiet: `Lsn::ZERO` sits before
    /// every segment, so whenever the log has a segment [`crate::Location::Archived`] is the only
    /// answer that position can get.
    ///
    /// A fresh store is the ordinary case, not an exotic one: every `PROJECTION_VERSION` bump
    /// produces one. Recovery is to restore the archived segments, or — when the loss is
    /// accepted — to say so in a way the log itself records; nothing here resets anything.
    ///
    /// **Partial archival only.** Archive the last segment too and there is no segment number
    /// left to notice it by: [`crate::open`] mints [`crate::FIRST_SEGMENT`] on the empty
    /// directory before the daemon replays, so a fresh store rebuilds to an empty projection and
    /// reports `Ok`. This variant is a refusal, not a boundary — the boundary needs a record of
    /// the archival in a segment that survives it.
    #[error(
        "rebuild would start from a suffix: the projection has applied nothing and the journal's \
         first present segment is {first_present}, so the records archived before it would be in \
         no projection"
    )]
    RebuildFromSuffix {
        /// The lowest segment number the journal still has.
        first_present: u32,
    },
    /// Replay handed the projection an lsn at or below one already applied in the same batch
    /// (replay asserts monotone lsn).
    #[error("projection apply out of order: lsn {got} after {last}")]
    NonMonotone {
        /// The last lsn applied.
        last: Lsn,
        /// The lsn that was not greater.
        got: Lsn,
    },
}

macro_rules! store_errors {
    ($($ty:ty),* $(,)?) => {
        $(impl From<$ty> for Error {
            fn from(err: $ty) -> Self {
                Self::Store(redb::Error::from(err))
            }
        })*
    };
}

store_errors!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError,
);

/// `Result` with the journal's error.
pub type Result<T, E = Error> = std::result::Result<T, E>;
