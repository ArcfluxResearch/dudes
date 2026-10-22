//! The Floor's ticket diff route against real temporary Git repositories: what a manifest, a page
//! and a content request return, and each limit and refusal that bounds them. The recorded review
//! is supplied by the test, so every case names the exact journal facts it stands on.

#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duded::diff_capture::DiffCapture;
use duded::floor_diff::{DiffReads, DiffRequest, Recorded, Reply, SESSION_ALLOWANCE};
use duded::git_read::Tree;
use duded::git_read::diff::DiffReader;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::diff_stamp::{StampOutcome, StampPayload, TicketBinding};
use dudes_proto::{
    DiffAvailability, DiffBinding, DiffContent, DiffFreshness, DiffKind, DiffOmissionReason,
    DiffPage, DiffRefusal, DiffResponse,
};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{AgentId, CardId, GoalId, Lsn, Seq, Timestamp};
use serde_json::{Value, json};

const CARD: CardId = CardId::from_parts(4, 1);
const GOAL: GoalId = GoalId::from_parts(4, 2);
const AGENT: AgentId = AgentId::from_parts(4, 3);
const BASELINE_LSN: Lsn = Lsn(10);
const CHECK_LSN: Lsn = Lsn(20);
const START: i64 = 1_000_000;

struct Real {
    host: OneShot,
    exhaust: AtomicBool,
}
impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.host.run(spec, timeout).map(|result| match result {
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
        cap: usize,
    ) -> io::Result<PredicateCaptured> {
        if self.exhaust.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(400));
            return Err(io::Error::other("the program was lost"));
        }
        self.host
            .run_captured(spec, timeout, cap)
            .map(|result| match result {
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
        self.host.sweep();
    }
}

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn binding() -> TicketBinding {
    TicketBinding {
        card: CARD,
        round: 1,
        goal: GOAL,
        claimant: AGENT,
    }
}

struct Lab {
    root: tempfile::TempDir,
    runner: Arc<Real>,
    capture: Arc<DiffCapture>,
    reads: DiffReads,
    git: PathBuf,
    recorded: Mutex<Result<Recorded, DiffRefusal>>,
    now: Arc<AtomicI64>,
}

impl Lab {
    fn new() -> Self {
        Self::with(None, Duration::ZERO)
    }

