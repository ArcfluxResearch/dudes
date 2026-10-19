use std::cell::RefCell;

use super::*;
use crate::layout::{NOTICES, PROGRAMS};
use crate::lifecycle::fake::Fake;
use dudes_proto::record::Hex32;
use dudes_proto::rpc::floor::ExportedSession;
use dudes_types::Timestamp;

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
            "-q",
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
            "-q",
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

impl Restart for Fake {
    fn start(&self, _: &mut dyn Write) -> Result<String, String> {
        self.events.borrow_mut().push("start".to_owned());
        if self.start_fails {
            return Err("it would not start".to_owned());
        }
        Ok(self.started_as.unwrap_or("0.86.0").to_owned())
    }

    fn export_sign_ins(&self) -> Result<SessionExport, String> {
        self.events.borrow_mut().push("export".to_owned());
        if self.export_fails {
            return Err("method not found".to_owned());
        }
        Ok(SessionExport {
            run: "old-run".to_owned(),
            origin: "http://127.0.0.1:43111".to_owned(),
            sessions: (0..self.sign_ins)
                .map(|index| ExportedSession {
                    digest: Hex32::new([u8::try_from(index).expect("a few"); 32]),
                    secret: None,
                    created: Timestamp::from_unix_millis(1_000),
                    last_used: Timestamp::from_unix_millis(2_000),
                    ticket_creates: 0,
                    ticket_assigns: 0,
                    check_runs: 0,
                })
                .collect(),
        })
    }

    fn import_sign_ins(&self, exported: &SessionExport) -> Result<u32, String> {
        self.events
            .borrow_mut()
            .push(format!("import {}", exported.sessions.len()));
        self.import_keeps.ok_or_else(|| "refused".to_owned())
    }
}

type Call = (PathBuf, Vec<String>, Duration);

#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per step the fake can fail"
)]
struct FakeRunner {
    curl_present: bool,
    latest: Result<String, String>,
    script: &'static [u8],
    download_ok: bool,
    install_ok: bool,
    install_times_out: bool,
    calls: RefCell<Vec<Call>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            curl_present: true,
            latest: Ok("https://github.com/ArcfluxResearch/dudes/releases/tag/v0.86.0".to_owned()),
            script: b"#!/bin/sh\nexit 0\n",
            download_ok: true,
            install_ok: true,
            install_times_out: false,
            calls: RefCell::default(),
        }
    }
}

