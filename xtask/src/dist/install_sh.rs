//! `install.sh` against releases this module's archive and checksum writers produce, served from
//! 127.0.0.1, on pretend hosts: fake `uname`, `id`, `sysctl` and `ldd` come first on `PATH`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use super::tests::write;
use super::{BINARIES, NOTICES, installer_targets, record_checksum, write_archive};

const INSTALL_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../install.sh");
const VERSION: &str = "1.2.3";

fn release(dir: &Path, targets: &[&str]) {
    release_at(dir, VERSION, targets);
}

/// [`release`] at `version`.
fn release_at(dir: &Path, version: &str, targets: &[&str]) {
    for target in targets {
        let tree = tempfile::tempdir().expect("a release tree");
        for binary in BINARIES {
            // `dudes ping` answers as a daemon of the version in DUDES_FAKE_DAEMON, or fails as
            // it does when no daemon runs.
            let ping = if binary == "dudes" {
                "if [ \"${1:-}\" = ping ]; then\n\
                 if [ -n \"${DUDES_FAKE_DAEMON:-}\" ]; then echo \"duded $DUDES_FAKE_DAEMON on /sock\"; exit 0; fi\n\
                 echo 'cannot reach the daemon' >&2; exit 1\nfi\n"
            } else {
                ""
            };
            write(
                &tree.path().join(binary),
                &format!("#!/bin/sh\n{ping}echo {binary} {target}\n"),
            );
        }
        for notice in NOTICES {
            write(
                &tree.path().join(notice),
                &format!("{notice} {version} {target}\n"),
            );
        }
        write(
            &tree.path().join("floor/release-manifest.json"),
            &format!("{{\"target\":\"{target}\"}}"),
        );
        write(&tree.path().join("floor/index.html"), "<!doctype html>");
        let name = format!("dudes-{version}-{target}");
        let archive = dir.join(format!("{name}.tar.gz"));
        write_archive(tree.path(), &name, &archive).expect("an archive");
        record_checksum(&dir.join("SHA256SUMS"), version, &archive).expect("a checksum line");
    }
}

/// What a test server answers: the files below a directory, a missing one with a 404, or every
/// request with a redirect to the same path under another base URL.
enum Answer {
    Files(PathBuf),
    RedirectTo(String),
}

/// A test server on 127.0.0.1, alive for the rest of this test process.
struct Served {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Served {
    /// The request paths this server has received, in order.
    fn requested(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

/// Serve the files below `root`.
fn serve(root: &Path) -> Served {
    start(Answer::Files(root.to_path_buf()))
}

fn start(answer: Answer) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("an address").port()
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            respond(stream, &answer, &log);
        }
    });
    Served { url, requests }
}