    fn with(deadline: Option<Duration>, interval: Duration) -> Self {
        let root = tempfile::tempdir().unwrap();
        let runner = Arc::new(Real {
            host: OneShot::new(),
            exhaust: AtomicBool::new(false),
        });
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let reader = DiffReader::new(
            runner.clone(),
            std::env::var_os("PATH"),
            &root.path().join("scratch"),
        );
        let now = Arc::new(AtomicI64::new(START));
        let capture_clock = Arc::clone(&now);
        let capture = Arc::new(DiffCapture::new(
            Arc::new(reader),
            Box::new(move || Timestamp::from_unix_millis(capture_clock.load(Ordering::Relaxed))),
        ));
        let route_clock = Arc::clone(&now);
        let mut route_reader = DiffReader::new(
            runner.clone(),
            std::env::var_os("PATH"),
            &root.path().join("route-scratch"),
        );
        if let Some(deadline) = deadline {
            route_reader = route_reader.with_deadline(deadline);
        }
        let reads = DiffReads::new(Arc::new(route_reader), capture.slot(), move || {
            Timestamp::from_unix_millis(route_clock.load(Ordering::Relaxed))
        })
        .with_limits(interval, Duration::from_secs(30));
        let lab = Self {
            root,
            runner,
            capture,
            reads,
            git,
            recorded: Mutex::new(Err(DiffRefusal::BaselineUnavailable)),
            now,
        };
        std::fs::create_dir(lab.repo()).unwrap();
        std::fs::create_dir(lab.root.path().join("home")).unwrap();
        lab.git(&["init", "--quiet", "-b", "main"]);
        lab.write("a.txt", "old a\n");
        lab.write("b.txt", "old b\n");
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "--quiet", "-m", "initial"]);
        lab
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }
    fn tree(&self) -> Tree {
        Tree::Checkout(self.repo())
    }
    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.repo().join(path), text).unwrap();
    }
    fn write_bytes(&self, path: &str, bytes: &[u8]) {
        std::fs::write(self.repo().join(path), bytes).unwrap();
    }
    fn git(&self, args: &[&str]) -> String {
        let mut command = vec![self.git.as_os_str().to_owned()];
        command.extend(args.iter().map(OsString::from));
        let spec = LaunchSpec {
            argv: command,
            env: vec![
                EnvVar::new("PATH", std::env::var_os("PATH").unwrap_or_default()),
                EnvVar::new("HOME", self.root.path().join("home")),
                EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
                EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
                EnvVar::new("GIT_CONFIG_COUNT", "1"),
                EnvVar::new("GIT_CONFIG_KEY_0", "maintenance.auto"),
                EnvVar::new("GIT_CONFIG_VALUE_0", "false"),
                EnvVar::new("GIT_AUTHOR_NAME", "Fixture"),
                EnvVar::new("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
                EnvVar::new("GIT_COMMITTER_NAME", "Fixture"),
                EnvVar::new("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
            ],
            cwd: self.repo(),
            stdin: LaunchInput::Null,
        };
        match self.runner.run(&spec, Duration::from_secs(120)).unwrap() {
            PredicateOutcome::Finished {
                exit: Some(0),
                output_tail,
                ..
            } => output_tail,
            other => panic!("git {args:?}: {other:?}"),
        }
    }

    /// Record the round start now, with no stamp.
    fn start_round(&self) {
        let baseline = self
            .capture
            .baseline(&self.tree(), binding(), cancel())
            .unwrap();
        *self.recorded.lock().unwrap() = Ok(Recorded {
            card: CARD,
            seq: Seq(3),
            round: 1,
            goal: GOAL,
            claimant: AGENT,
            tree: self.tree(),
            baseline_lsn: BASELINE_LSN,
            baseline,
            stamp: None,
        });
    }

    /// Record the post-check sample of the checkout as it is now.
    fn check_passes(&self) -> StampPayload {
        let stamp = {
            let recorded = self.recorded();
            self.capture
                .stamp(
                    &self.tree(),
                    &recorded.baseline,
                    BASELINE_LSN,
                    binding(),
                    CHECK_LSN,
                    cancel(),
                )
                .unwrap()
        };
        self.edit(|recorded| recorded.stamp = Some(stamp));
        stamp
    }

    fn recorded(&self) -> Recorded {
        self.recorded.lock().unwrap().clone().expect("a round")
    }

    fn edit(&self, change: impl FnOnce(&mut Recorded)) {
        change(self.recorded.lock().unwrap().as_mut().expect("a round"));
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the type `serve` takes for its lookup"
    )]
    fn lookup(&self, card: CardId) -> Option<Result<Recorded, DiffRefusal>> {
        assert_eq!(card, CARD, "the route asks about the ticket it was given");
        Some(self.recorded.lock().unwrap().clone())
    }

    fn ask(&self, session: u8, request: &Value) -> Reply {
        let request = DiffRequest::parse(request.to_string().as_bytes()).unwrap();
        self.reads.serve(
            [session; 32],
            &request,
            |card| self.lookup(card),
            || false,
            || true,
        )
    }

    fn respond(&self, session: u8, request: &Value) -> DiffResponse {
        let Reply::Body(bytes) = self.ask(session, request) else {
            panic!("no reply body")
        };
        serde_json::from_slice(&bytes).unwrap()
    }

    fn manifest(&self, session: u8) -> DiffPage {
        match self.respond(session, &manifest_request()) {
            DiffResponse::Manifest(page) => page,
            other => panic!("not a manifest: {other:?}"),
        }
    }

    fn refusal(&self, session: u8, request: &Value) -> DiffRefusal {
        match self.respond(session, request) {
            DiffResponse::Refused { reason } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn content(&self, session: u8, manifest: &DiffPage, ids: &[&str]) -> DiffContent {
        let request =
            json!({"mode": "content", "manifest_id": manifest.manifest_id, "entries": ids});
        match self.respond(session, &request) {
            DiffResponse::Content(content) => content,
            other => panic!("not content: {other:?}"),
        }
    }
}

fn manifest_request() -> Value {
    json!({"mode": "manifest", "card": CARD, "expected_seq": 3})
}

/// `count` lines of 100 bytes each, so a file stays under the line and side limits.
fn lines(letter: &str, count: usize) -> String {
    format!("{}\n", letter.repeat(99)).repeat(count)
}

fn ids(page: &DiffPage) -> Vec<String> {
    page.entries
        .iter()
        .filter_map(|entry| entry.entry_id.clone())
        .collect()
}

fn id_refs(owned: &[String]) -> Vec<&str> {
    owned.iter().map(String::as_str).collect()
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[test]
fn a_manifest_names_the_changed_files_and_content_returns_them_whole() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    lab.write("c.txt", "added\n");
    lab.git(&["add", "c.txt"]);
    lab.write("untracked-name", "not disclosed\n");

    let page = lab.manifest(1);
    let expected = DiffBinding {
        card: CARD,
        seq: Seq(3),
        round: 1,
        goal: GOAL,
        claimant: AGENT,
        check_lsn: None,
        baseline_id: page.binding.baseline_id.clone(),
        target_id: page.binding.target_id.clone(),
        sampler_version: 1,
        policy_version: 1,
        preexisting_tracked: false,
    };
    assert_eq!(page.binding, expected);
    assert!(is_fingerprint(&page.binding.baseline_id));
    assert!(is_fingerprint(&page.binding.target_id));
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind, entry.old_path.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", DiffKind::Modified, None),
            ("c.txt", DiffKind::Added, None)
        ]
    );
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.availability == DiffAvailability::Available)
    );
    assert_eq!(
        (
            page.next_cursor.as_deref(),
            page.omitted,
            page.untracked,
            page.truncated,
            page.freshness,
            page.expires_at.unix_millis()
        ),
        (None, 0, 1, false, DiffFreshness::Unknown, START + 60_000)
    );

    let content = lab.content(1, &page, &id_refs(&ids(&page)));
    assert_eq!(content.binding, expected);
    assert_eq!(content.freshness, DiffFreshness::Unknown);
    assert_eq!(
        content
            .files
            .iter()
            .map(|file| (
                file.path.as_str(),
                file.kind,
                file.old.as_str(),
                file.current.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", DiffKind::Modified, "old a\n", "new a\n"),
            ("c.txt", DiffKind::Added, "", "added\n")
        ]
    );
    assert!(content.omitted.is_empty() && !content.truncated);
}

