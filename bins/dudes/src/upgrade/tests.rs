use std::cell::RefCell;

use super::*;

fn v(text: &str) -> Version {
    Version::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
}

#[test]
fn versions_parse_strictly() {
    for good in [
        "0.0.0",
        "0.86.0",
        "10.20.30",
        "1.0.0-rc.1",
        "1.0.0-alpha-2.x",
    ] {
        assert!(Version::parse(good).is_some(), "{good}");
    }
    for bad in [
        "",
        "1",
        "1.0",
        "1.0.0.0",
        "v1.0.0",
        "01.0.0",
        "1.00.0",
        "1.0.-1",
        "1.0.0-",
        "1.0.0-a..b",
        "1.0.0+build",
        "1.0.0 ",
        " 1.0.0",
        "1.0.x",
        "99999999999999999999.0.0",
    ] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn versions_compare_by_semver_precedence() {
    let ascending = [
        "0.85.0",
        "0.86.0",
        "0.86.1",
        "0.100.0",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "2.0.0",
    ];
    for pair in ascending.windows(2) {
        assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        assert!(v(pair[1]) > v(pair[0]));
    }
    assert_eq!(v("0.86.0"), v("0.86.0"));
}

#[test]
fn the_latest_release_is_read_from_the_redirect_and_nothing_else() {
    let tag = "https://github.com/ArcfluxResearch/dudes/releases/tag/v";
    assert_eq!(latest_from(&format!("{tag}0.86.0")), Some(v("0.86.0")));
    assert_eq!(latest_from(&format!("  {tag}0.86.0\n")), Some(v("0.86.0")));
    for wrong in [
        String::new(),
        format!("{tag}0.86"),
        format!("{tag}0.86.0/../x"),
        format!("{tag}0.86.0?x=1"),
        "https://example.com/ArcfluxResearch/dudes/releases/tag/v0.86.0".to_owned(),
        "http://github.com/ArcfluxResearch/dudes/releases/tag/v0.86.0".to_owned(),
        "https://github.com/ArcfluxResearch/dudes/releases/tag/0.86.0".to_owned(),
    ] {
        assert_eq!(latest_from(&wrong), None, "{wrong}");
    }
}

#[test]
fn the_curl_and_installer_argument_vectors_are_exactly_these() {
    let shown = |args: Vec<OsString>| -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        shown(latest_arguments()),
        [
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "30",
            "--output",
            "/dev/null",
            "--write-out",
            "%{redirect_url}",
            "https://github.com/ArcfluxResearch/dudes/releases/latest",
        ]
    );
    assert_eq!(
        shown(download_arguments("0.86.0", Path::new("/tmp/d/install.sh"))),
        [
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "5",
            "--max-time",
            "120",
            "--max-filesize",
            "1048576",
            "--output",
            "/tmp/d/install.sh",
            "https://raw.githubusercontent.com/ArcfluxResearch/dudes/v0.86.0/install.sh",
        ]
    );
    assert_eq!(
        shown(install_arguments(
            Path::new("/tmp/d/install.sh"),
            "0.86.0",
            Path::new("/opt/x")
        )),
        [
            "/tmp/d/install.sh",
            "--version",
            "0.86.0",
            "--prefix",
            "/opt/x"
        ]
    );
    assert_eq!(CURL, "/usr/bin/curl");
}

struct FakeRunner {
    curl_present: bool,
    latest: Result<String, String>,
    script: &'static [u8],
    download_ok: bool,
    install_ok: bool,
    calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            curl_present: true,
            latest: Ok("https://github.com/ArcfluxResearch/dudes/releases/tag/v0.86.0".to_owned()),
            script: b"#!/bin/sh\nexit 0\n",
            download_ok: true,
            install_ok: true,
            calls: RefCell::default(),
        }
    }
}

