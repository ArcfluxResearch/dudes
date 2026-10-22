use core::str::FromStr;

use schemars::JsonSchema;

use super::floor::{SessionExport, SessionImportParams, SessionImportResult};
use super::recovery::{
    RecoveryInspectParams, RecoveryInspectResult, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoverySubmitParams, RecoverySubmitResult,
};
use super::ticket::{TicketCheckoutParams, TicketCheckoutResult};
use super::{Empty, Method, MethodShape, Methods, UnknownName};

/// Method names understood by the current protocol.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CurrentMethod {
    /// A method from the original method table.
    Legacy(Method),
    /// Read the current recorded checkout for one ticket.
    TicketCheckout,
    /// List the live Floor sign-ins so `dudes upgrade` can carry them across its restart.
    FloorSessionExport,
    /// Admit the sign-ins an earlier run exported.
    FloorSessionImport,
    /// Inspect one current recovery opportunity.
    RecoveryInspect,
    /// Submit one deliberately reviewed recovery choice.
    RecoverySubmit,
    /// Read the certified receipt for a retained request identity.
    RecoveryReceipt,
}

impl CurrentMethod {
    /// The JSON-RPC method name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Legacy(method) => method.name(),
            Self::TicketCheckout => "ticket.checkout",
            Self::FloorSessionExport => "floor.session.export",
            Self::FloorSessionImport => "floor.session.import",
            Self::RecoveryInspect => "recovery.inspect",
            Self::RecoverySubmit => "recovery.submit",
            Self::RecoveryReceipt => "recovery.receipt",
        }
    }

    /// The original method, when this entry belongs to that table.
    #[must_use]
    pub const fn legacy(self) -> Option<Method> {
        match self {
            Self::Legacy(method) => Some(method),
            Self::TicketCheckout
            | Self::FloorSessionExport
            | Self::FloorSessionImport
            | Self::RecoveryInspect
            | Self::RecoverySubmit
            | Self::RecoveryReceipt => None,
        }
    }
}

impl From<Method> for CurrentMethod {
    fn from(method: Method) -> Self {
        Self::Legacy(method)
    }
}

impl FromStr for CurrentMethod {
    type Err = UnknownName;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::TicketCheckout,
            Self::FloorSessionExport,
            Self::FloorSessionImport,
            Self::RecoveryInspect,
            Self::RecoverySubmit,
            Self::RecoveryReceipt,
        ]
        .into_iter()
        .find(|method| method.name() == text)
        .map_or_else(|| text.parse().map(Self::Legacy), Ok)
    }
}

impl core::fmt::Display for CurrentMethod {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// Current method params and results, for schema export.
#[non_exhaustive]
#[derive(JsonSchema)]
pub struct CurrentMethods {
    /// The original method table.
    #[serde(flatten)]
    pub legacy: Methods,
    /// The operator's recorded-checkout read.
    #[serde(rename = "ticket.checkout")]
    pub ticket_checkout: MethodShape<TicketCheckoutParams, TicketCheckoutResult>,
    /// The operator's read of the live Floor sign-ins.
    #[serde(rename = "floor.session.export")]
    pub floor_session_export: MethodShape<Empty, SessionExport>,
    /// The operator's admission of sign-ins an earlier run exported.
    #[serde(rename = "floor.session.import")]
    pub floor_session_import: MethodShape<SessionImportParams, SessionImportResult>,
    /// The operator's inert recovery inspection.
    #[serde(rename = "recovery.inspect")]
    pub recovery_inspect: MethodShape<RecoveryInspectParams, RecoveryInspectResult>,
    /// The operator's deliberate recovery submission.
    #[serde(rename = "recovery.submit")]
    pub recovery_submit: MethodShape<RecoverySubmitParams, RecoverySubmitResult>,
    /// The operator's inert historical receipt read.
    #[serde(rename = "recovery.receipt")]
    pub recovery_receipt: MethodShape<RecoveryReceiptParams, RecoveryReceiptResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_names_parse_and_print_as_themselves() {
        for method in [
            CurrentMethod::TicketCheckout,
            CurrentMethod::FloorSessionExport,
            CurrentMethod::FloorSessionImport,
            CurrentMethod::RecoveryInspect,
            CurrentMethod::RecoverySubmit,
            CurrentMethod::RecoveryReceipt,
        ] {
            assert_eq!(method.name().parse::<CurrentMethod>(), Ok(method));
            assert_eq!(method.legacy(), None);
        }
        assert_eq!(
            "floor.session.issue".parse::<CurrentMethod>(),
            Ok(CurrentMethod::Legacy(Method::FloorSessionIssue))
        );
        assert!("floor.session.exports".parse::<CurrentMethod>().is_err());
        for name in ["recovery.inspect", "recovery.submit", "recovery.receipt"] {
            assert!(name.parse::<Method>().is_err());
            assert!(!Method::ALL.iter().any(|method| method.name() == name));
        }
        assert!("recovery.submit.extra".parse::<CurrentMethod>().is_err());
    }
}
