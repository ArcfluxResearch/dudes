//! Recovery command integration tests.

#![cfg(unix)]

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::num::NonZeroUsize;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use dudes::args::{Command, RecoveryCommand};
use dudes::env::Environment;
use dudes::{exit, render};
use dudes_proto::record::Hex32;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::recovery::{
    RecoveryAllowedChoices, RecoveryAttemptView, RecoveryChoice, RecoveryInspectParams,
    RecoveryInspectResult, RecoveryKnownPrefix, RecoveryReadUnavailable,
    RecoveryReceiptInvalidation, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoveryReceiptState, RecoveryReceiptView, RecoverySubmitParams, RecoverySubmitRefusal,
    RecoverySubmitResult,
};
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_rpc::Dispatcher;
use dudes_rpc::auth::{PeerClassifier, PeerIdentity, PeerProvenance};
use dudes_rpc::dispatch::OperatorAdmission;
use dudes_rpc::methods::recovery::RecoveryControl;
use dudes_rpc::server::{Connection, Halt, Listener, LocalTransport, StopFlag, serve};
use dudes_rpc::testing::{FakeMintedTokens, FakeResolver};
use dudes_types::{AgentId, Lsn};
use serde_json::{Value, json};

mod common;

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);

impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Inspect(RecoveryInspectParams),
    Submit(Box<RecoverySubmitParams>),
    Receipt(RecoveryReceiptParams),
}

#[derive(Clone, Copy)]
enum Outcome {
    Accepted,
    Refused,
    Unknown,
}

struct Control {
    inspection: RecoveryInspectResult,
    outcome: Outcome,
    calls: Mutex<Vec<Call>>,
    diagnostics: Bytes,
    before_submit: Mutex<Vec<u8>>,
    receipt: Mutex<RecoveryReceiptResult>,
    drop_reply: AtomicBool,
    halt: Arc<Mutex<Option<Box<dyn Halt>>>>,
}

fn recorded(request: &RecoverySubmitParams) -> RecoveryReceiptView {
    RecoveryReceiptView {
        request_id: request.request_id,
        claim: dudes_proto::recovery::RecoveryClaimRef {
            episode: dudes_proto::recovery::RecoveryEpisodeId {
                work: request.expected.work,
                origin: Lsn(90),
            },
            authorization_lsn: Lsn(300),
        },
        choice: request.choice,
        status: RecoveryReceiptState::Pending,
        attempt: RecoveryAttemptView {
            spawn: None,
            confirmation: None,
            end: None,
        },
        prior_effects_unknown: true,
    }
}

