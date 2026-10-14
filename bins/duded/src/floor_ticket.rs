//! The Floor's ticket routes: one bounded request each, the limits that follow a session, and the
//! fixed replies. Nothing here names an author, a program, an argv or a ceiling: the daemon
//! resolves who is asking from the admitted session, and a ticket's check is chosen by id and
//! revision from the operator's own enrollment.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionEndBoundary, InstructionIssueAssociation,
    InstructionObservationOwner, InstructionPendingReview, InstructionReviewInterval,
    InstructionReviewWatermark,
};
use dudes_proto::record::Hex32;
use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAssignParams, TicketCreateParams, TicketEditParams, TicketReadParams,
    TicketReadResult, TicketRemoveParams, TicketStopParams,
};
use dudes_proto::{TicketRefusal, TicketRow};
use dudes_types::{AgentId, CardId, Lsn, Seq};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most bytes of a ticket request, and of the answer to a read.
pub const MAX_TICKET_BODY_BYTES: usize = 24_576;
/// The most JSON nesting a request may carry.
const MAX_DEPTH: usize = 8;
/// The most tickets one session may create.
pub const MOST_CREATES: u32 = 64;
/// The most assignments one session may make, counting each that was accepted.
pub const MOST_ASSIGNS: u32 = 32;
/// How long a session's counts are kept after its last request.
const KEEP: Duration = Duration::from_hours(24);

/// The `ticket` sibling a Floor snapshot carries exactly when `[floor.board]` is enabled: an empty
/// object, outside `FloorProjection` and `floor.delta`. A page shows no ticket form without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TicketSwitch {}

/// The one value [`TicketSwitch`] takes.
pub const TICKET_SWITCH: TicketSwitch = TicketSwitch {};

/// Which ticket action a routed POST carries. The path alone decides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketRoute {
    /// `POST /floor/ticket`.
    Create,
    /// `POST /floor/ticket/edit`.
    Edit,
    /// `POST /floor/ticket/assign`.
    Assign,
    /// `POST /floor/ticket/accept`.
    Accept,
    /// `POST /floor/ticket/stop`.
    Stop,
    /// `POST /floor/ticket/remove`.
    Remove,
    /// `POST /floor/ticket/read`.
    Read,
}

impl TicketRoute {
    /// The route a path names.
    #[must_use]
    pub fn of(path: &str) -> Option<Self> {
        Some(match path {
            "/floor/ticket" => Self::Create,
            "/floor/ticket/edit" => Self::Edit,
            "/floor/ticket/assign" => Self::Assign,
            "/floor/ticket/accept" => Self::Accept,
            "/floor/ticket/stop" => Self::Stop,
            "/floor/ticket/remove" => Self::Remove,
            "/floor/ticket/read" => Self::Read,
            _ => return None,
        })
    }
}

/// One admitted request. Each is exactly the members of the matching `ticket.*` method.
#[derive(Clone, Debug)]
pub enum TicketRequest {
    /// Make a ticket.
    Create(TicketCreateParams),
    /// Change a ticket in To do.
    Edit(TicketEditParams),
    /// Give a ticket to an agent.
    Assign(TicketAssignParams),
    /// Accept a ticket that is Ready for you.
    Accept(TicketAcceptParams),
    /// Stop a ticket that is Working or in Needs you.
    Stop(TicketStopParams),
    /// Remove a ticket that is not Working.
    Remove(TicketRemoveParams),
    /// Read a ticket's detail.
    Read(TicketReadParams),
}

