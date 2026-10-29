//! `agent.*` params and results.

use std::path::{Path, PathBuf};

use dudes_types::{AgentId, IsolationMode, Lsn, Provider, Usd};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::budget::AgentBudget;
use crate::decision::{GateStep, LoopDecision};

use super::hook::HookToken;
use super::pty::PtyId;

/// `agent.hire` params. Creates a managed agent; attached mode is opt-in later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("oneOf" = [
    {"required": ["cwd"], "properties": {"cwd": {"minLength": 1}, "project": {"type": "null"}}},
    {"required": ["project"], "properties": {"cwd": {"const": ""}, "project": {"type": "object"}}}
]))]
pub struct HireParams {
    /// Display name.
    pub name: String,
    /// Authored cast character. Absence preserves the automatic appearance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent colour. Absence preserves the neutral accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<dudes_types::AccentColour>,
    /// Role text, at most 512 UTF-8 bytes. The hire refuses a control character and preserves
    /// admitted text exactly, including surrounding space and an empty role.
    pub role: String,
    /// Department.
    pub department: String,
    /// Which CLI drives it.
    pub provider: Provider,
    /// Legacy directory target. Exactly one of `cwd` and `project` is required.
    #[serde(default, skip_serializing_if = "empty_directory")]
    pub cwd: PathBuf,
    /// An explicitly selected project and the exact current repository observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectHireTarget>,
    /// Daily ceiling.
    pub budget: AgentBudget,
    /// Whether to pass the provider's sandbox on-switch. Absent and `null` are `None`, which
    /// leaves it to the provider's configured default, and are admitted.
    ///
    /// **An explicit `true` or `false` is refused at the hire** with `-32004`: no adapter carries
    /// the instruction, so neither polarity does anything, and answering the call would promise
    /// containment nothing applies. The field stays on the wire rather than being removed, and the
    /// day an adapter can carry the switch the refusal is lifted with no wire change and no journal
    /// migration. That is why the refusal exists, both halves of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
    /// Managed-turn isolation for this agent. Absent and `null` leave the provider's configured
    /// selection in force; an explicit value is recorded on `AgentHired`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationMode>,
    /// Cap on the vendor's own agentic turns inside one Dudes turn, recorded on `AgentHired` and
    /// replayed for every launch.
    ///
    /// Absent and `null` are `None`: Dudes emits no flag and applies no default of its own, so
    /// only the provider's behaviour remains. An explicit `0` is refused before the hire is
    /// recorded — it never means unlimited or provider-default — and negative, fractional,
    /// nonnumeric or out-of-range values fail this typed decode as `-32602 InvalidParams`.
    ///
    /// **A backstop, not a ceiling.** It is no part of budget admission and no part of hop
    /// accounting. There is no update method in v1: a different value needs a
    /// retire and a rehire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_turns: Option<u32>,
    /// The figure the vendor checks its own locally computed cost estimate against, recorded on
    /// `AgentHired` and replayed for every launch. Micro-dollars on the wire; the
    /// CLI's `--max-budget-usd` converts whole dollars into them with checked arithmetic.
    ///
    /// Absence, zero and invalid values behave exactly as [`Self::max_turns`] describes.
    ///
    /// **Not a spend ceiling.** The vendor's estimate can cross it, so wiring it changes the
    /// relationship only to a cap plus an unbounded remainder. Dudes' spend ceilings are gate 5's
    /// and are what invariant 4 governs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option::<u64>", range(min = 1))]
    pub max_budget: Option<Usd>,
    /// The model the agent's launches name. It replaces the engine's configured model for
    /// this agent. Absent means the engine's own selection: the configured model when the
    /// configuration sets one, else the engine's default. The daemon checks the shape only; an
    /// engine that does not know the name fails the agent's first turn with its own error. The
    /// operator can change it for the next turn with `agent.model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<dudes_types::ModelName>,
    /// The operator's declaration that they vouch for `cwd`, recorded once on
    /// `AgentHired` and never re-read from configuration. Absence means untrusted, so an omitted
    /// field and an explicit `false` are one meaning.
    ///
    /// Only the operator may declare it: `agent.hire` requires an operator,
    /// so a call from an agent is refused before this field is read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted: bool,
    /// Where the agent's working tree comes from. Absent means `cwd` itself, as always.
    ///
    /// `worktree` asks the daemon to create a git worktree of the repository `cwd` is in, on a new
    /// branch, and to hire the agent into it. It requires `trusted`, and is refused for
    /// Codex and on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<Checkout>,
    /// With `checkout: worktree`, the lease whose branch tip the new worktree starts from, so the
    /// new agent sees that agent's commits and none of its uncommitted edits. Absent means the
    /// repository's `HEAD`. Refused without `checkout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<dudes_types::WorktreeId>,
    /// The operator-declared MCP servers to opt this agent in to, by name. Absent means none.
    ///
    /// Each name must be a server the daemon's startup file declares, given once. Admission
    /// checks engine support, isolation, secret availability and program ownership and trust.
    /// The operator may replace the desired set for later launches through `agent.mcp`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<dudes_types::McpServerName>,
}

