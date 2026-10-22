//! Whether the daemon finds each provider's program, and where its state lives, for the Floor's
//! `daemon` snapshot sibling and `system.doctor`'s provider table.
//!
//! The check asks [`crate::program::locate`], the search a launch runs, so a row describes the
//! file the next launch of that provider runs, unless the file changes in between. Program
//! discovery reads metadata and the kernel's access test. Opt-in Codex additionally admits its
//! managed home through bounded metadata probes owned by the daemon's one-shot runner.
//!
//! One thread runs every check, so a check blocked in the filesystem holds no loop or
//! connection thread. A reader waits at most one second after the running check started and then
//! goes without an answer. A result is reused for one second after it completes.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dudes_proto::rpc::system::{ProviderLoginHome, ProviderProbe, SignIn};
use dudes_providers::launch::EnvVar;
use dudes_types::Provider;
use serde::Serialize;

use crate::program;

/// How long a reader waits after the running check started.
const DEADLINE: Duration = Duration::from_secs(1);
/// How long after it completes a result answers a new reader.
const REUSE: Duration = Duration::from_secs(1);

const MAX_PROGRAM_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 1024;
const MAX_DETAIL_BYTES: usize = 2048;

/// One provider the registry can hire, and the program its adapter configures.
#[derive(Clone, Debug)]
pub(crate) struct Configured {
    pub(crate) provider: Provider,
    pub(crate) program: OsString,
}

/// What one check found for one provider: the file a launch would run, or why there is none.
#[derive(Clone, Debug)]
struct Found {
    login_home: Option<ProviderLoginHome>,
    provider: Provider,
    program: OsString,
    resolved: Result<PathBuf, program::Refusal>,
}

/// The daemon's one provider check. Clones share its thread, which ends when the last one drops.
#[derive(Clone, Debug)]
pub struct ProviderCheck(Arc<Owner>);

#[derive(Debug)]
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.changed.notify_all();
    }
}

#[derive(Debug)]
struct Shared {
    providers: Vec<Configured>,
    path: Option<OsString>,
    home: Home,
    state_dir: PathBuf,
    codex_profile: Option<Arc<crate::codex_profile::CodexProfile>>,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Checks asked for, numbered from 1. A new one is asked only while none runs.
    asked: u64,
    /// When check `asked` started, while it runs.
    running: Option<Instant>,
    finished: u64,
    /// The newest result that finished inside its deadline.
    fresh: Option<Fresh>,
    closed: bool,
    runs: usize,
    hold: Hold,
}

#[derive(Debug)]
struct Fresh {
    check: u64,
    at: Instant,
    found: Arc<[Found]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Hold {
    #[default]
    Idle,
    Armed,
    Held,
}

impl ProviderCheck {
    /// Start the check thread for `providers`, searching the `PATH` of `daemon_env`, which is the
    /// environment a managed child starts from.
    pub(crate) fn start(
        providers: Vec<Configured>,
        daemon_env: &[EnvVar],
        state_dir: PathBuf,
        codex_profile: Option<Arc<crate::codex_profile::CodexProfile>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            providers,
            path: program::child_path(daemon_env).map(OsStr::to_os_string),
            home: Home::of(daemon_env),
            state_dir,
            codex_profile,
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        let thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("dudes-provider-check".to_owned())
            .spawn(move || thread.serve())?;
        Ok(Self(Arc::new(Owner(shared))))
    }

    /// The `daemon` sibling's JSON, or `None` when the check did not answer in time.
    ///
    /// With `sign_in`, each found engine's row also says whether the engine is signed in, waiting for
    /// its status command as `system.doctor` does.
    pub(crate) fn sibling(
        &self,
        sign_in: Option<&crate::sign_in::SignInCheck>,
        busy: &std::collections::BTreeSet<Provider>,
    ) -> Option<Vec<u8>> {
        let shared = &self.0.0;
        let found = shared.found()?;
        let mut rows: Vec<ProviderProbe> = found.iter().map(probe).collect();
        if let Some(sign_in) = sign_in {
            sign_in.annotate_for_page(&mut rows, busy);
        }
        serde_json::to_vec(&shared.view(&found, &rows)).ok()
    }

