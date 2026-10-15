use std::fs::File;
use std::io::Cursor;

use super::*;
use crate::lifecycle::fake::Fake;

impl Owned for Fake {
    fn worktrees(&self) -> Result<Vec<PathBuf>, String> {
        if self.list_fails
            || (self.gone_after_stop && self.events.borrow().contains(&"stop".to_owned()))
        {
            return Err("unauthorized".to_owned());
        }
        Ok(self.listed.clone())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    layout: Layout,
}

fn redb_header() -> Vec<u8> {
    let mut bytes = REDB_MAGIC.to_vec();
    bytes.extend_from_slice(&[0; 32]);
    bytes
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a scratch directory");
        let base = root.path().canonicalize().expect("a real path");
        let home = base.join("home");
        let state = home.join(".local/share/dudes");
        let runtime = home.join(".local/state/dudes");
        let config_folder = home.join("config/dudes");
        let bin = home.join(".local/bin");
        for dir in [&state.join("journal"), &runtime, &config_folder, &bin] {
            std::fs::create_dir_all(dir).expect("a directory");
        }
        std::fs::write(state.join("journal/segment"), b"12345").expect("a segment");
        for name in NOTICES {
            std::fs::write(state.join(name), "").expect("a notice");
        }
        std::fs::write(runtime.join("duded.log"), b"log").expect("a log");
        std::fs::write(config_folder.join("config.toml"), b"[floor]\n").expect("a config");
        for name in PROGRAMS {
            std::fs::write(bin.join(name), vec![0_u8; 2048]).expect("a program");
        }
        let layout = Layout {
            home,
            state_dir: state,
            runtime_dir: runtime,
            config: config_folder.join("config.toml"),
            config_overridden: false,
            executable: bin.join("dudes"),
            lock_wait: Duration::from_millis(300),
            codex_home: None,
        };
        Self { root, layout }
    }

    fn base(&self) -> PathBuf {
        self.root.path().canonicalize().expect("a real path")
    }

    fn bin(&self) -> PathBuf {
        self.layout.executable.parent().expect("bin").to_path_buf()
    }

    fn run(
        &self,
        options: UninstallOptions,
        input: Option<&str>,
        daemon: &Fake,
    ) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut cursor = input.map(|text| Cursor::new(text.as_bytes().to_vec()));
        let terminal = cursor.as_mut().map(|cursor| cursor as &mut dyn BufRead);
        let code = run(&self.layout, options, terminal, daemon, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("text"),
            String::from_utf8(err).expect("text"),
        )
    }

    fn everything_exists(&self) -> bool {
        let l = &self.layout;
        [&l.state_dir, &l.runtime_dir, &l.config]
            .into_iter()
            .chain(
                PROGRAMS
                    .map(|name| self.bin().join(name))
                    .iter()
                    .collect::<Vec<_>>(),
            )
            .all(|path| path.exists())
    }

    /// A state directory a setting chose, with what the daemon writes in it.
    fn custom_state(&mut self) -> PathBuf {
        let custom = self.base().join("custom-state");
        std::fs::create_dir_all(custom.join("journal")).unwrap();
        std::fs::write(custom.join("journal/segment"), "x").unwrap();
        std::fs::write(custom.join(LOCK_NAME), "").unwrap();
        std::fs::write(custom.join("state.redb"), redb_header()).unwrap();
        self.layout.state_dir = custom.clone();
        custom
    }
}

fn yes() -> UninstallOptions {
    UninstallOptions {
        yes: true,
        ..UninstallOptions::default()
    }
}

fn dry() -> UninstallOptions {
    UninstallOptions {
        dry_run: true,
        ..UninstallOptions::default()
    }
}

fn running() -> Fake {
    Fake {
        running: true,
        ..Fake::default()
    }
}

