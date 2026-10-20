//! `dudes quickstart` with a setup: options from a script, and questions on a terminal.
//!
//! Each test builds an installed layout in a temporary root: a copy of `duded` beside a copy of the
//! Floor release fixture, a stand-in `claude` and `codex` on the daemon's `PATH`, a home, and a
//! repository to enroll. A stand-in records each run in `engine-runs`. Its status command exits 1
//! while `<engine>-signed-out` exists, and its login records how it ran in `<engine>-login`, then
//! exits 7. `SSH_TTY` is set on every run, so a terminal run prints the Floor link rather than
//! opening a real browser. Run `cargo build -p duded --locked` before a filtered run: the copy is
//! taken from beside the `dudes` under test.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use dudes::config::{Key, Settings};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

mod common;

use common::{Cli, run_program};

const SPAWN_MARKER: &str = "quickstart started duded pid ";
const FLOOR_QUESTION: &str = "Let the Floor act on your agents: hire, pause, answer, change goals, give tickets to agents, retire, recover, add projects and read their changes? A ticket given to an agent starts it, and agents then run and spend under your account. Anyone holding your one-time Floor link could start agents that act with your account's full reach. [Y/n]";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../duded/tests/fixtures/floor-release-v3"
);

/// An installed `duded` with a Floor, two stand-in engines, a home and a repository.
struct Layout {
    root: tempfile::TempDir,
}

