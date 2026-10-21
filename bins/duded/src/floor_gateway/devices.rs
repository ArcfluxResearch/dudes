//! The Floor's device routes: list the signed-in browsers, sign one out, and keep a new one.

use dudes_proto::record::{FloorDeviceRemoval, Hex32};
use dudes_types::Timestamp;
use serde::{Deserialize, Serialize};

use super::{FloorSessions, Response, SessionIdentity, action_json, error_response};
use crate::floor_session::{Keep, Removal};

pub(super) const MAX_DEVICES_BODY_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeviceVerb {
    List,
    Remove,
    Keep,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Nothing {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    device: Hex32,
}

#[derive(Serialize)]
struct Listed<'a> {
    id: Hex32,
    name: &'a str,
    signed_in: Timestamp,
    first_used: Option<Timestamp>,
    last_used: Option<Timestamp>,
    new: bool,
    this: bool,
}

pub(super) fn response<'a>(
    verb: DeviceVerb,
    session: &SessionIdentity,
    sessions: &FloorSessions,
    body: &[u8],
) -> Response<'a> {
    match verb {
        DeviceVerb::List => {
            if serde_json::from_slice::<Nothing>(body).is_err() {
                return error_response(400, "Bad Request");
            }
            let devices = sessions.devices(Some(session));
            let listed: Vec<_> = devices
                .iter()
                .map(|device| Listed {
                    id: device.device,
                    name: &device.name,
                    signed_in: device.signed_in,
                    first_used: device.first_used,
                    last_used: device.last_used,
                    new: device.new,
                    this: device.this,
                })
                .collect();
            action_json(200, "OK", &serde_json::json!({ "devices": listed }))
        }
        DeviceVerb::Remove => {
            let Ok(target) = serde_json::from_slice::<Target>(body) else {
                return error_response(400, "Bad Request");
            };
            let reason = if session.device() == Some(target.device) {
                FloorDeviceRemoval::SignedOut
            } else {
                FloorDeviceRemoval::Removed
            };
            match sessions.remove_device(target.device, reason) {
                Removal::Removed => action_json(200, "OK", &serde_json::json!({ "removed": true })),
                Removal::Unknown => {
                    action_json(404, "Not Found", &serde_json::json!({ "removed": false }))
                }
                Removal::Unavailable => error_response(503, "Service Unavailable"),
            }
        }
        DeviceVerb::Keep => {
            let Ok(target) = serde_json::from_slice::<Target>(body) else {
                return error_response(400, "Bad Request");
            };
            match sessions.keep_device(target.device, session) {
                Keep::Kept => action_json(200, "OK", &serde_json::json!({ "kept": true })),
                Keep::Unknown => {
                    action_json(404, "Not Found", &serde_json::json!({ "kept": false }))
                }
                Keep::Own => action_json(
                    409,
                    "Conflict",
                    &serde_json::json!({ "error": "own_device" }),
                ),
                Keep::Unavailable => error_response(503, "Service Unavailable"),
            }
        }
    }
}