fn output(success: bool, stdout: &str, stderr: &str) -> Output {
    Output {
        success,
        timed_out: false,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

impl Runner for FakeRunner {
    fn run(
        &self,
        program: &Path,
        args: &[OsString],
        _: &[EnvVar],
        limit: Duration,
    ) -> std::io::Result<Output> {
        let shown: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.calls
            .borrow_mut()
            .push((program.to_path_buf(), shown.clone(), limit));
        if program == Path::new(INSTALL_SH) {
            assert!(
                Path::new(&shown[0]).is_file(),
                "the installer exists while it runs"
            );
            return Ok(if self.install_times_out {
                Output {
                    timed_out: true,
                    ..output(false, "", "")
                }
            } else if self.install_ok {
                output(true, "Installed Dudes 0.86.0\n", "")
            } else {
                output(false, "", "dudes install: checksum mismatch\n")
            });
        }
        if !self.curl_present {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
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
        std::fs::create_dir_all(base.join("state")).unwrap();
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
            state_dir: base.join("state"),
            lock_wait: Duration::from_millis(300),
            daemon_override: None,
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
        daemon: &Fake,
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

fn now() -> UpgradeOptions {
    UpgradeOptions {
        check: false,
        now: true,
    }
}

fn running() -> Fake {
    Fake {
        running: true,
        ..Fake::default()
    }
}

fn busy() -> Fake {
    Fake {
        busy: vec!["Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is waiting for an answer".to_owned()],
        ..running()
    }
}

#[test]
fn check_prints_both_versions_runs_one_command_and_never_asks_the_daemon() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = running();
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
    let (code, out, _) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "Current version: 0.86.0\nLatest release:  0.86.0\nDudes is up to date. Nothing was changed.\n"
        )
    );
    assert_eq!(runner.calls.borrow().len(), 1);
}

#[test]
fn a_newer_copy_is_never_downgraded() {
    let mut fixture = Fixture::new();
    fixture.inputs.current = "0.87.0".to_owned();
    let runner = FakeRunner::default();
    let (code, out, _) = fixture.run(none(), &runner, &Fake::default());
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
    let (code, _, _) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(code, 0);
    assert_eq!(runner.calls.borrow().len(), 3);
}

#[test]
fn a_copy_install_sh_did_not_make_is_refused_before_anything_else_even_for_check() {
    for check in [false, true] {
        let mut fixture = Fixture::new();
        let dist = fixture.prefix().join("target/dist");
        std::fs::create_dir_all(&dist).unwrap();
        fixture.inputs.executable = dist.join("dudes");
        let runner = FakeRunner::default();
        let daemon = running();
        let (code, out, err) = fixture.run(UpgradeOptions { check, now: false }, &runner, &daemon);
        assert_eq!((code, out.as_str()), (exit::REFUSED, ""));
        assert_eq!(
            err,
            format!(
                "dudes: this copy of dudes ({}) was not put there by install.sh, such as a source build, a cargo install or a link to another copy, so dudes upgrade will not replace it. If you built it from source, rebuild it: https://github.com/ArcfluxResearch/dudes/blob/main/docs/building-from-source.md\n",
                dist.join("dudes").display()
            )
        );
        assert!(runner.calls.borrow().is_empty() && daemon.events.borrow().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn a_link_to_dudes_and_a_missing_notice_are_not_installs() {
    let mut fixture = Fixture::new();
    let link = fixture.prefix().join("linked");
    std::os::unix::fs::symlink(fixture.prefix().join("bin/dudes"), &link).unwrap();
    fixture.inputs.executable = link;
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &Fake::default());
    assert_eq!(code, exit::REFUSED, "{err}");
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.prefix().join("share/dudes/LICENSE-MIT")).unwrap();
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &Fake::default());
    assert_eq!(code, exit::REFUSED, "{err}");
}

#[test]
fn busy_agents_refuse_by_name_before_any_network_and_now_means_closing_time() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = busy();
    let (code, _, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!(
        (code, err.as_str()),
        (
            exit::REFUSED,
            "dudes: refusing to upgrade: these agents are busy: Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is waiting for an answer. Wait for them to finish, pass --now to stop with closing time (every agent gets up to 6 minutes to save its work), or run dudes stop --hard first to end their turns at once. Nothing was changed.\n"
        )
    );
    assert!(runner.calls.borrow().is_empty());
    assert_eq!(*daemon.events.borrow(), ["busy"]);

    let (code, out, _) = fixture.run(now(), &runner, &daemon);
    assert_eq!(code, 0);
    assert!(out.contains("Closing time: every agent gets up to 6 minutes to save its work before the daemon stops.\n"), "{out}");
    assert!(out.ends_with("0.85.0 → 0.86.0\n"), "{out}");
    assert_eq!(
        *daemon.events.borrow(),
        ["busy", "busy", "export", "stop", "start"]
    );
}

#[test]
fn the_busy_call_comes_first_even_with_now_and_a_refusal_downloads_nothing() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = Fake {
        busy_fails: true,
        ..running()
    };
    let (code, _, err) = fixture.run(now(), &runner, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(
        err.contains(
            "could not ask the daemon which agents are busy (unauthorized); nothing was changed"
        ),
        "{err}"
    );
    assert!(runner.calls.borrow().is_empty());
    assert_eq!(*daemon.events.borrow(), ["busy"]);
}

#[test]
fn a_daemon_that_does_not_answer_but_holds_the_lock_refuses_before_downloading() {
    let fixture = Fixture::new();
    let lock = std::fs::File::create(fixture.inputs.state_dir.join(crate::env::LOCK_NAME)).unwrap();
    lock.lock().unwrap();
    let runner = FakeRunner::default();
    let (code, _, err) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("a process holds the lock in"), "{err}");
    assert!(runner.calls.borrow().is_empty());
    drop(lock);
}

#[test]
fn a_lock_that_outlives_the_stop_is_reported_with_the_recovery_command() {
    let fixture = Fixture::new();
    let lock = std::fs::File::create(fixture.inputs.state_dir.join(crate::env::LOCK_NAME)).unwrap();
    lock.lock().unwrap();
    let daemon = running();
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    let bin = format!("{}/bin/dudes", fixture.prefix().display());
    assert_eq!(code, exit::OTHER);
    assert_eq!(
        err,
        format!(
            "dudes: 0.86.0 is installed, but a process still holds the lock in {} after the daemon stopped. Run {bin} stop, then {bin} start\n",
            fixture.inputs.state_dir.display()
        )
    );
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop"]);
    drop(lock);
}

#[test]
fn a_daemon_override_that_is_not_the_installed_daemon_is_refused_up_front() {
    let mut fixture = Fixture::new();
    fixture.inputs.daemon_override = Some(PathBuf::from("/opt/other/duded"));
    let runner = FakeRunner::default();
    let (code, _, err) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(
        err.contains("DUDES_DAEMON is set to /opt/other/duded, not"),
        "{err}"
    );
    assert!(runner.calls.borrow().is_empty());
    fixture.inputs.daemon_override = Some(fixture.prefix().join("bin/duded"));
    let (code, _, _) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(code, 0);
}

#[test]
fn a_full_upgrade_runs_exactly_three_commands_with_their_limits_and_restarts_the_daemon() {
    let fixture = Fixture::new();
    let runner = FakeRunner::default();
    let daemon = running();
    let (code, out, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!((code, err.as_str()), (0, ""));
    let prefix = fixture.prefix();
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 3);
    let scripted = calls[1].1[17].clone();
    assert!(
        scripted.starts_with(fixture.inputs.temp_dir.to_str().unwrap()),
        "{scripted}"
    );
    assert!(scripted.ends_with("/install.sh"));
    assert_eq!(calls[0].0, Path::new("/usr/bin/curl"));
    assert_eq!(calls[0].1[0], "-q");
    assert_eq!(
        calls[0].1.last().unwrap(),
        "https://github.com/ArcfluxResearch/dudes/releases/latest"
    );
    assert_eq!(calls[1].0, Path::new("/usr/bin/curl"));
    assert_eq!(calls[1].1[0], "-q");
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
    assert_eq!(
        [calls[0].2, calls[1].2, calls[2].2],
        [
            Duration::from_secs(150),
            Duration::from_secs(150),
            Duration::from_secs(600)
        ]
    );
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop", "start"]);
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
    let daemon = Fake::default();
    let (code, out, _) = fixture.run(none(), &FakeRunner::default(), &daemon);
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
    let daemon = running();
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
    assert_eq!(*daemon.events.borrow(), ["busy"]);
    assert!(fixture.temp_is_clean());
}

#[test]
fn an_installer_that_runs_too_long_is_stopped_and_reported() {
    let fixture = Fixture::new();
    let runner = FakeRunner {
        install_times_out: true,
        ..FakeRunner::default()
    };
    let daemon = running();
    let (code, _, err) = fixture.run(none(), &runner, &daemon);
    assert_eq!(code, exit::OTHER);
    assert!(
        err.starts_with("dudes: the installer did not finish in 10 minutes and was stopped."),
        "{err}"
    );
    assert_eq!(*daemon.events.borrow(), ["busy"]);
}

#[test]
fn a_failed_or_wrong_download_runs_no_installer() {
    let fixture = Fixture::new();
    let missing = FakeRunner {
        download_ok: false,
        ..FakeRunner::default()
    };
    let (code, _, err) = fixture.run(none(), &missing, &Fake::default());
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
    let (code, _, err) = fixture.run(none(), &html, &Fake::default());
    assert_eq!(code, exit::OTHER);
    assert!(
        err.contains("the downloaded installer is not a shell script; nothing was changed"),
        "{err}"
    );
    assert_eq!(html.calls.borrow().len(), 2);
    assert!(fixture.temp_is_clean());
}

#[test]
fn a_relative_temporary_directory_is_refused() {
    let mut fixture = Fixture::new();
    fixture.inputs.temp_dir = PathBuf::from("relative/tmp");
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &Fake::default());
    assert_eq!(code, exit::OTHER);
    assert!(
        err.contains("the temporary directory relative/tmp is not an absolute path"),
        "{err}"
    );
}

