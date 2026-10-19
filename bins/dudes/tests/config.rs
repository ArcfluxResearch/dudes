//! `dudes config`: the operator file's start-time settings, edited in place, byte for byte outside
//! the keys an edit names, and never into a file the daemon would refuse at start.
#![cfg(unix)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dudes::config::Settings;
use dudes::exit;
use dudes_proto::rpc::floor::EnrollmentReloadResult;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

/// The file the daemon reads for a `HOME` of `home`, by the daemon's own rule.
fn config_for(home: &Path) -> PathBuf {
    duded::config::operator_config_path(|name| (name == "HOME").then(|| home.into()))
        .expect("an absolute HOME names a file")
}

/// A hand-written file in `home`, and a CLI with no daemon to reach.
fn written(home: &Path, text: &str) -> (PathBuf, Cli) {
    let config = config_for(home);
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    std::fs::write(&config, text).expect("the original");
    let cli = Cli::new(&home.join("absent.sock"), home).env("HOME", home);
    (config, cli)
}

fn read(config: &Path) -> String {
    std::fs::read_to_string(config).expect("the file")
}

#[test]
fn codex_streaming_edits_preserve_comments_and_the_enabled_provider() {
    for original in [
        "# Keep Codex enabled.\n[providers.codex]\ncommand = '/opt/bin/codex'\nstreaming = false # keep this choice\n",
        "providers.codex.command = '/opt/bin/codex'\nproviders.codex.streaming = false # keep this choice\n",
        "providers = { codex = { command = '/opt/bin/codex', streaming = false } } # keep this choice\n",
    ] {
        let home = tempfile::tempdir().unwrap();
        let (config, cli) = written(home.path(), original);
        for value in ["on", "off"] {
            let edited = cli
                .clone()
                .args(["config", "set", "providers.codex.streaming", value])
                .run();
            assert_eq!(edited.code(), exit::SUCCESS, "{edited:?}");
            assert_eq!(
                duded::config::load_operator(&config)
                    .unwrap()
                    .providers
                    .codex
                    .unwrap()
                    .streaming,
                value == "on"
            );
            assert_eq!(
                read(&config),
                if value == "on" {
                    original.replace("streaming = false", "streaming = true")
                } else {
                    original.to_owned()
                }
            );
            let shown = cli.clone().arg("config").run();
            assert!(
                shown
                    .stdout
                    .lines()
                    .any(|line| line.split_whitespace().collect::<Vec<_>>()
                        == ["providers.codex.streaming", value]),
                "{shown:?}"
            );
        }
        let unset = cli
            .args(["config", "unset", "providers.codex.streaming"])
            .run();
        assert_eq!(unset.code(), exit::SUCCESS, "{unset:?}");
        assert!(read(&config).contains("command = '/opt/bin/codex'"));
    }

    let home = tempfile::tempdir().unwrap();
    let original = "# Keep Codex enabled.\n[providers.codex]\n";
    let (config, cli) = written(home.path(), original);
    let enabled = cli
        .clone()
        .args(["config", "set", "providers.codex.streaming", "on"])
        .run();
    assert_eq!(enabled.code(), exit::SUCCESS, "{enabled:?}");
    let unset = cli
        .args(["config", "unset", "providers.codex.streaming"])
        .run();
    assert_eq!(unset.code(), exit::SUCCESS, "{unset:?}");
    assert_eq!(read(&config), original);
    let codex = duded::config::load_operator(&config)
        .unwrap()
        .providers
        .codex
        .unwrap();
    assert!(!codex.streaming);
}

#[test]
fn chat_has_no_setting_and_an_old_key_is_left_alone() {
    let original = "# Keep this choice.\n[floor.chat]\nenabled = false # not yet\n\n[memory]\ncondense_model_enabled = false\n";
    let home = tempfile::tempdir().unwrap();
    let (config, cli) = written(home.path(), original);
    for args in [
        ["config", "set", "floor.chat", "on"],
        ["config", "unset", "floor.chat", ""],
    ] {
        let refused = cli
            .clone()
            .args(args.into_iter().filter(|word| !word.is_empty()))
            .run();
        assert_eq!(refused.code(), exit::USAGE, "{refused:?}");
        assert_eq!(
            refused.stderr.lines().next(),
            Some(
                "dudes: Chat is always on and has no setting; delete the `[floor.chat]` table \
                 from the configuration file to stop the daemon warning about it"
            ),
            "{refused:?}"
        );
        assert_eq!(read(&config), original);
    }
    let shown = cli.arg("config").run();
    assert_eq!(shown.code(), exit::SUCCESS, "{shown:?}");
    assert!(!shown.stdout.contains("floor.chat"), "{shown:?}");
    assert!(
        duded::config::load_operator_with_options(&config)
            .unwrap()
            .options
            .chat_key_ignored()
    );
}
const ORIGINAL: &str = "\
# Operator settings, written by hand.
[budget]
floor_usd_per_day = 100 # the floor-wide ceiling

[budget.per_turn_floor]
usd   = 0.000001   # aligned by hand
tokens = 1

[floor.answer]
enabled = false # not yet
# trailing note
";

