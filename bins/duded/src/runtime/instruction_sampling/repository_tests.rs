use super::*;
use crate::runtime::repository_hire_tests::{Runner, Setup};
use crate::runtime::work_test_support as common;
use crate::runtime::{Daemon, ExitCause};
use dudes_proto::instruction::{InstructionIssueAssociation, InstructionSettlement};
use dudes_proto::record::ConversationChargeScope;
use dudes_proto::repository::{
    RepositoryEntry, RepositorySet, RepositoryTicketCreated, RepositoryTicketStarted,
    RepositoryTurnIssued, TicketDeclaration, TicketStartRepositories,
};
use dudes_proto::{Actor, CheckBaseline, Entry, Goal, GoalBudget, Predicate, Record, TicketCheck};
use dudes_supervisor::Clock;
use dudes_types::{CardId, CardStatus, GoalId, Lsn, Seq, TurnId};

type TestDaemon = Daemon<common::SharedHost, Runner, common::TestClock, common::FixtureRng>;

fn append(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let appended = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let lsn = appended.lsn();
    let (main, checkpoint) = appended.into_parts();
    for (lsn, entry) in std::iter::once(main).chain(checkpoint) {
        daemon
            .sup
            .handle(
                Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    let _ = daemon.sup.take_effects();
    lsn
}

fn recorded(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let (lsn, crc) = daemon.ctx.writer.append_witnessed(&entry).unwrap();
    daemon.ctx.projections.apply(lsn, crc, &entry).unwrap();
    daemon
        .sup
        .handle(
            Input::Replayed { lsn, entry },
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    let _ = daemon.sup.take_effects();
    lsn
}

fn fixture() -> (Setup, TestDaemon, AgentId) {
    fixture_with(false)
}

fn setup_with_profile() -> Setup {
    let mut setup = Setup::new();
    let profile = setup.fixture.runtime.path().join("profile");
    std::fs::create_dir_all(profile.join(".claude")).unwrap();
    std::fs::write(profile.join(".claude/CLAUDE.md"), "profile instructions").unwrap();
    setup
        .settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "HOME",
            profile.to_str().unwrap(),
        ));
    setup
}

fn fixture_with(unconfined: bool) -> (Setup, TestDaemon, AgentId) {
    let setup = setup_with_profile();
    let (mut running, hires, params) = if unconfined {
        setup.start_from(setup.boot_unconfined())
    } else {
        setup.start(true)
    };
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    let mut daemon = if unconfined {
        setup.boot_unconfined()
    } else {
        setup.boot(false)
    };
    daemon.started().unwrap();
    (setup, daemon, agent)
}

fn ticket(daemon: &mut TestDaemon, agent: AgentId) -> RepositoryTurnGrant {
    ticket_declaring(daemon, agent, &["web", "api"])
}

#[expect(
    clippy::too_many_lines,
    reason = "one immutable ticket prefix retains the real declared set and check authority"
)]
fn ticket_declaring(
    daemon: &mut TestDaemon,
    agent: AgentId,
    declared: &[&str],
) -> RepositoryTurnGrant {
    let row = &daemon.sup.world().agents[&agent];
    let HireRepositories::Scoped(hire) = &row.repositories else {
        unreachable!()
    };
    let hired_lsn = row.mcp.hire_lsn;
    let declaration = TicketDeclaration {
        project: hire.project.id.clone(),
        catalog_lsn: hire.catalog_lsn,
        repositories: RepositorySet::new(
            dudes_proto::repository::RepoId::parse(declared[0]).unwrap(),
            declared
                .iter()
                .copied()
                .map(|id| {
                    let repo_id = dudes_proto::repository::RepoId::parse(id).unwrap();
                    RepositoryEntry {
                        value: hire
                            .repositories
                            .get(&repo_id)
                            .unwrap()
                            .recorded
                            .origin
                            .clone(),
                        repo_id,
                    }
                })
                .collect(),
        )
        .unwrap(),
    };
    let check_repo = declaration.repositories.primary_id().clone();
    let check_root = hire
        .repositories
        .get(&check_repo)
        .unwrap()
        .recorded
        .execution
        .clone();
    let check = TicketCheck {
        id: "verify".into(),
        label: "Check instructions".into(),
        predicate: Predicate::Command {
            argv: vec!["/usr/bin/true".into()],
            timeout_s: 1,
        },
    };
    let card = CardId::from_parts(3, 1);
    recorded(
        daemon,
        Record::TicketCreatedWithRepositories(Box::new(RepositoryTicketCreated {
            card,
            title: "Observe two repositories".into(),
            description: String::new(),
            department: "eng".into(),
            declaration: declaration.clone(),
            done_when: check.clone(),
            check_repo: check_repo.clone(),
            by: Actor::System,
            request: None,
        })),
    );
    recorded(
        daemon,
        Record::TicketAssigned {
            card,
            agent,
            by: Actor::System,
            expected_seq: Seq::ZERO,
            priority: None,
            message: None,
        },
    );
    let goal = Goal {
        id: GoalId::from_parts(3, 2),
        text: "Observe instructions".into(),
        predicate: check.predicate.clone(),
        completion_check: crate::ticket_index::goal_check(&check).map(|reference| {
            crate::ticket_index::scoped_check_reference(
                &declaration.project,
                &check_repo,
                reference,
            )
        }),
        baseline: Some(CheckBaseline::MayAlreadyPass),
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 4,
    };
    recorded(
        daemon,
        Record::TicketStartedWithRepositories(Box::new(RepositoryTicketStarted {
            card,
            agent,
            expected_seq: Seq(1),
            round: 1,
            goal,
            repositories: TicketStartRepositories {
                hired_lsn,
                declaration,
                check,
                check_repo,
                check_root,
            },
        })),
    );
    let world = daemon.sup.world();
    let HireRepositories::Scoped(hire) = &world.agents[&agent].repositories else {
        unreachable!()
    };
    RepositoryTurnGrant::select(
        hired_lsn,
        hire,
        world.active_repository_round(agent).unwrap(),
    )
    .unwrap()
}

