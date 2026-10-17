use super::work_test_support as common;
use super::*;
use dudes_types::IsolationMode;

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn hire(daemon: &mut TestDaemon, cwd: &std::path::Path, inherit: bool) -> AgentId {
    let answer = daemon
        .ask(Ask::Hire(Box::new(HireParams {
            isolation: inherit.then_some(IsolationMode::Inherit),
            ..common::hire_params(cwd)
        })))
        .expect("a hire before any child is admitted");
    let Answered::Hired(result) = answer else {
        panic!("unexpected {answer:?}")
    };
    result.agent
}

fn prepared(daemon: &mut TestDaemon, agent: AgentId) -> Effect {
    let params = common::goal_params(agent, &["/usr/bin/true"]);
    let mut queue = VecDeque::from([Input::Command(Command::GoalSet {
        agent,
        text: params.text,
        predicate: params.predicate,
        budget: params.budget,
        hop_cap: params.hop_cap,
        browser_goal: None,
        goal_replacement: None,
        completion_check: None,
        baseline: None,
    })]);
    let mut sample_request = None;
    for _ in 0..64 {
        if queue.is_empty() {
            let Job::Input(input) = daemon
                .jobs
                .recv_timeout(Duration::from_secs(3))
                .expect("the actual sampler must return its bounded response")
            else {
                panic!("launch preparation received an unrelated job");
            };
            let Input::InstructionSampled { request, .. } = input.as_ref() else {
                panic!("launch preparation received an unrelated input");
            };
            assert_eq!(Some(*request), sample_request);
            queue.push_back(*input);
        }
        let input = queue.pop_front().expect("launch preparation continues");
        daemon
            .sup
            .handle(input, &dudes_supervisor::RecoveryOperation::empty())
            .unwrap();
        for effect in daemon.sup.take_effects() {
            if let Effect::SampleInstructions { request, .. } = &effect {
                sample_request = Some(*request);
            }
            if matches!(effect, Effect::Spawn { .. }) {
                return effect;
            }
            daemon
                .ctx
                .execute(
                    &effect,
                    &mut queue,
                    &mut PumpReport::Discard,
                    &mut daemon.sup,
                    &mut daemon.closing,
                )
                .expect("prepare through the real persistence executor");
        }
    }
    panic!("bounded launch preparation did not produce Spawn")
}

#[test]
fn a_batch_sees_the_created_child_before_its_spawn_receipt_is_folded() {
    for shared in [true, false] {
        let fixture = common::Fixture::new();
        let distinct = fixture.state.path().join("distinct");
        std::fs::create_dir(&distinct).unwrap();
        let host = common::SharedHost::new();
        let settings = fixture.settings();
        let mut seams = common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: "/usr/bin/true".into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect();
        let mut daemon = crate::boot(&settings, seams).unwrap();
        let first_agent = hire(&mut daemon, fixture.state.path(), false);
        let second_agent = hire(
            &mut daemon,
            if shared {
                fixture.state.path()
            } else {
                &distinct
            },
            true,
        );
        let first = prepared(&mut daemon, first_agent);
        let second = prepared(&mut daemon, second_agent);
        let Effect::Spawn {
            turn: first_turn, ..
        } = &first
        else {
            unreachable!()
        };
        let mut queue = VecDeque::new();
        daemon
            .ctx
            .execute(
                &first,
                &mut queue,
                &mut PumpReport::Discard,
                &mut daemon.sup,
                &mut daemon.closing,
            )
            .unwrap();
        assert!(
            daemon.sup.world().turns[first_turn]
                .adoption_key()
                .is_none()
        );
        assert!(matches!(queue.front(), Some(Input::Spawned { turn, .. }) if turn == first_turn));
        daemon
            .ctx
            .execute(
                &second,
                &mut queue,
                &mut PumpReport::Discard,
                &mut daemon.sup,
                &mut daemon.closing,
            )
            .unwrap();
        if shared {
            assert_eq!(host.spawned().len(), 1);
            assert!(queue.iter().any(|input| matches!(input,
                Input::LaunchAdmissionRefused { agent, refusal: dudes_supervisor::SharedFolderRefusal::LivePeer { agent: peer, .. }, .. }
                    if *agent == second_agent && *peer == first_agent)));
        } else {
            assert_eq!(host.spawned().len(), 2);
            assert_eq!(
                queue
                    .iter()
                    .filter(|input| matches!(input, Input::Spawned { .. }))
                    .count(),
                2
            );
        }
        while let Some(input) = queue.pop_front() {
            daemon.pump(input).unwrap();
        }
        if shared {
            assert!(matches!(
                daemon.sup.world().agents[&second_agent].automatic_hold,
                Some(dudes_proto::AutomaticHold::LaunchRefused { .. })
            ));
        }
    }
}
