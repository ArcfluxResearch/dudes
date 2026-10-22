//! Invariants 6, 7 and 10 at the real PTY seam: launch input stays typed and exact, reads are
//! bounded, explicit writes and resizes reach the slave, and close/shutdown stop the whole tree.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_pty::{CloseOutcome, MAX_READ_BYTES, PtyHost, PtyRead, PtySize};

const DAEMON_PROBE: &str = "DUDES_PTY_DAEMON_PROBE";
const HELPER_PROBE: &str = "DUDES_PTY_HELPER_PROBE";
const GRANDCHILD_PROBE: &str = "DUDES_PTY_GRANDCHILD_PROBE";
const AMBIENT_CANARY: &str = "DUDES_PTY_AMBIENT_CANARY";
const EXPLICIT_ENV: &str = "DUDES_PTY_EXPLICIT";
const CHILD_BEAT: &str = "DUDES_PTY_CHILD_BEAT";
const GRANDCHILD_BEAT: &str = "DUDES_PTY_GRANDCHILD_BEAT";
const EXACT_ENV_VALUE: &str = "explicit-value-$();|&";
const LITERAL_ARGS: &[&str] = &["space value", "quote\"value", "$() ; | &"];

#[test]
fn pty_host_streams_writes_resizes_closes_and_dies_with_daemon() {
    let executable = std::env::current_exe().expect("current test executable");
    let status = Command::new(executable)
        .args(["--ignored", "--exact", "pty_daemon_probe", "--nocapture"])
        .env(DAEMON_PROBE, "1")
        .env(AMBIENT_CANARY, "must-not-reach-the-pty")
        .stdin(Stdio::null())
        .status()
        .expect("spawn isolated daemon probe");
    assert!(status.success(), "isolated daemon probe failed: {status}");
}

#[test]
#[ignore = "spawned by the exact acceptance test with a controlled ambient environment"]
fn pty_daemon_probe() {
    if std::env::var_os(DAEMON_PROBE).is_none() {
        return;
    }
    run_host_scenario();
}

#[test]
#[ignore = "spawned inside a real PTY by the daemon probe"]
fn pty_helper_probe() {
    if std::env::var_os(HELPER_PROBE).is_none() {
        return;
    }
    run_pty_helper();
}

#[test]
#[ignore = "spawned as the PTY helper's immediate grandchild"]
fn pty_grandchild_probe() {
    if std::env::var_os(GRANDCHILD_PROBE).is_none() {
        return;
    }
    let path = PathBuf::from(std::env::var_os(GRANDCHILD_BEAT).expect("grandchild beat path"));
    heartbeat_forever(&path);
}

