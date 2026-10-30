#![allow(missing_docs)]
//! A multi-repository hire without confinement launches every turn like a single-repository
//! agent in its own copy: in the primary copy, with each other hired copy added.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dudes_proto::record::{FileIdentity, Hex32, RecordedWorkspace, TrustPin, TrustedRepository};
use dudes_proto::repository::{
    CatalogRepository, ExecutionRepository, OwnedRepository, RepoId, RepositoryEntry,
    RepositoryHire, RepositoryPolicy, RepositorySet, ScopedRepositories,
};
use dudes_proto::{AgentBudget, BrowserNoteAttribution, Entry, Record};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input, RepositoryAdmission, SpendView};
use dudes_types::{AgentId, GitCommit, Lsn, Provider, Timestamp, WorktreeId};

const LABELS: [&str; 3] = ["service", "web", "docs"];

fn append(harness: &mut Harness, record: Record) -> Lsn {
    harness.append_replayed(Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    })
}

fn root() -> PathBuf {
    std::env::temp_dir().join("unconfined-launch")
}

fn copy_of(label: &str) -> PathBuf {
    root().join("copies").join(label)
}

fn hire_into(harness: &mut Harness, policy: RepositoryPolicy, labels: &[&str]) -> (AgentId, Lsn) {
    let ids: Vec<RepoId> = labels.iter().map(|id| RepoId::parse(id).unwrap()).collect();
    let origins: Vec<TrustedRepository> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| TrustedRepository {
            directory: root().join("origins").join(label),
            common_dir: root().join("origins").join(label).join(".git"),
            directory_identity: FileIdentity {
                device: 1,
                inode: 100 + index as u64,
            },
            common_identity: FileIdentity {
                device: 1,
                inode: 200 + index as u64,
            },
        })
        .collect();
    let catalog = append(
        harness,
        Record::ProjectRepositoriesRecorded {
            project: "service".into(),
            repositories: RepositorySet::new(
                ids[0].clone(),
                ids.iter()
                    .zip(&origins)
                    .map(|(repo_id, origin)| RepositoryEntry {
                        repo_id: repo_id.clone(),
                        value: CatalogRepository {
                            label: repo_id.as_str().into(),
                            origin: origin.clone(),
                        },
                    })
                    .collect(),
            )
            .unwrap(),
        },
    );
    let base = GitCommit::parse("0000000000000000000000000000000000000000").unwrap();
    let trusts: Vec<Lsn> = ids
        .iter()
        .zip(&origins)
        .map(|(repo_id, origin)| {
            append(
                harness,
                Record::RepositoryTrustSet {
                    project: "service".into(),
                    repo_id: repo_id.clone(),
                    origin: origin.clone(),
                    catalog_lsn: catalog,
                    pin: Some(TrustPin {
                        claude_tree: None,
                        mcp_json: None,
                        attributes: Hex32::new([7; 32]),
                        config: Hex32::new([8; 32]),
                    }),
                },
            )
        })
        .collect();
    let mut owned = Vec::new();
    for (index, (repo_id, origin)) in ids.iter().zip(&origins).enumerate() {
        let worktree = WorktreeId::from_bytes([index as u8 + 1; 16]);
        let path = copy_of(repo_id.as_str());
        append(
            harness,
            Record::WorktreeLeased {
                worktree,
                origin: origin.directory.clone(),
                path: path.clone(),
                branch: format!("dudes/{index:012}"),
                base,
                from: None,
            },
        );
        owned.push(RepositoryEntry {
            repo_id: repo_id.clone(),
            value: OwnedRepository {
                origin: origin.clone(),
                execution: ExecutionRepository {
                    directory: path,
                    directory_identity: FileIdentity {
                        device: 1,
                        inode: 300 + index as u64,
                    },
                    git_dir: origin.common_dir.join(format!("worktrees/copy{index}")),
                    git_identity: FileIdentity {
                        device: 1,
                        inode: 400 + index as u64,
                    },
                },
                worktree,
                base,
                trust_lsn: trusts[index],
            },
        });
    }
    let agent = AgentId::from_parts(1, 1);
    let hire = Box::new(RepositoryHire {
        id: agent,
        name: "Kim".into(),
        character: None,
        accent: None,
        role: "engineer".into(),
        department: "eng".into(),
        provider: Provider::Claude,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        max_turns: None,
        max_budget: None,
        model: None,
        browser_onboarding: None,
        repositories: ScopedRepositories {
            project: RecordedWorkspace {
                id: "service".into(),
                label: "Service".into(),
            },
            catalog_lsn: catalog,
            policy,
            repositories: RepositorySet::new(ids[0].clone(), owned).unwrap(),
        },
    });
    let hired = append(harness, hire.into_record());
    harness
        .supervisor
        .observe_repository_admission(RepositoryAdmission::new([(agent, hired, true)]));
    (agent, hired)
}

