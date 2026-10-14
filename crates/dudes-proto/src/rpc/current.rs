use core::str::FromStr;

use schemars::JsonSchema;

use super::floor::{SessionExport, SessionImportParams, SessionImportResult};
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
        }
    }

    /// The original method, when this entry belongs to that table.
    #[must_use]
    pub const fn legacy(self) -> Option<Method> {
        match self {
            Self::Legacy(method) => Some(method),
            Self::TicketCheckout | Self::FloorSessionExport | Self::FloorSessionImport => None,
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
        ] {
            assert_eq!(method.name().parse::<CurrentMethod>(), Ok(method));
            assert_eq!(method.legacy(), None);
        }
        assert_eq!(
            "floor.session.issue".parse::<CurrentMethod>(),
            Ok(CurrentMethod::Legacy(Method::FloorSessionIssue))
        );
        assert!("floor.session.exports".parse::<CurrentMethod>().is_err());
    }
}
