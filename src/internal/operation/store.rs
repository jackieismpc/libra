//! Operation Log v2 persistence, journal, and op-head compare-and-swap.

use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use git_internal::hash::ObjectHash;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    IntoActiveModel, QueryFilter, QueryOrder, TransactionTrait,
};
use thiserror::Error;

use crate::{
    internal::{
        model::{operation_head, operation_journal, operation_parent_v2, operation_v2},
        operation::view::{RepoViewV2, ViewCodecError},
    },
    utils::client_storage::ClientStorage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Command,
    ExternalSnapshot,
    Undo,
    Redo,
    Restore,
    Revert,
    Reconcile,
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::ExternalSnapshot => "external_snapshot",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Restore => "restore",
            Self::Revert => "revert",
            Self::Reconcile => "reconcile",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "command" => Ok(Self::Command),
            "external_snapshot" => Ok(Self::ExternalSnapshot),
            "undo" => Ok(Self::Undo),
            "redo" => Ok(Self::Redo),
            "restore" => Ok(Self::Restore),
            "revert" => Ok(Self::Revert),
            "reconcile" => Ok(Self::Reconcile),
            other => Err(StoreError::Corrupt(format!(
                "unknown operation kind '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatusV2 {
    Running,
    Success,
    Failed,
    Partial,
    Aborted,
}

impl OperationStatusV2 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Partial => "partial",
            Self::Aborted => "aborted",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "partial" => Ok(Self::Partial),
            "aborted" => Ok(Self::Aborted),
            other => Err(StoreError::Corrupt(format!(
                "unknown operation status '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationMetaV2 {
    pub command_name: Option<String>,
    pub description: Option<String>,
    pub args_digest: Option<String>,
    pub actor: Option<String>,
    pub worktree_id: Option<String>,
    pub scope_kind: String,
    pub causal_context_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationV2 {
    pub op_id: String,
    pub repo_id: String,
    pub parent_op_ids: Vec<String>,
    pub pre_view_oid: ObjectHash,
    pub post_view_oid: ObjectHash,
    pub kind: OperationKind,
    pub status: OperationStatusV2,
    pub metadata: OperationMetaV2,
    pub restores_op_id: Option<String>,
    pub reverts_op_id: Option<String>,
    pub predecessor_map_oid: Option<ObjectHash>,
    pub start_ts: i64,
    pub end_ts: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpHead {
    pub op_id: String,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationJournalEntry {
    pub journal_id: String,
    pub op_id: String,
    pub phase: JournalPhase,
    pub pre_view_oid: Option<ObjectHash>,
    pub target_view_oid: Option<ObjectHash>,
    pub owner: String,
    pub updated_at: i64,
    pub recovery_payload: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalPhase {
    Reserved,
    PreView,
    Mutation,
    PostView,
    Publish,
}

impl JournalPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::PreView => "pre_view",
            Self::Mutation => "mutation",
            Self::PostView => "post_view",
            Self::Publish => "publish",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "pre_view" => Ok(Self::PreView),
            "mutation" => Ok(Self::Mutation),
            "post_view" => Ok(Self::PostView),
            "publish" => Ok(Self::Publish),
            other => Err(StoreError::Corrupt(format!(
                "unknown journal phase '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("operation argument is invalid: {0}")]
    InvalidArgument(String),
    #[error("operation database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    #[error("operation storage error: {0}")]
    Storage(String),
    #[error("operation data is corrupt: {0}")]
    Corrupt(String),
    #[error("operation head CAS conflict; current heads: {current:?}")]
    CasConflict { current: Vec<OpHead> },
    #[error("operation object {0} has an invalid hash")]
    InvalidObjectHash(String),
}

#[derive(Clone)]
pub struct OperationStore {
    db: DatabaseConnection,
    storage: ClientStorage,
}

impl std::fmt::Debug for OperationStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationStore")
            .finish_non_exhaustive()
    }
}

impl OperationStore {
    pub fn new(db: DatabaseConnection, storage: ClientStorage) -> Self {
        Self { db, storage }
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub fn write_view_manifest(&self, view: &RepoViewV2) -> Result<ObjectHash, StoreError> {
        view.write_manifest(&self.storage)
            .map_err(|error| StoreError::Storage(error.to_string()))
    }

    pub fn load_view(&self, view_oid: &ObjectHash) -> Result<RepoViewV2, StoreError> {
        if self
            .storage
            .get_object_type(view_oid)
            .map_err(|error| StoreError::Storage(error.to_string()))?
            != git_internal::internal::object::types::ObjectType::Blob
        {
            return Err(StoreError::Corrupt(format!(
                "view object {view_oid} is not a blob"
            )));
        }
        let bytes = self
            .storage
            .get(view_oid)
            .map_err(|error| StoreError::Storage(error.to_string()))?;
        RepoViewV2::from_canonical_bytes(&bytes).map_err(|error| match error {
            ViewCodecError::InvalidJson(message) => StoreError::Corrupt(message),
            other => StoreError::Corrupt(other.to_string()),
        })
    }

    pub async fn write_operation(&self, operation: &OperationV2) -> Result<(), StoreError> {
        validate_operation(operation)?;
        let txn = self.db.begin().await?;
        let model = operation_v2::ActiveModel {
            op_id: Set(operation.op_id.clone()),
            repo_id: Set(operation.repo_id.clone()),
            format_version: Set(2),
            kind: Set(operation.kind.as_str().to_string()),
            status: Set(operation.status.as_str().to_string()),
            command_name: Set(operation.metadata.command_name.clone()),
            description: Set(operation.metadata.description.clone()),
            args_digest: Set(operation.metadata.args_digest.clone()),
            actor: Set(operation.metadata.actor.clone()),
            worktree_id: Set(operation.metadata.worktree_id.clone()),
            scope_kind: Set(operation.metadata.scope_kind.clone()),
            pre_view_oid: Set(operation.pre_view_oid.to_string()),
            post_view_oid: Set(operation.post_view_oid.to_string()),
            restores_op_id: Set(operation.restores_op_id.clone()),
            reverts_op_id: Set(operation.reverts_op_id.clone()),
            predecessor_map_oid: Set(operation.predecessor_map_oid.map(|oid| oid.to_string())),
            causal_context_id: Set(operation.metadata.causal_context_id.clone()),
            start_ts: Set(operation.start_ts),
            end_ts: Set(operation.end_ts),
        };
        model.insert(&txn).await?;
        for (ordinal, parent_op_id) in operation.parent_op_ids.iter().enumerate() {
            operation_parent_v2::ActiveModel {
                op_id: Set(operation.op_id.clone()),
                parent_op_id: Set(parent_op_id.clone()),
                ordinal: Set(i32::try_from(ordinal).map_err(|_| {
                    StoreError::InvalidArgument("too many operation parents".to_string())
                })?),
            }
            .insert(&txn)
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    pub async fn load_operation(&self, op_id: &str) -> Result<Option<OperationV2>, StoreError> {
        let Some(model) = operation_v2::Entity::find_by_id(op_id.to_string())
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        let parents = operation_parent_v2::Entity::find()
            .filter(operation_parent_v2::Column::OpId.eq(op_id.to_string()))
            .order_by_asc(operation_parent_v2::Column::Ordinal)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|parent| parent.parent_op_id)
            .collect();
        Ok(Some(operation_from_model(model, parents)?))
    }

    pub async fn current_op_heads(
        &self,
        repo_id: &str,
        scope_key: &str,
    ) -> Result<Vec<OpHead>, StoreError> {
        Ok(operation_head::Entity::find()
            .filter(operation_head::Column::RepoId.eq(repo_id.to_string()))
            .filter(operation_head::Column::ScopeKey.eq(scope_key.to_string()))
            .order_by_asc(operation_head::Column::Generation)
            .order_by_asc(operation_head::Column::OpId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| OpHead {
                op_id: row.op_id,
                generation: row.generation,
            })
            .collect())
    }

    /// Atomically replace the head set when `expected` still matches. If it
    /// does not, the proposed heads are retained as additional heads so a
    /// concurrent branch is never silently lost.
    pub async fn cas_update_op_heads(
        &self,
        repo_id: &str,
        scope_key: &str,
        expected: &[OpHead],
        proposed: &[OpHead],
    ) -> Result<Vec<OpHead>, StoreError> {
        let txn = self.db.begin().await?;
        let current = operation_head::Entity::find()
            .filter(operation_head::Column::RepoId.eq(repo_id.to_string()))
            .filter(operation_head::Column::ScopeKey.eq(scope_key.to_string()))
            .order_by_asc(operation_head::Column::Generation)
            .order_by_asc(operation_head::Column::OpId)
            .all(&txn)
            .await?
            .into_iter()
            .map(|row| OpHead {
                op_id: row.op_id,
                generation: row.generation,
            })
            .collect::<Vec<_>>();
        if current != expected {
            for head in proposed {
                operation_head::ActiveModel {
                    repo_id: Set(repo_id.to_string()),
                    scope_key: Set(scope_key.to_string()),
                    op_id: Set(head.op_id.clone()),
                    generation: Set(head.generation),
                }
                .insert(&txn)
                .await?;
            }
            txn.commit().await?;
            let mut merged = current;
            for head in proposed {
                if !merged.contains(head) {
                    merged.push(head.clone());
                }
            }
            merged.sort_by(|left, right| {
                left.generation
                    .cmp(&right.generation)
                    .then_with(|| left.op_id.cmp(&right.op_id))
            });
            return Err(StoreError::CasConflict { current: merged });
        }

        operation_head::Entity::delete_many()
            .filter(operation_head::Column::RepoId.eq(repo_id.to_string()))
            .filter(operation_head::Column::ScopeKey.eq(scope_key.to_string()))
            .exec(&txn)
            .await?;
        for head in proposed {
            operation_head::ActiveModel {
                repo_id: Set(repo_id.to_string()),
                scope_key: Set(scope_key.to_string()),
                op_id: Set(head.op_id.clone()),
                generation: Set(head.generation),
            }
            .insert(&txn)
            .await?;
        }
        txn.commit().await?;
        Ok(proposed.to_vec())
    }

    pub async fn reserve_journal(
        &self,
        journal_id: &str,
        op_id: &str,
        owner: &str,
        updated_at: i64,
    ) -> Result<(), StoreError> {
        operation_journal::ActiveModel {
            journal_id: Set(journal_id.to_string()),
            op_id: Set(op_id.to_string()),
            phase: Set(JournalPhase::Reserved.as_str().to_string()),
            pre_view_oid: Set(None),
            target_view_oid: Set(None),
            owner: Set(owner.to_string()),
            updated_at: Set(updated_at),
            recovery_payload: Set(None),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    pub async fn record_journal_phase(
        &self,
        journal_id: &str,
        phase: JournalPhase,
        pre_view_oid: Option<ObjectHash>,
        target_view_oid: Option<ObjectHash>,
        recovery_payload: Option<String>,
        updated_at: i64,
    ) -> Result<(), StoreError> {
        let mut model = operation_journal::Entity::find_by_id(journal_id.to_string())
            .one(&self.db)
            .await?
            .ok_or_else(|| StoreError::InvalidArgument(format!("unknown journal '{journal_id}'")))?
            .into_active_model();
        model.phase = Set(phase.as_str().to_string());
        model.pre_view_oid = Set(pre_view_oid.map(|oid| oid.to_string()));
        model.target_view_oid = Set(target_view_oid.map(|oid| oid.to_string()));
        model.recovery_payload = Set(recovery_payload);
        model.updated_at = Set(updated_at);
        model.update(&self.db).await?;
        Ok(())
    }

    pub async fn load_journal(
        &self,
        journal_id: &str,
    ) -> Result<Option<OperationJournalEntry>, StoreError> {
        let Some(model) = operation_journal::Entity::find_by_id(journal_id.to_string())
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(journal_from_model(model)?))
    }
}

fn validate_operation(operation: &OperationV2) -> Result<(), StoreError> {
    if operation.op_id.trim().is_empty() || operation.repo_id.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "op_id and repo_id must not be empty".to_string(),
        ));
    }
    if operation.metadata.scope_kind.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "scope_kind must not be empty".to_string(),
        ));
    }
    if operation.end_ts.is_some_and(|end| end < operation.start_ts) {
        return Err(StoreError::InvalidArgument(
            "end_ts must not precede start_ts".to_string(),
        ));
    }
    Ok(())
}

fn parse_oid(value: &str) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_str(value).map_err(|_| StoreError::InvalidObjectHash(value.to_string()))
}

fn operation_from_model(
    model: operation_v2::Model,
    parent_op_ids: Vec<String>,
) -> Result<OperationV2, StoreError> {
    if model.format_version != 2 {
        return Err(StoreError::Corrupt(format!(
            "unsupported operation format version {}",
            model.format_version
        )));
    }
    Ok(OperationV2 {
        op_id: model.op_id,
        repo_id: model.repo_id,
        parent_op_ids,
        pre_view_oid: parse_oid(&model.pre_view_oid)?,
        post_view_oid: parse_oid(&model.post_view_oid)?,
        kind: OperationKind::parse(&model.kind)?,
        status: OperationStatusV2::parse(&model.status)?,
        metadata: OperationMetaV2 {
            command_name: model.command_name,
            description: model.description,
            args_digest: model.args_digest,
            actor: model.actor,
            worktree_id: model.worktree_id,
            scope_kind: model.scope_kind,
            causal_context_id: model.causal_context_id,
        },
        restores_op_id: model.restores_op_id,
        reverts_op_id: model.reverts_op_id,
        predecessor_map_oid: model
            .predecessor_map_oid
            .as_deref()
            .map(parse_oid)
            .transpose()?,
        start_ts: model.start_ts,
        end_ts: model.end_ts,
    })
}

fn journal_from_model(
    model: operation_journal::Model,
) -> Result<OperationJournalEntry, StoreError> {
    Ok(OperationJournalEntry {
        journal_id: model.journal_id,
        op_id: model.op_id,
        phase: JournalPhase::parse(&model.phase)?,
        pre_view_oid: model.pre_view_oid.as_deref().map(parse_oid).transpose()?,
        target_view_oid: model
            .target_view_oid
            .as_deref()
            .map(parse_oid)
            .transpose()?,
        owner: model.owner,
        updated_at: model.updated_at,
        recovery_payload: model.recovery_payload,
    })
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use sea_orm::Database;

    use super::*;

    async fn store() -> OperationStore {
        let db = Database::connect("sqlite::memory:").await.expect("db");
        db.execute_unprepared(
            "CREATE TABLE operation(op_id TEXT PRIMARY KEY,repo_id TEXT NOT NULL,format_version INTEGER NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,command_name TEXT,description TEXT,args_digest TEXT,actor TEXT,worktree_id TEXT,scope_kind TEXT NOT NULL,pre_view_oid TEXT NOT NULL,post_view_oid TEXT NOT NULL,restores_op_id TEXT,reverts_op_id TEXT,predecessor_map_oid TEXT,causal_context_id TEXT,start_ts INTEGER NOT NULL,end_ts INTEGER); CREATE TABLE operation_parent(op_id TEXT NOT NULL,parent_op_id TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(op_id,parent_op_id)); CREATE TABLE operation_head(repo_id TEXT NOT NULL,scope_key TEXT NOT NULL,op_id TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(repo_id,scope_key,op_id)); CREATE TABLE operation_journal(journal_id TEXT PRIMARY KEY,op_id TEXT NOT NULL,phase TEXT NOT NULL,pre_view_oid TEXT,target_view_oid TEXT,owner TEXT NOT NULL,updated_at INTEGER NOT NULL,recovery_payload TEXT);",
        )
        .await
        .expect("schema");
        let dir = tempfile::tempdir().expect("temp");
        OperationStore::new(db, ClientStorage::init_local(dir.path().join("objects")))
    }

    fn oid(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes(&[byte; 20]).expect("oid")
    }

    fn operation() -> OperationV2 {
        OperationV2 {
            op_id: "op-1".to_string(),
            repo_id: "repo-1".to_string(),
            parent_op_ids: vec!["op-0".to_string(), "op-parent".to_string()],
            pre_view_oid: oid(1),
            post_view_oid: oid(2),
            kind: OperationKind::Command,
            status: OperationStatusV2::Success,
            metadata: OperationMetaV2 {
                command_name: Some("commit".to_string()),
                scope_kind: "main".to_string(),
                ..OperationMetaV2::default()
            },
            restores_op_id: None,
            reverts_op_id: None,
            predecessor_map_oid: None,
            start_ts: 1,
            end_ts: Some(2),
        }
    }

    #[tokio::test]
    async fn operation_and_journal_roundtrip() {
        let store = store().await;
        store.write_operation(&operation()).await.expect("write");
        let loaded = store
            .load_operation("op-1")
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded, operation());
        store
            .reserve_journal("journal-1", "op-1", "worker", 3)
            .await
            .expect("reserve");
        store
            .record_journal_phase(
                "journal-1",
                JournalPhase::PreView,
                Some(oid(1)),
                Some(oid(2)),
                None,
                4,
            )
            .await
            .expect("phase");
        assert_eq!(
            store
                .load_journal("journal-1")
                .await
                .unwrap()
                .unwrap()
                .phase,
            JournalPhase::PreView
        );
    }

    #[tokio::test]
    async fn head_cas_conflict_retains_both_branches() {
        let store = store().await;
        let first = [OpHead {
            op_id: "op-a".to_string(),
            generation: 1,
        }];
        store
            .cas_update_op_heads("repo", "main", &[], &first)
            .await
            .expect("first publish");
        let second = [OpHead {
            op_id: "op-b".to_string(),
            generation: 2,
        }];
        let error = store
            .cas_update_op_heads("repo", "main", &[], &second)
            .await
            .expect_err("stale CAS");
        assert!(matches!(error, StoreError::CasConflict { .. }));
        let heads = store.current_op_heads("repo", "main").await.expect("heads");
        assert_eq!(heads, vec![first[0].clone(), second[0].clone()]);
    }
}