#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end scenario keeps the exact acceptance seam in one place"
)]
fn run_host_scenario() {
    let temp = tempfile::tempdir().expect("tempdir");
    let executable = std::env::current_exe().expect("current test executable");
    let size = PtySize { cols: 80, rows: 24 };
    let mut host = PtyHost::new();

    let child_beat = temp.path().join("child.beat");
    let grandchild_beat = temp.path().join("grandchild.beat");
    let mut opened = host
        .open(
            helper_spec(&executable, temp.path(), &child_beat, &grandchild_beat),
            size,
        )
        .expect("open PTY");

    let startup = read_until(&mut opened.reader, b"READY");
    assert_contains(&startup, b"ENV_OK");
    assert_contains(&startup, b"ARGV_OK");
    let expected_cwd = fs::canonicalize(temp.path()).expect("canonical test cwd");
    assert_contains(
        &startup,
        format!("CWD:{}", expected_cwd.display()).as_bytes(),
    );
    await_beating(&child_beat);
    await_beating(&grandchild_beat);

    let input = b"ECHO:exact bytes $() ; | &\n";
    let written = host.write(opened.id, input).expect("write PTY");
    assert_eq!(written, input.len(), "host did not write the exact slice");
    let expected_write = format!("WRITE_HEX:{}", hex(input));
    let echoed = read_until(&mut opened.reader, expected_write.as_bytes());
    assert_contains(&echoed, expected_write.as_bytes());

    let resized = PtySize {
        cols: 120,
        rows: 40,
    };
    host.resize(opened.id, resized).expect("resize PTY");
    host.write(opened.id, b"SIZE\n").expect("request size");
    let sized = read_until(&mut opened.reader, b"SIZE:120x40");
    assert_contains(&sized, b"SIZE:120x40");

    host.write(opened.id, b"BURST:70000\n")
        .expect("request bounded burst");
    let burst = read_until(&mut opened.reader, b"BURST_END");
    let longest_z_run = burst
        .split(|byte| *byte != b'Z')
        .map(<[u8]>::len)
        .max()
        .unwrap_or_default();
    assert!(
        longest_z_run >= 70_000,
        "lost or reordered burst bytes: longest run was {longest_z_run}"
    );

    let first_close =
        call_and_publish_return(&[&child_beat, &grandchild_beat], || host.close(opened.id))
            .expect("close PTY");
    assert_eq!(first_close, CloseOutcome::Closed);
    assert_eq!(
        host.close(opened.id).expect("repeat close"),
        CloseOutcome::AlreadyClosed
    );
    assert_stopped_at_return(&child_beat);
    assert_stopped_at_return(&grandchild_beat);

    assert_return_event_rejects_premature_return(&mut host, &executable, temp.path(), size);

    let congested_child = temp.path().join("congested.beat");
    let congested_grandchild = temp.path().join("congested-grandchild.beat");
    let mut congested = host
        .open(
            helper_spec(
                &executable,
                temp.path(),
                &congested_child,
                &congested_grandchild,
            ),
            size,
        )
        .expect("open congested PTY");
    read_until(&mut congested.reader, b"READY");
    await_beating(&congested_child);
    await_beating(&congested_grandchild);
    host.write(congested.id, b"BURST:1000000\n")
        .expect("fill bounded output queue");
    thread::sleep(Duration::from_millis(80));
    let congested_close =
        call_and_publish_return(&[&congested_child, &congested_grandchild], || {
            host.close(congested.id)
        })
        .expect("close congested PTY");
    assert_eq!(congested_close, CloseOutcome::Closed);
    assert_stopped_at_return(&congested_child);
    assert_stopped_at_return(&congested_grandchild);

    let natural_child = temp.path().join("natural.beat");
    let natural_grandchild = temp.path().join("natural-grandchild.beat");
    let mut natural = host
        .open(
            helper_spec(
                &executable,
                temp.path(),
                &natural_child,
                &natural_grandchild,
            ),
            size,
        )
        .expect("open natural-exit PTY");
    read_until(&mut natural.reader, b"READY");
    await_beating(&natural_child);
    await_beating(&natural_grandchild);
    host.write(natural.id, b"EXIT\n")
        .expect("request natural exit");
    read_to_eof(&mut natural.reader);
    let natural_close = call_and_publish_return(&[&natural_child, &natural_grandchild], || {
        host.close(natural.id)
    })
    .expect("clean natural exit");
    assert_eq!(
        natural_close,
        CloseOutcome::Closed,
        "the map owned the naturally exited session on its first close"
    );
    assert_eq!(
        host.close(natural.id).expect("repeat natural close"),
        CloseOutcome::AlreadyClosed
    );
    assert_stopped_at_return(&natural_child);
    assert_stopped_at_return(&natural_grandchild);

    let mut shutdown_beats = Vec::new();
    for index in 0..2 {
        let child = temp.path().join(format!("shutdown-{index}.beat"));
        let grandchild = temp
            .path()
            .join(format!("shutdown-{index}-grandchild.beat"));
        let opened = host
            .open(
                helper_spec(&executable, temp.path(), &child, &grandchild),
                size,
            )
            .expect("open shutdown PTY");
        let mut reader = opened.reader;
        let ready = read_until(&mut reader, b"READY");
        assert_contains(&ready, b"READY");
        await_beating(&child);
        await_beating(&grandchild);
        shutdown_beats.push((child, grandchild, reader));
    }
    let shutdown_paths: Vec<&Path> = shutdown_beats
        .iter()
        .flat_map(|(child, grandchild, _)| [child.as_path(), grandchild.as_path()])
        .collect();
    call_and_publish_return(&shutdown_paths, || host.shutdown()).expect("shutdown every PTY");
    for (child, grandchild, _reader) in shutdown_beats {
        assert_stopped_at_return(&child);
        assert_stopped_at_return(&grandchild);
    }
}

fn assert_return_event_rejects_premature_return(
    host: &mut PtyHost,
    executable: &Path,
    cwd: &Path,
    size: PtySize,
) {
    let child = cwd.join("premature-return-mutant.beat");
    let grandchild = cwd.join("premature-return-mutant-grandchild.beat");
    let mut opened = host
        .open(helper_spec(executable, cwd, &child, &grandchild), size)
        .expect("open premature-return control PTY");
    read_until(&mut opened.reader, b"READY");
    await_beating(&child);
    await_beating(&grandchild);

    let planted_result = call_and_publish_return(&[&child, &grandchild], || {
        Ok::<_, std::io::Error>(CloseOutcome::Closed)
    })
    .expect("planted premature return");
    assert_eq!(planted_result, CloseOutcome::Closed);
    await_after_return_heartbeat(&child);
    await_after_return_heartbeat(&grandchild);

    host.close(opened.id)
        .expect("clean up premature-return control PTY");
}