impl Layout {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a root");
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).expect("a bin directory");
        let mut built = PathBuf::from(env!("CARGO_BIN_EXE_dudes"));
        built.set_file_name("duded");
        std::fs::copy(&built, bin.join("duded")).unwrap_or_else(|error| {
            panic!(
                "copying {}: {error}; run `cargo build -p duded --locked` first",
                built.display()
            )
        });
        copy_tree(Path::new(FLOOR_FIXTURE), &bin.join("floor"));
        let engines = root.path().join("engines");
        std::fs::create_dir(&engines).expect("an engines directory");
        std::fs::create_dir(root.path().join("probe-runs")).expect("probe records");
        for engine in ["claude", "codex"] {
            let path = engines.join(engine);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n\
                     [ \"$1\" = warm ] && exit 0\n\
                     root='{root}'\n\
                     echo \"{engine} $*\" >> \"$root/engine-runs\"\n\
                     printf '%s\\0' \"$(pwd -P)\" \"${{CODEX_HOME-}}\" \"$@\" > \"$root/probe-{engine}-$$\"\n\
                     mv \"$root/probe-{engine}-$$\" \"$root/probe-runs/{engine}-$$\"\n\
                     case \"$*\" in\n\
                     '--version') [ '{engine}' = codex ] && printf '%s\\n' 'codex-cli 0.160.0'; exit 0 ;;\n\
                     'auth status'|'login status') [ -e \"$root/{engine}-signed-out\" ] && exit 1; exit 0 ;;\n\
                     'auth login'|'login')\n\
                     if [ -t 0 ] && [ -t 1 ]; then held=tty; else held=no-tty; fi\n\
                     {{ echo \"$held\"; pwd -P\n\
                     echo \"${{DUDES_TOKEN-unset}} ${{DUDES_SOCK-unset}}\"; }} > \"$root/{engine}-login\"\n\
                     exit 7 ;;\n\
                     esac\n",
                    root = root.path().display()
                ),
            )
            .expect("a stand-in engine");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("a runnable stand-in");
            // macOS may pause a new program's first run to assess it; paying that here keeps the
            // daemon's three-second wait for a status check about the check.
            let warm = run_program(&path, &[std::ffi::OsString::from("warm")]);
            assert_eq!(warm.code(), 0, "{warm:?}");
        }
        std::fs::create_dir_all(root.path().join("repo/.git")).expect("a repository");
        std::fs::create_dir(root.path().join("home")).expect("a home");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn cli(&self) -> Cli {
        Cli::bare()
            .env("HOME", self.path("home"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("engines").display()),
            )
            .env("DUDES_DAEMON", self.path("bin/duded"))
            .env("DUDES_STATE_DIR", self.path("state"))
            .env("DUDES_RUNTIME_DIR", self.path("run"))
            .env("SSH_TTY", "/dev/ttys-test")
            .current_dir(&self.path("repo"))
    }

    fn sign_out(&self, engine: &str) {
        std::fs::write(self.path(&format!("{engine}-signed-out")), "").expect("sign out");
    }

    fn config(&self) -> PathBuf {
        let home = self.path("home").into_os_string();
        dudes::Environment::resolve(|name| (name == "HOME").then(|| home.clone()))
            .config()
            .expect("an absolute home names the operator file")
            .to_path_buf()
    }

    fn socket(&self) -> PathBuf {
        self.path("run/duded.sock")
    }

    fn repo(&self) -> PathBuf {
        self.path("repo").canonicalize().expect("the repository")
    }

    fn daemons(&self) -> Vec<i32> {
        std::fs::read_to_string(self.path("run/duded.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(SPAWN_MARKER)?.parse().ok())
            .collect()
    }

    fn engine_runs(&self) -> Option<String> {
        std::fs::read_to_string(self.path("engine-runs")).ok()
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        for pid in self.daemons() {
            let pid = Pid::from_raw(pid);
            let _ = kill(pid, Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(10);
            while kill(pid, None).is_ok() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = kill(pid, Signal::SIGKILL);
        }
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("a destination directory");
    for entry in std::fs::read_dir(from).expect("the Floor fixture") {
        let entry = entry.expect("a fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("a fixture file");
        }
    }
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// What a set-up quickstart prints after the daemon starts, in order: the ping, each engine with
/// its sign-in, the hire, and last the Floor. A terminal run gets the Floor link, printed, after
/// everything else so it stays on screen; a script is told the command and gets no live link.
fn assert_set_up(run_output: &str, layout: &Layout, terminal: bool) {
    let engines = layout.path("engines");
    let floor: &[&str] = if terminal {
        &[
            "http://127.0.0.1:",
            "#bootstrap=",
            "Paste this link into any browser within 60 seconds.",
        ]
    } else {
        &["Open Dudes with: dudes open"]
    };
    let expected: Vec<String> = [
        format!("duded {VERSION} on {}", layout.socket().display()),
        "Engines the daemon turned on:".to_owned(),
        format!(
            "  claude: found at {}, signed in",
            engines.join("claude").display()
        ),
        format!(
            "  codex: found at {}, signed in",
            engines.join("codex").display()
        ),
    ]
    .into_iter()
    .chain(["hired ".to_owned()])
    .chain(floor.iter().map(|line| (*line).to_owned()))
    .collect();
    if !terminal {
        assert!(
            !run_output.contains("#bootstrap="),
            "a script printed a live Floor link:\n{run_output}"
        );
    }
    let mut rest = run_output;
    for line in &expected {
        let at = rest
            .find(line.as_str())
            .unwrap_or_else(|| panic!("`{line}` is missing or out of order in:\n{run_output}"));
        rest = &rest[at + line.len()..];
    }
    if terminal {
        assert!(
            rest.trim().is_empty() || rest.trim_start().starts_with("It works once"),
            "something printed after the Floor link:\n{run_output}"
        );
    }
    assert_only_status_checks(layout);
}

/// The daemon ran each engine's status command, and nothing else of any engine ran.
fn assert_only_status_checks(layout: &Layout) {
    let runs = layout.engine_runs().unwrap_or_default();
    assert!(
        runs.lines()
            .all(|line| line == "claude auth status" || line == "codex login status"),
        "an engine ran something other than its status command:\n{runs}"
    );
}

/// The Floor switches a new install turns on. The terminal is not among them.
const NEW_INSTALL_ON: [Key; 11] = [
    Key::Answer,
    Key::Hire,
    Key::Control,
    Key::Goal,
    Key::Retire,
    Key::Renew,
    Key::Recovery,
    Key::Board,
    Key::Project,
    Key::Git,
    Key::Diff,
];

/// The file both setups write: Codex, the Floor switches in `floor_on`, no budget, and the workspace
/// with no ceiling.
fn assert_written(layout: &Layout, floor_on: &[Key], label: &str) {
    let config = layout.config();
    let file = Settings::read(&config).expect("the operator file");
    let shown: Vec<(Key, String)> = Key::ALL
        .into_iter()
        .map(|key| (key, file.shown(key)))
        .collect();
    let switch = |key| if floor_on.contains(&key) { "on" } else { "off" };
    let expected = [
        (Key::Codex, "on"),
        (Key::CodexStreaming, "off"),
        (Key::Pi, "off"),
        (Key::Answer, switch(Key::Answer)),
        (Key::Hire, switch(Key::Hire)),
        (Key::Control, switch(Key::Control)),
        (Key::Recovery, switch(Key::Recovery)),
        (Key::Goal, switch(Key::Goal)),
        (Key::Renew, switch(Key::Renew)),
        (Key::Retire, switch(Key::Retire)),
        (Key::Board, switch(Key::Board)),
        (Key::Terminal, switch(Key::Terminal)),
        (Key::Git, switch(Key::Git)),
        (Key::Diff, switch(Key::Diff)),
        (Key::Project, switch(Key::Project)),
        (Key::FloorUsdPerDay, "none"),
        (Key::PerTurnUsd, "0.25"),
        (Key::PerTurnTokens, "20000"),
    ]
    .map(|(key, value)| (key, value.to_owned()));
    assert_eq!(shown, expected);
    let text = std::fs::read_to_string(&config).expect("the operator file's text");
    for line in [
        "workspace_id = \"repo\"".to_owned(),
        "label = \"repo\"".to_owned(),
        format!("completion_check_label = \"{label}\""),
        format!("cwd = \"{}\"", layout.repo().display()),
        "department = \"engineering\"".to_owned(),
        "hop_cap = 5".to_owned(),
        "predicate_argv = [\"/usr/bin/true\"]".to_owned(),
        "predicate_timeout_s = 600".to_owned(),
    ] {
        assert!(text.contains(&line), "`{line}` is not in:\n{text}");
    }
    assert!(
        ["[budget", "agent_usd_per_day", "goal_usd"]
            .iter()
            .all(|key| !text.contains(key)),
        "no ceiling was written:\n{text}"
    );
}

/// A script answers every step with options, and nothing is asked.
#[test]
fn a_script_sets_up_enrolls_and_hires_without_a_question() {
    let layout = Layout::new();
    let run = layout
        .cli()
        .args([
            "quickstart",
            "--set",
            "providers.codex",
            "on",
            "--set",
            "floor.answer",
            "on",
            "--set",
            "floor.hire",
            "on",
            "--enroll",
            "--check-label",
            "README says hello",
            "--hire",
            "Kim",
            "--provider",
            "codex",
            "--",
            "/usr/bin/true",
        ])
        .run();

    assert_eq!(run.code(), dudes::exit::SUCCESS, "{run:?}");
    assert_eq!(run.stderr, "", "{run:?}");
    assert!(
        !run.stdout.contains('?'),
        "a script is asked nothing: {run:?}"
    );
    assert_set_up(&run.stdout, &layout, false);
    assert_written(&layout, &[Key::Answer, Key::Hire], "README says hello");
}

/// A script whose completion check the daemon cannot run still hires and enrolls, and the refused
/// goal names the agent left without one and the enrolled workspace that holds the same check.
#[test]
fn a_script_whose_check_cannot_run_hires_without_a_goal() {
    let layout = Layout::new();
    let run = layout
        .cli()
        .args([
            "quickstart",
            "--enroll",
            "--check-label",
            "q.txt exists",
            "--hire",
            "Kim",
            "--goal",
            "write q.txt",
            "--",
            "test -f q.txt",
        ])
        .run();

    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    let agent = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("hired "))
        .unwrap_or_else(|| panic!("the hire is printed: {run:?}"));
    assert_eq!(
        run.stderr,
        format!(
            "dudes: goal.set: the completion check cannot run: `test -f q.txt` is not on the \
             daemon's PATH; give each word as its own argument, not one quoted string: test -f \
             q.txt; Kim is already hired, so running quickstart again would hire a second agent; \
             give Kim a goal with: dudes goal --agent {agent} --text <text> --hop-cap 5 -- \
             <program> [args...]; project repo is enrolled with the same check, so the Floor \
             offers no hire there until you change its predicate_argv in {} and run dudes \
             project reload, then dudes project reload --apply <sha256> (-32602)\n",
            layout.config().display()
        )
    );
}

/// The first goal of a workspace quickstart enrolls in the same run records the workspace check's
/// name and label. A later run that finds the workspace enrolled hires with a goal and names no
/// check, because the check it asks for is not the enrolled one.
#[test]
fn a_script_that_enrolls_names_the_first_goals_check() {
    let layout = Layout::new();
    let quickstart = |name: &str| {
        let run = layout
            .cli()
            .args([
                "quickstart",
                "--set",
                "floor.hire",
                "on",
                "--enroll",
                "--check-label",
                " README says hello ",
                "--hire",
                name,
                "--goal",
                "Say hello in the README",
                "--",
                "/usr/bin/false",
            ])
            .run();
        assert_eq!(run.code(), dudes::exit::SUCCESS, "{run:?}");
        run
    };
    quickstart("Kim");
    let again = quickstart("Leo");
    assert!(
        again.stdout.contains("its entry stays as it is"),
        "{again:?}"
    );

    let journal = layout.path("state").join("journal");
    let checks: Vec<Option<(String, String)>> = dudes_journal::Reader::new(&journal)
        .iter_from(dudes_types::Lsn::ZERO)
        .expect("the journal")
        .filter_map(|item| match item.expect("an entry").1.record {
            dudes_proto::Record::GoalSet { goal, .. } => {
                Some(goal.completion_check.map(|check| (check.id, check.label)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        checks,
        [
            Some(("default".to_owned(), "README says hello".to_owned())),
            None
        ]
    );
}

/// A script whose settings break a start rule is refused whole: the rule is named, and nothing
/// is written or started.
#[test]
fn a_script_the_daemon_would_refuse_is_refused_whole() {
    let layout = Layout::new();
    let run = layout
        .cli()
        .args([
            "quickstart",
            "--set",
            "floor.goal",
            "on",
            "--enroll",
            "--",
            "/usr/bin/true",
        ])
        .run();

    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert_eq!(
        run.stderr,
        "dudes: the daemon would refuse this configuration when it starts, so nothing was \
         written: floor.goal on needs floor.control on\n"
    );
    assert!(!layout.config().exists(), "nothing was written");
    assert!(!layout.path("run").exists(), "nothing was started");
}

/// A script restarts a daemon that runs other settings only with `--restart`. Without it the
/// file is written, the differences are named and nothing is stopped. A daemon that runs the
/// file's settings is kept either way. An engine table written by `dudes config` alone counts as
/// other settings, which is the arm `floor.enrollment.reload` does not report.
#[test]
fn a_script_restarts_a_daemon_with_other_settings_only_when_asked() {
    let layout = Layout::new();
    let plain = layout.cli().arg("quickstart").run();
    assert_eq!(plain.code(), dudes::exit::SUCCESS, "{plain:?}");
    assert_eq!(
        plain.stdout.lines().count(),
        3,
        "without a terminal or options quickstart prints the ping and project hint: {plain:?}"
    );
    let first = layout.daemons();
    assert_eq!(first.len(), 1, "{plain:?}");

    let unasked = layout
        .cli()
        .args(["quickstart", "--set", "floor.answer", "on"])
        .run();
    assert_eq!(unasked.code(), dudes::exit::OTHER, "{unasked:?}");
    assert_eq!(
        unasked.stderr,
        format!(
            "dudes: wrote {}, but the running daemon reads other settings (memory) and \
             reads the file only when it starts. Nothing was stopped. To apply them, restart it, \
             which ends any turn it is running: dudes quickstart --restart\n",
            layout.config().display()
        )
    );
    assert_eq!(layout.daemons(), first, "nothing was stopped or started");
    assert!(alive(first[0]), "the running daemon was not asked to stop");

    let changed = layout.cli().args(["quickstart", "--restart"]).run();
    assert_eq!(changed.code(), dudes::exit::SUCCESS, "{changed:?}");
    assert!(
        changed.stdout.contains("Restarting the daemon so it reads"),
        "{changed:?}"
    );
    let second = layout.daemons();
    assert_eq!(second.len(), 2, "{changed:?}");
    assert!(!alive(first[0]), "the daemon with the old settings stopped");

    let same = layout.cli().args(["quickstart", "--yes"]).run();
    assert_eq!(same.code(), dudes::exit::SUCCESS, "{same:?}");
    assert!(!same.stdout.contains("Restarting"), "{same:?}");
    assert_eq!(
        layout.daemons(),
        second,
        "the daemon with these settings was kept"
    );

    let edited = layout
        .cli()
        .args(["config", "set", "providers.codex", "on"])
        .run();
    assert_eq!(edited.code(), dudes::exit::SUCCESS, "{edited:?}");
    let stale = layout.cli().args(["quickstart", "--yes"]).run();
    assert_eq!(stale.code(), dudes::exit::OTHER, "{stale:?}");
    assert!(
        stale.stderr.contains("kept ") && stale.stderr.contains("(providers.codex)"),
        "{stale:?}"
    );
    assert_eq!(layout.daemons(), second, "{stale:?}");
    let restarted = layout.cli().args(["quickstart", "--restart"]).run();
    assert_eq!(restarted.code(), dudes::exit::SUCCESS, "{restarted:?}");
    assert!(restarted.stdout.contains("Restarting"), "{restarted:?}");
    assert_eq!(layout.daemons().len(), 3, "{restarted:?}");
}

/// A daemon started on another `HOME` reads another operator file, so its comparison says nothing
/// about this one: a script counts that as a difference, stops nothing unless asked, and with
/// `--restart` starts a daemon that reads this file.
#[test]
fn a_script_restarts_a_daemon_that_reads_another_file_only_when_asked() {
    let layout = Layout::new();
    let started = layout.cli().arg("quickstart").run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let first = layout.daemons();
    assert_eq!(first.len(), 1, "{started:?}");
    let other = layout.path("other-home");
    std::fs::create_dir(&other).expect("another home");
    let other_config = {
        let home = other.clone().into_os_string();
        dudes::Environment::resolve(|name| (name == "HOME").then(|| home.clone()))
            .config()
            .expect("an absolute home names the operator file")
            .to_path_buf()
    };

    let unasked = layout
        .cli()
        .env("HOME", &other)
        .args(["quickstart", "--set", "floor.answer", "on"])
        .run();
    assert_eq!(unasked.code(), dudes::exit::OTHER, "{unasked:?}");
    assert_eq!(
        unasked.stderr,
        format!(
            "dudes: wrote {}, but the running daemon reads other settings (the running daemon \
             reads {}, not this file) and reads the file only when it starts. Nothing was \
             stopped. To apply them, restart it, which ends any turn it is running: dudes \
             quickstart --restart\n",
            other_config.display(),
            layout.config().display()
        )
    );
    assert_eq!(layout.daemons(), first, "nothing was stopped or started");

    let restarted = layout
        .cli()
        .env("HOME", &other)
        .args(["quickstart", "--restart"])
        .run();
    assert_eq!(restarted.code(), dudes::exit::SUCCESS, "{restarted:?}");
    assert!(
        restarted.stdout.contains(&format!(
            "Restarting the daemon so it reads {}.",
            other_config.display()
        )),
        "{restarted:?}"
    );
    assert_eq!(layout.daemons().len(), 2, "{restarted:?}");
    assert!(!alive(first[0]), "the daemon on the other file stopped");
}

/// A daemon of another version may compare settings this CLI does not know, so a script counts
/// the version as a difference and stops nothing unless asked.
#[test]
fn a_script_counts_a_daemon_of_another_version_as_a_difference() {
    let layout = Layout::new();
    std::fs::create_dir(layout.path("run")).expect("a runtime directory");
    std::fs::set_permissions(layout.path("run"), std::fs::Permissions::from_mode(0o700))
        .expect("an owner-only runtime directory");
    common::ping_as(&layout.socket(), "0.0.1");

    let unasked = layout
        .cli()
        .args(["quickstart", "--set", "floor.answer", "on"])
        .run();
    assert_eq!(unasked.code(), dudes::exit::OTHER, "{unasked:?}");
    assert_eq!(
        unasked.stderr,
        format!(
            "dudes: wrote {}, but the running daemon reads other settings (the running daemon is \
             0.0.1, this CLI is {VERSION}) and reads the file only when it starts. Nothing was \
             stopped. To apply them, restart it, which ends any turn it is running: dudes \
             quickstart --restart\n",
            layout.config().display()
        )
    );
    assert!(layout.daemons().is_empty(), "nothing was started");
}

#[test]
fn codex_streaming_changes_restart_once_without_disabling_codex() {
    let layout = Layout::new();
    let started = layout
        .cli()
        .args(["quickstart", "--set", "providers.codex", "on"])
        .run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let mut managed_home = None;
    for setting in ["on", "off"] {
        let before = layout.daemons();
        let changed = layout
            .cli()
            .args(["config", "set", "providers.codex.streaming", setting])
            .run();
        assert_eq!(changed.code(), dudes::exit::SUCCESS, "{changed:?}");
        let stale = layout.cli().args(["quickstart", "--yes"]).run();
        assert_eq!(stale.code(), dudes::exit::OTHER, "{stale:?}");
        assert!(stale.stderr.contains("providers.codex"), "{stale:?}");
        assert_eq!(layout.daemons(), before);
        let restarted = layout.cli().args(["quickstart", "--restart"]).run();
        assert_eq!(restarted.code(), dudes::exit::SUCCESS, "{restarted:?}");
        assert_eq!(layout.daemons().len(), before.len() + 1);
        let after = layout.daemons();
        let same = layout.cli().args(["quickstart", "--restart"]).run();
        assert_eq!(same.code(), dudes::exit::SUCCESS, "{same:?}");
        assert!(!same.stdout.contains("Restarting"), "{same:?}");
        assert_eq!(layout.daemons(), after);
        assert!(
            Settings::read(&layout.config())
                .unwrap()
                .satisfies(Key::Codex)
        );
        if setting == "on" {
            let doctor = layout.cli().args(["--json", "doctor"]).run();
            assert_eq!(doctor.code(), dudes::exit::SUCCESS, "{doctor:?}");
            let doctor: dudes_proto::rpc::system::DoctorResult = doctor.json();
            let row = doctor
                .providers
                .iter()
                .find(|row| row.provider == dudes_types::Provider::Codex)
                .unwrap();
            let Some(dudes_proto::rpc::system::ProviderLoginHome::Ready { path }) = &row.login_home
            else {
                panic!("the configured streaming profile was not admitted");
            };
            managed_home = Some(path.clone());
        }
    }
    assert_streaming_probe_calls(&layout, &managed_home.unwrap());
}

fn assert_streaming_probe_calls(layout: &Layout, home: &Path) {
    let mut version_seen = false;
    let mut managed_status_seen = false;
    for entry in std::fs::read_dir(layout.path("probe-runs")).unwrap() {
        let entry = entry.unwrap();
        let record = std::fs::read_to_string(entry.path()).unwrap();
        assert!(record.ends_with('\0'));
        let fields: Vec<_> = record.split_terminator('\0').collect();
        assert!(fields.len() >= 3);
        let arguments = &fields[2..];
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if name.starts_with("claude-") {
            assert_eq!(arguments, ["auth", "status"]);
        } else {
            assert!(name.starts_with("codex-"));
            if arguments == ["login", "status"] {
                assert_ne!(
                    fields[1],
                    home.to_str().unwrap(),
                    "managed home used without its guards"
                );
                continue;
            }
            assert_eq!(
                &fields[..2],
                [home.to_str().unwrap(), home.to_str().unwrap()]
            );
            if arguments == ["--version"] {
                version_seen = true;
            } else {
                assert_eq!(arguments.last(), Some(&"status"));
                assert_managed_login_arguments(&arguments[..arguments.len() - 1], home);
                managed_status_seen = true;
            }
        }
    }
    assert!(version_seen, "the streaming version check did not run");
    assert!(
        managed_status_seen,
        "the protected sign-in status check did not run"
    );
}

/// A value the daemon would refuse at start stops nothing and writes nothing: the CLI refuses the
/// options it can check, and the running daemon reads the new file before anything is stopped.
/// Either way the old daemon keeps running and the file is as it was.
#[test]
fn a_file_the_daemon_would_refuse_stops_nothing_and_is_put_back() {
    let layout = Layout::new();
    let started = layout
        .cli()
        .args(["quickstart", "--set", "floor.answer", "on"])
        .run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let daemons = layout.daemons();
    let config = layout.config();

    let zero = layout
        .cli()
        .args([
            "quickstart",
            "--restart",
            "--enroll",
            "--hop-cap",
            "0",
            "--",
            "/usr/bin/true",
        ])
        .run();
    assert_eq!(zero.code(), dudes::exit::USAGE, "{zero:?}");
    assert!(
        zero.stderr
            .starts_with("dudes: `--hop-cap` does not take `0`: a whole number above zero"),
        "{zero:?}"
    );

    let broken = format!(
        "{}[floor.bogus]\nenabled = true\n",
        std::fs::read_to_string(&config).expect("the operator file")
    );
    std::fs::write(&config, &broken).expect("break the file by hand");
    let refused = layout
        .cli()
        .args(["quickstart", "--restart", "--set", "floor.terminal", "on"])
        .run();
    assert_eq!(refused.code(), dudes::exit::OTHER, "{refused:?}");
    assert!(
        refused
            .stderr
            .contains(&format!("; {} was put back as it was", config.display())),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).expect("the operator file"),
        broken,
        "the file is as it was before quickstart"
    );
    assert_eq!(layout.daemons(), daemons, "nothing was started");
    assert!(alive(daemons[0]), "the running daemon was not stopped");
}

/// A pseudo-terminal's leader side: everything the program wrote, and a way to type.
struct Screen {
    seen: Vec<u8>,
    cursor: usize,
    chunks: Receiver<Vec<u8>>,
    keyboard: File,
}

impl Screen {
    fn new(leader: OwnedFd) -> Self {
        let keyboard = File::from(leader.try_clone().expect("share the leader"));
        let mut reader = File::from(leader);
        let (sender, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(read @ 1..) = reader.read(&mut buffer) {
                if sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            seen: Vec::new(),
            cursor: 0,
            chunks,
            keyboard,
        }
    }

    /// Wait until `text` appears after the last thing waited for.
    fn expect(&mut self, text: &str) {
        self.expect_any(&[text]);
    }

    fn expect_any(&mut self, texts: &[&str]) -> usize {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for (index, text) in texts.iter().enumerate() {
                if let Some(at) = self.seen[self.cursor..]
                    .windows(text.len())
                    .position(|window| window == text.as_bytes())
                {
                    self.cursor += at + text.len();
                    return index;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(error) => panic!(
                    "{texts:?} did not appear ({error}); the terminal shows:\n{}",
                    String::from_utf8_lossy(&self.seen)
                ),
            }
        }
    }

    fn answer(&mut self, line: &str) {
        self.keyboard
            .write_all(format!("{line}\n").as_bytes())
            .expect("type on the terminal");
    }

    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.seen).replace("\r\n", "\n")
    }
}

#[test]
fn quickstart_outside_a_repo_asks_for_a_folder_and_can_skip_it() {
    let layout = Layout::new();
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = layout
        .cli()
        .current_dir(layout.root.path())
        .arg("quickstart")
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(FLOOR_QUESTION);
    screen.answer("y");
    let found = screen.expect_any(&["Project folder (Enter to skip):", "Hire a first agent?"]);
    if found != 0 {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(found, 0, "{}", screen.shown());
    screen.answer("");
    screen.expect("Tickets and Floor hiring need a project.");
    screen.expect("and go ahead? [Y/n]");
    screen.answer("y");
    screen.expect("No project yet. In your project's folder run `dudes project add`.");
    assert!(child.wait().unwrap().success(), "{}", screen.shown());
    assert_eq!(
        screen.shown().matches("[Y/n]").count() + screen.shown().matches("Enter to skip):").count(),
        3
    );
    let listed = layout.cli().args(["--json", "project", "list"]).run();
    assert_eq!(
        listed.json::<serde_json::Value>()["running"],
        serde_json::json!([])
    );
}

#[test]
fn quickstart_outside_a_repo_enrolls_the_entered_folder() {
    let layout = Layout::new();
    std::fs::write(
        layout.path("repo/Cargo.toml"),
        "[package]\nname = 'service'\n",
    )
    .unwrap();
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = layout
        .cli()
        .current_dir(layout.root.path())
        .arg("quickstart")
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(FLOOR_QUESTION);
    screen.answer("y");
    let found = screen.expect_any(&["Project folder (Enter to skip):", "Hire a first agent?"]);
    if found != 0 {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(found, 0, "{}", screen.shown());
    screen.answer(layout.path("repo").to_str().unwrap());
    screen.expect("First agent engine (claude, codex; Enter to skip):");
    screen.answer("");
    screen.expect("and go ahead? [Y/n]");
    screen.answer("y");
    screen.expect("Engines the daemon turned on:");
    assert!(child.wait().unwrap().success(), "{}", screen.shown());
    let loaded = duded::config::load_operator(&layout.config()).unwrap();
    assert_eq!(loaded.enrollment.profiles[0].cwd, layout.repo());
    assert_eq!(loaded.enrollment.profiles[0].checks[0].label, "cargo test");
    assert!(
        loaded.enrollment.profiles[0].checks[0].may_already_pass,
        "a project's own tests usually pass before any work, and the Floor's Hire must allow that"
    );
    assert!(
        !screen.shown().contains("No project yet."),
        "{}",
        screen.shown()
    );
}

/// A repository with no test command still enrolls: the default check passes once files changed,
/// and the guide says so before it enrolls.
#[test]
fn quickstart_with_no_test_command_enrolls_the_files_changed_check() {
    let layout = Layout::new();
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = layout
        .cli()
        .current_dir(layout.root.path())
        .arg("quickstart")
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(FLOOR_QUESTION);
    screen.answer("y");
    screen.expect("Project folder (Enter to skip):");
    screen.answer(layout.path("repo").to_str().unwrap());
    screen.expect("No test command was found here.");
    screen.expect("First agent engine (claude, codex; Enter to skip):");
    screen.answer("");
    screen.expect("and go ahead? [Y/n]");
    screen.answer("y");
    screen.expect("Engines the daemon turned on:");
    assert!(child.wait().unwrap().success(), "{}", screen.shown());
    let loaded = duded::config::load_operator(&layout.config()).unwrap();
    let check = &loaded.enrollment.profiles[0].checks[0];
    assert_eq!(check.label, "files changed");
    assert!(check.may_already_pass);
    let dudes_proto::goal::Predicate::Command { argv, .. } = &check.predicate else {
        panic!("a command check: {:?}", check.predicate);
    };
    assert_eq!(*argv, dudes::setup::changed_check());
}

#[test]
fn start_floor_and_quickstart_show_a_missing_project_hint_only_to_people() {
    let layout = Layout::new();
    for command in [vec!["start"], vec!["floor", "open"], vec!["quickstart"]] {
        let human = layout.cli().args(&command).run();
        assert_eq!(human.code(), dudes::exit::SUCCESS, "{human:?}");
        assert!(
            human
                .stdout
                .contains("No project yet. In your project's folder run `dudes project add`.\n"),
            "{human:?}"
        );
        let json = layout.cli().arg("--json").args(&command).run();
        assert_eq!(json.code(), dudes::exit::SUCCESS, "{json:?}");
        assert!(!json.output().contains("No project yet."), "{json:?}");
        let _: serde_json::Value = json.json();
    }
    let added = layout
        .cli()
        .args(["project", "add", "--", "/usr/bin/true"])
        .run();
    assert_eq!(added.code(), dudes::exit::SUCCESS, "{added:?}");
    for command in [vec!["start"], vec!["floor", "open"], vec!["quickstart"]] {
        let human = layout.cli().args(&command).run();
        assert_eq!(human.code(), dudes::exit::SUCCESS, "{human:?}");
        assert!(!human.output().contains("No project yet."), "{human:?}");
    }
}

#[test]
fn billed_and_unbilled_guides_ask_at_most_five_questions_and_hire_idle() {
    for (billed, multiple) in [(false, false), (false, true), (true, false), (true, true)] {
        let layout = Layout::new();
        let engines = if multiple {
            layout.path("engines")
        } else {
            let only = layout.path("one-engine");
            std::fs::create_dir(&only).unwrap();
            std::fs::copy(layout.path("engines/claude"), only.join("claude")).unwrap();
            only
        };
        let mut cli = layout
            .cli()
            .env("PATH", format!("{}:/usr/bin:/bin", engines.display()));
        if billed {
            cli = cli.env("ANTHROPIC_API_KEY", "fixture-api-key");
        }
        let pty = nix::pty::openpty(
            None::<&nix::pty::Winsize>,
            None::<&nix::sys::termios::Termios>,
        )
        .unwrap();
        let mut child = cli.clone().arg("quickstart").spawn_on_terminal(&pty.slave);
        drop(pty.slave);
        let mut screen = Screen::new(pty.master);
        screen.expect(FLOOR_QUESTION);
        screen.answer("y");
        if billed {
            screen.expect("Daily dollar cap for the whole floor (Enter for none):");
            screen.answer("25");
        }
        screen.expect("repository [git ls-files");
        screen.answer("/usr/bin/true");
        if multiple {
            screen.expect("First agent engine (claude, codex; Enter to skip):");
            screen.answer("claude");
        } else {
            screen.expect("Hire a first agent named Kim on claude, idle? [Y/n]");
            screen.answer("");
        }
        screen.expect("and go ahead? [Y/n]");
        screen.answer("y");
        screen.expect("hired ");
        assert!(
            exit_status(&mut child, &screen).success(),
            "{}",
            screen.shown()
        );
        let shown = screen.shown();
        let questions = ["[Y/n]", "[y/N]", "]:", "):"]
            .iter()
            .map(|ending| shown.matches(ending).count())
            .sum::<usize>();
        assert_eq!(questions, if billed { 5 } else { 4 }, "{shown}");
        let listed = cli.args(["--json", "agents"]).run();
        let agents: Vec<dudes_proto::AgentView> = listed.json();
        assert_eq!(agents[0].name, "Kim");
        assert!(agents[0].goal.is_none(), "an idle hire has no goal");
        assert_only_status_checks(&layout);
    }
}

fn repeat_cli(layout: &Layout) -> Cli {
    layout
        .cli()
        .without("HOME")
        .env("DUDES_CONFIG", layout.path("operator.toml"))
}

fn repeat_guide(cli: &Cli, enroll: bool) -> String {
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = cli.clone().arg("quickstart").spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(FLOOR_QUESTION);
    screen.answer("y");
    if enroll {
        screen.expect("repository [git ls-files");
        screen.answer("/usr/bin/true");
    }
    screen.expect("Hire an agent named Kim on claude, idle? [y/N]");
    screen.answer("");
    screen.expect("and go ahead? [Y/n]");
    screen.answer("y");
    assert!(
        exit_status(&mut child, &screen).success(),
        "{}",
        screen.shown()
    );
    let shown = screen.shown();
    assert!(!shown.contains("first agent"), "{shown}");
    let questions = ["[Y/n]", "[y/N]", "]:", "):"]
        .iter()
        .map(|ending| shown.matches(ending).count())
        .sum::<usize>();
    assert!(questions <= 5, "{shown}");
    shown
}

#[test]
fn a_repeat_keeps_the_existing_idle_agent_when_enter_skips_hiring() {
    for stopped in [false, true] {
        let layout = Layout::new();
        std::fs::remove_file(layout.path("engines/codex")).unwrap();
        let cli = repeat_cli(&layout);
        let seeded = cli
            .clone()
            .args([
                "quickstart",
                "--enroll",
                "--hire",
                "already-here",
                "--provider",
                "claude",
                "--",
                "/usr/bin/true",
            ])
            .run();
        assert_eq!(seeded.code(), 0, "{seeded:?}");
        let before = cli.clone().args(["--json", "agents"]).run();
        let original: Vec<dudes_proto::AgentView> = before.json();
        assert_eq!(original.len(), 1);
        assert!(original[0].goal.is_none());
        if stopped {
            let stop = cli.clone().args(["stop", "--hard"]).run();
            assert_eq!(stop.code(), 0, "{stop:?}");
            assert_eq!(
                cli.clone().arg("ping").run().code(),
                dudes::exit::UNREACHABLE
            );
        }
        repeat_guide(&cli, false);
        let after = cli.clone().args(["--json", "agents"]).run();
        let agents: Vec<dudes_proto::AgentView> = after.json();
        assert_eq!(agents.len(), 1, "{after:?}");
        assert_eq!(agents[0].id, original[0].id);
        assert_eq!(agents[0].name, "already-here");
        assert!(agents[0].goal.is_none());
        assert_eq!(agents[0].turns, 0);
        assert_only_status_checks(&layout);
        let explicit = cli
            .clone()
            .args(["quickstart", "--hire", "requested", "--provider", "claude"])
            .run();
        assert_eq!(explicit.code(), 0, "{explicit:?}");
        let listed = cli.args(["--json", "agents"]).run();
        let agents: Vec<dudes_proto::AgentView> = listed.json();
        assert_eq!(agents.len(), 2);
        assert!(
            agents
                .iter()
                .any(|agent| agent.name == "requested" && agent.goal.is_none())
        );
    }
}

#[test]
fn a_responding_daemon_without_a_config_defaults_hiring_to_no() {
    let layout = Layout::new();
    std::fs::remove_file(layout.path("engines/codex")).unwrap();
    let cli = repeat_cli(&layout);
    let started = cli.clone().arg("start").run();
    assert_eq!(started.code(), 0, "{started:?}");
    assert!(!layout.path("operator.toml").exists());
    repeat_guide(&cli, true);
    let listed = cli.args(["--json", "agents"]).run();
    let agents: Vec<dudes_proto::AgentView> = listed.json();
    assert!(agents.is_empty(), "{listed:?}");
    assert_only_status_checks(&layout);
}

#[test]
fn yes_turns_on_the_new_install_floor_actions_and_leaves_the_terminal_off() {
    let layout = Layout::new();
    let started = layout.cli().args(["quickstart", "--yes"]).run();
    assert_eq!(started.code(), 0, "{started:?}");
    let file = Settings::read(&layout.config()).unwrap();
    for key in NEW_INSTALL_ON {
        assert!(file.satisfies(key), "{key}");
    }
    assert!(!file.satisfies(Key::Terminal));
    assert!(file.problems().is_empty());
}

#[test]
fn hire_asks_which_project_and_refuses_to_guess_without_a_terminal() {
    let layout = Layout::new();
    let started = layout.cli().args(["quickstart", "--yes"]).run();
    assert_eq!(started.code(), 0, "{started:?}");
    let added = layout
        .cli()
        .args(["project", "add", "--", "/usr/bin/true"])
        .run();
    assert_eq!(added.code(), 0, "{added:?}");
    let other = layout.path("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let added = layout
        .cli()
        .current_dir(&other)
        .args(["project", "add", "--", "/usr/bin/true"])
        .run();
    assert_eq!(added.code(), 0, "{added:?}");
    let args = [
        "hire",
        "--name",
        "Kim",
        "--role",
        "engineer",
        "--department",
        "engineering",
        "--provider",
        "claude",
    ];
    let refused = layout.cli().args(args).run();
    assert_eq!(refused.code(), dudes::exit::OTHER, "{refused:?}");
    assert_eq!(
        refused.stderr,
        "dudes: More than one project is enrolled. Run `dudes hire` in a terminal to choose one, or pass --cwd <project folder>.\n"
    );
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = layout.cli().args(args).spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect("Project (other, repo):");
    screen.answer("other");
    screen.expect("hired ");
    assert!(
        exit_status(&mut child, &screen).success(),
        "{}",
        screen.shown()
    );
    let listed = layout.cli().args(["--json", "agents"]).run();
    let agents: Vec<dudes_proto::AgentView> = listed.json();
    assert_eq!(agents[0].workspace.as_ref().unwrap().id, "other");
}

/// On a terminal with no options, each step is a question, and the answers give the same setup a
/// script gets from options.
#[test]
fn a_terminal_is_asked_each_step_and_gets_the_same_setup() {
    let layout = Layout::new();
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout.cli().arg("quickstart").spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);

    for (question, answer) in [
        (FLOOR_QUESTION, "y"),
        ("repository [git ls-files", "/usr/bin/true"),
        (
            "First agent engine (claude, codex; Enter to skip):",
            "codex",
        ),
        ("and go ahead? [Y/n]", ""),
    ] {
        screen.expect(question);
        screen.answer(answer);
    }
    screen.expect("hired ");
    screen.expect("run dudes open for a new one.");

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("quickstart did not exit:\n{}", screen.shown());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}:\n{}", screen.shown());
    let asked = ["[Y/n]", "[y/N]", "]:", "):"]
        .iter()
        .map(|ending| screen.shown().matches(ending).count())
        .sum::<usize>();
    assert!(
        asked <= 5,
        "quickstart asked {asked} questions:\n{}",
        screen.shown()
    );
    assert_set_up(&screen.shown(), &layout, true);
    assert_written(&layout, &NEW_INSTALL_ON, "/usr/bin/true");
}

/// On a terminal, a running daemon is named before the operator confirms, with what a restart
/// costs, and confirming restarts it.
#[test]
fn a_terminal_is_warned_before_a_restart() {
    let layout = Layout::new();
    let started = layout.cli().arg("quickstart").run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let first = layout.daemons();

    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout.cli().arg("quickstart").spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    for (question, answer) in [(FLOOR_QUESTION, "n"), ("repository [git ls-files", "-")] {
        screen.expect(question);
        screen.answer(answer);
    }
    screen.expect(
        "A daemon is running. When its settings differ from the file, quickstart restarts it, \
         which ends any turn it is running.",
    );
    screen.expect("and go ahead? [Y/n]");
    screen.answer("");
    screen.expect("Restarting the daemon so it reads");
    let status = child.wait().expect("quickstart exits");
    assert!(status.success(), "{status:?}:\n{}", screen.shown());
    assert_eq!(layout.daemons().len(), 2, "{}", screen.shown());
    assert!(!alive(first[0]), "the daemon with the old settings stopped");
}

/// The terminal's input ending is a stop: nothing is written and no daemon starts.
#[test]
fn a_terminal_that_ends_its_input_writes_nothing() {
    let layout = Layout::new();
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout.cli().arg("quickstart").spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(FLOOR_QUESTION);
    // Control-D at the start of a line ends a terminal's input.
    screen
        .keyboard
        .write_all(&[4])
        .expect("type an end of input");
    screen.expect("quickstart stopped at the end of its input; nothing was written");
    let status = child.wait().expect("quickstart exits");
    assert_eq!(status.code(), Some(i32::from(dudes::exit::USAGE)));
    assert!(!layout.config().exists());
    assert!(!layout.path("run").exists());
}

/// Answer the guided setup declining the Floor question, with no workspace and no hire, then
/// confirm.
fn answer_the_defaults(screen: &mut Screen) {
    for (question, answer) in [
        (FLOOR_QUESTION, "n"),
        ("repository [git ls-files", "-"),
        ("and go ahead? [Y/n]", ""),
    ] {
        screen.expect(question);
        screen.answer(answer);
    }
}

fn exit_status(child: &mut std::process::Child, screen: &Screen) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the command did not exit:\n{}", screen.shown());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_terminal_setup_prints_login_commands_without_another_question() {
    let layout = Layout::new();
    layout.sign_out("claude");
    layout.sign_out("codex");
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = layout.cli().arg("quickstart").spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    answer_the_defaults(&mut screen);
    screen.expect("Claude is signed out. Sign in with: dudes login claude");
    screen.expect("Codex is signed out. Sign in with: dudes login codex");
    assert!(
        exit_status(&mut child, &screen).success(),
        "{}",
        screen.shown()
    );
    assert!(!layout.path("claude-login").exists());
    assert!(!layout.path("codex-login").exists());
    assert_only_status_checks(&layout);
    assert!(!screen.shown().contains("Sign in to Claude now?"));
}

/// `dudes login` runs nothing without a terminal, nothing for an engine already signed in, and
/// only guidance for Pi; it prints the vendor command for the operator instead. A `signed_out`
/// answer is asked again on the next call, so signing in shows at once.
#[test]
fn dudes_login_without_a_terminal_prints_the_command_and_runs_nothing() {
    let layout = Layout::new();
    layout.sign_out("claude");
    let started = layout.cli().arg("quickstart").run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");

    let printed = layout.cli().args(["login", "claude"]).run();
    assert_eq!(printed.code(), dudes::exit::SUCCESS, "{printed:?}");
    assert_eq!(
        printed.stdout,
        format!(
            "Sign in to Claude by running this in a terminal: {} auth login\n",
            layout.path("engines/claude").display()
        )
    );

    std::fs::remove_file(layout.path("claude-signed-out")).expect("sign in");
    let signed_in = layout.cli().args(["login", "claude"]).run();
    assert_eq!(signed_in.code(), dudes::exit::SUCCESS, "{signed_in:?}");
    assert_eq!(
        signed_in.stdout,
        "The daemon reports Claude as signed in; nothing was run.\n"
    );

    let pi = layout.cli().args(["login", "pi"]).run();
    assert_eq!(pi.code(), dudes::exit::SUCCESS, "{pi:?}");
    assert_eq!(
        pi.stdout,
        "Pi signs in inside its own session: run pi, then type /login.\n"
    );

    let codex = layout.cli().args(["login", "codex"]).run();
    assert_eq!(codex.code(), dudes::exit::OTHER, "{codex:?}");
    assert_eq!(
        codex.stderr,
        "dudes: the daemon has no Codex engine; turn it on with dudes quickstart\n"
    );
    assert!(!layout.path("claude-login").exists(), "no login ran");
    assert_only_status_checks(&layout);
}

/// On a terminal, `dudes login` replaces itself with the engine's own login.
#[test]
fn dudes_login_on_a_terminal_replaces_itself_with_the_login() {
    let layout = Layout::new();
    layout.sign_out("claude");
    let started = layout.cli().arg("quickstart").run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout
        .cli()
        .args(["login", "claude"])
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(&format!(
        "Running {} auth login.",
        layout.path("engines/claude").display()
    ));
    let status = exit_status(&mut child, &screen);
    assert_eq!(status.code(), Some(7), "{}", screen.shown());
    assert!(
        std::fs::read_to_string(layout.path("claude-login"))
            .expect("the login ran")
            .starts_with("tty\n"),
        "the login holds the terminal"
    );
}

fn answer_login_doctor(
    root: &Path,
    provider: serde_json::Value,
) -> (Cli, std::thread::JoinHandle<()>) {
    use std::io::BufRead as _;
    let socket = root.join("doctor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let client_home = root.join("client-home");
    std::fs::create_dir(&client_home).unwrap();
    let ping = serde_json::json!({"version": VERSION, "socket": socket, "lsn": 0});
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut request)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "system.doctor");
        let reply = serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"ping":ping,"providers":[provider]}});
        writeln!(&stream, "{reply}").unwrap();
    });
    (Cli::new(&socket, root).env("HOME", client_home), server)
}