fn empty_directory(path: &Path) -> bool {
    path.as_os_str().is_empty()
}

/// An operator-selected project. Execution locations are resolved by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectHireTarget {
    /// The enrolled project's stable identity.
    #[schemars(length(min = 1, max = 32))]
    pub project_id: String,
    /// The exact current catalog record the operator observed.
    pub expected_revision: Lsn,
}

/// The exclusive target admitted at the hire boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HireTarget<'a> {
    /// An absolute legacy directory.
    Directory(&'a Path),
    /// A current project observation.
    Project(&'a ProjectHireTarget),
}

impl HireParams {
    /// Resolve the mutually exclusive target fields before any hire work.
    ///
    /// # Errors
    ///
    /// An absent or mixed target, a relative directory, or an empty project identity.
    pub fn target(&self) -> Result<HireTarget<'_>, &'static str> {
        match (self.cwd.as_os_str().is_empty(), &self.project) {
            (false, None) if self.cwd.is_absolute() => Ok(HireTarget::Directory(&self.cwd)),
            (false, None) => Err("cwd must be an absolute path"),
            (true, Some(project))
                if !project.project_id.is_empty() && project.project_id.len() <= 32 =>
            {
                Ok(HireTarget::Project(project))
            }
            (true, Some(_)) => Err("project_id must contain one through 32 bytes"),
            _ => Err("exactly one of cwd and project is required"),
        }
    }
}

/// The working tree an `agent.hire` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Checkout {
    /// A git worktree the daemon creates for this agent alone.
    Worktree,
}

/// `agent.hire` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HireResult {
    /// The new agent.
    pub agent: AgentId,
}

/// `agent.rename` params. The identity remains the `AgentId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameParams {
    /// The live agent.
    pub agent: AgentId,
    /// New display text, admitted with the same bounds as a new hire.
    pub name: String,
}

/// Read or completely replace a live agent's desired MCP server set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpParams {
    /// The live agent.
    pub agent: AgentId,
    /// Omission reads; an array replaces the complete set, including an empty array to clear.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "mcp_replacement"
    )]
    #[schemars(with = "Vec<dudes_types::McpServerName>")]
    pub mcp_servers: Option<Vec<dudes_types::McpServerName>>,
}

fn mcp_replacement<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<dudes_types::McpServerName>>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut names = Vec::<dudes_types::McpServerName>::deserialize(deserializer)?;
    names.sort();
    if let Some(repeated) = names.windows(2).find_map(|pair| match pair {
        [first, second] if first == second => Some(first),
        _ => None,
    }) {
        return Err(serde::de::Error::custom(format!(
            "MCP server `{repeated}` was given more than once"
        )));
    }
    Ok(Some(names))
}

