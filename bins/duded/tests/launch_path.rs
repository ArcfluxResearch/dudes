//! Invariant 7 says PATH lookup happens in the daemon. These runs put a planted `claude` in the
//! agent's working directory and a relative or empty entry on the launch's `PATH`, then run one
//! managed turn through the real host. The child's `execvp` would search that `PATH` after it
//! changed into the working directory, so the planted file must never be what runs.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{ModelSelection, Provider, Tier};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

/// A managed launch of `.0`, with the daemon's environment as the child's. A provider's default
/// configuration names the bare word `claude`, and the `stub-provider` build an absolute program.
#[derive(Debug)]
struct NamedAdapter(OsString);

impl Adapter for NamedAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &std::ffi::OsStr {
        &self.0
    }

    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        _session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        Ok(LaunchSpec {
            argv: vec![self.0.clone()],
            env: ctx.daemon_env.to_vec(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// An executable `claude` in `dir` that appends `who` to `mark` when it runs.
fn install_claude(dir: &Path, mark: &Path, who: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(dir).expect("the directory");
    let program = dir.join("claude");
    std::fs::write(
        &program,
        format!("#!/bin/sh\necho {who} >> '{}'\n", mark.display()),
    )
    .expect("write the program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("make it executable");
}

/// `duded::ChildEnds` as the host's stream sink, as `main.rs` bridges them.
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

struct Run {
    ran: String,
    outcome: TurnOutcome,
}

/// One managed turn of the bare name `claude` in `workspace` with `path` as the launch's `PATH`.
fn one_turn(fixture: &Fixture, workspace: &Path, mark: &Path, path: &str) -> Run {
    turn_of(fixture, OsString::from("claude"), workspace, mark, path)
}

/// One managed turn of `program` in `workspace` with `path` as the launch's whole `PATH`.
fn turn_of(fixture: &Fixture, program: OsString, workspace: &Path, mark: &Path, path: &str) -> Run {
    let settings = duded::settings::Settings {
        daemon_env: vec![EnvVar::new("PATH", path)],
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(NamedAdapter(program)) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&settings, seams).expect("boot"));
    let agent = common::hire(&running.handle, workspace);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let outcome = fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded { outcome, .. } => outcome.clone(),
        _ => None,
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    Run {
        ran: std::fs::read_to_string(mark).unwrap_or_default(),
        outcome,
    }
}

struct Scene {
    fixture: Fixture,
    workspace: PathBuf,
    bin: PathBuf,
    mark: PathBuf,
}

/// A workspace holding a planted `claude`, and a separate absolute directory for the real one.
fn scene() -> Scene {
    let fixture = Fixture::new();
    let root = fixture.state.path().to_path_buf();
    let workspace = root.join("workspace");
    let bin = root.join("bin");
    let mark = root.join("ran");
    install_claude(&workspace, &mark, "planted");
    Scene {
        fixture,
        workspace,
        bin,
        mark,
    }
}

#[test]
fn a_relative_path_entry_does_not_run_the_workspaces_planted_program() {
    let scene = scene();
    install_claude(&scene.bin, &scene.mark, "real");
    let path = format!(".:{}", scene.bin.display());
    let run = one_turn(&scene.fixture, &scene.workspace, &scene.mark, &path);
    assert_eq!(run.ran, "real\n", "the program on the absolute entry ran");
}

#[test]
fn an_empty_path_entry_refuses_the_launch_rather_than_run_the_planted_program() {
    let scene = scene();
    let run = one_turn(
        &scene.fixture,
        &scene.workspace,
        &scene.mark,
        "/nonexistent-launch-path:",
    );
    assert_eq!(run.ran, "", "nothing ran");
    assert!(
        matches!(&run.outcome, TurnOutcome::NotLaunched { reason } if reason == "managed process could not launch (NotFound)"),
        "the missing launch is reported without execution inputs: {:?}",
        run.outcome
    );
}

#[test]
fn control_an_absolute_path_launches_the_program_on_it() {
    let scene = scene();
    install_claude(&scene.bin, &scene.mark, "real");
    let path = scene.bin.display().to_string();
    let run = one_turn(&scene.fixture, &scene.workspace, &scene.mark, &path);
    assert_eq!(run.ran, "real\n");
}

#[test]
fn control_an_absolute_program_launches_whatever_the_path_holds() {
    let scene = scene();
    install_claude(&scene.bin, &scene.mark, "real");
    let run = turn_of(
        &scene.fixture,
        scene.bin.join("claude").into_os_string(),
        &scene.workspace,
        &scene.mark,
        "/nonexistent-launch-path:",
    );
    assert_eq!(
        run.ran, "real\n",
        "the stub-provider shape: an absolute program as configured"
    );
}

#[test]
fn two_real_fast_failed_processes_hold_before_a_third_launch() {
    use dudes_proto::{AutomaticHold, HoldReason, LoopDecision};
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let children = ChildEnds::new();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::default()),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(NamedAdapter("/usr/bin/false".into())) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&settings, seams).expect("boot"));
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    let cause = fixture.await_record("repeated failure hold", |record| match record {
        Record::AgentHeld {
            cause: cause @ AutomaticHold::RepeatedFastFailure { .. },
            ..
        } => Some(cause.clone()),
        _ => None,
    });
    fixture.await_record("hold decision", |record| match record {
        Record::LoopDecided {
            decision:
                LoopDecision::Hold {
                    reason: HoldReason::RepeatedFastFailure,
                },
            ..
        } => Some(()),
        _ => None,
    });
    running.handle.doctor().expect("barrier");
    let records = fixture.records();
    let ends: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::TurnEnded {
                turn,
                exit,
                elapsed_ms,
                ..
            } => Some((*turn, *exit, *elapsed_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 2);
    assert!(
        ends.iter()
            .all(|(_, exit, elapsed)| exit.code == Some(1) && elapsed.is_some_and(|ms| ms <= 5000))
    );
    assert_eq!(
        cause,
        AutomaticHold::RepeatedFastFailure {
            first: ends[0].0,
            latest: ends[1].0
        }
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TurnSpawned { .. }))
            .count(),
        2
    );
    running.stop(ExitCause::Ended);
    let mut projections =
        dudes_journal::Projections::open(&fixture.state.path().join("state.redb"))
            .expect("projection");
    let row = projections.agent(agent).expect("read row").expect("agent");
    assert_eq!(row.automatic_hold, Some(cause.clone()));
    let hold_lsn = fixture
        .entries()
        .iter()
        .find_map(|(lsn, e)| matches!(e.record, Record::AgentHeld { .. }).then_some(*lsn))
        .unwrap();
    assert_eq!(row.control_lsn, Some(hold_lsn));
    let views = duded::floor::snapshot(
        &mut projections,
        dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
    )
    .expect("views");
    assert_eq!(views.agents[0].automatic_hold, Some(cause));
}