fn output(success: bool, stdout: &str, stderr: &str) -> Output {
    Output {
        success,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

impl Runner for FakeRunner {
    fn exists(&self, _: &Path) -> bool {
        self.curl_present
    }

    fn run(
        &self,
        program: &Path,
        args: &[OsString],
        _: &[(OsString, OsString)],
    ) -> std::io::Result<Output> {
        let shown: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.calls
            .borrow_mut()
            .push((program.to_path_buf(), shown.clone()));
        if program == Path::new(INSTALL_SH) {
            assert!(
                Path::new(&shown[0]).is_file(),
                "the installer exists while it runs"
            );
            return Ok(if self.install_ok {
                output(true, "Installed Dudes 0.86.0\n", "")
            } else {
                output(false, "", "dudes install: checksum mismatch\n")
            });
        }
        if shown.contains(&"--write-out".to_owned()) {
            return Ok(match &self.latest {
                Ok(url) => output(true, url, ""),
                Err(reason) => output(false, "", reason),
            });
        }
        if !self.download_ok {
            return Ok(output(false, "", "curl: (22) 404"));
        }
        let at = shown
            .iter()
            .position(|arg| arg == "--output")
            .expect("an output file");
        std::fs::write(&shown[at + 1], self.script).expect("write the installer");
        Ok(output(true, "", ""))
    }
}

#[derive(Default)]
struct FakeDaemon {
    running: bool,
    busy: Vec<String>,
    stop_fails: bool,
    start_fails: bool,
    started_as: Option<&'static str>,
    events: RefCell<Vec<String>>,
}

impl Daemon for FakeDaemon {
    fn running(&self) -> bool {
        self.running
    }

    fn mid_turn(&self) -> Result<Vec<String>, String> {
        Ok(self.busy.clone())
    }

    fn stop(&self, now: bool, out: &mut dyn Write, _: &mut dyn Write) -> Result<(), String> {
        self.events.borrow_mut().push(format!("stop now={now}"));
        if self.stop_fails {
            return Err("it would not stop".to_owned());
        }
        writeln!(out, "the daemon stopped").map_err(|error| error.to_string())
    }

    fn start(&self, _: &mut dyn Write) -> Result<String, String> {
        self.events.borrow_mut().push("start".to_owned());
        if self.start_fails {
            return Err("it would not start".to_owned());
        }
        Ok(self.started_as.unwrap_or("0.86.0").to_owned())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    inputs: Inputs,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a scratch directory");
        let base = root.path().canonicalize().expect("a real path");
        let prefix = base.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/dudes")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        for name in PROGRAMS {
            std::fs::write(prefix.join("bin").join(name), "x").unwrap();
        }
        for name in NOTICES {
            std::fs::write(prefix.join("share/dudes").join(name), "x").unwrap();
        }
        let inputs = Inputs {
            windows: false,
            current: "0.85.0".to_owned(),
            executable: prefix.join("bin/dudes"),
            temp_dir: base.join("tmp"),
            child_env: Vec::new(),
        };
        Self { root, inputs }
    }

    fn prefix(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("prefix")
    }

    fn run(
        &self,
        options: UpgradeOptions,
        runner: &FakeRunner,
        daemon: &FakeDaemon,
    ) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&self.inputs, options, runner, daemon, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn temp_is_clean(&self) -> bool {
        std::fs::read_dir(&self.inputs.temp_dir)
            .unwrap()
            .next()
            .is_none()
    }
}

fn none() -> UpgradeOptions {
    UpgradeOptions::default()
}

#[test]
fn check_prints_both_versions_and_runs_one_command() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = FakeDaemon {
        running: true,
        ..FakeDaemon::default()
    };
    let (code, out, err) = fixture.run(
        UpgradeOptions {
            check: true,
            now: false,
        },
        &runner,
        &daemon,
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "Current version: 0.85.0\nLatest release:  0.86.0\nAn upgrade is available. Run dudes upgrade to install it.\n",
            ""
        )
    );
    assert_eq!(runner.calls.borrow().len(), 1);
    assert!(daemon.events.borrow().is_empty());
}

#[test]
fn already_current_says_so_and_changes_nothing() {
    let mut fixture = Fixture::new();
    fixture.inputs.current = "0.86.0".to_owned();
    let runner = FakeRunner::default();
    let daemon = FakeDaemon {
        running: true,
        ..FakeDaemon::default()
    };
    let (code, out, _) = fixture.run(none(), &runner, &daemon);
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "Current version: 0.86.0\nLatest release:  0.86.0\nDudes is up to date. Nothing was changed.\n"
        )
    );
    assert_eq!(
        runner.calls.borrow().len(),
        1,
        "only the latest-release lookup ran"
    );
    assert!(daemon.events.borrow().is_empty());
}

#[test]
fn a_newer_copy_is_never_downgraded() {
    let mut fixture = Fixture::new();
    fixture.inputs.current = "0.87.0".to_owned();
    let runner = FakeRunner::default();
    let (code, out, _) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(code, 0);
    assert!(
        out.ends_with("This dudes is newer than the latest release. Nothing was changed.\n"),
        "{out}"
    );
    assert_eq!(runner.calls.borrow().len(), 1);
}

#[test]
fn a_prerelease_current_upgrades_to_its_release() {
    let mut fixture = Fixture::new();
    fixture.inputs.current = "0.86.0-rc.1".to_owned();
    let runner = FakeRunner::default();
    let (code, _, _) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(code, 0);
    assert_eq!(runner.calls.borrow().len(), 3);
}

