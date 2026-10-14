//! `dudes uninstall`, run as the real binary against scratch directories.
//!
//! Every run names `HOME`, the state and runtime directories and the operator file inside a fresh
//! temporary directory, and runs a copy of the binary installed in a scratch prefix, so the
//! programs it removes are the copy's neighbours and never the build's.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use dudes::exit;
use dudes_proto::rpc::agent::HireResult;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

struct Scratch {
    root: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    bin: PathBuf,
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Scratch {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a scratch directory");
        let base = root.path().canonicalize().expect("a real path");
        let home = base.join("home");
        let state = base.join("state");
        let runtime = base.join("runtime");
        let config = base.join("configs/dudes.toml");
        let bin = base.join("prefix/bin");
        for dir in [
            &home,
            &state.join("journal"),
            &runtime,
            &bin,
            config.parent().unwrap(),
        ] {
            std::fs::create_dir_all(dir).expect("a directory");
        }
        std::fs::write(
            runtime.join("duded.log"),
            "quickstart started duded pid 1\n",
        )
        .expect("a log");
        std::fs::write(state.join("LOCK"), "").expect("a lock");
        let mut header = vec![b'r', b'e', b'd', b'b', 0x1A, 0x0A, 0xA9, 0x0D, 0x0A];
        header.extend_from_slice(&[0; 8]);
        std::fs::write(state.join("state.redb"), header).expect("a projection");
        std::fs::create_dir_all(base.join("prefix/share/dudes")).expect("share");
        for name in ["LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY.md"] {
            std::fs::write(base.join("prefix/share/dudes").join(name), "x").expect("a notice");
        }
        std::fs::write(&config, "[floor]\n").expect("a config");
        std::fs::copy(env!("CARGO_BIN_EXE_dudes"), bin.join("dudes")).expect("the installed dudes");
        std::fs::write(bin.join("duded"), "x").expect("duded");
        std::fs::write(bin.join("dudes-hook"), "x").expect("dudes-hook");
        Self {
            root,
            home,
            state,
            runtime,
            config,
            bin,
        }
    }

    fn env(&self) -> BTreeMap<OsString, OsString> {
        [
            ("HOME", &self.home),
            ("DUDES_STATE_DIR", &self.state),
            ("DUDES_RUNTIME_DIR", &self.runtime),
            ("DUDES_CONFIG", &self.config),
        ]
        .into_iter()
        .map(|(name, path)| (OsString::from(name), path.as_os_str().to_owned()))
        .collect()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(&self.env(), args)
    }

