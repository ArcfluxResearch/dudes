use super::*;
use duded::diff_capture::DiffCapture;
use duded::git_read::Tree;
use duded::git_read::diff::{BaselineCapture, DiffReader};
use dudes_proto::DiffRefusal;

const COMMIT: [&str; 10] = [
    "-c",
    "user.name=Test",
    "-c",
    "user.email=test@example.com",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
];

/// Git's own background maintenance holds a lock the reader reports as busy until it finishes.
fn once_idle<T>(mut read: impl FnMut() -> Result<T, DiffRefusal>) -> Result<T, DiffRefusal> {
    for _ in 0..100 {
        match read() {
            Err(DiffRefusal::Busy) => std::thread::sleep(Duration::from_millis(50)),
            settled => return settled,
        }
    }
    read()
}

struct Round {
    service: DiffCapture,
    runner: RealRunner,
    queries: Vec<PathBuf>,
    probes: Vec<(Tree, BaselineCapture)>,
}

impl Round {
    fn start(fixture: &Fixture) -> Self {
        let story = Story::seed(fixture);
        let hire = &story.hire.repositories;
        let service = DiffCapture::new(
            Arc::new(DiffReader::new(
                Arc::new(RealRunner(OneShot::new())),
                git_path(),
                fixture.runtime.path(),
            )),
            Box::new(|| Timestamp::from_unix_millis(1_000)),
        );
        let copies = ["web", "api"].map(|name| {
            hire.repositories
                .get(&RepoId::parse(name).unwrap())
                .unwrap()
                .clone()
        });
        let probes = copies
            .iter()
            .map(|copy| {
                let tree = Tree::recorded(copy, hire).unwrap();
                let baseline =
                    once_idle(|| service.reader().capture_baseline(&tree, Arc::default())).unwrap();
                (tree, baseline)
            })
            .collect();
        Self {
            service,
            runner: RealRunner(OneShot::new()),
            queries: copies
                .iter()
                .map(|copy| copy.execution.directory.clone())
                .collect(),
            probes,
        }
    }

    fn commit(&self, copy: usize, extra: &[&str]) {
        let mut args = COMMIT.to_vec();
        args.extend(["commit", "--quiet", "-m", "agent work"]);
        args.extend(extra);
        git(&self.queries[copy], &args);
    }

    fn unconfined(&self) -> Option<bool> {
        let committed = |index: usize| {
            let (tree, baseline) = self.probes.get(index)?;
            once_idle(|| self.service.committed_since(tree, baseline, Arc::default())).ok()
        };
        self.run(Some(&committed))
    }

    fn confined(&self) -> Option<bool> {
        self.run(None)
    }

    fn run(&self, committed: Option<duded::predicate::CommittedSince<'_>>) -> Option<bool> {
        duded::predicate::files_changed_or_committed(
            &self.runner,
            &dudes_config::check::changed_check(),
            &self.queries,
            &[],
            git_path().as_deref(),
            &self.queries,
            Duration::from_secs(3),
            committed,
        )
        .unwrap()
        .map(|(pass, _)| pass)
    }
}

#[test]
fn a_round_whose_only_change_is_a_commit_passes_for_an_unconfined_hire_only() {
    let fixture = Fixture::new();
    let round = Round::start(&fixture);
    assert_eq!(round.unconfined(), Some(false), "nothing changed yet");
    std::fs::write(round.queries[1].join("tracked.txt"), "committed\n").unwrap();
    git(&round.queries[1], &["add", "tracked.txt"]);
    round.commit(1, &[]);
    assert_eq!(round.unconfined(), Some(true));
    assert_eq!(
        round.confined(),
        Some(false),
        "a confined round keeps counting uncommitted work only"
    );
}

#[test]
fn a_head_that_moved_without_a_changed_path_does_not_count() {
    let fixture = Fixture::new();
    let round = Round::start(&fixture);
    round.commit(0, &["--allow-empty"]);
    assert_eq!(round.unconfined(), Some(false));
}

#[test]
fn uncommitted_changes_still_pass_and_a_committed_one_elsewhere_does_not_hide_them() {
    let fixture = Fixture::new();
    let round = Round::start(&fixture);
    std::fs::write(round.queries[0].join("tracked.txt"), "edited\n").unwrap();
    assert_eq!(round.unconfined(), Some(true));
    assert_eq!(round.confined(), Some(true));
}

#[test]
fn a_copy_whose_commit_query_fails_reads_unknown() {
    let fixture = Fixture::new();
    let mut round = Round::start(&fixture);
    round.probes[1].1.head = GitCommit::parse(&"ab".repeat(20)).unwrap();
    assert_eq!(round.unconfined(), None, "never read as nothing changed");
}