fn sample(
    daemon: &mut TestDaemon,
    owner: InstructionObservationOwner,
    grant: &RepositoryTurnGrant,
    baseline: Option<&RepositoryInstructionStart>,
) -> (
    RepositoryInstructionStart,
    Option<Vec<dudes_proto::repository_instruction::RepositoryInstructionComparison>>,
) {
    let job = daemon
        .ctx
        .repository_instruction_job(
            &daemon.sup,
            InstructionRequest(41),
            owner,
            grant,
            &ModelSelection::provider_default(),
            baseline,
        )
        .unwrap();
    assert!(
        daemon
            .ctx
            .instruction_sampler
            .submit_job(SampleJob::Repositories(job))
            .is_ok()
    );
    let crate::ports::Job::Input(input) = daemon.jobs.recv_timeout(SAMPLE_DEADLINE).unwrap() else {
        panic!("sample completion")
    };
    let Input::RepositoryInstructionsSampled {
        sample,
        comparisons,
        ..
    } = *input
    else {
        panic!("batch sample")
    };
    (*sample, comparisons)
}

fn issue(daemon: &mut TestDaemon, agent: AgentId, turn: TurnId, grant: RepositoryTurnGrant) {
    let round = grant.ticket.as_ref().unwrap();
    let seq = daemon
        .ctx
        .projections
        .card(round.card)
        .unwrap()
        .unwrap()
        .seq;
    recorded(
        daemon,
        Record::CardTransition {
            card: round.card,
            from: CardStatus::Doing,
            to: CardStatus::Review,
            by: Actor::System,
            expected_seq: seq,
            reason: None,
        },
    );
    let context_lsn = recorded(
        daemon,
        Record::GoalCleared {
            agent,
            goal: round.goal,
        },
    );
    let Record::BrowserNoteQueued { browser, .. } =
        dudes_proto::record::samples::record(dudes_proto::RecordKind::BrowserNoteQueued)
    else {
        unreachable!()
    };
    let task = "Observe instructions";
    let note_lsn = recorded(
        daemon,
        Record::ConversationMessageQueued {
            agent,
            text: task.into(),
            browser,
            context_lsn,
            charge_scope: ConversationChargeScope::AgentAndFloor,
        },
    );
    let lsn_of_decision = recorded(
        daemon,
        Record::ConversationTurnDecided {
            agent,
            note_lsn,
            context_lsn,
            charge_scope: ConversationChargeScope::AgentAndFloor,
            input_digest: "finite".into(),
            gate_trace: Vec::new(),
            decision: dudes_proto::NoteAdmission::Issue { task: task.into() },
        },
    );
    let payload = Record::ConversationTurnIssued {
        agent,
        turn,
        note_lsn,
        context_lsn,
        charge_scope: ConversationChargeScope::AgentAndFloor,
        task: task.into(),
        session: None,
        model_selection: ModelSelection::provider_default(),
        lsn_of_decision,
    }
    .issue_payload()
    .unwrap()
    .to_owned();
    recorded(
        daemon,
        Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued { grant, payload })),
    );
    assert!(daemon.sup.world().turns[&turn].goal.is_none());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one physical attempt covers live sampling, recovery and both projection replays"
)]
fn all_hire_sampling_and_recovery_review_an_undeclared_repository_without_expanding_the_grant() {
    let (setup, mut daemon, agent) = fixture();
    let grant = ticket(&mut daemon, agent);
    assert_eq!(
        grant
            .repositories
            .entries()
            .iter()
            .map(|member| member.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api"]
    );
    let turn = TurnId::from_parts(3, 3);
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: InstructionIssueAssociation::Prospective,
    };
    let copies = match &daemon.sup.world().agents[&agent].repositories {
        HireRepositories::Scoped(hire) => hire
            .repositories
            .entries()
            .iter()
            .map(|member| member.value.recorded.execution.directory.clone())
            .collect::<Vec<_>>(),
        HireRepositories::Legacy(_) => unreachable!(),
    };
    for path in &copies {
        std::fs::write(path.join("AGENTS.md"), "initial instructions").unwrap();
    }
    let (start, comparisons) = sample(&mut daemon, owner, &grant, None);
    assert!(comparisons.is_none());
    assert_eq!(
        start
            .members
            .iter()
            .map(|member| member.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    for member in &start.members {
        assert!(!member.snapshot.inputs.is_empty());
    }
    assert!(
        start.members[2].snapshot.inputs.iter().any(|input| input
            .path
            .to_path()
            .is_ok_and(|path| path == copies[2].join("AGENTS.md"))
            && matches!(
                input.fingerprint,
                dudes_proto::InstructionFingerprint::File { .. }
            )),
        "third repository must be observed"
    );
    let baseline_lsn = append(
        &mut daemon,
        Record::InstructionObservationBatchStarted(start.clone()),
    );
    issue(&mut daemon, agent, turn, grant.clone());
    let mut changed_current = daemon.sup.world().clone();
    changed_current.agents.get_mut(&agent).unwrap().mcp.hire_lsn = Lsn(999);
    assert_eq!(
        recorded_repository_hire(&changed_current, owner, &grant)
            .unwrap()
            .0,
        grant.hired_lsn
    );
    let mut wrong_grant = grant.clone();
    wrong_grant.hired_lsn = Lsn(999);
    assert!(recorded_repository_hire(&changed_current, owner, &wrong_grant).is_none());
    std::fs::write(
        copies[2].join("AGENTS.md"),
        "operator changed the third repository",
    )
    .unwrap();
    append(
        &mut daemon,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: dudes_types::Provenance::Observed,
            exit: dudes_proto::ExitStatus::UNKNOWN,
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            last_message: None,
            session: None,
            outcome: None,
            final_reply: None,
            elapsed_ms: None,
            session_usage: None,
            step_digest: None,
            context: None,
        },
    );
    drop(daemon);
    let mut daemon = setup.boot(false);
    daemon.started().unwrap();
    let crate::ports::Job::Input(input) = daemon.jobs.recv_timeout(SAMPLE_DEADLINE).unwrap() else {
        panic!("recovery sample")
    };
    daemon.pump(*input).unwrap();
    let attempt = daemon
        .sup
        .world()
        .instructions
        .attempt(baseline_lsn)
        .unwrap();
    let finish = attempt.finished.as_ref().unwrap();
    assert!(matches!(
        finish.settlement,
        InstructionSettlement::Compared {
            comparison: dudes_proto::instruction_state::InstructionComparisonEvidence::Repositories(
                _
            ),
            ..
        }
    ));
    let batch = setup
        .fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::InstructionObservationBatchFinished(batch) => Some(batch),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        batch
            .members
            .iter()
            .map(|member| member.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    let InstructionSettlement::Compared {
        comparison,
        recovery,
        ..
    } = &batch.members[2].settlement
    else {
        panic!("third comparison")
    };
    assert!(*recovery);
    assert!(
        !comparison.changes.is_empty(),
        "third finish {comparison:?}; start {:?}",
        start.members[2]
    );
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .pending_for_goal(grant.ticket.as_ref().unwrap().goal)
    );
    let review = daemon
        .sup
        .world()
        .instructions
        .review(grant.ticket.as_ref().unwrap().card, digest)
        .unwrap()
        .unwrap();
    assert!(review.changed_paths.iter().any(|change| {
        change
            .repo_id
            .as_ref()
            .is_some_and(|repo| repo.as_str() == "docs")
    }));
    assert_eq!(
        daemon.sup.world().turns[&turn]
            .repositories
            .as_ref()
            .unwrap()
            .grant(),
        Some(&grant)
    );
    assert_eq!(
        &daemon.sup.world().instructions,
        &daemon.ctx.projections.instruction_state().unwrap()
    );
    let expected = daemon.ctx.projections.instruction_state().unwrap();
    drop(daemon);
    for _ in 0..2 {
        let mut projection = dudes_journal::Projections::open(
            &setup.fixture.state.path().join("instruction-pair.redb"),
        )
        .unwrap();
        dudes_journal::replay(
            &dudes_journal::Reader::new(&setup.fixture.journal_dir()),
            &mut projection,
        )
        .unwrap();
        assert_eq!(projection.instruction_state().unwrap(), expected);
    }
    let mut rebuilt = dudes_journal::Projections::in_memory().unwrap();
    dudes_journal::replay(
        &dudes_journal::Reader::new(&setup.fixture.journal_dir()),
        &mut rebuilt,
    )
    .unwrap();
    assert_eq!(rebuilt.instruction_state().unwrap(), expected);
    assert!(setup.host.spawned().is_empty());
}

#[expect(
    clippy::too_many_lines,
    reason = "one missing-copy history proves both serialized completion refusals"
)]
fn unknown_copy(replace: bool) {
    let (setup, mut daemon, agent) = fixture();
    let grant = ticket(&mut daemon, agent);
    let turn = TurnId::from_parts(4, 1);
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: InstructionIssueAssociation::Prospective,
    };
    let HireRepositories::Scoped(hire) = &daemon.sup.world().agents[&agent].repositories else {
        unreachable!()
    };
    let third = hire.repositories.entries()[2]
        .value
        .recorded
        .execution
        .directory
        .clone();
    std::fs::write(third.join("AGENTS.md"), "before").unwrap();
    let (start, _) = sample(&mut daemon, owner, &grant, None);
    let baseline = append(
        &mut daemon,
        Record::InstructionObservationBatchStarted(start.clone()),
    );
    issue(&mut daemon, agent, turn, grant.clone());
    std::fs::rename(&third, third.with_file_name("retained-third")).unwrap();
    if replace {
        std::fs::create_dir(&third).unwrap();
        std::fs::write(third.join("AGENTS.md"), "replacement instructions").unwrap();
    }
    let end_lsn = append(
        &mut daemon,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: dudes_types::Provenance::Observed,
            exit: dudes_proto::ExitStatus::UNKNOWN,
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            last_message: None,
            session: None,
            outcome: None,
            final_reply: None,
            elapsed_ms: None,
            session_usage: None,
            step_digest: None,
            context: None,
        },
    );
    let (ending, comparisons) = sample(&mut daemon, owner, &grant, Some(&start));
    let expected_root = replace.then(|| crate::worktree::trust::identity(&third).unwrap());
    assert_eq!(ending.members[2].root, expected_root);
    assert_ne!(ending.members[2].root, start.members[2].root);
    let finish = dudes_proto::repository_instruction::RepositoryInstructionFinish {
        v: ending.v,
        binding: ending.binding,
        owner,
        ticket: ending.ticket,
        baseline: dudes_proto::InstructionBaselineRef::Started { lsn: baseline },
        members: comparisons
            .unwrap()
            .into_iter()
            .map(
                |member| dudes_proto::repository_instruction::RepositoryInstructionSettlement {
                    repo_id: member.repo_id,
                    ending_root: member.ending_root,
                    settlement: InstructionSettlement::Compared {
                        end: dudes_proto::InstructionEndBoundary::ManagedTurn { turn, end_lsn },
                        sampled_at: ending.members[0].snapshot.sampled_at,
                        recovery: false,
                        comparison: member.comparison,
                    },
                },
            )
            .collect(),
    };
    append(
        &mut daemon,
        Record::InstructionObservationBatchFinished(finish),
    );
    drop(daemon);
    let mut daemon = setup.boot(false);
    let card = grant.ticket.as_ref().unwrap().card;
    let seq = daemon.ctx.projections.card(card).unwrap().unwrap().seq;
    let review = daemon
        .sup
        .world()
        .instructions
        .review(card, digest)
        .unwrap()
        .unwrap();
    assert!(review.coverage_unknown);
    for (index, record) in [
        Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 7, uid: None },
            expected_seq: seq,
            watermark: review.watermark,
            action: dudes_proto::InstructionReviewAction::Accept {
                completion: dudes_proto::InstructionAcceptCompletion::Continue,
            },
        },
        Record::CardTransition {
            card,
            from: CardStatus::Review,
            to: CardStatus::Done,
            by: Actor::Peer { pid: 7, uid: None },
            expected_seq: seq,
            reason: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            // A refused append leaves the journal unresolved until a restart replays it.
            drop(daemon);
            daemon = setup.boot(false);
        }
        let next = daemon.ctx.writer.next_lsn().unwrap();
        let error = daemon
            .ctx
            .append(
                daemon.sup.world(),
                &Entry {
                    at: daemon.ctx.clock.now(),
                    record,
                },
            )
            .unwrap_err();
        assert!(error.chain().any(|cause| cause.downcast_ref::<dudes_journal::Error>().is_some_and(|error| matches!(error,
            dudes_journal::Error::InstructionAdmission { reason: dudes_proto::instruction_state::InstructionStateError::PendingComparison, .. }
            | dudes_journal::Error::Instruction { reason: dudes_proto::instruction_state::InstructionStateError::PendingComparison, .. }
        ))), "{error:#}");
        assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    }
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn a_missing_undeclared_copy_blocks_accept_and_done() {
    unknown_copy(false);
}