fn respond(mut stream: TcpStream, answer: &Answer, log: &Mutex<Vec<String>>) {
    let mut request = Vec::new();
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        request.push(line);
    }
    let path = request
        .first()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or("/");
    log.lock().expect("the request log").push(path.to_owned());
    let (status, headers, body) = match answer {
        Answer::Files(root) => match std::fs::read(root.join(path.trim_start_matches('/'))) {
            Ok(body) => ("200 OK", String::new(), body),
            Err(_) => ("404 Not Found", String::new(), Vec::new()),
        },
        Answer::RedirectTo(base) => (
            "302 Found",
            format!("Location: {base}{path}\r\n"),
            Vec::new(),
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(&body);
}

/// A pretend host: its own `HOME` and `TMPDIR`, fake tools first on `PATH`, and a umask that
/// would leave new files group-writable.
///
/// `TMPDIR` is `$HOME/tmp/install`, so a path three levels up from the script's temporary
/// directory lands in `$HOME`, inside the test.
struct Host {
    home: tempfile::TempDir,
    fakes: PathBuf,
    path: OsString,
}

impl Host {
    /// A host whose `uname -s` and `uname -m` print `system` and `machine`, run by uid 501, not
    /// under Rosetta and with glibc, and with the real tools after the fakes on `PATH`.
    fn new(system: &str, machine: &str) -> Self {
        let home = tempfile::tempdir().expect("a home");
        let fakes = home.path().join("fakes");
        std::fs::create_dir_all(home.path().join("tmp/install")).expect("a TMPDIR");
        let mut path = fakes.clone().into_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let host = Self { home, fakes, path };
        host.fake(
            "uname",
            &format!("case $1 in -s) echo {system} ;; -m) echo {machine} ;; esac"),
        );
        host.fake("id", "echo 501");
        host.fake("sysctl", "echo 0");
        host.fake("ldd", "echo 'ldd (GNU libc) 2.39'");
        host
    }

    /// Replace the fake `tool` with a script whose body is `script`.
    fn fake(&self, tool: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let file = self.fakes.join(tool);
        write(&file, &format!("#!/bin/sh\n{script}\n"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
            .expect("an executable fake");
    }

    /// Put only the fakes and these real tools on `PATH`.
    fn only(&mut self, tools: &[&str]) {
        let real = std::env::var_os("PATH").unwrap_or_default();
        for tool in tools {
            let found = std::env::split_paths(&real)
                .map(|dir| dir.join(tool))
                .find(|path| path.is_file())
                .unwrap_or_else(|| panic!("{tool} is on this machine's PATH"));
            std::os::unix::fs::symlink(found, self.fakes.join(tool)).expect("a linked tool");
        }
        self.path = self.fakes.clone().into_os_string();
    }

    /// The default prefix, `$HOME/.local`.
    fn prefix(&self) -> PathBuf {
        self.home.path().join(".local")
    }

    /// Run `install.sh` with `args` against the release at `url`.
    fn install(&self, url: &str, args: &[&str]) -> Output {
        self.run(Some(url), args)
    }

    /// Run `install.sh` with `args` and, when `url` is given, `DUDES_INSTALL_BASE_URL` set to it.
    fn run(&self, url: Option<&str>, args: &[&str]) -> Output {
        self.run_in(None, &[], url, args)
    }

    /// Run `install.sh` as `curl ... | sh -s -- <args>` does: the script arrives on standard input.
    fn pipe(&self, url: &str, args: &[&str]) -> Output {
        use std::process::Stdio;

        let mut child = Command::new("/bin/sh")
            .args(["-c", "umask 002 && exec /bin/sh -s -- \"$@\"", "sh"])
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path().join("tmp/install"))
            .env("PATH", &self.path)
            .env("DUDES_INSTALL_BASE_URL", url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh starts");
        let script = std::fs::read(INSTALL_SH).expect("install.sh");
        child
            .stdin
            .take()
            .expect("standard input")
            .write_all(&script)
            .expect("the script reaches sh");
        child.wait_with_output().expect("sh finishes")
    }

    /// [`Host::run`] from the working directory `dir` with `env` added to the environment.
    fn run_in(
        &self,
        dir: Option<&Path>,
        env: &[(&str, &Path)],
        url: Option<&str>,
        args: &[&str],
    ) -> Output {
        let mut command = Command::new("/bin/sh");
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        command
            .args(["-c", "umask 002 && exec /bin/sh \"$0\" \"$@\"", INSTALL_SH])
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path().join("tmp/install"))
            .env("PATH", &self.path)
            .envs(env.iter().copied());
        if let Some(url) = url {
            command.env("DUDES_INSTALL_BASE_URL", url);
        }
        let output = command.output().expect("sh runs install.sh");
        let left = std::fs::read_dir(self.home.path().join("tmp/install"))
            .expect("the TMPDIR")
            .count();
        assert_eq!(left, 0, "install.sh leaves nothing in TMPDIR: {output:?}");
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rollback_dirs(share: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(share)
        .expect("share exists")
        .map(|entry| entry.expect("share entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".install-rollback."))
        })
        .collect()
}

fn restore_dirs(bin: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(bin)
        .expect("bin exists")
        .map(|entry| entry.expect("bin entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".dudes-restore."))
        })
        .collect()
}

fn previous_installation(host: &Host) {
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
}

fn assert_previous_installation(host: &Host) {
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the previous binary"),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the previous Floor"),
        "previous Floor"
    );
    for notice in NOTICES {
        assert_eq!(
            std::fs::read_to_string(share.join(notice)).expect("the previous notice"),
            format!("previous {notice}\n")
        );
        assert!(!share.join(format!(".{notice}.new")).exists());
    }
    for binary in BINARIES {
        assert!(!bin.join(format!(".{binary}.new")).exists());
    }
    assert!(!share.join("floor.new").exists());
    assert!(restore_dirs(&bin).is_empty());
}

/// Assert `install.sh` refused with a message containing `reason` and installed nothing.
fn assert_refused(output: &Output, host: &Host, reason: &str) {
    assert!(!output.status.success(), "install.sh refused: {output:?}");
    assert!(
        stderr(output).contains(reason),
        "the refusal says {reason:?}: {}",
        stderr(output)
    );
    assert!(!host.prefix().exists(), "nothing was installed");
}

#[test]
fn it_installs_the_binaries_and_floor_and_says_what_to_run_next() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let url = serve(served.path()).url;
    let mut host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");

    let first = host.install(&url, &[]);

    assert!(first.status.success(), "installed: {first:?}");
    for binary in BINARIES {
        let ran = Command::new(bin.join(binary))
            .output()
            .expect("the installed binary runs");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            format!("{binary} aarch64-apple-darwin\n")
        );
    }
    assert_eq!(
        std::fs::read_to_string(
            host.prefix()
                .join("share/dudes/floor/release-manifest.json")
        )
        .expect("the installed Floor"),
        "{\"target\":\"aarch64-apple-darwin\"}"
    );
    for notice in NOTICES {
        assert_eq!(
            std::fs::read_to_string(host.prefix().join("share/dudes").join(notice))
                .expect("the installed notice"),
            format!("{notice} 1.2.3 aarch64-apple-darwin\n")
        );
    }
    let mode = |path: PathBuf| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("an installed path")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(bin.clone()), 0o755);
    assert_eq!(mode(bin.join("duded")), 0o755);
    assert_eq!(mode(host.prefix().join("share/dudes")), 0o755);
    assert_eq!(mode(host.prefix().join("share/dudes/floor")), 0o755);
    assert_eq!(
        mode(host.prefix().join("share/dudes/floor/index.html")),
        0o644
    );
    for notice in NOTICES {
        assert_eq!(mode(host.prefix().join("share/dudes").join(notice)), 0o644);
    }
    let said = stdout(&first);
    assert!(
        said.contains(&format!(
            "{} is not on your PATH. Add this line to your shell profile, then open a new shell:\n  \
             export PATH=\"{}:$PATH\"\n",
            bin.display(),
            bin.display()
        )),
        "the PATH line: {said}"
    );
    assert!(
        said.ends_with(&format!(
            "Next, start the daemon:\n  {}/dudes quickstart\n",
            bin.display()
        )),
        "a bin directory off PATH is run by its full path: {said}"
    );

    let mut path = bin.clone().into_os_string();
    path.push(":");
    path.push(&host.path);
    host.path = path;
    let again = host.install(&url, &["--version", VERSION]);
    assert!(again.status.success(), "reinstalled: {again:?}");
    assert!(
        stdout(&again).contains(&format!("{} is on your PATH.\n", bin.display())),
        "{}",
        stdout(&again)
    );
    assert!(
        stdout(&again).ends_with("Next, start the daemon:\n  dudes quickstart\n"),
        "{}",
        stdout(&again)
    );
    release_at(served.path(), "1.2.4", &["aarch64-apple-darwin"]);
    let updated = host.install(&url, &[]);
    assert!(updated.status.success(), "updated: {updated:?}");
    for notice in NOTICES {
        assert_eq!(
            std::fs::read_to_string(host.prefix().join("share/dudes").join(notice))
                .expect("the updated notice"),
            format!("{notice} 1.2.4 aarch64-apple-darwin\n")
        );
    }
}

#[test]
fn it_adds_a_launcher_that_runs_dudes_open() {
    for (system, machine, target) in [
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    ] {
        let served = tempfile::tempdir().expect("a release directory");
        release(served.path(), &[target]);
        let url = serve(served.path()).url;
        let host = Host::new(system, machine);
        let bin = host.prefix().join("bin");

        let installed = host.install(&url, &[]);

        assert!(installed.status.success(), "{system}: {installed:?}");
        let (launcher, expected) = if system == "Darwin" {
            (
                host.home
                    .path()
                    .join("Applications/Dudes.app/Contents/MacOS/Dudes"),
                format!("#!/bin/sh\nexec '{}/dudes' open\n", bin.display()),
            )
        } else {
            (
                host.home
                    .path()
                    .join(".local/share/applications/dudes.desktop"),
                format!("Exec=\"{}/dudes\" open\n", bin.display()),
            )
        };
        let text = std::fs::read_to_string(&launcher).expect("the launcher was written");
        assert!(text.contains(&expected), "{system}: {text}");
        assert!(
            stdout(&installed).contains("the Dudes launcher in "),
            "{system}"
        );
    }
}

/// What an upgrade says after installing, given the version of the daemon already running, if any,
/// and the `dudes` it tells the user to run, which is off `PATH` here.
fn upgrade_says(daemon: Option<&str>) -> (String, String) {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let url = serve(served.path()).url;
    let host = Host::new("Darwin", "arm64");
    let env = daemon.map(|version| ("DUDES_FAKE_DAEMON", Path::new(version)));
    let ran = host.run_in(None, env.as_slice(), Some(&url), &[]);
    assert!(ran.status.success(), "installed: {ran:?}");
    let run = host.prefix().join("bin/dudes");
    (stdout(&ran), run.display().to_string())
}

#[test]
fn it_names_a_daemon_left_running_on_another_version() {
    let (said, run) = upgrade_says(Some("1.0.0"));
    assert!(
        said.ends_with(&format!(
            "A Dudes daemon is still running version 1.0.0. Run `{run} stop`, then `{run} start`, \
             to use {VERSION}.\n"
        )),
        "{said}"
    );
    assert!(!said.contains("Next, start the daemon"), "{said}");
}

