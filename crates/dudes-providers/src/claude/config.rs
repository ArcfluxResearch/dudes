//! `[providers.claude]`: the operator's typed overrides for the Claude Code adapter.
//!
//! Only the overrides the adapter composes today are declared. Everything is a typed value;
//! the argv is assembled by the adapter from these parts, never concatenated from text.
//!
//! **Operator config fails closed.** Every type here is `deny_unknown_fields`, which is the
//! opposite of the rule for an inbound vendor payload (`#[serde(default)]`, never
//! `deny_unknown_fields` — `dudes-proto`'s `providers` module states it). The two rules differ
//! because the risks are opposite: a vendor may add a field at any time and a reader that
//! refuses one stops working, so an unknown field there is ignored; an operator's file is
//! finite and hand-written, and a key we silently ignore is a setting the operator believes
//! they made. `permision_mode = "default"` is the case that motivated this: misread as an
//! unknown key it leaves the default [`PermissionMode::Bypass`] standing, so a typo *widens*
//! the permission surface with no diagnostic. It is now an error.
//!
//! On the three enums the attribute is forward-looking, not load-bearing: measured, removing it
//! from any reddens nothing, because `deny_unknown_fields` governs a struct's (or a struct
//! variant's) fields, and what refuses `permission_mode = "defualt"` today is serde's ordinary
//! unknown-variant error. It is carried so a variant that later gains fields denies unknown
//! ones from the start. The behaviour it is mistaken for is pinned by a test either way — that
//! test is the guarantee, this attribute is not.

use std::ffi::{OsStr, OsString};

use dudes_types::IsolationMode;
use serde::{Deserialize, Serialize};

use crate::launch::EnvVar;

/// The argv parts the adapter composes itself, which an `extra_args` may therefore not carry.
///
/// **Three groups, and the reason is the same for all three.** An `extra_args` naming a part the
/// adapter composes is refused at hire rather than appended, because appended it would arrive
/// *after* the adapter's own copy and **win** — measured on Claude Code 2.1.269, where a second
/// `--settings` silently replaces the first (a run naming a missing file second fails with
/// `Settings file not found`, and the same pair reversed exits 0 without ever opening it), and a
/// second `--session-id` and a second `--output-format` resolve the same way. `extra_args` is
/// appended at [`ClaudeAdapter::argv`](crate::claude::managed::ClaudeAdapter)'s last step, so an
/// operator's duplicate is always the last occurrence.
///
/// 1. **the managed-turn isolation set** — `--bare`, `--setting-sources`, `--strict-mcp-config`,
///    `--settings` — composed from [`ClaudeConfig::isolation`] and not concatenated by the
///    operator. A second `--settings` would displace the file that carries `disableAllHooks`.
/// 2. **The typed overrides and their accepted aliases** — `--mcp-config`, and the two
///    spellings the vendor accepts of each of `--disallowedTools` and `--allowedTools`, plus
///    `--tools`. The kebab aliases are never *composed*; they are reserved because the vendor
///    treats them as the same option, which is why this list is a superset of the composition
///    rather than equal to it.
/// 3. **Every remaining flag the composition emits** — `-p`, `--output-format`,
///    `--verbose`, `--session-id`, `--resume`, `--max-turns`, `--max-budget-usd`,
///    `--dangerously-skip-permissions` — with `--print` and `-r`, the vendor's own spellings of
///    `-p` and `--resume`, on the same grounds as group 2. Two of these touch a recorded fact
///    rather than a preference: duplicating `--max-turns` or `--max-budget-usd` displaces the
///    per-agent backstop recorded on `AgentHired`, which the adapter composes from that record
///    alone — **not** an admission ceiling, since neither value enters gate
///    5's arithmetic or anything invariant 4 governs — and duplicating `--session-id` or
///    `--resume` replaces the session binding a turn's `Observed` provenance rests on. `--dangerously-skip-permissions` is reserved for the operator who did **not**
///    ask for it: it is composed only under [`PermissionMode::Bypass`], so a set computed from
///    one configuration would leave it admissible for the one operator who chose prompts.
///
/// `--` belongs to none of the three and is here for the opposite failure. The
/// adapter's own separator is tied to its own task element, so it never emits a `--` the task
/// does not follow — but `extra_args` is appended after that block, and in the stdin form the
/// task is not in the argv at all, so an operator's trailing `--` leaves the argv ending in a
/// separator nothing follows. That is the one shape the composition cannot produce and the
/// operator can.
///
/// **What the separator does to this vendor is measured, and as far as these runs reach it is
/// nothing**: it changes neither the input check nor the auth outcome. Every row below stops at
/// authentication, so what a run that gets past that would do with a dangling separator is
/// unmeasured and is not claimed here. Claude Code 2.1.252:
///
/// ```text
/// cfg=$(mktemp -d)
/// env -u CLAUDECODE -u CLAUDE_CODE_SESSION_ID -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_PID \
///     -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_EFFORT -u CLAUDE_CODE_EXECPATH \
///     CLAUDE_CONFIG_DIR="$cfg" ANTHROPIC_API_KEY=sk-ant-invalid \
///     claude -p --bare --max-turns 1 -- <<<'say hi'
/// ```
///
/// The scratch `CLAUDE_CONFIG_DIR` keeps the operator's own configuration out of the run; the
/// seven cleared names are [`NESTING_VARS`](super::managed::NESTING_VARS); and the bogus key is
/// what makes it free — `--bare` reads no OAuth credential and no keychain, so nothing real is
/// presented and no turn is charged. Each row is that command with the trailing `--` kept or
/// dropped, and with `<&-` in place of the here-string for a closed stdin:
///
/// | stdin    | trailing `--` | outcome                                             |
/// |----------|---------------|-----------------------------------------------------|
/// | closed   | yes           | rc=1, `Input must be provided … when using --print` |
/// | closed   | no            | rc=1, the same error                                |
/// | `say hi` | yes           | rc=1, `Invalid API key`                             |
/// | `say hi` | no            | rc=1, `Invalid API key`                             |
///
/// The separator changes neither pair: what decides the outcome is whether stdin carries the
/// prompt. The first two rows are what make the last two mean something — the input check
/// precedes the auth check, so reaching `Invalid API key` is proof the prompt was found rather
/// than proof the key was rejected early. They also foreclose a misreading of the first row: labelled
/// only "`--` with stdin closed", it reads as the separator having caused the failure. Row two shows the failure is empty stdin's.
///
/// The negative control, `--definitely-not-a-flag`, exits 1 with `unknown option`, so the
/// oracle can see a parse failure. **Where on the command line it sat is not recorded**, and
/// that gap is sharper here than it looks: this identical token was rejected in 0 s
/// on its own and *accepted as the prompt* after a `--` at 2.1.251, so position is the whole difference
/// between a control and a prompt. The outcome constrains it — a run reporting `unknown option`
/// cannot have had the token shielded by a separator, if that 2.1.251 behaviour held at
/// 2.1.252, which is itself unmeasured — but a re-derivation should place it before the
/// separator, where it is a control by construction rather than by inference.
///
/// Inert on one version is not a contract, and it is not the reason for the entry: the adapter
/// must not compose an argv element it cannot account for, and a separator this vendor ignores
/// is one the next one — or the next release — may read as the start of a prompt.
///
/// The isolation set enumerates the isolation spellings; the separator, the typed override flags
/// and the rest of the composition extend that set. **Whether a denylist is the right shape at
/// all is an open question, and adding entries does not answer that** —
/// Widening the membership left the shape alone, naming four leaks it cannot close:
/// a spelling the *vendor* adds (neither test here reads the vendor); a short cluster, since
/// `claude -pv` reaches `-p` while [`reserved`] matches `-p` by equality or `-p=`;
/// `--allow-dangerously-skip-permissions`, a separate option this adapter never composes; and a
/// flag that is not composed at all but contradicts a recorded fact, of which `--model` is the
/// live one (a managed turn's model goes in the settings file, not the argv; only an attached
/// launch passes `--model`, with the model the agent works on).
pub const RESERVED_EXTRA_ARGS: &[&str] = &[
    "--bare",
    "--setting-sources",
    "--strict-mcp-config",
    "--settings",
    "--mcp-config",
    "--disallowedTools",
    "--disallowed-tools",
    "--allowedTools",
    "--allowed-tools",
    "--tools",
    "-p",
    "--print",
    "--output-format",
    "--verbose",
    "--include-partial-messages",
    "--session-id",
    "--resume",
    "-r",
    "--max-turns",
    "--max-budget-usd",
    "--dangerously-skip-permissions",
    "--",
];

