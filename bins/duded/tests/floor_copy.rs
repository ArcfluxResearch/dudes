//! A browser hire into its own copy of a trusted project, driven through the real Floor gateway
//! against a real daemon, the real one-shot runner and real git. No model is called.
//!
//! The operator trusts a project through `workspace.trust` over the socket, as `dudes project trust`
//! does. The page only asks for the copy.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, SharedHost, TestClock};
use duded::config::load_operator;
use duded::floor_browser::Page;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use duded::worktree::git::Git;
use duded::worktree::trust;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::Record;
use dudes_proto::rpc::agent::{Checkout, HireParams, HireResult};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{Method, RpcError};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::LaunchSpec;
use dudes_providers::registry::Adapters;
use dudes_rpc::Client;
use dudes_supervisor::Clock as _;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v6"
);
const WAIT: Duration = Duration::from_secs(20);

struct Real(OneShot);

impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|outcome| match outcome {
            OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            OneShotOutcome::Swept => PredicateOutcome::Swept,
        })
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
            .map(|outcome| match outcome {
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

fn git_for_tests() -> Git {
    Git::new(
        Arc::new(Real(OneShot::new())),
        std::env::var_os("PATH"),
        Duration::from_secs(60),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git_for_tests()
        .exec(dir, &args, Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"))
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

/// Commit `content` at `path` with the given git mode without touching the working tree, so a
/// name that differs only by case from another can exist even on a case-insensitive file system.
fn commit_entry(dir: &Path, mode: &str, path: &str, content: &str) {
    let blob = dir.join(".git").join("entry-content");
    std::fs::write(&blob, content).unwrap();
    let oid = git(dir, &["hash-object", "-w", blob.to_str().unwrap()]);
    std::fs::remove_file(&blob).unwrap();
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{oid},{path}"),
        ],
    );
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "entry",
        ],
    );
}

/// A repository with one commit, outside the daemon's directories.
fn repo(root: &Path) -> PathBuf {
    let repo = std::fs::canonicalize(root).expect("canonical").join("repo");
    std::fs::create_dir_all(&repo).expect("repo directory");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write");
    commit_all(&repo, "first");
    repo
}

const BUDGET: &str = "[budget]\nfloor_usd_per_day = 100\n\n[budget.per_turn_floor]\nusd = 0.000001\ntokens = 1\n\n[floor.hire]\nenabled = true\n\n[memory]\ncondense_model_enabled = false\n";

/// The operator file, with the project `repo_1` at `repo` when there is one.
fn operator_file(repo: Option<&Path>) -> String {
    let program = serde_json::to_string(common::true_program()).expect("TOML string");
    let mut text = BUDGET.to_owned();
    if let Some(repo) = repo {
        let _ = write!(
            text,
            "\n[[floor.onboarding.workspaces]]\nworkspace_id = 'repo_1'\nlabel = 'Service'\ncompletion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [{program}]\npredicate_timeout_s = 10\nmay_already_pass = true\n",
            repo.display()
        );
    }
    text
}

struct Floor {
    running: common::Running,
    settings: duded::Settings,
    addr: SocketAddr,
    repo: PathBuf,
    clock: Arc<TestClock>,
}

fn floor(fixture: &Fixture) -> Floor {
    floor_with(fixture, ClaudeConfig::default())
}

fn floor_with(fixture: &Fixture, claude: ClaudeConfig) -> Floor {
    let repo = repo(fixture.runtime.path());
    let config = fixture.state.path().join("config.toml");
    std::fs::write(&config, operator_file(Some(&repo))).expect("operator file");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.predicate_path = std::env::var_os("PATH");
    settings.enrollment = load_operator(&config).expect("valid file").enrollment;
    settings.operator_config = Some(config);
    let clock = Arc::new(TestClock::new());
    let mut seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(Real(OneShot::new())),
        clock.clone(),
        Witness::default(),
    );
    seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(claude)) as Box<dyn Adapter>]);
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    Floor {
        running: common::start(daemon),
        settings,
        addr,
        repo,
        clock,
    }
}

