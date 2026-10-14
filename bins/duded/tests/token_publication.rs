//! Token publication keeps its answer barrier without rebuilding an unchanged snapshot.
//!
//! The work witness counts entries copied into a fresh owned map. It does not time the pump and it
//! does not claim to count hashes filtered before that map is returned. The scaling fixtures keep
//! every mint live, so 1 and 10,000 retained mintings are exactly 1 and 10,000 copied entries when
//! a fresh snapshot is built.

use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use duded::ports::{Answered, Ask};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_journal::{Fsync, JournalOptions};
use dudes_proto::rpc::agent::RetireParams;
use dudes_proto::rpc::hook::HookToken;
use dudes_proto::rpc::{ErrorCode, Method, RefusalGate, RefusedData};
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::LaunchSpec;
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, NativeBoundaryAcknowledgement,
    NativeBoundaryReceipt, OwnedTargetState, OwnedTargets, OwnedTargetsStatus, ProcessHost,
};
use dudes_supervisor::input::SpendView;
use dudes_supervisor::{Input, SpawnResult, blake3_hex};
use dudes_types::{AgentId, Provider, Timestamp, Usd};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};

const ONE: usize = 1;
const MANY: usize = 10_000;
const RETAINED_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";

fn retained_agent() -> AgentId {
    AgentId::from_parts(0, 0)
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

/// Write one live agent and `mintings` distinct retained token hashes for replay to fold.
fn seed_live_mint_history(fixture: &Fixture, mintings: usize) {
    let options = JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 0,
        ..JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the seed journal");
    writer
        .append(&entry(Record::AgentHired {
            character: None,
            accent: None,
            id: retained_agent(),
            name: "History".to_owned(),
            role: "backend engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: fixture.state.path().to_path_buf(),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }))
        .expect("append the retained agent");
    for index in 0..mintings {
        let token_hash = if index == 0 {
            blake3_hex(RETAINED_TOKEN.as_bytes())
        } else {
            format!("{index:064x}")
        };
        writer
            .append(&entry(Record::HookTokenMinted {
                agent: retained_agent(),
                token_hash,
            }))
            .expect("append a retained minting");
    }
}

/// A process-host adapter whose scripted kill observation is `Gone` and countable.
///
/// The runtime's `KillOutcome::Refused` → `Input::Liveness` → `TurnEnded` chain remains the shipped
/// one; only the platform observation at the existing host seam is deterministic.
#[derive(Clone, Debug)]
struct GoneOnKillHost {
    inner: SharedHost,
    kills: Arc<AtomicUsize>,
}

impl GoneOnKillHost {
    fn with_native_evidence(journal: PathBuf) -> Self {
        Self {
            inner: SharedHost::with_native_evidence(journal),
            kills: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn kills(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }
}

impl ProcessHost for GoneOnKillHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, _key: &AdoptionKey) -> KillOutcome {
        self.kills.fetch_add(1, Ordering::SeqCst);
        KillOutcome::Refused {
            liveness: Liveness::Gone,
        }
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        self.inner.owned_targets(limit)
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        self.inner.acknowledge_native_boundary_absence(key, receipt)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }
}

fn await_native_spawn(
    fixture: &Fixture,
    handle: &duded::Handle,
    host: &impl ProcessHost,
    agent: AgentId,
) -> dudes_types::TurnId {
    let (turn, key) =
        fixture.await_record("the exact owned managed spawn", |record| match record {
            Record::TurnSpawned {
                agent: owner,
                turn,
                pid,
                start_time,
                boot,
                ..
            } if *owner == agent => Some((
                *turn,
                AdoptionKey {
                    pid: *pid,
                    start_time: *start_time,
                    boot: boot.clone(),
                },
            )),
            _ => None,
        });
    let spawned = fixture.entries().into_iter().find_map(|(lsn, entry)| {
        matches!(entry.record, Record::TurnSpawned { agent: owner, turn: issued, .. } if owner == agent && issued == turn).then_some(lsn)
    }).unwrap();
    assert!(
        handle
            .doctor()
            .expect("finish the exact spawn's setup pump")
            .lsn
            >= spawned
    );
    let owned = host.owned_targets(1);
    assert_eq!(owned.status, OwnedTargetsStatus::Complete);
    let [target] = owned.targets.as_slice() else {
        panic!("one exact native target must be retained")
    };
    let native = target.key.as_ref().unwrap();
    assert_eq!(
        (native.pid, native.start_time, &native.boot),
        (key.pid, key.start_time, &key.boot)
    );
    assert_eq!(target.state, OwnedTargetState::Present);
    assert!(target.native_absence_receipt.is_none());
    turn
}

fn ping_code(client: &mut dudes_rpc::LocalClient, token: &HookToken) -> Option<i32> {
    client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the publication observation is answered")
        .error
        .map(|error| error.code)
}

fn assert_goal_fold(fixture: &Fixture, agent: AgentId, goal: dudes_types::GoalId) {
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::GoalSet { agent: owner, goal: changed, .. } if *owner == agent && changed.id == goal)));
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    assert_eq!(world.goal_of(agent).unwrap().goal.id, goal);
}

fn await_minted_setup(fixture: &Fixture, handle: &duded::Handle, agent: AgentId) -> HookToken {
    let hash = fixture.await_record("the exact setup mint", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    let entries = fixture.entries();
    let minted = entries.iter().find_map(|(lsn, entry)| {
        matches!(&entry.record, Record::HookTokenMinted { agent: owner, token_hash } if *owner == agent && *token_hash == hash).then_some(*lsn)
    }).unwrap();
    let (issued, turn) = entries
        .iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::TurnIssued {
                agent: owner, turn, ..
            } if owner == agent => Some((*lsn, turn)),
            _ => None,
        })
        .unwrap();
    let started = entries.iter().find_map(|(lsn, entry)| {
        matches!(entry.record, Record::InstructionObservationStarted {
            owner: dudes_proto::InstructionObservationOwner::Managed { agent: owner, turn: sample_turn, .. }, ..
        } if owner == agent && sample_turn == turn).then_some(*lsn)
    }).unwrap();
    assert!(started < issued && issued < minted);
    assert!(
        handle
            .doctor()
            .expect("finish the asynchronous mint's setup pump")
            .lsn
            >= minted
    );
    common::token_of(&hash)
}