/// The one API-key path this crate can see. There are two — this variable in the
/// daemon's environment, or an `apiKeyHelper` "in the `--settings` JSON" — and the second has
/// no route today: `managed-settings.json` is fixed at exactly `disableAllHooks`,
/// `autoMemoryEnabled` and `model`, checked by whole-file equality, so no helper key can reach
/// it and no field of this table produces one. [`ConfigError::BareWithoutApiKey`] therefore
/// says which path it looked for, rather than implying a config route that does not exist.
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The environment variables that mean a Claude turn can be billed.
///
/// Every name is documented as carrying a credential the vendor sends — an API key, a bearer
/// token, or a cloud-backend key (<https://code.claude.com/docs/en/env-vars>, fetched
/// 2026-09-12). [`API_KEY_VAR`] stays first because it is the one name the bare-mode check also
/// asks about; the rest are here because a managed child inherits them verbatim (`scrub_env`
/// drops only the `CLAUDE*` nesting names), so each one is a route by which a turn this daemon
/// issues can reach an invoice.
///
/// **This is not [`API_KEY_VAR`]'s question wearing a second name.** That one asks *can a bare turn
/// authenticate at all*, and answers the bare-mode check at [`ClaudeConfig::check`]; this one asks
/// *can this daemon's environment be billed*, and answers the admission the daemon makes before it
/// journals a hire or a goal. Merging them would make a bare-mode configuration error and a
/// billing-visibility fact the same value.
///
/// A name is only a credential here when it is present **and non-empty** — the predicate
/// [`carries_api_key`] already holds, and the reader of this list keeps.
///
/// # What qualifies for this list, and the routes that stay open
///
/// **A rule, not a count.** A name belongs here when the vendor documents it as *carrying* a
/// billing credential — the value in the daemon's environment is the key, the bearer token or the
/// backend key itself. A route whose credential resolves out of band is outside this list **by
/// construction**, and so is a name that merely *selects* such a credential: reading it says which
/// store the CLI will trust, not that a bill is running. The list grows when the page documents
/// another carried credential, never by promoting a selector.
///
/// So these routes stay open, which is why the refusal is a floor, not a
/// boundary*: an environment this list calls unbillable may still be billed by one of them.
///
/// * **The cloud backends' own credential chains.** The page gives `ANTHROPIC_AWS_API_KEY`
///   precedence *over AWS `SigV4`* and `ANTHROPIC_FOUNDRY_AUTH_TOKEN` *over the Azure default
///   credential chain*; Vertex application-default credentials resolve from a well-known file or a
///   metadata server. Each chain can bill with no name here to read, and
///   `ANTHROPIC_VERTEX_PROJECT_ID` addresses a project rather than carrying a credential.
/// * **Profile and federation selection.** `ANTHROPIC_PROFILE` names a stored profile, and
///   `ANTHROPIC_FEDERATION_RULE_ID` with `ANTHROPIC_ORGANIZATION_ID` select federation credentials
///   the page says "rank above your `/login` credential". All three are selectors; the credential
///   each reaches is not in the environment.
/// * **`ANTHROPIC_BASE_URL`, documented on the same page and deliberately not a member.** A
///   loopback proxy sets exactly the variable a paid gateway sets, so presence does not imply
///   billing and nothing this crate observes separates the two; deciding it by host, by an
///   operator-declared config key, or by dialling the host at admission are three different
///   decisions, each a record's rather than a list's. What keeps the deferral defensible: the
///   ordinary metered-gateway configuration exports a credential beside the base URL, and that
///   credential is `ANTHROPIC_AUTH_TOKEN` or [`API_KEY_VAR`] — both here. The residual is the
///   gateway that authenticates by network identity alone.
///
/// # This query is the wide one
///
/// The vendor also reports a credential source of its own, on the `system/init` line of a turn it
/// has already started. Measured on Claude Code 2.1.269 (2026-09-12, one fixture value per run):
/// with `ANTHROPIC_API_KEY` exported that field reads `"ANTHROPIC_API_KEY"`, and with
/// `ANTHROPIC_AUTH_TOKEN` exported it reads `"none"` — on a run whose token the vendor did send,
/// because the API answered `401 Invalid bearer token`. So that field names one member of this
/// list rather than the list, and a consumer of it is a second detector of an overlapping class,
/// never a fallback for this one.
pub(crate) const BILLING_ENV_VARS: &[&str] = &[
    API_KEY_VAR,
    "ANTHROPIC_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
];