impl RecoveryControl for Control {
    fn inspect_recovery(
        &self,
        params: RecoveryInspectParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryInspectResult, RpcError> {
        assert!(admission.pid() > 0);
        self.calls.lock().unwrap().push(Call::Inspect(params));
        Ok(self.inspection.clone())
    }
    fn submit_recovery(
        &self,
        params: &RecoverySubmitParams,
        admission: OperatorAdmission,
    ) -> Result<RecoverySubmitResult, RpcError> {
        assert_eq!(admission.pid(), std::process::id());
        self.before_submit
            .lock()
            .unwrap()
            .clone_from(&self.diagnostics.0.lock().unwrap());
        self.calls
            .lock()
            .unwrap()
            .push(Call::Submit(Box::new(*params)));
        let result = match self.outcome {
            Outcome::Accepted => {
                let receipt = recorded(params);
                *self.receipt.lock().unwrap() = RecoveryReceiptResult::Found {
                    receipt: Box::new(receipt),
                };
                RecoverySubmitResult::Accepted {
                    receipt: Box::new(receipt),
                }
            }
            Outcome::Refused => RecoverySubmitResult::Refused {
                reason: RecoverySubmitRefusal::StalePredecessor,
                prefix: RecoveryKnownPrefix {
                    required: Some(Lsn(200)),
                    resume: Some(Lsn(250)),
                },
            },
            Outcome::Unknown => RecoverySubmitResult::Unknown {
                request_id: params.request_id,
            },
        };
        if self.drop_reply.swap(false, Ordering::SeqCst) {
            self.halt.lock().unwrap().take().unwrap().halt();
        }
        Ok(result)
    }
    fn recovery_receipt(
        &self,
        params: RecoveryReceiptParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryReceiptResult, RpcError> {
        assert!(admission.pid() > 0);
        self.calls.lock().unwrap().push(Call::Receipt(params));
        Ok(self.receipt.lock().unwrap().clone())
    }
}

struct ListenerWithHalt {
    transport: LocalTransport,
    halt: Arc<Mutex<Option<Box<dyn Halt>>>>,
}

impl Listener for ListenerWithHalt {
    type Conn = UnixStream;
    fn accept(&self) -> io::Result<(Self::Conn, PeerIdentity)> {
        let (connection, peer) = self.transport.accept()?;
        *self.halt.lock().unwrap() = Some(connection.halt_handle()?);
        Ok((connection, peer))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    runtime: PathBuf,
    socket: PathBuf,
    control: Arc<Control>,
    stop: Arc<StopFlag>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(inspection: RecoveryInspectResult, outcome: Outcome) -> Self {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("run");
        let transport = LocalTransport::bind_in(&runtime, "duded.sock").unwrap();
        let socket = transport.path().to_path_buf();
        let halt = Arc::new(Mutex::new(None));
        let control = Arc::new(Control {
            inspection,
            outcome,
            calls: Mutex::new(Vec::new()),
            diagnostics: Bytes::default(),
            before_submit: Mutex::new(Vec::new()),
            receipt: Mutex::new(RecoveryReceiptResult::Absent {
                checked_through: Lsn(500),
            }),
            drop_reply: AtomicBool::new(false),
            halt: Arc::clone(&halt),
        });
        let mut dispatcher = Dispatcher::new(PeerClassifier::new(
            OperatorCredential::new("C".repeat(43)).unwrap(),
            Arc::new(FakeMintedTokens::none()),
        ));
        dudes_rpc::methods::register_recovery(&mut dispatcher, &control);
        std::fs::write(
            runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            "C".repeat(43),
        )
        .unwrap();
        let stop = Arc::new(StopFlag::new());
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let _ = serve(
                    &ListenerWithHalt { transport, halt },
                    &FakeResolver::new(PeerProvenance::NoRecordedAgent),
                    &dispatcher,
                    &stop,
                    NonZeroUsize::new(4).unwrap(),
                );
            })
        };
        Self {
            root,
            runtime,
            socket,
            control,
            stop,
            thread: Some(thread),
        }
    }
    fn environment(&self) -> Environment {
        Environment::resolve(|name| match name {
            "DUDES_SOCK" => Some(self.socket.as_os_str().to_owned()),
            "DUDES_RUNTIME_DIR" => Some(self.runtime.as_os_str().to_owned()),
            _ => None,
        })
    }
    fn file(&self) -> PathBuf {
        let path = self.root.path().join("inspection.json");
        std::fs::write(&path, serde_json::to_vec(&self.control.inspection).unwrap()).unwrap();
        path
    }
    fn run(&self, args: &[OsString], err: &mut dyn Write) -> (u8, Vec<u8>) {
        let mut out = Vec::new();
        let code = dudes::run(args, &self.environment(), None, &mut out, err);
        (code, out)
    }
    fn submit(&self, machine: bool, choice: &str, err: &mut dyn Write) -> (u8, Vec<u8>) {
        let mut args = words(&["recovery", "submit"]);
        args.push(self.file().into_os_string());
        args.extend(words(&["--choice", choice]));
        if machine {
            args.push("--json".into());
        }
        self.run(&args, err)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.stop();
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn inspection(nullable: bool, choices: RecoveryAllowedChoices) -> RecoveryInspectResult {
    let work = json!({"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"});
    let episode = json!({"work":work,"origin":90});
    serde_json::from_value(json!({"state":"available","through":500,"expected":{"work":work,"issue_lsn":20,"source_lsn":80,"episode":if nullable {Value::Null} else {episode.clone()},"expected_predecessor":if nullable {Value::Null} else {json!({"episode":episode,"authorization_lsn":300})},"control_lsn":if nullable {Value::Null} else {json!(90)},"ticket":if nullable {Value::Null} else {json!({"card":"01ARZ3NDEKTSV4RRFFQ69G5FAY","round":2})}},"reason":"held_or_paused_failure","choices":choices,"prior_effects_unknown":true,"waiting":{"open_question":"waiting","budget":"ready","hops":"ready","instruction_review":"ready","instruction_comparison":"waiting"}})).unwrap()
}

fn request_id(err: &[u8]) -> String {
    String::from_utf8_lossy(err)
        .lines()
        .find_map(|line| line.strip_prefix("Recovery request ID: "))
        .unwrap()
        .to_owned()
}

#[test]
fn recovery_parser_requires_explicit_choices_and_documents_the_retained_id() {
    let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    assert_eq!(
        dudes::args::parse(&words(&["recovery", "inspect", agent]))
            .unwrap()
            .command,
        Command::Recovery(RecoveryCommand::Inspect(agent.parse().unwrap()))
    );
    for (name, choice) in [
        ("try-recorded", RecoveryChoice::TryRecorded),
        ("fresh-with-recap", RecoveryChoice::FreshWithRecap),
        ("repeat-issued-fresh", RecoveryChoice::RepeatIssuedFresh),
    ] {
        assert_eq!(
            dudes::args::parse(&words(&[
                "--json",
                "recovery",
                "submit",
                "inspection.json",
                "--choice",
                name
            ]))
            .unwrap()
            .command,
            Command::Recovery(RecoveryCommand::Submit {
                inspection: PathBuf::from("inspection.json"),
                choice
            })
        );
    }
    assert_eq!(
        dudes::args::parse(&words(&["recovery", "receipt", &"0".repeat(64)]))
            .unwrap()
            .command,
        Command::Recovery(RecoveryCommand::Receipt(Hex32::new([0; 32])))
    );
    for args in [
        vec!["recovery"],
        vec!["recovery", "submit", "file"],
        vec!["recovery", "submit", "file", "--choice", "fresh"],
        vec!["recovery", "inspect", agent, "extra"],
        vec!["recovery", "receipt", "A"],
        vec![
            "recovery",
            "submit",
            "file",
            "--choice",
            "try-recorded",
            "--",
            "extra",
        ],
    ] {
        assert!(dudes::args::parse(&words(&args)).is_err(), "{args:?}");
    }
    for literal in [
        "Try recorded conversation",
        "prints and flushes its request ID to stderr",
        "including",
        "do not resend",
        "does not promise launch",
    ] {
        assert!(dudes::args::USAGE.contains(literal), "{literal}");
    }
}

#[test]
fn submit_preserves_every_reviewed_condition_and_flushes_the_id_before_the_only_call() {
    for nullable in [false, true] {
        for machine in [false, true] {
            let inspected = inspection(nullable, RecoveryAllowedChoices::RecordedOrFresh);
            let RecoveryInspectResult::Available { expected, .. } = &inspected else {
                unreachable!()
            };
            let fixture = Fixture::new(inspected.clone(), Outcome::Accepted);
            let mut err = BufWriter::with_capacity(4096, fixture.control.diagnostics.clone());
            let (code, out) = fixture.submit(machine, "try-recorded", &mut err);
            assert_eq!(code, exit::SUCCESS);
            let before = fixture.control.before_submit.lock().unwrap().clone();
            let id = request_id(&before);
            assert_eq!(id.len(), 64);
            let parsed: Hex32 = serde_json::from_value(json!(id)).unwrap();
            let calls = fixture.control.calls.lock().unwrap();
            assert_eq!(
                *calls,
                vec![Call::Submit(Box::new(RecoverySubmitParams {
                    request_id: parsed,
                    expected: **expected,
                    choice: RecoveryChoice::TryRecorded
                }))]
            );
            let encoded = serde_json::to_value(**expected).unwrap();
            if nullable {
                for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
                    assert_eq!(encoded[key], Value::Null);
                }
            }
            if machine {
                assert_eq!(
                    serde_json::from_slice::<Value>(&out).unwrap()["state"],
                    "accepted"
                );
            } else {
                assert!(
                    String::from_utf8(out)
                        .unwrap()
                        .contains("Authorization does not promise that work launched.")
                );
            }
            assert!(
                String::from_utf8(before)
                    .unwrap()
                    .contains("Try recorded conversation")
            );
        }
    }
}

struct FailingOutput {
    flush: bool,
    failed: bool,
}
impl Write for FailingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.flush && (self.failed || bytes.starts_with(b"Recovery request ID: ")) {
            self.failed = true;
            Err(io::Error::other("fixture write failed"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.failed = true;
        Err(io::Error::other("fixture flush failed"))
    }
}

#[test]
fn failed_id_output_or_flush_sends_no_mutating_call() {
    for flush in [false, true] {
        let fixture = Fixture::new(
            inspection(false, RecoveryAllowedChoices::FreshOnly),
            Outcome::Accepted,
        );
        let mut writer = FailingOutput {
            flush,
            failed: false,
        };
        let (code, out) = fixture.submit(true, "fresh-with-recap", &mut writer);
        assert_eq!(code, exit::OTHER);
        assert!(writer.failed);
        assert_eq!(out, b"");
        assert_eq!(*fixture.control.calls.lock().unwrap(), Vec::<Call>::new());
    }
}

#[test]
fn malformed_unavailable_oversized_and_unoffered_files_never_refresh_or_submit() {
    let fixture = Fixture::new(
        inspection(false, RecoveryAllowedChoices::FreshOnly),
        Outcome::Accepted,
    );
    let path = fixture.file();
    let mut oversized = serde_json::to_vec(&fixture.control.inspection).unwrap();
    oversized.resize(16 * 1024 + 1, b' ');
    let args = [
        OsString::from("recovery"),
        "submit".into(),
        path.clone().into_os_string(),
        "--choice".into(),
        "fresh-with-recap".into(),
    ];
    for bytes in [
        b"{".to_vec(),
        serde_json::to_vec(&json!({"state":"unavailable","reason":"claimed"})).unwrap(),
        oversized,
    ] {
        std::fs::write(&path, &bytes).unwrap();
        let mut err = Vec::new();
        let (code, out) = fixture.run(&args, &mut err);
        assert_eq!(code, exit::OTHER);
        assert_eq!(out, b"");
        assert!(
            !String::from_utf8(err)
                .unwrap()
                .contains("Recovery request ID:")
        );
    }
    let mut err = Vec::new();
    assert_eq!(
        fixture.submit(true, "try-recorded", &mut err).0,
        exit::OTHER
    );
    assert_eq!(*fixture.control.calls.lock().unwrap(), Vec::<Call>::new());
    let mut maximal = serde_json::to_vec(&fixture.control.inspection).unwrap();
    maximal.resize(16 * 1024, b' ');
    std::fs::write(&path, maximal).unwrap();
    let good = fixture.run(&args, &mut Vec::new());
    assert_eq!(good.0, exit::SUCCESS);
    assert!(matches!(
        fixture.control.calls.lock().unwrap().as_slice(),
        [Call::Submit(_)]
    ));
}

struct ReplaceOnFlush {
    bytes: Bytes,
    path: PathBuf,
    replacement: Vec<u8>,
}

impl Write for ReplaceOnFlush {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        std::fs::write(&self.path, &self.replacement)
    }
}

#[test]
fn changing_the_file_during_id_flush_keeps_the_original_reviewed_selector() {
    let original = inspection(true, RecoveryAllowedChoices::FreshOnly);
    let RecoveryInspectResult::Available { expected, .. } = &original else {
        unreachable!()
    };
    let fixture = Fixture::new(original.clone(), Outcome::Accepted);
    let mut err = BufWriter::with_capacity(
        4096,
        ReplaceOnFlush {
            bytes: fixture.control.diagnostics.clone(),
            path: fixture.file(),
            replacement: serde_json::to_vec(&inspection(
                false,
                RecoveryAllowedChoices::RecordedOrFresh,
            ))
            .unwrap(),
        },
    );
    assert_eq!(
        fixture.submit(true, "fresh-with-recap", &mut err).0,
        exit::SUCCESS
    );
    let calls = fixture.control.calls.lock().unwrap();
    let [Call::Submit(request)] = calls.as_slice() else {
        panic!("one submit")
    };
    assert_eq!(request.expected, **expected);
    assert_eq!(request.choice, RecoveryChoice::FreshWithRecap);
    assert!(
        String::from_utf8(fixture.control.before_submit.lock().unwrap().clone())
            .unwrap()
            .contains("Recovery request ID: ")
    );
}

#[test]
fn reply_loss_retains_one_id_and_a_later_receipt_never_resubmits() {
    let fixture = Fixture::new(
        inspection(false, RecoveryAllowedChoices::RepeatIssuedFresh),
        Outcome::Accepted,
    );
    fixture.control.drop_reply.store(true, Ordering::SeqCst);
    let mut err = Vec::new();
    let (code, out) = fixture.submit(true, "repeat-issued-fresh", &mut err);
    assert_eq!(code, exit::OTHER);
    let id = request_id(&err);
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap(),
        json!({"state":"reply_unavailable","request_id":id})
    );
    let diagnostic = String::from_utf8(err).unwrap();
    assert!(diagnostic.contains(&format!("dudes recovery receipt {id}")));
    assert!(diagnostic.contains("Do not resend the submission."));
    assert!(diagnostic.contains("prior effects may repeat"));
    let (code, out) = fixture.run(
        &words(&["--json", "recovery", "receipt", &id]),
        &mut Vec::new(),
    );
    assert_eq!(code, exit::SUCCESS);
    let receipt: RecoveryReceiptResult = serde_json::from_slice(&out).unwrap();
    let RecoveryReceiptResult::Found { receipt } = receipt else {
        panic!("found")
    };
    assert_eq!(serde_json::to_value(receipt.request_id).unwrap(), json!(id));
    let calls = fixture.control.calls.lock().unwrap();
    let [Call::Submit(submitted), Call::Receipt(lookup)] = calls.as_slice() else {
        panic!("one submit followed by one receipt")
    };
    let retained_id: Hex32 = serde_json::from_value(json!(id)).unwrap();
    assert_eq!(submitted.request_id, retained_id);
    assert_eq!(
        *lookup,
        RecoveryReceiptParams {
            request_id: retained_id
        }
    );
}

