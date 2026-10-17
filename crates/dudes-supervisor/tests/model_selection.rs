//! Model selection: a managed turn records the model it was launched under, and a
//! re-issue after a crash launches under the model the **journal** recorded rather than under
//! whatever the adapter is configured with by then.
//!
//! # The two mutants this file exists to catch
//!
//! 1. **The supervisor asks the adapter again at re-issue.** `reissue_turn` calls `launch`, and
//!    `launch` fetches the current adapter; before the model was recorded that is all there was, so a
//!    configuration change between the fsynced `TurnIssued` and the restart silently changed the
//!    model of a turn whose record never moved. The subject below changes exactly that
//!    configuration between the two runs.
//!
//! 2. **The adapter re-reads its own configured model instead of the selection it was handed**.
//!    The fake adapter here encodes the *given* selection into both the argv and
//!    the install file and keeps its configured model separate, so a `managed` that consulted
//!    `self` rather than the argument is visible in the launch the harness executes.
//!
//! # The control, and why the file is worthless without it
//!
//! The subject asserts that a **recorded** selection wins over current configuration. On its own
//! that is satisfied by a machine which ignores configuration entirely — including a machine that
//! hard-codes one model. The control is the legacy arm: a journal whose `TurnIssued` carries **no**
//! selection re-issues under the adapter's *current* configuration, so the two arms differ in
//! exactly the thing under test. This pair is the failing check: if the two cannot
//! be made to differ, the selection rule has to be rethought rather than patched.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dudes_proto::goal::Predicate;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{AgentBudget, Entry, Goal, GoalBudget, Record};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::LaunchSpec;
use dudes_providers::stream::TurnReader;
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_supervisor::{Command, Input};
use dudes_types::{
    AgentId, GoalId, Lsn, Mode, ModelSelection, Provider, SessionId, Tier, Timestamp, TurnId, Usd,
};

/// The model the adapter is configured with, shared so a test can change it **between** the two
/// daemon runs a `Harness::restart` models. A real operator edits a config file and restarts; this
/// is the same event with the file left out.
type Configured = Arc<Mutex<Option<String>>>;

/// An adapter that encodes the selection it is **given** into everything it composes, and reads
/// its own configuration only where the trait says it may.
#[derive(Clone)]
struct Selecting {
    configured: Configured,
}

/// How a selection appears in a launch: the selector, or this word for the provider default. The
/// two are different strings on purpose — an encoding that mapped both to "absent" would hide the
/// distinction model selection exists to keep.
fn encode(selection: &ModelSelection) -> String {
    selection
        .selector()
        .unwrap_or("(provider-default)")
        .to_owned()
}

impl Adapter for Selecting {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    /// The one place configuration is read. Everything else takes what it is handed.
    fn model_selection(&self) -> ModelSelection {
        ModelSelection {
            selector: self.configured.lock().expect("no test panics here").clone(),
        }
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        _session: &TurnSession,
        selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        Ok(LaunchSpec {
            argv: vec![OsString::from("true"), OsString::from(encode(selection))],
            env: Vec::new(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn managed_install(&self, ctx: &AgentCtx<'_>, selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall {
            files: vec![(ctx.agent_dir.join("model"), encode(selection))],
            ..ManagedInstall::default()
        }
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::None
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// No variable bills a turn on this stand-in (the empty answer).
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// A harness whose floor runs on [`Selecting`], sharing one configuration cell across the restart.
fn harness(configured: &Configured) -> Harness {
    let cell = Arc::clone(configured);
    Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || {
            Box::new(Selecting {
                configured: Arc::clone(&cell),
            })
        },
    )
}

fn configured(model: Option<&str>) -> Configured {
    Arc::new(Mutex::new(model.map(ToOwned::to_owned)))
}

/// What the launch for `turn` was composed under, read out of both halves of the `Spawn`: the argv
/// the child would have run and the file that would have been written beside it.
///
/// Both, not either. The two are composed by different trait methods from the same value, and a
/// change that threaded the selection into one of them would pass an assertion about the other.
fn launched_under(harness: &Harness, turn: TurnId) -> Vec<(String, String)> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn {
                turn: t,
                spec,
                install,
                ..
            } if *t == turn => {
                let argv = spec.argv[1].to_string_lossy().into_owned();
                let file = install.files[0].1.clone();
                Some((argv, file))
            }
            _ => None,
        })
        .collect()
}

/// The selection every `TurnIssued` in the trace recorded.
fn recorded(harness: &Harness) -> Vec<Option<ModelSelection>> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::TurnIssued {
                model_selection, ..
            } => Some(model_selection.clone()),
            _ => None,
        })
        .collect()
}

