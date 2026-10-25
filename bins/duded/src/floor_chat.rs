//! Bounded browser-note commands and their durable retry receipts.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_journal::Reader;
use dudes_proto::Record;
use dudes_proto::record::Hex32;
use dudes_types::{AgentId, GoalId, Lsn, TurnId};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Maximum normalized browser-note text length in UTF-8 bytes.
pub use dudes_proto::MAX_CHAT_TEXT_BYTES;
/// Maximum HTTP body length, checked before reading the body.
pub const MAX_CHAT_BODY_BYTES: usize = 16_384;
/// Complete Chat object and its sibling-key reservation when enabled.
pub const CHAT_RESERVE_BYTES: usize = dudes_proto::MAX_CHAT_BYTES;

/// The two browser-note actions in the closed browser authority contract.
///
/// This enum is exhaustive because every admitted action has a distinct parser and digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatVerb {
    /// Queue one normalized browser note.
    Send,
    /// Cancel an undelivered note or its ambiguous replay reissue.
    Cancel,
}

#[derive(Clone)]
pub(crate) enum ChatAction {
    Send {
        text: String,
        context_reset_turn: Option<TurnId>,
        /// The control identity the Floor's stop returned: the note goes in as the second step of
        /// stop and send, and the Floor's halt is lifted right after it.
        after_stop: Option<Lsn>,
    },
    Cancel {
        note_lsn: Lsn,
    },
}

/// One canonical command admitted by the browser Chat boundary.
#[derive(Clone)]
pub struct ChatRequest {
    pub(crate) request_id_digest: Hex32,
    pub(crate) request_wire: String,
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    pub(crate) action: ChatAction,
}

impl fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatRequest(<redacted>)")
    }
}

/// A successful durable append or a finite no-effect current-world refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatOutcome {
    /// The note is durable and queued.
    Queued {
        /// The request identity as the browser sent it.
        request_id: String,
        /// The destination agent.
        agent: AgentId,
        /// The note's durable identity.
        note_lsn: Lsn,
    },
    /// The note was cancelled before any dispatch began.
    Cancelled {
        /// The request identity as the browser sent it.
        request_id: String,
        /// The destination agent.
        agent: AgentId,
        /// The note's durable identity.
        note_lsn: Lsn,
    },
    /// Future replay dispatch was cancelled; prior delivery remains unknown.
    ReissueCancelled {
        /// The request identity as the browser sent it.
        request_id: String,
        /// The destination agent.
        agent: AgentId,
        /// The note's durable identity.
        note_lsn: Lsn,
    },
    /// A finite conversation needs explicit acknowledgement before a fresh context can run.
    ContextLost {
        /// Exact refused request identity.
        request_id: String,
        /// Destination agent.
        agent: AgentId,
        /// Exact turn whose vendor context was lost.
        turn_id: TurnId,
    },
    /// No journal effect was admitted.
    Refused {
        /// The request identity as the browser sent it.
        request_id: String,
        /// A fixed reason with no provider diagnostics or private text.
        reason: dudes_proto::ChatReason,
    },
}

