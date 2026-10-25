//! `dudes team` presents authenticated credentials and renders the public roster.
#![cfg(unix)]

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};
use dudes::exit;
use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::team::{RosterEntry, RosterStatus};
use dudes_rpc::testing::{FakeMintedTokens, FakeResolver, TestServer};
use dudes_rpc::{Dispatcher, PeerClassifier, PeerProvenance};
use dudes_types::AgentId;
use std::sync::{Arc, Mutex};

struct Team(Mutex<Vec<Option<AgentId>>>);
impl dudes_rpc::methods::TeamRoster for Team {
    fn roster(&self, caller: Option<AgentId>) -> Result<Vec<RosterEntry>, RpcError> {
        self.0.lock().unwrap().push(caller);
        let other = AgentId::from_parts(1, 1);
        Ok([
            (AgentId::NIL, "name\u{1b}[2J", "builder\nrole", "eng\rdept"),
            (other, "Kim", "reviewer", "eng"),
        ]
        .into_iter()
        .map(|(id, name, role, department)| RosterEntry {
            id,
            name: name.into(),
            role: role.into(),
            department: department.into(),
            status: RosterStatus::Working,
            held_card: None,
            you: caller == Some(id),
        })
        .collect())
    }
}

#[test]
fn team_presents_only_the_agent_token_and_escapes_human_text() {
    let token = "t".repeat(43);
    let credential = dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap();
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        credential,
        Arc::new(FakeMintedTokens::minted(&[(&token, AgentId::NIL)])),
    ));
    let team = Arc::new(Team(Mutex::new(vec![])));
    dudes_rpc::methods::register_team_roster(&mut dispatcher, &team);
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let server = TestServer::start_with_resolver(
        runtime.clone(),
        dispatcher,
        Arc::new(FakeResolver::new(PeerProvenance::Agent(AgentId::NIL))),
    );
    std::fs::write(
        runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "malformed credential fixture",
    )
    .unwrap();
    let cli = || Cli::new(server.socket(), &runtime).env("DUDES_TOKEN", &token);
    let json = cli().args(["--json", "team"]).run();
    assert_eq!(json.code(), exit::SUCCESS, "{}", json.output());
    let value = json.json::<serde_json::Value>();
    assert_eq!(value[0].as_object().unwrap().len(), 7);
    assert_eq!(
        (value[0]["you"].clone(), value[1]["you"].clone()),
        (true.into(), false.into())
    );
    assert_eq!(value[0]["name"], "name\u{1b}[2J");
    let human = cli().args(["team"]).run();
    assert_eq!(human.code(), exit::SUCCESS, "{}", human.output());
    assert!(
        human.stdout.contains("\\u{1b}[2J")
            && human.stdout.contains("builder\\u{a}role")
            && human.stdout.contains("eng\\u{d}dept")
    );
    assert!(!human.stdout.contains('\u{1b}'));
    let lines: Vec<_> = human.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", human.stdout);
    assert!(lines[0].ends_with("  (you)"), "{}", lines[0]);
    assert!(!lines[1].contains("(you)"), "{}", lines[1]);
    assert_eq!(
        *team.0.lock().unwrap(),
        [Some(AgentId::NIL), Some(AgentId::NIL)]
    );
    for args in [
        vec!["team", "--help"],
        vec!["team", "--unknown"],
        vec!["team", "--", "payload"],
    ] {
        assert_eq!(cli().args(args).run().code(), exit::USAGE);
    }
    assert_eq!(team.0.lock().unwrap().len(), 2);
    assert!(!json.output().contains(&token) && !human.output().contains(&token));

    let me = cli().args(["whoami"]).run();
    assert_eq!(me.code(), exit::SUCCESS, "{}", me.output());
    assert_eq!(
        me.stdout,
        "You are name\\u{1b}[2J (agent 00000000000000000000000000), role: builder\\u{a}role, department: eng\\u{d}dept.\n"
    );
    let me_json = cli().args(["--json", "whoami"]).run();
    assert_eq!(me_json.code(), exit::SUCCESS, "{}", me_json.output());
    let row = me_json.json::<RosterEntry>();
    assert!(row.you && row.id == AgentId::NIL);
    assert_eq!(team.0.lock().unwrap().len(), 4);
    assert_eq!(
        cli().args(["whoami", "--", "payload"]).run().code(),
        exit::USAGE
    );
}

#[test]
fn whoami_without_an_agent_token_is_a_plain_error() {
    let credential = dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap();
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        credential,
        Arc::new(FakeMintedTokens::minted(&[])),
    ));
    let team = Arc::new(Team(Mutex::new(vec![])));
    dudes_rpc::methods::register_team_roster(&mut dispatcher, &team);
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let server = TestServer::start_with_resolver(
        runtime.clone(),
        dispatcher,
        Arc::new(FakeResolver::new(PeerProvenance::NoRecordedAgent)),
    );
    std::fs::write(
        runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "C".repeat(43),
    )
    .unwrap();
    let operator = Cli::new(server.socket(), &runtime).args(["whoami"]).run();
    assert_eq!(operator.code(), exit::OTHER, "{}", operator.output());
    assert!(operator.stdout.is_empty(), "{}", operator.output());
    assert!(
        operator.stderr.contains("no agent matches this process"),
        "{}",
        operator.output()
    );
}

#[test]
fn team_operator_reads_the_live_daemon_roster() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let empty = cli.clone().args(["--json", "team"]).run();
    assert_eq!(empty.code(), exit::SUCCESS, "{}", empty.output());
    assert!(empty.json::<Vec<RosterEntry>>().is_empty());
    let hire = cli
        .clone()
        .args(["--json"])
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(hire.code(), exit::SUCCESS, "{}", hire.output());
    let id = hire.json::<dudes_proto::rpc::agent::HireResult>().agent;
    let rows = cli.args(["--json", "team"]).run();
    assert_eq!(rows.code(), exit::SUCCESS, "{}", rows.output());
    assert_eq!(
        rows.json::<Vec<RosterEntry>>(),
        [RosterEntry {
            id,
            name: "Hector".into(),
            role: "infra".into(),
            department: "infra".into(),
            status: RosterStatus::Idle,
            held_card: None,
            you: false
        }]
    );
}
