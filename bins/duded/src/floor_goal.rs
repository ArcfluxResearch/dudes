//! Browser goal requests, a text change or a renewal, and their finite outcomes.

use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_proto::GoalBudget;
use dudes_proto::record::{BrowserGoalKind, Hex32};
use serde::de::DeserializeOwned;

use crate::floor_actions::Enrollment;
use crate::goal_replacement::{Allowance, browser_digest, renew_digest};
use dudes_types::{AgentId, GoalId, Tokens, Usd};
use serde::{Deserialize, Serialize};

/// Most UTF-8 bytes one goal text carries after trimming: the onboarding goal's limit, so the
/// Floor's hire form and goal form accept the same text.
pub const MAX_GOAL_TEXT_BYTES: usize = 2_048;

/// Most bytes one goal request body may carry, checked before a byte of it is read. The largest
/// body a standard `JSON.stringify` produces for a valid text is 12,431 bytes.
pub const MAX_GOAL_BODY_BYTES: usize = 16_384;

/// The `goal` sibling a Floor snapshot carries exactly when goal change is enabled. Outside
/// `FloorProjection` and `floor.delta`, and discarded with the rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoalView {
    max_text_bytes: usize,
    /// Present exactly when renewal is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    renew: Option<RenewView>,
}

/// The allowance a renewal would give in each enrolled workspace, in id order. Not filtered by
/// Hire's rule: the route checks the agent's own workspace when a renewal arrives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RenewView {
    workspaces: Vec<RenewWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RenewWorkspace {
    workspace_id: String,
    hop_cap: String,
    goal_usd_micros: Option<String>,
    goal_tokens: Option<String>,
}

impl GoalView {
    /// The sibling for a daemon that changes goal text, and also renews from `renewal` when it
    /// is given.
    pub(crate) fn new(renewal: Option<&Enrollment>) -> Self {
        Self {
            max_text_bytes: MAX_GOAL_TEXT_BYTES,
            renew: renewal.map(|enrollment| RenewView {
                workspaces: enrollment
                    .profiles
                    .iter()
                    .map(|profile| {
                        let (hop_cap, goal_usd_micros, goal_tokens) =
                            Allowance::of(profile).decimals();
                        RenewWorkspace {
                            workspace_id: profile.id.clone(),
                            hop_cap,
                            goal_usd_micros,
                            goal_tokens,
                        }
                    })
                    .collect(),
            }),
        }
    }
}

/// One canonical, bounded goal change or renewal. Nothing in it chooses a predicate or a ceiling,
/// and a renewal's numbers only restate what the page displayed.
#[derive(Clone)]
pub struct GoalRequest {
    pub(crate) request_id: Hex32,
    pub(crate) request_wire: String,
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    pub(crate) goal: GoalId,
    pub(crate) text: String,
    /// `None` for a text change; for a renewal, the allowance the page displayed, which applies
    /// only while it equals the workspace's.
    pub(crate) renewal: Option<Allowance>,
    /// The enrolled check the request names, by id and revision; `None` keeps the current check.
    pub(crate) check: Option<(String, Hex32)>,
}

impl fmt::Debug for GoalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GoalRequest(<redacted>)")
    }
}

/// What an admitted goal change or renewal may truthfully report once the writer has run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalOutcome {
    /// This request's attributed `GoalSet` is durable and indexed, on the first attempt or on a
    /// same-key retry. A durable new goal: no turn is promised to have started or stopped.
    Applied {
        /// The request identity as the page sent it.
        request_id: String,
        /// The agent whose goal it replaced.
        agent: AgentId,
        /// The new goal.
        goal: GoalId,
    },
    /// No effect: the first reason, in a fixed order, that holds.
    Refused {
        /// The request identity as the page sent it.
        request_id: String,
        /// `agent_absent`, `agent_halted`, `multi_repository`, `stale_goal`, `turn_open`, `predicate_claimed`,
        /// `predicate_todo`, then `workspace_unavailable`, `profile_changed` and
        /// `goal_exhausted` for a change, or `goal_not_exhausted`, `workspace_unavailable` and
        /// `profile_changed` for a renewal, then `check_running`, `check_limit`,
        /// `check_cannot_run`, `check_already_passes` and `check_did_not_finish`.
        reason: &'static str,
    },
}

