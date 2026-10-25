use std::collections::BTreeMap;

use dudes_journal::{MemoryProjection, Projections};
use dudes_policy::{DepartmentPeer, HeldCardContext, ReverseDependent, WorkInput};
use dudes_types::{AgentId, CardStatus};

pub(crate) fn compose(
    projections: &Projections,
    memory: &MemoryProjection,
    mut seed: impl FnMut(AgentId) -> WorkInput,
) -> dudes_journal::Result<BTreeMap<AgentId, WorkInput>> {
    let agents = projections.agents()?;
    let cards = projections.cards()?;
    let held = projections.earliest_held_cards()?;
    Ok(agents
        .iter()
        .map(|agent| {
            let mut view = seed(agent.id);
            view.memory_digest = memory.work_digest(agent.id);
            view.held_card_context = held.get(&agent.id).map(|card| HeldCardContext {
                card: *card,
                waiting_on_me: cards
                    .iter()
                    .filter(|row| {
                        row.id != *card && row.status != CardStatus::Done && row.deps.contains(card)
                    })
                    .map(|row| ReverseDependent {
                        card: row.id,
                        title: row.title.clone(),
                        status: row.status,
                        held_by: row.claimed_by,
                    })
                    .collect(),
                peers: agents
                    .iter()
                    .filter(|row| row.id != agent.id && row.department == agent.department)
                    .map(|row| DepartmentPeer {
                        agent: row.id,
                        name: row.name.clone(),
                        held_card: held.get(&row.id).copied(),
                    })
                    .collect(),
            });
            (agent.id, view)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::{Actor, Entry, Record};
    use dudes_types::{CardId, Lsn, Seq, Timestamp};

    #[test]
    fn live_composition_preserves_seed_branch_and_canonical_memory_and_claim_order() {
        let root = tempfile::tempdir().unwrap();
        let mut projection = Projections::open(&root.path().join("state.redb")).unwrap();
        let mut memory = MemoryProjection::new(root.path()).unwrap();
        let agent = AgentId::from_parts(1, 1);
        let peer = AgentId::from_parts(1, 2);
        let later = CardId::from_parts(2, 1);
        let earlier = CardId::from_parts(2, 2);
        let dependent = CardId::from_parts(2, 3);
        let mut lsn = 1;
        let mut apply = |record: Record| {
            let entry = Entry {
                at: Timestamp::from_unix_millis(1),
                record,
            };
            projection.apply(Lsn(lsn), 0, &entry).unwrap();
            lsn += 1;
        };
        for id in [agent, peer] {
            apply(serde_json::from_value(serde_json::json!({"t":"agent_hired","id":id,"name":"peer","role":"worker","department":"same","provider":"claude","cwd":root.path(),"budget":{}})).unwrap());
        }
        for card in [later, earlier] {
            apply(Record::CardCreated {
                card,
                title: "own".to_owned(),
                department: "same".to_owned(),
                by: Actor::System,
                deps: vec![],
            });
        }
        apply(Record::Claim {
            card: earlier,
            agent,
            expected_seq: Seq(0),
        });
        apply(Record::Claim {
            card: later,
            agent,
            expected_seq: Seq(0),
        });
        apply(Record::CardCreated {
            card: dependent,
            title: "added after claim".to_owned(),
            department: "same".to_owned(),
            by: Actor::System,
            deps: vec![earlier],
        });
        let prepared = memory
            .prepare_append(
                agent,
                b"canonical current memory",
                dudes_proto::PeerProvenance::NoRecordedAgent,
            )
            .unwrap();
        memory.apply(Lsn(100), prepared.record());
        let seed = WorkInput {
            claimable_card: Some(dudes_policy::CardInput {
                id: later,
                title: "seed card".to_owned(),
            }),
            unread_mail: 9,
            memory_digest: "stale".to_owned(),
            held_card_context: None,
            identity: None,
        };
        let first = compose(&projection, &memory, |_| seed.clone()).unwrap();
        let view = &first[&agent];
        assert_eq!(view.claimable_card, seed.claimable_card);
        assert_eq!(view.unread_mail, 9);
        assert!(view.memory_digest.contains("canonical current memory"));
        let context = view.held_card_context.as_ref().unwrap();
        assert_eq!(context.card, earlier);
        assert_eq!(context.waiting_on_me[0].card, dependent);
        assert_eq!(context.peers[0].agent, peer);
        assert_eq!(
            first,
            compose(&projection, &memory, |_| seed.clone()).unwrap()
        );
    }
}
