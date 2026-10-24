//! A managed turn's standard output, read by the adapter of the agent's provider.
//!
//! [`Adapter::turn_reader`](crate::adapter::Adapter::turn_reader) hands the daemon one
//! [`TurnReader`] per turn. Every type here is in no vendor's vocabulary, so the daemon and the
//! supervisor never parse a vendor payload themselves.

use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::hash::{DefaultHasher, Hasher};

use dudes_proto::{ExitStatus, LiveActivityKind};
use dudes_types::{SessionId, Usage};

/// One managed turn's standard output, as far as it has been read. Pure: no I/O and no clock, so
/// an equal sequence of lines gives an equal result.
pub trait TurnReader: Send + fmt::Debug {
    /// Initial protocol writes, after the host has established ownership.
    fn start(&mut self) -> Vec<crate::protocol::ProtocolAction> {
        Vec::new()
    }
    /// Read one line of the child's standard output, without its newline.
    fn line(&mut self, text: &str) -> LineRead;
    /// Close the turn: the stream is at its end and `exit` is what the platform reported.
    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd;
}

/// What one line showed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LineRead {
    /// Bounded protocol actions for the host; a reader never writes a pipe itself.
    pub actions: Vec<crate::protocol::ProtocolAction>,
    /// Newly emitted assistant text. Concatenate fragments without inserting separators.
    pub text: Vec<TextBlock>,
    /// The session whose nonempty assistant text was omitted from this provisional preview.
    pub omitted_text: Option<SessionId>,
    /// What the vendor reports when the session opens or its credential, server, or plugin
    /// posture is observed later in the turn.
    pub session: Option<SessionObservation>,
    /// One fixed activity kind from a structured provider event, with no vendor content.
    pub activity: Option<LiveActivityKind>,
}

/// An assistant text fragment or completed block, beside the session its line named.
#[derive(Debug, PartialEq, Eq)]
pub struct TextBlock {
    /// The session the line named.
    pub session: SessionId,
    /// The text.
    pub text: String,
}

#[derive(Debug, Default)]
pub(crate) struct TextProgress {
    blocks: BTreeMap<usize, BlockProgress>,
    omitted_text: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum TextFragment<'a> {
    BlockStart(&'a str),
    Continuation(&'a str),
}

#[derive(Debug, Default)]
pub(crate) enum TextLayout {
    #[default]
    Empty,
    Written,
}

impl TextLayout {
    pub(crate) fn render(&mut self, fragment: TextFragment<'_>) -> String {
        let text = match fragment {
            TextFragment::BlockStart(text) if matches!(self, Self::Written) => {
                format!("\n{text}")
            }
            TextFragment::BlockStart(text) | TextFragment::Continuation(text) => text.to_owned(),
        };
        *self = Self::Written;
        text
    }
}

#[derive(Debug)]
enum BlockProgress {
    Open(EmittedText),
    Closed(EmittedText),
    Hidden,
}

#[derive(Debug, Default)]
struct EmittedText {
    bytes: usize,
    digest: DefaultHasher,
}

impl EmittedText {
    fn append<'a>(&mut self, text: &'a str) -> Option<TextFragment<'a>> {
        if text.is_empty() {
            return None;
        }
        let fragment = if self.bytes == 0 {
            TextFragment::BlockStart(text)
        } else {
            TextFragment::Continuation(text)
        };
        self.bytes = self.bytes.saturating_add(text.len());
        self.digest.write(text.as_bytes());
        Some(fragment)
    }

    fn remaining<'a>(&self, text: &'a str) -> Option<&'a str> {
        let prefix = text.get(..self.bytes)?;
        let mut digest = DefaultHasher::new();
        digest.write(prefix.as_bytes());
        (digest.finish() == self.digest.finish()).then(|| &text[self.bytes..])
    }
}

impl TextProgress {
    const MAX_TRACKED_BLOCKS: usize = 128;

    pub(crate) fn take_omitted_text(&mut self) -> bool {
        std::mem::take(&mut self.omitted_text)
    }

