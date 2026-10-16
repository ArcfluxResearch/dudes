//! Reading the journal: validation on open, torn-tail handling, and `iter_from(lsn)`.
//!
//! Reads never modify the journal. [`survey`] walks the segments and classifies what it finds;
//! [`repair`] adds the one truncation for the writer entry points. The writer drops the torn
//! bytes; the reader bounds itself before them:
//! [`Reader::open`] reports the tail, leaves every byte in place and stops reading there. That
//! is what keeps an offline `dudes replay` from cutting the frame a running daemon is in the
//! middle of `write_all`, which would lose that record on the daemon's next start.
//!
//! Torn tails. A frame whose `append` never returned is one nothing observable depended
//! on, so dropping it loses nothing. On open every frame of every segment is walked; a
//! decode failure in the *last* segment is a torn tail when the frame's claimed extent reaches
//! EOF, when the header itself is short, or when every byte from the failure to EOF is zero
//! (a file extended but never flushed). "Torn" is scoped to the *final* frame: before the
//! tail is reported, the bytes it covers are searched for a frame that decodes — a
//! damaged `len` on an earlier frame claims an extent past EOF just like a cut does, but an
//! intact frame after it proves the damage is not the tail. Only when nothing acknowledged can
//! follow is the tail reported and — on the writer's path alone — the file truncated to the last
//! good frame, synced, and `journal.torn{lsn}` logged.
//! Any other failure is corruption, which open refuses to repair: truncating there would
//! destroy acknowledged records. The search itself is bounded ([`FIND_FRAME_CRC_BUDGET`]): bytes
//! that keep it checksumming past the budget are refused as corruption too, never searched for
//! days and never truncated — open refuses, it does not hang.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use dudes_proto::{Entry, RecordKind};
use dudes_types::Lsn;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::frame::{self, FrameError, HEADER_LEN, Header, MAX_PAYLOAD};
use crate::segment::{FIRST_SEGMENT, list_segments, lsn_at, segment_path, sync_dir};

mod recovery;

#[cfg(test)]
std::thread_local! {
    static VALIDATION_PASSES: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
mod validation_tests;

/// Where and why a scan stopped short of a clean end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanFailure {
    /// Offset of the frame that failed.
    pub offset: u64,
    /// What the codec objected to.
    pub error: FrameError,
    /// Where the frame claimed to end, when a header could be read.
    pub claimed_end: Option<u64>,
}

/// Result of walking one segment's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Frames that decoded.
    pub frames: u64,
    /// Offset just past the last good frame.
    pub good_end: u64,
    /// `None` when the bytes end exactly on a frame boundary.
    pub failure: Option<ScanFailure>,
}

/// Walk frames in `bytes` until the end or the first failure. Pure and panic-free on any
/// input; the fuzz target drives this.
#[must_use]
pub fn scan(bytes: &[u8]) -> ScanOutcome {
    let mut cursor = io::Cursor::new(bytes);
    let mut buf = Vec::new();
    scan_reader(&mut cursor, &mut buf).unwrap_or_else(|_| unreachable!("a cursor does not fail"))
}

/// Read until `buf` is full or EOF; returns the bytes read.
fn read_full<R: Read>(r: &mut R, mut buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// What lies at a frame boundary.
#[derive(Debug)]
enum Next {
    /// Clean EOF: no bytes at all.
    End,
    /// A frame that decoded; its payload is in the caller's buffer.
    Frame(Header),
    /// An intact frame whose required version names a record kind this build does not
    /// understand: never corrupt or torn, so it is not a [`FrameError`].
    UnsupportedKind(Header),
    /// Bytes that are not a frame, and where the frame claimed to end when a header was read.
    Failed(FrameError, Option<u64>),
}

/// Read the frame at `offset` (the reader's current position) into `buf`. The one place the
/// framing rules are applied on the read side: the open-time scan and [`Iter`] both use it.
fn read_next_frame<R: Read>(r: &mut R, offset: u64, buf: &mut Vec<u8>) -> io::Result<Next> {
    let mut head = [0u8; HEADER_LEN];
    let n = read_full(r, &mut head)?;
    if n == 0 {
        return Ok(Next::End);
    }
    if n < HEADER_LEN {
        return Ok(Next::Failed(FrameError::Short { need: HEADER_LEN }, None));
    }
    let header = Header::parse(&head);
    // Nothing has checked `len` yet, so the extent is computed in `u64` from the header field
    // rather than through `usize`: on a 32-bit target `HEADER_LEN + u32::MAX` is not a `usize`.
    let claimed_end = Some(offset + header.claimed_len());
    if header.len as usize > MAX_PAYLOAD {
        return Ok(Next::Failed(
            FrameError::TooLong { len: header.len },
            claimed_end,
        ));
    }
    buf.clear();
    buf.resize(header.len as usize, 0);
    if read_full(r, buf)? < buf.len() {
        return Ok(Next::Failed(
            FrameError::Short {
                need: header.frame_len(),
            },
            claimed_end,
        ));
    }
    match frame::verify(header, buf) {
        Ok(()) => match frame::check_required_kind(header.ver, header.kind) {
            Ok(()) => Ok(Next::Frame(header)),
            Err(frame::UnknownRequiredKind { .. }) => Ok(Next::UnsupportedKind(header)),
        },
        Err(error) => Ok(Next::Failed(error, claimed_end)),
    }
}

/// The error a frame failure outside the torn tail becomes: version skew on an intact frame,
/// corruption otherwise.
fn frame_error(lsn: Lsn, error: FrameError) -> Error {
    match error {
        FrameError::Version { found } => Error::VersionSkew {
            lsn,
            found,
            expected: frame::REQUIRED_VERSION,
        },
        reason => Error::Corrupt { lsn, reason },
    }
}

/// Streaming form of [`scan`] over a file. An unsupported required kind is not a byte-framing
/// failure (the frame is intact), so this pure classifier counts it as a found frame like any
/// other; [`scan_reader_detailed`] is what [`survey`] uses to refuse it before repair.
pub(crate) fn scan_reader<R: Read>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<ScanOutcome> {
    let mut offset = 0u64;
    let mut frames = 0u64;
    loop {
        match read_next_frame(r, offset, buf)? {
            Next::End => {
                return Ok(ScanOutcome {
                    frames,
                    good_end: offset,
                    failure: None,
                });
            }
            Next::Frame(header) | Next::UnsupportedKind(header) => {
                offset += header.frame_len() as u64;
                frames += 1;
            }
            Next::Failed(error, claimed_end) => {
                return Ok(ScanOutcome {
                    frames,
                    good_end: offset,
                    failure: Some(ScanFailure {
                        offset,
                        error,
                        claimed_end,
                    }),
                });
            }
        }
    }
}

/// Why [`scan_reader_detailed`] stopped short of a segment's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanStop {
    /// A frame failed to decode.
    Malformed(ScanFailure),
    /// An intact required-version frame names a record kind this build does not understand.
    /// Never eligible for torn-tail treatment: the frame is intact, not cut.
    UnsupportedKind {
        /// Where the frame starts.
        offset: u64,
        /// The unrecognized kind tag.
        kind: u16,
    },
}

/// [`scan_reader`]'s outcome, plus why it stopped when it did not reach a clean end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DetailedScan {
    pub(crate) frames: u64,
    pub(crate) good_end: u64,
    pub(crate) stop: Option<ScanStop>,
    pub(crate) has_completed_contract: bool,
    has_recovery_contract: bool,
    fingerprint: [u8; 32],
}

fn completed_contract_payload(header: Header, payload: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Tag {
        t: String,
    }
    if recovery::payload(header, payload)
        || matches!(
            RecordKind::from_tag(header.kind),
            Some(
                RecordKind::TaughtNoteTurnDecided
                    | RecordKind::TaughtNoteOnlyTurnIssued
                    | RecordKind::MailWakeTurnDecided
                    | RecordKind::MailWakeOnlyTurnIssued
                    | RecordKind::InstructionObservationStarted
                    | RecordKind::InstructionObservationFinished
                    | RecordKind::TicketInstructionReviewed
                    | RecordKind::InstructionAttemptBoundarySettled
            )
        )
    {
        return true;
    }
    serde_json::from_slice::<Tag>(payload).is_ok_and(|tag| {
        matches!(
            tag.t.as_str(),
            "taught_note_turn_decided"
                | "taught_note_only_turn_issued"
                | "mail_wake_turn_decided"
                | "mail_wake_only_turn_issued"
                | "instruction_observation_started"
                | "instruction_observation_finished"
                | "ticket_instruction_reviewed"
                | "instruction_attempt_boundary_settled"
        )
    })
}

