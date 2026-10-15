//! One daemon-owned loopback listener serves only a verified static Floor shell.

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use duded::floor_gateway::{FloorStartFault, JOIN_BOUND, prepare};
use duded::ports::{ExitCause, Handle, Job};
use duded::settings::{FloorSettings, floor_from_values, installed_floor};
use sha2::{Digest as _, Sha256};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);
const EXPECTED_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
const STATIC_CONTRACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../floor/tests/fixtures/static-contract.json"
);

fn assert_shared_headers(response: &str, cache: &str) {
    assert!(response.contains("Connection: close\r\n"));
    assert!(response.contains(&format!("Cache-Control: {cache}\r\n")));
    assert!(response.contains(&format!("Content-Security-Policy: {EXPECTED_CSP}\r\n")));
    assert!(response.contains("X-Content-Type-Options: nosniff\r\n"));
    assert!(response.contains("Referrer-Policy: no-referrer\r\n"));
    assert!(response.contains("Cross-Origin-Resource-Policy: same-origin\r\n"));
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("access-control-allow")
    );
}

fn copy_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary release root");
    fs::create_dir(root.path().join("assets")).expect("asset directory");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        fs::copy(Path::new(FIXTURE).join(path), root.path().join(path)).expect("copy fixture file");
    }
    root
}

#[test]
fn only_generation_three_manifests_are_admitted() {
    let matched = copy_fixture();
    prepare(&floor(matched.path())).expect("the matched generation prepares");
    for wire in [1, 2, 4] {
        let root = copy_fixture();
        let path = root.path().join("release-manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["browser_wire_version"] = serde_json::json!(wire);
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let error = prepare(&floor(root.path())).expect_err("another generation must not prepare");
        assert!(format!("{error:#}").contains("browser wire version mismatch"));
    }
}

#[test]
fn old_and_mixed_generation_bundles_are_refused() {
    let old = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/floor-release-v1"
    ));
    let error = prepare(&floor(old)).expect_err("generation one is refused");
    assert!(format!("{error:#}").contains("version mismatch"));

    for replace in ["release-manifest.json", "assets/app-01234567.js"] {
        let mixed = copy_fixture();
        fs::copy(old.join(replace), mixed.path().join(replace)).unwrap();
        let error = prepare(&floor(mixed.path())).expect_err("mixed generations are refused");
        let reason = if replace == "release-manifest.json" {
            "version mismatch"
        } else {
            "SHA-256 mismatch"
        };
        assert!(format!("{error:#}").contains(reason), "{error:#}");
    }
}

#[test]
#[expect(
    clippy::disallowed_types,
    reason = "integration test starts the daemon binary, not an agent process"
)]
fn check_floor_runs_startup_admission_without_starting_the_daemon() {
    let root = copy_fixture();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_duded"))
            .arg("--check-floor")
            .arg(root.path())
            .env("HOME", root.path().join("absent-home"))
            .env("DUDES_STATE_DIR", root.path().join("index.html"))
            .output()
            .expect("the daemon admission command")
    };
    let admitted = run();
    assert!(
        admitted.status.success(),
        "{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    let paths: Vec<String> = serde_json::from_slice(&admitted.stdout).expect("admitted inventory");
    assert_eq!(
        paths,
        [
            "release-manifest.json",
            "index.html",
            "assets/app-01234567.js"
        ]
    );
    assert!(
        !root.path().join("absent-home").exists(),
        "no daemon state or configuration access"
    );
    let asset = root.path().join("assets/app-01234567.js");
    let mut bytes = fs::read(&asset).expect("asset bytes");
    bytes[0] ^= 1;
    fs::write(&asset, bytes).expect("same-size corrupt asset");
    let refused = run();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("asset"));
    assert!(refused.stdout.is_empty());
}

fn floor(root: &Path) -> FloorSettings {
    FloorSettings {
        assets_dir: root.to_path_buf(),
        port: Some(0),
    }
}

fn floor_on(root: &Path, port: u16) -> FloorSettings {
    FloorSettings {
        assets_dir: root.to_path_buf(),
        port: Some(port),
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    listener.local_addr().expect("reserved address").port()
}

fn daemon_handle() -> (Handle, mpsc::Receiver<Job>) {
    let (jobs, receiver) = mpsc::channel();
    (Handle::new(jobs), receiver)
}

fn clock() -> Arc<TestClock> {
    Arc::new(TestClock::new())
}

fn raw(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to Floor");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(request).expect("write request");
    stream.shutdown(Shutdown::Write).expect("finish request");
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset,
            "read response: {error}"
        );
    }
    response
}

fn response(addr: SocketAddr, method: &str, path: &str) -> Vec<u8> {
    raw(
        addr,
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("ASCII response")
}

fn replace_asset_path(root: &Path, path: &str) {
    let manifest = root.join("release-manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
            .expect("parse fixture");
    value["files"][0]["path"] = serde_json::Value::String(path.to_owned());
    fs::write(
        manifest,
        serde_json::to_vec_pretty(&value).expect("serialize manifest"),
    )
    .expect("write mutated manifest");
}

fn large_fixture() -> tempfile::TempDir {
    let root = copy_fixture();
    let path = "assets/large-12345678.js";
    let bytes = vec![b'x'; 4 * 1024 * 1024];
    fs::write(root.path().join(path), &bytes).expect("large response asset");
    let manifest = root.path().join("release-manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
            .expect("parse manifest");
    value["files"]
        .as_array_mut()
        .expect("file rows")
        .push(serde_json::json!({
            "path": path,
            "bytes": bytes.len(),
            "sha256": hex(&Sha256::digest(&bytes)),
            "cache": "immutable"
        }));
    let mut rows: Vec<_> = value["files"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            (
                row["path"].as_str().expect("path").to_owned(),
                row["bytes"].as_u64().expect("bytes"),
                row["sha256"].as_str().expect("sha").to_owned(),
            )
        })
        .collect();
    rows.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut tree = Sha256::new();
    for (path, bytes, sha) in rows {
        tree.update(path.as_bytes());
        tree.update([0]);
        tree.update(bytes.to_string().as_bytes());
        tree.update([0]);
        tree.update(sha.as_bytes());
        tree.update(b"\n");
    }
    value["asset_tree_sha256"] = serde_json::Value::String(hex(&tree.finalize()));
    fs::write(
        manifest,
        serde_json::to_vec_pretty(&value).expect("serialize manifest"),
    )
    .expect("write manifest");
    root
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut value, "{byte:02x}");
    }
    value
}

