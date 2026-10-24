//! Daemon-local output registry interface checks.
use std::time::Duration;

use duded::output::{Loss, OutputRegistry, OutputState, ReplyLoss, Unavailable};
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{AgentId, SessionId, TurnId};

fn id<T: From<ulid::Ulid>>(suffix: u128) -> T {
    T::from(ulid::Ulid::from(suffix))
}

#[test]
fn confirmation_and_full_child_key_gate_bounded_text() {
    let output = OutputRegistry::new();
    let agent: AgentId = id(1);
    let turn: TurnId = id(2);
    let key = AdoptionKey {
        pid: 42,
        start_time: 10,
        boot: Some("boot-a".into()),
    };
    let recycled = AdoptionKey {
        pid: 42,
        start_time: 11,
        boot: Some("boot-a".into()),
    };
    output.capture(&key, "s", "before");
    output.replace(agent, turn);
    output.register(agent, turn, &key);
    assert!(matches!(
        output.snapshot(agent).state,
        OutputState::Unavailable(Unavailable::Unbound)
    ));
    output.confirm(agent, turn, &SessionId::new("other"));
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { ref chunks, .. } if chunks.is_empty())
    );
    output.capture(&recycled, "other", "recycled");
    output.capture(&key, "wrong", "wrong-session");
    output.capture(&key, "other", "visible");
    let snapshot = output.snapshot(agent);
    assert!(
        matches!(snapshot.state, OutputState::Ready { ref chunks, .. } if chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>() == ["visible"])
    );
    assert!(
        !format!("{snapshot:?} {output:?}").contains("visible"),
        "diagnostic formatting does not reveal captured text"
    );
    output.end(agent, turn);
    output.capture(&key, "other", "late");
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { ref chunks, ended: true, .. } if chunks.len() == 1)
    );
}

#[test]
fn pending_overflow_chunk_limit_and_notification_label_loss() {
    let output = OutputRegistry::new();
    let agent: AgentId = id(11);
    let turn: TurnId = id(12);
    let key = AdoptionKey {
        pid: 101,
        start_time: 202,
        boot: None,
    };
    let before = output.snapshot(agent);
    output.replace(agent, turn);
    output.register(agent, turn, &key);
    assert!(
        output
            .wait_changed(agent, before.revision, Duration::from_millis(10))
            .revision
            > before.revision
    );
    for _ in 0..140 {
        output.capture(&key, "s", "abc");
    }
    assert!(matches!(
        output.snapshot(agent).state,
        OutputState::Unavailable(Unavailable::Unbound)
    ));
    output.confirm(agent, turn, &SessionId::new("s"));
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { first_seq, next_seq: 128, ref chunks, truncated: true, loss: Some(Loss::PendingOverflow), .. } if first_seq == 0 && chunks.len() == 128)
    );
    for _ in 0..3 {
        output.capture(&key, "s", "🙂");
    }
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { first_seq: 3, next_seq: 131, ref chunks, truncated: true, loss: Some(Loss::RingTruncation), .. } if chunks.len() == 128)
    );
    let usage = output.usage();
    assert_eq!(usage.rings, 1);
    assert!(usage.text_bytes <= 65_536 && usage.chunks <= 128);
}

#[test]
fn global_oldest_eviction_and_metadata_remain_bounded() {
    let output = OutputRegistry::new();
    for n in 0..130 {
        let agent: AgentId = id(1_000 + n);
        let turn: TurnId = id(2_000 + n);
        let key = AdoptionKey {
            pid: 1_000 + u32::try_from(n).expect("130 fixture children fit u32"),
            start_time: 5,
            boot: None,
        };
        output.replace(agent, turn);
        output.register(agent, turn, &key);
        output.confirm(agent, turn, &SessionId::new("s"));
        output.capture(&key, "s", &"x".repeat(65_536));
        let usage = output.usage();
        assert!(usage.rings <= 128 && usage.text_bytes <= 8 * 1_024 * 1_024);
    }
    assert!(matches!(
        output.snapshot(id::<AgentId>(1_000)).state,
        OutputState::Unavailable(Unavailable::Evicted)
    ));
    for n in 0..140 {
        let key = AdoptionKey {
            pid: 3_000 + n,
            start_time: 9,
            boot: None,
        };
        output.capture(&key, "s", "pending");
    }
    assert!(output.usage().pending_keys <= 128);
    assert!(output.usage().text_bytes <= 8 * 1_024 * 1_024);
    assert!(output.usage().identity_bytes <= 8 * 1_024 * 1_024);
    assert!(
        matches!(output.snapshot(id::<AgentId>(1_002)).state, OutputState::Ready { first_seq, truncated: true, loss: Some(Loss::GlobalEviction), .. } if first_seq > 0)
    );
}

