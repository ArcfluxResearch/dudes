//! `floor.enrollment.reload`: the running daemon adopts the `[floor.onboarding]` workspaces now in
//! its own operator file, only when the file and the change are the caller's.
//!
//! The file is read, hashed and parsed on the caller's connection thread, never on the loop
//! thread. One loop job then compares the parse with the running enrollment and decides, so a
//! snapshot, a hire's admission and a hire's goal step each see the old set or the new one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dudes_journal::MemoryCondenseConfig;
use dudes_proto::rpc::floor::{
    ChangedWorkspace, EnrollmentApply, EnrollmentRefusal, EnrollmentReloadParams,
    EnrollmentReloadResult,
};
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_rpc::Call;
use dudes_types::Timestamp;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::config::{
    ConfigError, OperatorConfig, parse_operator_and_memory_with_options, read_operator_for_reload,
};
use crate::floor_actions::{Enrollment, MarkKind, Profile, ReloadMark};
use crate::floor_switches::Switches;
use crate::ports::Handle;
use crate::runtime::{OnboardingPause, OnboardingPausePoint};

/// How long an add-only resync holds off the next one.
const ADD_RESYNC_WINDOW: Duration = Duration::from_secs(5);

/// One read of the operator file, carried from the connection thread to the loop.
#[derive(Debug)]
pub struct ReloadRead {
    /// The daemon-wide reload sequence number this read took.
    pub(crate) seq: u64,
    /// The SHA-256 of exactly the bytes read, as lowercase hex.
    pub(crate) sha256: String,
    pub(crate) path: PathBuf,
    /// The parse of exactly those bytes.
    pub(crate) config: OperatorConfig,
    pub(crate) memory_config: MemoryCondenseConfig,
    pub(crate) apply: Option<EnrollmentApply>,
    /// The caller's reported pid, for the log line only.
    pub(crate) pid: Option<u32>,
}

/// The connection-thread half, registered as the method's handler.
pub(crate) struct Reloader {
    path: Option<PathBuf>,
    handle: Handle,
    pause: OnboardingPause,
    /// Held across the read, so sequence order is read order.
    sequence: Arc<Mutex<u64>>,
}

impl Reloader {
    pub(crate) fn new(path: Option<PathBuf>, handle: Handle, pause: OnboardingPause) -> Self {
        Self {
            path,
            handle,
            pause,
            sequence: Arc::new(Mutex::new(0)),
        }
    }

    pub(crate) fn call(&self, call: &Call<'_>, pid: Option<u32>) -> Result<Value, RpcError> {
        let params: EnrollmentReloadParams = dudes_rpc::methods::params(call)?;
        let path = self.path.clone().ok_or_else(|| RpcError {
            code: ErrorCode::InternalError.code(),
            message: "the daemon has no operator configuration file".to_owned(),
            data: None,
        })?;
        let (seq, bytes) = {
            let mut sequence = self.sequence.lock().unwrap_or_else(PoisonError::into_inner);
            *sequence += 1;
            (
                *sequence,
                read_operator_for_reload(&path).map_err(|error| invalid(&error))?,
            )
        };
        let sha256 = hex(&Sha256::digest(bytes.as_deref().unwrap_or_default()));
        self.pause.hit(OnboardingPausePoint::AfterReloadHash);
        let (startup, memory_config) =
            parse_operator_and_memory_with_options(&path, bytes.as_deref())
                .map_err(|error| invalid(&error))?;
        let result = self.handle.enrollment_reload(ReloadRead {
            seq,
            sha256,
            path,
            config: startup.operator,
            memory_config,
            apply: params.apply,
            pid,
        })?;
        dudes_rpc::methods::encode(Method::FloorEnrollmentReload, &result)
    }
}

