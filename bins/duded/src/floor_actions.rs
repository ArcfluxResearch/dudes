//! Finite browser onboarding values kept inside the daemon.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dudes_journal::Reader;
use dudes_proto::record::{BrowserOnboardingHire, Hex32, RecordedWorkspace};
use dudes_proto::rpc::floor::{CheckView, ChecksView, WorkspaceChecks};
use dudes_proto::rpc::goal::{CompletionCheckDeclaration, ENROLLED_CHECK_ID};
use dudes_proto::{AgentBudget, CompletionCheckRef, GoalBudget, Predicate, Record};
use dudes_types::{AgentId, GoalId, IsolationMode, Lsn, Provider, Timestamp, Tokens, Usd};
use serde::Deserialize;
use serde::Serialize;

/// One operator-enrolled work context. Nothing in this type is taken from a browser request.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    /// Opaque identifier selected by the browser.
    pub id: String,
    /// Explicit safe workspace display name.
    pub label: String,
    /// The workspace's enrolled completion checks: its own check, from the workspace's keys,
    /// then each row of its `checks` table. Never empty.
    pub checks: Vec<EnrolledCheck>,
    /// Canonical operator-enrolled repository directory.
    pub cwd: PathBuf,
    /// `cwd` as the operator file wrote it. A reload compares written values, never canonical
    /// ones, so a directory that became a symlink after start does not read as a change.
    pub written_cwd: PathBuf,
    /// Fixed department for the hired agent.
    pub department: String,
    /// Fixed managed-turn isolation selection.
    pub isolation: IsolationMode,
    /// Optional daily ceilings; a dollar ceiling, when set, is positive.
    pub agent_budget: AgentBudget,
    /// Optional goal ceilings; a dollar ceiling, when set, is positive.
    pub goal_budget: GoalBudget,
    /// Positive limit on managed turns for the initial goal.
    pub hop_cap: u32,
    /// Set by the latest applied reload that added or changed this entry; run-local.
    pub mark: Option<ReloadMark>,
}

/// One operator-enrolled completion check. Its command never reaches the browser.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrolledCheck {
    /// Its id, unique within its workspace.
    pub id: String,
    /// Its display label.
    pub label: String,
    /// The typed command predicate.
    pub predicate: Predicate,
    /// The operator declared that it may already pass, so a goal with it does not run it first.
    pub may_already_pass: bool,
}

impl EnrolledCheck {
    /// The workspace's own check, from the keys the workspace entry has always had.
    #[must_use]
    pub fn enrolled(label: &str, predicate: Predicate) -> Self {
        Self {
            id: ENROLLED_CHECK_ID.to_owned(),
            label: label.to_owned(),
            predicate,
            may_already_pass: false,
        }
    }

    /// The name, label and revision a goal records for this check. `None` for an id or a label
    /// that names nothing, which enrollment refuses.
    pub(crate) fn reference(&self) -> Option<CompletionCheckRef> {
        let check = CompletionCheckDeclaration::new(&self.id, &self.label).ok()?;
        crate::goal_replacement::named_check(&check, &self.predicate, self.may_already_pass)
    }

    fn parts(&self) -> (Option<&[String]>, Option<u64>) {
        match &self.predicate {
            Predicate::Command { argv, timeout_s } => (Some(argv), Some(*timeout_s)),
            _ => (None, None),
        }
    }
}

/// What an applied reload did to one entry, shown beside it in the Hire wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReloadMark {
    /// `Added` only for an id this daemon run has never run before.
    pub kind: MarkKind,
    /// The daemon's time of the reload that set the mark.
    pub at: Timestamp,
    /// The daemon-run reload counter at that reload; a browser hire must present it.
    pub revision: u64,
}

/// Whether a reload first enrolled an id in this run or changed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkKind {
    /// An id this daemon run has never run before.
    Added,
    /// Any later add or change of an id.
    Changed,
}

impl Profile {
    /// The workspace's own check, the first of its list.
    pub(crate) fn first_check(&self) -> &EnrolledCheck {
        &self.checks[0]
    }

    /// The enrolled check with `id`.
    pub(crate) fn check(&self, id: &str) -> Option<&EnrolledCheck> {
        self.checks.iter().find(|check| check.id == id)
    }

    /// The revision a browser hire of this workspace must present; 0 while it has no mark.
    pub(crate) fn revision(&self) -> u64 {
        self.mark.map_or(0, |mark| mark.revision)
    }