fn rejected(path: &str) {
    let root = copy_fixture();
    replace_asset_path(root.path(), path);
    let error = prepare(&floor(root.path())).expect_err("unsafe manifest path is refused");
    assert!(
        format!("{error:#}").contains("manifest path"),
        "path {path:?} was refused for the wrong reason: {error:#}"
    );
}

#[test]
fn manifest_paths_reject_cross_platform_escapes_and_parent_symlinks() {
    for path in [
        "../secret",
        "assets/../secret",
        "assets/%2e%2e/secret",
        "assets\\..\\secret",
        "C:secret",
        "C:\\secret",
        "\\\\server\\share",
        "\\rooted",
        "/absolute",
        "assets//app.js",
    ] {
        rejected(path);
    }

    let valid = copy_fixture();
    prepare(&floor(valid.path())).expect("valid hashed control prepares");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let linked_file = copy_fixture();
        let target = linked_file.path().join("target.js");
        fs::rename(linked_file.path().join("assets/app-01234567.js"), &target)
            .expect("move target");
        symlink(&target, linked_file.path().join("assets/app-01234567.js")).expect("link asset");
        assert!(
            prepare(&floor(linked_file.path())).is_err(),
            "a symlinked file is refused"
        );

        let linked_parent = copy_fixture();
        let outside = tempfile::tempdir().expect("outside assets");
        fs::copy(
            linked_parent.path().join("assets/app-01234567.js"),
            outside.path().join("app-01234567.js"),
        )
        .expect("copy outside asset");
        fs::remove_dir_all(linked_parent.path().join("assets")).expect("remove real parent");
        symlink(outside.path(), linked_parent.path().join("assets")).expect("link parent");
        assert!(
            prepare(&floor(linked_parent.path())).is_err(),
            "a symlinked parent is refused"
        );
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let linked_file = copy_fixture();
        let target = linked_file.path().join("target.js");
        fs::rename(linked_file.path().join("assets/app-01234567.js"), &target)
            .expect("move target");
        symlink_file(&target, linked_file.path().join("assets/app-01234567.js"))
            .expect("link asset");
        assert!(prepare(&floor(linked_file.path())).is_err());

        let linked_parent = copy_fixture();
        let outside = tempfile::tempdir().expect("outside assets");
        fs::copy(
            linked_parent.path().join("assets/app-01234567.js"),
            outside.path().join("app-01234567.js"),
        )
        .expect("copy outside asset");
        fs::remove_dir_all(linked_parent.path().join("assets")).expect("remove real parent");
        symlink_dir(outside.path(), linked_parent.path().join("assets")).expect("link parent");
        assert!(prepare(&floor(linked_parent.path())).is_err());
    }

    let _windows_shapes_are_kept_in_the_cross_platform_table = PathBuf::from("C:\\secret");
}

