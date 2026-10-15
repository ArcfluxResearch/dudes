//! Exact-version admission and secret exclusion for Pi HTTP server opt-in.
#![cfg(unix)]

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::RunOptions;
use duded::startup::Witness;
use dudes_proto::Record;
use dudes_providers::adapter::Adapter;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_providers::pi::PiAdapter;
use dudes_providers::pi::config::PiConfig;
use dudes_providers::registry::Adapters;
use dudes_types::{McpServerName, Provider};

mod common;

struct VersionRunner {
    output: Vec<u8>,
    timed_out: bool,
    truncated: bool,
    calls: Mutex<Vec<(LaunchSpec, Duration, usize)>>,
}

impl PredicateRunner for VersionRunner {
    fn run(&self, _: &LaunchSpec, _: Duration) -> io::Result<PredicateOutcome> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cap: usize,
    ) -> io::Result<PredicateCaptured> {
        self.calls
            .lock()
            .unwrap()
            .push((spec.clone(), timeout, cap));
        Ok(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: self.output.clone(),
            truncated: self.truncated,
            timed_out: self.timed_out,
        })
    }

    fn sweep(&self) {}
}

struct GatedRunner {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<&'static [u8]>>,
}

impl PredicateRunner for GatedRunner {
    fn run(&self, _: &LaunchSpec, _: Duration) -> io::Result<PredicateOutcome> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn run_captured(&self, _: &LaunchSpec, _: Duration, _: usize) -> io::Result<PredicateCaptured> {
        self.entered.lock().unwrap().send(()).unwrap();
        let answer = self.release.lock().unwrap().recv().unwrap();
        Ok(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: answer.to_vec(),
            truncated: false,
            timed_out: false,
        })
    }

    fn sweep(&self) {}
}

struct PiCase {
    running: common::Running,
    runner: Arc<VersionRunner>,
    fixture: common::Fixture,
    program: std::path::PathBuf,
}

fn boot_case(output: &[u8], timed_out: bool, truncated: bool) -> PiCase {
    let fixture = common::Fixture::new();
    let program = fixture.state.path().join("pi");
    std::fs::write(&program, "not executed by the injected runner").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runner = Arc::new(VersionRunner {
        output: output.to_vec(),
        timed_out,
        truncated,
        calls: Mutex::default(),
    });
    let mut settings = fixture.settings();
    settings.daemon_env.extend([
        EnvVar::new("MEMORY_AUTH", "synthetic-pi-header"),
        EnvVar::new("DUDES_TOKEN", "synthetic-agent-token"),
        EnvVar::new("DUDES_SOCK", "/scratch/agent.sock"),
        EnvVar::new("DUDES_MAILBOX", "/scratch/mailbox"),
    ]);
    let mut seams = common::seams(
        &settings,
        common::SharedHost::new(),
        runner.clone(),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    seams.adapters = Adapters::new([Box::new(PiAdapter::new(PiConfig {
        command: program.to_str().unwrap().to_owned(),
    })) as Box<dyn Adapter>]);
    let declarations = dudes_providers::mcp::parse_servers(toml::from_str("[memory]\nurl = 'https://memory.example/mcp'\nheader_env = { Authorization = 'MEMORY_AUTH' }\n").unwrap()).unwrap();
    let daemon = duded::boot::boot_with_options(
        &settings,
        seams,
        RunOptions::default().with_mcp_servers(declarations),
    )
    .unwrap();
    let running = common::start(daemon);
    PiCase {
        running,
        runner,
        fixture,
        program,
    }
}

fn assert_probe(runner: &VersionRunner, program: &std::path::Path, state: &std::path::Path) {
    let calls = runner.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (spec, timeout, cap) = &calls[0];
    assert_eq!(
        spec.argv,
        [program.as_os_str(), std::ffi::OsStr::new("--version")]
    );
    assert_eq!(*timeout, Duration::from_secs(5));
    assert_eq!(*cap, 256);
    assert!(matches!(spec.stdin, LaunchInput::Null));
    assert_eq!(spec.cwd, state.join("condense-model-cwd"));
    for name in [
        "MEMORY_AUTH",
        "DUDES_MCP_MEMORY_HEADER_0",
        "DUDES_TOKEN",
        "DUDES_SOCK",
        "DUDES_MAILBOX",
    ] {
        assert!(!spec.env.iter().any(|var| var.name() == name), "{name}");
    }
}

#[test]
fn opted_in_hires_require_one_exact_tested_version_before_any_append() {
    for (output, timed_out, truncated) in [
        (b"1.0.3\n".as_slice(), false, false),
        (b"1.0.5\n", false, false),
        (b"2.0.0\n", false, false),
        (b"1.0.4-rc.1\n", false, false),
        (b"v1.0.4\n", false, false),
        (b"1.0.4\nsecond\n", false, false),
        (b"", false, false),
        (b"", true, false),
        (b"1.0.4\n", false, true),
        (b"1.0.4\n", false, false),
    ] {
        let PiCase {
            mut running,
            runner,
            fixture,
            program,
        } = boot_case(output, timed_out, truncated);
        let mut params = common::hire_params(fixture.state.path());
        params.provider = Provider::Pi;
        params.mcp_servers = vec![McpServerName::parse("memory").unwrap()];
        let before = fixture.records().len();
        let reply = running
            .handle
            .ask(duded::ports::Ask::Hire(Box::new(params.clone())));
        let accepted = output == b"1.0.4\n" && !timed_out && !truncated;
        if accepted {
            assert!(
                matches!(reply, Ok(duded::ports::Answered::Hired(_))),
                "{reply:?}"
            );
            assert_eq!(
                fixture
                    .records()
                    .iter()
                    .filter(|r| matches!(r, Record::AgentHired { .. }))
                    .count(),
                1
            );
        } else {
            let error = reply.expect_err("an untested or unusable answer refuses the hire");
            assert_eq!(error.code, -32004);
            assert!(
                error
                    .message
                    .starts_with("Pi MCP servers need a tested pi (1.0.4);"),
                "{}",
                error.message
            );
            assert_eq!(fixture.records().len(), before);
        }
        assert_probe(&runner, &program, fixture.state.path());
        params.mcp_servers.clear();
        params.name = "Plain".to_owned();
        assert!(matches!(
            running
                .handle
                .ask(duded::ports::Ask::Hire(Box::new(params))),
            Ok(duded::ports::Answered::Hired(_))
        ));
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            1,
            "plain hires never probe"
        );
        for (trusted, sandbox) in [(true, None), (false, Some(true))] {
            let mut invalid = common::hire_params(fixture.state.path());
            invalid.provider = Provider::Pi;
            invalid.mcp_servers = vec![McpServerName::parse("memory").unwrap()];
            invalid.trusted = trusted;
            invalid.sandbox = sandbox;
            let before = fixture.records().len();
            assert!(
                running
                    .handle
                    .ask(duded::ports::Ask::Hire(Box::new(invalid)))
                    .is_err()
            );
            assert_eq!(
                runner.calls.lock().unwrap().len(),
                1,
                "structural refusals precede process work"
            );
            assert_eq!(fixture.records().len(), before);
        }
        running.stop(duded::ExitCause::Ended);
    }
}