/// An edit changes only the keys it names and keeps their comments; undoing it gives back the
/// original bytes; and the daemon reads the edited file as the edit says.
#[test]
fn an_edit_and_its_undo_keep_every_other_byte() {
    let home = tempfile::tempdir().expect("home");
    let (config, cli) = written(home.path(), ORIGINAL);

    let set = cli
        .clone()
        .args([
            "config",
            "set",
            "providers.codex",
            "on",
            "floor.answer",
            "on",
        ])
        .args([
            "budget.per_turn_floor.tokens",
            "20000",
            "floor.terminal",
            "on",
        ])
        .run();
    assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
    assert_eq!(
        set.stdout,
        format!(
            "wrote {}; the daemon reads it when it starts\n",
            config.display()
        )
    );
    assert_eq!(
        read(&config),
        "\
# Operator settings, written by hand.
[budget]
floor_usd_per_day = 100 # the floor-wide ceiling

[budget.per_turn_floor]
usd   = 0.000001   # aligned by hand
tokens = 20000

[floor.answer]
enabled = true # not yet

[floor.terminal]
enabled = true

[providers.codex]
# trailing note
"
    );
    let loaded = duded::config::load_operator(&config).expect("the daemon admits the edit");
    assert!(loaded.providers.codex.is_some());
    assert!(loaded.answer_enabled);
    assert!(loaded.terminal_enabled);
    assert_eq!(loaded.budget.per_turn_floor.tokens.count(), 20_000);

    let undone = cli
        .clone()
        .args(["config", "set", "floor.answer", "off"])
        .args(["budget.per_turn_floor.tokens", "1"])
        .run();
    assert_eq!(undone.code(), exit::SUCCESS, "{}", undone.output());
    let unset = cli
        .args(["config", "unset", "floor.terminal", "providers.codex"])
        .run();
    assert_eq!(unset.code(), exit::SUCCESS, "{}", unset.output());
    assert_eq!(read(&config), ORIGINAL, "the edit and its undo are no edit");
}

/// The daemon's other spellings of a table, a dotted key and an inline table, are edited where
/// they are, and the daemon reads the result.
#[test]
fn an_edit_reaches_dotted_and_inline_tables() {
    for (original, edited) in [
        (
            "budget.floor_usd_per_day = 1\n",
            "budget.floor_usd_per_day = 1\n\n[budget.per_turn_floor]\nusd = 0.25\n",
        ),
        (
            "budget = { floor_usd_per_day = 1 }\n",
            "budget = { floor_usd_per_day = 1 , per_turn_floor = { usd = 0.25 } }\n",
        ),
    ] {
        let home = tempfile::tempdir().expect("home");
        let (config, cli) = written(home.path(), original);
        let set = cli
            .args(["config", "set", "budget.per_turn_floor.usd", "0.25"])
            .run();
        assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
        assert_eq!(read(&config), edited);
        let budget = duded::config::load_operator(&config)
            .expect("the daemon admits the edit")
            .budget;
        assert_eq!(budget.per_turn_floor.usd.micros(), 250_000, "{original}");
        assert_eq!(
            budget.floor_usd_per_day.map(dudes_types::Usd::micros),
            Some(1_000_000)
        );
    }
}

/// Each edit the daemon would refuse at start writes nothing and names the key it needs.
#[test]
fn an_edit_the_daemon_would_refuse_writes_nothing_and_names_the_key() {
    let off = "[floor.answer]\nenabled = true\n";
    let on = "[budget]\nfloor_usd_per_day = 5\n[budget.per_turn_floor]\nusd = 0.25\n\
              [floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n";
    let renewing = "[floor.hire]\nenabled = true\n[floor.control]\nenabled = true\n\
                    [floor.goal]\nenabled = true\n[floor.renew]\nenabled = true\n";
    for (original, edit, problems) in [
        (
            off,
            &["floor.retire", "on"][..],
            "floor.retire on needs floor.control on",
        ),
        (
            off,
            &["floor.goal", "on"],
            "floor.goal on needs floor.control on",
        ),
        (
            "[budget.per_turn_floor]\nusd = 0\n",
            &["budget.floor_usd_per_day", "0"],
            "budget.floor_usd_per_day needs budget.per_turn_floor.usd above zero",
        ),
        (
            on,
            &["floor.control", "off"],
            "floor.goal on needs floor.control on",
        ),
        (
            on,
            &["budget.per_turn_floor.usd", "0"],
            "budget.floor_usd_per_day needs budget.per_turn_floor.usd above zero",
        ),
        (
            off,
            &["floor.renew", "on"],
            "floor.renew on needs floor.goal on; floor.renew on needs floor.hire on",
        ),
        (
            on,
            &["floor.renew", "on"],
            "floor.renew on needs floor.hire on",
        ),
        (
            renewing,
            &["floor.goal", "off"],
            "floor.renew on needs floor.goal on",
        ),
        (
            renewing,
            &["floor.hire", "off"],
            "floor.renew on needs floor.hire on",
        ),
    ] {
        let home = tempfile::tempdir().expect("home");
        let (config, cli) = written(home.path(), original);
        let refused = cli.args(["config", "set"]).args(edit).run();
        assert_eq!(
            refused.code(),
            exit::OTHER,
            "{edit:?}: {}",
            refused.output()
        );
        assert_eq!(
            refused.stderr,
            format!(
                "dudes: the daemon would refuse this configuration when it starts, so nothing \
                 was written: {problems}\n"
            ),
            "{edit:?}"
        );
        assert_eq!(read(&config), original, "{edit:?}: nothing was written");
    }

    let home = tempfile::tempdir().expect("home");
    let (config, cli) = written(home.path(), off);
    let together = cli
        .args(["config", "set", "budget.floor_usd_per_day", "5"])
        .args(["budget.per_turn_floor.usd", "0.25", "floor.control", "on"])
        .args(["floor.goal", "on", "floor.retire", "on"])
        .args(["floor.hire", "on", "floor.renew", "on"])
        .run();
    assert_eq!(together.code(), exit::SUCCESS, "{}", together.output());
    assert_eq!(
        duded::config::load_operator(&config)
            .expect("control: the daemon admits the keys set together")
            .control,
        duded::settings::FloorControl::On {
            goal: true,
            retire: true,
            renew: true,
        }
    );
}