    fn run_with(&self, env: &BTreeMap<OsString, OsString>, args: &[&str]) -> Output {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut command = common::process(self.bin.join("dudes").as_os_str(), &args, Some(env));
        command.stdin(std::process::Stdio::null());
        let output = command.output().expect("run the installed dudes");
        Output {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn untouched(&self) -> bool {
        [&self.state, &self.runtime, &self.config]
            .into_iter()
            .chain(
                [
                    self.bin.join("dudes"),
                    self.bin.join("duded"),
                    self.bin.join("dudes-hook"),
                ]
                .iter()
                .collect::<Vec<_>>(),
            )
            .all(|path| path.exists())
    }
}

#[test]
fn without_a_terminal_and_without_yes_nothing_is_deleted() {
    let scratch = Scratch::new();
    let run = scratch.run(&["uninstall"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert_eq!(
        run.stderr,
        "dudes: uninstall needs --yes when it is not run on a terminal; nothing was deleted\n"
    );
    assert!(run.stdout.starts_with("Dudes will remove:\n"));
    assert!(scratch.untouched());
}

#[test]
fn a_dry_run_prints_the_exact_plan_and_deletes_nothing() {
    let scratch = Scratch::new();
    let run = scratch.run(&["uninstall", "--dry-run"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let program_size = std::fs::metadata(scratch.bin.join("dudes")).unwrap().len();
    let prefix = scratch.bin.parent().unwrap().display().to_string();
    let only = "only the files Dudes writes there";
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines[0], "Dudes will remove:");
    assert_eq!(
        lines[1],
        format!(
            "  runtime directory           {}  (31 B, {only})",
            scratch.runtime.display()
        )
    );
    assert_eq!(
        lines[2],
        format!(
            "  state directory             {}  (17 B, {only})",
            scratch.state.display()
        )
    );
    assert_eq!(
        lines[3],
        format!(
            "  operator file               {}  (8 B)",
            scratch.config.display()
        )
    );
    assert_eq!(
        lines[4],
        format!("  installed Floor and notices {prefix}/share/dudes  (3 B, {only})")
    );
    assert_eq!(
        lines[5],
        format!("  program                     {prefix}/bin/duded  (1 B)")
    );
    assert!(
        lines[6].starts_with(&format!(
            "  program                     {prefix}/bin/dudes  ("
        )),
        "{}",
        lines[6]
    );
    assert!(program_size > 1024);
    assert_eq!(
        lines[7],
        format!("  program                     {prefix}/bin/dudes-hook  (1 B)")
    );
    assert_eq!(lines[9], "This was a dry run. Nothing was deleted.");
    assert!(scratch.untouched());
}

#[test]
fn a_planted_worktree_refuses_and_deletes_nothing() {
    let scratch = Scratch::new();
    let repository = scratch.root.path().canonicalize().unwrap().join("repo");
    let admin = repository.join(".git/worktrees/01TREE");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/dudes/0123456789ab\n").unwrap();
    let tree = scratch.state.join("worktrees/01TREE");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

    let run = scratch.run(&["uninstall", "--yes"]);

    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert!(run.stderr.contains(&format!(
        "git -C '{}' status --short --ignored\n",
        tree.display()
    )));
    assert!(run.stderr.contains(&format!(
        "git -C '{}' worktree remove '{}'\n",
        repository.display(),
        tree.display()
    )));
    assert!(run.stderr.contains(&format!(
        "git -C '{}' branch -d 'dudes/0123456789ab'\n",
        repository.display()
    )));
    assert!(scratch.untouched());
    assert!(tree.exists());
}

#[test]
fn yes_removes_everything_and_leaves_the_config_folder() {
    let scratch = Scratch::new();
    let sibling = scratch.config.parent().unwrap().join("other.toml");
    std::fs::write(&sibling, "mine").unwrap();
    let run = scratch.run(&["uninstall", "--yes"]);
    assert_eq!(run.code, Some(0), "{}{}", run.stdout, run.stderr);
    for gone in [
        &scratch.state,
        &scratch.runtime,
        &scratch.config,
        &scratch.bin.join("dudes"),
        &scratch.bin.join("duded"),
        &scratch.bin.join("dudes-hook"),
    ] {
        assert!(!gone.exists(), "{} should be gone", gone.display());
    }
    assert!(
        sibling.exists(),
        "DUDES_CONFIG names a file, so only the file goes"
    );
    assert!(scratch.bin.is_dir());
    assert!(
        run.stdout.contains("\nDudes is uninstalled ("),
        "{}",
        run.stdout
    );
}

#[test]
fn an_agents_token_refuses() {
    let scratch = Scratch::new();
    let mut env = scratch.env();
    env.insert(
        OsString::from("DUDES_TOKEN"),
        OsString::from("t".repeat(dudes_proto::rpc::hook::HOOK_TOKEN_LEN)),
    );
    let run = scratch.run_with(&env, &["uninstall", "--yes"]);
    assert_eq!(run.code, Some(3));
    assert_eq!(
        run.stderr,
        "dudes: uninstall is for the operator and refuses to run with an agent's DUDES_TOKEN set\n"
    );
    assert!(scratch.untouched());
}

#[test]
fn json_is_refused() {
    let scratch = Scratch::new();
    let run = scratch.run(&["--json", "uninstall", "--yes"]);
    assert_eq!(run.code, Some(i32::from(exit::USAGE)));
    assert!(
        run.stderr
            .starts_with("dudes: `uninstall` prints text only"),
        "{}",
        run.stderr
    );
    assert!(scratch.untouched());
}

/// A running daemon with a busy agent refuses before stopping or deleting anything.
#[test]
fn a_busy_agent_refuses_and_changes_nothing() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let _running = start(&settings, host.clone(), Arc::new(OpenRunner::default()));
    let socket = settings.socket();
    let cli = || Cli::new(&socket, &settings.runtime_dir);

    let hired = cli()
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    let agent = hired.json::<HireResult>().agent;
    let set = cli()
        .arg("--json")
        .args(common::goal_args(&agent.to_string(), "make the tests pass"))
        .run();
    assert_eq!(set.code(), exit::SUCCESS, "{}", set.output());
    fixture.await_record("TurnIssued", |record| {
        matches!(record, dudes_proto::Record::TurnIssued { .. }).then_some(())
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.children() == 0 {
        assert!(std::time::Instant::now() < deadline, "no turn started");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let scratch = Scratch::new();
    let mut env = scratch.env();
    env.insert(
        OsString::from("DUDES_STATE_DIR"),
        settings.state_dir.as_os_str().to_owned(),
    );
    env.insert(
        OsString::from("DUDES_RUNTIME_DIR"),
        settings.runtime_dir.as_os_str().to_owned(),
    );
    env.insert(OsString::from("DUDES_SOCK"), socket.as_os_str().to_owned());

    let refused = scratch.run_with(&env, &["uninstall", "--yes"]);
    assert_eq!(
        refused.code,
        Some(3),
        "{}{}",
        refused.stdout,
        refused.stderr
    );
    assert!(
        refused.stderr.starts_with(&format!(
            "dudes: refusing to uninstall: these agents are busy: Hector ({agent}) is running a turn. "
        )),
        "{}",
        refused.stderr
    );
    assert!(settings.state_dir.join("journal").exists());
    assert!(scratch.bin.join("dudes").exists());
}