/// Replace the operator file and have the daemon adopt it, as `dudes project add` and `remove` do.
fn adopt(floor: &Floor, text: &str, added: &[&str], removed: &[&str]) {
    use sha2::Digest as _;
    std::fs::write(floor.settings.operator_config.as_ref().unwrap(), text).expect("rewrite");
    let sha = sha2::Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let result = operator(
        floor,
        Method::FloorEnrollmentReload,
        json!({"apply": {"sha256": sha, "added": added, "removed": removed, "changed": []}}),
    )
    .expect("the reload");
    assert_eq!(result["applied"], true, "{result}");
}

fn credential(floor: &Floor) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            floor
                .settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential"),
    )
    .expect("credential")
}

fn operator(floor: &Floor, method: Method, params: Value) -> Result<Value, RpcError> {
    let mut client = Client::connect(floor.settings.socket()).expect("daemon socket");
    let reply = client
        .call_as_operator(method, Some(params), credential(floor))
        .expect("answered");
    match reply.error {
        Some(error) => Err(error),
        None => Ok(reply.result.expect("a result")),
    }
}

fn set_trust(floor: &Floor, trusted: bool) -> Result<Value, RpcError> {
    operator(
        floor,
        Method::WorkspaceTrust,
        json!({ "workspace_id": "repo_1", "trusted": trusted }),
    )
}

fn session(floor: &Floor) -> Page {
    let clock = Arc::clone(&floor.clock);
    Page::enrol_new_on(
        floor.addr,
        &common::floor_link(&floor.settings),
        move || clock.now().unix_millis(),
    )
}

/// One browser hire of `repo_1` that asks for its own copy; the answer's status and body.
fn hire_in_copy(floor: &Floor, attempt: char) -> (u16, Value) {
    post_copy(&session(floor), attempt)
}

fn post_copy(page: &Page, attempt: char) -> (u16, Value) {
    post_copy_at(page, attempt, None)
}

/// A hire of a project a reload added must present that reload's revision.
fn post_copy_at(page: &Page, attempt: char, revision: Option<&str>) -> (u16, Value) {
    let reply = page.post(
        "/floor/onboard",
        &json!({
            "attempt_id": format!("{attempt}{}", "A".repeat(42)),
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
            "check_id": "default",
            "check_revision": common::check_revision_as(
                "default", "Checks pass", &[common::true_program()], 10, true),
            "own_copy": true,
            "workspace_revision": revision,
        }),
    );
    (reply.status(), reply.json())
}

fn hired_agents(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .count()
}

fn leases(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::WorktreeLeased { .. }))
        .count()
}

/// The directory the daemon keeps copies in, if it made any.
fn trees(floor: &Floor) -> usize {
    std::fs::read_dir(floor.settings.state_dir.join("worktrees")).map_or(0, Iterator::count)
}

#[track_caller]
fn assert_refused_with(reply: &(u16, Value), words: &str, fixture: &Fixture, floor: &Floor) {
    assert_eq!(reply.0, 409, "{reply:?}");
    assert_eq!(reply.1["state"], "refused", "{reply:?}");
    assert_eq!(reply.1["reason"], "own_copy", "{reply:?}");
    let note = reply.1["note"].as_str().expect("a plain sentence");
    assert!(note.contains(words), "{note}");
    assert!(note.contains("Nobody was hired"), "{note}");
    assert_eq!(hired_agents(fixture), 0, "nobody was hired");
    assert_eq!(leases(fixture), 0, "no copy was leased");
    assert_eq!(trees(floor), 0, "no copy was made");
}

