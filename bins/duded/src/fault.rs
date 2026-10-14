//! The two faults the crash-recovery tests need, behind a feature this daemon does not ship with.
//!
//! # What they are
//!
//! Both wrap a [`ProcessHost`] and, on the *n*th launch, stop answering: each touches a marker file
//! and parks the calling thread for good. Every other call passes straight through. What separates
//! them is one line, and that line is the whole reason there are two:
//!
//! | | where it parks | on-disk state when the signal lands |
//! |---|---|---|
//! | [`StallBeforeSpawn`] | **before** delegating | a fsynced `TurnIssued`, **no child**, no `TurnSpawned` |
//! | [`StallAfterSpawn`] | after delegating, **before answering** | a fsynced `TurnIssued`, **a live child**, no `TurnSpawned` |
//!
//! # Why the tests need them, and why the daemon does not kill itself
//!
//! The post-append crash test kills `duded` between the fsynced `TurnIssued` append and the launch. That
//! window is real but microseconds wide — `Ctx::execute` appends and folds, then the `Spawn`
//! effect follows on the next pump — so an outside killer cannot land in it, and a harness that
//! retried until it did would be flaky by construction. Flaky tests are bugs, not retries.
//!
//! So these **widen a window** and do nothing else. They create no process of their own, kill
//! nothing and write no journal record; each stops the loop at the exact instant one arm names and
//! leaves the killing to whoever is watching the marker. The `SIGKILL` in a transcript is a real
//! external signal sent by a person or a script.
//!
//! On-disk state when the signal lands is precisely what each arm is about. For the first: a
//! `TurnIssued` that `Writer::append` fsynced and folded, and no `TurnSpawned`. Nothing has been
//! launched, so there is no orphan either — which matters on Unix, where a `SIGKILL`ed daemon
//! cannot reap anything.
//!
//! For the second the orphan **is** the point, and what makes the state reachable is an ordering in
//! the executor rather than anything here: `Ctx::execute` registers the child with the stream sink
//! and queues `Input::Spawned` only *after* the host has answered. A park inside the answer
//! therefore leaves a real child running whose pid no record names, on a session no vendor line can
//! confirm — the loop that would fold one is the parked thread. That is the state the post-`Spawn`
//! arm restarts on; what a re-issue then announces depends on the turn's session, and for a
//! turn with no vendor-confirmed session behind it that is a freshly minted one.
//!
//! # Why a feature and not an environment variable
//!
//! Because then the released binary does not contain it at all, and that is the compiler's
//! statement rather than a promise in a comment — the shape `dudes-rpc`'s `testing` feature already
//! sets for the permissive authorisers. `cargo build --release --workspace` does not enable it;
//! the crash arm builds `-p duded --features fault-injection` and says so in its own output. What
//! the two binaries differ by is this wrapper in `main.rs`'s seam composition, which cannot reach
//! the journal, the fold, replay or the decide loop.
//!
//! # What it is not
//!
//! Not a general fault-injection framework. Two faults, a counter and a marker file each; the
//! second exists because the claim that a post-`Spawn` crash leaves *two children in two
//! independent sessions* is about a real child, and the before-`Spawn` fault only reaches the
//! branch where no child exists.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, NativeBoundaryAcknowledgement,
    NativeBoundaryReceipt, OwnedTargets, ProcessHost, SpawnResult,
};

/// Announce the window and never leave it.
///
/// The marker is written before the park and its write is not conditional on succeeding: a watcher
/// that never sees it will time out and say so, which is a better failure than a daemon that parks
/// silently.
///
/// Park rather than sleep: no wakeup this thread takes should let the call complete, and `park` can
/// return spuriously, so the loop is the guarantee rather than a duration.
fn announce_and_park(marker: &Path) -> ! {
    if let Err(error) = std::fs::write(marker, format!("{}\n", std::process::id())) {
        tracing::error!(%error, marker = %marker.display(), "could not write the marker");
    }
    loop {
        std::thread::park();
    }
}

/// A [`ProcessHost`] that stops the loop before one chosen launch.
#[derive(Debug)]
pub struct StallBeforeSpawn<H: ProcessHost> {
    inner: H,
    /// Which launch to stall before, counting from 1.
    nth: u32,
    /// Touched immediately before parking, so a watcher learns the daemon is in the window rather
    /// than guessing at it. Its **appearance** is the synchronisation point.
    marker: PathBuf,
    /// Launches seen so far. Atomic because `spawn` takes `&mut self` but the field is read in a
    /// `Debug` that a panic handler may reach.
    seen: AtomicU32,
}

impl<H: ProcessHost> StallBeforeSpawn<H> {
    /// Wrap `inner`, stalling before its `nth` launch and announcing it by creating `marker`.
    pub const fn new(inner: H, nth: u32, marker: PathBuf) -> Self {
        Self {
            inner,
            nth,
            marker,
            seen: AtomicU32::new(0),
        }
    }
}