/// A transport or writer refusal with no invented receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatRefusal {
    /// The admitting session is no longer valid.
    Forbidden,
    /// The same durable request identity has another action digest.
    Conflict,
    /// The append may be durable; retain the in-flight reservation.
    Unavailable,
    /// Nothing was appended; release the in-flight reservation.
    UnavailableNoEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendJson {
    request_id: String,
    agent: String,
    text: String,
    context_reset_turn: Option<String>,
    after_stop: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelJson {
    request_id: String,
    agent: String,
    note_lsn: String,
}

impl ChatRequest {
    /// Whether this is the second step of stop and send, which lifts a halt the Floor made.
    pub(crate) const fn follows_stop(&self) -> bool {
        matches!(
            self.action,
            ChatAction::Send {
                after_stop: Some(_),
                ..
            }
        )
    }

    /// Parse one bounded, canonical browser-note body without admitting a journal effect.
    ///
    /// The resulting command still requires an authenticated session and writer admission.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed objects, noncanonical identities or refused text bounds.
    pub fn parse(verb: ChatVerb, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CHAT_BODY_BYTES {
            bail!("Floor Chat request is too large");
        }
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor Chat request is not one JSON object");
        }
        let (request_wire, agent_wire, action, domain, field, value) = match verb {
            ChatVerb::Send => {
                let raw: SendJson =
                    serde_json::from_slice(bytes).context("invalid Floor Chat send JSON")?;
                let text = dudes_proto::normalized_chat_text(&raw.text)
                    .ok_or_else(|| anyhow!("Floor Chat text is outside its admitted bounds"))?
                    .to_owned();
                if raw.after_stop.is_some() && raw.context_reset_turn.is_some() {
                    bail!("a note after a stop cannot also reset a context");
                }
                let after_stop = raw
                    .after_stop
                    .as_deref()
                    .map(crate::floor_answer::decode_canonical_lsn)
                    .transpose()
                    .context("invalid Floor Chat after_stop")?;
                (
                    raw.request_id,
                    raw.agent,
                    ChatAction::Send {
                        text: text.clone(),
                        context_reset_turn: raw
                            .context_reset_turn
                            .as_deref()
                            .map(|value| {
                                let turn = TurnId::parse(value)
                                    .map_err(|_| anyhow!("invalid context reset turn"))?;
                                if turn.to_string() != value {
                                    bail!("noncanonical context reset turn");
                                }
                                Ok(turn)
                            })
                            .transpose()?,
                        after_stop,
                    },
                    "dudes.chat.send.v1",
                    "text",
                    text,
                )
            }
            ChatVerb::Cancel => {
                let raw: CancelJson =
                    serde_json::from_slice(bytes).context("invalid Floor Chat cancel JSON")?;
                let note_lsn = crate::floor_answer::decode_canonical_lsn(&raw.note_lsn)?;
                (
                    raw.request_id,
                    raw.agent,
                    ChatAction::Cancel { note_lsn },
                    "dudes.chat.cancel.v1",
                    "note_lsn",
                    raw.note_lsn,
                )
            }
        };
        let decoded = crate::floor_actions::decode_attempt(&request_wire)
            .context("invalid Floor Chat request id")?;
        let agent = AgentId::parse(&agent_wire).map_err(|_| anyhow!("invalid Floor Chat agent"))?;
        let reset = match &action {
            ChatAction::Send {
                context_reset_turn: Some(turn),
                ..
            } => Some(turn.to_string()),
            _ => None,
        };
        let after_stop = match &action {
            ChatAction::Send {
                after_stop: Some(lsn),
                ..
            } => Some(lsn.0.to_string()),
            _ => None,
        };
        let digest = match (&reset, &after_stop) {
            (Some(turn), _) => digest_fields(
                "dudes.chat.send.reset.v1",
                &[
                    ("agent", &agent_wire),
                    (field, &value),
                    ("context_reset_turn", turn),
                ],
            ),
            (None, Some(lsn)) => digest_fields(
                "dudes.chat.send.stop.v1",
                &[("agent", &agent_wire), (field, &value), ("after_stop", lsn)],
            ),
            (None, None) => request_digest(domain, &agent_wire, field, &value),
        };
        Ok(Self {
            request_id_digest: Hex32::new(Sha256::digest(decoded.bytes()).into()),
            request_wire,
            request_digest: digest,
            agent,
            action,
        })
    }
}

fn request_digest(domain: &str, agent: &str, field: &str, value: &str) -> Hex32 {
    digest_fields(domain, &[("agent", agent), (field, value)])
}