/// [`scan_reader`], but stopping at — and naming — the first intact frame whose required kind
/// this build does not understand, instead of folding it into the frame count. [`survey`] is the
/// only caller: refusing an unsupported required kind before repair or projections
/// is a journal-level compatibility question, not the byte-framing one [`scan`] answers.
pub(crate) fn scan_reader_detailed<R: Read>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<DetailedScan> {
    let mut offset = 0u64;
    let mut frames = 0u64;
    let mut has_completed_contract = false;
    let mut has_recovery_contract = false;
    let mut fingerprint = blake3::Hasher::new();
    loop {
        match read_next_frame(r, offset, buf)? {
            Next::End => {
                return Ok(DetailedScan {
                    frames,
                    good_end: offset,
                    stop: None,
                    has_completed_contract,
                    has_recovery_contract,
                    fingerprint: *fingerprint.finalize().as_bytes(),
                });
            }
            Next::Frame(header) => {
                fingerprint.update(&header.to_bytes());
                fingerprint.update(buf);
                has_completed_contract |= completed_contract_payload(header, buf);
                has_recovery_contract |= recovery::payload(header, buf);
                offset += header.frame_len() as u64;
                frames += 1;
            }
            Next::UnsupportedKind(header) => {
                return Ok(DetailedScan {
                    frames,
                    good_end: offset,
                    stop: Some(ScanStop::UnsupportedKind {
                        offset,
                        kind: header.kind,
                    }),
                    has_completed_contract,
                    has_recovery_contract,
                    fingerprint: *fingerprint.finalize().as_bytes(),
                });
            }
            Next::Failed(error, claimed_end) => {
                return Ok(DetailedScan {
                    frames,
                    good_end: offset,
                    stop: Some(ScanStop::Malformed(ScanFailure {
                        offset,
                        error,
                        claimed_end,
                    })),
                    has_completed_contract,
                    has_recovery_contract,
                    fingerprint: *fingerprint.finalize().as_bytes(),
                });
            }
        }
    }
}

/// The most payload bytes [`find_frame`] checksums before giving up: four maximal frames. A
/// genuine region after a damaged header holds at most one acknowledged frame to verify, and
/// a random 12-byte window passes the header pre-check with probability about 2⁻²⁴ — one
/// accidental candidate per 16 Mi offsets — so four leaves headroom of three maximal
/// accidents. Bytes that exhaust it are not a tail the reader can reason about.
pub const FIND_FRAME_CRC_BUDGET: usize = 4 * MAX_PAYLOAD;

/// What [`find_frame`] found in the bytes after a failed frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSearch {
    /// A frame decodes at this offset.
    Found(usize),
    /// No frame decodes anywhere in the searched range.
    Absent,
    /// The search stopped at [`FIND_FRAME_CRC_BUDGET`] with candidates left: the bytes are not
    /// a cut — a cut has at most one real frame to find — and the reader will not spend more
    /// on them.
    OverBudget,
}

/// The offset within `bytes` of the first intact supported frame (`ver` is supported, `len`
/// within the limit, crc32c correct), looking past the header at `0`: a damaged header claims
/// an extent the file cannot hold, but the next acknowledged frame, if any, still starts
/// within `HEADER_LEN + MAX_PAYLOAD` of it. The header fields are checked before the checksum
/// so a garbage region costs a header parse per byte, not a checksum per byte; and every
/// checksum is charged to [`FIND_FRAME_CRC_BUDGET`] *before* it runs, so a region whose every
/// fourth offset claims a maximal frame costs four checksums, not a checksum per offset. Pure
/// and panic-free; the fuzz target drives this too.
#[must_use]
pub fn find_frame(bytes: &[u8]) -> FrameSearch {
    let last_start = bytes.len().min(HEADER_LEN + MAX_PAYLOAD);
    let mut checksummed = 0usize;
    for at in HEADER_LEN..=last_start {
        let Some(head) = bytes[at..].first_chunk::<HEADER_LEN>() else {
            break;
        };
        let header = Header::parse(head);
        let len = header.len as usize;
        if !frame::supports_version(header.ver)
            || len > MAX_PAYLOAD
            || bytes.len() - at < header.frame_len()
        {
            continue;
        }
        if checksummed + len > FIND_FRAME_CRC_BUDGET {
            return FrameSearch::OverBudget;
        }
        checksummed += len;
        if frame::decode(&bytes[at..]).is_ok() {
            return FrameSearch::Found(at);
        }
    }
    FrameSearch::Absent
}

/// A torn tail: the bytes after the last good frame of the last segment, whose `append` never
/// returned. [`crate::open`] (the writer) drops them; [`Reader::open`] reports them, leaves them
/// on disk and stops reading there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Torn {
    /// Where the tail starts. On the writer's path the position is free again, but `open` starts
    /// a fresh segment after repair, so the next append's lsn is larger, not equal.
    pub lsn: Lsn,
    /// How many bytes the tail holds — dropped on the writer's path, left alone on the reader's.
    pub dropped_bytes: u64,
}

/// What open found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// The torn tail, when the last segment ends in one: dropped by [`crate::open`], reported
    /// and left in place by [`Reader::open`].
    pub torn: Option<Torn>,
    /// The position after the last good frame; `Lsn::ZERO` for an empty journal.
    pub end: Lsn,
    /// Frames found intact.
    pub frames: u64,
}

/// The segments present and the length of the last one, after repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) segments: Vec<u32>,
    pub(crate) last_len: u64,
    pub(crate) validated: ValidatedPrefix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedPrefix {
    first_segment: Option<u32>,
    fingerprint: [u8; 32],
}

/// True when every byte from `from` to EOF is zero.
fn rest_is_zero(file: &mut File, from: u64) -> io::Result<bool> {
    file.seek(SeekFrom::Start(from))?;
    let mut chunk = [0u8; 8192];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(true);
        }
        if chunk[..n].iter().any(|b| *b != 0) {
            return Ok(false);
        }
    }
}

/// True when the bytes from `from` to EOF (bounded: a header plus two maximal payloads is
/// enough to hold the damaged frame and the next one whole) may contain an acknowledged
/// frame: one decodes, or the search gave up before it could say — `journal.search_budget`
/// is logged and the caller treats the region as not a tail.
fn tail_may_hold_frame(file: &mut File, from: u64, file_len: u64, lsn: Lsn) -> io::Result<bool> {
    let bound = 2 * (HEADER_LEN + MAX_PAYLOAD) as u64;
    let len = usize::try_from(file_len.saturating_sub(from).min(bound))
        .expect("bounded by 32 MiB, which fits usize on every target");
    file.seek(SeekFrom::Start(from))?;
    let mut region = vec![0u8; len];
    let n = read_full(file, &mut region)?;
    Ok(match find_frame(&region[..n]) {
        FrameSearch::Found(_) => true,
        FrameSearch::Absent => false,
        FrameSearch::OverBudget => {
            tracing::warn!(
                target: "journal",
                lsn = %lsn,
                budget = FIND_FRAME_CRC_BUDGET,
                "journal.search_budget"
            );
            true
        }
    })
}

/// Whether a failure in the last segment is a torn tail (see the module docs): the failure
/// looks like a cut, and no acknowledged frame can lie in the bytes that would be dropped.
fn is_torn(failure: &ScanFailure, file_len: u64, file: &mut File, lsn: Lsn) -> io::Result<bool> {
    let looks_cut = match failure.error {
        FrameError::Short { .. } => true,
        FrameError::Version { .. } => false,
        FrameError::TooLong { .. } | FrameError::Crc { .. } => {
            failure.claimed_end.is_some_and(|end| end >= file_len)
                || rest_is_zero(file, failure.offset)?
        }
    };
    Ok(looks_cut && !tail_may_hold_frame(file, failure.offset, file_len, lsn)?)
}

fn validation_io_error(directory: &Path, step: &'static str, source: io::Error) -> Error {
    Error::ValidationScratch {
        directory: directory.to_path_buf(),
        step,
        source: Box::new(Error::Io(source)),
    }
}

fn validation_store_error(directory: &Path, step: &'static str, error: Error) -> Error {
    match error {
        Error::Store(_) => Error::ValidationScratch {
            directory: directory.to_path_buf(),
            step,
            source: Box::new(error),
        },
        other => other,
    }
}

pub(crate) fn resolve_validation_scratch(directory: &Path) -> Result<PathBuf> {
    directory
        .canonicalize()
        .map_err(|source| validation_io_error(directory, "resolve scratch directory", source))
}

