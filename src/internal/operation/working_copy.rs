//! Workspace state pointer and operation-head freshness checks.
//!
//! The pointer is a tiny, crash-safe sidecar in the resolved gitdir.  It is
//! not the source of truth for operation history: operation_head remains
//! authoritative, and this file only records which published snapshot the
//! working copy last materialized.

use std::path::PathBuf;

use git_internal::hash::ObjectHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::store::OpHeadsView;

/// The request-scoped path carrier used by pointer I/O.
pub type PinnedRequestScope = crate::internal::worktree_scope::RequestScope;

const POINTER_FILE_NAME: &str = "workspace-state-pointer.json";

/// The last operation/snapshot materialized into one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStatePointer {
    pub last_op_id: String,
    pub last_snapshot_oid: ObjectHash,
    pub generation: u64,
}

#[derive(Debug, Error)]
pub enum PointerError {
    #[error("workspace state pointer is missing at {0}")]
    Missing(PathBuf),
    #[error("failed to read workspace state pointer: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace state pointer JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace state pointer has an empty operation id")]
    EmptyOperationId,
    #[error("workspace state pointer write task failed: {0}")]
    WriteTask(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    Stale,
    Sibling,
}

impl WorkspaceStatePointer {
    pub fn path(scope: &PinnedRequestScope) -> PathBuf {
        scope.gitdir.join(POINTER_FILE_NAME)
    }

    pub async fn load(scope: &PinnedRequestScope) -> Result<Self, PointerError> {
        let path = Self::path(scope);
        let bytes = tokio::fs::read(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PointerError::Missing(path.clone())
            } else {
                PointerError::Io(error)
            }
        })?;
        let pointer = serde_json::from_slice::<Self>(&bytes)?;
        pointer.validate()?;
        Ok(pointer)
    }

    pub async fn save(&self, scope: &PinnedRequestScope) -> Result<(), PointerError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        let path = Self::path(scope);
        tokio::task::spawn_blocking(move || {
            crate::utils::atomic_write::write_atomic(&path, &bytes, true)
        })
        .await
        .map_err(|error| PointerError::WriteTask(error.to_string()))??;
        Ok(())
    }

    pub fn staleness(&self, heads: &OpHeadsView) -> Staleness {
        let Some(pointer_generation) = heads.generation(&self.last_op_id) else {
            return if heads.heads.keys().any(|head| {
                heads.is_ancestor(head, &self.last_op_id)
                    || heads.is_ancestor(&self.last_op_id, head)
            }) {
                Staleness::Stale
            } else {
                Staleness::Sibling
            };
        };

        if pointer_generation == self.generation {
            return Staleness::Fresh;
        }

        Staleness::Stale
    }

    fn validate(&self) -> Result<(), PointerError> {
        if self.last_op_id.is_empty() {
            return Err(PointerError::EmptyOperationId);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::internal::worktree_scope::{RequestScope, WorktreeScope};

    fn pointer(op_id: &str, generation: u64) -> WorkspaceStatePointer {
        WorkspaceStatePointer {
            last_op_id: op_id.to_string(),
            last_snapshot_oid: ObjectHash::from_type_and_data(
                git_internal::internal::object::types::ObjectType::Blob,
                op_id.as_bytes(),
            ),
            generation,
        }
    }

    #[test]
    fn matching_head_and_generation_is_fresh() {
        let heads =
            OpHeadsView::with_generations(vec![("op-1".to_string(), 3)]).expect("valid heads");
        assert_eq!(pointer("op-1", 3).staleness(&heads), Staleness::Fresh);
    }

    #[test]
    fn generation_mismatch_is_stale() {
        let heads =
            OpHeadsView::with_generations(vec![("op-1".to_string(), 4)]).expect("valid heads");
        assert_eq!(pointer("op-1", 3).staleness(&heads), Staleness::Stale);
    }

    #[test]
    fn unrelated_head_is_sibling() {
        let heads = OpHeadsView::new(vec!["op-2".to_string()]).expect("valid heads");
        assert_eq!(pointer("op-1", 1).staleness(&heads), Staleness::Sibling);
    }

    #[test]
    fn descendant_head_is_stale() {
        let mut heads = OpHeadsView::new(vec!["op-2".to_string()]).expect("valid heads");
        heads.add_ancestor("op-2", "op-1").expect("valid edge");
        assert_eq!(pointer("op-1", 0).staleness(&heads), Staleness::Stale);
    }

    #[tokio::test]
    async fn pointer_round_trip_uses_atomic_scope_path() {
        let dir = TempDir::new().expect("temporary pointer directory");
        let scope = RequestScope {
            scope: WorktreeScope::Main,
            workdir: dir.path().to_path_buf(),
            gitdir: dir.path().to_path_buf(),
            storage: dir.path().to_path_buf(),
            worktree_root: dir.path().to_path_buf(),
        };
        let expected = pointer("op-1", 2);
        expected.save(&scope).await.expect("pointer saves");
        assert_eq!(
            WorkspaceStatePointer::load(&scope)
                .await
                .expect("pointer loads"),
            expected
        );
        assert!(WorkspaceStatePointer::path(&scope).is_file());
    }

    #[tokio::test]
    async fn missing_pointer_is_reported_without_fallback() {
        let dir = TempDir::new().expect("temporary pointer directory");
        let scope = RequestScope {
            scope: WorktreeScope::Main,
            workdir: dir.path().to_path_buf(),
            gitdir: dir.path().join("gitdir"),
            storage: dir.path().to_path_buf(),
            worktree_root: dir.path().to_path_buf(),
        };
        let result = WorkspaceStatePointer::load(&scope).await;
        assert!(matches!(result, Err(PointerError::Missing(_))));
    }
}
