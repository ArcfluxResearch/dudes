use dudes_types::{Lsn, Seq, Timestamp, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::diff_stamp::TicketBinding;
use crate::instruction::{InstructionBaselineRef, InstructionReviewWatermark};

/// Derived ticket facts, without changing review or completion authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketReadContext {
    /// Exact current round, when independently joined.
    pub binding: Option<TicketBinding>,
    /// Applicability of the retained recorded check.
    pub check: TicketCheckApplicability,
    /// Path-free relationships for the pending review.
    pub recovered_review: TicketRecoveredReview,
}

/// Whether the recorded check describes the current work.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketCheckApplicability {
    /// The exact check and round stamp follow all admitted work.
    Current {
        /// Actual predicate result position.
        check_lsn: Lsn,
    },
    /// Later work makes this a historical result.
    Historical {
        /// Actual predicate result position.
        check_lsn: Lsn,
        /// Exact kind of later work.
        reason: TicketHistoricalCheckReason,
    },
    /// The required identities or physical attempt cannot be established.
    #[default]
    Unknown,
}

/// Admitted work after a recorded check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketHistoricalCheckReason {
    /// A new immutable turn was issued.
    NewIssue,
    /// An existing issue has a later exact physical attempt.
    LaterAttempt,
}

/// Completeness of a bounded history read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketHistoryState {
    /// All relevant relationships fit.
    Complete,
    /// A count, byte or traversal bound omitted history.
    Truncated,
    /// An exact source or relationship could not be read.
    #[default]
    Unavailable,
}

/// Earlier reviews and exact recovered intervals, without paths or provider data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketRecoveredReview {
    /// Completeness of the retained history.
    pub state: TicketHistoryState,
    /// Qualified recovered intervals only, including within a mixed review set.
    pub relations: Vec<TicketRecoveredRelation>,
    /// Actual earlier acknowledgment frames, grouped by their own position.
    pub acknowledgments: Vec<TicketAcknowledgedReview>,
}

/// One recovered interval and its earlier tracked attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketRecoveredRelation {
    /// Exact current review interval.
    pub interval: InstructionBaselineRef,
    /// Immutable turn of the recovered work.
    pub turn: TurnId,
    /// Actual authorization of this recovery.
    pub authorization_lsn: Lsn,
    /// Exact immutable issued task.
    pub issue_lsn: Lsn,
    /// Exact earlier attempt, when one source identifies it.
    pub earlier: Option<InstructionBaselineRef>,
    /// Independent tracked native-boundary status.
    pub boundary: TicketPriorBoundary,
    /// Earlier review lookup result.
    pub acknowledgment: TicketPriorReview,
}

/// Native-boundary status does not describe historical external effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketPriorBoundary {
    /// This exact attempt has a positive no-host-call proof, without a native boundary receipt.
    NoHostCall {
        /// Actual supporting observed record.
        proof_lsn: Lsn,
    },
    /// No exact earlier attempt was established.
    Unavailable,
    /// The exact attempt lacks a durable boundary settlement.
    Unsettled,
    /// A durable receipt settled the exact tracked boundary.
    Settled {
        /// Actual settlement position.
        lsn: Lsn,
    },
}

/// Lookup outcome for an exact earlier attempt's acknowledgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketPriorReview {
    /// The actual frame is retained in the acknowledgment section.
    Found {
        /// Actual acknowledgment position.
        lsn: Lsn,
    },
    /// The positively identified earlier attempt has no acknowledgment.
    None,
    /// Its frame or relationship could not be established.
    Unavailable,
    /// The acknowledgment was omitted by an explicit bound.
    Truncated,
}

/// A historical Operator acknowledgment. It supplies no current review watermark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketAcknowledgedReview {
    /// Actual acknowledgment frame position.
    pub lsn: Lsn,
    /// Card sequence admitted by that frame.
    pub expected_seq: Seq,
    /// Complete original review watermark.
    pub watermark: InstructionReviewWatermark,
    /// Actual record time.
    pub at: Timestamp,
    /// Finite action, without assignment message text.
    pub action: TicketAcknowledgmentAction,
    /// Exact earlier intervals linked to this acknowledgment.
    pub intervals: Vec<InstructionBaselineRef>,
}

/// The earlier review's distinct action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketAcknowledgmentAction {
    /// Instructions were acknowledged while work continued.
    AcceptedContinuing,
    /// Completed work was accepted.
    AcceptedCompleted,
    /// Changes were requested.
    ChangesRequested,
}
