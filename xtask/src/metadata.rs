//! A minimal view of `cargo metadata` — enough to walk the resolved dependency graph, and enough
//! to say what each workspace member is (its name and its manifest).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Run `cargo metadata --all-features --locked` in the current workspace and return its JSON.
///
/// One invocation shape for every consumer: the layering gate needs `resolve`, the package view
/// needs `packages` and `workspace_members`, and running the same command for both means a fixture
/// shaped like one is shaped like the other.
pub(crate) fn cargo_metadata() -> Result<Value> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--locked",
        ])
        .output()
        .context("failed to run `cargo metadata`")?;
    if !output.status.success() {
        bail!(
            "`cargo metadata` exited with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("`cargo metadata` output is not JSON")
}

/// One workspace member, as its own manifest describes it — no dependency resolution involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Package {
    /// Package name, spelled the way `cargo -p` takes it.
    pub(crate) name: String,
    /// The member's `Cargo.toml`, relative to the workspace root and `/`-separated, which is how
    /// git spells a path. Callers ask git about this string verbatim.
    pub(crate) manifest: String,
}

/// Every workspace member of a `cargo metadata` document, sorted by name.
///
/// Fails closed on anything it cannot read: a member id with no package entry, a manifest outside
/// the workspace root. A caller deciding which crates a
/// gate covers must not receive a short list because one member could not be described — a dropped
/// member reads exactly like a member that does not exist.
pub(crate) fn members(value: &Value) -> Result<Vec<Package>> {
    let root = value["workspace_root"]
        .as_str()
        .context("metadata has no `workspace_root`")?;
    let packages: HashMap<&str, &Value> = value["packages"]
        .as_array()
        .context("metadata has no `packages`")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();

    let mut members = Vec::new();
    for id in value["workspace_members"]
        .as_array()
        .context("metadata has no `workspace_members`")?
    {
        let id = id.as_str().context("workspace member id is not a string")?;
        let package = packages
            .get(id)
            .with_context(|| format!("workspace member `{id}` has no entry in `packages`"))?;
        let name = package["name"]
            .as_str()
            .with_context(|| format!("workspace member `{id}` has no name"))?;
        let manifest_path = package["manifest_path"]
            .as_str()
            .with_context(|| format!("package `{name}` has no `manifest_path`"))?;
        members.push(Package {
            name: name.to_owned(),
            manifest: relative_manifest(root, manifest_path)?,
        });
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(members)
}

/// A member's manifest path relative to the workspace root, `/`-separated.
///
/// `cargo metadata` reports absolute, platform-native paths; git wants a repository path. Anything
/// that is not a plain relative path under the root is an error rather than a best guess, because
/// the result is fed to git as a pathspec and a wrong pathspec matches nothing — which would read
/// as "this crate has no baseline" instead of as a bug.
fn relative_manifest(root: &str, manifest_path: &str) -> Result<String> {
    let relative = Path::new(manifest_path)
        .strip_prefix(root)
        .with_context(|| format!("manifest {manifest_path} is not under workspace root {root}"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("manifest {manifest_path} is not a plain path under {root}");
        };
        parts.push(
            part.to_str()
                .with_context(|| format!("manifest {manifest_path} is not UTF-8"))?,
        );
    }
    if parts.is_empty() {
        bail!("manifest {manifest_path} is the workspace root itself");
    }
    Ok(parts.join("/"))
}

/// One resolved package and its outgoing (non-dev) dependency edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) deps: Vec<String>,
}

/// The resolved graph, keyed by package id, plus the set of workspace member ids.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    pub(crate) nodes: HashMap<String, Node>,
    pub(crate) members: Vec<String>,
}

impl Graph {
    /// Build the graph from parsed `cargo metadata` JSON (format version 1).
    ///
    /// Edges that exist only as `dev-dependencies` are dropped: they never link into the artefact.
    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        let names: HashMap<&str, &str> = value["packages"]
            .as_array()
            .context("metadata has no `packages`")?
            .iter()
            .filter_map(|p| Some((p["id"].as_str()?, p["name"].as_str()?)))
            .collect();

