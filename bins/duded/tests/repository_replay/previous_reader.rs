use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::process::{Child, Stdio};
use std::time::Instant;

use super::*;

#[derive(Clone, Copy)]
enum Change {
    BrowserField,
    UnconfinedKind,
    MailTurnKind,
}

struct PreviousReader {
    fixture: Fixture,
    story: Story,
    binary: PathBuf,
}

impl PreviousReader {
    fn new() -> Self {
        let binary = std::env::var_os("DUDES_PREVIOUS_DAEMON")
            .map(PathBuf::from)
            .expect("set DUDES_PREVIOUS_DAEMON to the actual previous-release daemon");
        assert!(binary.is_absolute());
        let fixture = Fixture::new();
        let story = Story::seed(&fixture);
        for name in ["old-home", "old-run"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(fixture.runtime.path().join(name))
                .unwrap();
        }
        fs::write(fixture.runtime.path().join("old-operator.toml"), "").unwrap();
        Self {
            fixture,
            story,
            binary,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.fixture.runtime.path().join(name)
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the test starts an explicitly supplied previous release against scratch state"
    )]
    fn start(&self) -> Child {
        std::process::Command::new(&self.binary)
            .env_clear()
            .env("HOME", self.path("old-home"))
            .env("TMPDIR", self.fixture.runtime.path())
            .env("PATH", "/usr/bin:/bin")
            .env("DUDES_STATE_DIR", self.fixture.state.path())
            .env("DUDES_RUNTIME_DIR", self.path("old-run"))
            .env("DUDES_CONFIG", self.path("old-operator.toml"))
            .current_dir(self.fixture.runtime.path())
            .stdin(Stdio::null())
            .stdout(fs::File::create(self.path("old-stdout")).unwrap())
            .stderr(fs::File::create(self.path("old-stderr")).unwrap())
            .spawn()
            .unwrap()
    }

    fn logs(&self) -> String {
        ["old-stdout", "old-stderr", "old-run/duded.log"]
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
            if self.path("old-run/duded.sock").exists()
                && self.fixture.state.path().join("state.redb").exists()
            {
                child.kill().unwrap();
                child.wait().unwrap();
                fs::remove_dir_all(self.path("old-run")).unwrap();
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(self.path("old-run"))
                    .unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        panic!(
            "previous release did not read the unchanged repository history: {}",
            self.logs()
        );
    }

    fn rewrite_last_hire(&self, change: Change) {
        use dudes_journal::frame::{HEADER_LEN, Header, encode_required};
        let path = dudes_journal::segment::segment_path(
            &self.fixture.journal_dir(),
            self.story.hired_lsn.segment(),
        );
        let bytes = fs::read(&path).unwrap();
        let offset = usize::try_from(self.story.hired_lsn.offset()).unwrap();
        let header = Header::parse(bytes[offset..offset + HEADER_LEN].try_into().unwrap());
        assert_eq!(header.kind, RecordKind::AgentHiredWithRepositories.tag());
        assert_eq!(offset + header.frame_len(), bytes.len());
        let mut hire = self.story.hire.clone();
        let (record, newest) = match change {
            Change::BrowserField => {
                hire.browser_onboarding =
                    Some(Box::new(dudes_proto::record::BrowserOnboardingHire {
                        attempt_id: dudes_proto::record::Hex32::new([1; 32]),
                        request_digest: dudes_proto::record::Hex32::new([2; 32]),
                        profile_id: "service".into(),
                        session_digest: dudes_proto::record::Hex32::new([3; 32]),
                        intended_goal: None,
                        trust_lsn: None,
                    }));
                (Record::AgentHiredWithRepositories(Box::new(hire)), 103)
            }
            Change::UnconfinedKind => {
                hire.repositories.policy =
                    dudes_proto::repository::RepositoryPolicy::OperatorTrustV1;
                (
                    Record::AgentHiredWithUnconfinedRepositories(Box::new(hire)),
                    113,
                )
            }
            Change::MailTurnKind => (
                dudes_proto::record::samples::record(RecordKind::MailTurnDecided),
                115,
            ),
        };
        let kind = record.kind();
        let payload = serde_json::to_vec(&Entry {
            at: Timestamp::from_unix_millis(1_000),
            record,
        })
        .unwrap();
        let mut changed = bytes[..offset].to_vec();
        encode_required(kind, &payload, &mut changed).unwrap();
        fs::write(path, changed).unwrap();
        let (reader, _) = Reader::open(&self.fixture.journal_dir()).unwrap();
        for item in reader.iter_from(Lsn::ZERO).unwrap() {
            let (_, entry) = item.unwrap();
            assert!(entry.record.kind().tag() <= newest);
        }
    }

    fn state_bytes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut paths = fs::read_dir(self.fixture.journal_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .collect::<Vec<_>>();
        for name in ["state.redb", "floor-port"] {
            let path = self.fixture.state.path().join(name);
            if path.exists() {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }

    fn refuses(&self, change: Change) {
        self.rewrite_last_hire(change);
        let before = self.state_bytes();
        let mut child = self.start();
        let until = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "previous release served the changed hire record: {}",
                    self.logs()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let logs = self.logs();
        assert!(!status.success() && !logs.contains("panicked"), "{logs}");
        let lsn = self.story.hired_lsn;
        let (located, message) = match change {
            Change::BrowserField => (
                format!("journal record at {lsn}"),
                "unknown field `browser_onboarding`".to_owned(),
            ),
            Change::UnconfinedKind => (
                "journal required record kind 113".to_owned(),
                format!("record kind 113 at {lsn} is not understood"),
            ),
            Change::MailTurnKind => (
                "journal required record kind 114".to_owned(),
                format!("record kind 114 at {lsn} is not understood"),
            ),
        };
        assert!(logs.contains(&located) && logs.contains(&message), "{logs}");
        assert!(!self.path("old-run/duded.sock").exists());
        assert_eq!(self.state_bytes(), before);
    }
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_only_the_browser_group_field_cold() {
    PreviousReader::new().refuses(Change::BrowserField);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_only_the_browser_group_field_with_a_warm_projection() {
    let previous = PreviousReader::new();
    previous.boot_control();
    previous.refuses(Change::BrowserField);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_the_unconfined_hire_kind_cold() {
    PreviousReader::new().refuses(Change::UnconfinedKind);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_the_unconfined_hire_kind_with_a_warm_projection() {
    let previous = PreviousReader::new();
    previous.boot_control();
    previous.refuses(Change::UnconfinedKind);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_the_mail_turn_kind_cold() {
    PreviousReader::new().refuses(Change::MailTurnKind);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_refuses_the_mail_turn_kind_with_a_warm_projection() {
    let previous = PreviousReader::new();
    previous.boot_control();
    previous.refuses(Change::MailTurnKind);
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_reads_the_absent_browser_group_field_cold() {
    PreviousReader::new().boot_control();
}

#[test]
#[ignore = "requires DUDES_PREVIOUS_DAEMON pointing to the actual previous release"]
fn previous_release_reads_the_absent_browser_group_field_warm() {
    let previous = PreviousReader::new();
    previous.boot_control();
    previous.boot_control();
}
