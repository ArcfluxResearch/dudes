//! A teammate's request starts a turn for an idle chat agent, and the answer starts one for the
//! asker, through the Floor chat route, the mailbox and the journal.
#![allow(missing_docs)]
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ExitCause;
use duded::ports::Ask;
use duded::settings::{FloorSettings, Settings};
use duded::startup::Witness;
use dudes_proto::{
    ExhaustedReason, HoldReason, MailAct, MailFrom, MailTo, MailTurnAdmission, Record,
};
use dudes_providers::launch::EnvVar;
use dudes_supervisor::{Input, ProcessHost};
use dudes_types::{AgentId, MailId, ThreadId, TurnId, Usd};
use serde_json::{Value, json};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, UnconfirmedHost};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v11"
);
const DAY_MS: i64 = 86_400_000;

struct Rig {
    fixture: Fixture,
    running: common::Running,
    page: duded::floor_browser::Page,
    addr: SocketAddr,
    clock: Arc<TestClock>,
    host: SharedHost,
    next_mail: u64,
    next_chat: usize,
}

impl Rig {
    fn new() -> Self {
        Self::start(Fixture::new(), |_| {}, Arc::new(TestClock::new()))
    }

    fn start(
        fixture: Fixture,
        configure: impl FnOnce(&mut Settings),
        clock: Arc<TestClock>,
    ) -> Self {
        let host = SharedHost::with_native_evidence(fixture.journal_dir());
        Self::start_on(fixture, configure, clock, host.clone(), host)
    }

