//! The run-local agent-token mint.
//!
//! One token per agent per daemon run. The **plaintext lives here and nowhere else**: this module
//! is private, the map is a field of [`Supervisor`](crate::Supervisor), and nothing outside the
//! crate can reach a [`HookToken`] held in it. What the journal gets is the blake3 of the token,
//! carried by `Record::HookTokenMinted`; the plaintext is deliberately not journaled, which is
//! why a restart mints again and why every hash journaled for a live agent stays valid
//! (validity is set membership, never recency).
//!
//! **Two maps, not one, and the split is the durability requirement.** A token whose
//! `Persist{HookTokenMinted}` has been emitted but not acknowledged is not yet something a child
//! may be handed: a crash in that window would leave a live child holding a credential that is
//! not in the journal, so nothing could resolve its calls and it would look
//! exactly like a permissions bug. So [`Mint::draw`] puts the token in `pending`, and only
//! [`Mint::confirm`] — driven by the acknowledgement — moves it to `live`, which is the only map
//! [`Mint::live`] reads.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::rpc::hook::HookToken;
use dudes_types::AgentId;

use crate::host::{Rng, blake3_hex};
use crate::redact::Redactor;

/// The tokens this daemon run has drawn.
///
/// Not `Clone` and not `Debug`: a derived `Debug` would print the maps' *keys* safely and their
/// values through [`HookToken`]'s own redaction, but the type has no reason to be printed at all
/// and the derive is one edit away from a struct that does. Nothing here needs either.
pub(crate) struct Mint {
    /// Every plaintext drawn for an agent this run, live or not, until the agent retires: a child
    /// may still hold a token a later draw replaced, so it stays a secret for as long.
    drawn: BTreeMap<AgentId, BTreeSet<String>>,
    redactor: Redactor,
    /// Tokens whose `HookTokenMinted` is on the disk, by agent. The only ones a child may be
    /// handed, and one per agent: a later confirmed draw replaces an earlier one here while both
    /// hashes stay journaled and both stay valid (set membership, never recency).
    live: BTreeMap<AgentId, HookToken>,
    /// Tokens drawn this run whose record has been emitted and not yet acknowledged, **keyed by
    /// the hash that record carries** rather than by agent.
    ///
    /// **Several draws for one agent can be outstanding here at once, and nothing bounds it.** A
    /// mint is emitted lazily — from inside the acknowledgement of an *earlier* record, when
    /// `compose_pair` finds no live token — so a record emitted before the mint is necessarily
    /// still outstanding when the mint appears. The ordering contract the caller owes
    /// ([`Input::Persisted`](crate::Input::Persisted)) then *obliges* it to acknowledge that
    /// predecessor next, and that acknowledgement is what draws again. The contract bounds what
    /// may be acknowledged **after** a record; it cannot protect a lazily-emitted record from its
    /// own predecessors, so it is cited here for what it says and not for a bound it does not
    /// give.
    ///
    /// Keying by the hash is what makes that harmless: [`Mint::confirm`] is driven by the record
    /// that reached the disk and promotes the token *that record's hash* names, so no draw can
    /// displace another and no child is armed with a credential still in flight.
    pending: BTreeMap<String, (AgentId, HookToken)>,
}

