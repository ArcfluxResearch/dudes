//! `loop.pause`, `loop.resume` and `loop.halt`.
//!
//! Named `loop_control` because `loop` is a keyword.

use std::sync::Arc;

use dudes_proto::rpc::{AgentRef, Empty, Method, RpcError};
use serde_json::Value;

use crate::dispatch::Call;
use crate::methods::{Control, encode, params};

/// `loop.pause`: stop issuing turns for an agent. Hooks and mail keep working.
pub fn pause<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: AgentRef = params(&call)?;
        control.pause(request.agent)?;
        encode(Method::LoopPause, &Empty {})
    }
}

/// `loop.resume`: lift a pause.
pub fn resume<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: AgentRef = params(&call)?;
        control.resume(request.agent)?;
        encode(Method::LoopResume, &Empty {})
    }
}

/// `loop.halt`: stop the agent; a running turn ends at its next boundary.
pub fn halt<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: AgentRef = params(&call)?;
        control.halt(request.agent)?;
        encode(Method::LoopHalt, &Empty {})
    }
}
