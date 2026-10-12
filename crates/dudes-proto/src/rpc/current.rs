use core::str::FromStr;

use schemars::JsonSchema;

use super::ticket::{TicketCheckoutParams, TicketCheckoutResult};
use super::{Method, MethodShape, Methods, UnknownName};

/// Method names understood by the current protocol.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CurrentMethod {
    /// A method from the original method table.
    Legacy(Method),
    /// Read the current recorded checkout for one ticket.
    TicketCheckout,
}

impl CurrentMethod {
    /// The JSON-RPC method name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Legacy(method) => method.name(),
            Self::TicketCheckout => "ticket.checkout",
        }
    }

    /// The original method, when this entry belongs to that table.
    #[must_use]
    pub const fn legacy(self) -> Option<Method> {
        match self {
            Self::Legacy(method) => Some(method),
            Self::TicketCheckout => None,
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
        if text == Self::TicketCheckout.name() {
            Ok(Self::TicketCheckout)
        } else {
            text.parse().map(Self::Legacy)
        }
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
}
