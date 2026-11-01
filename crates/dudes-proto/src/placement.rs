//! Where an agent works now, and what it keeps in each other project it has worked in.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dudes_types::{Lsn, SessionId, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::budget::AgentBudget;
use crate::record::RecordedWorkspace;
use crate::repository::{HireRepositories, SingletonRepository};

/// The place an idle agent moves to, recorded in full so replay reads no configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMove {
    /// The folder the agent's turns run in: the project's shared folder, or the agent's own copy
    /// when it returns to one.
    pub cwd: PathBuf,
    /// The project it lands in.
    pub workspace: RecordedWorkspace,
    /// The agent's own copy in this project. A move never creates one, so this is only ever a copy
    /// the agent already holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeId>,
    /// The daily ceiling while the agent is here: the tighter of its hire's and the project's.
    pub budget: AgentBudget,
}

/// What an agent holds in one project: its folder and the conversation it keeps there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// The folder, project and own copy.
    pub repository: SingletonRepository,
    /// Whether the operator vouched for the folder.
    pub trusted: bool,
    /// The vendor-confirmed session in this folder.
    pub session: Option<SessionId>,
    /// The confirmation that established the session.
    pub session_lsn: Option<Lsn>,
}

/// An agent's places in the projects it is not in now, by project id.
pub type OtherProjects = BTreeMap<String, Placement>;

impl AgentMove {
    /// The repository authority an agent holds after landing here.
    #[must_use]
    pub fn repository(&self) -> SingletonRepository {
        SingletonRepository::recorded(&self.cwd, Some(&self.workspace), self.worktree)
    }

    /// Whether `placement` is this exact place: the same folder and the same own copy.
    #[must_use]
    pub fn is_place_of(&self, placement: &Placement) -> bool {
        placement.repository.cwd() == self.cwd && placement.repository.worktree() == self.worktree
    }

    /// Fold the move into the agent's place: what it leaves goes under its project, and what it
    /// arrives in becomes current. The World and the saved projection both fold a move through
    /// here, so they cannot disagree. An agent without a single-folder place in a project does not
    /// move, and this returns `false` having changed nothing; replay validation refuses such a
    /// record before it is folded.
    ///
    /// A stored place for the target is restored whole, with its trust and session. A stored
    /// shared-folder place whose folder is not this one is not reused, so the agent arrives as in
    /// a new place, untrusted and without a session.
    pub fn land(&self, here: Here<'_>) -> bool {
        let Here {
            repositories,
            trusted,
            session,
            session_lsn,
            other_projects,
        } = here;
        let Some(repository) = repositories.singleton().cloned() else {
            return false;
        };
        let Some(project) = repository.workspace().map(|workspace| workspace.id.clone()) else {
            return false;
        };
        other_projects.insert(
            project,
            Placement {
                repository,
                trusted: *trusted,
                session: session.take(),
                session_lsn: session_lsn.take(),
            },
        );
        let stored = other_projects
            .remove(&self.workspace.id)
            .filter(|stored| self.is_place_of(stored));
        *repositories = HireRepositories::Legacy(self.repository());
        *trusted = stored.as_ref().is_some_and(|stored| stored.trusted);
        *session = stored.as_ref().and_then(|stored| stored.session.clone());
        *session_lsn = stored.and_then(|stored| stored.session_lsn);
        true
    }
}

/// The parts of an agent row that a move rewrites, borrowed from whichever row holds them.
pub struct Here<'a> {
    /// The current repository authority.
    pub repositories: &'a mut HireRepositories,
    /// Whether the current folder is vouched for.
    pub trusted: &'a mut bool,
    /// The confirmed session in the current folder.
    pub session: &'a mut Option<SessionId>,
    /// The confirmation that established it.
    pub session_lsn: &'a mut Option<Lsn>,
    /// The places in other projects.
    pub other_projects: &'a mut OtherProjects,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str) -> RecordedWorkspace {
        RecordedWorkspace {
            id: id.into(),
            label: id.into(),
        }
    }

    fn to(project: &str, cwd: &str) -> AgentMove {
        AgentMove {
            cwd: cwd.into(),
            workspace: workspace(project),
            worktree: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
        }
    }

    struct Row {
        repositories: HireRepositories,
        trusted: bool,
        session: Option<SessionId>,
        session_lsn: Option<Lsn>,
        others: OtherProjects,
    }

    impl Row {
        fn in_service() -> Self {
            Self {
                repositories: HireRepositories::Legacy(SingletonRepository::recorded(
                    std::path::Path::new("/repo/service"),
                    Some(&workspace("service")),
                    None,
                )),
                trusted: true,
                session: Some(SessionId::new("service-session")),
                session_lsn: Some(Lsn(7)),
                others: OtherProjects::new(),
            }
        }

        fn land(&mut self, to: &AgentMove) -> bool {
            to.land(Here {
                repositories: &mut self.repositories,
                trusted: &mut self.trusted,
                session: &mut self.session,
                session_lsn: &mut self.session_lsn,
                other_projects: &mut self.others,
            })
        }

        fn cwd(&self) -> &std::path::Path {
            self.repositories.singleton().unwrap().cwd()
        }
    }

    #[test]
    fn a_new_place_is_untrusted_without_a_session() {
        let mut row = Row::in_service();
        assert!(row.land(&to("billing", "/repo/billing")));
        assert_eq!(row.cwd(), std::path::Path::new("/repo/billing"));
        assert!(!row.trusted);
        assert_eq!((row.session.clone(), row.session_lsn), (None, None));
        let kept = &row.others["service"];
        assert!(kept.trusted);
        assert_eq!(kept.session, Some(SessionId::new("service-session")));
        assert_eq!(kept.session_lsn, Some(Lsn(7)));
    }

    #[test]
    fn a_return_restores_the_stored_trust_and_conversation() {
        let mut row = Row::in_service();
        row.land(&to("billing", "/repo/billing"));
        row.session = Some(SessionId::new("billing-session"));
        row.session_lsn = Some(Lsn(9));
        assert!(row.land(&to("service", "/repo/service")));
        assert!(row.trusted);
        assert_eq!(row.session, Some(SessionId::new("service-session")));
        assert_eq!(row.session_lsn, Some(Lsn(7)));
        assert_eq!(
            row.others["billing"].session,
            Some(SessionId::new("billing-session"))
        );
        assert!(!row.others.contains_key("service"));
    }

    #[test]
    fn a_stored_shared_folder_that_is_not_the_target_folder_is_not_reused() {
        let mut row = Row::in_service();
        row.land(&to("billing", "/repo/billing"));
        assert!(row.land(&to("service", "/repo/service-moved")));
        assert_eq!(row.cwd(), std::path::Path::new("/repo/service-moved"));
        assert!(!row.trusted);
        assert_eq!((row.session.clone(), row.session_lsn), (None, None));
        assert!(!row.others.contains_key("service"));
    }

    #[test]
    fn an_agent_hired_outside_any_project_does_not_land() {
        let mut row = Row::in_service();
        row.repositories = HireRepositories::Legacy(SingletonRepository::default());
        assert!(!row.land(&to("billing", "/repo/billing")));
        assert!(row.others.is_empty());
        assert!(row.trusted);
    }
}
