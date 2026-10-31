use super::*;
use duded::diff_capture::DiffCapture;
use duded::git_read::Tree;
use duded::git_read::diff::{BaselineCapture, DiffReader};

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

struct Round {
    service: DiffCapture,
    runner: RealRunner,
    queries: Vec<PathBuf>,
    probes: Vec<(Tree, BaselineCapture)>,
    began: std::time::SystemTime,
}

impl Round {
    fn start(fixture: &Fixture) -> Self {
        Self::start_after(fixture, |_| {})
    }

    /// A round whose baseline is taken after `before` has touched the copies, the way an earlier
    /// ticket leaves them.
    fn start_after(fixture: &Fixture, before: impl FnOnce(&[PathBuf])) -> Self {
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
        before(
            &copies
                .iter()
                .map(|copy| copy.execution.directory.clone())
                .collect::<Vec<_>>(),
        );
        let probes = copies
            .iter()
            .map(|copy| {
                let tree = Tree::recorded(copy, hire).unwrap();
                let baseline = service
                    .reader()
                    .capture_baseline(&tree, Arc::default())
                    .unwrap();
                (tree, baseline)
            })
            .collect();
        Self {
            began: std::time::SystemTime::now(),
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
            self.service
                .committed_since(tree, baseline, Arc::default())
                .ok()
        };
        self.run(Some(&committed))
    }

    fn confined(&self) -> Option<bool> {
        self.run(None)
    }

    fn run(&self, committed: Option<duded::predicate::CommittedSince<'_>>) -> Option<bool> {
        duded::predicate::files_changed_in_round(
            &self.runner,
            &dudes_config::check::changed_check(),
            &self.queries,
            &[],
            git_path().as_deref(),
            &self.queries,
            Duration::from_secs(3),
            Some(&duded::predicate::RoundStart {
                began: vec![Some(self.began); self.queries.len()],
                committed,
            }),
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

#[test]
fn a_file_left_uncommitted_before_the_round_does_not_pass_but_one_changed_again_does() {
    let fixture = Fixture::new();
    let round = Round::start_after(&fixture, |copies| {
        std::fs::write(copies[0].join("tracked.txt"), "left by an earlier ticket\n").unwrap();
        std::fs::write(copies[1].join("left.txt"), "an earlier ticket's new file\n").unwrap();
    });
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        round.unconfined(),
        Some(false),
        "nothing changed since the round began"
    );
    assert_eq!(round.confined(), Some(false));
    std::fs::write(round.queries[0].join("tracked.txt"), "edited again\n").unwrap();
    assert_eq!(round.unconfined(), Some(true));
    assert_eq!(round.confined(), Some(true));
}
