//! A terminal child must not inherit the daemon's blocked stop signals.
//!
//! The daemon blocks `SIGTERM` on every thread, and a child inherits the mask of the thread that
//! spawns it. The shell below signals itself: with the mask cleared it dies on the spot and never
//! writes `after`; with `SIGTERM` still blocked the signal stays pending and it does.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use dudes_providers::launch::{LaunchInput, LaunchSpec};
use dudes_pty::{PtyHost, PtySize};
use nix::sys::signal::{SigSet, Signal};

#[test]
fn a_terminal_child_starts_with_the_stop_signal_unblocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let before = dir.path().join("before");
    let after = dir.path().join("after");
    let script = format!(
        "echo x > \"{}\"; kill -s TERM $$; echo x > \"{}\"",
        before.display(),
        after.display()
    );
    let spec = LaunchSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
        ],
        env: Vec::new(),
        cwd: dir.path().to_path_buf(),
        stdin: LaunchInput::Null,
    };

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.thread_block().expect("block SIGTERM on this thread");

    let mut host = PtyHost::new();
    let opened = host
        .open(spec, PtySize { cols: 80, rows: 24 })
        .expect("open the terminal child");
    wait_for(&before);
    sleep(Duration::from_millis(500));
    assert!(
        !after.exists(),
        "the child survived a SIGTERM sent to itself, so it started with SIGTERM blocked"
    );
    let _ = host.close(opened.id);
    let _ = host.shutdown();
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        sleep(Duration::from_millis(10));
    }
}