impl TicketRequest {
    /// Decode one body: a single JSON object of the route's members and no others, within the
    /// byte and depth bounds. An unknown or duplicate member is refused by the typed decoder,
    /// before any effect.
    pub(crate) fn parse(route: TicketRoute, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_TICKET_BODY_BYTES {
            bail!("Floor ticket request is too large");
        }
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor ticket request is not one JSON object");
        }
        if depth(bytes) > MAX_DEPTH {
            bail!("Floor ticket request is nested too deeply");
        }
        Ok(match route {
            TicketRoute::Create => {
                let request: TicketCreateParams = decode(bytes)?;
                named_check(
                    request.done_when.is_some(),
                    request.done_when_revision.is_some(),
                )?;
                Self::Create(request)
            }
            TicketRoute::Edit => {
                let request: TicketEditParams = decode(bytes)?;
                named_check(
                    request.done_when.is_some(),
                    request.done_when_revision.is_some(),
                )?;
                Self::Edit(request)
            }
            TicketRoute::Assign => {
                let request: FloorAssign = decode(bytes)?;
                Self::Assign(TicketAssignParams {
                    card: request.card,
                    agent: request.agent,
                    message: request.message,
                    expected_seq: request.expected_seq,
                    review_watermark: request.review_watermark,
                })
            }
            TicketRoute::Accept => {
                let request: FloorAccept = decode(bytes)?;
                Self::Accept(TicketAcceptParams {
                    card: request.card,
                    expected_seq: request.expected_seq,
                    review_watermark: request.review_watermark,
                })
            }
            TicketRoute::Stop => Self::Stop(decode(bytes)?),
            TicketRoute::Remove => Self::Remove(decode(bytes)?),
            TicketRoute::Read => Self::Read(decode(bytes)?),
        })
    }

    /// Whether the request counts against the session's creations or its assignments.
    pub(crate) const fn kind(&self) -> Option<Counted> {
        match self {
            Self::Create(_) => Some(Counted::Create),
            Self::Assign(_) => Some(Counted::Assign),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorAssign {
    card: CardId,
    agent: AgentId,
    #[serde(default)]
    message: Option<String>,
    expected_seq: Seq,
    #[serde(default, deserialize_with = "http_watermark")]
    review_watermark: Option<InstructionReviewWatermark>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorAccept {
    card: CardId,
    expected_seq: Seq,
    #[serde(default, deserialize_with = "http_watermark")]
    review_watermark: Option<InstructionReviewWatermark>,
}

fn http_watermark<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<InstructionReviewWatermark>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Watermark {
        through_lsn: String,
        set_digest: Hex32,
    }
    let Some(value) = Option::<Watermark>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let lsn = value
        .through_lsn
        .parse::<u64>()
        .map_err(serde::de::Error::custom)?;
    if value.through_lsn != lsn.to_string() {
        return Err(serde::de::Error::custom(
            "instruction review position is not canonical",
        ));
    }
    Ok(Some(InstructionReviewWatermark {
        through_lsn: Lsn(lsn),
        set_digest: value.set_digest,
    }))
}

/// A check is chosen by its id and the revision the page saw, both or neither, so that a changed
/// check is refused whatever the page sends.
fn named_check(id: bool, revision: bool) -> Result<()> {
    if id != revision {
        bail!("a check is named by its id and its revision together");
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(bytes)?)
}

/// The deepest nesting of `{` and `[` outside strings.
fn depth(bytes: &[u8]) -> usize {
    let (mut deepest, mut now) = (0_usize, 0_usize);
    let (mut in_string, mut escaped) = (false, false);
    for &byte in bytes {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                now += 1;
                deepest = deepest.max(now);
            }
            b'}' | b']' => now = now.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// What the Floor is told. Every case but `Done` has an empty or fixed body.
#[derive(Debug, PartialEq)]
pub enum TicketReply {
    /// `200`: the record is durable and projected. The body names the card and the position the
    /// answer is true through.
    Done(Value),
    /// `409`: refused before any append, for one of the fixed codes.
    Refused(&'static str),
    /// `400`: a malformed request.
    Invalid,
    /// `403`: the session no longer authorizes an append.
    Forbidden,
    /// `503`: the append outcome is unknown, or the daemon is ending.
    Unavailable,
}

/// The fixed code for a refusal that carries a card's state or sequence.
#[must_use]
pub fn refusal_code(code: &str) -> Option<&'static str> {
    [
        TicketRefusal::StaleCard,
        TicketRefusal::NotATicket,
        TicketRefusal::TicketCard,
        TicketRefusal::WrongState,
        TicketRefusal::ProjectUnavailable,
        TicketRefusal::DoneWhenChanged,
        TicketRefusal::AgentUnavailable,
        TicketRefusal::AgentNoProject,
        TicketRefusal::QueueFull,
        TicketRefusal::LimitReached,
        TicketRefusal::Busy,
        TicketRefusal::InstructionComparisonPending,
        TicketRefusal::InstructionReviewRequired,
    ]
    .into_iter()
    .map(TicketRefusal::code)
    .find(|known| *known == code)
}

/// Which count a request moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
    /// A creation.
    Create,
    /// An assignment.
    Assign,
}

#[derive(Debug)]
struct SessionCounts {
    creates: u32,
    assigns: u32,
    busy: bool,
    seen: Instant,
}

/// What follows each session: how many tickets it created and assigned, and whether a request of
/// its own is in flight. Held for the run, and dropped for a session not seen for a day.
#[derive(Debug, Default)]
pub struct TicketLimits {
    sessions: Mutex<BTreeMap<[u8; 32], SessionCounts>>,
}

impl TicketLimits {
    /// No session has asked yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    /// How many tickets `session` has created and assigned in this run.
    pub(crate) fn counts(&self, session: [u8; 32]) -> (u32, u32) {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions
            .get(&session)
            .map_or((0, 0), |counts| (counts.creates, counts.assigns))
    }

    /// Continue `session`'s counts from `creates` and `assigns`.
    pub(crate) fn seed(&self, session: [u8; 32], creates: u32, assigns: u32) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.insert(
            session,
            SessionCounts {
                creates,
                assigns,
                busy: false,
                seen: Instant::now(),
            },
        );
    }

    /// Admit one request of `session`, or the reason it is refused: `busy` while another of its
    /// requests is in flight, `limit_reached` when a count it would move is spent. A refused request changes nothing.
    pub(crate) fn begin(
        &self,
        session: [u8; 32],
        counted: Option<Counted>,
    ) -> Result<Turn<'_>, &'static str> {
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, counts| counts.busy || now.duration_since(counts.seen) < KEEP);
        let counts = sessions.entry(session).or_insert(SessionCounts {
            creates: 0,
            assigns: 0,
            busy: false,
            seen: now,
        });
        counts.seen = now;
        if counts.busy {
            return Err("busy");
        }
        if (counted == Some(Counted::Create) && counts.creates >= MOST_CREATES)
            || (counted == Some(Counted::Assign) && counts.assigns >= MOST_ASSIGNS)
        {
            return Err("limit_reached");
        }
        counts.busy = true;
        Ok(Turn {
            limits: self,
            session,
            counted,
        })
    }
}