    fn start_on<H: ProcessHost + Send + 'static>(
        fixture: Fixture,
        configure: impl FnOnce(&mut Settings),
        clock: Arc<TestClock>,
        process_host: H,
        host: SharedHost,
    ) -> Self {
        Self::start_checked(
            fixture,
            configure,
            clock,
            process_host,
            host,
            ScriptedRunner::scripted([]),
        )
    }

    fn start_checked<H: ProcessHost + Send + 'static>(
        fixture: Fixture,
        configure: impl FnOnce(&mut Settings),
        clock: Arc<TestClock>,
        process_host: H,
        host: SharedHost,
        runner: ScriptedRunner,
    ) -> Self {
        let mut settings = fixture.settings();
        settings.floor = Some(FloorSettings {
            assets_dir: PathBuf::from(FLOOR_FIXTURE),
            port: Some(0),
        });
        configure(&mut settings);
        let seams = common::seams(
            &settings,
            process_host,
            Arc::new(runner),
            Arc::clone(&clock),
            Witness::default(),
        );
        host.bind_child_ends(Arc::clone(&seams.children));
        let daemon = duded::boot(&settings, seams).expect("boot the daemon");
        let addr = daemon.floor_addr().expect("Floor address");
        let running = common::start(daemon);
        let page = common::floor_session_on(&settings, addr, {
            let clock = Arc::clone(&clock);
            move || {
                use dudes_supervisor::Clock as _;
                clock.now().unix_millis()
            }
        });
        Self {
            fixture,
            running,
            page,
            addr,
            clock,
            host,
            next_mail: 1,
            next_chat: 1,
        }
    }

    fn stop(mut self) -> Fixture {
        self.running.stop(ExitCause::Ended);
        self.fixture
    }

    fn hire(&self) -> AgentId {
        common::hire(&self.running.handle, self.fixture.state.path())
    }

    fn hire_without_ceiling(&self) -> AgentId {
        let mut params = common::hire_params(self.fixture.state.path());
        params.budget.usd_per_day = None;
        match self
            .running
            .handle
            .ask(Ask::Hire(Box::new(params)))
            .unwrap()
        {
            duded::ports::Answered::Hired(hired) => hired.agent,
            other => panic!("hire answered {other:?}"),
        }
    }

    fn chat(&mut self, agent: AgentId, text: &str) {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut wire = [b'A'; 43];
        wire[0] = ALPHABET[self.next_chat % 64];
        self.next_chat += 1;
        let (_, receipt) = common::floor_post(
            &self.page,
            "/floor/chat/send",
            &json!({"request_id": String::from_utf8(wire.to_vec()).unwrap(), "agent": agent, "text": text}),
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

    fn reply(&mut self, from: AgentId, to_mail: MailId, act: &str) -> MailId {
        self.send(
            from,
            json!({"in_reply_to": to_mail, "act": act, "body": "4"}),
        )
    }

    /// The journaled mail of `act` to `to`, newest first excluded: the `nth` one.
    fn mail_to(
        &self,
        to: AgentId,
        act: MailAct,
        nth: usize,
    ) -> (MailId, MailFrom, Option<ThreadId>) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let found: Vec<_> = self
                .fixture
                .records()
                .into_iter()
                .filter_map(|record| match record {
                    Record::Mail {
                        id,
                        from,
                        to: MailTo::Agent { agent },
                        act: found,
                        thread,
                        ..
                    } if agent == to && found == act => Some((id, from, thread)),
                    _ => None,
                })
                .collect();
            if let Some(mail) = found.into_iter().nth(nth) {
                return mail;
            }
            assert!(Instant::now() < end, "no {act:?} mail for {to}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn issues(&self, agent: AgentId) -> Vec<(TurnId, Vec<MailId>, String)> {
        self.fixture
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
            .collect()
    }

    fn issue(&self, agent: AgentId, nth: usize) -> (TurnId, Vec<MailId>, String) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = self.issues(agent).into_iter().nth(nth) {
                return found;
            }
            assert!(
                Instant::now() < end,
                "no mail turn issue {nth} for {agent}: {:?} {:?}",
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

    fn decisions(&self, agent: AgentId) -> Vec<(MailTurnAdmission, Option<String>)> {
        self.fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::MailTurnDecided(decision) if decision.agent == agent => Some((
                    decision.decision,
                    decision
                        .gate_trace
                        .last()
                        .and_then(|step| step.note.clone()),
                )),
                _ => None,
            })
            .collect()
    }

    fn await_decisions(
        &self,
        agent: AgentId,
        count: usize,
    ) -> Vec<(MailTurnAdmission, Option<String>)> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let found = self.decisions(agent);
            if found.len() >= count {
                return found;
            }
            assert!(Instant::now() < end, "decisions for {agent}: {found:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn spawned(&self, turn: TurnId) {
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
        self.fixture
            .await_record("chat turn", |record| match record {
                Record::IdentifiedConversationTurnIssued {
                    agent: found, turn, ..
                } if *found == agent => Some(*turn),
                _ => None,
            })
    }

    fn end_with_reply(&self, turn: TurnId, text: &str) {
        self.spawned(turn);
        common::end_turn_with_native_payload(
            &self.fixture,
            &self.host,
            Input::ChildExited {
                report: dudes_proto::TurnReport::default(),
                final_reply: Some(dudes_proto::ChatReply {
                    text: text.to_owned(),
                    completeness: dudes_proto::ChatReplyCompleteness::Complete,
                }),
                elapsed_ms: None,
                turn,
                exit: dudes_proto::ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                model_usage: std::collections::BTreeMap::new(),
                outcome: dudes_proto::TurnOutcome::Completed,
                last_message: None,
            },
        );
    }

    /// The rows of `agent`'s chat history in the first frame of a Floor subscription, which is
    /// what the Floor renders from.
    fn chat_rows(&self, agent: AgentId) -> Vec<Value> {
        let mut stream = TcpStream::connect(self.addr).expect("Floor stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("stream read bound");
        let head = self.page.upgrade_head("/floor/stream", "dudes-floor-v6");
        stream.write_all(head.as_bytes()).expect("upgrade write");
        let mut seen = Vec::new();
        let mut byte = [0];
        while !seen.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("upgrade response");
            seen.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&seen).starts_with("HTTP/1.1 101"));
        let payload = b"{\"type\":\"floor.subscribe\"}";
        let mut frame = vec![
            0x81,
            0x80 | u8::try_from(payload.len()).unwrap(),
            0,
            0,
            0,
            0,
        ];
        frame.extend_from_slice(payload);
        stream.write_all(&frame).expect("subscribe");
        let mut prefix = [0; 2];
        stream.read_exact(&mut prefix).expect("snapshot frame");
        let length = match prefix[1] & 0x7f {
            126 => {
                let mut bytes = [0; 2];
                stream.read_exact(&mut bytes).unwrap();
                usize::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0; 8];
                stream.read_exact(&mut bytes).unwrap();
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
            length => usize::from(length),
        };
        let mut body = vec![0; length];
        stream.read_exact(&mut body).expect("snapshot payload");
        let snapshot: Value = serde_json::from_slice(&body).expect("snapshot JSON");
        snapshot["chat"]["turns"]
            .as_array()
            .expect("chat turns")
            .iter()
            .filter(|row| row["agent_id"] == json!(agent))
            .cloned()
            .collect()
    }

    /// Chat rows once `count` of them exist for `agent`.
    fn await_chat_rows(&self, agent: AgentId, count: usize) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let rows = self.chat_rows(agent);
            if rows.len() >= count {
                return rows;
            }
            assert!(Instant::now() < end, "chat rows for {agent}: {rows:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Let mailbox passes and the loop run, then move the loop one beat.
    fn settle(&self) {
        let end = Instant::now() + Duration::from_millis(150);
        while Instant::now() < end {
            self.running.handle.doctor().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        self.running.handle.input(Input::Tick);
        self.running.handle.doctor().unwrap();
    }

    /// One request from `from` to `to` in a fresh thread, the receiving turn run to its end
    /// without a reply.
    fn asked_and_ended(&mut self, from: AgentId, to: AgentId, nth: usize) -> MailId {
        self.request(from, to);
        let (mail, _, _) = self.mail_to(to, MailAct::Request, nth);
        let (turn, covered, _) = self.issue(to, nth);
        assert_eq!(covered, vec![mail]);
        self.end(turn);
        mail
    }
}

#[test]
fn a_request_starts_the_teammates_turn_and_the_answer_starts_the_askers() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    rig.chat(a, "ask B what 2+2 is");
    let chat = rig.chat_turn(a);
    rig.spawned(chat);
    rig.request(a, b);
    rig.end(chat);

    let (delivered, from, _) = rig.mail_to(b, MailAct::Request, 0);
    assert_eq!(from, MailFrom::Agent { agent: a });
    let (b_turn, covered, task) = rig.issue(b, 0);
    assert_eq!(covered, vec![delivered]);
    assert!(task.starts_with("[You are "), "{task}");
    assert!(task.contains("[1 new mail]"));
    assert!(task.ends_with(dudes_proto::TaskCommandSet::Legacy.mail_teaching()));
    assert!(task.contains("`dudes mail send --reply-to <id> --act message --body <text>`"));
    rig.spawned(b_turn);

    rig.reply(b, delivered, "message");
    rig.end(b_turn);

    let (answer, from, _) = rig.mail_to(a, MailAct::Message, 0);
    assert_eq!(from, MailFrom::Agent { agent: b });
    assert!(
        !rig.fixture.records().iter().any(|record| matches!(record,
            Record::Mail { to: MailTo::Agent { agent }, act: MailAct::Request, .. } if *agent == a))
    );
    let (a_turn, covered, task) = rig.issue(a, 0);
    assert_eq!(covered, vec![answer]);
    assert!(task.contains("[1 new mail]"));
    rig.end(a_turn);
    assert_eq!(rig.issues(a).len(), 1);
    assert_eq!(rig.issues(b).len(), 1);
    rig.stop();
}