#[test]
fn the_plan_lists_every_path_with_its_size() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.run(dry(), None, &Fake::default());
    let l = &fixture.layout;
    let bin = fixture.bin();
    let expected = format!(
        "Dudes will remove:\n\
         \x20 runtime directory           {runtime}  (3 B)\n\
         \x20 state directory             {state}  (5 B)\n\
         \x20 operator file folder        {config}  (8 B, only the files Dudes writes there)\n\
         \x20 program                     {bin}/duded  (2.0 KiB)\n\
         \x20 program                     {bin}/dudes  (2.0 KiB)\n\
         \x20 program                     {bin}/dudes-hook  (2.0 KiB)\n\
         \n\
         This was a dry run. Nothing was deleted.\n",
        runtime = l.runtime_dir.display(),
        state = l.state_dir.display(),
        config = l.config.parent().unwrap().display(),
        bin = bin.display(),
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (exit::SUCCESS, expected.as_str(), "")
    );
    assert!(fixture.everything_exists());
}

fn plant_tree(fixture: &Fixture) -> (PathBuf, PathBuf) {
    let repository = fixture.base().join("repo");
    let admin = repository.join(".git/worktrees/01TREE");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/dudes/abc123def456\n").unwrap();
    let tree = fixture.layout.state_dir.join("worktrees/01TREE");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    (tree, repository)
}

#[test]
fn a_worktree_refuses_everything_and_prints_the_git_commands() {
    let fixture = Fixture::new();
    let (tree, repository) = plant_tree(&fixture);
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    let expected = format!(
        "dudes: refusing to uninstall: 1 agent worktree exists. A tree can hold the only copy of an agent's work, so it was not deleted.\n\
         \n\
         \x20 {tree}\n\
         \x20   repository: {repository}\n\
         \x20   git -C '{tree}' status --short --ignored\n\
         \x20   git -C '{repository}' worktree remove '{tree}'\n\
         \x20   git -C '{repository}' branch -d 'dudes/abc123def456'\n\
         \n\
         Review each tree for uncommitted work (?? untracked, M modified, !! ignored files that removal deletes), commit or copy out what you want to keep, remove it with the commands above, then run dudes uninstall again.\n",
        tree = tree.display(),
        repository = repository.display(),
    );
    assert_eq!((code, err), (exit::REFUSED, expected));
    assert!(fixture.everything_exists() && tree.exists());
}

#[test]
fn a_tree_without_a_readable_pointer_still_refuses() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.layout.state_dir.join("worktrees/01LOST")).unwrap();
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("repository: unknown (this tree's .git file is missing or unreadable)"));
    assert!(fixture.everything_exists());
}

#[test]
fn a_tree_only_the_daemon_lists_also_refuses_and_a_listing_error_is_a_refusal() {
    let fixture = Fixture::new();
    let elsewhere = fixture.base().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let daemon = Fake {
        listed: vec![elsewhere.clone(), fixture.base().join("never-created")],
        ..running()
    };
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains(&format!("  {}\n", elsewhere.display())));
    assert!(!err.contains("never-created"));
    assert!(!daemon.events.borrow().contains(&"stop".to_owned()));

    let broken = Fake {
        list_fails: true,
        ..running()
    };
    let (code, _, err) = fixture.run(yes(), None, &broken);
    assert_eq!(code, exit::REFUSED);
    assert!(
        err.contains(
            "could not ask the daemon for its worktrees (unauthorized); nothing was deleted"
        ),
        "{err}"
    );
    assert!(fixture.everything_exists());
}

#[test]
fn a_stopped_daemon_is_not_asked_again_so_the_delete_proceeds() {
    let fixture = Fixture::new();
    let daemon = Fake {
        gone_after_stop: true,
        ..running()
    };
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""));
    assert!(!fixture.layout.state_dir.exists());
}

#[test]
fn a_tree_that_appears_while_the_daemon_stops_refuses_before_any_delete() {
    let fixture = Fixture::new();
    let daemon = Fake {
        plant_on_stop: Some(fixture.layout.state_dir.join("worktrees/01LATE")),
        ..running()
    };
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("1 agent worktree exists"), "{err}");
    assert_eq!(*daemon.events.borrow(), ["busy", "stop"]);
    assert!(fixture.everything_exists());
}