/// A live issue journals the selection **before** the spawn, and the same
/// value reaches both the launch spec and the managed install.
#[test]
fn a_live_issue_records_the_selection_before_the_spawn_and_launches_under_it() {
    let cell = configured(Some("model-alpha"));
    let mut h = harness(&cell);
    let (agent, _) = h.hire_with_goal(&["npm", "test"]);
    let turn = h.open_turn(agent).expect("a turn was issued");

    assert_eq!(
        recorded(&h),
        vec![Some(ModelSelection::named("model-alpha"))],
        "the record carries the configured selector"
    );
    assert_eq!(
        launched_under(&h, turn),
        vec![("model-alpha".to_owned(), "model-alpha".to_owned())],
        "and the same value reached the argv and the install, not one of the two"
    );

    // Invariant 2's ordering, restated for this field: the record carrying the selection is
    // emitted before the launch that uses it, so a crash between them replays a turn whose model
    // is known.
    let issued = h
        .trace
        .iter()
        .position(|seen| matches!(seen.record(), Some(Record::TurnIssued { .. })))
        .expect("a TurnIssued was persisted");
    let spawned = h
        .trace
        .iter()
        .position(|seen| matches!(seen, Seen::Spawn { turn: t, .. } if *t == turn))
        .expect("a Spawn was emitted");
    assert!(
        issued < spawned,
        "the selection is journaled before it is acted on (invariant 2)"
    );
}

/// The provider default is a **recorded choice**, not an absence: an adapter configured with no
/// model still writes `Some`.
///
/// This is the arm that makes the three-state shape load-bearing. Written as a bare
/// `Option<String>` the record here would be `None` — indistinguishable from a journal written before the field —
/// and the re-issue control below would then apply to it wrongly.
#[test]
fn an_unconfigured_model_records_the_provider_default_rather_than_nothing() {
    let cell = configured(None);
    let mut h = harness(&cell);
    let (agent, _) = h.hire_with_goal(&["npm", "test"]);
    let turn = h.open_turn(agent).expect("a turn was issued");

    assert_eq!(
        recorded(&h),
        vec![Some(ModelSelection::provider_default())],
        "every new managed issue records Some, including the provider default"
    );
    assert_eq!(
        launched_under(&h, turn),
        vec![(
            "(provider-default)".to_owned(),
            "(provider-default)".to_owned()
        )],
    );
}

