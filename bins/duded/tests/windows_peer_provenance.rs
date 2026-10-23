//! Native Windows acceptance for the peer-provenance relation — **both directions**.
//!
//! # What this file is for, and why one direction would prove nothing
//!
//! The control is a pair: the *same* operator credential, presented by the *same* binary,
//! over the *same* pipe, refused from inside an agent's job and admitted from outside it. Either
//! half alone is satisfiable by a mistake. A daemon that refused everything would pass the inside
//! arm; a daemon that admitted everything — which is exactly `NoWalk`'s behaviour — would
//! pass the outside arm. Only the pair separates a walking resolver from one that answers a
//! constant, which is the claim this test exists to make false: *"a test that only ever passes
//! cannot tell a walking resolver from `NoWalk`."*
//!
//! The inside arm carries its own **red control** as well: the very same presentation, scored
//! against `NoWalk`'s column through the same `PeerClassifier`, must come back `Operator`. That is
//! the admission a walking resolver must not give, asserted rather than described, so the refusal below
//! cannot be read as "the fixture was broken".
//!
//! # Only a native Windows run executes these arms
//!
//! They compile and lint under `--target x86_64-pc-windows-msvc`, which establishes *compile and
//! lint only*: runtime failures such as a clock-tick collision or a Windows `ACCESS_DENIED` are
//! invisible to every type-checker on every platform. The only route that executes the arms is a
//! native Windows test run, so until one reports, every assertion here is a reviewed shape.
#![cfg(windows)]

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use duded::classify::{NoWalk, Resolver, WorldTokens};
use dudes_host::{RealHost, disconnect_named_pipe};
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_rpc::auth::{Authorizer as _, PeerIdentity, PeerPid, PeerResolution, PeerResolver as _};
use dudes_rpc::{
    Dispatcher, Listener as _, LocalClient, LocalTransport, PeerClassifier, PeerProvenance,
    PeerRole, Requirement, StopFlag, serve,
};
use dudes_supervisor::World;
use dudes_supervisor::host::{AdoptionKey, ContainmentUnproven, ProcessHost as _, SpawnResult};
use dudes_types::{AgentId, GoalId, Lsn, Mode, Provider, Timestamp, TurnId, Usd};

const RETURN_BOUND: Duration = Duration::from_secs(5);

const HELPER: &str = "DUDES_WINDOWS_PEER_HELPER";
const ENDPOINT: &str = "DUDES_WINDOWS_PEER_ENDPOINT";
const CONNECTED: &str = "DUDES_WINDOWS_PEER_CONNECTED";
const RESULT: &str = "DUDES_WINDOWS_PEER_RESULT";

/// How long any arm waits for a file or a response before calling the property false.
const BOUND: Duration = Duration::from_secs(10);

/// The adapter, duplicated from `bins/duded/src/main.rs`.
///
/// **Duplicated deliberately and not shared**: `main.rs` is a binary, so an integration test cannot
/// reach into it. The two are no longer the same shape — `main.rs` lifted its mapping into the
/// named `relation_of` so that it could be called with a variant the adapter can never receive,
/// while this copy still matches inline — so what they share is the five arms and their
/// destinations, not their text.
///
/// **The consequence: this suite exercises *this*
/// copy.** Nothing here can catch a wrong arm in `main.rs`; that mapping is pinned by
/// `the_adapter_maps_an_abandoned_relation_to_abandoned` in `main.rs`'s own test module, and by
/// nothing in this file. A fifth arm added there and forgotten here is a compile error; a fifth
/// arm *mis-pointed* there is caught only by that unit test.
#[derive(Debug)]
struct HostPeerProcesses(dudes_host::PeerProcesses);

impl duded::classify::ProcessRelation for HostPeerProcesses {
    fn begin_spawn(&self) -> u64 {
        self.0.begin_spawn()
    }

    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.0.finish_spawn(slot, key)
    }

    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.0.published(children)
    }

    fn resolve(
        &self,
        peer: u32,
        _pidversion: Option<u32>,
        children: &[AdoptionKey],
    ) -> duded::classify::Relation {
        match self.0.resolve(peer, children) {
            dudes_host::PeerRelation::Matched(index) => duded::classify::Relation::Matched(index),
            dudes_host::PeerRelation::Pending(slot) => duded::classify::Relation::Pending(slot),
            dudes_host::PeerRelation::NoMatch => duded::classify::Relation::NoMatch,
            dudes_host::PeerRelation::Unresolvable => duded::classify::Relation::Unresolvable,
            dudes_host::PeerRelation::Abandoned => duded::classify::Relation::Abandoned,
        }
    }
}

