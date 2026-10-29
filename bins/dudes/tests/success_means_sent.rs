//! `SUCCESS` means the daemon ran the method: no path that sends nothing may return it.
//!
//! # A socket nothing is listening on turns that rule into one assertion
//!
//! `src/run.rs`'s `dispatch` validates a hire's directory locally, then connects **before** it reads
//! a credential or writes a frame. Against a socket path with nothing behind it every valid argv
//! that *would* send can
//! only reach [`exit::UNREACHABLE`]. Under that environment [`exit::SUCCESS`] is therefore a
//! synonym for **"exited success having sent nothing"** — exactly what the rule forbids — and the
//! check becomes one assertion quantified over *argv* rather than a list of cases someone
//! remembered.
//!
//! # `UNREACHABLE` is a no-send code too, and the rule is not about it
//!
//! The rule is that a run that sends nothing returns `USAGE` or `OTHER`, never `SUCCESS`. A run can send nothing and still be
//! [`exit::UNREACHABLE`], and that is not an exception to it: `2` is given to "no
//! conversation happened", which is the code whose whole job is to say nothing was sent because
//! the socket was not there. `USAGE` and `OTHER` are the two a **local** outcome may return, and
//! `UNREACHABLE` here is the observable meaning *this argv was on its way to the daemon* — which is
//! what [`a_help_token_the_parser_never_stood_on_still_reaches_the_socket`] reads.
//!
//! # Two enumerations, and exactly what neither covers
//!
//! **From the type.** [`argv_producing`] maps every [`ArgError`] variant to an argv, in a `match`
//! with no wildcard arm. A new `ArgError` — a new way for this CLI to end a run having sent nothing
//! — does not compile until this file names an argv that produces it. That enumerates the
//! no-send paths from the code, and it is precisely a **stop sign at
//! `argv_producing`**, not a promise that every variant is asserted: the loop reads the hand-written
//! [`every_mistake`], which nothing links to the `match`. `argv_producing`'s own doc measures that
//! gap rather than describing it away.
//!
//! **From the input.** [`no_argv_over_this_clis_alphabet_exits_success_without_a_daemon`] runs
//! every argv of length 0 to [`MAX_LEN`] over [`ALPHABET`] — 4369 of them — and consults no list of
//! variants at all, so it reaches no-send outcomes nobody named.
//!
//! **What is left over, stated rather than implied.** Stable Rust cannot count an enum's variants,
//! so nothing here proves that [`every_mistake`] names every variant that exists *today*; only that
//! no variant can be *added* without a reviewer being stopped at [`argv_producing`], and that none
//! can be quietly *deleted* from the list ([`the_list_of_mistakes_has_not_shrunk`]). The sweep is
//! what bounds the rest, by reaching outcomes through inputs instead of through names. Four things
//! sit outside both:
//!
//! - Argvs longer than [`MAX_LEN`], and tokens outside [`ALPHABET`].
//! - **Four of the six socket-first commands' send paths, in the sweep.** The lines that reach the
//!   socket are `ping`, `tail`, one `why`, and valid `ask --text x` shapes; `hire` and `goal` need
//!   more than three tokens to parse, so the sweep cannot send them however wide its alphabet grows. They are
//!   driven down the send path by hand in
//!   [`a_help_token_the_parser_never_stood_on_still_reaches_the_socket`] instead.
//! - `quickstart`, whose no-daemon path deliberately starts one and then sends `system.ping`.
//!   `tests/quickstart.rs` owns that process boundary and its no-constant failure control.
//! - A no-send failure **after** a successful connect.
//!   [`a_connected_run_that_cannot_read_its_credential_exits_other`] covers the one an operator can
//!   actually produce. The two that remain — `wire`'s `Tail` arm and `encode` — have no live
//!   producer, and both return a `Failure`, whose `code` (`src/run.rs`) has three arms and none of
//!   them is `SUCCESS`.
//! - What a *served* call returns. `tests/exit_codes.rs` owns that side, against a real daemon.
#![cfg(unix)]

use std::collections::HashSet;
use std::ffi::OsString;
use std::mem::discriminant;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;

use dudes::args::{self, ArgError, Command, TicketCommand};
use dudes::{Environment, exit};

/// An agent id the parser accepts, so a line's outcome is about the token under test.
const AN_AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

// ---------------------------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------------------------

