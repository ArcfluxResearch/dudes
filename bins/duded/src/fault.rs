//! Native crash-test stops, compiled only with `fault-injection`.
//!
//! Spawn wrappers park before native launch or after a successful launch but before its answer.
//! Recovery stops select one exact work identity and one finite journal boundary. They create no
//! child, kill nothing, and append no journal record. The marker contains the daemon PID only;
//! journal positions and child identity must be observed separately.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, NativeBoundaryAcknowledgement,
    NativeBoundaryReceipt, NativeGroupPresence, OwnedTargets, ProbeAnswer, ProcessHost,
    SpawnResult,
};

/// Journal boundaries available to an instrumented recovery run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStopPoint {
    /// After preparation and command joining, before the Required write or any admission permit.
    BeforeRequired,
    /// After the Required commit and any automatic checkpoint, after guard drop, before projection.
    AfterRequired,
    /// After the exact command's Resume commit and any automatic checkpoint, after guard drop.
    AfterResume,
    /// After the Authorized commit and any automatic checkpoint, after guard drop.
    AfterAuthorized,
}

/// Exact work and command identity selected for a recovery stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryStopTarget {
    /// A startup Required boundary with no operator command.
    Startup {
        /// Exact interrupted work.
        work: dudes_proto::recovery::RecoveryWork,
    },
    /// A boundary joined to this admitted operator command.
    Command {
        /// Exact interrupted work.
        work: dudes_proto::recovery::RecoveryWork,
        /// Original command request ID.
        request_id: dudes_proto::record::Hex32,
    },
}

/// One armed recovery stop. No configuration means no stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryStop {
    point: RecoveryStopPoint,
    target: RecoveryStopTarget,
    marker: PathBuf,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryStopFields {
    point: RecoveryStopPoint,
    target: RecoveryStopTarget,
    marker: PathBuf,
}

