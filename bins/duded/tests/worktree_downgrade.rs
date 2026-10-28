//! Opt-in checks against an actual previous-release daemon, using only scratch state.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use dudes_journal::{JournalOptions, open};
use dudes_proto::record::samples;
use dudes_proto::{Entry, RecordKind};
use dudes_types::Timestamp;

struct Scratch {
    root: tempfile::TempDir,
    binary: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let binary = std::env::var_os("DUDES_PREVIOUS_DAEMON")
            .map(PathBuf::from)
            .expect("set DUDES_PREVIOUS_DAEMON to the previous-release daemon");
        assert!(binary.is_absolute());
        let root = tempfile::Builder::new()
            .prefix("dudes-wt-reader-")
            .tempdir_in("/tmp")
            .unwrap();
        for name in ["home", "state", "run"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.path().join(name))
                .unwrap();
        }
        fs::write(root.path().join("operator.toml"), "").unwrap();
        Self { root, binary }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn append(&self, kind: RecordKind) -> dudes_types::Lsn {
        let (mut writer, _) = open(&self.path("state/journal"), JournalOptions::default()).unwrap();
        writer
            .append(&Entry {
                at: Timestamp::UNIX_EPOCH,
                record: samples::record(kind),
            })
            .unwrap()
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the test harness starts a previous-release daemon; production child creation is guarded separately"
    )]
    fn start(&self) -> Child {
        std::process::Command::new(&self.binary)
            .env_clear()
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.root.path())
            .env("PATH", "/usr/bin:/bin")
            .env("DUDES_STATE_DIR", self.path("state"))
            .env("DUDES_RUNTIME_DIR", self.path("run"))
            .env("DUDES_CONFIG", self.path("operator.toml"))
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(fs::File::create(self.path("stdout")).unwrap())
            .stderr(fs::File::create(self.path("stderr")).unwrap())
            .spawn()
            .unwrap()
    }

    fn logs(&self) -> String {
        ["stdout", "stderr", "run/duded.log"]
            .into_iter()
            .map(|name| fs::read_to_string(self.path(name)).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn boot_control(&self) {
        let mut child = self.start();
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            assert!(child.try_wait().unwrap().is_none(), "{}", self.logs());
            if self.path("run/duded.sock").exists() && self.path("state/state.redb").exists() {
                child.kill().unwrap();
                child.wait().unwrap();
                fs::remove_dir_all(self.path("run")).unwrap();
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(self.path("run"))
                    .unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("previous release did not start: {}", self.logs());
    }
}

fn journal_bytes(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn refuses(warm: bool) {
    let scratch = Scratch::new();
    scratch.append(RecordKind::AgentRenamed);
    if warm {
        scratch.boot_control();
    }
    let lease_lsn = scratch.append(RecordKind::WorktreeLeasedWithIdentity);
    let before = journal_bytes(&scratch.path("state/journal"));
    let floor_before = fs::metadata(scratch.path("state/floor-port"))
        .and_then(|meta| meta.modified())
        .ok();
    let mut child = scratch.start();
    let until = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "previous release served a newer journal: {}",
                scratch.logs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    let logs = scratch.logs();
    assert!(
        logs.contains("112")
            && logs.contains("required")
            && logs.contains("journal")
            && logs.contains(&format!("at {lease_lsn}")),
        "{logs}"
    );
    assert!(!logs.contains("panicked"), "{logs}");
    assert!(!scratch.path("run/duded.sock").exists());
    assert_eq!(
        fs::metadata(scratch.path("state/floor-port"))
            .and_then(|meta| meta.modified())
            .ok(),
        floor_before
    );
    assert_eq!(journal_bytes(&scratch.path("state/journal")), before);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_a_required_pending_lease_cold_before_serving_or_appending() {
    refuses(false);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_a_required_pending_lease_warm_before_serving_or_appending() {
    refuses(true);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_reads_an_unchanged_journal_cold() {
    let scratch = Scratch::new();
    scratch.append(RecordKind::AgentRenamed);
    scratch.boot_control();
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_reads_an_unchanged_journal_warm() {
    let scratch = Scratch::new();
    scratch.append(RecordKind::AgentRenamed);
    scratch.boot_control();
    scratch.boot_control();
}
