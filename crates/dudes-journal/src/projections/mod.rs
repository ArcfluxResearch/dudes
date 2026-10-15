//! The redb projections. `state.redb` is a cache: a deterministic function of a journal prefix
//! that `dudes replay` rebuilds, never written except by [`Projections::apply`] with an entry
//! whose `append` has returned.
//!
//! Tables: `meta` (`schema_version`, `last_applied_lsn`, `last_applied_crc`); `agents`, `turns`,
//! `goals`, `board`, `budget_by_goal` and `mail` keyed by the 16 ULID bytes; `budget_by_day` by an agent and
//! a day ([`budget`]); `goal_first_done` orders each goal's first observed completion by `Entry.at` ([`goals`]); all
//! with `serde_json` rows. `activity` and `retired_agents` each hold a bounded display window.
//! A missing or mismatching `schema_version` drops
//! every table and resets `last_applied_lsn`, so the next replay starts from the first segment.
//! The board holds card views. Memory and hook tokens have their own journal folds.
//!
//! `last_applied_crc` is the `crc32c` of the frame at `last_applied_lsn`, written in the same
//! commit as the cursor it witnesses. An lsn is a position, so a
//! segment rewritten in place presents the same one for different bytes; the checksum is what
//! [`crate::replay`] compares to tell the frame it applied from another intact frame there.

mod activity;
pub mod agents;
mod board;
pub mod budget;
mod chat;
pub mod goals;
mod instructions;
mod mail;
mod mail_wakes;
mod question_history;
mod retired;
mod traces;
pub mod turns;
mod worktrees;

use std::collections::BTreeMap;
use std::path::Path;

use dudes_proto::{ActivityView, CardView, Entry, MessageView, RetiredAgentView, Spend};
use dudes_types::{AgentId, CardId, GoalId, Lsn, Timestamp, TurnId, UtcDay};
use redb::backends::InMemoryBackend;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, Table,
    TableDefinition,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use agents::AgentRow;
pub use budget::SpendViews;
pub use chat::{ChatOverlay, admits_pending};
pub use goals::{FirstDone, GoalRow};
pub use mail::MailView;
pub use turns::TurnRow;
pub use worktrees::WorktreeRow;

use crate::error::{Error, Result};
use crate::writer::Fsync;

/// The projection schema this build writes. Bump when a row's meaning changes, **when a table
/// is added**, or **when what a fold computes changes**; the store is then rebuilt from the
/// journal.
///
/// `47`: shared instruction attempt, review and scoped-boundary facts are folded with ticket
/// evidence sequences. Older caches must rebuild rather than silently omit those obligations.
///
/// `46`: a ticket's board row keeps its ticket fields under `ticket_row`, because a card now has
/// a `ticket` object of its own on the wire and the row flattens the card. A v45 store carried
/// forward would read the stored fields as that object and fail, so the bump forces the refold.
///
/// `45`: a ticket's board row carries the agent's last message and the daemon's newest check
/// result for its goal. A v44 store carried forward would hold neither for any turn or check run
/// it already applied, so the bump forces the refold that fills them.
///
/// `44`: a ticket that leaves its agent's queue loses its queue place, and a ticket given to an
/// agent again loses the goal of the round it leaves. A v43 store carried forward would keep both,
/// so a stale queue place or a stale pass could be read as current. The bump forces the refold.
///
/// `43`: board rows carry a ticket's note, project, check, queue place and round, and a ticket
/// record folds into the board, the goals and the agents. A v42 store carried forward would hold
/// no ticket field for any row it already applied, so the bump forces the refold.
///
/// `41`: a `goals` row carries the baseline its `GoalSet` recorded. A v40 store carried forward
/// would hold every goal it already applied without one, so the Floor would say a check did not
/// run before the first turn for a goal whose check failed before it. The bump forces the refold.
///
/// `40`: a goal's latest result carries `not_run`, which the `goals` fold now computes from the
/// result's evidence. A v39 store carried forward would keep `not_run` false for every result it
/// already applied, so a check that could not run would keep reading as a failed one. The bump
/// forces the refold that sets it.
///
/// `39`: `goals` carries each goal's first observed completion and `goal_first_done` indexes those
/// facts by `Entry.at`. A v38 store carried forward would hold the table empty for every
/// completion it already applied, so the Floor's count of goals finished today would read low for
/// ever. The bump forces the refold that fills it.
///
/// `38`: `worktrees` exists and `agents` carries the worktree a hire recorded. A v37
/// store carried forward would hold a table no record is ever folded into for the history it
/// already applied, so a lease recorded before the upgrade would be missing and a hire would be
/// admitted into its tree. The bump forces the refold that fills it.
///
/// `37`: non-terminal mail receipts have a per-recipient membership index.
///
/// `36`: main Conversation state coexists with distinct taught-note and mail-wake
/// authority, target progress and once-only issue accounting. Both earlier v35 layouts replay.
///
/// `35`: canonical mail rows retain thread membership alongside finite conversation state.
/// The separate v33 and v34 layouts must replay to reconstruct the combined cache.
///
/// `34`: finite conversation state and peer-mail holding-claim order share one cache layout.
/// Caches from the separate v32 and v33 builds must replay the journal.
///
/// `32`: browser-note lifecycle, completion proofs, Chat history and current holding Claim order
/// share one cache layout. Both earlier v31 layouts must replay the journal.
///
/// `31`: either browser-note tables or current holding Claim order, before those layouts merged.
///
/// `30`: the budget fold also charges a `MemoryCondense` record's own reported model usage, or
/// one unknown end at the reserve price when it reported none. A v29 cursor
/// past such a record undercounts spend against the agent, floor and goal it would have charged,
/// so replay must rebuild both budget tables.
///
/// `29`: the bounded Messages table is maintained during mail folds. A v28 cursor may be
/// past the mail records that populate the new table, so replay must rebuild it.
///
/// `28`: mail rows retain the original sender, acceptance time and creation order for the
/// bounded Messages view. A v27 cursor may have passed the Mail entries that supply those facts.
///
/// `27`: the bounded trace table retains completed-turn facts, and turn rows retain the recorded
/// spawn observation time. A v26 cursor may have passed those records, so replay must rebuild both.
///
/// `26`: retired rows include the character and accent already recorded at hire. A v25 archive
/// omitted both while its cursor passed the retirement, so only a rebuild can recover them.
///
/// `25`: agent rows retain the latest fixed live activity and its journal time. Earlier caches
/// must replay the observations and lifecycle transitions.
///
/// `24`: answered-question history retains a finite browser read window. Earlier caches must replay the answers.
///
/// `23`: settlement restores completed goals, retirement releases claims, and agent rows retain
/// automatic holds and rapid failure progress. Earlier caches must replay the changed folds.
///
/// `19`: the activity table retains a bounded allowlist of journal events. Earlier cursors may
/// be past these records without a window, so replay must rebuild it.
///
/// `18`: hires project their recorded workspace identity and label. Earlier caches must replay
/// the hire to recover metadata they previously dropped.
///
/// `17` since the board table folds card creation, claims and status moves. A v16 cursor may be
/// past card records while holding no card rows, so the store must rebuild.
///
/// `16` since the budget fold counts one unknown end for a turn whose running total is a reset of
/// its confirmed session's last recorded one, and keeps that last figure in the added
/// `session_totals` table. A v15 store has no table and a cursor
/// past the ends that fold would count, so after a downgrade and a re-upgrade those reset ends
/// would stay missing and admission would under-count until a rebuild.
///
/// `15` since agent rows carry the control identity, the position of the agent's newest `Pause`,
/// `Resume` or `Halt`. A v14 row decodes it as absent while its cursor sits past the record that
/// set it, so a Floor pause or resume presenting the true identity would be refused as stale until
/// a rebuild.
///
/// `14` since agent rows carry the open question's position and the question fold settles the
/// status rather than setting `Waiting` over an attached agent. A v13 row decodes the new field as
/// absent while its cursor sits past the `HumanQuestion` that set it, so an open question would
/// stay unanswerable by identity, and an attached asker would keep reading `Waiting`, until a
/// rebuild.
///
/// `13` since budget rows carry work tokens beside the all-kinds total. Version 12 rows decode the
/// added field as zero while retaining a cursor past the usage records that can derive it, so they
/// must rebuild rather than silently admit against zero work.
///
/// `12` since budget rows carry the journal-derived count of unobserved ends beside spend. A v11
/// store has a cursor past those ends and no count, so it must rebuild from the journal.
///
/// `11` since the `mail` table projects each `Record::Mail` recipient and latest `MailState`. A
/// v10 store has no table and may hold a cursor past every mail record, so opening the table
/// without a version reset would make every unread count zero until new mail arrived.
///
/// `10`: `AgentHired` now projects its recorded provider-run limits. A v9 store written by an
/// older fold can decode the extended row through `#[serde(default)]` while retaining `None` for
/// both fields and a cursor already past the hire; replay skips that lsn, so only a version reset
/// can rebuild the values the journal holds.
///
/// `9` since [`turns::TurnRow`] carries whether this turn has an accepted `SessionLearned`, and
/// both session folds use that replay-derived fact to invalidate an unconfirmed resume. A v8 row
/// decodes the added field as false, but retaining its cursor would keep every already-folded
/// confirmation false and invalidate a later resume incorrectly; rebuilding supplies the flag.
///
/// `8` since `agents.session` is cleared by `TurnEnded{outcome: LaunchRefused}`. A
/// v7 build already understands that inherited enum tag but does not fold its invalidation, so a
/// store whose cursor passed such a record keeps the old vendor-confirmed session. Rebuilding is
/// what applies the accepted fold before any later turn can resume it.
///
/// `7` since [`turns::TurnRow`] carries the turn's model selection and its observed per-model
/// usage. Both fields are `#[serde(default)]`, so a v6 row still *decodes* — which
/// is exactly why the bump is needed rather than optional: it would decode to `None`/empty for
/// every turn the old build folded, and [`Projections::apply_batch`] skips every lsn at or below
/// the stored cursor, so those rows would say "no model was recorded" about journals that record
/// one. A row that is quietly wrong is worse than a store that is rebuilt (what a
/// fold computes changed, so the version moves; and bumps do not coalesce across changes — this
/// one stands alone and rides nothing).
///
/// `6` since `meta` carries `last_applied_crc`. A v5 store carried forward would keep a cursor
/// with no witness beside it, and [`Projections::apply_batch`] skips every lsn at or below the
/// one already applied — so the key would stay absent for every record that store had already
/// folded, and [`crate::replay`]'s comparison would have nothing to compare until the log grew
/// past the old cursor. Rebuilding is what populates it.
///
/// **Bumps do not coalesce across changes.** Each repair takes its own number: had two repairs
/// shared one, a store rebuilt by the first build would have kept the second build's defect
/// for ever, because [`Projections::apply_batch`] never refolds a record at or below the cursor.
///
/// `5` since `agents.session` is established by `SessionLearned` alone. The field's *shape* did not change; what
/// changed is what the `TurnIssued` and `TurnEnded` folds compute for it. A change to what a fold computes
/// obliges a bump, because replay
/// cannot correct a store the old fold already wrote.* [`Projections::apply_batch`] skips every
/// record at or below the stored cursor, so a v4 store keeps the announced, unconfirmed id for
/// ever unless the version moves — and `AgentRow.session` is the field a re-issue resumes from.
/// **Bumps do not coalesce across changes**: this repair takes its own number rather than
/// riding the one below it, so a store stamped `4` is rebuilt and loses the unconfirmed id.
///
/// `4` because [`agents`]'s `Resume` arm cleared `paused` only, leaving `halted` a one-way
/// latch: a v3 store says `halted = true` for an agent resumed hours ago, while the supervisor's
/// fold of the same journal says `false`. Correcting
/// the arm repairs new stores only — [`Projections::apply_batch`] skips every lsn at or below
/// `last_applied_lsn`, so a `Resume` the previous build already folded is never folded again and
/// its wrong row outlives every later replay. The bump is what drops that row and refolds the
/// journal over it.
///
/// `3` since `goals`, `budget_by_goal` and `budget_by_day` exist. No existing row changed shape,
/// and that is not what makes the bump necessary. [`Projections::init`] drops the tables and
/// resets `last_applied_lsn` **only** where the stored version differs; outside that branch
/// `open_table` still creates a new table — empty — and [`Projections::apply_batch`] skips every
/// lsn at or below the one already applied. A v2 store carried forward would therefore hold
/// three tables no record will ever be folded into, every budget read would return zero for
/// ever, and gate 5 would admit turns for an agent that is over its ceiling, silently and with
/// no error. That is invariant 4 defeated by a stale projection; the bump is what forces the
/// rebuild that fills them.
///
/// `2` was the previous bump: the rows became at-derived and `turns.status` gained
/// a *reachable* `Killed`, so a v1 row saying `Ended` for a killed turn was wrong. Half-right is
/// worse than rebuilt.
pub const PROJECTION_VERSION: u64 = 47;
/// Entries applied per write transaction during replay.
pub const BATCH: usize = 1000;

const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");
const AGENTS: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("agents");
const TURNS: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("turns");
const GOALS: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("goals");
const BOARD: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("board");
const INSTRUCTIONS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("instructions");
const ACTIVITY: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("activity");
const QUESTION_HISTORY: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("question_history");
const TRACES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("traces");
const RETIRED_AGENTS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("retired_agents");
const MESSAGES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("messages");
const MAIL: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("mail");
const MAIL_PENDING: TableDefinition<'_, &[u8; 32], &[u8]> = TableDefinition::new("mail_pending");
const MAIL_WAKES: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("mail_wakes");
const MAIL_WAKE_BY_DAY: TableDefinition<'_, &[u8; 20], &[u8]> =
    TableDefinition::new("mail_wake_by_day");
const BUDGET_BY_GOAL: TableDefinition<'_, &[u8; 16], &[u8]> =
    TableDefinition::new("budget_by_goal");
const BUDGET_BY_DAY: TableDefinition<'_, &[u8; 20], &[u8]> = TableDefinition::new("budget_by_day");
const CHAT_NOTES: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("chat_notes");
const CHAT_ISSUES: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("chat_issues");
const CHAT_DECISIONS: TableDefinition<'_, &[u8; 16], &[u8]> =
    TableDefinition::new("chat_decisions");
const CHAT_COMPLETIONS: TableDefinition<'_, &[u8; 16], &[u8]> =
    TableDefinition::new("chat_completions");
const CHAT_WINDOWS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("chat_windows");
const WORKTREES: TableDefinition<'_, &[u8; 16], &[u8]> = TableDefinition::new("worktrees");
const GOAL_FIRST_DONE: TableDefinition<'_, &[u8; 25], &[u8]> =
    TableDefinition::new("goal_first_done");
const SESSION_TOTALS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("session_totals");
const SCHEMA_VERSION: &str = "schema_version";
const LAST_APPLIED_LSN: &str = "last_applied_lsn";
/// The `crc32c` of the frame at `last_applied_lsn`. A `u32` value in
/// a `u64` cell, so `META` needs no second table definition.
const LAST_APPLIED_CRC: &str = "last_applied_crc";

/// A ULID-keyed table with `serde_json` rows.
pub(crate) type RowTable<'txn> = Table<'txn, &'static [u8; 16], &'static [u8]>;

/// Read one row.
pub(crate) fn get_row<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    key: &[u8; 16],
) -> Result<Option<T>> {
    table
        .get(key)?
        .map(|guard| serde_json::from_slice(guard.value()).map_err(|source| Error::Row { source }))
        .transpose()
}

/// Write one row.
pub(crate) fn put_row<T: Serialize>(
    table: &mut RowTable<'_>,
    key: &[u8; 16],
    row: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(row).map_err(|source| Error::Row { source })?;
    table.insert(key, bytes.as_slice())?;
    Ok(())
}

/// Every row, in key order.
fn all_rows<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
) -> Result<Vec<T>> {
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        rows.push(serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?);
    }
    Ok(rows)
}

/// The projection store.
#[derive(Debug)]
pub struct Projections {
    db: Database,
    last_applied: Lsn,
    /// The witness for `last_applied`, as the `meta` cell holds it — `None` where the store has
    /// no key, which is not the same state as a checksum of zero.
    last_applied_crc: Option<u64>,
    durability: Durability,
}