/// A value or key `dudes config` does not take is an argument mistake: nothing is written, and the
/// message says what the key takes.
#[test]
fn a_value_the_key_does_not_take_is_refused_before_any_write() {
    let home = tempfile::tempdir().expect("home");
    let cli = Cli::new(&home.path().join("absent.sock"), home.path()).env("HOME", home.path());
    let dollars = "dollars of zero or more, with at most six decimal places";
    for (args, message) in [
        (
            &["budget.floor_usd_per_day", "-1"][..],
            format!("`budget.floor_usd_per_day` does not take `-1`: {dollars}"),
        ),
        (
            &["budget.per_turn_floor.usd", "0.1234567"],
            format!("`budget.per_turn_floor.usd` does not take `0.1234567`: {dollars}"),
        ),
        (
            &["budget.per_turn_floor.usd", " 1"],
            format!("`budget.per_turn_floor.usd` does not take ` 1`: {dollars}"),
        ),
        (
            &["budget.per_turn_floor.tokens", "1.5"],
            "`budget.per_turn_floor.tokens` does not take `1.5`: a whole number of tokens"
                .to_owned(),
        ),
        (
            &["floor.control", "yes"],
            "`floor.control` does not take `yes`: on or off".to_owned(),
        ),
        (
            &["floor.git", "true"],
            "`floor.git` does not take `true`: on or off".to_owned(),
        ),
        (
            &["floor.diff", "true"],
            "`floor.diff` does not take `true`: on or off".to_owned(),
        ),
        (
            &["floor.control"],
            "`floor.control` needs a value".to_owned(),
        ),
        (
            &["floor.bogus", "on"],
            "`floor.bogus` is not a config key; the keys are providers.codex, providers.codex.streaming, providers.pi, \
             floor.answer, floor.hire, floor.control, floor.recovery, floor.goal, floor.renew, floor.retire, floor.board, floor.terminal, \
             floor.git, floor.diff, budget.floor_usd_per_day, budget.per_turn_floor.usd, budget.per_turn_floor.tokens"
                .to_owned(),
        ),
    ] {
        let refused = cli.clone().args(["config", "set"]).args(args).run();
        assert_eq!(
            refused.code(),
            exit::USAGE,
            "{args:?}: {}",
            refused.output()
        );
        assert_eq!(
            refused.stderr.lines().next(),
            Some(format!("dudes: {message}").as_str()),
            "{args:?}"
        );
    }
    assert!(
        !config_for(home.path()).exists(),
        "no refused argument wrote a file"
    );
}

/// `dudes config` shows each key as the daemon will read it, and why it would refuse the file.
#[test]
fn show_prints_every_key_as_the_daemon_reads_it() {
    let home = tempfile::tempdir().expect("home");
    let cli = Cli::new(&home.path().join("absent.sock"), home.path()).env("HOME", home.path());
    let config = config_for(home.path());
    let absent = cli.clone().arg("config").run();
    assert_eq!(absent.code(), exit::SUCCESS, "{}", absent.output());
    assert!(!absent.stdout.contains("floor.chat"), "{absent:?}");
    assert!(
        absent
            .stdout
            .lines()
            .any(|line| line == "chat                          always on, with no setting"),
        "{absent:?}"
    );
    assert_eq!(
        absent.stdout.lines().next(),
        Some(
            format!(
                "file                          {} (absent, so every key has its default)",
                config.display()
            )
            .as_str()
        )
    );

    let (config, cli) = written(
        home.path(),
        "[budget]\nfloor_usd_per_day = 5\n[budget.per_turn_floor]\nusd = 0\n[providers.pi]\n\
         [floor.control]\nenabled = true\n",
    );
    let shown = cli.arg("config").run();
    assert_eq!(shown.code(), exit::SUCCESS, "{}", shown.output());
    assert_eq!(
        shown.stdout,
        format!(
            "\
file                          {}
providers.codex               off
providers.codex.streaming     off
providers.pi                  on
floor.answer                  off
floor.hire                    off
floor.control                 on
floor.recovery                off
floor.goal                    off
floor.renew                   off
floor.retire                  off
floor.board                   off
floor.terminal                off
floor.git                     off
floor.diff                    off
budget.floor_usd_per_day      5
budget.per_turn_floor.usd     0
budget.per_turn_floor.tokens  20000
chat                          always on, with no setting
the daemon will refuse to start with this file: budget.floor_usd_per_day needs \
budget.per_turn_floor.usd above zero
checked only the keys listed above; other file settings and journaled ceilings can also refuse startup
",
            config.display()
        )
    );
}

#[test]
fn show_states_its_validation_scope_for_settings_it_does_not_own() {
    let home = tempfile::tempdir().expect("home");
    let (config, cli) = written(home.path(), "[providers.codex]\ncommand = 42\n");
    assert!(duded::config::load_operator(&config).is_err());
    let shown = cli.arg("config").run();
    assert_eq!(shown.code(), exit::SUCCESS, "{}", shown.output());
    assert!(shown.stdout.ends_with(
        "checked only the keys listed above; other file settings and journaled ceilings can also refuse startup\n"
    ), "{}", shown.output());
}

/// A running daemon that reads no operator file cannot be compared with it, so an edit says it
/// takes a restart; an edit already in the file writes nothing and says so.
#[test]
fn an_edit_tells_a_running_daemon_to_restart() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let config = config_for(&home);
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("HOME", &home);

    let set = cli
        .clone()
        .args(["config", "set", "providers.pi", "on"])
        .run();
    assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
    assert_eq!(
        set.stdout,
        format!(
            "wrote {}; the running daemon reads these settings only when it starts, so restart it \
             to apply them: `dudes stop`, then `dudes start`\n",
            config.display()
        )
    );
    let again = cli.args(["config", "set", "providers.pi", "on"]).run();
    assert_eq!(again.code(), exit::SUCCESS, "{}", again.output());
    assert_eq!(
        again.stdout,
        format!(
            "{} already holds these settings; nothing was written\n",
            config.display()
        )
    );
}