impl RecoveryStop {
    /// Parse the native stop configuration without reading the environment or touching the marker.
    ///
    /// # Errors
    ///
    /// Refuses a present value unless it is at most 4,096 UTF-8 bytes, has the exact object shape,
    /// selects a supported point and target, and names an absolute marker path. Startup targets
    /// can select only Required boundaries.
    pub fn parse(value: Option<&std::ffi::OsStr>) -> anyhow::Result<Option<Self>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let value = value
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("recovery stop configuration must be UTF-8"))?;
        anyhow::ensure!(
            value.len() <= 4_096,
            "recovery stop configuration exceeds 4096 bytes"
        );
        let object: serde_json::Value = serde_json::from_str(value)
            .map_err(|_| anyhow::anyhow!("invalid recovery stop configuration"))?;
        anyhow::ensure!(
            object.is_object()
                && object
                    .get("target")
                    .is_some_and(serde_json::Value::is_object)
                && object
                    .get("target")
                    .and_then(|target| target.get("work"))
                    .is_some_and(serde_json::Value::is_object),
            "recovery stop configuration requires named object fields"
        );
        let fields: RecoveryStopFields = serde_json::from_str(value)
            .map_err(|_| anyhow::anyhow!("invalid recovery stop configuration fields"))?;
        let stop = Self {
            point: fields.point,
            target: fields.target,
            marker: fields.marker,
        };
        anyhow::ensure!(
            !matches!(stop.target, RecoveryStopTarget::Startup { .. })
                || matches!(
                    stop.point,
                    RecoveryStopPoint::BeforeRequired | RecoveryStopPoint::AfterRequired
                ),
            "recovery Resume and Authorized stops require an exact command"
        );
        anyhow::ensure!(
            stop.marker.is_absolute() && !stop.marker.as_os_str().as_encoded_bytes().contains(&0),
            "recovery stop marker must be an absolute path without NUL bytes"
        );
        Ok(Some(stop))
    }

    /// Whether this exact point, work and command identity select the armed stop.
    #[must_use]
    pub fn matches(&self, point: RecoveryStopPoint, target: RecoveryStopTarget) -> bool {
        self.point == point && self.target == target
    }

    /// Announce and park only when the exact point, work and command identity match.
    ///
    /// Call before acquiring any permit for `BeforeRequired`, or after a successful complete
    /// commit and guard drop for an `After` point. This callback receives no journal position.
    pub fn park_if_matches(&self, point: RecoveryStopPoint, target: RecoveryStopTarget) {
        if self.matches(point, target) {
            announce_and_park(&self.marker);
        }
    }
}

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

    fn probe_native_leader(&self, key: &AdoptionKey) -> ProbeAnswer {
        self.inner.probe_native_leader(key)
    }

    fn native_group_presence(&self, pid: u32) -> NativeGroupPresence {
        self.inner.native_group_presence(pid)
    }

    fn pid_namespace(&self) -> Option<String> {
        self.inner.pid_namespace()
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
/// This wrapper leaves the child untouched. Whether it outlives the `SIGKILL` is the host's doing:
/// a killed daemon runs no exit path, so on Unix only the keeper beside each child ends it, and a
/// host built without a keeper leaves it running.
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

    fn probe_native_leader(&self, key: &AdoptionKey) -> ProbeAnswer {
        self.inner.probe_native_leader(key)
    }

    fn native_group_presence(&self, pid: u32) -> NativeGroupPresence {
        self.inner.native_group_presence(pid)
    }

    fn pid_namespace(&self) -> Option<String> {
        self.inner.pid_namespace()
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

    #[test]
    fn before_spawn_wrapper_delegates_native_observations() {
        use crate::classify::native_observation_test_support::{Sentinel, assert_forwarded};
        let sentinel = Sentinel::new();
        let host = StallBeforeSpawn::new(sentinel.clone(), 1, PathBuf::new());
        assert_forwarded(&host, &sentinel);
        assert_eq!(host.seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn after_spawn_wrapper_delegates_native_observations() {
        use crate::classify::native_observation_test_support::{Sentinel, assert_forwarded};
        let sentinel = Sentinel::new();
        let host = StallAfterSpawn::new(sentinel.clone(), 1, PathBuf::new());
        assert_forwarded(&host, &sentinel);
        assert_eq!(host.seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_spawn_wrappers_and_peer_barrier_delegate_native_observations() {
        use crate::classify::native_observation_test_support::{Sentinel, assert_forwarded};
        let sentinel = Sentinel::new();
        let peer = crate::classify::PeerBarrier::new(
            sentinel.clone(),
            crate::classify::Resolver::fixed(Arc::new(crate::classify::NoWalk)),
        );
        let before = StallBeforeSpawn::new(peer, 1, PathBuf::new());
        let host = StallAfterSpawn::new(before, 1, PathBuf::new());
        assert_forwarded(&host, &sentinel);
        assert_eq!(host.seen.load(Ordering::SeqCst), 0);
        assert_eq!(host.inner.seen.load(Ordering::SeqCst), 0);
    }

    fn stop_value(point: &str, kind: &str) -> serde_json::Value {
        let mut target = serde_json::json!({
            "kind": kind,
            "work": {
                "agent": "00000000000000000000000001",
                "goal": "00000000000000000000000002",
                "turn": "00000000000000000000000003"
            }
        });
        if kind == "command" {
            target["request_id"] = serde_json::Value::String("11".repeat(32));
        }
        serde_json::json!({
            "point": point,
            "target": target,
            "marker": if cfg!(windows) { r"C:\recovery-stop" } else { "/recovery-stop" }
        })
    }

    fn parse_stop(value: &serde_json::Value) -> anyhow::Result<RecoveryStop> {
        RecoveryStop::parse(Some(std::ffi::OsStr::new(&value.to_string())))?
            .ok_or_else(|| anyhow::anyhow!("the fixture must arm a stop"))
    }

    #[test]
    fn recovery_stop_is_unarmed_only_when_configuration_is_absent() {
        assert_eq!(RecoveryStop::parse(None).unwrap(), None);
        for value in ["", "null", "false", "{}", "[]"] {
            assert!(RecoveryStop::parse(Some(std::ffi::OsStr::new(value))).is_err());
        }
    }

    #[test]
    fn recovery_stop_accepts_each_exact_command_stage() {
        for (name, point) in [
            ("before_required", RecoveryStopPoint::BeforeRequired),
            ("after_required", RecoveryStopPoint::AfterRequired),
            ("after_resume", RecoveryStopPoint::AfterResume),
            ("after_authorized", RecoveryStopPoint::AfterAuthorized),
        ] {
            let value = stop_value(name, "command");
            let stop = parse_stop(&value).unwrap();
            assert!(stop.matches(
                point,
                serde_json::from_value(value["target"].clone()).unwrap()
            ));
        }
    }

    #[test]
    fn recovery_stop_matches_only_exact_point_work_and_request() {
        let value = stop_value("after_resume", "command");
        let stop = parse_stop(&value).unwrap();
        let target: RecoveryStopTarget = serde_json::from_value(value["target"].clone()).unwrap();
        assert!(stop.matches(RecoveryStopPoint::AfterResume, target));
        for point in [
            RecoveryStopPoint::BeforeRequired,
            RecoveryStopPoint::AfterRequired,
            RecoveryStopPoint::AfterAuthorized,
        ] {
            assert!(!stop.matches(point, target));
        }
        for field in ["agent", "goal", "turn"] {
            let mut other = value["target"].clone();
            other["work"][field] = serde_json::json!("00000000000000000000000004");
            assert!(!stop.matches(
                RecoveryStopPoint::AfterResume,
                serde_json::from_value(other).unwrap()
            ));
        }
        let mut other = value["target"].clone();
        other["request_id"] = serde_json::json!("22".repeat(32));
        assert!(!stop.matches(
            RecoveryStopPoint::AfterResume,
            serde_json::from_value(other).unwrap()
        ));
        let startup = stop_value("after_required", "startup");
        assert!(!stop.matches(
            RecoveryStopPoint::AfterResume,
            serde_json::from_value(startup["target"].clone()).unwrap()
        ));
    }

    #[test]
    fn startup_recovery_stop_has_no_command_and_only_required_points() {
        for (name, point) in [
            ("before_required", RecoveryStopPoint::BeforeRequired),
            ("after_required", RecoveryStopPoint::AfterRequired),
        ] {
            let value = stop_value(name, "startup");
            let stop = parse_stop(&value).unwrap();
            let target = serde_json::from_value(value["target"].clone()).unwrap();
            assert!(stop.matches(point, target));
            let command = stop_value(name, "command");
            assert!(!stop.matches(
                point,
                serde_json::from_value(command["target"].clone()).unwrap()
            ));
        }
        for point in ["after_resume", "after_authorized"] {
            assert!(parse_stop(&stop_value(point, "startup")).is_err());
        }
        let mut value = stop_value("after_required", "startup");
        value["target"]["request_id"] = serde_json::json!("11".repeat(32));
        assert!(parse_stop(&value).is_err());
    }

    #[test]
    fn recovery_stop_parser_refuses_malformed_and_positional_fields() {
        let valid = stop_value("before_required", "command");
        assert!(parse_stop(&valid).is_ok());
        for path in [vec![], vec!["target"], vec!["target", "work"]] {
            let mut value = valid.clone();
            let mut object = &mut value;
            for field in path {
                object = &mut object[field];
            }
            object["extra"] = serde_json::json!(true);
            assert!(parse_stop(&value).is_err());
        }
        for field in ["point", "target", "marker"] {
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(parse_stop(&value).is_err());
        }
        let mut value = valid.clone();
        value["target"]
            .as_object_mut()
            .unwrap()
            .remove("request_id");
        assert!(parse_stop(&value).is_err());
        for request in [serde_json::Value::Null, serde_json::json!("AA".repeat(32))] {
            let mut value = valid.clone();
            value["target"]["request_id"] = request;
            assert!(parse_stop(&value).is_err());
        }
        for point in ["after_fsync", "after_permit", "resume", ""] {
            let mut value = valid.clone();
            value["point"] = serde_json::json!(point);
            assert!(parse_stop(&value).is_err());
        }
        for marker in ["", "relative-marker"] {
            let mut value = valid.clone();
            value["marker"] = serde_json::json!(marker);
            assert!(parse_stop(&value).is_err());
        }
        let mut value = valid.clone();
        value["marker"] =
            serde_json::json!(format!("{}\0marker", valid["marker"].as_str().unwrap()));
        assert!(parse_stop(&value).is_err());
        let mut value = valid.clone();
        value["target"]["work"] = serde_json::json!([
            "00000000000000000000000001",
            "00000000000000000000000002",
            "00000000000000000000000003"
        ]);
        assert!(parse_stop(&value).is_err());
        let mut value = valid.clone();
        value["target"] = serde_json::json!(["command", valid["target"]["work"], "11".repeat(32)]);
        assert!(parse_stop(&value).is_err());
        assert!(
            parse_stop(&serde_json::json!([
                valid["point"],
                valid["target"],
                valid["marker"]
            ]))
            .is_err()
        );
        let text = valid.to_string();
        for duplicate in [
            text.replacen('{', r#"{"point":"after_required","#, 1),
            text.replace(
                r#""kind":"command""#,
                r#""kind":"command","kind":"command""#,
            ),
            text.replace(
                r#""agent":"00000000000000000000000001""#,
                r#""agent":"00000000000000000000000001","agent":"00000000000000000000000001""#,
            ),
        ] {
            assert!(RecoveryStop::parse(Some(std::ffi::OsStr::new(&duplicate))).is_err());
        }
    }

    #[test]
    fn recovery_stop_limit_counts_original_utf8_configuration_bytes() {
        let value = stop_value("after_authorized", "command").to_string();
        let mut boundary = value.clone();
        boundary.push_str(&" ".repeat(4_096 - value.len()));
        assert!(RecoveryStop::parse(Some(std::ffi::OsStr::new(&boundary))).is_ok());
        boundary.push(' ');
        assert!(RecoveryStop::parse(Some(std::ffi::OsStr::new(&boundary))).is_err());
        let mut value = stop_value("after_authorized", "command");
        let marker = value["marker"].as_str().unwrap().to_owned();
        value["marker"] = serde_json::json!(format!("{marker}{}", "é".repeat(1_000)));
        assert!(parse_stop(&value).is_ok());
        value["marker"] = serde_json::json!(format!("{marker}{}", "é".repeat(2_048)));
        assert!(parse_stop(&value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_stop_refuses_present_non_utf8_configuration() {
        use std::os::unix::ffi::OsStrExt;
        assert!(RecoveryStop::parse(Some(std::ffi::OsStr::from_bytes(&[0xff]))).is_err());
    }

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