fn credential() -> OperatorCredential {
    OperatorCredential::new("C".repeat(OPERATOR_CREDENTIAL_LEN)).expect("valid credential")
}

fn agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("agent id")
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

/// A `World` in which `key` is the open child of a live agent — the publication the inside arm
/// withholds and then supplies.
fn world_with_child(key: &AdoptionKey) -> World {
    let agent = agent();
    let turn = TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("turn id");
    let goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("goal id");
    let mut world = World::default();
    world.apply(
        Lsn(1),
        &entry(Record::AgentHired {
            character: None,
            accent: None,
            id: agent,
            name: "Kim".to_owned(),
            role: "backend engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: std::env::temp_dir(),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }),
    );
    world.apply(
        Lsn(2),
        &entry(Record::TurnIssued {
            agent,
            turn,
            goal,
            task: "hold a local connection".to_owned(),
            session: None,
            mode: Mode::Managed,
            model_selection: None,
            lsn_of_decision: Lsn::from_parts(1, 1).expect("lsn"),
            answers: Vec::new(),
        }),
    );
    world.apply(
        Lsn(3),
        &entry(Record::TurnSpawned {
            agent,
            turn,
            pid: key.pid,
            start_time: key.start_time,
            boot: key.boot.clone(),
            session: None,
            reissued: false,

            mcp_grant: None,
        }),
    );
    world
}

fn presented() -> dudes_rpc::Presented {
    dudes_rpc::Presented {
        token: None,
        cred: Some(credential()),
    }
}

/// The test binary re-executes only this function as the process inside `RealHost`'s job.
#[test]
fn windows_peer_helper() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let endpoint = std::env::var_os(ENDPOINT).expect("helper endpoint");
    let connected =
        std::path::PathBuf::from(std::env::var_os(CONNECTED).expect("helper connected path"));
    let result = std::path::PathBuf::from(std::env::var_os(RESULT).expect("helper result path"));
    let mut client = LocalClient::connect(endpoint).expect("helper connects to the named pipe");
    std::fs::write(connected, b"connected").expect("helper records its connection");
    let response = client
        .call_as_operator(Method::AgentWhy, None, credential())
        .expect("helper receives a response");
    let outcome = response
        .error
        .map_or_else(|| "ok".to_owned(), |error| error.code.to_string());
    std::fs::write(result, outcome).expect("helper records the response code");
}

fn helper_spec(
    endpoint: &std::path::Path,
    connected: &std::path::Path,
    result: &std::path::Path,
    cwd: &std::path::Path,
) -> LaunchSpec {
    LaunchSpec {
        argv: [
            std::env::current_exe()
                .expect("test binary")
                .into_os_string(),
            OsString::from("--exact"),
            OsString::from("windows_peer_helper"),
            OsString::from("--quiet"),
        ]
        .into(),
        env: vec![
            EnvVar::new(HELPER, "1"),
            EnvVar::new(ENDPOINT, endpoint.as_os_str()),
            EnvVar::new(CONNECTED, connected.as_os_str()),
            EnvVar::new(RESULT, result.as_os_str()),
        ],
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    }
}

/// The server, with a **counting** handler.
///
/// The count is the observable the refusal arm reads, and it is server-side on purpose: the
/// helper's result file cannot tell a refusal from a crash — both leave it absent — while a
/// handler that never ran is a fact only this process can hold.
fn start_server(
    transport: Arc<LocalTransport>,
    resolver: Resolver,
    executions: Arc<AtomicUsize>,
) -> (Arc<StopFlag>, mpsc::Receiver<std::io::Result<()>>) {
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        credential(),
        Arc::new(WorldTokens::new()),
    ));
    dispatcher.register(Method::AgentWhy, Requirement::Operator, move |_| {
        executions.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "answer": "outside admitted" }))
    });
    let stop = Arc::new(StopFlag::new());
    let serving_stop = Arc::clone(&stop);
    let (finished, returned) = mpsc::channel();
    std::thread::spawn(move || {
        let result = serve(
            &*transport,
            &resolver,
            &dispatcher,
            &serving_stop,
            NonZeroUsize::new(4).expect("nonzero cap"),
        );
        let _ = finished.send(result);
    });
    (stop, returned)
}

