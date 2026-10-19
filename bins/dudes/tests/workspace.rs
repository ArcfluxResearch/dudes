//! `dudes project add|remove|reload|list`: the operator's file is edited in place,
//! byte for byte outside the one entry, and a running daemon adopts exactly the edit the command
//! made, never one it did not.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dudes::exit;
use dudes::workspace::{self, NewWorkspace, WorkspaceError};
use dudes_proto::rpc::floor::EnrollmentReloadResult;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

/// A hand-written file: comments, blank lines, other sections, one enrolled workspace.
fn original(repo_1: &Path) -> String {
    format!(
        "# Operator settings, written by hand.\n\
         [budget]\n\
         floor_usd_per_day = 100 # the floor-wide ceiling\n\
         \n\
         [budget.per_turn_floor]\n\
         usd = 0.000001\n\
         tokens = 1\n\
         \n\
         # The first workspace.\n\
         [[floor.onboarding.workspaces]]\n\
         workspace_id = \"repo_1\"\n\
         label   = \"Service\"   # aligned by hand\n\
         completion_check_label = \"Checks pass\"\n\
         cwd = \"{}\"\n\
         department = \"engineering\"\n\
         isolation = \"default\"\n\
         agent_usd_per_day = 10\n\
         goal_usd = 5\n\
         hop_cap = 3\n\
         predicate_argv = [\"/usr/bin/true\"]\n\
         predicate_timeout_s = 10\n\
         \n\
         [floor.answer]\n\
         enabled = false\n\
         # trailing note\n",
        repo_1.display()
    )
}

fn repo(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.join(".git")).expect("repository marker");
    path.canonicalize().expect("canonical repository")
}

/// The file the daemon reads for a `HOME` of `home`, by the daemon's own rule.
fn config_for(home: &Path) -> PathBuf {
    duded::config::operator_config_path(|name| (name == "HOME").then(|| home.into()))
        .expect("an absolute HOME names a file")
}

fn add_args(path: &Path, id: &str) -> Vec<OsString> {
    [
        "project",
        "add",
        path.to_str().expect("UTF-8 path"),
        "--id",
        id,
        "--completion-check-label",
        "Checks pass",
        "--department",
        "engineering",
        "--agent-usd-per-day",
        "10",
        "--goal-usd",
        "2.50",
        "--hop-cap",
        "3",
        "--timeout-s",
        "10",
        "--",
        "/usr/bin/true",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[test]
fn add_from_a_cargo_repository_enrolls_it_with_defaults() {
    let fixture = Fixture::new();
    let here = repo(fixture.state.path(), "My Service");
    std::fs::write(here.join("Cargo.toml"), "[package]\nname = 'service'\n").unwrap();
    let config = fixture.state.path().join("operator.toml");
    std::fs::write(&config, "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\ntokens = 1\n[memory]\ncondense_model_enabled = false\n").unwrap();
    let mut settings = fixture.settings();
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("DUDES_CONFIG", &config)
        .current_dir(&here);
    let added = cli.clone().args(["project", "add"]).run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    assert_eq!(
        added.stdout,
        format!(
            "Enrolled project my-service at {} with check cargo test. To change it, run `dudes project remove my-service` and add it again with your options.\n",
            here.display()
        )
    );
    let listed = cli.args(["--json", "project", "list"]).run();
    assert_eq!(listed.code(), exit::SUCCESS, "{}", listed.output());
    let listed: serde_json::Value = listed.json();
    assert_eq!(listed["running"], serde_json::json!(["my-service"]));
    let loaded = duded::config::load_operator(&config).unwrap();
    let profile = &loaded.enrollment.profiles[0];
    assert_eq!(profile.id, "my-service");
    assert_eq!(profile.cwd, here);
    assert_eq!(profile.department, "engineering");
    assert_eq!(profile.hop_cap, 5);
    assert_eq!(profile.checks[0].label, "cargo test");
    assert_eq!(
        profile.checks[0].predicate,
        dudes_proto::Predicate::Command {
            argv: vec!["cargo".to_owned(), "test".to_owned()],
            timeout_s: 600,
        }
    );
}

#[test]
fn default_enrollment_refuses_a_non_repo_and_falls_back_to_files_changed() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("operator.toml");
    let cli = Cli::new(&root.path().join("absent.sock"), root.path())
        .env("DUDES_CONFIG", &config)
        .current_dir(root.path());
    let outside = cli.clone().args(["project", "add"]).run();
    assert_eq!(outside.code(), exit::OTHER);
    assert!(
        outside.stderr.contains(
            "is not a git repository. Run `dudes project add` in a folder that holds .git."
        ),
        "{}",
        outside.output()
    );
    std::fs::write(root.path().join(".git"), "gitdir: elsewhere\n").unwrap();
    let unknown = cli.clone().args(["project", "add", "--id", "alpha"]).run();
    assert_eq!(unknown.code(), exit::SUCCESS, "{}", unknown.output());
    assert!(
        unknown.stdout.contains(
            "No test command was found here. The default check passes once the agent has edited"
        ),
        "{}",
        unknown.output()
    );
    assert!(
        unknown.stdout.contains("with check files changed."),
        "{}",
        unknown.output()
    );
    let loaded = duded::config::load_operator(&config).unwrap();
    assert_eq!(
        loaded.enrollment.profiles[0].checks[0].label,
        "files changed"
    );
    let removed = cli.clone().args(["project", "remove", "alpha"]).run();
    assert_eq!(removed.code(), exit::SUCCESS, "{}", removed.output());
    let explicit = cli.args(["project", "add", "--", "/usr/bin/true"]).run();
    assert_eq!(explicit.code(), exit::SUCCESS, "{}", explicit.output());
}

#[test]
fn default_project_ids_are_unique_and_cwd_is_canonical() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("operator.toml");
    let cli = Cli::new(&root.path().join("absent.sock"), root.path()).env("DUDES_CONFIG", &config);
    for (parent, expected) in [
        ("first", "service"),
        ("second", "service-2"),
        ("third", "service-3"),
    ] {
        let here = repo(root.path(), &format!("{parent}/Service"));
        std::fs::write(here.join("go.mod"), "module service\n").unwrap();
        let alias = root.path().join(parent).join("alias");
        std::os::unix::fs::symlink(&here, &alias).unwrap();
        let added = cli
            .clone()
            .current_dir(&alias)
            .args(["project", "add"])
            .run();
        assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
        let loaded = duded::config::load_operator(&config).unwrap();
        let last = loaded.enrollment.profiles.last().unwrap();
        assert_eq!(last.id, expected);
        assert_eq!(last.cwd, here);
        assert_eq!(last.checks[0].label, "go test ./...");
    }
}

