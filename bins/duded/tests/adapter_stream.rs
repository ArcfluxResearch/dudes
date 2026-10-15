//! The daemon reads a managed turn's standard output with the reader of the turn's own
//! adapter.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;

use duded::ports::{Answered, Ask};
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Arrival, Isolation, ManagedInstall, SessionAcquisition, TurnSession,
    UsageSource,
};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::launch::{EnvVar, LaunchError, LaunchSpec};
use dudes_providers::stream::{LineRead, ScopedUsage, SessionObservation, TurnEnd, TurnReader};
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{ModelSelection, Provider, SessionId, Tier, Tokens, Usage, Usd};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

const STUB_ENV: &str = "DUDES_ADAPTER_STREAM_STUB";
const SESSION: &str = "line-session-7";

#[test]
fn the_stub_entry_point() {
    use std::io::Write as _;

    if std::env::var_os(STUB_ENV).is_none() {
        return;
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "session {SESSION}");
    let _ = writeln!(out, "usage 120 45 330000");
    let _ = writeln!(out, "said the goal is met");
    let _ = out.flush();
    std::process::exit(0);
}

#[derive(Debug, Default)]
struct LineReader {
    session: Option<SessionId>,
    usage: Option<Usage>,
    said: Option<String>,
    unreadable: u64,
}

impl TurnReader for LineReader {
    fn line(&mut self, text: &str) -> LineRead {
        let mut read = LineRead::default();
        match text.split_once(' ') {
            Some(("session", id)) if self.session.is_none() => {
                self.session = Some(SessionId::new(id));
                read.session = Some(SessionObservation {
                    session: self.session.clone(),
                    ..SessionObservation::default()
                });
            }
            Some(("usage", numbers)) => {
                let numbers: Vec<u64> = numbers
                    .split(' ')
                    .filter_map(|number| number.parse().ok())
                    .collect();
                if let [input, output, micros] = numbers[..] {
                    self.usage = Some(Usage {
                        input: Tokens::new(input),
                        output: Tokens::new(output),
                        usd: Some(Usd::from_micros(micros)),
                        ..Usage::ZERO
                    });
                }
            }
            Some(("said", said)) => self.said = Some(said.to_owned()),
            _ => self.unreadable += 1,
        }
        read
    }

    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
        let completed = self.said.is_some() && exit.is_success();
        TurnEnd {
            final_observation: None,
            final_reply: None,
            outcome: if completed {
                TurnOutcome::Completed
            } else {
                TurnOutcome::NoResult
            },
            usage: self.usage.map(ScopedUsage::Turn),
            model_usage: BTreeMap::new(),
            last_message: self.said,
            usage_refused: None,
            session: self.session,
            attribution_refused: 0,
            unreadable: self.unreadable,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Reader {
    Lines,
    Claude,
}

#[derive(Debug)]
struct LineAdapter(Reader);

impl Adapter for LineAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::ObservedOnly
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SessionAcquisition::Assigned {
            resume: "resume",
            arrival: Arrival::Start,
        }
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
    ) -> Result<LaunchSpec, LaunchError> {
        let mut argv = vec![
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/adapter-stream-stand-in")
                .into_os_string(),
            OsString::from("exec"),
        ];
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        argv.extend([
            OsString::from("--add-dir"),
            ctx.agent_dir.join("outbox").into_os_string(),
        ]);
        argv.push(
            std::env::current_exe()
                .expect("current_exe")
                .into_os_string(),
        );
        argv.extend(["--exact", "the_stub_entry_point", "--quiet"].map(OsString::from));
        Ok(LaunchSpec {
            argv,
            env: ctx.daemon_env.to_vec(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        match self.0 {
            Reader::Lines => Box::new(LineReader::default()),
            Reader::Claude => ClaudeAdapter::default().turn_reader(),
        }
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::None
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

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

fn one_turn(reader: Reader) -> Vec<Record> {
    let fixture = Fixture::new();
    let mut daemon_env: Vec<EnvVar> = ["PATH", "HOME", "TMPDIR"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(*name, value)))
        .collect();
    daemon_env.push(EnvVar::new(STUB_ENV, "1"));
    let settings = duded::settings::Settings {
        daemon_env,
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let seams = Seams {
        adapters: [Box::new(LineAdapter(reader)) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: Arc::clone(&children),
        ..common::seams(
            &settings,
            RealHost::watching(Arc::new(HostStreams(children))),
            Arc::new(ScriptedRunner::scripted([common::passes()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        )
    };
    let mut running = common::start(boot(&settings, seams).expect("boot"));

    let hired = running.handle.ask(Ask::Hire(Box::new(HireParams {
        provider: Provider::Codex,
        ..common::hire_params(fixture.state.path())
    })));
    let Ok(Answered::Hired(hired)) = hired else {
        panic!("agent.hire answered {hired:?}");
    };
    common::set_goal(
        &running.handle,
        common::goal_params(hired.agent, &["/usr/bin/true"]),
    );
    fixture.await_record("TurnEnded", |record| {
        matches!(record, Record::TurnEnded { .. }).then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    fixture.records()
}

fn issued(records: &[Record]) -> Vec<Option<&SessionId>> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued { session, .. } => Some(session.as_ref()),
            _ => None,
        })
        .collect()
}

fn learned(records: &[Record]) -> Vec<&SessionId> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::SessionLearned { session, .. } => Some(session),
            _ => None,
        })
        .collect()
}

fn ended(records: &[Record]) -> (Option<Usage>, Option<TurnOutcome>, Option<&str>) {
    records
        .iter()
        .find_map(|record| match record {
            Record::TurnEnded {
                usage,
                outcome,
                last_message,
                ..
            } => Some((*usage, outcome.clone(), last_message.as_deref())),
            _ => None,
        })
        .expect("the turn ended")
}

#[test]
fn a_turn_is_read_by_its_agents_adapter() {
    let records = one_turn(Reader::Lines);
    assert_eq!(issued(&records), [None]);
    assert_eq!(learned(&records), [&SessionId::new(SESSION)]);
    assert_eq!(
        ended(&records),
        (
            Some(Usage {
                input: Tokens::new(120),
                output: Tokens::new(45),
                usd: Some(Usd::from_micros(330_000)),
                ..Usage::ZERO
            }),
            Some(TurnOutcome::Completed),
            Some("the goal is met"),
        )
    );
}

#[test]
fn the_same_child_read_by_claude_learns_no_session() {
    let records = one_turn(Reader::Claude);
    assert_eq!(
        learned(&records),
        Vec::<&SessionId>::new(),
        "Claude's reader finds no session in another vendor's lines"
    );
    assert_eq!(ended(&records), (None, Some(TurnOutcome::NoResult), None));
}