#[test]
fn oversized_session_identity_is_refused_without_pending_amplification() {
    let output = OutputRegistry::new();
    let key = AdoptionKey {
        pid: 8_001,
        start_time: 42,
        boot: None,
    };
    let huge_session = "s".repeat(900_000);
    output.capture(&key, &huge_session, &"x".repeat(65_536));
    let usage = output.usage();
    assert_eq!(
        usage.pending_keys, 0,
        "oversized untrusted identity is not retained"
    );
    assert_eq!(usage.text_bytes, 0);
    assert_eq!(usage.identity_bytes, 0);
    assert!(usage.refused > 0);
    let agent: AgentId = id(8_002);
    let turn: TurnId = id(8_003);
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new(huge_session));
    output.end(agent, turn);
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { ended: true, ref chunks, next_seq: 0, .. } if chunks.is_empty())
    );
    assert_eq!(
        output.usage().identity_bytes,
        0,
        "accepted but overcap identity does not enter this output view"
    );
    let bounded = "b".repeat(256);
    let next_key = AdoptionKey {
        pid: 8_004,
        start_time: 45,
        boot: None,
    };
    let next_agent: AgentId = id(8_005);
    let next_turn: TurnId = id(8_006);
    output.replace(next_agent, next_turn);
    assert!(output.register(next_agent, next_turn, &next_key));
    output.capture(&next_key, &bounded, "admitted-exact-session");
    output.confirm(next_agent, next_turn, &SessionId::new(bounded));
    assert_eq!(
        text_of(&output.snapshot(next_agent).state),
        "admitted-exact-session"
    );
}

fn text_of(state: &OutputState) -> String {
    match state {
        OutputState::Ready { chunks, .. } => {
            chunks.iter().map(|chunk| chunk.text.as_str()).collect()
        }
        _ => String::new(),
    }
}

#[test]
fn live_reply_retains_prefix_across_more_than_128_native_fragments() {
    let output = OutputRegistry::new();
    let agent = id(9_001);
    let turn = id(9_002);
    let key = AdoptionKey {
        pid: 9_003,
        start_time: 1,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new("session"));
    output.capture(&key, "session", "prefix ");
    for _ in 0..256 {
        output.capture(&key, "session", "x");
    }
    let snapshot = output.snapshot(agent);
    let reply = snapshot.reply.unwrap();
    assert_eq!(reply.text, format!("prefix {}", "x".repeat(256)));
    assert_eq!(reply.loss, None);
    assert!(!reply.ended);
    assert!(
        matches!(snapshot.state, OutputState::Ready { first_seq: 129, next_seq: 257, ref chunks, loss: Some(Loss::RingTruncation), .. } if chunks.len() == 128)
    );
    assert_eq!(output.usage().text_bytes, reply.text.len() + 128);
}

#[test]
fn live_reply_bounds_utf8_and_accounts_for_both_retained_views() {
    let output = OutputRegistry::new();
    let agent = id(9_101);
    let turn = id(9_102);
    let key = AdoptionKey {
        pid: 9_103,
        start_time: 1,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new("s"));
    output.capture(&key, "s", &"🙂".repeat(16_384));
    let full = output.snapshot(agent).reply.unwrap();
    assert_eq!(full.text.len(), 65_536);
    assert_eq!(full.loss, None);
    assert_eq!(output.usage().text_bytes, 131_072);
    output.capture(&key, "s", "tail");
    let trimmed = output.snapshot(agent).reply.unwrap();
    assert_eq!(trimmed.text, format!("{}tail", "🙂".repeat(16_383)));
    assert_eq!(trimmed.loss, Some(ReplyLoss::ReplyTruncation));
    assert!(trimmed.revision > full.revision);
    assert!(output.usage().text_bytes <= 131_072);
}