/// The CLI's second spelling of the daemon's config-path rule answers what the daemon's does.
#[test]
fn the_cli_and_the_daemon_resolve_one_config_path() {
    use duded::config::OperatorConfigPathError as Daemon;
    use dudes::env::NoConfig;
    let cases: [&[(&str, &str)]; 9] = [
        &[
            ("DUDES_CONFIG", "/chosen/dudes.toml"),
            ("HOME", "/home/operator"),
        ],
        &[("DUDES_CONFIG", "/chosen/dudes.toml")],
        &[
            ("DUDES_CONFIG", "relative.toml"),
            ("HOME", "/home/operator"),
        ],
        &[("DUDES_CONFIG", ""), ("HOME", "/home/operator")],
        &[("HOME", "/home/operator")],
        &[("HOME", "/home/operator"), ("XDG_CONFIG_HOME", "/xdg")],
        &[("HOME", "/home/operator"), ("XDG_CONFIG_HOME", "relative")],
        &[("HOME", "relative")],
        &[],
    ];
    for pairs in cases {
        let lookup = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        assert_eq!(
            dudes::Environment::resolve(lookup).config(),
            duded::config::operator_config_path(lookup)
                .as_deref()
                .map_err(|error| match error {
                    Daemon::EmptyOverride => NoConfig::Empty,
                    Daemon::RelativeOverride => NoConfig::Relative,
                    Daemon::NoRoot => NoConfig::NoRoot,
                }),
            "{pairs:?}"
        );
    }
    assert!(
        duded::config::operator_config_path(|name| {
            (name == "HOME").then(|| OsString::from("/home/operator"))
        })
        .is_ok(),
        "control: the agreement above is not two refusals"
    );
}

/// With no daemon running, `add` writes the canonical entry and nothing else, and `remove` gives
/// back the exact original bytes: comments, alignment, other sections and trailing text.
#[test]
fn add_then_remove_round_trips_every_other_byte() {
    let home = tempfile::tempdir().expect("home");
    let (repo_1, repo_2) = (repo(home.path(), "repo_1"), repo(home.path(), "repo_2"));
    let config = config_for(home.path());
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    let before = original(&repo_1);
    std::fs::write(&config, &before).expect("the original");
    let cli = Cli::new(&home.path().join("absent.sock"), home.path()).env("HOME", home.path());

    let added = cli.clone().args(add_args(&repo_2, "repo_2")).run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    assert!(
        added.stdout.contains("no daemon is running"),
        "{}",
        added.output()
    );
    let entry = format!(
        "\n[[floor.onboarding.workspaces]]\nworkspace_id = \"repo_2\"\nlabel = \"repo_2\"\n\
         completion_check_label = \"Checks pass\"\ncwd = \"{}\"\ndepartment = \"engineering\"\n\
         isolation = \"default\"\nagent_usd_per_day = 10\ngoal_usd = 2.50\nhop_cap = 3\n\
         predicate_argv = [\"/usr/bin/true\"]\npredicate_timeout_s = 10\n",
        repo_2.display()
    );
    assert_eq!(
        std::fs::read_to_string(&config).expect("the edited file"),
        before.replacen("\n[floor.answer]", &format!("{entry}\n[floor.answer]"), 1),
        "one entry after the last one, the dollars as typed, every other byte kept"
    );
    let loaded = duded::config::load_operator(&config).expect("the daemon admits the edited file");
    assert_eq!(loaded.enrollment.profiles.len(), 2);

    let removed = cli.args(["project", "remove", "repo_2"]).run();
    assert_eq!(removed.code(), exit::SUCCESS, "{}", removed.output());
    assert_eq!(
        std::fs::read_to_string(&config).expect("the restored file"),
        before,
        "add then remove is the original, byte for byte"
    );
    assert!(repo_2.join(".git").is_dir(), "remove touches no folder");
}

