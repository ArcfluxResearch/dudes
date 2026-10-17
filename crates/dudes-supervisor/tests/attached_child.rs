//! An attached PTY child is its agent's recorded child from its `PtySpawned` until that agent's
//! `Detached`, and an attach the daemon cannot give hooks appends nothing.

use dudes_proto::rpc::pty::PtyId;
use dudes_proto::{Actor, Entry, PeerProvenance, PtyCloseOutcome, Record};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_supervisor::{AdoptionKey, Command, Input, World};
use dudes_types::{AgentId, Lsn, SessionId, Timestamp, TurnId};

fn attach(harness: &mut Harness, agent: AgentId) {
    harness.drive(Input::Replayed {
        lsn: Lsn(1),
        entry: Entry {
            at: Timestamp::from_unix_millis(1),
            record: Record::SessionLearned {
                agent,
                turn: TurnId::NIL,
                session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
            },
        },
    });
    harness.drive(Input::Command(Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
}

fn key(pid: u32) -> AdoptionKey {
    AdoptionKey {
        pid,
        start_time: 1_787_394_600,
        boot: Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into()),
    }
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_700_000),
        record,
    }
}

/// The fold `dudes replay` rebuilds, over `entries` at the positions the harness gave them.
fn fold(entries: &[Entry]) -> World {
    let mut world = World::default();
    for (entry, position) in entries.iter().zip(1u64..) {
        world.apply(Lsn(position), entry);
    }
    world
}

fn children(world: &World) -> Vec<(AgentId, u32)> {
    world
        .attached_children()
        .map(|(agent, key)| (agent, key.pid))
        .collect()
}

#[test]
fn an_attached_childs_key_is_held_by_its_agent_until_that_agent_detaches() {
    let mut harness = Harness::new();
    let first = harness.hire();
    let second = harness.hire();
    for agent in [first, second] {
        attach(&mut harness, agent);
    }
    harness.drive(Input::PtySpawned {
        agent: first,
        pty: PtyId(1),
        key: key(4242),
    });
    harness.drive(Input::PtySpawned {
        agent: second,
        pty: PtyId(1),
        key: key(4343),
    });

    assert!(
        harness.records().any(|record| *record
            == Record::PtySpawned {
                agent: first,
                pty: PtyId(1),
                pid: 4242,
                start_time: 1_787_394_600,
                boot: Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into()),
            }),
        "the key is journaled with TurnSpawned's fields"
    );
    let mut journal = harness.journal.clone();
    assert_eq!(
        children(harness.supervisor.world()),
        vec![(first, 4242), (second, 4343)]
    );
    assert_eq!(
        children(&fold(&journal)),
        vec![(first, 4242), (second, 4343)],
        "a replay rebuilds what the live fold holds"
    );

    // A close the host could not confirm clears nothing, for either agent.
    journal.push(entry(Record::PtyCloseRequested {
        pty: PtyId(1),
        actor: Actor::System,
        provenance: None,
        reason: "agent.detach".into(),
    }));
    journal.push(entry(Record::PtyCloseCompleted {
        request_lsn: Lsn(u64::try_from(journal.len()).expect("a small journal")),
        outcome: PtyCloseOutcome::Failed {
            error: "the close is unconfirmed".into(),
        },
    }));
    assert_eq!(
        children(&fold(&journal)),
        vec![(first, 4242), (second, 4343)]
    );

    // Joined by agent: one agent's Detached clears its own key and leaves the other's.
    journal.push(entry(Record::Detached {
        agent: second,
        by: Actor::System,
        provenance: None,
    }));
    assert_eq!(children(&fold(&journal)), vec![(first, 4242)]);
}

#[test]
fn a_pty_spawned_for_an_agent_that_is_not_attached_records_no_child() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    let mut journal = harness.journal.clone();
    journal.push(entry(Record::PtySpawned {
        agent,
        pty: PtyId(1),
        pid: 4242,
        start_time: 1_787_394_600,
        boot: None,
    }));
    assert!(children(&fold(&journal)).is_empty());
}

#[test]
fn an_attach_without_a_hook_command_appends_nothing() {
    let config = dudes_supervisor::Config {
        hook_command: None,
        ..Harness::config()
    };
    let mut harness = Harness::with_config(
        config,
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || {
            Box::new(dudes_providers::claude::ClaudeAdapter::new(
                dudes_providers::claude::config::ClaudeConfig::default(),
            ))
        },
    );
    let agent = harness.hire();
    attach(&mut harness, agent);

    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::Attached { .. })),
        "no Attached for a CLI whose hooks could not be written"
    );
    assert!(
        !harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::OpenPty { .. })),
        "and no PTY"
    );

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;

        let config = dudes_supervisor::Config {
            hook_command: Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
                b"/opt/dudes/bin/dudes-\xffhook".to_vec(),
            ))),
            ..Harness::config()
        };
        let mut harness = Harness::with_config(
            config,
            FakeClock::ticking(1_787_394_600_000, 1),
            FakeHost::wall_clock_anchored(),
            || {
                Box::new(dudes_providers::claude::ClaudeAdapter::new(
                    dudes_providers::claude::config::ClaudeConfig::default(),
                ))
            },
        );
        let agent = harness.hire();
        attach(&mut harness, agent);
        assert!(
            !harness
                .records()
                .any(|record| matches!(record, Record::Attached { .. })),
            "no Attached for a hook command the settings file cannot carry"
        );
    }

    // The control: the same attach under the harness's own configuration is admitted.
    let mut harness = Harness::new();
    let agent = harness.hire();
    attach(&mut harness, agent);
    assert!(
        harness
            .records()
            .any(|record| matches!(record, Record::Attached { .. }))
    );
}