#[test]
fn daemon_unknown_and_refused_outcomes_remain_distinct_from_a_missing_reply() {
    for (outcome, state, code) in [
        (Outcome::Unknown, "unknown", exit::OTHER),
        (Outcome::Refused, "refused", exit::REFUSED),
    ] {
        let fixture = Fixture::new(
            inspection(false, RecoveryAllowedChoices::FreshOnly),
            outcome,
        );
        let mut err = Vec::new();
        let (actual, out) = fixture.submit(true, "fresh-with-recap", &mut err);
        assert_eq!(actual, code);
        let id = request_id(&err);
        let value: Value = serde_json::from_slice(&out).unwrap();
        if state == "unknown" {
            assert_eq!(value, json!({"state":"unknown","request_id":id}));
        } else {
            assert_eq!(
                value,
                json!({"state":"refused","reason":"stale_predecessor","prefix":{"required":200,"resume":250}})
            );
        }
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("Fresh context does not restore old conversation history.")
        );
        assert!(matches!(
            fixture.control.calls.lock().unwrap().as_slice(),
            [Call::Submit(_)]
        ));
    }
    let fixture = Fixture::new(
        inspection(false, RecoveryAllowedChoices::FreshOnly),
        Outcome::Refused,
    );
    let (code, out) = fixture.submit(false, "fresh-with-recap", &mut Vec::new());
    assert_eq!(code, exit::REFUSED);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Recovery refused: StalePredecessor.\nRequired barrier already acknowledged at 0:200.\nResume already acknowledged at 0:250.\n"
    );
    let mut out = Vec::new();
    render::recovery_submitted(
        &mut out,
        false,
        &RecoverySubmitResult::Refused {
            reason: RecoverySubmitRefusal::StalePredecessor,
            prefix: RecoveryKnownPrefix {
                required: None,
                resume: None,
            },
        },
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Recovery refused: StalePredecessor.\n"
    );
}