#[test]
fn floor_static_surface_has_exact_framing_csp_cache_and_no_inline_code() {
    let root = copy_fixture();
    let (handle, _jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let addr = gateway.addr();

    let shell = response(addr, "GET", "/floor/");
    let shell = text(&shell);
    assert!(shell.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(shell.contains("Content-Length: 83\r\n"));
    assert_shared_headers(shell, "no-store");
    assert!(shell.contains("Content-Type: text/html; charset=utf-8\r\n"));
    let body = shell.split_once("\r\n\r\n").expect("response head").1;
    let lowered = body.to_ascii_lowercase();
    assert!(!lowered.contains("<style"));
    assert!(!lowered.contains(" style="));
    assert!(!lowered.contains(" onclick="));
    assert_eq!(body.matches("<script").count(), 1);
    assert!(body.contains("src=\"/floor/assets/app-01234567.js\""));

    let manifest = text(&response(addr, "GET", "/floor/release-manifest.json")).to_owned();
    assert!(manifest.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_shared_headers(&manifest, "no-store");
    assert!(manifest.contains("Content-Type: application/json\r\n"));

    let asset = text(&response(addr, "GET", "/floor/assets/app-01234567.js")).to_owned();
    assert!(asset.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_shared_headers(&asset, "public, max-age=31536000, immutable");
    assert!(asset.contains("Content-Type: text/javascript; charset=utf-8\r\n"));

    for path in [
        "/floor/missing",
        "/floor/?query",
        "/floor/%2e",
        "/floor/%2f",
        "/floor/%5c",
        "/floor//",
        "/floor\\assets\\app-01234567.js",
        "/floor/session",
    ] {
        let refusal = text(&response(addr, "GET", path)).to_owned();
        assert!(
            refusal.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{path}: {refusal}"
        );
        assert!(refusal.contains("Content-Length: 0\r\n"));
        assert_shared_headers(&refusal, "no-store");
        assert!(refusal.ends_with("\r\n\r\n"));
        assert!(!refusal.contains(path));
    }

    let method = text(&response(addr, "POST", "/floor/")).to_owned();
    assert!(method.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
    assert_shared_headers(&method, "no-store");
    assert!(method.contains("Allow: GET\r\n"));
    assert!(method.contains("Content-Length: 0\r\n"));
    assert!(method.ends_with("\r\n\r\n"));

    gateway.stop();
}

fn refused_body_client(addr: SocketAddr, bytes: usize) -> TcpStream {
    let mut client = TcpStream::connect(addr).expect("refused client");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("response timeout");
    client
        .write_all(
            format!("POST /missing HTTP/1.1\r\nHost: floor\r\nContent-Length: {bytes}\r\n\r\n")
                .as_bytes(),
        )
        .expect("refused head");
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        client.read_exact(&mut byte).expect("strict refusal head");
        head.push(byte[0]);
    }
    let head = text(&head);
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
    assert_shared_headers(head, "no-store");
    assert!(head.contains("Content-Length: 0\r\n"));
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("half-close timeout");
    assert_eq!(client.read(&mut byte).expect("refusal write-side EOF"), 0);
    std::thread::sleep(Duration::from_millis(30));
    client
}

#[test]
fn refused_client_is_half_closed_before_bounded_stalled_body_cleanup() {
    let root = copy_fixture();
    let (handle, jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let probe = gateway.probe();
    let _client = refused_body_client(gateway.addr(), 4097);
    assert_eq!(
        probe.live_connections(),
        1,
        "the write-side EOF precedes request draining"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while probe.live_connections() != 0 {
        assert!(
            Instant::now() < deadline,
            "stalled body exceeded its cleanup bound"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(jobs.try_recv().is_err(), "discard dispatched no request");
    assert!(text(&response(gateway.addr(), "GET", "/floor/")).starts_with("HTTP/1.1 200 OK"));
    assert!(gateway.stop());
}

#[test]
fn refused_client_trickle_cannot_renew_the_cleanup_deadline() {
    let root = copy_fixture();
    let (handle, jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let probe = gateway.probe();
    let mut client = refused_body_client(gateway.addr(), 4097);
    assert_eq!(probe.live_connections(), 1, "the request is still draining");
    let deadline = Instant::now() + Duration::from_secs(1);
    while probe.live_connections() != 0 {
        assert!(
            Instant::now() < deadline,
            "trickle renewed the drain deadline"
        );
        let _ = client.write_all(b"x");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(jobs.try_recv().is_err(), "discard dispatched no request");
    assert!(gateway.stop());
}

#[test]
fn refused_client_over_byte_bound_still_completes_cleanup() {
    let root = copy_fixture();
    let (handle, jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let probe = gateway.probe();
    let mut client = refused_body_client(gateway.addr(), 70_000);
    assert_eq!(probe.live_connections(), 1, "the request is still draining");
    client
        .set_write_timeout(Some(Duration::from_secs(1)))
        .expect("body write bound");
    client
        .write_all(&vec![b'x'; 70_000])
        .expect("body larger than the drain byte bound");
    let deadline = Instant::now() + Duration::from_secs(1);
    while probe.live_connections() != 0 {
        assert!(
            Instant::now() < deadline,
            "over-bound body exceeded its cleanup bound"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(jobs.try_recv().is_err(), "discard dispatched no request");
    assert!(gateway.stop());
}

#[test]
fn gateway_stop_interrupts_a_refused_body_drain() {
    let root = copy_fixture();
    let (handle, jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let probe = gateway.probe();
    let _client = refused_body_client(gateway.addr(), 4097);
    assert_eq!(probe.live_connections(), 1, "the request is still draining");
    let started = Instant::now();
    assert!(gateway.stop(), "gateway joins after interrupting the drain");
    assert!(
        started.elapsed() < Duration::from_millis(125),
        "stop ignored the drain polling bound"
    );
    assert_eq!(probe.live_connections(), 0);
    assert!(jobs.try_recv().is_err(), "discard dispatched no request");
}

#[test]
fn idle_floor_listener_stops_within_floor_join_bound() {
    let root = copy_fixture();
    let port = free_port();
    let (handle, _jobs) = daemon_handle();
    let mut gateway = prepare(&floor_on(root.path(), port))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let started = Instant::now();
    gateway.stop();
    assert!(
        started.elapsed() < JOIN_BOUND,
        "idle stop exceeded {JOIN_BOUND:?}"
    );
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn partial_floor_request_stops_within_floor_join_bound() {
    let root = copy_fixture();
    let port = free_port();
    let (handle, _jobs) = daemon_handle();
    let mut gateway = prepare(&floor_on(root.path(), port))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let mut partial = TcpStream::connect(("127.0.0.1", port)).expect("partial client");
    partial
        .write_all(b"GET /floor/ HTTP/1.1\r\nHost:")
        .expect("partial head");
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    gateway.stop();
    assert!(
        started.elapsed() < JOIN_BOUND,
        "partial stop exceeded {JOIN_BOUND:?}"
    );
    let mut rest = Vec::new();
    let _ = partial.read_to_end(&mut rest);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn floor_start_failure_closes_listener_before_returning_error() {
    let root = copy_fixture();
    let occupied = TcpListener::bind(("127.0.0.1", 0)).expect("occupied listener");
    let occupied_port = occupied.local_addr().expect("occupied address").port();
    let (daemon, _jobs) = daemon_handle();
    let error = prepare(&floor_on(root.path(), occupied_port))
        .expect("prepare")
        .start(daemon, clock())
        .expect_err("occupied bind is refused");
    assert!(format!("{error:#}").contains("binding the Floor listener"));

    let withheld_port = free_port();
    let withheld = FloorStartFault::withhold_ready();
    let (daemon, _jobs) = daemon_handle();
    let started = Instant::now();
    let error = prepare(&floor_on(root.path(), withheld_port))
        .expect("prepare")
        .start_with_fault(daemon, clock(), &withheld)
        .expect_err("withheld readiness is refused");
    let message = format!("{error:#}");
    assert!(
        message.contains("readiness timed out"),
        "unexpected start error: {message}"
    );
    assert!(started.elapsed() < READY_BOUND_PLUS_JOIN);
    assert!(withheld.worker_ended());
    assert_eq!(withheld.unexpected_reports(), 0);
    assert!(TcpStream::connect(("127.0.0.1", withheld_port)).is_err());

    let worker_port = free_port();
    let worker = FloorStartFault::worker_first();
    let (daemon, jobs) = daemon_handle();
    let error = prepare(&floor_on(root.path(), worker_port))
        .expect("prepare")
        .start_with_fault(daemon, clock(), &worker)
        .expect_err("worker-first ordering is a startup error");
    assert!(format!("{error:#}").contains("ended before readiness"));
    assert!(worker.worker_ended());
    assert_eq!(worker.unexpected_reports(), 0);
    assert!(jobs.try_recv().is_err());
    assert!(TcpStream::connect(("127.0.0.1", worker_port)).is_err());

    let missing_worker_release = FloorStartFault::worker_release_timeout();
    let (daemon, jobs) = daemon_handle();
    let error = prepare(&floor_on(root.path(), free_port()))
        .expect("prepare")
        .start_with_fault(daemon, clock(), &missing_worker_release)
        .expect_err("missing worker release is a named startup failure");
    assert!(format!("{error:#}").contains("worker-first finalization timed out"));
    assert!(jobs.try_recv().is_err());

    let parent_port = free_port();
    let parent = FloorStartFault::parent_first();
    let (daemon, jobs) = daemon_handle();
    let mut gateway = prepare(&floor_on(root.path(), parent_port))
        .expect("prepare")
        .start_with_fault(daemon, clock(), &parent)
        .expect("parent commits readiness first");
    assert!(
        parent.wait_parent_release_observed(),
        "the worker observed the normal parent release"
    );
    assert!(matches!(
        jobs.recv_timeout(Duration::from_secs(1)),
        Ok(Job::Stop(ExitCause::FloorGatewayEnded))
    ));
    assert_eq!(parent.unexpected_reports(), 1);
    gateway.stop();

    let missing_parent_release = FloorStartFault::parent_release_timeout();
    let (daemon, jobs) = daemon_handle();
    let mut gateway = prepare(&floor_on(root.path(), free_port()))
        .expect("prepare")
        .start_with_fault(daemon, clock(), &missing_parent_release)
        .expect("parent commit remains observable");
    assert_eq!(
        missing_parent_release.wait_timeout_name(),
        Some("parent-first release timed out")
    );
    assert!(
        !missing_parent_release.wait_parent_release_observed(),
        "the withheld parent release was never observed"
    );
    assert!(matches!(
        jobs.recv_timeout(Duration::from_secs(1)),
        Ok(Job::Stop(ExitCause::FloorGatewayEnded))
    ));
    gateway.stop();

    let (daemon, _jobs) = daemon_handle();
    let mut valid = prepare(&floor_on(root.path(), withheld_port))
        .expect("prepare")
        .start(daemon, clock())
        .expect("released address starts normally");
    assert_eq!(valid.addr().port(), withheld_port);
    valid.stop();
}

const READY_BOUND_PLUS_JOIN: Duration = Duration::from_secs(4);

#[test]
fn floor_request_limits_and_deadlines_refuse_then_recover() {
    let root = copy_fixture();
    let (handle, _jobs) = daemon_handle();
    let mut gateway = prepare(&floor(root.path()))
        .expect("prepare")
        .start(handle, clock())
        .expect("start");
    let addr = gateway.addr();

    let request_line = |len: usize| {
        let prefix = "GET /";
        let suffix = " HTTP/1.1";
        assert!(len >= prefix.len() + suffix.len());
        format!(
            "{prefix}{}{suffix}",
            "a".repeat(len - prefix.len() - suffix.len())
        )
    };
    let at_line = raw(
        addr,
        format!("{}\r\nHost: floor\r\n\r\n", request_line(4096)).as_bytes(),
    );
    assert!(text(&at_line).starts_with("HTTP/1.1 404 Not Found"));
    let over_line = raw(
        addr,
        format!("{}\r\nHost: floor\r\n\r\n", request_line(4097)).as_bytes(),
    );
    assert!(text(&over_line).starts_with("HTTP/1.1 414 URI Too Long"));

    let with_total = |total: usize| {
        let prefix = "GET /missing HTTP/1.1\r\nHost: floor\r\nX-Pad: ";
        let suffix = "\r\n\r\n";
        format!(
            "{prefix}{}{suffix}",
            "a".repeat(total - prefix.len() - suffix.len())
        )
    };
    let at_head = raw(addr, with_total(16_384).as_bytes());
    assert!(text(&at_head).starts_with("HTTP/1.1 404 Not Found"));
    let over_head = raw(addr, with_total(16_385).as_bytes());
    assert!(text(&over_head).starts_with("HTTP/1.1 431 Request Header Fields Too Large"));

    let headers = |count: usize| {
        let mut request = String::from("GET /missing HTTP/1.1\r\nHost: floor\r\n");
        for index in 1..count {
            use std::fmt::Write as _;
            let _ = write!(&mut request, "X-{index}: a\r\n");
        }
        request.push_str("\r\n");
        request
    };
    assert!(text(&raw(addr, headers(64).as_bytes())).starts_with("HTTP/1.1 404 Not Found"));
    assert!(
        text(&raw(addr, headers(65).as_bytes()))
            .starts_with("HTTP/1.1 431 Request Header Fields Too Large")
    );

    assert_malformed_framing_refused_and_gateway_recovers(addr);

    let drip_addr = addr;
    let drip = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(drip_addr).expect("drip client");
        for byte in b"GET /floor/ HTTP/1.1\r\nHost: floor" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let mut output = Vec::new();
        let _ = stream.read_to_end(&mut output);
        output
    });
    let output = drip.join().expect("drip thread");
    assert!(output.is_empty(), "deadline refusal reflects no bytes");

    assert!(text(&response(addr, "GET", "/floor/")).starts_with("HTTP/1.1 200 OK"));
    gateway.stop();

    let large = large_fixture();
    let (handle, _jobs) = daemon_handle();
    let mut gateway = prepare(&floor(large.path()))
        .expect("prepare large fixture")
        .start(handle, clock())
        .expect("start large fixture");
    let addr = gateway.addr();
    let mut stalled = TcpStream::connect(addr).expect("non-reading client");
    stalled
        .write_all(b"GET /floor/assets/large-12345678.js HTTP/1.1\r\nHost: floor\r\n\r\n")
        .expect("request large response");
    std::thread::sleep(Duration::from_millis(650));
    assert!(
        text(&response(addr, "GET", "/floor/")).starts_with("HTTP/1.1 200 OK"),
        "a non-reading peer cannot hold the only server thread"
    );
    drop(stalled);
    gateway.stop();
}

fn assert_malformed_framing_refused_and_gateway_recovers(addr: SocketAddr) {
    for malformed in [
        b"GET /floor/ HTTP/1.1\nHost: floor\n\n".as_slice(),
        b"GET /floor/ HTTP/1.1\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: one\r\nHost: two\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nExpect: 100-continue\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nUpgrade: websocket\r\n\r\n",
        b"GET /floor/ HTTP/2.0\r\nHost: floor\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nContent-Length: 1\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        b"GET /floor/ HTTP/1.1\r\nHost: floor\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n",
    ] {
        let refused = raw(addr, malformed);
        assert!(text(&refused).starts_with("HTTP/1.1 400 Bad Request"));
        assert!(text(&refused).contains("Content-Length: 0\r\n"));
        assert!(text(&response(addr, "GET", "/floor/")).starts_with("HTTP/1.1 200 OK"));
    }
    let incomplete_at_cap = format!(
        "GET /floor/ HTTP/1.1\r\nHost: floor\r\nX: {}",
        "a".repeat(16_384)
    );
    let refused = raw(addr, incomplete_at_cap.as_bytes());
    assert!(text(&refused).starts_with("HTTP/1.1 431 Request Header Fields Too Large"));
    assert!(text(&response(addr, "GET", "/floor/")).starts_with("HTTP/1.1 200 OK"));
}

#[test]
fn manifest_and_catalog_limits_refuse_before_unbounded_read() {
    let root = copy_fixture();
    let manifest = root.path().join("release-manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
            .expect("parse manifest");
    value["files"][0]["bytes"] = serde_json::json!(64_u64 * 1024 * 1024 + 1);
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&value).expect("serialize"),
    )
    .expect("write manifest");
    let error =
        prepare(&floor(root.path())).expect_err("declared catalog overage is refused first");
    assert!(
        format!("{error:#}").contains("declared asset exceeds remaining catalog bytes"),
        "wrong refusal: {error:#}"
    );

    let manifest_over = copy_fixture();
    let file = fs::OpenOptions::new()
        .write(true)
        .open(manifest_over.path().join("release-manifest.json"))
        .expect("open manifest");
    file.set_len(1024 * 1024 + 1)
        .expect("sparse manifest limit+1");
    let error = prepare(&floor(manifest_over.path())).expect_err("manifest limit+1 is refused");
    assert!(format!("{error:#}").contains("release manifest exceeds"));
}

#[test]
fn hash_suffix_and_served_extensions_match_shared_contract() {
    let contract: serde_json::Value =
        serde_json::from_slice(&fs::read(STATIC_CONTRACT).expect("read shared contract"))
            .expect("parse shared contract");
    for path in contract["hashed_valid"]
        .as_array()
        .expect("valid hash cases")
    {
        assert!(duded::floor_gateway::hashed_asset_path(
            path.as_str().expect("path")
        ));
    }
    for path in contract["hashed_invalid"]
        .as_array()
        .expect("invalid hash cases")
    {
        assert!(!duded::floor_gateway::hashed_asset_path(
            path.as_str().expect("path")
        ));
    }
    for path in contract["served_valid"]
        .as_array()
        .expect("valid served cases")
    {
        assert!(duded::floor_gateway::served_content_type(path.as_str().expect("path")).is_some());
    }
    for path in contract["served_invalid"]
        .as_array()
        .expect("invalid served cases")
    {
        assert!(duded::floor_gateway::served_content_type(path.as_str().expect("path")).is_none());
    }
}

#[test]
fn floor_environment_is_typed_local_and_has_explicit_precedence() {
    assert_eq!(floor_from_values(None, None, None).expect("disabled"), None);
    let port_alone = floor_from_values(None, None, Some(OsString::from("9")))
        .expect_err("a port with no bundle is refused");
    assert!(
        port_alone.to_string().contains("DUDES_FLOOR_ASSETS"),
        "the refusal names what to set: {port_alone}"
    );
    assert!(floor_from_values(Some(OsString::from("relative")), None, None).is_err());
    let root = copy_fixture();
    for invalid in ["", "+1", "-1", " 1", "1 ", "65536", "one"] {
        assert!(
            floor_from_values(
                Some(root.path().as_os_str().to_owned()),
                None,
                Some(OsString::from(invalid)),
            )
            .is_err(),
            "invalid port {invalid:?}"
        );
    }
    let default = floor_from_values(Some(root.path().as_os_str().to_owned()), None, None)
        .expect("default port")
        .expect("enabled");
    assert_eq!(default.port, None);
    assert_eq!(default.assets_dir, root.path());
    let explicit = floor_from_values(
        Some(root.path().as_os_str().to_owned()),
        None,
        Some(OsString::from("43123")),
    )
    .expect("explicit port")
    .expect("enabled");
    assert_eq!(explicit.port, Some(43_123));
}

/// The bundle installed beside `duded` serves when `DUDES_FLOOR_ASSETS` is unset, and only then.
#[test]
fn an_installed_floor_bundle_serves_only_without_an_explicit_one() {
    let installed = copy_fixture();
    let explicit = copy_fixture();

    let fallback = floor_from_values(None, Some(installed.path().to_path_buf()), None)
        .expect("an installed bundle is accepted")
        .expect("and enabled");
    assert_eq!(fallback.assets_dir, installed.path());
    assert_eq!(fallback.port, None);

    let with_port = floor_from_values(
        None,
        Some(installed.path().to_path_buf()),
        Some(OsString::from("43123")),
    )
    .expect("a port applies to the installed bundle")
    .expect("enabled");
    assert_eq!(with_port.port, Some(43_123));

    let chosen = floor_from_values(
        Some(explicit.path().as_os_str().to_owned()),
        Some(installed.path().to_path_buf()),
        None,
    )
    .expect("explicit wins")
    .expect("enabled");
    assert_eq!(chosen.assets_dir, explicit.path());

    let absent = installed.path().join("not-installed");
    assert_eq!(
        floor_from_values(None, Some(absent), None).expect("no bundle is not an error"),
        None
    );
}

/// The uid that owns `dir`, which this test created: the uid the test runs as. Windows has none.
fn owner(dir: &Path) -> u32 {
    #[cfg(unix)]
    let uid = std::os::unix::fs::MetadataExt::uid(&fs::metadata(dir).expect("metadata"));
    #[cfg(not(unix))]
    let uid = {
        let _ = dir;
        0
    };
    uid
}

/// A `share/dudes/floor` directory made only its owner can write, whatever the test's umask.
fn shared_floor(prefix: &Path) -> PathBuf {
    let shared = prefix.join("share").join("dudes").join("floor");
    fs::create_dir_all(&shared).expect("the shared bundle directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("mode 0755");
    }
    shared
}

/// A Floor installed beside `duded` wins over one under its prefix's `share/dudes`, and neither
/// is found when neither directory exists.
#[test]
fn the_installed_floor_is_beside_duded_or_under_its_prefix() {
    let prefix = tempfile::tempdir().expect("a prefix");
    let me = owner(prefix.path());
    let exe = prefix.path().join("bin").join("duded");
    assert_eq!(installed_floor(&exe, me).expect("nothing unsafe"), None);

    let shared = shared_floor(prefix.path());
    assert_eq!(
        installed_floor(&exe, me).expect("a safe bundle"),
        Some(shared)
    );

    let beside = prefix.path().join("bin").join("floor");
    fs::create_dir_all(&beside).expect("the bundle directory beside duded");
    assert_eq!(installed_floor(&exe, me).expect("a bundle"), Some(beside));
}

/// `share/dudes/floor` counts only for a `duded` in a directory named `bin`: a `duded` in
/// `/tmp/x` must not serve whatever another user put in `/tmp/share/dudes/floor`.
#[test]
fn a_shared_floor_counts_only_for_a_duded_in_bin() {
    let prefix = tempfile::tempdir().expect("a prefix");
    let me = owner(prefix.path());
    let shared = shared_floor(prefix.path());

    assert_eq!(
        installed_floor(&prefix.path().join("x").join("duded"), me).expect("nothing unsafe"),
        None
    );
    assert_eq!(
        installed_floor(&prefix.path().join("bin").join("duded"), me).expect("a safe bundle"),
        Some(shared)
    );
}

/// A shared Floor its group or others can write is refused, by name.
#[cfg(unix)]
#[test]
fn a_shared_floor_others_can_write_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let prefix = tempfile::tempdir().expect("a prefix");
    let me = owner(prefix.path());
    let shared = shared_floor(prefix.path());
    let exe = prefix.path().join("bin").join("duded");
    for mode in [0o775, 0o757] {
        fs::set_permissions(&shared, fs::Permissions::from_mode(mode)).expect("a writable mode");
        let refused = installed_floor(&exe, me).expect_err("a writable bundle is refused");
        assert_eq!(
            refused.to_string(),
            format!(
                "the Floor bundle {} is writable by its group or others (mode {mode:04o}); run \
                 `chmod go-w` on it",
                shared.display()
            )
        );
    }
}

#[cfg(unix)]
#[test]
fn a_shared_floor_with_a_writable_parent_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let prefix = tempfile::tempdir().expect("a prefix");
    let me = owner(prefix.path());
    let shared = shared_floor(prefix.path());
    let exe = prefix.path().join("bin/duded");
    for parent in [
        prefix.path().to_path_buf(),
        prefix.path().join("share"),
        prefix.path().join("share/dudes"),
    ] {
        let original = fs::metadata(&parent)
            .expect("parent metadata")
            .permissions();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).expect("writable parent");
        let refused = installed_floor(&exe, me).expect_err("writable parent is refused");
        assert!(
            refused
                .to_string()
                .contains(&format!("{} is writable", parent.display())),
            "{refused}"
        );
        fs::set_permissions(&parent, original).expect("restore parent mode");
    }
    assert_eq!(installed_floor(&exe, me).expect("safe chain"), Some(shared));
}

/// A shared Floor that neither this user nor root owns is refused, by name; a root-owned one, as
/// a system-wide install leaves, is accepted.
#[cfg(unix)]
#[test]
fn a_shared_floor_another_user_owns_is_refused() {
    let prefix = tempfile::tempdir().expect("a prefix");
    let me = owner(prefix.path());
    let shared = shared_floor(prefix.path());
    let exe = prefix.path().join("bin").join("duded");

    let refused = installed_floor(&exe, me + 1).expect_err("another user's bundle is refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the Floor bundle {} is owned by uid {me}, not by this user or root, so another user \
             could replace the Floor; remove it and reinstall, or set DUDES_FLOOR_ASSETS",
            shared.display()
        )
    );

    let system = tempfile::tempdir().expect("a system prefix");
    fs::create_dir_all(system.path().join("share").join("dudes")).expect("share/dudes");
    let root_owned = system.path().join("share").join("dudes").join("floor");
    std::os::unix::fs::symlink("/", &root_owned).expect("a link to a root-owned directory");
    let system_exe = system.path().join("bin/duded");
    assert_eq!(
        installed_floor(&system_exe, me).expect("a root-owned bundle in a trusted prefix"),
        Some(root_owned)
    );
    let refused = installed_floor(&system_exe, me + 1)
        .expect_err("a root-owned Floor does not make its user-owned prefix safe");
    assert!(refused.to_string().contains("Floor ancestor"), "{refused}");
}

/// A release layout, with the fixture bundle installed as `floor` beside a copy of `duded`, serves
/// the Floor with no Floor variable set.
///
/// A copy, because the fallback is found relative to the running executable, and the build's own
/// `duded` has no `floor` beside it.
#[test]
fn a_copied_duded_serves_the_floor_installed_beside_it() {
    serves_the_floor_installed_at("duded", "floor", false);
}

/// The installed layout, with a copy of `duded` in `bin` and the fixture bundle in
/// `share/dudes/floor` under the same prefix, serves the Floor with no Floor variable set.
#[test]
fn a_duded_in_bin_serves_the_floor_under_share() {
    serves_the_floor_installed_at("bin/duded", "share/dudes/floor", false);
}

#[test]
fn a_running_daemon_keeps_serving_the_admitted_shared_floor_after_a_path_swap() {
    serves_the_floor_installed_at("bin/duded", "share/dudes/floor", true);
}

/// Copy `duded` and the fixture bundle to these paths under a fresh install directory, start the
/// copy with no Floor variable, and require it to serve the bundle.
fn serves_the_floor_installed_at(daemon: &str, bundle: &str, swap_after_start: bool) {
    let install = tempfile::tempdir().expect("an install directory");
    let state = tempfile::tempdir().expect("clean state");
    let runtime = tempfile::tempdir().expect("clean runtime");
    let home = tempfile::tempdir().expect("clean home");
    let daemon = install
        .path()
        .join(format!("{daemon}{}", std::env::consts::EXE_SUFFIX));
    fs::create_dir_all(daemon.parent().expect("a directory")).expect("the daemon's directory");
    fs::copy(env!("CARGO_BIN_EXE_duded"), &daemon).expect("copy duded");
    let bundle = install.path().join(bundle);
    fs::create_dir_all(bundle.join("assets")).expect("bundle directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755)).expect("mode 0755");
    }
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        fs::copy(Path::new(FIXTURE).join(path), bundle.join(path)).expect("copy fixture file");
    }
    let port = free_port();

    #[allow(
        clippy::disallowed_types,
        reason = "the harness starts the daemon binary; production src remains process-free"
    )]
    let mut command = std::process::Command::new(&daemon);
    command
        .env_clear()
        .env("HOME", home.path())
        .env("APPDATA", home.path().join("AppData"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DUDES_STATE_DIR", state.path())
        .env("DUDES_RUNTIME_DIR", runtime.path().join("run"))
        .env("DUDES_FLOOR_PORT", port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Winsock loads its providers from SystemRoot, and the Floor listener needs them.
    #[cfg(windows)]
    command.env(
        "SystemRoot",
        std::env::var_os("SystemRoot").expect("Windows has a SystemRoot"),
    );
    let mut daemon = command.spawn().expect("start the copied duded");

    let announced = first_stderr_line(&mut daemon);
    let socket = dudes_rpc::local_endpoint(runtime.path().join("run"), "duded.sock");
    assert!(
        announced.starts_with(&format!(
            "duded {} on {};",
            env!("CARGO_PKG_VERSION"),
            socket.display()
        )),
        "the startup line names the version and socket: {announced}"
    );
    assert!(
        announced.contains(&format!("Floor at http://127.0.0.1:{port}/floor/")),
        "the startup line names the Floor address: {announced}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let shell = loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.write_all(b"GET /floor/ HTTP/1.1\r\nHost: floor\r\n\r\n");
            let _ = stream.shutdown(Shutdown::Write);
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
            if text(&bytes).starts_with("HTTP/1.1 200 OK") {
                break bytes;
            }
        }
        if Instant::now() >= deadline || daemon.try_wait().expect("poll duded").is_some() {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!(
                "the copied duded did not serve the Floor installed at {}",
                bundle.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(text(&shell).contains("/floor/assets/app-01234567.js"));
    if swap_after_start {
        assert_admitted_floor_after_path_swap(&bundle, install.path(), port);
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}

fn assert_admitted_floor_after_path_swap(bundle: &Path, install: &Path, port: u16) {
    let saved = install.join("floor.saved");
    fs::rename(bundle, &saved).expect("swap out the Floor");
    fs::remove_dir_all(saved).expect("remove the old bundle");
    fs::create_dir_all(bundle.join("assets")).expect("replacement Floor path");
    for file in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        fs::write(bundle.join(file), "substituted Floor").expect("replacement content");
        let route = if file == "index.html" {
            "/floor/".to_owned()
        } else {
            format!("/floor/{file}")
        };
        let body = request_floor_body(port, &route);
        assert_eq!(
            body,
            fs::read(Path::new(FIXTURE).join(file)).expect("the admitted fixture bytes"),
            "the running daemon serves the admitted {file} after the path changes"
        );
    }
}

fn request_floor_body(port: u16, route: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the running Floor");
    write!(stream, "GET {route} HTTP/1.1\r\nHost: floor\r\n\r\n").expect("request the Floor again");
    stream.shutdown(Shutdown::Write).expect("end the request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("the response");
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK"),
        "{route} was refused"
    );
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP body");
    response[body + 4..].to_vec()
}

/// A daemon serving no Floor says so in its startup line, with what to set, and names its
/// operator file, absent here.
#[test]
fn a_daemon_without_a_floor_says_what_to_set_when_it_starts() {
    let state = tempfile::tempdir().expect("clean state");
    let runtime = tempfile::tempdir().expect("clean runtime");
    let home = tempfile::tempdir().expect("clean home");

    #[allow(
        clippy::disallowed_types,
        reason = "the harness starts the daemon binary; production src remains process-free"
    )]
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("APPDATA", home.path().join("AppData"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DUDES_STATE_DIR", state.path())
        .env("DUDES_RUNTIME_DIR", runtime.path().join("run"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut daemon = command.spawn().expect("start real duded");

    let announced = first_stderr_line(&mut daemon);
    let _ = daemon.kill();
    let _ = daemon.wait();
    let home = home.path().display().to_string();
    assert!(
        announced.contains(&format!("; config {home}")) && announced.contains(" (absent);"),
        "the startup line names the operator file and that it is absent: {announced}"
    );
    assert!(
        announced.ends_with(
            "; Floor off: install the built Floor as `floor` beside duded (`cargo xtask dist` \
             lays one out) or as `share/dudes/floor` beside duded's `bin` directory (install.sh \
             puts one there), or start duded directly with DUDES_FLOOR_ASSETS"
        ),
        "the startup line says the Floor is off and what to set: {announced}"
    );
}

/// The daemon's first line of standard error, or a failure within ten seconds.
fn first_stderr_line(daemon: &mut std::process::Child) -> String {
    use std::io::BufRead as _;

    let stderr = daemon.stderr.take().expect("standard error is piped");
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stderr).read_line(&mut line);
        let _ = sender.send(line);
    });
    let line = lines
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| {
            let _ = daemon.kill();
            panic!("duded wrote no startup line within ten seconds")
        });
    line.trim_end().to_owned()
}

#[test]
fn unexpected_floor_thread_end_stops_duded_and_closes_listeners() {
    let fixture = Fixture::new();
    let root = copy_fixture();
    let port = free_port();
    let mut settings = fixture.settings();
    settings.floor = Some(floor_on(root.path(), port));
    let fault = FloorStartFault::parent_first();
    let daemon = duded::boot::boot_with_floor_fault(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            std::sync::Arc::new(ScriptedRunner::default()),
            std::sync::Arc::new(TestClock::new()),
            duded::Witness::default(),
        ),
        Some(fault.clone()),
    )
    .expect("parent commits Floor readiness");
    let socket = settings.socket();
    assert!(socket.exists(), "the control door was bound");
    assert_eq!(daemon.floor_addr().expect("Floor address").port(), port);
    assert_eq!(daemon.run().expect("run"), ExitCause::FloorGatewayEnded);
    assert_eq!(fault.unexpected_reports(), 1);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(dudes_rpc::Client::connect(&socket).is_err());
}

#[test]
fn clean_host_single_duded_serves_bound_assets_on_loopback() {
    let state = tempfile::tempdir().expect("clean state");
    let runtime = tempfile::tempdir().expect("clean runtime");
    let home = tempfile::tempdir().expect("clean home");
    let port = free_port();
    let unrelated = TcpListener::bind(("127.0.0.1", 0)).expect("unrelated listener");
    let unrelated_port = unrelated.local_addr().expect("unrelated address").port();

    #[allow(
        clippy::disallowed_types,
        reason = "the harness starts the daemon binary; production src remains process-free"
    )]
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("APPDATA", home.path().join("AppData"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DUDES_STATE_DIR", state.path())
        .env("DUDES_RUNTIME_DIR", runtime.path().join("run"))
        .env("DUDES_FLOOR_ASSETS", Path::new(FIXTURE))
        .env("DUDES_FLOOR_PORT", port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Winsock loads its providers from SystemRoot, and the Floor listener needs them.
    #[cfg(windows)]
    command.env(
        "SystemRoot",
        std::env::var_os("SystemRoot").expect("Windows has a SystemRoot"),
    );
    let mut daemon = command.spawn().expect("start real duded");

    let deadline = Instant::now() + Duration::from_secs(10);
    let shell = loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.write_all(b"GET /floor/ HTTP/1.1\r\nHost: floor\r\n\r\n");
            let _ = stream.shutdown(Shutdown::Write);
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
            if text(&bytes).starts_with("HTTP/1.1 200 OK") {
                break bytes;
            }
        }
        assert!(
            Instant::now() < deadline,
            "real duded did not open the Floor listener"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(text(&shell).contains("/floor/assets/app-01234567.js"));
    assert_ne!(
        daemon.id(),
        std::process::id(),
        "duded is an independent process"
    );
    assert_ne!(port, unrelated_port);

    assert_eq!(listener_pid(port), daemon.id());
    assert_eq!(listener_pid(unrelated_port), std::process::id());
    assert_ne!(listener_pid(unrelated_port), daemon.id());

    let run_dir = runtime.path().join("run");
    let credential = std::fs::read_to_string(run_dir.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME))
        .expect("daemon credential");
    let credential =
        dudes_proto::rpc::cred::OperatorCredential::new(credential).expect("valid credential");
    let socket = dudes_rpc::local_endpoint(&run_dir, duded::settings::SOCKET_NAME);
    let mut client = dudes_rpc::Client::connect(socket).expect("control client");
    let stopped = client
        .call_as_operator(dudes_proto::rpc::Method::SystemShutdown, None, credential)
        .expect("shutdown response");
    assert!(stopped.error.is_none(), "shutdown accepted: {stopped:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.try_wait().expect("wait daemon") {
            assert!(status.success(), "daemon exit: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_types,
    reason = "the harness asks the OS which pid owns a listener; production src remains process-free"
)]
fn listener_pid(port: u16) -> u32 {
    let output = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()
        .expect("run lsof");
    assert!(output.status.success(), "lsof failed: {output:?}");
    let text = String::from_utf8(output.stdout).expect("lsof ASCII");
    let pids: Vec<u32> = text
        .lines()
        .filter_map(|line| line.strip_prefix('p'))
        .map(|pid| pid.parse().expect("pid"))
        .collect();
    assert_eq!(pids.len(), 1, "one owner for TCP {port}: {text}");
    pids[0]
}

#[cfg(target_os = "linux")]
fn listener_pid(port: u16) -> u32 {
    let table = fs::read_to_string("/proc/net/tcp").expect("read TCP table");
    let wanted = format!("0100007F:{port:04X}");
    let inode = table
        .lines()
        .skip(1)
        .find_map(|line| {
            let columns: Vec<_> = line.split_ascii_whitespace().collect();
            (columns.get(1) == Some(&wanted.as_str()) && columns.get(3) == Some(&"0A"))
                .then(|| columns.get(9).copied())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no listening inode for {wanted}"));
    let socket = format!("socket:[{inode}]");
    for process in fs::read_dir("/proc").expect("read processes").flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        if fds.flatten().any(|fd| {
            fs::read_link(fd.path()).is_ok_and(|target| target.to_string_lossy() == socket)
        }) {
            return pid;
        }
    }
    panic!("no process owns {socket}");
}

#[cfg(windows)]
#[allow(
    clippy::disallowed_types,
    reason = "the harness asks the OS which pid owns a listener; production src remains process-free"
)]
fn listener_pid(port: u16) -> u32 {
    let output = std::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .expect("run netstat");
    assert!(output.status.success(), "netstat failed: {output:?}");
    let wanted = format!("127.0.0.1:{port}");
    let text = String::from_utf8(output.stdout).expect("netstat text");
    let pids: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let columns: Vec<_> = line.split_ascii_whitespace().collect();
            (columns.len() >= 5
                && columns[0].eq_ignore_ascii_case("TCP")
                && columns[1] == wanted
                && columns[3].eq_ignore_ascii_case("LISTENING"))
            .then(|| columns[4].parse().expect("pid"))
        })
        .collect();
    assert_eq!(pids.len(), 1, "one owner for TCP {port}: {text}");
    pids[0]
}

#[test]
fn threaded_generation_refuses_old_and_mixed_asset_manifests_before_serving() {
    let old = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/floor-release-v2"
    ));
    let error = prepare(&floor(old)).expect_err("generation two assets are refused");
    assert!(format!("{error:#}").contains("version mismatch"));
    let matched = copy_fixture();
    prepare(&floor(matched.path())).expect("matching generation three fixture");
    let mixed = copy_fixture();
    let path = mixed.path().join("release-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["browser_wire_version"] = serde_json::json!(2);
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let error = prepare(&floor(mixed.path())).expect_err("mixed generation manifest is refused");
    assert!(format!("{error:#}").contains("browser wire"));
}