#[test]
fn a_restart_that_fails_prints_the_recovery_command() {
    let fixture = Fixture::new();
    let bin = format!("{}/bin/dudes", fixture.prefix().display());
    let daemon = Fake {
        start_fails: true,
        ..running()
    };
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(
        (code, err.as_str()),
        (exit::OTHER, format!("dudes: 0.86.0 is installed, but the daemon did not start (it would not start). Run {bin} start\n").as_str())
    );
    let daemon = Fake {
        stop_fails: true,
        ..running()
    };
    let (code, _, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, exit::OTHER);
    assert_eq!(
        err,
        format!(
            "dudes: 0.86.0 is installed, but the old daemon did not stop (it would not stop). Run {bin} stop, then {bin} start\n"
        )
    );
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop"]);
}

#[test]
fn a_daemon_that_answers_with_another_version_is_an_error() {
    let fixture = Fixture::new();
    let bin = format!("{}/bin/dudes", fixture.prefix().display());
    let daemon = Fake {
        started_as: Some("0.85.0"),
        ..running()
    };
    let (code, out, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, exit::OTHER);
    assert_eq!(
        err,
        format!(
            "dudes: 0.86.0 is installed, but the daemon that answered reports 0.85.0. Run {bin} stop, then {bin} start\n"
        )
    );
    assert!(!out.contains("→"), "{out}");
}

