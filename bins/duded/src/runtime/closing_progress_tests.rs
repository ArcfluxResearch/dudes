use super::work_test_support as common;
use super::*;
use crate::floor_stream::QueueRead;
use crate::settings::FloorSettings;

#[test]
fn a_deferred_cut_failure_keeps_its_initiating_error_and_abandons_output() {
    let fixture = common::Fixture::new();
    let mut settings = fixture.settings();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v3"
        )),
        port: Some(port),
    });
    let mut daemon = crate::boot(
        &settings,
        common::seams(
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("hire");
    };
    daemon
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            hired.agent,
            &["/usr/bin/true"],
        ))))
        .unwrap();
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let streams = daemon.ctx.floor_streams.as_ref().unwrap().clone();
    let opened = streams.subscribe().unwrap();
    let turn = daemon.sup.world().agents[&hired.agent].open_turn.unwrap();
    let folded = daemon.sup.world().last_applied_lsn;
    let output = daemon.ctx.children.output();
    let before = output.snapshot(hired.agent);
    daemon
        .onboarding_failure()
        .arm(OnboardingFailurePoint::SpendRefresh);
    let failure = daemon
        .pump(Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        })
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "injected durable spend refresh failure"
    );
    assert_eq!(daemon.sup.world().last_applied_lsn, folded);
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert!(matches!(
        opened.subscription.next(Duration::ZERO),
        QueueRead::Ended
    ));
    assert!(streams.subscribe().is_err());
    assert_eq!(output.snapshot(hired.agent).revision, before.revision);
    assert_eq!(output.snapshot(hired.agent).state, before.state);
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}