impl Projections {
    /// Create or open `state.redb` at `path` with durable commits. A schema-version mismatch
    /// (or a fresh file) drops every table and starts from `last_applied_lsn = 0`.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Fsync::Full)
    }

    /// [`Projections::open`] with the journal's sync policy: `Fsync::None` commits without
    /// syncing (dev only, like the journal's). A commit lost to a crash rolls the store back
    /// to the last durable one, rows and `last_applied_lsn` together, so replay catches up;
    /// a clean close persists everything.
    pub fn open_with(path: &Path, fsync: Fsync) -> Result<Self> {
        let durability = match fsync {
            Fsync::Full | Fsync::Fdatasync => Durability::Immediate,
            Fsync::None => Durability::None,
        };
        Self::init(Database::create(path)?, durability)
    }

    /// A store that lives in memory and is gone on drop: for tests, and for a replay that
    /// only wants to check a log — un-hidden when that command lands. Commits are not synced
    /// (there is nothing to sync to).
    #[doc(hidden)]
    pub fn in_memory() -> Result<Self> {
        let db = Database::builder().create_with_backend(InMemoryBackend::new())?;
        Self::init(db, Durability::None)
    }

    pub(crate) fn for_validation(file: std::fs::File) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .create_file(file)?;
        Self::init(db, Durability::None)
    }

    /// Check the schema version (resetting on mismatch), read the cursor and the witness beside
    /// it, and make sure the tables exist.
    fn init(db: Database, durability: Durability) -> Result<Self> {
        let txn = db.begin_write()?;
        let found = txn
            .open_table(META)?
            .get(SCHEMA_VERSION)?
            .map(|g| g.value());
        if found != Some(PROJECTION_VERSION) {
            let handles: Vec<_> = txn.list_tables()?.collect();
            for handle in handles {
                txn.delete_table(handle)?;
            }
            let mut meta = txn.open_table(META)?;
            meta.insert(SCHEMA_VERSION, &PROJECTION_VERSION)?;
            meta.insert(LAST_APPLIED_LSN, &0u64)?;
            tracing::warn!(
                target: "journal",
                found = ?found,
                expected = PROJECTION_VERSION,
                "projections.reset"
            );
        }
        let (last_applied, last_applied_crc) = {
            let meta = txn.open_table(META)?;
            // The cursor collapses a missing key into zero because `Lsn::ZERO` *is* "nothing
            // applied". The witness must not: `crc32c` legitimately takes the value `0`, so
            // `unwrap_or(0)` would answer either "no key stored" or "the checksum is zero"
            // wrongly, whichever way the comparison were then written.
            (
                Lsn(meta.get(LAST_APPLIED_LSN)?.map_or(0, |g| g.value())),
                meta.get(LAST_APPLIED_CRC)?.map(|g| g.value()),
            )
        };
        txn.open_table(AGENTS)?;
        txn.open_table(TURNS)?;
        txn.open_table(GOALS)?;
        txn.open_table(GOAL_FIRST_DONE)?;
        txn.open_table(BOARD)?;
        txn.open_table(INSTRUCTIONS)?;
        txn.open_table(WORKTREES)?;
        txn.open_table(MAIL)?;
        txn.open_table(MAIL_PENDING)?;
        txn.open_table(MAIL_WAKES)?;
        txn.open_table(MAIL_WAKE_BY_DAY)?;
        txn.open_table(MESSAGES)?;
        txn.open_table(ACTIVITY)?;
        txn.open_table(RETIRED_AGENTS)?;
        txn.open_table(QUESTION_HISTORY)?;
        txn.open_table(TRACES)?;
        txn.open_table(BUDGET_BY_GOAL)?;
        txn.open_table(BUDGET_BY_DAY)?;
        txn.open_table(SESSION_TOTALS)?;
        txn.open_table(CHAT_NOTES)?;
        txn.open_table(CHAT_ISSUES)?;
        txn.open_table(CHAT_DECISIONS)?;
        txn.open_table(CHAT_COMPLETIONS)?;
        txn.open_table(CHAT_WINDOWS)?;
        txn.commit()?;
        Ok(Self {
            db,
            last_applied,
            last_applied_crc,
            durability,
        })
    }

    /// The lsn of the last record applied; `Lsn::ZERO` for an empty store.
    #[must_use]
    pub fn last_applied_lsn(&self) -> Lsn {
        self.last_applied
    }

    /// The `crc32c` of the frame at [`Projections::last_applied_lsn`], as stored beside it
    /// — the witness [`crate::replay`] compares before applying
    /// anything.
    ///
    /// **Three states, not two.** `None` means no build has written a witness to this store —
    /// a v5 store carried across the version bump has none until it is rebuilt, and there is
    /// nothing to compare. `Some(0)` is a witness whose value happens to be zero, and is
    /// compared like any other. Callers must not flatten the two with `unwrap_or(0)`: reading
    /// it as "skip when zero" stops witnessing one store in 2³² (fail-open), and reading it as
    /// "compare whatever came back" refuses an untouched journal for a store with no key.
    #[must_use]
    pub fn last_applied_crc(&self) -> Option<u64> {
        self.last_applied_crc
    }

    /// Apply one entry at `lsn`, witnessed by `crc`, the checksum of the frame it was read from
    /// or written as: the same function live and on replay. Returns
    /// `false` without touching the store when `lsn` is at or below the last applied
    /// (an idempotence guarantee).
    ///
    /// The witness is an argument rather than something this crate derives, because only a
    /// caller holding the frame has it: live, it is [`crate::Writer::append_witnessed`]'s
    /// second return value; on replay, the checksum the reader decoded. **The argument places
    /// that responsibility on the caller; it does not prove it.** `crc` is a plain `u32` and
    /// nothing here checks where it came from — this crate's own tests pass a fixture constant
    /// for records that were never on disk. Provenance is a convention the two production
    /// callers keep, not a guarantee the type provides.
    pub fn apply(&mut self, lsn: Lsn, crc: u32, entry: &Entry) -> Result<bool> {
        Ok(self.apply_batch(std::iter::once((lsn, crc, entry)))? == 1)
    }

    /// Apply entries in one write transaction; `last_applied_lsn` and the `last_applied_crc`
    /// witnessing it move in the same commit, so a crash mid-batch leaves rows, lsn and witness
    /// consistent. Entries at or below the last applied lsn are skipped; within the batch lsns
    /// must strictly increase.
    ///
    /// Lsn order is what makes "last write wins by `lsn`" fall out of the
    /// arms rather than needing a branch in them: the highest-lsn record for a row is the last
    /// one this loop hands to it. It is also what makes the witness the right one: entries
    /// arrive in increasing lsn order, so the last one applied is the one the cursor names.
    #[expect(
        clippy::too_many_lines,
        reason = "one transaction folds each record through every table in the order the comment above fixes"
    )]
    pub fn apply_batch<'a>(
        &mut self,
        entries: impl IntoIterator<Item = (Lsn, u32, &'a Entry)>,
    ) -> Result<u64> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(self.durability)?;
        let mut applied = 0u64;
        let mut last = self.last_applied;
        // The checksum of the last entry applied in this batch. `Some` exactly when `applied`
        // is non-zero — the loop sets the two together — which is what lets one guard write the
        // cursor and its witness.
        let mut witness: Option<u32> = None;
        {
            let mut agents = txn.open_table(AGENTS)?;
            let mut turns = txn.open_table(TURNS)?;
            let mut goals = txn.open_table(GOALS)?;
            let mut first_done = txn.open_table(GOAL_FIRST_DONE)?;
            let mut meta = txn.open_table(META)?;
            let mut board = txn.open_table(BOARD)?;
            let mut instruction_rows = txn.open_table(INSTRUCTIONS)?;
            let mut mail = txn.open_table(MAIL)?;
            let mut pending = txn.open_table(MAIL_PENDING)?;
            let mut wakes = mail_wakes::Tables {
                states: txn.open_table(MAIL_WAKES)?,
                by_day: txn.open_table(MAIL_WAKE_BY_DAY)?,
            };
            let mut messages = txn.open_table(MESSAGES)?;
            let mut activity = txn.open_table(ACTIVITY)?;
            let mut retired = txn.open_table(RETIRED_AGENTS)?;
            let mut history = txn.open_table(QUESTION_HISTORY)?;
            let mut traces = txn.open_table(TRACES)?;
            let mut by_goal = txn.open_table(BUDGET_BY_GOAL)?;
            let mut by_day = txn.open_table(BUDGET_BY_DAY)?;
            let mut sessions = txn.open_table(SESSION_TOTALS)?;
            let mut leases = txn.open_table(WORKTREES)?;
            let mut chat = chat::Tables {
                notes: txn.open_table(CHAT_NOTES)?,
                issues: txn.open_table(CHAT_ISSUES)?,
                decisions: txn.open_table(CHAT_DECISIONS)?,
                completions: txn.open_table(CHAT_COMPLETIONS)?,
                windows: txn.open_table(CHAT_WINDOWS)?,
            };
            for (lsn, crc, entry) in entries {
                if lsn <= self.last_applied {
                    continue;
                }
                if lsn <= last {
                    return Err(Error::NonMonotone { last, got: lsn });
                }
                // Owner, then goal, then turn, then money. Only the last step is *ordered*, and
                // it is ordered by the type system rather than by this comment: `budget::apply`
                // takes the `EndedTurn` that `turns::apply` returns, because `TurnEnded` names
                // no goal and the goal lives on the turn's row (see `turns::EndedTurn`). Moving
                // it above `turns::apply` does not compile. `goals::apply` reads `agents` for
                // one arm, but that read is order-independent — see its doc comment.
                chat::apply(&mut chat, &agents, &turns, lsn, entry)?;
                mail_wakes::apply(
                    &mut wakes,
                    &mail_wakes::Facts {
                        agents: &agents,
                        goals: &goals,
                        turns: &turns,
                        mail: &mail,
                        pending: &pending,
                        completions: &chat.completions,
                    },
                    lsn,
                    entry,
                )?;
                let normal = mail_wakes::normal_carrier(entry);
                let carrier = normal.as_ref().unwrap_or(entry);
                activity::capture_before_source_update(
                    &mut activity,
                    &agents,
                    &board,
                    lsn,
                    carrier,
                )?;
                retired::capture(&mut retired, &agents, &by_day, &turns, &leases, entry)?;
                question_history::capture(&mut history, &agents, lsn, entry)?;
                agents::apply(&mut agents, &turns, &goals, lsn, carrier)?;
                goals::apply(&mut goals, &mut first_done, &agents, lsn, carrier)?;
                let (instruction_state, instruction_fold) = instructions::apply(
                    &mut instruction_rows,
                    &agents,
                    &board,
                    &goals,
                    last,
                    lsn,
                    entry,
                )?;
                if let Some(clear) = instruction_fold.cleared_goal {
                    let cleared = Entry {
                        at: entry.at,
                        record: dudes_proto::Record::GoalCleared {
                            agent: clear.agent,
                            goal: clear.goal,
                        },
                    };
                    agents::apply(&mut agents, &turns, &goals, lsn, &cleared)?;
                    goals::apply(&mut goals, &mut first_done, &agents, lsn, &cleared)?;
                }
                board::apply(&mut board, &turns, lsn, entry)?;
                board::apply_instruction(
                    &mut board,
                    lsn,
                    entry,
                    instruction_state.state(),
                    &instruction_fold,
                )?;
                worktrees::apply(&mut leases, entry)?;
                let ended = turns::apply(&mut turns, lsn, carrier)?;
                traces::capture(&mut traces, &agents, &turns, entry, ended.is_some())?;
                budget::apply(
                    &mut by_goal,
                    &mut by_day,
                    &mut sessions,
                    &agents,
                    entry,
                    ended.as_ref(),
                )?;
                if let Some(ended) = ended.as_ref() {
                    retired::refresh_totals(&mut retired, &by_day, &turns, ended.agent)?;
                }
                mail::apply(&mut mail, &mut pending, &mut messages, lsn, entry)?;
                last = lsn;
                witness = Some(crc);
                applied += 1;
            }
            // In this transaction or not at all: a cursor that moved without its
            // witness would meet the next replay as a checksum for a record it no longer names,
            // and refuse a journal nobody touched.
            if let Some(crc) = witness {
                meta.insert(LAST_APPLIED_LSN, &last.0)?;
                meta.insert(LAST_APPLIED_CRC, &u64::from(crc))?;
            }
        }
        if applied == 0 {
            txn.abort()?;
            return Ok(0);
        }
        txn.commit()?;
        self.last_applied = last;
        self.last_applied_crc = witness.map(u64::from);
        Ok(applied)
    }

    /// How many goals first completed, with an observed pass, in `[start, end)` by the `Entry.at`
    /// of their Done decision, and the position the answer is read at. `None` when a Done
    /// decision could not be resolved to its goal's own observed pass, because a total that may be
    /// low is not a total. Read in one transaction, so the count and the position agree.
    pub fn first_done_count(&self, start: Timestamp, end: Timestamp) -> Result<Option<(u64, Lsn)>> {
        let txn = self.db.begin_read()?;
        let index = txn.open_table(GOAL_FIRST_DONE)?;
        if goals::has_fault(&index)? {
            return Ok(None);
        }
        let lsn = Lsn(txn
            .open_table(META)?
            .get(LAST_APPLIED_LSN)?
            .map_or(0, |g| g.value()));
        if end <= start {
            return Ok(Some((0, lsn)));
        }
        let low = goals::fact_key(start, GoalId::NIL);
        let high = goals::fact_key(end, GoalId::NIL);
        let mut count = 0u64;
        for row in index.range::<&[u8; 25]>(&low..&high)? {
            row?;
            count += 1;
        }
        Ok(Some((count, lsn)))
    }

    /// Every lease the journal holds, removed ones included, in id order.
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>> {
        let txn = self.db.begin_read()?;
        all_rows(&txn.open_table(WORKTREES)?)
    }

    /// One agent.
    pub fn agent(&self, id: AgentId) -> Result<Option<AgentRow>> {
        let txn = self.db.begin_read()?;
        get_row(&txn.open_table(AGENTS)?, &id.to_bytes())
    }

    /// Every agent, by id.
    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        let txn = self.db.begin_read()?;
        all_rows(&txn.open_table(AGENTS)?)
    }

    /// One card, derived from its journal records.
    pub fn card(&self, id: CardId) -> Result<Option<CardView>> {
        let txn = self.db.begin_read()?;
        get_row(&txn.open_table(BOARD)?, &id.to_bytes())
    }

    /// Every card in canonical id order.
    pub fn cards(&self) -> Result<Vec<CardView>> {
        let txn = self.db.begin_read()?;
        all_rows(&txn.open_table(BOARD)?)
    }

    /// Cold reconstruction of all instruction history at the durable projection cut.
    pub fn instruction_state(&self) -> Result<dudes_proto::instruction_state::InstructionState> {
        let txn = self.db.begin_read()?;
        instructions::read(&txn.open_table(INSTRUCTIONS)?, self.last_applied)
    }

    /// Validate instruction admission against committed card/goal facts without inventing the
    /// next physical journal position. The caller holds serialized append authority until fsync.
    pub fn validate_next_instruction(&self, entry: &Entry) -> Result<()> {
        let txn = self.db.begin_read()?;
        let agents = txn.open_table(AGENTS)?;
        let board = txn.open_table(BOARD)?;
        let goals = txn.open_table(GOALS)?;
        let cache = instructions::load(
            &txn.open_table(INSTRUCTIONS)?,
            self.last_applied,
            entry,
            &[],
        )?;
        cache
            .state()
            .validate_next(
                entry,
                instructions::facts(&agents, &board, &goals, entry)?,
                instructions::hash,
            )
            .map_err(|reason| Error::InstructionAdmission {
                kind: entry.record.kind(),
                reason,
            })
    }

    /// Pure instruction preflight against this projection's actual current card and goal facts.
    /// Call on the serialized append boundary; a successful check changes no cache row.
    pub fn preflight_instruction(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<dudes_proto::instruction_state::InstructionFold> {
        let txn = self.db.begin_read()?;
        let agents = txn.open_table(AGENTS)?;
        let board = txn.open_table(BOARD)?;
        let goals = txn.open_table(GOALS)?;
        let mut cache = instructions::load(
            &txn.open_table(INSTRUCTIONS)?,
            self.last_applied,
            entry,
            &[],
        )?;
        cache
            .state_mut()
            .apply(
                lsn,
                entry,
                instructions::facts(&agents, &board, &goals, entry)?,
                instructions::hash,
            )
            .map_err(|reason| Error::Instruction { lsn, reason })
    }

    /// What the board keeps of one card beyond a plain card's fields; `None` for a card that is
    /// not a ticket and for one that does not exist.
    pub fn ticket(&self, id: CardId) -> Result<Option<(CardView, dudes_proto::TicketRow)>> {
        let txn = self.db.begin_read()?;
        let row = get_row::<board::BoardRow>(&txn.open_table(BOARD)?, &id.to_bytes())?;
        Ok(row.and_then(|row| row.ticket.map(|ticket| (row.view, ticket))))
    }

    /// Every ticket in canonical id order.
    pub fn tickets(&self) -> Result<Vec<(CardView, dudes_proto::TicketRow)>> {
        let txn = self.db.begin_read()?;
        Ok(all_rows::<board::BoardRow>(&txn.open_table(BOARD)?)?
            .into_iter()
            .filter_map(|row| row.ticket.map(|ticket| (row.view, ticket)))
            .collect())
    }

    /// Earliest unfinished holding for each agent, ordered by the current Claim's journal lsn.
    pub fn earliest_held_cards(&self) -> Result<BTreeMap<AgentId, CardId>> {
        let txn = self.db.begin_read()?;
        let mut held = BTreeMap::<AgentId, (Lsn, CardId)>::new();
        for row in all_rows::<board::BoardRow>(&txn.open_table(BOARD)?)? {
            if row.view.status == dudes_types::CardStatus::Done {
                continue;
            }
            if let (Some(agent), Some(lsn)) = (row.view.claimed_by, row.claimed_lsn) {
                let candidate = (lsn, row.view.id);
                held.entry(agent)
                    .and_modify(|first| *first = (*first).min(candidate))
                    .or_insert(candidate);
            }
        }
        Ok(held
            .into_iter()
            .map(|(agent, (_, card))| (agent, card))
            .collect())
    }

    /// Recent permitted journal events in ascending journal order.
    pub fn activity(&self) -> Result<Vec<ActivityView>> {
        let txn = self.db.begin_read()?;
        activity::read(&txn.open_table(ACTIVITY)?)
    }

    /// Retired agents in retirement journal order, within the archive limits.
    pub fn retired_agents(&self) -> Result<Vec<RetiredAgentView>> {
        let txn = self.db.begin_read()?;
        let mut rows = retired::read(&txn.open_table(RETIRED_AGENTS)?)?;
        let leases: Vec<WorktreeRow> = all_rows(&txn.open_table(WORKTREES)?)?;
        for row in &mut rows {
            let removed = row.worktree.as_ref().is_some_and(|_| {
                leases
                    .iter()
                    .any(|lease| lease.agent == Some(row.id) && lease.removed.is_some())
            });
            if removed && let Some(view) = row.worktree.as_mut() {
                view.state = dudes_proto::WorktreeViewState::Removed;
            }
        }
        Ok(rows)
    }

    /// Recent answered questions for current agents, under the browser's disclosure limits.
    pub fn question_history(&self) -> Result<Vec<dudes_proto::AnsweredQuestionView>> {
        let txn = self.db.begin_read()?;
        question_history::read(&txn.open_table(QUESTION_HISTORY)?)
    }

    /// Recent completed turns for current agents, with only Floor-permitted fields.
    pub fn traces(&self) -> Result<Vec<dudes_proto::TurnTraceView>> {
        let txn = self.db.begin_read()?;
        traces::read(&txn.open_table(TRACES)?)
    }

    /// One turn.
    pub fn turn(&self, id: TurnId) -> Result<Option<TurnRow>> {
        let txn = self.db.begin_read()?;
        get_row(&txn.open_table(TURNS)?, &id.to_bytes())
    }

    /// Every turn, by id.
    pub fn turns(&self) -> Result<Vec<TurnRow>> {
        let txn = self.db.begin_read()?;
        all_rows(&txn.open_table(TURNS)?)
    }

    /// Turns that are `Issued` or `Spawned` *and whose agent row still exists*: what the
    /// startup pass re-issues or adopts. An
    /// agent retired while its turn was open leaves the turn row as it was — no `TurnEnded`
    /// was journaled, so the projection invents none — but the turn has no owner and no goal
    /// to serve, so it is not open work; `AgentRetired` is not refused on that account.
    pub fn open_turns(&self) -> Result<Vec<TurnRow>> {
        let txn = self.db.begin_read()?;
        let agents = txn.open_table(AGENTS)?;
        let turns = all_rows::<TurnRow>(&txn.open_table(TURNS)?)?;
        let mut open = Vec::new();
        for turn in turns {
            if turn.is_open()
                && agents.get(&turn.agent.to_bytes())?.is_some()
                && !chat::clean_cancelled(&txn.open_table(CHAT_ISSUES)?, turn.turn)?
            {
                open.push(turn);
            }
        }
        Ok(open)
    }

    /// Bounded durable Chat history, with unresolved historical intents shown conservatively.
    pub fn chat(&self) -> Result<dudes_proto::ChatView> {
        self.chat_with_overlays(&BTreeMap::new())
    }

    /// Chat history refined by the current run's typed delivery proof and fixed reasons.
    pub fn chat_with_overlays(
        &self,
        overlays: &BTreeMap<Lsn, ChatOverlay>,
    ) -> Result<dudes_proto::ChatView> {
        let txn = self.db.begin_read()?;
        chat::read(
            &txn.open_table(CHAT_NOTES)?,
            &txn.open_table(CHAT_ISSUES)?,
            &txn.open_table(CHAT_WINDOWS)?,
            &txn.open_table(AGENTS)?,
            overlays,
        )
    }

    /// One goal.
    pub fn goal(&self, id: GoalId) -> Result<Option<GoalRow>> {
        let txn = self.db.begin_read()?;
        get_row(&txn.open_table(GOALS)?, &id.to_bytes())
    }

    /// Every goal, by id. A goal is never removed: `GoalCleared` marks it `Abandoned` so
    /// `dudes why` can still explain it.
    pub fn goals(&self) -> Result<Vec<GoalRow>> {
        let txn = self.db.begin_read()?;
        all_rows(&txn.open_table(GOALS)?)
    }

    /// What has been spent against one goal over its whole life; [`Spend::default`] for a goal
    /// nothing has been charged to.
    pub fn spend_by_goal(&self, goal: GoalId) -> Result<Spend> {
        let txn = self.db.begin_read()?;
        Ok(
            get_row::<budget::BudgetRow>(&txn.open_table(BUDGET_BY_GOAL)?, &goal.to_bytes())?
                .unwrap_or_default()
                .spend,
        )
    }

    /// What one agent spent on one UTC day. `day` is the caller's — this crate reads no clock.
    pub fn spend_by_day(&self, agent: AgentId, day: UtcDay) -> Result<Spend> {
        Ok(self.views_of(agent, GoalId::NIL, day)?.agent_today)
    }

    /// What every agent spent on one UTC day: the floor row.
    pub fn floor_spend_on(&self, day: UtcDay) -> Result<Spend> {
        Ok(self.views_of(AgentId::NIL, GoalId::NIL, day)?.floor_today)
    }

    /// The three scopes gate 5 admits against, from one read transaction — so a caller cannot
    /// assemble two of them from one store state and the third from another.
    ///
    /// `today` is the supervisor's: it computes the day from the same clock reading it then
    /// stamps on the `LoopDecided`, which is what lets `dudes why` rebuild the input from the
    /// journal alone. Nothing here reads a clock.
    ///
    /// Returns this crate's [`SpendViews`] rather than `dudes_policy::BudgetState`, for the
    /// reason in the [`budget`] module docs; the supervisor maps it field for field.
    pub fn spend_views(&self, agent: AgentId, goal: GoalId, today: UtcDay) -> Result<SpendViews> {
        self.views_of(agent, goal, today)
    }

    /// Current daily agent and Floor spending without any goal attribution.
    pub fn conversation_spend_views(&self, agent: AgentId, today: UtcDay) -> Result<SpendViews> {
        let txn = self.db.begin_read()?;
        budget::views(
            &txn.open_table(BUDGET_BY_GOAL)?,
            &txn.open_table(BUDGET_BY_DAY)?,
            agent,
            None,
            today,
        )
    }

    /// Agent-addressed mail whose latest state is `Durable` or `Notified`. Human mail,
    /// `Acked` and `Bounced` rows do not contribute.
    pub fn unread_by_agent(&self, agent: AgentId) -> Result<u32> {
        let txn = self.db.begin_read()?;
        mail::unread_by_agent(&txn.open_table(MAIL)?, agent)
    }

    /// Newest agent-to-agent mail within 128 rows and 65,536 serialized-array bytes,
    /// returned in ascending original creation order. Human mail and private payloads are omitted.
    pub fn messages(&self) -> Result<Vec<MessageView>> {
        Ok(self.mail_view()?.messages)
    }

    /// Read the bounded message window and its fixed thread membership at one journal cut.
    /// Missing or inconsistent canonical rows are refused rather than reconstructed from endpoints.
    pub fn mail_view(&self) -> Result<MailView> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        let lsn = Lsn(meta.get(LAST_APPLIED_LSN)?.map_or(0, |value| value.value()));
        mail::view(&txn.open_table(MAIL)?, &txn.open_table(MESSAGES)?, lsn)
    }

    /// One transaction behind [`Projections::spend_views`] and the single-scope readers, so
    /// those do not each open their own.
    fn views_of(&self, agent: AgentId, goal: GoalId, today: UtcDay) -> Result<SpendViews> {
        let txn = self.db.begin_read()?;
        budget::views(
            &txn.open_table(BUDGET_BY_GOAL)?,
            &txn.open_table(BUDGET_BY_DAY)?,
            agent,
            Some(goal),
            today,
        )
    }

    /// Row counts `(agents, turns)`. A test probe, not part of the contract; left a 2-tuple
    /// because twenty-odd tests call it and the new tables have readers of their own.
    #[doc(hidden)]
    pub fn counts(&self) -> Result<(u64, u64)> {
        let txn = self.db.begin_read()?;
        Ok((
            txn.open_table(AGENTS)?.len()?,
            txn.open_table(TURNS)?.len()?,
        ))
    }

    /// How many rows the first-completion index holds, facts and unresolved decisions together.
    /// A test probe, not part of the contract.
    #[doc(hidden)]
    pub fn first_done_rows(&self) -> Result<usize> {
        let txn = self.db.begin_read()?;
        let rows = txn.open_table(GOAL_FIRST_DONE)?.len()?;
        Ok(usize::try_from(rows).expect("an index row count fits usize"))
    }

    /// Every `budget_by_goal` row. A test probe, not part of the contract.
    #[doc(hidden)]
    pub fn budget_by_goal_rows(&self) -> Result<Vec<(GoalId, Spend)>> {
        let txn = self.db.begin_read()?;
        budget::goal_rows(&txn.open_table(BUDGET_BY_GOAL)?)
    }

    /// Every `budget_by_day` row, keyed `(agent, day)`. A test probe, not part of the contract.
    #[doc(hidden)]
    pub fn budget_by_day_rows(&self) -> Result<Vec<((AgentId, UtcDay), Spend)>> {
        let txn = self.db.begin_read()?;
        budget::day_rows(&txn.open_table(BUDGET_BY_DAY)?)
    }

    /// The **projection export**: this store's whole logical content as one canonical text
    /// document.
    ///
    /// # What it is for
    ///
    /// Invariant 1 says every projection is rebuildable from the journal. This is that sentence
    /// made falsifiable: the export of a store that has been restarted and replayed equals the
    /// export of a from-scratch replay of the same journal, or one of the two is wrong.
    ///
    /// # Why not the store's bytes
    ///
    /// Because [`Projections::open`] writes on every open — an unconditional `begin_write` at the
    /// top of `init` and an unconditional `commit` at the bottom, whatever it found — so a restart
    /// moves bytes that no row moved with, and a byte comparison comes out red on a daemon that
    /// behaved perfectly. Even a reopen that applies nothing moves bytes. Two stores built by the same route and never reopened *are*
    /// byte-identical; the comparison this method serves is between two that were not.
    ///
    /// # The document
    ///
    /// One line each for the cursor and its witness, then one line per row:
    ///
    /// ```text
    /// lsn <last_applied_lsn>
    /// crc <last_applied_crc, or `-` where the store has no key>
    /// <table> <key, lower-case hex> <row as JSON>
    /// ```
    ///
    /// The tables come in [`EXPORTED_TABLES`] order, and each table's rows come in redb key
    /// order. The cursor and the
    /// witness are part of the identity rather than an assumption behind it: "the same rows",
    /// said of two projections standing at different points in the log, is not the claim anyone
    /// wants.
    ///
    /// Rendering as **sorted lines rather than one blob** is what makes a mismatch `diff`-able to
    /// the divergent row, so the comparison keeps one value to print *and* a row-shaped failure
    /// message. Each row is deserialised and re-serialised rather than echoed, so the field order
    /// is this build's declaration order and a row written by another build is normalised rather
    /// than passed through.
    ///
    /// # What this requires of every projection
    ///
    /// **a projection that does not contribute here is invisible to the
    /// only check invariant 1 has.** Adding a table without extending this method is not a smaller
    /// change than adding a table — it is the same change with its check removed.
    /// `tests::the_export_names_every_table_the_store_holds` is where that is enforced rather
    /// than asked for.
    ///
    /// # Scope limit
    ///
    /// Both sides of a comparison must come from the **same build**. This is a determinism check,
    /// not a compatibility check: an export from build *N* and one from build *N+1* may differ for
    /// reasons that are not defects.
    ///
    /// One read transaction covers all tables, so a caller cannot assemble part of the
    /// document from one store state and part of it from another.
    #[expect(
        clippy::too_many_lines,
        reason = "one document with a line group per table"
    )]
    pub fn export(&self) -> Result<String> {
        let txn = self.db.begin_read()?;
        let mut lines = vec![
            format!("lsn {}", self.last_applied.0),
            match self.last_applied_crc {
                Some(crc) => format!("crc {crc}"),
                None => "crc -".to_owned(),
            },
        ];
        export_rows::<16, AgentRow>(&mut lines, "agents", &txn.open_table(AGENTS)?)?;
        export_rows::<16, TurnRow>(&mut lines, "turns", &txn.open_table(TURNS)?)?;
        export_rows::<16, GoalRow>(&mut lines, "goals", &txn.open_table(GOALS)?)?;
        export_rows::<25, u8>(
            &mut lines,
            "goal_first_done",
            &txn.open_table(GOAL_FIRST_DONE)?,
        )?;
        export_rows::<16, CardView>(&mut lines, "board", &txn.open_table(BOARD)?)?;
        for entry in txn.open_table(INSTRUCTIONS)?.iter()? {
            let (key, value) = entry?;
            let cell: serde_json::Value =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&cell).map_err(|source| Error::Row { source })?;
            lines.push(format!("instructions {} {json}", hex(key.value())));
        }
        export_rows::<16, WorktreeRow>(&mut lines, "worktrees", &txn.open_table(WORKTREES)?)?;
        export_rows::<16, budget::BudgetRow>(
            &mut lines,
            "budget_by_goal",
            &txn.open_table(BUDGET_BY_GOAL)?,
        )?;
        export_rows::<20, budget::BudgetRow>(
            &mut lines,
            "budget_by_day",
            &txn.open_table(BUDGET_BY_DAY)?,
        )?;
        export_rows::<16, mail::MailRow>(&mut lines, "mail", &txn.open_table(MAIL)?)?;
        export_rows::<32, ()>(&mut lines, "mail_pending", &txn.open_table(MAIL_PENDING)?)?;
        for entry in txn.open_table(SESSION_TOTALS)?.iter()? {
            let (key, value) = entry?;
            let row: dudes_types::Usage =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&row).map_err(|source| Error::Row { source })?;
            lines.push(format!(
                "session_totals {} {json}",
                hex(key.value().as_bytes())
            ));
        }
        for entry in txn.open_table(ACTIVITY)?.iter()? {
            let (key, value) = entry?;
            let rows: Vec<ActivityView> =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&rows).map_err(|source| Error::Row { source })?;
            lines.push(format!("activity {} {json}", hex(key.value().as_bytes())));
        }
        for entry in txn.open_table(RETIRED_AGENTS)?.iter()? {
            let (key, value) = entry?;
            let rows: Vec<RetiredAgentView> =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&rows).map_err(|source| Error::Row { source })?;
            lines.push(format!(
                "retired_agents {} {json}",
                hex(key.value().as_bytes())
            ));
        }
        for entry in txn.open_table(QUESTION_HISTORY)?.iter()? {
            let (key, value) = entry?;
            let rows: Vec<dudes_proto::AnsweredQuestionView> =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&rows).map_err(|source| Error::Row { source })?;
            lines.push(format!(
                "question_history {} {json}",
                hex(key.value().as_bytes())
            ));
        }
        for entry in txn.open_table(TRACES)?.iter()? {
            let (key, value) = entry?;
            let rows: serde_json::Value =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&rows).map_err(|source| Error::Row { source })?;
            lines.push(format!("traces {} {json}", hex(key.value().as_bytes())));
        }
        for entry in txn.open_table(MESSAGES)?.iter()? {
            let (key, value) = entry?;
            let rows: Vec<MessageView> =
                serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
            let json = serde_json::to_string(&rows).map_err(|source| Error::Row { source })?;
            lines.push(format!("messages {} {json}", hex(key.value().as_bytes())));
        }
        chat::export(
            &txn.open_table(CHAT_NOTES)?,
            &txn.open_table(CHAT_ISSUES)?,
            &txn.open_table(CHAT_DECISIONS)?,
            &txn.open_table(CHAT_COMPLETIONS)?,
            &txn.open_table(CHAT_WINDOWS)?,
            &mut lines,
        )?;
        export_rows::<16, dudes_proto::mail_wake::MailWakeState>(
            &mut lines,
            "mail_wakes",
            &txn.open_table(MAIL_WAKES)?,
        )?;
        export_rows::<20, u8>(
            &mut lines,
            "mail_wake_by_day",
            &txn.open_table(MAIL_WAKE_BY_DAY)?,
        )?;
        // A trailing newline, so the document is a text file and `diff` does not report the last
        // row as incomplete.
        lines.push(String::new());
        Ok(lines.join("\n"))
    }
}

