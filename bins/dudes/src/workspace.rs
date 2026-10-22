//! `dudes project add`: resolve the CLI's options into a [`NewWorkspace`]. The file edit itself
//! lives in `dudes-config`.

use std::path::{Path, PathBuf};

pub use dudes_config::workspace::*;

/// Enrollment options before repository defaults have been resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddOptions {
    /// Repository directory; the current directory when absent.
    pub path: Option<PathBuf>,
    /// Project id; a unique folder slug when absent.
    pub id: Option<String>,
    /// Display label; the id when absent.
    pub label: Option<String>,
    /// Check label; the check's argv when absent.
    pub completion_check_label: Option<String>,
    /// Department.
    pub department: String,
    /// Agent dollar ceiling.
    pub agent_usd_per_day: Option<String>,
    /// Goal dollar ceiling.
    pub goal_usd: Option<String>,
    /// Turn ceiling.
    pub hop_cap: u32,
    /// Explicit check; the repository's suggestion when absent.
    pub predicate_argv: Option<Vec<String>>,
    /// Check timeout.
    pub predicate_timeout_s: u64,
    /// Agent token ceiling.
    pub tokens_per_day: Option<u64>,
    /// Goal token ceiling.
    pub goal_tokens: Option<u64>,
}

impl AddOptions {
    pub(crate) fn resolve(
        &self,
        cwd: Option<&Path>,
        ids: &[String],
    ) -> Result<NewWorkspace, WorkspaceError> {
        let path = if let Some(path) = &self.path {
            canonical_repository(path)?
        } else {
            let cwd = cwd.ok_or(WorkspaceError::NoCurrentDirectory)?;
            let canonical = cwd.canonicalize().map_err(|source| WorkspaceError::Io {
                path: cwd.to_path_buf(),
                source,
            })?;
            canonical_repository(&canonical)?
        };
        let argv = self
            .predicate_argv
            .clone()
            .or_else(|| dudes_config::check::suggest_check(&path))
            .unwrap_or_else(dudes_config::check::changed_check);
        let id = self.id.clone().unwrap_or_else(|| project_id(&path, ids));
        Ok(NewWorkspace {
            path,
            label: self.label.clone().unwrap_or_else(|| id.clone()),
            id,
            completion_check_label: self
                .completion_check_label
                .clone()
                .unwrap_or_else(|| dudes_config::check::check_label(&argv)),
            department: self.department.clone(),
            agent_usd_per_day: self.agent_usd_per_day.clone(),
            goal_usd: self.goal_usd.clone(),
            hop_cap: self.hop_cap,
            predicate_argv: argv,
            predicate_timeout_s: self.predicate_timeout_s,
            tokens_per_day: self.tokens_per_day,
            goal_tokens: self.goal_tokens,
            may_already_pass: false,
        })
    }
}
