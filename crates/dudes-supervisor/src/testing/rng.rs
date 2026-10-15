use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use dudes_proto::Entry;
use dudes_proto::rpc::hook::{HOOK_TOKEN_LEN, HookToken};
use dudes_types::{AgentId, WorktreeId};
use serde_json::Value;
use ulid::Ulid;

use crate::{Rng, blake3_hex, session_id_from_ulid};

const MAX_COUNTER: u128 = (1 << 80) - 1;

/// A deterministic counter that excludes previously recorded output values.
#[derive(Clone, Default)]
pub struct FakeRng {
    counter: u128,
    occupied: BTreeSet<String>,
    tokens: OnceLock<FakeTokens>,
}

impl fmt::Debug for FakeRng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeRng")
            .field("counter", &self.counter)
            .field("occupied", &self.occupied.len())
            .finish_non_exhaustive()
    }
}

/// An observer of the tokens actually minted by one fixture's generator.
#[derive(Clone, Default)]
pub struct FakeTokens(Arc<Mutex<BTreeMap<String, String>>>);

impl fmt::Debug for FakeTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeTokens")
            .field("count", &self.0.lock().expect("fixture token book").len())
            .finish_non_exhaustive()
    }
}

impl FakeTokens {
    /// Retrieve a minted token by its recorded hash.
    ///
    /// # Panics
    ///
    /// Panics if this observer has not seen the hash being minted.
    #[must_use]
    pub fn token_of(&self, hash: &str) -> HookToken {
        let text = self
            .0
            .lock()
            .expect("fixture token book")
            .get(hash)
            .cloned()
            .expect("the fixture minted this token hash");
        assert_eq!(blake3_hex(text.as_bytes()), hash);
        HookToken::new(text).expect("the fixture recorded a well-formed token")
    }
}

impl FakeRng {
    /// A generator that has minted nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counter: 0,
            occupied: BTreeSet::new(),
            tokens: OnceLock::new(),
        }
    }

    /// The last consumed counter, including candidates excluded by reservations.
    #[must_use]
    pub const fn drawn(&self) -> u128 {
        self.counter
    }

    /// Observe future token draws without sharing the allocation counter.
    #[must_use]
    pub fn tokens(&self) -> FakeTokens {
        self.tokens.get_or_init(FakeTokens::default).clone()
    }

    /// Exclude all complete string values and map keys in the durable entries.
    ///
    /// # Errors
    ///
    /// Returns a serialization error without changing the generator.
    pub fn reserve_entries(&mut self, entries: &[Entry]) -> Result<(), serde_json::Error> {
        let mut occupied = BTreeSet::new();
        for entry in entries {
            collect_text(serde_json::to_value(entry)?, &mut occupied);
        }
        self.reserve_text(occupied);
        Ok(())
    }

    pub(super) fn after_draws(counter: u128) -> Self {
        Self {
            counter,
            ..Self::new()
        }
    }

    fn reserve_text(&mut self, occupied: BTreeSet<String>) {
        self.counter = occupied
            .iter()
            .filter_map(|text| invertible_counter(text))
            .fold(self.counter, u128::max);
        self.occupied.extend(occupied);
    }

    fn next_counter(&mut self) -> u128 {
        self.counter = self
            .counter
            .checked_add(1)
            .filter(|next| *next <= MAX_COUNTER)
            .expect("fake RNG exhausted its 80-bit counter");
        self.counter
    }
}

impl Rng for FakeRng {
    fn ulid(&mut self) -> Ulid {
        loop {
            let id = Ulid::from_parts(0, self.next_counter());
            let mut bytes = id.to_bytes();
            bytes.reverse();
            let outputs = [
                id.to_string(),
                WorktreeId::from_bytes(bytes).to_string(),
                session_id_from_ulid(id).to_string(),
            ];
            if outputs.iter().any(|text| self.occupied.contains(text)) {
                continue;
            }
            self.occupied.extend(outputs);
            return id;
        }
    }