#[test]
fn a_replaced_undeclared_copy_blocks_accept_and_done() {
    unknown_copy(true);
}

fn full_declaration(daemon: &TestDaemon, agent: AgentId) -> Vec<String> {
    let HireRepositories::Scoped(hire) = &daemon.sup.world().agents[&agent].repositories else {
        unreachable!()
    };
    hire.repositories
        .primary_first()
        .map(|member| member.repo_id.as_str().to_owned())
        .collect()
}

fn copies_of(daemon: &TestDaemon, agent: AgentId) -> Vec<std::path::PathBuf> {
    match &daemon.sup.world().agents[&agent].repositories {
        HireRepositories::Scoped(hire) => hire
            .repositories
            .entries()
            .iter()
            .map(|member| member.value.recorded.execution.directory.clone())
            .collect(),
        HireRepositories::Legacy(_) => unreachable!(),
    }
}

fn write_skill(copy: &Path, text: &str) {
    let directory = copy.join(".claude/skills/review");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("SKILL.md"), text).unwrap();
}

fn sampled_paths(member: &RepositoryInstructionSnapshot) -> Vec<std::path::PathBuf> {
    member
        .snapshot
        .inputs
        .iter()
        .filter(|input| {
            matches!(
                input.fingerprint,
                dudes_proto::InstructionFingerprint::File { .. }
            )
        })
        .filter_map(|input| input.path.to_path().ok())
        .collect()
}

