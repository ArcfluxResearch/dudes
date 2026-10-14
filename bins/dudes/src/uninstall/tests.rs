use std::cell::RefCell;
use std::io::Cursor;

use super::*;

struct Fixture {
    root: tempfile::TempDir,
    layout: Layout,
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
        };
        Self { root, layout }
    }

    fn base(&self) -> PathBuf {
        self.root.path().canonicalize().expect("a real path")
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
        let bin = l.executable.parent().expect("bin");
        [&l.state_dir, &l.runtime_dir, &l.config]
            .into_iter()
            .chain(
                PROGRAMS
                    .map(|name| bin.join(name))
                    .iter()
                    .collect::<Vec<_>>(),
            )
            .all(|path| path.exists())
    }
}

#[derive(Default)]
struct Fake {
    running: bool,
    busy: Vec<String>,
    listed: Vec<PathBuf>,
    stops: RefCell<Vec<bool>>,
}

impl Daemon for Fake {
    fn running(&self) -> bool {
        self.running
    }
    fn mid_turn(&self) -> Result<Vec<String>, String> {
        Ok(self.busy.clone())
    }
    fn worktrees(&self) -> Vec<PathBuf> {
        self.listed.clone()
    }
    fn stop(&self, now: bool, out: &mut dyn Write, _: &mut dyn Write) -> Result<(), String> {
        self.stops.borrow_mut().push(now);
        writeln!(out, "the daemon stopped").map_err(|error| error.to_string())
    }
}

fn yes() -> UninstallOptions {
    UninstallOptions {
        yes: true,
        ..UninstallOptions::default()
    }
}

#[test]
fn the_plan_lists_every_path_with_its_size() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.run(
        UninstallOptions {
            dry_run: true,
            ..UninstallOptions::default()
        },
        None,
        &Fake::default(),
    );
    let l = &fixture.layout;
    let bin = l.executable.parent().unwrap().display();
    let expected = format!(
        "Dudes will remove:\n\
         \x20 runtime directory           {runtime}  (3 B)\n\
         \x20 state directory             {state}  (5 B)\n\
         \x20 operator file folder        {config}  (8 B)\n\
         \x20 program                     {bin}/duded  (2.0 KiB)\n\
         \x20 program                     {bin}/dudes  (2.0 KiB)\n\
         \x20 program                     {bin}/dudes-hook  (2.0 KiB)\n\
         \n\
         This was a dry run. Nothing was deleted.\n",
        runtime = l.runtime_dir.display(),
        state = l.state_dir.display(),
        config = l.config.parent().unwrap().display(),
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (exit::SUCCESS, expected.as_str(), "")
    );
    assert!(fixture.everything_exists());
}

#[test]
fn a_worktree_refuses_everything_and_prints_the_git_commands() {
    let fixture = Fixture::new();
    let base = fixture.base();
    let repository = base.join("repo");
    let admin = repository.join(".git/worktrees/01TREE");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/dudes/abc123def456\n").unwrap();
    let tree = fixture.layout.state_dir.join("worktrees/01TREE");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

    let (code, _, err) = fixture.run(yes(), None, &Fake::default());

    let expected = format!(
        "dudes: refusing to uninstall: 1 agent worktree exists. A tree can hold the only copy of an agent's work, so nothing was deleted.\n\
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
    assert!(fixture.everything_exists());
    assert!(tree.exists());
}

#[test]
fn a_tree_without_a_readable_pointer_still_refuses() {
    let fixture = Fixture::new();
    let tree = fixture.layout.state_dir.join("worktrees/01LOST");
    std::fs::create_dir_all(&tree).unwrap();
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("repository: unknown (this tree's .git file is missing or unreadable)"));
    assert!(fixture.everything_exists());
}

#[test]
fn a_tree_only_the_daemon_lists_also_refuses() {
    let fixture = Fixture::new();
    let elsewhere = fixture.base().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let daemon = Fake {
        running: true,
        listed: vec![elsewhere.clone(), fixture.base().join("never-created")],
        ..Fake::default()
    };
    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains(&format!("  {}\n", elsewhere.display())));
    assert!(!err.contains("never-created"));
    assert!(daemon.stops.borrow().is_empty());
    assert!(fixture.everything_exists());
}