fn stop_server(
    endpoint: &std::path::Path,
    stop: &StopFlag,
    returned: &mpsc::Receiver<std::io::Result<()>>,
) {
    stop.stop();
    let wake = wake_server_without_waiting(endpoint);
    returned
        .recv_timeout(RETURN_BOUND)
        .expect("serve did not return after the pipe was halted")
        .expect("serve succeeds");
    drop(wake);
}

fn wake_server_without_waiting(endpoint: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(endpoint)
}

#[test]
fn busy_shutdown_wake_returns_before_the_server_completion_bound() {
    let root = tempfile::tempdir().expect("temporary pipe root");
    let transport = bind(&root.path().join("run"));
    let endpoint = transport.path().to_path_buf();
    let held = wake_server_without_waiting(&endpoint).expect("connect the sole pending instance");
    let (release, released) = mpsc::channel();
    let (wake, elapsed) = std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = released.recv_timeout(RETURN_BOUND + Duration::from_millis(250));
            let accepted = transport.accept();
            drop(accepted);
            drop(held);
        });
        let started = Instant::now();
        let wake = wake_server_without_waiting(&endpoint);
        let elapsed = started.elapsed();
        let _ = release.send(());
        (wake, elapsed)
    });
    assert!(
        elapsed < RETURN_BOUND,
        "wake waited for a busy pipe beyond {RETURN_BOUND:?}"
    );
    assert_eq!(
        wake.expect_err("busy pipe does not wait for an instance")
            .raw_os_error(),
        Some(231)
    );
}

/// A host that still owns a child cannot prove complete containment, even after the
/// child stopped and was reaped, so only an empty host shuts down without an error.
fn shut_down(mut host: RealHost) {
    let owned = host.owned();
    match host.shutdown() {
        Ok(()) => assert_eq!(
            owned, 0,
            "a host that owned {owned} children proved containment"
        ),
        Err(error) => {
            assert_ne!(owned, 0, "an empty host failed to shut down: {error}");
            assert!(ContainmentUnproven::is(&error), "{error}");
        }
    }
}

fn bind(runtime: &std::path::Path) -> Arc<LocalTransport> {
    Arc::new(
        LocalTransport::bind_in_with_credential_and_halt(
            runtime,
            "duded.sock",
            &credential(),
            disconnect_named_pipe,
        )
        .expect("bind protected pipe"),
    )
}

fn spawned(result: SpawnResult) -> AdoptionKey {
    match result {
        SpawnResult::Spawned(key) => key,
        SpawnResult::NotLaunched { reason } | SpawnResult::CleanupUnconfirmed { reason } => {
            panic!("the helper was not launched: {reason}")
        }
    }
}