fn validation_projection(dir: &Path, scratch: &Path) -> Result<crate::Projections> {
    let temporary = resolve_validation_scratch(scratch)?;
    let journal = dir
        .canonicalize()
        .map_err(|source| validation_io_error(scratch, "canonicalize journal", source))?;
    let overlap = temporary.starts_with(&journal);
    #[cfg(unix)]
    let overlap = overlap
        || source_in_ancestry(&journal, &temporary)
            .map_err(|source| validation_io_error(scratch, "check journal isolation", source))?;
    if overlap {
        return Err(validation_io_error(
            scratch,
            "check journal isolation",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "validation temporary directory must be outside the journal",
            ),
        ));
    }
    let file = validation_file(&temporary, scratch)?;
    validation_projection_from_file(file, scratch)
}

fn validation_file(directory: &Path, context_directory: &Path) -> Result<File> {
    let file = tempfile::tempfile_in(directory).map_err(|source| {
        validation_io_error(context_directory, "create anonymous file", source)
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| {
                validation_io_error(context_directory, "set private file permissions", source)
            })?;
    }
    Ok(file)
}

fn validation_projection_from_file(file: File, scratch: &Path) -> Result<crate::Projections> {
    crate::Projections::for_validation(file)
        .map_err(|error| validation_store_error(scratch, "initialize validation database", error))
}

fn apply_validation_batch(
    projections: &mut crate::Projections,
    batch: &[(Lsn, u32, Entry)],
    scratch: &Path,
) -> Result<u64> {
    projections
        .apply_batch(batch.iter().map(|(lsn, crc, entry)| (*lsn, *crc, entry)))
        .map_err(|error| validation_store_error(scratch, "apply validation batch", error))
}

