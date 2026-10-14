//! The seven startup steps.
//!
//! ```text
//! Locked → Opened+Replayed → Bound → Credentialed → Reaping → Daemon
//! ```
//!
//! # How much of the order the types carry, stated at the size of the evidence
//!
//! Three of the six links are **typed**, and a reorder across them does not compile:
//!
//! - **1 → 2.** [`open_and_replay`] takes the [`StateLock`] by value and gives it back, because
//!   opening a journal without holding the single-writer lock is precisely the bug the lock exists
//!   for. Step 2 cannot run before step 1 because it cannot be *called* before it.
//! - **4 → 5.** Step 5 reads the credential off the transport step 4 produced.
//! - **6 → 7.** The first decide is a method on the `Daemon` the reaper handle was moved into.
//!
//! **The other three are ordering by statement and nothing more**, and each was measured rather
//! than assumed:
//!
//! - **2 → 3.** Both happen inside [`open_and_replay`], in that order because they are written in
//!   it. Moving `dudes_journal::open` below the replay block — losing the torn-tail repair, which is
//!   step 2's whole stated reason — compiles clean. It is caught at *runtime*, because `open` also
//!   creates the directory, so many of the daemon's tests fail; but it is not caught by a type.
//! - **3 → 4.** `bind` needs only the settings, so a compiler cannot object to it happening earlier.
//! - **5 → 6.** `verify_credential` returns `()`; nothing consumes it, so moving it after the first
//!   decide compiles clean.
//!
//! Saying which is which is the difference between a guarantee and a wish. `startup.rs`'s header
//! does not restate this enumeration; it points here, so the claim lives in one place.
//!
//! # Why each step must come after the one before it
//!
//! - **Lock before anything.** Two writers over one journal each rotate to their own segment and
//!   each believe they are alone (invariant 1). Refusing here, before a file is opened, is also what
//!   makes the refusal *attributable*: a second daemon's recorder holds zero steps.
//! - **Journal open before replay.** `open()` is the only path that repairs a torn tail; a replay
//!   over an unrepaired directory would fold bytes no `append` ever returned for.
//! - **Replay before bind.** Binding first would serve a caller a projection that is behind the log.
//! - **Bind before the credential is read back.** They come from one call, deliberately — see
//!   [`bind`] — and the credential is the *product* of it that step 5 checks.
//! - **Credential before serving.** A daemon that binds without one serves an operator surface
//!   nothing can authenticate against.
//! - **Reaper before the first decide.** Deciding first would issue a turn beside a child nobody has
//!   probed.
//! - **First decide before the door opens.** `Input::Started` must arrive *before the first append*
//!   — `Reader::locate` catches a re-created current segment only while no append has happened since
//!   — so the accept loop starts after it, not with the bind. An operator call served earlier would
//!   append first and quietly break that.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_journal::{MemoryApply, MemoryProjection, Projections, Reader, replay};
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::{Method, system};
use dudes_rpc::auth::Requirement;
use dudes_rpc::methods::{
    register_agent_attach, register_agent_detach, register_agent_effects, register_all,
    register_memory_effect, register_pty_write,
};
use dudes_rpc::{
    CredentialCreationError, Dispatcher, Listener, LocalTransport, OperatorAuthoritySource,
    StopFlag, Subscribers, serve_with_authority,
};
use dudes_supervisor::Config;
use dudes_supervisor::{Clock, Input, ProcessHost, Rng, Supervisor};
use dudes_types::{Lsn, Provider};

use crate::lock::StateLock;
use crate::ports::{ExitCause, Handle, JournalReader, LoopControl};
use crate::predicate::PredicateRunner;
use crate::runtime::{Accept, AcceptPanic, ActivityFlusher, Ctx, Daemon, Reaper, Shared};
use crate::settings::{Seams, Settings};
use crate::socket;
#[cfg(unix)]
use crate::socket::{SocketIdentity, Standing};
use crate::startup::StartupStep;

/// The journal's directory inside the state directory.
pub const JOURNAL_DIR: &str = "journal";

/// The projection store's file inside the state directory.
pub const PROJECTION_FILE: &str = "state.redb";

/// Take the seven core steps plus the optional Floor steps and return a daemon ready to serve.
///
/// # Errors
///
/// Any step's own failure, named by the step it came from. The commonest is step 1's: another
/// daemon holds the single-writer lock on this state directory.
pub fn boot<H, P, C, R>(settings: &Settings, seams: Seams<H, P, C, R>) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_with_options(settings, seams, crate::settings::RunOptions::default())
}

/// Boot with additive startup options while preserving the legacy entry.
///
/// # Errors
///
/// The same named startup failures as [`boot`].
pub fn boot_with_options<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    options: crate::settings::RunOptions,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_with_options_and_floor_faults(
        settings,
        seams,
        options,
        None,
        crate::floor_stream::FloorStreamFault::default(),
    )
}

/// Boot with a release-unreachable Floor start controller for lifecycle tests.
#[doc(hidden)]
pub fn boot_with_floor_fault<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    floor_fault: Option<crate::floor_gateway::FloorStartFault>,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_with_floor_faults(
        settings,
        seams,
        floor_fault,
        crate::floor_stream::FloorStreamFault::default(),
    )
}

/// Boot with release-unreachable Floor start and stream controllers for integration tests.
#[doc(hidden)]
pub fn boot_with_floor_faults<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    floor_fault: Option<crate::floor_gateway::FloorStartFault>,
    stream_fault: crate::floor_stream::FloorStreamFault,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_with_options_and_floor_faults(
        settings,
        seams,
        crate::settings::RunOptions::default(),
        floor_fault,
        stream_fault,
    )
}

/// Boot an enabled run with owned release-unreachable Floor fault controls.
///
/// # Errors
///
/// The same named startup failures as [`boot`].
#[doc(hidden)]
pub fn boot_with_options_and_floor_faults<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    options: crate::settings::RunOptions,
    floor_fault: Option<crate::floor_gateway::FloorStartFault>,
    stream_fault: crate::floor_stream::FloorStreamFault,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_controlled(
        settings,
        seams,
        options,
        floor_fault,
        stream_fault,
        crate::BrowserLaunchPause::default(),
        memory_from_settings(settings)?,
    )
}

/// Boot with a readiness hold armed before startup replay begins.
///
/// # Errors
///
/// The same named startup failures as [`boot`].
#[doc(hidden)]
pub fn boot_with_preparation_pause<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    options: crate::settings::RunOptions,
    pause: crate::BrowserLaunchPause,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_controlled(
        settings,
        seams,
        options,
        None,
        crate::floor_stream::FloorStreamFault::default(),
        pause,
        memory_from_settings(settings)?,
    )
}

/// Boot with validated memory settings from the operator's same startup snapshot.
///
/// # Errors
/// The same ordered startup failures as [`boot`].
pub fn boot_with_memory_config<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    memory_config: dudes_journal::MemoryCondenseConfig,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_with_memory_config_and_options(
        settings,
        seams,
        memory_config,
        crate::settings::RunOptions::default(),
    )
}

/// Boot with validated memory settings and a Floor start controller for integration tests.
///
/// # Errors
/// The same ordered startup failures as [`boot`].
#[doc(hidden)]
pub fn boot_with_memory_config_and_floor_fault<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    memory_config: dudes_journal::MemoryCondenseConfig,
    floor_fault: crate::floor_gateway::FloorStartFault,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_controlled(
        settings,
        seams,
        crate::settings::RunOptions::default(),
        Some(floor_fault),
        crate::floor_stream::FloorStreamFault::default(),
        crate::BrowserLaunchPause::default(),
        memory_config,
    )
}

/// Boot with coherent memory settings and additive startup options.
///
/// # Errors
/// The same ordered startup failures as [`boot`].
pub fn boot_with_memory_config_and_options<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    memory_config: dudes_journal::MemoryCondenseConfig,
    options: crate::settings::RunOptions,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot_controlled(
        settings,
        seams,
        options,
        None,
        crate::floor_stream::FloorStreamFault::default(),
        crate::BrowserLaunchPause::default(),
        memory_config,
    )
}