#[test]
fn replies_name_no_directory_remote_or_author() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let Reply::Body(bytes) = lab.ask(1, &manifest_request()) else {
        panic!("no reply body")
    };
    let text = String::from_utf8(bytes).unwrap();
    for leak in [
        lab.root.path().to_str().unwrap(),
        "fixture@example.invalid",
        "Fixture",
        "/repo",
    ] {
        assert!(!text.contains(leak), "{leak} in {text}");
    }
}

#[test]
fn freshness_matches_the_recorded_stamp_and_names_a_change_after_the_check() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "agent edit\n");
    let stamp = lab.check_passes();
    let page = lab.manifest(1);
    assert_eq!(page.binding.check_lsn, Some(CHECK_LSN));
    assert_eq!(page.freshness, DiffFreshness::MatchesReviewSample);
    assert_eq!(stamp.check_lsn, CHECK_LSN);

    lab.write("a.txt", "edited after the check\n");
    let page = lab.manifest(2);
    assert_eq!(page.freshness, DiffFreshness::ChangedAfterCheck);
    assert_eq!(page.binding.check_lsn, Some(CHECK_LSN));

    lab.write("a.txt", "agent edit\n");
    assert_eq!(
        lab.manifest(3).freshness,
        DiffFreshness::MatchesReviewSample,
        "the same bytes match again"
    );
}

