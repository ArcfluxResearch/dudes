//! The Floor's Memory tab reads: one page of an agent's memory, or one whole entry, pulled when the
//! operator asks. The loop answers from the journal-derived memory projection and writes nothing;
//! the connection thread cuts the page to its byte bounds and serializes it.
//!
//! Memory text crosses here as written, except that every secret the daemon knows is replaced
//! before it is sent. The browser draws it as inert text.
//! Nothing here is evidence, and none of it reaches the stream.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_journal::{
    MemoryEntry, MemoryPage, MemoryProjection, MemorySummary, MemorySummaryKind, Withheld,
    WithheldAppend,
};
use dudes_proto::PeerProvenance;
use dudes_rpc::conditioned;
use dudes_supervisor::{Redactor, World};
use dudes_types::{AgentId, Lsn};
use serde::{Deserialize, Serialize, Serializer};

/// Most bytes a page or entry request body may carry, checked before a byte of it is read.
pub const MAX_MEMORY_BODY_BYTES: usize = 128;
/// How soon one session may start another page or entry read.
pub const SESSION_INTERVAL: Duration = Duration::from_millis(250);
const PAGE_ENTRIES: usize = 32;
const PREVIEW_BYTES: usize = 2_048;
/// The page's `entries` array, brackets and separators included.
const ENTRIES_SECTION_BYTES: usize = 65_536;
const ENTRY_TEXT_BYTES: usize = 65_536;

/// Which read a routed path asks for. The path alone decides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryRoute {
    /// `POST /floor/memory/page`.
    Page,
    /// `POST /floor/memory/entry`.
    Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Page { before: Option<Lsn> },
    Entry { lsn: Lsn },
}

/// One decoded request: the agent and what to read of its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRequest {
    agent: AgentId,
    read: Read,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageJson {
    agent_id: String,
    before: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryJson {
    agent_id: String,
    lsn: String,
}

/// A position as the browser sends it: `0`, or up to 20 digits with no leading zero, within `u64`.
fn canonical_lsn(text: &str) -> Option<Lsn> {
    let digits = (1..=20).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    if digits && (text == "0" || !text.starts_with('0')) {
        text.parse().ok().map(Lsn)
    } else {
        None
    }
}

impl MemoryRequest {
    /// Decode one body for `route`: one flat object with exactly the route's members.
    ///
    /// # Errors
    ///
    /// A body over [`MAX_MEMORY_BODY_BYTES`], one that is not an object, one with an unknown,
    /// missing or repeated member, an agent id that does not parse, and a position that is not
    /// canonical decimal text.
    pub fn parse(route: MemoryRoute, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MEMORY_BODY_BYTES {
            bail!("Floor memory request is too large");
        }
        // The derived decoder also reads a struct from a positional array.
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor memory request is not one JSON object");
        }
        let invalid_lsn = || anyhow!("invalid Floor memory position");
        let (agent_id, read) = match route {
            MemoryRoute::Page => {
                let raw: PageJson =
                    serde_json::from_slice(bytes).context("invalid Floor memory request JSON")?;
                let before = match raw.before {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(text) => {
                        Some(canonical_lsn(&text).ok_or_else(invalid_lsn)?)
                    }
                    _ => return Err(invalid_lsn()),
                };
                (raw.agent_id, Read::Page { before })
            }
            MemoryRoute::Entry => {
                let raw: EntryJson =
                    serde_json::from_slice(bytes).context("invalid Floor memory request JSON")?;
                let lsn = canonical_lsn(&raw.lsn).ok_or_else(invalid_lsn)?;
                (raw.agent_id, Read::Entry { lsn })
            }
        };
        let agent = AgentId::parse(&agent_id).map_err(|_| anyhow!("invalid Floor memory agent"))?;
        Ok(Self { agent, read })
    }
}

/// What the loop read for one request.
#[derive(Debug)]
pub enum MemoryAnswer {
    /// The journal records no agent with this id.
    AgentUnknown,
    /// One page, with the summary and the withheld appends when it is the first.
    Page {
        /// The agent has retired.
        retired: bool,
        /// The daemon fenced memory work after an integrity failure.
        fenced: bool,
        /// The entries below the cursor.
        page: MemoryPage,
        /// The active summary, on a first page.
        summary: Option<MemorySummary>,
        /// The withheld appends, on a first page.
        withheld: Option<Withheld>,
    },
    /// The applied entry at the position, or `None` when the agent has none there.
    Entry(Option<MemoryEntry>),
}