/// The settings the daemon in [`running_on`] runs.
const RUNNING: &str = "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n\
                       tokens = 1\n[floor.control]\nenabled = true\n[memory]\ncondense_model_enabled = false\n";

/// A daemon that runs [`RUNNING`] and reads `text` from the file for `home`, and a CLI whose
/// `HOME` is `cli_home`.
fn running_on(
    fixture: &Fixture,
    home: &Path,
    text: &str,
    cli_home: &Path,
) -> (common::Running, Cli) {
    running_with(fixture, home, text, cli_home, false)
}

/// [`running_on`], with goal change and Hire also on when `renewable` is.
fn running_with(
    fixture: &Fixture,
    home: &Path,
    text: &str,
    cli_home: &Path,
    renewable: bool,
) -> (common::Running, Cli) {
    let config = config_for(home);
    std::fs::create_dir_all(config.parent().expect("a parent")).expect("config directory");
    std::fs::write(&config, text).expect("the file the daemon reads");
    let mut settings = fixture.settings();
    settings.operator_config = Some(config);
    settings.floor_control = duded::settings::FloorControl::On {
        goal: renewable,
        retire: false,
        renew: false,
    };
    settings.floor_hire_enabled = renewable;
    let running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("HOME", cli_home);
    (running, cli)
}

fn set(cli: &Cli, key: &str, value: &str) -> String {
    let run = cli.clone().args(["config", "set", key, value]).run();
    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    run.stdout
}

fn restart(config: &Path, differences: &str) -> String {
    format!(
        "wrote {}; the running daemon reads other settings ({differences}) and reads the file only \
         when it starts, so restart it to apply them: `dudes stop`, then `dudes start`\n",
        config.display()
    )
}

fn no_restart(config: &Path) -> String {
    format!(
        "wrote {}; the running daemon already runs these settings, so no restart is needed\n",
        config.display()
    )
}

/// An edit names the settings that differ from what the daemon runs, and an edit back to them
/// needs no restart. A `floor.retire` still waiting for a restart stays named while other keys go
/// and come back.
#[test]
fn an_edit_asks_for_a_restart_only_for_settings_the_running_daemon_does_not_run() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    let (_running, cli) = running_on(&fixture, &home, RUNNING, &home);

    assert_eq!(
        set(&cli, "floor.answer", "on"),
        restart(&config, "floor.answer")
    );
    assert_eq!(set(&cli, "floor.answer", "off"), no_restart(&config));
    assert_eq!(
        set(&cli, "floor.retire", "on"),
        restart(&config, "floor.retire")
    );
    assert_eq!(
        set(&cli, "floor.answer", "on"),
        restart(&config, "floor.answer, floor.retire")
    );
    assert_eq!(
        set(&cli, "floor.answer", "off"),
        restart(&config, "floor.retire")
    );
    let reload = cli.clone().args(["project", "reload"]).run();
    assert!(
        reload
            .stdout
            .contains("\nneeds a daemon restart: floor.retire\n"),
        "{}",
        reload.output()
    );
    assert_eq!(set(&cli, "floor.retire", "off"), no_restart(&config));
}

/// Renewal is read only at start: turning it on beside a running goal change and Hire names it for
/// a restart, and turning it off again needs none.
#[test]
fn turning_renewal_on_asks_for_a_restart() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    let running = format!("{RUNNING}[floor.goal]\nenabled = true\n[floor.hire]\nenabled = true\n");
    let (_running, cli) = running_with(&fixture, &home, &running, &home, true);

    assert_eq!(
        set(&cli, "floor.renew", "on"),
        restart(&config, "floor.renew")
    );
    assert_eq!(set(&cli, "floor.renew", "off"), no_restart(&config));
}

#[test]
fn recovery_can_be_configured_without_control_but_requires_restart() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    let (_running, cli) = running_on(&fixture, &home, RUNNING, &home);
    assert_eq!(
        set(&cli, "floor.recovery", "on"),
        restart(&config, "floor.recovery")
    );
    let loaded = duded::config::load_operator(&config).unwrap();
    assert!(loaded.recovery_enabled);
    let report = cli.clone().args(["project", "reload"]).run();
    assert!(
        report
            .stdout
            .contains("\nneeds a daemon restart: floor.recovery\n"),
        "{}",
        report.output()
    );
    assert_eq!(set(&cli, "floor.recovery", "off"), no_restart(&config));
    let off = "[floor.recovery]\nenabled = true\n[memory]\ncondense_model_enabled = false\n";
    std::fs::write(&config, off).unwrap();
    assert!(Settings::read(&config).unwrap().problems().is_empty());
    let loaded = duded::config::load_operator(&config).unwrap();
    assert!(loaded.recovery_enabled);
    assert!(!loaded.control.pause_resume());
}

/// A daemon that reads another file cannot say whether this one matches it, so the edit names
/// that file and asks for the restart that makes the daemon read this one.
#[test]
fn an_edit_to_a_file_the_running_daemon_does_not_read_asks_for_a_restart() {
    let fixture = Fixture::new();
    let (theirs, mine) = (
        fixture.state.path().join("theirs"),
        fixture.state.path().join("mine"),
    );
    let (_running, cli) = running_on(&fixture, &theirs, RUNNING, &mine);

    assert_eq!(
        set(&cli, "floor.answer", "on"),
        format!(
            "wrote {}; the running daemon reads {}, not this file, so restart it to apply these \
             settings: `dudes stop`, then `dudes start`\n",
            config_for(&mine).display(),
            config_for(&theirs).display()
        )
    );
}