    /// `system.doctor`'s provider rows.
    pub(crate) fn probes(&self) -> Vec<ProviderProbe> {
        let shared = &self.0.0;
        match shared.found() {
            Some(found) => found.iter().map(probe).collect(),
            None => shared
                .providers
                .iter()
                .map(|configured| ProviderProbe {
                    login_home: (configured.provider == Provider::Codex
                        && shared.codex_profile.is_some())
                    .then(|| ProviderLoginHome::Unavailable {
                        reason: "the managed profile check did not finish in time".into(),
                    }),
                    provider: configured.provider,
                    command: None,
                    version: None,
                    ok: false,
                    detail: Some("the provider check did not finish in time".to_owned()),
                    sign_in: None,
                })
                .collect(),
        }
    }

    /// Hold the next check where it starts, until [`Self::release`]. Release-unreachable: only a
    /// test that keeps the booted daemon has this value.
    #[doc(hidden)]
    pub fn hold_next(&self) {
        self.0.0.lock().hold = Hold::Armed;
    }

    /// Whether a check reached the hold within `bound`.
    #[doc(hidden)]
    #[must_use]
    pub fn wait_held(&self, bound: Duration) -> bool {
        let shared = &self.0.0;
        let state = shared.lock();
        let (state, _) = shared
            .changed
            .wait_timeout_while(state, bound, |state| state.hold != Hold::Held)
            .unwrap_or_else(PoisonError::into_inner);
        state.hold == Hold::Held
    }

    /// Let a held check go on.
    #[doc(hidden)]
    pub fn release(&self) {
        self.0.0.lock().hold = Hold::Idle;
        self.0.0.changed.notify_all();
    }

