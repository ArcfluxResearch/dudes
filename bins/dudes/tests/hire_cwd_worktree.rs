//! Relative hire paths reach the native daemon's real Git worktree producer canonically.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use dudes::exit;
use dudes_journal::{Fsync, JournalOptions};
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::system::ShutdownResult;
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_rpc::Client;
use dudes_types::{AgentId, Provider, Timestamp};

const BOUND: Duration = Duration::from_secs(30);

struct NativeDaemon {
    child: Child,
    _lifeline: common::Lifeline,
    socket: PathBuf,
    runtime: PathBuf,
    ended: bool,
}

fn native_binary(name: &str) -> PathBuf {
    std::env::var_os("DUDES_TEST_CWD_BIN_DIR")
        .map_or_else(
            || {
                Path::new(env!("CARGO_BIN_EXE_dudes"))
                    .parent()
                    .expect("standalone directory")
                    .to_path_buf()
            },
            PathBuf::from,
        )
        .join(name)
}

fn native_environment(fixture: &common::Fixture, root: &Path) -> BTreeMap<OsString, OsString> {
    let home = root.join("home");
    fs::create_dir(&home).unwrap();
    [
        ("PATH", OsString::from("/usr/bin:/bin:/usr/sbin:/sbin")),
        ("HOME", home.into_os_string()),
        ("TMPDIR", root.as_os_str().to_owned()),
        (
            "DUDES_STATE_DIR",
            fixture.state.path().as_os_str().to_owned(),
        ),
        (
            "DUDES_RUNTIME_DIR",
            fixture.runtime.path().join("run").into_os_string(),
        ),
        ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
        ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
        ("LANG", OsString::from("C")),
        ("TZ", OsString::from("UTC")),
    ]
    .into_iter()
    .map(|(name, value)| (OsString::from(name), value))
    .collect()
}

fn native_hire(daemon: &NativeDaemon, here: &Path, args: &[OsString]) -> common::Run {
    let env = BTreeMap::from([
        (
            OsString::from("DUDES_SOCK"),
            daemon.socket.as_os_str().to_owned(),
        ),
        (
            OsString::from("DUDES_RUNTIME_DIR"),
            daemon.runtime.as_os_str().to_owned(),
        ),
    ]);
    let output = common::process(native_binary("dudes").as_os_str(), args, Some(&env))
        .current_dir(here)
        .output()
        .expect("frozen native hire CLI");
    common::Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

impl NativeDaemon {
    fn start(fixture: &common::Fixture, env: &BTreeMap<OsString, OsString>) -> Self {
        let source = native_binary("duded");
        assert!(source.is_file(), "preflight the standalone duded sibling");
        let bin = fixture.runtime.path().join("bin");
        fs::create_dir(&bin).expect("private daemon directory");
        let binary = bin.join("duded");
        fs::hard_link(&source, &binary)
            .or_else(|_| fs::copy(&source, &binary).map(|_| ()))
            .expect("install the test daemon");
        let lifeline = common::Lifeline::start(fixture.runtime.path());
        let stderr = fs::File::create(fixture.runtime.path().join("daemon.stderr")).unwrap();
        let settings = fixture.settings();
        let child = common::process(binary.as_os_str(), &[], Some(env))
            .current_dir(fixture.runtime.path())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("scratch standalone daemon");
        let mut daemon = Self {
            child,
            _lifeline: lifeline,
            socket: settings.socket(),
            runtime: settings.runtime_dir,
            ended: false,
        };
        fs::write(
            fixture.runtime.path().join("duded.log"),
            format!("quickstart started duded pid {}\n", daemon.child.id()),
        )
        .expect("register the daemon with its lifeline");
        let deadline = Instant::now() + BOUND;
        loop {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon startup ended"
            );
            let ping = common::Cli::new(&daemon.socket, &daemon.runtime)
                .arg("ping")
                .run();
            if ping.code() == exit::SUCCESS {
                break;
            }
            assert!(Instant::now() < deadline, "scratch daemon never served");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }

    fn stop(&mut self) -> io::Result<String> {
        let mut client = Client::connect(&self.socket)?;
        let value = fs::read_to_string(self.runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME))?;
        let credential = OperatorCredential::new(value).map_err(io::Error::other)?;
        let reply = client
            .call_as_operator(Method::SystemShutdown, None, credential)
            .map_err(io::Error::other)?;
        let result: ShutdownResult = serde_json::from_value(
            reply
                .result
                .ok_or_else(|| io::Error::other("shutdown has no result"))?,
        )
        .map_err(io::Error::other)?;
        drop(client);
        let deadline = Instant::now() + BOUND;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.ended = true;
                if !status.success() {
                    return Err(io::Error::other("scratch daemon failed"));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("scratch daemon did not stop"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let receipt = result
            .completion
            .ok_or_else(|| io::Error::other("no completion receipt"))?;
        let text = fs::read_to_string(receipt.path)?;
        if text != format!("complete {}\n", receipt.run) {
            return Err(io::Error::other("shutdown did not complete"));
        }
        Ok(text)
    }
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.stop();
        }
        if !self.ended && self.child.try_wait().ok().flatten().is_none() {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = self.child.wait();
        }
    }
}

