//! an adapter's declared session acquisition mode, not the turn's session variant
//! alone, decides what a managed launch names.

use std::ffi::OsString;
use std::path::Path;

use dudes_providers::adapter::{
    Adapter, AgentCtx, Arrival, Isolation, ManagedInstall, SessionAcquisition, TurnSession,
    UsageSource,
};
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{LaunchError, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_types::{AgentId, ModelSelection, Provider, SessionId, Tier};

const ASSIGNED: SessionAcquisition = SessionAcquisition::Assigned {
    resume: "--session",
    arrival: Arrival::Start,
};
const PINNED: SessionAcquisition = SessionAcquisition::Pinned {
    pin: "--session-id",
    resume: "--resume",
};

const MINTED: &str = "0190f6d2-6b1a-7c3e-9f00-000000000001";
const CONFIRMED: &str = "vendor-named-session";
const TASK: &str = "make the tests pass";

struct Fake(SessionAcquisition);

impl Adapter for Fake {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::ObservedOnly
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        self.0
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError> {
        let mut argv = vec![OsString::from("fake")];
        argv.extend(self.session_acquisition().argv(session)?);
        argv.push(task.into());
        Ok(LaunchSpec {
            argv,
            env: Vec::new(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::None
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

fn launch(mode: SessionAcquisition, session: &TurnSession) -> Result<Vec<OsString>, LaunchError> {
    let ctx = AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid"),
        cwd: Path::new("/repo"),
        agent_dir: Path::new("/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        daemon_env: &[],
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    };
    Fake(mode)
        .managed(&ctx, TASK, session, &ModelSelection::provider_default())
        .map(|spec| spec.argv)
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

#[test]
fn an_assigning_adapter_refuses_a_minted_session() {
    assert_eq!(
        launch(ASSIGNED, &TurnSession::First(SessionId::from(MINTED))),
        Err(LaunchError::SessionModeMismatch)
    );
}

#[test]
fn an_assigning_adapter_names_no_session_on_a_turn_that_announces_none() {
    assert_eq!(
        launch(ASSIGNED, &TurnSession::Assigned),
        Ok(argv(&["fake", TASK]))
    );
}

#[test]
fn an_assigning_adapter_resumes_the_session_its_vendor_named() {
    assert_eq!(
        launch(ASSIGNED, &TurnSession::Resume(SessionId::from(CONFIRMED))),
        Ok(argv(&["fake", "--session", CONFIRMED, TASK]))
    );
}

#[test]
fn a_pinning_adapter_renders_the_minted_session() {
    assert_eq!(
        launch(PINNED, &TurnSession::First(SessionId::from(MINTED))),
        Ok(argv(&["fake", "--session-id", MINTED, TASK]))
    );
}

#[test]
fn a_pinning_adapter_refuses_a_turn_that_announces_no_session() {
    assert_eq!(
        launch(PINNED, &TurnSession::Assigned),
        Err(LaunchError::SessionModeMismatch)
    );
}

#[test]
fn a_turn_session_has_an_id_only_when_it_names_one() {
    assert_eq!(
        TurnSession::First(SessionId::from(MINTED)).id(),
        Some(&SessionId::from(MINTED))
    );
    assert_eq!(
        TurnSession::Resume(SessionId::from(CONFIRMED)).id(),
        Some(&SessionId::from(CONFIRMED))
    );
    assert_eq!(TurnSession::Assigned.id(), None);
}