#[test]
fn a_trusted_project_hires_into_its_own_copy_as_the_command_line_does() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");

    let (status, body) = hire_in_copy(&floor, 'A');
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "complete", "{body}");

    // The same hire from the operator's command line, for comparison.
    let mut params: HireParams = common::hire_params(&floor.repo);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    let cli: HireResult = serde_json::from_value(
        operator(
            &floor,
            Method::AgentHire,
            serde_json::to_value(params).unwrap(),
        )
        .expect("the command line hire"),
    )
    .expect("typed result");

    let entries = fixture.entries();
    let grant = entries
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::WorkspaceTrustSet { trusted: true, .. }
            )
            .then_some(*lsn)
        })
        .expect("the grant is journaled");
    let hires: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match &entry.record {
            Record::AgentHired {
                id,
                cwd,
                trusted,
                worktree,
                browser_onboarding,
                ..
            } => Some((
                *id,
                cwd.clone(),
                *trusted,
                *worktree,
                browser_onboarding.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(hires.len(), 2);
    let (browser, command_line) = (&hires[0], &hires[1]);
    assert_eq!(command_line.0, cli.agent);
    for (_, cwd, trusted, worktree, _) in [browser, command_line] {
        assert!(*trusted, "a copy is a trusted hire");
        assert!(worktree.is_some(), "the hire names its lease");
        assert!(
            cwd.starts_with(floor.settings.state_dir.join("worktrees"))
                && cwd.join(".git").is_file(),
            "the agent works in a real tree the daemon made: {}",
            cwd.display()
        );
    }
    assert_eq!(
        browser
            .4
            .as_ref()
            .and_then(|onboarding| onboarding.trust_lsn),
        Some(grant),
        "the hire names the grant it used"
    );
    let leased = leases(&fixture);
    assert_eq!(leased, 2, "one lease for each hire");
    let tree = &browser.1;
    assert_eq!(
        git(tree, &["rev-parse", "--abbrev-ref", "HEAD"])
            .split('/')
            .next(),
        Some("dudes"),
        "the tree is on a branch of its own"
    );
    assert_ne!(*tree, floor.repo, "and it is not the project's folder");
}

/// The digests of a copy hire that names no model are today's, and the model reaches the agent
/// whether it is hired from the browser into a copy or from the command line.
#[test]
fn a_copy_hire_keeps_its_digest_and_every_hire_path_records_the_model() {
    use sha2::{Digest as _, Sha256};
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let page = session(&floor);
    let revision = common::check_revision_as(
        "default",
        "Checks pass",
        &[common::true_program()],
        10,
        true,
    );
    let named = |attempt: char, model: Option<&str>| {
        let mut body = json!({
            "attempt_id": format!("{attempt}{}", "A".repeat(42)),
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
            "check_id": "default", "check_revision": revision, "own_copy": true,
        });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        body
    };
    assert_eq!(page.post("/floor/onboard", &named('A', None)).status(), 200);
    assert_eq!(
        page.post("/floor/onboard", &named('B', Some("opus")))
            .status(),
        200
    );
    let mut params: HireParams = common::hire_params(&floor.repo);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    params.model = Some(dudes_types::ModelName::parse("haiku").unwrap());
    operator(
        &floor,
        Method::AgentHire,
        serde_json::to_value(params).unwrap(),
    )
    .expect("the command line hire");

    let framed = |domain: &[u8], fields: &[(&str, &str)]| {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        for (field, value) in fields {
            hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
            hasher.update(field.as_bytes());
            hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
            hasher.update(value.as_bytes());
        }
        let digest: [u8; 32] = hasher.finalize().into();
        digest
    };
    let common_fields = [
        ("workspace_id", "repo_1"),
        ("name", "Kim"),
        ("role", "engineer"),
        ("provider", "claude"),
        ("character", ""),
        ("accent", ""),
        ("check_id", "default"),
        ("check_revision", revision.as_str()),
    ];
    let mut v5 = common_fields.to_vec();
    v5.push(("own_copy", "true"));
    let mut v7 = v5.clone();
    v7.push(("model", "opus"));
    let hired: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                model,
                browser_onboarding,
                ..
            } => Some((
                model.map(|name| name.to_string()),
                browser_onboarding.map(|meta| meta.request_digest.bytes()),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(hired.len(), 3);
    assert_eq!(
        hired[0],
        (None, Some(framed(b"dudes.onboard.nogoal.v5\0", &v5)))
    );
    assert_eq!(
        hired[1],
        (
            Some("opus".to_owned()),
            Some(framed(b"dudes.onboard.nogoal.v7\0", &v7))
        )
    );
    assert_eq!(hired[2], (Some("haiku".to_owned()), None));
}

/// A copy hire that names a model and no check is admitted and records the model. Its digest is
/// neither the same request's without the model nor a plain hire's with the model, and the literal
/// digests pin the length-prefixed framing of the copy framing and the model framing.
#[test]
fn a_copy_hire_with_a_model_and_no_check_is_admitted_under_its_own_digest() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let page = session(&floor);
    let request = |attempt: char, copy: bool, model: Option<&str>| {
        let mut body = json!({
            "attempt_id": format!("{attempt}{}", "A".repeat(42)),
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
        });
        if copy {
            body["own_copy"] = json!(true);
        }
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        body
    };
    let with_model = page.post("/floor/onboard", &request('A', true, Some("opus")));
    assert_eq!(with_model.status(), 200, "{}", with_model.json());
    assert_eq!((hired_agents(&fixture), leases(&fixture)), (1, 1));
    let hired = |fixture: &Fixture| -> Vec<(Option<String>, String)> {
        fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::AgentHired {
                    model,
                    browser_onboarding: Some(meta),
                    ..
                } => Some((
                    model.map(|name| name.to_string()),
                    meta.request_digest
                        .bytes()
                        .iter()
                        .fold(String::new(), |mut hex, byte| {
                            write!(hex, "{byte:02x}").unwrap();
                            hex
                        }),
                )),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        hired(&fixture),
        [(
            Some("opus".to_owned()),
            "88975884fb60e51ecd9abd432a5ed5504a4ad76c1a80d6d95ee5ab1e970d0c2f".to_owned()
        )]
    );
    for other in [request('A', true, None), request('A', false, Some("opus"))] {
        assert_eq!(
            page.post("/floor/onboard", &other).status(),
            409,
            "the same attempt with another framing conflicts"
        );
    }
    let without = page.post("/floor/onboard", &request('B', true, None));
    assert_eq!(without.status(), 200, "{}", without.json());
    assert_eq!(
        hired(&fixture)[1],
        (
            None,
            "d7f8fcb9484d208b7458e9d383629515bb63445969e18b1e7d32f1e86672381c".to_owned()
        )
    );
}

/// A model in the Claude configuration's extra arguments is appended last and would beat a hire's
/// model without the journal knowing, so a hire that names one is refused whole, from the browser
/// and from the command line, while a hire that names none still goes through.
#[test]
fn a_claude_hire_that_names_a_model_is_refused_when_the_extra_arguments_set_one() {
    let fixture = Fixture::new();
    let claude = ClaudeConfig {
        extra_args: ["--model=haiku"].map(std::ffi::OsString::from).to_vec(),
        ..ClaudeConfig::default()
    };
    let floor = floor_with(&fixture, claude);
    let page = session(&floor);
    let request = |attempt: char, model: Option<&str>| {
        let mut body = json!({
            "attempt_id": format!("{attempt}{}", "A".repeat(42)),
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
        });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        body
    };
    let refused = page.post("/floor/onboard", &request('A', Some("opus")));
    assert_eq!(refused.status(), 409);
    assert_eq!(refused.json()["reason"], "model_fixed");
    assert!(
        refused.json()["note"]
            .as_str()
            .is_some_and(|note| note.contains("configuration sets this engine's model"))
    );
    let mut params: HireParams = common::hire_params(&floor.repo);
    params.model = Some(dudes_types::ModelName::parse("opus").unwrap());
    let error = operator(
        &floor,
        Method::AgentHire,
        serde_json::to_value(params).unwrap(),
    )
    .expect_err("the command line hire is refused too");
    assert!(error.message.contains("configuration sets"), "{error:?}");
    assert_eq!(hired_agents(&fixture), 0, "nothing was journaled");
    assert_eq!(
        page.post("/floor/onboard", &request('B', None)).status(),
        200
    );
    assert_eq!(hired_agents(&fixture), 1);
}

#[test]
fn a_hire_into_its_own_copy_needs_no_check() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let page = session(&floor);
    let request = json!({
        "attempt_id": format!("A{}", "A".repeat(42)),
        "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
        "own_copy": true,
    });
    let first = page.post("/floor/onboard", &request);
    assert_eq!(
        (first.status(), first.json()["state"].clone()),
        (200, json!("complete"))
    );
    assert_eq!((hired_agents(&fixture), leases(&fixture)), (1, 1));
    let retry = page.post("/floor/onboard", &request);
    assert_eq!(retry.json(), first.json());
    assert_eq!((hired_agents(&fixture), leases(&fixture)), (1, 1));
}

#[test]
fn an_own_copy_hire_never_answers_from_a_plain_hire_with_the_same_attempt() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let page = session(&floor);
    let plain = json!({
        "attempt_id": format!("A{}", "A".repeat(42)),
        "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
    });
    let mut copy = plain.clone();
    copy["own_copy"] = json!(true);
    assert_eq!(page.post("/floor/onboard", &plain).status(), 200);
    let second = page.post("/floor/onboard", &copy);
    assert_eq!(
        second.status(),
        409,
        "a different request under the same attempt conflicts"
    );
    assert_eq!((hired_agents(&fixture), leases(&fixture)), (1, 0));
}

