//! Closing progress wire compatibility.

use dudes_proto::projection::{ClosingProgress, FloorDeltaView, FloorProjectionView};
use dudes_proto::{ClosingPhase, FloorDelta, FloorProjection};
use dudes_types::Lsn;

#[test]
fn nested_views_add_exactly_five_fields_and_existing_readers_ignore_them() {
    let projection = FloorProjection {
        lsn: Lsn(7),
        agents: vec![],
        cards: vec![],
        mail: vec![],
        activity: vec![],
        retired_agents: vec![],
        closing_time: Some(ClosingPhase::Announced),
    };
    let delta = FloorDelta {
        lsn: Lsn(8),
        agents: vec![],
        cards: vec![],
        mail: vec![],
        activity: None,
        retired_agents: None,
        removed_agents: vec![],
        removed_cards: vec![],
        closing_time: None,
    };
    let progress = ClosingProgress {
        acknowledged: 1,
        settled: 1,
        owed: 1,
        running: 1,
        remaining_ms: 12_345,
    };
    let expected = serde_json::json!({ "acknowledged": 1, "settled": 1, "owed": 1, "running": 1, "remaining_ms": 12345 });
    let snapshot = serde_json::to_value(FloorProjectionView {
        projection: &projection,
        closing_progress: Some(progress),
    })
    .unwrap();
    let changes = serde_json::to_value(FloorDeltaView {
        delta: &delta,
        closing_progress: Some(progress),
    })
    .unwrap();
    assert_eq!(snapshot["closing_progress"], expected);
    assert_eq!(changes["closing_progress"], expected);
    assert_eq!(
        snapshot["closing_time"],
        serde_json::json!({ "kind": "announced" })
    );
    assert!(
        changes.get("closing_time").is_none(),
        "progress-only deltas keep the existing phase"
    );
    assert_eq!(
        serde_json::from_value::<FloorProjection>(snapshot).unwrap(),
        projection
    );
    assert_eq!(
        serde_json::from_value::<FloorDelta>(changes).unwrap(),
        delta
    );
    let empty = FloorProjection::default();
    let idle = serde_json::to_value(FloorProjectionView {
        projection: &empty,
        closing_progress: None,
    })
    .unwrap();
    assert!(idle.get("closing_time").is_none());
    assert!(idle.get("closing_progress").is_none());
    assert!(
        serde_json::to_value(FloorDeltaView {
            delta: &delta,
            closing_progress: None
        })
        .unwrap()
        .get("closing_progress")
        .is_none()
    );
}
