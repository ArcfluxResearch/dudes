//! Whether each engine is signed in, asked of the engine's own status command.
//!
//! Only a `system.doctor` call starts a check, and one check per provider runs at a time, on a
//! thread of its own. The child goes through the daemon's one-shot runner, so it has its own
//! process group or job, is killed at its deadline and is swept when the daemon stops. Its
//! standard streams are the null device and the daemon reads only its exit status. The answer
//! carries no provenance and reaches no gate.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dudes_proto::rpc::system::{ProviderLoginHome, ProviderProbe, SignIn};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_types::Provider;

use crate::predicate::{PredicateOutcome, PredicateRunner};

/// How long each bound in a check lasts.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// When the status command is killed with its group.
    pub deadline: Duration,
    /// How long `system.doctor` waits for a running check.
    pub wait: Duration,
    /// How long a `signed_in` answer is reused.
    pub reuse: Duration,
}

impl Timing {
    /// The timings the daemon runs with: a 10 s kill deadline, a 3 s doctor wait and a 30 s reuse window.
    pub const DAEMON: Self = Self {
        deadline: Duration::from_secs(10),
        wait: Duration::from_secs(3),
        reuse: Duration::from_secs(30),
    };
}

/// One engine's answer, and what the row's `detail` says about it.
type Answer = (SignIn, Option<&'static str>);

/// One registered engine: its status command's arguments, when it has one that is safe to run,
/// and the environment a managed turn of it starts with (`Adapter::child_env`).
#[derive(Clone, Debug)]
pub struct Check {
    /// The engine.
    pub provider: Provider,
    /// The arguments after the program, or `None` when no status command is safe to run.
    pub args: Option<&'static [&'static str]>,
    /// The environment the status command starts with, less `DUDES_TOKEN` and `DUDES_SOCK`.
    pub env: Vec<EnvVar>,
}

/// The daemon's sign-in checks. Clones share one state.
#[derive(Clone)]
pub struct SignInCheck(Arc<Shared>);

impl std::fmt::Debug for SignInCheck {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignInCheck")
            .finish_non_exhaustive()
    }
}

struct Shared {
    runner: Arc<dyn PredicateRunner>,
    cwd: PathBuf,
    timing: Timing,
    /// Every registered provider.
    checks: BTreeMap<Provider, Check>,
    slots: Mutex<BTreeMap<Provider, Slot>>,
    finished: Condvar,
}

#[derive(Debug, Default)]
struct Slot {
    started: u64,
    finished: u64,
    answer: Option<Answer>,
    signed_in_at: Option<Instant>,
    signed_out_at: Option<Instant>,
}

impl SignInCheck {
    /// Checks for `checks`, run by `runner` in `cwd`, each without `DUDES_TOKEN` or `DUDES_SOCK`.
    #[must_use]
    pub fn new(
        runner: Arc<dyn PredicateRunner>,
        cwd: PathBuf,
        checks: impl IntoIterator<Item = Check>,
        timing: Timing,
    ) -> Self {
        let checks = checks
            .into_iter()
            .map(|mut check| {
                strip_agent_vars(&mut check.env);
                (check.provider, check)
            })
            .collect();
        Self(Arc::new(Shared {
            runner,
            cwd,
            timing,
            checks,
            slots: Mutex::default(),
            finished: Condvar::new(),
        }))
    }

    /// Set each row's `sign_in`, starting a check where one is due and waiting at most
    /// [`Timing::wait`] for the checks that run. A row whose program was not found keeps none.
    pub fn annotate(&self, rows: &mut [ProviderProbe], busy: &BTreeSet<Provider>) {
        self.annotate_rows(rows, busy, false);
    }

    /// [`Self::annotate`] for the Floor's engine rows, which are asked for on every page load: a
    /// signed-out answer is reused for [`Timing::reuse`] like a signed-in one.
    pub(crate) fn annotate_for_page(&self, rows: &mut [ProviderProbe], busy: &BTreeSet<Provider>) {
        self.annotate_rows(rows, busy, true);
    }

    /// Drop what is remembered about `provider`, because its sign-in may have changed.
    pub(crate) fn forget(&self, provider: Provider) {
        if let Some(slot) = self.0.lock().get_mut(&provider) {
            slot.signed_in_at = None;
            slot.signed_out_at = None;
        }
    }

