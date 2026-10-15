//! A Codex agent is hired through the real daemon and its turns run the real
//! `CodexAdapter` against a stand-in program with no account behind it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::output::{OutputRegistry, OutputState};
use duded::ports::{Answered, Ask};
use duded::predicate::PredicateOutcome;
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::adapter::Adapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::codex::config::CodexConfig;
use dudes_providers::launch::EnvVar;
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{AgentId, Provider, SessionId, Tokens, Usage};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

const LOG: &str = "DUDES_CODEX_STAND_IN_LOG";
const THREAD: &str = "0190f6d2-6b1a-7c3e-9f00-000000000001";
const SANDBOX: &str = "sandbox_mode=\"workspace-write\"";

struct HostStreams(Arc<ChildEnds>);

impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostChildTurn(self.0.watch(key)))
    }
}

struct HostChildTurn(ChildTurn);

impl ChildStream for HostChildTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
    }

    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.ended_with_elapsed(
            ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

fn stand_in() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/codex-stand-in")
}

struct Run {
    records: Vec<Record>,
    output: Arc<OutputRegistry>,
    agent: AgentId,
    runs: Vec<Vec<String>>,
    home: PathBuf,
    outbox: PathBuf,
}

fn codex_run(predicates: impl IntoIterator<Item = PredicateOutcome>, turns: usize) -> Run {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("codex-home");
    std::fs::create_dir(&home).expect("the codex home");
    let log = fixture.state.path().join("stand-in.log");
    let mut daemon_env: Vec<EnvVar> = ["PATH", "HOME", "TMPDIR"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(*name, value)))
        .collect();
    daemon_env.extend([
        EnvVar::new("CODEX_HOME", home.as_os_str()),
        EnvVar::new(LOG, log.as_os_str()),
    ]);
    let settings = duded::settings::Settings {
        daemon_env,
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let output = children.output();
    let adapter = CodexAdapter::new(CodexConfig {
        streaming: false,
        command: stand_in().to_str().expect("a UTF-8 path").to_owned(),
    });
    let seams = Seams {
        adapters: [Box::new(adapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: Arc::clone(&children),
        ..common::seams(
            &settings,
            RealHost::watching(Arc::new(HostStreams(children))),
            Arc::new(ScriptedRunner::scripted(predicates)),
            Arc::new(TestClock::new()),
            Witness::default(),
        )
    };
    let mut running = common::start(boot(&settings, seams).expect("boot"));
    let agent = hire_codex(&running.handle, fixture.state.path()).expect("a Codex hire");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("the last TurnEnded", |record| {
        (fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { .. }))
            .count()
            >= turns
            && matches!(record, Record::TurnEnded { .. }))
        .then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let runs = std::fs::read_to_string(&log)
        .expect("the stand-in log")
        .split("run\n")
        .filter(|run| !run.is_empty())
        .map(|run| run.lines().map(ToOwned::to_owned).collect())
        .collect();
    Run {
        records: fixture.records(),
        output,
        agent,
        runs,
        home,
        outbox: fixture
            .state
            .path()
            .join("agents")
            .join(agent.to_string())
            .join("outbox")
            .canonicalize()
            .expect("the own outbox"),
    }
}

fn hire_codex(handle: &duded::Handle, cwd: &Path) -> Result<AgentId, String> {
    match handle.ask(Ask::Hire(Box::new(HireParams {
        provider: Provider::Codex,
        ..common::hire_params(cwd)
    }))) {
        Ok(Answered::Hired(hired)) => Ok(hired.agent),
        Ok(other) => panic!("agent.hire answered {other:?}"),
        Err(error) => Err(error.message),
    }
}

fn await_output(output: &OutputRegistry, agent: AgentId) -> OutputState {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut snapshot = output.snapshot(agent);
    loop {
        if matches!(snapshot.state, OutputState::Ready { ended: true, .. }) {
            return snapshot.state;
        }
        assert!(
            Instant::now() < deadline,
            "the Floor output never ended: {:?}",
            snapshot.state
        );
        snapshot = output.wait_changed(agent, snapshot.revision, Duration::from_millis(50));
    }
}

fn text(state: &OutputState) -> String {
    match state {
        OutputState::Ready { chunks, .. } => chunks.iter().map(|c| c.text.as_str()).collect(),
        _ => String::new(),
    }
}

fn issued(records: &[Record]) -> Vec<Option<SessionId>> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued { session, .. } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

fn learned(records: &[Record]) -> Vec<SessionId> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::SessionLearned { session, .. } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

fn outcomes(records: &[Record]) -> Vec<Option<TurnOutcome>> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::TurnEnded { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_codex_hire_is_refused_without_its_adapter() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = common::start(
        boot(
            &fixture.settings(),
            common::seams(
                &settings,
                common::SharedHost::new(),
                Arc::new(ScriptedRunner::scripted([])),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("boot"),
    );
    let refused = hire_codex(&running.handle, fixture.state.path())
        .expect_err("a daemon with no Codex adapter refuses a Codex hire");
    assert!(refused.contains("codex"), "{refused}");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn a_codex_turn_announces_no_session_and_reaches_the_floor() {
    let run = codex_run([common::passes()], 1);
    assert_eq!(issued(&run.records), [None]);
    assert_eq!(learned(&run.records), [SessionId::new(THREAD)]);
    assert_eq!(outcomes(&run.records), [Some(TurnOutcome::Completed)]);
    assert_eq!(
        text(&await_output(&run.output, run.agent)),
        "stand-in reply"
    );
}

#[test]
fn a_second_codex_turn_resumes_its_thread_in_the_same_home_and_sandbox() {
    let run = codex_run([common::fails(), common::passes()], 2);
    let home = format!("CODEX_HOME={}", run.home.display());
    let [first, second] = run.runs.as_slice() else {
        panic!("two stand-in runs, got {:?}", run.runs);
    };
    let mut prefix = vec![home.as_str(), "exec"];
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    prefix.extend([
        "--add-dir",
        run.outbox.to_str().expect("a UTF-8 fixture path"),
    ]);
    let mut fresh = prefix.clone();
    fresh.extend(["--json", "--ignore-user-config"]);
    let mut resumed = prefix;
    resumed.extend(["resume", THREAD, "--json", "--ignore-user-config"]);
    assert_eq!(first[..fresh.len()], fresh);
    assert_eq!(second[..resumed.len()], resumed);
    for run in [first, second] {
        assert!(
            run.windows(2).any(|pair| pair == ["-c", SANDBOX]),
            "every turn runs in the workspace-write sandbox: {run:?}"
        );
    }
    assert_eq!(
        issued(&run.records),
        [None, Some(SessionId::new(THREAD))],
        "the second turn resumes the thread the first turn's vendor named"
    );
    assert_eq!(
        outcomes(&run.records),
        [Some(TurnOutcome::Completed), Some(TurnOutcome::Completed)]
    );
    let charges: Vec<_> = run
        .records
        .iter()
        .filter_map(|record| match record {
            Record::TurnEnded {
                usage,
                session_usage,
                ..
            } => Some((*usage, *session_usage)),
            _ => None,
        })
        .collect();
    let first = Usage {
        input: Tokens::new(10),
        output: Tokens::new(2),
        ..Usage::ZERO
    };
    let second = Usage {
        input: Tokens::new(15),
        output: Tokens::new(3),
        ..Usage::ZERO
    };
    let total = Usage {
        input: Tokens::new(25),
        output: Tokens::new(5),
        ..Usage::ZERO
    };
    assert_eq!(
        charges,
        [(Some(first), Some(first)), (Some(second), Some(total))]
    );
}