/// Every table [`Projections::export`] covers, in the order it renders them.
///
/// Public and declared beside the method rather than inlined into it, so that the journal's authority
/// is a **check** and not prose: a test compares this with the tables the store actually holds, and
/// a projection that adds a table without extending the export fails there. A caller reading an
/// export can use it to say what the document is expected to contain. A slice rather than an array
/// so a later table extends the manifest value without changing its public Rust type again.
pub const EXPORTED_TABLES: &[&str] = &[
    "agents",
    "turns",
    "goals",
    "goal_first_done",
    "board",
    "instructions",
    "worktrees",
    "budget_by_goal",
    "budget_by_day",
    "mail",
    "mail_pending",
    "session_totals",
    "activity",
    "retired_agents",
    "question_history",
    "traces",
    "messages",
    "chat_notes",
    "chat_issues",
    "chat_decisions",
    "chat_completions",
    "chat_windows",
    "mail_wakes",
    "mail_wake_by_day",
];

/// One table's rows, in key order, as `<table> <hex key> <json row>` lines.
///
/// The key is rendered from the **table's own key bytes** rather than from an id field of the row,
/// so a row filed under a key that is not its own id shows up as a difference instead of being
/// silently re-derived. Hex rather than a typed rendering, because the two key widths — 16 bytes
/// for a ULID, 20 for `(agent, day)` — then need no per-table spelling, and the row beside it
/// already carries the ids in readable form.
fn export_rows<const N: usize, T: DeserializeOwned + Serialize>(
    lines: &mut Vec<String>,
    table: &'static str,
    rows: &impl ReadableTable<&'static [u8; N], &'static [u8]>,
) -> Result<()> {
    for entry in rows.iter()? {
        let (key, value) = entry?;
        let row: T =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&row).map_err(|source| Error::Row { source })?;
        lines.push(format!("{table} {} {json}", hex(key.value())));
    }
    Ok(())
}

