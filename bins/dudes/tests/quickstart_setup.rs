//! Setup refusals and engine login commands through the real CLI.
//! The login checks use a local doctor reply and executable stand-ins.
#![cfg(unix)]

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

mod common;

use common::{Cli, Lifeline, run_program};

const SPAWN_MARKER: &str = "quickstart started duded pid ";
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Layout {
    root: tempfile::TempDir,
}

impl Layout {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a root");
        std::fs::create_dir_all(root.path().join("repo/.git")).unwrap();
        std::fs::create_dir(root.path().join("home")).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn cli(&self) -> Cli {
        Cli::bare()
            .env("HOME", self.path("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("DUDES_DAEMON", self.path("missing-duded"))
            .env("DUDES_STATE_DIR", self.path("state"))
            .env("DUDES_RUNTIME_DIR", self.path("run"))
            .env("SSH_TTY", "/dev/ttys-test")
            .current_dir(&self.path("repo"))
    }

    fn config(&self) -> PathBuf {
        let home = self.path("home").into_os_string();
        dudes::Environment::resolve(|name| (name == "HOME").then(|| home.clone()))
            .config()
            .unwrap()
            .to_path_buf()
    }

    fn socket(&self) -> PathBuf {
        self.path("run/duded.sock")
    }

    fn daemons(&self) -> Vec<i32> {
        std::fs::read_to_string(self.path("run/duded.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(SPAWN_MARKER)?.parse().ok())
            .collect()
    }
}

const LIFELINE_REPORT: &str = "DUDES_TEST_LIFELINE_REPORT";

#[test]
fn lifeline_victim() {
    use std::os::unix::process::CommandExt as _;
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let Some(report) = std::env::var_os(LIFELINE_REPORT) else {
        return;
    };
    let root = tempfile::tempdir().expect("a daemon root");
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let mut built = PathBuf::from(env!("CARGO_BIN_EXE_dudes"));
    built.set_file_name("duded");
    let program = bin.join("duded");
    std::fs::hard_link(&built, &program)
        .or_else(|_| std::fs::copy(&built, &program).map(|_| ()))
        .expect("install the lifeline test daemon");
    let _lifeline = Lifeline::start(root.path());
    let env = std::collections::BTreeMap::from([
        ("HOME".into(), root.path().join("home").into_os_string()),
        (
            "DUDES_STATE_DIR".into(),
            root.path().join("state").into_os_string(),
        ),
        (
            "DUDES_RUNTIME_DIR".into(),
            root.path().join("run").into_os_string(),
        ),
        ("PATH".into(), "/usr/bin:/bin".into()),
    ]);
    let child = common::process(program.as_os_str(), &[], Some(&env))
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start the detached test daemon");
    let mut child = OwnedChild(child);
    let pid = child.0.id();
    std::fs::write(
        root.path().join("duded.log"),
        format!("{SPAWN_MARKER}{pid}\n"),
    )
    .unwrap();
    let socket = root.path().join("run/duded.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(child.0.try_wait().unwrap().is_none(), "the daemon exited");
        if dudes_rpc::Client::connect(&socket)
            .and_then(|mut client| {
                client
                    .call(dudes_proto::rpc::Method::SystemPing, None)
                    .map_err(std::io::Error::other)
            })
            .is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline, "the daemon never listened");
        std::thread::sleep(Duration::from_millis(5));
    }
    let record = format!("{pid}\n{}\n", root.path().display());
    std::fs::write(Path::new(&report).with_extension("tmp"), record).expect("a report");
    std::fs::rename(Path::new(&report).with_extension("tmp"), &report).expect("a report");
    std::thread::sleep(Duration::from_secs(60));
}

/// A test the runner kills never runs its destructors, and its daemon has left the group the
/// runner kills. The daemon still ends.
#[test]
fn a_daemon_ends_when_the_test_that_started_it_is_killed() {
    if std::env::var_os(LIFELINE_REPORT).is_some() {
        return;
    }
    let scratch = tempfile::tempdir().expect("a report directory");
    let report = scratch.path().join("report");
    let args = [
        "--exact".into(),
        "lifeline_victim".into(),
        "--test-threads=1".into(),
    ];
    let mut victim = common::process(
        std::env::current_exe()
            .expect("this test binary")
            .as_os_str(),
        &args,
        None,
    )
    .env(LIFELINE_REPORT, &report)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("start the victim");
    // Just under the runner's 10 s limit: a loaded machine can take most of it to start the victim.
    let deadline = Instant::now() + Duration::from_secs(9);
    while !report.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let recorded = std::fs::read_to_string(&report).unwrap_or_default();
    let mut lines = recorded.lines();
    let pid: i32 = lines.next().and_then(|line| line.parse().ok()).unwrap_or(0);
    let root = lines.next().map(PathBuf::from);
    victim.kill().expect("kill the victim");
    victim.wait().expect("reap the victim");
    assert!(
        pid > 1 && root.is_some(),
        "the victim did not start its daemon in time, so this run proves nothing: {recorded:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive(pid);
    if survived {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    if let Some(root) = root {
        let _ = std::fs::remove_dir_all(root);
    }
    assert!(!survived, "the daemon outlived its killed test");
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// A script whose settings break a start rule is refused whole: the rule is named, and nothing
/// is written or started.
#[test]
fn a_script_the_daemon_would_refuse_is_refused_whole() {
    let layout = Layout::new();
    let run = layout
        .cli()
        .args(["quickstart", "--set", "floor.control", "off"])
        .run();

    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert_eq!(
        run.stderr,
        "dudes: the daemon would refuse this configuration when it starts, so nothing was \
         written: floor.goal on needs floor.control on; floor.retire on needs floor.control on; \
         floor.board on needs floor.control on\n"
    );
    assert!(!layout.config().exists(), "nothing was written");
    assert!(!layout.path("run").exists(), "nothing was started");
}

/// A daemon of another version may compare settings this CLI does not know, so a script counts
/// the version as a difference. With an agent running a turn it stops nothing, names the agent and
/// prints the commands that restart it.
#[test]
fn a_busy_daemon_of_another_version_is_not_restarted() {
    let layout = Layout::new();
    std::fs::create_dir(layout.path("run")).expect("a runtime directory");
    std::fs::set_permissions(layout.path("run"), std::fs::Permissions::from_mode(0o700))
        .expect("an owner-only runtime directory");
    common::answer_as(
        &layout.socket(),
        "0.0.1",
        Some(serde_json::json!([{
            "id": "01JBQ8XK2M3N4P5Q6R7S8T9VWX", "name": "Ada", "role": "engineer",
            "department": "engineering", "provider": "claude", "status": "running",
            "mode": "managed",
        }])),
    );

    let unasked = layout
        .cli()
        .args(["quickstart", "--set", "floor.answer", "on"])
        .run();
    assert_eq!(unasked.code(), dudes::exit::OTHER, "{unasked:?}");
    assert_eq!(
        unasked.stderr,
        format!(
            "dudes: wrote {}, but the running daemon reads other settings (the running daemon is \
             0.0.1, this CLI is {VERSION}) only when it starts, and it was not restarted because \
             Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is running a turn. When they are done, restart it \
             with `dudes stop`, then `dudes start`. To restart it now and end their turns: dudes \
             quickstart --restart\n",
            layout.config().display()
        )
    );
    assert!(layout.daemons().is_empty(), "nothing was started");
}

/// A pseudo-terminal's leader side: everything the program wrote, and a way to type.
struct Screen {
    seen: Vec<u8>,
    cursor: usize,
    chunks: Receiver<Vec<u8>>,
}

impl Screen {
    fn new(leader: OwnedFd) -> Self {
        let mut reader = File::from(leader);
        let (sender, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(read @ 1..) = reader.read(&mut buffer) {
                if sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            seen: Vec::new(),
            cursor: 0,
            chunks,
        }
    }

    /// Wait until `text` appears after the last thing waited for.
    fn expect(&mut self, text: &str) {
        self.expect_any(&[text]);
    }

    fn expect_any(&mut self, texts: &[&str]) -> usize {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for (index, text) in texts.iter().enumerate() {
                if let Some(at) = self.seen[self.cursor..]
                    .windows(text.len())
                    .position(|window| window == text.as_bytes())
                {
                    self.cursor += at + text.len();
                    return index;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(error) => panic!(
                    "{texts:?} did not appear ({error}); the terminal shows:\n{}",
                    String::from_utf8_lossy(&self.seen)
                ),
            }
        }
    }

    /// Read what the program wrote before it exited.
    fn drain(&mut self) {
        while let Ok(chunk) = self.chunks.recv_timeout(Duration::from_millis(300)) {
            self.seen.extend(chunk);
        }
    }

    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.seen).replace("\r\n", "\n")
    }
}

fn built_daemon() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dudes")).with_file_name("duded")
}

/// The nine Floor actions a first run turns on, and the three switches it leaves off.
fn assert_first_run_floor(config: &Path) {
    let file: toml_edit::DocumentMut = std::fs::read_to_string(config)
        .expect("quickstart wrote the file")
        .parse()
        .expect("the file is TOML");
    let enabled = |name: &str| file.get("floor")?.get(name)?.get("enabled")?.as_bool();
    for name in [
        "answer", "hire", "control", "goal", "retire", "board", "renew", "recovery", "project",
    ] {
        assert_eq!(enabled(name), Some(true), "floor.{name} in {file}");
    }
    for name in ["git", "diff", "terminal"] {
        assert_ne!(enabled(name), Some(true), "floor.{name} in {file}");
    }
    assert!(
        file.get("floor")
            .and_then(|floor| floor.get("onboarding"))
            .is_none(),
        "a project was enrolled: {file}"
    );
}

fn stop(layout: &Layout) {
    let stopped = layout.cli().args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
}

/// A newcomer's `dudes quickstart` on a terminal asks nothing: it writes the nine Floor actions,
/// enrolls no project, hires no agent, starts the daemon and ends with the Floor link.
#[test]
fn quickstart_on_a_terminal_asks_nothing_and_sets_the_floor_up() {
    let layout = Layout::new();
    let _lifeline = Lifeline::start(layout.root.path());
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .expect("a pseudo-terminal");
    let mut child = layout
        .cli()
        .env("DUDES_DAEMON", built_daemon())
        .arg("quickstart")
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    let status = exit_status(&mut child, &screen);
    screen.drain();
    let shown = screen.shown();
    assert_eq!(status.code(), Some(0), "{shown}");
    for asked in ["[Y/n]", "[y/N]", "Enter", "go ahead", "Project folder"] {
        assert!(
            !shown.contains(asked),
            "quickstart asked `{asked}`:\n{shown}"
        );
    }
    assert!(
        shown.contains(&format!("Wrote {}:", layout.config().display())),
        "{shown}"
    );
    assert!(
        shown.contains("The changed-file views (floor.git, floor.diff) are off."),
        "{shown}"
    );
    assert!(
        shown.contains("no Floor link: the Floor is not configured"),
        "{shown}"
    );
    assert_first_run_floor(&layout.config());
    let agents = layout.cli().arg("agents").run();
    assert_eq!(agents.code(), dudes::exit::SUCCESS, "{agents:?}");
    assert!(!agents.stdout.contains("engineer"), "{agents:?}");
    stop(&layout);
}

/// `--yes` gives the same file as a plain run. A second run adopts a Floor switch in the running
/// daemon without a restart, and restarts the daemon for a start-time setting only because no
/// agent is running a turn.
#[test]
fn yes_matches_a_plain_run_and_a_rerun_applies_the_change() {
    let layout = Layout::new();
    let _lifeline = Lifeline::start(layout.root.path());
    let cli = || layout.cli().env("DUDES_DAEMON", built_daemon());
    let first = cli().args(["quickstart", "--yes"]).run();
    assert_eq!(first.code(), dudes::exit::SUCCESS, "{first:?}");
    assert_first_run_floor(&layout.config());
    let daemon = layout.daemons();
    assert_eq!(daemon.len(), 1, "{daemon:?}");

    let live = cli()
        .args(["quickstart", "--yes", "--set", "floor.git", "on"])
        .run();
    assert_eq!(live.code(), dudes::exit::SUCCESS, "{live:?}");
    assert!(
        live.stdout
            .contains("The running daemon now runs floor.git, with no restart."),
        "{live:?}"
    );
    assert_eq!(
        layout.daemons(),
        daemon,
        "a Floor switch restarted the daemon"
    );

    let restarted = cli()
        .args(["quickstart", "--yes", "--set", "providers.codex", "on"])
        .run();
    assert_eq!(restarted.code(), dudes::exit::SUCCESS, "{restarted:?}");
    assert!(
        restarted
            .stdout
            .contains("Restarting the daemon so it reads"),
        "{restarted:?}"
    );
    assert_eq!(layout.daemons().len(), 2, "{:?}", layout.daemons());
    stop(&layout);
}

fn exit_status(child: &mut std::process::Child, screen: &Screen) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the command did not exit:\n{}", screen.shown());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn dudes_login_without_a_terminal_prints_the_command_and_runs_nothing() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("claude");
    let marker = root.path().join("login-ran");
    std::fs::write(
        &program,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"claude", "command":program, "ok":true, "sign_in":"signed_out"
        }),
    );
    let printed = cli.args(["login", "claude"]).run();
    server.join().unwrap();
    assert_eq!(printed.code(), dudes::exit::SUCCESS, "{printed:?}");
    assert_eq!(
        printed.stdout,
        format!(
            "Sign in to Claude by running this in a terminal: {} auth login\n",
            program.display()
        )
    );
    assert!(!marker.exists(), "a nonterminal login ran the engine");
}

#[test]
fn dudes_login_on_a_terminal_replaces_itself_with_the_login() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("claude");
    std::fs::write(&program, "#!/bin/sh\n[ -t 0 ] && [ -t 1 ] || exit 11\n[ \"$1\" = auth ] && [ \"$2\" = login ] || exit 12\nexit 7\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"claude", "command":program, "ok":true, "sign_in":"signed_out"
        }),
    );
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let mut child = cli.args(["login", "claude"]).spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect(&format!("Running {} auth login.", program.display()));
    assert_eq!(
        exit_status(&mut child, &screen).code(),
        Some(7),
        "{}",
        screen.shown()
    );
    server.join().unwrap();
}

