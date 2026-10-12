//! Board RPC registration over the daemon's single writer loop.

use dudes_proto::Actor;
use dudes_proto::goal::Evidence;
use dudes_proto::rpc::board::{
    CardClaimParams, CardCreateParams, CardListParams, CardTransitionParams,
};
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_proto::{
    AgentView, CardTicket, CardView, FloorProjection, LiveActivityKind, LiveActivityView,
    Predicate, TicketReason,
};
use dudes_rpc::Dispatcher;
use dudes_rpc::auth::{PeerRole, Requirement};
use dudes_rpc::dispatch::error;
use dudes_rpc::methods::{encode, params};
use dudes_types::{AgentId, CardStatus, Lsn, Seq, Timestamp};

use crate::ports::{Answered, Ask, Handle};

const SNAPSHOT_SIBLING_BYTES: usize = 65_536;

pub(crate) fn actor(role: &PeerRole, pid: Option<u32>) -> Result<Actor, RpcError> {
    match role {
        PeerRole::Operator => pid
            .map(|pid| Actor::Peer { pid, uid: None })
            .ok_or_else(|| error(ErrorCode::Unauthorized, "board caller has no peer pid")),
        PeerRole::Agent { agent } => Ok(Actor::Agent { agent: *agent }),
        PeerRole::Unproven => Err(error(ErrorCode::Unauthorized, "board caller is unproven")),
    }
}

/// Register the four board methods with their declared caller roles.
pub(crate) fn register(dispatcher: &mut Dispatcher, handle: &Handle) {
    let create = handle.clone();
    dispatcher.register_audited(
        Method::BoardCreate,
        Requirement::Operator,
        move |call, pid| {
            let request: CardCreateParams = params(&call)?;
            let by = actor(call.role, pid)?;
            match create.ask(Ask::BoardCreate { request, by })? {
                Answered::CardCreated(result) => encode(Method::BoardCreate, &result),
                _ => Err(error(
                    ErrorCode::InternalError,
                    "board.create returned the wrong result",
                )),
            }
        },
    );

    let claim = handle.clone();
    dispatcher.register_audited(
        Method::BoardClaim,
        Requirement::OperatorOrAgent,
        move |call, pid| {
            let mut request: CardClaimParams = params(&call)?;
            let by = actor(call.role, pid)?;
            request.token = None;
            if let Actor::Agent { agent } = by {
                request.agent = agent;
            }
            claim.ask(Ask::BoardClaim { request, by })?;
            encode(Method::BoardClaim, &dudes_proto::rpc::Empty {})
        },
    );

    let transition = handle.clone();
    dispatcher.register_audited(
        Method::BoardTransition,
        Requirement::OperatorOrAgent,
        move |call, pid| {
            let mut request: CardTransitionParams = params(&call)?;
            let by = actor(call.role, pid)?;
            request.token = None;
            transition.ask(Ask::BoardTransition { request, by })?;
            encode(Method::BoardTransition, &dudes_proto::rpc::Empty {})
        },
    );

    let list = handle.clone();
    dispatcher.register(Method::BoardList, Requirement::Operator, move |call| {
        let request: CardListParams = params(&call)?;
        match list.ask(Ask::BoardList(request))? {
            Answered::Cards(cards) => encode(Method::BoardList, &cards),
            _ => Err(error(
                ErrorCode::InternalError,
                "board.list returned the wrong result",
            )),
        }
    });
}

pub(crate) fn predicate_result(view: &CardView, predicate: &Predicate) -> (bool, Evidence) {
    let evidence = Evidence::Card {
        card: view.id,
        status: view.status,
        by: view.status_by,
    };
    (
        dudes_supervisor::predicate_evidence_eligible(predicate, &evidence),
        evidence,
    )
}