#[test]
fn a_copy_is_refused_in_plain_words_and_nothing_is_made() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);

    let untrusted = hire_in_copy(&floor, 'A');
    assert_refused_with(&untrusted, "not trusted yet", &fixture, &floor);
    assert!(
        untrusted.1["note"]
            .as_str()
            .unwrap()
            .contains("dudes project trust repo_1")
    );

    set_trust(&floor, true).expect("the grant");
    std::fs::create_dir_all(floor.repo.join(".claude")).unwrap();
    std::fs::write(floor.repo.join(".claude").join("settings.json"), "{}\n").unwrap();
    commit_all(&floor.repo, "settings");
    let moved = hire_in_copy(&floor, 'B');
    assert_refused_with(&moved, "changed since you trusted it", &fixture, &floor);

    set_trust(&floor, true).expect("trusted again");
    set_trust(&floor, false).expect("and untrusted");
    let revoked = hire_in_copy(&floor, 'C');
    assert_refused_with(&revoked, "not trusted yet", &fixture, &floor);

    set_trust(&floor, true).expect("trusted once more");
    // A folder whose `.git` is no longer a repository: git finds none above it either.
    std::fs::remove_dir_all(floor.repo.join(".git")).unwrap();
    std::fs::create_dir(floor.repo.join(".git")).unwrap();
    let gone = hire_in_copy(&floor, 'D');
    assert_refused_with(&gone, "not a git repository", &fixture, &floor);
}