#[test]
fn a_group_request_starts_no_turn() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let c = rig.hire();
    rig.send(
        a,
        json!({"to": {"agents": [b, c]}, "act": "request", "body": "both of you"}),
    );
    let (group, _, _) = rig.mail_to(b, MailAct::Request, 0);
    rig.mail_to(c, MailAct::Request, 0);
    rig.settle();
    assert!(rig.decisions(b).is_empty() && rig.decisions(c).is_empty());

    // The same daemon does start a turn for a direct request, and covers only that one.
    rig.request(c, b);
    let (direct, _, _) = rig.mail_to(b, MailAct::Request, 1);
    let (_, covered, _) = rig.issue(b, 0);
    assert_eq!(covered, vec![direct]);
    assert_ne!(direct, group);
    assert!(rig.decisions(c).is_empty());
    rig.stop();
}

#[test]
fn an_ack_does_not_consume_the_answer_but_a_later_message_does() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let request = rig.asked_and_ended(a, b, 0);
    rig.reply(b, request, "ack");
    let (ack, _, _) = rig.mail_to(a, MailAct::Ack, 0);
    rig.settle();
    assert!(rig.decisions(a).is_empty(), "{:?}", rig.decisions(a));
    rig.reply(b, request, "message");
    let (answer, _, _) = rig.mail_to(a, MailAct::Message, 0);
    let (turn, covered, _) = rig.issue(a, 0);
    assert_eq!(covered, vec![answer]);
    assert_ne!(answer, ack);
    rig.end(turn);
    rig.stop();
}