/// The summary a new ticket's card will carry, at the size it can grow to: a place in
/// the longest queue, a round count no ticket reaches and every optional member present.
pub(crate) fn largest_ticket() -> CardTicket {
    CardTicket {
        position: u32::try_from(dudes_proto::ticket::MAX_QUEUE).ok(),
        round: u32::from(u16::MAX),
        reason: Some(TicketReason::ProjectUnavailable),
        stopping: true,
    }
}

pub(crate) fn admit_cards(
    mut agents: Vec<AgentView>,
    mut cards: Vec<CardView>,
) -> Result<(), RpcError> {
    for agent in &mut agents {
        agent.live_activity = Some(LiveActivityView {
            kind: LiveActivityKind::UsingTool,
            at: Timestamp::from_unix_millis(i64::MIN),
            expires_in_ms: 60_000,
        });
    }
    for card in &mut cards {
        card.status = CardStatus::Claimed;
        card.status_by = Actor::Agent {
            agent: AgentId::NIL,
        };
        card.seq = Seq(u64::MAX);
        card.claimed_by = Some(AgentId::NIL);
    }
    let projection = FloorProjection {
        lsn: Lsn(u64::MAX),
        agents,
        cards,
        mail: Vec::new(),
        activity: Vec::new(),
        retired_agents: Vec::new(),
        closing_time: None,
    };
    let message = serde_json::json!({ "type": "floor.snapshot", "projection": projection, "messages": [], "messages_lsn": u64::MAX.to_string(), "threads": [], "threads_lsn":u64::MAX.to_string() });
    let bytes = serde_json::to_vec(&message).map_err(|failure| {
        error(
            ErrorCode::InternalError,
            format!("could not size the board: {failure}"),
        )
    })?;
    if bytes.len()
        > crate::floor_websocket::MAX_MESSAGE_BYTES
            - crate::floor::MAX_MAIL_BYTES
            - crate::floor::ARCHIVE_RESERVE_BYTES
            - crate::floor::QUESTION_HISTORY_RESERVE_BYTES
            - crate::floor::TRACE_RESERVE_BYTES
            - dudes_proto::MAX_ACTIVITY_BYTES
            - SNAPSHOT_SIBLING_BYTES
            - crate::floor_chat::CHAT_RESERVE_BYTES
    {
        return Err(error(
            ErrorCode::Refused,
            "card would exceed the complete Floor snapshot budget",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_types::{CardId, Seq};

    fn done_by(status_by: Actor) -> CardView {
        let agent = AgentId::from_parts(1, 2);
        CardView {
            id: CardId::from_parts(1, 3),
            title: "Check".to_owned(),
            department: "engineering".to_owned(),
            status: CardStatus::Done,
            status_by,
            seq: Seq(2),
            claimed_by: Some(agent),
            deps: vec![],
            ticket: None,
        }
    }

    fn board(status: CardStatus) -> Predicate {
        Predicate::BoardState {
            card: CardId::from_parts(1, 3),
            status,
        }
    }

    #[test]
    fn board_admission_reserves_trace_archive_and_activity_before_they_grow() {
        let mut card = done_by(Actor::Agent {
            agent: AgentId::NIL,
        });
        card.title.clear();
        card.status = CardStatus::Claimed;
        card.seq = Seq(u64::MAX);
        card.claimed_by = Some(AgentId::NIL);
        let empty = FloorProjection {
            lsn: Lsn(u64::MAX),
            cards: vec![card.clone()],
            ..FloorProjection::default()
        };
        let overhead =
        serde_json::to_vec(&serde_json::json!({"type":"floor.snapshot", "projection":empty, "messages": [], "messages_lsn": u64::MAX.to_string(), "threads": [], "threads_lsn":u64::MAX.to_string()}))
            .unwrap()
            .len();
        let title_bytes = crate::floor_websocket::MAX_MESSAGE_BYTES
            - crate::floor::MAX_MAIL_BYTES
            - SNAPSHOT_SIBLING_BYTES
            - crate::floor::ARCHIVE_RESERVE_BYTES
            - crate::floor::QUESTION_HISTORY_RESERVE_BYTES
            - crate::floor::TRACE_RESERVE_BYTES
            - dudes_proto::MAX_ACTIVITY_BYTES
            - overhead
            - crate::floor_chat::CHAT_RESERVE_BYTES;
        card.title = "x".repeat(title_bytes);
        admit_cards(Vec::new(), vec![card.clone()]).unwrap();
        card.title.push('x');
        assert_eq!(
            admit_cards(Vec::new(), vec![card]).unwrap_err().code,
            ErrorCode::Refused.code()
        );
    }

    #[test]
    fn board_admission_reserves_a_later_live_kind_on_each_current_agent() {
        let agent: AgentView = serde_json::from_value(serde_json::json!({
            "id":"01ARZ3NDEKTSV4RRFFQ69G5FAV", "name":"Ada", "role":"builder",
            "department":"engineering", "provider":"claude", "status":"idle", "mode":"managed"
        }))
        .unwrap();
        let mut card = done_by(Actor::Agent {
            agent: AgentId::NIL,
        });
        card.title.clear();
        card.status = CardStatus::Claimed;
        card.seq = Seq(u64::MAX);
        card.claimed_by = Some(AgentId::NIL);
        let baseline = FloorProjection {
            lsn: Lsn(u64::MAX),
            agents: vec![agent.clone()],
            cards: vec![card.clone()],
            ..FloorProjection::default()
        };
        let overhead = serde_json::to_vec(&serde_json::json!({
            "type":"floor.snapshot", "projection":baseline, "messages": [], "messages_lsn": u64::MAX.to_string(), "threads": [], "threads_lsn":u64::MAX.to_string()
        }))
        .unwrap()
        .len();
        let title_bytes = crate::floor_websocket::MAX_MESSAGE_BYTES
            - crate::floor::MAX_MAIL_BYTES
            - SNAPSHOT_SIBLING_BYTES
            - crate::floor::ARCHIVE_RESERVE_BYTES
            - crate::floor::QUESTION_HISTORY_RESERVE_BYTES
            - crate::floor::TRACE_RESERVE_BYTES
            - dudes_proto::MAX_ACTIVITY_BYTES
            - crate::floor_chat::CHAT_RESERVE_BYTES
            - overhead;
        card.title = "x".repeat(title_bytes);
        assert_eq!(
            admit_cards(vec![agent.clone()], vec![card.clone()])
                .unwrap_err()
                .code,
            ErrorCode::Refused.code()
        );
        let mut max_activity_agent = agent.clone();
        max_activity_agent.live_activity = Some(LiveActivityView {
            kind: LiveActivityKind::UsingTool,
            at: Timestamp::from_unix_millis(i64::MIN),
            expires_in_ms: 60_000,
        });
        let added = serde_json::to_vec(&max_activity_agent).unwrap().len()
            - serde_json::to_vec(&agent).unwrap().len();
        card.title = "x".repeat(title_bytes - added);
        admit_cards(vec![agent], vec![card]).unwrap();
    }

    #[test]
    fn board_predicate_passes_only_an_operator_or_daemon_status() {
        let holder = Actor::Agent {
            agent: AgentId::from_parts(1, 2),
        };
        let sibling = Actor::Agent {
            agent: AgentId::from_parts(1, 4),
        };
        let peer = Actor::Peer { pid: 7, uid: None };
        let done = board(CardStatus::Done);
        assert_eq!(
            [holder, sibling, peer, Actor::System]
                .map(|by| predicate_result(&done_by(by), &done).0),
            [false, false, true, true]
        );
        assert!(!predicate_result(&done_by(peer), &board(CardStatus::Blocked)).0);
        let (_, evidence) = predicate_result(&done_by(sibling), &done);
        assert_eq!(
            evidence,
            Evidence::Card {
                card: CardId::from_parts(1, 3),
                status: CardStatus::Done,
                by: sibling,
            }
        );
    }
}