pub(crate) fn invalid(error: &ConfigError) -> RpcError {
    RpcError {
        code: ErrorCode::InvalidParams.code(),
        message: error.to_string(),
        data: None,
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Run-local reload state, kept by the loop thread beside the running enrollment.
#[derive(Debug)]
pub(crate) struct ReloadState {
    /// Every id this daemon run has run, so a re-add is marked `changed`, never `added`.
    ever_run: BTreeSet<String>,
    /// The sequence number of the latest applied reload.
    applied_seq: u64,
    /// The daemon-run counter a mark's revision takes, raised by each reload that marks.
    revision: u64,
    /// When the latest add-only resync went out.
    last_add_resync: Option<Instant>,
    /// An add-only change is waiting for the end of the window.
    add_pending: bool,
    window_id: u64,
}

impl ReloadState {
    pub(crate) fn new(start: &Enrollment) -> Self {
        Self {
            ever_run: start.profiles.iter().map(|p| p.id.clone()).collect(),
            applied_seq: 0,
            revision: 0,
            last_add_resync: None,
            add_pending: false,
            window_id: 0,
        }
    }

    /// A sequence number that no applied reload has passed, for a reload the loop thread reads
    /// itself.
    pub(crate) const fn local_seq(&self) -> u64 {
        self.applied_seq
    }

    /// Whether an add-only change may resync now; otherwise how long until the window ends.
    /// A later call inside the same window is covered by the one already waiting.
    pub(crate) fn add_resync(&mut self, now: Instant) -> AddResync {
        match self.last_add_resync {
            Some(last) if now < last + ADD_RESYNC_WINDOW => {
                if std::mem::replace(&mut self.add_pending, true) {
                    AddResync::Covered
                } else {
                    AddResync::At {
                        wait: last + ADD_RESYNC_WINDOW - now,
                        window_id: self.window_id,
                    }
                }
            }
            _ => {
                self.last_add_resync = Some(now);
                self.window_id += 1;
                AddResync::Now
            }
        }
    }

    /// The window ended: resync once if an addition is still waiting.
    pub(crate) fn window_ended(&mut self, window_id: u64, now: Instant) -> bool {
        if window_id != self.window_id {
            return false;
        }
        let pending = std::mem::take(&mut self.add_pending);
        if pending {
            self.last_add_resync = Some(now);
            self.window_id += 1;
        }
        pending
    }

    /// An immediate resync covers any addition waiting for the window's end.
    pub(crate) fn resynced(&mut self) {
        if std::mem::take(&mut self.add_pending) {
            self.window_id += 1;
        }
    }
}

/// What an add-only change asks of the resync timer.
pub(crate) enum AddResync {
    Now,
    /// Wake at the window's end.
    At {
        wait: Duration,
        window_id: u64,
    },
    /// A wake is already scheduled.
    Covered,
}

/// How an applied reload reaches open Floor streams.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resync {
    None,
    /// A change or a removal: at once.
    Now,
    /// Additions only: coalesced.
    Coalesced,
}

/// The loop job's answer, before the daemon acts on it.
pub(crate) struct Decision {
    pub(crate) result: EnrollmentReloadResult,
    /// The enrollment the file describes, marks set. Installed only when `result.applied`.
    pub(crate) candidate: Enrollment,
    /// The switches the file describes, as the run can serve them. Installed only when
    /// `result.applied`.
    pub(crate) switches: Switches,
    pub(crate) resync: Resync,
}

impl Decision {
    /// One log line for an applied or declined `apply`: the caller's pid, the
    /// result, every id, each changed id's fields, and the canonical `cwd` it would run in.
    pub(crate) fn log(&self, pid: Option<u32>) {
        let cwd = |id: &str| {
            self.candidate
                .profile(id)
                .map(|profile| profile.cwd.display().to_string())
                .unwrap_or_default()
        };
        let added: Vec<String> = self
            .result
            .added
            .iter()
            .map(|id| format!("{id} {}", cwd(id)))
            .collect();
        let changed: Vec<String> = self
            .result
            .changed
            .iter()
            .map(|entry| {
                let fields = entry.fields.join(",");
                if entry.fields.iter().any(|field| field == "cwd") {
                    format!("{} [{fields}] {}", entry.id, cwd(&entry.id))
                } else {
                    format!("{} [{fields}]", entry.id)
                }
            })
            .collect();
        tracing::info!(
            pid = ?pid,
            applied = self.result.applied,
            refused = ?self.result.refused,
            added = ?added,
            removed = ?self.result.removed,
            changed = ?changed,
            "floor.enrollment.reload apply"
        );
    }
}

/// The loop job, with no I/O: compare the read with the running set and decide.
///
/// # Errors
///
/// A catalog refused by admission: an added or changed entry whose canonical `cwd` differs from
/// the file, or a catalog over the cap once marked. A refusal changes nothing.
#[expect(
    clippy::too_many_lines,
    reason = "one pass: diff the profiles, diff the switches, then decide the apply"
)]
pub(crate) fn decide(
    running: &Enrollment,
    state: &mut ReloadState,
    read: &ReloadRead,
    running_sections: &OperatorConfig,
    running_memory: MemoryCondenseConfig,
    running_switches: Switches,
    now: Timestamp,
) -> Result<Decision, ConfigError> {
    let before: BTreeMap<&str, &Profile> = running
        .profiles
        .iter()
        .map(|profile| (profile.id.as_str(), profile))
        .collect();
    let file = &read.config.enrollment.profiles;
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut profiles = Vec::with_capacity(file.len());
    for profile in file {
        match before.get(profile.id.as_str()) {
            Some(old) => {
                let fields = old.changed_fields(profile);
                if fields.is_empty() {
                    // Unchanged: its start-time canonical `cwd` and its mark stay as they are.
                    profiles.push((*old).clone());
                    continue;
                }
                changed.push(ChangedWorkspace {
                    id: profile.id.clone(),
                    fields: fields.into_iter().map(str::to_owned).collect(),
                });
            }
            None => added.push(profile.id.clone()),
        }
        if profile.cwd != profile.written_cwd {
            return Err(ConfigError::non_canonical(&read.path, &profile.id));
        }
        profiles.push(Profile {
            mark: Some(ReloadMark {
                kind: if state.ever_run.contains(&profile.id) {
                    MarkKind::Changed
                } else {
                    MarkKind::Added
                },
                at: now,
                revision: state.revision + 1,
            }),
            ..profile.clone()
        });
    }
    let kept: BTreeSet<&str> = file.iter().map(|profile| profile.id.as_str()).collect();
    let removed: Vec<String> = before
        .keys()
        .filter(|id| !kept.contains(*id))
        .map(|id| (*id).to_owned())
        .collect();
    let mut candidate = Enrollment { profiles };
    candidate
        .validate(running_sections.budget.floor_usd_per_day)
        .map_err(|error| ConfigError::catalog(&read.path, error))?;

    let marked = !added.is_empty() || !changed.is_empty();
    // A daemon started without the terminal cannot serve it until it restarts, whatever the file
    // says. `not_applied` names that case.
    let switches = Switches {
        terminal: read.config.terminal_enabled && running_switches.terminal,
        ..Switches::from_operator(&read.config)
    };
    let switches_changed = running_switches.differing(switches);
    let resync = match (added.is_empty(), removed.is_empty() && changed.is_empty()) {
        (false, true) => Resync::Coalesced,
        (true, true) if switches_changed.is_empty() => Resync::None,
        _ => Resync::Now,
    };
    let mut result = EnrollmentReloadResult {
        applied: false,
        refused: None,
        sha256: read.sha256.clone(),
        path: read.path.to_str().map(str::to_owned),
        workspaces: running.profiles.iter().map(|p| p.id.clone()).collect(),
        added,
        removed,
        changed,
        switches: switches_changed.into_iter().map(str::to_owned).collect(),
        not_applied: not_applied(running_sections, running_memory, read),
    };
    let Some(apply) = &read.apply else {
        return Ok(Decision {
            result,
            candidate,
            switches,
            resync: Resync::None,
        });
    };
    result.refused = refusal(apply, read, &result, state);
    if result.refused.is_some() {
        return Ok(Decision {
            result,
            candidate,
            switches,
            resync: Resync::None,
        });
    }
    state.applied_seq = read.seq;
    if marked {
        state.revision += 1;
    }
    state
        .ever_run
        .extend(candidate.profiles.iter().map(|p| p.id.clone()));
    result.applied = true;
    result.workspaces = candidate.profiles.iter().map(|p| p.id.clone()).collect();
    Ok(Decision {
        result,
        candidate,
        switches,
        resync,
    })
}

