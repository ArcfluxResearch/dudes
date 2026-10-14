//! A new goal's completion check, run once before anything is appended.
//!
//! Every route that sets a goal shares this module: the outcome table, the run on its own thread,
//! the requesting thread's wait, and the per-agent and per-session limits on browser runs. Each
//! route keeps its own admission, which runs once before the baseline starts and once more, as the
//! recheck, in the same loop job as the route's append.
//!
//! A baseline is never a verdict. It emits no `PredicateDone`, no `PredicateResult` and no
//! `LoopDecided`, so the done gate never reads it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dudes_proto::record::Hex32;
use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData, RpcError};
use dudes_proto::{CardView, CheckBaseline, Predicate};
use dudes_providers::launch::EnvVar;
use dudes_types::AgentId;

use dudes_rpc::HangUp;

use crate::ports::Handle;
use crate::predicate::{self, PredicateOutcome, PredicateRunner};

/// The most baselines one Floor session may start in its lifetime.
pub(crate) const SESSION_BASELINES: u32 = 32;

/// What one baseline decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Baseline {
    /// The check did not pass before work; the goal records this.
    Admitted(CheckBaseline),
    /// Refused before any append.
    Refused(Refusal),
    /// No verdict: the run was swept by a daemon exit, its thread could not start, the runner
    /// errored, or the projection could not be read. Nothing was appended, and a retry may pass.
    Unavailable,
}

/// Why a baseline refused the goal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The check could not run: its directory or program is missing, the program canonicalized
    /// inside the tree or is not UTF-8, or the board card does not exist. Carries why.
    CannotRun(String),
    /// The check passed before any work.
    AlreadyPasses,
    /// The run timed out or a signal ended it, so it shows nothing about the state before work.
    DidNotFinish,
}

impl Refusal {
    /// The reason a browser route answers.
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::CannotRun(_) => "check_cannot_run",
            Self::AlreadyPasses => "check_already_passes",
            Self::DidNotFinish => "check_did_not_finish",
        }
    }
}

/// Map one finished command run. `program` is the canonical program the run spawned.
pub(crate) fn command(outcome: &PredicateOutcome, program: String) -> Baseline {
    match outcome {
        PredicateOutcome::Swept => Baseline::Unavailable,
        PredicateOutcome::Finished {
            timed_out: true, ..
        }
        | PredicateOutcome::Finished { exit: None, .. } => Baseline::Refused(Refusal::DidNotFinish),
        PredicateOutcome::Finished { exit: Some(0), .. } => {
            Baseline::Refused(Refusal::AlreadyPasses)
        }
        PredicateOutcome::Finished {
            exit: Some(exit), ..
        } => Baseline::Admitted(CheckBaseline::CommandFailed {
            program,
            exit: *exit,
        }),
    }
}

/// Read a board check, with the same eligibility a post-turn run applies.
pub(crate) fn board(card: Option<&CardView>, predicate: &Predicate) -> Baseline {
    let Predicate::BoardState { card: id, .. } = predicate else {
        return Baseline::Unavailable;
    };
    match card {
        None => Baseline::Refused(Refusal::CannotRun(format!(
            "the board card {id} does not exist"
        ))),
        Some(view) if crate::board::predicate_result(view, predicate).0 => {
            Baseline::Refused(Refusal::AlreadyPasses)
        }
        Some(_) => Baseline::Admitted(CheckBaseline::BoardNotMet),
    }
}

/// Who started a run, for the counts the routes read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    /// The agent whose goal the run decides; `None` for a hire, which has no agent yet.
    agent: Option<AgentId>,
    /// The Floor session that started it; `None` for a local route.
    session: Option<Hex32>,
}

/// Why a browser route may not start a run now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    /// A browser run for this agent, or for this session, has not ended.
    Running,
    /// This session has started [`SESSION_BASELINES`] runs.
    Exhausted,
}

impl Limit {
    /// The reason a browser route answers.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Running => "check_running",
            Self::Exhausted => "check_limit",
        }
    }
}

/// How many runs each Floor session has started. Shared with the Floor session export and
/// import, which carry the counts across `dudes upgrade`'s restart and are the only other users.
#[derive(Clone, Debug, Default)]
pub(crate) struct CheckRuns(Arc<Mutex<BTreeMap<Hex32, u32>>>);