fn memory_from_settings(settings: &Settings) -> Result<dudes_journal::MemoryCondenseConfig> {
    settings
        .operator_config
        .as_deref()
        .map(crate::config::load_operator_and_memory)
        .transpose()
        .map(|settings| {
            settings.map_or_else(
                dudes_journal::MemoryCondenseConfig::default,
                |(_, memory)| memory,
            )
        })
        .map_err(Into::into)
}

#[expect(
    clippy::too_many_lines,
    reason = "the ordered startup and optional Floor steps remain one chronology"
)]
fn boot_controlled<H, P, C, R>(
    settings: &Settings,
    seams: Seams<H, P, C, R>,
    options: crate::settings::RunOptions,
    floor_fault: Option<crate::floor_gateway::FloorStartFault>,
    stream_fault: crate::floor_stream::FloorStreamFault,
    preparation_pause: crate::BrowserLaunchPause,
    memory_config: dudes_journal::MemoryCondenseConfig,
) -> Result<Daemon<H, P, C, R>>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    let Seams {
        host,
        pty_host,
        predicates,
        clock,
        mut rng,
        resolver,
        adapters,
        children,
        witness,
    } = seams;

    // Once, before anything starts a child: every variable an `[mcp.servers]` declaration names
    // leaves the environment every child of this run starts from, and only an opted-in agent's
    // managed launch gets its servers' secrets back.
    let (daemon_env, secrets) =
        dudes_providers::mcp::split_secrets(&settings.daemon_env, options.mcp_servers());
    let mcp =
        dudes_providers::mcp::LoadedServers::new(options.mcp_servers(), &secrets, |command| {
            crate::program::on_path(
                std::ffi::OsStr::new(command),
                crate::program::child_path(&daemon_env),
            )
            .and_then(|path| path.into_os_string().into_string().ok())
        });
    let settings = &Settings {
        daemon_env,
        ..settings.clone()
    };

    // The bytes of the operator credential come out of the one randomness seam this daemon has
    // (the bytes come from the existing `Rng` seam, with no new trait), drawn here
    // because the seam is `&mut` and the machine takes it at step 3. **Drawing is not minting**:
    // what happens once per daemon run is the *file*, and that is step 4's call and step 5's check.
    let credential = operator_credential(&mut rng)?;
    let shutdown_run = rng.ulid().to_string();

    let (jobs_tx, jobs_rx) = mpsc::channel();
    let handle = Handle::new(jobs_tx);
    // As soon as there is a loop to reach, and before step 7 can spawn anything: a managed child's
    // watcher runs on its own thread and may report an end at any moment after the exec.
    children.arm(handle.clone());

    // --- 1 · the single-writer lock ------------------------------------------------------------
    let lock = StateLock::take(&settings.state_dir).with_context(|| {
        format!(
            "taking the single-writer lock on {}",
            settings.state_dir.display()
        )
    })?;
    witness.startup.record(StartupStep::Lock);

    // The registry cannot change after construction, so its list is read once here,
    // before it moves into the supervisor.
    let codex_profile = options
        .clone()
        .into_providers()
        .codex
        .as_ref()
        .filter(|config| config.streaming)
        .map(|_| {
            crate::codex_profile::CodexProfile::new(
                &settings.state_dir,
                &settings.daemon_env,
                Arc::clone(&predicates) as Arc<dyn PredicateRunner>,
            )
        });
    if let Some(profile) = &codex_profile {
        children.configure_codex_profile(Arc::clone(profile));
    }
    let provider_check = crate::provider_check::ProviderCheck::start(
        Provider::ALL
            .into_iter()
            .filter_map(|provider| {
                adapters
                    .get(provider)
                    .map(|adapter| crate::provider_check::Configured {
                        provider,
                        program: adapter.program().to_os_string(),
                    })
            })
            .collect(),
        &settings.daemon_env,
        settings.state_dir.clone(),
        codex_profile,
    )
    .context("starting the provider check thread")?;
    let sign_in = crate::sign_in::SignInCheck::new(
        Arc::clone(&predicates) as Arc<dyn PredicateRunner>,
        settings.state_dir.clone(),
        Provider::ALL.into_iter().filter_map(|provider| {
            adapters.get(provider).map(|adapter| crate::sign_in::Check {
                provider,
                args: adapter.sign_in_check(),
                env: adapter.child_env(&settings.daemon_env),
            })
        }),
        crate::sign_in::Timing::DAEMON,
    );

    let Replayed {
        lock,
        writer,
        projections,
        memory,
        mut sup,
        goal_index,
        journal_dir,
    } = open_and_replay(
        lock,
        settings,
        &witness,
        Shared::new(Arc::clone(&clock)),
        rng,
        adapters,
        mcp,
    )?;
    if let Some(missing) = crate::reserve::missing(
        sup.world(),
        settings.floor_usd_per_day,
        settings.per_turn_floor,
        crate::reserve::Candidate::None,
    ) {
        return Err(anyhow!(
            "a positive per-turn reserve is required: {missing}"
        ));
    }
    let onboarding_index =
        crate::floor_actions::OnboardingIndex::replay(&Reader::new(&journal_dir))
            .context("replaying browser onboarding attempts")?;
    let answer_index = crate::floor_answer::AnswerIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser answers")?;
    if options.chat_key_ignored() {
        tracing::warn!(
            "[floor.chat] in the operator file is ignored: Chat is always on and has no switch"
        );
    }
    let chat_index = crate::floor_chat::ChatIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser notes")?;
    let browser_launches = crate::chat_launch::LaunchPreparations::new(&handle, preparation_pause)
        .context("starting bounded browser launch preparation")?;
    let control_index = crate::floor_control::ControlIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser pauses and resumes")?;
    let rename_index = crate::floor_rename::DurableRenameIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser renames")?;
    let look_index = crate::floor_look::DurableLookIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser appearance edits")?;
    let retire_index = crate::floor_retire::RetireIndex::replay(&Reader::new(&journal_dir))
        .context("replaying browser retirements")?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mailbox =
        crate::mailbox::Mailbox::replay(settings.state_dir.clone(), &Reader::new(&journal_dir))
            .context("replaying mailbox items")?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let mailbox = crate::mailbox::Mailbox::unavailable();
    let mailbox_report = mailbox.report();
    // Publish the cursor replay established before the door can answer. Starting this at zero
    // would pair replay-derived supervisor health with an empty-journal ping until the next append.
    let last_lsn = Arc::new(AtomicU64::new(projections.last_applied_lsn().0));

    let prepared_floor = settings
        .floor
        .as_ref()
        .map(crate::floor_gateway::prepare)
        .transpose()?
        .map(|prepared| {
            prepared
                .with_answer(settings.floor_answer_enabled)
                .with_control(settings.floor_control)
                .with_terminal(settings.floor_terminal_enabled)
                .with_board(settings.floor_board_enabled)
                .with_remembered_port(crate::floor_port::remembered(&settings.state_dir))
        });
    if prepared_floor.is_some() {
        witness.startup.record(StartupStep::FloorPrepared);
    }

    // --- 4 · bind, and 5 · the operator credential ---------------------------------------------
    let (transport, identity) = bind_and_credential(settings, &credential, &witness)?;
    sup.set_agent_bin(crate::agent_bin::prepare(
        &settings.runtime_dir,
        settings.daemon_exe.as_deref(),
    ));
    let completion =
        crate::shutdown_receipt::Completion::create(&settings.runtime_dir, &shutdown_run)
            .context("creating shutdown completion receipt")?;

    let transport = Arc::new(transport);
    let socket = transport.path().to_path_buf();
    let stop = Arc::new(StopFlag::new());
    let subscribers = Arc::new(Subscribers::new());
    // Replay is the authority on whether this run ever had a pre-agent epoch. A retired agent is
    // still a recorded agent: acquisition never reopens after the first hire in the directory's
    // history.
    let operator_authority = authority_after_replay(!sup.world().agents.is_empty());
    // The third published value, beside the two above. Empty here and republished at the end of
    // every `pump` — including step 7's, which runs before the door opens, so the accept thread
    // never reads an empty one.
    let tokens = crate::classify::WorldTokens::new();

    // --- 6 · the reaper ------------------------------------------------------------------------
    let reaping = start_reaper(
        handle.clone(),
        settings.beat,
        identity,
        socket.clone(),
        witness.clone(),
    );
    witness.startup.record(StartupStep::ReaperStart);

    let memory_condenses = sup
        .world()
        .agents
        .keys()
        .copied()
        .filter(|agent| memory.condense_needed(*agent, memory_config))
        .collect();
    // A daemon-owned, permanently empty directory for the memory-condense model call to run in
    // — never the agent's own repository, so there is nothing
    // for the vendor CLI to auto-discover there: no `.claude/settings.json`, `.mcp.json`, hooks,
    // or `CLAUDE.md`. Created once, at boot, and never written to afterward.
    let condense_cwd = settings.state_dir.join("condense-model-cwd");
    open_condense_cwd(&condense_cwd)?;
    let mut closing = crate::closing::Waiting::new(sup.world());
    if let Some(session) = sup
        .world()
        .closing
        .as_ref()
        .filter(|session| !session.is_complete())
    {
        closing.observe(session);
    }
    let floor_closing = closing.floor_view(sup.world(), clock.now());
    let mut daemon = Daemon {
        closing,
        sup,
        ctx: Ctx {
            writer,
            projections,
            mailbox: Some(mailbox),
            floor_streams: None,
            enrollment: settings.enrollment.clone(),
            reloads: crate::enrollment_reload::ReloadState::new(&settings.enrollment),
            onboarding_index,
            answer_index,
            chat_index,
            running_mcp: options.mcp_servers().clone(),
            running_providers: options.into_providers(),
            browser_launches,
            resolved_browser_turn: None,
            answer_enabled: settings.floor_answer_enabled,
            hire_enabled: settings.floor_hire_enabled,
            control_index,
            control: settings.floor_control,
            goal_index,
            rename_index,
            look_index,
            retire_index,
            git_enabled: settings.floor_git_enabled,
            board_enabled: settings.floor_board_enabled,
            browser_grants: settings.floor_terminal_enabled.then(BTreeMap::new),
            #[cfg(feature = "testing")]
            terminal_skew: crate::floor_terminal::TerminalSkew::default(),
            closing_failed_after: None,
            ticket_reacted: None,
            floor_closing,
            pending_floor: BTreeMap::new(),
            pending_floor_base: None,
            browser_session: None,
            onboarding_pause: crate::runtime::OnboardingPause::default(),
            onboarding_failure: crate::runtime::OnboardingFailure::default(),
            memory,
            memory_config,
            memory_condenses,
            memory_integrity: crate::runtime::MemoryIntegrity::Healthy,
            memory_condense_pause: crate::runtime::MemoryCondensePause::default(),
            condense_model_enabled: settings.condense_model_enabled,
            condense_model: settings.condense_model.clone(),
            memory_condense_inflight: BTreeSet::new(),
            condense_cwd,
            pi: crate::runtime::PiAdmission::default(),
            host,
            pty_host: crate::classify::PtyBarrier::new(pty_host, resolver.clone()),
            live_ptys: BTreeMap::new(),
            cleanup_ptys: BTreeMap::new(),
            revoked_local_ptys: BTreeSet::new(),
            predicates,
            subscribers: Arc::clone(&subscribers),
            last_lsn: Arc::clone(&last_lsn),
            tokens: tokens.clone(),
            resolver: resolver.clone(),
            append_failure: crate::runtime::AppendFailure::default(),
            #[cfg(all(feature = "testing", any(target_os = "macos", target_os = "linux")))]
            mailbox_passes: Arc::default(),
            predicate_runs: BTreeMap::new(),
            baselines: crate::baseline::Baselines::default(),
            predicate_spawn_failure: crate::runtime::PredicateSpawnFailure::default(),
            accept_panic: AcceptPanic::default(),
            handle: handle.clone(),
            local_days: crate::local_day::LocalDays::system(Arc::clone(&clock)),
            clock: Arc::clone(&clock),
            predicate_env: settings.predicate_env.clone(),
            predicate_path: settings.predicate_path.clone(),
            children,
            confirmed_launch_failures: BTreeMap::new(),
            pending_attached_calls: BTreeMap::new(),
            attached_instruction_boundaries: BTreeMap::new(),
            pending_native_launches: BTreeMap::new(),
            native_acknowledgements: BTreeMap::new(),
            instruction_sampler: {
                let sample_clock = Arc::clone(&clock);
                crate::runtime::instruction_sampling::InstructionSampler::new(
                    handle.clone(),
                    Arc::new(move || sample_clock.now()),
                )?
            },
            pending_hook_replies: BTreeMap::new(),
        },
        jobs: jobs_rx,
        handle: handle.clone(),
        stop: Arc::clone(&stop),
        operator_authority: Arc::clone(&operator_authority),
        socket,
        #[cfg(unix)]
        socket_identity: identity,
        accept: None,
        floor: None,
        floor_heartbeat: None,
        floor_day_watch: None,
        transport: None,
        reaper: Some(reaping),
        activity_flusher: None,
        lock,
        witness: witness.clone(),
        provider_check: provider_check.clone(),
        exited: false,
        completion,
    };

    // --- 7 · the first decide ------------------------------------------------------------------
    // Before the door opens, and before any append: `Input::Started`'s own contract.
    daemon.started()?;
    witness.startup.record(StartupStep::FirstDecide);

    if let Some(prepared) = prepared_floor {
        let prepared = if settings.floor_git_enabled {
            prepared.with_git(Arc::new(git_reads(settings, &daemon)))
        } else {
            prepared
        };
        daemon.sup.enable_activity_sampling();
        daemon.activity_flusher = Some(start_activity_flusher(
            handle.clone(),
            Arc::clone(&daemon.ctx.children),
        ));
        if let Some(days) = stream_fault.local_days() {
            daemon.ctx.local_days = days;
        }
        daemon.ctx.floor_closing = daemon
            .closing
            .floor_view(daemon.sup.world(), daemon.ctx.clock.now());
        let initial = daemon
            .ctx
            .floor_snapshot()
            .context("building the post-started Floor projection")?;
        let streams = crate::floor_stream::FloorStreams::with_daemon_check(
            initial,
            stream_fault,
            daemon.ctx.children.output(),
            provider_check.clone(),
        )
        .context("serializing the post-started Floor projection")?;
        daemon.ctx.floor_streams = Some(streams.clone());
        daemon
            .ctx
            .publish_chat(&daemon.sup)
            .context("publishing startup browser note state")?;
        let floor = match floor_fault {
            Some(fault) => prepared.start_with_faults(
                handle.clone(),
                Arc::clone(&daemon.ctx.clock),
                streams,
                &fault,
            )?,
            None => prepared.start_with_streams(
                handle.clone(),
                Arc::clone(&daemon.ctx.clock),
                streams,
            )?,
        };
        if let Err(error) = crate::floor_port::remember(&settings.state_dir, floor.addr().port()) {
            tracing::warn!(%error, "the Floor port is not kept for the next run");
        }
        daemon.floor = Some(floor);
        // After the gateway, so a beat cannot precede the listener that carries it, and before
        // `FloorReady` is recorded, so the step means "beating" and not "about to".
        daemon.floor_heartbeat = Some(
            daemon
                .ctx
                .floor_streams
                .as_ref()
                .expect("the Floor stream registry was just installed")
                .start_heartbeat(crate::floor_stream::HEARTBEAT_PERIOD),
        );
        daemon.floor_day_watch = Some(
            daemon
                .ctx
                .floor_streams
                .as_ref()
                .expect("the Floor stream registry was just installed")
                .start_day_watch(
                    daemon.ctx.local_days.clone(),
                    crate::floor_stream::DAY_WATCH_PERIOD,
                ),
        );
        witness.startup.record(StartupStep::FloorReady);
    }

    let floor_carry = daemon
        .floor
        .as_ref()
        .map(|floor| floor.carry(daemon.ctx.baselines.check_runs()));
    open_the_door(
        &mut daemon,
        settings,
        Door {
            credential,
            resolver,
            tokens,
            handle,
            journal_dir,
            subscribers,
            last_lsn,
            transport,
            stop,
            operator_authority,
            floor_carry,
            provider_check,
            sign_in,
            mailbox_report,
        },
    );
    Ok(daemon)
}