#[test]
fn a_stale_grant_stops_a_copy_before_the_tree_is_made() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let vouch = trust::inspect(&git_for_tests(), &floor.repo).expect("vouched");
    let worktrees =
        duded::worktree::hire::WorktreeHires::new(floor.running.handle.clone(), git_for_tests());
    let mut params: HireParams = common::hire_params(&floor.repo);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    let ticket = duded::worktree::hire::TrustTicket {
        workspace_id: "repo_1".into(),
        lsn: dudes_types::Lsn(1),
        vouch,
    };
    let failure = worktrees
        .prepare(&params, Some(&ticket))
        .expect_err("a grant that is not the latest ends the copy");
    assert_eq!(failure.why, duded::worktree::hire::Why::TrustChanged);
    assert_eq!(leases(&fixture), 0);
    assert_eq!(trees(&floor), 0);
}

#[test]
fn a_grant_is_refused_for_each_setting_it_cannot_cover() {
    let fixture = Fixture::new();
    let repo = repo(fixture.runtime.path());
    let git_tool = git_for_tests();
    let inspect = || trust::inspect(&git_tool, &repo);
    let before = inspect().expect("a plain repository is accepted");

    for (key, value) in [
        ("include.path", "../other"),
        ("includeIf.gitdir:/x/.path", "../other"),
        ("core.attributesFile", "/tmp/attributes"),
        ("extensions.worktreeConfig", "true"),
        ("filter.lfs.clean", "cat"),
    ] {
        git(&repo, &["config", "--local", key, value]);
        assert!(
            matches!(inspect(), Err(trust::TrustRefusal::Unsupported(_))),
            "{key} is refused"
        );
        git(&repo, &["config", "--local", "--unset-all", key]);
    }
    assert_eq!(inspect().expect("clean again"), before);

    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", repo.join(".claude").join("settings.json")).unwrap();
    commit_all(&repo, "link");
    assert_eq!(inspect(), Err(trust::TrustRefusal::Symlink));
    git(&repo, &["rm", "-q", "-r", ".claude"]);
    commit_all(&repo, "unlink");
    std::os::unix::fs::symlink("/etc/hosts", repo.join(".mcp.json")).unwrap();
    commit_all(&repo, "mcp link");
    assert_eq!(inspect(), Err(trust::TrustRefusal::Symlink));
    git(&repo, &["rm", "-q", "-f", ".mcp.json"]);
    commit_all(&repo, "mcp unlink");
    assert!(
        inspect().is_ok(),
        "a control: the repository is clean again"
    );

    commit_entry(&repo, "120000", ".MCP.json", "/etc/hosts");
    assert_eq!(
        inspect(),
        Err(trust::TrustRefusal::Symlink),
        "case does not hide a link"
    );
    git(&repo, &["rm", "-q", "--cached", ".MCP.json"]);
    commit_entry(&repo, "100644", ".claude/a.json", "{}\n");
    commit_entry(&repo, "100644", ".Claude/b.json", "{}\n");
    assert_eq!(inspect(), Err(trust::TrustRefusal::CaseVariants));
}