/// The desired names for the next managed launch, rather than a current child's tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpResult {
    /// The unchanged agent identity.
    pub agent: AgentId,
    /// Complete canonical desired set.
    pub mcp_servers: Vec<dudes_types::McpServerName>,
    /// This call appended a changed replacement.
    pub changed: bool,
}

/// `agent.look` params. The engine and structural identity remain fixed. The model changes through
/// `agent.model`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
// Field-level `required` would remove the `null` arm from these Option schemas.
#[schemars(extend("required" = ["agent", "role", "character", "accent"]))]
pub struct LookParams {
    /// The live agent.
    pub agent: AgentId,
    /// New display role.
    pub role: String,
    /// Authored character, or `null` for automatic selection.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "Option<dudes_types::CastCharacter>")]
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent, or `null` for automatic selection.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "Option<dudes_types::AccentColour>")]
    pub accent: Option<dudes_types::AccentColour>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// `agent.model` params: the model the agent's next issued turn uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    /// The live, managed agent.
    pub agent: AgentId,
    /// The new override. Absent clears it, so the agent returns to the engine's own selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<dudes_types::ModelName>,
}

/// `agent.retire` params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireParams {
    /// The agent.
    pub agent: AgentId,
    /// Why.
    pub reason: String,
}

/// `agent.ask` params.
///
/// **There is deliberately no `agent` field.** The asker is the `PeerRole::Agent` the seam
/// resolved, never something the sender fills in: the method table grants this method to
/// `Agent` and to nothing else, and having no sender-supplied `agent` field is the cell that
/// makes the row safe to grant. [`AgentText`](super::AgentText) is the nearest existing shape and
/// is what all three of this method's neighbours use — `agent.steer`, `agent.answer` and the
/// `agent.question` notification — so reaching for it is the natural mistake, and it carries
/// exactly the field that would let one agent park another in `Wait`.
///
/// **`token` is declared here rather than left for the seam to find in the raw params.**
/// `Presented::token_of` reads the token out of `params["token"]` before dispatch, and every
/// params type in this module denies unknown fields — so a type that omitted `token` could not
/// carry one at all: the call would be refused `-32602` before the seam ever looked.
/// [`HookEventParams`](super::hook::HookEventParams) is the only other params type that declares
/// one.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskParams {
    /// The calling agent's token, and its sole attribution capability (invariant 8).
    ///
    /// Required rather than `Option`, unlike the four methods that take an optional token: `agent.ask`
    /// has no operator caller to make it optional for, so a call without one is refused before the
    /// handler.
    pub token: HookToken,
    /// The question, as the human will read it.
    ///
    /// Bounded in **bytes** by `dudes_rpc`'s `MAX_ASK_TEXT_BYTES`, and **refused rather than
    /// truncated**: a truncated question is one whose meaning the daemon changed,
    /// and an append cannot be un-journaled. The bound is deliberately not restated as a schema
    /// `maxLength`, which counts characters — that would put a bound on the wire that the handler
    /// does not enforce.
    pub text: String,
}

/// Borrowing serializer for one `agent.ask` params object.
///
/// A present token is wire-equivalent to [`AskParams`]. Absence omits the `token` key rather than
/// inventing a credential or encoding `null`, so the daemon remains the only component that
/// decides what an absent presentation means.
#[must_use]
pub fn ask_params<'a>(token: Option<&'a HookToken>, text: &'a str) -> impl Serialize + 'a {
    BorrowedAskParams { token, text }
}

/// Allocation-free counterpart of [`AskParams`] for callers borrowing an environment token.
#[derive(Serialize)]
struct BorrowedAskParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a HookToken>,
    text: &'a str,
}

/// `agent.attach` params.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachParams {
    /// The agent.
    pub agent: AgentId,
    /// Terminal columns.
    pub cols: u16,
    /// Terminal rows.
    pub rows: u16,
}

/// `agent.attach` result: the handle `pty.*` calls and notifications use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachResult {
    /// The PTY.
    pub pty: PtyId,
}

