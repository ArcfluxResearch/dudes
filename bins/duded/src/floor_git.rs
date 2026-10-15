//! The Floor's opt-in Git read route: the bounded request, the `git` snapshot
//! sibling, and the limits that keep one operator from running the daemon's `git` without end.
//!
//! The route is a pull, never a stream: the Floor asks when the operator opens or refreshes the
//! tab. One read runs daemon-wide, one session may read once per interval, and the read itself runs
//! on a worker the connection thread waits for. **At the deadline the connection answers
//! `timed_out`, releases the daemon-wide slot and abandons the worker**, so a file system that
//! never answers cannot hold the Floor listener's stop past its join bound. Abandoned workers are
//! counted, and at [`MAX_ABANDONED`] the route answers `busy` until one ends.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_proto::{GitState, GitView};
use dudes_supervisor::World;
use dudes_types::{AgentId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::git_read::{Lease, Tree};

/// Most bytes one request body may carry, checked before a byte of it is read. The canonical body
/// is under 60.
pub const MAX_GIT_BODY_BYTES: usize = 128;
/// How soon the same session may read again.
pub const SESSION_INTERVAL: Duration = Duration::from_secs(2);
/// How many abandoned workers the route tolerates before it answers `busy`.
pub const MAX_ABANDONED: usize = 2;
/// How often the waiting connection looks at its stop signal.
const POLL: Duration = Duration::from_millis(25);

/// The `git` sibling a Floor snapshot carries exactly when the Git tab is enabled: an empty
/// object, outside `FloorProjection` and `floor.delta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct GitSwitch {}

/// The one value [`GitSwitch`] takes.
pub const GIT_SWITCH: GitSwitch = GitSwitch {};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitJson {
    agent_id: String,
}

/// One bounded request: the agent to read, and nothing else. No revision, path or pattern comes
/// from the browser.
#[derive(Clone)]
pub struct GitRequest {
    pub(crate) agent: AgentId,
}

impl fmt::Debug for GitRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitRequest(<redacted>)")
    }
}

impl GitRequest {
    /// Decode one body: one flat object with exactly one string member.
    ///
    /// # Errors
    ///
    /// A body over [`MAX_GIT_BODY_BYTES`], one that is not an object, one with an unknown, missing
    /// or repeated member, and an agent id that does not parse.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_GIT_BODY_BYTES {
            bail!("Floor git request is too large");
        }
        // The derived decoder also reads a struct from a positional array; only an object is the
        // request this route describes.
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor git request is not one JSON object");
        }
        let raw: GitJson =
            serde_json::from_slice(bytes).context("invalid Floor git request JSON")?;
        let agent =
            AgentId::parse(&raw.agent_id).map_err(|_| anyhow!("invalid Floor git agent"))?;
        Ok(Self { agent })
    }
}

/// What the loop answers about the agent a request names, from recorded values only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitTarget {
    /// A live agent: read this tree on the worker.
    Read(Tree),
    /// A retired agent whose worktree has no removal recorded. Its tree is not read.
    Released,
    /// A retired agent whose worktree's removal is recorded.
    Removed,
}

impl GitTarget {
    /// The target for `agent` in `world`, or `None` when no live agent and no retired worktree
    /// agent has this id. A retired agent's worktree is answered by its recorded removal alone:
    /// one that a recorded child still holds active is released here, as the Floor shows it.
    #[must_use]
    pub fn of(world: &World, agent: AgentId) -> Option<Self> {
        let row = world.agents.get(&agent)?;
        let Some(worktree) = row.worktree else {
            return (!row.retired).then(|| Self::Read(Tree::Checkout(row.cwd.clone())));
        };
        Some(match world.worktrees.get(&worktree) {
            Some(lease) if lease.removed.is_none() && !row.retired => {
                Self::Read(Tree::Leased(Lease {
                    worktree,
                    origin: lease.origin.clone(),
                    path: lease.path.clone(),
                    base: lease.base,
                }))
            }
            Some(lease) if lease.removed.is_none() => Self::Released,
            _ => Self::Removed,
        })
    }
}

/// What reads a tree. [`crate::git_read::GitReader`] in production; a test replaces it to hold a
/// read open.
pub trait CheckoutRead: Send + Sync + 'static {
    /// Read `tree`, stamped `now`.
    fn read(&self, tree: &Tree, now: Timestamp) -> GitView;
}