/// The code the shell would see for `line`, against `socket` and `runtime_dir`.
///
/// In-process rather than through `env!("CARGO_BIN_EXE_dudes")`, and the value is the same one:
/// `src/main.rs` hands `run`'s `u8` straight to `ExitCode::from` and does nothing else with it.
/// `tests/exit_codes.rs` is where the real process is spawned; 2955 subprocesses would buy the same
/// number back at a cost that would make the sweep the first thing anyone deleted.
fn code_against(line: &[OsString], socket: &Path, runtime_dir: &Path) -> u8 {
    let socket = socket.as_os_str().to_owned();
    let runtime_dir = runtime_dir.as_os_str().to_owned();
    let env = Environment::resolve(|name| match name {
        "DUDES_SOCK" => Some(socket.clone()),
        "DUDES_RUNTIME_DIR" => Some(runtime_dir.clone()),
        // Nothing else, and `HOME` least of all: a fallback into the operator's real runtime
        // directory would point this at whatever daemon they happen to be running.
        _ => None,
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    dudes::run(line, &env, None, &mut out, &mut err)
}

/// A directory with no daemon in it, and a socket path inside it nothing ever binds.
struct NoDaemon {
    dir: tempfile::TempDir,
}

impl NoDaemon {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a directory with no daemon in it"),
        }
    }

    fn code(&self, line: &[OsString]) -> u8 {
        code_against(
            line,
            &self.dir.path().join("no-daemon.sock"),
            self.dir.path(),
        )
    }
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// A `goal` line whose predicate argv is `predicate` and whose text is `text`.
///
/// An empty `predicate` is a line whose payload after `--` is empty, which `src/args.rs` treats as
/// the same mistake as no `--` at all — so it is also the argv for [`ArgError::NoPredicate`].
fn goal_line(text: &str, predicate: &[&str]) -> Vec<OsString> {
    let mut line = vec!["goal", "--agent", AN_AGENT, "--text", text];
    line.extend_from_slice(&["--usd", "1", "--hop-cap", "1", "--"]);
    line.extend_from_slice(predicate);
    argv(&line)
}

/// A `hire` line whose `--name` value is `name`.
fn hire_line(name: &str, cwd: &Path) -> Vec<OsString> {
    let mut line = argv(&[
        "hire",
        "--name",
        name,
        "--role",
        "r",
        "--department",
        "d",
        "--provider",
        "claude",
        "--cwd",
    ]);
    line.push(cwd.as_os_str().to_owned());
    line.extend(argv(&["--usd-per-day", "1"]));
    line
}

// ---------------------------------------------------------------------------------------------
// The enumeration from the type
// ---------------------------------------------------------------------------------------------

/// One argv per [`ArgError`] variant, enumerated from the type.
///
/// **The `match` has no wildcard arm, so a variant added to `src/args.rs` does not build until this
/// function names an argv that produces it.** That is what the compiler forces, and it is the
/// enumeration this file offers.
///
/// **What it does not force is that the new variant is then *asserted*.** The test loop iterates
/// [`every_mistake`], which is a hand-written list this function is not linked to: adding an arm
/// here and forgetting the list there compiles, and every test passes. So
/// the guarantee is that no no-send outcome can be added without a reviewer being stopped at this
/// function, not that every variant is checked. [`every_mistake`] pins its own length, which
/// catches the list shrinking; nothing here can catch it failing to grow.
fn argv_producing(mistake: &ArgError) -> Vec<OsString> {
    match mistake {
        ArgError::NoCommand => Vec::new(),
        ArgError::UnknownCommand(_) => argv(&["nosuch"]),
        ArgError::UnknownFlag(_) => argv(&["ping", "--loud"]),
        ArgError::MissingValue(_) => argv(&["hire", "--name"]),
        ArgError::Missing(_) => argv(&["why"]),
        // The one line that cannot be written as text, which is the point of the variant: a byte
        // that is not UTF-8, where the wire is JSON and JSON is UTF-8.
        ArgError::NotUtf8(_) => vec![
            OsString::from("hire"),
            OsString::from("--name"),
            OsString::from_vec(vec![0xff]),
        ],
        ArgError::BadValue { .. } => argv(&["tail", "--from", "not-a-position"]),
        ArgError::MemorySearchInput(_) => argv(&["memory", "search", ""]),
        ArgError::NoPredicate => goal_line("t", &[]),
        ArgError::NoSubcommand(_) => argv(&["floor"]),
        ArgError::UnknownKey(_) => argv(&["config", "unset", "floor.bogus"]),
        ArgError::ChatAlwaysOn => argv(&["config", "set", "floor.chat", "on"]),
        // `config` edits a local file and has no contract type for `--json` to print.
        ArgError::TextOnly(_) => argv(&["--json", "config"]),
        // A quickstart option that only means something beside another one.
        ArgError::Requires(..) => argv(&["quickstart", "--goal-usd", "1"]),
        ArgError::Conflicting(..) => argv(&["mcp", AN_AGENT, "--clear", "--mcp", "memory"]),
        ArgError::NoteFile(_) => argv(&[
            "ticket",
            "add",
            "--title",
            "t",
            "--note-file",
            "/nonexistent/note",
        ]),
        ArgError::HelpRequested => argv(&["hire", "--help"]),
    }
}

/// Every variant of [`ArgError`], named once.
///
/// Hand-written, because stable Rust has no way to enumerate an enum's variants — this is the one
/// list here the compiler does not check, and [`argv_producing`] says exactly what that costs. The
/// values carried are placeholders: nothing reads anything but the variant.
///
/// The length is pinned by [`the_list_of_mistakes_has_not_shrunk`]. That is a real half of the
/// guarantee and not a tautology: it makes *deleting* an entry red. It cannot make *forgetting to
/// add* one red — a number and a list edited together are one edit — which is why the number is
/// asserted in a test of its own where a reader meets it rather than buried in the loop.
fn every_mistake() -> Vec<ArgError> {
    vec![
        ArgError::NoCommand,
        ArgError::UnknownCommand(String::new()),
        ArgError::UnknownFlag(String::new()),
        ArgError::MissingValue(""),
        ArgError::Missing(""),
        ArgError::NotUtf8(""),
        ArgError::BadValue {
            flag: "",
            value: String::new(),
            what: "",
        },
        ArgError::MemorySearchInput(dudes_proto::rpc::memory::MemorySearchInputError::QueryLength),
        ArgError::NoPredicate,
        ArgError::NoSubcommand(""),
        ArgError::UnknownKey(String::new()),
        ArgError::ChatAlwaysOn,
        ArgError::TextOnly(""),
        ArgError::Requires("", ""),
        ArgError::Conflicting("", ""),
        ArgError::NoteFile(String::new()),
        ArgError::HelpRequested,
    ]
}

#[test]
fn missing_ticket_notes_exit_usage_at_materialization_without_connecting() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("door.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let missing = root.path().join("absent-note.md");
    let env = Environment::resolve(|name| match name {
        "DUDES_SOCK" => Some(socket.as_os_str().to_owned()),
        "DUDES_RUNTIME_DIR" => Some(root.path().as_os_str().to_owned()),
        _ => None,
    });
    for mut line in [
        argv(&["ticket", "add", "--title", "Title"]),
        argv(&["ticket", "edit", AN_AGENT]),
    ] {
        line.extend([
            OsString::from("--note-file"),
            missing.as_os_str().to_owned(),
        ]);
        args::parse(&line).expect("syntax is valid before note materialization");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            dudes::run(&line, &env, None, &mut out, &mut err),
            exit::USAGE
        );
        assert!(String::from_utf8(err).unwrap().contains("--note-file"));
        assert!(out.is_empty());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

/// The count [`every_mistake`] is expected to have, so shrinking the list is red.
///
/// Every variant `src/args.rs` declares.
const EVERY_MISTAKE_COUNT: usize = 17;

/// **The hand-written list has not lost an entry.**
///
/// The half of [`argv_producing`]'s gap that *is* closable. A variant dropped from
/// [`every_mistake`] silently stops being asserted and nothing else notices, because the loop that
/// reads it iterates whatever it is given.
#[test]
fn the_list_of_mistakes_has_not_shrunk() {
    assert_eq!(
        every_mistake().len(),
        EVERY_MISTAKE_COUNT,
        "an entry left the list, so some no-send outcome is no longer asserted"
    );
}

/// **Every local argument outcome that sends nothing exits `USAGE` or `OTHER`.**
///
/// `HelpRequested` is in that set and is the reason this test exists: it is *not a mistake* — the
/// operator asked a question and got an answer — but it is still a run in which the daemon ran no
/// method, and `SUCCESS` is given to "the daemon ran the method and answered" alone.
#[test]
fn every_parse_outcome_that_sends_nothing_exits_usage_or_other() {
    let nowhere = NoDaemon::new();
    let mut named = HashSet::new();

    for mistake in every_mistake() {
        let line = argv_producing(&mistake);

        // The mapping is asserted rather than assumed. An argv that drifted into producing a
        // *different* variant would otherwise leave its own variant unchecked while this list still
        // looked complete — which is the failure mode the whole enumeration exists to prevent.
        if matches!(mistake, ArgError::NoteFile(_)) {
            let invocation = args::parse(&line).expect("note I/O follows transport admission");
            let Command::Ticket(TicketCommand::Add { note_file, .. }) = invocation.command else {
                panic!("the missing note must reach ticket materialization");
            };
            assert_eq!(note_file.as_deref(), Some(Path::new("/nonexistent/note")));
        } else {
            let produced = args::parse(&line).expect_err("this line is not a valid call");
            assert_eq!(
                discriminant(&produced),
                discriminant(&mistake),
                "{line:?} produces {produced:?}, not the {mistake:?} it is listed under"
            );
        }
        assert!(
            named.insert(discriminant(&mistake)),
            "{mistake:?} is listed twice, which is what a copy-paste that dropped another \
             variant looks like"
        );

        let code = nowhere.code(&line);
        assert!(
            code == exit::USAGE || code == exit::OTHER,
            "`dudes {line:?}` sent nothing and exited {code}; it must exit {} or {}",
            exit::USAGE,
            exit::OTHER
        );
    }

    // Positive control. Every assertion above is about a locally refused line, and a binary that
    // refused *every* line would satisfy all of them — so one well-formed call has to get past the
    // parser and reach the socket under the same harness.
    assert_eq!(
        nowhere.code(&argv(&["ping"])),
        exit::UNREACHABLE,
        "control: a well-formed call reaches the socket rather than the usage text"
    );
}

// ---------------------------------------------------------------------------------------------
// The enumeration from the input
// ---------------------------------------------------------------------------------------------

/// The tokens the sweep is drawn from: this CLI's own vocabulary, plus the three things an operator
/// types that are not vocabulary at all — a value a flag accepts, a value no flag accepts, and an
/// empty argument. `quickstart` is deliberately outside this socket-first corpus: an absent daemon
/// is its expected success path, not the no-send control this sweep creates.
///
/// [`AN_AGENT`] is here for a reason worth stating. Without a value `--agent` accepts, no `why` line
/// in the whole sweep parses, so `why`'s send path is never entered and the sweep reads as though it
/// covered six socket-first commands while covering five. Adding it moves `why` from
/// **0** sending lines to **1**.
const ALPHABET: [&str; 16] = [
    "--help", "-h", "help", "--", "--json", "ping", "hire", "goal", "ask", "why", "tail",
    "--agent", "--text", AN_AGENT, "x", "",
];

/// How long the swept argvs get. Three is what reaches a command word, a flag and its value.
const MAX_LEN: u32 = 3;

/// Every argv of length `0..=MAX_LEN` over [`ALPHABET`].
fn sweep() -> Vec<Vec<OsString>> {
    let mut lines = vec![Vec::new()];
    let mut frontier = vec![Vec::<OsString>::new()];
    for _ in 0..MAX_LEN {
        frontier = frontier
            .iter()
            .flat_map(|prefix| {
                ALPHABET.iter().map(|token| {
                    let mut line = prefix.clone();
                    line.push(OsString::from(*token));
                    line
                })
            })
            .collect();
        lines.extend(frontier.iter().cloned());
    }
    lines
}

/// **No argv at all exits `SUCCESS` when there is no daemon to have run anything.**
///
/// The arm that needs no list of variants: it quantifies over inputs, so a no-send path nobody
/// named is covered anyway. **The corpus is stated because a sweep proves nothing about what it did
/// not sweep** — 4369 argvs, every one of them 0 to 3 tokens drawn from [`ALPHABET`]. It does not
/// cover longer lines, tokens outside that alphabet, or anything past a successful connect.
///
/// **And it is lopsided in a way the count hides.** The sending lines are `ping`, `tail`, one
/// `why`, and valid `ask --text x` shapes: `hire` and `goal` send in **zero** of them and cannot
/// send in any, because their required flags do not fit in three tokens. The no-send half of this
/// sweep is broad; the send half is narrow, and covers four of six socket-first commands.
/// [`a_help_token_the_parser_never_stood_on_still_reaches_the_socket`] is where `hire` and `goal`
/// are driven down their send path, by hand and at full length.
#[test]
fn no_argv_over_this_clis_alphabet_exits_success_without_a_daemon() {
    let nowhere = NoDaemon::new();
    let lines = sweep();
    let corpus: usize = (0..=MAX_LEN).map(|n| ALPHABET.len().pow(n)).sum();
    assert_eq!(
        lines.len(),
        corpus,
        "the sweep is the corpus it claims to be"
    );
    // And the corpus is the one this file's header names. Pinned rather than merely derived: the
    // header's number is the evidence a reader checks, and widening `ALPHABET` or `MAX_LEN` without
    // moving it would leave a stated corpus that is no longer the one swept.
    assert_eq!(lines.len(), 4369, "the corpus this claim covers");

    let mut reached_the_socket = 0_usize;
    for line in &lines {
        // A lone help flag answers a request for the text and is the one sanctioned success: no
        // predicate argv holds exactly this by accident.
        if matches!(line.as_slice(), [only] if only == "--help" || only == "-h") {
            assert_eq!(nowhere.code(line), exit::SUCCESS, "{line:?}");
            continue;
        }
        let code = nowhere.code(line);
        assert_ne!(
            code,
            exit::SUCCESS,
            "`dudes {line:?}` exited SUCCESS with no daemon anywhere, so it succeeded having sent \
             nothing"
        );
        if code == exit::UNREACHABLE {
            reached_the_socket += 1;
        }
    }

    // Positive control. "Never SUCCESS" is also true of a binary that refuses every argv it is
    // handed, and such a binary would be broken in the opposite direction — so the sweep has to
    // show that some of these lines did get as far as the socket.
    assert!(
        reached_the_socket > 0,
        "control: nothing in the sweep reached the socket, so the assertion above proved nothing"
    );
}

// ---------------------------------------------------------------------------------------------
// The other half: what must still be sent
// ---------------------------------------------------------------------------------------------

/// **A help token the parser never stood on is an operand, and its call is still sent.**
///
/// This is the arm a whole-argv `--help` scan reddens, and the reason this file has a must-send
/// half at all. With help mapped to [`exit::USAGE`], reintroducing that scan turns each of these
/// lines into a `1` — still not `SUCCESS`, so both tests above would stay green while the defect
/// was back. What such a scan cannot see is *position*: the `--` split, and which
/// tokens a value-taking flag has already eaten.
#[test]
fn a_help_token_the_parser_never_stood_on_still_reaches_the_socket() {
    let nowhere = NoDaemon::new();

    for line in [
        // After `--`: the predicate's own argv, resolved by the daemon.
        goal_line("t", &["/usr/bin/make", "--help"]),
        goal_line("t", &["/usr/bin/sort", "-h"]),
        // In a value position: consumed by the flag that takes it, in every spelling.
        goal_line("help", &["/usr/bin/true"]),
        hire_line("help", nowhere.dir.path()),
        hire_line("--help", nowhere.dir.path()),
        hire_line("-h", nowhere.dir.path()),
    ] {
        assert_eq!(
            nowhere.code(&line),
            exit::UNREACHABLE,
            "this call must be sent, not answered with the usage text: {line:?}"
        );
    }

    // Control: the same harness gives a real help request a different code, so `UNREACHABLE` above
    // is a fact about these lines rather than what this environment returns for everything.
    assert_eq!(
        nowhere.code(&argv(&["hire", "--help"])),
        exit::USAGE,
        "control: a help request in a flag position is answered here and sends nothing"
    );
}

/// **A run that connected and still sent nothing exits `OTHER`.**
///
/// The no-send outcome the sweep structurally cannot reach: `src/run.rs`'s `dispatch` reads the
/// operator credential *after* the connection is up and *before* any frame goes out, so a
/// credential that exists and is not one ends the run with a socket open and nothing sent.
///
/// The listener is a bare [`UnixListener`] rather than a daemon on purpose — what is under test is
/// which side of the send the CLI stopped on, and a daemon would put a second explanation under
/// every code this reads.
#[test]
fn a_connected_run_that_cannot_read_its_credential_exits_other() {
    let dir = tempfile::tempdir().expect("a runtime directory");
    let socket = dir.path().join("duded.sock");
    let listener = UnixListener::bind(&socket).expect("a listener with nothing behind it");

    // Accepted and dropped, twice: enough for the two runs below, and bounded so the thread can be
    // joined instead of left blocked in `accept` for the rest of the binary's life.
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            drop(listener.accept());
        }
    });

    std::fs::write(
        dir.path().join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "not an operator credential",
    )
    .expect("write a credential that is not one");
    assert_eq!(
        code_against(&argv(&["ping"]), &socket, dir.path()),
        exit::OTHER,
        "connected, could not read the credential, and sent nothing"
    );

    // Control: the same socket and the same listener, with no credential file at all. Presenting
    // nothing is a call the daemon answers (`src/run.rs`'s header), so this run *does* send, and
    // the listener dropping it makes that `UNREACHABLE` — a different code, which is what shows
    // `OTHER` above was the credential read and not the listener.
    let empty = tempfile::tempdir().expect("a directory with no credential in it");
    assert_eq!(
        code_against(&argv(&["ping"]), &socket, empty.path()),
        exit::UNREACHABLE,
        "control: with nothing to read, the call is sent and the close is what ends it"
    );

    server.join().expect("the listener thread");
}