    fn agent_token(&mut self) -> HookToken {
        loop {
            let counter = self.next_counter();
            let text = format!("{counter:A>HOOK_TOKEN_LEN$}");
            let hash = blake3_hex(text.as_bytes());
            if self.occupied.contains(&text) || self.occupied.contains(&hash) {
                continue;
            }
            self.occupied.extend([text.clone(), hash.clone()]);
            let token = HookToken::new(text).expect("a counter padded with 'A' is base64url");
            self.tokens()
                .0
                .lock()
                .expect("fixture token book")
                .insert(hash, token.expose().to_owned());
            return token;
        }
    }
}

fn collect_text(value: Value, occupied: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            occupied.insert(text);
        }
        Value::Array(values) => {
            for value in values {
                collect_text(value, occupied);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                occupied.insert(key);
                collect_text(value, occupied);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn invertible_counter(text: &str) -> Option<u128> {
    if let Ok(id) = AgentId::parse(text) {
        let bytes = id.as_ulid().to_bytes();
        return (bytes[..6] == [0; 6]).then(|| u128::from_be_bytes(bytes));
    }
    let mut bytes = WorktreeId::parse(text).ok()?.to_bytes();
    bytes.reverse();
    (bytes[..6] == [0; 6]).then(|| u128::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reserve(rng: &mut FakeRng, value: Value) {
        let mut occupied = BTreeSet::new();
        collect_text(value, &mut occupied);
        rng.reserve_text(occupied);
    }

    #[test]
    fn empty_history_preserves_the_literal_fresh_sequence() {
        let mut rng = const { FakeRng::new() };
        rng.reserve_entries(&[]).unwrap();
        assert_eq!(rng.ulid().to_string(), "00000000000000000000000001");
        assert_eq!(
            rng.agent_token().expose(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2"
        );
        assert_eq!(rng.drawn(), 2);
    }

    #[test]
    fn nested_values_and_keys_raise_the_floor_without_losing_predraws() {
        let high = Ulid::from_parts(0, 700).to_string();
        let mut rng = FakeRng {
            counter: 500,
            ..FakeRng::new()
        };
        reserve(&mut rng, json!({"nested":[{high: "task"}]}));
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 701));
        reserve(&mut rng, json!({"old":Ulid::from_parts(0, 3).to_string()}));
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 702));
    }

    #[test]
    fn reversed_worktrees_are_inverted_before_raising_the_floor() {
        let mut bytes = Ulid::from_parts(0, 901).to_bytes();
        bytes.reverse();
        let mut rng = FakeRng::new();
        reserve(&mut rng, json!({"lease":WorktreeId::from_bytes(bytes)}));
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 902));
    }

    #[test]
    fn session_reservations_compare_the_real_lossy_encoding() {
        for lost_bit in [62, 76] {
            let mut rng = FakeRng {
                counter: (1 << lost_bit) + 6,
                ..FakeRng::new()
            };
            let session = session_id_from_ulid(Ulid::from_parts(0, 7));
            reserve(&mut rng, json!({"reissued_session":session}));
            assert_eq!(rng.ulid(), Ulid::from_parts(0, (1 << lost_bit) + 8));
        }
    }

    #[test]
    fn the_same_run_also_reserves_session_aliases() {
        let mut rng = FakeRng {
            counter: 6,
            ..FakeRng::new()
        };
        let old = session_id_from_ulid(rng.ulid());
        rng.counter = (1 << 62) + 6;
        let next = rng.ulid();
        assert_eq!(next, Ulid::from_parts(0, (1 << 62) + 8));
        assert_ne!(session_id_from_ulid(next), old);
    }

    #[test]
    fn opaque_sessions_do_not_become_counter_floors() {
        let mut rng = FakeRng::new();
        reserve(
            &mut rng,
            json!([
                "opaque-provider-session",
                "fffffffe-ffff-4fff-bfff-ffffffffffff"
            ]),
        );
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 1));
    }

    #[test]
    fn token_only_history_excludes_the_exact_hash() {
        let mut old = FakeRng {
            counter: 256,
            ..FakeRng::new()
        };
        let token = old.agent_token();
        let mut rng = FakeRng {
            counter: 256,
            ..FakeRng::new()
        };
        reserve(
            &mut rng,
            json!({"hash":blake3_hex(token.expose().as_bytes())}),
        );
        assert_ne!(rng.agent_token(), token);
        assert_eq!(rng.drawn(), 258);
    }

    #[test]
    fn exact_token_observation_has_no_search_window() {
        for counter in [256, 1 << 70] {
            let mut rng = FakeRng {
                counter,
                ..FakeRng::new()
            };
            let tokens = rng.tokens();
            let token = rng.agent_token();
            assert_eq!(
                tokens.token_of(&blake3_hex(token.expose().as_bytes())),
                token
            );
        }
    }

    #[test]
    #[should_panic(expected = "the fixture minted this token hash")]
    fn token_observation_refuses_unknown_hashes() {
        let _ = FakeTokens::default().token_of("unknown");
    }

    #[test]
    fn debug_does_not_expose_reserved_text_or_tokens() {
        let mut rng = FakeRng::new();
        reserve(&mut rng, json!({"task":"private fixture text"}));
        let token = rng.agent_token();
        let debug = format!("{rng:?} {:?}", rng.tokens());
        assert!(!debug.contains(token.expose()));
        assert!(!debug.contains("private fixture text"));
        assert!(!debug.contains(&blake3_hex(token.expose().as_bytes())));
    }

    #[test]
    fn preparation_is_idempotent_and_preserves_consumed_draws() {
        let mut once = FakeRng {
            counter: 800,
            ..FakeRng::new()
        };
        let mut twice = once.clone();
        let value = json!({"identity":Ulid::from_parts(0, 700).to_string()});
        reserve(&mut once, value.clone());
        reserve(&mut twice, value.clone());
        reserve(&mut twice, value);
        assert_eq!(once.ulid(), twice.ulid());
        assert_eq!(once.drawn(), 801);
    }

    #[test]
    fn a_later_serialization_error_leaves_earlier_reservations_unapplied() {
        use dudes_proto::{ExhaustedReason, LoopDecision, Record};
        use dudes_types::Timestamp;
        let mut rng = FakeRng::new();
        let known = rng.ulid();
        let before = rng.drawn();
        let entries = [
            Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::GoalCleared {
                    agent: AgentId::NIL,
                    goal: dudes_types::GoalId::from_parts(0, 900),
                },
            },
            Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::LoopDecided {
                    agent: AgentId::NIL,
                    input_digest: String::new(),
                    decision: LoopDecision::Exhausted {
                        reason: ExhaustedReason::MailWakeLimit,
                    },
                    gate_trace: Vec::new(),
                },
            },
        ];
        assert!(rng.reserve_entries(&entries).is_err());
        assert_eq!(rng.drawn(), before);
        assert_eq!(rng.occupied.len(), 3);
        assert!(rng.occupied.contains(&known.to_string()));
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 2));
    }

    fn assert_record_floor(record: dudes_proto::Record) {
        let mut rng = FakeRng::new();
        rng.reserve_entries(&[Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        }])
        .unwrap();
        assert_eq!(rng.ulid(), Ulid::from_parts(0, 901));
    }

    #[test]
    fn retired_agents_cleared_goals_removed_tickets_and_mail_keep_their_ids() {
        use dudes_proto::{Actor, Record};
        use dudes_types::{CardId, GoalId, MailId, MailState, Seq};
        assert_record_floor(Record::AgentRetired {
            id: AgentId::from_parts(0, 900),
            reason: "retired".into(),
            browser_retire: None,
        });
        assert_record_floor(Record::GoalCleared {
            agent: AgentId::NIL,
            goal: GoalId::from_parts(0, 900),
        });
        assert_record_floor(Record::TicketRemoved {
            card: CardId::from_parts(0, 900),
            by: Actor::System,
            expected_seq: Seq(1),
        });
        assert_record_floor(Record::MailState {
            id: MailId::from_parts(0, 900),
            state: MailState::Acked,
        });
    }

    #[test]
    fn ordinary_ticket_and_embedded_onboarding_goals_use_the_serialized_graph() {
        use dudes_proto::Record;
        use dudes_proto::record::{BrowserOnboardingHire, Hex32};
        use dudes_types::{CardId, GoalId, Seq};
        let (mut entries, agent, _) = super::super::journal_with_issued_turn(None, "task");
        let Record::GoalSet { goal, .. } = &mut entries[1].record else {
            unreachable!()
        };
        goal.id = GoalId::from_parts(0, 900);
        let goal = goal.clone();
        assert_record_floor(entries[1].record.clone());
        assert_record_floor(Record::TicketStarted {
            card: CardId::NIL,
            agent,
            goal: goal.clone(),
            expected_seq: Seq(1),
        });
        let Record::AgentHired {
            browser_onboarding, ..
        } = &mut entries[0].record
        else {
            unreachable!()
        };
        *browser_onboarding = Some(Box::new(BrowserOnboardingHire {
            attempt_id: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            profile_id: "fixture".into(),
            session_digest: Hex32::new([3; 32]),
            intended_goal: goal,
        }));
        assert_record_floor(entries[0].record.clone());
    }

    #[test]
    fn prospective_owners_reserve_turns_before_an_issue_exists() {
        use dudes_proto::{InstructionObservationOwner, Record};
        let mut harness = super::super::Harness::new();
        harness.hire_with_goal(&["/usr/bin/false"]);
        let mut record = harness
            .journal
            .into_iter()
            .find_map(|entry| {
                matches!(entry.record, Record::InstructionObservationStarted { .. })
                    .then_some(entry.record)
            })
            .unwrap();
        let Record::InstructionObservationStarted {
            owner: InstructionObservationOwner::Managed { turn, .. },
            ..
        } = &mut record
        else {
            unreachable!()
        };
        *turn = dudes_types::TurnId::from_parts(0, 900);
        assert_record_floor(record);
    }

    #[test]
    fn thread_and_plain_card_records_are_reservations_too() {
        use dudes_proto::record::{MailAct, MailFrom, MailTo};
        use dudes_proto::{Actor, Record};
        use dudes_types::{CardId, MailId, MailState, ThreadId};
        assert_record_floor(Record::CardCreated {
            card: CardId::from_parts(0, 900),
            title: "card".into(),
            department: "infra".into(),
            by: Actor::System,
            deps: Vec::new(),
        });
        assert_record_floor(Record::Mail {
            id: MailId::NIL,
            from: MailFrom::Human,
            to: MailTo::Human,
            act: MailAct::Message,
            subject: "thread".into(),
            body: Some(String::new()),
            body_len: 0,
            body_path: None,
            body_blake3: None,
            thread: Some(ThreadId::from_parts(0, 900)),
            thread_seq: 1,
            members: Vec::new(),
            state: MailState::Durable,
            actor: None,
            provenance: None,
        });
    }

    #[test]
    fn a_session_recorded_only_by_a_reissue_excludes_its_lost_bit_alias() {
        let session = session_id_from_ulid(Ulid::from_parts(0, 7));
        let mut rng = FakeRng {
            counter: (1 << 76) + 6,
            ..FakeRng::new()
        };
        rng.reserve_entries(&[Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record: dudes_proto::Record::TurnSpawned {
                agent: AgentId::NIL,
                turn: dudes_types::TurnId::NIL,
                pid: 1,
                start_time: 1,
                boot: None,
                session: Some(session),
                reissued: true,
                mcp_grant: None,
            },
        }])
        .unwrap();
        assert_eq!(rng.ulid(), Ulid::from_parts(0, (1 << 76) + 8));
    }

    #[test]
    #[should_panic(expected = "fake RNG exhausted its 80-bit counter")]
    fn ulid_exhaustion_never_wraps() {
        FakeRng {
            counter: MAX_COUNTER,
            ..FakeRng::new()
        }
        .ulid();
    }

    #[test]
    #[should_panic(expected = "fake RNG exhausted its 80-bit counter")]
    fn token_exhaustion_never_wraps() {
        FakeRng {
            counter: MAX_COUNTER,
            ..FakeRng::new()
        }
        .agent_token();
    }
}