#[test]
fn busy_agents_refuse_by_name_and_state_and_now_means_closing_time() {
    let fixture = Fixture::new();
    let daemon = Fake {
        busy: vec![
            "Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is running a turn".to_owned(),
            "Bob (01ARZ3NDEKTSV4RRFFQ69G5FAV) is attached to a person's live session".to_owned(),
        ],
        ..running()
    };
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(
        (code, err.as_str()),
        (
            exit::REFUSED,
            "dudes: refusing to uninstall: these agents are busy: Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is running a turn; Bob (01ARZ3NDEKTSV4RRFFQ69G5FAV) is attached to a person's live session. Wait for them to finish, pass --now to stop with closing time (every agent gets up to 6 minutes to save its work), or run dudes stop --hard first to end their turns at once. Nothing was changed.\n"
        )
    );
    assert_eq!(*daemon.events.borrow(), ["busy"]);
    assert!(fixture.everything_exists());

    let now = UninstallOptions {
        yes: true,
        now: true,
        ..UninstallOptions::default()
    };
    let (code, out, _) = fixture.run(now, None, &daemon);
    assert_eq!(code, exit::SUCCESS);
    assert!(out.contains("Closing time: every agent gets up to 6 minutes to save its work before the daemon stops.\n"), "{out}");
    assert_eq!(*daemon.events.borrow(), ["busy", "busy", "stop"]);
    assert!(!fixture.layout.state_dir.exists());
}

#[test]
fn the_busy_call_comes_first_even_with_now_and_a_refusal_stops_everything() {
    let fixture = Fixture::new();
    let daemon = Fake {
        busy_fails: true,
        ..running()
    };
    let now = UninstallOptions {
        yes: true,
        now: true,
        ..UninstallOptions::default()
    };
    let (code, _, err) = fixture.run(now, None, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(
        err.contains(
            "could not ask the daemon which agents are busy (unauthorized); nothing was changed"
        ),
        "{err}"
    );
    assert_eq!(*daemon.events.borrow(), ["busy"]);
    assert!(fixture.everything_exists());
}

#[test]
fn a_running_daemon_is_stopped_before_anything_is_deleted() {
    let fixture = Fixture::new();
    let daemon = running();
    let (code, out, _) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::SUCCESS);
    assert_eq!(*daemon.events.borrow(), ["busy", "stop"]);
    assert!(out.find("the daemon stopped").unwrap() < out.find("removed ").unwrap());
}

#[test]
fn a_lock_held_by_an_unseen_daemon_refuses_and_so_does_one_that_outlives_the_stop() {
    let fixture = Fixture::new();
    let lock = File::create(fixture.layout.state_dir.join(LOCK_NAME)).unwrap();
    lock.lock().unwrap();
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("a process holds the lock"), "{err}");
    let daemon = running();
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(
        err.contains("the daemon stopped but a process still holds the lock"),
        "{err}"
    );
    assert_eq!(*daemon.events.borrow(), ["busy", "stop"]);
    drop(lock);
    assert!(fixture.everything_exists());
}

#[test]
fn without_a_terminal_and_without_yes_nothing_is_deleted() {
    let fixture = Fixture::new();
    let (code, _, err) = fixture.run(UninstallOptions::default(), None, &Fake::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::REFUSED,
            "dudes: uninstall needs --yes when it is not run on a terminal; nothing was deleted\n"
        )
    );
    assert!(fixture.everything_exists());
}

#[test]
fn a_declined_prompt_changes_nothing_and_exits_zero() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.run(UninstallOptions::default(), Some("n\n"), &Fake::default());
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""));
    assert!(
        out.ends_with("Delete all of this? [y/N] Nothing was changed.\n"),
        "{out}"
    );
    assert!(fixture.everything_exists());
    let (code, _, _) = fixture.run(UninstallOptions::default(), Some("y\n"), &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(!fixture.layout.state_dir.exists());
}

#[test]
fn the_tail_names_no_engine_folder_the_user_does_not_have() {
    let fixture = Fixture::new();
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    for absent in [".claude", ".codex", "shell_snapshots", "session history"] {
        assert!(!out.contains(absent), "{absent} in:\n{out}");
    }
    assert!(out.contains("browser's site data for the Floor's 127.0.0.1 address"));
    assert!(out.contains("Branches named dudes/<id>"));
}