    /// How many checks have run.
    #[doc(hidden)]
    #[must_use]
    pub fn runs(&self) -> usize {
        self.0.0.lock().runs
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The check thread: run each check asked for, and keep its result only if it finished inside
    /// its deadline.
    fn serve(&self) {
        let mut state = self.lock();
        loop {
            while state.asked == state.finished && !state.closed {
                state = self.wait(state);
            }
            if state.closed {
                return;
            }
            let check = state.asked;
            let started = state.running.unwrap_or_else(Instant::now);
            drop(state);
            let found = self.run();
            state = self.lock();
            let at = Instant::now();
            state.finished = check;
            state.running = None;
            if at.duration_since(started) <= DEADLINE {
                state.fresh = Some(Fresh { check, at, found });
            }
            self.changed.notify_all();
        }
    }

    fn run(&self) -> Arc<[Found]> {
        {
            let mut state = self.lock();
            state.runs += 1;
            if state.hold == Hold::Armed {
                state.hold = Hold::Held;
                self.changed.notify_all();
                while state.hold == Hold::Held && !state.closed {
                    state = self.wait(state);
                }
            }
        }
        self.providers
            .iter()
            .map(|configured| {
                let resolved = program::locate(&configured.program, self.path.as_deref());
                let login_home = (configured.provider == Provider::Codex)
                    .then_some(self.codex_profile.as_ref())
                    .flatten()
                    .map(|profile| {
                        profile.login_home(resolved.as_ref().ok().map(PathBuf::as_path))
                    });
                Found {
                    provider: configured.provider,
                    program: configured.program.clone(),
                    resolved,
                    login_home,
                }
            })
            .collect()
    }

    /// A result that completed less than [`REUSE`] ago, or the running check's, waiting at most
    /// until [`DEADLINE`] after it started. `None` when it did not finish by then.
    fn found(&self) -> Option<Arc<[Found]>> {
        let mut state = self.lock();
        let now = Instant::now();
        if let Some(fresh) = &state.fresh
            && now.duration_since(fresh.at) < REUSE
        {
            return Some(Arc::clone(&fresh.found));
        }
        let started = if let Some(started) = state.running {
            started
        } else {
            state.asked += 1;
            state.running = Some(now);
            self.changed.notify_all();
            now
        };
        let check = state.asked;
        let deadline = started + DEADLINE;
        loop {
            if state.finished >= check {
                return state
                    .fresh
                    .as_ref()
                    .filter(|fresh| fresh.check == check)
                    .map(|fresh| Arc::clone(&fresh.found));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn view(&self, found: &[Found], rows: &[ProviderProbe]) -> DaemonView {
        DaemonView {
            state_dir: self.home.show(&self.state_dir),
            providers: found
                .iter()
                .zip(rows)
                .map(|(found, row)| ProviderView {
                    sign_in: row.sign_in.filter(|_| found.resolved.is_ok()),
                    provider: found.provider,
                    program: Path::new(&found.program)
                        .file_name()
                        .and_then(|name| displayable(name, MAX_PROGRAM_BYTES)),
                    found: found.resolved.is_ok(),
                    path: found
                        .resolved
                        .as_ref()
                        .ok()
                        .and_then(|path| self.home.show(path)),
                    detail: found
                        .resolved
                        .as_ref()
                        .err()
                        .and_then(|refusal| refusal.detail(|path| self.home.show(path)))
                        .and_then(|detail| displayable(OsStr::new(&detail), MAX_DETAIL_BYTES)),
                })
                .collect(),
        }
    }
}

/// The `daemon` sibling.
#[derive(Debug, Serialize)]
struct DaemonView {
    #[serde(skip_serializing_if = "Option::is_none")]
    state_dir: Option<String>,
    providers: Vec<ProviderView>,
}

#[derive(Debug, Serialize)]
struct ProviderView {
    provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    program: Option<String>,
    found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sign_in: Option<SignIn>,
}

/// A doctor row: the full path when it is UTF-8. A path that is not is left out with a detail,
/// because one such path would otherwise fail the whole reply's serialization.
fn probe(found: &Found) -> ProviderProbe {
    let path = found.resolved.as_ref().ok();
    let command = path.filter(|path| path.to_str().is_some()).cloned();
    let detail = (path.is_some() && command.is_none())
        .then(|| "the program was found at a path that is not valid UTF-8".to_owned());
    ProviderProbe {
        login_home: found.login_home.clone(),
        provider: found.provider,
        command,
        version: None,
        ok: path.is_some(),
        detail,
        sign_in: None,
    }
}

/// `value` as display text: 1 to `max` bytes of UTF-8 holding no control character, or nothing.
/// Omitted rather than truncated or lossily converted.
fn displayable(value: &OsStr, max: usize) -> Option<String> {
    let text = value.to_str()?;
    (!text.is_empty() && text.len() <= max && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

/// The daemon's home in the forms abbreviated to `~`: as written, and canonical.
#[derive(Clone, Debug, Default)]
struct Home {
    forms: Vec<PathBuf>,
    ignores_case: bool,
}

impl Home {
    /// `HOME`, or `USERPROFILE` on Windows, from the daemon's environment.
    fn of(env: &[EnvVar]) -> Self {
        let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        program::env_value(env, name).map_or_else(Self::default, |home| {
            let home = PathBuf::from(home);
            let ignores_case = volume_ignores_case(&home);
            Self::new(home, ignores_case)
        })
    }

    /// A home that is relative or the filesystem root abbreviates nothing.
    fn new(home: PathBuf, ignores_case: bool) -> Self {
        let usable = |path: &Path| path.is_absolute() && path.parent().is_some();
        if !usable(&home) {
            return Self::default();
        }
        let canonical = home
            .canonicalize()
            .ok()
            .filter(|canonical| usable(canonical) && *canonical != home);
        Self {
            forms: std::iter::once(home).chain(canonical).collect(),
            ignores_case,
        }
    }

    fn show(&self, path: &Path) -> Option<String> {
        match self.forms.iter().find_map(|home| self.within(home, path)) {
            Some(rest) if rest.as_os_str().is_empty() => Some("~".to_owned()),
            Some(rest) => displayable(Path::new("~").join(rest).as_os_str(), MAX_PATH_BYTES),
            None => displayable(path.as_os_str(), MAX_PATH_BYTES),
        }
    }

    /// What follows `home` in `path`, compared by whole components.
    fn within<'p>(&self, home: &Path, path: &'p Path) -> Option<&'p Path> {
        let mut rest = path.components();
        for part in home.components() {
            let next = rest.next()?;
            if !self.same(part.as_os_str(), next.as_os_str()) {
                return None;
            }
        }
        Some(rest.as_path())
    }

    fn same(&self, a: &OsStr, b: &OsStr) -> bool {
        a == b
            || (self.ignores_case
                && a.to_str()
                    .zip(b.to_str())
                    .is_some_and(|(a, b)| a.to_lowercase() == b.to_lowercase()))
    }
}

/// Whether the volume holding `home` ignores case, asked by looking the home up with its ASCII
/// case swapped: `pathconf(_PC_CASE_SENSITIVE)` has no safe binding, and this crate forbids
/// `unsafe`.
#[cfg(target_os = "macos")]
fn volume_ignores_case(home: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Some(text) = home.to_str() else {
        return false;
    };
    let swapped: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    if swapped == text {
        return false;
    }
    match (std::fs::metadata(home), std::fs::metadata(&swapped)) {
        (Ok(home), Ok(swapped)) => (home.dev(), home.ino()) == (swapped.dev(), swapped.ino()),
        _ => false,
    }
}

/// Windows path lookup ignores case by default.
#[cfg(windows)]
fn volume_ignores_case(_home: &Path) -> bool {
    true
}

#[cfg(not(any(target_os = "macos", windows)))]
fn volume_ignores_case(_home: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::program::{Refusal, RefusalHint};

    #[cfg(unix)]
    fn shown(home: &str, ignores_case: bool, path: &str) -> Option<String> {
        Home::new(PathBuf::from(home), ignores_case).show(Path::new(path))
    }

    #[cfg(unix)]
    #[test]
    fn browser_daemon_view_abbreviates_only_under_home() {
        assert_eq!(
            shown("/x/home/op", false, "/x/home/operator/s").as_deref(),
            Some("/x/home/operator/s")
        );
        assert_eq!(
            shown("/x/home/op", false, "/x/home/op/s").as_deref(),
            Some("~/s")
        );
        assert_eq!(
            shown("/x/home/op", false, "/x/home/op").as_deref(),
            Some("~")
        );

        let root = tempfile::tempdir().expect("a directory");
        let target = root.path().join("real-home");
        std::fs::create_dir(&target).expect("the home's target");
        let link = root.path().join("home-link");
        std::os::unix::fs::symlink(&target, &link).expect("a symlinked home");
        let through_target = target.canonicalize().expect("canonical").join("state");
        assert_eq!(
            Home::new(link.clone(), false)
                .show(&through_target)
                .as_deref(),
            Some("~/state"),
            "a path written through the home's target is under the home"
        );
        assert_eq!(
            Home::new(link, false).show(&root.path().join("elsewhere")),
            displayable(root.path().join("elsewhere").as_os_str(), MAX_PATH_BYTES),
            "control: a sibling of the home is not under it"
        );

        assert_eq!(
            shown("/Data/Al", true, "/data/al/s").as_deref(),
            Some("~/s")
        );
        assert_eq!(
            shown("/Data/Al", false, "/data/al/s").as_deref(),
            Some("/data/al/s"),
            "a case-sensitive volume compares case"
        );

        assert_eq!(shown("/", false, "/s").as_deref(), Some("/s"));
        assert_eq!(
            shown("relative", false, "relative/s").as_deref(),
            Some("relative/s")
        );
        assert_eq!(
            Home::of(&[]).show(Path::new("/home/op/s")).as_deref(),
            Some("/home/op/s"),
            "an unset home abbreviates nothing"
        );
        assert_eq!(
            shown("/home/op", false, "/etc/profiles/per-user/op/bin/prog").as_deref(),
            Some("/etc/profiles/per-user/op/bin/prog"),
            "an account name outside the home is shown, which is a stated limit"
        );
    }

    /// The volume's answer is asked, not assumed: the same case change abbreviates exactly when
    /// the volume holding the home resolves it to the same file.
    #[cfg(unix)]
    #[test]
    fn the_home_asks_its_volume_whether_case_matters() {
        let root = tempfile::tempdir().expect("a directory");
        let home = root.path().canonicalize().expect("canonical").join("Home");
        std::fs::create_dir(&home).expect("the home");
        std::fs::create_dir(home.join("State")).expect("a directory under it");
        let volume_ignores = home.parent().expect("a parent").join("hOME").exists();
        let changed = home.parent().expect("a parent").join("hOME").join("s");
        let env = [EnvVar::new("HOME", home.as_os_str())];
        let expected = if volume_ignores && cfg!(target_os = "macos") {
            "~/s".to_owned()
        } else {
            changed.to_str().expect("UTF-8").to_owned()
        };
        assert_eq!(Home::of(&env).show(&changed), Some(expected));
    }

    fn found(provider: Provider, program: &str, path: Option<PathBuf>) -> Found {
        Found {
            login_home: None,
            provider,
            program: OsString::from(program),
            resolved: path.ok_or_else(|| refusal(program, None)),
        }
    }

    fn refused(provider: Provider, program: &str, refusal: Refusal) -> Found {
        Found {
            login_home: None,
            provider,
            program: OsString::from(program),
            resolved: Err(refusal),
        }
    }

    fn refusal(written: impl Into<PathBuf>, hint: Option<RefusalHint>) -> Refusal {
        Refusal::Missing {
            written: written.into(),
            hint,
        }
    }

    fn shim(written: &str, shim: impl Into<PathBuf>) -> Refusal {
        refusal(written, Some(RefusalHint::WindowsShim(shim.into())))
    }

    /// The refusal of a Windows shim, in this test's own copy of the wording.
    fn shim_message(written: &str, shim: &str) -> String {
        format!(
            "`{written}` was not found as a runnable program; found Windows command shim `{shim}`, \
             which the daemon does not launch. Set the provider's `command` to the absolute path \
             of the real `.exe`. If the adapter supports script arguments, set `command` to the \
             absolute path of `node.exe` and pass the script separately."
        )
    }

    #[test]
    fn browser_daemon_view_shows_a_bounded_refusal_detail() {
        let name = "n".repeat(1000);
        let path = |len: usize| format!("/{}", "s".repeat(len - 1));
        let fixed = shim_message(&name, "").len();
        let at_cap = path(2048 - fixed);
        assert_eq!(shim_message(&name, &at_cap).len(), 2048);
        assert_eq!(
            view_of(
                Home::default(),
                "/state",
                &[refused(Provider::Claude, &name, shim(&name, &at_cap))]
            ),
            json!({"state_dir":"/state","providers":[{
                "provider":"claude","found":false,"detail":shim_message(&name, &at_cap)
            }]})
        );
        let over_cap = path(2049 - fixed);
        assert_eq!(
            view_of(
                Home::default(),
                "/state",
                &[refused(Provider::Claude, &name, shim(&name, &over_cap))]
            ),
            json!({"state_dir":"/state","providers":[{"provider":"claude","found":false}]})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refusal_shows_the_home_as_tilde_in_the_browser_and_in_full_at_launch() {
        let home = || Home::new(PathBuf::from("/home/op"), false);
        let under_home = shim("claude", "/home/op/AppData/Roaming/npm/claude.cmd");
        assert_eq!(
            std::io::Error::from(under_home.clone()).to_string(),
            shim_message("claude", "/home/op/AppData/Roaming/npm/claude.cmd"),
            "the launch's error keeps the full path"
        );
        let view = view_of(
            home(),
            "/home/op/state",
            &[refused(Provider::Claude, "claude", under_home)],
        );
        assert_eq!(
            view,
            json!({"state_dir":"~/state","providers":[{
                "provider":"claude","program":"claude","found":false,
                "detail":shim_message("claude", "~/AppData/Roaming/npm/claude.cmd")
            }]})
        );
        assert!(!view.to_string().contains("/home/op"));

        let executable = refusal(
            "/home/op/tools/codex",
            Some(RefusalHint::WindowsExecutable(PathBuf::from(
                "/home/op/tools/codex.exe",
            ))),
        );
        assert_eq!(
            view_of(
                home(),
                "/state",
                &[refused(Provider::Codex, "/home/op/tools/codex", executable)]
            )["providers"][0]["detail"],
            "`~/tools/codex` is not a file the daemon may run; `~/tools/codex.exe` exists. Set the \
             provider's `command` to the full absolute path of `~/tools/codex.exe`. `~` is only a \
             display abbreviation and is not expanded in configuration.",
            "the configured command is shown the same way"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refusal_path_outside_the_home_is_shown_in_full_and_one_d4_cannot_show_drops_the_detail() {
        use std::os::unix::ffi::OsStrExt as _;

        let detail = |at: PathBuf| {
            view_of(
                Home::new(PathBuf::from("/home/op"), false),
                "/state",
                &[refused(Provider::Claude, "claude", shim("claude", at))],
            )["providers"][0]
                .get("detail")
                .cloned()
        };
        assert_eq!(
            detail(PathBuf::from("/opt/npm/claude.cmd")),
            Some(json!(shim_message("claude", "/opt/npm/claude.cmd"))),
            "a path outside the home is shown in full, as `path` is"
        );
        let long = PathBuf::from(format!("/home/op/{}.cmd", "s".repeat(1020)));
        assert_eq!(detail(long.clone()), None, "its `~` form is 1,026 bytes");
        assert!(
            std::io::Error::from(shim("claude", long.clone()))
                .to_string()
                .contains(long.to_str().expect("UTF-8")),
            "the launch still names it"
        );
        assert_eq!(
            detail(PathBuf::from(OsStr::from_bytes(
                b"/home/op/\xff/claude.cmd"
            ))),
            None,
            "a path that is not UTF-8"
        );
    }

    fn view_of(home: Home, state_dir: &str, rows: &[Found]) -> Value {
        let shared = Shared {
            codex_profile: None,
            providers: Vec::new(),
            path: None,
            home,
            state_dir: PathBuf::from(state_dir),
            state: Mutex::default(),
            changed: Condvar::new(),
        };
        let probes: Vec<ProviderProbe> = rows.iter().map(probe).collect();
        serde_json::to_value(shared.view(rows, &probes)).expect("the view serializes")
    }

    #[test]
    fn browser_daemon_view_omits_undisplayable_values() {
        let long = |len: usize| format!("/{}", "p".repeat(len - 1));
        let rows = [
            found(
                Provider::Claude,
                "claude",
                Some(PathBuf::from("/bin/a\u{7}b")),
            ),
            found(Provider::Codex, "codex", Some(PathBuf::from(long(1025)))),
            found(Provider::Pi, &"n".repeat(256), None),
            found(Provider::Kimi, "kimi", Some(PathBuf::from(long(1024)))),
            found(Provider::Qwen, &"q".repeat(255), None),
        ];
        assert_eq!(
            view_of(Home::default(), &long(1025), &rows),
            json!({"providers":[
                {"provider":"claude","program":"claude","found":true},
                {"provider":"codex","program":"codex","found":true},
                {"provider":"pi","found":false},
                {"provider":"kimi","program":"kimi","found":true,"path":long(1024)},
                {"provider":"qwen","program":"q".repeat(255),"found":false},
            ]})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_omitted_from_the_view_and_from_doctor() {
        use std::os::unix::ffi::OsStrExt as _;

        let path = PathBuf::from(OsStr::from_bytes(b"/bin/\xff/prog"));
        let row = found(Provider::Claude, "prog", Some(path));
        assert_eq!(
            view_of(Home::default(), "/state", std::slice::from_ref(&row)),
            json!({"state_dir":"/state","providers":[
                {"provider":"claude","program":"prog","found":true}
            ]})
        );
        let doctor = probe(&row);
        assert_eq!(
            serde_json::to_value(&doctor).expect("one row that is not UTF-8 still serializes"),
            json!({"provider":"claude","ok":true,
                "detail":"the program was found at a path that is not valid UTF-8"})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_doctor_row_carries_the_full_path_and_no_version() {
        let home = Home::new(PathBuf::from("/h"), false);
        let row = found(Provider::Claude, "prog", Some(PathBuf::from("/h/bin/prog")));
        assert_eq!(
            serde_json::to_value(probe(&row)).expect("serializes"),
            json!({"provider":"claude","command":"/h/bin/prog","ok":true})
        );
        assert_eq!(
            view_of(home, "/h/state", &[row])["providers"][0]["path"],
            "~/bin/prog",
            "the Floor's row is the display form of the same file"
        );
        let missing = found(Provider::Claude, "prog", None);
        assert_eq!(
            serde_json::to_value(probe(&missing)).expect("serializes"),
            json!({"provider":"claude","ok":false})
        );
    }
}