#[test]
fn live_reply_matches_identity_and_survives_end_until_replacement() {
    let output = OutputRegistry::new();
    let agent = id(9_201);
    let turn = id(9_202);
    let key = AdoptionKey {
        pid: 9_203,
        start_time: 1,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.capture(&key, "wrong", "excluded");
    output.capture(&key, "s", "first");
    assert!(output.snapshot(agent).reply.is_none());
    output.confirm(agent, id(9_299), &SessionId::new("s"));
    assert!(output.snapshot(agent).reply.is_none());
    output.confirm(agent, turn, &SessionId::new("s"));
    let first = output.snapshot(agent).reply.unwrap();
    assert_eq!(
        (first.agent, first.turn, first.text.as_str()),
        (agent, turn, "first")
    );
    output.capture(&key, "wrong", "excluded");
    assert_eq!(output.snapshot(agent).reply.unwrap(), first);
    output.end(agent, turn);
    let ended = output.snapshot(agent).reply.unwrap();
    assert!(ended.ended);
    assert_eq!(ended.text, "first");
    assert!(ended.revision > first.revision);
    output.capture(&key, "s", "late");
    assert_eq!(output.snapshot(agent).reply.unwrap(), ended);
    output.replace(agent, id(9_204));
    assert!(output.snapshot(agent).reply.is_none());
    output.retire(agent);
    assert_eq!(output.usage().text_bytes, 0);
    assert!(output.snapshot(agent).reply.is_none());
}

#[test]
fn live_reply_revision_ignores_other_agents_before_and_after_end() {
    let output = OutputRegistry::new();
    let agent = id(9_401);
    let turn = id(9_402);
    let key = AdoptionKey {
        pid: 9_403,
        start_time: 1,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new("s"));
    output.capture(&key, "s", "stable");
    let other = id(9_404);
    let other_turn = id(9_405);
    let other_key = AdoptionKey {
        pid: 9_406,
        start_time: 1,
        boot: None,
    };
    output.replace(other, other_turn);
    assert!(output.register(other, other_turn, &other_key));
    output.confirm(other, other_turn, &SessionId::new("other"));
    for ended in [false, true] {
        if ended {
            output.end(agent, turn);
        }
        let before = output.snapshot(agent);
        output.capture(&other_key, "other", "unrelated");
        let after = output.snapshot(agent);
        assert!(after.revision > before.revision);
        assert_eq!(after.reply, before.reply);
    }
}

#[test]
fn live_reply_marks_pending_loss_and_global_eviction() {
    let output = OutputRegistry::new();
    let agent = id(9_301);
    let turn = id(9_302);
    let key = AdoptionKey {
        pid: 9_303,
        start_time: 1,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    for _ in 0..129 {
        output.capture(&key, "s", "p");
    }
    output.confirm(agent, turn, &SessionId::new("s"));
    let pending = output.snapshot(agent).reply.unwrap();
    assert_eq!(pending.text, "p".repeat(128));
    assert_eq!(pending.loss, Some(ReplyLoss::PendingOverflow));
    for n in 0..128 {
        let agent = id(10_000 + n);
        let turn = id(11_000 + n);
        let key = AdoptionKey {
            pid: 10_000 + u32::try_from(n).unwrap(),
            start_time: 2,
            boot: None,
        };
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.confirm(agent, turn, &SessionId::new("s"));
        output.capture(&key, "s", &"a".repeat(65_536));
        assert!(output.usage().text_bytes <= 8 * 1_024 * 1_024);
    }
    let evicted = output.snapshot(id(10_000)).reply.unwrap();
    assert_eq!(evicted.loss, Some(ReplyLoss::GlobalEviction));
    assert!(evicted.text.is_empty());
}

#[test]
fn eof_before_registration_preserves_pre_registration_output_for_the_durable_end() {
    let output = OutputRegistry::new();
    let agent: AgentId = id(8_101);
    let turn: TurnId = id(8_102);
    let key = AdoptionKey {
        pid: 8_103,
        start_time: 44,
        boot: None,
    };
    output.capture(&key, "session", "eligible");
    output.watch_finished(&key);
    output.capture(&key, "session", "late");
    output.replace(agent, turn);
    assert!(
        output.register(agent, turn, &key),
        "the full key remains claimable after EOF"
    );
    output.confirm(agent, turn, &SessionId::new("session"));
    output.end(agent, turn);
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { turn: seen, ended: true, ref chunks, .. } if seen == turn && chunks.len() == 1 && chunks[0].text == "eligible")
    );
    assert!(output.usage().refused > 0, "post-EOF callback was refused");
}

#[test]
fn durable_end_without_a_confirmed_init_has_an_explicit_empty_ended_view() {
    let output = OutputRegistry::new();
    let agent: AgentId = id(8_201);
    let turn: TurnId = id(8_202);
    output.replace(agent, turn);
    output.end(agent, turn);
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { turn: seen, ended: true, ref chunks, first_seq: 0, next_seq: 0, .. } if seen == turn && chunks.is_empty())
    );
}