#[test]
fn add_through_a_symlinked_config_keeps_the_link_and_edits_its_target() {
    let home = tempfile::tempdir().expect("home");
    let (repo_1, repo_2) = (repo(home.path(), "repo_1"), repo(home.path(), "repo_2"));
    let config = config_for(home.path());
    let target = home.path().join("dotfiles").join("config.toml");
    std::fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    std::fs::create_dir_all(target.parent().expect("target parent")).expect("dotfiles directory");
    std::fs::write(&target, original(&repo_1)).expect("target file");
    std::os::unix::fs::symlink(&target, &config).expect("config symlink");

    let added = Cli::new(&home.path().join("absent.sock"), home.path())
        .env("HOME", home.path())
        .args(add_args(&repo_2, "repo_2"))
        .run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    assert!(
        std::fs::symlink_metadata(&config)
            .expect("config symlink remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_link(&config).expect("link target"), target);
    assert!(
        std::fs::read_to_string(&target)
            .expect("edited target")
            .contains("workspace_id = \"repo_2\""),
        "the edit reaches the symlink target"
    );
}

#[test]
fn workspace_list_json_has_file_and_running_arrays() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let repo_1 = repo(&home, "repo_1");
    let config = config_for(&home);
    std::fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
    std::fs::write(&config, original(&repo_1)).expect("operator file");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config)
        .expect("valid start file")
        .enrollment;
    settings.operator_config = Some(config);
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );

    let listed = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("HOME", &home)
        .arg("--json")
        .args(["project", "list"])
        .run();
    assert_eq!(listed.code(), exit::SUCCESS, "{}", listed.output());
    assert_eq!(
        listed.stdout,
        "{\"file\":[\"repo_1\"],\"running\":[\"repo_1\"]}\n"
    );
}

/// With nothing enrolled and no daemon, `list` says so and names the file to enroll in.
#[test]
fn an_empty_workspace_list_names_the_file() {
    let home = tempfile::tempdir().expect("home");
    let runtime = tempfile::tempdir().expect("runtime");
    let config = config_for(home.path());

    let listed = Cli::new(&runtime.path().join("duded.sock"), runtime.path())
        .env("HOME", home.path())
        .args(["project", "list"])
        .run();
    assert_eq!(listed.code(), exit::SUCCESS, "{}", listed.output());
    assert_eq!(
        listed.stdout,
        format!(
            "no project is enrolled in {}; add one with `dudes project add`\n",
            config.display()
        )
    );
}

/// Each refusal is its own typed error, and the file is untouched by every one.
#[test]
fn add_refuses_each_bad_path_and_id_before_writing() {
    let home = tempfile::tempdir().expect("home");
    let repo_1 = repo(home.path(), "repo_1");
    let config = home.path().join("config.toml");
    let before = original(&repo_1);
    std::fs::write(&config, &before).expect("the original");
    let plain = home.path().join("plain");
    std::fs::create_dir_all(&plain).expect("a directory with no .git");
    let file = home.path().join("file");
    std::fs::write(&file, b"").expect("a file");
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&repo_1, &link).expect("a symlink");
    let new = |path: PathBuf, id: &str| NewWorkspace {
        path,
        id: id.to_owned(),
        label: id.to_owned(),
        completion_check_label: "Checks pass".to_owned(),
        department: "engineering".to_owned(),
        agent_usd_per_day: Some("10".to_owned()),
        goal_usd: Some("5".to_owned()),
        hop_cap: 3,
        predicate_argv: vec!["/usr/bin/true".to_owned()],
        predicate_timeout_s: 10,
        tokens_per_day: None,
        goal_tokens: None,
        may_already_pass: false,
    };
    let canonical_link = home.path().join("link").canonicalize().expect("target");
    for (arm, attempt) in [
        ("relative", new(PathBuf::from("repo_1"), "repo_2")),
        ("missing", new(home.path().join("absent"), "repo_2")),
        ("a file", new(file.clone(), "repo_2")),
        ("a symlink", new(link.clone(), "repo_2")),
        (
            "no .git",
            new(plain.canonicalize().expect("plain"), "repo_2"),
        ),
        ("a bad id", new(repo_1.clone(), "../repo")),
        ("a duplicate id", new(repo_1.clone(), "repo_1")),
    ] {
        let error = workspace::add(&config, &attempt).expect_err(arm);
        let typed = match (arm, &error) {
            ("relative", WorkspaceError::RelativePath(path)) => path == Path::new("repo_1"),
            ("missing", WorkspaceError::Missing(_))
            | ("a file", WorkspaceError::NotADirectory(_))
            | ("no .git", WorkspaceError::NotARepository(_)) => true,
            ("a symlink", WorkspaceError::NotCanonical { canonical, .. }) => {
                *canonical == canonical_link
            }
            ("a bad id", WorkspaceError::InvalidId(id)) => id == "../repo",
            ("a duplicate id", WorkspaceError::DuplicateId(id)) => id == "repo_1",
            _ => false,
        };
        assert!(typed, "{arm}: {error:?}");
        assert_eq!(
            std::fs::read_to_string(&config).expect("the file"),
            before,
            "{arm}: nothing was written"
        );
    }
    let control = workspace::add(&config, &new(repo(home.path(), "repo_2"), "repo_2"));
    assert!(control.is_ok(), "control: a canonical repository is added");
}

