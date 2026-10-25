//! The models each engine offers, held in memory for the Floor's engine report.
//!
//! Nothing here is journaled and a restart empties it: a list is a copy of something outside
//! Dudes, not state a replay must rebuild.
//!
//! - **Pi** is asked with `pi --list-models`, under the environment a Pi turn starts with. Every Pi
//!   agent starts from the daemon's environment, so they share one profile and one list.
//! - **Codex** is asked with `model/list` on an app-server process of its own, with no thread.
//! - **Claude** has no list command. Its list is the aliases it accepts plus the model ids its
//!   turns have reported.
//!
//! A listing child goes through the daemon's one-shot runner, so it has its own process group, is
//! killed at its deadline or when it has written more than the output cap, and is swept when the
//! daemon stops. One runs at a time per engine, and a second request for an engine whose child is
//! running waits for that child's answer instead of starting another.
//!
//! A list is what the engine's own program says, not what the account behind it is allowed to run,
//! so every list is reported as unchecked. A name typed by hand is never refused for being absent.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{ModelName, Provider};
use serde::Serialize;

use crate::predicate::{PredicateCaptured, PredicateRunner};

/// The most items a list holds.
pub const MAX_ITEMS: usize = 200;
/// The most model ids a Claude list keeps from turns it has seen.
pub const MAX_SEEN: usize = 50;
/// When a listing child is killed.
pub const DEADLINE: Duration = Duration::from_secs(5);
/// How much of a listing child's output is read before it is killed.
pub const OUTPUT_CAP: usize = 64 * 1024;

const CLAUDE_ALIASES: [&str; 3] = ["opus", "sonnet", "haiku"];

/// How a listed model came to be listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// The engine's own program listed it.
    Listed,
    /// A short name the engine accepts for a family of models.
    Alias,
    /// A turn reported running it.
    Seen,
}

/// One entry of a list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelItem {
    /// The name to give the engine.
    pub id: String,
    /// How it came to be listed.
    pub kind: ItemKind,
}

/// What one engine offered, and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelList {
    /// Milliseconds since the Unix epoch when the list was made.
    pub as_of: i64,
    /// Always false: no list is checked against the account behind the engine.
    pub checked: bool,
    /// At most [`MAX_ITEMS`] entries.
    pub items: Vec<ModelItem>,
}

/// Why a refresh produced no list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The engine is not configured, or its program was not found.
    EngineAbsent,
    /// The child was killed at its deadline.
    TimedOut,
    /// The child ran and printed nothing that reads as a list.
    Unreadable,
    /// The daemon is stopping.
    Stopping,
    /// The child could not be started, or exited without a list.
    Failed,
}

impl Refusal {
    /// The word the browser reads.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::EngineAbsent => "engine_absent",
            Self::TimedOut => "timed_out",
            Self::Unreadable => "unreadable",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
        }
    }
}

/// One engine the daemon can list: its program and the environment a turn of it starts with.
#[derive(Clone, Debug)]
pub struct Source {
    /// The engine.
    pub provider: Provider,
    /// The program, as configured.
    pub program: OsString,
    /// The environment the listing child starts with.
    pub env: Vec<EnvVar>,
}

/// The daemon's model lists. Clones share one state.
#[derive(Clone)]
pub struct EngineModels(Arc<Shared>);

impl std::fmt::Debug for EngineModels {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineModels")
            .finish_non_exhaustive()
    }
}

struct Shared {
    runner: Arc<dyn PredicateRunner>,
    cwd: PathBuf,
    sources: BTreeMap<Provider, Source>,
    codex_profile: Option<Arc<crate::codex_profile::CodexProfile>>,
    deadline: Duration,
    cap: usize,
    state: Mutex<State>,
    finished: Condvar,
}

#[derive(Default)]
struct State {
    lists: BTreeMap<Provider, ModelList>,
    /// The generation of the listing child running for each engine.
    running: BTreeMap<Provider, u64>,
    /// How each engine's newest finished listing ended, by generation.
    last: BTreeMap<Provider, (u64, Result<ModelList, Refusal>)>,
    generation: u64,
    /// Model ids Claude turns reported, oldest first.
    seen: Vec<String>,
    seen_at: i64,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

impl EngineModels {
    /// Lists for `sources`, with listing children run by `runner` in `cwd`.
    #[must_use]
    pub(crate) fn new(
        runner: Arc<dyn PredicateRunner>,
        cwd: PathBuf,
        sources: impl IntoIterator<Item = Source>,
        codex_profile: Option<Arc<crate::codex_profile::CodexProfile>>,
    ) -> Self {
        Self::build(runner, cwd, sources, codex_profile, DEADLINE, OUTPUT_CAP)
    }