fn helper_spec(executable: &Path, cwd: &Path, child: &Path, grandchild: &Path) -> LaunchSpec {
    let mut argv = vec![
        executable.as_os_str().to_owned(),
        "--ignored".into(),
        "--exact".into(),
        "pty_helper_probe".into(),
        "--nocapture".into(),
    ];
    for value in LITERAL_ARGS {
        argv.push("--skip".into());
        argv.push(OsString::from(value));
    }
    let env = vec![
        EnvVar::new(HELPER_PROBE, "1"),
        EnvVar::new(EXPLICIT_ENV, EXACT_ENV_VALUE),
        EnvVar::new(CHILD_BEAT, child.as_os_str()),
        EnvVar::new(GRANDCHILD_BEAT, grandchild.as_os_str()),
    ];
    #[cfg(windows)]
    let env = {
        let mut env = env;
        env.push(EnvVar::new(
            "SystemRoot",
            std::env::var_os("SystemRoot").expect("Windows SystemRoot"),
        ));
        env
    };
    LaunchSpec {
        argv,
        env,
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    }
}

fn run_pty_helper() {
    let child_beat = PathBuf::from(std::env::var_os(CHILD_BEAT).expect("child beat path"));
    let grandchild_beat =
        PathBuf::from(std::env::var_os(GRANDCHILD_BEAT).expect("grandchild beat path"));

    let executable = std::env::current_exe().expect("current test executable");
    let mut grandchild = Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "pty_grandchild_probe",
            "--nocapture",
        ])
        .env_clear()
        .env(GRANDCHILD_PROBE, "1")
        .env(GRANDCHILD_BEAT, &grandchild_beat)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn immediate grandchild");
    thread::spawn(move || {
        let _ = grandchild.wait();
    });

    let heartbeat = child_beat.clone();
    thread::spawn(move || heartbeat_forever(&heartbeat));

    assert_eq!(std::env::var(EXPLICIT_ENV).as_deref(), Ok(EXACT_ENV_VALUE));
    assert!(std::env::var_os(AMBIENT_CANARY).is_none());
    #[cfg(unix)]
    assert!(std::env::var_os("SHELL").is_none());
    let args: Vec<OsString> = std::env::args_os().collect();
    for literal in LITERAL_ARGS {
        assert!(
            args.iter().any(|arg| arg == literal),
            "literal argv element missing: {literal:?}"
        );
    }

    let mut output = std::io::stdout().lock();
    writeln!(output, "ENV_OK").expect("environment marker");
    writeln!(output, "ARGV_OK").expect("argv marker");
    writeln!(
        output,
        "CWD:{}",
        std::env::current_dir().expect("cwd").display()
    )
    .expect("cwd marker");
    writeln!(output, "READY").expect("ready marker");
    output.flush().expect("flush ready");

    let input = std::io::stdin();
    let mut input = BufReader::new(input.lock());
    loop {
        let mut line = Vec::new();
        if input.read_until(b'\n', &mut line).expect("read PTY input") == 0 {
            return;
        }
        if line == b"SIZE\n" || line == b"SIZE\r\n" {
            #[cfg(unix)]
            {
                let (cols, rows) = terminal_size();
                writeln!(output, "SIZE:{cols}x{rows}").expect("size marker");
            }
            #[cfg(windows)]
            report_terminal_size();
        } else if line == b"EXIT\n" || line == b"EXIT\r\n" {
            return;
        } else if let Some(count) = line
            .strip_prefix(b"BURST:")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.trim().parse::<usize>().ok())
        {
            output.write_all(&vec![b'Z'; count]).expect("burst bytes");
            writeln!(output, "\nBURST_END").expect("burst marker");
        } else if line.starts_with(b"ECHO:") {
            writeln!(output, "WRITE_HEX:{}", hex(&line)).expect("write marker");
        }
        output.flush().expect("flush helper output");
    }
}

#[cfg(unix)]
fn terminal_size() -> (u16, u16) {
    let size = rustix::termios::tcgetwinsize(std::io::stdin()).expect("terminal size");
    (size.ws_col, size.ws_row)
}

