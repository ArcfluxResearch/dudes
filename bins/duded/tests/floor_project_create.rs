//! `POST /floor/project/create`, driven through the real Floor gateway, the real one-shot runner
//! and the real `git`: a new project is a folder made beneath the operator's parent, a repository
//! with one empty commit, and an enrolled project.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, SharedHost, TestClock};
use duded::config::load_operator;
use duded::floor_browser::Page;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_supervisor::Clock as _;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v9"
);
const CREATE: &str = "/floor/project/create";
const ATTEMPT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const OTHER_ATTEMPT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAE";

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

/// The first `git` on the test's own `PATH`.
fn system_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git")
}

type Prepare = Box<dyn FnOnce(&Path, &Path)>;

struct Floor {
    fixture: Fixture,
    running: common::Running,
    page: Arc<Page>,
    runner: Arc<OneShot>,
    config: PathBuf,
    /// Where new projects go, in its canonical spelling.
    parent: PathBuf,
    /// Holds the parent and everything else the test makes.
    root: tempfile::TempDir,
}

/// What a test chooses about the daemon it boots.
struct Setup {
    /// The text after `create_parent =`, or `None` to leave it unset. `{parent}` is the folder and
    /// `{root}` the one that holds it.
    create_parent: Option<&'static str>,
    /// Appended to the operator file.
    extra: String,
    /// The `PATH` the daemon finds `git` on.
    path: Option<OsString>,
    /// Runs before the daemon boots, with the parent and the root.
    prepare: Prepare,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            create_parent: Some("'{parent}'"),
            extra: String::new(),
            path: None,
            prepare: Box::new(|_, _| {}),
        }
    }
}

fn floor(setup: Setup) -> Floor {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().expect("root");
    let canonical = root.path().canonicalize().expect("canonical root");
    let parent = canonical.join("projects");
    std::fs::create_dir(&parent).expect("parent");
    (setup.prepare)(&parent, &canonical);
    let mut text = String::from("[floor.hire]\nenabled = true\n[floor.project]\nenabled = true\n");
    if let Some(spelled) = setup.create_parent {
        writeln!(
            text,
            "create_parent = {}",
            spelled
                .replace("{parent}", parent.to_str().expect("UTF-8"))
                .replace("{root}", canonical.to_str().expect("UTF-8"))
        )
        .expect("text");
    }
    text.push_str(&setup.extra);
    let config = fixture.state.path().join("config.toml");
    std::fs::write(&config, &text).expect("operator file");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    let loaded = load_operator(&config).expect("a valid start file");
    settings.enrollment = loaded.enrollment;
    settings.floor_create_parent = loaded.create_parent;
    settings.operator_config = Some(config.clone());
    settings.floor_hire_enabled = true;
    settings.floor_project_enabled = true;
    if let Some(path) = setup.path {
        settings.predicate_path = Some(path);
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
    let running = common::start(daemon);
    let link = common::floor_link(&settings);
    let page = Arc::new(Page::enrol_new_on(addr, &link, move || {
        clock.now().unix_millis()
    }));
    Floor {
        fixture,
        running,
        page,
        runner,
        config,
        parent,
        root,
    }
}

impl Floor {
    fn create(&self, body: &Value) -> (u16, Value) {
        let reply = self.page.post(CREATE, body);
        (reply.status(), reply.json())
    }

    fn named(&self, name: &str, attempt: &str) -> (u16, Value) {
        self.create(&json!({"name": name, "attempt_id": attempt}))
    }

    fn refused(&self, name: &str, reason: &str) {
        let (status, body) = self.named(name, ATTEMPT);
        assert_eq!(
            (status, body["reason"].as_str()),
            (409, Some(reason)),
            "{name:?} {body}"
        );
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.parent)
            .expect("parent")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// Run the system's `git` in `cwd` through the same runner the daemon uses.
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let program = system_git();
        let mut words = vec![program.into_os_string()];
        words.extend(args.iter().map(OsString::from));
        let spec = LaunchSpec {
            argv: words,
            env: vec![
                EnvVar::new("PATH", std::env::var_os("PATH").unwrap_or_default()),
                EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
                EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            ],
            cwd: cwd.to_path_buf(),
            stdin: LaunchInput::Null,
        };
        match Real(Arc::clone(&self.runner))
            .run(&spec, Duration::from_secs(30))
            .expect("git ran")
        {
            PredicateOutcome::Finished {
                exit: Some(0),
                output_tail,
                ..
            } => output_tail.trim().to_owned(),
            other => panic!("git {args:?} failed: {other:?}"),
        }
    }

    fn finish(mut self) {
        assert_eq!(
            self.running.stop(duded::ExitCause::ShutdownRpc),
            duded::ExitCause::ShutdownRpc
        );
        drop(self.root);
    }
}