    /// Every key whose written value differs from `other`'s, by its TOML name, in file order.
    pub(crate) fn changed_fields(&self, other: &Self) -> Vec<&'static str> {
        let (own, other_own) = (self.first_check(), other.first_check());
        let ((argv, timeout), (other_argv, other_timeout)) = (own.parts(), other_own.parts());
        [
            ("label", self.label != other.label),
            ("completion_check_label", own.label != other_own.label),
            ("completion_check_id", own.id != other_own.id),
            ("cwd", self.written_cwd != other.written_cwd),
            ("department", self.department != other.department),
            ("isolation", self.isolation != other.isolation),
            (
                "agent_usd_per_day",
                self.agent_budget.usd_per_day != other.agent_budget.usd_per_day,
            ),
            ("goal_usd", self.goal_budget.usd != other.goal_budget.usd),
            ("hop_cap", self.hop_cap != other.hop_cap),
            ("predicate_argv", argv != other_argv),
            ("predicate_timeout_s", timeout != other_timeout),
            (
                "may_already_pass",
                own.may_already_pass != other_own.may_already_pass,
            ),
            (
                "tokens_per_day",
                self.agent_budget.tokens_per_day != other.agent_budget.tokens_per_day,
            ),
            (
                "goal_tokens",
                self.goal_budget.tokens != other.goal_budget.tokens,
            ),
            ("checks", self.checks[1..] != other.checks[1..]),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
    }
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("id", &self.id)
            .field("private_configuration", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// The running operator enrollment, loaded at start or by the latest applied reload. An empty
/// list disables browser onboarding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Enrollment {
    /// Admitted profiles, sorted by identifier.
    pub profiles: Vec<Profile>,
}

/// Why a catalog was refused. An id is named only once it is known to be a valid id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// More than 32 workspaces.
    TooMany,
    /// One workspace is refused: a duplicate id or an invalid profile.
    Workspace(String),
    /// A workspace whose id is not the restricted ASCII.
    InvalidId,
    /// The serialized catalog, marks included, exceeds 16 KiB.
    TooLarge,
    /// The Floor's list of enrolled checks would exceed its cap.
    ChecksTooLarge,
}

/// The engines a browser hire may choose, in `Provider` order.
pub(crate) const BROWSER_ENGINES: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Pi];