#[test]
fn a_source_build_is_refused_with_how_to_rebuild() {
    let mut fixture = Fixture::new();
    let dist = fixture.prefix().join("target/dist");
    std::fs::create_dir_all(&dist).unwrap();
    fixture.inputs.executable = dist.join("dudes");
    let runner = FakeRunner::default();
    let (code, _, err) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(code, exit::OTHER);
    assert_eq!(
        err,
        format!(
            "dudes: this copy of dudes ({}) was not put there by install.sh, so dudes upgrade will not replace it. If you built it from source, rebuild it: https://github.com/ArcfluxResearch/dudes/blob/main/docs/building-from-source.md\n",
            dist.join("dudes").display()
        )
    );
    assert_eq!(runner.calls.borrow().len(), 1, "nothing was downloaded");
}

#[test]
fn a_bin_directory_without_the_installers_notices_is_refused() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.prefix().join("share/dudes/LICENSE-MIT")).unwrap();
    let runner = FakeRunner::default();
    let (code, _, err) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(code, exit::OTHER);
    assert!(err.contains("was not put there by install.sh"), "{err}");
    assert_eq!(runner.calls.borrow().len(), 1);
}

#[test]
fn a_mid_turn_agent_refuses_unless_now() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = FakeDaemon {
        running: true,
        busy: vec!["Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX)".to_owned()],
        ..FakeDaemon::default()
    };
    let (code, _, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            "dudes: these agents are mid-turn: Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX). Restarting the daemon would end their turns. Wait for them to finish, or pass --now. Nothing was changed.\n"
        )
    );
    assert_eq!(runner.calls.borrow().len(), 1);
    assert!(daemon.events.borrow().is_empty());

    let (code, out, _) = fixture.run(
        UpgradeOptions {
            check: false,
            now: true,
        },
        &runner,
        &daemon,
    );
    assert_eq!(code, 0);
    assert!(out.ends_with("0.85.0 → 0.86.0\n"), "{out}");
    assert_eq!(*daemon.events.borrow(), ["stop now=true", "start"]);
}

#[test]
fn a_full_upgrade_runs_exactly_three_commands_and_restarts_the_daemon() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = FakeDaemon {
        running: true,
        ..FakeDaemon::default()
    };
    let (code, out, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!((code, err.as_str()), (0, ""));
    let prefix = fixture.prefix();
    let calls = runner.calls.borrow();
    let scripted = calls[1].1[16].clone();
    assert!(
        scripted.starts_with(fixture.inputs.temp_dir.to_str().unwrap()),
        "{scripted}"
    );
    assert!(scripted.ends_with("/install.sh"));
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, Path::new("/usr/bin/curl"));
    assert_eq!(
        calls[0].1.last().unwrap(),
        "https://github.com/ArcfluxResearch/dudes/releases/latest"
    );
    assert_eq!(calls[1].0, Path::new("/usr/bin/curl"));
    assert_eq!(
        calls[1].1.last().unwrap(),
        "https://raw.githubusercontent.com/ArcfluxResearch/dudes/v0.86.0/install.sh"
    );
    assert_eq!(calls[2].0, Path::new("/bin/sh"));
    assert_eq!(
        calls[2].1,
        [
            scripted.as_str(),
            "--version",
            "0.86.0",
            "--prefix",
            prefix.to_str().unwrap()
        ]
    );
    assert_eq!(*daemon.events.borrow(), ["stop now=false", "start"]);
    assert!(out.contains("the daemon stopped\n"));
    assert!(out.ends_with("0.85.0 → 0.86.0\n"), "{out}");
    assert!(
        fixture.temp_is_clean(),
        "the private directory is deleted afterwards"
    );
}

#[test]
fn without_a_daemon_it_installs_and_says_how_to_start() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = FakeDaemon::default();
    let (code, out, _) = fixture.run(none(), &runner, &daemon);
    assert_eq!(code, 0);
    assert!(
        out.ends_with(&format!(
            "0.85.0 → 0.86.0\nNo daemon was running. Start it with: {}/bin/dudes start\n",
            fixture.prefix().display()
        )),
        "{out}"
    );
    assert!(daemon.events.borrow().is_empty());
    assert!(fixture.temp_is_clean());
}