    /// Lists with no managed Codex profile and the deadline and output cap a listing child is
    /// held to. Release-unreachable: only a test uses other limits.
    #[doc(hidden)]
    #[must_use]
    pub fn with_limits(
        runner: Arc<dyn PredicateRunner>,
        cwd: PathBuf,
        sources: impl IntoIterator<Item = Source>,
        deadline: Duration,
        cap: usize,
    ) -> Self {
        Self::build(runner, cwd, sources, None, deadline, cap)
    }

    fn build(
        runner: Arc<dyn PredicateRunner>,
        cwd: PathBuf,
        sources: impl IntoIterator<Item = Source>,
        codex_profile: Option<Arc<crate::codex_profile::CodexProfile>>,
        deadline: Duration,
        cap: usize,
    ) -> Self {
        let sources = sources
            .into_iter()
            .map(|mut source| {
                crate::sign_in::strip_agent_vars(&mut source.env);
                (source.provider, source)
            })
            .collect();
        Self(Arc::new(Shared {
            runner,
            cwd,
            sources,
            codex_profile,
            deadline,
            cap,
            state: Mutex::new(State {
                seen_at: now_millis(),
                ..State::default()
            }),
            finished: Condvar::new(),
        }))
    }

    /// Remember model ids a Claude turn reported, newest last.
    pub fn note_seen<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let mut state = self.0.lock();
        let mut changed = false;
        for name in names {
            if ModelName::parse(name).is_err() {
                continue;
            }
            state.seen.retain(|seen| seen != name);
            state.seen.push(name.to_owned());
            changed = true;
        }
        if changed {
            let excess = state.seen.len().saturating_sub(MAX_SEEN);
            state.seen.drain(..excess);
            state.seen_at = now_millis();
        }
    }

    /// The list held for `provider`, or `None` when it has none.
    #[must_use]
    pub fn list(&self, provider: Provider) -> Option<ModelList> {
        let state = self.0.lock();
        if provider == Provider::Claude {
            return self
                .0
                .sources
                .contains_key(&Provider::Claude)
                .then(|| claude_list(&state));
        }
        state.lists.get(&provider).cloned()
    }