fn assert_managed_login_arguments(arguments: &[&str], home: &Path) {
    assert_eq!(arguments.len(), 7);
    assert_eq!(
        &arguments[..5],
        ["--disable", "hooks", "--disable", "plugins", "-c"]
    );
    assert_eq!(arguments[6], "login");
    let config: toml_edit::DocumentMut = arguments[5].parse().unwrap();
    let projects = config["projects"].as_inline_table().unwrap();
    assert_eq!(projects.len(), home.ancestors().count());
    for ancestor in home.ancestors() {
        let entry = projects
            .get(ancestor.to_str().unwrap())
            .unwrap()
            .as_inline_table()
            .unwrap();
        assert_eq!(
            entry.get("trust_level").unwrap().as_str(),
            Some("untrusted")
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn login_uses_the_daemons_home_and_preserves_legacy_environments() {
    for (provider, managed) in [("codex", true), ("codex", false), ("claude", false)] {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("Engine O'Brien;$HOME");
        std::fs::write(&program, "#!/bin/sh\n{ printf '%s\\n' \"$CODEX_HOME\" \"${DUDES_TOKEN-unset}\" \"${DUDES_SOCK-unset}\"; pwd -P; printf '%s\\n' \"$@\"; } > \"$LOGIN_RESULT\"\nexit 7\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let managed_home = root.path().join("daemon sign-in's;$HOME");
        std::fs::create_dir(&managed_home).unwrap();
        let managed_home = managed_home.canonicalize().unwrap();
        let client_home = root.path().join("client-codex-home");
        let login_home = managed.then(|| serde_json::json!({"state":"ready", "path":managed_home}));
        let (cli, server) = answer_login_doctor(
            root.path(),
            serde_json::json!({
                "provider":provider, "command":program, "ok":true, "sign_in":"signed_out", "login_home":login_home
            }),
        );
        let pty = nix::pty::openpty(
            None::<&nix::pty::Winsize>,
            None::<&nix::sys::termios::Termios>,
        )
        .unwrap();
        let result = root.path().join("login-result");
        let mut child = cli
            .env("CODEX_HOME", &client_home)
            .env("DUDES_TOKEN", "not-an-agent")
            .env("LOGIN_RESULT", &result)
            .args(["login", provider])
            .spawn_on_terminal(&pty.slave);
        drop(pty.slave);
        let mut screen = Screen::new(pty.master);
        screen.expect("Running ");
        assert_eq!(
            exit_status(&mut child, &screen).code(),
            Some(7),
            "{}",
            screen.shown()
        );
        server.join().unwrap();
        let written = std::fs::read_to_string(result).unwrap();
        let lines: Vec<_> = written.lines().collect();
        let expected_home = if managed { &managed_home } else { &client_home };
        assert_eq!(lines[0], expected_home.to_str().unwrap());
        assert_eq!(&lines[1..3], ["unset", "unset"]);
        let expected_cwd = if managed {
            managed_home.clone()
        } else {
            root.path().join("client-home").canonicalize().unwrap()
        };
        assert_eq!(lines[3], expected_cwd.to_str().unwrap());
        if managed {
            assert_managed_login_arguments(&lines[4..], &managed_home);
        } else if provider == "claude" {
            assert_eq!(&lines[4..], ["auth", "login"]);
        } else {
            assert_eq!(&lines[4..], ["login"]);
        }
    }
}

#[test]
fn codex_login_refuses_unavailable_or_invalid_home_even_when_signed_in() {
    for login_home in [
        serde_json::json!({"state":"unavailable","reason":"managed profile unavailable"}),
        serde_json::json!({"state":"ready","path":"relative-home"}),
        serde_json::json!({"state":"ready","path":"/tmp/../other-home"}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (cli, server) = answer_login_doctor(
            root.path(),
            serde_json::json!({
                "provider":"codex", "command":"/unused/codex", "ok":true, "sign_in":"signed_in", "login_home":login_home
            }),
        );
        let result = cli.args(["login", "codex"]).run();
        server.join().unwrap();
        assert_eq!(result.code(), dudes::exit::OTHER, "{result:?}");
        assert!(
            !result.stdout.contains("signed in; nothing was run"),
            "{result:?}"
        );
    }
}

#[test]
fn legacy_signed_in_login_keeps_the_no_run_shortcut_without_a_home_report() {
    let root = tempfile::tempdir().unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"codex", "ok":false, "sign_in":"signed_in"
        }),
    );
    let result = cli.args(["login", "codex"]).run();
    server.join().unwrap();
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    assert_eq!(
        result.stdout,
        "The daemon reports Codex as signed in; nothing was run.\n"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn printed_codex_login_command_preserves_the_literal_program_and_home() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("Codex O'Brien;$(false)");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\"\npwd -P\nprintf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let managed_home = root.path().join("daemon O'Brien;$(false)");
    std::fs::create_dir(&managed_home).unwrap();
    let managed_home = managed_home.canonicalize().unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"codex", "command":program, "ok":true, "sign_in":"signed_out",
            "login_home":{"state":"ready","path":managed_home}
        }),
    );
    let result = cli.args(["login", "codex"]).run();
    server.join().unwrap();
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    let command = result
        .stdout
        .strip_prefix("Sign in to Codex by running this in a terminal: ")
        .unwrap()
        .trim_end();
    let executed = run_program(Path::new("/bin/sh"), &["-c".into(), command.into()]);
    assert_eq!(executed.code(), 0, "{executed:?}");
    let lines: Vec<_> = executed.stdout.lines().collect();
    assert_eq!(
        &lines[..2],
        [
            managed_home.to_str().unwrap(),
            managed_home.to_str().unwrap()
        ]
    );
    assert_managed_login_arguments(&lines[2..], &managed_home);
}