#[cfg(unix)]
fn source_in_ancestry(source: &Path, directory: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    let source = fs::metadata(source)?;
    for ancestor in directory.ancestors() {
        let ancestor = fs::metadata(ancestor)?;
        if (source.dev(), source.ino()) == (ancestor.dev(), ancestor.ino()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Walk every segment and classify what is there, without changing a byte: segments are opened
/// for reading only, nothing is created there and nothing is truncated. Completed-contract
/// validation uses a disposable temporary projection. Corruption and version skew
/// are refused here, identically on both paths; a torn tail is *reported* in
/// [`OpenReport::torn`] for the caller to act on — or not. An unsupported required kind refuses
/// unconditionally: the frame is intact by construction (`scan_reader_detailed` only reports it
/// past `verify`), so it is never eligible for torn-tail treatment the way a cut or corrupt frame
/// in the last segment is.
pub(crate) fn survey(dir: &Path) -> Result<(OpenReport, Layout)> {
    survey_with_bound(dir, None, None, None)
}

fn survey_with_bound(
    dir: &Path,
    stop_at: Option<Lsn>,
    validated: Option<&ValidatedPrefix>,
    validation_scratch: Option<&Path>,
) -> Result<(OpenReport, Layout)> {
    survey_with_projection(dir, stop_at, validated, validation_scratch, None)
}

fn survey_with_projection(
    dir: &Path,
    stop_at: Option<Lsn>,
    validated: Option<&ValidatedPrefix>,
    validation_scratch: Option<&Path>,
    canonical: Option<&mut Option<crate::Projections>>,
) -> Result<(OpenReport, Layout)> {
    #[cfg(test)]
    VALIDATION_PASSES.with(|count| {
        let (scans, folds) = count.get();
        count.set((scans + 1, folds));
    });
    let segments: Vec<_> = list_segments(dir)?
        .into_iter()
        .filter(|segment| stop_at.is_none_or(|stop| *segment <= stop.segment()))
        .collect();
    let mut report = OpenReport::default();
    let mut last_len = 0;
    let mut buf = Vec::new();
    let mut has_completed_contract = false;
    let mut has_recovery_contract = false;
    let mut fingerprint = blake3::Hasher::new();
    for (i, &no) in segments.iter().enumerate() {
        let path = segment_path(dir, no);
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len().min(
            stop_at
                .filter(|stop| stop.segment() == no)
                .map_or(u64::MAX, Lsn::offset),
        );
        let scan = {
            let mut reader = BufReader::new(&mut file);
            scan_reader_detailed(&mut (&mut reader).take(file_len), &mut buf)?
        };
        has_completed_contract |= scan.has_completed_contract;
        has_recovery_contract |= scan.has_recovery_contract;
        if scan.frames != 0 {
            fingerprint.update(&no.to_le_bytes());
            fingerprint.update(&scan.good_end.to_le_bytes());
            fingerprint.update(&scan.fingerprint);
        }
        report.frames += scan.frames;
        last_len = scan.good_end;
        let Some(stop) = scan.stop else {
            continue;
        };
        let failure = match stop {
            ScanStop::UnsupportedKind { offset, kind } => {
                return Err(Error::RequiredKind {
                    lsn: lsn_at(no, offset)?,
                    kind,
                });
            }
            ScanStop::Malformed(failure) => failure,
        };
        let lsn = lsn_at(no, failure.offset)?;
        let last = i + 1 == segments.len();
        if last && is_torn(&failure, file_len, &mut file, lsn)? {
            report.torn = Some(Torn {
                lsn,
                dropped_bytes: file_len - scan.good_end,
            });
            continue;
        }
        return Err(frame_error(lsn, failure.error));
    }
    report.end = match segments.last() {
        Some(&no) => lsn_at(no, last_len)?,
        None => Lsn::ZERO,
    };
    let prefix = ValidatedPrefix {
        first_segment: segments.first().copied(),
        fingerprint: *fingerprint.finalize().as_bytes(),
    };
    if (canonical.is_some() || has_recovery_contract)
        && segments
            .first()
            .is_some_and(|first| *first != FIRST_SEGMENT)
    {
        return Err(Error::RecoveryStore(
            crate::error::RecoveryStoreError::IncompletePrefix,
        ));
    }
    if canonical.is_some()
        || (has_completed_contract
            && segments.first() == Some(&FIRST_SEGMENT)
            && validated != Some(&prefix))
    {
        #[cfg(test)]
        VALIDATION_PASSES.with(|count| {
            let (scans, folds) = count.get();
            count.set((scans, folds + 1));
        });
        let projections = fold_validation_prefix(dir, report.end, validation_scratch)?;
        if let Some(output) = canonical {
            *output = Some(projections);
        }
    }
    Ok((
        report,
        Layout {
            segments,
            last_len,
            validated: prefix,
        },
    ))
}

fn fold_validation_prefix(
    dir: &Path,
    end: Lsn,
    validation_scratch: Option<&Path>,
) -> Result<crate::Projections> {
    let reader = Reader {
        dir: dir.to_path_buf(),
        stop_at: Some(end),
        validated: None,
        validation_scratch: validation_scratch.map(Path::to_path_buf),
    };
    let scratch = validation_scratch.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let mut projections = validation_projection(dir, &scratch)?;
    let mut batch = Vec::with_capacity(crate::projections::BATCH);
    for frame in reader.witnessed_from(Lsn::ZERO)? {
        batch.push(frame?);
        if batch.len() == crate::projections::BATCH {
            apply_validation_batch(&mut projections, &batch, &scratch)?;
            batch.clear();
        }
    }
    apply_validation_batch(&mut projections, &batch, &scratch)?;
    Ok(projections)
}

/// Drop the torn tail [`survey`] reported: truncate its segment to the frame boundary before it,
/// sync the file and the directory, and log `journal.torn{lsn}`.
///
/// The one function in this crate that opens a segment for writing on open's behalf, and
/// [`repair`], reached only from the writer entry points, is its only caller. A
/// `mutate: bool` on `survey` would put both behaviours behind one door, and its wrong value
/// truncates a live journal.
fn truncate_torn(dir: &Path, torn: Torn) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .open(segment_path(dir, torn.lsn.segment()))?;
    file.set_len(torn.lsn.offset())?;
    file.sync_all()?;
    sync_dir(dir)?;
    let dropped_bytes = torn.dropped_bytes;
    tracing::warn!(target: "journal", lsn = %torn.lsn, dropped_bytes, "journal.torn");
    Ok(())
}

/// Validate every segment and drop a torn tail: the writer entry points call this, while a
/// reader uses [`survey`] and leaves the bytes where they are.
pub(crate) fn repair(
    dir: &Path,
    validation_scratch: Option<&Path>,
) -> Result<(OpenReport, Layout)> {
    let (report, layout) = survey_with_bound(dir, None, None, validation_scratch)?;
    if let Some(torn) = report.torn {
        truncate_torn(dir, torn)?;
    }
    Ok((report, layout))
}

/// Whether the record at an lsn is still in the log ([`Reader::locate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// Its segment is before the first present one: archived, not lost. The frame is gone, so
    /// there is nothing to witness and nothing is refused for that.
    Archived,
    /// An intact frame starts there, carrying this `crc32c` over `ver ‖ kind ‖ payload`.
    ///
    /// The checksum is what tells *this* frame from another intact one at the same offset: an
    /// lsn is a position (`segment ‖ offset`), so a rewritten segment presents the same lsn for
    /// different bytes. A record's identity is the pair — and the checksum is only
    /// ever compared against one already stored, never used to find anything.
    ///
    /// **Thirty-two bits, and error-detecting.** *Identity* here means exactly *the frame at
    /// this position is not the one that was read before*, detected where nobody was trying to
    /// hide it: unchosen rewrites collide at 2⁻³², and anyone who can rewrite a frame can
    /// rewrite it to whatever checksum they like. A `crc32c` is not a name for a record and
    /// nothing downstream may treat it as one.
    Frame {
        /// The frame header's `crc32c`.
        crc: u32,
    },
    /// Nothing is visible there in this reader's view: the position is at or past its open-time
    /// torn-tail bound, at or past the end of a present segment, or in a segment after the last
    /// present one (or the journal has no segment at all). The bound can hide bytes that landed
    /// after [`Reader::open`]; without one, bytes missing from the end were truncated or removed.
    Missing,
}

/// Reads records from a journal directory. Segments are listed at each
/// `iter_from`, so a reader created before the writer rotated still sees the new segment —
/// unless [`Reader::open`] bounded it at a torn tail, which is a snapshot taken once and never
/// re-surveyed (see [`Reader::open`]). A reader that must keep up with a live writer is a
/// [`Reader::new`] over a directory [`crate::open`] has already repaired.
#[derive(Clone, Debug)]
pub struct Reader {
    dir: PathBuf,
    /// Where an unrepaired torn tail starts, when [`Reader::open`] found one. The reader stops
    /// there instead of reading bytes whose `append` never returned — the read path's answer to
    /// the truncation the writer's path performs.
    stop_at: Option<Lsn>,
    pub(crate) validated: Option<ValidatedPrefix>,
    pub(crate) validation_scratch: Option<PathBuf>,
}

#[cfg(test)]
pub(crate) fn validation_counts() -> (u64, u64) {
    VALIDATION_PASSES.with(std::cell::Cell::get)
}

impl Reader {
    pub(crate) fn canonical_recovery_projection(
        &self,
    ) -> Result<(ValidatedPrefix, crate::Projections)> {
        let mut canonical = None;
        let (report, layout) = survey_with_projection(
            &self.dir,
            self.stop_at,
            self.validated.as_ref(),
            self.validation_scratch.as_deref(),
            Some(&mut canonical),
        )?;
        if report.torn.is_some() {
            return Err(Error::RecoveryStore(
                crate::error::RecoveryStoreError::JournalUncertain,
            ));
        }
        let projections = canonical.ok_or(Error::RecoveryStore(
            crate::error::RecoveryStoreError::Uncertified,
        ))?;
        Ok((layout.validated, projections))
    }

    pub(crate) fn validate_completed_contracts(&self) -> Result<Self> {
        let (report, layout) = survey_with_bound(
            &self.dir,
            self.stop_at,
            self.validated.as_ref(),
            self.validation_scratch.as_deref(),
        )?;
        Ok(Self {
            dir: self.dir.clone(),
            stop_at: Some(report.end),
            validated: Some(layout.validated),
            validation_scratch: self.validation_scratch.clone(),
        })
    }
    /// Validate every frame and return a reader without changing the journal or its cache.
    /// Completed-contract validation uses an anonymous temporary file with an 8 MiB database
    /// cache. The file has mode `0600` on Unix and may contain task or note text from the
    /// canonical projection. On tmpfs, its filesystem memory is outside process RSS.
    /// Temporary-storage failures name the scratch directory and operation and refuse open
    /// before any repair or cache reset.
    /// The temporary directory must be outside the journal directory. This is what a
    /// **one-shot offline read** — `dudes replay` — uses: the directory must already exist
    /// ([`Error::NoJournal`] if there is nothing there, a mistyped path being a question rather
    /// than an empty journal to invent), must contain a segment ([`Error::NotAJournal`] if the
    /// directory exists but has none), a torn tail is reported rather than dropped, and the reader
    /// stops before it. The daemon uses [`crate::open_with_scratch`], which validates the same way and, being
    /// the single writer, initializes a new journal or truncates a torn tail.
    ///
    /// **The bound is fixed at open time** and never re-surveyed: it records where the log ended
    /// when this reader was made. If the writer goes on to complete that frame and append more,
    /// this reader keeps stopping at the old position and reports a clean end there. A caller
    /// that must see the continuation opens a new reader; a caller that follows a live writer
    /// wants [`Reader::new`] over an already-repaired directory, not this.
    ///
    /// Both [`Reader::iter_from`] and [`Reader::locate`] honor the bound: iteration ends there,
    /// and locating that position or any later one returns [`Location::Missing`].
    pub fn open(dir: &Path) -> Result<(Self, OpenReport)> {
        // Not `Path::is_dir`: that is `metadata(..).map(..).unwrap_or(false)`, which would fold
        // EACCES, ELOOP and EIO into "no journal here" and invite the operator to create one over
        // a journal the OS merely refused to describe. "Absent" and "refused" are different
        // answers and only the first is `NoJournal`.
        match fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::NoJournal {
                    dir: dir.to_path_buf(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::NoJournal {
                    dir: dir.to_path_buf(),
                });
            }
            Err(e) => return Err(Error::Io(e)),
        }
        let (report, layout) = survey(dir)?;
        if layout.segments.is_empty() {
            return Err(Error::NotAJournal {
                dir: dir.to_path_buf(),
            });
        }
        Ok((
            Self {
                dir: dir.to_path_buf(),
                stop_at: report.torn.map(|torn| torn.lsn),
                validated: Some(layout.validated),
                validation_scratch: None,
            },
            report,
        ))
    }

    /// A reader over a directory that [`crate::open`] has already repaired in this process: it
    /// reads to the end of the log, frames appended after it was created included.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            stop_at: None,
            validated: None,
            validation_scratch: None,
        }
    }

    /// The journal directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Is the record at `lsn` still in the log, and — when it is — which frame is there?
    /// `lsn` must be a frame boundary the journal handed out. One frame is read, never a
    /// rescan; bytes at the position that are not an intact frame are corruption (`Corrupt`,
    /// or `VersionSkew`), not a [`Location`] — the record was read intact once, so the writer
    /// never wrote there again.
    ///
    /// [`Location::Frame`] carries the frame's `crc32c`, which is the whole of the answer to
    /// *which* frame: the position alone cannot distinguish a rewritten segment from the one
    /// the caller read.
    ///
    /// A reader returned by [`Reader::open`] reports [`Location::Missing`] at or after its fixed
    /// open-time torn-tail bound, matching [`Reader::iter_from`]'s view of the same snapshot.
    pub fn locate(&self, lsn: Lsn) -> Result<Location> {
        if self.stop_at.is_some_and(|stop| lsn >= stop) {
            return Ok(Location::Missing);
        }
        let segments = list_segments(&self.dir)?;
        let (Some(&first), Some(&last)) = (segments.first(), segments.last()) else {
            return Ok(Location::Missing);
        };
        if lsn.segment() < first {
            return Ok(Location::Archived);
        }
        if lsn.segment() > last {
            return Ok(Location::Missing);
        }
        let mut file = File::open(segment_path(&self.dir, lsn.segment()))?;
        file.seek(SeekFrom::Start(lsn.offset()))?;
        let mut buf = Vec::new();
        match read_next_frame(&mut BufReader::new(file), lsn.offset(), &mut buf)? {
            Next::Frame(header) => Ok(Location::Frame { crc: header.crc }),
            Next::End => Ok(Location::Missing),
            // Unreachable: `lsn` was read intact once, and `survey` already refuses to open a
            // journal (or repair one for `crate::open`) that holds an unsupported required kind.
            Next::UnsupportedKind(header) => Err(Error::RequiredKind {
                lsn,
                kind: header.kind,
            }),
            Next::Failed(error, _) => Err(frame_error(lsn, error)),
        }
    }

    /// Entries from the frame at `lsn` onward, across segment boundaries. `lsn` must be a
    /// frame boundary the journal handed out (or `Lsn::ZERO` for everything); a segment before
    /// the first present one is skipped as archived.
    pub fn iter_from(&self, lsn: Lsn) -> Result<Iter> {
        Ok(Iter(self.witnessed_from(lsn)?))
    }

    pub(crate) fn witnessed_between(&self, start: Lsn, end: Lsn) -> Result<Witnessed> {
        if start.segment() < FIRST_SEGMENT
            || end <= start
            || end
                .segment()
                .checked_sub(start.segment())
                .is_none_or(|distance| distance > 1)
            || self.stop_at.is_some_and(|bound| end > bound)
        {
            return Err(Error::RecoveryStore(
                crate::error::RecoveryStoreError::AppendMismatch,
            ));
        }
        let mut segments = vec![start.segment()];
        if end.segment() != start.segment() && end.offset() != 0 {
            segments.push(end.segment());
        }
        Ok(Witnessed {
            dir: self.dir.clone(),
            segments,
            next: 0,
            current: None,
            start_offset: start.offset(),
            boundary: ReadBoundary::Exact(end),
            done: false,
            buf: Vec::new(),
        })
    }

    pub(crate) fn witnessed_from(&self, lsn: Lsn) -> Result<Witnessed> {
        let segments = list_segments(&self.dir)?;
        let start = segments.partition_point(|no| *no < lsn.segment());
        let offset = if segments.get(start) == Some(&lsn.segment()) {
            lsn.offset()
        } else {
            0
        };
        Ok(Witnessed {
            dir: self.dir.clone(),
            segments,
            next: start,
            current: None,
            start_offset: offset,
            boundary: ReadBoundary::Live(self.stop_at),
            done: false,
            buf: Vec::new(),
        })
    }
}