fn git(repo: &Path, env: &BTreeMap<OsString, OsString>, args: &[&str]) {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let result = common::process(Path::new("/usr/bin/git").as_os_str(), &args, Some(env))
        .current_dir(repo)
        .output()
        .expect("scratch Git setup");
    assert!(
        result.status.success(),
        "Git setup: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn legacy_hire(id: AgentId, cwd: PathBuf) -> Record {
    Record::AgentHired {
        id,
        name: "Legacy".to_owned(),
        character: None,
        accent: None,
        role: "writer".to_owned(),
        department: "fixture".to_owned(),
        provider: Provider::Claude,
        cwd,
        workspace: Some(dudes_proto::record::RecordedWorkspace {
            id: "historical_workspace".to_owned(),
            label: "Historical workspace".to_owned(),
        }),
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

#[test]
#[expect(
    clippy::print_stdout,
    reason = "retain actual startup warning and unchanged legacy record observations"
)]
fn startup_warns_once_for_live_relative_cwd_without_mutating_history() {
    let fixture = common::Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let env = native_environment(&fixture, root.path());
    let local = dudes::Environment::resolve(|name| env.get(&OsString::from(name)).cloned());
    let config = local.config().unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(config, "[providers.codex]\ncommand = \"/usr/bin/false\"\n[providers.pi]\ncommand = \"/usr/bin/false\"\n").unwrap();
    let live = AgentId::from_parts(1, 101);
    let absolute = AgentId::from_parts(1, 102);
    let retired = AgentId::from_parts(1, 103);
    let sentinel = "legacy-relative-cwd-sentinel-831";
    let records = vec![
        legacy_hire(live, PathBuf::from(sentinel)),
        legacy_hire(absolute, root.path().canonicalize().unwrap()),
        legacy_hire(retired, PathBuf::from("retired-relative-sentinel-831")),
        Record::AgentRetired {
            id: retired,
            reason: "fixture".to_owned(),
            browser_retire: None,
        },
    ];
    {
        let (mut writer, _) = dudes_journal::open(
            &fixture.state.path().join("journal"),
            JournalOptions {
                fsync: Fsync::Full,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        for record in &records {
            writer
                .append(&Entry {
                    at: Timestamp::from_unix_millis(1_787_394_600_000),
                    record: record.clone(),
                })
                .unwrap();
        }
    }
    let mut daemon = NativeDaemon::start(&fixture, &env);
    let completion = daemon.stop().expect("native owned cleanup");
    assert_eq!(
        fixture.records(),
        records,
        "warning adds no journal or control record"
    );
    let log = fs::read_to_string(fixture.runtime.path().join("daemon.stderr")).unwrap();
    assert!(
        log.lines()
            .next()
            .unwrap()
            .starts_with(&format!("duded {} on ", env!("CARGO_PKG_VERSION")))
    );
    let warnings: Vec<_> = log
        .lines()
        .filter(|line| line.contains("legacy relative working directory"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "the live relative identity gets one startup warning"
    );
    assert!(warnings[0].contains(&live.to_string()));
    assert!(!warnings[0].contains(&absolute.to_string()));
    assert!(!warnings[0].contains(&retired.to_string()));
    assert!(!warnings[0].contains(sentinel));
    println!(
        "{}",
        serde_json::json!({"warning":warnings[0],"live_agent":live,"absolute_agent":absolute,"retired_agent":retired,"records_unchanged":true,"shutdown_completion":completion})
    );
}

#[test]
#[expect(
    clippy::print_stdout,
    reason = "retain actual native lease, hire and shutdown observations"
)]
fn a_relative_worktree_hire_records_a_canonical_origin_and_its_distinct_lease() {
    let fixture = common::Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo with spaces");
    fs::create_dir(&repo).unwrap();
    let env = native_environment(&fixture, root.path());
    git(&repo, &env, &["init", "-b", "main"]);
    git(
        &repo,
        &env,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    let canonical = repo.canonicalize().unwrap();
    let local = dudes::Environment::resolve(|name| env.get(&OsString::from(name)).cloned());
    let config = local.config().expect("scratch config path");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(config, format!(
        "[providers.codex]\ncommand = \"/usr/bin/false\"\n[providers.pi]\ncommand = \"/usr/bin/false\"\n[[floor.onboarding.workspaces]]\nworkspace_id = \"canonical_repo\"\nlabel = \"Canonical repository\"\ncompletion_check_label = \"Checks pass\"\ncwd = {}\ndepartment = \"engineering\"\nisolation = \"default\"\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [\"/usr/bin/true\"]\npredicate_timeout_s = 10\n",
        serde_json::to_string(canonical.to_str().unwrap()).unwrap()
    )).unwrap();
    let mut daemon = NativeDaemon::start(&fixture, &env);
    let mut args = common::hire_args(Path::new("repo with spaces"));
    args.insert(0, OsString::from("--json"));
    args.extend([OsString::from("--trusted"), OsString::from("--worktree")]);
    let result = native_hire(&daemon, root.path(), &args);
    let records = fixture.records();
    let completion = daemon.stop().expect("native owned cleanup");
    assert!(completion.starts_with("complete "));
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    let (id, origin, path) = records
        .iter()
        .find_map(|record| match record {
            Record::WorktreeLeased {
                worktree,
                origin,
                path,
                ..
            } => Some((*worktree, origin, path)),
            _ => None,
        })
        .expect("actual lease record");
    assert_eq!(origin.as_os_str(), canonical.as_os_str());
    assert_ne!(path, &canonical);
    assert!(path.is_dir() && path.join(".git").is_file());
    let (cwd, worktree, workspace, trusted, provider) = records
        .iter()
        .find_map(|record| match record {
            Record::AgentHired {
                cwd,
                worktree,
                workspace,
                trusted,
                provider,
                ..
            } => Some((cwd, worktree, workspace, trusted, provider)),
            _ => None,
        })
        .expect("actual hire record");
    assert_eq!(cwd, path);
    assert_eq!(*worktree, Some(id));
    assert!(*trusted);
    assert_eq!(*provider, dudes_types::Provider::Claude);
    assert_eq!(
        workspace.as_ref().map(|value| value.id.as_str()),
        Some("canonical_repo")
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "the fixture assigns no goal or model turn"
    );
    println!(
        "{}",
        serde_json::json!({
            "origin": origin,
            "lease_path": path,
            "agent_cwd": cwd,
            "worktree": id.to_string(),
            "workspace": workspace,
            "trusted": trusted,
            "provider": provider,
            "managed_turns": 0,
            "shutdown_completion": completion,
            "cli_exit": result.code(),
        })
    );
}
