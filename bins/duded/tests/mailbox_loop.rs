//! Mailbox progress shares the writer with hooks and jobs. Queued hooks must run before a backlog
//! drains, and paused agents must receive a denial through the socket route.
#![allow(missing_docs)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::hook_shim::{RecordingResolver, payload};
use common::{
    Fixture, Running, ScriptedRunner, SharedHost, TestClock, goal_params, hire, seams_with,
    set_goal,
};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::system::{DoctorMailboxResult, MailboxProblem};
use dudes_proto::{HookDecision, HookReply, PeerProvenance, Record};
use dudes_rpc::auth::PeerResolver;
use dudes_supervisor::Clock as _;
use dudes_types::{AgentId, MailId};
use serde_json::json;

mod common;

struct Floor {
    fixture: Fixture,
    settings: duded::Settings,
    running: Running,
    resolver: Arc<RecordingResolver>,
    agents: Vec<AgentId>,
}

impl Floor {
    /// A daemon with `count` hired agents. Hooks are sent as the first of them.
    fn new(count: usize) -> Self {
        let fixture = Fixture::new();
        let settings = fixture.settings();
        let resolver = RecordingResolver::new(PeerProvenance::Unresolvable);
        let parts = seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&resolver) as Arc<dyn PeerResolver>,
        );
        let daemon = boot(&settings, parts).expect("boot daemon");
        let running = common::start(daemon);
        let agents: Vec<_> = (0..count)
            .map(|_| hire(&running.handle, fixture.state.path()))
            .collect();
        set_goal(&running.handle, goal_params(agents[0], &["true"]));
        Self {
            fixture,
            settings,
            running,
            resolver,
            agents,
        }
    }

    fn directory(&self, agent: AgentId, name: &str) -> PathBuf {
        self.fixture
            .state
            .path()
            .join("agents")
            .join(agent.to_string())
            .join(name)
    }

    fn send(&self, sender: AgentId, recipient: AgentId, id: MailId) {
        let outbox = self.directory(sender, "outbox");
        fs::create_dir_all(&outbox).unwrap();
        fs::write(
            outbox.join(format!("{id}.json")),
            serde_json::to_vec(&json!({
                "to": {"kind": "agent", "agent": recipient},
                "act": "message", "subject": "load", "body": "inline"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// `system.doctor` from a peer the daemon cannot attribute to an agent, as an operator calls it.
    fn mailbox_report(&self) -> DoctorMailboxResult {
        self.resolver.set(PeerProvenance::Unresolvable);
        let credential = fs::read_to_string(
            self.settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap();
        let response = dudes_rpc::Client::connect(self.settings.socket())
            .unwrap()
            .call_as_operator(
                Method::SystemDoctor,
                None,
                OperatorCredential::new(credential).unwrap(),
            )
            .unwrap();
        serde_json::from_value(response.result.expect("a doctor result")).unwrap()
    }
}

#[test]
fn ten_thousand_inbox_entries_defer_the_recipient_without_blocking_a_queued_hook() {
    let mut floor = Floor::new(2);
    let (sender, recipient) = (floor.agents[0], floor.agents[1]);
    assert!(matches!(
        floor.running.handle.ask(duded::ports::Ask::Pause(sender)),
        Ok(duded::ports::Answered::Done)
    ));
    floor.running.stop(ExitCause::ShutdownRpc);
    let inbox = floor.directory(recipient, "inbox");
    fs::create_dir_all(&inbox).unwrap();
    for n in 0..10_000 {
        fs::write(inbox.join(format!("junk-{n}")), "").unwrap();
    }
    let id = MailId::from_parts(7, 1);
    floor.send(sender, recipient, id);
    let id = dudes_journal::mail_recipient_id(id, recipient);
    let clock = Arc::new(TestClock::new());
    let daemon = boot(
        &floor.settings,
        seams_with(
            &floor.settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
            Arc::clone(&floor.resolver) as Arc<dyn PeerResolver>,
        ),
    )
    .expect("reboot before queuing overflowed mailbox work");
    let handle = daemon.handle();
    let event = dudes_providers::claude::translate(sender, clock.now(), &payload("PreToolUse"))
        .expect("translate the hook");
    let hook = handle
        .hook_queued_for_test(event)
        .expect("queue the hook before the writer starts");
    let marker = handle
        .ask_queued(duded::ports::Ask::Pause(recipient))
        .expect("queue the writer-order marker after the hook");
    floor.running = common::start(daemon);
    let reply = hook
        .recv_timeout(Duration::from_secs(30))
        .expect("the writer did not answer the queued hook")
        .expect("the writer refused the queued hook")
        .into_reply_for_test();
    assert_eq!(
        reply,
        HookReply {
            decision: HookDecision::Deny {
                reason: "the loop is paused".to_owned(),
            },
            context: None,
        },
        "the writer answered the paused hook"
    );
    assert!(matches!(
        marker.recv_timeout(Duration::from_secs(30)),
        Ok(Ok(duded::ports::Answered::Done))
    ));
    assert!(
        floor
            .mailbox_report()
            .mailbox
            .diagnostics
            .iter()
            .any(|row| {
                row.agent == recipient
                    && row.filename == "inbox"
                    && row.problem == MailboxProblem::ScanOverflow
            })
    );
    floor.running.stop(ExitCause::ShutdownRpc);
    let records = floor.fixture.records();
    let marker = records
        .iter()
        .position(|record| matches!(record, Record::Pause { agent, .. } if *agent == recipient))
        .expect("the writer-order marker is durable");
    assert!(
        records[..marker]
            .iter()
            .any(|record| matches!(record, Record::Mail { id: found, .. } if *found == id))
    );
    assert!(!inbox.join(format!("{id}.json")).exists());
}

#[test]
fn back_to_back_jobs_do_not_each_run_a_mailbox_pass() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot daemon");
    let passes = daemon.mailbox_passes();
    let mut running = common::start(daemon);
    hire(&running.handle, fixture.state.path());
    let before = passes.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..200 {
        running.handle.doctor().unwrap();
    }
    let ran = passes.load(Ordering::Relaxed) - before;
    let gaps = u64::try_from(started.elapsed().as_millis() / 100).unwrap();
    assert!(ran <= gaps + 2, "{ran} passes in {:?}", started.elapsed());
    running.stop(ExitCause::ShutdownRpc);
}