        let mut nodes = HashMap::new();
        for node in value["resolve"]["nodes"]
            .as_array()
            .context("metadata has no `resolve.nodes`")?
        {
            let id = node["id"].as_str().context("resolve node without id")?;
            let name = names
                .get(id)
                .with_context(|| format!("unknown package id {id}"))?;
            let deps = node["deps"]
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .filter(|d| !dev_only(d))
                        .filter_map(|d| d["pkg"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            nodes.insert(
                id.to_owned(),
                Node {
                    name: (*name).to_owned(),
                    deps,
                },
            );
        }

        let members = value["workspace_members"]
            .as_array()
            .context("metadata has no `workspace_members`")?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect();

        Ok(Self { nodes, members })
    }

    /// Names of the workspace members, in `workspace_members` order.
    ///
    /// Fails closed like [`Graph::path_to`]: a member id with no resolve node means the metadata is
    /// corrupt, and a check that reads this to decide which members are covered by a rule must not
    /// silently drop the member it could not name — that would report "covered" for a crate it
    /// never saw.
    pub(crate) fn member_names(&self) -> Result<Vec<&str>> {
        self.members
            .iter()
            .map(|id| {
                self.nodes
                    .get(id)
                    .map(|node| node.name.as_str())
                    .with_context(|| {
                        format!("dependency graph is incomplete: `{id}` has no resolve node")
                    })
            })
            .collect()
    }

    /// Id of the workspace member called `name`, if it exists.
    pub(crate) fn member_id(&self, name: &str) -> Option<&str> {
        self.members
            .iter()
            .find(|id| self.nodes.get(*id).is_some_and(|n| n.name == name))
            .map(String::as_str)
    }

    /// Breadth-first search from `start` for a package called `target`; returns the path of
    /// package names (inclusive) if reachable.
    ///
    /// Fails closed: a dependency id with no `resolve` node is an error, not "unreachable". Cargo's
    /// resolve graph is complete, so a dangling edge means the metadata is corrupt, and a layering
    /// gate must not pass on a graph it could not walk.
    pub(crate) fn path_to(&self, start: &str, target: &str) -> Result<Option<Vec<String>>> {
        let mut parent: HashMap<&str, &str> = HashMap::new();
        let mut queue = std::collections::VecDeque::from([start]);
        let mut seen = std::collections::HashSet::from([start]);
        while let Some(id) = queue.pop_front() {
            let Some(node) = self.nodes.get(id) else {
                bail!("dependency graph is incomplete: `{id}` has no resolve node");
            };
            if node.name == target && id != start {
                let mut path = vec![node.name.clone()];
                let mut cur = id;
                while let Some(&p) = parent.get(cur) {
                    path.push(self.nodes[p].name.clone());
                    cur = p;
                }
                path.reverse();
                return Ok(Some(path));
            }
            for dep in &node.deps {
                if seen.insert(dep.as_str()) {
                    parent.insert(dep.as_str(), id);
                    queue.push_back(dep.as_str());
                }
            }
        }
        Ok(None)
    }
}

/// True when every `dep_kinds` entry is a dev-dependency.
///
/// `all`, not `any`: cargo emits several `dep_kinds` for one edge when the same crate is both a
/// normal and a dev dependency of a package, and such an edge does link into the artefact — four of
/// them resolve in this workspace today. Pruning on `any` would hide part of the shipped graph from
/// the layering gate while every rule still reported `ok`.
/// `crate::layering`'s `hook_normal_and_dev_tokio_is_a_violation` is what holds this to `all`.
fn dev_only(dep: &Value) -> bool {
    dep["dep_kinds"]
        .as_array()
        .is_some_and(|kinds| !kinds.is_empty() && kinds.iter().all(|k| k["kind"] == "dev"))
}