#[test]
fn a_missing_incomplete_or_foreign_stamp_is_unknown_never_fresh() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "agent edit\n");
    let sampled = lab.check_passes();

    lab.edit(|recorded| recorded.stamp = None);
    let page = lab.manifest(1);
    assert_eq!(
        (page.freshness, page.binding.check_lsn),
        (DiffFreshness::Unknown, None),
        "no stamp"
    );

    let mut unavailable = sampled;
    unavailable.outcome = StampOutcome::Unavailable {
        reason: DiffRefusal::TimedOut,
    };
    lab.edit(|recorded| recorded.stamp = Some(unavailable));
    let page = lab.manifest(2);
    assert_eq!(
        (page.freshness, page.binding.check_lsn),
        (DiffFreshness::Unknown, Some(CHECK_LSN)),
        "a stamp that sampled nothing"
    );

    let mut incomplete = sampled;
    if let StampOutcome::Sampled { complete, .. } = &mut incomplete.outcome {
        *complete = false;
    }
    lab.edit(|recorded| recorded.stamp = Some(incomplete));
    assert_eq!(
        lab.manifest(3).freshness,
        DiffFreshness::Unknown,
        "incomplete"
    );

    let mut foreign = sampled;
    foreign.versions.sampler += 1;
    lab.edit(|recorded| recorded.stamp = Some(foreign));
    assert_eq!(
        lab.manifest(4).freshness,
        DiffFreshness::Unknown,
        "another sampler's digest"
    );

    lab.edit(|recorded| recorded.stamp = Some(sampled));
    assert_eq!(
        lab.manifest(1).freshness,
        DiffFreshness::MatchesReviewSample,
        "control: the complete stamp matches"
    );
}

#[test]
fn a_binary_and_an_oversize_file_have_no_preview_and_a_late_change_is_omitted_whole() {
    let lab = Lab::new();
    lab.write_bytes("blob.bin", b"\0\x01\x02 binary");
    lab.write("big.txt", &"x".repeat(16 * 1024));
    lab.write("late.txt", "plain\n");
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "more files"]);
    lab.start_round();
    lab.write_bytes("blob.bin", b"\0\x01\x02 changed binary");
    lab.write("big.txt", &"y".repeat(16 * 1024 + 1));
    lab.write("late.txt", "plain edit\n");

    let page = lab.manifest(1);
    let rows = page
        .entries
        .iter()
        .map(|entry| {
            (
                entry.path.as_str(),
                entry.entry_id.is_some(),
                entry.availability,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            (
                "big.txt",
                false,
                DiffAvailability::Unavailable {
                    reason: DiffOmissionReason::OversizeFile
                }
            ),
            (
                "blob.bin",
                false,
                DiffAvailability::Unavailable {
                    reason: DiffOmissionReason::Binary
                }
            ),
            ("late.txt", true, DiffAvailability::Available),
        ]
    );
    lab.write_bytes("late.txt", b"\0 now binary");
    let late = ids(&page);
    let content = lab.content(1, &page, &id_refs(&late));
    assert!(content.files.is_empty());
    assert_eq!(content.omitted.len(), 1);
    assert_eq!(content.omitted[0].entry_id, late[0]);
    assert_eq!(content.omitted[0].reason, DiffOmissionReason::Binary);
}

#[test]
fn a_page_continues_the_cached_list_and_never_rediscovers() {
    let lab = Lab::new();
    for n in 0..70 {
        lab.write(&format!("f{n:02}.txt"), "old\n");
    }
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "many"]);
    lab.start_round();
    for n in 0..70 {
        lab.write(&format!("f{n:02}.txt"), "new\n");
    }
    let first = lab.manifest(1);
    assert_eq!(first.entries.len(), 64);
    assert_eq!(first.entries[0].path, "f00.txt");
    let cursor = first.next_cursor.clone().expect("more pages");
    lab.write("zz-after-manifest.txt", "x\n");
    lab.git(&["add", "zz-after-manifest.txt"]);
    let request = json!({"mode": "page", "manifest_id": first.manifest_id, "cursor": cursor});
    let DiffResponse::Page(second) = lab.respond(1, &request) else {
        panic!("not a page")
    };
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        [
            "f64.txt", "f65.txt", "f66.txt", "f67.txt", "f68.txt", "f69.txt"
        ],
        "the next page is the rest of the cached list, without the later file"
    );
    assert_eq!(second.next_cursor, None);
}

#[test]
fn a_cursor_or_entry_the_manifest_did_not_mint_is_refused() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    lab.write("b.txt", "new b\n");
    let page = lab.manifest(1);
    let minted = ids(&page);
    let foreign = "0".repeat(32);

    let page_request = json!({"mode": "page", "manifest_id": page.manifest_id, "cursor": foreign});
    assert_eq!(lab.refusal(1, &page_request), DiffRefusal::StaleManifest);

    let unknown = json!({"mode": "content", "manifest_id": page.manifest_id, "entries": [foreign]});
    assert_eq!(lab.refusal(1, &unknown), DiffRefusal::InvalidRequest);
    let repeated = json!({"mode": "content", "manifest_id": page.manifest_id, "entries": [minted[0], minted[0]]});
    assert_eq!(lab.refusal(1, &repeated), DiffRefusal::InvalidRequest);
    let mixed = json!({"mode": "content", "manifest_id": page.manifest_id, "entries": [minted[0], foreign]});
    assert_eq!(lab.refusal(1, &mixed), DiffRefusal::InvalidRequest);
}