#[test]
fn it_adds_nothing_when_the_running_daemon_is_the_version_just_installed() {
    let (said, _) = upgrade_says(Some(VERSION));
    assert!(!said.contains("still running"), "{said}");
    assert!(!said.contains("Next, start the daemon"), "{said}");
}

#[test]
fn it_keeps_the_first_run_advice_when_no_daemon_runs() {
    let (said, _) = upgrade_says(None);
    assert!(said.contains("Next, start the daemon:"), "{said}");
    assert!(!said.contains("still running"), "{said}");
}

#[test]
fn it_installs_when_the_script_arrives_on_standard_input() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let url = serve(served.path()).url;
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");

    let latest = host.pipe(&url, &[]);

    assert!(latest.status.success(), "installed: {latest:?}");
    assert!(
        stdout(&latest).starts_with("Installed Dudes 1.2.3 for aarch64-apple-darwin:"),
        "{}",
        stdout(&latest)
    );
    assert!(
        stdout(&latest).ends_with(&format!(
            "Next, start the daemon:\n  {}/dudes quickstart\n",
            bin.display()
        )),
        "{}",
        stdout(&latest)
    );

    let pinned = host.pipe(&url, &["--version", "v1.2.3"]);

    assert!(pinned.status.success(), "installed: {pinned:?}");
    assert!(
        stdout(&pinned).starts_with("Installed Dudes 1.2.3 for aarch64-apple-darwin:"),
        "{}",
        stdout(&pinned)
    );
    let ran = Command::new(bin.join("dudes"))
        .output()
        .expect("the installed binary runs");
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "dudes aarch64-apple-darwin\n"
    );
}

#[test]
fn a_missing_archive_notice_refuses_fresh_installs_and_upgrades() {
    for missing in NOTICES {
        let served = tempfile::tempdir().expect("a release directory");
        release(served.path(), &["aarch64-apple-darwin"]);
        let name = "dudes-1.2.3-aarch64-apple-darwin";
        let archive = served.path().join(format!("{name}.tar.gz"));
        let unpacked = tempfile::tempdir().expect("the release to alter");
        let unpacked_status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(unpacked.path())
            .status()
            .expect("tar unpacks the release");
        assert!(unpacked_status.success());
        let tree = unpacked.path().join(name);
        std::fs::remove_file(tree.join(missing)).expect("omit one archive notice");
        write_archive(&tree, name, &archive).expect("the incomplete archive");
        record_checksum(&served.path().join("SHA256SUMS"), VERSION, &archive)
            .expect("a matching checksum");
        let url = serve(served.path()).url;
        for upgrading in [false, true] {
            let host = Host::new("Darwin", "arm64");
            if upgrading {
                previous_installation(&host);
            }
            let refused = host.install(&url, &[]);
            assert!(!refused.status.success(), "missing {missing}: {refused:?}");
            assert!(stderr(&refused).contains(&format!("has no {missing}; nothing was installed")));
            if upgrading {
                assert_previous_installation(&host);
                assert!(rollback_dirs(&host.prefix().join("share/dudes")).is_empty());
            } else {
                assert!(!host.prefix().exists());
            }
        }
    }
}

#[test]
fn notice_staging_backup_and_swap_failures_keep_the_previous_installation() {
    for notice in NOTICES {
        for phase in ["stage", "backup", "swap"] {
            let served = tempfile::tempdir().expect("a release directory");
            release(served.path(), &["aarch64-apple-darwin"]);
            let host = Host::new("Darwin", "arm64");
            previous_installation(&host);
            match phase {
                "stage" => host.fake(
                    "cp",
                    &format!(
                        "if [ \"${{1##*/}}\" = {notice} ]; then exit 74; fi; exec /bin/cp \"$@\""
                    ),
                ),
                "backup" => host.fake(
                    "cp",
                    &format!(
                        "case \"$*\" in *\".install-rollback.\"*\"/{notice}\") exit 74 ;; esac; exec /bin/cp \"$@\""
                    ),
                ),
                "swap" => host.fake(
                    "mv",
                    &format!(
                        "case \"$*\" in *\".{notice}.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\""
                    ),
                ),
                _ => unreachable!(),
            }
            let failed = host.install(&serve(served.path()).url, &[]);
            assert!(!failed.status.success(), "{notice} {phase}: {failed:?}");
            assert_previous_installation(&host);
            assert!(rollback_dirs(&host.prefix().join("share/dudes")).is_empty());
        }
    }
}

#[test]
fn failed_first_notice_swaps_leave_no_binaries_floor_or_notices() {
    for notice in NOTICES {
        let served = tempfile::tempdir().expect("a release directory");
        release(served.path(), &["aarch64-apple-darwin"]);
        let host = Host::new("Darwin", "arm64");
        host.fake(
            "mv",
            &format!("case \"$*\" in *\".{notice}.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\""),
        );
        let failed = host.install(&serve(served.path()).url, &[]);
        assert!(!failed.status.success(), "{notice} swap: {failed:?}");
        let bin = host.prefix().join("bin");
        let share = host.prefix().join("share/dudes");
        for binary in BINARIES {
            assert!(!bin.join(binary).exists());
            assert!(!bin.join(format!(".{binary}.new")).exists());
        }
        for notice in NOTICES {
            assert!(!share.join(notice).exists());
            assert!(!share.join(format!(".{notice}.new")).exists());
        }
        assert!(!share.join("floor").exists());
        assert!(!share.join("floor.new").exists());
        assert!(rollback_dirs(&share).is_empty());
    }
}

#[test]
fn a_retry_recovers_notices_after_notice_rollback_was_interrupted() {
    for interrupted in NOTICES {
        let served = tempfile::tempdir().expect("a release directory");
        release(served.path(), &["aarch64-apple-darwin"]);
        let url = serve(served.path()).url;
        let host = Host::new("Darwin", "arm64");
        previous_installation(&host);
        let share = host.prefix().join("share/dudes");
        host.fake(
            "mv",
            &format!(
                "case \"$*\" in *\"floor.new\"* | *\".dudes-restore.\"*\"/{interrupted}\"* | *\".install-rollback.\"*\"/{interrupted}\"*) exit 74 ;; esac; exec /bin/mv \"$@\""
            ),
        );
        host.fake(
            "cp",
            &format!(
                "case \"$2\" in *\".install-rollback.\"*\"/{interrupted}\") exit 74 ;; esac; exec /bin/cp \"$@\""
            ),
        );
        let failed = host.install(&url, &[]);
        assert!(!failed.status.success(), "rollback interrupted: {failed:?}");
        assert!(stderr(&failed).contains("recovery incomplete; rerun install.sh"));
        let backups = rollback_dirs(&share);
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].join(interrupted)).expect("the retryable notice"),
            format!("previous {interrupted}\n")
        );
        for notice in NOTICES {
            if notice != interrupted {
                assert_eq!(
                    std::fs::read_to_string(share.join(notice)).expect("the restored notice"),
                    format!("previous {notice}\n")
                );
            }
        }
        host.fake("mv", "exec /bin/mv \"$@\"");
        host.fake("cp", "exec /bin/cp \"$@\"");
        host.fake("curl", "exit 74");
        let recovered = host.install("http://127.0.0.1:9", &[]);
        assert!(!recovered.status.success(), "download fails after recovery");
        assert!(stderr(&recovered).contains("could not download"));
        assert_previous_installation(&host);
        assert!(rollback_dirs(&share).is_empty());
        host.fake("curl", "exec /usr/bin/curl \"$@\"");
        let retried = host.install(&url, &[]);
        assert!(retried.status.success(), "retry installs: {retried:?}");
        for notice in NOTICES {
            assert_eq!(
                std::fs::read_to_string(share.join(notice)).expect("the installed notice"),
                format!("{notice} 1.2.3 aarch64-apple-darwin\n")
            );
        }
    }
}

