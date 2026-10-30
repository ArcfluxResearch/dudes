use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use dudes_proto::repository::{Confinement, HireRepositories};
use dudes_rpc::auth::{Authorizer, PeerClassifier, PeerProvenance, PeerRole, Presented, Refusal};
use dudes_supervisor::World;
use dudes_types::AgentId;

/// Scoped owners from completed journal folds, retained after retirement.
#[derive(Clone, Debug)]
pub(crate) struct ScopedHires(Arc<RwLock<BTreeMap<AgentId, Confinement>>>);

impl ScopedHires {
    pub(crate) fn new(replayed: &World) -> Self {
        Self(Arc::new(RwLock::new(members(replayed))))
    }

    pub(crate) fn publish(&self, folded: &World) {
        let members = members(folded);
        if let Ok(mut slot) = self.0.write() {
            *slot = members;
        }
    }

    fn confinement(&self, agent: AgentId) -> Option<Confinement> {
        self.0
            .read()
            .map_or(Some(Confinement::Confined), |members| {
                members.get(&agent).copied()
            })
    }
}

fn members(world: &World) -> BTreeMap<AgentId, Confinement> {
    world
        .agents
        .iter()
        .filter_map(|(agent, row)| match &row.repositories {
            HireRepositories::Scoped(hire) => Some((*agent, hire.policy.confinement())),
            HireRepositories::Legacy(_) => None,
        })
        .collect()
}

#[derive(Debug)]
pub(crate) struct ScopedSocketAuthorizer {
    classifier: PeerClassifier,
    hires: ScopedHires,
}

impl ScopedSocketAuthorizer {
    pub(crate) const fn new(classifier: PeerClassifier, hires: ScopedHires) -> Self {
        Self { classifier, hires }
    }
}

impl Authorizer for ScopedSocketAuthorizer {
    fn authorize(
        &self,
        presented: Presented,
        provenance: PeerProvenance,
        method: &str,
    ) -> Result<PeerRole, Refusal> {
        let role = self.classifier.authorize(presented, provenance, method)?;
        if let PeerProvenance::Agent(agent) = provenance
            && let Some(confinement) = self.hires.confinement(agent)
        {
            match confinement {
                Confinement::Confined => {
                    return Err(Refusal::unauthorized(format!(
                        "{method} is not yet available in a multi-repo ticket"
                    )));
                }
                Confinement::Unconfined => {}
            }
        }
        Ok(role)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use dudes_proto::record::samples;
    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_proto::{Entry, Record, RecordKind};
    use dudes_rpc::testing::FakeMintedTokens;
    use dudes_types::{Lsn, Timestamp};
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "ttttttttttttttttttttttttttttttttttttttttttt";

    fn entry(record: Record) -> Entry {
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        }
    }

    pub(crate) fn pending_hire() -> (World, Entry, AgentId) {
        let hire = samples::record(RecordKind::AgentHiredWithRepositories);
        let Record::AgentHiredWithRepositories(fields) = &hire else {
            unreachable!()
        };
        let owner = fields.id;
        let repository = fields.repositories.repositories.primary();
        let lease = Record::WorktreeLeased {
            worktree: repository.worktree,
            origin: repository.origin.directory.clone(),
            path: repository.execution.directory.clone(),
            branch: "dudes/030303030303".into(),
            base: repository.base,
            from: None,
        };
        let mut world = World::default();
        for (position, record) in [
            (0, samples::record(RecordKind::ProjectRepositoriesRecorded)),
            (128, samples::record(RecordKind::RepositoryTrustSet)),
            (192, lease),
        ] {
            world
                .apply_checked(Lsn::from_parts(1, position).unwrap(), &entry(record))
                .unwrap();
        }
        (world, entry(hire), owner)
    }

    fn classifier(agent: AgentId) -> PeerClassifier {
        PeerClassifier::new(
            OperatorCredential::new("C".repeat(43)).unwrap(),
            Arc::new(FakeMintedTokens::minted(&[(TOKEN, agent)])),
        )
    }

