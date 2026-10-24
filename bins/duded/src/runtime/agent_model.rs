use super::{
    Clock, Command, Daemon, ModelOutcome, ModelRefusal, ModelRequest, PredicateRunner, ProcessHost,
    PumpFailure, Rng, error,
};
use dudes_proto::Actor;
use dudes_proto::record::TicketRequestAttribution;
use dudes_proto::rpc::agent::ModelParams;
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_types::{AgentId, ModelName};

/// Why a model change was refused. The checks run in this order, and the first that fails names
/// the refusal. A malformed name never gets here: it is refused where the request is parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChangeRefusal {
    /// No such agent, or it is retired.
    AgentAbsent,
    /// A person holds the agent's terminal, and an attached agent has no selection to carry a model.
    Attached,
    /// The engine does not continue a session under a different model.
    EngineUnsupported,
    /// The engine's configuration pins a model.
    ModelFixed,
    /// The override is no longer the one the Floor page showed.
    StaleModel,
}

impl ChangeRefusal {
    pub(super) const fn reason(self) -> &'static str {
        match self {
            Self::AgentAbsent => "agent_absent",
            Self::Attached => "agent_attached",
            Self::EngineUnsupported => "model_unsupported",
            Self::ModelFixed => "model_fixed",
            Self::StaleModel => "stale_model",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::AgentAbsent => "no such live agent",
            Self::Attached => {
                "a person holds this agent's terminal; its model can change once it is released"
            }
            Self::EngineUnsupported => {
                "this engine cannot continue a session under a different model, so the model \
                 cannot change; hire a new agent to use another"
            }
            Self::ModelFixed => {
                "the configuration sets this engine's model in its extra arguments, so a change \
                 cannot choose another; remove that setting to choose"
            }
            Self::StaleModel => "the agent's model changed since it was read",
        }
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// The one place a model change is judged, for the command line and the Floor alike.
    /// `expected` is what the Floor page saw as the override; the command line sends none.
    fn model_change_refusal(
        &self,
        agent: AgentId,
        model: Option<&ModelName>,
        expected: Option<&Option<ModelName>>,
    ) -> Option<ChangeRefusal> {
        let Some(row) = self
            .sup
            .world()
            .agents
            .get(&agent)
            .filter(|row| !row.retired)
        else {
            return Some(ChangeRefusal::AgentAbsent);
        };
        if row.attached {
            return Some(ChangeRefusal::Attached);
        }
        if !self
            .sup
            .adapters()
            .get(row.provider)
            .is_some_and(dudes_providers::adapter::Adapter::resumes_under_changed_model)
        {
            return Some(ChangeRefusal::EngineUnsupported);
        }
        if model.is_some() && self.model_fixed_by_config(row.provider) {
            return Some(ChangeRefusal::ModelFixed);
        }
        expected
            .is_some_and(|expected| *expected != row.model)
            .then_some(ChangeRefusal::StaleModel)
    }

    fn model_held(&self, agent: AgentId, model: Option<&ModelName>) -> bool {
        self.sup
            .world()
            .agents
            .get(&agent)
            .is_some_and(|row| row.model.as_ref() == model)
    }

    pub(super) fn agent_model(&mut self, params: ModelParams, by: Actor) -> Result<(), RpcError> {
        if let Some(refusal) = self.model_change_refusal(params.agent, params.model.as_ref(), None)
        {
            let code = match refusal {
                ChangeRefusal::AgentAbsent => ErrorCode::NotFound,
                _ => ErrorCode::Refused,
            };
            return Err(error(code, refusal.message()));
        }
        if self.model_held(params.agent, params.model.as_ref()) {
            return Ok(());
        }
        self.pump_or_refuse(super::Input::Command(Command::ChangeModel {
            agent: params.agent,
            model: params.model,
            by,
            request: None,
        }))
    }

    pub(super) fn browser_model(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &ModelRequest,
    ) -> Result<ModelOutcome, ModelRefusal> {
        let changed = |model: Option<ModelName>| ModelOutcome::Changed {
            request_id: request.request_wire.clone(),
            agent: request.agent,
            model,
        };
        if let Some(indexed) = self.ctx.model_index.get(request.request_id_digest) {
            return if indexed.request_digest == request.request_digest {
                Ok(changed(indexed.model.clone()))
            } else {
                Err(ModelRefusal::Conflict)
            };
        }
        if self.ctx.journal_writes_stopped() {
            return Err(ModelRefusal::UnavailableNoEffect);
        }
        if let Some(refusal) = self.model_change_refusal(
            request.agent,
            request.model.as_ref(),
            Some(&request.expected),
        ) {
            return Ok(ModelOutcome::Refused {
                request_id: request.request_wire.clone(),
                reason: refusal.reason(),
            });
        }
        if self.model_held(request.agent, request.model.as_ref()) {
            self.ctx.model_index.remember_held(
                request.request_id_digest,
                crate::floor_model::IndexedModel {
                    request_digest: request.request_digest,
                    agent: request.agent,
                    model: request.model.clone(),
                },
            );
            return Ok(changed(request.model.clone()));
        }
        let pumped = self.pump_browser(
            session,
            Command::ChangeModel {
                agent: request.agent,
                model: request.model.clone(),
                by: Actor::BrowserSession {
                    session_digest: session.digest(),
                },
                request: Some(TicketRequestAttribution {
                    request_id_digest: request.request_id_digest,
                    request_digest: request.request_digest,
                }),
            },
        );
        if let Some(indexed) = self.ctx.model_index.get(request.request_id_digest)
            && indexed.request_digest == request.request_digest
        {
            return Ok(changed(indexed.model.clone()));
        }
        Err(match pumped {
            Err(PumpFailure::Forbidden) => ModelRefusal::Forbidden,
            Err(PumpFailure::NoEffect) => ModelRefusal::UnavailableNoEffect,
            Ok(()) | Err(PumpFailure::Uncertain) => ModelRefusal::Unavailable,
        })
    }
}