/// The same operator credential is refused from inside an agent's job and admitted
/// from outside it.
///
/// **This is the control, and it is red-before/green-after by construction.** The
/// red arm is not a separate run against an older tree: it is asserted here, in the same test, by
/// scoring the *identical* presentation against `NoWalk`'s column through the *identical*
/// classifier. That must answer `Operator` — the admission a walking resolver must not give — so the refusal
/// beside it cannot be the fixture failing to present anything.
#[test]
fn windows_operator_credential_is_refused_inside_an_agent_job_and_admitted_outside() {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    let connected = root.path().join("helper-connected");
    let result = root.path().join("inside-result");
    let transport = bind(&runtime);
    let endpoint = transport.path().to_path_buf();
    let mut host = RealHost::with_grace(Duration::from_millis(300));
    let resolver = Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    let executions = Arc::new(AtomicUsize::new(0));
    let (stop, returned) = start_server(
        Arc::clone(&transport),
        resolver.clone(),
        Arc::clone(&executions),
    );

    let pending = resolver
        .begin_spawn()
        .expect("live resolver arms the spawn");
    let key = spawned(host.spawn(
        &helper_spec(&endpoint, &connected, &result, root.path()),
        &ManagedInstall::default(),
    ));
    assert!(
        resolver.map_spawn(pending, &key),
        "host and daemon agree on the full adoption key"
    );

    // The helper connects before the agent mapping is published, so its connection is the one the
    // barrier holds. Waiting on its own marker rather than on a sleep is what makes that a fact.
    let deadline = Instant::now() + BOUND;
    while !connected.exists() {
        assert!(
            Instant::now() < deadline,
            "the immediate helper connection never reached the listening server"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the inside call reached the handler while the key-to-agent publication was withheld"
    );

    resolver.publish(&world_with_child(&key));
    let inside = loop {
        if let Ok(answer) = std::fs::read_to_string(&result) {
            break answer;
        }
        assert!(
            Instant::now() < deadline,
            "inside call produced no response"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        inside,
        ErrorCode::Unauthorized.code().to_string(),
        "an operator credential presented from inside an agent's job must be refused"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the refused inside call still ran the handler"
    );

    // The red control: the identical presentation, scored against `NoWalk`'s column.
    let inside_peer = PeerIdentity {
        uid: None,
        gid: None,
        pid: PeerPid::Reported {
            pid: i32::try_from(key.pid).expect("Windows pid fits identity"),
            pidversion: None,
        },
    };
    let PeerResolution::Resolved(old) = NoWalk.resolve(&inside_peer) else {
        panic!("NoWalk completes every resolution and never refuses");
    };
    let old_classifier = PeerClassifier::new(credential(), Arc::new(WorldTokens::new()));
    assert!(
        matches!(
            old_classifier.authorize(presented(), old, "agent.why"),
            Ok(PeerRole::Operator)
        ),
        "red control: the NoWalk column admits this same presentation as Operator"
    );

    // The other direction, same credential, same pipe, same handler.
    let mut outside = LocalClient::connect(&endpoint).expect("outside client connects");
    let outside = outside
        .call_as_operator(Method::AgentWhy, None, credential())
        .expect("outside client receives response");
    assert!(
        outside.error.is_none(),
        "outside operator was refused: {outside:?}"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "exactly the outside call reached the handler"
    );

    stop_server(&endpoint, &stop, &returned);
    assert_eq!(host.owned(), 1, "the host still owns the inside helper");
    shut_down(host);
}

/// A pending child that never reaches publication is terminated, its waiter released,
/// and its call never answered.
///
/// **The refusal is read from the counting handler, not from the helper's result file.** An absent
/// result file cannot tell a refused call from a helper that crashed, and a child being terminated
/// is exactly the circumstance in which it would crash — so the file proves nothing here while the
/// count proves the handler never ran.
#[test]
fn pending_spawn_timeout_terminates_and_releases_the_failed_child() {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    let connected = root.path().join("helper-connected");
    let result = root.path().join("inside-result");
    let transport = bind(&runtime);
    let endpoint = transport.path().to_path_buf();
    let mut host = RealHost::with_grace(Duration::from_millis(300));
    let resolver = Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    let executions = Arc::new(AtomicUsize::new(0));
    let (stop, returned) = start_server(
        Arc::clone(&transport),
        resolver.clone(),
        Arc::clone(&executions),
    );
    let pending = resolver
        .begin_spawn()
        .expect("live resolver arms the spawn");
    let mut spec = helper_spec(&endpoint, &connected, &result, root.path());
    // Enough stdin that the write cannot complete while the helper is busy connecting and calling:
    // the spawn stays unfinished, which is the state the barrier is about.
    spec.stdin = dudes_providers::launch::LaunchInput::WriteThenClose(vec![b'x'; 4 * 1024 * 1024]);

    let started = Instant::now();
    let outcome = host.spawn(&spec, &ManagedInstall::default());
    resolver.abort_spawn(pending);
    assert!(
        !matches!(outcome, SpawnResult::Spawned(_)),
        "the unpublished pending child was reported as a healthy spawn"
    );
    assert!(
        started.elapsed() < BOUND,
        "the publication barrier did not release within its bound"
    );
    assert!(connected.exists(), "control: the helper never connected");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the pending helper's call reached the handler before termination"
    );
    // **Ownership is asserted against the outcome the host reported, because there are two
    // distinct outcomes.** Only a *confirmed* absence releases the child: `Cleanup::Confirmed` removes
    // it and answers `NotLaunched`, while a cleanup the host could not confirm retains it and
    // answers `CleanupUnconfirmed`. An unconditional
    // `owned() == 0` would assert one of the two decided outcomes and call the other a
    // defect (`owned()` reads `1` only for `Unconfirmed`). What this test is about — the waiter released inside its bound,
    // the call never answered, the spawn never reported healthy — is asserted above and unchanged.
    match &outcome {
        SpawnResult::NotLaunched { .. } => assert_eq!(
            host.owned(),
            0,
            "a confirmed failed spawn still owns its child"
        ),
        SpawnResult::CleanupUnconfirmed { .. } => assert_eq!(
            host.owned(),
            1,
            "an unconfirmed cleanup must retain the child whose absence it could not confirm"
        ),
        // Named rather than a wildcard: `SpawnResult` has exactly three variants, so a `_` here
        // covers one — and would go on covering a fourth silently if one were ever added, which is
        // the whole of what `match_wildcard_for_single_variants` is about.
        other @ SpawnResult::Spawned(_) => {
            panic!("the unpublished pending child was reported as {other:?}")
        }
    }

    stop_server(&endpoint, &stop, &returned);
    shut_down(host);
}

/// A reported pid whose process cannot be opened resolves `Unresolvable`, never a
/// column that admits an operator credential by claiming the walk completed.
#[test]
fn a_refused_windows_process_read_is_unresolvable_and_never_a_completed_walk() {
    let host = RealHost::new();
    let resolver = Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    let peer = PeerIdentity {
        uid: None,
        gid: None,
        pid: PeerPid::Reported {
            pid: i32::MAX,
            pidversion: None,
        },
    };

    let answer = resolver.resolve(&peer);
    assert_eq!(
        answer,
        PeerResolution::Resolved(PeerProvenance::Unresolvable),
        "a platform read that refused is Unresolvable, and never an error"
    );
    assert_ne!(
        answer,
        PeerResolution::Resolved(PeerProvenance::NoRecordedAgent),
        "and never 'walked cleanly to no match', which the journal separates it from"
    );
    assert_ne!(
        answer,
        PeerResolution::Refuse,
        "a completed resolution is not a refusal: the producer list excludes it"
    );
}

/// On Windows: an attached PTY child registered with a duplicate of the PTY's job is
/// named by a walk, and once released the walk can no longer read its containment.
#[test]
fn an_attached_pty_childs_job_lets_the_walk_name_it() {
    let root = tempfile::tempdir().expect("temporary root");
    let mut ptys = dudes_pty::PtyHost::new();
    let opened = ptys
        .open(
            LaunchSpec {
                argv: vec![
                    OsString::from("cmd.exe"),
                    OsString::from("/c"),
                    OsString::from("ping -n 30 127.0.0.1 >NUL"),
                ],
                env: std::env::vars_os()
                    .map(|(name, value)| EnvVar::new(name, value))
                    .collect(),
                cwd: root.path().to_path_buf(),
                stdin: dudes_providers::launch::LaunchInput::Null,
            },
            dudes_pty::PtySize { cols: 80, rows: 24 },
        )
        .expect("the PTY opens");
    let pid = ptys.pid(opened.id).expect("the PTY runs a child");
    let job = ptys.job(opened.id).expect("a duplicate of the PTY's job");

    let processes = RealHost::new().peer_processes();
    let slot = processes.begin_spawn();
    let mut attached = processes.track_external(slot);
    let key = attached.register(pid, job).expect("the child's key");
    let named = processes.resolve(pid, std::slice::from_ref(&key));
    drop(attached);
    let released = processes.resolve(pid, std::slice::from_ref(&key));
    let closed = ptys.close(opened.id);

    assert!(closed.is_ok(), "the PTY closes: {closed:?}");
    assert_eq!(named, dudes_host::PeerRelation::Matched(0));
    assert_eq!(
        released,
        dudes_host::PeerRelation::Unresolvable,
        "without the job in the relation, containment cannot be read"
    );
}