/// The sibling as amended: an unset ceiling is `null`, never absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OnboardingView {
    floor_usd_per_day_micros: Option<String>,
    workspaces: Vec<WorkspaceView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WorkspaceView {
    workspace_id: String,
    label: String,
    engines: Vec<Provider>,
    goal_usd_micros: Option<String>,
    agent_usd_per_day_micros: Option<String>,
    agent_tokens_per_day: Option<String>,
    goal_tokens: Option<String>,
    hop_cap: u32,
    /// The operator trusts this project, so a hire may ask for its own copy of it. Nothing else
    /// of the grant is shown.
    trusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reload_mark: Option<MarkView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MarkView {
    kind: MarkKind,
    at: String,
    revision: String,
}

impl Enrollment {
    /// Validate the display catalog, marks included, before the daemon can serve it.
    ///
    /// # Errors
    ///
    /// The first refusal, naming a workspace only by a valid id.
    pub fn validate(&mut self, floor_usd_per_day: Option<Usd>) -> Result<(), CatalogError> {
        if self.profiles.len() > 32 {
            return Err(CatalogError::TooMany);
        }
        if self
            .profiles
            .iter()
            .any(|profile| !valid_workspace_id(&profile.id))
        {
            return Err(CatalogError::InvalidId);
        }
        self.profiles.sort_by(|a, b| a.id.cmp(&b.id));
        for pair in self.profiles.windows(2) {
            if pair[0].id == pair[1].id {
                return Err(CatalogError::Workspace(pair[0].id.clone()));
            }
        }
        for profile in &self.profiles {
            if !valid_display(&profile.label)
                || !valid_checks(&profile.checks)
                || profile.department.is_empty()
                || profile.isolation != IsolationMode::Default
                || profile.hop_cap == 0
                || profile.agent_budget.usd_per_day == Some(Usd::ZERO)
                || profile.goal_budget.usd == Some(Usd::ZERO)
                || !profile.cwd.is_absolute()
            {
                return Err(CatalogError::Workspace(profile.id.clone()));
            }
        }
        // The largest view this catalog can serve: which engines a run offers is not known here.
        let view = Self::view(
            floor_usd_per_day,
            self.profiles
                .iter()
                .map(|profile| (profile, BROWSER_ENGINES.to_vec(), true)),
        );
        let bytes = serde_json::to_vec(&view).map_err(|_| CatalogError::TooLarge)?;
        if bytes.len() > 16_384 {
            return Err(CatalogError::TooLarge);
        }
        if !self.checks_view().fits() {
            return Err(CatalogError::ChecksTooLarge);
        }
        Ok(())
    }

    /// The Floor's list of every enrolled workspace's checks, in id order, own check first.
    pub(crate) fn checks_view(&self) -> ChecksView {
        ChecksView {
            workspaces: self
                .profiles
                .iter()
                .map(|profile| WorkspaceChecks {
                    workspace_id: profile.id.clone(),
                    checks: profile
                        .checks
                        .iter()
                        .filter_map(|check| {
                            check.reference().map(|named| CheckView {
                                id: named.id,
                                label: named.label,
                                revision: named.revision,
                                may_already_pass: check.may_already_pass,
                            })
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// The sole safe wire form of eligible profiles, already sorted at config admission, each
    /// beside the engines offered for it and whether the operator trusts it.
    pub(crate) fn view<'a>(
        floor: Option<Usd>,
        profiles: impl IntoIterator<Item = (&'a Profile, Vec<Provider>, bool)>,
    ) -> OnboardingView {
        let dollars = |usd: Option<Usd>| usd.map(|usd| usd.micros().to_string());
        let tokens = |tokens: Option<Tokens>| tokens.map(|tokens| tokens.count().to_string());
        OnboardingView {
            floor_usd_per_day_micros: dollars(floor),
            workspaces: profiles
                .into_iter()
                .map(|(profile, engines, trusted)| WorkspaceView {
                    workspace_id: profile.id.clone(),
                    label: profile.label.clone(),
                    engines,
                    goal_usd_micros: dollars(profile.goal_budget.usd),
                    agent_usd_per_day_micros: dollars(profile.agent_budget.usd_per_day),
                    agent_tokens_per_day: tokens(profile.agent_budget.tokens_per_day),
                    goal_tokens: tokens(profile.goal_budget.tokens),
                    hop_cap: profile.hop_cap,
                    trusted,
                    reload_mark: profile.mark.map(|mark| MarkView {
                        kind: mark.kind,
                        at: rfc3339_seconds(mark.at),
                        revision: mark.revision.to_string(),
                    }),
                })
                .collect(),
        }
    }

    pub(crate) fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub(crate) fn workspace_for_cwd(&self, cwd: &Path) -> Option<RecordedWorkspace> {
        let mut matches = self.profiles.iter().filter(|profile| profile.cwd == cwd);
        let profile = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(RecordedWorkspace {
            id: profile.id.clone(),
            label: profile.label.clone(),
        })
    }
}

/// One indexed durable browser attempt. The hire record retains all effective profile values.
#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    pub(crate) agent: AgentId,
    pub(crate) hire: Record,
    pub(crate) metadata: BrowserOnboardingHire,
    pub(crate) goal_set: bool,
    pub(crate) goal_conflict: bool,
}

/// A replayable in-memory index; only journal records may change it.
#[derive(Debug, Default)]
pub(crate) struct OnboardingIndex {
    attempts: BTreeMap<Hex32, Attempt>,
    hires_per_session: BTreeMap<Hex32, u32>,
}

impl OnboardingIndex {
    pub(crate) fn replay(reader: &Reader) -> Result<Self> {
        let mut index = Self::default();
        for item in reader
            .iter_from(Lsn::ZERO)
            .context("reading onboarding journal index")?
        {
            let (_, entry) = item.context("reading onboarding journal entry")?;
            index.apply(&entry.record)?;
        }
        Ok(index)
    }

    pub(crate) fn apply(&mut self, record: &Record) -> Result<()> {
        match record {
            Record::AgentHired {
                id,
                browser_onboarding: Some(metadata),
                ..
            } => {
                if self.attempts.contains_key(&metadata.attempt_id) {
                    bail!("duplicate browser onboarding attempt in journal");
                }
                let count = self
                    .hires_per_session
                    .entry(metadata.session_digest)
                    .or_default();
                *count = count
                    .checked_add(1)
                    .context("browser hire count overflow")?;
                self.attempts.insert(
                    metadata.attempt_id,
                    Attempt {
                        agent: *id,
                        hire: record.clone(),
                        metadata: (**metadata).clone(),
                        goal_set: false,
                        goal_conflict: false,
                    },
                );
            }
            Record::GoalSet {
                agent,
                goal,
                browser_onboarding: Some(metadata),
                ..
            } => {
                let attempt = self
                    .attempts
                    .get_mut(&metadata.attempt_id)
                    .context("browser goal without matching hire")?;
                if attempt.agent != *agent
                    || attempt.metadata.intended_goal != *goal
                    || attempt.goal_set
                {
                    bail!("browser goal does not match its durable hire");
                }
                attempt.goal_set = true;
            }
            Record::GoalSet {
                agent,
                browser_onboarding: None,
                ..
            }
            | Record::TicketStarted { agent, .. } => {
                for attempt in self.attempts.values_mut() {
                    if attempt.agent == *agent && !attempt.goal_set {
                        attempt.goal_conflict = true;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn count(&self, session: Hex32) -> u32 {
        self.hires_per_session.get(&session).copied().unwrap_or(0)
    }

    pub(crate) fn attempt(&self, id: Hex32) -> Option<&Attempt> {
        self.attempts.get(&id)
    }
}

/// A UTC instant as an RFC 3339 string to the second, such as `2026-09-28T03:40:00Z`.
fn rfc3339_seconds(at: Timestamp) -> String {
    let seconds = at.unix_millis().div_euclid(1000);
    let (days, time) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days since 1970-01-01 to a proleptic Gregorian civil date (Howard Hinnant's algorithm).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

pub(crate) fn valid_workspace_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Canonical, bounded browser fields with no execution choice.
#[derive(Clone)]
pub struct OnboardRequest {
    pub(crate) attempt_id: Hex32,
    pub(crate) attempt_wire: String,
    pub(crate) request_digest: Hex32,
    pub(crate) workspace_id: String,
    /// The revision of the workspace the page was offered; absent on the wire means 0.
    pub(crate) workspace_revision: u64,
    pub(crate) character: Option<dudes_types::CastCharacter>,
    pub(crate) accent: Option<dudes_types::AccentColour>,
    /// The engine: Claude when the request names none.
    pub(crate) provider: Provider,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) goal_text: String,
    /// The enrolled check the hire names, by id and revision. Absent only on a retry of an
    /// attempt the onboarding index already holds.
    pub(crate) check: Option<(String, Hex32)>,
    /// The agent works in a git worktree of its own rather than in the project folder.
    pub(crate) own_copy: bool,
}

impl fmt::Debug for OnboardRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OnboardRequest(<redacted>)")
    }
}

/// What the authenticated POST may truthfully report after the daemon job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnboardOutcome {
    /// Both matching domain records are durable.
    Complete {
        /// Canonical attempt id to return to the page.
        attempt_id: String,
        /// Durable hired agent.
        agent: AgentId,
        /// Durable initial goal.
        goal: GoalId,
    },
    /// Only the matching hire is durable.
    Partial {
        /// Canonical attempt id to return to the page.
        attempt_id: String,
        /// Durable hired agent.
        agent: AgentId,
        /// Bounded reason selected by the decision order.
        reason: &'static str,
        /// Current projected goal, which may be a local winner.
        current_goal: Option<GoalId>,
        /// Current projected retirement state.
        retired: bool,
    },
}

/// What a browser hire's first job answers: the request's answer, or its check running once
/// before anything is appended.
#[derive(Debug)]
pub enum OnboardStep {
    /// The request's answer.
    Answer(Result<OnboardOutcome, OnboardRefusal>),
    /// The hire's check is running; the request waits on it.
    Check {
        /// The run.
        ticket: crate::baseline::Ticket,
        /// What a retried partial attempt answers when its check refuses the goal; `None` for a
        /// new attempt, which answers the refusal.
        on_refusal: Option<OnboardOutcome>,
    },
    /// The hire asked for its own copy of the project: the connection thread makes the copy,
    /// then asks for the hire into it.
    Copy {
        /// The hire the copy is made for. Only the copy's admission reads it.
        params: Box<dudes_proto::rpc::agent::HireParams>,
        /// What the hire's check run saw, for the hire that follows.
        baseline: dudes_proto::CheckBaseline,
        /// The operator's grant the copy rests on.
        trust: Box<crate::worktree::hire::TrustTicket>,
    },
}

/// Why a browser hire's own copy was not made. Each is a sentence the page shows as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyRefusal {
    /// Only Claude Code agents can work in a copy for now.
    Engine,
    /// The platform cannot make copies.
    Platform,
    /// The operator has not trusted this project.
    NotTrusted(String),
    /// The operator trusted this project, but its location or settings are not what they vouched
    /// for any more.
    TrustChanged(String),
    /// The project's folder is not inside a git repository.
    NotARepository,
    /// Anything else that stopped the copy: git, the folder, or the daemon's admission.
    Failed,
}

impl fmt::Display for CopyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine => f.write_str(
                "Only Claude Code agents can work in their own copy for now. Choose Claude Code, \
                 or hire without a copy. Nobody was hired.",
            ),
            Self::Platform => f.write_str(
                "Own copies are not available on Windows yet. Hire without a copy. Nobody was hired.",
            ),
            Self::NotTrusted(id) => write!(
                f,
                "This project is not trusted yet, so Dudes will not make a copy of it. A copy runs \
                 the repository's own Claude settings, such as environment values and helper \
                 commands, without asking. To allow it, run `dudes project trust {id}` in a \
                 terminal, or hire without a copy. Nobody was hired."
            ),
            Self::TrustChanged(id) => write!(
                f,
                "This project's folder or settings changed since you trusted it, so Dudes did not \
                 make a copy. Run `dudes project trust {id}` again to vouch for what it holds \
                 now, or hire without a copy. Nobody was hired."
            ),
            Self::NotARepository => f.write_str(
                "This project's folder is not a git repository, so Dudes cannot make a separate \
                 copy of it. Hire without a copy, or turn the folder into a git repository first. \
                 Nobody was hired.",
            ),
            Self::Failed => f.write_str(
                "Dudes could not make the copy, so nobody was hired. Check that git is installed \
                 and the project's folder is healthy, then try again, or hire without a copy.",
            ),
        }
    }
}

/// An empty HTTP refusal with no invented domain identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnboardRefusal {
    /// Session or pre-hire admission refused without an effect.
    Forbidden,
    /// The same attempt id names a different canonical request.
    Conflict,
    /// Durability cannot be established; the same attempt may be checked later.
    Unavailable,
    /// This request appended nothing and may retry immediately in the same run.
    UnavailableNoEffect,
    /// A new attempt's typed input failed validation before any effect. The route
    /// answers it as it answers a request that does not parse.
    InvalidInput,
    /// A new hire presented a workspace revision other than the running one, or
    /// a check that is not enrolled for the workspace at that revision: nothing was appended.
    ProfileChanged,
    /// The hire's check refused it before any append, or a limit on the session's check runs
    /// did; the reason names which.
    Check(&'static str),
    /// Current owned-child admission refused before a hire was appended.
    SharedFolder(dudes_supervisor::SharedFolderRefusal),
    /// The hire's own copy was refused or could not be made; nothing was hired.
    OwnCopy(CopyRefusal),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnboardJson {
    attempt_id: String,
    workspace_id: String,
    #[serde(default)]
    workspace_revision: Option<String>,
    #[serde(default)]
    character: Option<dudes_types::CastCharacter>,
    #[serde(default)]
    accent: Option<dudes_types::AccentColour>,
    #[serde(default)]
    provider: Option<Provider>,
    name: String,
    role: String,
    goal_text: String,
    #[serde(default)]
    check_id: Option<String>,
    #[serde(default)]
    check_revision: Option<Hex32>,
    #[serde(default)]
    own_copy: Option<bool>,
}

/// The request digest's domain and the fields it frames after the four text fields. v1 frames
/// none, v2 adds the appearance, v3, for a request that names an engine, always frames all seven,
/// v4, for a request that names a check, frames v3's seven and then the check, and v5, for a request
/// that asks for its own copy, frames v4's fields and then the choice. A request valid under an
/// older domain keeps its bytes.
fn request_framing<'a>(
    check: Option<(&'a str, &'a str)>,
    names_engine: bool,
    own_copy: bool,
    seven: [(&'static str, &'a str); 3],
) -> (&'static [u8], Vec<(&'static str, &'a str)>) {
    let [_, character, accent] = seven;
    match (check, names_engine, character.1, accent.1) {
        (Some((id, revision)), ..) if own_copy => (
            b"dudes.onboard.v5\0",
            seven
                .into_iter()
                .chain([
                    ("check_id", id),
                    ("check_revision", revision),
                    ("own_copy", "true"),
                ])
                .collect(),
        ),
        (Some((id, revision)), ..) => (
            b"dudes.onboard.v4\0",
            seven
                .into_iter()
                .chain([("check_id", id), ("check_revision", revision)])
                .collect(),
        ),
        (None, true, ..) => (b"dudes.onboard.v3\0", seven.to_vec()),
        (None, false, "", "") => (b"dudes.onboard.v1\0", Vec::new()),
        (None, false, ..) => (b"dudes.onboard.v2\0", vec![character, accent]),
    }
}

impl OnboardRequest {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 4096 || crate::floor_stream::json_nesting(bytes)? > 16 {
            bail!("browser onboarding request is too large or nested");
        }
        // The derived decoder also reads a struct from a positional array; only an object is a
        // valid request body.
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("browser onboarding request is not one JSON object");
        }
        let raw: OnboardJson =
            serde_json::from_slice(bytes).context("invalid browser onboarding JSON")?;
        let attempt_id =
            decode_attempt(&raw.attempt_id).context("invalid browser onboarding attempt id")?;
        if !valid_workspace_id(&raw.workspace_id) {
            bail!("invalid browser onboarding workspace id");
        }
        let workspace_revision = match raw.workspace_revision.as_deref() {
            None => 0,
            Some(text) => {
                canonical_decimal(text).context("invalid browser onboarding workspace revision")?
            }
        };
        let name = dudes_types::DisplayName::parse(&raw.name)
            .context("invalid browser hire name")?
            .into_string();
        let role = dudes_types::DisplayRole::parse(&raw.role)
            .context("invalid browser hire role")?
            .into_string();
        let goal_text = raw
            .goal_text
            .trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
            .to_owned();
        if !(1..=2048).contains(&goal_text.len()) {
            bail!("browser onboarding text is outside its byte limit");
        }
        // A Claude request names no engine; naming one explicitly would give one hire two digests.
        let provider = match raw.provider {
            None => Provider::Claude,
            Some(provider @ (Provider::Codex | Provider::Pi)) => provider,
            Some(_) => bail!("browser onboarding provider is not codex or pi"),
        };
        let character = raw.character.map_or("", dudes_types::CastCharacter::as_str);
        let accent = raw.accent.map_or("", dudes_types::AccentColour::as_str);
        let check = match (raw.check_id, raw.check_revision) {
            (None, None) => None,
            (Some(id), Some(revision)) if CompletionCheckDeclaration::valid_id(&id) => {
                Some((id, revision))
            }
            _ => bail!("invalid browser onboarding check"),
        };
        let revision = check
            .as_ref()
            .map(|(id, revision)| (id.as_str(), crate::goal_replacement::hex(*revision)));
        let own_copy = raw.own_copy.unwrap_or(false);
        if own_copy && check.is_none() {
            bail!("a browser hire in its own copy names a check");
        }
        let (domain, extra) = request_framing(
            revision
                .as_ref()
                .map(|(id, revision)| (*id, revision.as_str())),
            raw.provider.is_some(),
            own_copy,
            [
                ("provider", provider.as_str()),
                ("character", character),
                ("accent", accent),
            ],
        );
        let request_digest = crate::goal_replacement::digest(
            domain,
            [
                ("workspace_id", raw.workspace_id.as_str()),
                ("name", name.as_str()),
                ("role", role.as_str()),
                ("goal_text", goal_text.as_str()),
            ]
            .into_iter()
            .chain(extra)
            .map(|(field, value)| (field.to_owned(), value.to_owned())),
        );
        Ok(Self {
            character: raw.character,
            accent: raw.accent,
            provider,
            attempt_id,
            attempt_wire: raw.attempt_id,
            request_digest,
            workspace_id: raw.workspace_id,
            workspace_revision,
            name,
            role,
            goal_text,
            check,
            own_copy,
        })
    }
}

/// A `u64` written as a canonical decimal string: digits only, no leading zero but `0` itself.
fn canonical_decimal(text: &str) -> Result<u64> {
    if text.is_empty()
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        bail!("not a canonical decimal");
    }
    Ok(text.parse()?)
}

pub(crate) fn decode_attempt(value: &str) -> Result<Hex32> {
    if value.len() != 43 {
        bail!("attempt id must encode 32 bytes");
    }
    let mut bytes = [0_u8; 32];
    let mut output = 0;
    let mut pending = 0_u32;
    let mut bits = 0_u32;
    for character in value.bytes() {
        let digit = match character {
            b'A'..=b'Z' => u32::from(character - b'A'),
            b'a'..=b'z' => u32::from(character - b'a') + 26,
            b'0'..=b'9' => u32::from(character - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("attempt id is not base64url"),
        };
        pending = (pending << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes[output] = u8::try_from(pending >> bits).expect("at most eight decoded bits");
            output += 1;
            pending &= (1 << bits) - 1;
        }
    }
    if output != 32 || bits != 2 || pending != 0 {
        bail!("attempt id has noncanonical trailing bits");
    }
    Ok(Hex32::new(bytes))
}

fn valid_display(value: &str) -> bool {
    (1..=128).contains(&value.len()) && !value.chars().any(char::is_control)
}

/// A workspace's checks: at least one, each id unique, each label a display label that names
/// something once trimmed, and each a command with something to run and a positive timeout.
fn valid_checks(checks: &[EnrolledCheck]) -> bool {
    !checks.is_empty()
        && checks.iter().enumerate().all(|(at, check)| {
            valid_display(&check.label)
                && check.reference().is_some()
                && matches!(&check.predicate, Predicate::Command { argv, timeout_s } if !argv.is_empty() && *timeout_s > 0)
                && checks[..at].iter().all(|earlier| earlier.id != check.id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fields_have_one_domain_separated_request_digest() {
        let request = OnboardRequest::parse(br#"{"attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","workspace_id":"repo_1","name":" Kim \n","role":"engineer","goal_text":"ship"}"#)
            .expect("canonical request");
        assert_eq!(request.name, "Kim");
        assert_eq!(request.attempt_id.bytes(), [0; 32]);
        assert_eq!(
            serde_json::to_value(request.request_digest).unwrap(),
            "2e33d3aad10a50a1eeae4c310d191a3ca16648446e070f4ad65cac40e6cc1b3e"
        );
    }

    #[test]
    fn a_browser_hire_refuses_a_control_character_in_the_name_or_the_role() {
        let parse = |name: &str, role: &str| {
            OnboardRequest::parse(
                &serde_json::to_vec(&serde_json::json!({
                    "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    "workspace_id": "repo_1", "name": name, "role": role, "goal_text": "ship"
                }))
                .unwrap(),
            )
        };
        for (name, role) in [
            ("Kim\u{1b}]52;c;aGk=\u{7}", "engineer"),
            ("Kim", "engi\u{1b}[2Jneer"),
            ("Kim", "engi\nneer"),
            ("Kim", "engi\u{9b}1mneer"),
        ] {
            assert!(parse(name, role).is_err(), "{name:?} {role:?}");
        }
        let admitted = parse("Kim\u{202e}", " engi\u{2066}neer\n").expect("bidi is not a control");
        assert_eq!(
            (admitted.name.as_str(), admitted.role.as_str()),
            ("Kim\u{202e}", "engi\u{2066}neer")
        );
    }

    #[test]
    fn appearance_choices_bind_the_v2_digest_without_changing_legacy_v1() {
        let legacy = serde_json::json!({
            "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer", "goal_text": "ship"
        });
        let parse = |value: &serde_json::Value| {
            OnboardRequest::parse(&serde_json::to_vec(value).unwrap()).unwrap()
        };
        let old = parse(&legacy);
        let mut nulls = legacy.clone();
        nulls["character"] = serde_json::Value::Null;
        nulls["accent"] = serde_json::Value::Null;
        assert_eq!(old.request_digest, parse(&nulls).request_digest);
        let mut explicit = legacy.clone();
        explicit["character"] = serde_json::json!("h");
        explicit["accent"] = serde_json::json!("sky");
        let chosen = parse(&explicit);
        assert_eq!(
            serde_json::to_value(chosen.request_digest).unwrap(),
            "1622b613581c628c006be7a1a8847d062127c98f6e222787a986fd9a0165e428"
        );
        assert_ne!(old.request_digest, chosen.request_digest);
        for (field, value) in [
            ("character", serde_json::json!("c")),
            ("accent", serde_json::json!("rose")),
            ("character", serde_json::Value::Null),
            ("accent", serde_json::Value::Null),
        ] {
            let mut changed = explicit.clone();
            changed[field] = value;
            assert_ne!(chosen.request_digest, parse(&changed).request_digest);
        }
        for (field, value) in [
            ("character", "a"),
            ("character", ""),
            ("accent", "red"),
            ("accent", "#fff"),
        ] {
            let mut unknown = legacy.clone();
            unknown[field] = serde_json::json!(value);
            assert!(OnboardRequest::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
        }
    }

    /// A request that names Codex or Pi binds the engine under `dudes.onboard.v3`, which frames all
    /// seven fields with an absent appearance as empty. A Claude request names no engine and keeps
    /// its v1 or v2 bytes, pinned above; an explicit `claude`, another provider or a non-string is
    /// refused. The expected digests were computed outside this crate from the framing rule.
    #[test]
    fn an_engine_binds_the_v3_digest_and_claude_keeps_v1_and_v2() {
        let legacy = serde_json::json!({
            "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer", "goal_text": "ship"
        });
        let parse =
            |value: &serde_json::Value| OnboardRequest::parse(&serde_json::to_vec(value).unwrap());
        let digest = |value: &serde_json::Value| {
            serde_json::to_value(parse(value).expect("a canonical request").request_digest).unwrap()
        };
        let with = |pairs: &[(&str, serde_json::Value)]| {
            let mut value = legacy.clone();
            for (field, item) in pairs {
                value[*field] = item.clone();
            }
            value
        };
        assert_eq!(
            digest(&legacy),
            "2e33d3aad10a50a1eeae4c310d191a3ca16648446e070f4ad65cac40e6cc1b3e"
        );
        assert_eq!(
            digest(&with(&[("provider", serde_json::Value::Null)])),
            "2e33d3aad10a50a1eeae4c310d191a3ca16648446e070f4ad65cac40e6cc1b3e",
            "a null provider is an absent one"
        );
        assert_eq!(
            digest(&with(&[
                ("character", "h".into()),
                ("accent", "sky".into())
            ])),
            "1622b613581c628c006be7a1a8847d062127c98f6e222787a986fd9a0165e428"
        );
        let codex = with(&[("provider", "codex".into())]);
        assert_eq!(
            digest(&codex),
            "9cd190db8747e1e0bbb2cdb5e3c6912d28cfcde0ae8930b2a6ee1bc7da4db2bc"
        );
        assert_eq!(parse(&codex).unwrap().provider, Provider::Codex);
        assert_eq!(
            digest(&with(&[
                ("provider", "pi".into()),
                ("character", "h".into()),
                ("accent", "sky".into())
            ])),
            "4c4f265e712489be83fbdc4e151c69110810ea4c40a418341935c0fce746ae2c"
        );
        assert_eq!(parse(&legacy).unwrap().provider, Provider::Claude);
        for refused in ["claude", "copilot", "Codex", ""] {
            assert!(
                parse(&with(&[("provider", refused.into())])).is_err(),
                "{refused:?} is refused"
            );
        }
        assert!(parse(&with(&[("provider", 1.into())])).is_err());
    }

    /// The derived struct decoder also accepts positional arrays. This route accepts objects.
    #[test]
    fn only_an_object_is_a_request() {
        let object = br#"{"attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","workspace_id":"repo_1","name":"Kim","role":"engineer","goal_text":"ship"}"#;
        let spaced = [b" \n".as_slice(), object].concat();
        assert!(
            OnboardRequest::parse(&spaced).is_ok(),
            "onboarding: leading whitespace before the object"
        );
        let array =
            br#"["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","repo_1","Kim","engineer","ship"]"#;
        assert!(
            OnboardRequest::parse(array).is_err(),
            "onboarding: admitted an array"
        );
    }

    /// A mark's time is RFC 3339 to the second, UTC, across the epoch and a leap day.
    #[test]
    fn a_mark_time_is_rfc3339_to_the_second() {
        for (seconds, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (1_709_208_000, "2024-02-29T12:00:00Z"),
            (1_790_000_000, "2026-09-21T14:13:20Z"),
        ] {
            assert_eq!(
                rfc3339_seconds(Timestamp::from_unix_millis(seconds * 1000 + 999)),
                text
            );
        }
    }

    #[test]
    fn nesting_above_sixteen_is_rejected_before_struct_decode() {
        let nested = format!("{}0{}", "[".repeat(17), "]".repeat(17));
        let body = format!(
            "{{\"attempt_id\":\"{}\",\"workspace_id\":\"repo_1\",\"name\":\"Kim\",\"role\":\"engineer\",\"goal_text\":\"ship\",\"extra\":{nested}}}",
            "A".repeat(43),
        );
        let error = OnboardRequest::parse(body.as_bytes()).expect_err("depth refusal");
        assert!(error.to_string().contains("too large or nested"));
    }

    /// A goal that takes over from a browser hire's intended goal before the onboarding goal
    /// lands makes that attempt conflicted. A ticket's start installs a goal the same way.
    #[test]
    fn ticket_start_conflicts_an_unfinished_hire_attempt() {
        use dudes_proto::RecordKind;
        use dudes_proto::record::samples;

        let attempt_id = Hex32::new([3; 32]);
        for carrier in [RecordKind::GoalSet, RecordKind::TicketStarted] {
            let mut hire = samples::record(RecordKind::AgentHired);
            let Record::AgentHired {
                id,
                browser_onboarding,
                ..
            } = &mut hire
            else {
                unreachable!("the sample is a hire");
            };
            let Record::GoalSet { goal, .. } = samples::record(RecordKind::GoalSet) else {
                unreachable!("the sample is a goal set");
            };
            let agent = *id;
            *browser_onboarding = Some(Box::new(BrowserOnboardingHire {
                attempt_id,
                request_digest: Hex32::new([4; 32]),
                profile_id: "repo_1".into(),
                session_digest: Hex32::new([5; 32]),
                intended_goal: goal,
                trust_lsn: None,
            }));
            let mut index = OnboardingIndex::default();
            index.apply(&hire).unwrap();
            assert!(!index.attempt(attempt_id).unwrap().goal_conflict);
            let mut other = samples::record(carrier);
            match &mut other {
                Record::GoalSet { agent: owner, .. }
                | Record::TicketStarted { agent: owner, .. } => {
                    *owner = agent;
                }
                _ => unreachable!("a goal carrier"),
            }
            index.apply(&other).unwrap();
            assert!(
                index.attempt(attempt_id).unwrap().goal_conflict,
                "{carrier:?} conflicts the unfinished attempt"
            );
        }
    }
}
