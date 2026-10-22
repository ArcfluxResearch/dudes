//! Runtime and state directories.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use dudes_proto::rpc::system::PingResult;

mod common;
use common::Cli;

struct Scenario {
    root: tempfile::TempDir,
    env: BTreeMap<OsString, OsString>,
    runtime: PathBuf,
    state: PathBuf,
    cwd: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        #[cfg(unix)]
        let root = tempfile::tempdir_in("/tmp").expect("short storage scenario path");
        #[cfg(windows)]
        let root = tempfile::tempdir().expect("isolated storage scenario");
        let profile = root.path().join("profile with spaces");
        let cwd = root.path().join("cwd");
        fs::create_dir_all(&cwd).expect("isolated cwd");
        let mut env = BTreeMap::new();
        for name in ["PATH", "SystemRoot", "TMPDIR", "LANG"] {
            if let Some(value) = std::env::var_os(name) {
                env.insert(name.into(), value);
            }
        }
        env.insert(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" }.into(),
            profile.as_os_str().to_owned(),
        );
        env.insert(
            "APPDATA".into(),
            root.path().join("config").into_os_string(),
        );
        env.insert(
            "XDG_CONFIG_HOME".into(),
            root.path().join("config").into_os_string(),
        );
        Self {
            runtime: profile.join(".local/state/dudes"),
            state: profile.join(".local/share/dudes"),
            root,
            env,
            cwd,
        }
    }

    fn cli(&self) -> Cli {
        self.env
            .iter()
            .fold(Cli::bare().current_dir(&self.cwd), |cli, (name, value)| {
                cli.env(name.to_str().expect("test variable name"), value)
            })
    }

    fn expected_endpoint(&self) -> PathBuf {
        dudes_rpc::local_endpoint(&self.runtime, "duded.sock")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.path().join("standalone.log")).unwrap_or_default()
    }

    fn spawn(&self) -> OwnedDaemon {
        fs::create_dir_all(self.runtime.parent().expect("runtime parent"))
            .expect("standalone runtime parents");
        let log = File::create(self.root.path().join("standalone.log")).expect("daemon log");
        let mut binary = PathBuf::from(env!("CARGO_BIN_EXE_dudes"));
        binary.set_file_name(format!("duded{}", std::env::consts::EXE_SUFFIX));
        assert!(
            binary.is_file(),
            "build the sibling daemon at {}",
            binary.display()
        );
        // A test the runner kills runs no destructors, so on Unix the daemon is a copy under the
        // scenario's own `bin` and a lifeline ends it when the test is gone.
        #[cfg(unix)]
        let (binary, lifeline) = {
            let bin = self.root.path().join("bin");
            fs::create_dir_all(&bin).expect("private daemon directory");
            let installed = bin.join("duded");
            fs::hard_link(&binary, &installed)
                .or_else(|_| fs::copy(&binary, &installed).map(|_| ()))
                .expect("install the test daemon");
            (installed, common::Lifeline::start(self.root.path()))
        };
        let child = common::process(binary.as_os_str(), &[], Some(&self.env))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("stdout log"))
            .stderr(log)
            .spawn()
            .expect("start standalone daemon");
        #[cfg(unix)]
        fs::write(
            self.root.path().join("duded.log"),
            format!("quickstart started duded pid {}\n", child.id()),
        )
        .expect("register the daemon with its lifeline");
        OwnedDaemon {
            child,
            #[cfg(unix)]
            _lifeline: lifeline,
        }
    }

    fn assert_ready(&self, daemon: &mut OwnedDaemon) {
        let resolved = dudes::Environment::resolve(|name| self.env.get(OsStr::new(name)).cloned());
        assert_eq!(resolved.runtime_dir(), Ok(self.runtime.as_path()));
        assert_eq!(resolved.socket(), Ok(self.expected_endpoint().as_path()));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let ping = self.cli().args(["--json", "ping"]).run();
            if ping.code() == dudes::exit::SUCCESS {
                let answer: PingResult = ping.json();
                assert_eq!(answer.socket, self.expected_endpoint(), "{ping:?}");
                break;
            }
            assert!(
                daemon
                    .child
                    .try_wait()
                    .expect("daemon exit status")
                    .is_none(),
                "standalone daemon exited before ping: {}",
                self.log()
            );
            assert!(
                Instant::now() < deadline,
                "standalone daemon did not answer at {}: {ping:?}\n{}",
                self.expected_endpoint().display(),
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            self.state.join("LOCK").is_file(),
            "no state lock at {}",
            self.state.display()
        );
        assert!(
            self.state.join("journal").is_dir(),
            "no journal at {}",
            self.state.display()
        );
        assert!(
            !self.cwd.join(".local").exists(),
            "default storage borrowed cwd"
        );
        daemon.stop(self);
    }
}