/// The segment a [`Witnessed`] is reading.
#[derive(Debug)]
struct OpenSegment {
    segment: u32,
    reader: BufReader<File>,
    /// Offset of the next frame.
    offset: u64,
}

/// The iterator [`Reader::iter_from`] returns: `(lsn, entry)` per frame. Fused on error: once
/// an item is `Err`, the iterator is finished — a reader that skipped a bad frame would pair
/// real entries with wrong lsns.
#[derive(Debug)]
pub struct Iter(Witnessed);

impl Iterator for Iter {
    type Item = Result<(Lsn, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|item| item.map(|(lsn, _, entry)| (lsn, entry)))
    }
}

#[derive(Clone, Copy, Debug)]
enum ReadBoundary {
    Live(Option<Lsn>),
    Exact(Lsn),
}

impl ReadBoundary {
    const fn end(self) -> Option<Lsn> {
        match self {
            Self::Live(end) => end,
            Self::Exact(end) => Some(end),
        }
    }
    const fn exact_end(self) -> Option<Lsn> {
        match self {
            Self::Live(_) => None,
            Self::Exact(end) => Some(end),
        }
    }
}

/// [`Iter`] with each entry's frame checksum: what [`Reader::witnessed_from`] returns.
#[derive(Debug)]
pub(crate) struct Witnessed {
    dir: PathBuf,
    segments: Vec<u32>,
    next: usize,
    current: Option<OpenSegment>,
    start_offset: u64,
    boundary: ReadBoundary,
    done: bool,
    buf: Vec<u8>,
}

impl Witnessed {
    /// Open the next segment, seeking into the first one.
    fn advance(&mut self) -> Result<bool> {
        let Some(&segment) = self.segments.get(self.next) else {
            return Ok(false);
        };
        let mut file = File::open(segment_path(&self.dir, segment))?;
        let offset = std::mem::take(&mut self.start_offset);
        let lsn = lsn_at(segment, offset)?;
        let file_len = file.metadata()?.len();
        let another_segment_follows = self.next + 1 < self.segments.len();
        if self.boundary.end().is_none_or(|stop| lsn < stop)
            && (offset > file_len || (offset == file_len && another_segment_follows))
        {
            return Err(frame_error(lsn, FrameError::Short { need: HEADER_LEN }));
        }
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        self.current = Some(OpenSegment {
            segment,
            reader: BufReader::new(file),
            offset,
        });
        self.next += 1;
        Ok(true)
    }

    /// One frame from the current segment: `None` at its end, `Some(Err)` on a frame that is
    /// not what the writer produced.
    fn read_frame(&mut self) -> Option<Result<(Lsn, Header)>> {
        let current = self.current.as_mut()?;
        let lsn = match lsn_at(current.segment, current.offset) {
            Ok(lsn) => lsn,
            Err(e) => return Some(Err(e)),
        };
        if self.boundary.end().is_some_and(|stop| lsn >= stop) {
            if self.boundary.exact_end().is_some_and(|end| lsn > end) {
                return Some(Err(Error::RecoveryStore(
                    crate::error::RecoveryStoreError::AppendMismatch,
                )));
            }
            return None;
        }
        let last = self.next == self.segments.len();
        match read_next_frame(&mut current.reader, current.offset, &mut self.buf) {
            Ok(Next::End) => {
                if last
                    && self.boundary.exact_end().is_some_and(|end| {
                        end != lsn
                            && !(end.offset() == 0
                                && current.segment.checked_add(1) == Some(end.segment()))
                    })
                {
                    return Some(Err(Error::RecoveryStore(
                        crate::error::RecoveryStoreError::AppendMismatch,
                    )));
                }
                None
            }
            Ok(Next::Frame(header)) => {
                current.offset += header.frame_len() as u64;
                if let Some(end) = self.boundary.exact_end() {
                    match lsn_at(current.segment, current.offset) {
                        Ok(after) if after > end => {
                            return Some(Err(Error::RecoveryStore(
                                crate::error::RecoveryStoreError::AppendMismatch,
                            )));
                        }
                        Err(error) => return Some(Err(error)),
                        _ => {}
                    }
                }
                Some(Ok((lsn, header)))
            }
            // Unreachable for a journal `survey` has already passed (`Reader::open`, or
            // `Reader::new` over a directory `crate::open` repaired first): both refuse an
            // unsupported required kind before any reader walks this far. Kept as a refusal
            // rather than a panic so a future caller that skips that gate fails safe.
            Ok(Next::UnsupportedKind(header)) => Some(Err(Error::RequiredKind {
                lsn,
                kind: header.kind,
            })),
            Ok(Next::Failed(FrameError::Short { .. }, _))
                if last && self.boundary.exact_end().is_none() =>
            {
                None
            }
            Ok(Next::Failed(error, _)) => Some(Err(frame_error(lsn, error))),
            Err(e) => Some(Err(e.into())),
        }
    }

    /// The next item, before fusing.
    fn step(&mut self) -> Option<Result<(Lsn, u32, Entry)>> {
        loop {
            if self.current.is_none() {
                match self.advance() {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(e) => return Some(Err(e)),
                }
            }
            let Some(frame) = self.read_frame() else {
                self.current = None;
                continue;
            };
            let (lsn, header) = match frame {
                Ok(f) => f,
                Err(e) => return Some(Err(e)),
            };
            if RecordKind::from_tag(header.kind).is_none()
                && !completed_contract_payload(header, &self.buf)
            {
                if self.boundary.exact_end().is_some() {
                    return Some(Err(Error::RecoveryStore(
                        crate::error::RecoveryStoreError::AppendMismatch,
                    )));
                }
                tracing::debug!(target: "journal", lsn = %lsn, kind = header.kind, "journal.skip_unknown_kind");
                continue;
            }
            return Some(
                serde_json::from_slice::<Entry>(&self.buf)
                    .map_err(|source| Error::Decode { lsn, source })
                    .and_then(|entry| {
                        dudes_proto::validate_instruction_record(&entry.record).map_err(
                            |reason| Error::Instruction {
                                lsn,
                                reason: reason.into(),
                            },
                        )?;
                        let contract = completed_contract_payload(header, &self.buf);
                        dudes_proto::recovery::validate_record_shape(&entry.record)
                            .map_err(|reason| Error::RecoveryRecord { lsn, reason })?;
                        if contract
                            && (header.ver != frame::REQUIRED_VERSION
                                || header.kind != entry.record.kind().tag())
                        {
                            if recovery::is_kind(entry.record.kind())
                                || RecordKind::from_tag(header.kind).is_some_and(recovery::is_kind)
                            {
                                return Err(Error::RecoveryFrame {
                                    lsn,
                                    kind: header.kind,
                                    version: header.ver,
                                    expected_kind: entry.record.kind().tag(),
                                });
                            }
                            if is_instruction_kind(entry.record.kind())
                                || RecordKind::from_tag(header.kind)
                                    .is_some_and(is_instruction_kind)
                            {
                                return Err(Error::InstructionFrame {
                                    lsn,
                                    kind: header.kind,
                                    version: header.ver,
                                    expected_kind: entry.record.kind().tag(),
                                });
                            }
                            if matches!(
                                entry.record,
                                dudes_proto::Record::TaughtNoteTurnDecided { .. }
                                    | dudes_proto::Record::TaughtNoteOnlyTurnIssued { .. }
                            ) || matches!(
                                RecordKind::from_tag(header.kind),
                                Some(
                                    RecordKind::TaughtNoteTurnDecided
                                        | RecordKind::TaughtNoteOnlyTurnIssued
                                )
                            ) {
                                return Err(Error::TaughtNoteFrame {
                                    lsn,
                                    kind: header.kind,
                                    version: header.ver,
                                    expected_kind: entry.record.kind().tag(),
                                });
                            }
                            return Err(Error::MailWakeFrame {
                                lsn,
                                kind: header.kind,
                                version: header.ver,
                                expected_kind: entry.record.kind().tag(),
                            });
                        }
                        Ok((lsn, header.crc, entry))
                    }),
            );
        }
    }
}

