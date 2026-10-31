//! The one rule for when a request has been answered.

use dudes_types::AgentId;

use crate::MailAct;

/// The facts about one mail that decide whether it answers a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailFact {
    /// The sending agent.
    pub from: AgentId,
    /// The receiving agent.
    pub to: AgentId,
    /// What the mail asks or does.
    pub act: MailAct,
    /// Whether the mail was bounced and never reached its recipient.
    pub bounced: bool,
}

/// The position of the mail that answers the request at `request`, given one thread's mail in
/// order.
///
/// The answer is the first later mail from the request's recipient back to its sender that is a
/// `Message` or a `Request` and is not bounced. An `Ack` or a `Card` is not an answer. A mail
/// at `request` that is not a `Request`, or a position outside the thread, has no answer.
#[must_use]
pub fn request_answer(thread: &[MailFact], request: usize) -> Option<usize> {
    let asked = thread
        .get(request)
        .filter(|mail| mail.act == MailAct::Request)?;
    let later = request + 1;
    thread[later..]
        .iter()
        .position(|mail| {
            mail.from == asked.to
                && mail.to == asked.from
                && matches!(mail.act, MailAct::Message | MailAct::Request)
                && !mail.bounced
        })
        .map(|offset| later + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(n: u8) -> AgentId {
        AgentId::from_parts(1, u128::from(n))
    }

    fn mail(from: u8, to: u8, act: MailAct) -> MailFact {
        MailFact {
            from: agent(from),
            to: agent(to),
            act,
            bounced: false,
        }
    }

    #[test]
    fn an_ack_does_not_answer_but_the_message_after_it_does() {
        let thread = [
            mail(1, 2, MailAct::Request),
            mail(2, 1, MailAct::Ack),
            mail(2, 1, MailAct::Card),
        ];
        assert_eq!(request_answer(&thread, 0), None);
        let thread = [
            mail(1, 2, MailAct::Request),
            mail(2, 1, MailAct::Ack),
            mail(2, 1, MailAct::Message),
            mail(2, 1, MailAct::Request),
        ];
        assert_eq!(request_answer(&thread, 0), Some(2));
    }

    #[test]
    fn a_request_from_the_recipient_answers() {
        let thread = [mail(1, 2, MailAct::Request), mail(2, 1, MailAct::Request)];
        assert_eq!(request_answer(&thread, 0), Some(1));
    }

    #[test]
    fn bounced_third_party_earlier_and_same_direction_mail_do_not_answer() {
        let mut bounced = mail(2, 1, MailAct::Message);
        bounced.bounced = true;
        let thread = [
            mail(2, 1, MailAct::Message),
            mail(1, 2, MailAct::Request),
            bounced,
            mail(3, 1, MailAct::Message),
            mail(1, 2, MailAct::Message),
        ];
        assert_eq!(request_answer(&thread, 1), None);
        assert_eq!(request_answer(&thread, 0), None);
        assert_eq!(request_answer(&thread, 9), None);
    }
}