/// **The subject.** An un-spawned turn re-issued after the adapter's
/// configuration changed launches under the model its `TurnIssued` recorded.
#[test]
fn a_reissue_uses_the_recorded_selection_not_the_new_configuration() {
    let cell = configured(Some("model-alpha"));
    let mut h = harness(&cell);
    // The crash window: the record reaches the disk, the process never starts.
    h.launch = false;
    let (agent, _) = h.hire_with_goal(&["npm", "test"]);
    let turn = h.open_turn(agent).expect("a turn was issued");
    assert_eq!(
        recorded(&h),
        vec![Some(ModelSelection::named("model-alpha"))]
    );

    // Between the two runs the operator changes the configured model.
    *cell.lock().unwrap() = Some("model-beta".to_owned());
    h.trace.clear();
    h.launch = true;
    let recovery_from = h.trace.len();
    h.restart();
    assert!(
        h.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    h.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        h.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = h.supervisor.recovery_opportunity(agent).unwrap().expected;
    let handled = h.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(h.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(
        launched_under(&h, turn),
        vec![("model-alpha".to_owned(), "model-alpha".to_owned())],
        "the re-issue launches under the recorded selection, not the new configuration"
    );
    assert!(
        recorded(&h).is_empty(),
        "and writes no second TurnIssued: the first is already fsynced (invariant 2)"
    );
}

#[test]
fn a_legacy_issue_without_a_selection_reissues_under_current_configuration() {
    let cell = configured(Some("model-beta"));
    let mut h = harness(&cell);
    let (entries, turn) = legacy_journal();
    h.journal = entries;
    let agent = h
        .journal
        .iter()
        .find_map(|entry| match entry.record {
            Record::AgentHired { id, .. } => Some(id),
            _ => None,
        })
        .unwrap();
    let recovery_from = h.trace.len();
    h.restart();
    assert!(
        h.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    h.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        h.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = h.supervisor.recovery_opportunity(agent).unwrap().expected;
    let handled = h.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(h.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(
        launched_under(&h, turn),
        vec![("model-beta".to_owned(), "model-beta".to_owned())],
        "a journal that recorded no selection cannot reconstruct one, so the adapter's current \
         configuration is used"
    );
}

/// A journal holding one agent, one goal and one turn issued without a `model_selection` — the
/// shape a build before the model was recorded wrote. Built by hand because this machine cannot produce it.
fn legacy_journal() -> (Vec<Entry>, TurnId) {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid");
    let goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("a ulid");
    let turn = TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("a ulid");
    let at = |step: i64| Timestamp::from_unix_millis(1_787_394_600_000 + step);
    let entries = vec![
        Entry {
            at: at(0),
            record: Record::AgentHired {
                character: None,
                accent: None,
                id: agent,
                name: "Hector".to_owned(),
                role: "infra".to_owned(),
                department: "infra".to_owned(),
                provider: Provider::Claude,
                cwd: PathBuf::from("/repo"),
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
            },
        },
        Entry {
            at: at(1),
            record: Record::GoalSet {
                agent,
                goal: Goal {
                    completion_check: None,
                    id: goal,
                    text: "make the tests pass".to_owned(),
                    predicate: Predicate::Command {
                        argv: vec!["npm".to_owned(), "test".to_owned()],
                        timeout_s: 600,
                    },
                    budget: GoalBudget {
                        usd: Some(Usd::from_dollars(5)),
                        tokens: None,
                    },
                    hop_cap: 8,
                    baseline: None,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        },
        Entry {
            at: at(2),
            record: Record::TurnIssued {
                agent,
                turn,
                goal,
                task: "make the tests pass".to_owned(),
                session: Some(SessionId::new("11111111-1111-4111-8111-111111111111")),
                mode: Mode::Managed,
                // The field under test: absent, as a journal from before model selection has it.
                model_selection: None,
                lsn_of_decision: Lsn(1),
                answers: Vec::new(),
            },
        },
    ];
    (entries, turn)
}

/// The run-local `World` keeps the one fact a re-issue has to replay — the turn's recorded
/// selection — and deliberately not the usage map, which nothing this crate decides ever reads.
/// The persistent projection is the one that carries *both* facts, and it is
/// `dudes-journal`'s replay property that asserts it. Asserted here rather than there because this
/// is where a real machine produces the records the fold reads.
#[test]
fn the_world_keeps_the_selection_a_reissue_needs() {
    let cell = configured(Some("model-alpha"));
    let mut h = harness(&cell);
    h.launch = false;
    let (agent, _) = h.hire_with_goal(&["npm", "test"]);
    let turn = h.open_turn(agent).expect("a turn was issued");

    assert_eq!(
        h.supervisor.world().turns[&turn].model_selection,
        Some(ModelSelection::named("model-alpha")),
        "the run-local world keeps the selection, which is what a re-issue reads"
    );
    // The control: a turn whose journal recorded nothing keeps nothing, so the field above is a
    // fact the fold copied rather than a value the row is always given.
    let (entries, legacy) = legacy_journal();
    let mut bare = harness(&cell);
    bare.journal = entries;
    bare.launch = false;
    bare.restart();
    assert_eq!(
        bare.supervisor.world().turns[&legacy].model_selection,
        None,
        "a legacy row keeps no selection"
    );
}