/// Why an `apply` is declined, in a fixed order, or `None` when it applies.
fn refusal(
    apply: &EnrollmentApply,
    read: &ReloadRead,
    result: &EnrollmentReloadResult,
    state: &ReloadState,
) -> Option<EnrollmentRefusal> {
    if apply.sha256 != read.sha256 {
        Some(EnrollmentRefusal::FileChanged)
    } else if !names_the_change(apply, result) {
        Some(EnrollmentRefusal::UnexpectedChanges)
    } else if read.seq < state.applied_seq {
        Some(EnrollmentRefusal::Superseded)
    } else {
        None
    }
}

/// Whether `apply` names exactly the change, as sets of ids.
fn names_the_change(apply: &EnrollmentApply, result: &EnrollmentReloadResult) -> bool {
    let set = |ids: &[String]| ids.iter().cloned().collect::<BTreeSet<_>>();
    let changed: Vec<String> = result.changed.iter().map(|c| c.id.clone()).collect();
    set(&apply.added) == set(&result.added)
        && set(&apply.removed) == set(&result.removed)
        && set(&apply.changed) == set(&changed)
}

/// The other sections whose parsed value differs from the running one; each needs a restart.
fn not_applied(
    running: &OperatorConfig,
    running_memory: MemoryCondenseConfig,
    read: &ReloadRead,
) -> Vec<String> {
    let file = &read.config;
    [
        ("budget", running.budget != file.budget),
        (
            "providers.codex",
            running.providers.codex != file.providers.codex,
        ),
        ("providers.pi", running.providers.pi != file.providers.pi),
        (
            "floor.terminal",
            file.terminal_enabled && !running.terminal_enabled,
        ),
        ("mcp.servers", running.mcp_servers != file.mcp_servers),
        (
            "memory",
            running_memory != read.memory_config
                || running.condense_model_enabled != file.condense_model_enabled
                || running.condense_model != file.condense_model,
        ),
    ]
    .into_iter()
    .filter(|(_, differs)| *differs)
    .map(|(name, _)| name.to_owned())
    .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const BUDGET: &str =
        "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n";

    fn repo(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(path.join(".git")).expect("repository marker");
        path.canonicalize().expect("canonical repository")
    }

    fn entry(id: &str, cwd: &Path) -> String {
        format!(
            "[[floor.onboarding.workspaces]]\nworkspace_id = '{id}'\nlabel = 'Workspace'\ncompletion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = ['/usr/bin/true']\npredicate_timeout_s = 10\n",
            cwd.display()
        )
    }

    fn file(entries: &[String]) -> String {
        format!("{BUDGET}{}", entries.concat())
    }

    fn read(seq: u64, text: &str, apply: bool, change: (&[&str], &[&str], &[&str])) -> ReloadRead {
        let path = PathBuf::from("/operator/config.toml");
        let sha256 = hex(&Sha256::digest(text.as_bytes()));
        let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
        let (startup, memory_config) =
            parse_operator_and_memory_with_options(&path, Some(text.as_bytes()))
                .expect("a valid file");
        ReloadRead {
            seq,
            apply: apply.then(|| EnrollmentApply {
                sha256: sha256.clone(),
                added: ids(change.0),
                removed: ids(change.1),
                changed: ids(change.2),
            }),
            sha256,
            config: startup.operator,
            memory_config,
            path,
            pid: None,
        }
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_unix_millis(1_790_000_000_000 + seconds * 1000)
    }

    /// Each key of one entry, changed alone, is reported by its TOML name. A comparison of only
    /// the keys the Floor shows would miss `cwd` and the predicate, which change where and against
    /// what a hire runs.
    #[test]
    fn a_reload_reports_every_changed_field_by_its_toml_name() {
        let root = tempfile::tempdir().expect("root");
        let (repo_1, other) = (repo(root.path(), "repo_1"), repo(root.path(), "other"));
        let base = entry("repo_1", &repo_1);
        let running = read(
            1,
            &file(std::slice::from_ref(&base)),
            false,
            (&[], &[], &[]),
        );
        let sections = running.config.clone();
        let repo_cwd = format!("cwd = '{}'", repo_1.display());
        let other_cwd = format!("cwd = '{}'", other.display());
        for (field, from, to) in [
            ("label", "label = 'Workspace'", "label = 'Renamed'"),
            ("completion_check_label", "'Checks pass'", "'Other check'"),
            ("cwd", repo_cwd.as_str(), other_cwd.as_str()),
            ("department", "'engineering'", "'infra'"),
            (
                "agent_usd_per_day",
                "agent_usd_per_day = 10",
                "agent_usd_per_day = 11",
            ),
            ("goal_usd", "goal_usd = 5", "goal_usd = 6"),
            ("hop_cap", "hop_cap = 3", "hop_cap = 4"),
            ("predicate_argv", "['/usr/bin/true']", "['/usr/bin/false']"),
            ("predicate_timeout_s", "timeout_s = 10", "timeout_s = 11"),
            (
                "tokens_per_day",
                "hop_cap = 3",
                "hop_cap = 3\ntokens_per_day = 7",
            ),
            ("goal_tokens", "hop_cap = 3", "hop_cap = 3\ngoal_tokens = 7"),
        ] {
            let text = file(&[base.replacen(from, to, 1)]);
            let mut state = ReloadState::new(&running.config.enrollment);
            let decision = decide(
                &running.config.enrollment,
                &mut state,
                &read(2, &text, false, (&[], &[], &[])),
                &sections,
                running.memory_config,
                Switches::default(),
                at(1),
            )
            .expect("admitted");
            assert_eq!(
                decision.result.changed,
                vec![ChangedWorkspace {
                    id: "repo_1".to_owned(),
                    fields: vec![field.to_owned()],
                }],
                "{field}"
            );
            assert_eq!(
                decision.candidate.profiles[0].mark.map(|mark| mark.kind),
                Some(MarkKind::Changed),
                "{field}"
            );
        }
    }

    /// Marks accumulate for the run: a later change turns `added` into `changed` with a later
    /// time and a higher revision, a re-add after a removal is `changed`, and reverting an entry
    /// marks it again.
    #[test]
    fn marks_accumulate_and_a_readded_id_is_changed() {
        let root = tempfile::tempdir().expect("root");
        let (repo_1, repo_2, moved) = (
            repo(root.path(), "repo_1"),
            repo(root.path(), "repo_2"),
            repo(root.path(), "moved"),
        );
        let start = read(
            1,
            &file(&[entry("repo_1", &repo_1)]),
            false,
            (&[], &[], &[]),
        );
        let sections = start.config.clone();
        let mut running = start.config.enrollment.clone();
        let mut state = ReloadState::new(&running);
        let mut step =
            |seq: u64, text: String, change: (&[&str], &[&str], &[&str]), when: Timestamp| {
                let decision = decide(
                    &running,
                    &mut state,
                    &read(seq, &text, true, change),
                    &sections,
                    start.memory_config,
                    Switches::default(),
                    when,
                )
                .expect("admitted");
                assert!(decision.result.applied, "{:?}", decision.result);
                running = decision.candidate;
                (running.clone(), decision.resync)
            };
        let mark = |enrollment: &Enrollment, id: &str| enrollment.profile(id).and_then(|p| p.mark);
        let marked = |kind, when, revision| {
            Some(ReloadMark {
                kind,
                at: when,
                revision,
            })
        };

        let (after, resync) = step(
            2,
            file(&[entry("repo_1", &repo_1), entry("repo_2", &repo_2)]),
            (&["repo_2"][..], &[][..], &[][..]),
            at(1),
        );
        assert_eq!(resync, Resync::Coalesced, "an addition alone is coalesced");
        assert_eq!(
            mark(&after, "repo_1"),
            None,
            "an unchanged entry stays unmarked"
        );
        assert_eq!(mark(&after, "repo_2"), marked(MarkKind::Added, at(1), 1));

        let (after, resync) = step(
            3,
            file(&[entry("repo_1", &repo_1), entry("repo_2", &moved)]),
            (&[][..], &[][..], &["repo_2"][..]),
            at(2),
        );
        assert_eq!(resync, Resync::Now, "a change resyncs at once");
        assert_eq!(mark(&after, "repo_2"), marked(MarkKind::Changed, at(2), 2));

        let (after, resync) = step(
            4,
            file(&[entry("repo_2", &moved)]),
            (&[][..], &["repo_1"][..], &[][..]),
            at(3),
        );
        assert_eq!(resync, Resync::Now, "a removal resyncs at once");
        assert_eq!(
            mark(&after, "repo_2"),
            marked(MarkKind::Changed, at(2), 2),
            "an entry the file leaves unchanged keeps its mark and revision"
        );
        let (after, _) = step(
            5,
            file(&[entry("repo_1", &moved), entry("repo_2", &moved)]),
            (&["repo_1"][..], &[][..], &[][..]),
            at(4),
        );
        assert_eq!(
            mark(&after, "repo_1").map(|m| m.kind),
            Some(MarkKind::Changed),
            "a re-add is not an add"
        );

        let (after, _) = step(
            6,
            file(&[entry("repo_1", &repo_1), entry("repo_2", &moved)]),
            (&[][..], &[][..], &["repo_1"][..]),
            at(5),
        );
        assert_eq!(
            mark(&after, "repo_1").map(|m| (m.kind, m.at)),
            Some((MarkKind::Changed, at(5))),
            "a revert is a change"
        );
    }

    /// At most one add-only resync per window: the first goes at once, later ones wait for the
    /// window's end and share one wake, and an immediate resync covers a waiting one.
    #[test]
    fn add_only_resyncs_coalesce_within_the_window() {
        let mut state = ReloadState::new(&Enrollment::default());
        let start = Instant::now();
        let second = |seconds| start + Duration::from_secs(seconds);
        assert!(matches!(state.add_resync(start), AddResync::Now));
        let window_id = match state.add_resync(second(1)) {
            AddResync::At { wait, window_id } => {
                assert_eq!(wait, Duration::from_secs(4));
                window_id
            }
            _ => panic!("the second addition waits"),
        };
        assert!(matches!(state.add_resync(second(2)), AddResync::Covered));
        assert!(
            state.window_ended(window_id, second(5)),
            "one resync at the window's end"
        );
        assert!(!state.window_ended(window_id, second(5)), "and only one");
        let next_window = match state.add_resync(second(6)) {
            AddResync::At { wait, window_id } => {
                assert_eq!(wait, Duration::from_secs(4));
                window_id
            }
            _ => panic!("the next addition waits"),
        };
        assert!(
            next_window != window_id,
            "the resync at the window's end opens the next window"
        );
        state.resynced();
        assert!(
            !state.window_ended(next_window, second(10)),
            "an immediate resync covered the waiting addition"
        );
        assert!(matches!(state.add_resync(second(11)), AddResync::Now));
    }

    #[test]
    fn an_old_timer_cannot_end_a_later_add_resync_window() {
        let mut state = ReloadState::new(&Enrollment::default());
        let start = Instant::now();
        let second = |seconds| start + Duration::from_secs(seconds);

        assert!(matches!(state.add_resync(start), AddResync::Now));
        let AddResync::At {
            window_id: old_window,
            ..
        } = state.add_resync(second(1))
        else {
            panic!("the second addition waits");
        };
        state.resynced();
        assert!(matches!(state.add_resync(second(6)), AddResync::Now));
        let AddResync::At {
            window_id: current_window,
            ..
        } = state.add_resync(second(7))
        else {
            panic!("the fourth addition waits");
        };

        assert_ne!(old_window, current_window);
        assert!(
            !state.window_ended(old_window, second(8)),
            "the old timer owns no pending addition"
        );
        assert!(
            state.window_ended(current_window, second(11)),
            "the current window ends after five seconds"
        );
    }

    /// A start-time section the file sets differently from the running daemon is named once, by
    /// its table, and a file that matches names none. The terminal needs a restart only to turn on.
    #[test]
    fn each_start_time_section_that_differs_needs_a_restart() {
        let running = OperatorConfig {
            condense_model_enabled: true,
            ..OperatorConfig::default()
        };
        let mut file_read = read(1, "", false, (&[], &[], &[]));
        file_read.config.budget.floor_usd_per_day = Some(dudes_types::Usd::from_dollars(1));
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file_read),
            ["budget"]
        );
        file_read.config = running.clone();
        assert!(not_applied(&running, MemoryCondenseConfig::default(), &file_read).is_empty());
        file_read.config.terminal_enabled = true;
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file_read),
            ["floor.terminal"]
        );
        let started_with_terminal = OperatorConfig {
            terminal_enabled: true,
            ..running.clone()
        };
        file_read.config = running.clone();
        assert!(
            not_applied(
                &started_with_terminal,
                MemoryCondenseConfig::default(),
                &file_read
            )
            .is_empty(),
            "turning the terminal off is a switch, not a restart"
        );
    }

    /// An applied reload carries every `[floor.*]` switch the file changes, by its key, and a
    /// terminal the daemon did not start with stays off.
    #[test]
    fn a_reload_runs_the_switches_the_file_changes() {
        let running = read(1, "", false, (&[], &[], &[]));
        let sections = OperatorConfig {
            condense_model_enabled: true,
            ..OperatorConfig::default()
        };
        let text = "[floor.answer]\nenabled = true\n[floor.control]\nenabled = true\n\
                    [floor.goal]\nenabled = true\n[floor.terminal]\nenabled = true\n\
                    [floor.project]\nenabled = true\n";
        let file = read(2, text, true, (&[], &[], &[]));
        let mut state = ReloadState::new(&running.config.enrollment);
        let decision = decide(
            &running.config.enrollment,
            &mut state,
            &file,
            &sections,
            running.memory_config,
            Switches::default(),
            at(1),
        )
        .expect("admitted");
        assert!(decision.result.applied, "{:?}", decision.result);
        assert_eq!(
            decision.result.switches,
            [
                "floor.answer",
                "floor.control",
                "floor.goal",
                "floor.project"
            ]
        );
        assert_eq!(decision.result.not_applied, ["floor.terminal"]);
        assert!(decision.switches.answer && decision.switches.project);
        assert!(!decision.switches.terminal, "no restart, no terminal");
        assert_eq!(decision.resync, Resync::Now);
    }

    #[test]
    fn pi_restart_comparison_covers_changed_restored_and_omitted_settings() {
        let mut running = OperatorConfig::default();
        running.providers.pi = Some(dudes_providers::pi::config::PiConfig::default());
        let mut file = read(
            1,
            "[providers.pi]\ncommand = 'other-pi'\n[memory]\ncondense_model_enabled = false\n",
            false,
            (&[], &[], &[]),
        );
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file),
            ["providers.pi"]
        );
        file.config.providers = running.providers.clone();
        assert!(not_applied(&running, MemoryCondenseConfig::default(), &file).is_empty());
        file.config.providers.pi = None;
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file),
            ["providers.pi"]
        );
        running.providers.pi = None;
        assert!(not_applied(&running, MemoryCondenseConfig::default(), &file).is_empty());
    }

    /// A reload reads `[mcp.servers]` as structure only, so a variable the daemon lacks refuses
    /// nothing, and any difference from the running table needs a restart.
    #[test]
    fn mcp_servers_need_a_restart_and_parse_without_their_variables() {
        let declared = |url: &str| {
            format!(
                "[mcp.servers.memory]\nurl = '{url}'\nbearer_env = 'MEMORY_BEARER_NEVER_SET'\n[memory]\ncondense_model_enabled = false\n"
            )
        };
        let running = OperatorConfig::default();
        let file = read(
            1,
            &declared("https://memory.example.com/mcp"),
            false,
            (&[], &[], &[]),
        );
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file),
            ["mcp.servers"]
        );
        let same = OperatorConfig {
            mcp_servers: file.config.mcp_servers.clone(),
            ..OperatorConfig::default()
        };
        assert!(not_applied(&same, MemoryCondenseConfig::default(), &file).is_empty());
        let moved = read(
            2,
            &declared("https://other.example.com/mcp"),
            false,
            (&[], &[], &[]),
        );
        assert_eq!(
            not_applied(&same, MemoryCondenseConfig::default(), &moved),
            ["mcp.servers"]
        );
    }

    #[test]
    fn changing_codex_streaming_requires_restart_even_with_both_tables_present() {
        let mut running = OperatorConfig::default();
        running.providers.codex = Some(dudes_providers::codex::config::CodexConfig::default());
        let mut file = read(
            1,
            "[providers.codex]\nstreaming = true\n[memory]\ncondense_model_enabled = false\n",
            false,
            (&[], &[], &[]),
        );
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file),
            ["providers.codex"]
        );
        running.providers = file.config.providers.clone();
        assert!(not_applied(&running, MemoryCondenseConfig::default(), &file).is_empty());
        file.config.providers.codex.as_mut().unwrap().command = "other-codex".into();
        assert_eq!(
            not_applied(&running, MemoryCondenseConfig::default(), &file),
            ["providers.codex"]
        );
    }
}