#[test]
fn live_durable_overcap_session_has_acknowledged_empty_view_before_end() {
    let output = OutputRegistry::new();
    let agent: AgentId = id(8_301);
    let turn: TurnId = id(8_302);
    let key = AdoptionKey {
        pid: 8_303,
        start_time: 46,
        boot: None,
    };
    let session = "s".repeat(257);
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.capture(&key, &session, "must-stay-hidden");
    let before = output.snapshot(agent);
    assert!(matches!(
        before.state,
        OutputState::Unavailable(Unavailable::Unbound)
    ));
    let durable_session = SessionId::new(session.clone());
    output.confirm(id(8_304), turn, &durable_session);
    output.confirm(agent, id(8_305), &durable_session);
    assert_eq!(
        output.snapshot(agent).revision,
        before.revision,
        "wrong pair cannot acknowledge the ring"
    );
    output.confirm(agent, turn, &durable_session);
    let live = output.snapshot(agent);
    assert!(
        live.revision > before.revision,
        "durable confirmation wakes an armed reader"
    );
    assert!(
        matches!(live.state, OutputState::Ready { agent: seen_agent, turn: seen_turn, ended: false, ref chunks, first_seq: 0, next_seq: 0, .. } if seen_agent == agent && seen_turn == turn && chunks.is_empty())
    );
    let refused_before = output.usage().refused;
    output.capture(&key, "short", "late-short-session-text");
    output.capture(&key, &session, "late-overcap-text");
    output.confirm(agent, turn, &SessionId::new("short"));
    assert!(
        output.usage().refused >= refused_before + 3,
        "no callback or duplicate confirmation reopens the gate"
    );
    assert_eq!(
        output.snapshot(agent),
        live,
        "refusals do not publish text or a new acknowledgement"
    );
    assert_eq!(output.usage().identity_bytes, 0);
    assert_eq!(output.usage().text_bytes, 0);
    output.end(agent, turn);
    assert!(
        matches!(output.snapshot(agent).state, OutputState::Ready { ended: true, ref chunks, .. } if chunks.is_empty())
    );
    let next_turn: TurnId = id(8_306);
    output.replace(agent, next_turn);
    let next_key = AdoptionKey {
        pid: 8_307,
        start_time: 47,
        boot: None,
    };
    assert!(output.register(agent, next_turn, &next_key));
    let replacement = output.snapshot(agent);
    output.confirm(agent, turn, &durable_session);
    assert_eq!(
        output.snapshot(agent),
        replacement,
        "late old-turn confirmation cannot bind replacement"
    );
}

#[test]
fn a_known_secret_split_across_pieces_never_reaches_the_ring_or_the_reply() {
    const SECRET: &str = "tok_9fQ2LmZx81KdPvR4sWbN7hYcA3eJuT6o";
    let redactor = dudes_supervisor::Redactor::new();
    redactor.set_source("test", [SECRET.to_owned()]);
    let output = OutputRegistry::with_redactor(redactor);
    let agent: AgentId = id(1);
    let turn: TurnId = id(2);
    let key = AdoptionKey {
        pid: 42,
        start_time: 10,
        boot: Some("boot-a".into()),
    };
    output.replace(agent, turn);
    output.register(agent, turn, &key);
    output.confirm(agent, turn, &SessionId::new("s"));
    let (head, tail) = SECRET.split_at(11);
    for piece in ["the token is ", head, tail, " ok, and ", &SECRET[..6]] {
        output.capture(&key, "s", piece);
        let snapshot = output.snapshot(agent);
        let shown = format!("{:?}", chunks_of(&snapshot));
        assert!(
            !shown.contains(&SECRET[..8]),
            "released part of the secret: {shown}"
        );
    }
    output.watch_finished(&key);
    let snapshot = output.snapshot(agent);
    let text: String = chunks_of(&snapshot).join("");
    assert_eq!(text, "the token is (redacted) ok, and tok_9f");
    assert_eq!(snapshot.reply.unwrap().text, text);
}

#[test]
fn a_secret_across_the_ring_chunk_boundary_is_replaced_before_it_is_chunked() {
    const SECRET: &str = "tok_9fQ2LmZx81KdPvR4sWbN7hYcA3eJuT6o";
    let redactor = dudes_supervisor::Redactor::new();
    redactor.set_source("test", [SECRET.to_owned()]);
    let output = OutputRegistry::with_redactor(redactor);
    let agent: AgentId = id(1);
    let turn: TurnId = id(2);
    let key = AdoptionKey {
        pid: 42,
        start_time: 10,
        boot: Some("boot-a".into()),
    };
    output.replace(agent, turn);
    output.register(agent, turn, &key);
    output.confirm(agent, turn, &SessionId::new("s"));
    let text = format!("{}{SECRET}{}", "a".repeat(4_090), "b".repeat(50));
    output.capture(&key, "s", &text);
    output.watch_finished(&key);
    let snapshot = output.snapshot(agent);
    let chunks = chunks_of(&snapshot);
    assert!(chunks.len() > 1, "the text crossed a chunk boundary");
    for chunk in &chunks {
        assert!(
            !chunk.contains(&SECRET[..6]),
            "a chunk kept part of the secret"
        );
    }
    assert_eq!(
        chunks.join(""),
        format!("{}(redacted){}", "a".repeat(4_090), "b".repeat(50))
    );
}

fn chunks_of(snapshot: &duded::output::OutputSnapshot) -> Vec<String> {
    match &snapshot.state {
        OutputState::Ready { chunks, .. } => chunks.iter().map(|c| c.text.clone()).collect(),
        other => panic!("not ready: {other:?}"),
    }
}