#[test]
fn a_missing_curl_prints_the_one_liner() {
    let fixture = Fixture::new();
    let runner = FakeRunner {
        curl_present: false,
        ..FakeRunner::default()
    };
    let (code, _, err) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            "dudes: /usr/bin/curl was not found. Run the installer yourself, then dudes stop and dudes start: curl -fsSL https://raw.githubusercontent.com/ArcfluxResearch/dudes/main/install.sh | sh\n"
        )
    );
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
        let (code, _, err) = fixture.run(none(), &runner, &Fake::default());
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
    let (code, _, err) = fixture.run(none(), &runner, &Fake::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::REFUSED,
            "dudes: dudes upgrade is not supported on Windows yet; see the install steps in https://github.com/ArcfluxResearch/dudes/blob/main/docs/building-from-source.md\n"
        )
    );
    assert!(runner.calls.borrow().is_empty());
}

fn signed_in(sign_ins: usize, import_keeps: Option<u32>) -> Fake {
    Fake {
        sign_ins,
        import_keeps,
        ..running()
    }
}

#[test]
fn sign_ins_are_exported_before_the_stop_and_imported_after_the_start() {
    let fixture = Fixture::new();
    let daemon = signed_in(2, Some(2));
    let (code, out, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        *daemon.events.borrow(),
        ["busy", "export", "stop", "start", "import 2"]
    );
    assert!(
        out.ends_with(
            "0.85.0 → 0.86.0\nKept 2 of 2 Floor sign-ins. Reload an open Floor tab to continue.\n"
        ),
        "{out}"
    );
    assert!(
        !out.contains("0101010101") && !out.contains("43111"),
        "the upgrade prints counts, never digests or the origin: {out}"
    );
}

#[test]
fn a_stop_that_fails_imports_nothing_and_says_nothing_about_sign_ins() {
    let fixture = Fixture::new();
    let daemon = Fake {
        stop_fails: true,
        ..signed_in(2, Some(2))
    };
    let (code, out, _) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, exit::OTHER);
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop"]);
    assert!(!out.contains("sign-in"), "{out}");
}

#[test]
fn a_start_that_fails_or_answers_with_another_version_imports_nothing() {
    for (start_fails, started_as) in [(true, None), (false, Some("0.85.0"))] {
        let fixture = Fixture::new();
        let daemon = Fake {
            start_fails,
            started_as,
            ..signed_in(2, Some(2))
        };
        let (code, _, _) = fixture.run(none(), &FakeRunner::default(), &daemon);
        assert_eq!(code, exit::OTHER);
        assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop", "start"]);
    }
}

#[test]
fn an_export_or_import_that_fails_still_upgrades_and_says_to_sign_in_again() {
    let fixture = Fixture::new();
    let daemon = Fake {
        export_fails: true,
        ..signed_in(2, Some(2))
    };
    let (code, out, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop", "start"]);
    assert!(
        out.ends_with(
            "This upgrade could not keep Floor sign-ins: the running version did not hand them over (a version from before this feature cannot). If a Floor tab was open, run dudes floor open to sign in again.\n"
        ),
        "{out}"
    );

    let daemon = signed_in(2, None);
    let (code, out, err) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.ends_with("Kept 0 of 2 Floor sign-ins. Run dudes floor open to sign in again.\n"),
        "{out}"
    );

    let daemon = signed_in(3, Some(1));
    let (_, out, _) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert!(out.contains("Kept 1 of 3 Floor sign-ins."), "{out}");
}

#[test]
fn a_daemon_with_no_sign_ins_prints_nothing_about_them_and_imports_nothing() {
    let fixture = Fixture::new();
    let daemon = signed_in(0, Some(0));
    let (code, out, _) = fixture.run(none(), &FakeRunner::default(), &daemon);
    assert_eq!(code, 0);
    assert_eq!(*daemon.events.borrow(), ["busy", "export", "stop", "start"]);
    assert!(out.ends_with("0.85.0 → 0.86.0\n"), "{out}");
}