fn turn_ended(agent: AgentId, turn: TurnId) -> Record {
    Record::TurnEnded {
        agent,
        turn,
        mode: Mode::Managed,
        provenance: dudes_types::Provenance::Observed,
        exit: dudes_proto::ExitStatus::UNKNOWN,
        usage: None,
        model_usage: std::collections::BTreeMap::new(),
        last_message: None,
        session: None,
        outcome: None,
        final_reply: None,
        elapsed_ms: None,
        session_usage: None,
        step_digest: None,
        context: None,
    }
}

#[test]
fn an_added_copy_is_sampled_for_its_skills_and_memory_files() {
    let (_setup, mut daemon, agent) = fixture_with(true);
    let declared = full_declaration(&daemon, agent);
    let grant = ticket_declaring(
        &mut daemon,
        agent,
        &declared.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn: TurnId::from_parts(3, 3),
        issue: InstructionIssueAssociation::Prospective,
    };
    let copies = copies_of(&daemon, agent);
    for copy in &copies {
        std::fs::write(copy.join("CLAUDE.md"), "memory").unwrap();
        write_skill(copy, "review v1");
    }
    let (start, _) = sample(&mut daemon, owner, &grant, None);
    let primary_index = start
        .members
        .iter()
        .position(|member| member.repo_id == *grant.repositories.primary_id())
        .unwrap();
    let primary = sampled_paths(&start.members[primary_index]);
    assert!(
        primary.contains(&copies[primary_index].join("CLAUDE.md")),
        "{primary:?}"
    );
    assert!(!primary.contains(&copies[primary_index].join(".claude/skills/review/SKILL.md")));
    for (index, member) in start.members.iter().enumerate() {
        if index != primary_index {
            assert_eq!(
                sampled_paths(member),
                [
                    copies[index].join(".claude/skills/review/SKILL.md"),
                    copies[index].join("CLAUDE.md"),
                ],
                "an added copy contributes its skill tree and its memory file"
            );
        }
    }
}