impl Mint {
    pub(crate) fn with_redactor(redactor: Redactor) -> Self {
        Self {
            drawn: BTreeMap::new(),
            redactor,
            live: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    fn source(agent: AgentId) -> String {
        format!("agent-token:{agent}")
    }

    /// The agent retired: its tokens are no longer anything a child can be armed with, and they
    /// stop being a secret to hide.
    pub(crate) fn retire(&mut self, agent: AgentId) {
        self.live.remove(&agent);
        self.pending.retain(|_, (owner, _)| *owner != agent);
        self.drawn.remove(&agent);
        self.redactor.clear_source(&Self::source(agent));
    }

    /// This run's token for `agent`, when the record carrying its hash has been acknowledged.
    ///
    /// `None` covers both "never drawn" and "drawn, not yet durable", because the caller does the
    /// same thing in either case: emit a mint and park its launch under it.
    pub(crate) fn live(&self, agent: AgentId) -> Option<&HookToken> {
        self.live.get(&agent)
    }

    /// Draw a fresh token for `agent` and answer the hash the journal records.
    ///
    /// The hash is the key this draw is held under, so the answer is not only what the caller
    /// journals but the handle it will confirm with. The token itself never leaves this module by
    /// value — the caller gets a hex digest and, once [`Self::confirm`] has run, a borrow through
    /// [`Self::live`].
    pub(crate) fn draw<R: Rng>(&mut self, agent: AgentId, rng: &mut R) -> String {
        let token = rng.agent_token();
        let hash = blake3_hex(token.expose().as_bytes());
        let drawn = self.drawn.entry(agent).or_default();
        drawn.insert(token.expose().to_owned());
        self.redactor
            .set_source(&Self::source(agent), drawn.iter().cloned());
        self.pending.insert(hash.clone(), (agent, token));
        hash
    }

    /// The draw `token_hash` names is on the disk: it may now arm a child.
    ///
    /// Driven by the acknowledgement of the record carrying that hash, so the token promoted is
    /// the one that record made durable — never a later draw that happens to be outstanding for
    /// the same agent.
    ///
    /// A no-op when the hash is not pending, which is what replay is — `Input::Replayed` folds a
    /// `HookTokenMinted` from a previous run whose plaintext died with that daemon. `agent` is a
    /// cross-check rather than a lookup: the record and the draw must agree about whose credential
    /// this is, and promoting under a disagreeing id would put one agent's token in another's
    /// slot. Unreachable through `Supervisor::mint`, which passes one id to both.
    pub(crate) fn confirm(&mut self, agent: AgentId, token_hash: &str) {
        if let Some((minted_for, token)) = self.pending.remove(token_hash)
            && minted_for == agent
        {
            self.live.insert(agent, token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeRng;

    fn agent(n: u128) -> AgentId {
        AgentId::from_ulid(ulid::Ulid::from_parts(0, n))
    }

    /// A drawn token is not usable until it is confirmed, and confirming one agent's does not
    /// confirm another's. The window between the two is the "fsynced before the spawn" window.
    #[test]
    fn a_token_arms_a_child_only_once_its_record_is_confirmed() {
        let mut mint = Mint::with_redactor(Redactor::new());
        let mut rng = FakeRng::new();
        let (a, b) = (agent(1), agent(2));

        assert!(mint.live(a).is_none(), "nothing is drawn yet");
        let hash = mint.draw(a, &mut rng);
        assert_eq!(hash.len(), 64, "blake3 as lowercase hex");
        assert!(
            mint.live(a).is_none(),
            "a drawn token is not live until its record is acknowledged"
        );

        mint.draw(b, &mut rng);
        mint.confirm(a, &hash);
        assert!(mint.live(a).is_some(), "and it is live once it is");
        assert!(
            mint.live(b).is_none(),
            "confirming one agent's token confirms only that agent's"
        );
    }

    /// A token is a secret from the moment it is drawn, before its record is acknowledged, since
    /// the plaintext exists from then.
    #[test]
    fn a_pending_token_is_a_secret_before_it_is_confirmed() {
        let redactor = Redactor::new();
        let mut mint = Mint::with_redactor(redactor.clone());
        let mut rng = FakeRng::new();
        let a = agent(1);
        let hash = mint.draw(a, &mut rng);
        assert!(mint.live(a).is_none(), "a control: it is still pending");
        let pending = mint.pending[&hash].1.expose().to_owned();
        assert_eq!(redactor.scrub(&format!("t={pending}")), "t=(redacted)");
    }

    /// The hash is of the token that was drawn, and with two draws outstanding for one agent each
    /// acknowledgement promotes **its own** — not whichever draw came last.
    ///
    /// That is the shape `push_open_pty` produces when two `Attached` records are acknowledged in
    /// emission order before either mint is: the mint is emitted from inside an acknowledgement,
    /// so the second `Attached` is still outstanding when the first mint appears. Keyed by agent,
    /// the first mint's acknowledgement would promote the second draw — a child armed with a
    /// credential still in flight.
    #[test]
    fn each_confirm_promotes_the_draw_its_hash_names() {
        let mut mint = Mint::with_redactor(Redactor::new());
        let mut rng = FakeRng::new();
        let a = agent(1);

        let first = mint.draw(a, &mut rng);
        let second = mint.draw(a, &mut rng);
        assert_ne!(
            first, second,
            "a control: a second draw is a second credential"
        );
        assert!(
            mint.live(a).is_none(),
            "a control: two draws are outstanding and neither is live"
        );

        mint.confirm(a, &first);
        let promoted = mint.live(a).expect("confirmed").expose().to_owned();
        assert_eq!(
            first,
            blake3_hex(promoted.as_bytes()),
            "the live token is the one the acknowledged record made durable"
        );

        mint.confirm(a, &second);
        assert_eq!(
            second,
            blake3_hex(mint.live(a).expect("confirmed").expose().as_bytes()),
            "and the later acknowledgement then replaces it: one live token per agent, both \
             hashes journaled and both valid"
        );
    }

    /// Confirming a hash this run did not draw is inert: that is replay folding a previous run's
    /// `HookTokenMinted`, whose plaintext died with that daemon. So is confirming
    /// under an id that disagrees with the draw — the cross-check `confirm` documents, which
    /// drops the draw rather than promoting it into the wrong agent's slot.
    #[test]
    fn confirming_a_hash_this_run_did_not_draw_is_inert() {
        let mut mint = Mint::with_redactor(Redactor::new());
        let mut rng = FakeRng::new();
        let (a, b) = (agent(1), agent(2));

        mint.confirm(a, "a hash from a previous daemon run");
        assert!(mint.live(a).is_none());

        let hash = mint.draw(a, &mut rng);
        mint.confirm(b, &hash);
        assert!(mint.live(a).is_none(), "not promoted for the drawing agent");
        assert!(mint.live(b).is_none(), "and not for the confirming one");
    }
}