    fn annotate_rows(
        &self,
        rows: &mut [ProviderProbe],
        busy: &BTreeSet<Provider>,
        cache_out: bool,
    ) {
        let shared = &self.0;
        let mut waiting = Vec::new();
        {
            let mut slots = shared.lock();
            for (index, row) in rows.iter_mut().enumerate() {
                if matches!(&row.login_home, Some(ProviderLoginHome::Unavailable { .. })) {
                    set(
                        row,
                        (
                            SignIn::Unknown,
                            Some("the managed Codex profile is unavailable"),
                        ),
                    );
                    continue;
                }
                let Some(check) = shared.checks.get(&row.provider) else {
                    continue;
                };
                let Some(args) = check.args else {
                    set(row, (SignIn::Unknown, Some(no_command(row.provider))));
                    continue;
                };
                let Some(program) = row.command.clone().filter(|_| row.ok) else {
                    continue;
                };
                if busy.contains(&row.provider) {
                    set(row, (SignIn::Unknown, Some("a turn is running")));
                    continue;
                }
                let slot = slots.entry(row.provider).or_default();
                if slot
                    .signed_in_at
                    .is_some_and(|at| at.elapsed() < shared.timing.reuse)
                {
                    set(row, (SignIn::SignedIn, None));
                    continue;
                }
                if cache_out
                    && slot
                        .signed_out_at
                        .is_some_and(|at| at.elapsed() < shared.timing.reuse)
                {
                    set(row, (SignIn::SignedOut, None));
                    continue;
                }
                if slot.started == slot.finished {
                    slot.started += 1;
                    let mut spec = LaunchSpec {
                        argv: std::iter::once(program.into_os_string())
                            .chain(args.iter().map(OsString::from))
                            .collect(),
                        env: check.env.clone(),
                        cwd: shared.cwd.clone(),
                        stdin: dudes_providers::launch::LaunchInput::Null,
                    };
                    if let Some(ProviderLoginHome::Ready { path }) = &row.login_home {
                        let Ok(plan) = dudes_providers::codex::login::managed_login_status(path)
                        else {
                            finish(
                                slot,
                                (
                                    SignIn::Unknown,
                                    Some("the managed Codex profile is unavailable"),
                                ),
                            );
                            continue;
                        };
                        spec.argv.truncate(1);
                        spec.argv.extend(plan.args);
                        spec.cwd = plan.cwd;
                        spec.env.retain(|var| var.name() != "CODEX_HOME");
                        spec.env.push(EnvVar::new("CODEX_HOME", path.as_os_str()));
                    }
                    shared.start(row.provider, spec, slot);
                }
                waiting.push((index, row.provider, slot.started));
            }
        }
        let deadline = Instant::now() + shared.timing.wait;
        let mut slots = shared.lock();
        for (index, provider, check) in waiting {
            loop {
                let slot = &slots[&provider];
                if slot.finished >= check {
                    let answer = slot.answer.unwrap_or((SignIn::Unknown, None));
                    set(&mut rows[index], answer);
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    set(&mut rows[index], (SignIn::Unknown, Some("still checking")));
                    break;
                }
                slots = shared
                    .finished
                    .wait_timeout(slots, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
    }
}

/// Remove the variables that would hand a child the daemon's agent identity.
pub(crate) fn strip_agent_vars(env: &mut Vec<EnvVar>) {
    env.retain(|var| {
        !var.name().to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case("DUDES_TOKEN") || name.eq_ignore_ascii_case("DUDES_SOCK")
        })
    });
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Provider, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run check `slot.started` on a thread of its own. A thread that cannot start is a launch
    /// failure, answered at once.
    fn start(self: &Arc<Self>, provider: Provider, spec: LaunchSpec, slot: &mut Slot) {
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("dudes-sign-in-{provider}"))
            .spawn(move || {
                let answer = answer(
                    &shared
                        .runner
                        .run_discarding_output(&spec, shared.timing.deadline),
                );
                let mut slots = shared.lock();
                if let Some(slot) = slots.get_mut(&provider) {
                    finish(slot, answer);
                }
                drop(slots);
                shared.finished.notify_all();
            });
        if spawned.is_err() {
            finish(slot, (SignIn::Unknown, Some(COULD_NOT_START)));
        }
    }
}

const COULD_NOT_START: &str = "the status command could not start";

fn finish(slot: &mut Slot, answer: Answer) {
    slot.finished = slot.started;
    slot.answer = Some(answer);
    slot.signed_in_at = (answer.0 == SignIn::SignedIn).then(Instant::now);
    slot.signed_out_at = (answer.0 == SignIn::SignedOut).then(Instant::now);
}

/// 0 is signed in, 1 is signed out, and every other outcome is unknown.
fn answer(outcome: &std::io::Result<PredicateOutcome>) -> Answer {
    match outcome {
        Ok(PredicateOutcome::Finished {
            timed_out: true, ..
        }) => (
            SignIn::Unknown,
            Some("the status command did not finish in time"),
        ),
        Ok(PredicateOutcome::Finished { exit: Some(0), .. }) => (SignIn::SignedIn, None),
        Ok(PredicateOutcome::Finished { exit: Some(1), .. }) => (SignIn::SignedOut, None),
        Ok(PredicateOutcome::Finished { exit: Some(_), .. }) => (
            SignIn::Unknown,
            Some("the status command exited with a status other than 0 or 1"),
        ),
        Ok(PredicateOutcome::Finished { exit: None, .. }) => (
            SignIn::Unknown,
            Some("the status command ended on a signal"),
        ),
        Ok(PredicateOutcome::Swept) => (SignIn::Unknown, Some("the daemon is stopping")),
        Err(_) => (SignIn::Unknown, Some(COULD_NOT_START)),
    }
}

fn set(row: &mut ProviderProbe, (sign_in, detail): Answer) {
    row.sign_in = Some(sign_in);
    if let Some(detail) = detail {
        row.detail = Some(match row.detail.take() {
            Some(earlier) => format!("{earlier}; {detail}"),
            None => detail.to_owned(),
        });
    }
}

fn no_command(provider: Provider) -> &'static str {
    match provider {
        Provider::Pi => "Pi has no status command that is safe to run",
        _ => "this engine has no status command that is safe to run",
    }
}
