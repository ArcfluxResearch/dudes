//! A managed agent reaches the daemon's own `dudes`: the daemon's one-entry `agent-bin` directory
//! leads the child's `PATH`, and a managed Claude launch names a fixed guard file in
//! `CLAUDE_ENV_FILE`. Neither touches a task byte or a journal record.

use std::ffi::OsStr;
use std::path::PathBuf;

use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_types::AgentId;

const AGENT_BIN: &str = "/run/dudes/agent-bin";

fn spawn_of(
    config: dudes_supervisor::machine::Config,
) -> (AgentId, LaunchSpec, Vec<(PathBuf, String)>) {
    let mut harness = Harness::with_config(
        config,
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || Box::new(ClaudeAdapter::default()),
    );
    let (agent, _goal) = harness.hire_with_goal(&["npm", "test"]);
    harness.open_turn(agent).expect("a managed turn");
    harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Spawn { spec, install, .. } => Some((agent, spec.clone(), install.files.clone())),
            _ => None,
        })
        .expect("one managed launch")
}

fn config_with(path: Option<&str>, agent_bin: Option<&str>) -> dudes_supervisor::machine::Config {
    let mut config = Harness::config();
    config.daemon_env = vec![EnvVar::new("HOME", "/home/op"), EnvVar::new("LANG", "C")];
    if let Some(path) = path {
        config.daemon_env.push(EnvVar::new("PATH", path));
    }
    config.agent_bin = agent_bin.map(PathBuf::from);
    config
}

fn value<'a>(spec: &'a LaunchSpec, name: &str) -> Option<&'a OsStr> {
    spec.env
        .iter()
        .find(|var| var.name() == name)
        .map(EnvVar::expose_value)
}

fn names(spec: &LaunchSpec) -> Vec<String> {
    spec.env
        .iter()
        .map(|var| var.name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_agent_bin_directory_leads_path_and_nothing_else_moves() {
    let path = std::env::join_paths(["/opt/a", "/usr/bin"]).unwrap();
    let (_, without, _) = spawn_of(config_with(path.to_str(), None));
    let (_, with, _) = spawn_of(config_with(path.to_str(), Some(AGENT_BIN)));
    assert_eq!(
        std::env::split_paths(value(&with, "PATH").unwrap()).collect::<Vec<_>>(),
        [AGENT_BIN, "/opt/a", "/usr/bin"].map(PathBuf::from)
    );
    assert_eq!(
        names(&with),
        names(&without),
        "no variable added, removed or moved"
    );
    for var in &with.env {
        if var.name() != "PATH" && var.name() != "DUDES_TOKEN" {
            assert_eq!(
                value(&without, &var.name().to_string_lossy()),
                Some(var.expose_value())
            );
        }
    }
    assert_eq!(with.argv, without.argv);
}

#[test]
fn without_an_agent_bin_path_is_untouched() {
    let (_, spec, _) = spawn_of(config_with(Some("/opt/a:/usr/bin"), None));
    assert_eq!(value(&spec, "PATH"), Some(OsStr::new("/opt/a:/usr/bin")));
}

#[test]
fn a_child_without_a_path_is_given_none() {
    let (_, spec, _) = spawn_of(config_with(None, Some(AGENT_BIN)));
    assert_eq!(value(&spec, "PATH"), None);
}

#[cfg(unix)]
#[test]
fn a_directory_holding_the_path_separator_adds_nothing() {
    let (_, spec, _) = spawn_of(config_with(Some("/usr/bin"), Some("/run/a:b/agent-bin")));
    assert_eq!(value(&spec, "PATH"), Some(OsStr::new("/usr/bin")));
}

#[cfg(unix)]
#[test]
fn a_claude_launch_names_the_fixed_guard_file_in_its_own_directory() {
    let mut config = config_with(Some("/usr/bin"), Some(AGENT_BIN));
    config
        .daemon_env
        .push(EnvVar::new("CLAUDE_ENV_FILE", "/home/op/venv.sh"));
    let data_dir = config.data_dir.clone();
    let (agent, spec, files) = spawn_of(config);
    let expected = data_dir
        .join("agents")
        .join(agent.to_string())
        .join("claude-env.sh");
    assert_eq!(value(&spec, "CLAUDE_ENV_FILE"), Some(expected.as_os_str()));
    assert_eq!(
        names(&spec)
            .iter()
            .filter(|n| *n == "CLAUDE_ENV_FILE")
            .count(),
        1,
        "the operator's own value is replaced, not joined"
    );
    let guard: Vec<_> = files.iter().filter(|(path, _)| *path == expected).collect();
    assert_eq!(guard.len(), 1);
    assert_eq!(
        guard[0].1.as_bytes(),
        b"unset -f dudes 2>/dev/null\nunalias dudes 2>/dev/null\n"
    );
}

#[cfg(unix)]
#[test]
fn two_agents_get_two_guard_files() {
    let mut harness = Harness::with_config(
        config_with(Some("/usr/bin"), None),
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || Box::new(ClaudeAdapter::default()),
    );
    for _ in 0..2 {
        let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
        harness.open_turn(agent).expect("a managed turn");
    }
    let files: Vec<_> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => value(spec, "CLAUDE_ENV_FILE").map(OsStr::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 2);
    assert_ne!(files[0], files[1]);
}