/// A running daemon adopts exactly `add`'s edit. An edit the operator did not make is shown,
/// declined, and the file goes back; `list` flags it and `reload --apply` adopts it on purpose.
#[test]
fn a_running_daemon_adopts_only_the_edit_the_command_made() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let (repo_1, repo_2, repo_3, stray) = (
        repo(&home, "repo_1"),
        repo(&home, "repo_2"),
        repo(&home, "repo_3"),
        repo(&home, "stray"),
    );
    let config = config_for(&home);
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    std::fs::write(&config, original(&repo_1)).expect("the original");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config)
        .expect("valid start file")
        .enrollment;
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("HOME", &home);

    let added = cli
        .clone()
        .arg("--json")
        .args(add_args(&repo_2, "repo_2"))
        .run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    let result: EnrollmentReloadResult = added.json();
    assert!(result.applied);
    assert_eq!(result.workspaces, ["repo_1", "repo_2"]);

    let entry = format!(
        "\n[[floor.onboarding.workspaces]]\nworkspace_id = \"stray\"\nlabel = \"Stray\"\n\
         completion_check_label = \"Checks pass\"\ncwd = \"{}\"\ndepartment = \"engineering\"\n\
         isolation = \"default\"\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\n\
         predicate_argv = [\"/usr/bin/true\"]\npredicate_timeout_s = 10\n",
        stray.display()
    );
    let mut hand_edited = std::fs::read_to_string(&config).expect("the file");
    hand_edited.push_str(&entry);
    std::fs::write(&config, &hand_edited).expect("an edit the operator did not make");

    let declined = cli
        .clone()
        .arg("--json")
        .args(add_args(&repo_3, "repo_3"))
        .run();
    assert_eq!(declined.code(), exit::REFUSED, "{}", declined.output());
    let result: EnrollmentReloadResult = declined.json();
    assert!(!result.applied);
    assert_eq!(
        result.added,
        ["repo_3", "stray"],
        "the real change is shown"
    );
    assert_eq!(
        std::fs::read_to_string(&config).expect("the file"),
        hand_edited,
        "the file goes back to its bytes before this add"
    );

    let listed = cli.clone().args(["project", "list"]).run();
    assert_eq!(listed.code(), exit::SUCCESS, "{}", listed.output());
    assert!(
        listed
            .stdout
            .contains("stray  in the file only: the daemon has not adopted it"),
        "{}",
        listed.stdout
    );
    assert!(listed.stdout.contains("repo_2  in the file and running"));

    let checked = cli.clone().arg("--json").args(["project", "reload"]).run();
    let check: EnrollmentReloadResult = checked.json();
    assert_eq!(check.added, ["stray"]);
    let applied = cli
        .arg("--json")
        .args(["project", "reload", "--apply", &check.sha256])
        .run();
    assert_eq!(applied.code(), exit::SUCCESS, "{}", applied.output());
    let result: EnrollmentReloadResult = applied.json();
    assert!(result.applied);
    assert_eq!(result.workspaces, ["repo_1", "repo_2", "stray"]);
}

/// The daemon applied the edit but read the file through another path, here a symlinked `HOME`:
/// the file keeps the edit the daemon now runs, and the difference is a warning, not a rollback.
#[test]
fn an_apply_through_another_path_keeps_the_edit_and_warns() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let (repo_1, repo_2) = (repo(&home, "repo_1"), repo(&home, "repo_2"));
    let alias = fixture.state.path().join("alias");
    std::os::unix::fs::symlink(&home, &alias).expect("a symlinked HOME");
    let config = config_for(&home);
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    std::fs::write(&config, original(&repo_1)).expect("the original");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config)
        .expect("valid start file")
        .enrollment;
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );

    let added = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("HOME", &alias)
        .arg("--json")
        .args(add_args(&repo_2, "repo_2"))
        .run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    let result: EnrollmentReloadResult = added.json();
    assert!(result.applied);
    assert!(
        added.stderr.contains("warning: the daemon read"),
        "{}",
        added.output()
    );
    assert!(
        std::fs::read_to_string(&config)
            .expect("the file")
            .contains("workspace_id = \"repo_2\""),
        "the file keeps the entry the daemon now runs"
    );
}