#[test]
fn ids_belong_to_the_session_that_received_them() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let page = lab.manifest(1);
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    assert_eq!(lab.refusal(2, &request), DiffRefusal::StaleManifest);
    assert!(matches!(lab.respond(1, &request), DiffResponse::Content(_)));
}

#[test]
fn a_new_manifest_invalidates_the_sessions_old_ids() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let old = lab.manifest(1);
    let new = lab.manifest(1);
    assert_ne!(old.manifest_id, new.manifest_id);
    let request = json!({"mode": "content", "manifest_id": old.manifest_id, "entries": ids(&old)});
    assert_eq!(lab.refusal(1, &request), DiffRefusal::StaleManifest);
}

#[test]
fn a_manifest_expires_sixty_seconds_after_creation_and_reads_do_not_renew_it() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let page = lab.manifest(1);
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    lab.now.store(START + 59_999, Ordering::Relaxed);
    assert!(matches!(lab.respond(1, &request), DiffResponse::Content(_)));
    lab.now.store(START + 60_000, Ordering::Relaxed);
    assert_eq!(lab.refusal(1, &request), DiffRefusal::Expired);
    assert_eq!(
        lab.refusal(1, &request),
        DiffRefusal::Expired,
        "the session keeps being told, until it asks for a new manifest"
    );
    assert_eq!(lab.manifest(1).entries.len(), 1);
}

#[test]
fn at_most_four_sessions_hold_a_manifest_and_a_session_may_replace_its_own() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    for session in 1..=4 {
        assert_eq!(lab.manifest(session).entries.len(), 1);
    }
    assert_eq!(lab.refusal(5, &manifest_request()), DiffRefusal::Busy);
    assert_eq!(
        lab.manifest(1).entries.len(),
        1,
        "a holder replaces its own"
    );
    lab.now.store(START + 60_000, Ordering::Relaxed);
    assert_eq!(
        lab.manifest(5).entries.len(),
        1,
        "expired manifests free their places"
    );
}

#[test]
fn a_session_may_start_one_read_per_interval_and_other_sessions_are_unaffected() {
    let lab = Lab::with(None, Duration::from_secs(3600));
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let page = lab.manifest(1);
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    assert_eq!(
        lab.refusal(1, &request),
        DiffRefusal::Busy,
        "content inside the interval"
    );
    assert_eq!(lab.refusal(1, &manifest_request()), DiffRefusal::Busy);
    assert_eq!(lab.manifest(2).entries.len(), 1, "another session");
}

#[test]
fn the_daemon_wide_slot_is_shared_with_the_captures() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    {
        let slot = lab.capture.slot();
        let _held = slot.capture();
        assert_eq!(lab.refusal(1, &manifest_request()), DiffRefusal::Busy);
    }
    assert_eq!(lab.manifest(1).entries.len(), 1);
}

#[test]
fn a_page_reads_only_the_cached_manifest_and_takes_no_slot() {
    let lab = Lab::new();
    for n in 0..70 {
        lab.write(&format!("f{n:02}.txt"), "old\n");
    }
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "many"]);
    lab.start_round();
    for n in 0..70 {
        lab.write(&format!("f{n:02}.txt"), "new\n");
    }
    let first = lab.manifest(1);
    let cursor = first.next_cursor.clone().expect("more pages");
    let slot = lab.capture.slot();
    let _capture = slot.capture();
    let page = json!({"mode": "page", "manifest_id": first.manifest_id, "cursor": cursor});
    let DiffResponse::Page(second) = lab.respond(1, &page) else {
        panic!("not a page")
    };
    assert_eq!(
        second.entries.len(),
        6,
        "served while a capture holds the slot"
    );
    let content =
        json!({"mode": "content", "manifest_id": first.manifest_id, "entries": ids(&first)[..1]});
    assert_eq!(
        lab.refusal(1, &content),
        DiffRefusal::Busy,
        "control: a content read still needs the slot"
    );
}