#[test]
fn legacy_binary_and_floor_backups_recover_without_changing_notices() {
    use std::os::unix::fs::PermissionsExt as _;

    let host = Host::new("Darwin", "arm64");
    previous_installation(&host);
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    let rollback = share.join(".install-rollback.previous");
    std::fs::create_dir(&rollback).expect("the interrupted backup");
    std::fs::set_permissions(&rollback, std::fs::Permissions::from_mode(0o700))
        .expect("the backup is owner-only");
    for binary in BINARIES {
        write(&rollback.join(binary), "previous binary");
        write(&bin.join(binary), "new binary");
    }
    write(&rollback.join("floor.was-present"), "");
    write(&rollback.join("ready"), "");
    std::fs::rename(share.join("floor"), share.join("floor.old")).expect("the old Floor");
    write(&share.join("floor/release-manifest.json"), "new Floor");
    host.fake("curl", "exit 74");

    let recovered = host.install("http://127.0.0.1:9", &[]);

    assert!(!recovered.status.success(), "download fails after recovery");
    assert!(stderr(&recovered).contains("could not download"));
    assert_previous_installation(&host);
    assert!(rollback_dirs(&share).is_empty());
    assert!(!share.join("floor.old").exists());
}

#[test]
fn staging_failure_keeps_the_previous_binaries_and_floor() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
    host.fake(
        "cp",
        "if [ \"${1##*/}\" = dudes-hook ]; then exit 74; fi; exec /bin/cp \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "staging failed: {failed:?}");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the prior binary"),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the prior Floor"),
        "previous Floor"
    );
    for binary in BINARIES {
        assert!(
            !bin.join(format!(".{binary}.new")).exists(),
            "EXIT removes a partly staged {binary}"
        );
    }
    assert!(!share.join("floor.new").exists());
    assert_previous_installation(&host);
}

#[test]
fn failed_backup_staging_cleans_its_incomplete_transaction() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    host.fake(
        "cp",
        "case \"$*\" in *\".install-rollback.\"*\"/dudes\"*) exit 74 ;; esac; exec /bin/cp \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(
        !failed.status.success(),
        "backup staging failed: {failed:?}"
    );
    assert!(rollback_dirs(&share).is_empty());
    assert!(!share.join("floor").exists());
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the previous binary"),
            "previous binary"
        );
    }
}

#[test]
fn failed_binary_swap_restores_all_previous_binaries_and_floor() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
    host.fake(
        "mv",
        "case \"$*\" in *\".dudes.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "swap was interrupted: {failed:?}");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the prior binary"),
            "previous binary",
            "{binary} rolled back after a later move failed"
        );
        assert!(!bin.join(format!(".{binary}.new")).exists());
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the prior Floor"),
        "previous Floor"
    );
    assert!(!share.join("floor.new").exists());
    assert_previous_installation(&host);
}

#[test]
fn failed_floor_swap_restores_the_previous_binaries_and_floor() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
    host.fake(
        "mv",
        "case \"$*\" in *\"floor.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "Floor swap failed: {failed:?}");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the previous binary"),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the previous Floor"),
        "previous Floor"
    );
    assert!(rollback_dirs(&share).is_empty());
    assert!(!share.join("floor.new").exists());
    assert_previous_installation(&host);
}

#[test]
fn failed_swap_preserves_a_previous_binary_symlink() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    let previous = host.home.path().join("previous-duded");
    write(&previous, "previous binary");
    std::os::unix::fs::symlink(&previous, bin.join("duded")).expect("prior symlink");
    for binary in ["dudes", "dudes-hook"] {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    host.fake(
        "mv",
        "case \"$*\" in *\".dudes.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "swap failed: {failed:?}");
    assert!(
        std::fs::symlink_metadata(bin.join("duded"))
            .expect("restored entry")
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_link(bin.join("duded")).unwrap(), previous);
    assert_eq!(
        std::fs::read_to_string(previous).unwrap(),
        "previous binary"
    );
    assert!(rollback_dirs(&share).is_empty());
}

#[test]
fn planted_rollback_entries_are_refused_before_recovery() {
    for symlinked in [true, false] {
        let host = Host::new("Darwin", "arm64");
        let share = host.prefix().join("share/dudes");
        std::fs::create_dir_all(&share).expect("share");
        let planted = share.join(".install-rollback.planted");
        let victim = host.home.path().join("victim");
        write(&victim, "planted secret control");
        if symlinked {
            std::os::unix::fs::symlink(&victim, &planted).expect("planted symlink");
        } else {
            std::fs::create_dir(&planted).expect("planted directory");
            write(&planted.join("unrelated"), "planted");
        }

        let refused = host.install("http://127.0.0.1:9", &[]);

        assert!(
            !refused.status.success(),
            "unsafe path was accepted: {refused:?}"
        );
        assert!(stderr(&refused).contains("installation backup has an unsafe entry"));
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "planted secret control"
        );
        assert!(
            std::fs::symlink_metadata(&planted).is_ok(),
            "the installer did not touch the planted path"
        );
    }
}

#[test]
fn a_planted_ready_transaction_cannot_remove_installed_binaries() {
    use std::os::unix::fs::PermissionsExt as _;

    for complete_but_public in [false, true] {
        let host = Host::new("Darwin", "arm64");
        let bin = host.prefix().join("bin");
        let share = host.prefix().join("share/dudes");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&share).expect("share");
        for binary in BINARIES {
            write(&bin.join(binary), "previous binary");
        }
        let planted = share.join(".install-rollback.planted");
        std::fs::create_dir(&planted).expect("planted directory");
        std::fs::set_permissions(
            &planted,
            std::fs::Permissions::from_mode(if complete_but_public { 0o755 } else { 0o700 }),
        )
        .expect("planted mode");
        write(&planted.join("ready"), "");
        if complete_but_public {
            for binary in BINARIES {
                write(&planted.join(format!("absent.{binary}")), "");
            }
            write(&planted.join("floor.was-absent"), "");
        }

        let refused = host.install("http://127.0.0.1:9", &[]);

        assert!(
            !refused.status.success(),
            "planted recovery ran: {refused:?}"
        );
        assert!(stderr(&refused).contains("installation backup has an unsafe entry"));
        for binary in BINARIES {
            assert_eq!(
                std::fs::read_to_string(bin.join(binary)).expect("untouched binary"),
                "previous binary"
            );
        }
        assert!(planted.exists(), "unsafe recovery was not deleted");
    }
}