#[test]
fn an_unavailable_pi_cannot_answer_the_version_probe_or_append_a_hire() {
    let PiCase {
        mut running,
        runner,
        fixture,
        program,
    } = boot_case(b"1.0.4\n", false, false);
    std::fs::remove_file(program).unwrap();
    let mut params = common::hire_params(fixture.state.path());
    params.provider = Provider::Pi;
    params.mcp_servers = vec![McpServerName::parse("memory").unwrap()];
    let before = fixture.records();
    let error = running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(params)))
        .unwrap_err();
    assert_eq!(error.code, -32004);
    assert_eq!(
        error.message,
        "Pi MCP servers need a tested pi (1.0.4); the installed pi did not answer"
    );
    assert_eq!(fixture.records(), before);
    assert!(runner.calls.lock().unwrap().is_empty());
    running.stop(duded::ExitCause::Ended);
}

#[test]
fn changed_pi_edits_probe_last_but_reads_noops_and_clear_do_not_probe() {
    for output in [b"1.0.4\n".as_slice(), b"1.0.5\n"] {
        let PiCase {
            mut running,
            runner,
            fixture,
            program,
        } = boot_case(output, false, false);
        let mut hire = common::hire_params(fixture.state.path());
        hire.provider = Provider::Pi;
        let duded::ports::Answered::Hired(hired) = running
            .handle
            .ask(duded::ports::Ask::Hire(Box::new(hire)))
            .unwrap()
        else {
            panic!("a hire");
        };
        assert!(runner.calls.lock().unwrap().is_empty());
        let before = fixture.records();
        let ask = |servers| {
            duded::ports::Ask::Mcp(dudes_proto::rpc::agent::McpParams {
                agent: hired.agent,
                mcp_servers: servers,
            })
        };
        let answer = running
            .handle
            .ask(ask(Some(vec![McpServerName::parse("memory").unwrap()])));
        assert_probe(&runner, &program, fixture.state.path());
        if output == b"1.0.4\n" {
            let duded::ports::Answered::Mcp(result) = answer.unwrap() else {
                panic!("an MCP result");
            };
            assert!(result.changed);
            for request in [
                ask(None),
                ask(Some(result.mcp_servers)),
                ask(Some(Vec::new())),
                ask(Some(Vec::new())),
            ] {
                assert!(matches!(
                    running.handle.ask(request).unwrap(),
                    duded::ports::Answered::Mcp(_)
                ));
            }
            assert_eq!(runner.calls.lock().unwrap().len(), 1);
            let changes = fixture
                .records()
                .into_iter()
                .filter_map(|record| match record {
                    Record::AgentMcpServersChanged { mcp_servers, .. } => Some(mcp_servers),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                changes,
                [vec![McpServerName::parse("memory").unwrap()], Vec::new()]
            );
        } else {
            assert_eq!(
                answer.unwrap_err().code,
                dudes_proto::rpc::ErrorCode::Refused.code()
            );
            assert_eq!(fixture.records(), before);
        }
        running.stop(duded::ExitCause::Ended);
    }
}

struct GatedCase {
    running: common::Running,
    fixture: common::Fixture,
    probe_started: std::sync::mpsc::Receiver<()>,
    release_probe: std::sync::mpsc::Sender<&'static [u8]>,
}

impl GatedCase {
    fn new() -> Self {
        let fixture = common::Fixture::new();
        let settings = fixture.settings();
        let program = fixture.state.path().join("pi");
        std::fs::write(&program, "not executed by the injected runner").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (entered, probe_started) = std::sync::mpsc::channel();
        let (release_probe, release) = std::sync::mpsc::channel();
        let runner = Arc::new(GatedRunner {
            entered: Mutex::new(entered),
            release: Mutex::new(release),
        });
        let mut seams = common::seams(
            &settings,
            common::SharedHost::new(),
            runner,
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = Adapters::new([Box::new(PiAdapter::new(PiConfig {
            command: program.to_str().unwrap().to_owned(),
        })) as Box<dyn Adapter>]);
        let declarations = dudes_providers::mcp::parse_servers(
            toml::from_str("[memory]\nurl = 'https://memory.example/mcp'\n").unwrap(),
        )
        .unwrap();
        let daemon = duded::boot::boot_with_options(
            &fixture.settings(),
            seams,
            RunOptions::default().with_mcp_servers(declarations),
        )
        .unwrap();
        Self {
            running: common::start(daemon),
            fixture,
            probe_started,
            release_probe,
        }
    }

    fn hire(&self, name: &str) -> duded::ports::Ask {
        let mut params = common::hire_params(self.fixture.state.path());
        params.provider = Provider::Pi;
        name.clone_into(&mut params.name);
        params.mcp_servers = vec![McpServerName::parse("memory").unwrap()];
        duded::ports::Ask::Hire(Box::new(params))
    }

    fn wait_for_probe(&self) {
        self.probe_started
            .recv_timeout(Duration::from_secs(10))
            .expect("the hire starts its own probe");
    }
}

#[test]
fn a_slow_version_probe_never_stalls_the_loop_and_every_hire_is_probed() {
    let mut case = GatedCase::new();
    std::thread::scope(|scope| {
        let ask = case.hire("First");
        let handle = &case.running.handle;
        let hire = scope.spawn(move || handle.ask(ask));
        case.wait_for_probe();
        let (listed, answer) = std::sync::mpsc::channel();
        let handle = &case.running.handle;
        scope.spawn(move || {
            let _ = listed.send(handle.ask(duded::ports::Ask::AgentList));
        });
        let listing = answer.recv_timeout(Duration::from_secs(3));
        case.release_probe.send(b"1.0.4\n").unwrap();
        listing
            .expect("the loop answers while the probe is still running")
            .expect("the list");
        assert!(matches!(
            hire.join().unwrap(),
            Ok(duded::ports::Answered::Hired(_))
        ));
    });
    std::thread::scope(|scope| {
        let ask = case.hire("Second");
        let handle = &case.running.handle;
        let hire = scope.spawn(move || handle.ask(ask));
        case.wait_for_probe();
        case.release_probe.send(b"1.0.4\n").unwrap();
        assert!(matches!(
            hire.join().unwrap(),
            Ok(duded::ports::Answered::Hired(_))
        ));
    });
    case.running.stop(duded::ExitCause::Ended);
}

#[test]
fn two_parked_hires_are_each_judged_by_their_own_probe() {
    let mut case = GatedCase::new();
    let replies = std::thread::scope(|scope| {
        let ask = case.hire("First");
        let handle = &case.running.handle;
        let first = scope.spawn(move || handle.ask(ask));
        let ask = case.hire("Second");
        let handle = &case.running.handle;
        let second = scope.spawn(move || handle.ask(ask));
        case.wait_for_probe();
        case.wait_for_probe();
        case.release_probe.send(b"1.0.4\n").unwrap();
        case.release_probe.send(b"1.0.5\n").unwrap();
        [first.join().unwrap(), second.join().unwrap()]
    });
    let hired = replies
        .iter()
        .filter(|reply| matches!(reply, Ok(duded::ports::Answered::Hired(_))))
        .count();
    let refused = replies
        .iter()
        .filter(|reply| {
            reply.as_ref().err().is_some_and(|error| {
                error
                    .message
                    .starts_with("Pi MCP servers need a tested pi (1.0.4);")
            })
        })
        .count();
    assert_eq!((hired, refused), (1, 1), "{replies:?}");
    case.running.stop(duded::ExitCause::Ended);
}