/// `agent.why` result: the last `LoopDecided`, as journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WhyResult {
    /// Where the record is.
    pub lsn: Lsn,
    /// blake3 of the decision's input, lowercase hex — or the **empty string** when no digest
    /// could be produced, carried through from the record verbatim. See
    /// `Record::LoopDecided.input_digest`, which owns the rule: the sentinel means "the input of
    /// this decision is not identified", never a hash, and is not comparable with another
    /// decision's digest.
    pub input_digest: String,
    /// The verdict.
    pub decision: LoopDecision,
    /// Gates consulted, in order.
    pub gate_trace: Vec<GateStep>,
}

#[cfg(test)]
mod tests {
    use dudes_types::IsolationMode;

    use super::*;

    #[test]
    fn mcp_read_and_clear_are_distinct_and_replacements_are_canonical() {
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let read: McpParams = serde_json::from_value(serde_json::json!({"agent": agent})).unwrap();
        assert_eq!(read.mcp_servers, None);
        assert_eq!(
            serde_json::to_value(read).unwrap(),
            serde_json::json!({"agent": agent})
        );
        for (given, expected) in [
            (serde_json::json!([]), serde_json::json!([])),
            (
                serde_json::json!(["search", "memory"]),
                serde_json::json!(["memory", "search"]),
            ),
        ] {
            let replacement: McpParams = serde_json::from_value(serde_json::json!({
                "agent": agent, "mcp_servers": given
            }))
            .unwrap();
            assert!(replacement.mcp_servers.is_some());
            assert_eq!(
                serde_json::to_value(replacement).unwrap(),
                serde_json::json!({
                    "agent": agent, "mcp_servers": expected
                })
            );
        }
    }

