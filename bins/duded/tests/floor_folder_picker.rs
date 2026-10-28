//! `POST /floor/project/choose`, driven through the real Floor gateway and the real one-shot runner
//! with a stand-in program in place of the system's folder dialog. No test opens a real dialog.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::ffi::OsString;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, SharedHost, TestClock};
use duded::floor_browser::Page;
use duded::folder_picker::FolderPicker;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::Clock as _;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v10"
);
const CHOOSE: &str = "/floor/project/choose";

struct Real(Arc<OneShot>);

impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|outcome| match outcome {
            OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            OneShotOutcome::Swept => PredicateOutcome::Swept,
        })
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
            .map(|outcome| match outcome {
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

struct Floor {
    running: common::Running,
    picker: Arc<FolderPicker>,
    page: Arc<Page>,
    runner: Arc<OneShot>,
}

fn floor(fixture: &Fixture, project: bool, remote: bool) -> Floor {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_project_enabled = project;
    if remote {
        settings
            .daemon_env
            .push(EnvVar::new("SSH_CONNECTION", "10.0.0.2 5000 10.0.0.1 22"));
    }
    let clock = Arc::new(TestClock::new());
    let runner = Arc::new(OneShot::new());
    let seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(Real(Arc::clone(&runner))),
        clock.clone(),
        Witness::default(),
    );
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr: SocketAddr = daemon.floor_addr().expect("the Floor listener");
    let picker = daemon.folder_picker();
    let running = common::start(daemon);
    let link = common::floor_link(&settings);
    let page = Arc::new(Page::enrol_new_on(addr, &link, move || {
        clock.now().unix_millis()
    }));
    Floor {
        running,
        picker,
        page,
        runner,
    }
}

/// A stand-in dialog: a shell script that runs `body`.
fn stand_in(dir: &Path, name: &str, body: &str) -> Vec<OsString> {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("executable");
    vec![path.into_os_string()]
}

fn choose(page: &Page) -> Value {
    let reply = page.post(CHOOSE, &json!({}));
    assert_eq!(reply.status(), 200, "{}", reply.head);
    reply.json()
}

fn finish(mut floor: Floor) {
    assert_eq!(
        floor.running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn a_chosen_path_comes_back_and_nothing_is_journaled() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let floor = floor(&fixture, true, false);
    floor.picker.use_stand_in(
        stand_in(scripts.path(), "path", "printf '/work/kim/app/\\n'"),
        Duration::from_secs(5),
    );
    assert!(floor.picker.available());
    let before = fixture.records();
    assert_eq!(choose(&floor.page), json!({ "path": "/work/kim/app/" }));
    assert_eq!(fixture.records(), before, "choosing wrote a record");
    finish(floor);
}

#[test]
fn a_dialog_that_exits_one_is_cancelled() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let floor = floor(&fixture, true, false);
    floor.picker.use_stand_in(
        stand_in(scripts.path(), "cancel", "exit 1"),
        Duration::from_secs(5),
    );
    assert_eq!(choose(&floor.page), json!({ "cancelled": true }));
    finish(floor);
}

#[test]
fn a_second_request_while_a_dialog_is_open_is_busy_and_the_first_still_answers() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let started = scripts.path().join("started");
    let release = scripts.path().join("release");
    let floor = floor(&fixture, true, false);
    let body = format!(
        ": > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nprintf '/tmp/chosen\\n'",
        started.display(),
        release.display()
    );
    floor.picker.use_stand_in(
        stand_in(scripts.path(), "wait", &body),
        Duration::from_secs(20),
    );
    let first = {
        let page = Arc::clone(&floor.page);
        std::thread::spawn(move || choose(&page))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the dialog never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(choose(&floor.page), json!({ "busy": true }));
    std::fs::write(&release, b"").expect("release");
    assert_eq!(
        first.join().expect("first"),
        json!({ "path": "/tmp/chosen" })
    );
    assert_eq!(
        choose(&floor.page),
        json!({ "path": "/tmp/chosen" }),
        "the dialog is free again"
    );
    finish(floor);
}

#[test]
fn a_remote_daemon_has_no_dialog() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let floor = floor(&fixture, true, true);
    floor.picker.use_stand_in(
        stand_in(scripts.path(), "path", "printf '/tmp/x\\n'"),
        Duration::from_secs(5),
    );
    assert!(!floor.picker.available(), "a remote login offers a dialog");
    assert_eq!(choose(&floor.page), json!({ "unavailable": true }));
    finish(floor);
}

#[test]
fn the_deadline_ends_a_dialog_nobody_answers() {
    let fixture = Fixture::new();
    let floor = floor(&fixture, true, false);
    floor.picker.use_stand_in(
        vec!["/bin/sleep".into(), "60".into()],
        Duration::from_millis(100),
    );
    assert_eq!(choose(&floor.page), json!({ "cancelled": true }));
    assert_eq!(
        floor.runner.in_flight(),
        0,
        "the dialog was reaped before the answer"
    );
    finish(floor);
}

#[test]
fn output_that_is_not_one_absolute_path_is_not_a_choice() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let floor = floor(&fixture, true, false);
    // One script, run by `/bin/sh` with the case as its argument: macOS scans each new executable
    // the first time it runs, which made a script per case the slowest thing in this file.
    let script = scripts.path().join("cases");
    std::fs::write(
        &script,
        r#"case "$1" in
words) printf 'not a path\n' ;;
relative) printf 'code/app\n' ;;
two) printf '/a\n/b\n' ;;
escape) printf '/tmp/\033[2J\n' ;;
nothing) exit 0 ;;
crash) exit 3 ;;
esac
"#,
    )
    .expect("the cases script");
    for name in ["words", "relative", "two", "escape", "nothing", "crash"] {
        let argv = vec![
            "/bin/sh".into(),
            script.clone().into_os_string(),
            name.into(),
        ];
        floor.picker.use_stand_in(argv, Duration::from_secs(5));
        assert_eq!(
            choose(&floor.page),
            json!({ "unavailable": true }),
            "{name}"
        );
    }
    finish(floor);
}

#[test]
fn the_body_is_an_empty_object_and_the_route_needs_the_project_switch() {
    let fixture = Fixture::new();
    let scripts = tempfile::tempdir().expect("scripts");
    let floor = floor(&fixture, true, false);
    floor.picker.use_stand_in(
        stand_in(scripts.path(), "path", "printf '/tmp/x\\n'"),
        Duration::from_secs(5),
    );
    for bad in ["", "[]", "{\"path\":\"/etc\"}", "null", "{} {}"] {
        let reply = floor.page.post_bytes(CHOOSE, bad.as_bytes());
        assert_eq!(reply.status(), 400, "{bad:?}");
    }
    finish(floor);

    let off_fixture = Fixture::new();
    let off = floor_off(&off_fixture);
    let reply = off.page.post(CHOOSE, &json!({}));
    assert_ne!(reply.status(), 200, "{}", reply.head);
    finish(off);
}

fn floor_off(fixture: &Fixture) -> Floor {
    floor(fixture, false, false)
}
