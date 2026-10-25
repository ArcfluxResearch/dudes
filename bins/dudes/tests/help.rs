//! Command discovery and focused help through the real CLI.

mod common;

use common::Cli;
use dudes::exit;

const COMMANDS: &[&str] = &[
    "quickstart",
    "start",
    "open",
    "ping",
    "recovery",
    "status",
    "stop",
    "uninstall",
    "upgrade",
    "closing",
    "agents",
    "team",
    "whoami",
    "pause",
    "resume",
    "halt",
    "retire",
    "goal",
    "hire",
    "devices",
    "worktree",
    "rename",
    "look",
    "model",
    "mcp",
    "ask",
    "why",
    "memory",
    "mail",
    "tail",
    "board",
    "ticket",
    "project",
    "config",
    "floor",
    "login",
];

#[test]
fn top_level_help_is_a_short_index_with_every_command() {
    let run = Cli::bare().arg("--help").run();
    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    assert!(run.stderr.is_empty(), "{}", run.output());
    assert!(
        run.stdout.lines().count() <= 100,
        "{} lines",
        run.stdout.lines().count()
    );
    assert!(run.stdout.contains("dudes <command> --help"));
    for &command in COMMANDS {
        assert!(
            run.stdout
                .lines()
                .any(|line| line.split_whitespace().next() == Some(command)),
            "missing {command}"
        );
    }
    assert!(!run.stdout.contains("--agent-usd-per-day"));
}

#[test]
fn every_command_has_focused_help_with_both_help_flags() {
    for &command in COMMANDS {
        for flag in ["--help", "-h"] {
            let run = Cli::bare().args([command, flag]).run();
            assert_eq!(
                run.code(),
                exit::USAGE,
                "{command} {flag}: {}",
                run.output()
            );
            assert!(run.stderr.is_empty(), "{command} {flag}: {}", run.output());
            assert!(
                run.stdout.contains("USAGE"),
                "{command} {flag}: {}",
                run.output()
            );
            assert!(
                run.stdout.contains(command),
                "{command} {flag}: {}",
                run.output()
            );
            assert!(
                !run.stdout.contains("COMMANDS"),
                "{command} {flag}: {}",
                run.output()
            );
        }
    }
}

#[test]
fn quickstart_help_points_to_the_floor_for_sign_in() {
    let run = Cli::bare().args(["quickstart", "--help"]).run();
    assert_eq!(run.code(), exit::USAGE, "{}", run.output());
    assert!(
        run.stdout.contains("sign in to an engine on the Floor"),
        "{}",
        run.output()
    );
    assert!(!run.stdout.contains("prints the dudes login command"));
}