/// Answer `request` from recorded values only. Reads; never writes.
#[must_use]
pub fn answer(
    world: &World,
    memory: &MemoryProjection,
    fenced: bool,
    redactor: &Redactor,
    request: MemoryRequest,
) -> MemoryAnswer {
    let agent = request.agent;
    let Some(row) = world.agents.get(&agent) else {
        return MemoryAnswer::AgentUnknown;
    };
    match request.read {
        Read::Page { before } => {
            let first = before.is_none();
            let mut page = memory.page(agent, before, PAGE_ENTRIES, PREVIEW_BYTES);
            for entry in &mut page.entries {
                scrub_entry(redactor, entry);
            }
            let mut summary = first.then(|| memory.summary(agent)).flatten();
            if let Some(summary) = &mut summary {
                summary.text = redactor.scrub(&summary.text).into_owned();
            }
            MemoryAnswer::Page {
                retired: row.retired,
                fenced,
                page,
                summary,
                withheld: first.then(|| memory.withheld(agent)),
            }
        }
        Read::Entry { lsn } => {
            let mut entry = memory.entry(agent, lsn, ENTRY_TEXT_BYTES);
            if let Some(entry) = &mut entry {
                scrub_entry(redactor, entry);
            }
            MemoryAnswer::Entry(entry)
        }
    }
}

fn scrub_entry(redactor: &Redactor, entry: &mut MemoryEntry) {
    entry.text = redactor.scrub(&entry.text).into_owned();
}

/// The per-session interval between the starts of two page or entry reads.
#[derive(Debug)]
pub struct MemoryReads {
    interval: Duration,
    last: Mutex<BTreeMap<[u8; 32], Instant>>,
}

impl MemoryReads {
    /// No session has read yet.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_interval(SESSION_INTERVAL)
    }

    /// No session has read yet, and each must wait `interval` between reads.
    #[must_use]
    pub const fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether `session` may start a read now. A refused start does not move its interval.
    pub(crate) fn begin(&self, session: [u8; 32]) -> bool {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        last.retain(|_, began| now.duration_since(*began) < self.interval);
        if last.contains_key(&session) {
            return false;
        }
        last.insert(session, now);
        true
    }
}

impl Default for MemoryReads {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Ok,
    AgentUnknown,
    NotFound,
    Busy,
}

/// A position as decimal text, so that JavaScript reads it exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LsnText(Lsn);

impl Serialize for LsnText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Writer {
    Agent,
    Operator,
    Unrecorded,
}