#[cfg(windows)]
fn report_terminal_size() {
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let status = Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            r#"[Console]::WriteLine("SIZE:{0}x{1}", [Console]::WindowWidth, [Console]::WindowHeight)"#,
        ])
        .status()
        .expect("run terminal size probe");
    assert!(status.success(), "terminal size probe failed: {status}");
}

fn read_until(reader: &mut dudes_pty::PtyReader, marker: &[u8]) -> Vec<u8> {
    read_until_from(Vec::new(), reader, marker)
}

fn read_to_eof(reader: &mut dudes_pty::PtyReader) {
    loop {
        match reader.read().expect("read PTY to EOF") {
            PtyRead::Data(bytes) => assert!(bytes.len() <= MAX_READ_BYTES),
            PtyRead::Eof => return,
        }
    }
}

fn read_until_from(
    mut received: Vec<u8>,
    reader: &mut dudes_pty::PtyReader,
    marker: &[u8],
) -> Vec<u8> {
    loop {
        match reader.read().expect("read PTY output") {
            PtyRead::Data(bytes) => {
                assert!(bytes.len() <= MAX_READ_BYTES, "oversized PTY read");
                received.extend_from_slice(&bytes);
                if received
                    .windows(marker.len())
                    .any(|window| window == marker)
                {
                    return received;
                }
            }
            PtyRead::Eof => panic!("PTY reached EOF before marker {marker:?}: {received:?}"),
        }
    }
}

fn assert_contains(haystack: &[u8], needle: &[u8]) {
    assert!(
        haystack
            .windows(needle.len())
            .any(|window| window == needle),
        "missing {needle:?} in {haystack:?}"
    );
}

fn heartbeat_forever(path: &Path) -> ! {
    let gate_path = heartbeat_gate(path);
    let gate = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&gate_path)
        .expect("open heartbeat gate");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .expect("open heartbeat");
    let mut count = 0_u64;
    loop {
        gate.lock().expect("lock heartbeat gate");
        if return_event(path).exists() {
            fs::write(after_return_event(path), b"heartbeat after return")
                .expect("record heartbeat after return");
        }
        file.seek(SeekFrom::Start(0)).expect("seek heartbeat");
        write!(file, "{count:020}").expect("write heartbeat");
        file.flush().expect("flush heartbeat");
        gate.unlock().expect("unlock heartbeat gate");
        count = count.wrapping_add(1);
        thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Beat {
    count: u64,
}

fn heartbeat(path: &Path) -> Option<Beat> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    let mut fields = text.split_whitespace();
    Some(Beat {
        count: fields.next()?.parse().ok()?,
    })
}

fn await_beating(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = None;
    while Instant::now() < deadline {
        if let Some(current) = heartbeat(path) {
            if previous.is_some_and(|old: Beat| current.count > old.count) {
                return;
            }
            previous = Some(current);
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("heartbeat never advanced: {}", path.display());
}

fn call_and_publish_return<T>(beats: &[&Path], operation: impl FnOnce() -> T) -> T {
    let gates: Vec<File> = beats
        .iter()
        .map(|path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(heartbeat_gate(path))
                .expect("open established heartbeat gate")
        })
        .collect();
    for gate in &gates {
        gate.lock().expect("lock return-event heartbeat gate");
    }
    let result = operation();
    for path in beats {
        File::create(return_event(path)).expect("publish PTY method return event");
    }
    for gate in gates.iter().rev() {
        gate.unlock().expect("release return-event heartbeat gate");
    }
    result
}

fn assert_stopped_at_return(path: &Path) {
    let before = heartbeat(path).expect("heartbeat before stopped check");
    thread::sleep(Duration::from_millis(120));
    let after = heartbeat(path).expect("heartbeat after stopped check");
    assert_eq!(
        before,
        after,
        "heartbeat still advanced: {}",
        path.display()
    );
    assert!(
        !after_return_event(path).exists(),
        "heartbeat process observed the PTY method return event: {}",
        path.display()
    );
}

fn await_after_return_heartbeat(path: &Path) {
    let marker = after_return_event(path);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "premature-return mutant did not fire the heartbeat control: {}",
        path.display()
    );
}

fn heartbeat_gate(path: &Path) -> PathBuf {
    path.with_extension("heartbeat-gate")
}

fn return_event(path: &Path) -> PathBuf {
    path.with_extension("pty-method-returned")
}

fn after_return_event(path: &Path) -> PathBuf {
    path.with_extension("heartbeat-after-return")
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}