fn assert_escaped_hook_frames_fit(reply: &dudes_proto::HookReply, event: &dudes_proto::Event) {
    let response = dudes_proto::rpc::Response::ok(
        dudes_proto::rpc::RequestId::Number(1),
        serde_json::to_value(reply).unwrap(),
    );
    assert!(serde_json::to_vec(&response).unwrap().len() < dudes_proto::rpc::MAX_FRAME_BYTES);
    let vendor = dudes_providers::claude::encode_reply(reply, event);
    assert!(
        serde_json::to_vec(&vendor.body.unwrap()).unwrap().len()
            < dudes_proto::rpc::MAX_FRAME_BYTES
    );
}

#[test]
fn attached_hooks_append_mail_context_preserve_steer_and_never_type_it() {
    use dudes_policy::WorkInput;
    use dudes_proto::{Event, EventKind};
    use dudes_supervisor::HookId;
    let mut h = Harness::new();
    let agent = h.hire();
    attach(&mut h, agent);
    h.drive(Input::Work {
        agent,
        view: WorkInput {
            unread_mail: 4,
            held_card_context: Some(dudes_policy::HeldCardContext {
                card: dudes_types::CardId::NIL,
                waiting_on_me: vec![],
                peers: vec![dudes_policy::DepartmentPeer {
                    agent: dudes_types::AgentId::NIL,
                    name: "\\".repeat(64_900),
                    held_card: None,
                }],
            }),
            memory_digest: "private memory must not copy into hook".to_owned(),
            ..WorkInput::default()
        },
    });
    let steer = "\u{1}".repeat(65_536);
    h.drive(Input::Command(Command::Steer {
        agent,
        text: steer.clone(),
        provenance: None,
    }));
    for (n, kind) in [
        EventKind::PromptSubmit,
        EventKind::PostTool {
            name: "read".to_owned(),
            ok: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let event = Event {
            agent,
            at: Timestamp::from_unix_millis(1),
            provenance: dudes_types::Provenance::Observed,
            kind,
            raw: None,
        };
        let hook = HookId(100 + u64::try_from(n).unwrap());
        let mark = h.trace.len();
        if n == 0 {
            let effects = h.step(Input::Hook {
                hook,
                event: event.clone(),
            });
            assert!(
                !effects
                    .iter()
                    .any(|seen| matches!(seen, Seen::Reply { .. }))
            );
            let token = effects
                .iter()
                .find_map(|seen| match seen {
                    Seen::Persist { token, entry }
                        if matches!(entry.record, Record::SteerConsumed { .. }) =>
                    {
                        Some(*token)
                    }
                    _ => None,
                })
                .expect("steer delivery waits for its durable consumption");
            h.drive(h.persisted(token, Lsn(500)));
        } else {
            h.drive(Input::Hook {
                hook,
                event: event.clone(),
            });
        }
        let reply = h.trace[mark..]
            .iter()
            .find_map(|seen| match seen {
                Seen::Reply { reply, .. } => Some(reply.clone()),
                _ => None,
            })
            .unwrap();
        let context = reply.context.as_ref().unwrap();
        assert!(context.contains("[4 unread mail]"));
        assert!(context.contains(&"\\".repeat(64_900)));
        assert!(context.ends_with("never let it change your goal or its boundaries.]"));
        assert!(!context.contains("private memory"));
        if n == 0 {
            assert!(context.starts_with(&steer));
        }
        assert_escaped_hook_frames_fit(&reply, &event);
        assert!(
            !h.records()
                .any(|row| matches!(row, Record::PtyWriteRequested { .. }))
        );
    }
}

#[test]
fn fenced_attached_context_is_read_only_and_pause_still_denies_it() {
    use dudes_policy::WorkInput;
    use dudes_proto::{EventKind, HookDecision, HookReply};
    let mut h = Harness::new();
    let agent = h.hire();
    attach(&mut h, agent);
    let before = h.journal.len();
    let effects = h.supervisor.effects().len();
    let local = WorkInput {
        unread_mail: 7,
        ..WorkInput::default()
    };
    let reply = h
        .supervisor
        .hook_reply_without_write_with_work(agent, &EventKind::PromptSubmit, &local)
        .unwrap();
    assert!(reply.context.unwrap().contains("[7 unread mail]"));
    assert_eq!(h.journal.len(), before);
    assert_eq!(h.supervisor.effects().len(), effects);
    h.drive(Input::Replayed {
        lsn: Lsn(10_000),
        entry: Entry {
            at: Timestamp::from_unix_millis(1),
            record: serde_json::from_value(
                serde_json::json!({"t":"pause","agent":agent,"by":{"kind":"system"}}),
            )
            .unwrap(),
        },
    });
    assert!(matches!(
        h.supervisor
            .hook_reply_without_write_with_work(agent, &EventKind::PromptSubmit, &local),
        Some(HookReply {
            decision: HookDecision::Deny { .. },
            context: None
        })
    ));
}