pub(crate) fn digest_fields(domain: &str, fields: &[(&str, &str)]) -> Hex32 {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    for (field, value) in fields {
        hasher.update(
            u16::try_from(field.len())
                .expect("static field name")
                .to_be_bytes(),
        );
        hasher.update(field.as_bytes());
        hasher.update(
            u32::try_from(value.len())
                .expect("bounded field")
                .to_be_bytes(),
        );
        hasher.update(value.as_bytes());
    }
    Hex32::new(hasher.finalize().into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatReceipt {
    Queued,
    Cancelled,
    ReissueCancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexedChat {
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    pub(crate) note_lsn: Lsn,
    pub(crate) receipt: ChatReceipt,
}

impl IndexedChat {
    pub(crate) fn outcome(self, request_id: String) -> ChatOutcome {
        match self.receipt {
            ChatReceipt::Queued => ChatOutcome::Queued {
                request_id,
                agent: self.agent,
                note_lsn: self.note_lsn,
            },
            ChatReceipt::Cancelled => ChatOutcome::Cancelled {
                request_id,
                agent: self.agent,
                note_lsn: self.note_lsn,
            },
            ChatReceipt::ReissueCancelled => ChatOutcome::ReissueCancelled {
                request_id,
                agent: self.agent,
                note_lsn: self.note_lsn,
            },
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ChatIndex {
    receipts: BTreeMap<Hex32, IndexedChat>,
}

impl ChatIndex {
    pub(crate) fn replay(reader: &Reader) -> Result<Self> {
        let mut index = Self::default();
        for item in reader
            .iter_from(Lsn::ZERO)
            .context("reading Floor Chat retry index")?
        {
            let (lsn, entry) = item.context("reading Floor Chat retry entry")?;
            index.apply(lsn, &entry.record)?;
        }
        Ok(index)
    }

    pub(crate) fn apply(&mut self, lsn: Lsn, record: &Record) -> Result<()> {
        let (browser, agent, note_lsn, receipt) = match record {
            Record::ConversationContextResetQueued { agent, browser, .. }
            | Record::ConversationMessageQueued { agent, browser, .. }
            | Record::BrowserNoteQueued { agent, browser, .. } => {
                (browser, *agent, lsn, ChatReceipt::Queued)
            }
            Record::BrowserNoteCancelled {
                agent,
                browser,
                note_lsn,
            }
            | Record::BrowserNoteIssuedCancelled {
                agent,
                browser,
                note_lsn,
                ..
            } => (browser, *agent, *note_lsn, ChatReceipt::Cancelled),
            Record::BrowserNoteReissueCancelled {
                agent,
                browser,
                note_lsn,
                ..
            } => (browser, *agent, *note_lsn, ChatReceipt::ReissueCancelled),
            _ => return Ok(()),
        };
        if self.receipts.contains_key(&browser.request_id_digest) {
            bail!("duplicate Floor Chat request in journal");
        }
        self.receipts.insert(
            browser.request_id_digest,
            IndexedChat {
                request_digest: browser.request_digest,
                agent,
                note_lsn,
                receipt,
            },
        );
        Ok(())
    }

    pub(crate) fn get(&self, request_id_digest: Hex32) -> Option<IndexedChat> {
        self.receipts.get(&request_id_digest).copied()
    }
}

pub(crate) fn pending_fits(
    view: &dudes_proto::ChatView,
    agent: AgentId,
    goal: Option<GoalId>,
    text: &str,
) -> Result<bool> {
    use dudes_proto::{ChatPendingState, ChatPendingView};
    let candidate = ChatPendingView {
        agent_id: agent,
        note_lsn: u64::MAX.to_string(),
        goal_id: goal,
        text: text.to_owned(),
        state: ChatPendingState::Queued,
        turn_id: None,
        cancellable: true,
        reason: None,
    };
    Ok(dudes_journal::admits_pending(&view.pending, &candidate)?)
}

pub(crate) fn validate_view(view: &dudes_proto::ChatView) -> Result<()> {
    use dudes_proto::{
        ChatPendingState, ChatTurnState, MAX_CHAT_CONTEXT, MAX_CHAT_CONTEXT_BYTES,
        MAX_CHAT_CONTEXT_PER_AGENT, MAX_CHAT_PENDING, MAX_CHAT_PENDING_BYTES,
        MAX_CHAT_PENDING_PER_AGENT, MAX_CHAT_TURN_BYTES, MAX_CHAT_TURN_BYTES_PER_AGENT,
        MAX_CHAT_TURNS, MAX_CHAT_TURNS_PER_AGENT,
    };
    if !view.enabled || view.max_text_bytes != MAX_CHAT_TEXT_BYTES {
        bail!("invalid Chat capability");
    }
    validate_rows(
        &view.pending,
        MAX_CHAT_PENDING,
        MAX_CHAT_PENDING_PER_AGENT,
        MAX_CHAT_PENDING_BYTES,
        None,
        |row| row.agent_id,
    )?;
    validate_rows(
        &view.turns,
        MAX_CHAT_TURNS,
        MAX_CHAT_TURNS_PER_AGENT,
        MAX_CHAT_TURN_BYTES,
        Some(MAX_CHAT_TURN_BYTES_PER_AGENT),
        |row| row.agent_id,
    )?;
    validate_rows(
        &view.context_sent,
        MAX_CHAT_CONTEXT,
        MAX_CHAT_CONTEXT_PER_AGENT,
        MAX_CHAT_CONTEXT_BYTES,
        None,
        |row| row.agent_id,
    )?;
    let mut pending = BTreeMap::new();
    for row in &view.pending {
        let note = crate::floor_answer::decode_canonical_lsn(&row.note_lsn)?;
        valid_text(&row.text)?;
        if (row.state == ChatPendingState::Queued) != row.turn_id.is_none() {
            bail!("Chat pending state has no matching turn identity");
        }
        if pending.insert(note, row).is_some() {
            bail!("duplicate Chat pending note");
        }
    }
    let mut turns = BTreeMap::new();
    let mut turn_ids = std::collections::BTreeSet::new();
    for row in &view.turns {
        let note = crate::floor_answer::decode_canonical_lsn(&row.note_lsn)?;
        valid_text(&row.text)?;
        if row
            .reply
            .as_ref()
            .is_some_and(|reply| reply.len() > dudes_proto::MAX_CHAT_REPLY_BYTES)
        {
            bail!("Chat final reply exceeds its bound");
        }
        if turns.insert(note, row).is_some() || !turn_ids.insert(row.turn_id) {
            bail!("duplicate Chat note turn");
        }
        if matches!(
            row.state,
            ChatTurnState::Reserved | ChatTurnState::DeliveryUncertain
        ) && !pending.contains_key(&note)
        {
            bail!("Chat unresolved turn has no pending reservation");
        }
        if row.truncated.is_some_and(|_| row.reply.is_none()) {
            bail!("Chat truncation claim has no matching reply");
        }
        if let Some(pending) = pending.get(&note)
            && (pending.agent_id != row.agent_id
                || pending.turn_id != Some(row.turn_id)
                || pending.text != row.text)
        {
            bail!("unmatched Chat pending turn");
        }
    }
    let mut contexts = std::collections::BTreeSet::new();
    for row in &view.context_sent {
        let note = crate::floor_answer::decode_canonical_lsn(&row.note_lsn)?;
        if !contexts.insert(note) || turns.contains_key(&note) || pending.contains_key(&note) {
            bail!("Chat context receipt is duplicated, pending or joined to a Managed turn");
        }
    }
    if serde_json::to_vec(view)?.len() + b",\"chat\":".len() > CHAT_RESERVE_BYTES {
        bail!("Chat exceeds the complete sibling reservation");
    }
    Ok(())
}

pub(crate) fn validate_current_agents(
    view: &dudes_proto::ChatView,
    agents: &[dudes_proto::AgentView],
) -> Result<()> {
    let current: std::collections::BTreeSet<_> = agents.iter().map(|agent| agent.id).collect();
    if view
        .pending
        .iter()
        .map(|row| row.agent_id)
        .chain(view.turns.iter().map(|row| row.agent_id))
        .chain(view.context_sent.iter().map(|row| row.agent_id))
        .any(|agent| !current.contains(&agent))
    {
        bail!("Chat row names an absent or retired agent");
    }
    Ok(())
}

pub(crate) fn retain_current_agents(
    view: &mut dudes_proto::ChatView,
    agents: &[dudes_proto::AgentView],
) {
    let current: std::collections::BTreeSet<_> = agents.iter().map(|agent| agent.id).collect();
    view.pending.retain(|row| current.contains(&row.agent_id));
    view.turns.retain(|row| current.contains(&row.agent_id));
    view.context_sent
        .retain(|row| current.contains(&row.agent_id));
}

fn valid_text(text: &str) -> Result<()> {
    if !(1..=MAX_CHAT_TEXT_BYTES).contains(&text.len())
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n'))
    {
        bail!("invalid Chat note text");
    }
    Ok(())
}

fn validate_rows<T: serde::Serialize>(
    rows: &[T],
    global_count: usize,
    agent_count: usize,
    global_bytes: usize,
    agent_bytes: Option<usize>,
    agent: impl Fn(&T) -> AgentId,
) -> Result<()> {
    if rows.len() > global_count || serde_json::to_vec(rows)?.len() > global_bytes {
        bail!("Chat array exceeds its global bound");
    }
    let mut agents: BTreeMap<AgentId, Vec<&T>> = BTreeMap::new();
    for row in rows {
        agents.entry(agent(row)).or_default().push(row);
    }
    for rows in agents.values() {
        if rows.len() > agent_count {
            bail!("Chat array exceeds its per-agent count");
        }
        if let Some(bound) = agent_bytes
            && serde_json::to_vec(rows)?.len() > bound
        {
            bail!("Chat array exceeds its per-agent bytes");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn send(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"request_id":REQUEST_ID,"agent":AGENT,"text":text}))
            .unwrap()
    }

    #[test]
    fn send_normalizes_only_ascii_edges_and_hashes_decoded_identity() {
        let request = ChatRequest::parse(ChatVerb::Send, &send(" \t\r\nmain\u{a0}\n ")).unwrap();
        let ChatAction::Send { text, .. } = request.action else {
            panic!("send")
        };
        assert_eq!(text, "main\u{a0}");
        assert_eq!(
            serde_json::to_value(request.request_id_digest).unwrap(),
            "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925"
        );
        assert_eq!(request.agent.to_string(), AGENT);
    }

    #[test]
    fn exact_object_and_canonical_identities_are_required() {
        let valid = String::from_utf8(send("hello")).unwrap();
        assert!(ChatRequest::parse(ChatVerb::Send, valid.as_bytes()).is_ok());
        for bad in [
            format!("[\"{REQUEST_ID}\",\"{AGENT}\",\"hello\"]"),
            valid.replace("\"text\":\"hello\"", "\"text\":{}"),
            valid.replace(
                "\"text\":\"hello\"",
                "\"text\":\"hello\",\"text\":\"again\"",
            ),
            valid.replace("\"text\":\"hello\"", "\"text\":\"hello\",\"other\":\"x\""),
            valid.replace(AGENT, &AGENT.to_lowercase()),
            valid.replace(REQUEST_ID, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB"),
            valid.replace("hello", "\\ud800"),
            format!("{valid} {{}}"),
        ] {
            assert!(
                ChatRequest::parse(ChatVerb::Send, bad.as_bytes()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn normalization_is_bounded_in_utf8_and_controls_are_refused() {
        assert!(ChatRequest::parse(ChatVerb::Send, &send(&"😀".repeat(1_024))).is_ok());
        assert!(ChatRequest::parse(ChatVerb::Send, &send("a\tb\nc")).is_ok());
        for bad in [
            "😀".repeat(1_025),
            " \t\r\n".into(),
            "a\rb".into(),
            "a\u{7f}b".into(),
            "a\u{85}b".into(),
        ] {
            assert!(ChatRequest::parse(ChatVerb::Send, &send(&bad)).is_err());
        }
        assert!(ChatRequest::parse(ChatVerb::Send, &vec![b' '; 16_385]).is_err());
    }

    #[test]
    fn cancel_accepts_only_canonical_positive_u64_positions() {
        for position in ["1", "18446744073709551615"] {
            let body = serde_json::to_vec(
                &serde_json::json!({"request_id":REQUEST_ID,"agent":AGENT,"note_lsn":position}),
            )
            .unwrap();
            let request = ChatRequest::parse(ChatVerb::Cancel, &body).unwrap();
            let ChatAction::Cancel { note_lsn } = request.action else {
                panic!("cancel")
            };
            assert_eq!(note_lsn.0.to_string(), position);
        }
        for position in ["0", "01", "+1", "-1", "1.0", "18446744073709551616"] {
            let body = serde_json::to_vec(
                &serde_json::json!({"request_id":REQUEST_ID,"agent":AGENT,"note_lsn":position}),
            )
            .unwrap();
            assert!(ChatRequest::parse(ChatVerb::Cancel, &body).is_err());
        }
    }

    #[test]
    fn action_digests_are_length_framed_and_request_identity_is_outside_them() {
        let request = ChatRequest::parse(ChatVerb::Send, &send(" \nmain \t")).unwrap();
        assert_eq!(
            serde_json::to_value(request.request_digest).unwrap(),
            "20fdda8ddfeec3f0fd592d89c1b1231add7d8ee50a32fac5dda81250c9ac0eff"
        );
        let mut other: serde_json::Value = serde_json::from_slice(&send("main")).unwrap();
        other["request_id"] = "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let other =
            ChatRequest::parse(ChatVerb::Send, &serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(other.request_digest, request.request_digest);
        assert_ne!(other.request_id_digest, request.request_id_digest);
        let cancel = serde_json::to_vec(
            &serde_json::json!({"request_id":REQUEST_ID,"agent":AGENT,"note_lsn":"1099511628288"}),
        )
        .unwrap();
        let cancel = ChatRequest::parse(ChatVerb::Cancel, &cancel).unwrap();
        assert_eq!(
            serde_json::to_value(cancel.request_digest).unwrap(),
            "61727a1cfe0ff5e4614e1a3aee941bc7da64e8bbee754e50b91cf680df2ee0d7"
        );
        assert_ne!(cancel.request_digest, request.request_digest);
    }

    fn pending(number: u64, agent: AgentId, text: &str) -> dudes_proto::ChatPendingView {
        dudes_proto::ChatPendingView {
            agent_id: agent,
            note_lsn: number.to_string(),
            goal_id: Some(GoalId::NIL),
            text: text.to_owned(),
            state: dudes_proto::ChatPendingState::Queued,
            turn_id: None,
            cancellable: true,
            reason: None,
        }
    }

    fn turn(number: u64, agent: AgentId) -> dudes_proto::ChatTurnView {
        dudes_proto::ChatTurnView {
            reissued: false,
            agent_id: agent,
            turn_id: dudes_types::TurnId::from_parts(1, u128::from(number)),
            note_lsn: number.to_string(),
            text: "\\".repeat(4_096),
            state: dudes_proto::ChatTurnState::Ended,
            reply: Some("\\".repeat(4_096)),
            truncated: None,
            failure: None,
            ran_models: Vec::new(),
        }
    }

    #[test]
    fn pending_counts_and_escaped_reservations_are_checked_before_append() {
        let mut view = dudes_proto::ChatView::empty();
        let agent = AgentId::parse(AGENT).unwrap();
        view.pending = (1..=20)
            .map(|number| pending(number, agent, "hello"))
            .collect();
        validate_view(&view).unwrap();
        assert!(!pending_fits(&view, agent, Some(GoalId::NIL), "hello").unwrap());
        view.pending.push(pending(21, agent, "hello"));
        assert!(validate_view(&view).is_err());
        view.pending = (1..=15)
            .map(|number| pending(number, agent, &"\\".repeat(4_096)))
            .collect();
        validate_view(&view).unwrap();
        assert!(!pending_fits(&view, agent, Some(GoalId::NIL), &"\\".repeat(4_096)).unwrap());
        view.pending.pop();
        assert!(pending_fits(&view, agent, Some(GoalId::NIL), &"\\".repeat(4_096)).unwrap());
    }

    #[test]
    fn final_history_counts_every_escaped_byte_per_agent_and_globally() {
        let mut view = dudes_proto::ChatView::empty();
        let agent = AgentId::parse(AGENT).unwrap();
        let mut row = turn(1, agent);
        row.reply = Some("\\".repeat(60_000));
        row.truncated = Some(false);
        view.turns.push(row.clone());
        validate_view(&view).unwrap();
        row.turn_id = dudes_types::TurnId::from_parts(1, 2);
        row.note_lsn = "2".into();
        view.turns.push(row.clone());
        assert!(validate_view(&view).is_err());
        view.turns = (1..=4)
            .map(|number| {
                let mut value = row.clone();
                value.agent_id = AgentId::from_parts(1, number);
                value.turn_id = dudes_types::TurnId::from_parts(1, number);
                value.note_lsn = number.to_string();
                value
            })
            .collect();
        validate_view(&view).unwrap();
        row.agent_id = AgentId::from_parts(1, 5);
        row.turn_id = dudes_types::TurnId::from_parts(1, 5);
        row.note_lsn = "5".into();
        view.turns.push(row);
        assert!(validate_view(&view).is_err());
    }

    #[test]
    fn view_refuses_noncanonical_positions_and_unmatched_note_turn_joins() {
        let mut view = dudes_proto::ChatView::empty();
        let agent = AgentId::parse(AGENT).unwrap();
        view.pending.push(pending(1, agent, "hello"));
        validate_view(&view).unwrap();
        view.pending[0].note_lsn = "01".into();
        assert!(validate_view(&view).is_err());
        view.pending[0].note_lsn = "1".into();
        view.pending[0].state = dudes_proto::ChatPendingState::Reserved;
        assert!(validate_view(&view).is_err());
        let mut row = turn(1, agent);
        row.text = "hello".into();
        row.reply = None;
        view.pending[0].turn_id = Some(row.turn_id);
        view.turns.push(row);
        validate_view(&view).unwrap();
        view.turns[0].text = "another note".into();
        assert!(validate_view(&view).is_err());
    }

    #[test]
    fn retirement_removes_every_public_corpus_without_changing_the_capability() {
        let mut view = dudes_proto::ChatView::empty();
        let agent = AgentId::parse(AGENT).unwrap();
        view.pending.push(pending(1, agent, "pending private note"));
        view.turns.push(turn(2, agent));
        view.context_sent.push(dudes_proto::ChatContextView {
            agent_id: agent,
            note_lsn: "3".into(),
            state: dudes_proto::ChatContextState::SentAsContext,
        });
        validate_view(&view).unwrap();
        assert!(validate_current_agents(&view, &[]).is_err());
        retain_current_agents(&mut view, &[]);
        assert_eq!(view, dudes_proto::ChatView::empty());
        validate_current_agents(&view, &[]).unwrap();
    }
}