/// Create the condense model call's own directory at mode 0700 from the start — `DirBuilder`
/// with a Unix mode applies it atomically at `mkdir`, unlike `create_dir_all` followed by a
/// separate `chmod`, which leaves a window where the directory exists at whatever the process
/// umask gives it. If the directory already exists, from an earlier boot, refuse
/// to start unless it is still exactly what this daemon would have created: a directory, owned
/// by this process's effective user, at mode 0700 — anything else could let another local user
/// read it or plant a file the condense model call would then run beside.
pub(crate) fn open_condense_cwd(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error).with_context(|| {
                    format!("creating the condense directory {}", path.display())
                });
            }
            Err(_) => {}
        }
        let metadata = std::fs::symlink_metadata(path).with_context(|| {
            format!(
                "checking the existing condense directory {}",
                path.display()
            )
        })?;
        let owner = nix::unistd::geteuid().as_raw();
        if !metadata.is_dir() {
            bail!("{} already exists and is not a directory", path.display());
        }
        if metadata.uid() != owner {
            bail!(
                "{} already exists but is not owned by this daemon's user",
                path.display()
            );
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            bail!(
                "{} already exists with permissions other than 0700",
                path.display()
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating the condense directory {}", path.display()))
    }
}

#[cfg(all(test, unix))]
mod condense_cwd_tests {
    use super::open_condense_cwd;