fn write_memory(copy: &Path, text: &str) {
    std::fs::write(copy.join("CLAUDE.md"), text).unwrap();
}

#[test]
fn a_skill_changed_in_a_secondary_copy_during_a_ticket_round_blocks_done_until_reviewed() {
    changed_in_a_secondary_copy(write_skill);
}

#[test]
fn a_memory_file_changed_in_a_secondary_copy_during_a_ticket_round_is_pending_review() {
    changed_in_a_secondary_copy(write_memory);
}

#[expect(
    clippy::too_many_lines,
    reason = "one physical attempt covers the baseline, the edit, the comparison and the refused completion"
)]
fn changed_in_a_secondary_copy(write: fn(&Path, &str)) {
    let (setup, mut daemon, agent) = fixture_with(true);
    let declared = full_declaration(&daemon, agent);
    let grant = ticket_declaring(
        &mut daemon,
        agent,
        &declared.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let turn = TurnId::from_parts(3, 3);
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: InstructionIssueAssociation::Prospective,
    };
    let copies = copies_of(&daemon, agent);
    let secondary_id = declared.last().unwrap().clone();
    let HireRepositories::Scoped(hire) = &daemon.sup.world().agents[&agent].repositories else {
        unreachable!()
    };
    let secondary = hire
        .repositories
        .entries()
        .iter()
        .position(|member| member.repo_id.as_str() == secondary_id)
        .unwrap();
    write(&copies[secondary], "review v1");
    let (start, _) = sample(&mut daemon, owner, &grant, None);
    let baseline = append(
        &mut daemon,
        Record::InstructionObservationBatchStarted(start.clone()),
    );
    issue(&mut daemon, agent, turn, grant.clone());
    write(&copies[secondary], "review v2, now with an extra step");
    let end_lsn = append(&mut daemon, turn_ended(agent, turn));
    let (ending, comparisons) = sample(&mut daemon, owner, &grant, Some(&start));
    let finish = dudes_proto::repository_instruction::RepositoryInstructionFinish {
        v: ending.v,
        binding: ending.binding,
        owner,
        ticket: ending.ticket,
        baseline: dudes_proto::InstructionBaselineRef::Started { lsn: baseline },
        members: comparisons
            .unwrap()
            .into_iter()
            .map(
                |member| dudes_proto::repository_instruction::RepositoryInstructionSettlement {
                    repo_id: member.repo_id,
                    ending_root: member.ending_root,
                    settlement: InstructionSettlement::Compared {
                        end: dudes_proto::InstructionEndBoundary::ManagedTurn { turn, end_lsn },
                        sampled_at: ending.members[0].snapshot.sampled_at,
                        recovery: false,
                        comparison: member.comparison,
                    },
                },
            )
            .collect(),
    };
    append(
        &mut daemon,
        Record::InstructionObservationBatchFinished(finish),
    );
    let card = grant.ticket.as_ref().unwrap().card;
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .pending_for_goal(grant.ticket.as_ref().unwrap().goal)
    );
    let review = daemon
        .sup
        .world()
        .instructions
        .review(card, digest)
        .unwrap()
        .unwrap();
    let changed: Vec<_> = review
        .changed_paths
        .iter()
        .filter_map(|change| change.repo_id.as_ref().map(|repo| repo.as_str().to_owned()))
        .collect();
    assert_eq!(changed, [secondary_id]);
    let seq = daemon.ctx.projections.card(card).unwrap().unwrap().seq;
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let error = daemon
        .ctx
        .append(
            daemon.sup.world(),
            &Entry {
                at: daemon.ctx.clock.now(),
                record: Record::CardTransition {
                    card,
                    from: CardStatus::Review,
                    to: CardStatus::Done,
                    by: Actor::Peer { pid: 7, uid: None },
                    expected_seq: seq,
                    reason: None,
                },
            },
        )
        .unwrap_err();
    assert!(
        error.chain().any(
            |cause| cause
                .downcast_ref::<dudes_journal::Error>()
                .is_some_and(|error| matches!(
                    error,
                    dudes_journal::Error::InstructionAdmission { .. }
                        | dudes_journal::Error::Instruction { .. }
                ))
        ),
        "{error:#}"
    );
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn an_edit_in_a_non_ticket_turn_is_recorded_and_is_preexisting_at_the_next_ticket_baseline() {
    use crate::runtime::repository_hire_tests::unconfined_sites::send_chat;

    let setup = setup_with_profile();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let copies = setup.copies_of(agent);
    write_skill(&copies[1], "review v1");
    send_chat(&running, agent);
    let turn = setup
        .fixture
        .await_record("the spawn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    write_skill(&copies[1], "review v2, edited during a chat turn");
    common::end_turn(&running.handle, turn);
    let finished =
        setup
            .fixture
            .await_record("the chat turn's observation", |record| match record {
                Record::InstructionObservationBatchFinished(batch) => Some(batch.clone()),
                _ => None,
            });
    running.stop(ExitCause::Ended);
    assert_eq!(finished.ticket, None);
    let skill = copies[1].join(".claude/skills/review/SKILL.md");
    let changed: Vec<_> = finished
        .members
        .iter()
        .flat_map(|member| match &member.settlement {
            InstructionSettlement::Compared { comparison, .. } => comparison
                .changes
                .iter()
                .filter_map(|change| change.path.to_path().ok())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(changed, std::slice::from_ref(&skill));

    let mut daemon = setup.boot_unconfined();
    daemon.started().unwrap();
    let declared = full_declaration(&daemon, agent);
    let grant = ticket_declaring(
        &mut daemon,
        agent,
        &declared.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn: TurnId::from_parts(3, 3),
        issue: InstructionIssueAssociation::Prospective,
    };
    let (start, _) = sample(&mut daemon, owner, &grant, None);
    assert!(
        start
            .members
            .iter()
            .any(|member| sampled_paths(member).contains(&skill)),
        "the edited skill is part of the ticket's baseline"
    );
    let (_, comparisons) = sample(&mut daemon, owner, &grant, Some(&start));
    let ticket_changes: usize = comparisons
        .unwrap()
        .iter()
        .map(|member| member.comparison.changes.len())
        .sum();
    assert_eq!(ticket_changes, 0, "the earlier edit is preexisting");
}