#[test]
fn ordinary_work_keeps_the_pin_and_a_settings_change_moves_it() {
    let fixture = Fixture::new();
    let repo = repo(fixture.runtime.path());
    let git_tool = git_for_tests();
    let granted = trust::inspect(&git_tool, &repo).expect("vouched");
    let verify = || {
        let base = git_tool.commit(&repo, "HEAD").expect("HEAD");
        trust::verify(&git_tool, &repo, base, &granted)
    };
    assert_eq!(verify(), Ok(()));

    git(
        &repo,
        &["remote", "add", "origin", "https://example.com/x.git"],
    );
    git(&repo, &["config", "branch.main.remote", "origin"]);
    std::fs::write(repo.join("tracked.txt"), "two\n").unwrap();
    commit_all(&repo, "ordinary");
    assert_eq!(
        verify(),
        Ok(()),
        "a remote, tracking and a commit leave the pin"
    );

    git(&repo, &["config", "core.autocrlf", "true"]);
    assert_eq!(verify(), Err(trust::TrustRefusal::Changed));
    git(&repo, &["config", "--unset", "core.autocrlf"]);
    assert_eq!(verify(), Ok(()));

    std::fs::write(
        repo.join(".git").join("info").join("attributes"),
        "* text\n",
    )
    .unwrap();
    assert_eq!(verify(), Err(trust::TrustRefusal::Changed));
    std::fs::remove_file(repo.join(".git").join("info").join("attributes")).unwrap();

    std::fs::write(repo.join(".mcp.json"), "{}\n").unwrap();
    commit_all(&repo, "mcp");
    assert_eq!(verify(), Err(trust::TrustRefusal::Changed));
}

