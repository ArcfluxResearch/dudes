use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use dudes_supervisor::{
    AdoptionKey, Clock, Liveness, ProcessHost, Rng, SharedFolderRefusal, Supervisor,
};
use dudes_types::{AgentId, IsolationMode, Mode, Provider};

use super::{Ctx, Shared};
use crate::predicate::PredicateRunner;

const MAX_CHILDREN: usize = 512;

fn canonical_folder(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    std::fs::metadata(&canonical)
        .ok()?
        .is_dir()
        .then_some(canonical)
}

struct Census<'a, H> {
    host: &'a H,
    world: &'a dudes_supervisor::World,
    candidate: Option<AgentId>,
    folder: PathBuf,
    examined: usize,
    conflict: Option<(AgentId, String)>,
    unknown: Option<SharedFolderRefusal>,
}

impl<H: ProcessHost> Census<'_, H> {
    fn room(&mut self) -> Result<(), SharedFolderRefusal> {
        if self.examined == MAX_CHILDREN {
            self.unknown.get_or_insert(SharedFolderRefusal::ChildLimit);
            return Err(SharedFolderRefusal::ChildLimit);
        }
        self.examined += 1;
        Ok(())
    }

    fn child(&mut self, agent: AgentId, cwd: &Path, key: Option<&AdoptionKey>) {
        if Some(agent) == self.candidate {
            return;
        }
        let live = key.map_or(Liveness::Unknown, |key| self.host.alive(key));
        if live == Liveness::Gone {
            return;
        }
        let Some(folder) = canonical_folder(cwd) else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerFolderUnresolved { agent });
            return;
        };
        if folder != self.folder {
            return;
        }
        if live == Liveness::Unknown {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerLivenessUnknown { agent });
        } else if let Some(row) = self.world.agents.get(&agent) {
            if self
                .conflict
                .as_ref()
                .is_none_or(|(other, _)| agent < *other)
            {
                let name = dudes_types::DisplayName::parse(&row.name).map_or_else(
                    |_| "agent".to_owned(),
                    dudes_types::DisplayName::into_string,
                );
                self.conflict = Some((agent, name));
            }
        } else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
        }
    }

    fn owner_folder(&self, agent: AgentId) -> Option<&Path> {
        let row = self.world.agents.get(&agent)?;
        match row.worktree {
            Some(lease) => self
                .world
                .worktrees
                .get(&lease)
                .map(|lease| lease.path.as_path()),
            None => Some(row.cwd.as_path()),
        }
    }

    fn recorded(&mut self, agent: AgentId, key: Option<&AdoptionKey>) {
        let Some(cwd) = self.owner_folder(agent).map(Path::to_path_buf) else {
            if key.is_none_or(|key| self.host.alive(key) != Liveness::Gone) {
                self.unknown
                    .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
            }
            return;
        };
        self.child(agent, &cwd, key);
    }

    fn result(self) -> Result<(), SharedFolderRefusal> {
        if let Some((agent, name)) = self.conflict {
            Err(SharedFolderRefusal::LivePeer { agent, name })
        } else if let Some(unknown) = self.unknown {
            Err(unknown)
        } else {
            Ok(())
        }
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn admit_shared_folder<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: Option<AgentId>,
        provider: Provider,
        requested: Option<IsolationMode>,
        mode: Mode,
        cwd: &Path,
    ) -> Result<(), SharedFolderRefusal> {
        if provider != Provider::Claude
            || (requested != Some(IsolationMode::Inherit)
                && !sup
                    .adapters()
                    .get(provider)
                    .is_some_and(|adapter| adapter.context_mode(mode).claude_inherit()))
        {
            return Ok(());
        }
        let folder = canonical_folder(cwd).ok_or(SharedFolderRefusal::CandidateFolderUnresolved)?;
        let world = sup.world();
        let mut census = Census {
            host: &self.host,
            world,
            candidate: agent,
            folder,
            examined: 0,
            conflict: None,
            unknown: None,
        };
        let mut turns = BTreeSet::new();
        let current: Result<(), SharedFolderRefusal> =
            self.children.visit_owned(|turn, key, cwd| {
                census.room()?;
                turns.insert(turn);
                if let Some(row) = world.turns.get(&turn) {
                    match cwd {
                        Some(cwd) => census.child(row.agent, cwd, Some(&key)),
                        None => census.recorded(row.agent, Some(&key)),
                    }
                } else if self.host.alive(&key) != Liveness::Gone {
                    census
                        .unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
                Ok(())
            });
        if current.is_err() {
            return census.result();
        }
        let uncertain: Result<bool, SharedFolderRefusal> =
            self.children.visit_uncertain(|turn, cwd| {
                census.room()?;
                if let Some(row) = world.turns.get(&turn) {
                    census.child(row.agent, cwd, None);
                } else {
                    census
                        .unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
                Ok(())
            });
        match uncertain {
            Ok(false) => {}
            Ok(true) | Err(_) => {
                census
                    .unknown
                    .get_or_insert(SharedFolderRefusal::ChildLimit);
                return census.result();
            }
        }
        let mut attachments = BTreeSet::new();
        for (owner, pty) in &self.live_ptys {
            if census.room().is_err() {
                return census.result();
            }
            if let Some((key, cwd)) = self.pty_host.receipt(*pty) {
                census.child(*owner, cwd, Some(key));
                attachments.insert(*owner);
            }
        }
        for (turn, row) in world.open_children() {
            if !turns.contains(&turn) {
                if census.room().is_err() {
                    return census.result();
                }
                census.recorded(row.agent, row.adoption_key().as_ref());
            }
        }
        for (owner, key) in world.attached_children() {
            if !attachments.contains(&owner) {
                if census.room().is_err() {
                    return census.result();
                }
                census.recorded(owner, Some(key));
                attachments.insert(owner);
            }
        }
        for owner in self
            .live_ptys
            .keys()
            .filter(|owner| !attachments.contains(owner))
        {
            census.recorded(*owner, None);
        }
        for cleanup in self.cleanup_ptys.values() {
            if !cleanup.close_confirmed {
                if census.room().is_err() {
                    return census.result();
                }
                census.recorded(cleanup.agent, None);
            }
        }
        census.result()
    }
}
