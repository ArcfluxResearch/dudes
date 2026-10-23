//! A managed leader's exit must not release its process-group ownership before its descendants stop.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{AdoptionKey, ProcessHost};
use rustix::process::{Pid, Signal};

const TREE: &str = r#"#!/bin/sh
set -eu
root=$2
case "$1" in
grandchild)
    trap '' TERM
    printf '%s' "$$" > "$root/grandchild.pid"
    n=0
    while :; do
        n=$((n + 1))
        printf '%s' "$n" > "$root/grandchild.beat"
        /bin/sleep 0.02
    done
    ;;
child)
    "$0" grandchild "$root" &
    printf '%s' "$$" > "$root/child.pid"
    wait
    ;;
leader)
    "$0" child "$root" &
    printf '%s' "$$" > "$root/leader.pid"
    wait
    ;;
esac
"#;

struct TreeCleanup {
    group: Pid,
    beat: PathBuf,
}

struct EndStreams(mpsc::Sender<ChildExit>);

struct EndStream(mpsc::Sender<ChildExit>);

impl ChildStreams for EndStreams {
    fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(EndStream(self.0.clone()))
    }
}

impl ChildStream for EndStream {
    fn line(&mut self, _text: &str) {}

    fn ended(self: Box<Self>, exit: ChildExit) {
        let _ = self.0.send(exit);
    }
}

impl Drop for TreeCleanup {
    fn drop(&mut self) {
        if common::beating(&self.beat) {
            let _ = rustix::process::kill_process_group(self.group, Signal::KILL);
        }
    }
}

fn start_tree(host: &mut RealHost, root: &Path) -> TreeCleanup {
    let script = root.join("owned-tree");
    fs::write(&script, TREE).expect("write helper");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable helper");
    let key = common::spawned(
        host.spawn(
            &LaunchSpec {
                argv: vec![script.into(), "leader".into(), root.into()],
                env: Vec::new(),
                cwd: root.to_path_buf(),
                stdin: dudes_providers::launch::LaunchInput::Null,
            },
            &common::no_install(),
        ),
        "spawn tree",
    );
    let leader = common::await_number(&root.join("leader.pid"));
    assert_eq!(leader, u64::from(key.pid));
    common::await_number(&root.join("child.pid"));
    common::await_number(&root.join("grandchild.pid"));
    let tree = TreeCleanup {
        group: Pid::from_raw(i32::try_from(key.pid).expect("pid fits")).expect("nonzero pid"),
        beat: root.join("grandchild.beat"),
    };
    assert!(
        common::beating_after_start(&tree.beat),
        "the grandchild runs before cleanup"
    );
    tree
}

#[test]
fn shutdown_stops_a_term_ignoring_grandchild_after_its_leader_exits() {
    let root = tempfile::tempdir().expect("tree directory");
    let (ends, ended) = mpsc::channel();
    let mut host =
        RealHost::watching_with_grace(Arc::new(EndStreams(ends)), Duration::from_millis(150));
    let tree = start_tree(&mut host, root.path());
    let _ = host.shutdown();
    assert!(
        !common::beating(&tree.beat),
        "the grandchild survived the leader's reap and daemon shutdown"
    );
    assert_eq!(
        ended
            .recv_timeout(Duration::from_secs(2))
            .expect("inherited stdout closed")
            .signal,
        Some(15)
    );
}

#[test]
fn a_leaders_exit_stops_its_term_ignoring_grandchild_without_shutdown() {
    let root = tempfile::tempdir().expect("tree directory");
    let mut host = RealHost::with_grace(Duration::from_millis(150));
    let tree = start_tree(&mut host, root.path());
    rustix::process::kill_process(tree.group, Signal::TERM).expect("terminate only the leader");
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !common::beating(&tree.beat),
        "the grandchild outlived its managed leader"
    );
    let _ = host.shutdown();
}
