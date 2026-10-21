//! The Floor's device table: every browser that signed in, and what became of it.
//!
//! The fold reads recorded values and the time each entry was written, so a replay rebuilds the
//! same table. Whether a device has expired is the daemon's decision from the clock; the fold
//! only keeps the facts.

use dudes_proto::Record;
use dudes_proto::record::{FloorDeviceRemoval, Hex32};
use dudes_types::Timestamp;

use super::World;

/// One signed-in browser, as the confirmed entries describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    /// The Floor origin it signed in on.
    pub origin: String,
    /// The label its browser chose, already cleaned by the daemon.
    pub name: String,
    /// SHA-256 of its cookie.
    pub cookie: Hex32,
    /// SHA-256 of the secret its page sends.
    pub secret: Hex32,
    /// When it signed in.
    pub enrolled: Timestamp,
    /// When it first made a request.
    pub first_used: Option<Timestamp>,
    /// When it was last recorded as used.
    pub last_used: Option<Timestamp>,
    /// When someone answered its "new browser" notice with Keep.
    pub acknowledged: Option<Timestamp>,
    /// How and when it ended, once a removal is recorded.
    pub removed: Option<(FloorDeviceRemoval, Timestamp)>,
}

impl World {
    pub(super) fn fold_floor_device(&mut self, at: Timestamp, record: &Record) {
        match record {
            Record::FloorDeviceEnrolled {
                device,
                origin,
                name,
                cookie,
                secret,
            } => {
                self.floor_devices
                    .entry(*device)
                    .or_insert_with(|| DeviceRow {
                        origin: origin.clone(),
                        name: name.clone(),
                        cookie: *cookie,
                        secret: *secret,
                        enrolled: at,
                        first_used: None,
                        last_used: None,
                        acknowledged: None,
                        removed: None,
                    });
            }
            Record::FloorDeviceFirstUsed { device } => {
                if let Some(row) = self.live_device(device) {
                    row.first_used.get_or_insert(at);
                    row.last_used = Some(row.last_used.map_or(at, |last| last.max(at)));
                }
            }
            Record::FloorDeviceUsed { device } => {
                if let Some(row) = self.live_device(device) {
                    row.first_used.get_or_insert(at);
                    row.last_used = Some(row.last_used.map_or(at, |last| last.max(at)));
                }
            }
            Record::FloorDeviceAcknowledged { device } => {
                if let Some(row) = self.live_device(device) {
                    row.acknowledged.get_or_insert(at);
                }
            }
            Record::FloorDeviceRemoved { device, reason } => {
                if let Some(row) = self.live_device(device) {
                    row.removed = Some((*reason, at));
                }
            }
            _ => {}
        }
    }

    fn live_device(&mut self, device: &Hex32) -> Option<&mut DeviceRow> {
        self.floor_devices
            .get_mut(device)
            .filter(|row| row.removed.is_none())
    }
}