#[test]
fn real_classifier_requires_the_operator_credential_for_inspect_submit_and_receipt() {
    let fixture = Fixture::new(
        inspection(false, RecoveryAllowedChoices::FreshOnly),
        Outcome::Accepted,
    );
    let file = fixture.file();
    let agent = AgentId::from_parts(1, 1).to_string();
    let commands = [
        words(&["--json", "recovery", "inspect", &agent]),
        vec![
            "--json".into(),
            "recovery".into(),
            "submit".into(),
            file.into_os_string(),
            "--choice".into(),
            "fresh-with-recap".into(),
        ],
        words(&["--json", "recovery", "receipt", &"0".repeat(64)]),
    ];
    for credential in [None, Some("W".repeat(43))] {
        let path = fixture.runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
        if let Some(text) = credential {
            std::fs::write(&path, text).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        for command in &commands {
            let mut err = Vec::new();
            let (code, out) = fixture.run(command, &mut err);
            assert_eq!(code, exit::REFUSED);
            assert_eq!(out, b"");
            let text = String::from_utf8(err).unwrap();
            let error: RpcError =
                serde_json::from_str(text.lines().find(|line| line.starts_with('{')).unwrap())
                    .unwrap();
            assert_eq!(error.code, ErrorCode::Unauthorized.code());
        }
    }
    assert_eq!(*fixture.control.calls.lock().unwrap(), Vec::<Call>::new());
    std::fs::write(
        fixture.runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "C".repeat(43),
    )
    .unwrap();
    for command in &commands {
        assert_eq!(fixture.run(command, &mut Vec::new()).0, exit::SUCCESS);
    }
    assert!(matches!(
        fixture.control.calls.lock().unwrap().as_slice(),
        [Call::Inspect(_), Call::Submit(_), Call::Receipt(_)]
    ));
}

#[test]
fn binary_inspection_exports_one_json_value_usable_as_the_reviewed_file() {
    let fixture = Fixture::new(
        inspection(true, RecoveryAllowedChoices::FreshOnly),
        Outcome::Accepted,
    );
    let result = common::Cli::new(&fixture.socket, &fixture.runtime)
        .args([
            "--json",
            "recovery",
            "inspect",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ])
        .run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert_eq!(
        serde_json::from_str::<RecoveryInspectResult>(&result.stdout).unwrap(),
        fixture.control.inspection
    );
    assert_eq!(result.stderr, "");
    assert!(matches!(
        fixture.control.calls.lock().unwrap().as_slice(),
        [Call::Inspect(_)]
    ));
}

#[test]
fn human_receipts_preserve_all_lifecycle_and_absence_meanings() {
    let request:RecoverySubmitParams=serde_json::from_value(json!({"request_id":"0".repeat(64),"expected":match inspection(true,RecoveryAllowedChoices::FreshOnly) {RecoveryInspectResult::Available {expected,..}=>expected,RecoveryInspectResult::Unavailable { .. }=>unreachable!()},"choice":"fresh_with_recap"})).unwrap();
    let mut receipt = recorded(&request);
    let turn = request.expected.work.turn;
    for (status, literal) in [
        (
            RecoveryReceiptState::Pending,
            "Pending: the reserved turn has not been issued.",
        ),
        (
            RecoveryReceiptState::IssueBound {
                turn,
                issue_lsn: Lsn(20),
            },
            "Issue-bound: turn 01ARZ3NDEKTSV4RRFFQ69G5FAX, issue 0:20; no consuming physical receipt is recorded.",
        ),
        (
            RecoveryReceiptState::Consumed {
                turn,
                issue_lsn: Lsn(20),
                evidence_lsn: Lsn(400),
            },
            "Consumed: turn 01ARZ3NDEKTSV4RRFFQ69G5FAX, issue 0:20, physical receipt 0:400.",
        ),
        (
            RecoveryReceiptState::Invalidated {
                invalidation_lsn: Lsn(400),
                reason: RecoveryReceiptInvalidation::ControlChanged,
            },
            "Invalidated at 0:400: ControlChanged.",
        ),
        (
            RecoveryReceiptState::Superseded {
                turn,
                issue_lsn: Lsn(20),
                successor: receipt.claim.episode,
                barrier_lsn: Lsn(500),
            },
            "Superseded by a new recovery episode at 0:500; the owed issue is retained.",
        ),
    ] {
        receipt.status = status;
        let mut out = Vec::new();
        render::recovery_receipt(
            &mut out,
            false,
            &RecoveryReceiptResult::Found {
                receipt: Box::new(receipt),
            },
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(literal));
        assert!(text.contains("Authorization does not promise that work launched."));
    }
    let mut out = Vec::new();
    render::recovery_receipt(
        &mut out,
        false,
        &RecoveryReceiptResult::Absent {
            checked_through: Lsn(500),
        },
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "No authorization at checked cut 0:500. A Required or Resume prefix may still have been recorded.\n"
    );
    let mut out = Vec::new();
    render::recovery_receipt(
        &mut out,
        false,
        &RecoveryReceiptResult::Unavailable {
            reason: RecoveryReadUnavailable::InvalidHistory,
        },
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Recovery receipt unavailable: InvalidHistory. Neither presence nor absence is certified.\n"
    );
}