#[test]
fn the_tail_names_only_the_engine_folders_that_exist() {
    let mut fixture = Fixture::new();
    let elsewhere = fixture.base().join("codex-elsewhere");
    std::fs::create_dir_all(elsewhere.join("shell_snapshots")).unwrap();
    std::fs::create_dir_all(fixture.layout.home.join(".claude/projects")).unwrap();
    fixture.layout.codex_home = Some(elsewhere);
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        out.contains("    ~/.claude/projects/. Dudes does not read"),
        "{out}"
    );
    assert!(!out.contains("~/.codex/sessions/"), "{out}");
    assert!(out.contains("find ~/.codex/shell_snapshots"), "{out}");
}

#[test]
fn a_full_delete_leaves_nothing_behind_and_prints_what_it_cannot_do() {
    let fixture = Fixture::new();
    for folder in [
        ".claude/projects",
        ".codex/sessions",
        ".codex/shell_snapshots",
    ] {
        std::fs::create_dir_all(fixture.layout.home.join(folder)).unwrap();
    }
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""));
    let l = &fixture.layout;
    let bin = fixture.bin();
    for gone in [
        &l.state_dir,
        &l.runtime_dir,
        l.config.parent().unwrap(),
        &bin.join("duded"),
        &bin.join("dudes"),
        &bin.join("dudes-hook"),
    ] {
        assert!(!gone.exists(), "{} should be gone", gone.display());
    }
    assert!(
        bin.is_dir(),
        "the bin folder itself is not Dudes' to delete"
    );
    assert!(
        out.contains("\nDudes is uninstalled (7 items removed).\n"),
        "{out}"
    );
    assert!(out.contains(
        "find ~/.codex/shell_snapshots -type f -exec grep -q 'DUDES_TOKEN=' {} \\; -print -delete\n"
    ));
    assert!(out.contains("~/.claude/projects/ and ~/.codex/sessions/"));
    assert!(out.contains("browser's site data for the Floor's 127.0.0.1 address"));
}

fn other_prefix(fixture: &mut Fixture) -> PathBuf {
    let prefix = fixture.base().join("opt");
    std::fs::create_dir_all(prefix.join("share/dudes/floor")).unwrap();
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    for name in NOTICES {
        std::fs::write(prefix.join("share/dudes").join(name), "x").unwrap();
    }
    for name in PROGRAMS {
        std::fs::write(prefix.join("bin").join(name), "x").unwrap();
    }
    fixture.layout.executable = prefix.join("bin/dudes");
    prefix
}

#[test]
fn an_install_into_another_prefix_also_removes_its_share_folder_when_that_empties_it() {
    let mut fixture = Fixture::new();
    let prefix = other_prefix(&mut fixture);
    let share = prefix.join("share/dudes");
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        out.contains(&format!("removed {}\n", share.display())),
        "{out}"
    );
    assert!(!share.exists() && prefix.join("share").is_dir());
}

#[test]
fn a_share_folder_with_a_foreign_file_keeps_the_folder_and_the_file() {
    let mut fixture = Fixture::new();
    let prefix = other_prefix(&mut fixture);
    let share = prefix.join("share/dudes");
    std::fs::write(share.join("mine.txt"), "x").unwrap();
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        out.contains(&format!(
            "kept {}: it holds files that are not Dudes': mine.txt\n",
            share.display()
        )),
        "{out}"
    );
    assert!(share.join("mine.txt").exists() && !share.join("floor").exists());
}