    #[test]
    fn mcp_rejects_null_duplicate_malformed_and_unknown_input() {
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        for bad in [
            serde_json::json!({"agent": agent, "mcp_servers": null}),
            serde_json::json!({"agent": agent, "mcp_servers": ["memory", "memory"]}),
            serde_json::json!({"agent": agent, "mcp_servers": ["Memory"]}),
            serde_json::json!({"agent": agent, "mcp_servers": "memory"}),
            serde_json::json!({"agent": agent, "provider": "claude"}),
            serde_json::json!({"mcp_servers": []}),
        ] {
            assert!(
                serde_json::from_value::<McpParams>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn mcp_schema_does_not_admit_null_or_require_replacement() {
        let schema = crate::schema::schema_for::<McpParams>().to_value();
        assert_eq!(schema["required"], serde_json::json!(["agent"]));
        assert_eq!(schema["properties"]["mcp_servers"]["type"], "array");
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn look_requires_explicit_nullable_appearance_fields() {
        let complete = serde_json::json!({
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "role": "designer",
            "character": null, "accent": null,
        });
        assert!(serde_json::from_value::<LookParams>(complete.clone()).is_ok());
        for omitted in ["character", "accent"] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(omitted);
            assert!(
                serde_json::from_value::<LookParams>(incomplete).is_err(),
                "{omitted}"
            );
        }
    }

    /// The allocation-free CLI form is the owned `agent.ask` contract when a token is present.
    /// With no token it emits the deliberate presentation-free object, which cannot decode as
    /// [`AskParams`] and is therefore unable to reach the handler as an admitted agent call.
    #[test]
    fn borrowing_ask_params_matches_owned_and_preserves_absence() {
        let token = HookToken::new("A".repeat(super::super::hook::HOOK_TOKEN_LEN))
            .expect("well-formed token");
        let owned = serde_json::to_value(AskParams {
            token,
            text: "which branch?".to_owned(),
        })
        .unwrap();
        let borrowing = serde_json::to_value(ask_params(
            Some(&HookToken::new("A".repeat(super::super::hook::HOOK_TOKEN_LEN)).unwrap()),
            "which branch?",
        ))
        .unwrap();
        assert_eq!(borrowing, owned);

        let absent = serde_json::to_value(ask_params(None, "which branch?")).unwrap();
        assert_eq!(absent, serde_json::json!({"text": "which branch?"}));
        assert!(serde_json::from_value::<AskParams>(absent).is_err());
    }

    fn required_hire_fields() -> serde_json::Value {
        serde_json::json!({
            "name": "Kim",
            "role": "backend engineer",
            "department": "engineering",
            "provider": "claude",
            "cwd": "/work/repo",
            "budget": {"usd_per_day": 1}
        })
    }

    #[test]
    fn project_hire_target_retains_the_exact_revision_and_excludes_a_directory() {
        let mut wire = required_hire_fields();
        wire.as_object_mut().unwrap().remove("cwd");
        wire["project"] = serde_json::json!({"project_id":"service","expected_revision":17});
        let params: HireParams = serde_json::from_value(wire.clone()).unwrap();
        let HireTarget::Project(target) = params.target().unwrap() else {
            panic!("project target")
        };
        assert_eq!(target.project_id, "service");
        assert_eq!(target.expected_revision, Lsn(17));
        assert_eq!(
            serde_json::to_value(params).unwrap()["project"],
            wire["project"]
        );
        wire["cwd"] = serde_json::json!("/work/repo");
        assert!(
            serde_json::from_value::<HireParams>(wire)
                .unwrap()
                .target()
                .is_err()
        );
        let mut missing = required_hire_fields();
        missing.as_object_mut().unwrap().remove("cwd");
        assert!(
            serde_json::from_value::<HireParams>(missing)
                .unwrap()
                .target()
                .is_err()
        );
    }

    #[test]
    fn project_hire_target_and_generated_schema_refuse_mixed_or_future_authority() {
        let mut wire = required_hire_fields();
        wire.as_object_mut().unwrap().remove("cwd");
        wire["project"] = serde_json::json!({"project_id":"service","expected_revision":17});
        let schema = crate::schema::schema_for::<HireParams>().to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&wire));
        wire["project"]["execution_path"] = serde_json::json!("/untrusted/client/path");
        assert!(serde_json::from_value::<HireParams>(wire.clone()).is_err());
        assert!(!validator.is_valid(&wire));
        wire["project"]
            .as_object_mut()
            .unwrap()
            .remove("execution_path");
        wire["cwd"] = serde_json::json!("/work/repo");
        assert!(!validator.is_valid(&wire));
        let mut missing = required_hire_fields();
        missing.as_object_mut().unwrap().remove("cwd");
        assert!(!validator.is_valid(&missing));
    }

    /// an old client omitting `isolation` keeps working, while a client that
    /// selects a mode gets that typed choice back on the wire; `None` adds no key.
    #[test]
    fn hire_isolation_is_additive_and_skip_none() {
        let absent: HireParams = serde_json::from_value(required_hire_fields()).unwrap();
        assert_eq!(absent.isolation, None);
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("isolation")
                .is_none()
        );

        let mut explicit = required_hire_fields();
        explicit["isolation"] = serde_json::json!("bare");
        let explicit: HireParams = serde_json::from_value(explicit).unwrap();
        assert_eq!(explicit.isolation, Some(IsolationMode::Bare));
        assert_eq!(
            serde_json::to_value(explicit).unwrap()["isolation"],
            serde_json::json!("bare")
        );
    }

    /// an old client omitting `trusted` keeps working and is untrusted; a
    /// declaration round-trips; an untrusted hire adds no key.
    #[test]
    fn hire_trusted_is_additive_and_absence_means_untrusted() {
        let absent: HireParams = serde_json::from_value(required_hire_fields()).unwrap();
        assert!(!absent.trusted);
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("trusted")
                .is_none()
        );

        let mut declared = required_hire_fields();
        declared["trusted"] = serde_json::json!(true);
        let declared: HireParams = serde_json::from_value(declared).unwrap();
        assert!(declared.trusted);
        assert_eq!(
            serde_json::to_value(declared).unwrap()["trusted"],
            serde_json::json!(true)
        );
    }
}