#[test]
fn a_thread_gets_two_mail_turns_and_a_third_request_is_refused_once() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let first = rig.asked_and_ended(a, b, 0);
    rig.send(
        a,
        json!({"in_reply_to": first, "act": "request", "body": "and 3+3?"}),
    );
    let (second, _, _) = rig.mail_to(b, MailAct::Request, 1);
    let (turn, covered, _) = rig.issue(b, 1);
    assert_eq!(covered, vec![second]);
    rig.end(turn);

    rig.send(
        a,
        json!({"in_reply_to": first, "act": "request", "body": "and 4+4?"}),
    );
    let decisions = rig.await_decisions(b, 3);
    assert_eq!(
        decisions[2],
        (
            MailTurnAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            },
            Some("mail turn thread limit reached: 2 of 2".to_owned())
        )
    );
    rig.settle();
    assert_eq!(rig.decisions(b).len(), 3, "a refusal is not decided again");
    assert_eq!(rig.issues(b).len(), 2);
    rig.stop();
}

#[test]
fn a_sender_gets_four_turns_a_day_and_the_next_day_decides_the_fifth_again() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    for nth in 0..4 {
        rig.asked_and_ended(a, b, nth);
    }
    let fifth = {
        rig.request(a, b);
        rig.mail_to(b, MailAct::Request, 4).0
    };
    let decisions = rig.await_decisions(b, 5);
    assert_eq!(
        decisions[4],
        (
            MailTurnAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            },
            Some("mail turn sender day limit reached: 4 of 4".to_owned())
        )
    );
    rig.settle();
    assert_eq!(rig.decisions(b).len(), 5);
    assert_eq!(rig.issues(b).len(), 4);

    rig.clock.advance(DAY_MS);
    rig.settle();
    let (turn, covered, _) = rig.issue(b, 4);
    assert_eq!(covered, vec![fifth]);
    assert!(matches!(
        rig.decisions(b).last(),
        Some((MailTurnAdmission::Issue { .. }, _))
    ));
    rig.end(turn);
    rig.stop();
}

#[test]
fn a_paused_agent_gets_no_mail_turn_until_it_resumes() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    rig.running.handle.ask(Ask::Pause(b)).unwrap();
    rig.request(a, b);
    let (mail, _, _) = rig.mail_to(b, MailAct::Request, 0);
    let decisions = rig.await_decisions(b, 1);
    assert_eq!(
        decisions[0].0,
        MailTurnAdmission::Hold {
            reason: HoldReason::Paused
        }
    );
    rig.settle();
    assert!(rig.issues(b).is_empty());
    rig.running.handle.ask(Ask::Resume(b)).unwrap();
    let (turn, covered, _) = rig.issue(b, 0);
    assert_eq!(covered, vec![mail]);
    rig.end(turn);
    rig.stop();
}

#[test]
fn an_agent_with_a_goal_is_not_given_a_mail_turn() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let c = rig.hire();
    rig.running.handle.ask(Ask::Pause(b)).unwrap();
    common::set_goal(
        &rig.running.handle,
        common::goal_params(b, &[common::true_program()]),
    );
    rig.request(a, b);
    rig.request(a, c);
    rig.mail_to(b, MailAct::Request, 0);
    let (_, covered, _) = rig.issue(c, 0);
    assert_eq!(covered.len(), 1);
    rig.settle();
    assert!(rig.decisions(b).is_empty() && rig.issues(b).is_empty());
    rig.stop();
}

