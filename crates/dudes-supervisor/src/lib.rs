//! The sans-io machine of the Dudes supervisor.
//!
//! One entry point, [`Supervisor::handle`], reports checked durable progress; [`Supervisor::effects`]
//! and [`Supervisor::take_effects`] expose its effects. Handling performs no I/O, awaits nothing,
//! and reads no clock except through the [`Clock`](host::Clock) seam.
//! The runtime that executes the effects lives elsewhere. The machine joins operation-scoped
//! authority observations to confirmed journal state. The pure policy remains reproducible from
//! its journal-derived inputs.
//!
//! The invariant the crate exists to protect is **invariant 2, persist before spawn**: a
//! `Effect::Spawn` for turn *T* is emitted only in response to an `Input::Persisted` — for the
//! token that carried `Record::TurnIssued{T}`, or, when the agent's token for this run is not yet
//! on the disk, for the `Record::HookTokenMinted` that arms it. Browser-note turns
//! additionally wait for `BrowserNoteDispatchIntent`, with revocable preparation before it.
//! There is one
//! constructor of `Effect::Spawn` in this crate and it is on that path. The converse is an
//! observation, not a precondition: when the host reports `Input::Spawned` the machine emits
//! `Persist{TurnSpawned}` and nothing waits on it.
//!
//! # The shape
//!
//! - [`host`] — the seams: [`ProcessHost`](host::ProcessHost), [`Clock`](host::Clock),
//!   [`Rng`](host::Rng), the [`AdoptionKey`](host::AdoptionKey) and the
//!   [`liveness()`](host::liveness) table. Everything non-deterministic enters here.
//! - [`input`] — what reaches the machine, including [`Input::Replayed`] so replay feeds the same
//!   fold as the live path.
//! - [`effect`] — what the machine asks for. A `{effect:?}` prints environment **names** only,
//!   because every value is an [`EnvVar`](dudes_providers::launch::EnvVar) that redacts itself.
//!   The secret it protects is the agent token: every child this crate launches,
//!   managed or attached, carries `DUDES_SOCK` and `DUDES_TOKEN` in its environment and never in
//!   argv, and `tests/agent_token.rs` pins the whole chain.
//! - [`world`] — the in-memory fold of confirmed entries. A projection, rebuilt by replay; it
//!   holds **no spend arithmetic** — spend arrives on [`Input::Spend`], supplied by the caller —
//!   and no board, mail or memory state either, which arrive the same way on [`Input::Work`].
//! - [`agent_fsm`] — the six agent states and the total transition function.
//! - [`machine`] — [`Supervisor`] itself.
//! - [`RecoveryOperation`] borrows exact recovery-subject observations for one invocation and
//!   supplies no append permit. Neither the operation nor its observations enter [`World`].
//! - [`predicate_evidence_eligible`] — the rule for which evidence may complete a goal,
//!   applied by the machine to a live result and by [`world`] to a replayed one.
//! - [`doctor`] — what `system.doctor` asks the machine about adoption keys and session recovery.
//!   Derived from the host and journal-folded state, and read by nothing that decides.
//! - `testing` — `FakeHost`, `FakeClock`, `FakeRng` and the harness the tests drive. **Not in a
//!   default build**: it is behind the `testing` feature, for the reason `src/testing.rs`'s header
//!   owns. Unlinked here on purpose — an intra-doc link to a `cfg`-gated module resolves only when
//!   the feature is on, and a doc build that has to enable it to stay clean is the gate leaking.
//!
//! # Layering
//!
//! Invariant 9: this crate depends on no data-plane crate (`dudes-pty`, `dudes-proxy`), and on no
//! journal either — reading redb inside `handle()` would be I/O, and this machine performs none.
//! `cargo xtask layering` is the gate for all of it: the row forbids `dudes-pty`, `dudes-proxy`,
//! `dudes-journal`, `redb` and `tokio`, so "sans-io" is checked rather than asserted.

pub mod agent_fsm;
pub mod closing;
pub mod doctor;
pub mod effect;
pub mod host;
pub mod input;
mod launch_admission;
pub mod machine;
mod predicate_evidence;
mod recovery_operation;
// Off in a default build. `src/testing.rs`'s header owns the reason; `cfg(test)` is here because
// this crate's own unit tests reach the fakes through `crate::testing`, and a bare feature gate
// would not compile.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod token;
pub mod world;

pub use agent_fsm::FsmEvent;
pub use doctor::KeyHealth;
pub use effect::Effect;
pub use host::{
    AdoptionKey, Clock, HostBoot, KillOutcome, Liveness, MAX_OWNED_TARGET_QUERY_ROWS,
    NativeBoundaryAcknowledgement, NativeBoundaryReceipt, OwnedTargetEvidence, OwnedTargetState,
    OwnedTargets, OwnedTargetsStatus, ProbeAnswer, ProcessHost, Rng, SpawnResult, blake3_hex,
    closing_input_digest, input_digest, key_is_interpretable, liveness, session_id_from_ulid,
};
pub use input::{
    BrowserAnswerCommand, ClientId, Command, HookId, Input, PersistToken, RuntimeSignal, SpendView,
};
pub use launch_admission::SharedFolderRefusal;
pub use machine::{Config, InstructionSource, Supervisor};
pub use predicate_evidence::predicate_evidence_eligible;
pub use recovery_operation::{
    RecoveryAvailability, RecoveryOperation, RecoverySubjectKey, RecoverySubjectUse,
    with_recovery_operation,
};
pub use world::{MintedFor, WorktreeRow, WorktreeState, World};