/// The permission surface of a managed turn. Dudes' own permission surface is the argv, never
/// the repository's settings file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionMode {
    /// The vendor's default: a managed turn with no prompt to answer is denied the tool.
    Default,
    /// `--dangerously-skip-permissions`: the loop is unattended, so nothing waits on a prompt.
    #[default]
    Bypass,
}

/// One Claude settings source, encoded into the comma-separated value of
/// `--setting-sources`. Closed and typed so operator misspellings fail configuration parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingSource {
    /// User-wide settings.
    User,
    /// Settings from the agent's repository.
    Project,
    /// Machine-local project settings.
    Local,
}

impl SettingSource {
    /// The vendor's spelling inside `--setting-sources`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// The `[providers.claude]` table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    /// The program; resolved by PATH lookup in the daemon, never by a shell.
    pub command: OsString,
    /// The permission surface.
    pub permission_mode: PermissionMode,
    /// A pinned model, written into `managed-settings.json` when set.
    pub model: Option<String>,
    /// Isolation of the managed turn.
    pub isolation: IsolationMode,
    /// Settings sources to load. `None` resolves from the recorded repository trust:
    /// untrusted to empty, trusted to `project`.
    pub setting_sources: Option<Vec<SettingSource>>,
    /// Tool rules denied by the vendor.
    pub disallowed_tools: Vec<String>,
    /// Tool rules allowed without a prompt.
    pub allowed_tools: Vec<String>,
    /// Built-in tools made available to the turn.
    pub tools: Vec<String>,
    /// Escape hatch: typed parts appended last — before `--` when the task is in the argv, and
    /// at the end of the argv when it is on stdin. Still no shell.
    ///
    /// [`ClaudeConfig::check`] is the static table validation, before any agent exists; per-agent
    /// repository-trust validation happens when `managed` receives `AgentCtx`. `argv` appends this
    /// vector as it stands, so a part that must never reach an argv has to be in
    /// [`RESERVED_EXTRA_ARGS`] — a trailing `--` is there because nothing downstream would catch
    /// it.
    pub extra_args: Vec<OsString>,
    /// The largest task, in bytes, that still travels as one argv element. A task of exactly
    /// this many bytes is the last element after `--`; one byte more is delivered on
    /// [`LaunchSpec::stdin`](crate::launch::LaunchSpec::stdin) and the argv ends at the last
    /// flag. The vendor takes the prompt either way — "as an argument or piped on stdin"
    /// (<https://code.claude.com/docs/en/headless>, fetched 2026-08-22) — so the choice is
    /// about the kernel, not about the CLI.
    ///
    /// A ceiling is needed because one argv element is bounded and the bound is not a constant
    /// this crate can compute. Linux caps a single element at `MAX_ARG_STRLEN`, 32 pages =
    /// 131072 bytes (`include/uapi/linux/binfmts.h`; `execve(2)` then fails `E2BIG`) — read,
    /// not measured here. macOS has no separate per-element cap at all: measured on Darwin
    /// 23.1.0 with `kern.argmax` = 1048576, the largest single element that execs was
    /// **1048506** bytes with a two-variable environment and **848484** with a 200008-byte
    /// variable added — the environment spends the same budget, byte for byte. That is the
    /// reason this is a configured number rather than a derived one: the adapter passes the
    /// daemon's whole environment through ([`scrub_env`](crate::claude::scrub_env) removes
    /// only the nesting variables), so the true ceiling moves with the operator's shell and
    /// cannot be known where the argv is composed.
    ///
    /// The default is the smaller platform's half: well under Linux's 131072 with room for a
    /// task that grew past its estimate, and far enough under macOS's measured figure that no
    /// plausible environment closes the gap.
    pub task_argv_max_bytes: usize,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            command: OsString::from("claude"),
            permission_mode: PermissionMode::default(),
            model: None,
            isolation: IsolationMode::default(),
            setting_sources: None,
            disallowed_tools: Vec::new(),
            allowed_tools: Vec::new(),
            tools: Vec::new(),
            extra_args: Vec::new(),
            task_argv_max_bytes: 65536,
        }
    }
}