fn queue_chat(harness: &mut Harness, agent: AgentId, text: &str) {
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: text.to_owned(),
        browser: BrowserNoteAttribution {
            request_id_digest: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            session_digest: Hex32::new([3; 32]),
        },
        context_lsn,
        context_reset_turn: None,
    }));
}

fn spawns(harness: &Harness) -> Vec<&dudes_providers::launch::LaunchSpec> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec),
            _ => None,
        })
        .collect()
}

fn add_dirs(argv: &[OsString]) -> Vec<&Path> {
    argv.windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| Path::new(&pair[1]))
        .collect()
}

#[test]
fn a_chat_turn_starts_in_the_primary_copy_with_each_other_copy_added_in_order() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::OperatorTrustV1, &LABELS);
    harness.drive(Input::Spend {
        agent,
        view: SpendView::default(),
    });
    queue_chat(&mut harness, agent, "Who are you?");

    let launched = spawns(&harness);
    assert_eq!(launched.len(), 1, "one chat turn launched");
    let spec = launched[0];
    assert_eq!(spec.cwd, copy_of("service"));
    let dirs = add_dirs(&spec.argv);
    assert_eq!(dirs.len(), 3, "chat files, then the two other copies");
    assert!(dirs[0].ends_with("files"));
    assert_eq!(dirs[1], copy_of("web"));
    assert_eq!(dirs[2], copy_of("docs"));

    let task = spec.argv.last().unwrap().to_str().unwrap();
    let end = spec.argv.len() - 1;
    assert_eq!(spec.argv[end - 1], "--");
    assert!(
        spec.argv[..end - 1].iter().all(|part| part != task),
        "the task is passed once, after the separator, and nothing follows it"
    );
    let section = format!(
        "service: {}\nweb: {}\ndocs: {}",
        copy_of("service").display(),
        copy_of("web").display(),
        copy_of("docs").display()
    );
    assert!(
        task.contains(&format!("Your repositories follow, primary first, one `label: path` line each.]\n\n{section}\n\nWho are you?")),
        "{task}"
    );
}

#[test]
fn a_general_goal_turn_starts_in_the_primary_copy() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::OperatorTrustV1, &LABELS);
    harness.set_goal(agent, &["true"]);
    let launched = spawns(&harness);
    assert_eq!(launched.len(), 1, "the goal's first turn launched");
    assert_eq!(launched[0].cwd, copy_of("service"));
    assert_eq!(
        add_dirs(&launched[0].argv)[1..],
        [copy_of("web").as_path(), copy_of("docs").as_path()]
    );
    let task = launched[0].argv.last().unwrap().to_str().unwrap();
    assert!(task.starts_with("[You are Kim"));
    assert!(task.contains("\n\nservice: "));
    assert!(task.contains("`dudes memory read`"), "the legacy teaching");
}

#[test]
fn a_confined_hire_never_takes_the_ordinary_launch() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::WorkingTreeV1, &LABELS);
    harness.set_goal(agent, &["true"]);
    assert!(spawns(&harness).is_empty());
    assert!(
        harness
            .trace
            .iter()
            .all(|seen| !matches!(seen, Seen::Spawn { .. })),
        "a confined hire reaches the scoped path or nothing"
    );
}