/// What a goal change's or renewal's first job answers: the request's answer, or its check
/// running once before anything is appended.
#[derive(Debug)]
pub enum GoalStep {
    /// The request's answer.
    Answer(Result<GoalOutcome, GoalRefusal>),
    /// The check is running; the request waits on it.
    Check(crate::baseline::Ticket),
}

/// An empty HTTP refusal with no invented identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalRefusal {
    /// The session expired or was revoked before the append permit; nothing was appended.
    Forbidden,
    /// The same request identity names a different request digest.
    Conflict,
    /// This request's own append may be durable; the same identity may be checked again later.
    Unavailable,
    /// This request appended nothing and may be retried at once with the same identity.
    UnavailableNoEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalJson {
    request_id: String,
    agent: String,
    goal: String,
    text: String,
    #[serde(default)]
    check_id: Option<String>,
    #[serde(default)]
    check_revision: Option<Hex32>,
}

/// A renewal body: the change's four members and the allowance the page displayed. Each ceiling
/// member must be present, as a string or `null`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewJson {
    request_id: String,
    agent: String,
    goal: String,
    text: String,
    hop_cap: String,
    #[serde(deserialize_with = "Option::deserialize")]
    goal_usd_micros: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    goal_tokens: Option<String>,
    #[serde(default)]
    check_id: Option<String>,
    #[serde(default)]
    check_revision: Option<Hex32>,
}

/// One flat object of exactly `T`'s members: an unknown member is refused by
/// `deny_unknown_fields`, a duplicate by the derived decoder, and a nested value by the type of
/// every member, all before any effect.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_GOAL_BODY_BYTES {
        bail!("Floor goal request is too large");
    }
    // The derived decoder also reads a struct from a positional array; only an object is a
    // valid request body.
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        bail!("Floor goal request is not one JSON object");
    }
    serde_json::from_slice(bytes).context("invalid Floor goal request JSON")
}

/// `text` as the canonical decimal form of a number, so a body that spells one value two ways
/// cannot carry two digests.
fn canonical<T: std::str::FromStr + ToString>(text: &str) -> Option<T> {
    text.parse::<T>()
        .ok()
        .filter(|value| value.to_string() == text)
}

/// A request's named check: both members or neither, and a well-formed id.
fn check(id: Option<String>, revision: Option<Hex32>) -> Result<Option<(String, Hex32)>> {
    match (id, revision) {
        (None, None) => Ok(None),
        (Some(id), Some(revision))
            if dudes_proto::rpc::goal::CompletionCheckDeclaration::valid_id(&id) =>
        {
            Ok(Some((id, revision)))
        }
        _ => bail!("invalid Floor goal check"),
    }
}