/// The answer never arrives, so nobody knows whether the daemon applied the edit: the file keeps
/// it, and the message says the outcome is unknown.
#[test]
fn a_lost_answer_keeps_the_edit_and_says_the_outcome_is_unknown() {
    use std::io::BufRead as _;

    let home = tempfile::tempdir().expect("home");
    let (repo_1, repo_2) = (repo(home.path(), "repo_1"), repo(home.path(), "repo_2"));
    let config = config_for(home.path());
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    std::fs::write(&config, original(&repo_1)).expect("the original");
    let socket = home.path().join("stub.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("a stub daemon");
    let stub = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the CLI connects");
        let mut request = String::new();
        let _ = std::io::BufReader::new(stream).read_line(&mut request);
        request
    });

    let added = Cli::new(&socket, home.path())
        .env("HOME", home.path())
        .args(add_args(&repo_2, "repo_2"))
        .run();
    let request = stub.join().expect("the stub");
    assert!(
        request.contains("floor.enrollment.reload"),
        "the CLI asked the daemon to apply: {request}"
    );
    assert_eq!(added.code(), exit::UNREACHABLE, "{}", added.output());
    assert!(added.stderr.contains("unknown"), "{}", added.output());
    assert!(
        std::fs::read_to_string(&config)
            .expect("the file")
            .contains("workspace_id = \"repo_2\""),
        "the file keeps an edit the daemon may already run"
    );
}

#[test]
fn reload_refuses_to_compare_a_daemon_of_another_version() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("daemon.sock");
    common::ping_as(&socket, "0.0.1");
    for (apply, machine) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut cli = Cli::new(&socket, home.path())
            .env("HOME", home.path())
            .args(["project", "reload"]);
        if machine {
            cli = cli.arg("--json");
        }
        if apply {
            cli = cli.args(["--apply", &"a".repeat(64)]);
        }
        let result = cli.run();
        assert_eq!(result.code(), exit::OTHER, "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert!(
            result
                .stderr
                .contains("the running daemon is 0.0.1, this CLI is"),
            "{result:?}"
        );
        assert!(result.stderr.contains("dudes stop"), "{result:?}");
        assert!(result.stderr.contains("dudes start"), "{result:?}");
        assert_eq!(result.stderr.matches("dudes stop").count(), 1);
    }
}

#[test]
fn reload_refuses_to_compare_or_apply_another_file() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "").unwrap();
    let mut settings = fixture.settings();
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    for (apply, machine) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut cli = Cli::new(&settings.socket(), &settings.runtime_dir)
            .env("HOME", fixture.state.path().join("other-home"))
            .args(["project", "reload"]);
        if machine {
            cli = cli.arg("--json");
        }
        if apply {
            cli = cli.args(["--apply", &"a".repeat(64)]);
        }
        let result = cli.run();
        assert_eq!(result.code(), exit::OTHER, "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert!(result.stderr.contains("not this file"), "{result:?}");
        assert!(result.stderr.contains("HOME"), "{result:?}");
        assert!(result.stderr.contains("retry"), "{result:?}");
        assert_eq!(
            result.stderr.contains("XDG_CONFIG_HOME"),
            !cfg!(target_os = "macos"),
            "{result:?}",
        );
        assert!(!result.stderr.contains("APPDATA"), "{result:?}");
        assert_eq!(std::fs::read(&config).unwrap(), b"");
        assert!(!config_for(&fixture.state.path().join("other-home")).exists());
    }
}