impl Writer {
    /// An applied append comes from the agent itself or from an operator with the authority to
    /// write; any other recorded value is one only a hand-made journal holds.
    fn of(agent: AgentId, provenance: Option<PeerProvenance>) -> Self {
        match provenance {
            Some(PeerProvenance::Agent(writer)) if writer == agent => Self::Agent,
            Some(PeerProvenance::NoRecordedAgent) => Self::Operator,
            _ => Self::Unrecorded,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WithheldWriter {
    Agent { agent: AgentId },
    OutsideAgents,
    Unresolved,
}

impl From<PeerProvenance> for WithheldWriter {
    fn from(provenance: PeerProvenance) -> Self {
        match provenance {
            PeerProvenance::Agent(agent) => Self::Agent { agent },
            PeerProvenance::NoRecordedAgent => Self::OutsideAgents,
            PeerProvenance::Unresolvable => Self::Unresolved,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    OverCeiling,
    ProvenanceMismatch,
    TargetMismatch,
    AuthorityUnavailable,
    RoleMismatch,
    Other,
}

impl Reason {
    fn of(reason: &str) -> Self {
        match reason {
            conditioned::TEXT_OVER_CEILING => Self::OverCeiling,
            conditioned::PROVENANCE_MISMATCH => Self::ProvenanceMismatch,
            conditioned::TARGET_MISMATCH => Self::TargetMismatch,
            conditioned::AUTHORITY_UNAVAILABLE => Self::AuthorityUnavailable,
            conditioned::ROLE_MISMATCH => Self::RoleMismatch,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Model,
    Excerpts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SummaryWire<'a> {
    lsn: LsnText,
    at: i64,
    kind: Kind,
    sections: u64,
    bytes: u64,
    text: &'a str,
}

impl<'a> SummaryWire<'a> {
    fn of(summary: &'a MemorySummary) -> Self {
        Self {
            lsn: LsnText(summary.lsn),
            at: summary.at.unix_millis(),
            kind: match summary.kind {
                MemorySummaryKind::Model => Kind::Model,
                MemorySummaryKind::Excerpts => Kind::Excerpts,
            },
            sections: summary.sections,
            bytes: summary.bytes,
            text: &summary.text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MarkerWire {
    lsn: LsnText,
    at: i64,
    writer: WithheldWriter,
    len: u64,
    reason: Reason,
}

impl From<&WithheldAppend> for MarkerWire {
    fn from(marker: &WithheldAppend) -> Self {
        Self {
            lsn: LsnText(marker.lsn),
            at: marker.at.unix_millis(),
            writer: marker.provenance.into(),
            len: marker.len,
            reason: Reason::of(&marker.reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WithheldWire {
    count: u64,
    newest: Vec<MarkerWire>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct EntryWire<'a> {
    lsn: LsnText,
    at: i64,
    writer: Writer,
    condensed: bool,
    len: u64,
    text: &'a str,
    cut: bool,
}

impl<'a> EntryWire<'a> {
    fn of(agent: AgentId, entry: &'a MemoryEntry) -> Self {
        Self {
            lsn: LsnText(entry.lsn),
            at: entry.at.unix_millis(),
            writer: Writer::of(agent, entry.provenance),
            condensed: entry.condensed,
            len: entry.len,
            text: &entry.text,
            cut: entry.cut,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PageReply<'a> {
    state: State,
    retired: bool,
    fenced: bool,
    count: u64,
    through: Option<LsnText>,
    summary: Option<SummaryWire<'a>>,
    withheld: Option<WithheldWire>,
    entries: Vec<EntryWire<'a>>,
    older: Option<LsnText>,
}

impl PageReply<'_> {
    const fn refused(state: State) -> Self {
        Self {
            state,
            retired: false,
            fenced: false,
            count: 0,
            through: None,
            summary: None,
            withheld: None,
            entries: Vec::new(),
            older: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct EntryReply<'a> {
    state: State,
    entry: Option<EntryWire<'a>>,
}

fn json(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).expect("a finite memory reply serializes")
}

/// Newest first, the entries whose exact serialized bytes fit the section, and the position the
/// next page starts below: the last entry kept, or none once the oldest entry is kept.
fn within_section(
    agent: AgentId,
    entries: &[MemoryEntry],
    more: bool,
) -> (Vec<EntryWire<'_>>, Option<LsnText>) {
    let mut used = "[]".len();
    let mut kept = Vec::new();
    for entry in entries {
        let wire = EntryWire::of(agent, entry);
        let size = json(&wire).len() + usize::from(!kept.is_empty());
        if used + size > ENTRIES_SECTION_BYTES {
            break;
        }
        used += size;
        kept.push(wire);
    }
    let older = (more || kept.len() < entries.len())
        .then(|| kept.last().map(|entry| entry.lsn))
        .flatten();
    (kept, older)
}

impl MemoryRequest {
    /// The reply body when this session asked again within its interval.
    #[must_use]
    pub fn busy(self) -> Vec<u8> {
        self.refused(State::Busy)
    }

    fn refused(self, state: State) -> Vec<u8> {
        match self.read {
            Read::Page { .. } => json(&PageReply::refused(state)),
            Read::Entry { .. } => json(&EntryReply { state, entry: None }),
        }
    }

    /// The reply body for the loop's `answer` to this request.
    #[must_use]
    pub fn reply(self, answer: &MemoryAnswer) -> Vec<u8> {
        match answer {
            MemoryAnswer::AgentUnknown => self.refused(State::AgentUnknown),
            MemoryAnswer::Page {
                retired,
                fenced,
                page,
                summary,
                withheld,
            } => {
                let (entries, older) = within_section(self.agent, &page.entries, page.more);
                json(&PageReply {
                    state: State::Ok,
                    retired: *retired,
                    fenced: *fenced,
                    count: page.count,
                    through: page.through.map(LsnText),
                    summary: summary.as_ref().map(SummaryWire::of),
                    withheld: withheld.as_ref().map(|withheld| WithheldWire {
                        count: withheld.count,
                        newest: withheld.newest.iter().map(MarkerWire::from).collect(),
                    }),
                    entries,
                    older,
                })
            }
            MemoryAnswer::Entry(None) => self.refused(State::NotFound),
            MemoryAnswer::Entry(Some(entry)) => json(&EntryReply {
                state: State::Ok,
                entry: Some(EntryWire::of(self.agent, entry)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "01J00000000000000000000000";

    fn agent() -> AgentId {
        AgentId::parse(AGENT).unwrap()
    }

    #[test]
    fn a_request_is_one_object_with_exactly_its_route_members() {
        let page = |body: &str| MemoryRequest::parse(MemoryRoute::Page, body.as_bytes());
        let entry = |body: &str| MemoryRequest::parse(MemoryRoute::Entry, body.as_bytes());
        assert_eq!(
            page(&format!(r#"{{"agent_id":"{AGENT}","before":null}}"#)).unwrap(),
            MemoryRequest {
                agent: agent(),
                read: Read::Page { before: None }
            }
        );
        assert_eq!(
            page(&format!(
                r#"{{"agent_id":"{AGENT}","before":"18446744073709551615"}}"#
            ))
            .unwrap()
            .read,
            Read::Page {
                before: Some(Lsn(u64::MAX))
            }
        );
        assert_eq!(
            entry(&format!(r#"{{"agent_id":"{AGENT}","lsn":"0"}}"#))
                .unwrap()
                .read,
            Read::Entry { lsn: Lsn(0) }
        );
        for bad in [
            format!(r#"{{"agent_id":"{AGENT}"}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":null,"extra":1}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":null,"before":null}}"#),
            format!(r#"["{AGENT}",null]"#),
            r#"{"agent_id":"not-an-id","before":null}"#.to_owned(),
            format!(r#"{{"agent_id":"{AGENT}","before":"01"}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":"-1"}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":"18446744073709551616"}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":""}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":7}}"#),
            format!(r#"{{"agent_id":"{AGENT}","lsn":"7"}}"#),
        ] {
            assert!(page(&bad).is_err(), "{bad}");
        }
        for bad in [
            format!(r#"{{"agent_id":"{AGENT}","lsn":null}}"#),
            format!(r#"{{"agent_id":"{AGENT}","lsn":"+7"}}"#),
            format!(r#"{{"agent_id":"{AGENT}","before":null}}"#),
            format!(r#"{{"agent_id":"{AGENT}","lsn":"7"}}{}"#, " ".repeat(128)),
        ] {
            assert!(entry(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn writers_and_reasons_map_to_fixed_labels() {
        let other = AgentId::parse("01J00000000000000000000001").unwrap();
        for (provenance, label) in [
            (Some(PeerProvenance::Agent(agent())), "\"agent\""),
            (Some(PeerProvenance::NoRecordedAgent), "\"operator\""),
            (None, "\"unrecorded\""),
            (Some(PeerProvenance::Agent(other)), "\"unrecorded\""),
            (Some(PeerProvenance::Unresolvable), "\"unrecorded\""),
        ] {
            assert_eq!(
                serde_json::to_string(&Writer::of(agent(), provenance)).unwrap(),
                label,
                "{provenance:?}"
            );
        }
        for (provenance, label) in [
            (
                PeerProvenance::Agent(other),
                r#"{"kind":"agent","agent":"01J00000000000000000000001"}"#,
            ),
            (
                PeerProvenance::NoRecordedAgent,
                r#"{"kind":"outside_agents"}"#,
            ),
            (PeerProvenance::Unresolvable, r#"{"kind":"unresolved"}"#),
        ] {
            assert_eq!(
                serde_json::to_string(&WithheldWriter::from(provenance)).unwrap(),
                label
            );
        }
        for (reason, label) in [
            ("memory text exceeds the append ceiling", "\"over_ceiling\""),
            (
                "peer provenance does not satisfy the effect",
                "\"provenance_mismatch\"",
            ),
            (
                "resolved agent does not own the target",
                "\"target_mismatch\"",
            ),
            (
                "operator authority unavailable",
                "\"authority_unavailable\"",
            ),
            (
                "resolved role does not satisfy the effect",
                "\"role_mismatch\"",
            ),
            ("decoded payload exceeds the method ceiling", "\"other\""),
        ] {
            assert_eq!(serde_json::to_string(&Reason::of(reason)).unwrap(), label);
        }
    }

    /// The Floor's decoder test decodes these same bytes.
    #[test]
    fn a_page_serializes_to_the_literal_the_floor_decodes() {
        let reply = PageReply {
            state: State::Ok,
            retired: false,
            fenced: false,
            count: 2,
            through: Some(LsnText(Lsn(7))),
            summary: Some(SummaryWire {
                lsn: LsnText(Lsn(9)),
                at: 1_787_394_600_000,
                kind: Kind::Excerpts,
                sections: 1,
                bytes: 5,
                text: "[Condensed memory: exact excerpts; 0 sections omitted]\n[append 7 bytes 0..5]\nfirst",
            }),
            withheld: Some(WithheldWire {
                count: 1,
                newest: vec![MarkerWire {
                    lsn: LsnText(Lsn(11)),
                    at: 1_787_394_600_002,
                    writer: WithheldWriter::OutsideAgents,
                    len: 3,
                    reason: Reason::AuthorityUnavailable,
                }],
            }),
            entries: vec![
                EntryWire {
                    lsn: LsnText(Lsn(10)),
                    at: 1_787_394_600_001,
                    writer: Writer::Agent,
                    condensed: false,
                    len: 6,
                    text: "second",
                    cut: false,
                },
                EntryWire {
                    lsn: LsnText(Lsn(7)),
                    at: 1_787_394_600_000,
                    writer: Writer::Operator,
                    condensed: true,
                    len: 5,
                    text: "first",
                    cut: false,
                },
            ],
            older: None,
        };
        assert_eq!(
            String::from_utf8(json(&reply)).unwrap(),
            r#"{"state":"ok","retired":false,"fenced":false,"count":2,"through":"7","summary":{"lsn":"9","at":1787394600000,"kind":"excerpts","sections":1,"bytes":5,"text":"[Condensed memory: exact excerpts; 0 sections omitted]\n[append 7 bytes 0..5]\nfirst"},"withheld":{"count":1,"newest":[{"lsn":"11","at":1787394600002,"writer":{"kind":"outside_agents"},"len":3,"reason":"authority_unavailable"}]},"entries":[{"lsn":"10","at":1787394600001,"writer":"agent","condensed":false,"len":6,"text":"second","cut":false},{"lsn":"7","at":1787394600000,"writer":"operator","condensed":true,"len":5,"text":"first","cut":false}],"older":null}"#
        );
        assert_eq!(
            String::from_utf8(
                MemoryRequest {
                    agent: agent(),
                    read: Read::Entry { lsn: Lsn(3) }
                }
                .busy()
            )
            .unwrap(),
            r#"{"state":"busy","entry":null}"#
        );
        assert_eq!(
            String::from_utf8(
                MemoryRequest {
                    agent: agent(),
                    read: Read::Page { before: None }
                }
                .busy()
            )
            .unwrap(),
            r#"{"state":"busy","retired":false,"fenced":false,"count":0,"through":null,"summary":null,"withheld":null,"entries":[],"older":null}"#
        );
    }

    #[test]
    fn a_session_waits_out_its_interval_and_others_do_not() {
        let reads = MemoryReads::new();
        assert!(reads.begin([1; 32]));
        assert!(
            !reads.begin([1; 32]),
            "the same session inside its interval"
        );
        assert!(reads.begin([2; 32]), "another session at once");
        std::thread::sleep(SESSION_INTERVAL + Duration::from_millis(20));
        assert!(reads.begin([1; 32]), "the same session after its interval");
    }
}