/// The edit shows the running daemon's refusal of the file instead of advising a restart, which
/// this refusal would also stop.
#[test]
fn an_edit_to_a_file_the_running_daemon_refuses_shows_the_refusal() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    let (_running, cli) = running_on(&fixture, &home, RUNNING, &home);
    std::fs::write(&config, format!("{RUNNING}condense_after_kib = 0\n"))
        .expect("a file the daemon refuses");

    assert_eq!(
        set(&cli, "floor.answer", "on"),
        format!(
            "wrote {}; the running daemon refuses the file: configuration at {} has invalid \
             [memory] settings\n",
            config.display(),
            config.display()
        )
    );
}

/// A daemon of another version may compare settings this CLI does not know, so the edit asks for a
/// restart without asking it anything else.
#[test]
fn an_edit_beside_a_daemon_of_another_version_asks_for_a_restart() {
    let home = tempfile::tempdir().expect("home");
    let socket = home.path().join("duded.sock");
    common::ping_as(&socket, "0.0.1");
    let cli = Cli::new(&socket, home.path()).env("HOME", home.path());

    assert_eq!(
        set(&cli, "floor.answer", "on"),
        format!(
            "wrote {}; the running daemon is 0.0.1, this CLI is {}, so restart it to apply these \
             settings: `dudes stop`, then `dudes start`\n",
            config_for(home.path()).display(),
            env!("CARGO_PKG_VERSION")
        )
    );
}

/// Workspace entries are not start-time settings: a reload adopts them while the daemon runs, so
/// the edit names them apart from the restart and points to that reload.
#[test]
fn an_edit_names_workspace_changes_apart_and_points_to_the_reload() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let config = config_for(&home);
    let repo = fixture.state.path().join("repo_1");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let repo = repo.canonicalize().expect("canonical repository");
    let (_running, cli) = running_on(
        &fixture,
        &home,
        &format!(
            "{RUNNING}[[floor.onboarding.workspaces]]\nworkspace_id = \"repo_1\"\nlabel = \"Service\"\n\
             completion_check_label = \"Checks pass\"\ncwd = \"{}\"\ndepartment = \"engineering\"\n\
             isolation = \"default\"\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\n\
             predicate_argv = [\"/usr/bin/true\"]\npredicate_timeout_s = 10\n",
            repo.display()
        ),
        &home,
    );

    let written = set(&cli, "floor.answer", "on");
    let check: EnrollmentReloadResult = cli
        .clone()
        .args(["--json", "project", "reload"])
        .run()
        .json();
    assert_eq!(
        written,
        format!(
            "{}the running daemon has not adopted the file's project changes (project repo_1 \
             added); `dudes project reload --apply {}` adopts them without a restart\n",
            restart(&config, "floor.answer"),
            check.sha256
        )
    );
}

