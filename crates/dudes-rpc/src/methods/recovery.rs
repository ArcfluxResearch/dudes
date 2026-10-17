//! Operator recovery commands and inert historical reads.

use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::recovery::{
    RecoveryInspectParams, RecoveryInspectResult, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoverySubmitParams, RecoverySubmitResult,
};

use crate::dispatch::OperatorAdmission;

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
        params: RecoverySubmitParams,
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
