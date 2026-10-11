use std::time::{Duration, Instant};

use dudes_proto::rpc::closing::{CLOSING_GRACE_SECONDS, ClosingStarted, ClosingStatus};
use dudes_types::{AgentId, Lsn, Timestamp};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Expiry {
    monotonic: Instant,
    wall: Timestamp,
}

impl Expiry {
    fn remaining_ms(self, now: Timestamp) -> u64 {
        self.remaining_ms_at(now, Instant::now())
    }

    fn remaining_ms_at(self, now: Timestamp, monotonic_now: Instant) -> u64 {
        let monotonic = self.monotonic.saturating_duration_since(monotonic_now);
        let wall =
            u64::try_from(self.wall.unix_millis().saturating_sub(now.unix_millis())).unwrap_or(0);
        u64::try_from(monotonic.as_millis())
            .unwrap_or(u64::MAX)
            .min(wall)
            .min(CLOSING_GRACE_SECONDS * 1000)
    }

    fn expired_at(self, now: Timestamp, monotonic_now: Instant) -> bool {
        monotonic_now >= self.monotonic || now >= self.wall
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FloorClosing {
    pub(crate) phase: dudes_proto::ClosingPhase,
    pub(crate) progress: dudes_proto::projection::ClosingProgress,
    expiry: Option<Expiry>,
}

impl FloorClosing {
    pub(crate) fn progress(self, now: Timestamp) -> dudes_proto::projection::ClosingProgress {
        let mut progress = self.progress;
        if let Some(expiry) = self.expiry {
            progress.remaining_ms = expiry.remaining_ms(now);
        }
        progress
    }

    pub(crate) fn same_counts(self, other: Self) -> bool {
        (
            self.progress.acknowledged,
            self.progress.settled,
            self.progress.owed,
            self.progress.running,
        ) == (
            other.progress.acknowledged,
            other.progress.settled,
            other.progress.owed,
            other.progress.running,
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Request {
    Announce,
    Ack {
        announcement: Lsn,
        agent: AgentId,
    },
    Status {
        announcement: Lsn,
        after: Option<AgentId>,
    },
    AfterResponse,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::{ClosingPhase, Entry, Record};

    fn announced(world: &mut dudes_supervisor::World) {
        world.apply(
            Lsn(7),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::ClosingTime {
                    phase: ClosingPhase::Announced,
                },
            },
        );
    }

    #[test]
    fn remaining_uses_the_same_earlier_deadline_and_restored_closing_gets_no_time() {
        let mut world = dudes_supervisor::World::default();
        let mut live = Waiting::new(&world);
        announced(&mut world);
        let session = world.closing.as_ref().unwrap();
        live.observe(session);
        assert!(
            (359_000..=360_000)
                .contains(&live.remaining_ms(Timestamp::UNIX_EPOCH, session.announced_at))
        );
        assert_eq!(
            live.remaining_ms(Timestamp::from_unix_millis(359_500), session.announced_at),
            500
        );
        assert_eq!(
            live.remaining_ms(Timestamp::from_unix_millis(360_001), session.announced_at),
            0
        );
        assert!(live.expired(Timestamp::from_unix_millis(360_001), session.announced_at));
        let mut restored = Waiting::new(&world);
        restored.observe(session);
        assert_eq!(
            restored.remaining_ms(Timestamp::UNIX_EPOCH, session.announced_at),
            0
        );
        assert!(restored.expired(Timestamp::UNIX_EPOCH, session.announced_at));
    }

    #[test]
    fn displayed_zero_does_not_expire_a_positive_fraction_of_a_millisecond() {
        let start = Instant::now();
        let expiry = Expiry {
            monotonic: start + Duration::from_micros(500),
            wall: Timestamp::from_unix_millis(1),
        };
        assert_eq!(expiry.remaining_ms_at(Timestamp::UNIX_EPOCH, start), 0);
        assert!(!expiry.expired_at(Timestamp::UNIX_EPOCH, start));
        assert!(expiry.expired_at(Timestamp::UNIX_EPOCH, expiry.monotonic));
        assert!(expiry.expired_at(Timestamp::from_unix_millis(1), start));
    }
}

#[derive(Debug)]
pub(crate) enum Answer {
    Announced(ClosingStarted),
    Status(ClosingStatus),
    Ack,
}

pub(crate) struct Waiting {
    pub(crate) grace: Duration,
    pub(crate) announcement: Option<Lsn>,
    pub(crate) deadline: Option<Instant>,
    /// This run inherited an incomplete closure from the journal rather than announcing its own
    /// (a crash receives no fresh saving time). `observe` reads this once, to
    /// give that inherited closure a zero monotonic deadline instead of a fresh `grace`.
    pub(crate) restored: bool,
    pub(crate) responses: usize,
    pub(crate) report_written: bool,
}

impl Waiting {
    pub(crate) fn new(world: &dudes_supervisor::World) -> Self {
        Self {
            grace: Duration::from_secs(CLOSING_GRACE_SECONDS),
            announcement: None,
            deadline: None,
            restored: world
                .closing
                .as_ref()
                .is_some_and(|session| !session.is_complete()),
            responses: 0,
            report_written: false,
        }
    }

    /// Arm the monotonic half of the deadline the first time this run sees `session`'s
    /// announcement. A closure this run announced itself gets the full `grace`; one it inherited
    /// incomplete from a crashed run (`restored`) gets none — its deadline is already `Instant::
    /// now()`, so the very next `expired` check completes it at once rather than waiting out a
    /// second interval it is not owed.
    pub(crate) fn observe(&mut self, session: &dudes_supervisor::closing::ClosingSession) {
        if self.announcement != Some(session.announcement) {
            self.announcement = Some(session.announcement);
            self.deadline = Some(
                Instant::now()
                    + if self.restored {
                        Duration::ZERO
                    } else {
                        self.grace
                    },
            );
            self.report_written = false;
        }
    }

    pub(crate) fn wall_deadline(&self, at: Timestamp) -> Timestamp {
        at.saturating_add_millis(i64::try_from(self.grace.as_millis()).unwrap_or(i64::MAX))
    }

    pub(crate) fn expired(&self, now: Timestamp, at: Timestamp) -> bool {
        let wall = self.wall_deadline(at);
        self.deadline.map_or(now >= wall, |monotonic| {
            Expiry { monotonic, wall }.expired_at(now, Instant::now())
        })
    }

    pub(crate) fn remaining_ms(&self, now: Timestamp, at: Timestamp) -> u64 {
        self.expiry(at).remaining_ms(now)
    }

    fn expiry(&self, at: Timestamp) -> Expiry {
        Expiry {
            monotonic: self.deadline.unwrap_or_else(|| Instant::now() + self.grace),
            wall: self.wall_deadline(at),
        }
    }

    pub(crate) fn floor_view(
        &self,
        world: &dudes_supervisor::World,
        now: Timestamp,
    ) -> Option<FloorClosing> {
        let session = world
            .closing
            .as_ref()
            .filter(|session| self.announcement == Some(session.announcement))?;
        let counts = session.counts(world);
        Some(FloorClosing {
            phase: dudes_supervisor::closing::floor_phase(session.phase),
            progress: dudes_proto::projection::ClosingProgress {
                acknowledged: counts.acknowledged,
                settled: counts.settled,
                owed: counts.owed,
                running: counts.running,
                remaining_ms: self.remaining_ms(now, session.announced_at),
            },
            expiry: (!session.is_complete()).then(|| self.expiry(session.announced_at)),
        })
    }
}