    fn block(&mut self, index: usize, initial: BlockProgress) -> Option<&mut BlockProgress> {
        let full = self.blocks.len() >= Self::MAX_TRACKED_BLOCKS;
        match self.blocks.entry(index) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(entry) if !full => Some(entry.insert(initial)),
            Entry::Vacant(_) => None,
        }
    }

    pub(crate) fn start(&mut self, index: usize, visible: bool) {
        let _ = self.block(
            index,
            if visible {
                BlockProgress::Open(EmittedText::default())
            } else {
                BlockProgress::Hidden
            },
        );
    }

    pub(crate) fn delta<'a>(&mut self, index: usize, text: &'a str) -> Option<TextFragment<'a>> {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.omitted_text |= self.blocks.len() >= Self::MAX_TRACKED_BLOCKS && !text.is_empty();
            return None;
        };
        let BlockProgress::Open(emitted) = block else {
            return None;
        };
        emitted.append(text)
    }

    pub(crate) fn stop(&mut self, index: usize) {
        if let Some(block) = self.blocks.get_mut(&index) {
            *block = match std::mem::replace(block, BlockProgress::Hidden) {
                BlockProgress::Open(emitted) => BlockProgress::Closed(emitted),
                other => other,
            };
        }
    }

    pub(crate) fn complete<'a>(&mut self, index: usize, text: &'a str) -> Option<TextFragment<'a>> {
        self.stop(index);
        let Some(block) = self.block(index, BlockProgress::Closed(EmittedText::default())) else {
            self.omitted_text |= !text.is_empty();
            return None;
        };
        let (BlockProgress::Open(emitted) | BlockProgress::Closed(emitted)) = block else {
            return None;
        };
        let remaining = emitted.remaining(text)?;
        emitted.append(remaining)
    }
}

/// What a managed turn's stream reported about its session: as it opened, or, for the tool
/// servers, when the vendor first named one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionObservation {
    /// The non-empty session id the vendor announced, when it announced one.
    pub session: Option<SessionId>,
    /// The credential the vendor said the turn runs on, when it said.
    pub credential: Option<Credential>,
    /// The tool servers the vendor named, by name: the whole list of an init, or a server a tool
    /// call or a startup report named for the first time in the turn.
    pub servers: Vec<String>,
    /// When [`Self::servers`] is the vendor's whole list for the turn, the servers on it the
    /// vendor reported connected. `None` when the observation names servers one at a time, so a
    /// declared server it leaves out is not thereby missing.
    pub connected: Option<Vec<String>>,
    /// The plugins the child loaded, by name when the vendor gave one. A plugin the vendor ships
    /// inside its own program is not listed.
    pub plugins: Vec<Option<String>>,
}

/// The credential a vendor reported for a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// No API key: a subscription login, a bearer token or a cloud provider.
    NoApiKey,
    /// An API key, from the source the vendor named.
    ApiKey(String),
}

/// A cost the vendor reported that cannot be charged.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// NaN, infinite, negative or beyond `u64` micro-dollars.
    #[error("vendor reported an unusable cost: {0}")]
    InvalidCost(f64),
    /// No total cost was reported and this model reported no cost of its own, so a sum of the
    /// others would under-charge. Invariant 4: a partial sum is never accounted.
    #[error("vendor reported no cost for model `{model}`")]
    MissingCost {
        /// The model the vendor named without a cost.
        model: String,
    },
}

/// A usage figure a stream reported, tagged with the span it covers. A reader holds one turn's
/// lines only, so it cannot turn a running total into one turn's share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopedUsage {
    /// This turn's own consumption, which is the turn's charge.
    Turn(Usage),
    /// The vendor session's running total at the end of this turn, every earlier turn on the
    /// session included. It is **not yet a turn charge**: see [`ScopedUsage::charge`].
    Session {
        /// The running total.
        total: Usage,
        /// This turn's own calls, as far as the stream showed them: a lower bound on the turn's
        /// usage. Zero when the stream shows no call of its own.
        calls: Usage,
    },
}