fn journal_with_ceiling(journal: &str) -> Fixture {
    let fixture = Fixture::new();
    if journal != "none" {
        let settings = fixture.settings();
        let mut running = start(
            &settings,
            SharedHost::new(),
            Arc::new(OpenRunner::default()),
        );
        let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
        let mut hire = common::hire_args(fixture.state.path());
        hire.truncate(hire.len() - 2);
        if journal == "agent dollars" {
            hire.extend(["--usd-per-day".into(), "5".into()]);
        }
        if journal == "agent tokens" {
            hire.extend(["--tokens-per-day".into(), "5".into()]);
        }
        let hired = cli.clone().arg("--json").args(hire).run();
        assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
        let hired: dudes_proto::rpc::agent::HireResult = hired.json();
        if journal.starts_with("goal") {
            let goal = cli
                .args([
                    "goal",
                    "--agent",
                    &hired.agent.to_string(),
                    "--text",
                    "a journaled ceiling",
                    "--hop-cap",
                    "1",
                    if journal == "goal dollars" {
                        "--usd"
                    } else {
                        "--tokens"
                    },
                    "5",
                    "--",
                    "/usr/bin/false",
                ])
                .run();
            assert_eq!(goal.code(), exit::SUCCESS, "{}", goal.output());
        }
        running.stop();
    }
    fixture
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the full config matrix and its journal controls verify one agreement rule"
)]
fn the_cli_refuses_exactly_what_the_daemon_refuses_at_start() {
    let floors = [None, Some("0"), Some("5")];
    let reserves = [None, Some("0"), Some("0.25")];
    let tokens = [None, Some("0"), Some("1")];
    let budget = |floor: Option<&str>, usd: Option<&str>, tokens: Option<&str>| {
        let mut text = String::new();
        if let Some(floor) = floor {
            let _ = write!(text, "[budget]\nfloor_usd_per_day = {floor}\n");
        }
        if usd.is_some() || tokens.is_some() {
            let _ = writeln!(text, "[budget.per_turn_floor]");
        }
        if let Some(usd) = usd {
            let _ = writeln!(text, "usd = {usd}");
        }
        if let Some(tokens) = tokens {
            let _ = writeln!(text, "tokens = {tokens}");
        }
        text
    };
    let directory = tempfile::tempdir().expect("a config directory");
    let path = directory.path().join("config.toml");

    let mut journals = Vec::new();
    for journal in [
        "none",
        "agent dollars",
        "agent tokens",
        "goal dollars",
        "goal tokens",
    ] {
        let fixture = journal_with_ceiling(journal);
        journals.push((journal, fixture));
    }

    let boot_admits = |fixture: &Fixture, loaded: &duded::config::OperatorConfig| {
        let mut settings = fixture.settings();
        settings.floor_usd_per_day = loaded.budget.floor_usd_per_day;
        settings.per_turn_floor = loaded.budget.per_turn_floor;
        duded::boot(
            &settings,
            common::seams(
                &settings,
                SharedHost::new(),
                Arc::new(OpenRunner::default()),
            ),
        )
        .is_ok()
    };

    let mut budget_admission = Vec::new();
    for floor in floors {
        for usd in reserves {
            for tokens in tokens {
                std::fs::write(&path, budget(floor, usd, tokens)).expect("the budget file");
                let loaded =
                    duded::config::load_operator(&path).expect("a budget the daemon reads");
                budget_admission.push(boot_admits(&journals[0].1, &loaded));
            }
        }
    }

    for (journal, fixture) in journals.iter().skip(1) {
        for reserve in ["0", "0.25"] {
            let dollars = journal.ends_with("dollars");
            let text = budget(
                None,
                Some(if dollars { reserve } else { "0" }),
                Some(if dollars || reserve == "0" { "0" } else { "1" }),
            );
            std::fs::write(&path, text).expect("the journal reserve file");
            let loaded = duded::config::load_operator(&path).expect("a reserve the daemon reads");
            assert_eq!(
                boot_admits(fixture, &loaded),
                reserve != "0",
                "the daemon's {journal} reserve rule"
            );
        }
    }

    let ruled = ["control", "goal", "retire", "renew"];
    let unruled = ["answer", "hire", "terminal", "git"];
    let mut accepted = 0;
    for mask in 0..1_u32 << (ruled.len() + unruled.len()) {
        let mut budget_index = 0;
        for floor in floors {
            for usd in reserves {
                for tokens in tokens {
                    let mut text = budget(floor, usd, tokens);
                    let on = ruled
                        .iter()
                        .enumerate()
                        .filter(|(bit, _)| mask & 1 << bit != 0)
                        .map(|(_, name)| *name)
                        .chain(unruled.iter().enumerate().filter_map(|(bit, name)| {
                            (mask & 1 << (ruled.len() + bit) != 0).then_some(*name)
                        }));
                    for name in on {
                        let _ = write!(text, "[floor.{name}]\nenabled = true\n");
                    }
                    std::fs::write(&path, &text).expect("the file");
                    let cli = Settings::read(&path)
                        .expect("a file the CLI reads")
                        .problems()
                        .is_empty();
                    let parsed = duded::config::load_operator(&path).is_ok();
                    for (journal, _) in &journals {
                        let reserves_for_journal = match *journal {
                            "agent dollars" | "goal dollars" => usd != Some("0"),
                            "agent tokens" | "goal tokens" => tokens != Some("0"),
                            _ => true,
                        };
                        let daemon =
                            parsed && budget_admission[budget_index] && reserves_for_journal;
                        assert_eq!(
                            cli && reserves_for_journal,
                            daemon,
                            "the CLI and the daemon disagree for {journal}:\n{text}"
                        );
                        if *journal == "none" {
                            accepted += usize::from(daemon);
                        }
                    }
                    budget_index += 1;
                }
            }
        }
    }
    assert_eq!(
        accepted,
        12 * 8 * 21,
        "control: both verdicts occur, in the number the rules give: 12 sets of the control, \
         goal, retire, renew and hire switches with goal and retire only under control and renew \
         only beside goal and hire, times 8 sets of the other switches, times 21 budgets in which \
         a floor ceiling has a dollar reserve"
    );
}

#[test]
fn representative_switches_reach_boot_with_journaled_ceilings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for journal in [
        "none",
        "agent dollars",
        "agent tokens",
        "goal dollars",
        "goal tokens",
    ] {
        let fixture = journal_with_ceiling(journal);
        for switches in [
            "",
            "[floor.answer]\nenabled = true\n[floor.hire]\nenabled = true\n",
            "[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.retire]\nenabled = true\n",
            "[floor.terminal]\nenabled = true\n[floor.git]\nenabled = true\n",
            "[floor.goal]\nenabled = true\n",
            "[floor.retire]\nenabled = true\n",
            "[floor.hire]\nenabled = true\n[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.renew]\nenabled = true\n",
            "[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.renew]\nenabled = true\n",
        ] {
            for (usd, tokens) in [("0", "0"), ("0", "1"), ("0.25", "0"), ("0.25", "1")] {
                let text =
                    format!("[budget.per_turn_floor]\nusd = {usd}\ntokens = {tokens}\n{switches}");
                std::fs::write(&path, &text).unwrap();
                let cli = Settings::read(&path).unwrap().problems().is_empty();
                let reserve_admits = match journal {
                    "agent dollars" | "goal dollars" => usd != "0",
                    "agent tokens" | "goal tokens" => tokens != "0",
                    _ => true,
                };
                let daemon = duded::config::load_operator(&path).is_ok_and(|loaded| {
                    let mut settings = fixture.settings();
                    settings.operator_config = Some(path.clone());
                    settings.floor_usd_per_day = loaded.budget.floor_usd_per_day;
                    settings.per_turn_floor = loaded.budget.per_turn_floor;
                    settings.floor_answer_enabled = loaded.answer_enabled;
                    settings.floor_hire_enabled = loaded.hire_enabled;
                    settings.floor_control = loaded.control;
                    settings.floor_terminal_enabled = loaded.terminal_enabled;
                    settings.floor_git_enabled = loaded.git_enabled;
                    settings.floor_diff_enabled = loaded.diff_enabled;
                    duded::boot(
                        &settings,
                        common::seams(
                            &settings,
                            SharedHost::new(),
                            Arc::new(OpenRunner::default()),
                        ),
                    )
                    .is_ok()
                });
                assert_eq!(daemon, cli && reserve_admits, "{journal}:\n{text}");
            }
        }
    }
}