#[test]
fn a_visible_billing_credential_without_a_ceiling_refuses_the_turn() {
    let rig = Rig::new();
    let a = rig.hire_without_ceiling();
    let b = rig.hire_without_ceiling();
    let fixture = rig.stop();
    let mut rig = Rig::start(
        fixture,
        |settings| {
            settings.floor_usd_per_day = None;
            settings.daemon_env.push(EnvVar::new(
                "ANTHROPIC_API_KEY",
                "fixture-value-not-a-credential",
            ));
        },
        Arc::new(TestClock::new()),
    );
    let mut params = common::hire_params(rig.fixture.state.path());
    params.budget.usd_per_day = Some(Usd::from_dollars(25));
    let duded::ports::Answered::Hired(hired) =
        rig.running.handle.ask(Ask::Hire(Box::new(params))).unwrap()
    else {
        panic!("hire");
    };
    let c = hired.agent;
    rig.request(a, b);
    rig.mail_to(b, MailAct::Request, 0);
    let decisions = rig.await_decisions(b, 1);
    assert_eq!(
        decisions[0].0,
        MailTurnAdmission::Exhausted {
            reason: ExhaustedReason::CeilingRequired
        }
    );
    rig.request(a, c);
    let (_, covered, _) = rig.issue(c, 0);
    assert_eq!(covered.len(), 1);
    assert!(rig.issues(b).is_empty());
    rig.stop();
}

#[test]
fn a_crash_between_issue_and_spawn_reissues_the_turn_once() {
    let fixture = Fixture::new();
    let host = UnconfirmedHost::with_native_evidence(true, fixture.journal_dir());
    let mut rig = Rig::start_on(
        fixture,
        |_| {},
        Arc::new(TestClock::new()),
        host.clone(),
        host.shared(),
    );
    let a = rig.hire();
    let b = rig.hire();
    rig.request(a, b);
    let (turn, covered, _) = rig.issue(b, 0);
    rig.settle();
    assert_eq!(covered.len(), 1);
    assert_eq!(host.children(), 0);
    let fixture = rig.stop();
    assert!(
        !fixture.records().iter().any(
            |record| matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn)
        )
    );

    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    rig.spawned(turn);
    rig.settle();
    let records = rig.stop().records();
    let count = |wanted: fn(&Record, TurnId) -> bool| {
        records.iter().filter(|record| wanted(record, turn)).count()
    };
    assert_eq!(
        count(
            |record, turn| matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn)
        ),
        1
    );
    assert_eq!(
        count(|record, turn| matches!(record, Record::MailTurnIssued(issue) if issue.turn == turn)),
        1
    );
}

#[test]
fn a_pause_between_the_crash_and_the_restart_keeps_the_turn_unlaunched_until_resume() {
    let fixture = Fixture::new();
    let host = UnconfirmedHost::with_native_evidence(true, fixture.journal_dir());
    let mut rig = Rig::start_on(
        fixture,
        |_| {},
        Arc::new(TestClock::new()),
        host.clone(),
        host.shared(),
    );
    let a = rig.hire();
    let b = rig.hire();
    rig.request(a, b);
    let (turn, _, _) = rig.issue(b, 0);
    rig.running.handle.ask(Ask::Pause(b)).unwrap();
    let fixture = rig.stop();

    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    rig.settle();
    assert!(
        !rig.fixture.records().iter().any(
            |record| matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn)
        )
    );
    rig.running.handle.ask(Ask::Resume(b)).unwrap();
    rig.spawned(turn);
    rig.stop();
}

fn mail_turn_story(records: &[Record]) -> Vec<dudes_proto::RecordKind> {
    records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Mail { .. }
                    | Record::MailTurnDecided(_)
                    | Record::MailTurnIssued(_)
                    | Record::TurnSpawned { .. }
                    | Record::TurnEnded { .. }
            )
        })
        .map(Record::kind)
        .collect()
}