/// Options ask nothing, even on a terminal: no login question and no browser. Quickstart prints
/// the commands instead.
#[test]
fn options_on_a_terminal_ask_nothing_and_print_the_commands() {
    let layout = Layout::new();
    layout.sign_out("claude");
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout
        .cli()
        .args(["quickstart", "--yes"])
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect("Claude is signed out. Sign in with: dudes login claude");
    screen.expect("Open Dudes with: dudes open");
    let status = exit_status(&mut child, &screen);
    assert!(status.success(), "{status:?}:\n{}", screen.shown());
    let shown = screen.shown();
    for absent in ["Hire a first agent?", "#bootstrap="] {
        assert!(!shown.contains(absent), "`{absent}` in:\n{shown}");
    }
    assert!(!layout.path("claude-login").exists(), "no login ran");
}

#[test]
fn setup_escapes_every_mailbox_diagnostic_filename() {
    let layout = Layout::new();
    let started = layout
        .cli()
        .args([
            "quickstart",
            "--set",
            "providers.codex",
            "on",
            "--enroll",
            "--hire",
            "Mail",
            "--provider",
            "codex",
            "--",
            "/usr/bin/true",
        ])
        .run();
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let reader = dudes_journal::Reader::new(&layout.path("state/journal"));
    let agent = reader
        .iter_from(dudes_types::Lsn::ZERO)
        .unwrap()
        .find_map(|entry| match entry.unwrap().1.record {
            dudes_proto::Record::AgentHired { id, .. } => Some(id),
            _ => None,
        })
        .expect("a hired agent");
    let done = layout.path(&format!("state/agents/{agent}/inbox/.done"));
    std::fs::create_dir_all(&done).unwrap();
    let filename = "marker\u{1b}]52;clipboard\nforged";
    std::fs::write(done.join(filename), "unread content").unwrap();
    let agents = layout.cli().args(["--json", "agents"]).run();
    assert_eq!(agents.code(), dudes::exit::SUCCESS, "{agents:?}");
    assert!(agents.stdout.contains(&agent.to_string()), "{agents:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        let result = layout
            .cli()
            .args(["quickstart", "--set", "providers.codex", "on"])
            .run();
        if result
            .stdout
            .contains("marker\\u{1b}]52;clipboard\\u{a}forged")
        {
            break result;
        }
        assert!(
            Instant::now() < deadline,
            "mailbox diagnostics did not appear: {result:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    assert!(
        result
            .stdout
            .contains("marker\\u{1b}]52;clipboard\\u{a}forged"),
        "{result:?}"
    );
    assert!(!result.stdout.contains('\u{1b}'), "{result:?}");
    assert!(!result.stdout.contains("unread content"), "{result:?}");
}