impl GoalRequest {
    /// Decode one goal change body.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: GoalJson = decode(bytes)?;
        Self::new(
            raw.request_id,
            &raw.agent,
            &raw.goal,
            &raw.text,
            None,
            check(raw.check_id, raw.check_revision)?,
        )
    }

    /// Decode one renewal body: a change's members, plus a hop cap from 1 to `u32::MAX` and each
    /// goal ceiling as canonical decimal text or `null`.
    pub(crate) fn parse_renewal(bytes: &[u8]) -> Result<Self> {
        let raw: RenewJson = decode(bytes)?;
        let hop_cap = canonical::<u32>(&raw.hop_cap)
            .filter(|cap| *cap > 0)
            .context("invalid Floor renewal hop cap")?;
        let ceiling = |text: Option<String>| {
            text.map(|text| canonical::<u64>(&text).context("invalid Floor renewal ceiling"))
                .transpose()
        };
        let allowance = Allowance {
            hop_cap,
            budget: GoalBudget {
                usd: ceiling(raw.goal_usd_micros)?.map(Usd::from_micros),
                tokens: ceiling(raw.goal_tokens)?.map(Tokens::new),
            },
        };
        Self::new(
            raw.request_id,
            &raw.agent,
            &raw.goal,
            &raw.text,
            Some(allowance),
            check(raw.check_id, raw.check_revision)?,
        )
    }

    fn new(
        request_wire: String,
        agent_wire: &str,
        goal_wire: &str,
        text: &str,
        renewal: Option<Allowance>,
        check: Option<(String, Hex32)>,
    ) -> Result<Self> {
        let request_id = crate::floor_actions::decode_attempt(&request_wire)
            .context("invalid Floor goal request id")?;
        let agent = AgentId::parse(agent_wire).map_err(|_| anyhow!("invalid Floor goal agent"))?;
        let goal = GoalId::parse(goal_wire).map_err(|_| anyhow!("invalid Floor goal identity"))?;
        let text = text
            .trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
            .to_owned();
        if !(1..=MAX_GOAL_TEXT_BYTES).contains(&text.len()) {
            bail!("Floor goal text is outside its byte limit");
        }
        // The text becomes a launch task, which no launch can
        // carry with U+0000 in it.
        if text.contains('\0') {
            bail!("Floor goal text carries U+0000");
        }
        let named = check
            .as_ref()
            .map(|(id, revision)| (id.as_str(), *revision));
        let request_digest = match &renewal {
            None => browser_digest(agent_wire, goal_wire, &text, named),
            Some(allowance) => renew_digest(agent_wire, goal_wire, &text, allowance, named),
        };
        Ok(Self {
            request_id,
            request_wire,
            request_digest,
            agent,
            goal,
            text,
            renewal,
            check,
        })
    }

    /// The journal's name for what this request does.
    pub(crate) const fn kind(&self) -> BrowserGoalKind {
        if self.renewal.is_some() {
            BrowserGoalKind::Renewal
        } else {
            BrowserGoalKind::Change
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::goal_replacement::{Leftover, leftover};
    use dudes_proto::{Goal, Predicate, Spend};
    use serde_json::Value;

    use super::*;

    const REQUEST_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const GOAL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

    fn body(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_id": REQUEST_ID,
            "agent": AGENT,
            "goal": GOAL,
            "text": text,
        }))
        .unwrap()
    }

    /// The digest is pinned to one vector computed outside Rust (Python's hashlib over the same
    /// framing), over the trimmed text, and the request identity does not enter it.
    #[test]
    fn canonical_fields_have_one_domain_separated_request_digest() {
        let request = GoalRequest::parse(&body(" ship the fix \n")).expect("canonical");
        assert_eq!(request.text, "ship the fix");
        assert_eq!(request.request_id.bytes(), [0; 32]);
        assert_eq!(
            serde_json::to_value(request.request_digest).unwrap(),
            "742208c8c53272d9967d380f438f37db3c926858fcba28f057484aef8a4badb6"
        );
        let other_id = serde_json::to_vec(&serde_json::json!({
            "request_id": "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": AGENT,
            "goal": GOAL,
            "text": "ship the fix",
        }))
        .unwrap();
        assert_eq!(
            GoalRequest::parse(&other_id).unwrap().request_digest,
            request.request_digest
        );
    }

    /// One flat object and nothing else: the derived decoder would read the same four values
    /// from a positional array, so the array is refused first.
    #[test]
    fn only_an_object_is_a_request() {
        let spaced = [b" \n".as_slice(), &body("ship")].concat();
        assert!(
            GoalRequest::parse(&spaced).is_ok(),
            "control: leading space"
        );
        let array =
            serde_json::to_vec(&serde_json::json!([REQUEST_ID, AGENT, GOAL, "ship"])).unwrap();
        assert!(GoalRequest::parse(&array).is_err(), "admitted an array");
    }

    fn renewal(hop_cap: &str, usd: &Value, tokens: &Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_id": REQUEST_ID,
            "agent": AGENT,
            "goal": GOAL,
            "text": " ship the fix \n",
            "hop_cap": hop_cap,
            "goal_usd_micros": usd,
            "goal_tokens": tokens,
        }))
        .unwrap()
    }

    /// A renewal restates the allowance it was shown: a `null` ceiling is uncapped and a decimal one
    /// is that cap, and its digest is the renewal's own, not the change's.
    #[test]
    fn a_renewal_carries_the_shown_allowance_and_its_own_digest() {
        let request =
            GoalRequest::parse_renewal(&renewal("3", &Value::from("1000000"), &Value::Null))
                .expect("canonical");
        assert_eq!(request.kind(), BrowserGoalKind::Renewal);
        assert_eq!(request.text, "ship the fix");
        assert_eq!(
            request.renewal,
            Some(Allowance {
                hop_cap: 3,
                budget: GoalBudget {
                    usd: Some(Usd::from_micros(1_000_000)),
                    tokens: None,
                },
            })
        );
        assert_eq!(
            serde_json::to_value(request.request_digest).unwrap(),
            "2265b43204cdb80fa6a87f6b51368325626f6fed1c57fc4912b6ca1872c26a8c"
        );
        assert_eq!(
            GoalRequest::parse(&body("ship")).unwrap().kind(),
            BrowserGoalKind::Change
        );
        let zero = GoalRequest::parse_renewal(&renewal("3", &Value::Null, &Value::from("0")))
            .expect("a zero token cap");
        assert_eq!(
            zero.renewal.unwrap().budget.tokens,
            Some(Tokens::ZERO),
            "zero is a cap, never read as absent"
        );
    }

    /// Each allowance member is canonical decimal text in range, and each ceiling member is
    /// present. A renewal body is not a change body, and the reverse.
    #[test]
    fn a_renewal_refuses_a_noncanonical_or_missing_allowance() {
        for (label, bytes) in [
            ("zero turns", renewal("0", &Value::Null, &Value::Null)),
            (
                "past u32",
                renewal("4294967296", &Value::Null, &Value::Null),
            ),
            ("leading zero", renewal("03", &Value::Null, &Value::Null)),
            ("plus sign", renewal("+3", &Value::Null, &Value::Null)),
            (
                "past u64",
                renewal("3", &Value::from("18446744073709551616"), &Value::Null),
            ),
            ("a number", renewal("3", &Value::from(5), &Value::Null)),
            ("empty", renewal("3", &Value::Null, &Value::from(""))),
            (
                "leading space",
                renewal("3", &Value::from(" 5"), &Value::Null),
            ),
            ("a change body", body("ship")),
        ] {
            assert!(GoalRequest::parse_renewal(&bytes).is_err(), "{label}");
        }
        let mut missing: serde_json::Map<String, Value> =
            serde_json::from_slice(&renewal("3", &Value::Null, &Value::Null)).unwrap();
        missing.remove("goal_tokens");
        assert!(
            GoalRequest::parse_renewal(&serde_json::to_vec(&missing).unwrap()).is_err(),
            "a missing ceiling member"
        );
        assert!(
            GoalRequest::parse_renewal(&renewal("4294967295", &Value::Null, &Value::Null)).is_ok(),
            "control: the largest hop cap"
        );
        assert!(
            GoalRequest::parse(&renewal("3", &Value::Null, &Value::Null)).is_err(),
            "a renewal body on the change route"
        );
    }

    fn goal(usd: Option<u64>, tokens: Option<u64>, hop_cap: u32) -> Goal {
        Goal {
            completion_check: None,
            id: GoalId::parse(GOAL).unwrap(),
            text: "old".to_owned(),
            predicate: Predicate::Command {
                argv: vec!["/usr/bin/true".to_owned()],
                timeout_s: 10,
            },
            budget: GoalBudget {
                usd: usd.map(Usd::from_micros),
                tokens: tokens.map(Tokens::new),
            },
            hop_cap,
            baseline: None,
        }
    }

    /// A change or a renewal names a check with both members or neither, and naming one moves its
    /// digest to the domain that also frames the check.
    #[test]
    fn a_named_check_takes_both_members_and_its_own_digest_domain() {
        let revision = "ab".repeat(32);
        let named = |body: Vec<u8>, id: Option<&str>, revision: Option<&str>| {
            let mut value: Value = serde_json::from_slice(&body).unwrap();
            if let Some(id) = id {
                value["check_id"] = Value::from(id);
            }
            if let Some(revision) = revision {
                value["check_revision"] = Value::from(revision);
            }
            serde_json::to_vec(&value).unwrap()
        };
        for (body, parse) in [
            (
                body("ship"),
                GoalRequest::parse as fn(&[u8]) -> Result<GoalRequest>,
            ),
            (
                renewal("3", &Value::Null, &Value::Null),
                GoalRequest::parse_renewal,
            ),
        ] {
            let kept = parse(&body).unwrap();
            let chosen =
                parse(&named(body.clone(), Some("contributing"), Some(&revision))).unwrap();
            assert_eq!(
                chosen.check,
                Some((
                    "contributing".to_owned(),
                    serde_json::from_value(Value::from(revision.as_str())).unwrap()
                ))
            );
            assert_ne!(chosen.request_digest, kept.request_digest);
            for half in [
                named(body.clone(), Some("contributing"), None),
                named(body.clone(), None, Some(&revision)),
                named(body.clone(), Some("Contributing"), Some(&revision)),
            ] {
                assert!(parse(&half).is_err());
            }
        }
    }

    fn spend(usd: u64, tokens: u64, work_tokens: u64) -> Spend {
        Spend {
            usd: Usd::from_micros(usd),
            tokens: Tokens::new(tokens),
            work_tokens: Tokens::new(work_tokens),
        }
    }

    /// Each capped unit loses the reserved figure (tokens compare work tokens, as the budget gate does),
    /// an uncapped unit stays uncapped, and the hop cap loses the turns issued.
    #[test]
    fn leftover_subtracts_the_reserve_in_each_capped_unit_and_the_turns_issued() {
        assert_eq!(
            leftover(&goal(Some(150), Some(1_000), 4), 1, spend(100, 900, 400)),
            Some(Leftover {
                budget: GoalBudget {
                    usd: Some(Usd::from_micros(50)),
                    tokens: Some(Tokens::new(600)),
                },
                hop_cap: 3,
            })
        );
        assert_eq!(
            leftover(&goal(None, None, 4), 3, spend(u64::MAX, 0, 0)),
            Some(Leftover {
                budget: GoalBudget {
                    usd: None,
                    tokens: None,
                },
                hop_cap: 1,
            }),
            "an uncapped unit bounds nothing"
        );
    }

    /// A zero remainder in the hop cap or in any capped unit leaves the new goal nothing.
    #[test]
    fn leftover_is_none_when_any_capped_remainder_is_zero() {
        assert_eq!(leftover(&goal(Some(150), None, 4), 4, spend(0, 0, 0)), None);
        assert_eq!(
            leftover(&goal(Some(150), None, 4), 1, spend(150, 0, 0)),
            None
        );
        assert_eq!(
            leftover(&goal(Some(150), None, 4), 1, spend(200, 0, 0)),
            None
        );
        assert_eq!(
            leftover(&goal(None, Some(10), 4), 1, spend(0, 99, 10)),
            None,
            "tokens compare work tokens"
        );
        assert_eq!(leftover(&goal(Some(0), None, 4), 0, spend(0, 0, 0)), None);
    }
}
