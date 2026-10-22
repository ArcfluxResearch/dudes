//! Native and source-level checks for the Unix process-ancestry relation.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use dudes_host::{PeerRelation, RealHost};
#[cfg(target_os = "linux")]
use dudes_supervisor::host::ProcessHost;

#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

#[test]
fn r4_walk_uses_full_adoption_key_and_stops_on_cycle() {
    let source = fs::read_to_string(common::src_dir().join("peer.rs")).expect("read peer.rs");
    for required in [
        "BTreeSet",
        "left.pid == right.pid",
        "left.start_time == right.start_time",
        "left.boot.as_deref() == right.boot.as_deref()",
        "!visited.insert(current)",
    ] {
        assert!(
            source.contains(required),
            "the ancestry source lost `{required}`"
        );
    }
    for forbidden in ["for _ in 0..", "take(MAX", "MAX_HOPS"] {
        assert!(
            !source.contains(forbidden),
            "the ancestry walk gained a fixed cap through `{forbidden}`"
        );
    }

    #[cfg(target_os = "linux")]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut host = RealHost::with_grace(Duration::from_millis(200));
        let processes = host.peer_processes();
        let mark = dir.path().join("peer.pid");
        let key = common::spawned(
            host.spawn(
                &common::helper_spec(common::PARK, &mark, dir.path()),
                &common::no_install(),
            ),
            "spawn ancestry subject",
        );
        common::await_number(&mark);
        assert_eq!(
            processes.resolve(key.pid, std::slice::from_ref(&key)),
            PeerRelation::Matched(0),
            "a real Linux child matches its full start/boot key"
        );
        let mut mismatched = key.clone();
        mismatched.start_time = mismatched.start_time.wrapping_add(1);
        assert_eq!(
            processes.resolve(key.pid, &[mismatched]),
            PeerRelation::NoMatch,
            "a pid-only match is insufficient"
        );
        common::shutdown_retaining_incomplete(&mut host, "shutdown ancestry subject");
    }
}

fn call_order(source: &str) -> Result<(), &'static str> {
    let read = source
        .find("let buffered = read_process(pid);")
        .ok_or("missing buffered BSDInfo read")?;
    let validate = source
        .find("match validate_pidversion(pid, pidversion)")
        .ok_or("missing pidversion validation")?;
    if read >= validate {
        return Err("pidversion was validated before the BSDInfo row was buffered");
    }
    if source[validate..].contains("read_process(pid)") {
        return Err("the starting pid was read again after validation");
    }
    if !source[validate..].contains("Ok(true) => Read::Found(process)") {
        return Err("the buffered row is not consumed only on validation success");
    }
    Ok(())
}

#[test]
fn macos_reads_starting_row_before_pidversion_validation() {
    let source = fs::read_to_string(common::src_dir().join("macos.rs")).expect("read macos.rs");
    call_order(&source).expect("production macOS call order");

    let swapped = source.replacen(
        "let buffered = read_process(pid);",
        "match validate_pidversion(pid, pidversion);\nlet buffered = read_process(pid);",
        1,
    );
    assert!(
        call_order(&swapped).is_err(),
        "the swapped-order mutant passed"
    );

    let reread = source.replacen(
        "Ok(true) => Read::Found(process)",
        "Ok(true) => { let _ = read_process(pid); Read::Found(process) }",
        1,
    );
    assert!(
        call_order(&reread).is_err(),
        "the post-validation-read mutant passed"
    );
}