    #[test]
    fn a_fresh_path_is_created_at_mode_0700() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().expect("a scratch directory");
        let path = parent.path().join("condense-model-cwd");
        open_condense_cwd(&path).expect("create");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn an_existing_path_at_mode_0700_is_accepted() {
        use std::os::unix::fs::DirBuilderExt as _;

        let parent = tempfile::tempdir().expect("a scratch directory");
        let path = parent.path().join("condense-model-cwd");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("pre-create");
        open_condense_cwd(&path).expect("a matching existing directory must be accepted");
    }

    #[test]
    fn an_existing_path_at_the_wrong_mode_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().expect("a scratch directory");
        let path = parent.path().join("condense-model-cwd");
        std::fs::create_dir(&path).expect("pre-create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("loosen permissions");
        open_condense_cwd(&path)
            .expect_err("a directory at the wrong mode must refuse the daemon's own start");
    }

    #[test]
    fn an_existing_regular_file_is_refused() {
        let parent = tempfile::tempdir().expect("a scratch directory");
        let path = parent.path().join("condense-model-cwd");
        std::fs::write(&path, b"not a directory").expect("pre-create a file");
        open_condense_cwd(&path).expect_err("a file in place of the directory must be refused");
    }

    /// A symlink whose target matches every other check (a directory, owned by this process, at
    /// mode 0700) must still be refused. `symlink_metadata` is what makes the difference: it
    /// reports the link itself rather than following it, so `is_dir()` is false here even though
    /// the target it points at would pass. Swapping it for `metadata`, which follows the link,
    /// would read the target's attributes instead and let this through.
    #[test]
    fn an_existing_symlink_is_refused() {
        use std::os::unix::fs::DirBuilderExt as _;

        let parent = tempfile::tempdir().expect("a scratch directory");
        let real = parent.path().join("real-target");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .expect("pre-create the symlink's target");
        let path = parent.path().join("condense-model-cwd");
        std::os::unix::fs::symlink(&real, &path).expect("stand a symlink in for the directory");
        open_condense_cwd(&path).expect_err("a symlink in place of the directory must be refused");
    }
}

fn authority_after_replay(has_recorded_agent: bool) -> Arc<OperatorAuthoritySource> {
    Arc::new(OperatorAuthoritySource::from_replayed_agents(
        has_recorded_agent,
    ))
}

/// Everything the accept loop needs, bundled so [`open_the_door`] takes one argument rather than
/// nine.
struct Door {
    /// This run's operator credential, moved here rather than copied: the classifier compares
    /// against it, and [`bind`] has already written the file a client reads it from.
    credential: OperatorCredential,
    /// The peer-classification column, per connection. See [`Seams::resolver`].
    resolver: crate::classify::Resolver,
    /// The presented-token half of peer classification: the projection the loop publishes.
    tokens: crate::classify::WorldTokens,
    handle: Handle,
    journal_dir: std::path::PathBuf,
    subscribers: Arc<Subscribers>,
    last_lsn: Arc<AtomicU64>,
    transport: Arc<LocalTransport>,
    stop: Arc<StopFlag>,
    operator_authority: Arc<OperatorAuthoritySource>,
    floor_carry: Option<crate::floor_session::FloorCarry>,
    provider_check: crate::provider_check::ProviderCheck,
    sign_in: crate::sign_in::SignInCheck,
    mailbox_report: Arc<std::sync::Mutex<system::MailboxReport>>,
}

/// The Floor's Git read: the daemon's runner and `PATH`, a scratch directory under the runtime
/// directory that no agent can reach, and whatever a crashed run left there swept.
fn git_reads<H, P, C, R>(
    settings: &Settings,
    daemon: &Daemon<H, P, C, R>,
) -> crate::floor_git::GitReads
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    let reader = crate::git_read::GitReader::new(
        Arc::clone(&daemon.ctx.predicates) as Arc<dyn PredicateRunner>,
        settings.predicate_path.clone(),
        settings.runtime_dir.join("git-read"),
    );
    reader.sweep_scratch();
    let clock = Arc::clone(&daemon.ctx.clock);
    crate::floor_git::GitReads::new(Arc::new(reader), move || clock.now())
}

/// Build the classifier and the method table, and start the accept loop.
///
/// **After startup step 7, not with the bind.** `Input::Started` must arrive before the first
/// append, and an operator call served earlier would append first — so the socket is bound at step 4
/// and answered from only once the first decide has happened.
///
/// # This is where the classifier is built, because this is where its inputs exist
///
/// [`PeerClassifier`](dudes_rpc::PeerClassifier) needs the operator credential and a token
/// projection. The credential is minted inside [`boot`] at step 4 and cannot be handed in from
/// outside; the projection is the fold step 3 replayed, published by the loop that step 7 just
/// ran. Both are in scope here and nowhere earlier, so nothing has to be late-bound, optional, or
/// filled in after the door is already open — which is what a daemon with a window of "the door
/// answers and the classifier is not there yet" would have.
fn open_the_door<H, P, C, R>(daemon: &mut Daemon<H, P, C, R>, settings: &Settings, door: Door)
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    let classifier = dudes_rpc::PeerClassifier::new(door.credential, Arc::new(door.tokens));
    let git = crate::worktree::git::Git::new(
        Arc::clone(&daemon.ctx.predicates) as Arc<dyn PredicateRunner>,
        settings.predicate_path.clone(),
        settings.git_checkout_deadline,
    );
    let pending = daemon.pending_leases();
    let worktrees = Arc::new(crate::worktree::hire::WorktreeHires::new(
        daemon.handle(),
        git,
    ));
    let cleanup = Arc::clone(&worktrees);
    let _ = std::thread::Builder::new()
        .name("worktree-cleanup".to_owned())
        .spawn(move || cleanup.clean_up_after_a_crash(pending));
    let dispatcher = Arc::new(dispatch_with_floor(
        settings,
        classifier,
        crate::hooks::Ingress::new(door.handle, Arc::clone(&daemon.ctx.clock)),
        &door.journal_dir,
        &door.subscribers,
        door.last_lsn,
        Arc::clone(&door.operator_authority),
        door.floor_carry,
        daemon.ctx.onboarding_pause.clone(),
        door.provider_check,
        Some(door.sign_in),
        daemon.completion.result(),
        door.mailbox_report,
        Some(worktrees),
    ));
    // Wrapped here, rather than by whoever supplied it: the seam's job is to choose the peer-classification
    // column, and turning a resolver that can no longer answer into a daemon that stops and says so
    // is this function's — because this is where the loop that the stop has to reach exists.
    let resolver = crate::classify::GuardedResolver::wrap(Arc::new(door.resolver), daemon.handle());
    daemon.accept = Some(start_accept(
        AcceptDoor {
            inner: Arc::clone(&door.transport),
            panic: daemon.ctx.accept_panic.clone(),
        },
        resolver,
        dispatcher,
        door.stop,
        settings.max_connections,
        daemon.handle(),
        door.operator_authority,
    ));
    // The transport outlives the accept thread it was handed to, so the daemon keeps a share of it:
    // dropping the last `Arc` closes the listener, and a listener closed while the loop still runs
    // is a daemon with no door and no way to say so.
    daemon.keep_transport(door.transport);
}