/// Why a `[providers.claude]` table was refused. Operator config fails closed: a setting we
/// would ignore, or one whose promise the adapter cannot keep, is an error at hire and not a
/// surprise at the first turn.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `extra_args` carries one of [`RESERVED_EXTRA_ARGS`].
    ///
    /// "composes itself" rather than "composes from `isolation`", which is true of the
    /// isolation flags and false of `--`: the separator comes from the task, not from the
    /// mode. An operator reading the message should not be sent to the wrong setting.
    #[error(
        "reserved argv part `{flag}` in extra_args, which the adapter composes itself; \
         expected an argument other than {}",
        RESERVED_EXTRA_ARGS.join(", ")
    )]
    ReservedFlag {
        /// The reserved part, as this crate spells it: `--settings` for both `--settings <p>`
        /// and `--settings=<p>`.
        flag: String,
    },
    /// `isolation = "bare"` with no API-key path for the adapter to see.
    #[error(
        "isolation = \"bare\" needs an API-key path: expected a non-empty {} in the daemon \
         environment, because bare mode reads no OAuth credential and no keychain",
        API_KEY_VAR
    )]
    BareWithoutApiKey,
    /// A configured model name that starts with `-`, which an attached launch would pass right
    /// after `--model` and the vendor could read as a flag.
    #[error("the configured model `{model}` starts with `-`; expected a model name")]
    ModelLooksLikeFlag {
        /// The configured name.
        model: String,
    },
    /// The model is set but empty, which an attached launch could not pass after `--model`.
    #[error("the configured model is empty; leave `model` out to use the engine's own model")]
    EmptyModel,
}

impl ClaudeConfig {
    /// The model `extra_args` name with `--model <x>` or `--model=<x>`, the last one winning as it
    /// does on the command line. Appended after the settings file, it overrides that file's model.
    #[must_use]
    pub fn extra_args_model(&self) -> Option<OsString> {
        let mut found = None;
        let mut args = self.extra_args.iter();
        while let Some(arg) = args.next() {
            if arg == "--model" {
                found = Some(args.next().cloned().unwrap_or_default());
            } else if let Some(value) = arg.to_str().and_then(|text| text.strip_prefix("--model="))
            {
                found = Some(OsString::from(value));
            }
        }
        found
    }