#[test]
fn a_copy_install_sh_did_not_make_keeps_its_programs_and_says_how_to_remove_them() {
    let mut fixture = Fixture::new();
    let installed = fixture.bin().join("dudes");
    let dist = fixture.base().join("checkout/target/dist");
    std::fs::create_dir_all(&dist).unwrap();
    for name in PROGRAMS {
        std::fs::write(dist.join(name), "x").unwrap();
    }
    fixture.layout.executable = dist.join("dudes");
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        out.contains("The programs are not removed: this copy of dudes ("),
        "{out}"
    );
    for name in PROGRAMS {
        assert!(
            out.contains(&format!("  rm '{}'\n", dist.join(name).display())),
            "{out}"
        );
        assert!(dist.join(name).exists());
    }
    assert!(!fixture.layout.state_dir.exists());
    assert!(installed.exists(), "the real bin is not this copy's");
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_to_dudes_is_not_an_install() {
    let mut fixture = Fixture::new();
    let real = fixture.bin().join("dudes");
    let link = fixture.base().join("linked-dudes");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    fixture.layout.executable = link.clone();
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(out.contains("The programs are not removed"), "{out}");
    assert!(
        out.contains(&format!("  rm '{}'\n", real.display())),
        "{out}"
    );
    assert!(real.exists() && link.exists());
}

fn refused(fixture: &Fixture, needle: &str) {
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED, "{err}");
    assert!(err.contains(needle), "{err}");
    assert!(err.ends_with("Nothing was deleted.\n"), "{err}");
}

#[test]
fn the_home_directory_and_its_parents_and_root_are_never_deleted() {
    let mut fixture = Fixture::new();
    fixture.layout.state_dir = fixture.layout.home.clone();
    refused(
        &fixture,
        "is the home directory or a parent of it, which Dudes never deletes",
    );
    fixture.layout.state_dir = fixture.layout.home.join(".local/share/dudes");
    fixture.layout.runtime_dir = fixture.base();
    refused(
        &fixture,
        "is the home directory or a parent of it, which Dudes never deletes",
    );
    fixture.layout.runtime_dir = PathBuf::from("/");
    refused(&fixture, "/ is the filesystem root");
}

#[test]
fn an_override_that_only_has_the_names_is_not_enough() {
    let mut fixture = Fixture::new();
    let other = fixture.base().join("projects");
    std::fs::create_dir_all(other.join("journal")).unwrap();
    std::fs::write(other.join(LOCK_NAME), "").unwrap();
    std::fs::write(other.join("state.redb"), "not a database").unwrap();
    fixture.layout.state_dir = other.clone();
    refused(
        &fixture,
        &format!(
            "{} is not the default location and does not hold what Dudes writes there",
            other.display()
        ),
    );
    assert!(other.join("journal").exists());
}

#[test]
fn a_runtime_override_needs_a_credential_a_socket_or_the_quickstart_log() {
    let mut fixture = Fixture::new();
    let custom = fixture.base().join("custom-run");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("duded.log"), "somebody else's log").unwrap();
    fixture.layout.runtime_dir = custom.clone();
    refused(&fixture, "does not hold what Dudes writes there");
    std::fs::write(
        custom.join("operator.cred"),
        "c".repeat(dudes_proto::rpc::cred::OPERATOR_CREDENTIAL_LEN),
    )
    .unwrap();
    let (code, _, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(!custom.exists());
}

#[test]
fn an_override_keeps_the_folder_and_a_foreign_file_and_deletes_only_what_dudes_wrote() {
    let mut fixture = Fixture::new();
    let custom = fixture.custom_state();
    std::fs::write(custom.join("notes.txt"), "mine").unwrap();
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS, "{out}");
    assert!(custom.join("notes.txt").exists());
    for gone in ["journal", LOCK_NAME, "state.redb"] {
        assert!(!custom.join(gone).exists(), "{gone}");
    }
    assert!(
        out.contains(&format!(
            "kept {}: it holds files that are not Dudes': notes.txt\n",
            custom.display()
        )),
        "{out}"
    );
}

