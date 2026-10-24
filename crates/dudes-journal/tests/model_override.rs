#![allow(missing_docs)]
//! The agent's model override as the projection keeps it: set by the hire, replaced or cleared by
//! a model change, and the same whether or not the writer added checkpoints between the records.

use dudes_journal::{Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::record::samples;
use dudes_proto::{Actor, Entry, Record, RecordKind};
use dudes_types::{AgentId, ModelName, Timestamp};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

fn changed(agent: AgentId, model: Option<&str>) -> Entry {
    entry(Record::AgentModelChanged {
        agent,
        model: model.map(|name| ModelName::parse(name).unwrap()),
        by: Actor::Peer { pid: 7, uid: None },
        request: None,
    })
}

fn override_after(every: u64, steps: &[Entry]) -> Option<ModelName> {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: every,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    for step in steps {
        writer.append(step).unwrap();
    }
    drop(writer);
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let mut store = Projections::in_memory().unwrap();
    replay(&reader, &mut store).unwrap();
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        unreachable!()
    };
    store
        .agent(id)
        .unwrap()
        .expect("the agent is projected")
        .model
}

#[test]
fn the_override_starts_at_the_hire_and_follows_each_change_to_a_clear() {
    let hire = entry(samples::record(RecordKind::AgentHired));
    let Record::AgentHired { id, .. } = &hire.record else {
        unreachable!()
    };
    let id = *id;
    for every in [0, 2] {
        assert_eq!(
            override_after(every, std::slice::from_ref(&hire))
                .as_ref()
                .map(ModelName::as_str),
            Some("sonnet"),
            "the hire's model is the first override (checkpoint every {every})"
        );
        assert_eq!(
            override_after(every, &[hire.clone(), changed(id, Some("opus"))])
                .as_ref()
                .map(ModelName::as_str),
            Some("opus"),
            "a change replaces it (checkpoint every {every})"
        );
        assert_eq!(
            override_after(
                every,
                &[hire.clone(), changed(id, Some("opus")), changed(id, None)]
            ),
            None,
            "a clear returns the agent to the engine's own selection (checkpoint every {every})"
        );
    }
}
