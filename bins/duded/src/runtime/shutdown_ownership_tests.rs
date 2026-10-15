use super::work_test_support as common;
use super::*;

#[test]
fn an_owned_child_sampled_and_spawned_before_run_is_swept_when_the_daemon_drops() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let runner = Arc::new(common::ScriptedRunner::default());
    let witness = Witness::default();
    let settings = fixture.settings();
    let seams = common::seams(
        &settings,
        host.clone(),
        Arc::clone(&runner),
        Arc::new(common::TestClock::new()),
        witness.clone(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let mut daemon = crate::boot(&settings, seams).unwrap();
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("the owned child needs its recorded agent");
    };
    daemon
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            hired.agent,
            &["/usr/bin/true"],
        ))))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let turn = loop {
        if let Some(turn) = daemon.sup.world().agents[&hired.agent].open_turn
            && daemon.sup.world().turns[&turn].is_open_child()
        {
            break turn;
        }
        let Job::Input(input) = daemon
            .jobs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the actual baseline worker must return before the fixture deadline")
        else {
            panic!("only the actual starting sample is expected");
        };
        daemon.pump(*input).unwrap();
    };
    let pid = daemon.sup.world().turns[&turn].pid.unwrap();
    let key = host.peek(|host| host.key_for(pid));
    let mut owned = Vec::new();
    daemon
        .ctx
        .children
        .visit_owned(|observed_turn, observed_key, _| {
            owned.push((
                observed_turn,
                observed_key.pid,
                observed_key.start_time,
                observed_key.boot,
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        owned,
        vec![(turn, key.pid, key.start_time, key.boot.clone())]
    );
    assert_eq!(host.spawned().len(), 1);
    assert_eq!(host.alive_of(pid), dudes_supervisor::Liveness::Alive);
    assert_eq!(host.shutdowns(), 0);
    let records = fixture.records();
    let started = records
        .iter()
        .position(|record| {
            matches!(record,
        Record::InstructionObservationStarted {
            owner: dudes_proto::InstructionObservationOwner::Managed { turn: observed, .. }, ..
        } if *observed == turn)
        })
        .unwrap();
    let spawned = records
        .iter()
        .position(|record| {
            matches!(record,
        Record::TurnSpawned { turn: observed, .. } if *observed == turn)
        })
        .unwrap();
    assert!(started < spawned);
    drop(daemon);
    assert_eq!(host.shutdowns(), 1);
    assert_eq!(host.alive_of(pid), dudes_supervisor::Liveness::Gone);
    assert_eq!(runner.sweeps(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::ORDER.to_vec());
}