/// `bytes` as lower-case hexadecimal: how [`Projections::export`] writes a table key.
///
/// One function so the export and the test that reads a key back cannot disagree about the
/// spelling — a test that re-derived the rendering would agree with itself and pin nothing. What
/// pins the rendering itself is [`tests::hex_is_lower_case_two_digits_per_byte`], against a value
/// worked out by hand.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut out, byte| match write!(out, "{byte:02x}") {
            // Writing to a `String` cannot fail: `fmt::Write for String` never returns an error.
            Ok(()) | Err(_) => out,
        },
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use dudes_proto::{
        Actor, Goal, GoalBudget, KillDeferredReason, KillReason, LiveActivityKind, MailAct,
        MailFrom, MailTo, Predicate, Record, TurnOutcome,
    };
    use dudes_types::{
        AgentStatus, CardStatus, GoalStatus, MailId, MailState, Mode, Provenance, Seq, SessionId,
        Timestamp, Tokens, TurnStatus, Usage, Usd,
    };
    use redb::TableHandle as _;

    use super::*;

    /// A stamp far enough from the epoch to have the width a real one has.
    const AT_BASE: i64 = 1_787_394_600_000;

    /// The witness these tests hand [`Projections::apply`]. They are about the fold, which never
    /// reads it back; the one place the value has to be the journal's own checksum is
    /// [`crate::replay`], and that is where it is tested. A named constant rather than `0`, so a
    /// site that reads it back at all is visibly reading a fixture.
    const WITNESS: u32 = 0x9e37_79b9;

    /// Milliseconds in a day, for stamps that are *about* the day boundary. The boundary is
    /// constructed from this and never looked up: [`Timestamp::utc_day`] is a floor division by
    /// this constant, so no calendar, clock or time zone is involved on either side.
    const DAY_MS: i64 = 86_400_000;

    /// 2026-08-23T00:00:00Z, the midnight the day-boundary tests straddle.
    const MIDNIGHT_MS: i64 = 20_688 * DAY_MS;

    fn agent() -> AgentId {
        AgentId::from_parts(1, 1)
    }

    fn at(i: i64) -> Timestamp {
        Timestamp::from_unix_millis(AT_BASE + i)
    }

    fn day_of(millis: i64) -> UtcDay {
        Timestamp::from_unix_millis(millis).utc_day()
    }

    fn goal_id() -> GoalId {
        GoalId::from_parts(3, 1)
    }

    /// The ceiling [`goal_set`] writes. Named so the assertion and the fixture cannot drift:
    /// `budget` is gate 5's admission input, and it is non-default here so a row that dropped it
    /// reads differently from one that kept it.
    fn goal_budget() -> GoalBudget {
        GoalBudget {
            usd: Some(Usd::from_dollars(2)),
            tokens: None,
        }
    }

    /// The predicate [`goal_set`] writes: gate 4's input, with a non-empty `argv`.
    fn goal_predicate() -> Predicate {
        Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 1,
        }
    }

    /// A `GoalSet` for `agent`, with the ceiling and hop cap the row carries.
    fn goal_set(agent: AgentId, id: GoalId) -> Entry {
        stamped(
            0,
            Record::GoalSet {
                agent,
                goal: Goal {
                    completion_check: None,
                    id,
                    text: "ship it".into(),
                    predicate: goal_predicate(),
                    budget: goal_budget(),
                    hop_cap: 9,
                    baseline: None,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        )
    }

    /// Usage a provider reported in full: `cents` of cost and `tokens` input tokens.
    fn usage(cents: u64, tokens: u64) -> Usage {
        Usage {
            input: Tokens::new(tokens),
            usd: Some(Usd::from_cents(cents)),
            ..Usage::ZERO
        }
    }

    /// The [`Spend`] that [`usage`] charges: the cost as reported, and every token counted once.
    fn spend(cents: u64, tokens: u64) -> Spend {
        Spend {
            usd: Usd::from_cents(cents),
            tokens: Tokens::new(tokens),
            work_tokens: Tokens::new(tokens),
        }
    }

    /// Apply `entries` at lsns 1, 2, 3…, the way a replay of a fresh journal would.
    fn apply_all(p: &mut Projections, entries: &[Entry]) {
        apply_all_from(p, Lsn(1), entries);
    }

    /// Apply `entries` at consecutive lsns from `first`, to continue a history already applied.
    fn apply_all_from(p: &mut Projections, first: Lsn, entries: &[Entry]) {
        for (i, entry) in entries.iter().enumerate() {
            let lsn = Lsn(first.0 + u64::try_from(i).unwrap());
            assert!(p.apply(lsn, WITNESS, entry).unwrap(), "{entry:?}");
        }
    }

    /// Stamp a record. Where a test cares about `at` it passes a distinct `i`; where it does
    /// not, `at` never influences the outcome — `lsn` orders.
    fn stamped(i: i64, record: Record) -> Entry {
        Entry { at: at(i), record }
    }

    /// Stamp `version` into an existing store's `meta` table, as another build would have.
    pub(crate) fn write_schema_version(path: &Path, version: u64) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(SCHEMA_VERSION, &version)
            .unwrap();
        txn.commit().unwrap();
    }

    fn budget_day_key(agent: AgentId, day: UtcDay) -> [u8; 20] {
        let mut key = [0u8; 20];
        key[..16].copy_from_slice(&agent.to_bytes());
        key[16..]
            .copy_from_slice(&(day.days_since_epoch().cast_unsigned() ^ 0x8000_0000).to_be_bytes());
        key
    }

    /// Write the projection shape emitted by version 12: the cursor is already past the history,
    /// and each budget row has an all-kinds token total but no work-token column.
    fn write_version_12_budget_rows(
        path: &Path,
        agent: AgentId,
        goal: GoalId,
        day: UtcDay,
        last_lsn: u64,
        usd_micros: u64,
        tokens: u64,
    ) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        let row = serde_json::to_vec(&serde_json::json!({
            "spend": { "usd": usd_micros, "tokens": tokens },
            "unknown_ends": 0
        }))
        .unwrap();
        {
            let mut by_goal = txn.open_table(BUDGET_BY_GOAL).unwrap();
            by_goal.insert(&goal.to_bytes(), row.as_slice()).unwrap();
        }
        {
            let mut by_day = txn.open_table(BUDGET_BY_DAY).unwrap();
            let agent_key = budget_day_key(agent, day);
            let floor_key = budget_day_key(AgentId::NIL, day);
            by_day.insert(&agent_key, row.as_slice()).unwrap();
            by_day.insert(&floor_key, row.as_slice()).unwrap();
        }
        {
            let mut meta = txn.open_table(META).unwrap();
            meta.insert(SCHEMA_VERSION, &12).unwrap();
            meta.insert(LAST_APPLIED_LSN, &last_lsn).unwrap();
        }
        txn.commit().unwrap();
    }

    /// Put an existing store's `last_applied_crc` into one of its three states, as a build
    /// without the key (`None`) or a log whose frame checksum happened to be that value
    /// (`Some`) would have left it. `last_applied_lsn` is not touched.
    pub(crate) fn write_last_applied_crc(path: &Path, crc: Option<u64>) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(META).unwrap();
            match crc {
                Some(crc) => {
                    meta.insert(LAST_APPLIED_CRC, &crc).unwrap();
                }
                None => {
                    meta.remove(LAST_APPLIED_CRC).unwrap();
                }
            }
        }
        txn.commit().unwrap();
    }

    /// Stamp `last_applied_lsn` into an existing store's `meta` table, as a build that folded
    /// further than this one would have left it.
    fn write_last_applied_lsn(path: &Path, lsn: u64) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(LAST_APPLIED_LSN, &lsn)
            .unwrap();
        txn.commit().unwrap();
    }

    fn hired() -> Entry {
        hired_as(agent())
    }

    fn hired_as(id: AgentId) -> Entry {
        stamped(
            0,
            Record::AgentHired {
                character: None,
                accent: None,
                id,
                name: "Kim".into(),
                role: "engineer".into(),
                department: "eng".into(),
                provider: dudes_types::Provider::Claude,
                cwd: "/work".into(),
                budget: dudes_proto::AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(1)),
                    tokens_per_day: None,
                },
                sandbox: None,
                isolation: None,
                // `None` here so the shared fixture cannot supply what
                // `recorded_run_limits_reach_the_agent_row_and_absence_is_none` is asserting.
                max_turns: None,
                max_budget: None,
                trusted: false,
                workspace: None,
                worktree: None,
                mcp_servers: Vec::new(),
                browser_onboarding: None,
            },
        )
    }

    fn mail(id: MailId, to: MailTo) -> Entry {
        stamped(
            0,
            Record::Mail {
                id,
                thread: None,
                thread_seq: 0,
                members: Vec::new(),
                from: MailFrom::Human,
                to,
                act: MailAct::Message,
                subject: "projection control".into(),
                body: None,
                body_len: 0,
                body_path: None,
                body_blake3: None,
                state: MailState::Durable,
                actor: None,
                provenance: None,
            },
        )
    }

    fn mail_state(id: MailId, state: MailState) -> Entry {
        stamped(0, Record::MailState { id, state })
    }

    fn message(id: MailId, at: i64) -> Entry {
        let mut entry = mail(
            id,
            MailTo::Agent {
                agent: AgentId::from_parts(1, 2),
            },
        );
        entry.at = Timestamp::from_unix_millis(at);
        let Record::Mail { from, .. } = &mut entry.record else {
            unreachable!()
        };
        *from = MailFrom::Agent { agent: agent() };
        entry
    }

    fn threaded_message(id: MailId, thread: dudes_types::ThreadId, members: Vec<AgentId>) -> Entry {
        let mut entry = message(id, 900);
        if let Record::Mail {
            thread: destination,
            members: fixed,
            thread_seq,
            ..
        } = &mut entry.record
        {
            *destination = Some(thread);
            *fixed = members;
            *thread_seq = 1;
        }
        entry
    }

    #[test]
    fn paired_mail_uses_fixed_membership_and_evicts_only_the_last_visible_thread() {
        let mut store = Projections::in_memory().unwrap();
        let thread = dudes_types::ThreadId::from_parts(4, 1);
        let members = vec![
            agent(),
            AgentId::from_parts(1, 2),
            AgentId::from_parts(1, 3),
        ];
        let entry = threaded_message(MailId::from_parts(999, 1), thread, members.clone());
        store.apply(Lsn(1), 0, &entry).unwrap();
        let view = store.mail_view().unwrap();
        assert_eq!(view.lsn, Lsn(1));
        assert_eq!(view.messages[0].thread, Some(thread));
        assert_eq!(
            view.threads,
            vec![dudes_proto::ThreadView {
                id: thread,
                members
            }]
        );
        store
            .apply(
                Lsn(2),
                0,
                &mail_state(MailId::from_parts(999, 1), MailState::Acked),
            )
            .unwrap();
        assert_eq!(store.mail_view().unwrap().messages[0].lsn, "1");
        for n in 3..=129 {
            store
                .apply(Lsn(n), 0, &message(MailId::from_parts(n, 1), 900))
                .unwrap();
        }
        assert_eq!(store.mail_view().unwrap().threads.len(), 1);
        store
            .apply(Lsn(130), 0, &message(MailId::from_parts(130, 1), 900))
            .unwrap();
        let view = store.mail_view().unwrap();
        assert_eq!(view.messages.len(), 128);
        assert!(view.threads.is_empty());
        assert!(view.messages.iter().all(|row| row.thread.is_none()));
    }

    #[test]
    fn paired_mail_refuses_missing_rows_metadata_drift_and_inconsistent_fixed_members() {
        let thread = dudes_types::ThreadId::from_parts(4, 1);
        let members = vec![
            agent(),
            AgentId::from_parts(1, 2),
            AgentId::from_parts(1, 3),
        ];
        for field in [
            "missing", "id", "from", "to", "at", "lsn", "thread", "members", "state",
        ] {
            let mut store = Projections::in_memory().unwrap();
            let id = MailId::from_parts(4, 1);
            store
                .apply(Lsn(1), 0, &threaded_message(id, thread, members.clone()))
                .unwrap();
            store
                .apply(
                    Lsn(2),
                    0,
                    &threaded_message(MailId::from_parts(4, 2), thread, members.clone()),
                )
                .unwrap();
            let txn = store.db.begin_write().unwrap();
            {
                let mut table = txn.open_table(MAIL).unwrap();
                let key = id.to_bytes();
                if field == "missing" {
                    table.remove(&key).unwrap();
                } else {
                    let mut row: serde_json::Value =
                        serde_json::from_slice(table.get(&key).unwrap().unwrap().value()).unwrap();
                    row[field] = match field {
                        "id" => serde_json::json!(MailId::from_parts(4, 99)),
                        "from" | "to" => {
                            serde_json::json!({"kind":"agent","agent": AgentId::from_parts(1, 9)})
                        }
                        "at" => serde_json::json!(901),
                        "lsn" => serde_json::json!(3),
                        "thread" => serde_json::json!(dudes_types::ThreadId::from_parts(4, 2)),
                        "members" => serde_json::json!([agent(), AgentId::from_parts(1, 2)]),
                        "state" => serde_json::json!({"state":"acked"}),
                        _ => unreachable!(),
                    };
                    table
                        .insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
                        .unwrap();
                }
            }
            txn.commit().unwrap();
            assert!(
                matches!(
                    store.mail_view(),
                    Err(Error::MailViewCorrupt { mail: Some(_) })
                ),
                "{field}"
            );
        }
        for members in [
            vec![],
            vec![agent()],
            vec![agent(), agent()],
            vec![AgentId::from_parts(1, 2), agent()],
            vec![agent(), AgentId::from_parts(1, 3)],
            (1..=9).map(|n| AgentId::from_parts(1, n)).collect(),
        ] {
            let mut store = Projections::in_memory().unwrap();
            let id = MailId::from_parts(4, 1);
            store
                .apply(Lsn(1), 0, &threaded_message(id, thread, members))
                .unwrap();
            assert!(
                matches!(store.mail_view(), Err(Error::MailViewCorrupt { mail: Some(found) }) if found == id)
            );
        }
    }

    #[test]
    fn threaded_cache_reconstructs_metadata_past_each_predecessor_cursor() {
        for version in [27, 28, 29, 30, 31, 32, 33, 34] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.redb");
            let journal = dir.path().join("journal");
            let (mut writer, _) = crate::open(
                &journal,
                crate::JournalOptions {
                    fsync: Fsync::None,
                    checkpoint_every: 0,
                    ..crate::JournalOptions::default()
                },
            )
            .unwrap();
            let id = MailId::from_parts(4, 1);
            let thread = dudes_types::ThreadId::from_parts(4, 1);
            let members = vec![
                agent(),
                AgentId::from_parts(1, 2),
                AgentId::from_parts(1, 3),
            ];
            let entry = threaded_message(id, thread, members.clone());
            let expected;
            {
                let mut store = Projections::open_with(&path, Fsync::None).unwrap();
                for entry in [entry, mail_state(id, MailState::Notified { n: 4 })] {
                    let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
                    store.apply(lsn, crc, &entry).unwrap();
                }
                expected = store.mail_view().unwrap();
            }
            {
                let db = Database::create(&path).unwrap();
                let txn = db.begin_write().unwrap();
                {
                    let mut mail = txn.open_table(MAIL).unwrap();
                    let key = id.to_bytes();
                    let mut row: serde_json::Value =
                        serde_json::from_slice(mail.get(&key).unwrap().unwrap().value()).unwrap();
                    for key in ["thread", "members"] {
                        row.as_object_mut().unwrap().remove(key);
                    }
                    mail.insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
                        .unwrap();
                    let mut window = txn.open_table(MESSAGES).unwrap();
                    let mut rows: serde_json::Value =
                        serde_json::from_slice(window.get("window").unwrap().unwrap().value())
                            .unwrap();
                    for row in rows.as_array_mut().unwrap() {
                        row.as_object_mut().unwrap().remove("thread");
                    }
                    window
                        .insert("window", serde_json::to_vec(&rows).unwrap().as_slice())
                        .unwrap();
                }
                txn.commit().unwrap();
            }
            write_schema_version(&path, version);
            let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
            assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
            assert_eq!(
                crate::replay(&crate::Reader::new(&journal), &mut rebuilt)
                    .unwrap()
                    .applied,
                2
            );
            assert_eq!(rebuilt.mail_view().unwrap(), expected);
        }
    }

    #[test]
    fn messages_preserve_original_metadata_and_replay_rebuilds_a_version_27_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let mut store = Projections::open_with(&path, Fsync::None).unwrap();
        let id = MailId::from_parts(9_999_999, 7);
        let entry = message(id, 900);
        let (creation, crc) = writer.append_witnessed(&entry).unwrap();
        store.apply(creation, crc, &entry).unwrap();
        let mut expected = dudes_proto::MessageView {
            id,
            from: agent(),
            to: AgentId::from_parts(1, 2),
            at: Timestamp::from_unix_millis(900),
            lsn: creation.0.to_string(),
            state: dudes_proto::MessageState::Durable,
            thread: None,
        };
        assert_eq!(store.messages().unwrap(), vec![expected.clone()]);
        for (state, label) in [
            (
                MailState::Notified { n: 5 },
                dudes_proto::MessageState::Notified,
            ),
            (MailState::Acked, dudes_proto::MessageState::Acked),
            (
                MailState::Bounced {
                    why: "private reason".into(),
                },
                dudes_proto::MessageState::Bounced,
            ),
        ] {
            let mut entry = mail_state(id, state);
            entry.at = Timestamp::from_unix_millis(-500);
            let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
            store.apply(lsn, crc, &entry).unwrap();
            expected.state = label;
            assert_eq!(store.messages().unwrap(), vec![expected.clone()]);
        }
        let mut collision = mail(id, MailTo::Human);
        collision.at = Timestamp::from_unix_millis(123);
        let (lsn, crc) = writer.append_witnessed(&collision).unwrap();
        store.apply(lsn, crc, &collision).unwrap();
        assert_eq!(store.messages().unwrap(), vec![expected.clone()]);
        drop(writer);
        drop(store);
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(MAIL).unwrap();
                let key = id.to_bytes();
                let mut row: serde_json::Value =
                    serde_json::from_slice(table.get(&key).unwrap().unwrap().value()).unwrap();
                for field in ["from", "at", "lsn"] {
                    row.as_object_mut().unwrap().remove(field);
                }
                table
                    .insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        write_schema_version(&path, 27);
        let mut replayed = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(replayed.last_applied_lsn(), Lsn::ZERO);
        crate::replay(&crate::Reader::new(&journal), &mut replayed).unwrap();
        assert_eq!(replayed.messages().unwrap(), vec![expected]);
        assert_eq!(replayed.last_applied_lsn(), lsn);
    }

    #[test]
    fn messages_replay_rebuilds_a_version_28_cache_without_a_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let mut store = Projections::open_with(&path, Fsync::None).unwrap();
        for number in 1..=140 {
            let entry = message(MailId::from_parts(141 - number, 1), 900);
            let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
            store.apply(lsn, crc, &entry).unwrap();
        }
        let expected = serde_json::to_vec(&store.messages().unwrap()).unwrap();
        assert_eq!(
            store.messages().unwrap().first().unwrap().id,
            MailId::from_parts(128, 1)
        );
        drop(store);
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(TableDefinition::<&str, &[u8]>::new("messages"))
                .unwrap();
            txn.commit().unwrap();
        }
        write_schema_version(&path, 28);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        let reader = crate::Reader::new(&journal);
        assert_eq!(crate::replay(&reader, &mut rebuilt).unwrap().applied, 140);
        assert_eq!(
            serde_json::to_vec(&rebuilt.messages().unwrap()).unwrap(),
            expected
        );
        assert_eq!(crate::replay(&reader, &mut rebuilt).unwrap().applied, 0);
        let expected_export = rebuilt.export().unwrap();
        drop(rebuilt);
        let reopened = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            serde_json::to_vec(&reopened.messages().unwrap()).unwrap(),
            expected
        );
        assert_eq!(reopened.export().unwrap(), expected_export);
    }

    #[test]
    fn messages_select_newest_creation_order_despite_backwards_clocks_and_mail_ids() {
        let mut store = Projections::in_memory().unwrap();
        for number in 1..=1_000 {
            let entry = message(
                MailId::from_parts(1_001 - number, 1),
                -i64::try_from(number).unwrap(),
            );
            store.apply(Lsn(number), WITNESS, &entry).unwrap();
            assert!(store.messages().unwrap().len() <= 128);
        }
        let rows = store.messages().unwrap();
        assert_eq!(rows.len(), 128);
        assert_eq!(rows.first().unwrap().lsn, "873");
        assert_eq!(rows.last().unwrap().lsn, "1000");
        assert_eq!(rows.first().unwrap().at, Timestamp::from_unix_millis(-873));
        assert_eq!(rows.last().unwrap().id, MailId::from_parts(1, 1));
        assert!(serde_json::to_vec(&rows).unwrap().len() <= 65_536);
        store
            .apply(
                Lsn(1_001),
                WITNESS,
                &mail_state(MailId::from_parts(1_000, 1), MailState::Acked),
            )
            .unwrap();
        assert_eq!(store.messages().unwrap(), rows);
    }

    /// A managed `TurnIssued` for `agent()`.
    fn issued(turn: TurnId) -> Entry {
        issued_for(agent(), turn)
    }

    fn issued_for(id: AgentId, turn: TurnId) -> Entry {
        issued_on(id, turn, goal_id())
    }

    fn issued_on(id: AgentId, turn: TurnId, goal: GoalId) -> Entry {
        stamped(
            0,
            Record::TurnIssued {
                agent: id,
                turn,
                goal,
                task: "work".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn::ZERO,
                answers: Vec::new(),
            },
        )
    }

    /// A managed `TurnEnded` at `at(i)`, reporting no usage.
    fn ended(
        id: AgentId,
        turn: TurnId,
        i: i64,
        provenance: Provenance,
        outcome: Option<TurnOutcome>,
    ) -> Entry {
        ended_using(id, turn, at(i), provenance, outcome, None)
    }

    /// A managed `TurnEnded` stamped at an absolute instant, reporting `usage`.
    fn ended_using(
        id: AgentId,
        turn: TurnId,
        at: Timestamp,
        provenance: Provenance,
        outcome: Option<TurnOutcome>,
        usage: Option<Usage>,
    ) -> Entry {
        Entry {
            at,
            record: Record::TurnEnded {
                final_reply: None,
                elapsed_ms: None,
                session_usage: None,
                agent: id,
                turn,
                mode: Mode::Managed,
                provenance,
                exit: dudes_proto::ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage,
                model_usage: BTreeMap::new(),
                last_message: None,
                session: None,
                outcome,
            },
        }
    }

    /// A managed turn that ran and reported `usage`, as two entries.
    fn turn_costing(
        id: AgentId,
        turn: TurnId,
        goal: GoalId,
        at: Timestamp,
        usage: Usage,
    ) -> [Entry; 2] {
        [
            issued_on(id, turn, goal),
            ended_using(
                id,
                turn,
                at,
                Provenance::Observed,
                Some(TurnOutcome::Completed),
                Some(usage),
            ),
        ]
    }

    /// A managed `TurnIssued` that **announces** `session`: the id the daemon minted, or the one
    /// it asked to resume. Announcing is not confirming.
    fn issued_announcing(turn: TurnId, session: &str) -> Entry {
        let mut entry = issued(turn);
        if let Record::TurnIssued { session: s, .. } = &mut entry.record {
            *s = Some(SessionId::new(session));
        }
        entry
    }

    /// Read one agent row straight out of the file, bypassing the version check
    /// [`Projections::open`] runs — the only way to look inside a store stamped with another
    /// build's version without triggering the reset under test.
    fn raw_agent_row(path: &Path, id: AgentId) -> AgentRow {
        let db = Database::create(path).unwrap();
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(AGENTS).unwrap();
        get_row(&table, &id.to_bytes()).unwrap().unwrap()
    }

    /// Put `session` on one agent row outside the fold, as the previous build's `TurnIssued` arm did.
    /// The companion of
    /// [`write_schema_version`]: together they are a store the previous build wrote.
    fn write_agent_session(path: &Path, id: AgentId, session: Option<SessionId>) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(AGENTS).unwrap();
            let key = id.to_bytes();
            let mut row: AgentRow = get_row(&table, &key).unwrap().unwrap();
            row.session = session;
            put_row(&mut table, &key, &row).unwrap();
        }
        txn.commit().unwrap();
    }

    /// a store written under another schema version is dropped whole
    /// on open — every table, and `last_applied_lsn` back to zero — so replay rebuilds it.
    #[test]
    fn schema_mismatch_drops_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut p = Projections::open(&path).unwrap();
        assert!(p.apply(Lsn(1), WITNESS, &hired()).unwrap());
        assert_eq!(p.counts().unwrap(), (1, 0));
        drop(p);
        write_schema_version(&path, PROJECTION_VERSION + 1);
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(TableDefinition::<&str, u64>::new("stray"))
                .unwrap();
            txn.commit().unwrap();
        }
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(p.counts().unwrap(), (0, 0));
        let txn = p.db.begin_read().unwrap();
        let names: Vec<String> = txn
            .list_tables()
            .unwrap()
            .map(|h| h.name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == "stray"), "{names:?}");
        drop(txn);
        drop(p);

        // Control: reopening at the *same* version keeps the rows and the lsn. So the drop
        // above is attributable to the version mismatch, not to reopening the store.
        let mut p = Projections::open(&path).unwrap();
        assert!(p.apply(Lsn(1), WITNESS, &hired()).unwrap());
        drop(p);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn(1));
        assert_eq!(p.counts().unwrap(), (1, 0));
    }

    #[test]
    fn an_old_cache_is_rebuilt_with_the_spawned_mcp_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let turn = TurnId::from_parts(1, 7);
        let records = [
            hired(),
            stamped(
                0,
                Record::AgentMcpServersChanged {
                    agent: agent(),
                    mcp_servers: vec![dudes_types::McpServerName::parse("a").unwrap()],
                },
            ),
            issued(turn),
            stamped(
                0,
                Record::McpLaunchGranted {
                    agent: agent(),
                    turn,
                    policy_lsn: Lsn(2),
                    mcp_servers: vec![dudes_types::McpServerName::parse("a").unwrap()],
                },
            ),
            stamped(
                0,
                Record::TurnSpawned {
                    agent: agent(),
                    turn,
                    pid: 42,
                    start_time: 7,
                    boot: None,
                    session: None,
                    reissued: false,
                    mcp_grant: Some(Lsn(4)),
                },
            ),
        ];
        let mut projections = Projections::open(&path).unwrap();
        for (index, record) in records.iter().enumerate() {
            projections
                .apply(Lsn(u64::try_from(index).unwrap() + 1), WITNESS, record)
                .unwrap();
        }
        assert_eq!(
            projections.turn(turn).unwrap().unwrap().mcp_grant,
            Some(Lsn(4))
        );
        drop(projections);
        write_schema_version(&path, 41);
        let mut rebuilt = Projections::open(&path).unwrap();
        assert!(rebuilt.turn(turn).unwrap().is_none());
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        for (index, record) in records.iter().enumerate() {
            rebuilt
                .apply(Lsn(u64::try_from(index).unwrap() + 1), WITNESS, record)
                .unwrap();
        }
        assert_eq!(rebuilt.turn(turn).unwrap().unwrap().mcp_grant, Some(Lsn(4)));
    }

    /// The bump itself: both rows became at-derived and `turns.status`
    /// gained a reachable `Killed`, so a store the *previous* build wrote must be dropped —
    /// not carried forward with `None`s that nothing will ever fill, because `last_applied_lsn`
    /// is already past the records that would fill them.
    ///
    /// The literal `1` is the whole point of this test. `schema_mismatch_drops_everything`
    /// stamps `PROJECTION_VERSION + 1`, which holds at *any* value of the constant and so
    /// cannot tell whether the constant was bumped when the rows changed meaning. This can.
    #[test]
    fn a_version_17_store_rebuilds_the_activity_window() {
        assert_eq!(PROJECTION_VERSION, 47);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let records = [
            hired(),
            stamped(
                1,
                dudes_proto::Record::Pause {
                    agent: agent(),
                    browser: None,
                },
            ),
        ];
        {
            let mut store = Projections::open(&path).unwrap();
            apply_all(&mut store, &records);
            assert_eq!(store.activity().unwrap().len(), 1);
        }
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(ACTIVITY).unwrap();
            txn.commit().unwrap();
        }
        write_schema_version(&path, 17);
        let mut store = Projections::open(&path).unwrap();
        assert_eq!(store.last_applied_lsn(), Lsn::ZERO);
        assert!(store.activity().unwrap().is_empty());
        apply_all(&mut store, &records);
        assert_eq!(store.activity().unwrap()[0].lsn, "2");
    }

    #[test]
    fn a_version_24_store_rebuilds_the_current_live_kind() {
        assert_eq!(PROJECTION_VERSION, 47);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let entries = [
            hired(),
            stamped(
                1,
                Record::LiveActivityObserved {
                    agent: agent(),
                    kind: LiveActivityKind::UsingTool,
                },
            ),
        ];
        let mut store = Projections::open(&path).unwrap();
        apply_all(&mut store, &entries);
        assert_eq!(
            store.agent(agent()).unwrap().unwrap().live_activity,
            Some(agents::LiveActivityRow {
                kind: LiveActivityKind::UsingTool,
                at: at(1),
            })
        );
        drop(store);
        write_schema_version(&path, 24);
        let mut rebuilt = Projections::open(&path).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        apply_all(&mut rebuilt, &entries);
        assert_eq!(
            rebuilt.agent(agent()).unwrap().unwrap().live_activity,
            Some(agents::LiveActivityRow {
                kind: LiveActivityKind::UsingTool,
                at: at(1),
            })
        );
    }

    #[test]
    fn a_version_25_store_rebuilds_retired_appearance() {
        assert_eq!(PROJECTION_VERSION, 47);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut hire = hired();
        let Record::AgentHired {
            character, accent, ..
        } = &mut hire.record
        else {
            unreachable!()
        };
        *character = Some(dudes_types::CastCharacter::H);
        *accent = Some(dudes_types::AccentColour::Sky);
        let entries = [
            hire,
            stamped(
                1,
                Record::AgentRetired {
                    id: agent(),
                    reason: "done".to_owned(),
                    browser_retire: None,
                },
            ),
        ];
        let mut store = Projections::open(&path).unwrap();
        apply_all(&mut store, &entries);
        assert_eq!(
            store.retired_agents().unwrap()[0].character,
            Some(dudes_types::CastCharacter::H)
        );
        drop(store);
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(RETIRED_AGENTS).unwrap();
                let bytes = table.get("window").unwrap().unwrap().value().to_vec();
                let mut old: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let row = old[0].as_object_mut().unwrap();
                row.remove("character");
                row.remove("accent");
                let bytes = serde_json::to_vec(&old).unwrap();
                table.insert("window", bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        write_schema_version(&path, 25);
        let mut rebuilt = Projections::open(&path).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert!(rebuilt.retired_agents().unwrap().is_empty());
        apply_all(&mut rebuilt, &entries);
        let rows = rebuilt.retired_agents().unwrap();
        let row = &rows[0];
        assert_eq!(row.role, "engineer");
        assert_eq!(row.character, Some(dudes_types::CastCharacter::H));
        assert_eq!(row.accent, Some(dudes_types::AccentColour::Sky));
    }

    #[test]
    fn a_turn_end_after_a_human_question_keeps_waiting_activity() {
        let mut store = Projections::in_memory().unwrap();
        apply_all(
            &mut store,
            &[
                hired(),
                stamped(
                    1,
                    Record::HumanQuestion {
                        agent: agent(),
                        text: "which branch?".into(),
                    },
                ),
                ended(
                    agent(),
                    TurnId::from_parts(2, 1),
                    2,
                    Provenance::Observed,
                    Some(TurnOutcome::Completed),
                ),
            ],
        );
        let row = store.agent(agent()).unwrap().unwrap();
        assert_eq!(row.status, AgentStatus::Waiting);
        assert_eq!(
            row.live_activity,
            Some(agents::LiveActivityRow {
                kind: LiveActivityKind::Waiting,
                at: at(2),
            })
        );
    }

    #[test]
    fn a_version_1_store_is_dropped_by_this_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut p = Projections::open(&path).unwrap();
        assert!(p.apply(Lsn(1), WITNESS, &hired()).unwrap());
        assert_eq!(p.counts().unwrap(), (1, 0));
        drop(p);
        write_schema_version(&path, 1);
        let p = Projections::open(&path).unwrap();
        assert_eq!(
            p.counts().unwrap(),
            (0, 0),
            "a store written under PROJECTION_VERSION 1 must not be carried forward"
        );
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
    }

    #[test]
    fn a_version_8_store_is_rebuilt_for_per_turn_confirmation() {
        const LATCHED: u64 = 8;
        assert_eq!(PROJECTION_VERSION, 47);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let turn = TurnId::from_parts(2, 1);
        let mut p = Projections::open(&path).unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &issued_announcing(turn, "session-a"))
            .unwrap();
        p.apply(
            Lsn(3),
            WITNESS,
            &stamped(
                0,
                Record::SessionLearned {
                    agent: agent(),
                    turn,
                    session: SessionId::new("session-a"),
                },
            ),
        )
        .unwrap();
        assert!(p.turn(turn).unwrap().unwrap().session_confirmed);
        drop(p);

        write_schema_version(&path, LATCHED);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.counts().unwrap(), (0, 0));
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
    }

    /// A launch refusal changes what the agents fold computes for an already-declared record. A v7
    /// cursor may therefore sit past `LaunchRefused` while retaining session A; version 8 must
    /// discard that stale row before replay rebuilds it.
    #[test]
    fn a_version_7_store_is_rebuilt_for_launch_refused_session_clear() {
        const LATCHED: u64 = 7;
        assert_eq!(PROJECTION_VERSION, 47);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let turn = TurnId::from_parts(2, 1);
        let session_a = Some(SessionId::new("session-a"));
        let history = [
            hired(),
            issued_announcing(turn, "session-a"),
            stamped(
                2,
                Record::SessionLearned {
                    agent: agent(),
                    turn,
                    session: SessionId::new("session-a"),
                },
            ),
            ended(
                agent(),
                turn,
                3,
                Provenance::Observed,
                Some(TurnOutcome::LaunchRefused {
                    reason: "task is empty".into(),
                }),
            ),
        ];
        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &history);
        drop(p);

        // The stale field is the one the v7 fold leaves behind. The cursor and every other row
        // come from the real history, so the reset cannot pass on an empty fixture.
        write_agent_session(&path, agent(), session_a.clone());
        write_schema_version(&path, LATCHED);
        assert_eq!(raw_agent_row(&path, agent()).session, session_a);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.counts().unwrap(), (0, 0));
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
    }

    /// The other half of [`only_session_learned_establishes_the_agents_session`], and the same clause
    /// of **invariant 1**: `state.redb` is a projection *`dudes replay` can rebuild*. Without the
    /// bump it is not one. `apply_batch` skips every record at or below the stored cursor, so
    /// replaying the journal into a store the old fold already advanced corrects nothing, and the
    /// version drop is the only thing that does. A change to what a fold computes obliges a bump.
    ///
    /// **`LATCHED` is a literal and not `PROJECTION_VERSION - 1`**, for the reason
    /// `a_version_1_store_is_dropped_by_this_build` gives: a relative stamp holds at any value of
    /// the constant, so it moves with the thing it is supposed to detect and both halves of this
    /// repair could be dropped under it. With the literal the test is red under either half
    /// missing — no bump leaves the cursor and the stale row in place; no fold repair rebuilds
    /// the same stale row.
    ///
    /// **A literal alone is not enough, because what this asserts is `PROJECTION_VERSION !=
    /// LATCHED` and what it means is "*this* repair bumped the version".** Those coincide only
    /// while the constant is the number this repair moved it to: at `6` with `LATCHED` still `4`
    /// the test passes with this repair's own bump dropped, which is exactly the case it exists
    /// to catch. **The pin is what keeps the bump
    /// falsifiable; `LATCHED` is what keeps the fixture faithful to the store an operator
    /// actually holds** — and, measured rather than assumed, what keeps the bump half *doubly*
    /// covered. At the correct latch a dropped bump is caught twice, by the pin and by the
    /// reset path itself; at a stale one it is caught by the pin alone, because a store stamped
    /// two repairs back mismatches either way and rebuilds either way. So a stale latch does not
    /// make this green, but it does spend the redundancy that survives the pin being deleted.
    #[test]
    fn a_version_4_store_keeps_no_unconfirmed_session() {
        // The version the previous build stamped: the one this repair moved *from*.
        const LATCHED: u64 = 4;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "if this moves, move LATCHED above it — a merge past another bump must re-derive \
             both numbers together, or this test passes with this repair's own bump dropped"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let turn = TurnId::from_parts(2, 1);
        let history = [hired(), issued_announcing(turn, "s-announced")];
        let announced = Some(SessionId::new("s-announced"));

        // The store as the previous build left it: the announced id on the agent row, the cursor
        // already past the records that put it there, stamped with the version that build wrote.
        {
            let mut p = Projections::open(&path).unwrap();
            apply_all(&mut p, &history);
        }
        write_agent_session(&path, agent(), announced.clone());
        write_schema_version(&path, LATCHED);
        assert_eq!(
            raw_agent_row(&path, agent()).session,
            announced,
            "control: the fixture holds the row the old fold wrote, so the assertion below is \
             about the reset and not about a row that was never written"
        );

        // The fixed build opens it, and the same journal is replayed into it.
        let mut p = Projections::open(&path).unwrap();
        for (i, entry) in history.iter().enumerate() {
            // The `bool` is deliberately dropped: in a store the bump did not reset these lsns
            // are skipped, and the assertion below is what must report that, not a panic here.
            p.apply(Lsn(u64::try_from(i).unwrap() + 1), WITNESS, entry)
                .unwrap();
        }
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().session,
            None,
            "a store written before the repair must not keep an unconfirmed session id"
        );
    }

    /// `Resume` lifts *both* standing causes, because it is the only record that lifts either:
    /// `machine.rs` routes `Command::Resume` through `Then::ClearedCause`, the supervisor's own
    /// fold clears the pair, and no other record and no wire method un-halts an agent. Clearing
    /// `paused` alone made `halted` a one-way latch here, so the two folds of one journal
    /// answered differently about the same agent after a crash.
    ///
    /// `pause` is the control: it passes before the fix as well, so a green `halt` case is
    /// attributable to the `Resume` arm rather than to the fixture. Each case also asserts the
    /// cause took hold *before* the resume, so neither can pass by never having been set.
    #[test]
    fn resume_lifts_both_halt_and_pause() {
        for (name, cause) in [
            // The control runs first, so a failure of the `halt` case is reported by a run that
            // has already carried `pause` through the same fixture.
            (
                "pause",
                Record::Pause {
                    agent: agent(),
                    browser: None,
                },
            ),
            (
                "halt",
                Record::Halt {
                    agent: agent(),
                    text: Some("stop".into()),
                },
            ),
        ] {
            let mut p = Projections::in_memory().unwrap();
            apply_all_from(&mut p, Lsn(1), &[hired(), stamped(1, cause)]);
            let standing = p.agent(agent()).unwrap().unwrap();
            assert!(
                standing.halted || standing.paused,
                "{name}: the cause must take hold before a resume can lift it: {standing:?}"
            );

            apply_all_from(
                &mut p,
                Lsn(3),
                &[stamped(
                    2,
                    Record::Resume {
                        agent: agent(),
                        browser: None,
                    },
                )],
            );
            let row = p.agent(agent()).unwrap().unwrap();
            assert!(!row.halted, "{name}: resume must clear `halted`: {row:?}");
            assert!(!row.paused, "{name}: resume must clear `paused`: {row:?}");
        }
    }

    /// The bump that goes with the arm above, stated as the thing an operator sees: a store the
    /// **previous build wrote** stops reading `halted` for an agent that was resumed.
    ///
    /// Correcting the arm alone repairs new stores only. [`Projections::apply_batch`] skips
    /// every lsn at or below `last_applied_lsn`, so the `Resume` that build already folded is
    /// never folded again and its wrong row survives every later replay. Only the version
    /// mismatch drops it.
    ///
    /// The literal `3` is the test: written as `PROJECTION_VERSION - 1` it would hold at any
    /// value of the constant and could not tell whether the constant moved when the arm was
    /// corrected. `3` is what the build carrying the latch stamped, so this is red until the
    /// constant is past it.
    #[test]
    fn a_store_written_before_the_resume_fix_stops_reading_halted() {
        /// The `PROJECTION_VERSION` of the build whose `Resume` arm cleared `paused` only.
        const LATCHED: u64 = 3;

        /// Fold a history from lsn 1, as the startup replay does: an entry at or below the
        /// store's `last_applied_lsn` is skipped, not refused.
        fn replay(p: &mut Projections, history: &[Entry]) {
            for (i, entry) in history.iter().enumerate() {
                p.apply(Lsn(u64::try_from(i).unwrap() + 1), WITNESS, entry)
                    .unwrap();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let history = [
            hired(),
            stamped(
                1,
                Record::Halt {
                    agent: agent(),
                    text: None,
                },
            ),
            stamped(
                2,
                Record::Resume {
                    agent: agent(),
                    browser: None,
                },
            ),
        ];

        // The store that build left on disk. It folded all three records — its `Resume` arm
        // moved neither flag — so the row is what `hired` then `Halt` produce, and its lsn is
        // past the resume. Both are written here rather than asserted about, because the build
        // that wrote them is gone.
        {
            let mut p = Projections::open(&path).unwrap();
            apply_all_from(&mut p, Lsn(1), &history[..2]);
            let row = p.agent(agent()).unwrap().unwrap();
            assert!(
                row.halted && !row.paused,
                "the row the latched build left: {row:?}"
            );
        }
        write_last_applied_lsn(&path, 3);

        // Control, as `a_version_2_store_is_rebuilt_by_this_build` does it: stamped at the
        // version this build writes, the same store is **not** rebuilt — the replay below folds
        // nothing and the latched row survives. So the repair is attributable to the version
        // mismatch rather than to reopening, and this is what "the arm alone repairs new stores
        // only" looks like from an operator's chair.
        write_schema_version(&path, PROJECTION_VERSION);
        {
            let mut p = Projections::open(&path).unwrap();
            replay(&mut p, &history);
            let row = p.agent(agent()).unwrap().unwrap();
            assert!(row.halted, "no mismatch, no rebuild: {row:?}");
        }

        // This build opens it and replays the journal, as `duded` does at startup. A record
        // already applied is skipped rather than refused, which is the whole hazard: without
        // the bump that replay folds nothing and the row above is what every later read returns.
        write_schema_version(&path, LATCHED);
        let mut p = Projections::open(&path).unwrap();
        replay(&mut p, &history);
        let row = p.agent(agent()).unwrap().unwrap();
        assert!(
            !row.halted,
            "a store written before the fix must not carry `halted` forward: {row:?}"
        );
        // Controls: the journal really was refolded over the dropped tables, rather than the
        // row being read back empty or the store left untouched at its old lsn.
        assert_eq!(row.name, "Kim", "{row:?}");
        assert_eq!(p.last_applied_lsn(), Lsn(3));
    }

    /// `TurnOutcome::Killed` is what makes [`TurnStatus::Killed`]
    /// reachable, and the end's `at` is what dates the row.
    #[test]
    fn killed_outcome_projects_turn_status_killed() {
        let turn = TurnId::from_parts(2, 1);
        // Every outcome that is not a kill leaves the row `Ended` — the control that the arm
        // discriminates on the outcome instead of always writing one status.
        let cases = [
            (
                Some(TurnOutcome::Killed {
                    by: KillReason::Halt,
                }),
                TurnStatus::Killed,
            ),
            (Some(TurnOutcome::Completed), TurnStatus::Ended),
            // `Vanished` is an end the reaper observed, not a kill.
            (Some(TurnOutcome::Vanished), TurnStatus::Ended),
            (
                Some(TurnOutcome::NotLaunched {
                    reason: "host spawn failed".into(),
                }),
                TurnStatus::Ended,
            ),
            (None, TurnStatus::Ended),
        ];
        for (outcome, expected) in cases {
            let mut p = Projections::in_memory().unwrap();
            p.apply(Lsn(1), WITNESS, &hired()).unwrap();
            p.apply(Lsn(2), WITNESS, &issued(turn)).unwrap();
            let end = ended(agent(), turn, 5, Provenance::Observed, outcome.clone());
            assert!(p.apply(Lsn(3), WITNESS, &end).unwrap());
            let row = p.turn(turn).unwrap().unwrap();
            assert_eq!(row.status, expected, "{outcome:?}");
            assert_eq!(row.outcome, outcome, "{outcome:?}");
            assert_eq!(row.ended_at, Some(at(5)), "{outcome:?}");
            assert!(!row.is_open(), "{row:?}");
            let agent = p.agent(agent()).unwrap().unwrap();
            assert_eq!(agent.open_turn, None, "{outcome:?}");
            assert_eq!(agent.status, AgentStatus::Idle, "{outcome:?}");
        }
    }

    /// a second `TurnEnded` on the same turn rewrites the row, last write
    /// wins **by `lsn`** — not by arrival. No v1 emitter produces one; this states the rule.
    ///
    /// It states the *other* half too, which only the budget tables can show: **the row follows
    /// last-write-wins and the spend does not** — every accepted end is charged, so two ends for
    /// one turn charge twice ([`budget::apply`]). A sum cannot be rewritten without storing the
    /// charge it would have to undo, so the error is deliberately an over-count: that refuses
    /// turns rather than admitting them over a ceiling, the only direction invariant 4 can
    /// tolerate. Both ends therefore report usage — with `usage: None` the rule is invisible,
    /// because nothing is charged either way.
    #[test]
    fn second_turn_ended_rewrites_the_row_last_write_wins_by_lsn() {
        let turn = TurnId::from_parts(2, 1);
        let first = ended_using(
            agent(),
            turn,
            at(5),
            Provenance::Inferred,
            None,
            Some(usage(5, 100)),
        );
        let second = ended_using(
            agent(),
            turn,
            at(9),
            Provenance::Observed,
            Some(TurnOutcome::Killed {
                by: KillReason::Halt,
            }),
            Some(usage(7, 200)),
        );

        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &issued(turn)).unwrap();
        p.apply(Lsn(3), WITNESS, &first).unwrap();
        p.apply(Lsn(4), WITNESS, &second).unwrap();
        let row = p.turn(turn).unwrap().unwrap();
        assert_eq!(row.provenance, Some(Provenance::Observed));
        assert_eq!(row.ended_at, Some(at(9)));
        assert_eq!(row.status, TurnStatus::Killed);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().last_turn_end,
            Some(Provenance::Observed)
        );
        // The row took the higher-lsn end's usage and nothing else; the three spend scopes took
        // both ends. 5 + 7 cents and 100 + 200 tokens is the over-count, stated rather than
        // documented — the standard `turns.rs` sets for `KillDeferred`.
        assert_eq!(row.usage, Some(usage(7, 200)));
        let day = day_of(AT_BASE + 9);
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(12, 300));
        assert_eq!(p.spend_by_day(agent(), day).unwrap(), spend(12, 300));
        assert_eq!(p.floor_spend_on(day).unwrap(), spend(12, 300));

        // Control 1: the same two ends in the opposite lsn order leave the other one standing,
        // so the row follows `lsn` rather than always taking the `Observed` one.
        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &issued(turn)).unwrap();
        p.apply(Lsn(3), WITNESS, &second).unwrap();
        p.apply(Lsn(4), WITNESS, &first).unwrap();
        let row = p.turn(turn).unwrap().unwrap();
        assert_eq!(row.provenance, Some(Provenance::Inferred));
        assert_eq!(row.ended_at, Some(at(5)));
        assert_eq!(row.status, TurnStatus::Ended);

        // Control 2: a superseder replayed at an lsn already applied changes nothing at all,
        // so "last write wins" never means "latest arrival wins".
        assert!(!p.apply(Lsn(4), WITNESS, &second).unwrap());
        assert_eq!(p.turn(turn).unwrap().unwrap(), row);
    }

    /// Gate 2's input: `last_turn_end` and `last_turn_ended_at` come from
    /// the highest-lsn `TurnEnded` **for that agent**, and from no other agent's.
    #[test]
    fn last_turn_end_is_the_highest_lsn_turn_ended() {
        let other = AgentId::from_parts(1, 2);
        let (t1, t2, t3) = (
            TurnId::from_parts(2, 1),
            TurnId::from_parts(2, 2),
            TurnId::from_parts(2, 3),
        );
        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &hired_as(other)).unwrap();
        p.apply(Lsn(3), WITNESS, &issued(t1)).unwrap();
        p.apply(
            Lsn(4),
            WITNESS,
            &ended(agent(), t1, 5, Provenance::Inferred, None),
        )
        .unwrap();
        assert_eq!(
            (
                p.agent(agent()).unwrap().unwrap().last_turn_end,
                p.agent(agent()).unwrap().unwrap().last_turn_ended_at
            ),
            (Some(Provenance::Inferred), Some(at(5)))
        );

        // A later end for the *other* agent leaves this one's fields alone.
        p.apply(Lsn(5), WITNESS, &issued_for(other, t2)).unwrap();
        p.apply(
            Lsn(6),
            WITNESS,
            &ended(
                other,
                t2,
                7,
                Provenance::Observed,
                Some(TurnOutcome::Completed),
            ),
        )
        .unwrap();
        let row = p.agent(agent()).unwrap().unwrap();
        assert_eq!(
            (row.last_turn_end, row.last_turn_ended_at),
            (Some(Provenance::Inferred), Some(at(5)))
        );
        let row = p.agent(other).unwrap().unwrap();
        assert_eq!(
            (row.last_turn_end, row.last_turn_ended_at),
            (Some(Provenance::Observed), Some(at(7)))
        );

        // Control: a later end for *this* agent does move both fields, so the assertion above
        // is not passing because the fields are simply never written twice.
        p.apply(Lsn(7), WITNESS, &issued(t3)).unwrap();
        p.apply(
            Lsn(8),
            WITNESS,
            &ended(
                agent(),
                t3,
                11,
                Provenance::Observed,
                Some(TurnOutcome::Completed),
            ),
        )
        .unwrap();
        let row = p.agent(agent()).unwrap().unwrap();
        assert_eq!(
            (row.last_turn_end, row.last_turn_ended_at),
            (Some(Provenance::Observed), Some(at(11)))
        );
    }

    /// **`SessionLearned` is the only record that establishes `AgentRow.session`**
    /// This extends to the projection the rule that only a confirmed session is resumable; `TurnIssued` announces an id,
    /// `TurnSpawned` names the one the child was launched on and `TurnEnded` may repeat it; none
    /// may establish the field a later turn resumes from. Session-invalidating records and
    /// a replayed launch refusal may clear it, which never turns an unconfirmed id into a
    /// resumable one.
    ///
    /// **Invariant 1**, second clause: `state.redb` is *a deterministic function of a prefix of
    /// the journal*, and the supervisor's `World` folds the same
    /// records into the same field. Two folds reaching two answers means at least one is not
    /// that function. What the supervisor knows after a crash is exactly what replay produces.
    /// A derived projection field is not new persistent state, so the invariant holds.
    ///
    /// **Totality is what the name claims, so all four session-carrying records are here.**
    /// A writer planted in any one arm, `TurnSpawned` included, turns the test red.
    ///
    /// **Two controls, because the assertion is a `None` and a `None` is what a fold that did
    /// nothing at all also leaves.** (a) The *turn* row takes the same ids from the same records
    /// (`turns.rs`, unchanged), so the agent row's `None` is this fold declining an unconfirmed
    /// id rather than the records carrying none. (b) `SessionLearned` in the middle still binds,
    /// so the arm was not deleted along with the other three. The supervisor's `World` is the
    /// out-of-crate control that has folded these records this way since only a confirmed session was resumable
    /// (`crates/dudes-supervisor/src/world.rs`: its `TurnIssued` arm touches `open_turn`, its
    /// `TurnSpawned` and `TurnEnded` arms the turn row and `last_turn_end`, never `row.session`).
    #[test]
    fn only_session_learned_establishes_the_agents_session() {
        let turn = TurnId::from_parts(2, 1);
        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();

        p.apply(Lsn(2), WITNESS, &issued_announcing(turn, "s-announced"))
            .unwrap();
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().session,
            None,
            "an announced id is not a vendor-confirmed session"
        );
        assert_eq!(
            p.turn(turn).unwrap().unwrap().session,
            Some(SessionId::new("s-announced")),
            "control: the record did carry an id, and the turn row took it"
        );

        p.apply(
            Lsn(3),
            WITNESS,
            &stamped(
                0,
                Record::TurnSpawned {
                    agent: agent(),
                    turn,
                    pid: 1,
                    start_time: 1,
                    boot: None,
                    session: Some(SessionId::new("s-spawned")),
                    reissued: false,

                    mcp_grant: None,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().session,
            None,
            "the id a child was launched on is not a confirmation either"
        );
        assert_eq!(
            p.turn(turn).unwrap().unwrap().session,
            Some(SessionId::new("s-spawned")),
            "control: that record carried an id too, and the turn row took it"
        );

        p.apply(
            Lsn(4),
            WITNESS,
            &stamped(
                0,
                Record::SessionLearned {
                    agent: agent(),
                    turn,
                    session: SessionId::new("s-confirmed"),
                },
            ),
        )
        .unwrap();
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().session,
            Some(SessionId::new("s-confirmed")),
            "control: the one writer still writes"
        );

        let mut end = ended(
            agent(),
            turn,
            5,
            Provenance::Observed,
            Some(TurnOutcome::Completed),
        );
        if let Record::TurnEnded { session, .. } = &mut end.record {
            *session = Some(SessionId::new("s-ended"));
        }
        p.apply(Lsn(5), WITNESS, &end).unwrap();
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().session,
            Some(SessionId::new("s-confirmed")),
            "an id repeated on `TurnEnded` neither confirms nor rebinds"
        );
        assert_eq!(
            p.turn(turn).unwrap().unwrap().session,
            Some(SessionId::new("s-ended")),
            "control: that record carried an id too"
        );
    }

    /// The replay-only no-child end invalidates the vendor-confirmed
    /// session in the persistent projection. A non-triggering child end is the control.
    #[test]
    fn launch_refused_clears_confirmed_session_in_redb() {
        fn fold(outcome: TurnOutcome) -> AgentRow {
            let prior = TurnId::from_parts(2, 1);
            let refused = TurnId::from_parts(3, 1);
            let session_a = SessionId::new("session-a");
            let mut p = Projections::in_memory().unwrap();
            let history = [
                hired(),
                issued_announcing(prior, session_a.as_str()),
                stamped(
                    2,
                    Record::SessionLearned {
                        agent: agent(),
                        turn: prior,
                        session: session_a.clone(),
                    },
                ),
                ended(
                    agent(),
                    prior,
                    3,
                    Provenance::Observed,
                    Some(TurnOutcome::Completed),
                ),
                issued_announcing(refused, session_a.as_str()),
                Entry {
                    at: at(5),
                    record: Record::TurnEnded {
                        final_reply: None,
                        elapsed_ms: None,
                        session_usage: None,
                        agent: agent(),
                        turn: refused,
                        mode: Mode::Managed,
                        provenance: Provenance::Observed,
                        exit: dudes_proto::ExitStatus::UNKNOWN,
                        usage: None,
                        model_usage: BTreeMap::new(),
                        last_message: None,
                        session: None,
                        outcome: Some(outcome),
                    },
                },
            ];
            apply_all(&mut p, &history);
            p.agent(agent()).unwrap().unwrap()
        }

        let refused = fold(TurnOutcome::LaunchRefused {
            reason: "task contains a NUL byte at byte offset 4".into(),
        });
        assert_eq!(refused.session, None);
        assert_eq!(refused.open_turn, None);

        let vanished = fold(TurnOutcome::Vanished);
        assert_eq!(
            vanished.session,
            Some(SessionId::new("session-a")),
            "a non-triggering end keeps the vendor-confirmed session"
        );
        assert_eq!(vanished.open_turn, None);
    }

    /// `KillDeferred` records that a standing kill was *not* carried out,
    /// so it moves no row — but it is a record like any other and the lsn must advance past it,
    /// or replay stalls on it for ever.
    #[test]
    fn kill_deferred_moves_no_row_and_advances_the_lsn() {
        let turn = TurnId::from_parts(2, 1);
        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &issued(turn)).unwrap();
        let before = (p.agent(agent()).unwrap(), p.turn(turn).unwrap());
        assert!(
            before.0.is_some() && before.1.is_some(),
            "a known agent and an open turn"
        );

        let deferred = stamped(
            0,
            Record::KillDeferred {
                agent: agent(),
                turn,
                reason: KillDeferredReason::LivenessUnknown,
            },
        );
        // The control the absence assertion needs: `apply` returned `true` and the lsn did
        // move, so "unchanged" is not "nothing was applied at all".
        assert!(p.apply(Lsn(3), WITNESS, &deferred).unwrap());
        assert_eq!(p.last_applied_lsn(), Lsn(3));
        assert_eq!((p.agent(agent()).unwrap(), p.turn(turn).unwrap()), before);
    }

    /// an lsn at or below `last_applied_lsn` is a no-op; a batch whose
    /// lsns do not strictly increase is refused and leaves nothing behind.
    #[test]
    fn idempotent_by_lsn_and_monotone_within_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Projections::open(&dir.path().join("state.redb")).unwrap();
        assert!(p.apply(Lsn(10), WITNESS, &hired()).unwrap());
        let retire = stamped(
            0,
            Record::AgentRetired {
                id: agent(),
                reason: "x".into(),
                browser_retire: None,
            },
        );
        assert!(
            !p.apply(Lsn(10), WITNESS, &retire).unwrap(),
            "same lsn: no-op"
        );
        assert!(
            !p.apply(Lsn(3), WITNESS, &retire).unwrap(),
            "older lsn: no-op"
        );
        assert_eq!(p.counts().unwrap(), (1, 0));
        let err = p
            .apply_batch([(Lsn(12), WITNESS, &retire), (Lsn(11), WITNESS, &retire)])
            .unwrap_err();
        assert!(matches!(err, Error::NonMonotone { .. }), "{err:?}");
        assert_eq!(p.counts().unwrap(), (1, 0), "the batch was aborted whole");
        assert_eq!(p.last_applied_lsn(), Lsn(10));
        assert_eq!(
            p.apply_batch([(Lsn(5), WITNESS, &retire), (Lsn(12), WITNESS, &retire)])
                .unwrap(),
            1
        );
        assert_eq!(p.counts().unwrap(), (0, 0));
        assert_eq!(p.last_applied_lsn(), Lsn(12));
    }

    /// the witness moves with the cursor, in the same commit, and
    /// carries the checksum of the *last applied* entry — never a later one the batch skipped
    /// and never a stale one. A store reopened from the file reads back what was committed, and
    /// a fresh store has no witness at all rather than a zero.
    #[test]
    fn the_witness_moves_with_the_cursor_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn::ZERO, None),
            "a fresh store has no witness, which is not a witness of zero"
        );
        let retire = stamped(
            0,
            Record::AgentRetired {
                id: agent(),
                reason: "x".into(),
                browser_retire: None,
            },
        );
        assert!(p.apply(Lsn(10), 111, &hired()).unwrap());
        assert_eq!(p.last_applied_crc(), Some(111));
        // A batch: the witness is the last entry's, not the first's.
        assert_eq!(
            p.apply_batch([(Lsn(11), 222, &retire), (Lsn(12), 333, &retire)])
                .unwrap(),
            2
        );
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn(12), Some(333))
        );
        // A batch whose entries are all skipped moves neither.
        assert_eq!(p.apply_batch([(Lsn(12), 444, &retire)]).unwrap(), 0);
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn(12), Some(333))
        );
        // Committed, not merely cached in the handle.
        drop(p);
        let p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn(12), Some(333))
        );
    }

    /// The **write** side of the three checksum-witness states: a checksum of `0` is a witness, and
    /// `apply_batch` commits it like any other. The forbidden branch here is not on the read
    /// path at all — it is `if crc != 0 { meta.insert(..) }`, which leaves a cursor that moved
    /// without its witness and so meets the next replay with a checksum for a record it no
    /// longer names.
    ///
    /// **The reopen is the mechanism of this test, not a flourish.** `apply_batch` assigns
    /// `self.last_applied_crc` whether or not it wrote the cell, so in-process every assertion
    /// reads the cached value and cannot tell the two implementations apart. The committed
    /// cell is only observable through `init`, which is to say through a restart — the same
    /// place the daemon would meet the damage.
    ///
    /// The read-side pair is
    /// [`crate::replay::tests::an_absent_witness_is_skipped_and_a_stored_zero_is_compared`];
    /// this arm is the case "a store with a genuine `0` checksum passes".
    #[test]
    fn a_zero_witness_is_committed_not_skipped_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        assert!(p.apply(Lsn(10), 0, &hired()).unwrap());
        // The control that names what this test cannot see: in-process the cached value is
        // `Some(0)` under a correct `apply_batch` *and* under one that skipped the insert.
        assert_eq!(p.last_applied_crc(), Some(0));
        drop(p);
        // Only the reopen reads the cell. A skipped insert reads back `None` here — the
        // fail-open state — against a cursor that did move.
        let p = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn(10), Some(0)),
            "a zero checksum is a witness, not an absent key"
        );
    }

    /// A record naming an agent or turn the projection does not know is skipped, never a
    /// panic and never a row: the lsn still advances, so replay does not stall on it.
    #[test]
    fn unknown_agent_and_turn_are_skipped() {
        let mut p = Projections::in_memory().unwrap();
        let stranger = AgentId::from_parts(9, 9);
        let turn = TurnId::from_parts(9, 9);
        let entries = [
            stamped(
                0,
                Record::Pause {
                    agent: stranger,
                    browser: None,
                },
            ),
            stamped(
                0,
                Record::AgentRetired {
                    id: stranger,
                    reason: "gone".into(),
                    browser_retire: None,
                },
            ),
            stamped(
                0,
                Record::GoalReassigned {
                    goal: GoalId::from_parts(9, 9),
                    from: stranger,
                    to: stranger,
                    reason: None,
                },
            ),
            stamped(
                0,
                Record::TurnSpawned {
                    agent: stranger,
                    turn,
                    pid: 1,
                    start_time: 1,
                    boot: None,
                    session: None,
                    reissued: false,

                    mcp_grant: None,
                },
            ),
            stamped(
                0,
                Record::SessionLearned {
                    agent: stranger,
                    turn,
                    session: SessionId::new("s"),
                },
            ),
            ended(stranger, turn, 0, Provenance::Observed, None),
            stamped(
                0,
                Record::KillDeferred {
                    agent: stranger,
                    turn,
                    reason: KillDeferredReason::LivenessUnknown,
                },
            ),
        ];
        for (i, entry) in entries.iter().enumerate() {
            let lsn = Lsn(u64::try_from(i).unwrap() + 1);
            assert!(p.apply(lsn, WITNESS, entry).unwrap(), "{entry:?}");
            assert_eq!(p.counts().unwrap(), (0, 0), "{entry:?}");
            assert_eq!(p.last_applied_lsn(), lsn);
        }
        assert_eq!(p.turn(turn).unwrap(), None);
        assert_eq!(p.agent(stranger).unwrap(), None);
    }

    /// An agent retired while its managed turn was open: the turn row stays as it was (no
    /// `TurnEnded` was journaled), but `open_turns()` no longer lists it — no owner, no goal.
    #[test]
    fn open_turns_excludes_turns_of_retired_agents() {
        let mut p = Projections::in_memory().unwrap();
        let turn = TurnId::from_parts(2, 1);
        p.apply(Lsn(1), WITNESS, &hired()).unwrap();
        p.apply(Lsn(2), WITNESS, &issued(turn)).unwrap();
        assert_eq!(p.open_turns().unwrap().len(), 1);
        p.apply(
            Lsn(3),
            WITNESS,
            &stamped(
                0,
                Record::AgentRetired {
                    id: agent(),
                    reason: "laid off".into(),
                    browser_retire: None,
                },
            ),
        )
        .unwrap();
        let row = p.turn(turn).unwrap().unwrap();
        assert!(row.is_open(), "the row is left as it was: {row:?}");
        assert_eq!(p.open_turns().unwrap(), vec![]);
    }

    /// the two recorded provider-run limits are carried onto the projected
    /// [`AgentRow`], and a hire that asked for neither projects as `None`.
    ///
    /// **Both polarities in one test, because one of them is free.** The `None` arm alone would
    /// pass against an `AgentHired` arm that dropped both fields on the floor — `AgentRow::default`
    /// supplies `None` — so the recorded arm is what the assertion rests on, and the absent arm is
    /// the control that says the recorded one is not simply a constant. The two rows are written
    /// under different agent ids so both survive in the table at once.
    #[test]
    fn recorded_run_limits_reach_the_agent_row_and_absence_is_none() {
        let capped = AgentId::from_parts(9, 9);
        let mut with_limits = hired_as(capped);
        if let Record::AgentHired {
            max_turns,
            max_budget,
            ..
        } = &mut with_limits.record
        {
            *max_turns = Some(7);
            *max_budget = Some(Usd::from_micros(2_500_000));
        }

        let mut p = Projections::in_memory().unwrap();
        p.apply(Lsn(1), WITNESS, &with_limits).unwrap();
        p.apply(Lsn(2), WITNESS, &hired()).unwrap();

        let limits: Vec<_> = p
            .agents()
            .unwrap()
            .iter()
            .map(|row| (row.id, row.max_turns, row.max_budget))
            .collect();
        assert!(
            limits.contains(&(capped, Some(7), Some(Usd::from_micros(2_500_000)))),
            "the recorded limits reach the row: {limits:?}"
        );
        assert!(
            limits.contains(&(agent(), None, None)),
            "control: a hire that asked for neither projects as no cap: {limits:?}"
        );
    }

    /// unread is the latest-state set of agent-addressed `Durable` and `Notified`
    /// mail. Live apply and replay use one fold, and the current version rebuilds a version-10 store whose
    /// cursor could otherwise leave the newly added table empty forever.
    ///
    /// Every exclusion is in the same history as the positive arm: agent B distinguishes a
    /// recipient filter from a global count, while `Acked`, `Bounced` and `MailTo::Human`
    /// distinguish latest-state filtering from counting every `Mail` record ever appended.
    #[test]
    #[allow(clippy::too_many_lines)] // one assertion per accepted state/replay/version arm
    fn unread_mail_is_rebuilt_from_latest_mail_state() {
        const LATCHED: u64 = 10;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "this new table owns the 10 -> 11 bump; re-derive the latched store if the current \
             version moves"
        );

        let a = agent();
        let b = AgentId::from_parts(1, 2);
        let a_mail = MailId::from_parts(4, 1);
        let b_mail = MailId::from_parts(4, 2);
        let human_mail = MailId::from_parts(4, 3);
        let history = [
            hired_as(a),
            hired_as(b),
            mail_state(MailId::from_parts(4, 0), MailState::Acked),
            mail(a_mail, MailTo::Agent { agent: a }),
            mail_state(a_mail, MailState::Notified { n: 3 }),
            mail(b_mail, MailTo::Agent { agent: b }),
            mail_state(a_mail, MailState::Acked),
            mail_state(
                b_mail,
                MailState::Bounced {
                    why: "unknown recipient".into(),
                },
            ),
            mail(human_mail, MailTo::Human),
        ];

        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let live_path = dir.path().join("live.redb");
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let mut live = Projections::open_with(&live_path, Fsync::None).unwrap();
        for (index, entry) in history.iter().enumerate() {
            let (lsn, crc) = writer.append_witnessed(entry).unwrap();
            assert!(live.apply(lsn, crc, entry).unwrap());
            let counts = (
                live.unread_by_agent(a).unwrap(),
                live.unread_by_agent(b).unwrap(),
            );
            match index {
                3 | 4 => assert_eq!(counts, (1, 0), "Durable and Notified count for A only"),
                5 => assert_eq!(counts, (1, 1), "B's item affects B only"),
                6 => assert_eq!(counts, (0, 1), "Acked removes A's item"),
                7 | 8 => assert_eq!(counts, (0, 0), "Bounced and Human never count"),
                _ => assert_eq!(counts, (0, 0), "hires and an unknown state invent no mail"),
            }
        }
        drop(writer);

        let live_export = live.export().unwrap();
        assert_eq!(
            live_export
                .lines()
                .filter(|line| line.starts_with("mail "))
                .count(),
            3,
            "the canonical export contains every latest-state mail row"
        );
        assert!(
            EXPORTED_TABLES.contains(&"mail"),
            "the table is in the canonical census"
        );

        let reader = crate::Reader::new(&journal);
        let mut fresh =
            Projections::open_with(&dir.path().join("fresh.redb"), Fsync::None).unwrap();
        crate::replay(&reader, &mut fresh).unwrap();
        assert_eq!(fresh.unread_by_agent(a).unwrap(), 0);
        assert_eq!(fresh.unread_by_agent(b).unwrap(), 0);
        assert_eq!(
            fresh.export().unwrap(),
            live_export,
            "fresh replay equals live apply"
        );

        let legacy_path = dir.path().join("legacy.redb");
        let mut legacy = Projections::open_with(&legacy_path, Fsync::None).unwrap();
        crate::replay(&reader, &mut legacy).unwrap();
        let legacy_cursor = legacy.last_applied_lsn();
        assert_ne!(
            legacy_cursor,
            Lsn::ZERO,
            "control: the old store has a cursor"
        );
        drop(legacy);
        {
            let db = Database::create(&legacy_path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(MAIL).unwrap();
            txn.open_table(META)
                .unwrap()
                .insert(SCHEMA_VERSION, &LATCHED)
                .unwrap();
            txn.commit().unwrap();
        }

        let mut rebuilt = Projections::open_with(&legacy_path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO, "version 10 resets");
        assert_eq!(
            rebuilt.counts().unwrap(),
            (0, 0),
            "all old rows reset together"
        );
        crate::replay(&reader, &mut rebuilt).unwrap();
        assert_eq!(
            rebuilt.export().unwrap(),
            live_export,
            "version reset replays the table"
        );
    }

    /// `last_applied_lsn` is durable: reopening the store resumes where it left off.
    #[test]
    fn last_applied_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut p = Projections::open(&path).unwrap();
        p.apply(Lsn(7), WITNESS, &hired()).unwrap();
        drop(p);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn(7));
        assert_eq!(p.agents().unwrap().len(), 1);
    }

    /// `budget_by_day` buckets by the **UTC day of the entry's `at`**, so
    /// two ends either side of a UTC midnight are two rows and never one.
    ///
    /// The boundary is constructed, not looked up: [`Timestamp::utc_day`] is a floor division by
    /// [`DAY_MS`], so `MIDNIGHT_MS - 1` and `MIDNIGHT_MS` are adjacent days on any machine in any
    /// time zone, with no calendar and no clock involved.
    #[test]
    fn two_turn_ends_across_utc_midnight_land_in_two_day_rows() {
        // Control: the two stamps really do straddle a day boundary. Without this the test
        // passes just as well if `utc_day` returns a constant.
        assert_ne!(day_of(MIDNIGHT_MS - 1), day_of(MIDNIGHT_MS));
        let (before, after) = (day_of(MIDNIGHT_MS - 1), day_of(MIDNIGHT_MS));

        let (t1, t2, t3) = (
            TurnId::from_parts(2, 1),
            TurnId::from_parts(2, 2),
            TurnId::from_parts(2, 3),
        );
        let mut entries = vec![hired(), goal_set(agent(), goal_id())];
        // Same agent throughout, so the two rows can differ only in the day component.
        entries.extend(turn_costing(
            agent(),
            t1,
            goal_id(),
            Timestamp::from_unix_millis(MIDNIGHT_MS - 1),
            usage(5, 100),
        ));
        entries.extend(turn_costing(
            agent(),
            t2,
            goal_id(),
            Timestamp::from_unix_millis(MIDNIGHT_MS),
            usage(7, 200),
        ));
        // A third end later the *same* day as the second: it must add into that row rather than
        // make a third. Without this the test passes against a table keyed by turn.
        entries.extend(turn_costing(
            agent(),
            t3,
            goal_id(),
            Timestamp::from_unix_millis(MIDNIGHT_MS + 3 * 3_600_000),
            usage(11, 300),
        ));
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &entries);

        assert_eq!(p.spend_by_day(agent(), before).unwrap(), spend(5, 100));
        assert_eq!(p.spend_by_day(agent(), after).unwrap(), spend(18, 500));
        // Two agent rows and two floor rows, in `(agent, day)` order — the NIL floor agent first.
        assert_eq!(
            p.budget_by_day_rows().unwrap(),
            vec![
                ((AgentId::NIL, before), spend(5, 100)),
                ((AgentId::NIL, after), spend(18, 500)),
                ((agent(), before), spend(5, 100)),
                ((agent(), after), spend(18, 500)),
            ]
        );
        assert_eq!(p.floor_spend_on(after).unwrap(), spend(18, 500));
        // The goal is not day-bucketed: it holds the whole life in one row.
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(23, 600));
    }

    /// A `Vanished` turn reports no usage, so it increments the reserve count without inventing
    /// observed spend.
    #[test]
    fn a_vanished_turn_with_no_usage_increments_unknown_ends_without_spend() {
        let day = day_of(MIDNIGHT_MS);
        let (t1, t2) = (TurnId::from_parts(2, 1), TurnId::from_parts(2, 2));
        let mut p = Projections::in_memory().unwrap();
        apply_all(
            &mut p,
            &[
                hired(),
                goal_set(agent(), goal_id()),
                issued_on(agent(), t1, goal_id()),
                ended_using(
                    agent(),
                    t1,
                    Timestamp::from_unix_millis(MIDNIGHT_MS),
                    Provenance::Observed,
                    Some(TurnOutcome::Vanished),
                    None,
                ),
            ],
        );

        assert_eq!(p.spend_by_day(agent(), day).unwrap(), Spend::default());
        assert_eq!(p.floor_spend_on(day).unwrap(), Spend::default());
        let view = p.spend_views(agent(), goal_id(), day).unwrap();
        assert_eq!(view.goal_unknown_ends, 1);
        assert_eq!(view.agent_unknown_ends_today, 1);
        assert_eq!(view.floor_unknown_ends_today, 1);
        let row = p.turn(t1).unwrap().unwrap();
        assert_eq!(row.status, TurnStatus::Ended);
        assert_eq!(row.outcome, Some(TurnOutcome::Vanished));
        assert_eq!(row.usage, None);

        // Control: a second turn on the *same key* — same agent, same day, same goal — charging
        // real usage reads back exactly. So the zeroes above are the `Vanished` turn's silence,
        // not a broken key or a reader that always returns nothing.
        apply_all_from(
            &mut p,
            Lsn(5),
            &turn_costing(
                agent(),
                t2,
                goal_id(),
                Timestamp::from_unix_millis(MIDNIGHT_MS + 1),
                usage(9, 400),
            ),
        );
        assert_eq!(p.spend_by_day(agent(), day).unwrap(), spend(9, 400));
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(9, 400));
        let view = p.spend_views(agent(), goal_id(), day).unwrap();
        assert_eq!(view.goal_unknown_ends, 1, "priced usage adds no count");
        assert_eq!(view.goal, spend(9, 400));
    }

    /// a `GoalReassigned` moves the owner and nothing else: the same `GoalId`, predicate, hop
    /// count and spend move to another agent. The hop count is a field
    /// the row must not reset; the spend needs nothing, because `budget_by_goal` is keyed by the
    /// goal and the key does not move.
    #[test]
    fn goal_reassignment_keeps_turns_on_goal_and_budget_by_goal() {
        let other = AgentId::from_parts(1, 2);
        let day = day_of(MIDNIGHT_MS);
        let (t1, t2, t3) = (
            TurnId::from_parts(2, 1),
            TurnId::from_parts(2, 2),
            TurnId::from_parts(2, 3),
        );
        let at = |i: i64| Timestamp::from_unix_millis(MIDNIGHT_MS + i);

        let mut entries = vec![hired(), hired_as(other), goal_set(agent(), goal_id())];
        entries.extend(turn_costing(agent(), t1, goal_id(), at(1), usage(5, 100)));
        entries.extend(turn_costing(agent(), t2, goal_id(), at(2), usage(7, 200)));
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &entries);

        // Both counters are non-zero *before* the reassignment, so "carried over" below is not
        // "was always zero".
        let row = p.goal(goal_id()).unwrap().unwrap();
        assert_eq!((row.agent, row.turns_on_goal), (agent(), 2));
        assert_eq!(row.status, GoalStatus::InProgress);
        // The two fields `GoalSet` writes that only this asserts. Both fail silently and
        // totally if the row drops them: an empty `budget` is reached before the first dollar,
        // so gate 5 issues nothing and the loop stalls with no error
        // ([`dudes_policy::budget`]), and an empty `argv` makes gate 4's predicate
        // unsatisfiable, so the goal can never be `Done`.
        assert_eq!(
            (&row.predicate, row.budget),
            (&goal_predicate(), goal_budget())
        );
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(12, 300));

        let mut next = Lsn(u64::try_from(entries.len()).unwrap() + 1);
        apply_all_from(
            &mut p,
            next,
            &[stamped(
                0,
                Record::GoalReassigned {
                    goal: goal_id(),
                    from: agent(),
                    to: other,
                    reason: None,
                },
            )],
        );
        next = Lsn(next.0 + 1);

        // The reassignment demonstrably happened: the goal changed hands on both agent rows.
        assert_eq!(p.agent(agent()).unwrap().unwrap().goal, None);
        assert_eq!(p.agent(other).unwrap().unwrap().goal, Some(goal_id()));
        let row = p.goal(goal_id()).unwrap().unwrap();
        assert_eq!(row.agent, other, "the owner moved");
        assert_eq!(row.turns_on_goal, 2, "the hop count did not reset");
        assert_eq!(
            (&row.predicate, row.budget),
            (&goal_predicate(), goal_budget()),
            "the predicate and the ceiling moved with the goal"
        );
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(12, 300));

        // And the new owner's turn adds to the same counters rather than starting fresh ones.
        apply_all_from(
            &mut p,
            next,
            &turn_costing(other, t3, goal_id(), at(3), usage(11, 400)),
        );
        let row = p.goal(goal_id()).unwrap().unwrap();
        assert_eq!(row.turns_on_goal, 3);
        assert_eq!(p.spend_by_goal(goal_id()).unwrap(), spend(23, 700));
        // Day spend is per agent, so the third turn lands on the new owner's row, not the old
        // one's — and the floor row holds both.
        assert_eq!(p.spend_by_day(agent(), day).unwrap(), spend(12, 300));
        assert_eq!(p.spend_by_day(other, day).unwrap(), spend(11, 400));
        assert_eq!(p.floor_spend_on(day).unwrap(), spend(23, 700));
    }

    /// Gate 5's three scopes come from three different rows, and `spend_views` reads each from
    /// its own (invariant 4: goal, agent and floor ceilings are separate admissions).
    ///
    /// Every value is distinct, and the two agents' spends are unequal, so a transposed field or
    /// a scope answered from the wrong table shows up as a wrong number rather than passing on a
    /// coincidence. `today` is a bucket the caller names: a day with no spend reads zero for the
    /// agent and the floor while the goal — which is not day-scoped — keeps its whole life.
    #[test]
    fn spend_views_reads_each_scope_from_its_own_row() {
        let other = AgentId::from_parts(1, 2);
        let others_goal = GoalId::from_parts(3, 2);
        let day = day_of(MIDNIGHT_MS);
        let at = |i: i64| Timestamp::from_unix_millis(MIDNIGHT_MS + i);

        let mut entries = vec![
            hired(),
            hired_as(other),
            goal_set(agent(), goal_id()),
            goal_set(other, others_goal),
        ];
        entries.extend(turn_costing(
            agent(),
            TurnId::from_parts(2, 1),
            goal_id(),
            at(1),
            usage(5, 100),
        ));
        entries.extend(turn_costing(
            other,
            TurnId::from_parts(2, 2),
            others_goal,
            at(2),
            usage(7, 200),
        ));
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &entries);

        assert_eq!(
            p.spend_views(agent(), goal_id(), day).unwrap(),
            SpendViews {
                goal: spend(5, 100),
                agent_today: spend(5, 100),
                floor_today: spend(12, 300),
                ..SpendViews::default()
            }
        );
        assert_eq!(
            p.spend_views(other, others_goal, day).unwrap(),
            SpendViews {
                goal: spend(7, 200),
                agent_today: spend(7, 200),
                floor_today: spend(12, 300),
                ..SpendViews::default()
            },
            "the floor is shared; the other two are not"
        );

        // A day nobody spent on: the two day scopes are zero and the goal's life is not.
        let quiet = day_of(MIDNIGHT_MS + DAY_MS);
        assert_eq!(
            p.spend_views(agent(), goal_id(), quiet).unwrap(),
            SpendViews {
                goal: spend(5, 100),
                agent_today: Spend::default(),
                floor_today: Spend::default(),
                ..SpendViews::default()
            }
        );
    }

    /// A version-12 store has no work-token column and its cursor is already past the usage that
    /// can derive one. Opening this build must drop those rows before replay; otherwise serde's
    /// default silently turns non-zero spend into zero admission spend.
    #[test]
    fn a_version_12_budget_store_rebuilds_work_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let day = day_of(MIDNIGHT_MS);
        let turn = TurnId::from_parts(2, 1);
        let reported = Usage {
            input: Tokens::new(5),
            output: Tokens::new(3),
            cache_read: Tokens::new(100),
            cache_creation: Tokens::new(2),
            usd: Some(Usd::from_cents(7)),
        };
        let history = [
            hired(),
            goal_set(agent(), goal_id()),
            issued_on(agent(), turn, goal_id()),
            ended_using(
                agent(),
                turn,
                Timestamp::from_unix_millis(MIDNIGHT_MS),
                Provenance::Observed,
                Some(TurnOutcome::Completed),
                Some(reported),
            ),
        ];
        let expected = Spend {
            usd: Usd::from_cents(7),
            tokens: Tokens::new(110),
            work_tokens: Tokens::new(10),
        };

        drop(Projections::open(&path).unwrap());
        write_version_12_budget_rows(
            &path,
            agent(),
            goal_id(),
            day,
            u64::try_from(history.len()).unwrap(),
            expected.usd.micros(),
            expected.tokens.count(),
        );

        let mut p = Projections::open(&path).unwrap();
        for (index, entry) in history.iter().enumerate() {
            let _ = p
                .apply(Lsn(u64::try_from(index).unwrap() + 1), WITNESS, entry)
                .unwrap();
        }
        let rebuilt = p.spend_views(agent(), goal_id(), day).unwrap();
        assert_eq!(rebuilt.goal, expected);
        assert_eq!(rebuilt.agent_today, expected);
        assert_eq!(rebuilt.floor_today, expected);
        assert_eq!(
            p.last_applied_lsn(),
            Lsn(u64::try_from(history.len()).unwrap())
        );
    }

    #[test]
    fn a_version_11_budget_store_is_rebuilt_for_unknown_ends() {
        assert_eq!(PROJECTION_VERSION, 47);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let day = day_of(MIDNIGHT_MS);
        let turn = TurnId::from_parts(2, 1);
        let entries = [
            hired(),
            goal_set(agent(), goal_id()),
            issued_on(agent(), turn, goal_id()),
            ended_using(
                agent(),
                turn,
                Timestamp::from_unix_millis(MIDNIGHT_MS),
                Provenance::Observed,
                Some(TurnOutcome::Vanished),
                None,
            ),
        ];

        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &entries);
        let expected = p.spend_views(agent(), goal_id(), day).unwrap();
        assert_eq!(
            expected.goal_unknown_ends, 1,
            "the fixture carries the new row state"
        );
        drop(p);

        write_schema_version(&path, PROJECTION_VERSION);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.spend_views(agent(), goal_id(), day).unwrap(), expected);
        drop(p);

        write_schema_version(&path, 11);
        let mut p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO, "v11 is reset");
        assert_eq!(
            p.spend_views(agent(), goal_id(), day).unwrap(),
            SpendViews::default(),
            "the stale row is dropped before replay"
        );
        apply_all(&mut p, &entries);
        assert_eq!(p.spend_views(agent(), goal_id(), day).unwrap(), expected);
    }

    /// For the bump that added `goals`, `budget_by_goal` and
    /// `budget_by_day`: a store the *previous* build wrote is dropped, because
    /// [`Projections::init`] creates the new tables empty and never folds them when the version
    /// matches — so a carried-forward store answers every budget read with zero and gate 5
    /// admits over a ceiling in silence.
    ///
    /// **The literal `2` is the test.** Written as `PROJECTION_VERSION - 1` it cannot fail in
    /// the world it is meant to catch: leave the constant at 2 and the expression is 1, a v1
    /// store still mismatches, still rebuilds, and the test is green — while the real v2 store
    /// keeps its high `last_applied_lsn` and never folds the new tables. It passes precisely
    /// where the bug is live. `a_version_1_store_is_dropped_by_this_build` makes the same
    /// argument: a relative stamp holds at any value of the constant and so cannot tell whether
    /// the constant was bumped.
    #[test]
    fn a_version_2_store_is_rebuilt_by_this_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let (t1, t2) = (TurnId::from_parts(2, 1), TurnId::from_parts(2, 2));
        let mut entries = vec![hired(), goal_set(agent(), goal_id())];
        entries.extend(turn_costing(
            agent(),
            t1,
            goal_id(),
            Timestamp::from_unix_millis(MIDNIGHT_MS - 1),
            usage(5, 100),
        ));
        entries.extend(turn_costing(
            agent(),
            t2,
            goal_id(),
            Timestamp::from_unix_millis(MIDNIGHT_MS),
            usage(7, 200),
        ));

        // What a from-scratch fold of this history produces: the answer a rebuild must reach.
        let mut scratch = Projections::in_memory().unwrap();
        apply_all(&mut scratch, &entries);
        let expected = (
            scratch.goals().unwrap(),
            scratch.budget_by_goal_rows().unwrap(),
            scratch.budget_by_day_rows().unwrap(),
            scratch.last_applied_lsn(),
        );
        assert!(
            !expected.1.is_empty() && !expected.2.is_empty(),
            "the fixture has to spend something or the rebuild proves nothing"
        );

        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &entries);
        drop(p);

        // Control: stamped at the *current* version the store is
        // **not** rebuilt — rows and lsn survive the reopen. So the drop below is attributable
        // to the version, and this test cannot pass by rebuilding unconditionally.
        write_schema_version(&path, PROJECTION_VERSION);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), expected.3);
        assert_eq!(p.budget_by_day_rows().unwrap(), expected.2);
        drop(p);

        write_schema_version(&path, 2);
        let mut p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO, "a v2 store is not resumed");
        assert_eq!(p.counts().unwrap(), (0, 0));
        assert_eq!(p.goals().unwrap(), vec![]);
        assert_eq!(p.budget_by_goal_rows().unwrap(), vec![]);
        assert_eq!(p.budget_by_day_rows().unwrap(), vec![]);

        // …and the replay that follows the reset re-folds every table to the same values.
        apply_all(&mut p, &entries);
        assert_eq!(
            (
                p.goals().unwrap(),
                p.budget_by_goal_rows().unwrap(),
                p.budget_by_day_rows().unwrap(),
                p.last_applied_lsn()
            ),
            expected
        );
    }

    /// For the bump that added `last_applied_crc`: a store the *previous*
    /// build wrote is dropped, because its cursor is already past every record that would put a
    /// witness beside it. [`Projections::apply_batch`] skips every lsn at or below
    /// `last_applied_lsn`, so a carried-forward v5 store keeps a cursor with **no** witness for
    /// the life of that store, and [`crate::replay`]'s comparison has nothing to compare — the
    /// check that the witness exists to add would be installed and permanently inert. The bump is what
    /// forces the rebuild that fills the key.
    ///
    /// **`LATCHED` is the literal `5` and not `PROJECTION_VERSION - 1`**, for the reason
    /// [`a_version_2_store_is_rebuilt_by_this_build`] sets out at length: a relative stamp holds
    /// at *any* value of the constant and so cannot tell whether the constant was bumped at all.
    /// It is `5` because a store stamped any older number models a build further back than the
    /// one an operator actually holds.
    ///
    /// **And the literal alone is not enough**, which
    /// [`a_version_4_store_keeps_no_unconfirmed_session`] states in full and this test does not
    /// restate: what the reset arm asserts is `PROJECTION_VERSION != LATCHED`, and what it means
    /// is *this* repair bumped the version. Those coincide only while the constant is the number
    /// this repair moved it to, so the pin below is what keeps them together when the constant
    /// moves again.
    #[test]
    fn a_version_5_store_is_rebuilt_by_this_build() {
        // The version the previous build stamped: the one this repair moved *from*.
        const LATCHED: u64 = 5;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "if this moves, re-derive LATCHED above it — a merge past another bump must move \
             both, or this test passes with this repair's own bump dropped"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let entries = [hired(), goal_set(agent(), goal_id())];
        let witness = Some(u64::from(WITNESS));

        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &entries);
        let cursor = p.last_applied_lsn();
        assert_ne!(cursor, Lsn::ZERO, "the fixture has to fold something");
        assert_eq!(p.last_applied_crc(), witness);
        drop(p);

        // Control: stamped at the *current* version the store is **not** rebuilt — cursor and
        // witness both survive the reopen. So the reset below is attributable to the version,
        // and this test cannot pass by rebuilding unconditionally.
        write_schema_version(&path, PROJECTION_VERSION);
        let p = Projections::open(&path).unwrap();
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (cursor, witness)
        );
        drop(p);

        // A v5 store: dropped whole. The witness goes with the tables, so the rebuilt store
        // starts from the one state that is *not* a stale witness — no key at all.
        write_schema_version(&path, LATCHED);
        let mut p = Projections::open(&path).unwrap();
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (Lsn::ZERO, None),
            "a v5 store is not resumed"
        );
        assert_eq!(p.counts().unwrap(), (0, 0));

        // …and the replay that follows the reset is what puts a witness beside the cursor —
        // the thing a carried-forward v5 store could never have acquired.
        apply_all(&mut p, &entries);
        assert_eq!(
            (p.last_applied_lsn(), p.last_applied_crc()),
            (cursor, witness)
        );
    }

    // ---- the projection export -------------------------------------------------

    /// The key rendering, against values worked out by hand rather than against itself: two
    /// lower-case digits per byte, leading zero kept, and an empty slice is an empty string.
    #[test]
    fn hex_is_lower_case_two_digits_per_byte() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00]), "00");
        assert_eq!(hex(&[0x0f, 0xf0]), "0ff0");
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the export inventory fixture must populate every persistent table"
    )]
    fn history_touching_every_table() -> Vec<Entry> {
        let turn = TurnId::from_parts(7, 1);
        let retired_id = AgentId::from_parts(99, 1);
        let mut history = vec![
            hired(),
            hired_as(retired_id),
            stamped(
                0,
                Record::HumanQuestion {
                    agent: agent(),
                    text: "Which card next?".into(),
                },
            ),
            stamped(
                0,
                Record::HumanAnswer {
                    agent: agent(),
                    text: "This card".into(),
                    provenance: None,
                    browser_answer: None,
                },
            ),
            stamped(
                0,
                Record::AgentRetired {
                    id: retired_id,
                    reason: "finished".into(),
                    browser_retire: None,
                },
            ),
            message(MailId::from_parts(4, 99), 0),
            goal_set(agent(), goal_id()),
            card_created(),
            stamped(
                0,
                Record::WorktreeLeased {
                    worktree: dudes_types::WorktreeId::from_bytes([3; 16]),
                    origin: "/work".into(),
                    path: "/data/worktrees/tree".into(),
                    branch: "dudes/030303030303".into(),
                    base: dudes_types::GitCommit::parse("0123456789abcdef0123456789abcdef01234567")
                        .unwrap(),
                    from: None,
                },
            ),
        ];
        // Before the first turn, so its end stays the history's last record.
        history.extend(turn_on_session(
            TurnId::from_parts(7, 2),
            "s-export",
            true,
            5,
        ));
        history.extend([
            issued(turn),
            ended_using(
                agent(),
                turn,
                at(0),
                Provenance::Observed,
                Some(TurnOutcome::Completed),
                Some(usage(25, 11)),
            ),
        ]);
        history.push(stamped(
            0,
            dudes_proto::Record::PredicateResult {
                goal: goal_id(),
                turn: None,
                provenance: Provenance::Observed,
                pass: true,
                evidence: dudes_proto::Evidence::Command {
                    argv: vec!["true".into()],
                    program: None,
                    exit: Some(0),
                    output_tail: String::new(),
                },
            },
        ));
        let completion_lsn = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(stamped(
            0,
            dudes_proto::Record::LoopDecided {
                agent: agent(),
                input_digest: "completed".into(),
                decision: dudes_proto::LoopDecision::Done,
                gate_trace: Vec::new(),
            },
        ));
        let note_lsn = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(stamped(
            0,
            dudes_proto::Record::BrowserNoteQueued {
                agent: agent(),
                text: "export note".into(),
                destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal_id() },
                browser: dudes_proto::BrowserNoteAttribution {
                    request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
                    request_digest: dudes_proto::record::Hex32::new([2; 32]),
                    session_digest: dudes_proto::record::Hex32::new([3; 32]),
                },
            },
        ));
        let lsn_of_decision = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(stamped(
            0,
            dudes_proto::Record::NoteTurnDecided {
                agent: agent(),
                goal: goal_id(),
                note_lsn,
                completion_lsn,
                input_digest: "note".into(),
                gate_trace: Vec::new(),
                decision: dudes_proto::NoteAdmission::Issue {
                    task: "export note".into(),
                },
            },
        ));
        history.push(stamped(
            0,
            dudes_proto::Record::NoteOnlyTurnIssued {
                agent: agent(),
                turn: TurnId::from_parts(7, 3),
                goal: goal_id(),
                task: "export note".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: dudes_types::ModelSelection::default(),
                lsn_of_decision,
                answers: Vec::new(),
                note_lsn,
                completion_lsn,
            },
        ));
        history.push(ended_using(
            agent(),
            TurnId::from_parts(7, 3),
            at(0),
            Provenance::Observed,
            Some(TurnOutcome::Completed),
            Some(Usage::ZERO),
        ));
        let id = MailId::from_parts(4, 100);
        let receipt_lsn = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(mail(id, MailTo::Agent { agent: agent() }));
        let reference = dudes_proto::MailRef {
            id,
            lsn: receipt_lsn,
        };
        let task = dudes_proto::MailWakeTask::new(1).unwrap();
        let trace = || {
            dudes_proto::Gate::ORDER
                .into_iter()
                .map(|gate| dudes_proto::GateStep {
                    gate,
                    fired: gate == dudes_proto::Gate::Work,
                    note: None,
                })
                .collect()
        };
        let root_lsn = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(stamped(
            0,
            Record::MailWakeTurnDecided(dudes_proto::MailWakeDecision {
                agent: agent(),
                goal: goal_id(),
                completion_lsn,
                purpose: dudes_proto::MailWakePurpose::FreshPreparation {
                    plan: dudes_proto::MailWakePlan {
                        trigger: reference,
                        frontier: receipt_lsn,
                        unread_count: 1,
                        task: task.clone(),
                        targets: vec![dudes_proto::MailWakeTarget {
                            mail: reference,
                            original_count: 0,
                            target_count: 1,
                        }],
                    },
                },
                input_digest: dudes_proto::MailWakeDigest {
                    domain: dudes_proto::MailWakeDigestDomain::Fresh,
                    value: dudes_proto::record::Hex32::new([4; 32]),
                },
                gate_trace: trace(),
                decision: dudes_proto::MailWakeAdmission::Issue { task: task.clone() },
            }),
        ));
        history.push(mail_state(id, MailState::Notified { n: 1 }));
        let lsn_of_decision = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(stamped(
            0,
            Record::MailWakeTurnDecided(dudes_proto::MailWakeDecision {
                agent: agent(),
                goal: goal_id(),
                completion_lsn,
                purpose: dudes_proto::MailWakePurpose::ExistingPreparation { root_lsn },
                input_digest: dudes_proto::MailWakeDigest {
                    domain: dudes_proto::MailWakeDigestDomain::Resume,
                    value: dudes_proto::record::Hex32::new([5; 32]),
                },
                gate_trace: trace(),
                decision: dudes_proto::MailWakeAdmission::Issue { task: task.clone() },
            }),
        ));
        history.push(stamped(
            0,
            Record::MailWakeOnlyTurnIssued(dudes_proto::MailWakeIssue {
                agent: agent(),
                goal: goal_id(),
                turn: TurnId::from_parts(7, 4),
                completion_lsn,
                root_lsn,
                lsn_of_decision,
                task,
                session: None,
                mode: Mode::Managed,
                model_selection: dudes_types::ModelSelection::default(),
                answers: Vec::new(),
            }),
        ));
        history
    }

    #[test]
    fn ordinary_records_skip_wake_rows_but_retirement_writes_existing_candidates() {
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &[hired(), goal_set(agent(), goal_id())]);
        assert!(
            !p.export()
                .unwrap()
                .lines()
                .any(|line| line.starts_with("mail_wakes "))
        );
        p.apply(
            Lsn(3),
            WITNESS,
            &stamped(
                0,
                Record::LoopDecided {
                    agent: agent(),
                    input_digest: "completed".into(),
                    decision: dudes_proto::LoopDecision::Done,
                    gate_trace: Vec::new(),
                },
            ),
        )
        .unwrap();
        p.apply(
            Lsn(4),
            WITNESS,
            &mail(MailId::from_parts(4, 100), MailTo::Agent { agent: agent() }),
        )
        .unwrap();
        let read = |p: &Projections| {
            let txn = p.db.begin_read().unwrap();
            get_row::<dudes_proto::mail_wake::MailWakeState>(
                &txn.open_table(MAIL_WAKES).unwrap(),
                &agent().to_bytes(),
            )
            .unwrap()
            .unwrap()
        };
        let candidate = read(&p);
        assert_eq!(candidate.candidate.unwrap().trigger.lsn, Lsn(4));
        p.apply(
            Lsn(5),
            WITNESS,
            &stamped(
                0,
                Record::GoalCleared {
                    agent: agent(),
                    goal: goal_id(),
                },
            ),
        )
        .unwrap();
        let cleared = read(&p);
        assert!(cleared.candidate.is_none());
        assert_eq!(cleared.last_lsn, Lsn(5));
        p.apply(
            Lsn(6),
            WITNESS,
            &stamped(
                0,
                Record::AgentRetired {
                    id: agent(),
                    reason: "finished".into(),
                    browser_retire: None,
                },
            ),
        )
        .unwrap();
        let retired = read(&p);
        assert!(retired.candidate.is_none());
        assert_eq!(retired.last_lsn, Lsn(6));
    }

    fn card_created() -> Entry {
        stamped(
            0,
            Record::CardCreated {
                card: CardId::from_parts(4, 1),
                title: "Exported card".to_owned(),
                department: "eng".to_owned(),
                by: dudes_proto::Actor::Peer { pid: 42, uid: None },
                deps: Vec::new(),
            },
        )
    }

    #[test]
    fn held_cards_follow_current_claim_lsn_through_completion_retirement_and_reclaim() {
        let mut projected = Projections::in_memory().unwrap();
        let a = CardId::from_parts(4, 1);
        let b = CardId::from_parts(4, 2);
        let owner = agent();
        let next = AgentId::from_parts(1, 2);
        let records = [
            Record::CardCreated {
                card: a,
                title: "A".into(),
                department: "eng".into(),
                by: Actor::System,
                deps: vec![],
            },
            Record::CardCreated {
                card: b,
                title: "B".into(),
                department: "eng".into(),
                by: Actor::System,
                deps: vec![],
            },
            Record::Claim {
                card: b,
                agent: owner,
                expected_seq: Seq::ZERO,
            },
            Record::Claim {
                card: a,
                agent: owner,
                expected_seq: Seq::ZERO,
            },
            Record::CardTransition {
                card: b,
                from: CardStatus::Claimed,
                to: CardStatus::Doing,
                by: Actor::Agent { agent: owner },
                expected_seq: Seq(1),
                reason: None,
            },
            Record::CardTransition {
                card: b,
                from: CardStatus::Doing,
                to: CardStatus::Done,
                by: Actor::Agent { agent: owner },
                expected_seq: Seq(2),
                reason: None,
            },
            Record::AgentRetired {
                id: owner,
                reason: "finished".into(),
                browser_retire: None,
            },
            Record::Claim {
                card: a,
                agent: next,
                expected_seq: Seq(2),
            },
            Record::CardTransition {
                card: b,
                from: CardStatus::Done,
                to: CardStatus::Todo,
                by: Actor::System,
                expected_seq: Seq(4),
                reason: None,
            },
            Record::Claim {
                card: b,
                agent: next,
                expected_seq: Seq(5),
            },
        ];
        for (index, record) in records.into_iter().enumerate() {
            projected
                .apply(Lsn(index as u64 + 1), WITNESS, &stamped(0, record))
                .unwrap();
            let held = projected.earliest_held_cards().unwrap();
            match index {
                2..=4 => assert_eq!(held, BTreeMap::from([(owner, b)])),
                5 => assert_eq!(held, BTreeMap::from([(owner, a)])),
                7..=9 => assert_eq!(held, BTreeMap::from([(next, a)])),
                _ => assert!(held.is_empty()),
            }
        }
        assert!(
            serde_json::to_value(projected.card(a).unwrap().unwrap())
                .unwrap()
                .get("claimed_lsn")
                .is_none()
        );
    }

    #[test]
    fn held_cards_replay_a_version_30_cache_past_its_claim_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let entries = [
            card_created(),
            stamped(
                0,
                Record::Claim {
                    card: CardId::from_parts(4, 1),
                    agent: agent(),
                    expected_seq: Seq::ZERO,
                },
            ),
        ];
        {
            let mut projected = Projections::open_with(&path, Fsync::None).unwrap();
            for entry in &entries {
                let (lsn, crc) = writer.append_witnessed(entry).unwrap();
                projected.apply(lsn, crc, entry).unwrap();
            }
        }
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            let mut board = txn.open_table(BOARD).unwrap();
            let id = CardId::from_parts(4, 1).to_bytes();
            let bytes = board.get(&id).unwrap().unwrap().value().to_vec();
            let mut row: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            row.as_object_mut().unwrap().remove("claimed_lsn");
            board
                .insert(&id, serde_json::to_vec(&row).unwrap().as_slice())
                .unwrap();
            drop(board);
            txn.commit().unwrap();
        }
        write_schema_version(&path, 30);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        crate::replay(&crate::Reader::new(&journal), &mut rebuilt).unwrap();
        assert_eq!(
            rebuilt.earliest_held_cards().unwrap(),
            BTreeMap::from([(agent(), CardId::from_parts(4, 1))])
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one acknowledged cursor, old cache layout and reopened replay are compared as one complete migration witness"
    )]
    fn cache_33_replays_raw_note_authority_and_preserves_paired_floor_membership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let mut p = Projections::open_with(&path, Fsync::None).unwrap();
        for e in [hired(), goal_set(agent(), goal_id())] {
            let (lsn, crc) = writer.append_witnessed(&e).unwrap();
            p.apply(lsn, crc, &e).unwrap();
        }
        let completed = stamped(
            0,
            Record::LoopDecided {
                agent: agent(),
                input_digest: "done".into(),
                gate_trace: vec![],
                decision: dudes_proto::LoopDecision::Done,
            },
        );
        let (done, crc) = writer.append_witnessed(&completed).unwrap();
        p.apply(done, crc, &completed).unwrap();
        let queued = stamped(
            0,
            Record::BrowserNoteQueued {
                agent: agent(),
                text: "run the tests".into(),
                destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal_id() },
                browser: dudes_proto::BrowserNoteAttribution {
                    request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
                    request_digest: dudes_proto::record::Hex32::new([2; 32]),
                    session_digest: dudes_proto::record::Hex32::new([3; 32]),
                },
            },
        );
        let (note, crc) = writer.append_witnessed(&queued).unwrap();
        p.apply(note, crc, &queued).unwrap();
        let decided = stamped(
            0,
            Record::NoteTurnDecided {
                agent: agent(),
                goal: goal_id(),
                note_lsn: note,
                completion_lsn: done,
                input_digest: "raw".into(),
                gate_trace: vec![],
                decision: dudes_proto::NoteAdmission::Issue {
                    task: "run the tests".into(),
                },
            },
        );
        let (decision, crc) = writer.append_witnessed(&decided).unwrap();
        p.apply(decision, crc, &decided).unwrap();
        let turn = TurnId::from_parts(9, 1);
        let issue = stamped(
            0,
            Record::NoteOnlyTurnIssued {
                agent: agent(),
                turn,
                goal: goal_id(),
                task: "run the tests".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: dudes_types::ModelSelection::default(),
                lsn_of_decision: decision,
                answers: vec![],
                note_lsn: note,
                completion_lsn: done,
            },
        );
        let (lsn, crc) = writer.append_witnessed(&issue).unwrap();
        p.apply(lsn, crc, &issue).unwrap();
        let thread = dudes_types::ThreadId::from_parts(9, 1);
        let mail = threaded_message(
            MailId::from_parts(9, 1),
            thread,
            vec![agent(), AgentId::from_parts(1, 2)],
        );
        let (lsn, crc) = writer.append_witnessed(&mail).unwrap();
        p.apply(lsn, crc, &mail).unwrap();
        let expected = p.export().unwrap();
        let mail = p.mail_view().unwrap();
        drop(p);
        drop(writer);
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut decisions = tx.open_table(CHAT_DECISIONS).unwrap();
                let key = agent().to_bytes();
                let mut row: serde_json::Value =
                    serde_json::from_slice(decisions.get(&key).unwrap().unwrap().value()).unwrap();
                let note = row["note"].clone();
                row["note"] = serde_json::json!([
                    note["lsn"],
                    note["goal"],
                    note["note_lsn"],
                    note["completion_lsn"],
                    note["issued"],
                    note["reason"]
                ]);
                decisions
                    .insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
                    .unwrap();
                let mut issues = tx.open_table(CHAT_ISSUES).unwrap();
                let key = turn.to_bytes();
                let mut row: serde_json::Value =
                    serde_json::from_slice(issues.get(&key).unwrap().unwrap().value()).unwrap();
                row.as_object_mut().unwrap().remove("contract");
                issues
                    .insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
                    .unwrap();
            }
            tx.commit().unwrap();
        }
        write_schema_version(&path, 33);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert_eq!(
            crate::replay(&crate::Reader::new(&journal), &mut rebuilt)
                .unwrap()
                .applied,
            7
        );
        assert_eq!(rebuilt.export().unwrap(), expected);
        assert_eq!(rebuilt.mail_view().unwrap(), mail);
    }

    fn integration_claim_chat_entries() -> [Entry; 8] {
        let a = CardId::from_parts(4, 1);
        let b = CardId::from_parts(4, 2);
        let mut done = dudes_proto::record::samples::record(dudes_proto::RecordKind::LoopDecided);
        if let Record::LoopDecided {
            agent: owner,
            decision,
            ..
        } = &mut done
        {
            *owner = agent();
            *decision = dudes_proto::LoopDecision::Done;
        }
        let mut note =
            dudes_proto::record::samples::record(dudes_proto::RecordKind::BrowserNoteQueued);
        if let Record::BrowserNoteQueued {
            agent: owner,
            destination,
            text,
            ..
        } = &mut note
        {
            *owner = agent();
            *destination = dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal_id() };
            *text = "after completion".to_owned();
        }
        [
            hired(),
            goal_set(agent(), goal_id()),
            card_created(),
            stamped(
                0,
                Record::CardCreated {
                    card: b,
                    title: "first held".to_owned(),
                    department: "eng".to_owned(),
                    by: Actor::System,
                    deps: Vec::new(),
                },
            ),
            stamped(
                0,
                Record::Claim {
                    card: b,
                    agent: agent(),
                    expected_seq: Seq::ZERO,
                },
            ),
            stamped(
                0,
                Record::Claim {
                    card: a,
                    agent: agent(),
                    expected_seq: Seq::ZERO,
                },
            ),
            stamped(0, done),
            stamped(0, note),
        ]
    }

    fn prior_layout_replays_claim_order_and_chat(version: u64, chat_layout: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let journal = dir.path().join("journal");
        let a = CardId::from_parts(4, 1);
        let b = CardId::from_parts(4, 2);
        let entries = integration_claim_chat_entries();
        let (mut writer, _) = crate::open(
            &journal,
            crate::JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..crate::JournalOptions::default()
            },
        )
        .unwrap();
        let mut completion = Lsn::ZERO;
        let last;
        {
            let mut projected = Projections::open_with(&path, Fsync::None).unwrap();
            for (n, entry) in entries.iter().enumerate() {
                let (lsn, crc) = writer.append_witnessed(entry).unwrap();
                projected.apply(lsn, crc, entry).unwrap();
                if n == 6 {
                    completion = lsn;
                }
            }
            last = projected.last_applied_lsn();
            assert_eq!(
                projected.earliest_held_cards().unwrap(),
                BTreeMap::from([(agent(), b)])
            );
            assert_eq!(
                projected.chat().unwrap().pending[0].text,
                "after completion"
            );
        }
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            if chat_layout {
                let mut board = txn.open_table(BOARD).unwrap();
                for card in [a, b] {
                    let id = card.to_bytes();
                    let bytes = board.get(&id).unwrap().unwrap().value().to_vec();
                    let mut row: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    row.as_object_mut().unwrap().remove("claimed_lsn");
                    board
                        .insert(&id, serde_json::to_vec(&row).unwrap().as_slice())
                        .unwrap();
                }
            } else {
                txn.delete_table(CHAT_NOTES).unwrap();
                txn.delete_table(CHAT_ISSUES).unwrap();
                txn.delete_table(CHAT_DECISIONS).unwrap();
                txn.delete_table(CHAT_COMPLETIONS).unwrap();
                txn.delete_table(CHAT_WINDOWS).unwrap();
            }
            txn.commit().unwrap();
        }
        write_schema_version(&path, version);
        let mut rebuilt = Projections::open_with(&path, Fsync::None).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        let reader = crate::Reader::new(&journal);
        assert_eq!(crate::replay(&reader, &mut rebuilt).unwrap().applied, 8);
        assert_eq!(rebuilt.last_applied_lsn(), last);
        assert_eq!(
            rebuilt.earliest_held_cards().unwrap(),
            BTreeMap::from([(agent(), b)])
        );
        assert_eq!(
            rebuilt.goal(goal_id()).unwrap().unwrap().status,
            dudes_types::GoalStatus::Achieved
        );
        assert_eq!(rebuilt.chat().unwrap().pending[0].text, "after completion");
        let txn = rebuilt.db.begin_read().unwrap();
        assert_eq!(
            get_row::<Lsn>(
                &txn.open_table(CHAT_COMPLETIONS).unwrap(),
                &goal_id().to_bytes()
            )
            .unwrap(),
            Some(completion)
        );
    }

    #[test]
    fn integrated_cache_replays_chat_version_31_past_claim_cursor() {
        prior_layout_replays_claim_order_and_chat(31, true);
    }

    #[test]
    fn integrated_cache_replays_mail_version_31_past_chat_cursor() {
        prior_layout_replays_claim_order_and_chat(31, false);
    }

    #[test]
    fn integrated_cache_replays_conversation_version_33_past_claim_cursor() {
        prior_layout_replays_claim_order_and_chat(33, true);
    }

    #[test]
    fn integrated_cache_replays_version_34_past_claim_cursor() {
        prior_layout_replays_claim_order_and_chat(34, true);
    }

    #[test]
    fn integrated_cache_replays_mail_version_32_past_chat_cursor() {
        prior_layout_replays_claim_order_and_chat(32, false);
    }

    #[test]
    fn a_version_16_store_rebuilds_cards_past_its_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let entry = card_created();
        let id = CardId::from_parts(4, 1);
        {
            let mut projected = Projections::open(&path).unwrap();
            projected.apply(Lsn(1), WITNESS, &entry).unwrap();
            assert_eq!(projected.card(id).unwrap().unwrap().title, "Exported card");
        }
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(BOARD).unwrap();
            txn.commit().unwrap();
        }
        write_schema_version(&path, 16);
        let mut rebuilt = Projections::open(&path).unwrap();
        assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
        assert!(rebuilt.apply(Lsn(1), WITNESS, &entry).unwrap());
        assert_eq!(rebuilt.card(id).unwrap().unwrap().title, "Exported card");
    }

    /// **The journal's authority, enforced rather than asked for.** The tables the export renders
    /// are exactly the tables the store holds, `meta` aside — so a projection that adds a table
    /// and forgets [`Projections::export`] fails here instead of quietly shrinking the only check
    /// invariant 1 has.
    ///
    /// The store's side is **derived** from redb rather than written out again: a hand-kept second
    /// list would agree with [`EXPORTED_TABLES`] by copying, which pins nothing.
    #[test]
    fn the_export_names_every_table_the_store_holds() {
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &history_touching_every_table());

        let txn = p.db.begin_read().unwrap();
        let mut in_store: Vec<String> = txn
            .list_tables()
            .unwrap()
            .map(|handle| handle.name().to_owned())
            .filter(|name| name != META.name())
            .collect();
        in_store.sort();
        let mut exported: Vec<String> = EXPORTED_TABLES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        exported.sort();
        assert_eq!(
            in_store, exported,
            "every table but `meta` contributes to the export"
        );

        // And the constant is not decoration: each of those names really begins a line.
        let export = p.export().unwrap();
        for table in EXPORTED_TABLES {
            assert!(
                export
                    .lines()
                    .any(|line| line.starts_with(&format!("{table} "))),
                "no `{table}` row in the export:\n{export}"
            );
        }
    }

    /// The document's shape: the cursor and its witness first, then one line per row, each line
    /// `<table> <hex key> <json>`. The hex key is the table's own key — here, the agent's id.
    #[test]
    fn the_export_is_a_cursor_a_witness_and_one_line_per_row() {
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &history_touching_every_table());
        let export = p.export().unwrap();
        let lines: Vec<&str> = export.lines().collect();

        assert_eq!(lines[0], format!("lsn {}", p.last_applied_lsn().0));
        assert_eq!(lines[1], format!("crc {}", u64::from(WITNESS)));
        assert!(
            export.ends_with('\n'),
            "the export is a text document: {export:?}"
        );

        let agent_line = lines
            .iter()
            .find(|line| line.starts_with("agents "))
            .expect("one agent was hired");
        let mut parts = agent_line.splitn(3, ' ');
        assert_eq!(parts.next(), Some("agents"));
        let key = parts.next().expect("a key");
        let json = parts.next().expect("a row");
        assert_eq!(
            key,
            hex(&agent().to_bytes()),
            "the key is the table's key, in hex"
        );
        assert_eq!(key.len(), 32, "a ULID key is sixteen bytes of hex");
        let row: AgentRow = serde_json::from_str(json).expect("the row is its own JSON");
        assert_eq!(row, p.agent(agent()).unwrap().unwrap());
    }

    /// A store with no key writes `crc -`, which is not the same document as a store whose
    /// witness happens to be zero. The export keeps those two states apart and so does this.
    #[test]
    fn a_store_with_no_witness_exports_a_dash_and_not_a_zero() {
        let p = Projections::in_memory().unwrap();
        assert!(p.export().unwrap().contains("crc -"));

        // The control: a store that *has* folded something writes the number, so the line above
        // is not passing because the export omits the field.
        let mut folded = Projections::in_memory().unwrap();
        apply_all(&mut folded, &[hired()]);
        let export = folded.export().unwrap();
        assert!(
            export.contains(&format!("crc {}", u64::from(WITNESS))),
            "{export}"
        );
        assert!(!export.contains("crc -"), "{export}");
    }

    /// The export moves when the content moves. Without this, every equality assertion built on
    /// it could be passing on a constant.
    #[test]
    fn the_export_changes_when_a_row_changes() {
        let entries = history_touching_every_table();
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &entries[..entries.len() - 1]);
        let before = p.export().unwrap();

        apply_all_from(
            &mut p,
            Lsn(u64::try_from(entries.len()).unwrap()),
            &entries[entries.len() - 1..],
        );
        let after = p.export().unwrap();
        assert_ne!(before, after, "the last record changed rows and the cursor");

        // Sharper than "they differ": the turn's own line differs, so the change is a row and not
        // only the cursor line at the top.
        let turn_lines = |export: &str| {
            export
                .lines()
                .filter(|line| line.starts_with("turns "))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_ne!(turn_lines(&before), turn_lines(&after));
    }

    fn attached_by_system() -> Entry {
        stamped(
            1,
            Record::Attached {
                agent: agent(),
                by: dudes_proto::Actor::System,
                hook_port: None,
                provenance: None,
            },
        )
    }

    fn question(text: &str) -> Entry {
        stamped(
            2,
            Record::HumanQuestion {
                agent: agent(),
                text: text.into(),
            },
        )
    }

    /// "Waiting" means an open question on an agent that is **not** attached: the question fold
    /// settles the status in the same order as every other fold, so a human holding the keyboard
    /// still reads `Attached` while the question is kept for the Floor to show.
    ///
    /// Control: the same question on a managed agent reads `Waiting`.
    #[test]
    fn an_attached_agent_that_asks_stays_attached_with_its_question() {
        let mut attached = Projections::in_memory().unwrap();
        apply_all(
            &mut attached,
            &[hired(), attached_by_system(), question("which branch?")],
        );
        let row = attached.agent(agent()).unwrap().expect("attached row");
        assert_eq!(row.status, AgentStatus::Attached, "{row:?}");
        assert_eq!(row.question.as_deref(), Some("which branch?"));

        let mut managed = Projections::in_memory().unwrap();
        apply_all(&mut managed, &[hired(), question("which branch?")]);
        let row = managed.agent(agent()).unwrap().expect("managed row");
        assert_eq!(row.status, AgentStatus::Waiting, "control: {row:?}");
        assert_eq!(row.question.as_deref(), Some("which branch?"));
    }

    fn waited(text: &str) -> Entry {
        stamped(
            3,
            Record::LoopDecided {
                agent: agent(),
                input_digest: String::new(),
                decision: dudes_proto::LoopDecision::Wait {
                    question: text.into(),
                },
                gate_trace: Vec::new(),
            },
        )
    }

    fn answered() -> Entry {
        stamped(
            4,
            Record::HumanAnswer {
                agent: agent(),
                text: "main".into(),
                provenance: None,
                browser_answer: None,
            },
        )
    }

    /// A question's identity is the position of the `HumanQuestion` that opened it: that record sets it, a `LoopDecided{Wait}` repeating the text does not, and the
    /// answer clears it with the text.
    #[test]
    fn question_identity_is_set_by_the_question_alone_and_cleared_by_the_answer() {
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &[hired(), waited("stale words")]);
        let row = p.agent(agent()).unwrap().expect("row");
        assert_eq!(row.question.as_deref(), Some("stale words"));
        assert_eq!(row.question_lsn, None, "a decision carries no position");

        apply_all_from(&mut p, Lsn(3), &[question("which branch?")]);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().question_lsn,
            Some(Lsn(3))
        );
        apply_all_from(&mut p, Lsn(4), &[waited("which branch?")]);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().question_lsn,
            Some(Lsn(3)),
            "the decision leaves the question's own position in place"
        );

        apply_all_from(&mut p, Lsn(5), &[answered()]);
        let row = p.agent(agent()).unwrap().unwrap();
        assert_eq!((row.question, row.question_lsn), (None, None));
        assert_eq!(row.status, AgentStatus::Idle);
    }

    /// The bump that goes with this fold: a store the previous build wrote holds an open question with no
    /// position under a cursor already past its `HumanQuestion`, so it must rebuild.
    ///
    /// `LATCHED` is a literal for the reason `a_version_4_store_keeps_no_unconfirmed_session`
    /// gives, and the pin beside it keeps the bump falsifiable.
    #[test]
    fn a_version_13_store_is_rebuilt_for_question_identity() {
        const LATCHED: u64 = 13;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "if this moves, move LATCHED above it — a merge past another bump must re-derive \
             both numbers together"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let entries = [hired(), question("which branch?")];
        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &entries);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().question_lsn,
            Some(Lsn(2))
        );
        drop(p);

        write_schema_version(&path, PROJECTION_VERSION);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn(2), "the current version is kept");
        drop(p);

        write_schema_version(&path, LATCHED);
        let mut p = Projections::open(&path).unwrap();
        assert_eq!(p.counts().unwrap(), (0, 0), "v13 is dropped whole");
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
        apply_all(&mut p, &entries);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().question_lsn,
            Some(Lsn(2))
        );
    }

    fn control(i: i64, record: Record) -> Entry {
        stamped(i, record)
    }

    /// The control identity is the position of the newest `Pause`, `Resume` or `Halt`, each of
    /// which sets it whatever its attribution — a record decoded from the older format with no
    /// `browser` key included. Nothing else moves it.
    #[test]
    fn control_identity_is_set_by_pause_resume_and_halt() {
        let mut p = Projections::in_memory().unwrap();
        apply_all(&mut p, &[hired(), question("which branch?")]);
        assert_eq!(p.agent(agent()).unwrap().unwrap().control_lsn, None);

        let older: Record = serde_json::from_value(serde_json::json!({
            "t": "pause",
            "agent": agent(),
        }))
        .unwrap();
        apply_all_from(&mut p, Lsn(3), &[control(3, older)]);
        assert_eq!(p.agent(agent()).unwrap().unwrap().control_lsn, Some(Lsn(3)));

        apply_all_from(
            &mut p,
            Lsn(4),
            &[control(
                4,
                Record::Halt {
                    agent: agent(),
                    text: None,
                },
            )],
        );
        assert_eq!(p.agent(agent()).unwrap().unwrap().control_lsn, Some(Lsn(4)));

        apply_all_from(
            &mut p,
            Lsn(5),
            &[control(
                5,
                Record::Resume {
                    agent: agent(),
                    browser: Some(dudes_proto::record::BrowserControl {
                        request_id: dudes_proto::record::Hex32::new([1; 32]),
                        request_digest: dudes_proto::record::Hex32::new([2; 32]),
                        control_lsn: Some(Lsn(4)),
                        session_digest: dudes_proto::record::Hex32::new([3; 32]),
                    }),
                },
            )],
        );
        let row = p.agent(agent()).unwrap().unwrap();
        assert_eq!(row.control_lsn, Some(Lsn(5)));
        assert!(!row.paused && !row.halted);

        apply_all_from(&mut p, Lsn(6), &[answered()]);
        assert_eq!(
            p.agent(agent()).unwrap().unwrap().control_lsn,
            Some(Lsn(5)),
            "an answer is not a control record"
        );
    }

    /// The bump that goes with this fold: a store the previous build wrote holds a paused agent with no
    /// control identity under a cursor already past its `Pause`, so it must rebuild.
    ///
    /// `LATCHED` is a literal for the reason `a_version_4_store_keeps_no_unconfirmed_session`
    /// gives, and the pin beside it keeps the bump falsifiable.
    #[test]
    fn a_version_14_store_is_rebuilt_for_control_identity() {
        const LATCHED: u64 = 14;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "if this moves, move LATCHED above it — a merge past another bump must re-derive \
             both numbers together"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let entries = [
            hired(),
            control(
                2,
                Record::Pause {
                    agent: agent(),
                    browser: None,
                },
            ),
        ];
        let mut p = Projections::open(&path).unwrap();
        apply_all(&mut p, &entries);
        assert_eq!(p.agent(agent()).unwrap().unwrap().control_lsn, Some(Lsn(2)));
        drop(p);

        write_schema_version(&path, PROJECTION_VERSION);
        let p = Projections::open(&path).unwrap();
        assert_eq!(p.last_applied_lsn(), Lsn(2), "the current version is kept");
        drop(p);

        write_schema_version(&path, LATCHED);
        let mut p = Projections::open(&path).unwrap();
        assert_eq!(p.counts().unwrap(), (0, 0), "v14 is dropped whole");
        assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
        apply_all(&mut p, &entries);
        assert_eq!(p.agent(agent()).unwrap().unwrap().control_lsn, Some(Lsn(2)));
    }

    /// A managed turn that ran on `session`, confirmed by a `SessionLearned` or not, charged one
    /// token and recording the vendor's running total `total`.
    fn turn_on_session(turn: TurnId, session: &str, confirmed: bool, total: u64) -> Vec<Entry> {
        let mut entries = vec![issued(turn)];
        if confirmed {
            entries.push(stamped(
                0,
                Record::SessionLearned {
                    agent: agent(),
                    turn,
                    session: SessionId::new(session),
                },
            ));
        }
        let mut end = ended_using(
            agent(),
            turn,
            at(0),
            Provenance::Observed,
            Some(TurnOutcome::Completed),
            Some(usage(0, 1)),
        );
        if let Record::TurnEnded {
            session: ended_on,
            session_usage,
            ..
        } = &mut end.record
        {
            *ended_on = Some(SessionId::new(session));
            *session_usage = Some(usage(0, total));
        }
        entries.push(end);
        entries
    }

    fn goal_unknown_ends(p: &Projections) -> u32 {
        p.spend_views(agent(), goal_id(), at(0).utc_day())
            .unwrap()
            .goal_unknown_ends
    }

    /// a confirmed session's running total that falls counts one unknown
    /// end, and a rising one counts none. A turn the vendor did not confirm on the session neither
    /// counts a reset nor replaces the session's last figure, so the later 60 is measured against
    /// the reset turn's 40 and not against the unconfirmed 500.
    #[test]
    fn a_reset_of_a_confirmed_sessions_running_total_counts_one_unknown_end() {
        let mut p = Projections::in_memory().unwrap();
        let mut history = vec![hired(), goal_set(agent(), goal_id())];
        for (i, (confirmed, total)) in [
            (true, 100),
            (true, 150),
            (true, 40),
            (false, 500),
            (true, 60),
        ]
        .into_iter()
        .enumerate()
        {
            let turn = TurnId::from_parts(2, u128::try_from(i).unwrap() + 1);
            history.extend(turn_on_session(turn, "s-a", confirmed, total));
        }
        apply_all(&mut p, &history);
        assert_eq!(goal_unknown_ends(&p), 1);
    }

    /// The store the v15 build leaves is rebuilt on open: it priced the reset turn but counted no
    /// unknown end, and its cursor is already past the record. Same pattern and reasons as
    /// [`a_version_4_store_keeps_no_unconfirmed_session`].
    #[test]
    fn a_version_15_store_counts_the_reset_unknown_end() {
        const LATCHED: u64 = 15;
        assert_eq!(
            PROJECTION_VERSION, 47,
            "if this moves, move LATCHED above it — a merge past another bump must re-derive \
             both numbers together, or this test passes with this repair's own bump dropped"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut history = vec![hired(), goal_set(agent(), goal_id())];
        history.extend(turn_on_session(TurnId::from_parts(2, 1), "s-a", true, 100));
        history.extend(turn_on_session(TurnId::from_parts(2, 2), "s-a", true, 40));
        {
            let mut p = Projections::open(&path).unwrap();
            apply_all(&mut p, &history);
            assert_eq!(goal_unknown_ends(&p), 1);
        }
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut by_goal = txn.open_table(BUDGET_BY_GOAL).unwrap();
                let key = goal_id().to_bytes();
                let mut row: budget::BudgetRow =
                    serde_json::from_slice(by_goal.get(&key).unwrap().unwrap().value()).unwrap();
                row.unknown_ends = 0;
                let bytes = serde_json::to_vec(&row).unwrap();
                by_goal.insert(&key, bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        write_schema_version(&path, LATCHED);

        let mut p = Projections::open(&path).unwrap();
        assert_eq!(
            p.last_applied_lsn(),
            Lsn::ZERO,
            "the stale store was dropped"
        );
        apply_all(&mut p, &history);
        assert_eq!(goal_unknown_ends(&p), 1, "the rebuild counts the reset");
    }

    /// First-completion facts.
    mod first_done {
        use super::*;

        fn predicate_result(goal: GoalId, provenance: Provenance, pass: bool) -> Entry {
            stamped(
                0,
                Record::PredicateResult {
                    goal,
                    turn: None,
                    provenance,
                    pass,
                    evidence: dudes_proto::Evidence::Command {
                        argv: vec!["true".into()],
                        program: None,
                        exit: Some(i32::from(!pass)),
                        output_tail: String::new(),
                    },
                },
            )
        }

        fn done_at(agent: AgentId, millis: i64) -> Entry {
            Entry {
                at: Timestamp::from_unix_millis(millis),
                record: Record::LoopDecided {
                    agent,
                    input_digest: "done".into(),
                    decision: dudes_proto::LoopDecision::Done,
                    gate_trace: Vec::new(),
                },
            }
        }

        fn goal_n(n: u64) -> GoalId {
            GoalId::from_parts(100 + n, 1)
        }

        fn count(p: &Projections, start: i64, end: i64) -> Option<u64> {
            p.first_done_count(
                Timestamp::from_unix_millis(start),
                Timestamp::from_unix_millis(end),
            )
            .unwrap()
            .map(|(count, _)| count)
        }

        fn finished(p: &mut Projections, first: Lsn, goal: GoalId, millis: i64) -> Lsn {
            let entries = [
                goal_set(agent(), goal),
                predicate_result(goal, Provenance::Observed, true),
                done_at(agent(), millis),
            ];
            apply_all_from(p, first, &entries);
            Lsn(first.0 + 3)
        }

        #[test]
        fn an_observed_done_counts_once_and_survives_goal_replacement_and_duplicates() {
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &[hired()]);
            let next = finished(&mut p, Lsn(2), goal_n(1), 5_000);
            assert_eq!(count(&p, 0, 10_000), Some(1));
            assert_eq!(
                p.goal(goal_n(1)).unwrap().unwrap().first_done,
                Some(FirstDone {
                    lsn: Lsn(4),
                    at: Timestamp::from_unix_millis(5_000)
                })
            );
            // A later Done for the same goal, even stamped earlier, neither adds nor moves it.
            apply_all_from(&mut p, next, &[done_at(agent(), 1_000)]);
            assert_eq!(count(&p, 0, 10_000), Some(1));
            assert_eq!(count(&p, 0, 2_000), Some(0));
            // A replacement goal does not erase the prior completion, and counts apart.
            let next = finished(&mut p, Lsn(next.0 + 1), goal_n(2), 6_000);
            assert_eq!(count(&p, 0, 10_000), Some(2));
            let cleared = stamped(
                0,
                Record::GoalCleared {
                    agent: agent(),
                    goal: goal_n(1),
                },
            );
            let retired = stamped(
                0,
                Record::AgentRetired {
                    id: agent(),
                    reason: "done".into(),
                    browser_retire: None,
                },
            );
            apply_all_from(&mut p, next, &[cleared, retired]);
            assert_eq!(count(&p, 0, 10_000), Some(2));
        }

        #[test]
        fn the_window_is_half_open_and_ordered_by_entry_at_not_by_lsn() {
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &[hired()]);
            let next = finished(&mut p, Lsn(2), goal_n(1), 9_000);
            finished(&mut p, next, goal_n(2), -3_000);
            assert_eq!(count(&p, -3_000, 9_000), Some(1), "end is exclusive");
            assert_eq!(count(&p, -3_001, 9_001), Some(2));
            assert_eq!(count(&p, -3_000, -2_999), Some(1), "start is inclusive");
            assert_eq!(count(&p, 9_000, 9_000), Some(0), "an empty window is zero");
        }

        #[test]
        fn a_pass_alone_and_an_inferred_pass_never_count_and_the_latter_withholds_the_total() {
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &[hired(), goal_set(agent(), goal_n(1))]);
            apply_all_from(
                &mut p,
                Lsn(3),
                &[predicate_result(goal_n(1), Provenance::Observed, true)],
            );
            assert_eq!(
                count(&p, 0, 10_000),
                Some(0),
                "a pass without Done is nothing"
            );
            apply_all_from(&mut p, Lsn(4), &[goal_set(agent(), goal_n(2))]);
            apply_all_from(
                &mut p,
                Lsn(5),
                &[predicate_result(goal_n(2), Provenance::Inferred, true)],
            );
            apply_all_from(&mut p, Lsn(6), &[done_at(agent(), 5_000)]);
            assert_eq!(
                count(&p, 0, 10_000),
                None,
                "a Done on an inferred pass is contradictory, so no total is offered"
            );
            assert_eq!(p.goal(goal_n(2)).unwrap().unwrap().first_done, None);
            assert_eq!(
                p.export()
                    .unwrap()
                    .lines()
                    .filter(|line| line.starts_with("goal_first_done 01"))
                    .count(),
                1
            );
        }

        #[test]
        fn a_failing_result_and_a_missing_goal_withhold_the_total() {
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &[hired(), done_at(agent(), 1_000)]);
            assert_eq!(count(&p, 0, 10_000), None, "Done without a goal");
            let mut q = Projections::in_memory().unwrap();
            apply_all(
                &mut q,
                &[
                    hired(),
                    goal_set(agent(), goal_n(1)),
                    predicate_result(goal_n(1), Provenance::Observed, false),
                    done_at(agent(), 1_000),
                ],
            );
            assert_eq!(count(&q, 0, 10_000), None, "Done on a failing result");
        }

        #[test]
        fn the_count_passes_the_activity_window_of_sixty_four() {
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &[hired()]);
            let mut next = Lsn(2);
            for n in 1..=70 {
                next = finished(&mut p, next, goal_n(n), i64::try_from(n).unwrap() * 1_000);
                apply_all_from(
                    &mut p,
                    next,
                    &[stamped(
                        0,
                        Record::Pause {
                            agent: agent(),
                            browser: None,
                        },
                    )],
                );
                next = Lsn(next.0 + 1);
            }
            assert_eq!(p.activity().unwrap().len(), 64);
            assert_eq!(count(&p, 0, 100_000), Some(70));
            assert_eq!(count(&p, 31_000, 71_000), Some(40));
        }

        #[test]
        fn batched_one_by_one_and_reopened_stores_export_identically() {
            let mut history = vec![hired()];
            let mut p = Projections::in_memory().unwrap();
            apply_all(&mut p, &history);
            let mut next = Lsn(2);
            for n in 1..=3 {
                next = finished(&mut p, next, goal_n(n), i64::try_from(n).unwrap() * 7_000);
            }
            history.extend([
                goal_set(agent(), goal_n(1)),
                predicate_result(goal_n(1), Provenance::Observed, true),
                done_at(agent(), 7_000),
                goal_set(agent(), goal_n(2)),
                predicate_result(goal_n(2), Provenance::Observed, true),
                done_at(agent(), 14_000),
                goal_set(agent(), goal_n(3)),
                predicate_result(goal_n(3), Provenance::Observed, true),
                done_at(agent(), 21_000),
            ]);
            let live = p.export().unwrap();
            let entries: Vec<_> = history
                .iter()
                .enumerate()
                .map(|(i, entry)| (Lsn(u64::try_from(i).unwrap() + 1), WITNESS, entry))
                .collect();
            let mut batch = Projections::in_memory().unwrap();
            batch.apply_batch(entries).unwrap();
            assert_eq!(batch.export().unwrap(), live, "one batch");
            assert!(live.contains("goal_first_done "));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.redb");
            {
                let mut reopened = Projections::open(&path).unwrap();
                apply_all(&mut reopened, &history[..5]);
            }
            let mut reopened = Projections::open(&path).unwrap();
            apply_all_from(&mut reopened, Lsn(6), &history[5..]);
            assert_eq!(reopened.export().unwrap(), live, "resumed after a reopen");
            assert_eq!(count(&reopened, 0, 100_000), Some(3));
        }

        #[test]
        fn a_version_38_store_rebuilds_the_first_completion_facts() {
            assert_eq!(PROJECTION_VERSION, 47);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.redb");
            let history = [
                hired(),
                goal_set(agent(), goal_n(1)),
                predicate_result(goal_n(1), Provenance::Observed, true),
                done_at(agent(), 5_000),
            ];
            {
                let mut p = Projections::open(&path).unwrap();
                apply_all(&mut p, &history);
            }
            write_schema_version(&path, 38);
            let mut p = Projections::open(&path).unwrap();
            assert_eq!(p.last_applied_lsn(), Lsn::ZERO);
            assert_eq!(count(&p, 0, 10_000), Some(0));
            apply_all(&mut p, &history);
            assert_eq!(count(&p, 0, 10_000), Some(1));
        }
    }
}