/// Return copied entries for the required initial snapshot and for one later fold-free pump.
fn publication_work(mintings: usize) -> (usize, usize) {
    let fixture = Fixture::new();
    seed_live_mint_history(&fixture, mintings);
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot the retained history");
    let work = daemon.token_publication_work();
    let initial = work.copied_entries();
    let mut running = common::start(daemon);

    let before = work.copied_entries();
    running.handle.input(Input::Spend {
        agent: retained_agent(),
        view: SpendView::default(),
    });
    running
        .handle
        .doctor()
        .expect("the FIFO barrier follows the quiet pump");
    let quiet = work.copied_entries() - before;

    running.stop(ExitCause::Ended);
    (initial, quiet)
}

/// The direct work unit stays constant when the fold did not move, independent of mint history.
#[test]
fn token_publication_work_is_constant_in_mint_history() {
    let one = publication_work(ONE);
    let many = publication_work(MANY);

    assert_eq!(
        (one.0, many.0),
        (ONE, MANY),
        "control: a fresh publication copies every live retained minting"
    );
    assert_eq!(
        (one.1, many.1),
        (0, 0),
        "a quiet pump must reuse the published Arc instead of copying mint history"
    );
}

#[test]
fn changed_token_projection_is_published_before_answer() {
    let fixture = Fixture::new();
    seed_live_mint_history(&fixture, ONE);
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = common::seams_with(
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let sentinel = HookToken::new(RETAINED_TOKEN).unwrap();
    let mut client = Client::connect(settings.socket()).expect("connect before the goal subject");
    assert_eq!(
        ping_code(&mut client, &sentinel),
        Some(ErrorCode::Unauthorized.code())
    );

    let agent = common::hire(&running.handle, fixture.state.path());
    let before_goal = fixture.entries();
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    let after_goal = fixture.entries();
    assert!(after_goal.starts_with(&before_goal));
    assert_goal_fold(&fixture, agent, goal);
    assert_eq!(
        ping_code(&mut client, &sentinel),
        Some(ErrorCode::Unauthorized.code()),
        "the goal answer publishes its completed owner fold before later setup"
    );
    if let Some(hash) = after_goal[before_goal.len()..]
        .iter()
        .find_map(|(_, entry)| match &entry.record {
            Record::HookTokenMinted {
                agent: owner,
                token_hash,
            } if *owner == agent => Some(token_hash.clone()),
            _ => None,
        })
    {
        assert_eq!(
            ping_code(&mut client, &common::token_of(&hash)),
            Some(ErrorCode::Unauthorized.code()),
            "a mint present in the immediate suffix is already published"
        );
    }
    let token = await_minted_setup(&fixture, &running.handle, agent);
    let turn = await_native_spawn(&fixture, &running.handle, &host, agent);

    assert_eq!(
        ping_code(&mut client, &token),
        Some(ErrorCode::Unauthorized.code()),
        "the later mint setup publishes the live owner"
    );

    let retired = running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "the publication control completed".to_owned(),
        })))
        .expect("retire is answered");
    assert!(matches!(retired, Answered::Done));
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentRetired { id, .. } if *id == agent))
    );

    let demoted = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the retirement publication observation is answered");
    let error = demoted
        .error
        .expect("the retire answer must precede token-owner removal");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal data")).expect("a RefusedData");
    assert_eq!(data.gate, RefusalGate::Token);
    assert_eq!(
        column.asked(),
        1,
        "the same live connection observes mint and retirement publication"
    );

    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    running.stop(ExitCause::Ended);
}

