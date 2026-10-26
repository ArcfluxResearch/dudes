//! The bounded agent-to-agent metadata permitted on the Floor stream.

use dudes_types::{AgentId, MailId, ThreadId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) const POSITIVE_ORDER_PATTERN: &str = "^([1-9][0-9]{0,18}|1[0-7][0-9]{18}|18[0-3][0-9]{17}|184[0-3][0-9]{16}|1844[0-5][0-9]{15}|18446[0-6][0-9]{14}|184467[0-3][0-9]{13}|1844674[0-3][0-9]{12}|184467440[0-6][0-9]{10}|1844674407[0-2][0-9]{9}|18446744073[0-6][0-9]{8}|1844674407370[0-8][0-9]{6}|18446744073709[0-4][0-9]{5}|184467440737095[0-4][0-9]{4}|18446744073709550[0-9]{3}|18446744073709551[0-5][0-9]{2}|1844674407370955160[0-9]|1844674407370955161[0-4]|18446744073709551615)$";

/// Maximum rows in the complete Messages window.
pub const MAX_MESSAGES: usize = 128;
/// Maximum bytes of the complete serialized Messages array, including JSON escaping.
pub const MAX_MESSAGES_BYTES: usize = 65_536;

/// Maximum rows in the complete live thread graph.
pub const MAX_THREADS: usize = 128;
/// Maximum bytes of the complete serialized thread array.
pub const MAX_THREADS_BYTES: usize = 65_536;

/// Fixed membership of a thread with at least one visible message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadView {
    /// The journaled conversation identity.
    pub id: ThreadId,
    /// The opener's distinct members in canonical id order.
    #[schemars(length(min = 2, max = 8))]
    pub members: Vec<AgentId>,
}

/// A content-free delivery label with no notification count or bounce reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    /// The daemon accepted the mail durably.
    Durable,
    /// The recipient was notified.
    Notified,
    /// The recipient acknowledged the mail.
    Acked,
    /// The mail could not be delivered.
    Bounced,
}

/// The sender's declared purpose; neither choice grants authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageAct {
    /// Ordinary correspondence.
    Message,
    /// The sender expects an answer.
    Request,
}

/// An answer relationship derived from the current mail window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    /// The sender did not mark this as a request.
    NotExpected,
    /// A later, non-bounced return message exists in this thread.
    Answered,
    /// No answer exists and the recipient retired or delivery bounced.
    Undeliverable,
    /// No answer exists and this was the thread's twelfth message.
    ThreadFull,
    /// No qualifying answer is visible yet.
    Awaiting,
}

/// Maximum UTF-8 bytes in a displayed mail caption, including its ellipsis.
pub const MAX_MAIL_SUBJECT_BYTES: usize = 120;

/// Normalize a complete scrubbed subject without truncating it. The admission caller must scrub
/// again afterwards because removing format characters can join fragments of a known secret.
#[must_use]
pub fn normalize_mail_subject(text: &str) -> String {
    text.chars()
        .filter_map(|c| {
            if c.is_whitespace() {
                Some(' ')
            } else if c.is_control() || is_format(c) {
                None
            } else {
                Some(c)
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const fn is_format(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
        | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// Bound an admission-scrubbed subject for plain-text browser display.
#[must_use]
pub fn mail_subject_caption(subject: &str) -> String {
    let mut caption = normalize_mail_subject(subject);
    if caption.len() > MAX_MAIL_SUBJECT_BYTES {
        let mut end = MAX_MAIL_SUBJECT_BYTES - '…'.len_utf8();
        while !caption.is_char_boundary(end) {
            end -= 1;
        }
        caption.truncate(end);
        caption.push('…');
    }
    caption
}

/// Original journal metadata and the current fixed delivery label for agent mail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageView {
    /// The sender's mail identity.
    pub id: MailId,
    /// The structural sending agent.
    pub from: AgentId,
    /// The addressed agent.
    pub to: AgentId,
    /// The original Mail entry's journal acceptance time.
    pub at: Timestamp,
    /// The original Mail entry's creation order as canonical decimal text.
    #[schemars(regex(pattern = POSITIVE_ORDER_PATTERN))]
    pub lsn: String,
    /// The most recently recorded delivery state.
    pub state: MessageState,
    /// The journaled thread, absent for legacy messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
    /// A bounded, admission-scrubbed caption, absent for historical or unknown policies.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<String>")]
    pub subject: Option<String>,
    /// The permitted sender-declared act. Other acts remain hidden.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<MessageAct>")]
    pub act: Option<MessageAct>,
    /// The current answer relationship; never a persisted completion verdict.
    pub reply_status: ReplyStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_collapse_line_breaks_remove_controls_and_cut_on_utf8_boundaries() {
        assert_eq!(
            normalize_mail_subject(" a\t b\u{85}c\u{2028}d\u{202e}\0 "),
            "a b c d"
        );
        let caption = mail_subject_caption(&"界".repeat(50));
        assert_eq!(caption.len(), 120);
        assert!(caption.ends_with('…'));
    }

    #[test]
    fn typed_mail_pair_requires_combined_trimming() {
        let row = MessageView {
            subject: None,
            act: None,
            reply_status: ReplyStatus::NotExpected,
            id: MailId::from_parts(u64::MAX, u128::MAX),
            from: AgentId::from_parts(u64::MAX, u128::MAX),
            to: AgentId::from_parts(u64::MAX, u128::MAX - 1),
            at: Timestamp::from_unix_millis(i64::MIN),
            lsn: u64::MAX.to_string(),
            state: MessageState::Notified,
            thread: Some(ThreadId::from_parts(u64::MAX, u128::MAX)),
        };
        let thread = ThreadView {
            id: row.thread.unwrap(),
            members: (0..8)
                .rev()
                .map(|n| AgentId::from_parts(u64::MAX, u128::MAX - n))
                .collect(),
        };
        let messages = vec![row; MAX_MESSAGES];
        let threads = vec![thread; MAX_THREADS];
        let message_bytes = serde_json::to_vec(&messages).unwrap().len();
        let thread_bytes = serde_json::to_vec(&threads).unwrap().len();
        assert!(message_bytes < MAX_MESSAGES_BYTES);
        assert!(thread_bytes < MAX_THREADS_BYTES);
        assert!(message_bytes + thread_bytes > MAX_MESSAGES_BYTES);
    }
}