    /// Ask `provider` for its models again, waiting for the answer. A Claude list needs no child
    /// and is made at once. When a child is already running for the engine, this waits for that
    /// child's answer and starts none of its own.
    ///
    /// # Errors
    /// Why no list was made.
    pub fn refresh(&self, provider: Provider) -> Result<ModelList, Refusal> {
        let shared = &self.0;
        let Some(source) = shared.sources.get(&provider) else {
            return Err(Refusal::EngineAbsent);
        };
        if provider == Provider::Claude {
            return Ok(claude_list(&shared.lock()));
        }
        let mut state = shared.lock();
        if let Some(&running) = state.running.get(&provider) {
            let give_up = std::time::Instant::now() + shared.deadline + Duration::from_secs(2);
            loop {
                if let Some((generation, result)) = state.last.get(&provider)
                    && *generation >= running
                {
                    return result.clone();
                }
                let left = give_up.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(Refusal::TimedOut);
                }
                state = shared
                    .finished
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        state.generation += 1;
        let generation = state.generation;
        state.running.insert(provider, generation);
        drop(state);
        let mut guard = Running {
            shared,
            provider,
            generation,
            result: Err(Refusal::Failed),
        };
        guard.result = shared.list_with(provider, source);
        guard.result.clone()
    }
}

/// Settles a listing on every way out, including a panic, so a waiting request is never left behind.
struct Running<'a> {
    shared: &'a Shared,
    provider: Provider,
    generation: u64,
    result: Result<ModelList, Refusal>,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.running.remove(&self.provider);
        if let Ok(list) = &self.result {
            state.lists.insert(self.provider, list.clone());
        }
        state
            .last
            .insert(self.provider, (self.generation, self.result.clone()));
        drop(state);
        self.shared.finished.notify_all();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn list_with(&self, provider: Provider, source: &Source) -> Result<ModelList, Refusal> {
        let path = crate::program::child_path(&source.env);
        let program =
            crate::program::resolve(&source.program, path).map_err(|_| Refusal::EngineAbsent)?;
        let mut env = source.env.clone();
        let (argv, stdin) = if provider == Provider::Pi {
            let mut argv = vec![program.into_os_string()];
            argv.extend(
                dudes_providers::pi::models::LIST_ARGS
                    .iter()
                    .map(OsString::from),
            );
            (argv, LaunchInput::Null)
        } else {
            if let Some(profile) = &self.codex_profile
                && let dudes_proto::rpc::system::ProviderLoginHome::Ready { path } =
                    profile.login_home(Some(&program))
            {
                env.retain(|var| var.name() != "CODEX_HOME");
                env.push(EnvVar::new("CODEX_HOME", path.as_os_str()));
            }
            (
                dudes_providers::codex::model_list::argv(program.into_os_string()),
                LaunchInput::WriteThenClose(dudes_providers::codex::model_list::input()),
            )
        };
        let spec = LaunchSpec {
            argv,
            env,
            cwd: self.cwd.clone(),
            stdin,
        };
        let (stdout, truncated) = match self.runner.run_captured(&spec, self.deadline, self.cap) {
            Ok(PredicateCaptured::Finished {
                timed_out: true, ..
            }) => return Err(Refusal::TimedOut),
            Ok(PredicateCaptured::Finished {
                stdout, truncated, ..
            }) => (stdout, truncated),
            Ok(PredicateCaptured::Swept) => return Err(Refusal::Stopping),
            Err(_) => return Err(Refusal::Failed),
        };
        let stdout = if truncated {
            let whole = stdout
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |end| end + 1);
            &stdout[..whole]
        } else {
            &stdout[..]
        };
        let ids = match provider {
            Provider::Pi => dudes_providers::pi::models::parse_list(stdout),
            _ => dudes_providers::codex::model_list::parse(stdout),
        };
        let mut distinct = BTreeSet::new();
        let items: Vec<ModelItem> = ids
            .into_iter()
            .filter(|id| ModelName::parse(id).is_ok() && distinct.insert(id.clone()))
            .take(MAX_ITEMS)
            .map(|id| ModelItem {
                id,
                kind: ItemKind::Listed,
            })
            .collect();
        if items.is_empty() {
            return Err(Refusal::Unreadable);
        }
        Ok(ModelList {
            as_of: now_millis(),
            checked: false,
            items,
        })
    }
}