/// A value the daemon does not read is shown as unreadable and counted as a refusal, as the
/// daemon's loader refuses it.
#[test]
fn an_unreadable_value_is_a_refusal_on_both_sides() {
    let directory = tempfile::tempdir().expect("a config directory");
    let path = directory.path().join("config.toml");
    for (text, key, problem) in [
        (
            "[floor.control]\nenabled = \"yes\"\n",
            dudes::config::Key::Control,
            "floor.control holds a value the daemon does not read",
        ),
        (
            "[budget]\nfloor_usd_per_day = -1\n",
            dudes::config::Key::FloorUsdPerDay,
            "budget.floor_usd_per_day holds a value the daemon does not read",
        ),
        (
            "[budget.per_turn_floor]\ntokens = 1.5\n",
            dudes::config::Key::PerTurnTokens,
            "budget.per_turn_floor.tokens holds a value the daemon does not read",
        ),
        (
            "providers = 5\n",
            dudes::config::Key::Codex,
            "providers.codex holds a value the daemon does not read",
        ),
        (
            "[floor.git]\nenabled = 1\n",
            dudes::config::Key::Git,
            "floor.git holds a value the daemon does not read",
        ),
        (
            "[floor.diff]\nenabled = 1\n",
            dudes::config::Key::Diff,
            "floor.diff holds a value the daemon does not read",
        ),
    ] {
        std::fs::write(&path, text).expect("the file");
        let settings = Settings::read(&path).expect("a TOML document");
        assert_eq!(settings.shown(key), "unreadable", "{text}");
        assert_eq!(
            settings
                .problems()
                .first()
                .map(ToString::to_string)
                .as_deref(),
            Some(problem),
            "{text}"
        );
        assert!(
            duded::config::load_operator(&path).is_err(),
            "the daemon refuses it too: {text}"
        );
    }
}

/// The daemon also refuses to start over its journal: a hired agent with a token ceiling needs a
/// token reserve. The CLI cannot see the journal, so an edit that takes a reserve to zero is
/// written with a warning, and the next start then fails as the warning says.
#[test]
fn taking_a_reserve_to_zero_warns_before_the_daemon_refuses_to_start() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    let (config, cli) = written(
        &home,
        "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\ntokens = 1\n",
    );
    let loaded =
        |config: &Path| duded::config::load_operator(config).expect("a file the daemon reads");
    let mut settings = fixture.settings();
    settings.per_turn_floor = loaded(&config).budget.per_turn_floor;
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let hired = Cli::new(&settings.socket(), &settings.runtime_dir)
        .args(common::hire_args(fixture.state.path()))
        .args(["--tokens-per-day", "60000"])
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    running.stop();

    let zeroed = cli
        .clone()
        .args(["config", "set", "budget.per_turn_floor.tokens", "0"])
        .run();
    assert_eq!(zeroed.code(), exit::SUCCESS, "{}", zeroed.output());
    assert_eq!(
        zeroed.stderr,
        "dudes: warning: budget.per_turn_floor.tokens is now zero, and the daemon refuses to start \
         while an agent it has hired, or that agent's goal, has a token ceiling; set it above zero \
         again before the next start if one does\n"
    );
    assert_eq!(
        read(&config),
        "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\ntokens = 0\n"
    );
    let restart = |per_turn_floor| {
        let mut again = fixture.settings();
        again.per_turn_floor = per_turn_floor;
        duded::boot(
            &again,
            common::seams(&again, SharedHost::new(), Arc::new(OpenRunner::default())),
        )
        .map(drop)
        .map_err(|error| format!("{error:#}"))
    };
    let refused = restart(loaded(&config).budget.per_turn_floor)
        .expect_err("the hired agent's token ceiling now has no reserve");
    assert!(
        refused.contains("set [budget.per_turn_floor] tokens above zero"),
        "{refused}"
    );
    assert!(
        restart(settings.per_turn_floor).is_ok(),
        "control: the same journal starts with the reserve the edit removed"
    );

    let unfloored = cli
        .clone()
        .args(["config", "unset", "budget.floor_usd_per_day"])
        .run();
    assert_eq!(unfloored.code(), exit::SUCCESS, "{}", unfloored.output());
    let dollars = cli
        .clone()
        .args(["config", "set", "budget.per_turn_floor.usd", "0"])
        .run();
    assert_eq!(dollars.code(), exit::SUCCESS, "{}", dollars.output());
    assert!(
        dollars.stderr.starts_with(
            "dudes: warning: budget.per_turn_floor.usd is now zero, and the daemon refuses to \
             start while an agent it has hired, or that agent's goal, has a dollar ceiling;"
        ),
        "{}",
        dollars.stderr
    );
    let raised = cli
        .clone()
        .args(["config", "set", "budget.per_turn_floor.tokens", "5"])
        .run();
    assert_eq!(raised.code(), exit::SUCCESS, "{}", raised.output());
    assert_eq!(
        raised.stderr, "",
        "control: a raised reserve warns of nothing"
    );
    let restored = cli
        .args(["config", "unset", "budget.per_turn_floor.tokens"])
        .run();
    assert_eq!(restored.code(), exit::SUCCESS, "{}", restored.output());
    assert_eq!(
        restored.stderr, "",
        "unset restores the default reserve, not zero"
    );
    assert_eq!(
        loaded(&config).budget.per_turn_floor.tokens,
        dudes_types::Tokens::new(20_000)
    );
}

