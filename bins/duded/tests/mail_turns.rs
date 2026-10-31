//! A teammate's request starts a turn for an idle chat agent, and the answer starts one for the
//! asker, through the Floor chat route, the mailbox and the journal.
#![allow(missing_docs)]
#![cfg(unix)]

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ExitCause;
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_proto::{
    ExhaustedReason, HoldReason, MailAct, MailFrom, MailTo, MailTurnAdmission, Record,
};
use dudes_types::{AgentId, MailId, TurnId};
use serde_json::{Value, json};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v10"
);

struct Rig {
    fixture: Fixture,
    running: common::Running,
    page: duded::floor_browser::Page,
    clock: Arc<TestClock>,
    host: SharedHost,
    next_mail: u64,
}

fn rig() -> Rig {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    let clock = Arc::new(TestClock::new());
    let host = SharedHost::with_native_evidence(settings.state_dir.join(duded::boot::JOURNAL_DIR));
    let seams = common::seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::scripted([])),
        Arc::clone(&clock),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = duded::boot(&settings, seams).expect("boot the daemon");
    let addr: SocketAddr = daemon.floor_addr().expect("Floor address");
    let running = common::start(daemon);
    let page = common::floor_session_on(&settings, addr, {
        let clock = Arc::clone(&clock);
        move || {
            use dudes_supervisor::Clock as _;
            clock.now().unix_millis()
        }
    });
    Rig {
        fixture,
        running,
        page,
        clock,
        host,
        next_mail: 1,
    }
}

fn request_id(number: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut wire = [b'A'; 43];
    wire[0] = ALPHABET[number % 64];
    wire[1] = ALPHABET[number / 64];
    String::from_utf8(wire.to_vec()).unwrap()
}

impl Rig {
    fn hire(&self) -> AgentId {
        common::hire(&self.running.handle, self.fixture.state.path())
    }

    fn chat(&self, number: usize, agent: AgentId, text: &str) {
        let (_, receipt) = common::floor_post(
            &self.page,
            "/floor/chat/send",
            &json!({"request_id": request_id(number), "agent": agent, "text": text}),
        );
        assert_eq!(receipt["state"], "queued");
    }

    fn outbox(&self, agent: AgentId) -> PathBuf {
        let dir = self
            .fixture
            .state
            .path()
            .join("agents")
            .join(agent.to_string())
            .join("outbox");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a stand-in engine does when it runs `dudes mail send`: drop one file in its outbox.
    fn send(&mut self, from: AgentId, mut body: Value) -> MailId {
        let id = MailId::from_parts(9, u128::from(self.next_mail));
        self.next_mail += 1;
        body["subject"] = json!("question");
        fs::write(
            self.outbox(from).join(format!("{id}.json")),
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        id
    }

    fn request(&mut self, from: AgentId, to: AgentId) -> MailId {
        self.send(
            from,
            json!({"to": {"kind": "agent", "agent": to}, "act": "request", "body": "what is 2+2?"}),
        )
    }

    fn mail_to(&self, to: AgentId, act: MailAct) -> (MailId, MailFrom) {
        self.fixture.await_record("mail", |record| match record {
            Record::Mail {
                id,
                from,
                to: MailTo::Agent { agent },
                act: found,
                ..
            } if *agent == to && *found == act => Some((*id, *from)),
            _ => None,
        })
    }

    fn mail_turn_issue(&self, agent: AgentId, skip: usize) -> (TurnId, Vec<MailId>, String) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let issues: Vec<_> = self
                .fixture
                .records()
                .into_iter()
                .filter_map(|record| match record {
                    Record::MailTurnIssued(issue) if issue.agent == agent => Some((
                        issue.turn,
                        issue.covered.iter().map(|mail| mail.id).collect(),
                        issue.task.as_str().to_owned(),
                    )),
                    _ => None,
                })
                .collect();
            if let Some(found) = issues.into_iter().nth(skip) {
                return found;
            }
            assert!(
                Instant::now() < end,
                "no mail turn issue for {agent}: {:?} {:?}",
                self.decisions(agent),
                self.fixture
                    .records()
                    .iter()
                    .map(Record::kind)
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn held(&self) -> Vec<Record> {
        self.fixture
            .records()
            .into_iter()
            .filter(|record| matches!(record, Record::AgentHeld { .. }))
            .collect()
    }

    fn spawned(&self, turn: TurnId) {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if self.fixture.records().iter().any(|record| matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(self.held().is_empty(), "{:?}", self.held());
        self.fixture.await_record("spawn", |record| match record {
            Record::TurnSpawned { turn: found, .. } if *found == turn => Some(()),
            _ => None,
        });
    }

    fn end(&self, turn: TurnId) {
        self.spawned(turn);
        common::end_turn_with_native(&self.fixture, &self.running.handle, &self.host, turn);
    }

    fn chat_turn(&self, agent: AgentId) -> TurnId {
        self.fixture.await_record("chat turn", |record| match record {
            Record::IdentifiedConversationTurnIssued { agent: found, turn, .. }
                if *found == agent =>
            {
                Some(*turn)
            }
            _ => None,
        })
    }

    fn decisions(&self, agent: AgentId) -> Vec<MailTurnAdmission> {
        self.fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::MailTurnDecided(decision) if decision.agent == agent => {
                    Some(decision.decision)
                }
                _ => None,
            })
            .collect()
    }

    fn issues(&self, agent: AgentId) -> usize {
        self.fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::MailTurnIssued(issue) if issue.agent == agent))
            .count()
    }

    fn settle(&self) {
        let end = Instant::now() + Duration::from_millis(600);
        while Instant::now() < end {
            self.running.handle.doctor().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_request_starts_the_teammates_turn_and_the_answer_starts_the_askers() {
    let mut rig = rig();
    let a = rig.hire();
    let b = rig.hire();
    rig.chat(1, a, "ask B what 2+2 is");
    let chat = rig.chat_turn(a);
    rig.spawned(chat);
    let asked = rig.request(a, b);
    let _ = asked;
    rig.end(chat);

    let (delivered, from) = rig.mail_to(b, MailAct::Request);
    assert_eq!(from, MailFrom::Agent { agent: a });
    let (b_turn, covered, task) = rig.mail_turn_issue(b, 0);
    assert_eq!(covered, vec![delivered]);
    assert!(task.starts_with(&format!("[You are ")), "{task}");
    assert!(task.contains("[1 new mail]"));
    assert!(task.ends_with(dudes_proto::TaskCommandSet::Legacy.mail_teaching()));
    rig.spawned(b_turn);

    let answer = rig.send(
        b,
        json!({"in_reply_to": delivered, "act": "message", "body": "4"}),
    );
    let _ = answer;
    rig.end(b_turn);

    let (reply, from) = rig.mail_to(a, MailAct::Message);
    assert_eq!(from, MailFrom::Agent { agent: b });
    let (a_turn, covered, task) = rig.mail_turn_issue(a, 0);
    assert_eq!(covered, vec![reply]);
    assert!(task.contains("[1 new mail]"));
    rig.end(a_turn);
    assert_eq!(rig.issues(a), 1);
    assert_eq!(rig.issues(b), 1);
    rig.running.stop(ExitCause::ShutdownRpc);
    let _ = (ExhaustedReason::MailWakeLimit, HoldReason::Paused, &rig.clock);
}