#[test]
fn a_manifest_read_gives_way_to_a_waiting_capture() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let slot = lab.capture.slot();
    let place = slot.announce();
    assert_eq!(lab.refusal(1, &manifest_request()), DiffRefusal::Busy);
    drop(place);
    assert_eq!(lab.manifest(1).entries.len(), 1);
}

#[test]
fn successful_replies_are_charged_whole_until_the_session_allowance_is_spent() {
    let lab = Lab::new();
    for n in 0..4 {
        lab.write(&format!("big{n}.txt"), &lines("o", 150));
    }
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "big files"]);
    lab.start_round();
    for n in 0..4 {
        lab.write(&format!("big{n}.txt"), &lines("n", 159));
    }
    let Reply::Body(manifest) = lab.ask(1, &manifest_request()) else {
        panic!("no reply body")
    };
    let DiffResponse::Manifest(page) = serde_json::from_slice(&manifest).unwrap() else {
        panic!("not a manifest")
    };
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    let mut spent = manifest.len();
    let mut replies = 0;
    for _ in 0..12 {
        let Reply::Body(bytes) = lab.ask(1, &request) else {
            panic!("no reply body")
        };
        match serde_json::from_slice(&bytes).unwrap() {
            DiffResponse::Content(_) => {
                assert!(bytes.len() > 100_000 && bytes.len() <= 128 * 1024);
                spent += bytes.len();
                replies += 1;
            }
            DiffResponse::Refused { reason } => {
                assert_eq!(reason, DiffRefusal::SessionLimit);
                assert_eq!(
                    bytes.len(),
                    br#"{"mode":"refused","reason":"session_limit"}"#.len()
                );
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        (7..=9).contains(&replies),
        "{replies} repeats of a ~120 KiB reply fit before the limit"
    );
    assert!(spent <= SESSION_ALLOWANCE, "{spent} bytes charged");
    assert!(
        SESSION_ALLOWANCE - spent < spent / (replies + 1),
        "another whole reply would not have fit"
    );
    assert_eq!(
        lab.manifest(2).entries.len(),
        4,
        "a new session has its own allowance"
    );
}

#[test]
fn an_exhausted_allowance_refuses_every_mode_with_no_content() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let reads = DiffReads::new(lab.capture.reader(), lab.capture.slot(), || {
        Timestamp::from_unix_millis(START)
    })
    .with_limits(Duration::ZERO, Duration::from_secs(30))
    .with_allowance(1_200);
    let ask = |request: &Value| {
        let request = DiffRequest::parse(request.to_string().as_bytes()).unwrap();
        let Reply::Body(bytes) = reads.serve(
            [1; 32],
            &request,
            |card| lab.lookup(card),
            || false,
            || true,
        ) else {
            panic!("no reply body")
        };
        (
            bytes.len(),
            serde_json::from_slice::<DiffResponse>(&bytes).unwrap(),
        )
    };
    let (first, DiffResponse::Manifest(page)) = ask(&manifest_request()) else {
        panic!("not a manifest")
    };
    assert!(first > 600 && first <= 1_200, "{first}");
    let page_request =
        json!({"mode": "page", "manifest_id": page.manifest_id, "cursor": "0".repeat(32)});
    let content =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    let (_, refused) = ask(&manifest_request());
    assert_eq!(
        refused,
        DiffResponse::Refused {
            reason: DiffRefusal::SessionLimit
        },
        "the second manifest would exceed the allowance"
    );
    for request in [manifest_request(), page_request, content] {
        let (length, response) = ask(&request);
        assert_eq!(
            (length, response),
            (
                br#"{"mode":"refused","reason":"session_limit"}"#.len(),
                DiffResponse::Refused {
                    reason: DiffRefusal::SessionLimit
                }
            )
        );
    }
}

#[test]
fn a_revoked_session_gets_no_reply_and_a_stopping_daemon_owes_none() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let request = DiffRequest::parse(manifest_request().to_string().as_bytes()).unwrap();
    assert_eq!(
        lab.reads.serve(
            [1; 32],
            &request,
            |card| lab.lookup(card),
            || false,
            || false
        ),
        Reply::Revoked
    );
    assert_eq!(
        lab.reads
            .serve([2; 32], &request, |card| lab.lookup(card), || true, || true),
        Reply::Stopping
    );
    assert!(matches!(lab.ask(3, &manifest_request()), Reply::Body(_)));
}