#[test]
fn replay_folds_the_same_covered_mail_and_decides_nothing_twice() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let request = rig.asked_and_ended(a, b, 0);
    rig.reply(b, request, "message");
    let (turn, _, _) = rig.issue(a, 0);
    rig.end(turn);
    let story = mail_turn_story(&rig.fixture.records());
    assert_eq!(story.len(), 10, "{story:?}");
    let fixture = rig.stop();

    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    rig.settle();
    let fixture = rig.stop();
    assert_eq!(
        mail_turn_story(&fixture.records()),
        story,
        "a warm replay decides and issues nothing"
    );

    fs::remove_file(fixture.state.path().join(duded::boot::PROJECTION_FILE)).unwrap();
    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    rig.settle();
    let fixture = rig.stop();
    assert_eq!(
        mail_turn_story(&fixture.records()),
        story,
        "a cold replay decides and issues nothing"
    );
}

fn thread_of(rig: &Rig, to: AgentId, act: MailAct, nth: usize) -> ThreadId {
    rig.mail_to(to, act, nth).2.expect("a threaded mail")
}

#[test]
fn both_chats_show_the_mail_started_turns_with_their_replies() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    rig.chat(a, "ask B what 2+2 is");
    let chat = rig.chat_turn(a);
    rig.spawned(chat);
    rig.request(a, b);
    rig.end_with_reply(chat, "Asked B.");

    let (delivered, _, _) = rig.mail_to(b, MailAct::Request, 0);
    let thread = thread_of(&rig, b, MailAct::Request, 0);
    let (b_turn, _, _) = rig.issue(b, 0);
    rig.spawned(b_turn);
    rig.reply(b, delivered, "message");
    rig.mail_to(a, MailAct::Message, 0);
    rig.end_with_reply(b_turn, "It is 4.");
    let (a_turn, _, _) = rig.issue(a, 0);
    rig.end_with_reply(a_turn, "B says it is 4.");

    let a_rows = rig.await_chat_rows(a, 2);
    assert_eq!(a_rows.len(), 2, "{a_rows:?}");
    assert_eq!(a_rows[0]["text"], "ask B what 2+2 is");
    assert_eq!(a_rows[0]["reply"], "Asked B.");
    assert_eq!(a_rows[0]["cause"], Value::Null);
    assert_eq!(a_rows[1]["turn_id"], json!(a_turn));
    assert_eq!(a_rows[1]["text"], Value::Null);
    assert_eq!(a_rows[1]["state"], "ended");
    assert_eq!(a_rows[1]["reply"], "B says it is 4.");
    assert_eq!(
        a_rows[1]["cause"],
        json!({"sender": b, "thread": thread, "subject": "question"})
    );
    assert_eq!(a_rows[1]["notice"], Value::Null);

    let b_rows = rig.await_chat_rows(b, 1);
    assert_eq!(b_rows.len(), 1, "{b_rows:?}");
    assert_eq!(b_rows[0]["turn_id"], json!(b_turn));
    assert_eq!(b_rows[0]["reply"], "It is 4.");
    assert_eq!(b_rows[0]["cause"]["sender"], json!(a));
    assert_eq!(b_rows[0]["cause"]["thread"], json!(thread));
    rig.stop();
}

#[test]
fn a_capped_request_adds_one_notice_to_the_recipients_chat() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let first = rig.asked_and_ended(a, b, 0);
    rig.send(
        a,
        json!({"in_reply_to": first, "act": "request", "body": "and 3+3?"}),
    );
    let (turn, _, _) = rig.issue(b, 1);
    rig.end(turn);
    rig.send(
        a,
        json!({"in_reply_to": first, "act": "request", "body": "and 4+4?"}),
    );
    rig.await_decisions(b, 3);
    rig.settle();

    let rows = rig.await_chat_rows(b, 3);
    let notices: Vec<_> = rows
        .iter()
        .filter(|row| row["notice"] != Value::Null)
        .collect();
    assert_eq!(notices.len(), 1, "{rows:?}");
    assert_eq!(
        notices[0]["notice"],
        json!({"sender": a, "reason": "thread_limit"})
    );
    assert_eq!(notices[0]["turn_id"], Value::Null);
    assert_eq!(notices[0]["text"], Value::Null);
    assert_eq!(notices[0]["cause"], Value::Null);
    assert_eq!(
        rows.iter()
            .filter(|row| row["cause"] != Value::Null)
            .count(),
        2
    );
    assert_eq!(rig.chat_rows(a).len(), 0, "the sender's chat has no notice");
    rig.stop();
}