#[test]
fn a_retry_recovers_backups_after_rollback_itself_was_interrupted() {
    use std::os::unix::fs::PermissionsExt as _;

    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
    host.fake(
        "mv",
        "case \"$*\" in *\".dudes.new\"* | *\".dudes-restore.\"* | *\".install-rollback.\"*\"/duded\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );
    host.fake(
        "cp",
        "case \"$*\" in *\"/bin/duded\") exit 74 ;; esac; exec /bin/cp \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);
    assert!(!failed.status.success(), "swap failed: {failed:?}");
    let backups = rollback_dirs(&share);
    assert_eq!(backups.len(), 1, "one retryable backup");
    assert_eq!(
        std::fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777,
        0o700,
        "the backup is owner-only"
    );
    assert!(
        restore_dirs(&bin).is_empty(),
        "failed recovery left a stage"
    );
    assert_eq!(
        std::fs::read_to_string(backups[0].join("duded")).expect("the retained backup"),
        "previous binary"
    );

    host.fake("mv", "exec /bin/mv \"$@\"");
    host.fake("cp", "exec /bin/cp \"$@\"");
    host.fake("curl", "exit 74");
    let retried = host.install("http://127.0.0.1:9", &[]);
    assert!(
        !retried.status.success(),
        "download failed after recovery: {retried:?}"
    );
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the recovered binary"),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the recovered Floor"),
        "previous Floor"
    );
    assert!(rollback_dirs(&share).is_empty());
    assert!(restore_dirs(&bin).is_empty());
    assert_previous_installation(&host);
}

#[test]
fn failed_rollback_moves_restore_the_previous_set_by_copy() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    let previous_daemon = host.home.path().join("previous-duded");
    write(&previous_daemon, "previous binary");
    std::fs::remove_file(bin.join("duded")).expect("replace old daemon with a symlink");
    std::os::unix::fs::symlink(&previous_daemon, bin.join("duded")).expect("old daemon symlink");
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    host.fake(
        "mv",
        "case \"$*\" in *\".dudes.new\"* | *\".dudes-restore.\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "swap failed: {failed:?}");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).expect("the previous binary"),
            "previous binary",
            "the failed move left a mixed binary set"
        );
    }
    assert!(
        std::fs::symlink_metadata(bin.join("duded"))
            .expect("restored daemon entry")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(bin.join("duded")).unwrap(),
        previous_daemon
    );
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the previous Floor"),
        "previous Floor"
    );
    assert!(rollback_dirs(&share).is_empty());
    assert!(restore_dirs(&bin).is_empty());
}

#[test]
fn an_existing_binary_directory_is_refused_before_staging() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(bin.join("dudes")).expect("directory at the binary path");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    write(&bin.join("dudes/sentinel"), "untouched");
    write(&bin.join(".duded.new"), "stale staged binary");
    write(
        &share.join("floor.new/release-manifest.json"),
        "stale staged Floor",
    );
    for binary in ["duded", "dudes-hook"] {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");

    let refused = host.install(&serve(served.path()).url, &[]);

    assert!(
        !refused.status.success(),
        "directory was accepted: {refused:?}"
    );
    assert!(stderr(&refused).contains("existing dudes path is not a binary file"));
    assert_eq!(
        std::fs::read_to_string(bin.join("dudes/sentinel")).unwrap(),
        "untouched"
    );
    for binary in ["duded", "dudes-hook"] {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).unwrap(),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json")).unwrap(),
        "previous Floor"
    );
    assert!(rollback_dirs(&share).is_empty());
    assert!(!bin.join(".duded.new").exists());
    assert!(!share.join("floor.new").exists());
}

#[test]
fn failed_first_install_swap_leaves_no_new_binaries_or_floor() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    host.fake(
        "mv",
        "case \"$*\" in *\".dudes.new\"*) exit 74 ;; esac; exec /bin/mv \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(!failed.status.success(), "swap failed: {failed:?}");
    for binary in BINARIES {
        assert!(!bin.join(binary).exists(), "new {binary} survived rollback");
        assert!(!bin.join(format!(".{binary}.new")).exists());
    }
    assert!(!share.join("floor").exists());
    assert!(!share.join("floor.new").exists());
    assert!(rollback_dirs(&share).is_empty());
}

#[test]
fn failed_first_install_after_floor_swap_removes_the_new_floor() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    host.fake(
        "rm",
        "case \"$*\" in *\"/ready\"*) exit 74 ;; esac; exec /bin/rm \"$@\"",
    );

    let failed = host.install(&serve(served.path()).url, &[]);

    assert!(
        !failed.status.success(),
        "commit cleanup failed: {failed:?}"
    );
    for binary in BINARIES {
        assert!(!bin.join(binary).exists(), "new {binary} survived rollback");
    }
    assert!(!share.join("floor").exists(), "new Floor survived rollback");
    for notice in NOTICES {
        assert!(
            !share.join(notice).exists(),
            "new {notice} survived rollback"
        );
    }
    assert_eq!(
        rollback_dirs(&share).len(),
        1,
        "failed cleanup keeps a retry marker"
    );
}

#[test]
fn the_next_run_cleans_staged_leftovers_even_when_download_fails() {
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor.new")).expect("staged Floor");
    for binary in BINARIES {
        write(&bin.join(format!(".{binary}.new")), "staged binary");
    }
    for notice in NOTICES {
        write(&share.join(format!(".{notice}.new")), "staged notice");
    }
    host.fake("curl", "exit 74");

    let failed = host.install("http://127.0.0.1:9", &[]);

    assert!(!failed.status.success(), "download failed: {failed:?}");
    assert!(!share.join("floor.new").exists());
    for binary in BINARIES {
        assert!(!bin.join(format!(".{binary}.new")).exists());
    }
    for notice in NOTICES {
        assert!(!share.join(format!(".{notice}.new")).exists());
    }
}

#[test]
fn a_retry_restores_the_previous_floor_before_a_failed_download() {
    let host = Host::new("Darwin", "arm64");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(share.join("floor.old")).expect("interrupted Floor swap");
    write(
        &share.join("floor.old/release-manifest.json"),
        "previous Floor",
    );
    host.fake("curl", "exit 74");

    let failed = host.install("http://127.0.0.1:9", &[]);

    assert!(!failed.status.success(), "download failed: {failed:?}");
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the restored Floor"),
        "previous Floor"
    );
    assert!(!share.join("floor.old").exists());
}

