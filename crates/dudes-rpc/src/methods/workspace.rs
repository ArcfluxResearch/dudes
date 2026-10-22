//! `workspace.*` handlers. Operator-only: trusting a project is the operator's statement about
//! their own repository, and no agent or browser may make it.

use std::sync::Arc;

use dudes_proto::rpc::workspace::TrustParams;
use dudes_proto::rpc::{Method, RpcError};
use serde_json::Value;

use super::{Control, encode, params};
use crate::dispatch::Call;

/// `workspace.trust`: trust or stop trusting one enrolled project.
pub fn trust<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: TrustParams = params(&call)?;
        encode(Method::WorkspaceTrust, &control.workspace_trust(request)?)
    }
}
