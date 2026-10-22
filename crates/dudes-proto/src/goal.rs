//! A goal and the predicate that decides it. Done is observed, never declared (invariant 3).

use std::path::PathBuf;

use dudes_types::{CardId, CardStatus, GoalId, Usage, Usd};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::budget::GoalBudget;
use crate::record::Actor;

/// What an agent is working toward: text for the model, a predicate for the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Goal {
    /// Minted at `goal.set`; a `GoalSet` whose id already exists is rejected.
    pub id: GoalId,
    /// The goal as the model reads it.
    pub text: String,
    /// How the daemon decides it is met.
    pub predicate: Predicate,
    /// The recorded completion check summary, whenever the check was given a name: an enrolled
    /// check, or one declared to `goal.set` or `goal.replace`. Absent means no name was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_check: Option<CompletionCheckRef>,
    /// What the daemon saw of the check before the goal's first turn. Absent for a goal set
    /// before goal set ran the check, and for a predicate the daemon has no runner for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<CheckBaseline>,
    /// Ceiling for the whole goal.
    pub budget: GoalBudget,
    /// Most managed turns the goal may consume (gate 6).
    pub hop_cap: u32,
}

/// The configured check content, without its command or a claim of semantic adequacy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheckRef {
    /// Operator-supplied safe identifier.
    pub id: String,
    /// SHA-256 of the named check content.
    pub revision: crate::record::Hex32,
    /// Public-safe description shown by the Floor.
    pub label: String,
}

/// What the daemon saw of a goal's check before its first turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckBaseline {
    /// A command check ran once before any turn on this goal and exited non-zero.
    CommandFailed {
        /// The canonical program the run spawned.
        program: String,
        /// Its exit code.
        exit: i32,
    },
    /// A board check's card was not at the goal's status with an eligible author.
    BoardNotMet,
    /// The operator declared that the check may already pass, so it did not run.
    MayAlreadyPass,
}

/// A [`CheckBaseline`] as a reader needs it: whether the check failed before work, or may have
/// passed before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckBaselineKind {
    /// The check ran before the first turn and did not pass.
    Failed,
    /// The operator declared that the check may already pass, so it did not run.
    MayAlreadyPass,
}

impl CheckBaseline {
    /// What a reader needs of this baseline.
    #[must_use]
    pub const fn kind(&self) -> CheckBaselineKind {
        match self {
            Self::CommandFailed { .. } | Self::BoardNotMet => CheckBaselineKind::Failed,
            Self::MayAlreadyPass => CheckBaselineKind::MayAlreadyPass,
        }
    }
}

/// Something the daemon can run to decide whether a goal is met. Externally tagged:
/// `{"command":{"argv":["npm","test"],"timeout_s":600}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// A command the daemon runs in the agent's working directory exits 0.
    ///
    /// The child is not an agent child: its environment is a daemon-owned allowlist and carries
    /// neither `DUDES_SOCK` nor `DUDES_TOKEN`. What it *reads* is the agent's
    /// own tree, and that residual is accepted: an agent that can write `.cargo/config.toml`,
    /// `package.json` or a `Makefile` there authors what exit 0 means.
    ///
    /// `argv` is a typed list: nothing here is ever handed to a shell (invariant 7).
    Command {
        /// Program and arguments. PATH lookup happens in the daemon: a bare `argv[0]` resolves
        /// against the daemon's own `PATH` — never the agent's, and never a relative entry — and
        /// the result is canonicalized, so the absolute path that is spawned is not re-resolved
        /// afterwards.
        argv: Vec<String>,
        /// Kill and fail after this many seconds.
        timeout_s: u64,
    },
    /// A path hashes to a known value.
    PathHash {
        /// Relative to the agent's working directory.
        path: PathBuf,
        /// Expected blake3 digest, lowercase hex.
        blake3: String,
    },
    /// A board card is in a given status, set by an operator or the daemon. A status an agent
    /// set does not complete the goal, and `goal.set` refuses one that asks for `claimed`.
    BoardState {
        /// The card.
        card: CardId,
        /// The status it must be in.
        status: CardStatus,
    },
    /// An agent-hook verifier judges the outcome; it has its own budget line.
    AgentVerify {
        /// The verifier's instructions.
        prompt: String,
        /// Ceiling for the verifier's own turns.
        budget: Usd,
    },
}