/// Turning an adapter off deletes its table, so a table that also pins a `command` is refused,
/// naming the setting: the next `on` would otherwise run whatever `codex` the daemon's PATH holds.
#[test]
fn turning_off_an_adapter_that_pins_a_command_is_refused() {
    let home = tempfile::tempdir().expect("home");
    let original = "[providers.codex]\ncommand = \"/opt/vetted/codex\"\n\n[providers.pi]\n";
    let (config, cli) = written(home.path(), original);
    for verb in [
        &["set", "providers.codex", "off"][..],
        &["unset", "providers.codex"],
    ] {
        let refused = cli.clone().arg("config").args(verb).run();
        assert_eq!(
            refused.code(),
            exit::OTHER,
            "{verb:?}: {}",
            refused.output()
        );
        assert_eq!(
            refused.stderr,
            "dudes: providers.codex holds other settings (`command`) that turning it off would \
             delete; remove them from the file by hand first if you mean to\n",
            "{verb:?}"
        );
        assert_eq!(read(&config), original, "{verb:?}: nothing was written");
    }
    let bare = cli.args(["config", "set", "providers.pi", "off"]).run();
    assert_eq!(bare.code(), exit::SUCCESS, "{}", bare.output());
    assert_eq!(
        read(&config),
        "[providers.codex]\ncommand = \"/opt/vetted/codex\"\n",
        "control: a table with nothing else in it turns off"
    );
}

/// Every Floor switch, Hire's included, is written into a file that sets no budget,
/// and the daemon loads what was written.
#[test]
fn floor_switches_are_written_without_a_budget() {
    let home = tempfile::tempdir().expect("home");
    let (config, cli) = written(home.path(), "");
    let set = cli
        .clone()
        .args(["config", "set", "floor.hire", "on", "floor.control", "on"])
        .args(["floor.goal", "on", "floor.retire", "on", "floor.git", "on"])
        .args(["floor.diff", "on"])
        .run();
    assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
    assert!(!read(&config).contains("budget"), "{}", read(&config));
    assert!(
        read(&config).contains("[floor.git]\nenabled = true\n"),
        "{}",
        read(&config)
    );
    assert!(
        read(&config).contains("[floor.diff]\nenabled = true\n"),
        "{}",
        read(&config)
    );
    let loaded = duded::config::load_operator(&config).expect("the daemon loads it");
    assert!(loaded.hire_enabled);
    assert!(loaded.git_enabled);
    assert!(loaded.diff_enabled);
    assert_eq!(
        loaded.control,
        duded::settings::FloorControl::On {
            goal: true,
            retire: true,
            renew: false,
        }
    );
    assert_eq!(loaded.budget.floor_usd_per_day, None);

    let off = cli
        .args(["config", "set", "floor.git", "off", "floor.diff", "off"])
        .run();
    assert_eq!(off.code(), exit::SUCCESS, "{}", off.output());
    let loaded = duded::config::load_operator(&config).expect("the daemon loads it");
    assert!(!loaded.git_enabled);
    assert!(!loaded.diff_enabled);
}

/// `dudes config` shows an unset reserve as the daemon's default and reads it as
/// above zero, so a floor ceiling beside it is written. An explicit zero beside that ceiling is
/// still refused, naming the rule the daemon checks at start.
#[test]
fn an_unset_reserve_reads_as_the_default_reserve() {
    let home = tempfile::tempdir().expect("home");
    let (config, cli) = written(home.path(), "");
    let settings = Settings::read(&config).expect("an empty file");
    assert_eq!(settings.shown(dudes::config::Key::PerTurnUsd), "0.25");
    assert_eq!(settings.shown(dudes::config::Key::PerTurnTokens), "20000");
    assert_eq!(settings.shown(dudes::config::Key::FloorUsdPerDay), "none");
    let set = cli
        .clone()
        .args(["config", "set", "budget.floor_usd_per_day", "5"])
        .run();
    assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
    let loaded = duded::config::load_operator(&config).expect("the daemon loads it");
    assert_eq!(
        loaded.budget.per_turn_floor,
        dudes_proto::Spend::DEFAULT_PER_TURN_FLOOR
    );
    let zero = cli
        .args(["config", "set", "budget.per_turn_floor.usd", "0"])
        .run();
    assert_eq!(zero.code(), exit::OTHER, "{}", zero.output());
    assert_eq!(
        zero.stderr,
        "dudes: the daemon would refuse this configuration when it starts, so nothing was \
         written: budget.floor_usd_per_day needs budget.per_turn_floor.usd above zero\n"
    );
}

/// `DUDES_CONFIG` redirects the CLI to a file outside `HOME`, the real file there stays untouched,
/// and a relative value is refused without a fallback to the default.
#[test]
fn dudes_config_redirects_the_cli_and_a_relative_value_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let (default_file, cli) = written(home.path(), "[budget]\nfloor_usd_per_day = 7\n");
    let chosen_dir = tempfile::tempdir().unwrap();
    let chosen = chosen_dir.path().join("ops.toml");
    std::fs::write(&chosen, "[budget]\nfloor_usd_per_day = 3\n").unwrap();

    let shown = cli.clone().env("DUDES_CONFIG", &chosen).arg("config").run();
    assert_eq!(shown.code(), exit::SUCCESS, "{shown:?}");
    assert!(
        shown
            .stdout
            .lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>()
                == ["budget.floor_usd_per_day", "3"]),
        "{shown:?}"
    );

    let edited = cli
        .clone()
        .env("DUDES_CONFIG", &chosen)
        .args(["config", "set", "budget.floor_usd_per_day", "4"])
        .run();
    assert_eq!(edited.code(), exit::SUCCESS, "{edited:?}");
    assert!(read(&chosen).contains("= 4"), "{}", read(&chosen));
    assert_eq!(read(&default_file), "[budget]\nfloor_usd_per_day = 7\n");

    let refused = cli.env("DUDES_CONFIG", "ops.toml").arg("config").run();
    assert_ne!(refused.code(), exit::SUCCESS, "{refused:?}");
    assert!(
        refused
            .stderr
            .contains("DUDES_CONFIG must be an absolute path"),
        "{refused:?}"
    );
}