#[test]
fn each_host_gets_its_own_target() {
    let targets = [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
    ];
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &targets);
    let url = serve(served.path()).url;
    let cases = [
        ("Darwin", "arm64", None, "aarch64-apple-darwin"),
        ("Darwin", "x86_64", None, "x86_64-apple-darwin"),
        (
            "Darwin",
            "x86_64",
            Some(("sysctl", "echo 1")),
            "aarch64-apple-darwin",
        ),
        // A glibc host gets the static build too: the gnu archive needs the glibc it was built on.
        ("Linux", "aarch64", None, "aarch64-unknown-linux-musl"),
        ("Linux", "x86_64", None, "x86_64-unknown-linux-musl"),
        (
            "Linux",
            "aarch64",
            Some(("ldd", "echo 'musl libc (aarch64)' >&2; exit 1")),
            "aarch64-unknown-linux-musl",
        ),
        (
            "Linux",
            "x86_64",
            Some(("ldd", "echo 'musl libc (x86_64)' >&2; exit 1")),
            "x86_64-unknown-linux-musl",
        ),
    ];
    let probed_targets: BTreeSet<_> = cases.iter().map(|case| case.3).collect();
    let installable_targets: BTreeSet<_> = installer_targets()
        .expect("install.sh target list")
        .into_iter()
        .filter(|target| !target.ends_with("-gnu"))
        .collect();
    assert_eq!(probed_targets, installable_targets);
    for (system, machine, fake, target) in cases {
        let host = Host::new(system, machine);
        if let Some((tool, script)) = fake {
            host.fake(tool, script);
        }
        let output = host.install(&url, &[]);
        assert!(output.status.success(), "{system} {machine}: {output:?}");
        let ran = Command::new(host.prefix().join("bin/dudes"))
            .output()
            .expect("the installed dudes runs");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            format!("dudes {target}\n"),
            "{system} {machine} with {fake:?}"
        );
    }
}

#[test]
fn an_unsupported_platform_is_refused() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["x86_64-unknown-linux-gnu"]);
    let url = serve(served.path()).url;
    for (system, machine) in [("FreeBSD", "arm64"), ("Linux", "i686")] {
        let host = Host::new(system, machine);
        let output = host.install(&url, &[]);
        assert_refused(
            &output,
            &host,
            &format!("unsupported platform: {system} {machine}"),
        );
    }
}

#[test]
fn a_partial_download_fails_the_checksum() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let archive = served
        .path()
        .join(format!("dudes-{VERSION}-aarch64-apple-darwin.tar.gz"));
    let bytes = std::fs::read(&archive).expect("the archive");
    std::fs::write(&archive, &bytes[..bytes.len() / 2]).expect("a truncated download");
    let host = Host::new("Darwin", "arm64");

    let output = host.install(&serve(served.path()).url, &[]);

    assert_refused(
        &output,
        &host,
        &format!("checksum mismatch for dudes-{VERSION}-aarch64-apple-darwin.tar.gz"),
    );
}

#[test]
fn a_truncated_archive_installs_nothing() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let archive = served
        .path()
        .join(format!("dudes-{VERSION}-aarch64-apple-darwin.tar.gz"));
    let bytes = std::fs::read(&archive).expect("the archive");
    std::fs::write(&archive, &bytes[..bytes.len() / 2]).expect("a truncated archive");
    record_checksum(&served.path().join("SHA256SUMS"), VERSION, &archive)
        .expect("a matching checksum");
    let host = Host::new("Darwin", "arm64");

    let output = host.install(&serve(served.path()).url, &[]);

    assert_refused(
        &output,
        &host,
        &format!(
            "could not unpack dudes-{VERSION}-aarch64-apple-darwin.tar.gz; it is damaged or \
             incomplete"
        ),
    );
}

#[test]
fn a_target_or_version_missing_from_sha256sums_is_refused() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["x86_64-unknown-linux-gnu"]);
    let url = serve(served.path()).url;
    let host = Host::new("Darwin", "arm64");

    let latest = host.install(&url, &[]);
    assert_refused(
        &latest,
        &host,
        &format!("{url}/SHA256SUMS lists no release for aarch64-apple-darwin;"),
    );

    let linux = Host::new("Linux", "x86_64");
    let pinned = linux.install(&url, &["--version", "9.9.9"]);
    assert_refused(
        &pinned,
        &linux,
        &format!(
            "{url}/SHA256SUMS lists no release for x86_64-unknown-linux-musl at version 9.9.9"
        ),
    );
}

#[test]
fn root_installs_only_with_an_explicit_prefix() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let url = serve(served.path()).url;
    let host = Host::new("Darwin", "arm64");
    host.fake("id", "echo 0");

    let refused = host.install(&url, &[]);
    assert_refused(
        &refused,
        &host,
        "refusing to install as root without --prefix",
    );

    let chosen = host.home.path().join("opt");
    let chosen_arg = chosen.to_str().expect("a UTF-8 prefix");
    let allowed = host.install(&url, &["--prefix", chosen_arg]);
    assert!(allowed.status.success(), "{allowed:?}");
    assert!(chosen.join("bin/duded").is_file());
    for notice in NOTICES {
        assert_eq!(
            std::fs::read_to_string(chosen.join("share/dudes").join(notice))
                .expect("the chosen prefix notice"),
            format!("{notice} 1.2.3 aarch64-apple-darwin\n")
        );
    }
}

#[test]
fn missing_curl_or_checksum_tool_is_refused() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let url = serve(served.path()).url;

    let mut without_curl = Host::new("Darwin", "arm64");
    without_curl.only(&[]);
    let output = without_curl.install(&url, &[]);
    assert_refused(
        &output,
        &without_curl,
        "curl is required to download the release",
    );

    let mut without_hash = Host::new("Darwin", "arm64");
    without_hash.only(&["curl"]);
    let output = without_hash.install(&url, &[]);
    assert_refused(
        &output,
        &without_hash,
        "sha256sum or shasum is required to verify the download",
    );
}

#[test]
fn only_https_or_loopback_http_base_urls_are_accepted() {
    let host = Host::new("Darwin", "arm64");
    for url in ["http://example.com", "http://127.0.0.1.example.com"] {
        let output = host.install(url, &[]);
        assert_refused(
            &output,
            &host,
            "DUDES_INSTALL_BASE_URL must be https, or http on 127.0.0.1 or localhost",
        );
    }
    let output = host.install("http://user:s3cret@127.0.0.1:1", &[]);
    assert_refused(
        &output,
        &host,
        "DUDES_INSTALL_BASE_URL is refused: it carries credentials",
    );
    assert!(
        !stderr(&output).contains("s3cret"),
        "the refusal does not repeat the password: {}",
        stderr(&output)
    );
}

#[test]
fn a_loopback_base_url_follows_no_redirect() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let elsewhere = serve(served.path());
    let redirecting = start(Answer::RedirectTo(elsewhere.url.clone()));
    let host = Host::new("Darwin", "arm64");

    let output = host.install(&redirecting.url, &[]);

    assert_refused(
        &output,
        &host,
        &format!("could not download {}/SHA256SUMS", redirecting.url),
    );
    assert_eq!(redirecting.requested(), ["/SHA256SUMS"]);
    assert_eq!(elsewhere.requested(), Vec::<String>::new());
}

