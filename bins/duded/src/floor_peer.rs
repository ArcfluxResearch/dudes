//! Which processes may use the Floor, decided from the process on the other end of the connection.
//!
//! The Floor trusts a signed-in page. An agent runs as the same user and can open loopback
//! connections, so a request is also refused when the kernel says its connection belongs to a
//! process inside an agent. Nothing the sender writes is read: the owner comes from the socket
//! tables, never from a header.
//!
//! A request is refused when any process holding the connection is inside an agent, when any
//! holder's chain of parents cannot be followed to the top, and when any holder descends from the
//! daemon itself: the browser is started by the user or the CLI, never by the daemon, so a daemon
//! descendant is an agent whose pid is not registered yet.
//!
//! A process that left its agent's tree and group (a double fork with `setsid`) and reparented to
//! the init process is not caught.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, PoisonError};

use dudes_host::tcp_peer;
pub(crate) use dudes_host::tcp_peer::SocketOwnerLookup;
#[cfg(test)]
pub(crate) use dudes_host::tcp_peer::{ProcessRef, SocketOwner};
use dudes_proto::FloorPeerRefusal;
use dudes_types::{AgentId, Timestamp};

/// How long one agent's refusals share a single journal record.
const COALESCE_MS: i64 = 60_000;

/// A process an agent runs as the root of its own tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgentRoot {
    /// The root process.
    pub(crate) pid: u32,
    /// The agent that owns it, when the daemon has already recorded the child.
    pub(crate) agent: Option<AgentId>,
}

/// The agent processes alive now. Read on every check, so it never lags a spawn.
pub(crate) trait AgentRoots: fmt::Debug + Send + Sync {
    fn live(&self) -> Vec<AgentRoot>;
}

/// A daemon that tracks no agents, as in a test that hires none.
#[derive(Debug, Default)]
pub(crate) struct NoAgents;

impl AgentRoots for NoAgents {
    fn live(&self) -> Vec<AgentRoot> {
        Vec::new()
    }
}

/// What the Floor does with one request's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allowed,
    Refused {
        reason: FloorPeerRefusal,
        agent: Option<AgentId>,
    },
}

type OwnerOf = dyn Fn(SocketAddrV4, SocketAddrV4) -> SocketOwnerLookup + Send + Sync;
type GroupOf = dyn Fn(u32) -> Option<u32> + Send + Sync;

/// The peer check, and the memory of which refusals were already journaled.
pub(crate) struct PeerGate {
    roots: Arc<dyn AgentRoots>,
    owner_of: Box<OwnerOf>,
    group_of: Box<GroupOf>,
    own_uid: Option<u32>,
    daemon_pid: Option<u32>,
    journaled: Mutex<BTreeMap<Option<AgentId>, Timestamp>>,
}

impl fmt::Debug for PeerGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerGate").finish_non_exhaustive()
    }
}

impl PeerGate {
    /// The gate over the operating system's socket tables.
    ///
    /// `daemon_pid` is the daemon's own process; nothing that descends from it may use the Floor.
    pub(crate) fn system(roots: Arc<dyn AgentRoots>, daemon_pid: Option<u32>) -> Self {
        Self::with(
            roots,
            Box::new(tcp_peer::loopback_socket_owner),
            Box::new(tcp_peer::process_group),
            tcp_peer::current_uid(),
            daemon_pid,
        )
    }