#[test]
fn a_paused_recipient_adds_one_notice_and_the_resumed_turn_adds_its_exchange() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    rig.running.handle.ask(Ask::Pause(b)).unwrap();
    rig.request(a, b);
    rig.await_decisions(b, 1);
    rig.settle();
    let rows = rig.await_chat_rows(b, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["notice"], json!({"sender": a, "reason": "paused"}));

    rig.running.handle.ask(Ask::Resume(b)).unwrap();
    let (turn, _, _) = rig.issue(b, 0);
    rig.end_with_reply(turn, "On it.");
    let rows = rig.await_chat_rows(b, 2);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["notice"]["reason"], "paused");
    assert_eq!(rows[1]["reply"], "On it.");
    assert_eq!(rows[1]["cause"]["sender"], json!(a));
    rig.stop();
}

#[test]
fn a_replay_rebuilds_the_same_chat_cold_and_warm() {
    let mut rig = Rig::new();
    let a = rig.hire();
    let b = rig.hire();
    let request = rig.asked_and_ended(a, b, 0);
    rig.reply(b, request, "message");
    let (turn, _, _) = rig.issue(a, 0);
    rig.end_with_reply(turn, "B answered.");
    rig.running.handle.ask(Ask::Pause(b)).unwrap();
    rig.request(a, b);
    rig.await_decisions(b, 2);
    let before = (rig.await_chat_rows(a, 1), rig.await_chat_rows(b, 2));
    assert_eq!(before.0[0]["reply"], "B answered.");
    assert_eq!(before.1[1]["notice"]["reason"], "paused");
    let fixture = rig.stop();

    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    assert_eq!((rig.chat_rows(a), rig.chat_rows(b)), before, "warm");
    let fixture = rig.stop();

    fs::remove_file(fixture.state.path().join(duded::boot::PROJECTION_FILE)).unwrap();
    let rig = Rig::start(fixture, |_| {}, Arc::new(TestClock::new()));
    assert_eq!((rig.chat_rows(a), rig.chat_rows(b)), before, "cold");
    rig.stop();
}

#[test]
fn a_mail_wake_of_an_agent_with_a_goal_adds_an_exchange() {
    let fixture = Fixture::new();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let mut rig = Rig::start_checked(
        fixture,
        |_| {},
        Arc::new(TestClock::new()),
        host.clone(),
        host,
        ScriptedRunner::scripted([common::passes()]),
    );
    let a = rig.hire();
    let b = rig.hire();
    common::set_goal(
        &rig.running.handle,
        common::goal_params(b, &[common::true_program()]),
    );
    let goal_turn = rig.fixture.await_turn();
    rig.end(goal_turn);
    rig.fixture
        .await_record("goal done", |record| match record {
            Record::LoopDecided {
                agent,
                decision: dudes_proto::LoopDecision::Done,
                ..
            } if *agent == b => Some(()),
            _ => None,
        });

    rig.send(
        a,
        json!({"to": {"kind": "agent", "agent": b}, "act": "message", "body": "fyi"}),
    );
    let (_, _, thread) = rig.mail_to(b, MailAct::Message, 0);
    rig.settle();
    let wake = rig
        .fixture
        .await_record("mail wake issue", |record| match record {
            Record::MailWakeOnlyTurnIssued(issue)
            | Record::IdentifiedMailWakeOnlyTurnIssued(issue)
                if issue.agent == b =>
            {
                Some(issue.turn)
            }
            _ => None,
        });
    rig.end_with_reply(wake, "Noted.");

    let rows = rig.await_chat_rows(b, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["turn_id"], json!(wake));
    assert_eq!(rows[0]["text"], Value::Null);
    assert_eq!(rows[0]["reply"], "Noted.");
    assert_eq!(rows[0]["cause"]["sender"], json!(a));
    assert_eq!(rows[0]["cause"]["thread"], json!(thread));
    assert!(rig.issues(b).is_empty(), "a wake is not a mail turn");
    rig.stop();
}