/// What a predicate run produced, enough to be re-checked by a reader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    /// A [`Predicate::Command`] ran.
    Command {
        /// What the operator wrote at `goal.set`, never rewritten. What the
        /// daemon resolved it to is `program`.
        argv: Vec<String>,
        /// The daemon-resolved, canonicalized program that was spawned.
        /// Absent whenever nothing ran, including resolution failure, a non-UTF-8 canonical path,
        /// or an in-tree refusal; `argv` is then what was attempted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        /// Exit code; `None` when killed by the timeout or a signal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
        /// The last few KiB of stdout and stderr, interleaved as captured.
        #[serde(default)]
        output_tail: String,
    },
    /// A [`Predicate::PathHash`] was computed.
    Hash {
        /// The path hashed.
        path: PathBuf,
        /// What the goal expected.
        expected: String,
        /// What the file hashed to; `None` when it did not exist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actual: Option<String>,
    },
    /// A [`Predicate::BoardState`] was read from the projection.
    Card {
        /// The card.
        card: CardId,
        /// Its status at the decision `lsn`.
        status: CardStatus,
        /// Who authored that status, copied from `CardView::status_by`. Without it this
        /// evidence is not "enough to be re-checked by a reader": the check made on it is
        /// the board-status check, and that is a question about the author.
        by: Actor,
    },
    /// A [`Predicate::AgentVerify`] verifier answered.
    Verdict {
        /// The verifier's reasoning, for `dudes why`.
        #[serde(default)]
        rationale: String,
        /// What the verifier's turns cost.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

impl Evidence {
    /// Whether this result says its check never started: a failed command result with neither a
    /// spawned program nor an exit code, which is what the runner journals when it refuses to
    /// spawn. A timeout or a signal names its program, and fixtures journal `pass: true` with no
    /// program, so all three terms are needed.
    #[must_use]
    pub fn not_run(&self, pass: bool) -> bool {
        !pass
            && matches!(
                self,
                Self::Command {
                    program: None,
                    exit: None,
                    ..
                }
            )
    }
}

/// Whether `evidence` may support a passing result for `predicate`.
///
/// A [`Predicate::BoardState`] pass needs [`Evidence::Card`] naming the requested card and status,
/// where that status can complete a goal and its author is an operator peer or the daemon. Any
/// other evidence fails it. Every other predicate kind keeps its own judgement, so this answers
/// `true` for it.
///
/// The two matches are the board-completion table with the retirement Todo row, written exhaustively so that a
/// new status or author fails to compile here until someone decides its row.
#[must_use]
pub fn predicate_evidence_eligible(predicate: &Predicate, evidence: &Evidence) -> bool {
    let card = match evidence {
        Evidence::Card { card, status, by } => Some((*card, *status, by)),
        _ => None,
    };
    predicate_card_evidence_eligible(predicate, card)
}

pub(crate) fn predicate_card_evidence_eligible(
    predicate: &Predicate,
    evidence: Option<(CardId, CardStatus, &Actor)>,
) -> bool {
    let Predicate::BoardState { card, status } = predicate else {
        return true;
    };
    let Some((seen_card, seen_status, by)) = evidence else {
        return false;
    };
    let status_can_complete = match status {
        CardStatus::Doing | CardStatus::Review | CardStatus::Blocked | CardStatus::Done => true,
        // Claim records always name the claimer, so no author could ever make a Claimed row pass.
        // Retirement requeues the retired agent's unfinished cards to Todo as the daemon, so a
        // passing Todo row would let one agent's retirement finish another's goal.
        CardStatus::Claimed | CardStatus::Todo => false,
    };
    let author_can_complete = match by {
        Actor::Peer { .. } | Actor::System => true,
        Actor::Agent { .. } | Actor::BrowserSession { .. } => false,
    };
    seen_card == *card && seen_status == *status && status_can_complete && author_can_complete
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `goal.set` RPC example shows the wire shape of a command predicate.
    #[test]
    fn command_predicate_matches_the_design_example() {
        let json = serde_json::json!({"command": {"argv": ["npm", "test"], "timeout_s": 600}});
        let predicate: Predicate = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            predicate,
            Predicate::Command {
                argv: vec!["npm".into(), "test".into()],
                timeout_s: 600
            }
        );
        assert_eq!(serde_json::to_value(&predicate).unwrap(), json);
    }

    /// Every predicate round-trips under its external tag, and the sample count matches the
    /// schema so a new variant without a sample fails here.
    #[test]
    fn every_predicate_round_trips() {
        let card = CardId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAZ").unwrap();
        let samples = [
            (
                Predicate::Command {
                    argv: vec!["npm".into(), "test".into()],
                    timeout_s: 600,
                },
                "command",
            ),
            (
                Predicate::PathHash {
                    path: PathBuf::from("dist/app.js"),
                    blake3: "ab".repeat(32),
                },
                "path_hash",
            ),
            (
                Predicate::BoardState {
                    card,
                    status: CardStatus::Done,
                },
                "board_state",
            ),
            (
                Predicate::AgentVerify {
                    prompt: "Does the PR do what the card says?".into(),
                    budget: Usd::from_dollars(2),
                },
                "agent_verify",
            ),
        ];
        let schema = schemars::schema_for!(Predicate);
        let variants = schema.as_value()["oneOf"].as_array().unwrap().len();
        assert_eq!(variants, samples.len(), "a Predicate has no sample");
        for (predicate, tag) in samples {
            let json = serde_json::to_value(&predicate).unwrap();
            assert!(json.get(tag).is_some(), "{tag}: {json}");
            assert_eq!(
                serde_json::from_value::<Predicate>(json).unwrap(),
                predicate
            );
        }
        assert_eq!(
            serde_json::to_value(Predicate::BoardState {
                card,
                status: CardStatus::Done
            })
            .unwrap(),
            serde_json::json!({"board_state": {"card": "01ARZ3NDEKTSV4RRFFQ69G5FAZ", "status": "done"}})
        );
    }

    /// Payload fields only ever grow within frame `ver = 1`: command
    /// evidence written before `program` existed still reads, as `None` — and a run with no
    /// resolved program writes exactly that shape back, so absent stays absent on the wire.
    #[test]
    fn command_evidence_without_a_program_still_deserialises() {
        let before = serde_json::json!({
            "kind": "command",
            "argv": ["npm", "test"],
            "exit": 0,
            "output_tail": "ok"
        });
        let evidence: Evidence = serde_json::from_value(before.clone()).unwrap();
        assert_eq!(
            evidence,
            Evidence::Command {
                argv: vec!["npm".into(), "test".into()],
                program: None,
                exit: Some(0),
                output_tail: "ok".into(),
            }
        );
        assert_eq!(serde_json::to_value(&evidence).unwrap(), before);
    }

    /// The evidence separates what the operator wrote from what the daemon ran:
    /// the resolved program sits beside `argv`, never in place of `argv[0]`, so a reader can tell
    /// the two apart in one payload.
    #[test]
    fn command_evidence_records_the_resolved_program_beside_the_operator_argv() {
        let evidence = Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(0),
            output_tail: String::new(),
        };
        let json = serde_json::to_value(&evidence).unwrap();
        assert_eq!(json["argv"], serde_json::json!(["npm", "test"]));
        assert_eq!(json["program"], "/opt/toolchain/bin/npm");
        assert_eq!(serde_json::from_value::<Evidence>(json).unwrap(), evidence);
    }

    /// A result whose program never started is the runner's refusal: no pass, no program, no
    /// exit. A timeout or a signal names the program it spawned, and a fixture's pass names none.
    #[test]
    fn only_a_refused_command_result_did_not_run() {
        let command = |program: Option<&str>, exit: Option<i32>| Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: program.map(Into::into),
            exit,
            output_tail: String::new(),
        };
        let npm = Some("/opt/toolchain/bin/npm");
        for (evidence, pass, not_run) in [
            (command(None, None), false, true),
            (command(None, None), true, false),
            (command(npm, None), false, false),
            (command(npm, Some(1)), false, false),
            (command(npm, Some(0)), true, false),
            (command(None, Some(1)), false, false),
            (
                Evidence::Hash {
                    path: PathBuf::from("dist/app.js"),
                    expected: "ab".into(),
                    actual: None,
                },
                false,
                false,
            ),
        ] {
            assert_eq!(evidence.not_run(pass), not_run, "{evidence:?} pass={pass}");
        }
    }

    /// `Evidence::Card.by` is **required**, and required-ness is the property.
    /// Evidence is "what a predicate run produced, enough to be re-checked by a reader", and
    /// what a `BoardState` run is re-checked against is a question about the author, so card
    /// evidence that may arrive without one is evidence that cannot be re-checked — and an
    /// optional author reads as *absent means fine*, a bug class returning through
    /// the type system. This goes red the moment `by` becomes an `Option` or gains a
    /// `#[serde(default)]`.
    ///
    /// Contrast `command_evidence_without_a_program_still_deserialises` above: `program` is
    /// optional because "nothing was resolved" is a real outcome it has to be able to say.
    /// There is no run in which a card had no author, so `by` has no absent case to encode.
    #[test]
    fn card_evidence_without_an_author_is_refused() {
        let with = serde_json::json!({
            "kind": "card",
            "card": "01ARZ3NDEKTSV4RRFFQ69G5FAZ",
            "status": "done",
            "by": {"kind": "peer", "pid": 77, "uid": 501}
        });

        // Control: the same payload with the author present parses and round-trips.
        let evidence: Evidence =
            serde_json::from_value(with.clone()).expect("control: the sample is well formed");
        assert_eq!(
            serde_json::to_value(&evidence).unwrap(),
            with,
            "control: it round-trips"
        );

        let mut without = with;
        without.as_object_mut().unwrap().remove("by");
        let err = serde_json::from_value::<Evidence>(without)
            .expect_err("card evidence must name the status's author");
        assert!(
            err.to_string().contains("missing field `by`"),
            "refused for the wrong reason: {err}"
        );
    }
}