#[test]
fn a_settings_entry_in_another_case_moves_the_pin() {
    let fixture = Fixture::new();
    let repo = repo(fixture.runtime.path());
    let git_tool = git_for_tests();
    let granted = trust::inspect(&git_tool, &repo).expect("vouched");
    let verify = || {
        let base = git_tool.commit(&repo, "HEAD").expect("HEAD");
        trust::verify(&git_tool, &repo, base, &granted)
    };
    commit_entry(&repo, "100644", ".Claude/settings.json", "{}\n");
    assert_eq!(
        verify(),
        Err(trust::TrustRefusal::Changed),
        ".Claude is pinned"
    );
    git(&repo, &["rm", "-q", "--cached", ".Claude/settings.json"]);
    commit_entry(&repo, "100644", ".MCP.json", "{}\n");
    assert_eq!(
        verify(),
        Err(trust::TrustRefusal::Changed),
        ".MCP.json is pinned"
    );
}

fn trust_records(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::WorkspaceTrustSet { .. }))
        .count()
}

#[test]
fn an_untrust_with_nothing_to_revoke_appends_nothing() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let untrust = |id: &str| {
        operator(
            &floor,
            Method::WorkspaceTrust,
            json!({ "workspace_id": id, "trusted": false }),
        )
        .expect("answered")
    };
    assert_eq!(untrust("repo_1")["changed"], false);
    assert_eq!(
        untrust("gone")["changed"],
        false,
        "an unknown project needs no enrollment"
    );
    assert_eq!(trust_records(&fixture), 0, "no required record for nothing");

    assert_eq!(set_trust(&floor, true).unwrap()["changed"], true);
    assert_eq!(untrust("repo_1")["changed"], true);
    assert_eq!(untrust("repo_1")["changed"], false);
    assert_eq!(trust_records(&fixture), 2, "one grant and one revocation");
}

#[test]
fn removing_a_project_revokes_its_grant_and_adding_it_back_needs_trust_again() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    adopt(&floor, &operator_file(None), &[], &["repo_1"]);
    assert_eq!(trust_records(&fixture), 2, "the removal revoked the grant");

    adopt(&floor, &operator_file(Some(&floor.repo)), &["repo_1"], &[]);
    // The reload that added the project back marked it with revision 1, which a hire must present.
    let page = session(&floor);
    let back = post_copy_at(&page, 'A', Some("1"));
    assert_refused_with(&back, "not trusted yet", &fixture, &floor);

    set_trust(&floor, true).expect("trusted again");
    let again = post_copy_at(&page, 'B', Some("1"));
    assert_eq!(again.0, 200, "and then the copy is made: {again:?}");
}

#[test]
fn a_revoke_between_the_copy_and_the_hire_removes_the_copy_and_hires_nobody() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    set_trust(&floor, true).expect("the grant");
    let (reached, release) = floor.running.handle.hold_after_copy();
    let held = {
        let page = session(&floor);
        std::thread::spawn(move || post_copy(&page, 'A'))
    };
    reached
        .recv_timeout(WAIT)
        .expect("the copy was made and the hire is waiting");
    assert_eq!(leases(&fixture), 1, "the lease is journaled");
    assert_eq!(trees(&floor), 1, "and the tree is on disk");
    assert_eq!(
        operator(
            &floor,
            Method::WorkspaceTrust,
            json!({ "workspace_id": "repo_1", "trusted": false })
        )
        .expect("untrust")["changed"],
        true
    );
    release.send(()).expect("release");
    let reply = held.join().expect("the hire request");
    assert_eq!(reply.0, 409, "{reply:?}");
    assert_eq!(reply.1["reason"], "own_copy", "{reply:?}");
    assert_eq!(hired_agents(&fixture), 0, "nobody was hired");
    assert_eq!(trees(&floor), 0, "the copy is gone");
    assert!(
        fixture.records().iter().any(|record| matches!(
            record,
            Record::WorktreeRemoved {
                how: dudes_proto::WorktreeRemoval::NeverAssigned,
                ..
            }
        )),
        "its lease ended"
    );
}