impl Iterator for Witnessed {
    type Item = Result<(Lsn, u32, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.step();
        if matches!(item, None | Some(Err(_))) {
            self.done = true;
        }
        item
    }
}

fn is_instruction_kind(kind: RecordKind) -> bool {
    matches!(
        kind,
        RecordKind::InstructionObservationStarted
            | RecordKind::InstructionObservationFinished
            | RecordKind::TicketInstructionReviewed
            | RecordKind::InstructionAttemptBoundarySettled
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use dudes_proto::Record;
    use dudes_types::{AgentId, Timestamp};

    use super::*;
    use crate::segment;
    use crate::writer::{Fsync, JournalOptions, Writer};

    /// A stamp far enough from the epoch to have the width a real one has.
    const AT_BASE: i64 = 1_787_394_600_000;

    fn at(i: u64) -> Timestamp {
        Timestamp::from_unix_millis(AT_BASE + i64::try_from(i).unwrap())
    }

    fn record(i: u64) -> Entry {
        Entry {
            at: at(i),
            record: Record::Pause {
                agent: AgentId::from_parts(i, 1),
                browser: None,
            },
        }
    }

    fn fill(dir: &Path, n: u64, rotate_at: u64) -> Vec<Lsn> {
        let mut w = Writer::start(
            dir,
            1,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at,
                checkpoint_every: 0,
            },
        )
        .unwrap();
        (1..=n).map(|i| w.append(&record(i)).unwrap()).collect()
    }

    /// The scanner stops at the first bad frame and reports where the good bytes end, so the
    /// torn-tail rule can be applied from its output alone.
    #[test]
    fn scan_reports_good_end_and_failure() {
        let mut bytes = Vec::new();
        frame::encode(1, b"a", &mut bytes).unwrap();
        frame::encode(2, b"bb", &mut bytes).unwrap();
        let clean = scan(&bytes);
        assert_eq!(clean.frames, 2);
        assert_eq!(clean.good_end, bytes.len() as u64);
        assert_eq!(clean.failure, None);
        let whole = bytes.len();
        bytes.truncate(whole - 1);
        let torn = scan(&bytes);
        assert_eq!(torn.frames, 1);
        assert_eq!(torn.good_end, 13);
        assert_eq!(
            torn.failure,
            Some(ScanFailure {
                offset: 13,
                error: FrameError::Short { need: 14 },
                claimed_end: Some(whole as u64),
            })
        );
        assert_eq!(scan(&[]).failure, None);
    }

    /// `scan` is panic-free on any input *on any target*: the extent of the largest header a
    /// disk can hold is reported as the number it is, `HEADER_LEN + u32::MAX`. The header is
    /// read before anything checks `len`, so computing that extent through `usize` — 32 bits
    /// wide on a 32-bit target — overflows.
    ///
    /// In debug that is a panic, and it is what makes this worth fixing: it breaks the
    /// panic-free-on-any-input contract that `scan` and `frame::decode` advertise and that the
    /// `journal_frame` fuzz target asserts.
    ///
    /// In release the wrap is fail-closed rather than data loss, and the direction matters. It
    /// can only wrap *down*, to at most 11; `claimed_end`'s sole consumer is `is_torn`, which is
    /// monotone in `end`; and this line is reached only once a full twelve-byte header has been
    /// read at `offset`, so `file_len >= offset + 12` exceeds any wrapped extent and the
    /// `end >= file_len` test is unconditionally false. The damaged header therefore never
    /// looks like a cut — the effect is that `open` *refuses* a journal it would otherwise have
    /// repaired. That costs availability; invariant 1 is never at risk.
    ///
    /// The arithmetic cannot fail on a 64-bit host. With an unchecked add this reddens under
    /// `cargo +nightly miri test --target i686-unknown-linux-gnu`.
    #[test]
    fn a_maximal_len_does_not_overflow_claimed_end() {
        let header = Header {
            len: u32::MAX,
            ver: frame::VERSION,
            kind: 1,
            crc: 0,
        };
        assert_eq!(
            scan(&header.to_bytes()).failure,
            Some(ScanFailure {
                offset: 0,
                error: FrameError::TooLong { len: u32::MAX },
                claimed_end: Some(HEADER_LEN as u64 + u64::from(u32::MAX)),
            })
        );
    }

    /// A header `scan` refuses allocates nothing. `read_next_frame` has read only twelve bytes
    /// when it decides, so the `TooLong` check must stay *above* `buf.resize(len, 0)`: below it,
    /// twelve bytes of hostile input drive a 4.29 GB allocation on the way to being rejected.
    /// Shipped code is correct — this pins it, because the extent its sibling test asserts is
    /// reported three lines from an allocation bound nothing else guards, and the only other
    /// thing that would catch a regression is a fuzz target no CI job builds. `capacity` is
    /// asserted as well as emptiness: it is the allocation that matters, and a buffer can be
    /// emptied after it has been grown.
    #[test]
    fn a_refused_header_allocates_nothing() {
        let header = Header {
            len: u32::MAX,
            ver: frame::VERSION,
            kind: 1,
            crc: 0,
        };
        let mut buf = Vec::new();
        let outcome = scan_reader(&mut io::Cursor::new(header.to_bytes()), &mut buf).unwrap();
        assert_eq!(
            outcome.failure.map(|f| f.error),
            Some(FrameError::TooLong { len: u32::MAX })
        );
        assert!(buf.is_empty(), "a refused header must not fill the buffer");
        assert_eq!(buf.capacity(), 0, "a refused header must not allocate");
    }

    /// `iter_from(Lsn::ZERO)` yields every record in order across segment files, and
    /// `iter_from(lsn)` starts exactly at that record.
    #[test]
    fn iter_crosses_segments_and_starts_at_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 30, 256);
        let reader = Reader::new(dir.path());
        let all: Vec<_> = reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(all.len(), 30);
        assert_eq!(all.iter().map(|(l, _)| *l).collect::<Vec<_>>(), lsns);
        assert_eq!(all[4].1, record(5));
        let from_mid: Vec<_> = reader
            .iter_from(lsns[17])
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(from_mid.len(), 13);
        assert_eq!(from_mid[0].0, lsns[17]);
        // A segment before the first present one is archived, not an error.
        assert_eq!(
            reader
                .iter_from(Lsn::from_parts(0, 99).unwrap())
                .unwrap()
                .count(),
            30
        );
    }

    /// Invariant 1: an lsn in a present segment is a position the journal handed out, so seeking
    /// past that segment's bytes is corruption rather than permission to continue at the next
    /// segment or report a clean tail. Exact EOF remains the live tail only in the final segment.
    #[test]
    fn iter_from_past_a_present_segments_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 30, 256);
        let segments = list_segments(dir.path()).unwrap();
        assert!(segments.len() > 1, "fixture must rotate");

        let earlier = segments[0];
        let earlier_len = fs::metadata(segment_path(dir.path(), earlier))
            .unwrap()
            .len();
        for offset in [earlier_len, earlier_len + 1] {
            let requested = Lsn::from_parts(earlier, offset).unwrap();
            let mut iter = Reader::new(dir.path()).iter_from(requested).unwrap();
            assert!(
                matches!(
                    iter.next(),
                    Some(Err(Error::Corrupt {
                        lsn,
                        reason: FrameError::Short { need: HEADER_LEN }
                    })) if lsn == requested
                ),
                "offset {offset}"
            );
            assert!(iter.next().is_none(), "an error must fuse the iterator");
        }

        let later = Lsn::from_parts(segments[1], 0).unwrap();
        let later_record = lsns.iter().position(|lsn| *lsn == later).unwrap() as u64 + 1;
        assert_eq!(
            Reader::new(dir.path())
                .iter_from(later)
                .unwrap()
                .next()
                .unwrap()
                .unwrap(),
            (later, record(later_record))
        );

        let final_segment = *segments.last().unwrap();
        let final_len = fs::metadata(segment_path(dir.path(), final_segment))
            .unwrap()
            .len();
        let beyond_final = Lsn::from_parts(final_segment, final_len + 1).unwrap();
        let mut beyond = Reader::new(dir.path()).iter_from(beyond_final).unwrap();
        assert!(matches!(
            beyond.next(),
            Some(Err(Error::Corrupt {
                lsn,
                reason: FrameError::Short { need: HEADER_LEN }
            })) if lsn == beyond_final
        ));
        assert!(beyond.next().is_none(), "an error must fuse the iterator");

        let final_end = Lsn::from_parts(final_segment, final_len).unwrap();
        assert!(
            Reader::new(dir.path())
                .iter_from(final_end)
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// A fixed reader snapshot keeps hiding the torn lsn after the writer repairs that tail and
    /// rotates. The repaired length and later intact record are controls that make the earlier
    /// segment's exact EOF otherwise look like the invalid seek guarded by [`Witnessed::advance`].
    #[test]
    fn open_reader_keeps_its_tail_bound_after_repair_and_rotation() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 2, segment::DEFAULT_ROTATE_AT);
        let tail_segment = lsns.last().unwrap().segment();
        let path = segment_path(dir.path(), tail_segment);
        let tail_at = lsn_at(tail_segment, fs::metadata(&path).unwrap().len()).unwrap();

        let mut frame = Vec::new();
        let entry = record(3);
        let payload = serde_json::to_vec(&entry).unwrap();
        frame::encode(entry.record.kind().tag(), &payload, &mut frame).unwrap();
        let mut raw = OpenOptions::new().append(true).open(&path).unwrap();
        raw.write_all(&frame[..=HEADER_LEN]).unwrap();
        drop(raw);

        let (reader, observed) = Reader::open(dir.path()).unwrap();
        assert_eq!(observed.torn.map(|torn| torn.lsn), Some(tail_at));

        let before_repair = fs::metadata(&path).unwrap().len();
        let (mut writer, repaired) = crate::open(
            dir.path(),
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: segment::DEFAULT_ROTATE_AT,
                checkpoint_every: 0,
            },
        )
        .unwrap();
        assert_eq!(repaired.torn.map(|torn| torn.lsn), Some(tail_at));
        assert!(before_repair > tail_at.offset());
        assert_eq!(fs::metadata(&path).unwrap().len(), tail_at.offset());

        let later = writer.append(&entry).unwrap();
        assert!(
            later.segment() > tail_segment,
            "writer open must rotate after repairing the non-empty segment"
        );
        assert_eq!(
            Reader::new(dir.path())
                .iter_from(later)
                .unwrap()
                .next()
                .unwrap()
                .unwrap(),
            (later, entry)
        );
        assert!(
            reader.iter_from(tail_at).unwrap().next().is_none(),
            "the old reader must keep its open-time tail bound"
        );
    }

    /// Journal format: a reader that does not know `kind` skips `len` bytes and continues.
    #[test]
    fn unknown_kind_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        let payload = serde_json::to_vec(&record(1)).unwrap();
        frame::encode(1000, b"{\"t\":\"from_the_future\"}", &mut bytes).unwrap();
        frame::encode(RecordKind::Pause.tag(), &payload, &mut bytes).unwrap();
        fs::write(segment_path(dir.path(), 1), &bytes).unwrap();
        let (reader, report) = Reader::open(dir.path()).unwrap();
        assert_eq!(report.frames, 2);
        let got: Vec<_> = reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(got, vec![(Lsn::from_parts(1, 35).unwrap(), record(1))]);
    }

    /// Torn tail is the *final* frame only: a damaged header whose claimed extent runs
    /// past the end is followed by an intact frame, and the search finds it; a real cut, or a
    /// garbled final frame, has nothing after it.
    #[test]
    fn find_frame_sees_past_a_damaged_header() {
        let mut bytes = Vec::new();
        frame::encode(1, b"first", &mut bytes).unwrap();
        let second_at = bytes.len();
        frame::encode(2, b"second", &mut bytes).unwrap();
        bytes[1] ^= 0x04; // first frame now claims 1029 bytes: past EOF
        assert_eq!(find_frame(&bytes), FrameSearch::Found(second_at));
        let mut cut = Vec::new();
        frame::encode(1, b"a frame cut short by a crash", &mut cut).unwrap();
        cut.truncate(cut.len() - 3);
        assert_eq!(find_frame(&cut), FrameSearch::Absent);
        let mut garbled = Vec::new();
        frame::encode(1, b"garbled", &mut garbled).unwrap();
        let n = garbled.len();
        garbled[n - 1] ^= 0xFF;
        assert_eq!(find_frame(&garbled), FrameSearch::Absent);
        assert_eq!(find_frame(&[]), FrameSearch::Absent);
        assert_eq!(find_frame(&[0u8; 1000]), FrameSearch::Absent);
    }

    /// Invariant 1: open refuses, never hangs, on bytes it cannot prove are a tail — the search
    /// that protects acknowledged frames from truncation checksums at most
    /// `FIND_FRAME_CRC_BUDGET` bytes and says when it gave up. Every fourth offset of a
    /// `[1, 0, 0xFF, 0]` region claims a ~16 MiB frame; four are charged, then `OverBudget`.
    /// A genuine frame after a few such candidates is still found inside the budget.
    #[test]
    fn find_frame_stops_at_the_checksum_budget() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes.extend([1u8, 0, 0xFF, 0].iter().cycle().take(32 * 1024 * 1024));
        assert_eq!(find_frame(&bytes), FrameSearch::OverBudget);
        // Candidates whose extent does not fit cost a header parse, not budget.
        assert_eq!(find_frame(&bytes[..4096]), FrameSearch::Absent);
        // Candidates at 12, 16 and 20 are charged (three of the four the budget allows); the
        // frame at 28 is found.
        let mut genuine = Vec::new();
        frame::encode(7, b"genuine", &mut genuine).unwrap();
        let at = HEADER_LEN + 16;
        bytes[at..at + genuine.len()].copy_from_slice(&genuine);
        assert_eq!(find_frame(&bytes), FrameSearch::Found(at));
    }

    /// Invariant 1: the reader answers "is this record still in the log" exactly, so replay
    /// can tell loss from archival: `Archived` before the first present segment, `Frame` at
    /// every lsn handed out, `Missing` past the end of a present segment and in a segment
    /// after the last present one, `Corrupt` mid-frame.
    ///
    /// `Frame` also carries the checksum of the frame that is there, and it is the
    /// one the iterator decodes at that lsn — the same witness by either route, which is what
    /// lets a projection written on the live path be checked on the replay path.
    #[test]
    fn locate_distinguishes_archived_frame_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 30, 256);
        let reader = Reader::new(dir.path());
        let last = *lsns.last().unwrap();
        assert!(last.segment() > 2, "needs several segments");
        fs::remove_file(segment_path(dir.path(), 1)).unwrap();
        let by_iteration: HashMap<Lsn, u32> = reader
            .witnessed_from(Lsn::ZERO)
            .unwrap()
            .map(|item| {
                let (lsn, crc, _) = item.unwrap();
                (lsn, crc)
            })
            .collect();
        for lsn in &lsns {
            let expect = if lsn.segment() == 1 {
                Location::Archived
            } else {
                Location::Frame {
                    crc: by_iteration[lsn],
                }
            };
            assert_eq!(reader.locate(*lsn).unwrap(), expect, "{lsn}");
        }
        // The control the equality above needs: the checksums are not all the same value, so
        // `Frame { crc }` matching is a statement about *this* frame.
        assert!(
            by_iteration.values().collect::<HashSet<_>>().len() > 1,
            "distinct records must have distinct checksums for this to discriminate"
        );
        assert_eq!(reader.locate(Lsn::ZERO).unwrap(), Location::Archived);
        let end = fs::metadata(segment_path(dir.path(), last.segment()))
            .unwrap()
            .len();
        let past_end = Lsn::from_parts(last.segment(), end).unwrap();
        assert_eq!(reader.locate(past_end).unwrap(), Location::Missing);
        let next_segment = Lsn::from_parts(last.segment() + 1, 0).unwrap();
        assert_eq!(reader.locate(next_segment).unwrap(), Location::Missing);
        let mid = Lsn::from_parts(last.segment(), last.offset() + HEADER_LEN as u64).unwrap();
        let err = reader.locate(mid).unwrap_err();
        assert!(
            matches!(err, Error::Corrupt { lsn, .. } if lsn == mid),
            "{err:?}"
        );
        // No segment at all: nothing is in the log.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            Reader::new(empty.path()).locate(lsns[0]).unwrap(),
            Location::Missing
        );
    }

    /// An error ends the iterator: starting mid-frame yields one `Corrupt` and then nothing,
    /// never a real record paired with a wrong lsn.
    #[test]
    fn an_error_ends_the_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 30, 256);
        let mid = Lsn::from_parts(lsns[3].segment(), lsns[3].offset() + HEADER_LEN as u64).unwrap();
        let items: Vec<_> = Reader::new(dir.path()).iter_from(mid).unwrap().collect();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            matches!(&items[0], Err(Error::Corrupt { lsn, .. }) if *lsn == mid),
            "{items:?}"
        );
    }

    /// Version skew on the `Iter` path: an intact frame with a future header version is
    /// `VersionSkew` at its lsn, and ends the iteration.
    #[test]
    fn version_skew_ends_the_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let lsns = fill(dir.path(), 2, segment::DEFAULT_ROTATE_AT);
        let path = segment_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        let at = bytes.len();
        let payload = serde_json::to_vec(&record(3)).unwrap();
        let header = Header {
            len: u32::try_from(payload.len()).unwrap(),
            ver: 3,
            kind: RecordKind::Pause.tag(),
            crc: frame::crc32c(3, RecordKind::Pause.tag(), &payload),
        };
        bytes.extend_from_slice(&header.to_bytes());
        bytes.extend_from_slice(&payload);
        frame::encode(RecordKind::Pause.tag(), &payload, &mut bytes).unwrap();
        fs::write(&path, &bytes).unwrap();
        let items: Vec<_> = Reader::new(dir.path())
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect();
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[1].as_ref().unwrap().0, lsns[1]);
        let skew = Lsn::from_parts(1, at as u64).unwrap();
        assert!(
            matches!(
                &items[2],
                Err(Error::VersionSkew { lsn, found: 3, expected: 2 }) if *lsn == skew
            ),
            "{items:?}"
        );
    }

    /// a `ver = 1` payload without `at` — a frame no released build can
    /// have produced — surfaces as the *existing* [`Error::Decode`] at its lsn. No new
    /// variant, and never a silent skip: a known kind that does not decode is a hard error.
    #[test]
    fn payload_without_at_is_a_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        // The agent `record(0)` names, so the control below compares against a known entry.
        let agent = AgentId::from_parts(0, 1);
        let unstamped = format!("{{\"t\":\"pause\",\"agent\":\"{agent}\"}}");
        frame::encode(RecordKind::Pause.tag(), unstamped.as_bytes(), &mut bytes).unwrap();
        fs::write(segment_path(dir.path(), 1), &bytes).unwrap();
        let (reader, _) = Reader::open(dir.path()).unwrap();
        let err = reader.iter_from(Lsn::ZERO).unwrap().next().unwrap();
        assert!(matches!(err, Err(Error::Decode { .. })), "{err:?}");

        // Control: the same frame, the same kind, the same fields — with `at` added it reads.
        // So the error above is caused by the missing field, not by the frame, the kind, or
        // malformed JSON in general.
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        let stamped = format!("{{\"at\":{AT_BASE},\"t\":\"pause\",\"agent\":\"{agent}\"}}");
        frame::encode(RecordKind::Pause.tag(), stamped.as_bytes(), &mut bytes).unwrap();
        fs::write(segment_path(dir.path(), 1), &bytes).unwrap();
        let (reader, _) = Reader::open(dir.path()).unwrap();
        let got = reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(got, (Lsn::from_parts(1, 0).unwrap(), record(0)));
    }

    /// the journal round-trips *every* [`Record`] kind as an [`Entry`] —
    /// append one sample per [`RecordKind`], read them back equal and in order.
    ///
    /// Each sample gets a **distinct** `at`, which is the control: a writer that dropped `at`,
    /// or one that stamped a constant, still round-trips when every sample shares one stamp.
    #[test]
    fn every_record_kind_round_trips_as_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::start(
            dir.path(),
            1,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: segment::DEFAULT_ROTATE_AT,
                checkpoint_every: 0,
            },
        )
        .unwrap();
        let entries: Vec<Entry> = RecordKind::KNOWN
            .iter()
            .enumerate()
            .map(|(i, kind)| Entry {
                at: at(u64::try_from(i).unwrap()),
                record: dudes_proto::record::samples::record(*kind),
            })
            .collect();
        let lsns: Vec<Lsn> = entries.iter().map(|e| w.append(e).unwrap()).collect();
        drop(w);

        let back: Vec<(Lsn, Entry)> = Reader::new(dir.path())
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(back.len(), RecordKind::KNOWN.len());
        assert_eq!(back.iter().map(|(l, _)| *l).collect::<Vec<_>>(), lsns);
        assert_eq!(
            back.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
            entries
        );
    }

    #[test]
    fn checked_append_interval_selects_segments_without_directory_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        let start = Lsn::from_parts(41, 7).unwrap();
        let end = Lsn::from_parts(42, 23).unwrap();
        let interval = Reader::new(&absent).witnessed_between(start, end).unwrap();
        assert_eq!(interval.segments, vec![41, 42]);
        assert_eq!(interval.start_offset, 7);
        assert_eq!(interval.boundary.exact_end(), Some(end));
        let too_wide = Lsn::from_parts(43, 23).unwrap();
        assert!(
            Reader::new(&absent)
                .witnessed_between(start, too_wide)
                .is_err()
        );
    }

    #[test]
    fn checked_append_interval_reads_only_its_two_real_segments_after_long_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::start(
            dir.path(),
            1,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: 1,
                checkpoint_every: 0,
            },
        )
        .unwrap();
        let record = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent: AgentId::from_parts(19, 1),
                browser: None,
            },
        };
        for _ in 0..20 {
            writer.append(&record).unwrap();
        }
        let start = writer.append(&record).unwrap();
        let second = writer.append(&record).unwrap();
        let end = writer.next_lsn().unwrap();
        let interval = writer.reader().witnessed_between(start, end).unwrap();
        assert_eq!(interval.segments, vec![start.segment(), second.segment()]);
        let entries = interval.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, start);
        assert_eq!(entries[1].0, second);
        assert_eq!(entries[0].2, record);
        assert_eq!(entries[1].2, record);
    }

    #[test]
    fn checked_append_interval_accepts_a_complete_same_segment_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::start(
            dir.path(),
            1,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: segment::DEFAULT_ROTATE_AT,
                checkpoint_every: 0,
            },
        )
        .unwrap();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent: AgentId::from_parts(19, 1),
                browser: None,
            },
        };
        let start = writer.append(&entry).unwrap();
        let end = writer.next_lsn().unwrap();
        let frames = writer
            .reader()
            .witnessed_between(start, end)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].2, entry);
    }

    #[test]
    fn checked_append_interval_refuses_an_empty_final_segment_while_live_tail_waits() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::start(
            dir.path(),
            1,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: 1,
                checkpoint_every: 1,
            },
        )
        .unwrap();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent: AgentId::from_parts(19, 1),
                browser: None,
            },
        };
        let start = writer.append(&entry).unwrap();
        let end = writer.next_lsn().unwrap();
        assert_ne!(start.segment(), end.segment());
        File::options()
            .write(true)
            .open(segment_path(dir.path(), end.segment()))
            .unwrap()
            .set_len(0)
            .unwrap();
        let strict = writer
            .reader()
            .witnessed_between(start, end)
            .unwrap()
            .collect::<Result<Vec<_>>>();
        assert!(
            strict.is_err(),
            "strict interval accepted an unconsumed end: {strict:?}"
        );
        let live = writer
            .reader()
            .iter_from(start)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].1, entry);
    }
}
