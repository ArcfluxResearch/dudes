//! The restart half of `dudes upgrade` against a real `duded`: the sign-ins a Floor tab holds
//! survive an upgrade and do not survive a plain stop and start.
//!
//! The installer step is replaced by writing a second Floor bundle beside the daemon, which is all
//! the installer changes that the Floor can see. Everything from the export to the import is the
//! shipped code, run against two real daemon runs.
#![cfg(unix)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dudes::Environment;
use sha2::{Digest as _, Sha256};

fn built_daemon() -> PathBuf {
    let mut daemon = PathBuf::from(env!("CARGO_BIN_EXE_dudes"));
    daemon.set_file_name("duded");
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

fn http(origin: &str, request_line: &str, extra: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(origin).expect("the Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    let request = format!(
        "{request_line} HTTP/1.1\r\nHost: {origin}\r\nOrigin: http://{origin}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("the request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (head.to_owned(), body.to_owned())
}

struct Installed {
    _root: tempfile::TempDir,
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
        let daemon = bin.join("duded");
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
            _root: root,
            env,
            floor,
            origin: String::new(),
            cookie: String::new(),
        };
        let (code, out, err) = installed.cli(&["start"]);
        assert_eq!(code, 0, "{out}{err}");
        installed.sign_in();
        installed
    }

    fn cli(&self, words: &[&str]) -> (u8, String, String) {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = dudes::run(&argv, &self.env, None, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("text"),
            String::from_utf8(err).expect("text"),
        )
    }

    /// One browser's sign-in, as the Floor page does it: open a link, exchange its bearer.
    fn sign_in(&mut self) {
        let (code, out, err) = self.cli(&["floor", "open"]);
        assert_eq!(code, 0, "{out}{err}");
        let link = out.lines().next().expect("a sign-in link");
        let (address, bearer) = link
            .split_once("/floor/#bootstrap=")
            .expect("a bootstrap link");
        address
            .strip_prefix("http://")
            .expect("loopback")
            .clone_into(&mut self.origin);
        let (head, _) = http(
            &self.origin,
            "POST /floor/session",
            "Content-Type: text/plain;charset=UTF-8\r\n",
            bearer,
        );
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        head.lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
            .and_then(|rest| rest.split(';').next())
            .expect("a session cookie")
            .clone_into(&mut self.cookie);
    }

    fn build(&self) -> String {
        let (_, manifest) = http(&self.origin, "GET /floor/release-manifest.json", "", "");
        let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("a manifest");
        manifest["asset_tree_sha256"]
            .as_str()
            .expect("a tree hash")
            .to_owned()
    }

    fn recovers(&self) -> bool {
        let (head, _) = http(
            &self.origin,
            "POST /floor/session/recover",
            &format!("Cookie: dudes_floor_session={}\r\n", self.cookie),
            "",
        );
        head.starts_with("HTTP/1.1 200 OK")
    }

    fn plain_restart(&self) {
        let (code, out, err) = self.cli(&["stop"]);
        assert_eq!(code, 0, "{out}{err}");
        let (code, out, err) = self.cli(&["start"]);
        assert_eq!(code, 0, "{out}{err}");
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = self.cli(&["stop", "--hard"]);
    }
}

#[test]
fn an_upgrade_keeps_the_open_sign_in_and_a_plain_restart_does_not() {
    let installed = Installed::start();
    let before = installed.build();
    assert!(
        installed.recovers(),
        "control: the sign-in works before the upgrade"
    );

    write_bundle(&installed.floor, "bbbbbbbb");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dudes::run::upgrade_restart_for_test(
        &installed.env,
        env!("CARGO_PKG_VERSION"),
        &mut out,
        &mut err,
    );
    let out = String::from_utf8(out).expect("text");
    assert_eq!(code, Ok(0), "{out}\n{}", String::from_utf8_lossy(&err));
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

    installed.plain_restart();
    assert!(
        !installed.recovers(),
        "a plain stop and start signs the browser out"
    );
}

#[test]
fn a_restart_with_no_open_sign_in_says_nothing_about_sign_ins() {
    let mut installed = Installed::start();
    installed.plain_restart();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dudes::run::upgrade_restart_for_test(
        &installed.env,
        env!("CARGO_PKG_VERSION"),
        &mut out,
        &mut err,
    );
    let out = String::from_utf8(out).expect("text");
    assert_eq!(code, Ok(0), "{out}");
    assert!(!out.contains("sign-in"), "{out}");
    installed.sign_in();
    assert!(installed.recovers());
}

#[test]
fn an_upgrade_that_dies_after_the_export_and_before_the_import_restores_nothing() {
    use dudes_proto::rpc::CurrentMethod;

    let installed = Installed::start();
    let credential = dudes::read_operator_credential(installed.env.runtime_dir().expect("runtime"))
        .expect("the credential reads");
    let mut client = dudes_rpc::LocalClient::connect(installed.env.socket().expect("socket"))
        .expect("connect to the daemon");
    let exported = client
        .call_current_presenting(CurrentMethod::FloorSessionExport, None, credential)
        .expect("export is answered")
        .result
        .expect("the operator exports");
    assert_eq!(exported["sessions"].as_array().map(Vec::len), Some(1));
    installed.plain_restart();
    assert!(
        !installed.recovers(),
        "an export that never reached an import signs nobody back in"
    );
}