#[test]
fn a_rework_a_reassignment_and_a_retirement_each_make_the_manifest_stale() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");

    let page = lab.manifest(1);
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    lab.edit(|recorded| {
        recorded.round = 2;
        recorded.goal = GoalId::from_parts(4, 9);
    });
    assert_eq!(lab.refusal(1, &request), DiffRefusal::StaleTicket, "rework");
    assert_eq!(
        lab.refusal(1, &request),
        DiffRefusal::StaleManifest,
        "the old ids are gone"
    );

    lab.start_round();
    let page = lab.manifest(1);
    let request =
        json!({"mode": "page", "manifest_id": page.manifest_id, "cursor": "0".repeat(32)});
    lab.edit(|recorded| recorded.claimant = AgentId::from_parts(4, 9));
    assert_eq!(
        lab.refusal(1, &request),
        DiffRefusal::StaleTicket,
        "reassignment"
    );

    lab.start_round();
    let page = lab.manifest(1);
    let request =
        json!({"mode": "content", "manifest_id": page.manifest_id, "entries": ids(&page)});
    *lab.recorded.lock().unwrap() = Err(DiffRefusal::StaleTicket);
    assert_eq!(
        lab.refusal(1, &request),
        DiffRefusal::StaleTicket,
        "retirement"
    );
}

#[test]
fn a_card_that_moved_since_the_browser_looked_is_stale() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let behind = json!({"mode": "manifest", "card": CARD, "expected_seq": 2});
    assert_eq!(lab.refusal(1, &behind), DiffRefusal::StaleTicket);
    let ahead = json!({"mode": "manifest", "card": CARD, "expected_seq": 4});
    assert_eq!(lab.refusal(1, &ahead), DiffRefusal::StaleTicket);
    assert_eq!(lab.manifest(1).entries.len(), 1);
}

#[test]
fn a_binding_that_changes_while_the_checkout_is_read_returns_nothing() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let request = DiffRequest::parse(manifest_request().to_string().as_bytes()).unwrap();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let reply = lab.reads.serve(
        [1; 32],
        &request,
        |card| {
            let mut recorded = lab.lookup(card)?;
            if calls.fetch_add(1, Ordering::Relaxed) == 1 {
                recorded.as_mut().unwrap().seq = Seq(4);
            }
            Some(recorded)
        },
        || false,
        || true,
    );
    let Reply::Body(bytes) = reply else {
        panic!("no reply body")
    };
    assert_eq!(
        serde_json::from_slice::<DiffResponse>(&bytes).unwrap(),
        DiffResponse::Refused {
            reason: DiffRefusal::StaleTicket
        }
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let page_request =
        json!({"mode": "page", "manifest_id": "0".repeat(32), "cursor": "0".repeat(32)});
    assert_eq!(
        lab.refusal(1, &page_request),
        DiffRefusal::StaleManifest,
        "no manifest was kept"
    );
}

#[test]
fn a_ticket_without_a_recorded_baseline_has_no_diff() {
    let lab = Lab::new();
    lab.start_round();
    *lab.recorded.lock().unwrap() = Err(DiffRefusal::BaselineUnavailable);
    assert_eq!(
        lab.refusal(1, &manifest_request()),
        DiffRefusal::BaselineUnavailable
    );
    lab.start_round();
    lab.edit(|recorded| {
        recorded.baseline = lab
            .capture
            .baseline_unavailable(binding(), DiffRefusal::Unreadable);
    });
    assert_eq!(
        lab.refusal(1, &manifest_request()),
        DiffRefusal::BaselineUnavailable
    );
}

