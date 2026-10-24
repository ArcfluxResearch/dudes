//! Registration of `agent.model`, which is operator-only and records who asked.

use dudes_proto::rpc::agent::ModelParams;
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_rpc::Dispatcher;
use dudes_rpc::auth::Requirement;
use dudes_rpc::dispatch::error;
use dudes_rpc::methods::{encode, params};

use crate::board::actor;
use crate::ports::{Ask, Handle};

pub(crate) fn register(dispatcher: &mut Dispatcher, handle: &Handle) {
    let handle = handle.clone();
    dispatcher.register_audited(
        Method::AgentModel,
        Requirement::Operator,
        move |call, pid| {
            let params: ModelParams = params(&call)?;
            let by = actor(call.role, pid)?;
            match handle.ask(Ask::Model { params, by })? {
                crate::ports::Answered::Done => {
                    encode(Method::AgentModel, &dudes_proto::rpc::Empty {})
                }
                _ => Err(error(
                    ErrorCode::InternalError,
                    "agent.model returned the wrong result",
                )),
            }
        },
    );
}
