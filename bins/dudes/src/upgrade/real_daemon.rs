//! The restart half of `dudes upgrade` against a real `duded`: the sign-ins a Floor tab holds
//! survive an upgrade and do not survive a plain stop and start.
//!
//! The installer step is replaced by writing a second Floor bundle beside the daemon, which is all
//! the installer changes that the Floor can see. Everything from the export to the import is the
//! shipped code, driven against two real daemon runs.

use std::io::Read as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dudes_proto::rpc::Method;
use dudes_proto::rpc::floor::SessionIssueResult;
use sha2::{Digest as _, Sha256};

use super::*;
use crate::env::Environment;
use crate::run::Lifecycle;

struct NoRunner;

impl Runner for NoRunner {
    fn run(&self, _: &Path, _: &[OsString], _: &[EnvVar], _: Duration) -> std::io::Result<Output> {
        Err(std::io::Error::other("the restart runs no installer"))
    }
}

fn built_daemon() -> PathBuf {
    let mut daemon = std::env::current_exe().expect("the test executable");
    daemon.pop();
    daemon.pop();
    daemon.push(format!("duded{}", std::env::consts::EXE_SUFFIX));
    assert!(
        daemon.is_file(),
        "{} is absent; run `cargo build -p duded --locked` before the filtered nextest command",
        daemon.display()
    );
    daemon
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// A complete Floor bundle whose one asset is named `app-<tag>.js`, so two tags are two builds.
fn write_bundle(dir: &Path, tag: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("assets")).expect("the bundle directory");
    let asset_path = format!("assets/app-{tag}.js");
    let asset = format!("console.log('{tag}');\n");
    let index =
        format!("<!doctype html><script type=\"module\" src=\"/floor/{asset_path}\"></script>");
    std::fs::write(dir.join(&asset_path), &asset).expect("the asset");
    std::fs::write(dir.join("index.html"), &index).expect("the index");
    let mut rows = [
        (asset_path.as_str(), asset.as_bytes(), "immutable"),
        ("index.html", index.as_bytes(), "no-store"),
    ];
    rows.sort_by_key(|row| row.0);
    let mut tree = Sha256::new();
    let mut files = Vec::new();
    for (path, bytes, cache) in rows {
        let digest = hex(&Sha256::digest(bytes));
        tree.update(path.as_bytes());
        tree.update([0]);
        tree.update(bytes.len().to_string().as_bytes());
        tree.update([0]);
        tree.update(digest.as_bytes());
        tree.update(b"\n");
        files.push(serde_json::json!({
            "path": path, "bytes": bytes.len(), "sha256": digest, "cache": cache
        }));
    }
    let manifest = serde_json::json!({
        "schema_version": 1,
        "base_path": "/floor/",
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "browser_wire_version": 3,
        "asset_tree_sha256": hex(&tree.finalize()),
        "files": files,
    });
    std::fs::write(
        dir.join("release-manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("the manifest"),
    )
    .expect("the manifest file");
}

fn http(origin: &str, request: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(origin).expect("the Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    let request = format!(
        "{request} HTTP/1.1\r\nHost: {origin}\r\nOrigin: http://{origin}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("the request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (head.to_owned(), body.to_owned())
}

struct Installed {
    root: tempfile::TempDir,
    env: Environment,
    floor: PathBuf,
    origin: String,
    cookie: String,
}

impl Installed {
    fn start() -> Self {
        let root = tempfile::tempdir().expect("a scratch install");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("the install directory");
        let daemon = bin.join(format!("duded{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(built_daemon(), &daemon).expect("copy duded");
        let floor = bin.join("floor");
        write_bundle(&floor, "aaaaaaaa");
        let home = root.path().join("home");
        std::fs::create_dir(&home).expect("a home");
        let variables = [
            ("HOME", home.into_os_string()),
            (
                "DUDES_RUNTIME_DIR",
                root.path().join("run").into_os_string(),
            ),
            (
                "DUDES_STATE_DIR",
                root.path().join("state").into_os_string(),
            ),
            ("DUDES_DAEMON", daemon.into_os_string()),
            (
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            ),
        ];
        let env = Environment::resolve(move |name| {
            variables
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| value.clone())
        });
        let mut installed = Self {
            root,
            env,
            floor,
            origin: String::new(),
            cookie: String::new(),
        };
        let version = Lifecycle(&installed.env)
            .start(&mut Vec::new())
            .expect("the daemon starts");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        installed.sign_in();
        installed
    }

    /// One browser's sign-in, as the Floor page does it: issue a link, exchange its bearer.
    fn sign_in(&mut self) {
        let credential = crate::credential::read_operator_credential(
            self.env.runtime_dir().expect("a runtime directory"),
        )
        .expect("the credential reads");
        let mut client = dudes_rpc::LocalClient::connect(self.env.socket().expect("a socket"))
            .expect("connect to the daemon");
        let response =
            crate::call::call_presenting(&mut client, Method::FloorSessionIssue, None, credential)
                .expect("floor.session.issue is answered");
        let issued: SessionIssueResult =
            serde_json::from_value(response.result.expect("an issue result")).expect("typed");
        issued
            .base_url
            .strip_prefix("http://")
            .expect("a loopback origin")
            .clone_into(&mut self.origin);
        let (head, _) = http(&self.origin, "POST /floor/session", &issued.bootstrap);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        head.lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
            .and_then(|rest| rest.split(';').next())
            .expect("a session cookie")
            .clone_into(&mut self.cookie);
    }

    fn build(&self) -> String {
        let (_, manifest) = http(&self.origin, "GET /floor/release-manifest.json", "");
        let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("a manifest");
        manifest["asset_tree_sha256"]
            .as_str()
            .expect("a tree hash")
            .to_owned()
    }

    fn recovers(&self) -> bool {
        let request = format!(
            "POST /floor/session/recover HTTP/1.1\r\nHost: {origin}\r\nOrigin: http://{origin}\r\nCookie: dudes_floor_session={cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            origin = self.origin,
            cookie = self.cookie
        );
        let mut stream = TcpStream::connect(&self.origin).expect("the Floor listener");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("a read timeout");
        stream.write_all(request.as_bytes()).expect("the request");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response.starts_with("HTTP/1.1 200 OK")
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            windows: false,
            current: "0.86.0".to_owned(),
            executable: self.root.path().join("bin/dudes"),
            temp_dir: self.root.path().to_path_buf(),
            child_env: Vec::new(),
            state_dir: self.root.path().join("state"),
            lock_wait: Duration::from_secs(10),
            daemon_override: None,
        }
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = Lifecycle(&self.env).stop(&mut Vec::new(), &mut Vec::new());
    }
}

#[test]
fn an_upgrade_keeps_the_open_sign_in_and_a_plain_restart_does_not() {
    let installed = Installed::start();
    let before = installed.build();
    assert!(!installed.cookie.is_empty());

    write_bundle(&installed.floor, "bbbbbbbb");
    let inputs = installed.inputs();
    let daemon = Lifecycle(&installed.env);
    let context = Context {
        inputs: &inputs,
        runner: &NoRunner,
        daemon: &daemon,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = restart(
        &context,
        env!("CARGO_PKG_VERSION"),
        &installed.root.path().join("prefix"),
        true,
        &mut out,
        &mut err,
    );
    let out = String::from_utf8(out).expect("text");
    assert!(
        code.is_ok(),
        "{code:?}\n{out}\n{}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        out.ends_with("Kept 1 of 1 Floor sign-ins. Reload an open Floor tab to continue.\n"),
        "{out}"
    );
    assert_ne!(
        installed.build(),
        before,
        "the new run serves the new build"
    );
    assert!(
        installed.recovers(),
        "the sign-in from before the upgrade reaches the new run"
    );

    let _ = Lifecycle(&installed.env).stop(&mut Vec::new(), &mut Vec::new());
    assert!(lifecycle::wait_for_unlock(
        &inputs.state_dir,
        inputs.lock_wait
    ));
    Lifecycle(&installed.env)
        .start(&mut Vec::new())
        .expect("a plain start");
    assert!(
        !installed.recovers(),
        "a plain stop and start signs the browser out"
    );
}

#[test]
fn an_upgrade_that_dies_between_the_stop_and_the_import_restores_nothing() {
    let installed = Installed::start();
    let daemon = Lifecycle(&installed.env);
    let exported = daemon.export_sign_ins().expect("the operator exports");
    assert_eq!(exported.sessions.len(), 1);
    daemon.stop(&mut Vec::new(), &mut Vec::new()).expect("stop");
    assert!(lifecycle::wait_for_unlock(
        &installed.root.path().join("state"),
        Duration::from_secs(10)
    ));
    daemon.start(&mut Vec::new()).expect("start");
    assert!(
        !installed.recovers(),
        "an export that was never imported signs nobody back in"
    );
    assert_eq!(
        daemon.import_sign_ins(&exported),
        Ok(1),
        "control: the same export does restore when it is imported"
    );
    assert!(installed.recovers());
}