/// A `SHA256SUMS` line with a third field naming a path outside the temporary directory is
/// refused before the script writes anything but `SHA256SUMS`.
#[test]
fn a_sha256sums_line_with_a_third_field_is_refused_before_any_download() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let sums = served.path().join("SHA256SUMS");
    let line = std::fs::read_to_string(&sums).expect("the sums");
    let crafted = format!("{} ../../../victim/.profile\n", line.trim_end());
    assert_eq!(
        crafted,
        format!(
            "{}  dudes-{VERSION}-aarch64-apple-darwin.tar.gz ../../../victim/.profile\n",
            &line[..64]
        )
    );
    std::fs::write(&sums, crafted).expect("the crafted sums");
    write(&served.path().join("victim/.profile"), "planted");
    let site = serve(served.path());
    let host = Host::new("Darwin", "arm64");
    let victim = host.home.path().join("victim");
    std::fs::create_dir(&victim).expect("a directory the crafted path reaches");

    for args in [&[][..], &["--version", VERSION][..]] {
        let output = host.install(&site.url, args);
        assert_refused(
            &output,
            &host,
            &format!(
                "{}/SHA256SUMS has a malformed line for aarch64-apple-darwin; nothing was \
                 installed",
                site.url
            ),
        );
    }
    assert!(
        !victim.join(".profile").exists(),
        "nothing was written there"
    );
    assert_eq!(site.requested(), ["/SHA256SUMS", "/SHA256SUMS"]);
}

#[test]
fn a_version_that_is_not_semantic_is_refused() {
    let served = tempfile::tempdir().expect("a release directory");
    release_at(served.path(), "1.2", &["aarch64-apple-darwin"]);
    let site = serve(served.path());
    let host = Host::new("Darwin", "arm64");

    let latest = host.install(&site.url, &[]);
    assert_refused(
        &latest,
        &host,
        "SHA256SUMS has a malformed line for aarch64-apple-darwin",
    );

    let pinned = host.install(&site.url, &["--version", "1.2"]);
    assert_refused(&pinned, &host, "not a release version: 1.2");
    assert_eq!(site.requested(), ["/SHA256SUMS"]);
}

#[test]
fn failed_restore_continues_with_later_files_and_says_to_rerun() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    host.fake("mv", r#"case "$*" in *"floor.new"* | *".dudes-restore."*"/duded"* | *".install-rollback."*"/duded"*) exit 74 ;; esac; exec /bin/mv "$@""#);
    host.fake(
        "cp",
        r#"case "$2" in *".install-rollback."*"/duded") exit 74 ;; esac; exec /bin/cp "$@""#,
    );
    let failed = host.install(&serve(served.path()).url, &[]);
    assert!(!failed.status.success(), "Floor swap failed: {failed:?}");
    for binary in ["dudes", "dudes-hook"] {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).unwrap(),
            "previous binary",
            "later {binary} was restored"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json")).unwrap(),
        "previous Floor"
    );
    assert!(
        stderr(&failed).contains("rerun install.sh"),
        "actionable recovery: {failed:?}"
    );
    assert_eq!(
        rollback_dirs(&share).len(),
        1,
        "failed file remains retryable"
    );
}

#[test]
fn failed_restore_copy_falls_back_to_renaming_the_backup() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    host.fake(
        "mv",
        r#"case "$*" in *"floor.new"*) exit 74 ;; esac; exec /bin/mv "$@""#,
    );
    host.fake("cp", r#"case "$2" in *".install-rollback."*"/duded" | *"floor.old") exit 74 ;; esac; exec /bin/cp "$@""#);
    let failed = host.install(&serve(served.path()).url, &[]);
    assert!(!failed.status.success(), "Floor swap failed: {failed:?}");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).unwrap(),
            "previous binary"
        );
    }
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json")).unwrap(),
        "previous Floor"
    );
    assert!(
        rollback_dirs(&share).is_empty(),
        "successful recovery removes backup"
    );
}

#[test]
fn a_retry_preserves_renamed_backups_when_another_restore_failed() {
    let served = tempfile::tempdir().expect("a release directory");
    release(served.path(), &["aarch64-apple-darwin"]);
    let host = Host::new("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let share = host.prefix().join("share/dudes");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(share.join("floor")).expect("Floor");
    for binary in BINARIES {
        write(&bin.join(binary), "previous binary");
    }
    write(&share.join("floor/release-manifest.json"), "previous Floor");
    for notice in NOTICES {
        write(&share.join(notice), &format!("previous {notice}\n"));
    }
    host.fake("mv", r#"case "$*" in *"floor.new"* | *".install-rollback."*"/dudes-hook"*) exit 74 ;; esac; exec /bin/mv "$@""#);
    host.fake("cp", r#"case "$2" in *".install-rollback."*"/duded" | *".install-rollback."*"/dudes-hook" | *".install-rollback."*"/LICENSE-APACHE" | *"floor.old") exit 74 ;; esac; exec /bin/cp "$@""#);
    let failed = host.install(&serve(served.path()).url, &[]);
    assert!(!failed.status.success());
    let backup = rollback_dirs(&share);
    assert_eq!(backup.len(), 1);
    assert!(!backup[0].join("duded").exists(), "backup was renamed");
    assert!(
        !backup[0].join("LICENSE-APACHE").exists(),
        "notice backup was renamed"
    );
    assert_eq!(
        std::fs::read_to_string(bin.join("duded")).unwrap(),
        "previous binary"
    );
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json")).unwrap(),
        "previous Floor"
    );
    assert!(!share.join("floor.old").exists(), "Floor was renamed");
    host.fake("mv", "exec /bin/mv \"$@\"");
    host.fake("cp", "exec /bin/cp \"$@\"");
    host.fake("curl", "exit 74");
    let retried = host.install("http://127.0.0.1:9", &[]);
    assert!(!retried.status.success(), "download fails after recovery");
    for binary in BINARIES {
        assert_eq!(
            std::fs::read_to_string(bin.join(binary)).unwrap(),
            "previous binary"
        );
    }
    assert!(rollback_dirs(&share).is_empty());
    assert!(restore_dirs(&bin).is_empty());
    assert_previous_installation(&host);
}

/// A host whose `curl` fails the test run, so a local install proves it made no request.
fn offline_host(system: &str, machine: &str) -> Host {
    let host = Host::new(system, machine);
    host.fake("curl", "echo 'curl was called' >&2; exit 99");
    host
}

#[test]
fn from_installs_a_local_release_directory_without_the_network() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    let host = offline_host("Darwin", "arm64");
    let bin = host.prefix().join("bin");
    let from = local.path().to_str().expect("a UTF-8 path");

    let output = host.run(None, &["--from", from]);

    assert!(output.status.success(), "installed: {output:?}");
    let mode = |path: PathBuf| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("an installed path")
            .permissions()
            .mode()
            & 0o777
    };
    for binary in BINARIES {
        let ran = Command::new(bin.join(binary))
            .output()
            .expect("the installed binary runs");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            format!("{binary} aarch64-apple-darwin\n")
        );
        assert_eq!(mode(bin.join(binary)), 0o755);
    }
    let share = host.prefix().join("share/dudes");
    assert_eq!(
        std::fs::read_to_string(share.join("floor/release-manifest.json"))
            .expect("the installed Floor"),
        "{\"target\":\"aarch64-apple-darwin\"}"
    );
    assert_eq!(mode(share.join("floor/index.html")), 0o644);
    for notice in NOTICES {
        assert_eq!(
            std::fs::read_to_string(share.join(notice)).expect("the installed notice"),
            format!("{notice} 1.2.3 aarch64-apple-darwin\n")
        );
        assert_eq!(mode(share.join(notice)), 0o644);
    }
    assert!(stdout(&output).starts_with("Installed Dudes 1.2.3 for aarch64-apple-darwin:"));
}