#[test]
fn a_mid_turn_agent_refuses_unless_now() {
    let fixture = Fixture::new();
    let daemon = Fake {
        running: true,
        busy: vec![
            "Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX)".to_owned(),
            "Bob (01ARZ3NDEKTSV4RRFFQ69G5FAV)".to_owned(),
        ],
        ..Fake::default()
    };

    let (code, _, err) = fixture.run(yes(), None, &daemon);
    assert_eq!(
        (code, err.as_str()),
        (
            exit::REFUSED,
            "dudes: refusing to uninstall: these agents are mid-turn: Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX), Bob (01ARZ3NDEKTSV4RRFFQ69G5FAV). Wait for them to finish, or pass --now to stop the daemon and end their turns at once. Nothing was deleted.\n"
        )
    );
    assert!(daemon.stops.borrow().is_empty());
    assert!(fixture.everything_exists());

    let now = UninstallOptions {
        yes: true,
        now: true,
        ..UninstallOptions::default()
    };
    let (code, _, _) = fixture.run(now, None, &daemon);
    assert_eq!(code, exit::SUCCESS);
    assert_eq!(*daemon.stops.borrow(), [true]);
    assert!(!fixture.layout.state_dir.exists());
}

#[test]
fn a_running_daemon_is_stopped_the_closing_way_before_anything_is_deleted() {
    let fixture = Fixture::new();
    let daemon = Fake {
        running: true,
        ..Fake::default()
    };
    let (code, out, _) = fixture.run(yes(), None, &daemon);
    assert_eq!(code, exit::SUCCESS);
    assert_eq!(*daemon.stops.borrow(), [false]);
    let stopped = out.find("the daemon stopped").expect("stop output");
    let first = out.find("removed ").expect("a deletion");
    assert!(stopped < first);
}

#[test]
fn a_lock_held_by_an_unseen_daemon_refuses() {
    let fixture = Fixture::new();
    let lock = File::create(fixture.layout.state_dir.join(LOCK_NAME)).unwrap();
    lock.lock().unwrap();
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("a process holds the lock"), "{err}");
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
            exit::OTHER,
            "dudes: uninstall needs --yes when it is not run on a terminal; nothing was deleted\n"
        )
    );
    assert!(fixture.everything_exists());
}

#[test]
fn a_terminal_asks_and_only_y_deletes() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.run(UninstallOptions::default(), Some("n\n"), &Fake::default());
    assert_eq!(
        (code, err.as_str()),
        (
            exit::OTHER,
            "dudes: uninstall canceled; nothing was deleted\n"
        )
    );
    assert!(out.ends_with("Delete all of this? [y/N] "), "{out}");
    assert!(fixture.everything_exists());

    let (code, _, _) = fixture.run(UninstallOptions::default(), Some("y\n"), &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(!fixture.layout.state_dir.exists());
}

#[test]
fn a_full_delete_leaves_nothing_behind_and_prints_what_it_cannot_do() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!((code, err.as_str()), (exit::SUCCESS, ""));
    let l = &fixture.layout;
    let bin = l.executable.parent().unwrap();
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
    assert!(out.contains(&format!("removed {}\n", l.state_dir.display())));
    assert!(out.contains("\nDudes is uninstalled (6 items removed).\n"));
    assert!(out.contains(
        "find ~/.codex/shell_snapshots -type f -exec grep -q 'DUDES_TOKEN=' {} \\; -print -delete\n"
    ));
    assert!(out.contains("~/.claude/projects/ and ~/.codex/sessions/"));
    assert!(out.contains("browser's site data for the Floor's 127.0.0.1 address"));
}

#[test]
fn an_install_into_another_prefix_also_removes_its_share_folder() {
    let mut fixture = Fixture::new();
    let prefix = fixture.base().join("opt");
    let share = prefix.join("share/dudes");
    std::fs::create_dir_all(share.join("floor")).unwrap();
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    std::fs::write(share.join("LICENSE-MIT"), "x").unwrap();
    for name in PROGRAMS {
        std::fs::write(prefix.join("bin").join(name), "x").unwrap();
    }
    fixture.layout.executable = prefix.join("bin/dudes");
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(out.contains(&format!("removed {}\n", share.display())));
    assert!(!share.exists());
    assert!(prefix.join("share").is_dir());
}