impl<H: ProcessHost> ProcessHost for StallBeforeSpawn<H> {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let seen = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
        if seen != self.nth {
            return self.inner.spawn(spec, install);
        }
        tracing::warn!(
            nth = self.nth,
            marker = %self.marker.display(),
            "fault-injection: stalling before this launch; the turn is journaled and no child exists"
        );
        announce_and_park(&self.marker)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        self.inner.owned_targets(limit)
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        self.inner.acknowledge_native_boundary_absence(key, receipt)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        self.inner.write_hooks(install)
    }
}

/// A [`ProcessHost`] that stops the loop after one chosen launch, with that launch's child alive.
///
/// The difference from [`StallBeforeSpawn`] is one statement — the delegation happens, and its
/// answer is never handed back. What the daemon does with an answer it never receives is the fault:
/// no `Input::Spawned`, so no `TurnSpawned`, so no record naming the pid or the session the child
/// was launched on (`Ctx::execute`). Nor any other record: the parked thread is the loop, and the
/// loop is the single writer, so whatever else arrives waits in a queue nobody drains.
///
/// The child itself is untouched and outlives the `SIGKILL`, because on Unix every child leads a
/// process group of its own and a killed daemon runs no exit path.
#[derive(Debug)]
pub struct StallAfterSpawn<H: ProcessHost> {
    inner: H,
    /// Which launch to stall after, counting from 1.
    nth: u32,
    /// Touched immediately after the child exists and immediately before parking, so a watcher
    /// that sees it knows both facts at once. Its **appearance** is the synchronisation point.
    marker: PathBuf,
    /// Launches seen so far, counted on the way in — a launch the inner host refuses is one this
    /// wrapper saw, exactly as in [`StallBeforeSpawn`], so the two arms number launches alike.
    seen: AtomicU32,
}

impl<H: ProcessHost> StallAfterSpawn<H> {
    /// Wrap `inner`, stalling after its `nth` launch and announcing it by creating `marker`.
    pub const fn new(inner: H, nth: u32, marker: PathBuf) -> Self {
        Self {
            inner,
            nth,
            marker,
            seen: AtomicU32::new(0),
        }
    }
}