impl ScopedUsage {
    /// The turn's charge, given what its session's earlier turns were charged.
    ///
    /// A `Turn` figure is its own charge. A `Session` figure is charged what it adds to
    /// `charged`, so a turn that recorded no usage is paid for by the next one. It is charged
    /// whole, never less, when that difference cannot be the turn's: a count below `charged` is a
    /// vendor reset, and a difference below the turn's own `calls` in any count means the figure
    /// was not a running total on top of `charged` after all.
    #[must_use]
    pub fn charge(self, charged: Usage) -> Usage {
        match self {
            Self::Turn(usage) => usage,
            Self::Session { total, calls } => total
                .checked_sub(charged)
                .filter(|added| added.checked_sub(calls).is_some())
                .unwrap_or(total),
        }
    }
}

/// What one managed turn's stream and exit status together say.
#[derive(Debug)]
pub struct TurnEnd {
    /// One aggregate observation emitted at close, including completed use before abnormal EOF.
    pub final_observation: Option<SessionObservation>,
    /// The journalled outcome. Never `Killed` or `Vanished`: both belong to the
    /// supervisor.
    pub outcome: dudes_proto::TurnOutcome,
    /// The usage the stream reported, tagged with the span it covers. `None` when the stream
    /// reported none, or reported a cost that cannot be charged (see `usage_refused`).
    pub usage: Option<ScopedUsage>,
    /// What the stream attributed to each model it named, keys as reported.
    pub model_usage: BTreeMap<String, Usage>,
    /// The model's final message, when the turn completed and the vendor reported one.
    pub last_message: Option<String>,
    /// The completed reply before the legacy `last_message` limit, with explicit completeness.
    pub final_reply: Option<FinalReply>,
    /// Why no usage was accounted, when the stream reported a cost that could **not** be charged.
    pub usage_refused: Option<UsageError>,
    /// The non-empty session the vendor announced, when it announced one.
    pub session: Option<SessionId>,
    /// How many result lines were refused because they did not bind to this turn's session.
    pub attribution_refused: u64,
    /// How many lines the reader could not read at all. Non-zero is the signal that a payload
    /// shape has drifted.
    pub unreadable: u64,
}

/// Longest `last_message` or `reason` the journal keeps, in bytes, marker included. A reader
/// hands the vendor text over whole; the supervisor scrubs it and then cuts it with [`bound`].
pub const MAX_TEXT_BYTES: usize = 4096;

/// Longest completed conversation reply, in UTF-8 bytes including its truncation marker.
pub const MAX_FINAL_REPLY_BYTES: usize = 65_536;

/// The completed provider reply retained for a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalReply {
    /// The reply text, bounded by [`MAX_FINAL_REPLY_BYTES`].
    pub text: String,
    /// Whether the provider's completed reply fits in `text`.
    pub completeness: FinalReplyCompleteness,
}

/// Whether a completed conversation reply was shortened for storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalReplyCompleteness {
    /// All of the completed reply is retained.
    Complete,
    /// The reply was cut on a UTF-8 boundary and ends with `…`.
    Truncated,
}

impl FinalReply {
    /// The reply as the vendor wrote it, whole. The supervisor scrubs it and then cuts it with
    /// [`Self::bounded`], so a secret that straddles the limit is never half kept.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            completeness: FinalReplyCompleteness::Complete,
        }
    }

    /// This reply cut to [`MAX_FINAL_REPLY_BYTES`] on a UTF-8 boundary, marked truncated when
    /// anything was cut.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        if self.text.len() > MAX_FINAL_REPLY_BYTES {
            self.text = bound_to(&self.text, MAX_FINAL_REPLY_BYTES);
            self.completeness = FinalReplyCompleteness::Truncated;
        }
        self
    }
}