    /// Whether this table is one the adapter can honour, given the environment the daemon will
    /// hand a managed child. Pure: the environment is an argument, nothing is read here.
    ///
    /// Two rules, reported first-failing-first: no `extra_args` may carry an argv part the
    /// adapter composes itself, and [`IsolationMode::Bare`] needs an API-key path. Args are matched as parsed elements —
    /// `--settings` and `--settings=<p>`, never a scan of a joined string (invariant 7).
    ///
    /// # Errors
    ///
    /// [`ConfigError::ReservedFlag`] or [`ConfigError::BareWithoutApiKey`].
    pub fn check(&self, daemon_env: &[EnvVar]) -> Result<(), ConfigError> {
        if let Some(flag) = self.extra_args.iter().find_map(|arg| reserved(arg)) {
            return Err(ConfigError::ReservedFlag {
                flag: flag.to_owned(),
            });
        }
        if self.isolation == IsolationMode::Bare && !carries_api_key(daemon_env) {
            return Err(ConfigError::BareWithoutApiKey);
        }
        let pinned = self.extra_args_model();
        let named = self
            .model
            .as_deref()
            .into_iter()
            .chain(pinned.as_deref().and_then(OsStr::to_str));
        for model in named {
            if model.is_empty() {
                return Err(ConfigError::EmptyModel);
            }
            if model.starts_with('-') {
                return Err(ConfigError::ModelLooksLikeFlag {
                    model: model.to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The reserved flag `arg` is, in either of the two forms the vendor accepts: the flag alone,
/// or `<flag>=<value>`.
///
/// That the second form is a form at all is checked, not assumed: on Claude Code 2.1.251
/// (2026-08-30) `claude --settings=/tmp/nope.json --help` exits 1 with
/// `Error: Settings file not found: /tmp/nope.json`, so the value was parsed off the flag. The
/// control matters more than the case — `claude --settings-file=/tmp/nope.json --help` prints
/// the help text, because `--help` short-circuits an unknown flag rather than rejecting it. It
/// is the file error, never the help, that answers this question; and the same pair shows
/// `--settings-file` is not a vendor flag, which is what the accepted control in the test below
/// stands on.
///
/// Compared as bytes because [`OsStr`] is not always UTF-8; every reserved part is ASCII, and
/// an ASCII prefix of an encoded [`OsStr`] compares correctly on both platforms — the same
/// comparison the nesting scrub makes. `--settings-of-mine` is not `--settings`: without the
/// `=` the match is equality, so a longer flag is not swept in.
///
/// `--` is what makes that last sentence load-bearing rather than decorative. It is a prefix of
/// every long flag, so the `=` requirement is the only thing standing between this function and
/// refusing the whole escape hatch; the accepted arm of
/// `a_bare_separator_in_extra_args_is_refused` is what pins that, and it runs first so a
/// mutation reaches it. The one thing the second form does reach for `--` is `--=<value>`,
/// which is not a spelling of any vendor flag.
fn reserved(arg: &OsStr) -> Option<&'static str> {
    let bytes = arg.as_encoded_bytes();
    RESERVED_EXTRA_ARGS.iter().copied().find(|flag| {
        let flag = flag.as_bytes();
        bytes == flag || (bytes.starts_with(flag) && bytes.get(flag.len()) == Some(&b'='))
    })
}

/// Whether the daemon's environment carries an API key for a bare turn. Set-but-empty is not a
/// key. The value is touched only to ask whether it is empty; nothing here copies or reports it.
fn carries_api_key(daemon_env: &[EnvVar]) -> bool {
    daemon_env
        .iter()
        .any(|var| var.name() == OsStr::new(API_KEY_VAR) && !var.expose_value().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A misspelled key is an error, not a silently defaulted setting. The motivating case is
    /// `permision_mode`: ignored, it leaves [`PermissionMode::Bypass`] standing, so the typo
    /// *widens* the permission surface — the operator believes they asked for prompts and got
    /// `--dangerously-skip-permissions`. The positive control is the correctly spelled key,
    /// which must still parse and must still change the value.
    #[test]
    fn a_misspelled_operator_key_is_refused_rather_than_silently_defaulted() {
        let typo = serde_json::from_str::<ClaudeConfig>(r#"{"permision_mode":"default"}"#)
            .expect_err("a misspelled key must not parse");
        assert!(
            typo.to_string().contains("permision_mode"),
            "the error names the offending key, got: {typo}"
        );
        for key in [
            "comand",
            "modl",
            "isolaton",
            "setting_source",
            "mcp_configs",
            "mcp_config",
            "disallow_tools",
            "allow_tools",
            "tool",
            "extra_arg",
            "permissionMode",
        ] {
            assert!(
                serde_json::from_str::<ClaudeConfig>(&format!(r#"{{"{key}":"x"}}"#)).is_err(),
                "unknown key {key} must be refused"
            );
        }

        let spelled = serde_json::from_str::<ClaudeConfig>(r#"{"permission_mode":"default"}"#)
            .expect("the correctly spelled key still parses");
        assert_eq!(spelled.permission_mode, PermissionMode::Default);
        assert_ne!(
            spelled.permission_mode,
            ClaudeConfig::default().permission_mode,
            "the control is only a control if the key actually moves the value"
        );
        assert_eq!(
            serde_json::from_str::<ClaudeConfig>("{}").unwrap(),
            ClaudeConfig::default(),
            "an empty table is still every default: deny_unknown_fields is not deny_missing"
        );
    }

    /// A misspelled *value* is refused for the same reason, at every enum in the table: an
    /// unknown variant is an error rather than the `#[default]`. Same failure shape as the
    /// key typo — `permission_mode = "defualt"` must not leave `Bypass` standing.
    #[test]
    fn a_misspelled_enum_value_is_refused_rather_than_silently_defaulted() {
        for bad in ["defualt", "bypas", "Default", "skip"] {
            let err = serde_json::from_str::<PermissionMode>(&format!("\"{bad}\""))
                .expect_err("an unknown permission mode must not parse");
            assert!(err.to_string().contains("unknown variant"), "got: {err}");
            assert!(
                serde_json::from_str::<ClaudeConfig>(&format!(r#"{{"permission_mode":"{bad}"}}"#))
                    .is_err(),
                "and it is refused through the table too"
            );
            assert!(serde_json::from_str::<IsolationMode>(&format!("\"{bad}\"")).is_err());
        }
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"default\"").unwrap(),
            PermissionMode::Default,
        );
        assert_eq!(
            serde_json::from_str::<IsolationMode>("\"inherit\"").unwrap(),
            IsolationMode::Inherit,
        );
    }

    /// A config *this crate serialised* is one it reads back, every field populated — so
    /// `deny_unknown_fields` refuses nothing we emit. That is the whole claim; note what it is
    /// not.
    ///
    /// It round-trips serde's own encoding, not operator text, and for two fields those differ.
    /// `command` and `extra_args` are [`OsString`], which serde encodes as a platform-tagged
    /// byte array — the default `command` emits as `{"Unix":[99,108,97,117,100,101]}`, not
    /// `"claude"`. Measured: `{"command":"claude"}` and `{"extra_args":["--foo"]}` both fail with
    /// serde's "unknown variant ..., expected Unix or Windows", while `{"model":"haiku"}` and
    /// `{"task_argv_max_bytes":1024}` parse — so 8 of the 10 fields accept the text an operator
    /// would write and 2 do not. Round-tripping cannot see this, because it never presents
    /// operator text to the reader; the two `from_str` lines at the end of the body do, so the
    /// count above is checked rather than asserted in prose.
    ///
    /// That gap is a wire-format question — changing the field types is not something to fix by
    /// loosening the reader.
    #[test]
    fn every_field_round_trips_through_the_denying_reader() {
        let config = ClaudeConfig {
            command: OsString::from("/usr/local/bin/claude"),
            permission_mode: PermissionMode::Default,
            model: Some("haiku".into()),
            isolation: IsolationMode::Inherit,
            setting_sources: Some(vec![SettingSource::User, SettingSource::Local]),
            disallowed_tools: vec!["Bash(rm *)".to_owned()],
            allowed_tools: vec!["Read".to_owned()],
            tools: vec!["Read".to_owned(), "Edit".to_owned()],
            extra_args: vec![
                OsString::from("--max-thinking-tokens"),
                OsString::from("100"),
            ],
            task_argv_max_bytes: 1024,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<ClaudeConfig>(&json).unwrap(), config);

        // The ceiling is the one new field an operator can also write as plain text, which is
        // the half of the comment above that round-tripping cannot check. The control is the
        // field that cannot: `command` in the same shape must still fail.
        assert_eq!(
            serde_json::from_str::<ClaudeConfig>(r#"{"task_argv_max_bytes":1024}"#)
                .expect("a number is a number in either encoding")
                .task_argv_max_bytes,
            1024
        );
        assert!(
            serde_json::from_str::<ClaudeConfig>(r#"{"command":"claude"}"#).is_err(),
            "the control: an OsString field still refuses the text an operator would write"
        );
        let plain = serde_json::from_str::<ClaudeConfig>(
            r#"{"setting_sources":["user","local"],"disallowed_tools":["Bash(rm *)"],"allowed_tools":["Read"],"tools":["Read","Edit"]}"#,
        )
        .expect("the four typed overrides accept ordinary operator text");
        assert_eq!(
            plain.setting_sources,
            Some(vec![SettingSource::User, SettingSource::Local])
        );
        assert_eq!(plain.disallowed_tools, ["Bash(rm *)"]);
        assert_eq!(plain.allowed_tools, ["Read"]);
        assert_eq!(plain.tools, ["Read", "Edit"]);
        assert!(
            serde_json::from_str::<ClaudeConfig>(r#"{"setting_sources":["workspace"]}"#).is_err(),
            "an unknown setting source is refused"
        );
    }

    /// A model in `extra_args` is found in both spellings, the last one wins, and a flag with no
    /// value still counts as fixing one.
    #[test]
    fn a_model_in_extra_args_is_found_in_both_spellings() {
        let model = |args: &[&str]| {
            ClaudeConfig {
                extra_args: args.iter().map(OsString::from).collect(),
                ..ClaudeConfig::default()
            }
            .extra_args_model()
        };
        assert_eq!(model(&[]), None);
        assert_eq!(model(&["--add-dir", "/tmp/x", "--modelx"]), None);
        assert_eq!(model(&["--model", "haiku"]), Some("haiku".into()));
        assert_eq!(model(&["--model=haiku"]), Some("haiku".into()));
        assert_eq!(model(&["--model", "a", "--model=b"]), Some("b".into()));
        assert_eq!(model(&["--model"]), Some(OsString::new()));
    }

    /// the two managed-run limit spellings are refused in `extra_args` — bare
    /// and `--flag=value` — and reserving them widens nothing else.
    ///
    /// **Why this exists beside
    /// [`extra_args_carrying_a_flag_the_adapter_composes_is_refused`], which iterates the whole
    /// constant.** That test's refusal loop is generated *from* [`RESERVED_EXTRA_ARGS`], so it can
    /// only ever assert about whatever the constant currently says; its hand-written table is what
    /// pins membership, and membership is the half that is fixed by the run-limit flags. This test is the *behaviour*
    /// of those two names, written out as the four exact strings the run limits produce — including the
    /// padded decimal `--max-budget-usd=2.500000` that `Usd::to_decimal` actually emits, which is
    /// the spelling an operator would copy out of a composed argv and paste into their config.
    ///
    /// **The positive control is the point, not a courtesy.** `--max-thinking-tokens 100` is a
    /// real vendor flag this adapter never composes, and it shares a `--max-` prefix with both
    /// reserved names. A [`reserved`] that matched by prefix rather than by equality-or-`=` would
    /// refuse it, turning a two-name reservation into a deny class over every `--max-*` flag the
    /// vendor has or later adds. It runs **first** deliberately: a control placed after a failing
    /// subject never executes, and so proves nothing about the run that failed.
    #[test]
    fn managed_limit_spellings_are_reserved_without_widening_extra_args() {
        let allowed = ClaudeConfig {
            extra_args: ["--max-thinking-tokens", "100"]
                .map(OsString::from)
                .to_vec(),
            ..ClaudeConfig::default()
        };
        assert_eq!(
            allowed.check(&[]),
            Ok(()),
            "an unrelated vendor flag sharing the `--max-` prefix stays available: reserving two \
             names is not a deny class over every flag that begins like them"
        );

        for (written, flag) in [
            ("--max-turns", "--max-turns"),
            ("--max-turns=3", "--max-turns"),
            ("--max-budget-usd", "--max-budget-usd"),
            ("--max-budget-usd=2.500000", "--max-budget-usd"),
        ] {
            let config = ClaudeConfig {
                extra_args: vec![OsString::from(written)],
                ..ClaudeConfig::default()
            };
            assert_eq!(
                config.check(&[]),
                Err(ConfigError::ReservedFlag {
                    flag: flag.to_owned()
                }),
                "`{written}` must be refused: the adapter composes {flag} from the recorded \
                 `AgentHired` field, so a raw copy in `extra_args` would duplicate or override it"
            );
        }
    }

    /// an `extra_args` carrying a flag the adapter composes is refused, in
    /// both forms the vendor accepts (`--settings <p>` and `--settings=<p>`) and wherever it
    /// sits in the vector. Appended, such a flag arrives *after* the adapter's own copy and
    /// wins — a second `--settings` displacing the file that carries `disableAllHooks`.
    ///
    /// The second half is the arm that makes the first mean something: a `check()` that
    /// refused every table would pass the loop above. The controls are the escape hatch still
    /// working — `--max-thinking-tokens 100`, two other real vendor flags, and
    /// `--settings-file`, which is not a vendor flag at all but is the shape a prefix match
    /// would over-refuse.
    #[test]
    fn extra_args_carrying_a_flag_the_adapter_composes_is_refused() {
        for flag in RESERVED_EXTRA_ARGS {
            for written in [(*flag).to_owned(), format!("{flag}=x")] {
                let config = ClaudeConfig {
                    extra_args: vec![OsString::from(&written)],
                    ..ClaudeConfig::default()
                };
                let err = config
                    .check(&[])
                    .expect_err("a reserved flag must be refused");
                assert_eq!(
                    err,
                    ConfigError::ReservedFlag {
                        flag: (*flag).to_owned()
                    },
                    "`{written}` carries {flag}"
                );
                let message = err.to_string();
                assert!(
                    message.contains(flag),
                    "the message names the flag: {message}"
                );
                for reserved in RESERVED_EXTRA_ARGS {
                    assert!(
                        message.contains(reserved),
                        "the message must list every reserved flag, and omits \
                         {reserved}: {message}"
                    );
                }
            }
        }
        // **The failing case per reserved name, and it must stay literal.** The loop above
        // iterates `RESERVED_EXTRA_ARGS` itself, so it is a fixture derived from the list it
        // guards: drop an entry and it simply stops generating that case and goes green. This
        // table is written out by hand, so dropping an entry reddens exactly one row and names
        // it. Measured: removing `--disallowed-tools` from the constant fails here and leaves
        // `every_flag_the_adapter_composes_is_reserved_in_extra_args` green, because that alias
        // is reserved and never composed — which is why both tests exist.
        for required in [
            "--bare",
            "--setting-sources",
            "--strict-mcp-config",
            "--settings",
            "--mcp-config",
            "--disallowedTools",
            "--disallowed-tools",
            "--allowedTools",
            "--allowed-tools",
            "--tools",
            "-p",
            "--print",
            "--output-format",
            "--verbose",
            "--include-partial-messages",
            "--session-id",
            "--resume",
            "-r",
            "--max-turns",
            "--max-budget-usd",
            "--dangerously-skip-permissions",
            "--",
        ] {
            assert!(
                RESERVED_EXTRA_ARGS.contains(&required),
                "a composed flag or its accepted alias is no longer reserved: {required}"
            );
        }
        let buried = ClaudeConfig {
            extra_args: [
                "--max-thinking-tokens",
                "100",
                "--settings",
                "/tmp/mine.json",
            ]
            .map(OsString::from)
            .to_vec(),
            ..ClaudeConfig::default()
        };
        assert!(
            buried.check(&[]).is_err(),
            "a reserved flag is refused wherever it sits, not only first"
        );

        for allowed in [
            vec![],
            vec!["--max-thinking-tokens", "100"],
            vec!["--add-dir", "/tmp/other"],
            vec!["--settings-file"],
            // `--model` is a real vendor flag the managed launch **never composes** — a pinned
            // model travels in `managed-settings.json` — so it stays admissible, and
            // that it does is a decision rather than an oversight. The residual risk is
            // that an operator writing it here would pin a model the journal does not record.
            vec!["--model", "haiku"],
            // The two new short/long entries make `-p` and `--print` prefixes of other tokens
            // for the first time. Without the `=` requirement in `reserved`, a prefix match
            // would sweep both of these in.
            vec!["-pq"],
            vec!["--printer"],
        ] {
            let config = ClaudeConfig {
                extra_args: allowed.iter().map(OsString::from).collect(),
                ..ClaudeConfig::default()
            };
            config
                .check(&[])
                .unwrap_or_else(|err| panic!("{allowed:?} must still pass: {err}"));
        }
    }

    #[test]
    fn a_configured_model_that_starts_with_a_dash_is_refused() {
        let refused = |config: ClaudeConfig| config.check(&[]).is_err();
        assert!(refused(ClaudeConfig {
            model: Some("-x".into()),
            ..ClaudeConfig::default()
        }));
        for args in [vec!["--model", "-x"], vec!["--model=-x"]] {
            assert!(refused(ClaudeConfig {
                extra_args: args.into_iter().map(OsString::from).collect(),
                ..ClaudeConfig::default()
            }));
        }
        assert!(!refused(ClaudeConfig {
            model: Some("haiku".into()),
            ..ClaudeConfig::default()
        }));
    }

    #[test]
    fn an_empty_configured_model_is_refused() {
        let empty_key = ClaudeConfig {
            model: Some(String::new()),
            ..ClaudeConfig::default()
        };
        assert!(matches!(empty_key.check(&[]), Err(ConfigError::EmptyModel)));
        let empty_pin = ClaudeConfig {
            extra_args: ["--model", ""].map(OsString::from).to_vec(),
            ..ClaudeConfig::default()
        };
        assert!(matches!(empty_pin.check(&[]), Err(ConfigError::EmptyModel)));
    }

    /// a bare `--` in `extra_args` is refused, because appended it composes a
    /// separator the adapter itself never emits. `argv` ties its own `--` to its own task
    /// element, so it cannot push one the task does not follow; `extra_args` is appended after
    /// that block, and in the stdin form the task is not in the argv at all — so an operator's
    /// trailing `--` leaves the argv ending in a separator nothing follows.
    ///
    /// **The acceptance control runs first, deliberately.** A refusal assertion that fails
    /// skips everything after it, so a control placed below the subject never executes on the
    /// one run that most needs it — the mutation run. Put first, it shows on that same run
    /// that `check()` still admits the escape hatch rather than having started refusing
    /// everything.
    ///
    /// The refused table is literals rather than a loop over [`RESERVED_EXTRA_ARGS`], and that
    /// is what makes this test the statement of the invariant: a loop over the const covers
    /// `--` today and goes green again the moment the entry is removed, because it would
    /// simply stop generating the case.
    #[test]
    fn a_bare_separator_in_extra_args_is_refused() {
        // Control first. `--max-thinking-tokens 100` is the named arm; `---` and
        // `--x` are the same shape one step narrower, since `--` is a prefix of both. Measured,
        // a `reserved` that drops the `=` requirement and prefix-matches is caught by the first
        // of those three.
        //
        // `-` is not swept in by a prefix match at all — nothing beginning with `--` is a
        // prefix of it, and under that mutant it passes. It guards the other side of the same
        // comparison: with the operands swapped, so `reserved` asks `flag.starts_with(bytes)`,
        // every entry begins with `-` and `["-"]` is refused. Measured, it is the only arm in
        // this list that fires under that one.
        for allowed in [
            vec!["--max-thinking-tokens", "100"],
            vec!["-"],
            vec!["---"],
            vec!["--x"],
            // `-p` and `--print` are reserved, so `-` now has a two-byte entry behind it and
            // `--print` is a prefix of longer tokens. Both arms of `reserved`'s comparison are
            // controlled here for the new entries as they already were for `--`.
            vec!["-pq"],
            vec!["--printer"],
        ] {
            ClaudeConfig {
                extra_args: allowed.iter().map(OsString::from).collect(),
                ..ClaudeConfig::default()
            }
            .check(&[])
            .unwrap_or_else(|err| panic!("{allowed:?} is not a separator and must pass: {err}"));
        }

        // The subject: alone, and in the tail position the probe composed it in.
        for refused in [
            vec!["--"],
            vec!["--max-thinking-tokens", "100", "--"],
            // The one new class the `<flag>=<value>` arm reaches, recorded rather than
            // stumbled on later: `--=x` matches `--` on that arm. No vendor flag has that
            // shape, and the controls above are what show no real one is swept in with it.
            vec!["--=x"],
        ] {
            let config = ClaudeConfig {
                extra_args: refused.iter().map(OsString::from).collect(),
                ..ClaudeConfig::default()
            };
            let Err(err) = config.check(&[]) else {
                panic!("{refused:?} composes a dangling separator and must be refused");
            };
            assert_eq!(
                err,
                ConfigError::ReservedFlag {
                    flag: "--".to_owned()
                },
                "{refused:?} is refused as the separator itself"
            );

            // The operator-visible text, on substrings the message's trailing
            // `RESERVED_EXTRA_ARGS.join(", ")` cannot supply. That tail names every reserved
            // part by construction, so a `contains(flag)` over the whole message is true
            // whatever the `{flag}` slot renders and whatever the sentence says: it pins the
            // tail and nothing else. The backticked spelling pins the slot, and "composes
            // itself" pins the correction — `--` comes from the task, not from
            // `isolation`, so an operator sent to the isolation setting is sent to the wrong
            // one.
            let message = err.to_string();
            assert!(
                message.contains("part `--` in extra_args, which the adapter composes itself"),
                "the message must name the offending part and what composes it: {message}"
            );
            assert!(
                !message.contains("composes from"),
                "`--` is not composed from `isolation` and the message must not say it is: \
                 {message}"
            );
        }
    }

    /// The billing list holds the documented credential names, in order, and holding them does
    /// **not** absorb the bare-mode check the first one already answers.
    ///
    /// Two questions, two names. [`API_KEY_VAR`] asks *can a bare turn
    /// authenticate at all*; [`BILLING_ENV_VARS`] asks *can this daemon's environment be billed*.
    /// The second assertion is the control: an implementation that merged them — deleting the
    /// bare-mode path and reading the list there instead — would satisfy the first and fail this.
    ///
    /// The first assertion is written out rather than counted, so it fails by name on a dropped
    /// name, on an added one nothing sourced, and on a reordering that moves [`API_KEY_VAR`] off
    /// the front — the position the bare-mode check pins, because that is the one name the bare-mode
    /// check also reads. `ANTHROPIC_BASE_URL` is deliberately absent; the constant's own doc says
    /// why, and this list is where that decision would be quietly undone.
    #[test]
    fn the_billing_list_holds_the_documented_credential_names() {
        assert_eq!(
            BILLING_ENV_VARS,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "AWS_BEARER_TOKEN_BEDROCK",
                "ANTHROPIC_AWS_API_KEY",
                "ANTHROPIC_FOUNDRY_API_KEY",
                "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
            ],
            "the documented credential names, API_KEY_VAR first"
        );
        assert_eq!(
            BILLING_ENV_VARS[0], API_KEY_VAR,
            "the bare-mode variable stays the first element"
        );
        let bare = ClaudeConfig {
            isolation: IsolationMode::Bare,
            ..ClaudeConfig::default()
        };
        assert_eq!(
            bare.check(&[EnvVar::new("PATH", "/usr/bin")]),
            Err(ConfigError::BareWithoutApiKey),
            "the bare-mode check still asks its own question"
        );
    }

    /// `isolation = "bare"` is available only where the adapter can see an
    /// API-key path, because bare mode reads no OAuth credential and no keychain. The failure
    /// is not silent — `r7-bare` carries `"is_error": true` and `tests/golden.rs` maps that
    /// fixture to `TurnOutcome::Failed` — but it arrives dressed as `Not logged in · Please run
    /// /login` under `subtype: "success"`, and it costs a whole turn to learn something the
    /// configuration already said.
    ///
    /// Three arms, because two would not separate the rules: refused against a keyless
    /// environment, accepted once a key is there, and the other two isolation modes accepted
    /// against that same keyless environment. A `check()` that refused every table fails the
    /// second; one that ignored `isolation` fails the third.
    #[test]
    fn bare_needs_an_api_key_path_and_the_other_isolation_modes_do_not() {
        let keyless = [
            EnvVar::new("PATH", "/usr/bin"),
            EnvVar::new("CLAUDE_CONFIG_DIR", "/home/x/.claude"),
        ];
        let bare = ClaudeConfig {
            isolation: IsolationMode::Bare,
            ..ClaudeConfig::default()
        };
        assert_eq!(bare.check(&keyless), Err(ConfigError::BareWithoutApiKey));
        assert!(
            bare.check(&keyless)
                .unwrap_err()
                .to_string()
                .contains(API_KEY_VAR),
            "the message names the path it looked for"
        );
        assert_eq!(
            bare.check(&[EnvVar::new(API_KEY_VAR, "")]),
            Err(ConfigError::BareWithoutApiKey),
            "set but empty is not a key"
        );
        bare.check(&[EnvVar::new(API_KEY_VAR, "fixture-value-not-a-credential")])
            .expect("bare passes once the adapter can see a key");

        for mode in [IsolationMode::Default, IsolationMode::Inherit] {
            ClaudeConfig {
                isolation: mode,
                ..ClaudeConfig::default()
            }
            .check(&keyless)
            .expect("only bare needs the key");
        }
    }
}
