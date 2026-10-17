//! Operator recovery commands and inert historical reads.

use std::sync::Arc;

use dudes_proto::rpc::recovery::{
    RecoveryInspectParams, RecoveryInspectResult, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoverySubmitParams, RecoverySubmitResult,
};
use dudes_proto::rpc::{CurrentMethod, RpcError};

use crate::Dispatcher;
use crate::dispatch::OperatorAdmission;
use crate::recovery::RecoveryParams;

/// The daemon's recovery boundary for an already admitted operator call.
pub trait RecoveryControl: Send + Sync + 'static {
    /// Inspect without changing the journal or settling work.
    ///
    /// # Errors
    ///
    /// A transport or daemon availability error, without storage details.
    fn inspect_recovery(
        &self,
        params: RecoveryInspectParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryInspectResult, RpcError>;

    /// Submit one explicit intent; a dropped response does not authorize resubmission.
    ///
    /// # Errors
    ///
    /// A transport or daemon availability error, without storage details.
    fn submit_recovery(
        &self,
        params: &RecoverySubmitParams,
        admission: OperatorAdmission,
    ) -> Result<RecoverySubmitResult, RpcError>;

    /// Read a certified historical receipt using only its request identity.
    ///
    /// # Errors
    ///
    /// A transport or daemon availability error, without storage details.
    fn recovery_receipt(
        &self,
        params: RecoveryReceiptParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryReceiptResult, RpcError>;
}

/// Register the three strict recovery routes with fixed Operator admission.
pub fn register_recovery<C: RecoveryControl>(dispatcher: &mut Dispatcher, control: &Arc<C>) {
    for method in [
        CurrentMethod::RecoveryInspect,
        CurrentMethod::RecoverySubmit,
        CurrentMethod::RecoveryReceipt,
    ] {
        let control = Arc::clone(control);
        dispatcher.register_recovery(method, move |params, admission| match params {
            RecoveryParams::Inspect(params) => {
                super::encode_current(method, &control.inspect_recovery(params, admission)?)
            }
            RecoveryParams::Submit(params) => super::encode_current(
                method,
                &control.submit_recovery(params.as_ref(), admission)?,
            ),
            RecoveryParams::Receipt(params) => {
                super::encode_current(method, &control.recovery_receipt(params, admission)?)
            }
        });
    }
}