#[test]
fn from_honors_version() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    let host = offline_host("Darwin", "arm64");
    let from = local.path().to_str().expect("a path");

    let missing = host.run(None, &["--from", from, "--version", "9.9.9"]);
    assert_refused(
        &missing,
        &host,
        "lists no release for aarch64-apple-darwin at version 9.9.9",
    );

    let pinned = host.run(None, &["--from", from, "--version", VERSION]);
    assert!(pinned.status.success(), "installed: {pinned:?}");
}

#[test]
fn from_refuses_a_checksum_mismatch_and_installs_nothing() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    let archive = local
        .path()
        .join(format!("dudes-{VERSION}-aarch64-apple-darwin.tar.gz"));
    let bytes = std::fs::read(&archive).expect("the archive");
    std::fs::write(&archive, &bytes[..bytes.len() / 2]).expect("a damaged archive");
    let host = offline_host("Darwin", "arm64");

    let output = host.run(None, &["--from", local.path().to_str().expect("a path")]);

    assert_refused(
        &output,
        &host,
        &format!("checksum mismatch for dudes-{VERSION}-aarch64-apple-darwin.tar.gz"),
    );
}

#[test]
fn from_refuses_a_missing_directory_or_missing_files() {
    let host = offline_host("Darwin", "arm64");
    let parent = tempfile::tempdir().expect("a parent");
    let missing = parent.path().join("absent");

    let none = host.run(None, &["--from", missing.to_str().expect("a path")]);
    assert_refused(&none, &host, "is not a directory");

    let empty = host.run(None, &["--from", parent.path().to_str().expect("a path")]);
    assert_refused(&empty, &host, "has no SHA256SUMS");

    let unnamed = host.run(None, &["--from"]);
    assert_refused(&unnamed, &host, "--from needs a value");
}

#[test]
fn from_refuses_a_base_url_alongside_it() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    let host = offline_host("Darwin", "arm64");

    let output = host.run(
        Some("https://example.com"),
        &["--from", local.path().to_str().expect("a path")],
    );

    assert_refused(
        &output,
        &host,
        "--from cannot be combined with DUDES_INSTALL_BASE_URL",
    );
}

#[test]
fn from_refuses_an_empty_value_instead_of_downloading() {
    let host = offline_host("Darwin", "arm64");

    let output = host.run(None, &["--from", ""]);

    assert_refused(&output, &host, "--from needs a non-empty directory");
    assert!(
        !stderr(&output).contains("could not download"),
        "no download was tried: {}",
        stderr(&output)
    );

    let with_url = host.run(Some("https://example.com"), &["--from", ""]);
    assert_refused(&with_url, &host, "--from needs a non-empty directory");
}

#[test]
fn from_ignores_cdpath_when_it_resolves_a_relative_directory() {
    let parent = tempfile::tempdir().expect("a parent");
    let local = parent.path().join("dist");
    std::fs::create_dir(&local).expect("a release directory");
    release(&local, &["aarch64-apple-darwin"]);
    let elsewhere = tempfile::tempdir().expect("another directory");
    std::fs::create_dir(elsewhere.path().join("dist")).expect("a decoy with the same name");
    let host = offline_host("Darwin", "arm64");

    let output = host.run_in(
        Some(elsewhere.path()),
        &[("CDPATH", parent.path())],
        None,
        &["--from", "dist"],
    );

    assert_refused(&output, &host, "has no SHA256SUMS");
    assert!(
        stderr(&output).contains(&format!(
            "{}/dist has no SHA256SUMS",
            elsewhere
                .path()
                .canonicalize()
                .expect("a real path")
                .display()
        )),
        "the decoy directory beside the working directory is the one used: {}",
        stderr(&output)
    );
}

#[test]
fn from_accepts_a_relative_path_a_path_with_spaces_and_a_symlink() {
    let parent = tempfile::tempdir().expect("a parent");
    let spaced = parent.path().join("release files");
    std::fs::create_dir(&spaced).expect("a directory with a space");
    release(&spaced, &["aarch64-apple-darwin"]);
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(&spaced, &link).expect("a symlink");
    for (dir, from) in [
        (None, spaced.to_str().expect("a path")),
        (Some(parent.path()), "release files"),
        (None, link.to_str().expect("a path")),
    ] {
        let host = offline_host("Darwin", "arm64");
        let output = host.run_in(dir, &[], None, &["--from", from]);
        assert!(
            output.status.success(),
            "installed from {from:?}: {output:?}"
        );
        assert!(host.prefix().join("bin/duded").is_file());
    }
}

#[test]
fn from_refuses_a_listed_archive_that_is_missing() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    std::fs::remove_file(
        local
            .path()
            .join(format!("dudes-{VERSION}-aarch64-apple-darwin.tar.gz")),
    )
    .expect("a missing archive");
    let host = offline_host("Darwin", "arm64");

    let output = host.run(None, &["--from", local.path().to_str().expect("a path")]);

    assert_refused(
        &output,
        &host,
        &format!("has no dudes-{VERSION}-aarch64-apple-darwin.tar.gz"),
    );
}

#[test]
fn from_needs_no_curl_on_the_path() {
    let local = tempfile::tempdir().expect("a release directory");
    release(local.path(), &["aarch64-apple-darwin"]);
    let mut host = Host::new("Darwin", "arm64");
    let real = std::env::var_os("PATH").unwrap_or_default();
    let hash = ["sha256sum", "shasum"]
        .into_iter()
        .find_map(|tool| {
            std::env::split_paths(&real)
                .map(|dir| dir.join(tool))
                .find(|path| path.is_file())
                .map(|path| (tool, path))
        })
        .expect("a checksum tool on this machine's PATH");
    host.only(&[
        "awk", "grep", "tar", "cp", "mv", "rm", "mkdir", "chmod", "mktemp", "find", "cat", "gzip",
    ]);
    host.fake(hash.0, &format!("exec {} \"$@\"", hash.1.display()));
    assert!(!host.fakes.join("curl").exists(), "curl is not on PATH");

    let output = host.run(None, &["--from", local.path().to_str().expect("a path")]);

    assert!(
        output.status.success(),
        "installed without curl: {output:?}"
    );
}
