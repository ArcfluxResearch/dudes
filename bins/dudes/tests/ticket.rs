//! `dudes ticket` against a real daemon: the whole flow in one story, every refusal the methods
//! return with its sentence and exit code, and the help text that names the commands.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Cli, Fixture, OpenRunner, Running, SharedHost, start};
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::board::CardCreateResult;
use dudes_proto::rpc::ticket::{
    TicketAssignOutcome, TicketAssignResult, TicketCreateResult, TicketReadResult,
};
use dudes_proto::{
    AgentBudget, ExitStatus, GoalBudget, InstructionEndBoundary, InstructionGapKind,
    InstructionObservationOwner, Predicate, Record, TicketReason, TurnOutcome,
};
use dudes_supervisor::Input;
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{AgentId, CardStatus, IsolationMode, TurnId, Usd};

struct World {
    fixture: Fixture,
    running: Running,
    runner: Arc<OpenRunner>,
    host: SharedHost,
    cli: Cli,
    repo: PathBuf,
}

fn profile(id: &str, cwd: &Path) -> Profile {
    Profile {
        id: id.to_owned(),
        label: format!("{id} workspace"),
        checks: vec![EnrolledCheck {
            id: "default".to_owned(),
            label: "default passes".to_owned(),
            predicate: Predicate::Command {
                argv: vec!["/usr/bin/true".to_owned()],
                timeout_s: 10,
            },
            may_already_pass: true,
        }],
        cwd: cwd.to_path_buf(),
        written_cwd: cwd.to_path_buf(),
        department: "engineering".to_owned(),
        isolation: IsolationMode::Default,
        agent_budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        goal_budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 4,
        mark: None,
    }
}

fn world() -> World {
    let fixture = Fixture::new();
    let repo = fixture.state.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo = repo.canonicalize().unwrap();
    let mut settings = fixture.settings();
    settings.enrollment = Enrollment {
        profiles: vec![profile("service", &repo)],
    };
    let runner = Arc::new(OpenRunner::default());
    let host =
        SharedHost::with_native_evidence(fixture.state.path().join(duded::boot::JOURNAL_DIR));
    let running = start(&settings, host.clone(), Arc::clone(&runner));
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    World {
        fixture,
        running,
        runner,
        host,
        cli,
        repo,
    }
}

#[test]
fn adding_a_ticket_without_a_project_names_the_one_step_setup() {
    let fixture = Fixture::new();
    let config = fixture.state.path().join("operator.toml");
    std::fs::write(&config, "").unwrap();
    let mut settings = fixture.settings();
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("DUDES_CONFIG", config);
    let refused = cli
        .clone()
        .args(["ticket", "add", "--title", "Fix sign-in"])
        .run();
    assert_eq!(refused.code(), dudes::exit::REFUSED, "{refused:?}");
    assert_eq!(
        refused.stderr,
        "dudes: No project yet. In your project's folder run `dudes project add`. (-32004)\n"
    );
    let json = cli
        .args(["--json", "ticket", "add", "--title", "Fix sign-in"])
        .run();
    assert_eq!(json.code(), dudes::exit::REFUSED, "{json:?}");
    assert_eq!(
        json.json_error().message,
        "That project is not set up any more."
    );
    assert!(
        fixture.records().is_empty(),
        "no ticket or census journals a change"
    );
}

impl World {
    fn dudes<const N: usize>(&self, args: [&str; N]) -> common::Run {
        self.cli.clone().args(args).run()
    }

    fn hire(&self, name: &str, cwd: &Path) -> String {
        let agent = self.hire_active(name, cwd);
        let paused = self.dudes(["pause", &agent]);
        assert_eq!(paused.code(), 0, "{}", paused.output());
        agent
    }

    fn hire_active(&self, name: &str, cwd: &Path) -> String {
        let run = self
            .cli
            .clone()
            .args(["--json", "hire", "--name", name, "--role", "engineer"])
            .args([
                "--department",
                "engineering",
                "--provider",
                "claude",
                "--cwd",
            ])
            .arg(cwd)
            .args(["--usd-per-day", "10"])
            .run();
        assert_eq!(run.code(), 0, "{}", run.output());
        run.json::<HireResult>().agent.to_string()
    }