#[test]
fn reload_names_the_adapter_tables_that_need_a_restart() {
    use dudes_providers::adapter::Adapter;
    use dudes_providers::claude::ClaudeAdapter;
    use dudes_providers::claude::config::ClaudeConfig;
    use dudes_providers::codex::CodexAdapter;
    use dudes_providers::codex::config::CodexConfig;
    use dudes_providers::pi::PiAdapter;
    use dudes_providers::pi::config::PiConfig;
    use dudes_providers::registry::Adapters;

    for provider in ["codex", "pi"] {
        for (provider_running, provider_configured, needs_restart) in [
            (false, false, None),
            (false, true, Some("needs a daemon restart: providers.codex")),
            (true, false, Some("needs a daemon restart: providers.codex")),
            (true, true, Some("needs a daemon restart: providers.codex")),
        ] {
            let fixture = Fixture::new();
            let home = fixture.state.path().join("home");
            let config = config_for(&home);
            std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
            let providers = if provider_configured {
                format!("[providers.{provider}]\n")
            } else {
                String::new()
            };
            std::fs::write(
                &config,
                format!(
                    "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n\
             tokens = 1\n{providers}[memory]\ncondense_model_enabled = false\n"
                ),
            )
            .expect("the operator configuration");
            let mut settings = fixture.settings();
            settings.operator_config = Some(config);
            let mut seams = common::seams(
                &settings,
                SharedHost::new(),
                Arc::new(OpenRunner::default()),
            );
            let mut adapters: Vec<Box<dyn Adapter>> =
                vec![Box::new(ClaudeAdapter::new(ClaudeConfig::default()))];
            if provider_running {
                if provider == "codex" {
                    adapters.push(Box::new(CodexAdapter::new(CodexConfig::default())));
                } else {
                    adapters.push(Box::new(PiAdapter::new(PiConfig::default())));
                }
            }
            seams.adapters = Adapters::new(adapters);
            let _running = common::start_with(&settings, seams);
            let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("HOME", &home);

            let checked = cli.clone().args(["project", "reload"]).run();
            assert_eq!(checked.code(), exit::SUCCESS, "{}", checked.output());
            let expected = needs_restart.map(|text| text.replace("codex", provider));
            let lines: Vec<_> = checked
                .stdout
                .lines()
                .filter(|line| line.starts_with("needs a daemon restart"))
                .collect();
            assert_eq!(
                lines,
                expected.iter().map(String::as_str).collect::<Vec<_>>(),
                "{provider} running: {provider_running}\n{}",
                checked.stdout
            );
            assert_eq!(
                checked
                    .stdout
                    .matches(&format!("providers.{provider}"))
                    .count(),
                usize::from(needs_restart.is_some()),
                "{provider}: {}",
                checked.stdout,
            );
            assert!(
                !checked.stdout.contains("not compared"),
                "the daemon compares every start-time setting\n{}",
                checked.stdout
            );
            let machine: EnrollmentReloadResult =
                cli.arg("--json").args(["project", "reload"]).run().json();
            assert_eq!(
                machine.not_applied,
                if provider_configured {
                    vec![format!("providers.{provider}")]
                } else {
                    vec![]
                },
                "the daemon compares against its default empty startup configuration"
            );
        }
    }
}

#[test]
fn an_applied_reload_is_printed_when_the_restart_comparison_fails() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;

    let home = tempfile::tempdir().expect("home");
    let socket = home.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("a stand-in daemon");
    let config = config_for(home.path());
    let server = std::thread::spawn(move || {
        for result in [
            serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "socket": "daemon.sock", "lsn": 0}),
            serde_json::json!({
                "path": config, "sha256": "abc", "added": [],
                "removed": [], "changed": [], "not_applied": [], "workspaces": [],
                "applied": true, "refused": null
            }),
            serde_json::Value::Null,
        ] {
            let (stream, _) = listener.accept().expect("a CLI connection");
            let mut text = String::new();
            BufReader::new(&stream)
                .read_line(&mut text)
                .expect("request");
            let request: serde_json::Value = serde_json::from_str(&text).expect("JSON request");
            let answer = if result.is_null() {
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32603, "message": "restart comparison unavailable"
                }})
            } else {
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            };
            writeln!(&stream, "{answer}").expect("answer");
        }
    });
    let run = Cli::new(&socket, home.path())
        .env("HOME", home.path())
        .args(["project", "reload"])
        .run();
    assert_eq!(run.code(), exit::OTHER, "{}", run.output());
    assert!(run.stdout.ends_with("applied\n"), "{}", run.output());
    assert!(
        run.stderr.contains("restart comparison unavailable"),
        "{}",
        run.output()
    );
    server.join().expect("server");
}

/// `add` without `--agent-usd-per-day` or `--goal-usd` writes no ceiling key, and
/// the daemon loads a workspace with no ceiling in either unit.
#[test]
fn add_writes_a_workspace_with_no_ceiling() {
    let home = tempfile::tempdir().expect("home");
    let repo_1 = repo(home.path(), "repo_1");
    let config = config_for(home.path());
    let mut args = add_args(&repo_1, "repo_1");
    for flag in ["--agent-usd-per-day", "--goal-usd"] {
        let at = args.iter().position(|arg| arg == flag).expect("the flag");
        args.drain(at..=at + 1);
    }
    let added = Cli::new(&home.path().join("absent.sock"), home.path())
        .env("HOME", home.path())
        .args(args)
        .run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    assert_eq!(
        std::fs::read_to_string(&config).expect("the new file"),
        format!(
            "[[floor.onboarding.workspaces]]\nworkspace_id = \"repo_1\"\nlabel = \"repo_1\"\n\
             completion_check_label = \"Checks pass\"\ncwd = \"{}\"\ndepartment = \"engineering\"\n\
             isolation = \"default\"\nhop_cap = 3\npredicate_argv = [\"/usr/bin/true\"]\n\
             predicate_timeout_s = 10\n",
            repo_1.display()
        )
    );
    let loaded = duded::config::load_operator(&config).expect("the daemon admits it");
    let profile = &loaded.enrollment.profiles[0];
    assert_eq!(
        (profile.agent_budget, profile.goal_budget),
        (
            dudes_proto::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            dudes_proto::GoalBudget {
                usd: None,
                tokens: None,
            }
        )
    );
}