/// A new project is a folder with mode 0700, a repository on `main` with one empty commit by the
/// fixed author, and an enrolled project the daemon already runs. A retry of the same attempt gets
/// the same answer; a new attempt for the same name finds the folder in the way. It writes no
/// journal record, so it grants no trust and starts no agent.
#[test]
fn a_project_created_on_the_floor_is_an_enrolled_repository() {
    let floor = floor(Setup::default());
    let before = floor.fixture.records();
    let (status, created) = floor.named("fresh-app", ATTEMPT);
    assert_eq!(status, 200, "{created}");
    let folder = floor.parent.join("fresh-app");
    assert_eq!(
        created,
        json!({
            "project_id": "fresh-app", "label": "fresh-app", "check_label": "files changed",
            "hireable": true, "reason": null, "folder": folder
        })
    );
    let mode = std::fs::metadata(&folder)
        .expect("folder")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    assert_eq!(
        floor.git(&folder, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(floor.git(&folder, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(
        floor.git(&folder, &["log", "-1", "--format=%an <%ae>|%cn <%ce>|%s"]),
        "Dudes <dudes@localhost>|Dudes <dudes@localhost>|Initialize project"
    );
    assert_eq!(
        floor.git(&folder, &["ls-tree", "-r", "HEAD"]),
        "",
        "an empty tree"
    );
    assert_eq!(floor.git(&folder, &["status", "--porcelain"]), "");
    assert!(!folder.join(".git/hooks").exists(), "a template was copied");

    let written = std::fs::read_to_string(&floor.config).expect("operator file");
    assert_eq!(written.matches("workspace_id").count(), 1, "{written}");
    assert!(
        written.contains(&format!("cwd = \"{}\"", folder.display())),
        "{written}"
    );
    let reply = floor
        .page
        .post("/floor/project/preview", &json!({"path": folder}));
    assert_eq!(
        reply.json()["already"],
        json!({"project_id": "fresh-app", "label": "fresh-app"}),
        "live with no restart"
    );
    assert_eq!(floor.fixture.records(), before, "creating wrote a record");

    let (status, again) = floor.named("fresh-app", ATTEMPT);
    assert_eq!((status, &again), (200, &created), "a retry is told again");
    assert_eq!(floor.git(&folder, &["rev-list", "--count", "HEAD"]), "1");
    let (status, body) = floor.named("fresh-app", OTHER_ATTEMPT);
    assert_eq!(
        (status, body["reason"].as_str()),
        (409, Some("exists")),
        "{body}"
    );
    let (status, _) = floor.named("other-app", ATTEMPT);
    assert_eq!(status, 409, "the attempt was used for another name");
    floor.finish();
}

/// Anything that is not one plain name is refused before a folder is made, and a path is not a
/// field the page may send.
#[test]
fn names_that_are_not_one_plain_name_are_refused() {
    let floor = floor(Setup::default());
    for name in [
        "",
        "a/b",
        "../escape",
        "..",
        ".",
        ".hidden",
        "-flag",
        "_x",
        "with space",
        "tab\t",
        "nul\u{0}byte",
        "back\\slash",
        "caf\u{e9}",
        "/absolute",
        "~",
        &"x".repeat(65),
    ] {
        floor.refused(name, "bad_name");
    }
    for body in [
        json!({"name": "ok", "path": "/tmp/elsewhere", "attempt_id": ATTEMPT}),
        json!({"name": "ok", "parent": "/tmp", "attempt_id": ATTEMPT}),
        json!({"name": "ok", "folder": "/tmp/ok", "attempt_id": ATTEMPT}),
        json!({"name": "ok"}),
        json!({"name": "ok", "attempt_id": "short"}),
        json!({"name": 1, "attempt_id": ATTEMPT}),
    ] {
        let (status, _) = floor.create(&body);
        assert_eq!(status, 400, "{body}");
    }
    assert_eq!(floor.entries(), Vec::<String>::new(), "something was made");
    assert!(!Path::new("/tmp/elsewhere").exists());
    let (status, _) = floor.named(&"x".repeat(64), ATTEMPT);
    assert_eq!(status, 200, "64 characters is the most");
    floor.finish();
}

/// Anything already at the name is in the way, a dangling link included, and nothing it points at
/// is made or changed.
#[test]
fn a_child_that_exists_in_any_form_is_refused_untouched() {
    let floor = floor(Setup {
        prepare: Box::new(|parent, root| {
            std::fs::create_dir(parent.join("dir")).expect("dir");
            std::fs::write(parent.join("dir/keep"), "mine").expect("file");
            std::fs::write(parent.join("file"), "x").expect("file");
            std::os::unix::fs::symlink(root.join("nowhere"), parent.join("dangling"))
                .expect("dangling link");
            std::os::unix::fs::symlink(parent.join("dir"), parent.join("alias")).expect("link");
        }),
        ..Setup::default()
    });
    for name in ["dir", "file", "dangling", "alias"] {
        floor.refused(name, "exists");
    }
    assert!(
        !floor.root.path().join("nowhere").exists(),
        "a link was followed"
    );
    assert_eq!(
        std::fs::read_to_string(floor.parent.join("dir/keep")).expect("kept"),
        "mine"
    );
    assert!(!floor.parent.join("dir/.git").exists());
    assert_eq!(floor.entries(), ["alias", "dangling", "dir", "file"]);
    floor.finish();
}

/// A parent that is spelled through a link, or is not there, is not used, and nothing is made
/// through the link.
#[test]
fn a_parent_spelled_through_a_link_or_missing_is_unusable() {
    for spelled in ["'{root}/link/projects'", "'{root}/missing'"] {
        let floor = floor(Setup {
            create_parent: Some(spelled),
            prepare: Box::new(|_, root| {
                std::fs::create_dir_all(root.join("real/projects")).expect("real");
                std::os::unix::fs::symlink(root.join("real"), root.join("link")).expect("link");
            }),
            ..Setup::default()
        });
        floor.refused("child", "parent_unusable");
        assert!(
            !floor.root.path().join("real/projects/child").exists(),
            "{spelled}: the link was followed"
        );
        floor.finish();
    }
}

/// With no parent set, the route refuses and nothing is made.
#[test]
fn without_a_parent_there_is_no_creation() {
    let floor = floor(Setup {
        create_parent: None,
        ..Setup::default()
    });
    floor.refused("child", "no_parent");
    assert_eq!(floor.entries(), Vec::<String>::new());
    floor.finish();
}

/// The 32nd project is the last, and the 33rd is refused before a folder exists.
#[test]
fn the_project_cap_is_checked_before_anything_is_made() {
    let mut extra = String::new();
    for index in 0..32 {
        write!(
            extra,
            "[[floor.onboarding.workspaces]]\nworkspace_id = 'p{index:02}'\nlabel = 'P{index:02}'\n\
             completion_check_label = 'Checks pass'\ncwd = 'REPO{index:02}'\ndepartment = 'engineering'\n\
             isolation = 'default'\nhop_cap = 3\npredicate_argv = ['/usr/bin/true']\npredicate_timeout_s = 10\n"
        )
        .expect("text");
    }
    let repos = tempfile::tempdir().expect("repos");
    let base = repos.path().canonicalize().expect("canonical");
    for index in 0..32 {
        std::fs::create_dir_all(base.join(format!("r{index:02}/.git"))).expect("repo");
        extra = extra.replace(
            &format!("REPO{index:02}"),
            base.join(format!("r{index:02}")).to_str().expect("UTF-8"),
        );
    }
    let floor = floor(Setup {
        extra,
        ..Setup::default()
    });
    floor.refused("one-too-many", "full");
    assert_eq!(floor.entries(), Vec::<String>::new());
    floor.finish();
}

/// While another change holds the reload lock, the creation answers busy and makes nothing.
#[test]
fn a_creation_during_a_reload_answers_busy() {
    let floor = floor(Setup::default());
    let lock = floor.running.handle.reload_lock();
    let held = lock.lock().expect("lock");
    floor.refused("child", "busy");
    assert_eq!(floor.entries(), Vec::<String>::new());
    drop(held);
    let (status, body) = floor.named("child", ATTEMPT);
    assert_eq!(status, 200, "{body}");
    floor.finish();
}

/// A failure after the file began to change keeps the folder and names it, even when the file is
/// put back exactly: here the file already holds a project this daemon has not loaded, so the
/// daemon declines the reload and the write is undone.
#[test]
fn a_failure_after_the_file_was_replaced_keeps_the_folder_and_names_it() {
    let floor = floor(Setup::default());
    let hand = floor
        .root
        .path()
        .canonicalize()
        .expect("root")
        .join("by-hand");
    std::fs::create_dir_all(hand.join(".git")).expect("repository marker");
    let mut text = std::fs::read_to_string(&floor.config).expect("operator file");
    write!(
        text,
        "[[floor.onboarding.workspaces]]\nworkspace_id = 'by-hand'\nlabel = 'By hand'\n\
         completion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\n\
         isolation = 'default'\nhop_cap = 3\npredicate_argv = ['/usr/bin/true']\npredicate_timeout_s = 10\n",
        hand.display()
    )
    .expect("text");
    std::fs::write(&floor.config, &text).expect("hand edit");

    let (status, body) = floor.named("kept", ATTEMPT);
    let folder = floor.parent.join("kept");
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["reason"], "config_changed", "{body}");
    assert_eq!(body["config"], "restored", "{body}");
    assert_eq!(
        body["left_behind"],
        json!({"path": folder, "verified": true}),
        "{body}"
    );
    assert_eq!(
        std::fs::read_to_string(&floor.config).expect("file"),
        text,
        "put back"
    );
    assert_eq!(floor.git(&folder, &["rev-list", "--count", "HEAD"]), "1");

    let (status, again) = floor.named("kept", ATTEMPT);
    assert_eq!((status, &again), (409, &body), "a retry is told the same");
    floor.finish();
}

/// A git that cannot make the first commit fails before the file is touched, so the folder this
/// attempt made is removed and nothing is left to tell the operator about.
#[test]
fn a_failure_before_the_file_is_touched_removes_the_folder() {
    let shim = tempfile::tempdir().expect("shim");
    let real = system_git();
    let script = shim.path().canonicalize().expect("canonical").join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" commit \"*) echo refused >&2; exit 1;; esac\nexec '{}' \"$@\"\n",
            real.display()
        ),
    )
    .expect("shim");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("exec");
    let floor = floor(Setup {
        path: Some(script.parent().expect("dir").as_os_str().to_owned()),
        ..Setup::default()
    });
    let before = std::fs::read_to_string(&floor.config).expect("operator file");
    let (status, body) = floor.named("doomed", ATTEMPT);
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["reason"], "create_failed", "{body}");
    assert!(body.get("left_behind").is_none(), "{body}");
    assert_eq!(floor.entries(), Vec::<String>::new(), "the folder was left");
    assert_eq!(
        std::fs::read_to_string(&floor.config).expect("file"),
        before
    );
    floor.finish();
}

/// With no git on the daemon's path the creation is refused before a folder exists.
#[test]
fn no_git_means_no_folder() {
    let empty = tempfile::tempdir().expect("empty path");
    let floor = floor(Setup {
        path: Some(
            empty
                .path()
                .canonicalize()
                .expect("canonical")
                .into_os_string(),
        ),
        ..Setup::default()
    });
    floor.refused("child", "git_unavailable");
    assert_eq!(floor.entries(), Vec::<String>::new());
    floor.finish();
}