    pub(crate) fn with(
        roots: Arc<dyn AgentRoots>,
        owner_of: Box<OwnerOf>,
        group_of: Box<GroupOf>,
        own_uid: Option<u32>,
        daemon_pid: Option<u32>,
    ) -> Self {
        Self {
            roots,
            owner_of,
            group_of,
            own_uid,
            daemon_pid,
            journaled: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether the process holding `client`'s end of a connection to `server` may use the Floor.
    ///
    /// Nothing is cached: every call asks the kernel again. A client that already left, or one the
    /// platform cannot find, is refused.
    pub(crate) fn check(&self, client: SocketAddr, server: SocketAddr) -> Verdict {
        let (SocketAddr::V4(client), SocketAddr::V4(server)) = (client, server) else {
            return refused(FloorPeerRefusal::NoOwner, None);
        };
        let owners = match (self.owner_of)(client, server) {
            SocketOwnerLookup::Unsupported => {
                return refused(FloorPeerRefusal::Unsupported, None);
            }
            SocketOwnerLookup::NotFound => return refused(FloorPeerRefusal::NoOwner, None),
            SocketOwnerLookup::Found(owners) if owners.is_empty() => {
                return refused(FloorPeerRefusal::NoOwner, None);
            }
            SocketOwnerLookup::Found(owners) => owners,
        };
        if owners.iter().any(|owner| self.own_uid != Some(owner.uid)) {
            return refused(FloorPeerRefusal::OtherUser, None);
        }
        let lineage = || owners.iter().flat_map(|owner| owner.lineage.iter());
        if let Some(daemon) = self.daemon_pid
            && lineage().any(|process| process.pid == daemon)
        {
            return refused(FloorPeerRefusal::Agent, None);
        }
        for root in self.roots.live() {
            let group = (self.group_of)(root.pid);
            if lineage().any(|process| process.pid == root.pid || Some(process.group) == group) {
                return refused(FloorPeerRefusal::Agent, root.agent);
            }
        }
        Verdict::Allowed
    }

    /// Whether a refusal naming `agent` should be journaled at `now`, which it is once a minute.
    pub(crate) fn should_journal(&self, agent: Option<AgentId>, now: Timestamp) -> bool {
        let mut journaled = self
            .journaled
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if journaled
            .get(&agent)
            .is_some_and(|last| (0..COALESCE_MS).contains(&now.millis_since(*last)))
        {
            return false;
        }
        journaled.insert(agent, now);
        true
    }
}

const fn refused(reason: FloorPeerRefusal, agent: Option<AgentId>) -> Verdict {
    Verdict::Refused { reason, agent }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use super::*;

    #[derive(Debug)]
    struct Roots(Vec<AgentRoot>);

    impl AgentRoots for Roots {
        fn live(&self) -> Vec<AgentRoot> {
            self.0.clone()
        }
    }

    const ME: u32 = 501;
    const AGENT_ROOT: u32 = 900;
    const DAEMON: u32 = 700;

    fn agent() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn gate(lookup: SocketOwnerLookup, roots: Vec<AgentRoot>) -> PeerGate {
        PeerGate::with(
            Arc::new(Roots(roots)),
            Box::new(move |_, _| lookup.clone()),
            Box::new(Some),
            Some(ME),
            Some(DAEMON),
        )
    }

    fn holder(uid: u32, lineage: &[(u32, u32)]) -> SocketOwner {
        SocketOwner {
            uid,
            lineage: lineage
                .iter()
                .map(|&(pid, group)| ProcessRef { pid, group })
                .collect(),
        }
    }

    fn owner(uid: u32, lineage: &[(u32, u32)]) -> SocketOwnerLookup {
        SocketOwnerLookup::Found(vec![holder(uid, lineage)])
    }

    fn root() -> AgentRoot {
        AgentRoot {
            pid: AGENT_ROOT,
            agent: Some(agent()),
        }
    }

    #[test]
    fn a_browser_outside_every_agent_is_allowed() {
        let browser = owner(ME, &[(3000, 3000), (2000, 2000), (1, 1)]);
        assert_eq!(
            gate(browser, vec![root()]).check(addr(50000), addr(8080)),
            Verdict::Allowed
        );
    }

    #[test]
    fn a_descendant_of_an_agent_root_is_refused_with_the_agent() {
        let deep = owner(
            ME,
            &[(4000, 4000), (3000, 3000), (AGENT_ROOT, AGENT_ROOT), (1, 1)],
        );
        assert_eq!(
            gate(deep, vec![root()]).check(addr(50000), addr(8080)),
            Verdict::Refused {
                reason: FloorPeerRefusal::Agent,
                agent: Some(agent())
            }
        );
    }

    #[test]
    fn a_process_in_an_agents_group_is_refused_though_its_parents_are_not() {
        let same_group = owner(ME, &[(4000, AGENT_ROOT), (1, 1)]);
        assert_eq!(
            gate(same_group, vec![root()]).check(addr(50000), addr(8080)),
            Verdict::Refused {
                reason: FloorPeerRefusal::Agent,
                agent: Some(agent())
            }
        );
    }

    #[test]
    fn a_connection_held_by_two_processes_is_refused_whatever_the_order() {
        let browser = holder(ME, &[(3000, 3000), (1, 1)]);
        let agent_child = holder(ME, &[(4000, 4000), (AGENT_ROOT, AGENT_ROOT), (1, 1)]);
        for holders in [
            vec![browser.clone(), agent_child.clone()],
            vec![agent_child, browser],
        ] {
            assert_eq!(
                gate(SocketOwnerLookup::Found(holders), vec![root()])
                    .check(addr(50000), addr(8080)),
                Verdict::Refused {
                    reason: FloorPeerRefusal::Agent,
                    agent: Some(agent())
                }
            );
        }
    }

    #[test]
    fn an_owner_descending_from_the_daemon_is_refused_before_any_agent_is_registered() {
        let new_agent = owner(ME, &[(4000, 4000), (DAEMON, DAEMON), (1, 1)]);
        assert_eq!(
            gate(new_agent, Vec::new()).check(addr(50000), addr(8080)),
            Verdict::Refused {
                reason: FloorPeerRefusal::Agent,
                agent: None
            }
        );
    }

    #[test]
    fn a_root_the_daemon_has_not_yet_recorded_still_refuses_without_naming_an_agent() {
        let child = owner(ME, &[(4000, 4000), (AGENT_ROOT, AGENT_ROOT)]);
        let unrecorded = AgentRoot {
            pid: AGENT_ROOT,
            agent: None,
        };
        assert_eq!(
            gate(child, vec![unrecorded]).check(addr(50000), addr(8080)),
            Verdict::Refused {
                reason: FloorPeerRefusal::Agent,
                agent: None
            }
        );
    }

    #[test]
    fn an_owner_that_cannot_be_found_or_cannot_be_looked_up_is_refused() {
        for (lookup, reason) in [
            (SocketOwnerLookup::NotFound, FloorPeerRefusal::NoOwner),
            (
                SocketOwnerLookup::Unsupported,
                FloorPeerRefusal::Unsupported,
            ),
        ] {
            assert_eq!(
                gate(lookup, Vec::new()).check(addr(50000), addr(8080)),
                Verdict::Refused {
                    reason,
                    agent: None
                }
            );
        }
    }

    #[test]
    fn another_users_process_is_refused() {
        assert_eq!(
            gate(owner(ME + 1, &[(3000, 3000)]), Vec::new()).check(addr(50000), addr(8080)),
            Verdict::Refused {
                reason: FloorPeerRefusal::OtherUser,
                agent: None
            }
        );
    }

    #[test]
    fn a_daemon_that_does_not_know_its_own_user_refuses_everyone() {
        let unknown_self = PeerGate::with(
            Arc::new(NoAgents),
            Box::new(|_, _| owner(ME, &[(3000, 3000)])),
            Box::new(|_| None),
            None,
            None,
        );
        assert!(matches!(
            unknown_self.check(addr(50000), addr(8080)),
            Verdict::Refused { .. }
        ));
    }

    #[test]
    fn refusals_for_one_agent_are_journaled_once_a_minute() {
        let gate = gate(SocketOwnerLookup::NotFound, Vec::new());
        let at = |ms: i64| Timestamp::from_unix_millis(1_000_000 + ms);
        assert!(gate.should_journal(Some(agent()), at(0)));
        assert!(!gate.should_journal(Some(agent()), at(59_999)));
        assert!(
            gate.should_journal(None, at(1)),
            "no agent is its own bucket"
        );
        assert!(gate.should_journal(Some(agent()), at(60_000)));
        assert!(!gate.should_journal(Some(agent()), at(60_001)));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_system_gate_allows_this_test_process_and_refuses_a_stranger() {
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_held, _) = listener.accept().unwrap();
        let gate = PeerGate::system(Arc::new(NoAgents), None);
        let here = (client.local_addr().unwrap(), listener.local_addr().unwrap());
        assert_eq!(gate.check(here.0, here.1), Verdict::Allowed);
        let inside = PeerGate::system(
            Arc::new(Roots(vec![AgentRoot {
                pid: std::process::id(),
                agent: Some(agent()),
            }])),
            None,
        );
        assert_eq!(
            inside.check(here.0, here.1),
            Verdict::Refused {
                reason: FloorPeerRefusal::Agent,
                agent: Some(agent())
            }
        );
        assert!(matches!(
            gate.check(addr(1), here.1),
            Verdict::Refused {
                reason: FloorPeerRefusal::NoOwner,
                ..
            }
        ));
    }
}