#[test]
fn a_failed_installer_relays_its_message_and_leaves_the_daemon_alone() {
    let fixture = Fixture::new();
    let runner = FakeRunner {
        install_ok: false,
        ..FakeRunner::default()
    };
    let daemon = FakeDaemon {
        running: true,
        ..FakeDaemon::default()
    };
    let (code, _, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!(code, exit::OTHER);
    assert!(
        err.starts_with("dudes install: checksum mismatch\n"),
        "{err}"
    );
    assert!(
        err.contains("the installer failed. It restores the previous installation"),
        "{err}"
    );
    assert!(daemon.events.borrow().is_empty());
    assert!(fixture.temp_is_clean());
}

#[test]
fn a_failed_or_wrong_download_runs_no_installer() {
    let fixture = Fixture::new();
    let missing = FakeRunner {
        download_ok: false,
        ..FakeRunner::default()
    };
    let (code, _, err) = fixture.run(none(), &missing, &FakeDaemon::default());
    assert_eq!(code, exit::OTHER);
    assert!(
        err.contains("could not download the installer: curl: (22) 404. Nothing was changed."),
        "{err}"
    );
    assert_eq!(missing.calls.borrow().len(), 2);

    let html = FakeRunner {
        script: b"<html>Not Found</html>",
        ..FakeRunner::default()
    };
    let (code, _, err) = fixture.run(none(), &html, &FakeDaemon::default());
    assert_eq!(code, exit::OTHER);
    assert!(
        err.contains("the downloaded installer is not a shell script; nothing was changed"),
        "{err}"
    );
    assert_eq!(html.calls.borrow().len(), 2);
    assert!(fixture.temp_is_clean());
}

#[test]
fn a_restart_that_fails_prints_the_recovery_command() {
    let fixture = Fixture::new();
    let bin = format!("{}/bin/dudes", fixture.prefix().display());
    let daemon = FakeDaemon {
        running: true,
        start_fails: true,
        ..FakeDaemon::default()
    };
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            format!("dudes: 0.86.0 is installed, but the daemon did not start (it would not start). Run {bin} start\n").as_str()
        )
    );
    let daemon = FakeDaemon {
        running: true,
        stop_fails: true,
        ..FakeDaemon::default()
    };
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, exit::OTHER);
    assert_eq!(
        err,
        format!(
            "dudes: 0.86.0 is installed, but the old daemon did not stop (it would not stop). Run {bin} stop, then {bin} start\n"
        )
    );
    assert_eq!(*daemon.events.borrow(), ["stop now=false"]);
}

#[test]
fn a_daemon_that_answers_with_another_version_is_warned_about() {
    let fixture = Fixture::new();
    let daemon = FakeDaemon {
        running: true,
        started_as: Some("0.85.0"),
        ..FakeDaemon::default()
    };
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, 0);
    assert_eq!(
        err,
        "dudes: warning: the daemon that answered reports 0.85.0, not 0.86.0\n"
    );
}

#[test]
fn a_missing_curl_prints_the_one_liner_and_runs_nothing() {
    let fixture = Fixture::new();
    let runner = FakeRunner {
        curl_present: false,
        ..FakeRunner::default()
    };
    let (code, _, err) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            "dudes: /usr/bin/curl was not found. Run the installer yourself, then dudes stop and dudes start: curl -fsSL https://raw.githubusercontent.com/ArcfluxResearch/dudes/main/install.sh | sh\n"
        )
    );
    assert!(runner.calls.borrow().is_empty());
}

#[test]
fn a_github_failure_changes_nothing() {
    let fixture = Fixture::new();
    for (latest, expected) in [
        (
            Err("curl: (6) Could not resolve host".to_owned()),
            "could not read the latest release from GitHub: curl: (6) Could not resolve host",
        ),
        (
            Ok(String::new()),
            "GitHub did not name a latest release in a form this dudes understands",
        ),
        (
            Ok("https://evil.example/releases/tag/v9.9.9".to_owned()),
            "GitHub did not name a latest release in a form this dudes understands",
        ),
    ] {
        let runner = FakeRunner {
            latest,
            ..FakeRunner::default()
        };
        let (code, _, err) = fixture.run(none(), &runner, &FakeDaemon::default());
        assert_eq!(
            (code, err.as_str()),
            (exit::OTHER, format!("dudes: {expected}\n").as_str())
        );
        assert_eq!(runner.calls.borrow().len(), 1);
    }
}

#[test]
fn windows_is_refused_before_anything_runs() {
    let mut fixture = Fixture::new();
    fixture.inputs.windows = true;
    let runner = FakeRunner::default();
    let (code, _, err) = fixture.run(none(), &runner, &FakeDaemon::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            "dudes: dudes upgrade is not supported on Windows yet; see the install steps in https://github.com/ArcfluxResearch/dudes/blob/main/docs/building-from-source.md\n"
        )
    );
    assert!(runner.calls.borrow().is_empty());
}
