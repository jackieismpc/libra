//! The single operation boundary for mutating commands.

use std::{future::Future, path::Path};

use git_internal::hash::ObjectHash;
use thiserror::Error;

use super::{
    store::{
        JournalPhase, OpHead, OperationKind, OperationMetaV2, OperationStatusV2, OperationStore,
        OperationV2, StoreError,
    },
    working_copy::{PointerError, WorkspaceStatePointer},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationClass {
    ReadOnly,
    WorkingCopy,
    Repository,
    Ref,
    Index,
    External,
    InternalWorker,
}

impl MutationClass {
    fn produces_operation(self) -> bool {
        !matches!(self, Self::ReadOnly | Self::InternalWorker)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClassificationError {
    #[error("unknown command mutation class: {0}")]
    UnknownCommand(String),
}

pub fn classify_command(command_name: &str) -> Result<MutationClass, ClassificationError> {
    let name = command_name.trim().to_ascii_lowercase();
    let class = match name.as_str() {
        "status" | "log" | "show" | "diff" | "op" | "help" => MutationClass::ReadOnly,
        "add" | "checkout" | "switch" | "restore" | "reset" | "clean" => MutationClass::WorkingCopy,
        "commit" | "merge" | "rebase" | "cherry-pick" | "revert" | "am" => {
            MutationClass::Repository
        }
        "branch" | "tag" | "fetch" | "push" | "update-ref" => MutationClass::Ref,
        "index" | "update-index" => MutationClass::Index,
        "agent" | "cloud" | "external" => MutationClass::External,
        "status-io-worker" | "internal-worker" => MutationClass::InternalWorker,
        _ => {
            return Err(ClassificationError::UnknownCommand(
                command_name.to_string(),
            ));
        }
    };
    Ok(class)
}

#[derive(Debug, Error)]
pub enum MiddlewareError {
    #[error(transparent)]
    Classification(#[from] ClassificationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Pointer(#[from] PointerError),
    #[error("operation action failed: {0}")]
    Action(String),
    #[error("operation middleware requires a non-empty operation id")]
    EmptyOperationId,
}

pub struct OperationMiddleware {
    store: OperationStore,
    repo_id: String,
    scope_key: String,
    scope_kind: String,
    pointer_path: Option<std::path::PathBuf>,
}

impl OperationMiddleware {
    pub fn new(
        store: OperationStore,
        repo_id: impl Into<String>,
        scope_key: impl Into<String>,
        pointer_path: Option<impl AsRef<Path>>,
    ) -> Self {
        let scope_key = scope_key.into();
        let scope_kind = if scope_key.trim().is_empty() || scope_key == "main" {
            "main"
        } else {
            "linked"
        };
        Self {
            store,
            repo_id: repo_id.into(),
            scope_key,
            scope_kind: scope_kind.to_string(),
            pointer_path: pointer_path.map(|path| path.as_ref().to_path_buf()),
        }
    }

    /// Execute one classified command through the operation journal and
    /// publish CAS. Read-only and internal worker classes are deliberately
    /// bypassed; `InternalWorker` never produces an Operation.
    pub async fn run_with_operation<F, Fut, R>(
        &self,
        command_name: &str,
        op_id: impl Into<String>,
        pre_view_oid: ObjectHash,
        post_view_oid: ObjectHash,
        external_snapshot_oid: Option<ObjectHash>,
        action: F,
    ) -> Result<R, MiddlewareError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<R, String>>,
    {
        let class = classify_command(command_name)?;
        if !class.produces_operation() {
            return action().await.map_err(MiddlewareError::Action);
        }
        let op_id = op_id.into();
        if op_id.trim().is_empty() {
            return Err(MiddlewareError::EmptyOperationId);
        }
        let mut expected = self
            .store
            .current_op_heads(&self.repo_id, &self.scope_key)
            .await?;
        let mut effective_pre_view = pre_view_oid;
        if let Some(snapshot_oid) = external_snapshot_oid {
            let external_id = format!("{op_id}-external-snapshot");
            let external = OperationV2 {
                op_id: external_id.clone(),
                repo_id: self.repo_id.clone(),
                parent_op_ids: expected.iter().map(|head| head.op_id.clone()).collect(),
                pre_view_oid,
                post_view_oid: snapshot_oid,
                kind: OperationKind::ExternalSnapshot,
                status: OperationStatusV2::Success,
                metadata: OperationMetaV2 {
                    scope_kind: self.scope_kind.clone(),
                    ..Default::default()
                },
                restores_op_id: None,
                reverts_op_id: None,
                predecessor_map_oid: None,
                start_ts: 0,
                end_ts: Some(0),
            };
            self.store.write_operation(&external).await?;
            let generation = expected
                .iter()
                .map(|head| head.generation)
                .max()
                .unwrap_or(0)
                + 1;
            expected = self
                .store
                .cas_update_op_heads(
                    &self.repo_id,
                    &self.scope_key,
                    &expected,
                    &[OpHead {
                        op_id: external_id,
                        generation,
                    }],
                )
                .await?;
            effective_pre_view = snapshot_oid;
        }

        let parents = expected.iter().map(|head| head.op_id.clone()).collect();
        let operation = OperationV2 {
            op_id: op_id.clone(),
            repo_id: self.repo_id.clone(),
            parent_op_ids: parents,
            pre_view_oid: effective_pre_view,
            post_view_oid,
            kind: OperationKind::Command,
            status: OperationStatusV2::Running,
            metadata: OperationMetaV2 {
                command_name: Some(command_name.to_string()),
                scope_kind: self.scope_kind.clone(),
                ..Default::default()
            },
            restores_op_id: None,
            reverts_op_id: None,
            predecessor_map_oid: None,
            start_ts: 0,
            end_ts: None,
        };
        self.store.write_operation(&operation).await?;
        let journal_id = format!("journal-{op_id}");
        self.store
            .reserve_journal(&journal_id, &op_id, "middleware", 0)
            .await?;
        self.store
            .record_journal_phase(
                &journal_id,
                JournalPhase::PreView,
                Some(effective_pre_view),
                None,
                None,
                0,
            )
            .await?;
        self.store
            .record_journal_phase(
                &journal_id,
                JournalPhase::Mutation,
                Some(effective_pre_view),
                None,
                None,
                0,
            )
            .await?;
        let result = action().await;
        match result {
            Ok(value) => {
                self.store
                    .record_journal_phase(
                        &journal_id,
                        JournalPhase::PostView,
                        Some(effective_pre_view),
                        Some(post_view_oid),
                        None,
                        1,
                    )
                    .await?;
                self.store
                    .update_operation_status(&op_id, OperationStatusV2::Success, Some(1))
                    .await?;
                let generation = expected
                    .iter()
                    .map(|head| head.generation)
                    .max()
                    .unwrap_or(0)
                    + 1;
                self.store
                    .cas_update_op_heads(
                        &self.repo_id,
                        &self.scope_key,
                        &expected,
                        &[OpHead {
                            op_id: op_id.clone(),
                            generation,
                        }],
                    )
                    .await?;
                self.store
                    .record_journal_phase(
                        &journal_id,
                        JournalPhase::Publish,
                        Some(effective_pre_view),
                        Some(post_view_oid),
                        None,
                        1,
                    )
                    .await?;
                if let Some(path) = &self.pointer_path {
                    WorkspaceStatePointer::new(op_id, post_view_oid, generation)?.save(path)?;
                }
                Ok(value)
            }
            Err(error) => {
                self.store
                    .update_operation_status(&op_id, OperationStatusV2::Failed, Some(1))
                    .await?;
                Err(MiddlewareError::Action(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    use super::*;

    #[test]
    fn all_seven_classes_are_explicit() {
        let names = [
            "status",
            "add",
            "commit",
            "branch",
            "index",
            "agent",
            "internal-worker",
        ];
        let classes = names
            .iter()
            .map(|name| classify_command(name).expect("class"))
            .collect::<Vec<_>>();
        assert_eq!(
            classes,
            vec![
                MutationClass::ReadOnly,
                MutationClass::WorkingCopy,
                MutationClass::Repository,
                MutationClass::Ref,
                MutationClass::Index,
                MutationClass::External,
                MutationClass::InternalWorker,
            ]
        );
    }

    #[test]
    fn unknown_commands_fail_closed() {
        assert!(matches!(
            classify_command("future-command"),
            Err(ClassificationError::UnknownCommand(_))
        ));
    }

    #[tokio::test]
    async fn failed_action_keeps_failed_operation() {
        let db = Database::connect("sqlite::memory:").await.expect("db");
        db.execute_unprepared("CREATE TABLE operation(op_id TEXT PRIMARY KEY,repo_id TEXT NOT NULL,format_version INTEGER NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,command_name TEXT,description TEXT,args_digest TEXT,actor TEXT,worktree_id TEXT,scope_kind TEXT NOT NULL,pre_view_oid TEXT NOT NULL,post_view_oid TEXT NOT NULL,restores_op_id TEXT,reverts_op_id TEXT,predecessor_map_oid TEXT,causal_context_id TEXT,start_ts INTEGER NOT NULL,end_ts INTEGER); CREATE TABLE operation_parent(op_id TEXT NOT NULL,parent_op_id TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(op_id,parent_op_id)); CREATE TABLE operation_head(repo_id TEXT NOT NULL,scope_key TEXT NOT NULL,op_id TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(repo_id,scope_key,op_id)); CREATE TABLE operation_journal(journal_id TEXT PRIMARY KEY,op_id TEXT NOT NULL,phase TEXT NOT NULL,pre_view_oid TEXT,target_view_oid TEXT,owner TEXT NOT NULL,updated_at INTEGER NOT NULL,recovery_payload TEXT);").await.expect("schema");
        let objects = tempfile::tempdir().expect("objects");
        let store = OperationStore::new(
            db,
            crate::utils::client_storage::ClientStorage::init_local(objects.path().to_path_buf()),
        );
        let middleware = OperationMiddleware::new(store.clone(), "repo", "main", None::<&Path>);
        let error = middleware
            .run_with_operation::<_, _, ()>(
                "commit",
                "op",
                ObjectHash::from_bytes(&[1; 20]).unwrap(),
                ObjectHash::from_bytes(&[2; 20]).unwrap(),
                None,
                || async { Err("boom".to_string()) },
            )
            .await
            .expect_err("failure");
        assert!(matches!(error, MiddlewareError::Action(_)));
        assert_eq!(
            store.load_operation("op").await.unwrap().unwrap().status,
            OperationStatusV2::Failed
        );
    }
}