    #[test]
    fn only_a_completed_hire_fold_changes_existing_socket_authorizers() {
        let (mut world, pending, agent) = pending_hire();
        let hires = ScopedHires::new(&world);
        let authorizer = ScopedSocketAuthorizer::new(classifier(agent), hires.clone());
        let peer = PeerProvenance::Agent(agent);
        assert!(!world.agents.contains_key(&agent));
        assert_eq!(
            authorizer.authorize(Presented::default(), peer, "system.ping"),
            Ok(PeerRole::Unproven)
        );
        world
            .apply_checked(Lsn::from_parts(1, 256).unwrap(), &pending)
            .unwrap();
        hires.publish(&world);
        for method in [
            "system.ping",
            "agent.ask",
            "memory.read",
            "team.roster",
            "no.such.method",
        ] {
            let params = json!({"token": TOKEN});
            let refusal = authorizer
                .authorize(Presented::from_params(Some(&params)), peer, method)
                .unwrap_err();
            assert!(
                refusal
                    .to_string()
                    .contains("not yet available in a multi-repo ticket")
            );
        }
        world
            .apply_checked(
                Lsn::from_parts(1, 384).unwrap(),
                &entry(Record::AgentRetired {
                    id: agent,
                    reason: "finished".into(),
                    browser_retire: None,
                }),
            )
            .unwrap();
        hires.publish(&world);
        assert!(
            authorizer
                .authorize(Presented::default(), peer, "system.ping")
                .is_err()
        );
        assert!(
            ScopedSocketAuthorizer::new(classifier(agent), ScopedHires::new(&world))
                .authorize(Presented::default(), peer, "system.ping")
                .is_err()
        );
    }

    #[test]
    fn the_socket_admits_an_unconfined_hires_agent_and_still_refuses_a_confined_one() {
        let (mut world, confined, agent) = pending_hire();
        world
            .apply_checked(Lsn::from_parts(1, 256).unwrap(), &confined)
            .unwrap();
        let params = json!({"token": TOKEN});
        let peer = PeerProvenance::Agent(agent);
        let authorizer = ScopedSocketAuthorizer::new(classifier(agent), ScopedHires::new(&world));
        assert!(
            authorizer
                .authorize(Presented::from_params(Some(&params)), peer, "agent.ask")
                .is_err()
        );

        let (mut world, _, agent) = pending_hire();
        let unconfined = entry(samples::record(
            RecordKind::AgentHiredWithUnconfinedRepositories,
        ));
        world
            .apply_checked(Lsn::from_parts(1, 256).unwrap(), &unconfined)
            .unwrap();
        let authorizer = ScopedSocketAuthorizer::new(classifier(agent), ScopedHires::new(&world));
        assert_eq!(
            authorizer.authorize(Presented::from_params(Some(&params)), peer, "agent.ask"),
            Ok(PeerRole::Agent { agent })
        );
    }

    #[test]
    fn scoped_policy_preserves_the_classifiers_refusals_and_legacy_identity() {
        let (mut world, pending, agent) = pending_hire();
        world
            .apply_checked(Lsn::from_parts(1, 256).unwrap(), &pending)
            .unwrap();
        let authorizer = ScopedSocketAuthorizer::new(classifier(agent), ScopedHires::new(&world));
        let params = json!({"token": "u".repeat(43)});
        assert_eq!(
            authorizer.authorize(
                Presented::from_params(Some(&params)),
                PeerProvenance::Agent(agent),
                "system.ping"
            ),
            Err(Refusal::Token)
        );
        let params = json!({"token": TOKEN});
        let presented = || Presented::from_params(Some(&params));
        let peer = PeerProvenance::Agent(agent);
        let unrestricted =
            ScopedSocketAuthorizer::new(classifier(agent), ScopedHires::new(&World::default()));
        assert_eq!(
            unrestricted.authorize(presented(), peer, "team.roster"),
            classifier(agent).authorize(presented(), peer, "team.roster")
        );
        assert_eq!(
            unrestricted.authorize(presented(), peer, "team.roster"),
            Ok(PeerRole::Agent { agent })
        );
    }

    #[test]
    fn unreadable_scope_membership_refuses_agent_ping_and_preserves_operator_identity() {
        let (world, _, agent) = pending_hire();
        let hires = ScopedHires::new(&world);
        let poisoned = hires.clone();
        let _ = std::panic::catch_unwind(move || {
            let _held = poisoned.0.write().unwrap();
            panic!("injected publication panic");
        });
        let authorizer = ScopedSocketAuthorizer::new(classifier(agent), hires);
        assert!(
            authorizer
                .authorize(
                    Presented::default(),
                    PeerProvenance::Agent(agent),
                    "system.ping"
                )
                .is_err()
        );
        let operator = Presented {
            token: None,
            cred: Some(OperatorCredential::new("C".repeat(43)).unwrap()),
        };
        assert_eq!(
            authorizer.authorize(operator, PeerProvenance::NoRecordedAgent, "agent.list"),
            Ok(PeerRole::Operator)
        );
    }
}