fn check_add_args(workspace: &str, id: &str, label: &str) -> Vec<OsString> {
    [
        "project",
        "check",
        "add",
        workspace,
        "--id",
        id,
        "--label",
        label,
        "--timeout-s",
        "10",
        "--",
        "test",
        "-f",
        "CONTRIBUTING.md",
    ]
    .map(OsString::from)
    .to_vec()
}

/// `dudes project check add` writes one row to the workspace's checks, a running daemon adopts
/// it as a change to that workspace without a restart, and `check remove` gives back every byte.
#[test]
fn check_add_then_remove_round_trips_and_a_running_daemon_adopts_each() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let repo_1 = repo(&home, "repo_1");
    let config = config_for(&home);
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    let before = original(&repo_1);
    std::fs::write(&config, &before).expect("the original");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config)
        .expect("valid start file")
        .enrollment;
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("HOME", &home);

    let added = cli
        .clone()
        .arg("--json")
        .args(check_add_args(
            "repo_1",
            "contributing",
            "CONTRIBUTING.md exists",
        ))
        .run();
    assert_eq!(added.code(), exit::SUCCESS, "{}", added.output());
    let result: EnrollmentReloadResult = added.json();
    assert!(result.applied);
    assert_eq!(result.changed.len(), 1);
    assert_eq!(
        (
            result.changed[0].id.as_str(),
            result.changed[0].fields.as_slice()
        ),
        ("repo_1", ["checks".to_owned()].as_slice())
    );
    let loaded = duded::config::load_operator(&config).expect("the daemon admits the edited file");
    let checks: Vec<_> = loaded.enrollment.profiles[0]
        .checks
        .iter()
        .map(|check| (check.id.as_str(), check.label.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            ("default", "Checks pass"),
            ("contributing", "CONTRIBUTING.md exists")
        ]
    );

    let removed = cli
        .clone()
        .arg("--json")
        .args(["project", "check", "remove", "repo_1", "contributing"])
        .run();
    assert_eq!(removed.code(), exit::SUCCESS, "{}", removed.output());
    let result: EnrollmentReloadResult = removed.json();
    assert!(result.applied);
    assert_eq!(
        std::fs::read_to_string(&config).expect("the restored file"),
        before,
        "add then remove is the original, byte for byte"
    );
}

/// `check add` refuses an unknown workspace, a bad or taken id, a bad label, and a list of checks
/// the Floor could not carry, before it writes anything.
#[test]
fn check_add_refuses_before_writing() {
    let home = tempfile::tempdir().expect("home");
    let repo_1 = repo(home.path(), "repo_1");
    let config = config_for(home.path());
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    let before = original(&repo_1);
    std::fs::write(&config, &before).expect("the original");
    let cli = Cli::new(&home.path().join("absent.sock"), home.path()).env("HOME", home.path());
    for (args, why) in [
        (
            check_add_args("repo_2", "contributing", "Exists"),
            "not enroll `repo_2`",
        ),
        (
            check_add_args("repo_1", "Contributing", "Exists"),
            "is not a check id",
        ),
        (
            check_add_args("repo_1", "default", "Exists"),
            "already has a check `default`",
        ),
        (
            check_add_args("repo_1", "contributing", "a\nb"),
            "a check label is 1 to 128 bytes",
        ),
    ] {
        let refused = cli.clone().args(args).run();
        assert_ne!(refused.code(), exit::SUCCESS, "{}", refused.output());
        assert!(refused.stderr.contains(why), "{}", refused.output());
        assert_eq!(
            std::fs::read_to_string(&config).expect("the file"),
            before,
            "{why}"
        );
    }

    let label = "x".repeat(128);
    let mut written = 0;
    let refused = loop {
        let id = format!("check-{written}");
        let attempt = cli
            .clone()
            .args(check_add_args("repo_1", &id, &label))
            .run();
        if attempt.code() != exit::SUCCESS {
            break attempt;
        }
        written += 1;
        assert!(written < 200, "the list never reached its cap");
    };
    assert!(
        refused.stderr.contains("16384 byte cap"),
        "{}",
        refused.output()
    );
    let kept = std::fs::read_to_string(&config).expect("the file");
    let loaded = duded::config::load_operator(&config).expect("the last list that fit loads");
    assert_eq!(loaded.enrollment.profiles[0].checks.len(), written + 1);
    std::fs::write(
        &config,
        format!(
            "{kept}[[floor.onboarding.workspaces.checks]]\nid = \"over\"\nlabel = \"{label}\"\nargv = [\"test\"]\ntimeout_s = 10\n"
        ),
    )
    .expect("one row more by hand");
    let Err(error) = duded::config::load_operator(&config) else {
        panic!("the daemon loaded a list over its cap");
    };
    assert!(error.to_string().contains("16384 byte cap"), "{error}");
}