/// `dudes-hook` as an attached CLI's settings may name it: an absolute UTF-8 path to an existing
/// regular file. Anything else is `None`, which refuses the attach, because Claude
/// Code treats a hook it cannot run as a non-blocking error and the CLI would run ungoverned.
fn usable_hook_command(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let usable = path.is_absolute()
        && path.to_str().is_some()
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file());
    if !usable {
        tracing::warn!(
            path = %path.display(),
            "the hook command is not an absolute UTF-8 path to a file, so attaching is refused"
        );
    }
    usable.then(|| path.to_path_buf())
}

fn operator_credential(rng: &mut impl Rng) -> Result<OperatorCredential> {
    OperatorCredential::new(rng.agent_token().expose())
        .map_err(|error| anyhow!("the operator credential was not well formed: {error}"))
}

/// What startup steps 2 and 3 produce.
struct Replayed<H, R>
where
    H: Clock,
    R: Rng,
{
    /// Handed in and handed back. The lock is not *used* by these two steps; requiring it is how
    /// "the journal is opened under the single-writer lock" becomes a thing the compiler checks
    /// rather than a thing the call order happens to do.
    lock: StateLock,
    writer: dudes_journal::Writer,
    projections: Projections,
    memory: MemoryProjection,
    sup: Supervisor<H, R>,
    goal_index: crate::goal_replacement::GoalChanges,
    journal_dir: std::path::PathBuf,
}

/// Startup steps 2 and 3: open the journal and the projection, then fold the log into both.
///
/// Split out of [`boot`] for its length alone; the order inside it is the normative one and the
/// chain still runs it between step 1 and step 4, because it takes what step 1 produced by having
/// been called after it and hands step 4 what it needs.
fn open_and_replay<C, R>(
    lock: StateLock,
    settings: &Settings,
    witness: &crate::startup::Witness,
    clock: Shared<C>,
    rng: R,
    adapters: dudes_providers::registry::Adapters,
    mcp: dudes_providers::mcp::LoadedServers,
) -> Result<Replayed<Shared<C>, R>>
where
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    // --- 2 · the journal -----------------------------------------------------------------------
    let journal_dir = settings.state_dir.join(JOURNAL_DIR);
    let (writer, report) =
        dudes_journal::open_with_scratch(&journal_dir, settings.journal, &settings.state_dir)
            .with_context(|| format!("opening the journal at {}", journal_dir.display()))?;
    if report.torn.is_some() {
        tracing::warn!(?report, "a torn tail was repaired at open");
    }
    let mut projections = Projections::open(&settings.state_dir.join(PROJECTION_FILE))
        .context("opening the projection store")?;
    witness.startup.record(StartupStep::JournalOpen);

    // --- 3 · replay ----------------------------------------------------------------------------
    let reader = writer.reader();
    let mut goal_index = crate::goal_replacement::GoalChanges::default();
    let mut sup = Supervisor::new(
        Config {
            data_dir: settings.state_dir.clone(),
            socket: settings.socket(),
            daemon_env: settings.daemon_env.clone(),
            floor_usd_per_day: settings.floor_usd_per_day,
            per_turn_floor: settings.per_turn_floor,
            breaker: settings.breaker,
            hook_command: settings
                .hook_command
                .as_deref()
                .and_then(usable_hook_command),
            agent_bin: None,
            mcp,
        },
        clock,
        rng,
    )
    .with_adapters(adapters);
    let mut memory = MemoryProjection::new(settings.state_dir.clone())
        .context("opening the applied-memory state root")?;
    // The same fold, from the same log, in the same order — `Input::Replayed` is what makes a
    // replayed decision equal to the live one (invariant 1). Nothing here is acted on: replay emits
    // no effects, and `Input::Started` is what runs the crash rules.
    let mut folded = 0usize;
    for item in reader.iter_from(Lsn::ZERO).context("reading the journal")? {
        let (lsn, entry) = item.context("reading the journal")?;
        goal_index
            .validate(sup.world(), &entry.record)
            .with_context(|| format!("invalid goal replacement at {lsn:?}"))?;
        goal_index.apply(&entry.record);
        sup.world()
            .validate_mcp_record(lsn, &entry.record)
            .with_context(|| format!("invalid MCP launch history at {lsn:?}"))?;
        match memory.apply_entry(lsn, &entry) {
            Ok(MemoryApply::Corrupt(reason)) => {
                tracing::error!(?lsn, ?reason, "memory record was excluded during replay");
            }
            Err(error) => {
                tracing::error!(?lsn, %error, "condense record was excluded during replay");
            }
            Ok(_) => {}
        }
        let effects = sup.handle(Input::Replayed { lsn, entry });
        debug_assert!(effects.is_empty(), "replay emits no effects");
        folded += 1;
    }
    let applied = replay(&reader, &mut projections).context("replaying into the projections")?;
    for (agent, view) in crate::work::compose(&projections, &memory, |agent| sup.work_input(agent))?
    {
        let effects = sup.handle(Input::Work { agent, view });
        debug_assert!(
            effects.is_empty(),
            "work projection refresh emits no effects"
        );
    }
    tracing::info!(projected = applied.applied, folded, "replay complete");
    witness.startup.record(StartupStep::Replay);

    Ok(Replayed {
        lock,
        writer,
        projections,
        memory,
        sup,
        goal_index,
        journal_dir,
    })
}

/// Startup steps 4 and 5, which one call produces and a second checks.
///
/// Split from [`boot`] for its length. The two steps stay two — each records its own
/// [`StartupStep`], and step 5 fails the boot when step 4 minted nothing — because the order is
/// normative and a reader has to be able to see both happen.
fn bind_and_credential(
    settings: &Settings,
    credential: &OperatorCredential,
    witness: &crate::startup::Witness,
) -> Result<(LocalTransport, Identity)> {
    let (transport, identity) = bind(settings, credential)?;
    witness.startup.record(StartupStep::Bind);

    let credential_path = transport
        .credential_path()
        .ok_or_else(|| {
            anyhow!(
                "the transport bound without minting an operator credential, so this daemon would \
                 serve an operator surface nothing can authenticate against"
            )
        })?
        .to_path_buf();
    verify_credential(&credential_path)?;
    witness.startup.record(StartupStep::MintCredential);
    Ok((transport, identity))
}

/// Startup step 4: reclaim a dead run's files, then bind the socket and write the credential.
///
/// **One call does both**, and that is deliberate rather than a shortcut: the socket and
/// the credential share one directory-establishing call. Unix gives the mode to `mkdir(2)` and
/// reads it back; Windows creates the directory with an inheritable creator-only DACL and reads it
/// back before this call. Putting the credential anywhere else would add a second path-resolution
/// site to a surface whose first one has only just been hardened.
///
/// So step 5 is not a second write; it is the check that this call did the first one. Swap
/// `bind_in_with_credential` for `bind_in` and the directory is still established, the socket is
/// still bound — and `credential_path()` answers `None`, which is what makes step 5 go red for the
/// mint being gone rather than for the directory being missing.
fn bind(
    settings: &Settings,
    credential: &OperatorCredential,
) -> Result<(LocalTransport, Identity)> {
    #[cfg(windows)]
    dudes_win::create_runtime_dir_creator_only(&settings.runtime_dir).with_context(|| {
        format!(
            "creating the runtime directory {}",
            settings.runtime_dir.display()
        )
    })?;
    socket::reclaim_stale(&settings.runtime_dir, &settings.socket_name).with_context(|| {
        format!(
            "clearing the runtime directory {}",
            settings.runtime_dir.display()
        )
    })?;
    let transport = match LocalTransport::bind_in_with_credential_and_halt(
        &settings.runtime_dir,
        &settings.socket_name,
        credential,
        dudes_host::disconnect_named_pipe,
    ) {
        Ok(transport) => transport,
        // A credential failure names its own path, and the socket's context would misattribute it.
        Err(error)
            if error.get_ref().is_some_and(
                <dyn std::error::Error + Send + Sync>::is::<CredentialCreationError>,
            ) =>
        {
            return Err(error.into());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("binding {}", settings.socket().display()));
        }
    };
    let identity = read_identity(transport.path());
    Ok((transport, identity))
}

