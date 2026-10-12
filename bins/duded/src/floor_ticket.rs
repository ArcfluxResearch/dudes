//! The Floor's ticket routes: one bounded request each, the limits that follow a session, and the
//! fixed replies. Nothing here names an author, a program, an argv or a ceiling: the daemon
//! resolves who is asking from the admitted session, and a ticket's check is chosen by id and
//! revision from the operator's own enrollment.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAssignParams, TicketCreateParams, TicketEditParams, TicketReadParams,
    TicketReadResult, TicketRemoveParams, TicketStopParams,
};
use dudes_proto::{TicketRefusal, TicketRow};
use serde::de::DeserializeOwned;
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
    /// Decode one body: a single flat JSON object of the route's members and no others, within the
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
            TicketRoute::Assign => Self::Assign(decode(bytes)?),
            TicketRoute::Accept => Self::Accept(decode(bytes)?),
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
/// the agent's own words and is sent as a string for the page to show as text. When the answer
/// would pass its bound, the summary is cut until it fits.
#[must_use]
pub fn read_body(read: &TicketReadResult) -> Value {
    let mut body = read_value(read);
    loop {
        let size = serde_json::to_vec(&body).map_or(usize::MAX, |bytes| bytes.len());
        if size <= MAX_TICKET_BODY_BYTES {
            return body;
        }
        let Some(summary) = body["ticket"]["summary"].as_str().map(str::to_owned) else {
            return body;
        };
        let mut cut = summary.len() / 2;
        while !summary.is_char_boundary(cut) {
            cut -= 1;
        }
        body["ticket"]["summary"] = if cut == 0 {
            Value::Null
        } else {
            json!(summary[..cut])
        };
    }
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
        ..
    } = ticket;
    json!({
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
}