#[test]
fn an_override_that_empties_is_removed() {
    let mut fixture = Fixture::new();
    let custom = fixture.custom_state();
    let (code, _, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(!custom.exists());
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_refused_and_named() {
    let mut fixture = Fixture::new();
    let real = fixture.custom_state();
    let link = fixture.base().join("link-state");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    fixture.layout.state_dir = link.clone();
    refused(
        &fixture,
        &format!(
            "{} is a symbolic link, and Dudes does not delete through one",
            link.display()
        ),
    );
    assert!(real.join("journal").exists());
}

#[test]
fn a_config_folder_keeps_a_foreign_file() {
    let fixture = Fixture::new();
    let folder = fixture.layout.config.parent().unwrap().to_path_buf();
    std::fs::write(folder.join("other.toml"), "mine").unwrap();
    let (code, _, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(folder.join("other.toml").exists() && !folder.join("config.toml").exists());
}

#[test]
fn with_dudes_config_set_only_an_operator_file_goes() {
    let mut fixture = Fixture::new();
    let folder = fixture.base().join("my-configs");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("dudes.toml");
    std::fs::write(&file, "[floor.hire]\nenabled = true\n").unwrap();
    std::fs::write(folder.join("other.toml"), "keep").unwrap();
    fixture.layout.config = file.clone();
    fixture.layout.config_overridden = true;
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(out.contains(&format!("removed {}\n", file.display())));
    assert!(!file.exists() && folder.join("other.toml").exists());
}

#[test]
fn with_dudes_config_set_a_file_that_is_not_an_operator_file_is_skipped_and_explained() {
    let mut fixture = Fixture::new();
    let file = fixture.base().join("Cargo.toml");
    std::fs::write(&file, "[package]\nname = \"x\"\n").unwrap();
    fixture.layout.config = file.clone();
    fixture.layout.config_overridden = true;
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        out.contains(&format!(
            "skipped: {} is not deleted: it has tables an operator file does not: package",
            file.display()
        )),
        "{out}"
    );
    assert!(file.exists());
    std::fs::write(&file, "not toml [").unwrap();
    let (_, out, _) = fixture.run(yes(), None, &Fake::default());
    assert!(
        out.contains("is not deleted: it is not valid TOML"),
        "{out}"
    );
    assert!(file.exists());
}

#[cfg(unix)]
fn locked_journal(state: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let locked = state.join("journal");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    locked
}

#[cfg(unix)]
fn unlock(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_partial_failure_lists_what_remains_keeps_the_programs_and_a_rerun_finishes() {
    let fixture = Fixture::new();
    let locked = locked_journal(&fixture.layout.state_dir);
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    unlock(&locked);
    let l = &fixture.layout;
    assert_eq!(code, exit::OTHER, "{err}");
    assert!(
        err.starts_with("dudes: uninstall did not finish: 3 removed, 4 remain\n"),
        "{err}"
    );
    assert!(
        err.contains(&format!("Still present:\n  {}\n", l.state_dir.display())),
        "{err}"
    );
    assert!(err.ends_with(
        "The programs were kept so you can run dudes uninstall again once the cause is fixed.\n"
    ));
    assert!(out.contains(&format!("removed {}\n", l.runtime_dir.display())));
    assert!(fixture.bin().join("dudes").exists());
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""), "{out}");
    assert!(!l.state_dir.exists() && !fixture.bin().join("dudes").exists());
}

#[cfg(unix)]
#[test]
fn a_partial_failure_in_an_override_keeps_its_markers_so_a_rerun_is_allowed() {
    let mut fixture = Fixture::new();
    let custom = fixture.custom_state();
    std::fs::write(custom.join("notes.txt"), "mine").unwrap();
    let locked = locked_journal(&custom);
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    unlock(&locked);
    assert_eq!(code, exit::OTHER, "{err}");
    assert!(custom.join("state.redb").exists() && custom.join(LOCK_NAME).exists());
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""), "{out}");
    assert!(!custom.join("journal").exists() && custom.join("notes.txt").exists());
}

#[test]
fn nothing_installed_is_not_an_error() {
    let fixture = Fixture::new();
    let (code, _, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(
        (code, out.as_str()),
        (
            exit::SUCCESS,
            "Dudes has nothing installed or stored here.\n"
        )
    );
}

#[test]
fn sizes_print_in_binary_units_with_one_decimal() {
    let shown: Vec<String> = [
        0,
        1023,
        1024,
        1536,
        5 * 1024 * 1024,
        3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024,
        u64::MAX,
    ]
    .map(human)
    .into();
    assert_eq!(
        shown,
        [
            "0 B",
            "1023 B",
            "1.0 KiB",
            "1.5 KiB",
            "5.0 MiB",
            "3.5 GiB",
            "16777215.9 TiB"
        ]
    );
}
