//! `dudes mail read` and `dudes mail list` name a sender the roster knows and print the id alone otherwise.
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::fs;
use std::sync::Arc;

use common::Cli;
use dudes::exit;
use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::team::{RosterEntry, RosterStatus};
use dudes_rpc::testing::{FakeMintedTokens, FakeResolver, TestServer};
use dudes_rpc::{Dispatcher, PeerClassifier, PeerProvenance};
use dudes_types::{AgentId, MailId};
use serde_json::json;

struct Team;
impl dudes_rpc::methods::TeamRoster for Team {
    fn roster(&self, caller: Option<AgentId>) -> Result<Vec<RosterEntry>, RpcError> {
        Ok(vec![RosterEntry {
            id: AgentId::from_parts(1, 2),
            name: "Kim\u{1b}[2J".into(),
            role: "reviewer".into(),
            department: "eng".into(),
            status: RosterStatus::Idle,
            held_card: None,
            you: caller == Some(AgentId::from_parts(1, 2)),
        }])
    }
}

#[test]
fn read_and_list_name_a_known_sender_and_keep_the_id() {
    let token = "t".repeat(43);
    let me = AgentId::from_parts(1, 1);
    let kim = AgentId::from_parts(1, 2);
    let stranger = AgentId::from_parts(1, 3);
    let credential = dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap();
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        credential,
        Arc::new(FakeMintedTokens::minted(&[(&token, me)])),
    ));
    dudes_rpc::methods::register_team_roster(&mut dispatcher, &Arc::new(Team));
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let server = TestServer::start_with_resolver(
        runtime.clone(),
        dispatcher,
        Arc::new(FakeResolver::new(PeerProvenance::Agent(me))),
    );
    let mailbox = root.path().join(me.to_string());
    fs::create_dir_all(mailbox.join("outbox")).unwrap();
    fs::create_dir_all(mailbox.join("inbox/.done")).unwrap();
    for (n, from) in [(1, kim), (2, stranger)] {
        let id = MailId::from_parts(1, n);
        let marker = json!({ "id":id, "from":{"kind":"agent","agent":from},
            "to":{"kind":"agent","agent":me}, "act":"message", "subject":"hello",
            "body":"hi", "body_len":2 });
        fs::write(
            mailbox.join("inbox").join(format!("{id}.json")),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    }
    let cli = || {
        Cli::new(server.socket(), &runtime)
            .env("DUDES_TOKEN", &token)
            .env("DUDES_MAILBOX", &mailbox)
    };
    for command in ["read", "list"] {
        let out = cli().args(["mail", command]).run();
        assert_eq!(out.code(), exit::SUCCESS, "{}", out.output());
        assert!(
            out.stdout
                .contains(&format!("from Kim\\u{{1b}}[2J ({kim})\n")),
            "{}",
            out.stdout
        );
        assert!(
            out.stdout.contains(&format!("from {stranger}\n")),
            "{}",
            out.stdout
        );
        assert!(!out.stdout.contains('\u{1b}'));
    }
    let offline = Cli::bare()
        .env("DUDES_MAILBOX", &mailbox)
        .env("DUDES_SOCK", root.path().join("absent.sock"))
        .args(["mail", "list"])
        .run();
    assert!(
        offline.stdout.contains(&format!("from {kim}\n")),
        "{}",
        offline.stdout
    );
}