    /// The id and task text of the `count`th turn the daemon issued, once it has.
    fn turn(&self, count: usize) -> (TurnId, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let turns: Vec<_> = self
                .fixture
                .records()
                .into_iter()
                .filter_map(|record| match record {
                    Record::TurnIssued { turn, task, .. } => Some((turn, task)),
                    _ => None,
                })
                .collect();
            if let Some(found) = turns.get(count - 1) {
                return found.clone();
            }
            assert!(std::time::Instant::now() < deadline, "no turn {count}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The agent's engine finishes `turn` saying `message`, and the daemon's check now passes.
    fn finish(&self, turn: TurnId, message: &str) {
        let agent = self
            .fixture
            .await_record("owner of the issued turn", |record| match record {
                Record::TurnIssued {
                    agent,
                    turn: issued,
                    ..
                } if *issued == turn => Some(*agent),
                _ => None,
            });
        let key =
            self.fixture
                .await_record("exact managed spawn before reap", |record| match record {
                    Record::TurnSpawned {
                        agent: owner,
                        turn: spawned,
                        pid,
                        start_time,
                        boot,
                        ..
                    } if *owner == agent && *spawned == turn => Some(AdoptionKey {
                        pid: *pid,
                        start_time: *start_time,
                        boot: boot.clone(),
                    }),
                    _ => None,
                });
        self.runner.pass();
        self.host.mark_native_reaped(&key);
        self.host.finish_owned_child(
            &key,
            Input::ChildExited {
                final_reply: None,
                elapsed_ms: Some(5),
                turn,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                model_usage: std::collections::BTreeMap::new(),
                outcome: TurnOutcome::Completed,
                last_message: Some(message.to_owned()),
            },
        );
        self.fixture
            .await_record("exact managed end after reap", |record| {
                matches!(record, Record::TurnEnded { agent: owner, turn: ended, .. }
                if *owner == agent && *ended == turn)
                .then_some(())
            });
        self.host.await_native_acknowledged(&key);
    }

    fn settled_review(&self, card: &str, turn: TurnId) -> TicketReadResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let shown = self.dudes(["--json", "ticket", "show", card]);
            assert_eq!(shown.code(), 0, "{}", shown.output());
            let read = shown.json::<TicketReadResult>();
            if let Some(review) = &read.ticket.instruction_review
                && !review.pending_comparison
                && (read.card.claimed_by.is_none()
                    || read.ticket.check.is_some_and(|check| check.pass))
            {
                assert_eq!(read.card.status, CardStatus::Blocked);
                assert_eq!(
                    read.ticket.reason,
                    Some(TicketReason::SharedInstructionsUnknown)
                );
                assert_eq!(review.intervals.len(), 1);
                let interval = &review.intervals[0];
                assert!(matches!(interval.owner,
                    InstructionObservationOwner::Managed { turn: owned, .. } if owned == turn));
                assert!(matches!(interval.end,
                    Some(InstructionEndBoundary::ManagedTurn { turn: ended, .. }) if ended == turn));
                assert!(interval.finished_lsn.is_some());
                for reason in [
                    InstructionGapKind::ConfigurationUnknown,
                    InstructionGapKind::PluginUnknown,
                    InstructionGapKind::PolicyUnknown,
                ] {
                    assert!(
                        review.gaps.iter().any(|gap| gap.reason == reason),
                        "{read:?}"
                    );
                }
                return read;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "instruction review did not settle: {read:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn assert_review_guidance(&self, card: &str, shown: &str, sequence: &str) {
        let refused = self.dudes(["ticket", "accept", card]);
        let expected = format!(
            "Read the current instruction review before acknowledging it. \
             Run `dudes ticket show {card}` to read it. \
             It ends with the exact command for what it shows."
        );
        assert!(refused.stderr.contains(&expected), "{}", refused.stderr);
        assert!(
            !refused.stderr.contains("--expected-seq"),
            "{}",
            refused.stderr
        );
        let expected = format!(
            "To act on exactly this state:\n  \
             dudes ticket accept {card} --expected-seq {sequence}\n  \
             dudes ticket changes {card} --message \"<what to change>\" \
             --expected-seq {sequence}\n\
             If the ticket changes first, Dudes refuses and you read it again.\n"
        );
        assert!(shown.ends_with(&expected), "{shown}");
        let stale = (sequence.parse::<u64>().unwrap() - 1).to_string();
        let stale_accept = self.dudes(["ticket", "accept", card, "--expected-seq", &stale]);
        let stale_changes = self.dudes([
            "ticket",
            "changes",
            card,
            "--message",
            "Cover the empty form.",
            "--expected-seq",
            &stale,
        ]);
        for run in [stale_accept, stale_changes] {
            assert_eq!(run.code(), 3, "{}", run.output());
            assert!(
                run.stderr
                    .contains("This ticket changed while you were looking. Reload and try again."),
                "{}",
                run.stderr
            );
        }
        let after = self.dudes(["ticket", "show", card]);
        assert!(
            after.stdout.contains("State: Needs you"),
            "{}",
            after.stdout
        );
    }

    fn assert_remove_refused_for_review(&self, card: &str) {
        let refused = self.dudes(["ticket", "remove", card]);
        assert_eq!(refused.code(), dudes::exit::REFUSED, "{}", refused.output());
        let expected = "This ticket has an instruction review nobody has acknowledged yet. \
                        Accept it or ask for changes first, then remove the ticket.";
        assert!(refused.stderr.contains(expected), "{}", refused.stderr);
    }

    fn assert_review_required<const N: usize>(&self, args: [&str; N]) {
        let refused = self.cli.clone().arg("--json").args(args).run();
        assert_eq!(refused.code(), dudes::exit::REFUSED, "{}", refused.output());
        assert_eq!(
            refused.json_error().data.unwrap()["reason"],
            "instruction_review_required"
        );
    }

    /// Show `card` until it is in `state`, and answer the text.
    fn show_until(&self, card: &str, state: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let shown = self.dudes(["ticket", "show", card]);
            assert_eq!(shown.code(), 0, "{}", shown.output());
            if shown.stdout.contains(&format!("State: {state}")) {
                return shown.stdout;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never reached {state}: {}",
                shown.stdout
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn add(&self, title: &str) -> String {
        let run = self.dudes(["--json", "ticket", "add", "--title", title]);
        assert_eq!(run.code(), 0, "{}", run.output());
        run.json::<TicketCreateResult>().card.to_string()
    }
}

#[test]
fn the_whole_flow_adds_lists_shows_edits_gives_and_removes() {
    let world = world();
    let cody = world.hire("Cody", &world.repo);

    let added = world.dudes([
        "ticket",
        "add",
        "--title",
        "Fix the login page",
        "--note",
        "It 500s on a blank password.",
    ]);
    assert_eq!(added.code(), 0, "{}", added.output());
    let card = added
        .stdout
        .strip_prefix("ticket ")
        .expect("the first words are the card")
        .trim()
        .to_owned();

    let listed = world.dudes(["ticket", "list"]);
    assert_eq!(listed.code(), 0, "{}", listed.output());
    assert!(listed.stdout.contains(&card), "{}", listed.stdout);
    assert!(listed.stdout.contains("To do"), "{}", listed.stdout);
    assert!(listed.stdout.contains("Fix the login page"));

    let shown = world.dudes(["ticket", "show", &card]);
    assert_eq!(shown.code(), 0, "{}", shown.output());
    for line in [
        "Fix the login page",
        "To do",
        "service",
        "default passes",
        "It 500s on a blank password.",
    ] {
        assert!(shown.stdout.contains(line), "{line}: {}", shown.stdout);
    }

    let edited = world.dudes([
        "ticket",
        "edit",
        &card,
        "--title",
        "Fix the sign-in page",
        "--note",
        "",
    ]);
    assert_eq!(edited.code(), 0, "{}", edited.output());
    let shown = world.dudes(["--json", "ticket", "show", &card]);
    let read = shown.json::<TicketReadResult>();
    assert_eq!(read.card.title, "Fix the sign-in page");
    assert_eq!(read.ticket.description, "");

    let given = world.dudes(["ticket", "give", &card, "--agent", &cody]);
    assert_eq!(given.code(), 0, "{}", given.output());
    assert_eq!(given.stdout, "Queued. Cody is paused.\n");

    let shown = world.dudes(["--json", "ticket", "show", &card]);
    let read = shown.json::<TicketReadResult>();
    assert_eq!(read.card.status, CardStatus::Claimed);
    assert_eq!(read.card.claimed_by.map(|a| a.to_string()), Some(cody));
    assert_eq!(read.position, Some(1));
    let text = world.dudes(["ticket", "show", &card]);
    assert!(
        text.stdout.contains("queued for Cody, 1st"),
        "{}",
        text.stdout
    );

    let todo = world.dudes(["ticket", "list", "--status", "todo"]);
    assert!(todo.stdout.contains(&card), "{}", todo.stdout);
    assert!(todo.stdout.contains("Cody"), "{}", todo.stdout);
    let working = world.dudes(["ticket", "list", "--status", "working"]);
    assert_eq!(working.code(), 0, "{}", working.output());
    assert_eq!(working.stdout, "");

    let removed = world.dudes(["ticket", "remove", &card]);
    assert_eq!(removed.code(), 0, "{}", removed.output());
    let listed = world.dudes(["ticket", "list"]);
    assert_eq!(listed.code(), 0, "{}", listed.output());
    assert_eq!(listed.stdout, "");
}

#[test]
fn give_answers_at_once_in_json_and_carries_the_message() {
    let world = world();
    let cody = world.hire("Cody", &world.repo);
    let first = world.add("First");
    let second = world.add("Second");
    for (card, position) in [(&first, 1), (&second, 2)] {
        let run = world.dudes([
            "--json",
            "ticket",
            "give",
            card,
            "--agent",
            &cody,
            "--message",
            "Please hurry.",
        ]);
        assert_eq!(run.code(), 0, "{}", run.output());
        let TicketAssignOutcome::Queued { position: got, .. } =
            run.json::<TicketAssignResult>().outcome
        else {
            panic!("a paused agent only queues");
        };
        assert_eq!(got, position);
    }
    let text = world.dudes(["ticket", "give", &first, "--agent", &cody]);
    assert_eq!(text.stdout, "Queued. Cody is paused.\n");
}

#[test]
fn add_can_give_the_new_ticket_in_the_same_command() {
    let world = world();
    let cody = world.hire("Cody", &world.repo);
    let run = world.dudes(["ticket", "add", "--title", "Both", "--give-to", &cody]);
    assert_eq!(run.code(), 0, "{}", run.output());
    assert!(run.stdout.starts_with("ticket "), "{}", run.stdout);
    assert!(
        run.stdout.ends_with("Queued. Cody is paused.\n"),
        "{}",
        run.stdout
    );

    let refused = world.dudes([
        "ticket",
        "add",
        "--title",
        "Orphan",
        "--give-to",
        &AgentId::from_parts(9, 9).to_string(),
    ]);
    assert_eq!(refused.code(), 3, "{}", refused.output());
    assert!(
        refused.stdout.starts_with("ticket "),
        "the ticket exists: {}",
        refused.stdout
    );
    assert!(
        refused
            .stderr
            .contains("That agent is not available for this project."),
        "{}",
        refused.stderr
    );
}

#[test]
fn every_refusal_prints_its_sentence_with_the_clis_existing_exit_code() {
    let world = world();
    let cody = world.hire("Cody", &world.repo);
    let elsewhere = world.hire("Ellis", world.fixture.state.path());
    let card = world.add("Subject");
    let plain = world
        .dudes([
            "--json",
            "board",
            "create",
            "--title",
            "Plain",
            "--department",
            "engineering",
        ])
        .json::<CardCreateResult>()
        .card
        .to_string();
    let nobody = AgentId::from_parts(9, 9).to_string();

    let refusals: Vec<(Vec<&str>, u8, &str)> = vec![
        (
            vec![
                "ticket",
                "edit",
                &card,
                "--title",
                "x",
                "--expected-seq",
                "9",
            ],
            4,
            "This ticket changed while you were looking. Reload and try again.",
        ),
        (
            vec!["ticket", "show", &plain],
            3,
            "That card is not a ticket.",
        ),
        (
            vec!["ticket", "edit", &plain, "--title", "x"],
            3,
            "That card is not a ticket.",
        ),
        (
            vec!["ticket", "give", &plain, "--agent", &cody],
            3,
            "That card is not a ticket.",
        ),
        (
            vec!["ticket", "remove", &plain],
            3,
            "That card is not a ticket.",
        ),
        (
            vec!["ticket", "add", "--title", "x", "--project", "gone"],
            3,
            "That project is not set up any more.",
        ),
        (
            vec!["ticket", "give", &card, "--agent", &nobody],
            3,
            "That agent is not available for this project.",
        ),
        (
            vec!["ticket", "give", &card, "--agent", &elsewhere],
            3,
            "That agent was hired before projects were recorded. Hire a new agent in this project with `dudes hire`.",
        ),
    ];
    for (args, code, sentence) in refusals {
        let run = world.cli.clone().args(&args).run();
        assert_eq!(run.code(), code, "{args:?}: {}", run.output());
        assert!(run.stdout.is_empty(), "{args:?}: {}", run.stdout);
        assert!(run.stderr.contains(sentence), "{args:?}: {}", run.stderr);
    }

    let mut cards = vec![card];
    for index in 1..8 {
        cards.push(world.add(&format!("Queued {index}")));
    }
    for card in &cards {
        let run = world.dudes(["ticket", "give", card, "--agent", &cody]);
        assert_eq!(run.code(), 0, "{}", run.output());
    }
    let ninth = world.add("Ninth");
    let full = world.dudes(["ticket", "give", &ninth, "--agent", &cody]);
    assert_eq!(full.code(), 3, "{}", full.output());
    assert!(
        full.stderr
            .contains("That agent already has 8 tickets waiting."),
        "{}",
        full.stderr
    );

    let json = world.dudes(["--json", "ticket", "give", &ninth, "--agent", &cody]);
    assert_eq!(json.code(), 3);
    assert_eq!(
        json.json_error().message,
        "That agent already has 8 tickets waiting."
    );
}

#[test]
fn a_malformed_request_names_the_field_and_exits_4() {
    let world = world();
    let run = world.dudes(["ticket", "add", "--title", ""]);
    assert_eq!(run.code(), 4, "{}", run.output());
    assert!(run.stderr.contains("title"), "{}", run.stderr);
    let card = world.add("Subject");
    let run = world.dudes(["ticket", "edit", &card]);
    assert_eq!(run.code(), 1, "{}", run.output());
}

#[test]
fn bad_arguments_exit_1_and_send_nothing() {
    let cli = Cli::bare();
    for args in [
        vec!["ticket"],
        vec!["ticket", "bogus"],
        vec!["ticket", "add"],
        vec!["ticket", "give", "00000000000000000000000000000000"],
        vec!["ticket", "show"],
        vec!["ticket", "list", "--status", "bogus"],
        vec!["ticket", "list", "--agent", "x"],
    ] {
        let run = cli.clone().args(&args).run();
        assert_eq!(run.code(), 1, "{args:?}: {}", run.output());
    }
}

#[test]
fn help_lists_the_ticket_commands_with_one_worked_example() {
    let run = Cli::bare().arg("--help").run();
    assert_eq!(run.code(), 0, "{}", run.output());
    for command in [
        "ticket add --title",
        "ticket edit <card>",
        "ticket give <card> --agent",
        "ticket remove <card>",
        "ticket list [--status",
        "ticket show <card>",
        "ticket open <card> [--editor <absolute-program> | --print]",
    ] {
        assert!(run.stdout.contains(command), "{command}");
    }
    assert!(run.stdout.contains("--json never launches an editor"));
    assert!(
        run.stdout
            .contains("Lease verification requires macOS or Linux")
    );
    assert_eq!(run.stdout.matches("EXAMPLE: tickets").count(), 1);
    for command in [
        "ticket accept <card>",
        "ticket changes <card> --message",
        "ticket stop <card>",
    ] {
        assert!(run.stdout.contains(command), "{command}");
    }
}

#[test]
fn the_loop_gives_by_name_reviews_accepts_asks_for_changes_stops_and_removes() {
    let world = world();
    let cody = world.hire_active("Cody", &world.repo);

    let first = world.add("Fix the sign-in page");
    let given = world.dudes(["ticket", "give", &first, "--agent", "Cody"]);
    assert_eq!(given.code(), 0, "{}", given.output());
    assert_eq!(given.stdout, "Cody is working on it.\n");
    let shown = world.dudes(["ticket", "show", &first]);
    assert!(
        shown.stdout.contains("State: Working, Cody"),
        "{}",
        shown.stdout
    );

    let (turn, _) = world.turn(1);
    world.finish(turn, "I fixed the blank password case.");
    let read = world.settled_review(&first, turn);
    let sequence = read.card.seq.0.to_string();
    let shown = world.show_until(&first, "Needs you");
    for line in [
        "The agent says:",
        "  I fixed the blank password case.",
        "Last recorded check: passed, exit status 0, ",
        "Open the Floor to see the changed files.",
    ] {
        assert!(shown.contains(line), "{line}: {shown}");
    }

    world.assert_review_required(["ticket", "accept", &first]);
    world.assert_review_guidance(&first, &shown, &sequence);
    let accepted = world.dudes(["ticket", "accept", &first, "--expected-seq", &sequence]);
    assert_eq!(accepted.code(), 0, "{}", accepted.output());
    assert_eq!(accepted.stdout, "Ticket accepted.\n");
    let shown = world.dudes(["ticket", "show", &first]);
    assert!(shown.stdout.contains("State: Done"), "{}", shown.stdout);

    let second = world.add("Fix the sign-up page");
    let given = world.dudes(["ticket", "give", &second, "--agent", &cody]);
    assert_eq!(given.stdout, "Cody is working on it.\n");
    let (turn, _) = world.turn(2);
    world.finish(turn, "Done.");
    let read = world.settled_review(&second, turn);
    let sequence = read.card.seq.0.to_string();
    world.show_until(&second, "Needs you");

    world.assert_review_required([
        "ticket",
        "changes",
        &second,
        "--message",
        "Also cover the empty form.",
    ]);

    let changes = world.dudes([
        "ticket",
        "changes",
        &second,
        "--message",
        "Also cover the empty form.",
        "--expected-seq",
        &sequence,
    ]);
    assert_eq!(changes.code(), 0, "{}", changes.output());
    assert_eq!(changes.stdout, "Cody is working on it.\n");
    let (turn, task) = world.turn(3);
    assert!(task.contains("Also cover the empty form."), "{task}");
    let shown = world.dudes(["ticket", "show", &second]);
    assert!(shown.stdout.contains("Round: 2"), "{}", shown.stdout);

    let stopped = world.dudes(["ticket", "stop", &second]);
    assert_eq!(stopped.code(), 0, "{}", stopped.output());
    assert_eq!(stopped.stdout, "Ticket stopped.\n");
    let shown = world.dudes(["ticket", "show", &second]);
    assert!(shown.stdout.contains("State: To do"), "{}", shown.stdout);

    world.finish(turn, "Stopped after the empty-form change.");
    let read = world.settled_review(&second, turn);
    let sequence = read.card.seq.0.to_string();
    world.assert_remove_refused_for_review(&second);
    let acknowledged = world.dudes(["ticket", "accept", &second, "--expected-seq", &sequence]);
    assert_eq!(acknowledged.code(), 0, "{}", acknowledged.output());
    assert_eq!(
        acknowledged.stdout,
        "Instruction review acknowledged. The ticket remains open.\n"
    );

    let removed = world.dudes(["ticket", "remove", &second]);
    assert_eq!(removed.code(), 0, "{}", removed.output());
    let listed = world.dudes(["ticket", "list"]);
    assert!(listed.stdout.contains(&first), "{}", listed.stdout);
    assert!(!listed.stdout.contains(&second), "{}", listed.stdout);
}

#[test]
fn an_agent_name_must_match_exactly_one_live_agent() {
    let world = world();
    let one = world.hire("Cody", &world.repo);
    let two = world.hire("Cody", &world.repo);
    world.hire("Codex", &world.repo);
    let card = world.add("Subject");

    for agent in ["Nobody", "cody"] {
        let run = world.dudes(["ticket", "give", &card, "--agent", agent]);
        assert_eq!(run.code(), 4, "{}", run.output());
        assert!(run.stdout.is_empty(), "{}", run.stdout);
        let sentence = format!("No live agent is named \"{agent}\".");
        assert!(run.stderr.contains(&sentence), "{}", run.stderr);
    }

    let run = world.dudes(["ticket", "give", &card, "--agent", "Cody"]);
    assert_eq!(run.code(), 4, "{}", run.output());
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    for part in [
        "More than one live agent is named \"Cody\"",
        one.as_str(),
        two.as_str(),
        "Use an agent id.",
    ] {
        assert!(run.stderr.contains(part), "{part}: {}", run.stderr);
    }

    let by_id = world.dudes(["ticket", "give", &card, "--agent", &one]);
    assert_eq!(by_id.code(), 0, "{}", by_id.output());
    assert_eq!(by_id.stdout, "Queued. Cody is paused.\n");
    let added = world.dudes(["ticket", "add", "--title", "Named", "--give-to", "Codex"]);
    assert_eq!(added.code(), 0, "{}", added.output());
    assert!(
        added.stdout.ends_with("Queued. Codex is paused.\n"),
        "{}",
        added.stdout
    );
}

#[test]
fn the_review_commands_refuse_with_the_daemons_sentence_and_exit_code() {
    let world = world();
    let cody = world.hire("Cody", &world.repo);
    let card = world.add("Subject");
    let plain = world
        .dudes([
            "--json",
            "board",
            "create",
            "--title",
            "Plain",
            "--department",
            "engineering",
        ])
        .json::<CardCreateResult>()
        .card
        .to_string();

    let refusals: Vec<(Vec<&str>, u8, &str)> = vec![
        (
            vec!["ticket", "accept", &card],
            3,
            "You can't do that to a ticket that is To do.",
        ),
        (
            vec!["ticket", "stop", &card],
            3,
            "You can't do that to a ticket that is To do.",
        ),
        (
            vec![
                "ticket",
                "changes",
                &card,
                "--message",
                "x",
                "--agent",
                &cody,
            ],
            3,
            "You can't do that to a ticket that is To do.",
        ),
        (
            vec!["ticket", "accept", &card, "--expected-seq", "9"],
            4,
            "This ticket changed while you were looking. Reload and try again.",
        ),
        (
            vec!["ticket", "accept", &plain],
            3,
            "That card is not a ticket.",
        ),
        (
            vec!["ticket", "stop", &plain],
            3,
            "That card is not a ticket.",
        ),
    ];
    for (args, code, sentence) in refusals {
        let run = world.cli.clone().args(&args).run();
        assert_eq!(run.code(), code, "{args:?}: {}", run.output());
        assert!(run.stdout.is_empty(), "{args:?}: {}", run.stdout);
        assert!(run.stderr.contains(sentence), "{args:?}: {}", run.stderr);
    }
}

#[test]
fn changes_needs_a_message_and_the_review_commands_take_a_card() {
    let cli = Cli::bare();
    let card = "00000000000000000000000004";
    for args in [
        vec!["ticket", "changes", card],
        vec!["ticket", "changes", "--message", "x"],
        vec!["ticket", "accept"],
        vec!["ticket", "stop"],
        vec!["ticket", "accept", card, "--message", "x"],
        vec!["ticket", "stop", card, "--agent", "Cody"],
    ] {
        let run = cli.clone().args(&args).run();
        assert_eq!(run.code(), 1, "{args:?}: {}", run.output());
    }
    let run = cli.clone().args(["ticket", "changes", card]).run();
    assert!(
        run.stderr.contains("`--message` is required"),
        "{}",
        run.stderr
    );
    let run = cli
        .clone()
        .args(["ticket", "accept", card, "--message", "x"])
        .run();
    assert!(
        run.stderr.contains("unknown option `--message`"),
        "{}",
        run.stderr
    );
}

#[test]
fn opening_a_ticket_cannot_print_and_launch_an_editor() {
    let run = Cli::bare()
        .args([
            "ticket",
            "open",
            "00000000000000000000000004",
            "--print",
            "--editor",
            "/usr/bin/true",
        ])
        .run();

    assert_eq!(run.code(), 1, "{}", run.output());
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "dudes: `--print` cannot be combined with `--editor`\n\
         Run `dudes --help` for every command and its options.\n"
    );
}

#[test]
fn opening_an_assigned_ticket_prints_its_recorded_directory() {
    let world = world();
    let agent = world.hire("Cody", &world.repo);
    let card = world.add("Review the recorded checkout");
    let given = world.dudes(["ticket", "give", &card, "--agent", &agent]);
    assert_eq!(given.code(), 0, "{}", given.output());

    let opened = world.dudes(["ticket", "open", &card, "--print"]);
    assert_eq!(opened.code(), 0, "{}", opened.output());
    assert_eq!(opened.stdout, format!("{}\n", world.repo.display()));
    assert_eq!(opened.stderr, "");
}

#[test]
fn machine_checkout_output_cannot_be_combined_with_an_explicit_editor() {
    const CARD: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    for args in [
        vec![
            "--json",
            "ticket",
            "open",
            CARD,
            "--editor",
            "/usr/bin/true",
        ],
        vec![
            "ticket",
            "open",
            CARD,
            "--editor",
            "/usr/bin/true",
            "--json",
        ],
    ] {
        let run = Cli::bare().args(args).run();
        assert_eq!(run.code(), 1, "{}", run.output());
        assert_eq!(run.stdout, "");
        assert_eq!(
            run.stderr,
            "dudes: `--json` cannot be combined with `--editor`\nRun `dudes --help` for every command and its options.\n"
        );
    }
}

#[test]
fn opening_without_editor_configuration_prints_the_explicit_launch_form() {
    let world = world();
    let agent = world.hire("Cody", &world.repo);
    let card = world.add("Review locally");
    assert_eq!(
        world
            .dudes(["ticket", "give", &card, "--agent", &agent])
            .code(),
        0
    );

    let opened = world.dudes(["ticket", "open", &card]);
    assert_eq!(opened.code(), 0, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!(
            "{}\nTo open an editor, run `dudes ticket open {card} --editor /absolute/program`.\n",
            world.repo.display()
        )
    );
    assert_eq!(opened.stderr, "");
}

#[cfg(debug_assertions)]
#[test]
fn checkout_reads_survive_a_stopped_writer_while_ticket_changes_stay_fenced() {
    let world = world();
    let agent = world.hire("Cody", &world.repo);
    let card = world.add("Review after a write failure");
    assert_eq!(
        world
            .dudes(["ticket", "give", &card, "--agent", &agent])
            .code(),
        0
    );
    world.running.append_failure.short_write_next(5);
    let failed = world.dudes(["--json", "ticket", "add", "--title", "Fail this append"]);
    assert_eq!(failed.json_error().code, -32603, "{}", failed.output());
    let before = world.dudes(["--json", "ping"]).json::<serde_json::Value>()["lsn"].clone();
    let shown = world.dudes(["ticket", "show", &card]);
    assert_eq!(shown.code(), 0, "{}", shown.output());

    let opened = world.dudes(["ticket", "open", &card, "--print"]);
    assert_eq!(opened.code(), 0, "{}", opened.output());
    assert_eq!(opened.stdout, format!("{}\n", world.repo.display()));
    let changed = world.dudes(["--json", "ticket", "add", "--title", "Still fenced"]);
    assert_eq!(changed.json_error().code, -32006, "{}", changed.output());
    assert_eq!(
        world.dudes(["--json", "ping"]).json::<serde_json::Value>()["lsn"],
        before
    );
}