/// A completed retirement fold remains refused after a later quiet pump.
///
/// Observed from outside the agent's tree, because the failed `TurnEnded` leaves its turn child
/// recorded, and inside the tree it still resolves to its agent.
#[test]
fn failed_retirement_stays_refused_after_the_next_quiet_pump() {
    let fixture = Fixture::new();
    seed_live_mint_history(&fixture, ONE);
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let host = GoneOnKillHost::with_native_evidence(fixture.journal_dir());
    let seams = common::seams_with(
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    host.inner.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot the retained live token");
    let append_failure = daemon.append_failure();
    let mut running = common::start(daemon);

    common::set_goal(
        &running.handle,
        common::goal_params(retained_agent(), &[common::true_program()]),
    );
    await_native_spawn(&fixture, &running.handle, &host, retained_agent());
    let token = HookToken::new(RETAINED_TOKEN).expect("the retained token is well formed");
    let mut client = Client::connect(settings.socket()).expect("connect with the retained token");
    let before = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the pre-retirement observation is answered");
    assert_eq!(
        before.error.map(|error| error.code),
        Some(ErrorCode::Unauthorized.code()),
        "control: replay published the retained live token, which row 2 refuses outside its tree"
    );

    append_failure.fail_nth(NonZeroUsize::new(2).expect("two is nonzero"));
    let failure = running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retained_agent(),
            reason: "exercise the failed-pump publication".to_owned(),
        })))
        .expect_err("the TurnEnded append is injected to fail");
    assert_eq!(failure.code, ErrorCode::InternalError.code());
    assert_eq!(
        host.kills(),
        1,
        "the retirement reached the Gone kill refusal"
    );
    let records = fixture.records();
    assert!(records.iter().any(
        |record| matches!(record, Record::AgentRetired { id, .. } if *id == retained_agent())
    ));
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnEnded { .. })),
        "the second append fails after the durable retirement fold"
    );
    assert_eq!(
        host.owned_targets(1).targets[0].state,
        OwnedTargetState::Present,
        "a Gone kill answer alone supplies no native reap evidence"
    );

    running.handle.input(Input::Spend {
        agent: retained_agent(),
        view: SpendView::default(),
    });
    running
        .handle
        .doctor()
        .expect("the daemon serves a FIFO barrier after the failed Ask pump");
    let after = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the post-quiet-pump observation is answered");
    let error = after
        .error
        .expect("the completed retirement must remain published after a quiet pump");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal data")).expect("a RefusedData");
    assert_eq!(data.gate, RefusalGate::Token);

    running.stop(ExitCause::Ended);
}

/// A failed pump publishes its completed retirement fold before returning the error.
///
/// Observed from outside the agent's tree, because the failed `TurnEnded` leaves its turn child
/// recorded.
#[test]
fn failed_retirement_refuses_the_same_connection_before_another_pump() {
    let fixture = Fixture::new();
    seed_live_mint_history(&fixture, ONE);
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let host = GoneOnKillHost::with_native_evidence(fixture.journal_dir());
    let seams = common::seams_with(
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    host.inner.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot the retained live token");
    let append_failure = daemon.append_failure();
    let mut running = common::start(daemon);

    common::set_goal(
        &running.handle,
        common::goal_params(retained_agent(), &[common::true_program()]),
    );
    await_native_spawn(&fixture, &running.handle, &host, retained_agent());
    let token = HookToken::new(RETAINED_TOKEN).expect("the retained token is well formed");
    let mut client = Client::connect(settings.socket()).expect("connect with the retained token");
    let before = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the pre-retirement observation is answered");
    assert_eq!(
        before.error.map(|error| error.code),
        Some(ErrorCode::Unauthorized.code()),
        "control: replay published the retained live token, which row 2 refuses outside its tree"
    );

    append_failure.fail_nth(NonZeroUsize::new(2).expect("two is nonzero"));
    let failure = running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retained_agent(),
            reason: "exercise immediate failed-pump publication".to_owned(),
        })))
        .expect_err("the TurnEnded append is injected to fail");
    assert_eq!(failure.code, ErrorCode::InternalError.code());
    assert_eq!(
        host.kills(),
        1,
        "the retirement reached the Gone kill refusal"
    );
    let records = fixture.records();
    assert!(records.iter().any(
        |record| matches!(record, Record::AgentRetired { id, .. } if *id == retained_agent())
    ));
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnEnded { .. })),
        "the second append fails after the durable retirement fold"
    );
    assert_eq!(
        host.owned_targets(1).targets[0].state,
        OwnedTargetState::Present,
        "a Gone kill answer alone supplies no native reap evidence"
    );

    let after = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("the immediate post-error observation is answered");
    let error = after
        .error
        .expect("the failed Ask must publish completed folds before returning");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal data")).expect("a RefusedData");
    assert_eq!(data.gate, RefusalGate::Token);
    assert_eq!(
        column.asked(),
        1,
        "the token projection changed on the same live connection"
    );

    running.stop(ExitCause::Ended);
}