#[test]
fn a_share_folder_without_installer_files_is_left_alone() {
    let mut fixture = Fixture::new();
    let prefix = fixture.base().join("opt");
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    std::fs::create_dir_all(prefix.join("share/dudes")).unwrap();
    std::fs::write(prefix.join("share/dudes/mine.txt"), "x").unwrap();
    fixture.layout.executable = prefix.join("bin/dudes");
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(
        !out.contains(&prefix.join("share/dudes").display().to_string()),
        "{out}"
    );
    assert!(prefix.join("share/dudes/mine.txt").exists());
}

fn refused(fixture: &Fixture, needle: &str) {
    let (code, _, err) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::REFUSED, "{err}");
    assert!(err.contains(needle), "{err}");
    assert!(err.ends_with("Nothing was deleted.\n"), "{err}");
}

#[test]
fn the_home_directory_is_never_deleted() {
    let mut fixture = Fixture::new();
    fixture.layout.state_dir = fixture.layout.home.clone();
    refused(
        &fixture,
        "is the home directory or a parent of it, which Dudes never deletes",
    );
    assert!(fixture.layout.home.exists());
}

#[test]
fn a_parent_of_the_home_directory_is_never_deleted() {
    let mut fixture = Fixture::new();
    fixture.layout.runtime_dir = fixture.base();
    std::fs::write(fixture.base().join("duded.log"), "marked").unwrap();
    refused(
        &fixture,
        "is the home directory or a parent of it, which Dudes never deletes",
    );
    assert!(fixture.everything_exists());
}

#[test]
fn the_filesystem_root_is_never_deleted() {
    let mut fixture = Fixture::new();
    fixture.layout.state_dir = PathBuf::from("/");
    refused(&fixture, "/ is the filesystem root");
}

#[test]
fn an_override_without_dudes_files_is_not_deleted() {
    let mut fixture = Fixture::new();
    let other = fixture.base().join("projects");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("notes.txt"), "mine").unwrap();
    fixture.layout.state_dir = other.clone();
    refused(
        &fixture,
        &format!(
            "{} is not the default location and holds none of Dudes' own files (journal, state.redb, LOCK)",
            other.display()
        ),
    );
    assert!(other.join("notes.txt").exists());
}

#[test]
fn an_override_with_dudes_files_is_deleted() {
    let mut fixture = Fixture::new();
    let custom = fixture.base().join("custom-state");
    std::fs::create_dir_all(custom.join("journal")).unwrap();
    fixture.layout.state_dir = custom.clone();
    let (code, _, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(!custom.exists());
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_refused_and_named() {
    let mut fixture = Fixture::new();
    let real = fixture.base().join("real-state");
    std::fs::create_dir_all(real.join("journal")).unwrap();
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
fn with_dudes_config_set_only_that_file_goes() {
    let mut fixture = Fixture::new();
    let folder = fixture.base().join("my-configs");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("dudes.toml");
    std::fs::write(&file, "x").unwrap();
    std::fs::write(folder.join("other.toml"), "keep").unwrap();
    fixture.layout.config = file.clone();
    fixture.layout.config_overridden = true;
    let (code, out, _) = fixture.run(yes(), None, &Fake::default());
    assert_eq!(code, exit::SUCCESS);
    assert!(out.contains(&format!("removed {}\n", file.display())));
    assert!(!file.exists());
    assert!(folder.join("other.toml").exists());
}

#[cfg(unix)]
#[test]
fn a_partial_failure_lists_what_remains_and_keeps_the_programs() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let locked = fixture.layout.state_dir.join("journal");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (code, out, err) = fixture.run(yes(), None, &Fake::default());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let l = &fixture.layout;
    let bin = l.executable.parent().unwrap();
    assert_eq!(code, exit::OTHER, "{err}");
    assert!(
        err.starts_with("dudes: uninstall did not finish: 2 removed, 4 remain\n"),
        "{err}"
    );
    assert!(
        err.contains(&format!("Still present:\n  {}\n", l.state_dir.display())),
        "{err}"
    );
    assert!(
        err.contains(&format!("  {}\n", bin.join("dudes").display())),
        "{err}"
    );
    assert!(err.ends_with(
        "The programs were kept so you can run dudes uninstall again once the cause is fixed.\n"
    ));
    assert!(out.contains(&format!("removed {}\n", l.runtime_dir.display())));
    assert!(!l.runtime_dir.exists());
    assert!(bin.join("dudes").exists());
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