impl<H: ProcessHost> ProcessHost for StallAfterSpawn<H> {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let seen = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
        // Before the arming check, and not inside its `else`: the child of the armed launch is the
        // whole subject of the arm, so a refusal here must surface as a refusal rather than as a
        // park on a window that never opened. The watcher then times out and says the daemon never
        // reached it, which is the true statement.
        let key = match self.inner.spawn(spec, install) {
            SpawnResult::Spawned(key) => key,
            other @ (SpawnResult::NotLaunched { .. } | SpawnResult::CleanupUnconfirmed { .. }) => {
                return other;
            }
        };
        if seen != self.nth {
            return SpawnResult::Spawned(key);
        }
        tracing::warn!(
            nth = self.nth,
            marker = %self.marker.display(),
            "fault-injection: stalling after this launch; the child is alive and no record names it"
        );
        announce_and_park(&self.marker)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        self.inner.owned_targets(limit)
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        self.inner.acknowledge_native_boundary_absence(key, receipt)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        self.inner.write_hooks(install)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use dudes_providers::launch::LaunchSpec;
    use dudes_supervisor::testing::FakeHost;

    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            argv: vec!["/usr/bin/true".into()],
            cwd: std::path::PathBuf::from("/"),
            env: Vec::new(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        }
    }

    fn spawned(result: SpawnResult, context: &str) -> AdoptionKey {
        match result {
            SpawnResult::Spawned(key) => key,
            SpawnResult::NotLaunched { reason } | SpawnResult::CleanupUnconfirmed { reason } => {
                panic!("{context}: {reason}")
            }
        }
    }

    /// Every launch but the armed one is handed straight through, and the counter is the launch
    /// number rather than an index. The arm this cannot exercise is the stall itself, which parks
    /// for good by design; what it does pin is the off-by-one that would arm the wrong launch and
    /// make the crash arm stop somewhere the transcript does not describe.
    #[test]
    fn every_launch_but_the_armed_one_is_delegated() {
        // Armed on the third: two launches must reach the inner host untouched.
        let mut host = StallBeforeSpawn::new(FakeHost::wall_clock_anchored(), 3, PathBuf::new());
        let install = ManagedInstall::default();
        spawned(host.spawn(&spec(), &install), "first launch");
        spawned(host.spawn(&spec(), &install), "second launch");
        assert_eq!(
            host.inner.spawned.len(),
            2,
            "both launches reached the real host"
        );
    }

    /// `0` means never, which is what an unset or unparseable environment variable resolves to.
    /// The counter starts at 1, so no launch can equal it — asserted rather than reasoned, because
    /// a wrapper that stalled on `0` would hang the daemon of anyone who built the feature.
    #[test]
    fn a_zero_arms_nothing() {
        let mut host = StallBeforeSpawn::new(FakeHost::wall_clock_anchored(), 0, PathBuf::new());
        let install = ManagedInstall::default();
        for _ in 0..5 {
            spawned(host.spawn(&spec(), &install), "a launch");
        }
        assert_eq!(host.inner.spawned.len(), 5);
    }

    /// The same two properties for the after-spawn fault: an unarmed launch is delegated *and its
    /// answer is handed back*, which is the half `StallBeforeSpawn` cannot have.
    #[test]
    fn every_launch_but_the_armed_one_is_answered() {
        let mut host = StallAfterSpawn::new(FakeHost::wall_clock_anchored(), 3, PathBuf::new());
        let install = ManagedInstall::default();
        let first = spawned(host.spawn(&spec(), &install), "first launch");
        let second = spawned(host.spawn(&spec(), &install), "second launch");
        assert_eq!(
            host.inner.spawned.len(),
            2,
            "both launches reached the host"
        );
        // Field by field, because `AdoptionKey` has no `PartialEq` on purpose (`host.rs`). One
        // field is enough here: two answers naming one pid would mean an answer was fabricated.
        assert_ne!(first.pid, second.pid, "each answer names its own child");
    }

    #[test]
    fn a_zero_arms_nothing_after_a_launch_either() {
        let mut host = StallAfterSpawn::new(FakeHost::wall_clock_anchored(), 0, PathBuf::new());
        let install = ManagedInstall::default();
        for _ in 0..5 {
            spawned(host.spawn(&spec(), &install), "a launch");
        }
        assert_eq!(host.inner.spawned.len(), 5);
    }

    /// A host that stops inside `spawn` until the test lets it through, so an ordering can be
    /// observed rather than raced. `entered` says the call is in flight; `release` lets it finish;
    /// `launched` counts the launches that completed, where the test can read it.
    struct Gated {
        inner: FakeHost,
        entered: Sender<()>,
        release: Receiver<()>,
        launched: Arc<AtomicU32>,
    }

    impl ProcessHost for Gated {
        fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
            self.entered.send(()).expect("the test is waiting");
            self.release.recv().expect("the test releases this call");
            let key = match self.inner.spawn(spec, install) {
                SpawnResult::Spawned(key) => key,
                other @ (SpawnResult::NotLaunched { .. }
                | SpawnResult::CleanupUnconfirmed { .. }) => return other,
            };
            self.launched.fetch_add(1, Ordering::SeqCst);
            SpawnResult::Spawned(key)
        }

        fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
            self.inner.kill(key)
        }

        fn alive(&self, key: &AdoptionKey) -> Liveness {
            self.inner.alive(key)
        }

        fn boot_identity(&self) -> HostBoot {
            self.inner.boot_identity()
        }

        fn shutdown(&mut self) -> io::Result<()> {
            self.inner.shutdown()
        }
    }

    /// **The property the post-`Spawn` arm rests on**: when the marker appears, the child already
    /// exists. Swap the two statements in `spawn` and this fails on the first assertion — the arm
    /// would otherwise kill a daemon in the *other* window and report on a child that was never
    /// created, which is the before-spawn branch wearing the after-spawn name.
    ///
    /// The gate is what makes it a measurement instead of a race: the launch is held *inside* the
    /// inner host, so "no marker yet" is read at a moment the wrapper cannot have passed.
    #[test]
    fn the_child_exists_before_the_window_is_announced() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let marker = dir.path().join("stalled");
        let (entered, entering) = channel();
        let (release, released) = channel();
        let launched = Arc::new(AtomicU32::new(0));
        let host = Gated {
            inner: FakeHost::wall_clock_anchored(),
            entered,
            release: released,
            launched: Arc::clone(&launched),
        };
        let mut host = StallAfterSpawn::new(host, 1, marker.clone());

        // The armed call never returns, so it runs on a thread of its own and is never joined.
        std::thread::spawn(move || {
            let _parked = host.spawn(&spec(), &ManagedInstall::default());
        });

        // Bounded, because the mutant this test exists to catch does not *fail* the assertion
        // below — it never reaches it. A wrapper that announced the window before delegating parks
        // its thread with the inner host never entered, and an unbounded `recv` would hang the
        // suite instead of reddening it. Measured with the two statements swapped: this is the
        // line the test fails on, in 10s, and it is the only one that fails.
        entering
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect(
                "the armed launch never reached the inner host: was the window announced first?",
            );
        assert!(
            !marker.exists(),
            "the window was announced before the child existed"
        );
        assert_eq!(
            launched.load(Ordering::SeqCst),
            0,
            "nothing has launched yet"
        );

        release.send(()).expect("the launch is let through");
        let mut waited = 0;
        while !marker.exists() {
            waited += 1;
            assert!(waited < 600, "the window was never announced");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            launched.load(Ordering::SeqCst),
            1,
            "the child that the window is about"
        );
    }
}