/// One admitted request. The session's other requests wait until it ends.
#[derive(Debug)]
pub(crate) struct Turn<'a> {
    limits: &'a TicketLimits,
    session: [u8; 32],
    counted: Option<Counted>,
}

impl Turn<'_> {
    /// The record is durable: the request counts.
    pub(crate) fn succeeded(&self) {
        let mut sessions = self
            .limits
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(counts) = sessions.get_mut(&self.session) {
            match self.counted {
                Some(Counted::Create) => counts.creates = counts.creates.saturating_add(1),
                Some(Counted::Assign) => counts.assigns = counts.assigns.saturating_add(1),
                None => {}
            }
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut sessions = self
            .limits
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(counts) = sessions.get_mut(&self.session) {
            counts.busy = false;
        }
    }
}

/// The answer to a read: the card, and what the board keeps of the ticket. The agent's summary is
/// the agent's own words and is sent as a string for the page to show as text. Shortened display
/// fields and omitted evidence are marked. Any retained native path identity and review watermark
/// are unchanged.
#[must_use]
pub fn read_body(read: &TicketReadResult) -> Value {
    let mut body = read_value(read);
    loop {
        let size = serde_json::to_vec(&body).map_or(usize::MAX, |bytes| bytes.len());
        if size <= MAX_TICKET_BODY_BYTES {
            return body;
        }
        if let Some(summary) = body["ticket"]["summary"].as_str().map(str::to_owned) {
            let mut cut = summary.len() / 2;
            while !summary.is_char_boundary(cut) {
                cut -= 1;
            }
            body["ticket"]["summary"] = if cut == 0 {
                Value::Null
            } else {
                json!(summary[..cut])
            };
            if !summary.is_empty() {
                mark_truncated(&mut body, TicketReadTruncation::Summary);
            }
            continue;
        }
        if trim_instruction_evidence(&mut body["ticket"]["instruction_review"]) {
            mark_truncated(&mut body, TicketReadTruncation::InstructionEvidence);
            continue;
        }
        if !trim_ticket_display_text(&mut body) {
            return json!({"state":"unavailable", "truncated":[TicketReadTruncation::Ticket]});
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum TicketReadTruncation {
    Note,
    Title,
    CheckLabel,
    WaitingNote,
    Summary,
    InstructionEvidence,
    Ticket,
}

fn mark_truncated(body: &mut Value, field: TicketReadTruncation) {
    if body["ticket"]["truncated"].is_null() {
        body["ticket"]["truncated"] = json!([]);
    }
    let fields = body["ticket"]["truncated"]
        .as_array_mut()
        .expect("truncation fields");
    let field = json!(field);
    if !fields.contains(&field) {
        fields.push(field);
    }
}

fn trim_ticket_display_text(body: &mut Value) -> bool {
    for (field, pointer) in [
        (TicketReadTruncation::Note, "/ticket/note"),
        (TicketReadTruncation::Title, "/card/title"),
        (TicketReadTruncation::CheckLabel, "/ticket/check/label"),
        (TicketReadTruncation::WaitingNote, "/ticket/waiting/note"),
    ] {
        let Some(text) = body
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        let mut cut = text.len() / 2;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        *body.pointer_mut(pointer).expect("existing display field") = json!(text[..cut]);
        mark_truncated(body, field);
        return true;
    }
    false
}

fn mark_evidence_omitted(review: &mut Value) {
    let gaps = review["gaps"].as_array_mut().expect("review gaps exist");
    if !gaps.iter().any(|gap| gap["reason"] == "metadata_limit") {
        gaps.push(
            json!({"reason":"metadata_limit", "context":{"kind":"unresolved"},
            "agent":null, "path":null, "display":null, "display_truncated":false}),
        );
    }
}

fn trim_instruction_evidence(review: &mut Value) -> bool {
    if review["gaps"].as_array().is_none() {
        return false;
    }
    for name in ["changed_paths", "intervals"] {
        if let Some(rows) = review[name].as_array_mut()
            && rows.len() > 1
        {
            rows.pop();
            mark_evidence_omitted(review);
            return true;
        }
    }
    let gaps = review["gaps"].as_array_mut().expect("review gaps exist");
    if gaps
        .iter()
        .filter(|gap| gap["reason"] != "metadata_limit")
        .count()
        > 1
    {
        let index = gaps
            .iter()
            .rposition(|gap| gap["reason"] != "metadata_limit")
            .expect("original gap exists");
        gaps.remove(index);
        mark_evidence_omitted(review);
        return true;
    }
    if let Some(index) = gaps.iter().position(|gap| !gap["path"].is_null()) {
        let gap = &mut gaps[index];
        gap["path"] = Value::Null;
        gap["display"] = Value::Null;
        gap["display_truncated"] = json!(false);
        mark_evidence_omitted(review);
        return true;
    }
    false
}

fn read_value(read: &TicketReadResult) -> Value {
    let TicketReadResult {
        card,
        ticket,
        position,
        waiting,
    } = read;
    let TicketRow {
        description,
        project,
        done_when,
        round,
        reason,
        summary,
        check,
        instruction_review,
        coverage,
        ..
    } = ticket;
    let mut body = json!({
        "state": "ok",
        "card": {
            "id": card.id,
            "title": card.title,
            "status": card.status,
            "seq": card.seq,
            "claimed_by": card.claimed_by,
        },
        "ticket": {
            "note": description,
            "project": project,
            "check": {"id": done_when.id, "label": done_when.label},
            "round": round,
            "reason": reason,
            "summary": summary,
            "check_result": check,
            "position": position,
            "waiting": waiting,
        },
    });
    if let Some(review) = instruction_review {
        body["ticket"]["instruction_review"] = instruction_review_value(review);
    }
    if let Some(coverage) = coverage {
        body["ticket"]["coverage"] = json!(coverage);
    }
    body
}

fn instruction_interval_value(interval: &InstructionReviewInterval) -> Value {
    let (kind, agent, turn, attachment_lsn, issue_lsn) = match interval.owner {
        InstructionObservationOwner::Managed { agent, turn, issue } => {
            let issue = match issue {
                InstructionIssueAssociation::Prospective => None,
                InstructionIssueAssociation::Issued { issue_lsn } => Some(issue_lsn.0.to_string()),
            };
            ("managed", agent, Some(turn), None, issue)
        }
        InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn,
        } => (
            "attached_process",
            agent,
            None,
            Some(attachment_lsn.0.to_string()),
            None,
        ),
        InstructionObservationOwner::AttachedTurn {
            agent,
            turn,
            attachment_lsn,
        } => (
            "attached_turn",
            agent,
            Some(turn),
            attachment_lsn.map(|lsn| lsn.0.to_string()),
            None,
        ),
    };
    let (baseline_lsn, missing_start) = match interval.baseline {
        InstructionBaselineRef::Started { lsn } => (Some(lsn.0.to_string()), false),
        InstructionBaselineRef::MissingTurnBoundary { .. }
        | InstructionBaselineRef::MissingIssueBoundary { .. } => (None, true),
    };
    let end_lsn = interval
        .end
        .map(|end| match end {
            InstructionEndBoundary::ManagedTurn { end_lsn, .. }
            | InstructionEndBoundary::AttachedProcess { end_lsn, .. }
            | InstructionEndBoundary::AttachedTurn { end_lsn, .. } => end_lsn.0.to_string(),
        })
        .or_else(|| match interval.baseline {
            InstructionBaselineRef::MissingTurnBoundary { end_lsn, .. } => {
                Some(end_lsn.0.to_string())
            }
            InstructionBaselineRef::Started { .. }
            | InstructionBaselineRef::MissingIssueBoundary { .. } => None,
        });
    json!({
        "kind": kind, "agent": agent, "turn": turn,
        "attachment_lsn": attachment_lsn, "issue_lsn": issue_lsn,
        "baseline_lsn": baseline_lsn, "missing_start": missing_start,
        "card": interval.ticket.card, "goal": interval.ticket.goal,
        "assignment_lsn": interval.ticket.assignment_lsn.0.to_string(),
        "seq_at_start": interval.ticket.seq_at_start,
        "finished_lsn": interval.finished_lsn.map(|lsn| lsn.0.to_string()),
        "end_lsn": end_lsn, "sampled_at": interval.sampled_at, "recovery": interval.recovery,
    })
}

fn instruction_review_value(review: &InstructionPendingReview) -> Value {
    json!({
        "watermark": {
            "through_lsn": review.watermark.through_lsn.0.to_string(),
            "set_digest": review.watermark.set_digest,
        },
        "pending_comparison": review.pending_comparison,
        "intervals": review.intervals.iter().map(instruction_interval_value).collect::<Vec<_>>(),
        "changed_paths": review.changed_paths.iter().map(|change| json!({
            "path": change.path, "display": change.path.display(),
            "display_truncated": change.path.display_truncated(), "context": change.context,
        })).collect::<Vec<_>>(),
        "gaps": review.gaps.iter().map(|gap| json!({
            "reason": gap.reason, "context": gap.context, "agent": gap.agent,
            "path": gap.path, "display": gap.path.as_ref().map(dudes_proto::instruction::NativeInstructionPath::display),
            "display_truncated": gap.path.as_ref().is_some_and(dudes_proto::instruction::NativeInstructionPath::display_truncated),
        })).collect::<Vec<_>>(),
    })
}

/// The body of a successful action: the card, its sequence when it still exists, and the journal
/// position the answer is true through.
#[must_use]
pub fn done_body(
    state: &'static str,
    card: dudes_types::CardId,
    seq: Option<dudes_types::Seq>,
    lsn: dudes_types::Lsn,
    start: Option<Value>,
) -> Value {
    let mut body = json!({"state": state, "card": card, "lsn": lsn.0.to_string()});
    if let Some(seq) = seq {
        body["seq"] = json!(seq);
    }
    if let Some(start) = start {
        body["start"] = start;
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_counts_nesting_outside_strings() {
        assert_eq!(depth(br#"{"a":"{{{{"}"#), 1);
        assert_eq!(depth(br#"{"a":[[[[[[[[1]]]]]]]]}"#), 9);
        assert_eq!(depth(br#"{"a":"\"{{"}"#), 1);
    }

    #[test]
    fn a_request_is_one_flat_object_of_the_routes_members() {
        let ok = br#"{"title":"Fix it"}"#;
        let rev = "0".repeat(64);
        let both = format!(r#"{{"title":"x","done_when":"default","done_when_revision":"{rev}"}}"#);
        assert!(TicketRequest::parse(TicketRoute::Create, both.as_bytes()).is_ok());
        for lone in [
            br#"{"title":"x","done_when":"default"}"#.to_vec(),
            format!(r#"{{"title":"x","done_when_revision":"{rev}"}}"#).into_bytes(),
        ] {
            assert!(TicketRequest::parse(TicketRoute::Create, &lone).is_err());
        }
        let lone_edit =
            br#"{"card":"01JBQ8XK2M3N4P5Q6R7S8T9VWX","expected_seq":1,"done_when":"default"}"#;
        assert!(TicketRequest::parse(TicketRoute::Edit, lone_edit).is_err());
        assert!(TicketRequest::parse(TicketRoute::Create, ok).is_ok());
        for bad in [
            &br#"{"title":"x","author":"me"}"#[..],
            br#"{"title":"x","title":"y"}"#,
            br#"["x"]"#,
            br#"{"title":{"a":1}}"#,
            br#"{"title":"x","argv":["rm"]}"#,
            b"",
        ] {
            assert!(
                TicketRequest::parse(TicketRoute::Create, bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let big = format!(r#"{{"title":"{}"}}"#, "x".repeat(MAX_TICKET_BODY_BYTES));
        assert!(TicketRequest::parse(TicketRoute::Create, big.as_bytes()).is_err());
    }

    #[test]
    fn a_session_has_one_request_in_flight_and_spends_its_counts_on_success() {
        let limits = TicketLimits::new();
        let key = [7_u8; 32];
        let first = limits.begin(key, Some(Counted::Create)).expect("first");
        assert_eq!(limits.begin(key, None).err(), Some("busy"), "one at a time");
        assert!(limits.begin([8_u8; 32], None).is_ok(), "per session");
        first.succeeded();
        drop(first);
        for _ in 1..MOST_CREATES {
            let turn = limits.begin(key, Some(Counted::Create)).expect("within");
            turn.succeeded();
        }
        assert_eq!(
            limits.begin(key, Some(Counted::Create)).err(),
            Some("limit_reached")
        );
        assert!(limits.begin(key, Some(Counted::Assign)).is_ok());
        assert!(limits.begin(key, None).is_ok(), "reads are not counted");
    }

    #[test]
    fn a_refused_create_does_not_count() {
        let limits = TicketLimits::new();
        let key = [1_u8; 32];
        for _ in 0..(MOST_CREATES * 2) {
            drop(limits.begin(key, Some(Counted::Create)).expect("not spent"));
        }
    }

    #[test]
    fn only_the_fixed_codes_are_refusals() {
        assert_eq!(refusal_code("stale_card"), Some("stale_card"));
        assert_eq!(refusal_code("limit_reached"), Some("limit_reached"));
        assert_eq!(refusal_code("rm -rf"), None);
    }

    #[test]
    fn reviewed_actions_decode_exact_decimal_positions_without_changing_socket_types() {
        let card = CardId::from_parts(1, 1);
        let agent = AgentId::from_parts(1, 2);
        let watermark = json!({"through_lsn":"9007199254740993", "set_digest":"dd".repeat(32)});
        let accept = json!({"card":card, "expected_seq":7, "review_watermark":watermark});
        let TicketRequest::Accept(parsed) = TicketRequest::parse(
            TicketRoute::Accept,
            &serde_json::to_vec(&accept).expect("json"),
        )
        .expect("accept") else {
            panic!("accept request");
        };
        assert_eq!(
            parsed.review_watermark.expect("watermark").through_lsn,
            Lsn(9_007_199_254_740_993)
        );
        assert_eq!(
            serde_json::to_value(parsed).expect("socket json")["review_watermark"]["through_lsn"],
            9_007_199_254_740_993_u64
        );
        let assign = json!({"card":card, "agent":agent, "message":"Revisit the change", "expected_seq":7, "review_watermark":watermark});
        let TicketRequest::Assign(parsed) = TicketRequest::parse(
            TicketRoute::Assign,
            &serde_json::to_vec(&assign).expect("json"),
        )
        .expect("assign") else {
            panic!("assign request");
        };
        assert_eq!(parsed.message.as_deref(), Some("Revisit the change"));
        assert_eq!(
            parsed.review_watermark.expect("watermark").through_lsn,
            Lsn(9_007_199_254_740_993)
        );
        for position in [
            json!(9_007_199_254_740_992_u64),
            json!("01"),
            json!("+1"),
            json!("18446744073709551616"),
        ] {
            let mut bad = accept.clone();
            bad["review_watermark"]["through_lsn"] = position;
            assert!(
                TicketRequest::parse(
                    TicketRoute::Accept,
                    &serde_json::to_vec(&bad).expect("json")
                )
                .is_err()
            );
        }
        let duplicate = format!(
            r#"{{"card":"{card}","expected_seq":7,"review_watermark":{{"through_lsn":"1","through_lsn":"2","set_digest":"{}"}}}}"#,
            "dd".repeat(32)
        );
        assert!(TicketRequest::parse(TicketRoute::Accept, duplicate.as_bytes()).is_err());
    }

    fn instruction_read_fixture() -> TicketReadResult {
        use dudes_proto::instruction::{
            InstructionChangedPath, InstructionCoverageGap, InstructionGapKind,
            InstructionPathContext, InstructionTicketAssociation, NativeInstructionPath,
        };
        use dudes_proto::{CardView, Predicate, TicketCheck};
        use dudes_types::{CardStatus, GoalId, Timestamp, TurnId};

        let card = CardId::from_parts(1, 1);
        let agent = AgentId::from_parts(1, 2);
        let turn = TurnId::from_parts(1, 3);
        let interval = InstructionReviewInterval {
            owner: InstructionObservationOwner::Managed {
                agent,
                turn,
                issue: InstructionIssueAssociation::Issued { issue_lsn: Lsn(10) },
            },
            baseline: InstructionBaselineRef::Started { lsn: Lsn(11) },
            ticket: InstructionTicketAssociation {
                card,
                goal: GoalId::from_parts(1, 4),
                assignment_lsn: Lsn(9),
                seq_at_start: Seq(2),
            },
            finished_lsn: Some(Lsn(9_007_199_254_740_993)),
            end: Some(InstructionEndBoundary::ManagedTurn {
                turn,
                end_lsn: Lsn(13),
            }),
            sampled_at: Some(Timestamp::UNIX_EPOCH),
            recovery: true,
        };
        let path = NativeInstructionPath::Utf8 {
            value: format!("/project/{}", "a".repeat(3_900)),
        };
        let review = InstructionPendingReview {
            watermark: InstructionReviewWatermark {
                through_lsn: Lsn(9_007_199_254_740_993),
                set_digest: Hex32::new([0xdd; 32]),
            },
            pending_comparison: false,
            intervals: vec![interval; 8],
            changed_paths: vec![
                InstructionChangedPath {
                    path: path.clone(),
                    context: InstructionPathContext::SharedProject
                };
                4
            ],
            gaps: vec![
                InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::ReadFailed,
                    context: InstructionPathContext::SharedProject,
                    agent: Some(agent),
                    path: Some(path)
                };
                4
            ],
        };
        TicketReadResult {
            card: CardView {
                id: card,
                title: "Review instructions".to_owned(),
                department: "engineering".to_owned(),
                status: CardStatus::Blocked,
                status_by: dudes_proto::record::Actor::System,
                seq: Seq(3),
                claimed_by: Some(agent),
                deps: vec![],
                ticket: None,
            },
            ticket: TicketRow {
                description: "n".repeat(8_192),
                project: "service".to_owned(),
                done_when: TicketCheck {
                    id: "default".to_owned(),
                    label: "passes".to_owned(),
                    predicate: Predicate::Command {
                        argv: vec!["false".to_owned()],
                        timeout_s: 1,
                    },
                },
                goal: None,
                queue: None,
                message: None,
                round: 1,
                reason: None,
                summary: Some("s".repeat(4_096)),
                check: None,
                instruction_review: Some(review),
                coverage: None,
            },
            position: None,
            waiting: None,
        }
    }

    #[test]
    fn bounded_read_retains_known_change_original_gap_and_exact_review_cut() {
        let read = instruction_read_fixture();
        let body = read_body(&read);
        assert!(serde_json::to_vec(&body).expect("json").len() <= MAX_TICKET_BODY_BYTES);
        assert_eq!(body["ticket"]["note"], "n".repeat(8_192));
        let shown = &body["ticket"]["instruction_review"];
        assert_eq!(shown["watermark"]["through_lsn"], "9007199254740993");
        assert_eq!(shown["intervals"][0]["finished_lsn"], "9007199254740993");
        assert_eq!(shown["changed_paths"].as_array().expect("paths").len(), 1);
        let gaps = shown["gaps"].as_array().expect("gaps");
        assert!(gaps.iter().any(|gap| gap["reason"] == "read_failed"));
        assert!(gaps.iter().any(|gap| gap["reason"] == "metadata_limit"));
    }

    #[test]
    fn escaping_heavy_valid_ticket_read_stays_within_the_transport_bound() {
        use dudes_proto::instruction::{InstructionCoverageGap, NativeInstructionPath};
        use dudes_proto::rpc::goal::CompletionCheckDeclaration;
        use dudes_proto::ticket::{MAX_NOTE_BYTES, MAX_TITLE_BYTES, clean_text, clean_title};
        use dudes_proto::{TicketWait, TicketWaiting};

        let mut read = instruction_read_fixture();
        read.ticket.description = format!("x{}x", "\n".repeat(8_190));
        read.card.title = "\"".repeat(256);
        read.ticket.done_when.label = "\\".repeat(128);
        let waiting = TicketWait::Ticket { card: read.card.id };
        read.waiting = Some(TicketWaiting {
            wait: waiting,
            note: waiting.sentence("", &read.card.title),
        });
        read.ticket.summary = None;
        let path = NativeInstructionPath::Utf8 {
            value: format!("/project/{}", vec!["\n".repeat(200); 10].join("/")),
        };
        assert!(clean_text(&read.ticket.description, MAX_NOTE_BYTES));
        assert!(clean_title(&read.card.title, MAX_TITLE_BYTES));
        CompletionCheckDeclaration::new(&read.ticket.done_when.id, &read.ticket.done_when.label)
            .expect("valid check label");
        assert!(clean_text(
            &read.waiting.as_ref().expect("waiting").note,
            600
        ));
        path.validate().expect("valid bounded native path");
        assert_eq!(serde_json::to_vec(&path).expect("path json").len(), 4_048);
        assert_eq!(path.display().len(), 4_018);
        let review = read.ticket.instruction_review.as_mut().expect("review");
        review.intervals.truncate(1);
        review.changed_paths.truncate(1);
        review.changed_paths[0].path = path.clone();
        review.gaps = vec![InstructionCoverageGap::metadata_limit()];
        let initial = read_value(&read);
        assert!(serde_json::to_vec(&initial).expect("json").len() > MAX_TICKET_BODY_BYTES);
        let body = read_body(&read);
        assert!(serde_json::to_vec(&body).expect("json").len() <= MAX_TICKET_BODY_BYTES);
        assert_eq!(body["state"], "ok");
        assert_eq!(
            body["ticket"]["instruction_review"]["changed_paths"][0]["path"],
            serde_json::to_value(path).expect("native identity")
        );
        assert_eq!(
            body["ticket"]["instruction_review"]["watermark"]["through_lsn"],
            "9007199254740993"
        );
        assert!(
            body["ticket"]["truncated"]
                .as_array()
                .expect("omission marker")
                .contains(&json!("note"))
        );
    }

    #[test]
    fn oversized_identity_data_is_omitted_instead_of_returning_an_oversized_read() {
        let mut read = instruction_read_fixture();
        read.ticket.project = "x".repeat(MAX_TICKET_BODY_BYTES);
        let body = read_body(&read);
        assert_eq!(body, json!({"state":"unavailable", "truncated":["ticket"]}));
        assert!(serde_json::to_vec(&body).expect("json").len() <= MAX_TICKET_BODY_BYTES);
    }

    #[test]
    fn a_missing_issue_boundary_is_not_serialized_as_a_baseline_or_end() {
        let read = instruction_read_fixture();
        let mut interval = read.ticket.instruction_review.expect("review").intervals[0].clone();
        let turn = dudes_types::TurnId::from_parts(1, 3);
        interval.baseline = InstructionBaselineRef::MissingIssueBoundary {
            turn,
            issue_lsn: Lsn(10),
        };
        interval.end = None;
        interval.sampled_at = None;
        interval.recovery = true;
        let shown = instruction_interval_value(&interval);
        assert_eq!(shown["issue_lsn"], "10");
        assert_eq!(shown["baseline_lsn"], Value::Null);
        assert_eq!(shown["end_lsn"], Value::Null);
        assert_eq!(shown["sampled_at"], Value::Null);
        assert_eq!(shown["missing_start"], true);
        assert_eq!(shown["recovery"], true);
        assert_eq!(shown["seq_at_start"], 2);
    }
}
