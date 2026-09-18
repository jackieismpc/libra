//! Isolated workspace types and materialization — plan-20260920 RC-03/RC-07.
//!
//! KEEP `review` / `investigate` depend on this module instead of
//! `agent::runtime` or `orchestrator`. Orchestrator types are re-exported
//! until RC-23 deletes the executor SCC.

use anyhow::Result;
use uuid::Uuid;

pub use crate::internal::ai::orchestrator::{
    types::TaskWorkspaceBackend,
    workspace::{FuseProvisionState, SubAgentWorkspace, SubAgentWorkspaceError},
};
use crate::internal::ai::{
    agent_run::{
        AgentRunId, event_store::AgentRunEventStore, workspace_sizing::measure_workspace_sizing,
    },
    orchestrator::workspace::materialize_sub_agent_workspace,
};

/// Isolation settings for a review / investigate / sub-agent workspace.
#[derive(Clone)]
pub struct WorkspaceIsolationConfig {
    /// Per-session FUSE-provisioning state (degrades to copy backend
    /// when FUSE is unavailable). `Arc`-backed, cheap to clone.
    pub fuse_state: FuseProvisionState,
    /// `.libra/sessions` root the per-run `AgentRunEventStore` writes the
    /// `WorkspaceMaterialized` event under.
    pub sessions_root: std::path::PathBuf,
    /// Whether an expensive full-copy fallback is permitted when the
    /// preferred (size-selected) strategy cannot be materialized
    /// (`code.multi_agent.allow_full_copy`).
    pub allow_full_copy: bool,
}

impl std::fmt::Debug for WorkspaceIsolationConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceIsolationConfig")
            .field("sessions_root", &self.sessions_root)
            .field("allow_full_copy", &self.allow_full_copy)
            .finish_non_exhaustive()
    }
}

/// Materialize an isolated workspace for a review / investigate / sub-agent
/// run. Isolation failure is terminal: falling back to the main worktree
/// would violate S2-INV-03.
pub fn materialize_isolated_workspace(
    main_working_dir: &std::path::Path,
    thread_id: Uuid,
    agent_run_id: AgentRunId,
    isolation: &WorkspaceIsolationConfig,
) -> Result<SubAgentWorkspace, SubAgentWorkspaceError> {
    let sizing = measure_workspace_sizing(
        &main_working_dir.join(crate::utils::util::ROOT_DIR),
        main_working_dir,
    );
    let store = AgentRunEventStore::new(isolation.sessions_root.clone());

    materialize_sub_agent_workspace(
        main_working_dir,
        sizing,
        thread_id,
        agent_run_id,
        isolation.allow_full_copy,
        &isolation.fuse_state,
        &store,
    )
}