#[test]
fn unsupported_object_storage_and_a_maintenance_lock_are_finite_refusals() {
    let lab = Lab::new();
    lab.start_round();
    lab.write("a.txt", "new a\n");
    let info = lab.repo().join(".git/objects/info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("alternates"), "/elsewhere/objects\n").unwrap();
    assert_eq!(
        lab.refusal(1, &manifest_request()),
        DiffRefusal::UnsupportedStorage
    );
    std::fs::remove_file(info.join("alternates")).unwrap();

    let lock = lab.repo().join(".git/objects/maintenance.lock");
    std::fs::write(&lock, "").unwrap();
    assert_eq!(lab.refusal(1, &manifest_request()), DiffRefusal::Busy);
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(lab.manifest(1).entries.len(), 1);
}

#[test]
fn a_read_that_spends_its_budget_answers_timed_out() {
    let lab = Lab::with(Some(Duration::from_millis(150)), Duration::ZERO);
    lab.start_round();
    lab.write("a.txt", "new a\n");
    lab.runner.exhaust.store(true, Ordering::Relaxed);
    assert_eq!(lab.refusal(1, &manifest_request()), DiffRefusal::TimedOut);
}

#[test]
fn a_worker_the_deadline_abandons_is_counted_and_keeps_the_slot_until_it_ends() {
    let lab = Lab::with(None, Duration::ZERO);
    let reads = DiffReads::new(lab.capture.reader(), lab.capture.slot(), || {
        Timestamp::from_unix_millis(START)
    })
    .with_limits(Duration::ZERO, Duration::from_millis(30));
    lab.start_round();
    lab.write("a.txt", "new a\n");
    lab.runner.exhaust.store(true, Ordering::Relaxed);
    let request = DiffRequest::parse(manifest_request().to_string().as_bytes()).unwrap();
    let ask = |session: u8| {
        let Reply::Body(bytes) = reads.serve(
            [session; 32],
            &request,
            |card| lab.lookup(card),
            || false,
            || true,
        ) else {
            panic!("no reply body")
        };
        serde_json::from_slice::<DiffResponse>(&bytes).unwrap()
    };
    let timed_out = DiffResponse::Refused {
        reason: DiffRefusal::TimedOut,
    };
    assert_eq!(ask(1), timed_out);
    assert_eq!(reads.abandoned(), 1);
    assert_eq!(
        ask(2),
        DiffResponse::Refused {
            reason: DiffRefusal::Busy
        },
        "the first worker still holds the slot"
    );
    assert_eq!(reads.abandoned(), 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while reads.abandoned() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the abandoned worker never ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    lab.runner.exhaust.store(false, Ordering::Relaxed);
    assert_eq!(lab.manifest(3).entries.len(), 1, "the slot is free again");
}

#[test]
fn requests_are_closed_flat_and_bounded() {
    let id = "a".repeat(32);
    let ok = [
        json!({"mode": "manifest", "card": CARD, "expected_seq": 3}),
        json!({"mode": "page", "manifest_id": id, "cursor": id}),
        json!({"mode": "content", "manifest_id": id, "entries": [id]}),
    ];
    for request in &ok {
        assert!(
            DiffRequest::parse(request.to_string().as_bytes()).is_ok(),
            "{request}"
        );
    }
    let nine = vec![id.clone(); 9];
    let bad = [
        "[]".to_owned(),
        "{}".to_owned(),
        json!({"mode": "manifest", "card": CARD}).to_string(),
        json!({"mode": "manifest", "card": CARD, "expected_seq": 3, "path": "/etc/passwd"})
            .to_string(),
        json!({"mode": "manifest", "card": CARD, "expected_seq": 3, "revision": "HEAD~3"})
            .to_string(),
        json!({"mode": "content", "manifest_id": id, "entries": [id], "pattern": "*"}).to_string(),
        json!({"mode": "page", "manifest_id": "short", "cursor": id}).to_string(),
        json!({"mode": "page", "manifest_id": id.to_uppercase(), "cursor": id}).to_string(),
        json!({"mode": "content", "manifest_id": id, "entries": []}).to_string(),
        json!({"mode": "content", "manifest_id": id, "entries": nine}).to_string(),
        json!({"mode": "everything"}).to_string(),
        format!(r#"{{"mode":"manifest","card":"{CARD}","expected_seq":3,"expected_seq":4}}"#),
        format!("{}{}", ok[0], " ".repeat(1024)),
    ];
    for request in &bad {
        assert!(DiffRequest::parse(request.as_bytes()).is_err(), "{request}");
    }
}

#[test]
fn the_debug_form_of_a_request_names_no_id() {
    let id = "a".repeat(32);
    let request = DiffRequest::parse(
        json!({"mode": "page", "manifest_id": id, "cursor": id})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(format!("{request:?}"), "DiffRequest(<redacted>)");
}