impl CheckRuns {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Hex32, u32>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The runs `session` has started, none when it has started none.
    pub(crate) fn started(&self, session: Hex32) -> u32 {
        self.lock().get(&session).copied().unwrap_or(0)
    }

    /// Continue `session`'s count from `started`.
    pub(crate) fn seed(&self, session: Hex32, started: u32) {
        self.lock().insert(session, started);
    }
}

/// The runs the loop thread counts. Run-local, except that `dudes upgrade` carries each Floor
/// session's count into the next run along with the session.
#[derive(Debug, Default)]
pub(crate) struct Baselines {
    next: u64,
    running: BTreeMap<u64, Mark>,
    started: CheckRuns,
}

impl Baselines {
    /// The shared per-session counts.
    pub(crate) fn check_runs(&self) -> CheckRuns {
        self.started.clone()
    }

    /// Count a run a local route starts for `agent`. Local routes have no limit.
    pub(crate) fn start_local(&mut self, agent: AgentId) -> u64 {
        self.insert(Mark {
            agent: Some(agent),
            session: None,
        })
    }

    /// Count a run a browser route starts for `agent` (none for a hire) under `session`, or say
    /// which limit refuses it.
    pub(crate) fn start_browser(
        &mut self,
        agent: Option<AgentId>,
        session: Hex32,
    ) -> Result<u64, Limit> {
        if self.running.values().any(|mark| {
            mark.session == Some(session)
                || (mark.session.is_some() && agent.is_some() && mark.agent == agent)
        }) {
            return Err(Limit::Running);
        }
        let mut counts = self.started.lock();
        let started = counts.entry(session).or_default();
        if *started >= SESSION_BASELINES {
            return Err(Limit::Exhausted);
        }
        *started += 1;
        drop(counts);
        Ok(self.insert(Mark {
            agent,
            session: Some(session),
        }))
    }

    /// Stop counting a run. Idempotent: the run's thread and its recheck both end it.
    pub(crate) fn end(&mut self, id: u64) {
        self.running.remove(&id);
    }

    /// Whether a run a local route started for `agent` has not ended.
    pub(crate) fn local_running(&self, agent: AgentId) -> bool {
        self.running
            .values()
            .any(|mark| mark.session.is_none() && mark.agent == Some(agent))
    }

    fn insert(&mut self, mark: Mark) -> u64 {
        self.next += 1;
        self.running.insert(self.next, mark);
        self.next
    }
}

/// What a run needs, owned, so it can leave the loop thread.
pub(crate) struct Run<P> {
    /// The runner every predicate child goes through, so the exit path's sweep reaches this one.
    pub(crate) runner: Arc<P>,
    /// The handle the run's end is reported through.
    pub(crate) handle: Handle,
    /// The daemon-owned environment and `PATH`.
    pub(crate) env: Vec<EnvVar>,
    /// The daemon's `PATH`, which resolves the program.
    pub(crate) path: Option<OsString>,
}

