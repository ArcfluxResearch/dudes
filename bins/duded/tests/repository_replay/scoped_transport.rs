use super::*;

use dudes_proto::rpc::{ErrorCode, Method};
use dudes_rpc::auth::PeerProvenance;

const SCOPED_TOKEN: &str = "sssssssssssssssssssssssssssssssssssssssssss";
const LEGACY_TOKEN: &str = "lllllllllllllllllllllllllllllllllllllllllll";

#[test]
fn replayed_scoped_hires_refuse_socket_calls_before_handlers_without_an_environment_marker() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let legacy = AgentId::from_parts(5, 1);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    append(&mut writer, legacy_hire(legacy, fixture.state.path()));
    for (agent, token) in [(story.hire.id, SCOPED_TOKEN), (legacy, LEGACY_TOKEN)] {
        append(
            &mut writer,
            Record::HookTokenMinted {
                agent,
                token_hash: dudes_supervisor::blake3_hex(token.as_bytes()),
            },
        );
    }
    drop(writer);
    let settings = settings(&fixture);
    let column = common::ScriptedColumn::new(PeerProvenance::Agent(story.hire.id));
    let mut seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(RealRunner(OneShot::new())),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    seams.resolver = duded::classify::Resolver::fixed(column.clone());
    let mut running = common::start(boot(&settings, seams).unwrap());
    let before = fixture.records();
    let mut scoped = Client::connect(settings.socket()).unwrap();
    let ping = scoped.call(Method::SystemPing, None).unwrap();
    let error = ping.error.expect("credentialless scoped ping is refused");
    assert_eq!(error.code, ErrorCode::Unauthorized.code());
    assert!(
        error
            .message
            .contains("not yet available in a multi-repo ticket")
    );
    refuse_socket_methods(&mut scoped, story.hire.id);
    column.set(PeerProvenance::NoRecordedAgent);
    let mut operator = Client::connect(settings.socket()).unwrap();
    assert!(
        operator
            .call_as_operator(Method::AgentList, None, credential(&settings))
            .unwrap()
            .error
            .is_none()
    );
    assert!(
        operator
            .call(
                Method::SystemPing,
                Some(json!({
                    "agent": story.hire.id,
                    "DUDES_COMMAND_TRANSPORT": "file-mail"
                }))
            )
            .unwrap()
            .error
            .is_none()
    );
    column.set(PeerProvenance::Agent(legacy));
    let mut ordinary = Client::connect(settings.socket()).unwrap();
    let response = ordinary
        .call(Method::TeamRoster, Some(json!({"token": LEGACY_TOKEN})))
        .unwrap();
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(fixture.records(), before);
    assert!(column.asked() >= 3);
    running.stop(ExitCause::Ended);
}

fn refuse_socket_methods(scoped: &mut dudes_rpc::LocalClient, agent: AgentId) {
    for method in Method::ALL {
        let params = if *method == Method::GoalReplace {
            json!({
                "request_id": "0".repeat(64),
                "agent": agent,
                "expected_goal": dudes_types::GoalId::from_parts(1, 1),
                "text": "Finish the check",
                "predicate": {"command": {"argv": ["true"], "timeout_s": 1}},
                "check": {"id": "tests", "label": "Tests"}
            })
        } else {
            json!({"token": SCOPED_TOKEN})
        };
        let response = scoped.call(*method, Some(params)).unwrap();
        let error = response
            .error
            .unwrap_or_else(|| panic!("{} reached its handler", method.name()));
        assert_eq!(
            error.code,
            ErrorCode::Unauthorized.code(),
            "{}: {error:?}",
            method.name()
        );
        assert!(
            error
                .message
                .contains("not yet available in a multi-repo ticket")
        );
    }
}