fn claude_list(state: &State) -> ModelList {
    let aliases = CLAUDE_ALIASES.iter().map(|alias| ModelItem {
        id: (*alias).to_owned(),
        kind: ItemKind::Alias,
    });
    let seen = state
        .seen
        .iter()
        .rev()
        .filter(|id| !CLAUDE_ALIASES.contains(&id.as_str()))
        .map(|id| ModelItem {
            id: id.clone(),
            kind: ItemKind::Seen,
        });
    ModelList {
        as_of: state.seen_at,
        checked: false,
        items: aliases.chain(seen).take(MAX_ITEMS).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::predicate::PredicateOutcome;

    struct Scripted {
        outcome: Mutex<Option<std::io::Result<PredicateCaptured>>>,
        runs: AtomicUsize,
        specs: Mutex<Vec<LaunchSpec>>,
        gate: Option<(Mutex<bool>, Condvar)>,
    }

    impl Scripted {
        fn new(outcome: std::io::Result<PredicateCaptured>) -> Self {
            Self {
                outcome: Mutex::new(Some(outcome)),
                runs: AtomicUsize::new(0),
                specs: Mutex::default(),
                gate: None,
            }
        }
    }

    impl PredicateRunner for Scripted {
        fn run(&self, _: &LaunchSpec, _: Duration) -> std::io::Result<PredicateOutcome> {
            Err(std::io::ErrorKind::Unsupported.into())
        }

        fn run_captured(
            &self,
            spec: &LaunchSpec,
            _: Duration,
            _: usize,
        ) -> std::io::Result<PredicateCaptured> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.specs.lock().unwrap().push(spec.clone());
            if let Some((open, changed)) = &self.gate {
                let mut open = open.lock().unwrap();
                while !*open {
                    open = changed.wait(open).unwrap();
                }
            }
            self.outcome
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(std::io::ErrorKind::Other.into()))
        }

        fn sweep(&self) {}
    }

    fn finished(stdout: &str) -> PredicateCaptured {
        PredicateCaptured::Finished {
            exit: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            truncated: false,
            timed_out: false,
        }
    }

    fn models(runner: &Arc<Scripted>, provider: Provider) -> EngineModels {
        let program = std::env::current_exe().expect("a running test binary");
        EngineModels::new(
            runner.clone(),
            std::env::temp_dir(),
            [
                Source {
                    provider,
                    program: program.into_os_string(),
                    env: vec![
                        EnvVar::new("PATH", "/usr/bin"),
                        EnvVar::new("DUDES_TOKEN", "secret"),
                        EnvVar::new("PI_CODING_AGENT_DIR", "/profile"),
                    ],
                },
                Source {
                    provider: Provider::Claude,
                    program: "claude".into(),
                    env: Vec::new(),
                },
            ],
            None,
        )
    }

    fn ids(list: &ModelList) -> Vec<(&str, ItemKind)> {
        list.items
            .iter()
            .map(|item| (item.id.as_str(), item.kind))
            .collect()
    }

    #[test]
    fn a_pi_table_becomes_unchecked_provider_qualified_items_under_the_agents_environment() {
        let table = "provider  model  context  max-out  thinking  images\n\
openai  gpt-5.1  400K  128K  yes  yes\n\
openai-codex  gpt-5.1  400K  128K  yes  yes\n\
openai  gpt-5.1  400K  128K  yes  yes\n";
        let runner = Arc::new(Scripted::new(Ok(finished(table))));
        let lists = models(&runner, Provider::Pi);
        assert_eq!(lists.list(Provider::Pi), None);
        let list = lists.refresh(Provider::Pi).expect("a list");
        assert_eq!(
            ids(&list),
            [
                ("openai/gpt-5.1", ItemKind::Listed),
                ("openai-codex/gpt-5.1", ItemKind::Listed)
            ]
        );
        assert!(!list.checked);
        assert_eq!(lists.list(Provider::Pi), Some(list));
        let spec = runner.specs.lock().unwrap()[0].clone();
        assert_eq!(spec.argv[1], "--list-models");
        assert!(matches!(spec.stdin, LaunchInput::Null));
        let names: Vec<_> = spec.env.iter().map(|var| var.name().to_owned()).collect();
        assert_eq!(names, ["PATH", "PI_CODING_AGENT_DIR"]);
    }

    #[test]
    fn a_codex_answer_becomes_items_from_one_write_then_close() {
        let capture = include_str!(
            "../../../crates/dudes-providers/tests/fixtures/codex/app-server-model-list.jsonl"
        );
        let runner = Arc::new(Scripted::new(Ok(finished(capture))));
        let lists = models(&runner, Provider::Codex);
        let list = lists.refresh(Provider::Codex).expect("a list");
        assert_eq!(list.items.len(), 7);
        assert_eq!(list.items[0].id, "gpt-6.1-sol");
        let spec = runner.specs.lock().unwrap()[0].clone();
        assert_eq!(spec.argv[1..4], ["app-server", "--listen", "stdio://"]);
        assert!(
            matches!(spec.stdin, LaunchInput::WriteThenClose(bytes) if bytes.ends_with(b"\"params\":{}}\n"))
        );
    }

    #[test]
    fn a_list_holds_at_most_two_hundred_valid_distinct_ids() {
        let mut table = String::from("provider model context\n");
        for number in 0..300 {
            let _ = writeln!(table, "p m{number} 1K");
        }
        table.push_str("p bad$name 1K\n");
        let runner = Arc::new(Scripted::new(Ok(finished(&table))));
        let list = models(&runner, Provider::Pi)
            .refresh(Provider::Pi)
            .expect("a list");
        assert_eq!(list.items.len(), MAX_ITEMS);
        assert_eq!(list.items[199].id, "p/m199");
    }

    #[test]
    fn each_way_a_listing_fails_is_named() {
        let cases = [
            (
                Ok(PredicateCaptured::Finished {
                    exit: None,
                    stdout: Vec::new(),
                    truncated: false,
                    timed_out: true,
                }),
                Refusal::TimedOut,
            ),
            (Ok(PredicateCaptured::Swept), Refusal::Stopping),
            (Err(std::io::ErrorKind::Other.into()), Refusal::Failed),
            (Ok(finished("No models available.\n")), Refusal::Unreadable),
            (Ok(finished("")), Refusal::Unreadable),
        ];
        for (outcome, refusal) in cases {
            let runner = Arc::new(Scripted::new(outcome));
            assert_eq!(
                models(&runner, Provider::Pi).refresh(Provider::Pi),
                Err(refusal)
            );
        }
        let runner = Arc::new(Scripted::new(Ok(finished(""))));
        let lists = models(&runner, Provider::Pi);
        assert_eq!(lists.refresh(Provider::Codex), Err(Refusal::EngineAbsent));
        assert_eq!(runner.runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn output_cut_at_the_cap_drops_its_partial_last_row() {
        let runner = Arc::new(Scripted::new(Ok(PredicateCaptured::Finished {
            exit: None,
            stdout: b"provider model context\np m1 1K\np m2 1K\np m3".to_vec(),
            truncated: true,
            timed_out: false,
        })));
        let list = models(&runner, Provider::Pi)
            .refresh(Provider::Pi)
            .expect("a list");
        assert_eq!(
            ids(&list),
            [("p/m1", ItemKind::Listed), ("p/m2", ItemKind::Listed)]
        );
    }

    #[test]
    fn claude_lists_its_aliases_then_the_ids_seen_newest_first_and_needs_no_child() {
        let runner = Arc::new(Scripted::new(Ok(finished(""))));
        let lists = models(&runner, Provider::Pi);
        assert_eq!(
            ids(&lists.list(Provider::Claude).expect("a list")),
            [
                ("opus", ItemKind::Alias),
                ("sonnet", ItemKind::Alias),
                ("haiku", ItemKind::Alias)
            ]
        );
        lists.note_seen([
            "claude-opus-4-1",
            "claude-haiku-4-5",
            "sonnet",
            "bad name",
            "claude-opus-4-1",
        ]);
        let list = lists.refresh(Provider::Claude).expect("a list");
        assert_eq!(
            ids(&list),
            [
                ("opus", ItemKind::Alias),
                ("sonnet", ItemKind::Alias),
                ("haiku", ItemKind::Alias),
                ("claude-opus-4-1", ItemKind::Seen),
                ("claude-haiku-4-5", ItemKind::Seen),
            ]
        );
        let many: Vec<String> = (0..80).map(|number| format!("m{number}")).collect();
        lists.note_seen(many.iter().map(String::as_str));
        let list = lists.list(Provider::Claude).expect("a list");
        assert_eq!(list.items.len(), 3 + MAX_SEEN);
        assert_eq!(list.items[3].id, "m79");
        assert_eq!(runner.runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_second_refresh_waits_for_the_running_child_and_shares_its_answer() {
        let mut scripted = Scripted::new(Ok(finished("provider model context\np m1 1K\n")));
        scripted.gate = Some((Mutex::new(false), Condvar::new()));
        let runner = Arc::new(scripted);
        let lists = models(&runner, Provider::Pi);
        let first = {
            let lists = lists.clone();
            std::thread::spawn(move || lists.refresh(Provider::Pi))
        };
        while runner.runs.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let second = {
            let lists = lists.clone();
            std::thread::spawn(move || lists.refresh(Provider::Pi))
        };
        std::thread::sleep(Duration::from_millis(100));
        let (open, changed) = runner.gate.as_ref().expect("a gate");
        *open.lock().unwrap() = true;
        changed.notify_all();
        let (first, second) = (first.join().unwrap(), second.join().unwrap());
        assert_eq!(first, second);
        assert_eq!(ids(&first.expect("a list")), [("p/m1", ItemKind::Listed)]);
        assert_eq!(runner.runs.load(Ordering::SeqCst), 1);
    }
}