fn new_workspace(path: &Path, argv: &[&str]) -> NewWorkspace {
    NewWorkspace {
        path: path.to_path_buf(),
        id: "app".to_owned(),
        label: "app".to_owned(),
        completion_check_label: "tests".to_owned(),
        department: "engineering".to_owned(),
        agent_usd_per_day: None,
        goal_usd: None,
        hop_cap: 3,
        predicate_argv: argv.iter().map(|part| (*part).to_owned()).collect(),
        predicate_timeout_s: 10,
        tokens_per_day: None,
        goal_tokens: None,
        may_already_pass: false,
    }
}

/// The CLI refuses what `duded::predicate::spec_for` refuses for a program with a separator.
#[test]
fn a_check_program_inside_the_repository_is_refused_as_the_daemon_refuses_it() {
    let root = tempfile::tempdir().expect("a root");
    let here = repo(root.path(), "app");
    std::fs::write(here.join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::create_dir(here.join("bin")).unwrap();
    std::fs::write(here.join("bin/check"), "").unwrap();
    std::os::unix::fs::symlink(here.join("test.sh"), root.path().join("outside-link")).unwrap();
    let absolute = here.join("test.sh").display().to_string();
    let linked = root.path().join("outside-link").display().to_string();
    let cases = [
        "./test.sh",
        "bin/check",
        "./bin/../test.sh",
        absolute.as_str(),
        linked.as_str(),
        "/bin/sh",
        "./missing.sh",
        "sh",
    ];
    for program in cases {
        let argv = vec![program.to_owned(), "x".to_owned()];
        let daemon =
            duded::predicate::spec_for(&argv, &here, &[], std::env::var_os("PATH").as_deref());
        let daemon_refuses_inside =
            matches!(&daemon, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied);
        let config = root.path().join("operator.toml");
        let cli = workspace::add(&config, &new_workspace(&here, &[program, "x"]));
        let cli_refuses_inside = matches!(cli, Err(WorkspaceError::CheckInsideRepository { .. }));
        assert_eq!(cli_refuses_inside, daemon_refuses_inside, "{program}");
        if cli_refuses_inside {
            assert!(!config.exists(), "{program} wrote the file");
        }
    }
}

#[test]
fn the_refusal_names_the_rule_and_a_way_out() {
    let root = tempfile::tempdir().expect("a root");
    let here = repo(root.path(), "app");
    std::fs::write(here.join("test.sh"), "").unwrap();
    let config = root.path().join("operator.toml");
    let error = workspace::add(&config, &new_workspace(&here, &["./test.sh"]))
        .expect_err("a program inside the repository");
    let text = error.to_string();
    assert!(
        text.contains("`./test.sh` is inside the project's repository"),
        "{text}"
    );
    assert!(text.contains("could change it"), "{text}");
    assert!(text.contains("installed outside the repository"), "{text}");
    assert!(text.contains("`sh ./test.sh` for a shell script"), "{text}");
    assert!(text.contains("review its changes"), "{text}");
    workspace::add(&config, &new_workspace(&here, &["sh", "test.sh"])).expect("sh test.sh is fine");
}

#[test]
fn the_example_command_quotes_a_program_with_whitespace() {
    let root = tempfile::tempdir().expect("a root");
    let here = repo(root.path(), "app");
    std::fs::write(here.join("my check.sh"), "").unwrap();
    let config = root.path().join("operator.toml");
    let error = workspace::add(&config, &new_workspace(&here, &["./my check.sh"]))
        .expect_err("a program inside the repository");
    let text = error.to_string();
    assert!(
        text.contains("`sh './my check.sh'` for a shell script"),
        "{text}"
    );
}

#[test]
fn check_add_refuses_a_program_inside_the_workspace_before_writing() {
    let root = tempfile::tempdir().expect("a root");
    let here = repo(root.path(), "app");
    std::fs::write(here.join("test.sh"), "").unwrap();
    let config = root.path().join("operator.toml");
    workspace::add(&config, &new_workspace(&here, &["sh", "test.sh"])).unwrap();
    let before = std::fs::read(&config).unwrap();
    let check = |argv: &[&str]| workspace::NewCheck {
        workspace: "app".to_owned(),
        id: "mine".to_owned(),
        label: "mine".to_owned(),
        argv: argv.iter().map(|part| (*part).to_owned()).collect(),
        timeout_s: 10,
        may_already_pass: false,
    };
    let error = workspace::add_check(&config, &check(&["./test.sh"])).expect_err("inside");
    assert!(
        matches!(error, WorkspaceError::CheckInsideRepository { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);
    workspace::add_check(&config, &check(&["sh", "test.sh"])).expect("outside");
}