struct OwnedDaemon {
    child: Child,
    #[cfg(unix)]
    _lifeline: common::Lifeline,
}

impl OwnedDaemon {
    fn stop(&mut self, scenario: &Scenario) {
        let stopped = scenario.cli().args(["stop"]).run();
        assert_eq!(
            stopped.code(),
            dudes::exit::SUCCESS,
            "{stopped:?}\n{}",
            scenario.log()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("reap standalone daemon") {
                assert!(status.success(), "standalone daemon exited with {status}");
                break;
            }
            assert!(Instant::now() < deadline, "standalone daemon survived stop");
            thread::sleep(Duration::from_millis(20));
        }
        let ping = scenario.cli().args(["ping"]).run();
        assert_ne!(
            ping.code(),
            dudes::exit::SUCCESS,
            "daemon answers after reap"
        );
    }
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn standalone_daemon_and_cli_use_the_same_literal_storage_defaults() {
    let scenario = Scenario::new();
    let mut daemon = scenario.spawn();
    scenario.assert_ready(&mut daemon);
}

#[test]
fn absolute_home_wins_over_userprofile_in_both_binaries() {
    let mut scenario = Scenario::new();
    let home = scenario.root.path().join("explicit home");
    scenario
        .env
        .insert("HOME".into(), home.as_os_str().to_owned());
    scenario.env.insert(
        "USERPROFILE".into(),
        scenario.root.path().join("other profile").into_os_string(),
    );
    scenario.runtime = home.join(".local/state/dudes");
    scenario.state = home.join(".local/share/dudes");
    let mut daemon = scenario.spawn();
    scenario.assert_ready(&mut daemon);
    assert!(!scenario.root.path().join("other profile/.local").exists());
}

#[test]
fn storage_overrides_win_independently_and_relative_state_stays_anchored() {
    for (runtime_override, state_override) in [(true, false), (false, true), (true, true)] {
        let mut scenario = Scenario::new();
        if runtime_override {
            scenario.runtime = scenario.root.path().join("explicit runtime");
            scenario.env.insert(
                "DUDES_RUNTIME_DIR".into(),
                scenario.runtime.clone().into_os_string(),
            );
        }
        if state_override {
            scenario.state = scenario.cwd.join("relative-state");
            scenario
                .env
                .insert("DUDES_STATE_DIR".into(), "relative-state".into());
        }
        if runtime_override && state_override {
            scenario.env.remove(OsStr::new("HOME"));
            scenario.env.remove(OsStr::new("USERPROFILE"));
            #[cfg(target_os = "macos")]
            scenario.env.insert(
                "HOME".into(),
                scenario.root.path().join("config home").into_os_string(),
            );
        }
        let mut daemon = scenario.spawn();
        scenario.assert_ready(&mut daemon);
    }
}

#[test]
fn both_binaries_refuse_invalid_default_roots_without_creating_cwd_state() {
    for bad in [
        None,
        Some(""),
        Some("relative-home"),
        Some("C:relative"),
        Some(r"\relative"),
    ] {
        let mut scenario = Scenario::new();
        scenario.env.remove(OsStr::new("HOME"));
        scenario.env.remove(OsStr::new("USERPROFILE"));
        if let Some(bad) = bad {
            scenario.env.insert("HOME".into(), bad.into());
            scenario.env.insert(
                "USERPROFILE".into(),
                scenario.root.path().join("valid profile").into_os_string(),
            );
        }
        let resolved =
            dudes::Environment::resolve(|name| scenario.env.get(OsStr::new(name)).cloned());
        assert!(
            resolved.runtime_dir().is_err(),
            "invalid default root {bad:?}"
        );
        let cli = scenario.cli().args(["quickstart"]).run();
        let mut daemon = scenario.spawn();
        let deadline = Instant::now() + Duration::from_secs(5);
        while daemon
            .child
            .try_wait()
            .expect("refusing daemon status")
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "invalid root did not refuse\n{}",
                scenario.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
        let log = scenario.log();
        let cli_error = cli
            .stderr
            .strip_prefix("dudes: ")
            .expect("CLI refusal")
            .trim();
        let daemon_error = log.strip_prefix("duded: ").expect("daemon refusal").trim();
        assert_eq!(cli_error, daemon_error, "root {bad:?}");
        assert_ne!(cli.code(), dudes::exit::SUCCESS);
        assert!(
            fs::read_dir(&scenario.cwd)
                .expect("isolated cwd")
                .next()
                .is_none()
        );
        assert!(!scenario.state.exists());
    }
}