/// One command check run in flight, as the requesting thread holds it.
pub struct Ticket {
    /// The run's count among [`Baselines`].
    pub(crate) id: u64,
    /// The operator's argv, for the refusal a terminal prints.
    pub(crate) argv: Vec<String>,
    /// The directory the check runs in.
    pub(crate) cwd: PathBuf,
    /// The check's own timeout.
    pub(crate) timeout_s: u64,
    outcome: Receiver<Baseline>,
    /// Set to end the run's child early, when its caller has gone.
    cancel: Arc<AtomicBool>,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Ticket {
    /// Wait for the run. A daemon that is stopping answers [`Baseline::Unavailable`] at once, so a
    /// waiting connection never holds the exit path's join; the run's child is swept with every
    /// other predicate child. A caller that `hang_up` reports gone ends the run's child and gets
    /// [`Baseline::Unavailable`], so its check does not run on beside the agent's next goal.
    pub(crate) fn wait(&self, stopping: &AtomicBool, hang_up: &HangUp) -> Baseline {
        loop {
            match recv_for(stopping, &self.outcome, WAIT_TICK) {
                Waited::Answer(answer) => return answer,
                Waited::Gone => return Baseline::Unavailable,
                Waited::Pending if hang_up.hung_up() => {
                    self.cancel.store(true, Ordering::Release);
                    return Baseline::Unavailable;
                }
                Waited::Pending => {}
            }
        }
    }
}

/// How long one wait for a run lasts before the caller's hang-up is read again.
const WAIT_TICK: Duration = Duration::from_millis(100);

/// What one bounded wait for an answer saw.
enum Waited<T> {
    /// The answer.
    Answer(T),
    /// The daemon is stopping, or the answer can no longer come.
    Gone,
    /// Neither, yet.
    Pending,
}

/// Wait up to `tick` for one answer. An answer already waiting is taken even while the daemon
/// stops; otherwise a stopping daemon ends the wait at once, so a waiting connection never holds
/// the exit path's join on connections.
fn recv_for<T>(stopping: &AtomicBool, answers: &Receiver<T>, tick: Duration) -> Waited<T> {
    let deadline = std::time::Instant::now() + tick;
    loop {
        match answers.try_recv() {
            Ok(answer) => return Waited::Answer(answer),
            Err(mpsc::TryRecvError::Disconnected) => return Waited::Gone,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if stopping.load(Ordering::Acquire) {
            return Waited::Gone;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Waited::Pending;
        }
        match answers.recv_timeout(left.min(Duration::from_millis(25))) {
            Ok(answer) => return Waited::Answer(answer),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Waited::Gone,
        }
    }
}

/// Wait for one answer, or `None` once the daemon is stopping or the answer can no longer come.
pub(crate) fn recv_until_stopping<T>(stopping: &AtomicBool, answers: &Receiver<T>) -> Option<T> {
    loop {
        match recv_for(stopping, answers, WAIT_TICK) {
            Waited::Answer(answer) => return Some(answer),
            Waited::Gone => return None,
            Waited::Pending => {}
        }
    }
}

/// Start one command check on its own thread. `None` when the thread could not start.
pub(crate) fn start<P: PredicateRunner>(
    id: u64,
    run: Run<P>,
    argv: Vec<String>,
    timeout_s: u64,
    cwd: PathBuf,
) -> Option<Ticket> {
    let (send, outcome) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let ticket = Ticket {
        id,
        argv: argv.clone(),
        cwd: cwd.clone(),
        timeout_s,
        outcome,
        cancel: Arc::clone(&cancel),
    };
    std::thread::Builder::new()
        .name(format!("dudes-baseline-{id}"))
        .spawn(move || {
            let end = End {
                handle: run.handle,
                id,
            };
            let outcome = match predicate::spec_for(&argv, &cwd, &run.env, run.path.as_deref()) {
                Err(_) if !cwd.is_dir() => Baseline::Refused(Refusal::CannotRun(format!(
                    "{} does not exist",
                    cwd.display()
                ))),
                Err(error) => Baseline::Refused(Refusal::CannotRun(error.to_string())),
                Ok((program, spec)) => {
                    match run
                        .runner
                        .run_cancellable(&spec, Duration::from_secs(timeout_s), &cancel)
                    {
                        Ok(finished) => command(&finished, program),
                        Err(error) => {
                            tracing::error!(%error, "a goal's check could not be run before its first turn");
                            Baseline::Unavailable
                        }
                    }
                }
            };
            tracing::info!(?outcome, "a goal's check ran once before its first turn");
            // The run's end is queued before its caller can answer, so a retry the answer
            // prompts never finds the run still counted.
            drop(end);
            let _ = send.send(outcome);
        })
        .ok()
        .map(|_| ticket)
}

/// Run `recheck` once the run admits the goal. `recheck` is the route's whole admission again and
/// its append, in one loop job.
pub(crate) fn admit_after_baseline<T>(
    ticket: &Ticket,
    stopping: &AtomicBool,
    hang_up: &HangUp,
    recheck: impl FnOnce(CheckBaseline) -> T,
) -> Result<T, Baseline> {
    match ticket.wait(stopping, hang_up) {
        Baseline::Admitted(baseline) => Ok(recheck(baseline)),
        other => Err(other),
    }
}

/// The `goal.set` answer for a check that refused the goal. `passed` says what showed the check
/// passing, such as "`test -f hello.txt` exited 0 in /repo". The message is what a terminal prints
/// after "Refused:", and `data` names the gate, so a client can add its own remedy.
pub(crate) fn goal_set_refusal(refusal: &Refusal, passed: &str, timeout_s: u64) -> RpcError {
    let (gate, message) = match refusal {
        Refusal::AlreadyPasses => (
            RefusalGate::CheckAlreadyPasses,
            format!(
                "the completion check already passes ({passed}), so it cannot show when this \
                 goal is done"
            ),
        ),
        Refusal::DidNotFinish => (
            RefusalGate::CheckDidNotFinish,
            format!(
                "the completion check did not finish (it timed out after {timeout_s} s, or a \
                 signal ended it), so it cannot show whether the check already passes"
            ),
        ),
        Refusal::CannotRun(why) => (
            RefusalGate::CheckCannotRun,
            format!("the completion check could not run, because {why}"),
        ),
    };
    RpcError {
        code: ErrorCode::Refused.code(),
        message,
        data: serde_json::to_value(RefusedData { gate }).ok(),
    }
}

impl Ticket {
    /// What showed this run's check passing.
    pub(crate) fn passed(&self) -> String {
        format!(
            "`{}` exited 0 in {}",
            self.argv.join(" "),
            self.cwd.display()
        )
    }
}

/// Reports a run's end to the loop when the run's thread ends, however it ends.
struct End {
    handle: Handle,
    id: u64,
}

impl Drop for End {
    fn drop(&mut self) {
        self.handle.baseline_ended(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(exit: Option<i32>, timed_out: bool) -> PredicateOutcome {
        PredicateOutcome::Finished {
            exit,
            output_tail: String::new(),
            timed_out,
        }
    }

    /// The outcome table: a pass refuses, a timeout or a signal refuses as unfinished, a non-zero
    /// exit is admitted with the program that ran, and a swept run decides nothing.
    #[test]
    fn a_command_run_maps_to_one_row_of_the_table() {
        let program = || "/usr/bin/test".to_owned();
        assert_eq!(
            command(&finished(Some(0), false), program()),
            Baseline::Refused(Refusal::AlreadyPasses)
        );
        assert_eq!(
            command(&finished(Some(1), false), program()),
            Baseline::Admitted(CheckBaseline::CommandFailed {
                program: program(),
                exit: 1
            })
        );
        assert_eq!(
            command(&finished(None, true), program()),
            Baseline::Refused(Refusal::DidNotFinish)
        );
        assert_eq!(
            command(&finished(Some(1), true), program()),
            Baseline::Refused(Refusal::DidNotFinish),
            "a run that hit its timeout did not finish, whatever its status"
        );
        assert_eq!(
            command(&finished(None, false), program()),
            Baseline::Refused(Refusal::DidNotFinish),
            "a signal ended it"
        );
        assert_eq!(
            command(&PredicateOutcome::Swept, program()),
            Baseline::Unavailable
        );
    }

    /// A browser route runs one check per agent and per session at a time, and 32 per session.
    #[test]
    fn browser_runs_are_limited_per_agent_and_per_session() {
        let agent = AgentId::from_parts(1, 1);
        let other = AgentId::from_parts(1, 2);
        let (one, two) = (Hex32::new([1; 32]), Hex32::new([2; 32]));
        let mut runs = Baselines::default();
        let first = runs.start_browser(Some(agent), one).unwrap();
        assert_eq!(runs.start_browser(Some(other), one), Err(Limit::Running));
        assert_eq!(runs.start_browser(Some(agent), two), Err(Limit::Running));
        let local = runs.start_local(agent);
        assert!(runs.local_running(agent));
        assert!(
            !runs.local_running(other),
            "a browser run is not a local one"
        );
        runs.end(local);
        assert!(!runs.local_running(agent));
        runs.end(first);
        runs.end(first);
        for _ in 1..SESSION_BASELINES {
            let id = runs.start_browser(None, one).unwrap();
            runs.end(id);
        }
        assert_eq!(runs.start_browser(None, one), Err(Limit::Exhausted));
        assert!(runs.start_browser(None, two).is_ok(), "another session");
    }
}