/// A stable, hand-written name for an exit status; never a `Debug` rendering.
pub(crate) fn exit_name(exit: ExitStatus) -> String {
    match (exit.code, exit.signal) {
        (_, Some(signal)) => format!("signal {signal}"),
        (Some(code), None) => format!("exit code {code}"),
        (None, None) => "exit status unknown".to_owned(),
    }
}

/// Cut `text` to at most [`MAX_TEXT_BYTES`] bytes on a char boundary, ending with `…` when
/// anything was cut.
#[must_use]
pub fn bound(text: &str) -> String {
    bound_to(text, MAX_TEXT_BYTES)
}

/// Cut `text` to at most `limit` bytes on a char boundary, ending with `…` when anything was cut.
#[must_use]
pub fn bound_to(text: &str, limit: usize) -> String {
    const MARKER: char = '…';
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit - MARKER.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = String::with_capacity(limit);
    cut.push_str(&text[..end]);
    cut.push(MARKER);
    cut
}

#[cfg(test)]
mod tests {
    use dudes_types::{Tokens, Usd};

    use super::*;

    #[test]
    fn sparse_block_starts_bound_state_and_keep_tracked_text() {
        let mut progress = TextProgress::default();
        let mut layout = TextLayout::default();
        progress.start(7, true);
        assert_eq!(layout.render(progress.delta(7, "first").unwrap()), "first");
        for index in 0..50_000 {
            let sparse = usize::MAX - index * 2;
            progress.start(sparse, index % 2 == 0);
            progress.stop(sparse);
        }
        assert_eq!(progress.blocks.len(), 128);
        progress.start(0, true);
        assert!(progress.delta(0, "untracked").is_none());
        assert!(progress.complete(0, "untracked").is_none());
        assert_eq!(progress.blocks.len(), 128);
        assert_eq!(
            layout.render(progress.complete(7, "first reply").unwrap()),
            " reply"
        );
        assert!(progress.complete(7, "first reply").is_none());
    }

    #[test]
    fn sparse_completions_bound_state_and_keep_existing_reconciliation() {
        let mut progress = TextProgress::default();
        let mut layout = TextLayout::default();
        assert_eq!(
            layout.render(progress.complete(7, "first").unwrap()),
            "first"
        );
        for index in 0..50_000 {
            let sparse = usize::MAX - index * 2;
            let _ = progress.complete(sparse, "");
        }
        assert_eq!(progress.blocks.len(), 128);
        assert!(progress.complete(0, "untracked").is_none());
        progress.start(0, true);
        assert!(progress.delta(0, "untracked").is_none());
        assert_eq!(progress.blocks.len(), 128);
        assert_eq!(
            layout.render(progress.complete(7, "first reply").unwrap()),
            " reply"
        );
        let mut next = TextProgress::default();
        next.start(0, true);
        assert_eq!(layout.render(next.delta(0, "next").unwrap()), "\nnext");
        assert_eq!(next.blocks.len(), 1);
    }

    fn usage(input: u64, usd: Option<u64>) -> Usage {
        Usage {
            input: Tokens::new(input),
            usd: usd.map(Usd::from_micros),
            ..Usage::ZERO
        }
    }

    fn session(total: Usage, calls: u64) -> ScopedUsage {
        ScopedUsage::Session {
            total,
            calls: usage(calls, None),
        }
    }

    #[test]
    fn a_session_total_is_charged_what_it_adds_and_a_turn_figure_as_given() {
        let charged = usage(100, Some(50));
        for (reported, expected) in [
            (
                ScopedUsage::Turn(usage(250, Some(80))),
                usage(250, Some(80)),
            ),
            (session(usage(250, Some(80)), 0), usage(150, Some(30))),
            (session(usage(40, Some(80)), 0), usage(40, Some(80))),
            (session(usage(250, Some(40)), 0), usage(250, Some(40))),
            (session(usage(250, None), 0), usage(250, None)),
            (session(usage(250, Some(80)), 150), usage(150, Some(30))),
            (session(usage(250, Some(80)), 151), usage(250, Some(80))),
        ] {
            assert_eq!(reported.charge(charged), expected, "{reported:?}");
        }
    }
}