fn answer_login_doctor(
    root: &Path,
    provider: serde_json::Value,
) -> (Cli, std::thread::JoinHandle<()>) {
    use std::io::BufRead as _;
    let socket = root.join("doctor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let client_home = root.join("client-home");
    std::fs::create_dir(&client_home).unwrap();
    let ping = serde_json::json!({"version": VERSION, "socket": socket, "lsn": 0});
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut request)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "system.doctor");
        let reply = serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"ping":ping,"providers":[provider]}});
        writeln!(&stream, "{reply}").unwrap();
    });
    (Cli::new(&socket, root).env("HOME", client_home), server)
}

fn assert_managed_login_arguments(arguments: &[&str], home: &Path) {
    assert_eq!(arguments.len(), 7);
    assert_eq!(
        &arguments[..5],
        ["--disable", "hooks", "--disable", "plugins", "-c"]
    );
    assert_eq!(arguments[6], "login");
    let config: toml_edit::DocumentMut = arguments[5].parse().unwrap();
    let projects = config["projects"].as_inline_table().unwrap();
    assert_eq!(projects.len(), home.ancestors().count());
    for ancestor in home.ancestors() {
        let entry = projects
            .get(ancestor.to_str().unwrap())
            .unwrap()
            .as_inline_table()
            .unwrap();
        assert_eq!(
            entry.get("trust_level").unwrap().as_str(),
            Some("untrusted")
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn managed_codex_login_uses_the_daemons_home() {
    codex_login_environment(true);
}

#[test]
fn legacy_codex_login_preserves_the_client_environment() {
    codex_login_environment(false);
}

fn codex_login_environment(managed: bool) {
    let provider = "codex";
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("Engine O'Brien;$HOME");
    std::fs::write(&program, "#!/bin/sh\n{ printf '%s\\n' \"$CODEX_HOME\" \"${DUDES_TOKEN-unset}\" \"${DUDES_SOCK-unset}\"; pwd -P; printf '%s\\n' \"$@\"; } > \"$LOGIN_RESULT\"\nexit 7\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let managed_home = root.path().join("daemon sign-in's;$HOME");
    std::fs::create_dir(&managed_home).unwrap();
    let managed_home = managed_home.canonicalize().unwrap();
    let client_home = root.path().join("client-codex-home");
    let login_home = managed.then(|| serde_json::json!({"state":"ready", "path":managed_home}));
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":provider, "command":program, "ok":true, "sign_in":"signed_out", "login_home":login_home
        }),
    );
    let pty = nix::pty::openpty(
        None::<&nix::pty::Winsize>,
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let result = root.path().join("login-result");
    let mut child = cli
        .env("CODEX_HOME", &client_home)
        .env("DUDES_TOKEN", "not-an-agent")
        .env("LOGIN_RESULT", &result)
        .args(["login", provider])
        .spawn_on_terminal(&pty.slave);
    drop(pty.slave);
    let mut screen = Screen::new(pty.master);
    screen.expect("Running ");
    assert_eq!(
        exit_status(&mut child, &screen).code(),
        Some(7),
        "{}",
        screen.shown()
    );
    server.join().unwrap();
    let written = std::fs::read_to_string(result).unwrap();
    let lines: Vec<_> = written.lines().collect();
    let expected_home = if managed { &managed_home } else { &client_home };
    assert_eq!(lines[0], expected_home.to_str().unwrap());
    assert_eq!(&lines[1..3], ["unset", "unset"]);
    let expected_cwd = if managed {
        managed_home.clone()
    } else {
        root.path().join("client-home").canonicalize().unwrap()
    };
    assert_eq!(lines[3], expected_cwd.to_str().unwrap());
    if managed {
        assert_managed_login_arguments(&lines[4..], &managed_home);
    } else if provider == "claude" {
        assert_eq!(&lines[4..], ["auth", "login"]);
    } else {
        assert_eq!(&lines[4..], ["login"]);
    }
}

#[test]
fn codex_login_refuses_unavailable_or_invalid_home_even_when_signed_in() {
    for login_home in [
        serde_json::json!({"state":"unavailable","reason":"managed profile unavailable"}),
        serde_json::json!({"state":"ready","path":"relative-home"}),
        serde_json::json!({"state":"ready","path":"/tmp/../other-home"}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (cli, server) = answer_login_doctor(
            root.path(),
            serde_json::json!({
                "provider":"codex", "command":"/unused/codex", "ok":true, "sign_in":"signed_in", "login_home":login_home
            }),
        );
        let result = cli.args(["login", "codex"]).run();
        server.join().unwrap();
        assert_eq!(result.code(), dudes::exit::OTHER, "{result:?}");
        assert!(
            !result.stdout.contains("signed in; nothing was run"),
            "{result:?}"
        );
    }
}

#[test]
fn legacy_signed_in_login_keeps_the_no_run_shortcut_without_a_home_report() {
    let root = tempfile::tempdir().unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"codex", "ok":false, "sign_in":"signed_in"
        }),
    );
    let result = cli.args(["login", "codex"]).run();
    server.join().unwrap();
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    assert_eq!(
        result.stdout,
        "The daemon reports Codex as signed in; nothing was run.\n"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn printed_codex_login_command_preserves_the_literal_program_and_home() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("Codex O'Brien;$(false)");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\"\npwd -P\nprintf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let managed_home = root.path().join("daemon O'Brien;$(false)");
    std::fs::create_dir(&managed_home).unwrap();
    let managed_home = managed_home.canonicalize().unwrap();
    let (cli, server) = answer_login_doctor(
        root.path(),
        serde_json::json!({
            "provider":"codex", "command":program, "ok":true, "sign_in":"signed_out",
            "login_home":{"state":"ready","path":managed_home}
        }),
    );
    let result = cli.args(["login", "codex"]).run();
    server.join().unwrap();
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    let command = result
        .stdout
        .strip_prefix("Sign in to Codex by running this in a terminal: ")
        .unwrap()
        .trim_end();
    let executed = run_program(Path::new("/bin/sh"), &["-c".into(), command.into()]);
    assert_eq!(executed.code(), 0, "{executed:?}");
    let lines: Vec<_> = executed.stdout.lines().collect();
    assert_eq!(
        &lines[..2],
        [
            managed_home.to_str().unwrap(),
            managed_home.to_str().unwrap()
        ]
    );
    assert_managed_login_arguments(&lines[2..], &managed_home);
}