/// The socket identity captured at bind, where the platform has one.
#[cfg(unix)]
type Identity = Option<SocketIdentity>;
/// Windows answers on a named pipe, which has no path a peer can squat at.
#[cfg(not(unix))]
struct Identity;

/// Read `(st_dev, st_ino)` immediately after the bind.
///
/// After the bind and therefore after `verify_bound_socket`, which is the step that establishes a
/// real socket is what stands at the path. Capturing before it would remember whatever an attacker
/// had pointed a link at.
#[cfg(unix)]
fn read_identity(path: &std::path::Path) -> Identity {
    match SocketIdentity::of(path) {
        Ok(identity) => Some(identity),
        // Not fatal: the daemon is bound and serving either way, and losing the squat check is
        // losing a detection rather than a guarantee. Loud, because a daemon that cannot read its
        // own socket has something else wrong with it.
        Err(error) => {
            tracing::error!(%error, socket = %path.display(), "could not read the socket's identity");
            None
        }
    }
}

/// No identity to read where there is no socket file.
#[cfg(not(unix))]
fn read_identity(_path: &std::path::Path) -> Identity {
    Identity
}

/// Startup step 5's check: the credential file is there, owner-only, and well formed.
fn verify_credential(path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the operator credential at {}", path.display()))?;
    if text.len() != OPERATOR_CREDENTIAL_LEN {
        bail!(
            "the operator credential at {} is {} characters, not {OPERATOR_CREDENTIAL_LEN}",
            path.display(),
            text.len()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != dudes_rpc::operator_credential_permissions() {
            bail!(
                "the operator credential at {} is mode {mode:04o}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    if !dudes_win::is_creator_only(path)? {
        bail!(
            "the operator credential at {} is not restricted to the creating user",
            path.display()
        );
    }
    Ok(())
}

/// Startup step 6: the beat, and the socket-identity re-comparison that rides on it.
fn start_reaper(
    handle: Handle,
    beat: Duration,
    identity: Identity,
    socket: std::path::PathBuf,
    witness: crate::startup::Witness,
) -> Reaper {
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("dudes-reaper".to_owned())
        .spawn(move || {
            loop {
                // `recv_timeout` is the sleep *and* the wake: a beat of thirty seconds must not
                // become thirty seconds of shutdown latency.
                match stopped.recv_timeout(beat) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    // Told to stop, or the daemon is gone.
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                check_socket(&identity, &socket, &witness);
                handle.input(Input::Tick);
            }
        })
        .expect("the reaper thread");
    Reaper { stop, thread }
}

fn start_activity_flusher(
    handle: Handle,
    children: Arc<crate::children::ChildEnds>,
) -> ActivityFlusher {
    children.sample_activity();
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("dudes-activity-sampler".to_owned())
        .spawn(move || {
            loop {
                match stopped.recv_timeout(Duration::from_secs(1)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                children.flush_activities();
                handle.input(Input::ActivityFlush);
            }
        })
        .expect("the activity sampler thread");
    ActivityFlusher { stop, thread }
}

/// Compare the socket's identity against the one captured at bind, and say so loudly if it moved.
///
/// **Raises no breaker rung**, because the requirement is permissive (a squatted socket may be reported, nothing more). A rung is per *agent* and is
/// raised by an `Input::Signal`; a squatted control socket is a daemon-wide fact with no agent to
/// attach it to, and no `RuntimeSignal` or `BreakerSignal` variant names it — `BreakerSignal` being
/// a wire type, adding one would change the published schemas. Invariant 3 is what bounds
/// what this may cause: an `Inferred` event may never be the sole cause of `Stop` or `Done`, and
/// nothing here produces an `Input` at all.
#[cfg(unix)]
fn check_socket(identity: &Identity, socket: &std::path::Path, witness: &crate::startup::Witness) {
    let Some(identity) = identity.as_ref() else {
        // Nothing was captured at bind, so there is nothing to compare and nothing to record — and
        // that emptiness is what a test reads to catch a capture that stopped happening.
        return;
    };
    let standing = identity.standing(socket);
    // Recorded before it is logged: the log is the operator's channel and this is the test's, and a
    // detection with no readable consequence is a clause satisfied by a `tracing` call.
    witness.sockets.record(standing);
    match standing {
        Standing::Same => {}
        Standing::Replaced => tracing::error!(
            socket = %socket.display(),
            "the control socket has been replaced: another process unlinked this daemon's socket \
             and bound its own at the same path, so new clients are reaching it and not this daemon"
        ),
        Standing::Gone => tracing::error!(
            socket = %socket.display(),
            "the control socket has been unlinked: no new client can reach this daemon"
        ),
        Standing::Unreadable => tracing::warn!(
            socket = %socket.display(),
            "the control socket's path could not be read"
        ),
    }
}

/// Nothing to compare where there is no socket file.
#[cfg(not(unix))]
#[allow(clippy::missing_const_for_fn)]
fn check_socket(
    _identity: &Identity,
    _socket: &std::path::Path,
    _witness: &crate::startup::Witness,
) {
}

/// `floor.session.export` and `floor.session.import`, which `dudes upgrade` uses to keep Floor
/// sign-ins across its restart.
///
/// Both are `Operator` only, like `floor.session.issue`: an import chooses the digests it admits,
/// so a caller that is not the operator could admit a cookie of its own. A daemon that serves no
/// Floor exports nothing and imports nothing.
fn register_floor_carry(
    dispatcher: &mut Dispatcher,
    carry: Option<crate::floor_session::FloorCarry>,
    run: Option<String>,
) {
    use dudes_proto::rpc::floor::{SessionExport, SessionImportParams, SessionImportResult};
    use dudes_proto::rpc::{CurrentMethod, Empty};

    let exporting = carry.clone();
    let exporting_run = run.clone();
    dispatcher.register_current_audited(
        CurrentMethod::FloorSessionExport,
        Requirement::Operator,
        move |call, pid| {
            let _: Empty = dudes_rpc::methods::params(&call)?;
            let exported = exporting.as_ref().map_or_else(
                || SessionExport {
                    run: String::new(),
                    origin: String::new(),
                    sessions: Vec::new(),
                },
                |carry| carry.export(exporting_run.as_deref()),
            );
            tracing::info!(
                ?pid,
                sessions = exported.sessions.len(),
                "floor sign-ins exported"
            );
            dudes_rpc::methods::encode_current(CurrentMethod::FloorSessionExport, &exported)
        },
    );
    dispatcher.register_current_audited(
        CurrentMethod::FloorSessionImport,
        Requirement::Operator,
        move |call, pid| {
            let params: SessionImportParams = dudes_rpc::methods::params(&call)?;
            let restored = carry
                .as_ref()
                .map_or(0, |carry| carry.import(run.as_deref(), &params.exported));
            tracing::info!(?pid, restored, "floor sign-ins imported");
            dudes_rpc::methods::encode_current(
                CurrentMethod::FloorSessionImport,
                &SessionImportResult { restored },
            )
        },
    );
}

/// The method table: `register_all`'s nine ordinary rows, the three conditioned rows,
/// the four board rows, `hook.event`, plus `floor.session.issue`, `floor.enrollment.reload`,
/// `agent.list`, `system.ping`, `system.doctor` and `system.shutdown`.
///
/// This is the handler for `system.shutdown`. It is registered `Requirement::Operator` like every
/// other state-changing method, and what an operator credential *resolves* to is the classifier's business and not this function's.
///
/// # Why this is `pub`
///
/// It is the daemon's **only** production dispatcher — the one `Dispatcher::new` in `crates/` and
/// `bins/` that is not inside a test target or a `#[cfg(test)]` module — so the set of methods it
/// registers *is* the routed surface. `tests/routed_methods.rs` folds it to hold every declared
/// method to a route or to a named exemption, and a function it could only reach by copying would
/// be a copy that drifts. Nothing is published from here: `duded` is `publish = false`, so this
/// widens no public library surface.
pub fn dispatch<C>(
    settings: &Settings,
    classifier: dudes_rpc::PeerClassifier,
    hooks: crate::hooks::Ingress<C>,
    journal_dir: &std::path::Path,
    subscribers: &Arc<Subscribers>,
    last_lsn: Arc<AtomicU64>,
    operator_authority: Arc<OperatorAuthoritySource>,
) -> Dispatcher
where
    C: Clock + Send + Sync + 'static,
{
    dispatch_with_floor(
        settings,
        classifier,
        hooks,
        journal_dir,
        subscribers,
        last_lsn,
        operator_authority,
        None,
        crate::runtime::OnboardingPause::default(),
        crate::provider_check::ProviderCheck::start(
            Vec::new(),
            &settings.daemon_env,
            settings.state_dir.clone(),
            None,
        )
        .expect("the provider check thread starts"),
        None,
        system::ShutdownResult { completion: None },
        Arc::new(std::sync::Mutex::new(system::MailboxReport::default())),
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the private extension preserves the existing production-dispatch test seam"
)]
fn dispatch_with_floor<C>(
    settings: &Settings,
    classifier: dudes_rpc::PeerClassifier,
    hooks: crate::hooks::Ingress<C>,
    journal_dir: &std::path::Path,
    subscribers: &Arc<Subscribers>,
    last_lsn: Arc<AtomicU64>,
    operator_authority: Arc<OperatorAuthoritySource>,
    floor_carry: Option<crate::floor_session::FloorCarry>,
    reload_pause: crate::runtime::OnboardingPause,
    provider_check: crate::provider_check::ProviderCheck,
    sign_in: Option<crate::sign_in::SignInCheck>,
    completion: system::ShutdownResult,
    mailbox_report: Arc<std::sync::Mutex<system::MailboxReport>>,
    worktrees: Option<Arc<crate::worktree::hire::WorktreeHires>>,
) -> Dispatcher
where
    C: Clock + Send + Sync + 'static,
{
    let mut dispatcher = Dispatcher::new(classifier);
    let handle = hooks.handle().clone();
    let control = LoopControl::new(handle.clone(), operator_authority);
    let control = Arc::new(match worktrees {
        Some(worktrees) => control.with_worktrees(worktrees),
        None => control,
    });
    let journal = Arc::new(JournalReader::new(journal_dir.to_path_buf()));
    register_all(&mut dispatcher, &control, &journal, subscribers);
    crate::board::register(&mut dispatcher, &handle);
    crate::ticket::register(&mut dispatcher, &handle);
    register_agent_effects(&mut dispatcher, &control);
    register_agent_attach(&mut dispatcher, &control);
    register_agent_detach(&mut dispatcher, &control);
    register_pty_write(&mut dispatcher, &control);
    register_memory_effect(&mut dispatcher, &control);
    dudes_rpc::methods::register_memory_read(&mut dispatcher, &control);
    dudes_rpc::methods::register_memory_search(&mut dispatcher, &control);
    dudes_rpc::methods::register_team_roster(&mut dispatcher, &control);
    crate::hooks::register(&mut dispatcher, hooks, settings.max_connections);
    let run = completion
        .completion
        .as_ref()
        .map(|receipt| receipt.run.clone());
    let issue_carry = floor_carry.clone();
    register_floor_carry(&mut dispatcher, floor_carry, run);
    dispatcher.register(
        Method::FloorSessionIssue,
        // The local operator mints only a read-only web bootstrap.
        Requirement::Operator,
        move |call| {
            let _: dudes_proto::rpc::Empty = dudes_rpc::methods::params(&call)?;
            let sessions = issue_carry
                .as_ref()
                .map(crate::floor_session::FloorCarry::sessions)
                .ok_or_else(|| dudes_proto::rpc::RpcError {
                    code: dudes_proto::rpc::ErrorCode::InternalError.code(),
                    message: "the Floor is not configured: install the built Floor as a \
                              `floor` directory beside duded (`cargo xtask dist` lays one out) \
                              or as `share/dudes/floor` beside duded's `bin` directory \
                              (install.sh puts one there) and restart the daemon; \
                              DUDES_FLOOR_ASSETS names another directory, \
                              but only for a duded started directly, because `dudes quickstart` \
                              does not pass it on"
                        .to_owned(),
                    data: None,
                })?;
            let result = sessions.issue().map_err(|_| dudes_proto::rpc::RpcError {
                code: dudes_proto::rpc::ErrorCode::InternalError.code(),
                message: "the Floor session is unavailable".to_owned(),
                data: None,
            })?;
            dudes_rpc::methods::encode(Method::FloorSessionIssue, &result)
        },
    );
    let reloader = crate::enrollment_reload::Reloader::new(
        settings.operator_config.clone(),
        handle.clone(),
        reload_pause,
    );
    dispatcher.register_audited(
        Method::FloorEnrollmentReload,
        // Only the operator makes the daemon adopt its own file.
        Requirement::Operator,
        move |call, pid| reloader.call(&call, pid),
    );

    let socket = settings.socket();
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let ping_state = Arc::new(move |lsn| system::PingResult {
        version: version.clone(),
        socket: socket.clone(),
        lsn,
    });
    let ping = Arc::clone(&ping_state);
    dispatcher.register(
        Method::SystemPing,
        // The one method a peer that has proved nothing may call, which is
        // what keeps a daemon probeable by a client carrying no credential at all.
        Requirement::AnyRole,
        dudes_rpc::system::ping(move || {
            ping(Lsn(last_lsn.load(std::sync::atomic::Ordering::Relaxed)))
        }),
    );
    register_agent_list(&mut dispatcher, handle.clone());
    let doctor_ping = Arc::clone(&ping_state);
    register_doctor(
        &mut dispatcher,
        handle.clone(),
        move |lsn| doctor_ping(lsn),
        provider_check,
        sign_in,
        mailbox_report,
    );
    register_closing(&mut dispatcher, handle.clone());
    dispatcher.register_after_response(
        Method::SystemShutdown,
        Requirement::Operator,
        move |_call| dudes_rpc::methods::encode(Method::SystemShutdown, &completion),
        move || {
            handle.stop(ExitCause::ShutdownRpc);
        },
    );
    dispatcher
}

fn register_closing(dispatcher: &mut Dispatcher, handle: Handle) {
    use dudes_proto::rpc::closing::ClosingParams;
    use dudes_rpc::auth::PeerRole;
    let after = handle.clone();
    dispatcher.register_after_response(
        Method::SystemClosingTime,
        Requirement::OperatorOrAgent,
        move |call| {
            let params = call
                .params
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if params.to_string().len() > 1_024 {
                return Err(dudes_rpc::dispatch::error(
                    dudes_proto::rpc::ErrorCode::InvalidParams,
                    "closing parameters exceed their byte bound",
                ));
            }
            let legacy = params.as_object().is_some_and(serde_json::Map::is_empty);
            let request = if legacy {
                if !matches!(call.role, PeerRole::Operator) {
                    return Err(dudes_rpc::dispatch::error(
                        dudes_proto::rpc::ErrorCode::Unauthorized,
                        "only the operator may announce closing",
                    ));
                }
                crate::closing::Request::Announce
            } else {
                let params: ClosingParams = serde_json::from_value(params).map_err(|_| {
                    dudes_rpc::dispatch::error(
                        dudes_proto::rpc::ErrorCode::InvalidParams,
                        "invalid closing parameters",
                    )
                })?;
                match (call.role, params) {
                    (PeerRole::Operator, ClosingParams::Announce) => {
                        crate::closing::Request::Announce
                    }
                    (PeerRole::Operator, ClosingParams::Status { lsn, after }) => {
                        crate::closing::Request::Status {
                            announcement: lsn,
                            after,
                        }
                    }
                    (PeerRole::Agent { agent }, ClosingParams::Ack { lsn, .. }) => {
                        crate::closing::Request::Ack {
                            announcement: lsn,
                            agent: *agent,
                        }
                    }
                    _ => {
                        return Err(dudes_rpc::dispatch::error(
                            dudes_proto::rpc::ErrorCode::Unauthorized,
                            "the closing action is not admitted for this role",
                        ));
                    }
                }
            };
            match handle.closing_call(request)? {
                crate::closing::Answer::Announced(_) if legacy => dudes_rpc::methods::encode(
                    Method::SystemClosingTime,
                    &dudes_proto::rpc::Empty {},
                ),
                crate::closing::Answer::Announced(value) => {
                    dudes_rpc::methods::encode(Method::SystemClosingTime, &value)
                }
                crate::closing::Answer::Status(value) => {
                    dudes_rpc::methods::encode(Method::SystemClosingTime, &value)
                }
                crate::closing::Answer::Ack => dudes_rpc::methods::encode(
                    Method::SystemClosingTime,
                    &dudes_proto::rpc::Empty {},
                ),
            }
        },
        move || after.closing_after_response(),
    );
}

fn register_doctor(
    dispatcher: &mut Dispatcher,
    doctor: Handle,
    ping: impl Fn(Lsn) -> system::PingResult + Send + Sync + 'static,
    provider_check: crate::provider_check::ProviderCheck,
    sign_in: Option<crate::sign_in::SignInCheck>,
    mailbox_report: Arc<std::sync::Mutex<system::MailboxReport>>,
) {
    dispatcher.register(Method::SystemDoctor, Requirement::Operator, move |_call| {
        let snapshot = doctor.doctor()?;
        let mut providers = provider_check.probes();
        if let Some(sign_in) = &sign_in {
            sign_in.annotate(&mut providers, &snapshot.busy);
        }
        let mut result = dudes_rpc::methods::encode(
            Method::SystemDoctor,
            &system::DoctorJournalResult {
                journal_stopped: snapshot.journal_stopped,
                doctor: system::DoctorResult {
                    ping: ping(snapshot.lsn),
                    boot: Some(snapshot.health.boot),
                    providers,
                    agents: snapshot.agents,
                    open_turns: snapshot.health.turns,
                    body_integrity: Vec::new(),
                },
            },
        )?;
        let report = mailbox_report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        result["mailbox"] = dudes_rpc::methods::encode(Method::SystemDoctor, &report)?;
        Ok(result)
    });
}

/// `agent.list`, read from the loop's projection in writer order.
fn register_agent_list(dispatcher: &mut Dispatcher, handle: Handle) {
    dispatcher.register(
        Method::AgentList,
        // The roster is operator reconnaissance.
        Requirement::Operator,
        move |call| {
            let _: dudes_proto::rpc::Empty = dudes_rpc::methods::params(&call)?;
            match handle.ask(crate::ports::Ask::AgentList)? {
                crate::ports::Answered::Agents(agents) => {
                    dudes_rpc::methods::encode(Method::AgentList, &agents)
                }
                _ => Err(dudes_proto::rpc::RpcError {
                    code: dudes_proto::rpc::ErrorCode::InternalError.code(),
                    message: "agent.list returned the wrong result".to_owned(),
                    data: None,
                }),
            }
        },
    );
}

/// The listener the accept thread actually asks, and the one seam that can make it panic.
///
/// A newtype rather than a generic parameter: `start_accept` has exactly one caller and one
/// listener, so nothing is bought by making the function generic and a concrete type keeps
/// [`Daemon::keep_transport`](crate::runtime::Daemon)'s retained share the same `Arc<LocalTransport>`
/// it always was.
struct AcceptDoor {
    inner: Arc<LocalTransport>,
    panic: AcceptPanic,
}

impl Listener for AcceptDoor {
    type Conn = <LocalTransport as Listener>::Conn;

    /// The transport's accept, and then the seam.
    ///
    /// **After the inner accept, not before**, so the panic is raised with a real connection
    /// accepted and the loop at exactly the point `serve` would next use it — the frame
    /// [`AcceptPanic`] exists to reach. Unarmed this is one relaxed load per connection.
    fn accept(&self) -> std::io::Result<(Self::Conn, dudes_rpc::auth::PeerIdentity)> {
        let accepted = self.inner.accept()?;
        assert!(
            !self.panic.take(),
            "the accept loop was armed to panic on this accept"
        );
        Ok(accepted)
    }
}

/// Start the accept loop, and tell the daemon to stop if it ever ends unbidden.
fn start_accept(
    transport: AcceptDoor,
    resolver: Arc<dyn dudes_rpc::auth::PeerResolver>,
    dispatcher: Arc<Dispatcher>,
    stop: Arc<StopFlag>,
    max_connections: std::num::NonZeroUsize,
    handle: Handle,
    operator_authority: Arc<OperatorAuthoritySource>,
) -> Accept {
    let (ended_tx, ended) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("dudes-accept".to_owned())
        .spawn(move || {
            // `catch_unwind` because this frame is the *only* one that can act on a panic out
            // of `serve`. The alternative site is `Daemon::exit`'s `accept.thread.join()`, which
            // returns the payload — and which runs only after the loop has already been told to
            // stop, so it can report a panic and can never cause the exit that a panic makes
            // nobody ask for. `serve` itself cannot decide either: it does not know what an
            // embedder wants a broken loop to mean, and says so.
            //
            // `AssertUnwindSafe`, narrowly: after the catch this closure logs a constant, posts a
            // `Job::Stop` and sends on a channel. It reads none of the state `serve` was mutating,
            // and the one thing it does read — `stop.is_stopped()` — is behind a lock that
            // recovers from poisoning. Nothing derived from a broken invariant leaves this frame.
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                serve_with_authority(
                    &transport,
                    &*resolver,
                    &dispatcher,
                    &operator_authority,
                    &stop,
                    max_connections,
                )
            }));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "the accept loop failed"),
                Err(_payload) => {
                    // A constant, and the payload is not lost by dropping it: the panic hook runs
                    // at the panic site, before the unwind, so the message and any backtrace are
                    // already on stderr whether or not this resumes.
                    tracing::error!(
                        "the accept loop panicked; the door is gone, so the daemon is stopping \
                         and saying so"
                    );
                    handle.stop(ExitCause::AcceptLoopPanicked);
                }
            }
            // **Not `resume_unwind`, and that is the difference from `classify::GuardedResolver`.**
            // The guard re-raises because `resolve` has no failure value and a fabricated
            // `PeerProvenance` is invariant 8 breached; this closure returns `()` and has nothing to
            // fabricate. Falling through keeps the channel below meaning one thing — a send says
            // the accept loop has ended, however it ended — and leaves `Daemon::exit`'s
            // `Disconnected` arm reachable only by a panic raised below this point, which is what
            // that arm now says.
            if !stop.is_stopped() {
                // The door is gone and nobody asked for it to be. A daemon that cannot be reached
                // cannot be told to stop either, so it stops itself — and this is the path
                // `ExitCause::Ended` names.
                //
                // **After a caught panic this fires too, and the named cause still wins.**
                // `Handle::stop` only sends a `Job::Stop` (`ports.rs:262-264`); the flag this reads
                // is set by the exit sequence at `runtime::Daemon::exit`'s step 4
                // (`runtime.rs:993`), which has not run yet. So the catch above sends
                // `AcceptLoopPanicked` and this sends `Ended` right behind it. `Daemon::serve`
                // returns on the **first** `Job::Stop` it takes (`runtime.rs:411`) and `drain`
                // discards every later one (`runtime.rs:922`, `:959`) — and both sends go through
                // the one `Sender` this closure owns, so the order they were written in is the
                // order the loop reads them. Nothing is gained by suppressing the second send and
                // a condition is gained by trying.
                handle.stop(ExitCause::Ended);
            }
            // Told last, so a receiver that sees this knows the accept loop has ended — by
            // returning, or by a panic this thread caught and named.
            let _ = ended_tx.send(());
        })
        .expect("the accept thread");
    Accept { ended, thread }
}
