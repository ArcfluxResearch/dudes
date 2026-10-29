//! Immutable inputs for one app-server process and one issued task.

use std::fmt;
use std::path::{Path, PathBuf};

use dudes_types::ModelSelection;

use crate::adapter::TurnSession;
use crate::protocol::StartupFrame;

/// One task and the daemon-owned profile in which its thread must run.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexStartup {
    pub(crate) home: PathBuf,
    pub(crate) agent_dir: PathBuf,
    pub(crate) cwd: PathBuf,
    canonical_cwd: Option<PathBuf>,
    scoped_roots: Option<Box<[PathBuf]>>,
    pub(crate) task: String,
    pub(crate) session: TurnSession,
    pub(crate) selection: ModelSelection,
}

impl CodexStartup {
    /// Capture the already-issued task. Filesystem admission belongs to the daemon.
    #[must_use]
    pub fn new(
        home: PathBuf,
        agent_dir: PathBuf,
        cwd: PathBuf,
        task: String,
        session: TurnSession,
        selection: ModelSelection,
    ) -> Self {
        Self {
            home,
            agent_dir,
            cwd,
            canonical_cwd: None,
            scoped_roots: None,
            task,
            session,
            selection,
        }
    }

    /// Persistent provider-owned auth and session directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Per-agent directory; private temporary files live below it.
    #[must_use]
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    pub(crate) fn writable_roots(&self) -> Vec<PathBuf> {
        if let Some(roots) = &self.scoped_roots {
            return roots.to_vec();
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let roots = vec![self.agent_dir.join("tmp"), self.agent_dir.join("outbox")];
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let roots = vec![self.agent_dir.join("tmp")];
        roots
    }

    pub(crate) fn with_scoped_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.scoped_roots = Some(roots.into_boxed_slice());
        self
    }

    pub(crate) const fn is_scoped(&self) -> bool {
        self.scoped_roots.is_some()
    }

    /// The exact ordered additional roots of an unarmed scoped recipe.
    #[must_use]
    pub fn scoped_roots(&self) -> Option<&[PathBuf]> {
        self.scoped_roots.as_deref()
    }

    /// Session inputs captured for this exact issued task.
    #[must_use]
    pub fn session(&self) -> &TurnSession {
        &self.session
    }

    /// Working directory captured with this task.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Replace filesystem spellings after the daemon has resolved and admitted them.
    #[must_use]
    pub fn resolved_paths(
        &self,
        home: PathBuf,
        agent_dir: PathBuf,
        canonical_cwd: PathBuf,
    ) -> Self {
        Self {
            home,
            agent_dir,
            canonical_cwd: Some(canonical_cwd),
            ..self.clone()
        }
    }

    /// Workspace target resolved during the launch admission.
    #[must_use]
    pub fn canonical_cwd(&self) -> Option<&Path> {
        self.canonical_cwd.as_deref()
    }

    /// First request, containing no task or authentication data.
    #[must_use]
    pub fn initialize() -> StartupFrame {
        StartupFrame::new(
            br#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"dudes","version":"1"},"capabilities":{"explicitGatewayOauth":true,"experimentalApi":false,"requestAttestation":false}}}"#.to_vec(),
            false,
        ).expect("the fixed initialization frame is bounded")
    }
}

impl fmt::Debug for CodexStartup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexStartup")
            .field("home", &self.home)
            .field("agent_dir", &self.agent_dir)
            .field("cwd", &self.cwd)
            .field("canonical_cwd", &self.canonical_cwd)
            .field("scoped_roots", &self.scoped_roots)
            .field("task_bytes", &self.task.len())
            .field("session", &self.session)
            .field("selection", &self.selection)
            .finish()
    }
}