impl CheckoutRead for crate::git_read::GitReader {
    fn read(&self, tree: &Tree, now: Timestamp) -> GitView {
        Self::read(self, tree, now)
    }
}

#[derive(Default)]
struct State {
    /// A read holds the daemon-wide slot.
    running: bool,
    /// When each session last began a read, for the session interval.
    last: BTreeMap<[u8; 32], Instant>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Workers whose connection gave up on them and which have not ended.
    abandoned: Arc<AtomicUsize>,
}

/// The daemon-wide slot, released when the read ends, times out or is abandoned.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
    }
}

/// One worker's end, which the connection and the worker settle between them under one lock.
#[derive(Default)]
pub(crate) struct Worker {
    ending: Mutex<Ending>,
}

#[derive(Default)]
struct Ending {
    done: bool,
    abandoned: bool,
}

impl Worker {
    /// The connection stops waiting. Counted only if the worker has not already ended.
    pub(crate) fn abandon(&self, abandoned: &AtomicUsize) {
        let mut ending = self.ending.lock().unwrap_or_else(PoisonError::into_inner);
        if !ending.done {
            ending.abandoned = true;
            abandoned.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The worker ends, normally or by unwinding. An abandoned worker stops counting.
    fn finish(&self, abandoned: &AtomicUsize) {
        let mut ending = self.ending.lock().unwrap_or_else(PoisonError::into_inner);
        ending.done = true;
        if ending.abandoned {
            abandoned.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Settles a worker's end when the worker's closure ends, however it ends.
pub(crate) struct Finish {
    pub(crate) worker: Arc<Worker>,
    pub(crate) abandoned: Arc<AtomicUsize>,
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.worker.finish(&self.abandoned);
    }
}

/// The route's reader and limits, shared by every Floor connection.
pub struct GitReads {
    reader: Arc<dyn CheckoutRead>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    shared: Arc<Shared>,
    interval: Duration,
    deadline: Duration,
}

impl fmt::Debug for GitReads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitReads").finish_non_exhaustive()
    }
}

impl GitReads {
    /// The route over `reader`, stamping replies with `clock`, with the limits.
    #[must_use]
    pub fn new(
        reader: Arc<dyn CheckoutRead>,
        clock: impl Fn() -> Timestamp + Send + Sync + 'static,
    ) -> Self {
        Self {
            reader,
            clock: Arc::new(clock),
            shared: Arc::default(),
            interval: SESSION_INTERVAL,
            deadline: crate::git_read::READ_DEADLINE,
        }
    }

    /// The same route with other limits, for tests that would otherwise wait out the real ones.
    #[must_use]
    #[doc(hidden)]
    pub fn with_limits(mut self, interval: Duration, deadline: Duration) -> Self {
        self.interval = interval;
        self.deadline = deadline;
        self
    }

    /// How many abandoned workers have not yet ended.
    #[must_use]
    #[doc(hidden)]
    pub fn abandoned(&self) -> usize {
        self.shared.abandoned.load(Ordering::Acquire)
    }

    fn begin(&self, session: [u8; 32]) -> Option<Slot> {
        if self.shared.abandoned.load(Ordering::Acquire) >= MAX_ABANDONED {
            return None;
        }
        let now = Instant::now();
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state
            .last
            .retain(|_, began| now.duration_since(*began) < self.interval);
        if state.running || state.last.contains_key(&session) {
            return None;
        }
        state.running = true;
        state.last.insert(session, now);
        Some(Slot(Arc::clone(&self.shared)))
    }

    /// Answer one authenticated request from `session`.
    ///
    /// `lookup` asks the loop what the journal records about the agent; `stopped` is polled while
    /// the read runs. `None` means the listener is stopping and no answer is owed.
    pub(crate) fn serve(
        &self,
        session: [u8; 32],
        lookup: impl FnOnce() -> Result<Option<GitTarget>, crate::ports::Unavailable>,
        stopped: impl Fn() -> bool,
    ) -> Result<Option<GitView>, crate::ports::Unavailable> {
        let now = (self.clock)();
        let Some(_slot) = self.begin(session) else {
            return Ok(Some(GitView::refused(GitState::Busy, now)));
        };
        let tree = match lookup()? {
            Some(GitTarget::Read(tree)) => tree,
            Some(GitTarget::Released) => {
                return Ok(Some(GitView::refused(GitState::WorktreeReleased, now)));
            }
            Some(GitTarget::Removed) => {
                return Ok(Some(GitView::refused(GitState::WorktreeRemoved, now)));
            }
            None => return Ok(Some(GitView::refused(GitState::AgentUnknown, now))),
        };
        let (tx, rx) = mpsc::channel();
        let worker = Arc::new(Worker::default());
        let finish = Finish {
            worker: Arc::clone(&worker),
            abandoned: Arc::clone(&self.shared.abandoned),
        };
        let reader = Arc::clone(&self.reader);
        let spawned = thread::Builder::new()
            .name("dudes-git-read".to_owned())
            .spawn(move || {
                let _finish = finish;
                let _ = tx.send(reader.read(&tree, now));
            });
        if spawned.is_err() {
            return Ok(Some(GitView::refused(GitState::Unreadable, now)));
        }
        let started = Instant::now();
        loop {
            if stopped() {
                worker.abandon(&self.shared.abandoned);
                return Ok(None);
            }
            let left = self.deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                worker.abandon(&self.shared.abandoned);
                return Ok(Some(GitView::refused(GitState::TimedOut, now)));
            }
            match rx.recv_timeout(left.min(POLL)) {
                Ok(view) => return Ok(Some(view)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Ok(Some(GitView::refused(GitState::Unreadable, now)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dudes_proto::{AgentBudget, Entry, Record, WorktreeRemoval};
    use dudes_types::{GitCommit, Lsn, Provider, WorktreeId};

    use super::*;

    const AGENT: &str = "01J00000000000000000000000";

    #[test]
    fn a_request_is_one_object_with_one_agent_id() {
        let body = format!("{{\"agent_id\":\"{AGENT}\"}}");
        assert!(GitRequest::parse(body.as_bytes()).is_ok());
        for bad in [
            format!("[\"{AGENT}\"]"),
            format!("{{\"agent_id\":\"{AGENT}\",\"path\":\"/etc\"}}"),
            format!("{{\"agent_id\":\"{AGENT}\",\"agent_id\":\"{AGENT}\"}}"),
            "{\"agent_id\":\"not-an-id\"}".to_owned(),
            "{\"agent_id\":7}".to_owned(),
            "{}".to_owned(),
            String::new(),
            format!(
                "{{\"agent_id\":\"{AGENT}\"}}{}",
                " ".repeat(MAX_GIT_BODY_BYTES)
            ),
        ] {
            assert!(GitRequest::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_sibling_is_an_empty_object() {
        assert_eq!(serde_json::to_string(&GIT_SWITCH).unwrap(), "{}");
    }

    const NOW: Timestamp = Timestamp::from_unix_millis(5);
    const FAST: Duration = Duration::from_millis(20);

    /// A reader the test controls: it ends when told to, or never.
    struct Gated {
        release: Mutex<mpsc::Receiver<()>>,
        reads: AtomicUsize,
        panics: bool,
    }

    impl Gated {
        fn new(panics: bool) -> (Arc<Self>, mpsc::Sender<()>) {
            let (release, gate) = mpsc::channel();
            let reader = Arc::new(Self {
                release: Mutex::new(gate),
                reads: AtomicUsize::new(0),
                panics,
            });
            (reader, release)
        }
    }

    impl CheckoutRead for Gated {
        fn read(&self, _tree: &Tree, now: Timestamp) -> GitView {
            self.reads.fetch_add(1, Ordering::AcqRel);
            assert!(!self.panics, "a reader that panics");
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
            let mut view = GitView::refused(GitState::Ok, now);
            view.head = None;
            view
        }
    }

    fn reads(reader: Arc<Gated>, interval: Duration, deadline: Duration) -> GitReads {
        GitReads::new(reader, || NOW).with_limits(interval, deadline)
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the type `serve` takes for its lookup"
    )]
    fn here() -> Result<Option<GitTarget>, crate::ports::Unavailable> {
        Ok(Some(GitTarget::Read(Tree::Checkout(PathBuf::from(
            "/checkout",
        )))))
    }

    fn state(view: Result<Option<GitView>, crate::ports::Unavailable>) -> GitState {
        view.expect("the lookup is available")
            .expect("an answer")
            .state
    }

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn a_read_that_ends_answers_and_frees_the_slot() {
        let (reader, release) = Gated::new(false);
        release.send(()).unwrap();
        let reads = reads(reader, Duration::ZERO, Duration::from_secs(5));
        assert_eq!(state(reads.serve(key(1), here, || false)), GitState::Ok);
        release.send(()).unwrap();
        assert_eq!(
            state(reads.serve(key(2), here, || false)),
            GitState::Ok,
            "the next session may read at once"
        );
        assert_eq!(reads.abandoned(), 0);
    }

    #[test]
    fn a_read_past_its_deadline_answers_timed_out_frees_the_slot_and_is_counted() {
        let (reader, release) = Gated::new(false);
        let reads = reads(reader.clone(), Duration::ZERO, FAST);
        let started = Instant::now();
        assert_eq!(
            state(reads.serve(key(1), here, || false)),
            GitState::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(reads.abandoned(), 1, "the worker still runs and is counted");

        release.send(()).unwrap();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while reads.abandoned() != 0 {
            assert!(
                Instant::now() < deadline,
                "the abandoned worker was never uncounted"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            state(reads.serve(key(2), here, || false)),
            GitState::Ok,
            "the slot was free while the worker ran, and nothing is abandoned now"
        );
    }

    #[test]
    fn two_abandoned_workers_make_the_route_busy_until_one_ends() {
        let (reader, release) = Gated::new(false);
        let reads = reads(reader.clone(), Duration::ZERO, FAST);
        for session in 1..=MAX_ABANDONED {
            let session = u8::try_from(session).unwrap();
            assert_eq!(
                state(reads.serve(key(session), here, || false)),
                GitState::TimedOut
            );
        }
        assert_eq!(reads.abandoned(), MAX_ABANDONED);
        let before = reader.reads.load(Ordering::Acquire);
        assert_eq!(state(reads.serve(key(9), here, || false)), GitState::Busy);
        assert_eq!(
            reader.reads.load(Ordering::Acquire),
            before,
            "a busy answer starts no read"
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while reads.abandoned() == MAX_ABANDONED {
            assert!(Instant::now() < deadline, "no worker ended");
            thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        release.send(()).unwrap();
        assert_ne!(state(reads.serve(key(9), here, || false)), GitState::Busy);
    }

    #[test]
    fn one_read_runs_at_a_time_and_a_session_waits_out_its_interval() {
        let (reader, release) = Gated::new(false);
        let reads = Arc::new(reads(
            reader,
            Duration::from_millis(300),
            Duration::from_secs(5),
        ));
        let first = {
            let reads = Arc::clone(&reads);
            thread::spawn(move || state(reads.serve(key(1), here, || false)))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reads.shared.state.lock().unwrap().running {
            assert!(Instant::now() < deadline, "the first read never started");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            state(reads.serve(key(2), here, || false)),
            GitState::Busy,
            "another session, while one read runs"
        );
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), GitState::Ok);
        assert_eq!(
            state(reads.serve(key(1), here, || false)),
            GitState::Busy,
            "the same session, inside its interval"
        );
        thread::sleep(Duration::from_millis(350));
        release.send(()).unwrap();
        assert_eq!(state(reads.serve(key(1), here, || false)), GitState::Ok);
    }

    #[test]
    fn an_unknown_agent_and_a_retired_worktree_are_states_and_start_no_read() {
        let (reader, _release) = Gated::new(false);
        let reads = reads(reader.clone(), Duration::ZERO, FAST);
        for (session, target, answer) in [
            (1, None, GitState::AgentUnknown),
            (2, Some(GitTarget::Released), GitState::WorktreeReleased),
            (3, Some(GitTarget::Removed), GitState::WorktreeRemoved),
        ] {
            assert_eq!(
                state(reads.serve(key(session), || Ok(target), || false)),
                answer
            );
        }
        assert_eq!(reader.reads.load(Ordering::Acquire), 0);
    }

    #[test]
    fn an_unavailable_lookup_starts_no_read_and_is_not_an_unknown_agent() {
        let (reader, _release) = Gated::new(false);
        let reads = reads(reader.clone(), Duration::ZERO, FAST);
        assert!(matches!(
            reads.serve(key(1), || Err(crate::ports::Unavailable), || false),
            Err(crate::ports::Unavailable)
        ));
        assert_eq!(reader.reads.load(Ordering::Acquire), 0);
        assert_eq!(reads.abandoned(), 0);
    }

    #[test]
    fn a_stopping_listener_gets_no_answer_and_does_not_wait_for_the_read() {
        let (reader, _release) = Gated::new(false);
        let reads = reads(reader, Duration::ZERO, Duration::from_secs(30));
        let started = Instant::now();
        assert!(reads.serve(key(1), here, || true).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(reads.abandoned(), 1);
    }

    #[test]
    fn a_reader_that_panics_is_unreadable_and_leaves_nothing_counted() {
        let (reader, _release) = Gated::new(true);
        let reads = reads(reader, Duration::ZERO, Duration::from_secs(5));
        assert_eq!(
            state(reads.serve(key(1), here, || false)),
            GitState::Unreadable
        );
        assert_eq!(reads.abandoned(), 0);
        assert_eq!(
            state(reads.serve(key(2), here, || false)),
            GitState::Unreadable,
            "the slot was freed"
        );
    }

    /// A world folded from `records`, numbered from one.
    fn world(records: Vec<Record>) -> World {
        let mut world = World::default();
        for (at, record) in (1..).zip(records) {
            world.apply(
                Lsn(at),
                &Entry {
                    at: Timestamp::from_unix_millis(i64::try_from(at).unwrap()),
                    record,
                },
            );
        }
        world
    }

    fn hired(id: AgentId, cwd: &str, worktree: Option<WorktreeId>) -> Record {
        Record::AgentHired {
            id,
            name: "Ada".into(),
            character: None,
            accent: None,
            role: "engineer".into(),
            department: "eng".into(),
            provider: Provider::Claude,
            cwd: PathBuf::from(cwd),
            workspace: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: worktree.is_some(),
            worktree,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }
    }

    fn retired(id: AgentId) -> Record {
        Record::AgentRetired {
            id,
            reason: "done".into(),
            browser_retire: None,
        }
    }

    #[test]
    fn the_lookup_reads_the_recorded_lease_and_answers_a_retired_worktree_by_its_removal() {
        let agent = AgentId::parse(AGENT).unwrap();
        let plain = AgentId::parse("01J00000000000000000000001").unwrap();
        let worktree = WorktreeId::from_bytes([0xab; 16]);
        let base = GitCommit::parse(&"c".repeat(40)).unwrap();
        let leased = Record::WorktreeLeased {
            worktree,
            origin: PathBuf::from("/repo"),
            path: PathBuf::from("/data/worktrees/abab"),
            branch: "dudes/abababababab".into(),
            base,
            from: None,
        };
        let removed = Record::WorktreeRemoved {
            worktree,
            how: WorktreeRemoval::Operator,
        };
        let hire = hired(agent, "/data/worktrees/abab", Some(worktree));

        let live = world(vec![
            leased.clone(),
            hire.clone(),
            hired(plain, "/plain", None),
        ]);
        assert_eq!(
            GitTarget::of(&live, agent),
            Some(GitTarget::Read(Tree::Leased(Lease {
                worktree,
                origin: PathBuf::from("/repo"),
                path: PathBuf::from("/data/worktrees/abab"),
                base,
            })))
        );
        assert_eq!(
            GitTarget::of(&live, plain),
            Some(GitTarget::Read(Tree::Checkout(PathBuf::from("/plain"))))
        );

        let gone = world(vec![
            leased.clone(),
            hire.clone(),
            hired(plain, "/plain", None),
            retired(agent),
            retired(plain),
        ]);
        assert_eq!(GitTarget::of(&gone, agent), Some(GitTarget::Released));
        assert_eq!(GitTarget::of(&gone, plain), None, "a retired plain agent");

        let cleared = world(vec![leased, hire, retired(agent), removed]);
        assert_eq!(GitTarget::of(&cleared, agent), Some(GitTarget::Removed));

        let unrecorded = world(vec![hired(agent, "/data/worktrees/abab", Some(worktree))]);
        assert_eq!(
            GitTarget::of(&unrecorded, agent),
            Some(GitTarget::Removed),
            "a hire that names no recorded lease is never read"
        );
        assert_eq!(
            GitTarget::of(&live, AgentId::parse("01J00000000000000000000002").unwrap()),
            None
        );
    }
}
