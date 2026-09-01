//! Durable storage for the v2 operation DAG.
//!
//! The operation row is deliberately small and redacted.  View manifests are
//! content-addressed objects, while SQLite stores the searchable DAG edges,
//! head generations, and recovery journal.  A publish therefore follows one
//! ordering rule: write immutable objects first, then publish the relational
//! rows in a write-locked transaction.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use chrono::Utc;
use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, QueryResult, Statement};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    internal::{
        db::begin_write_transaction,
        operation::view::{RepoViewV2, ViewError},
    },
    utils::client_storage::ClientStorage,
};

/// The semantic kind of an operation-log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    fn as_str(self) -> &'static str {
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
}

impl fmt::Display for OperationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OperationKind {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "command" => Ok(Self::Command),
            "external_snapshot" => Ok(Self::ExternalSnapshot),
            "undo" => Ok(Self::Undo),
            "redo" => Ok(Self::Redo),
            "restore" => Ok(Self::Restore),
            "revert" => Ok(Self::Revert),
            "reconcile" => Ok(Self::Reconcile),
            _ => Err(StoreError::InvalidEnum {
                field: "operation kind",
                value: value.to_string(),
            }),
        }
    }
}

/// Lifecycle state persisted for an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatusV2 {
    Running,
    Success,
    Failed,
    Partial,
    Aborted,
}

impl OperationStatusV2 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Partial => "partial",
            Self::Aborted => "aborted",
        }
    }
}

impl fmt::Display for OperationStatusV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OperationStatusV2 {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "partial" => Ok(Self::Partial),
            "aborted" => Ok(Self::Aborted),
            _ => Err(StoreError::InvalidEnum {
                field: "operation status",
                value: value.to_string(),
            }),
        }
    }
}

/// Searchable, redacted operation metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationMetaV2 {
    pub command_name: Option<String>,
    pub description: Option<String>,
    pub args_digest: Option<String>,
    pub actor: Option<String>,
    pub causal_context_id: Option<String>,
}

/// Immutable operation payload stored alongside the operation row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationV2 {
    pub op_id: String,
    pub parent_op_ids: Vec<String>,
    pub pre_view_oid: ObjectHash,
    pub post_view_oid: ObjectHash,
    pub kind: OperationKind,
    pub status: OperationStatusV2,
    pub metadata: OperationMetaV2,
    pub restores_op_id: Option<String>,
    pub reverts_op_id: Option<String>,
    pub predecessor_map_oid: Option<ObjectHash>,
}

/// Recovery journal phase.  Phases are monotonic for a given journal id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Reserved,
    PreView,
    Mutation,
    PostView,
    Publish,
}

impl JournalPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::PreView => "pre_view",
            Self::Mutation => "mutation",
            Self::PostView => "post_view",
            Self::Publish => "publish",
        }
    }
}

impl fmt::Display for JournalPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JournalPhase {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "pre_view" => Ok(Self::PreView),
            "mutation" => Ok(Self::Mutation),
            "post_view" => Ok(Self::PostView),
            "publish" => Ok(Self::Publish),
            _ => Err(StoreError::InvalidEnum {
                field: "journal phase",
                value: value.to_string(),
            }),
        }
    }
}

/// One durable recovery-journal record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub journal_id: String,
    pub op_id: String,
    pub phase: JournalPhase,
    pub pre_view_oid: Option<ObjectHash>,
    pub target_view_oid: Option<ObjectHash>,
    pub owner: String,
    pub updated_at: i64,
    pub recovery_payload: Option<String>,
}

/// The currently published heads and their generation numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpHeadsView {
    pub heads: BTreeMap<String, u64>,
    /// Direct parent edges, keyed by child operation id.  Keeping these in
    /// the view lets working-copy freshness distinguish a sibling from a
    /// descendant without making a database query on every check.
    pub ancestors: BTreeMap<String, BTreeSet<String>>,
}

impl OpHeadsView {
    pub fn new(heads: Vec<String>) -> Result<Self, StoreError> {
        Self::with_generations(heads.into_iter().map(|head| (head, 0)).collect())
    }

    pub fn with_generations(heads: Vec<(String, u64)>) -> Result<Self, StoreError> {
        let mut view = Self::default();
        for (head, generation) in heads {
            if head.is_empty() {
                return Err(StoreError::Validation(
                    "operation head id cannot be empty".to_string(),
                ));
            }
            if view.heads.insert(head.clone(), generation).is_some() {
                return Err(StoreError::Validation(format!(
                    "duplicate operation head id '{head}'"
                )));
            }
        }
        Ok(view)
    }

    pub fn head_ids(&self) -> Vec<String> {
        self.heads.keys().cloned().collect()
    }

    pub fn generation(&self, op_id: &str) -> Option<u64> {
        self.heads.get(op_id).copied()
    }

    pub fn add_ancestor(
        &mut self,
        child_op_id: impl Into<String>,
        parent_op_id: impl Into<String>,
    ) -> Result<(), StoreError> {
        let child_op_id = child_op_id.into();
        let parent_op_id = parent_op_id.into();
        if child_op_id.is_empty() || parent_op_id.is_empty() {
            return Err(StoreError::Validation(
                "operation parent ids cannot be empty".to_string(),
            ));
        }
        self.ancestors
            .entry(child_op_id)
            .or_default()
            .insert(parent_op_id);
        Ok(())
    }

    /// Returns true when `ancestor` is the same operation or is reachable by
    /// following parent edges from `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        if ancestor == descendant {
            return true;
        }
        let mut pending = vec![descendant.to_string()];
        let mut visited = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            if let Some(parents) = self.ancestors.get(&current) {
                if parents.contains(ancestor) {
                    return true;
                }
                pending.extend(parents.iter().cloned());
            }
        }
        false
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] DbErr),
    #[error("object storage error: {0}")]
    Object(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid object hash '{0}'")]
    InvalidObjectHash(String),
    #[error("invalid {field}: {value}")]
    InvalidEnum { field: &'static str, value: String },
    #[error("compare-and-swap conflict; current heads: {current_heads:?}")]
    CasConflict { current_heads: Vec<String> },
    #[error("validation error: {0}")]
    Validation(String),
    #[error("view error: {0}")]
    View(#[from] ViewError),
    #[error("operation or journal entry not found: {0}")]
    NotFound(String),
}

/// SQLite plus content-addressed object storage for operation-log v2.
#[derive(Clone)]
pub struct OperationStoreV2 {
    db: DatabaseConnection,
    storage: ClientStorage,
    repo_id: String,
}

impl OperationStoreV2 {
    /// Construct a store without a repository id.  Use [`Self::for_repo`] or
    /// [`Self::new_for_repo`] for writes; the empty-id constructor is useful
    /// for view-only callers and keeps database/storage wiring lightweight.
    pub fn new(db: DatabaseConnection, storage: ClientStorage) -> Self {
        Self {
            db,
            storage,
            repo_id: String::new(),
        }
    }

    pub fn new_for_repo(
        repo_id: impl Into<String>,
        db: DatabaseConnection,
        storage: ClientStorage,
    ) -> Self {
        Self {
            db,
            storage,
            repo_id: repo_id.into(),
        }
    }

    pub fn for_repo(mut self, repo_id: impl Into<String>) -> Self {
        self.repo_id = repo_id.into();
        self
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub async fn write_view_manifest(&self, view: &RepoViewV2) -> Result<ObjectHash, StoreError> {
        let bytes = view.to_canonical_bytes()?;
        let oid = ObjectHash::from_type_and_data(ObjectType::Blob, &bytes);
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .put(&oid, &bytes, ObjectType::Blob)
                .map_err(|error| StoreError::Object(error.to_string()))
        })
        .await
        .map_err(|error| StoreError::Object(error.to_string()))??;
        Ok(oid)
    }

    pub async fn load_view(&self, oid: &ObjectHash) -> Result<RepoViewV2, StoreError> {
        let storage = self.storage.clone();
        let expected_oid = *oid;
        let bytes = tokio::task::spawn_blocking(move || {
            storage
                .get(&expected_oid)
                .map_err(|error| StoreError::Object(error.to_string()))
        })
        .await
        .map_err(|error| StoreError::Object(error.to_string()))??;
        let actual_oid = ObjectHash::from_type_and_data(ObjectType::Blob, &bytes);
        if actual_oid != *oid {
            return Err(StoreError::InvalidObjectHash(format!(
                "manifest {oid} contains object {actual_oid}"
            )));
        }
        RepoViewV2::from_canonical_bytes(&bytes).map_err(StoreError::View)
    }

    pub async fn write_operation(&self, operation: &OperationV2) -> Result<(), StoreError> {
        if self.repo_id.is_empty() {
            return Err(StoreError::Validation(
                "operation store repository id cannot be empty".to_string(),
            ));
        }
        if operation.op_id.is_empty() {
            return Err(StoreError::Validation(
                "operation id cannot be empty".to_string(),
            ));
        }
        validate_parent_ids(&operation.parent_op_ids)?;

        let txn = begin_write_transaction(&self.db).await?;
        let start_ts = Utc::now().timestamp_millis();
        let insert_result = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO operation (op_id, repo_id, format_version, kind, status, \
                 command_name, description, args_digest, actor, worktree_id, scope_kind, \
                 pre_view_oid, post_view_oid, restores_op_id, reverts_op_id, \
                 predecessor_map_oid, causal_context_id, start_ts, end_ts) \
                 VALUES (?, ?, 2, ?, ?, ?, ?, ?, ?, NULL, 'repository', ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    operation.op_id.clone().into(),
                    self.repo_id.clone().into(),
                    operation.kind.to_string().into(),
                    operation.status.to_string().into(),
                    operation.metadata.command_name.clone().into(),
                    operation.metadata.description.clone().into(),
                    operation.metadata.args_digest.clone().into(),
                    operation.metadata.actor.clone().into(),
                    operation.pre_view_oid.to_string().into(),
                    operation.post_view_oid.to_string().into(),
                    operation.restores_op_id.clone().into(),
                    operation.reverts_op_id.clone().into(),
                    operation
                        .predecessor_map_oid
                        .map(|oid| oid.to_string())
                        .into(),
                    operation.metadata.causal_context_id.clone().into(),
                    start_ts.into(),
                    match operation.status {
                        OperationStatusV2::Running => None,
                        OperationStatusV2::Success
                        | OperationStatusV2::Failed
                        | OperationStatusV2::Partial
                        | OperationStatusV2::Aborted => Some(start_ts),
                    }
                    .into(),
                ],
            ))
            .await;
        if let Err(error) = insert_result {
            let _ = txn.rollback().await;
            return Err(StoreError::Database(error));
        }

        for (ordinal, parent_op_id) in operation.parent_op_ids.iter().enumerate() {
            if let Err(error) = txn
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO operation_parent (op_id, parent_op_id, ordinal) VALUES (?, ?, ?)",
                    [
                        operation.op_id.clone().into(),
                        parent_op_id.clone().into(),
                        (ordinal as i64).into(),
                    ],
                ))
                .await
            {
                let _ = txn.rollback().await;
                return Err(StoreError::Database(error));
            }
        }
        txn.commit().await?;
        Ok(())
    }

    /// Atomically replaces the head set if its current value equals
    /// `expected_heads`.  A conflict leaves the database untouched.
    pub async fn cas_update_op_heads(
        &self,
        repo_id: &str,
        scope_key: &str,
        expected_heads: &[String],
        new_heads: &[String],
    ) -> Result<(), StoreError> {
        let expected = normalize_heads(expected_heads)?;
        let replacement = normalize_heads(new_heads)?;
        let txn = begin_write_transaction(&self.db).await?;
        let current_rows = match query_head_rows(&txn, repo_id, scope_key).await {
            Ok(rows) => rows,
            Err(error) => {
                let _ = txn.rollback().await;
                return Err(error);
            }
        };
        let current_heads: Vec<String> = current_rows
            .iter()
            .map(|(op_id, _)| op_id.clone())
            .collect();
        if current_heads != expected {
            let _ = txn.rollback().await;
            return Err(StoreError::CasConflict { current_heads });
        }

        let generation = current_rows
            .iter()
            .map(|(_, generation)| *generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        if let Err(error) = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM operation_head WHERE repo_id = ? AND scope_key = ?",
                [repo_id.into(), scope_key.into()],
            ))
            .await
        {
            let _ = txn.rollback().await;
            return Err(StoreError::Database(error));
        }
        for op_id in replacement {
            if let Err(error) = txn
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO operation_head (repo_id, scope_key, op_id, generation) VALUES (?, ?, ?, ?)",
                    [
                        repo_id.into(),
                        scope_key.into(),
                        op_id.into(),
                        generation.into(),
                    ],
                ))
                .await
            {
                let _ = txn.rollback().await;
                return Err(StoreError::Database(error));
            }
        }
        txn.commit().await?;
        Ok(())
    }

    pub async fn read_heads(
        &self,
        repo_id: &str,
        scope_key: &str,
    ) -> Result<Vec<String>, StoreError> {
        Ok(query_head_rows(&self.db, repo_id, scope_key)
            .await?
            .into_iter()
            .map(|(op_id, _)| op_id)
            .collect())
    }

    pub async fn read_heads_view(
        &self,
        repo_id: &str,
        scope_key: &str,
    ) -> Result<OpHeadsView, StoreError> {
        let rows = query_head_rows(&self.db, repo_id, scope_key).await?;
        let mut view = OpHeadsView::with_generations(rows)?;
        let parent_rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "WITH RECURSIVE reachable(op_id) AS ( \
                 SELECT op_id FROM operation_head WHERE repo_id = ? AND scope_key = ? \
                 UNION \
                 SELECT parent_op_id FROM operation_parent \
                 JOIN reachable ON operation_parent.op_id = reachable.op_id \
                 ) SELECT operation_parent.op_id, operation_parent.parent_op_id \
                 FROM operation_parent JOIN reachable \
                 ON operation_parent.op_id = reachable.op_id \
                 ORDER BY operation_parent.op_id, operation_parent.ordinal",
                [repo_id.into(), scope_key.into()],
            ))
            .await?;
        for row in parent_rows {
            view.add_ancestor(
                row.try_get_by_index::<String>(0)?,
                row.try_get_by_index::<String>(1)?,
            )?;
        }
        Ok(view)
    }

    pub async fn append_journal(&self, entry: &JournalEntry) -> Result<(), StoreError> {
        if entry.journal_id.is_empty() || entry.op_id.is_empty() || entry.owner.is_empty() {
            return Err(StoreError::Validation(
                "journal id, operation id, and owner cannot be empty".to_string(),
            ));
        }
        self.db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO operation_journal (journal_id, op_id, phase, pre_view_oid, \
                 target_view_oid, owner, updated_at, recovery_payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(journal_id) DO UPDATE SET op_id = excluded.op_id, \
                 phase = excluded.phase, pre_view_oid = excluded.pre_view_oid, \
                 target_view_oid = excluded.target_view_oid, owner = excluded.owner, \
                 updated_at = excluded.updated_at, recovery_payload = excluded.recovery_payload",
                [
                    entry.journal_id.clone().into(),
                    entry.op_id.clone().into(),
                    entry.phase.to_string().into(),
                    entry.pre_view_oid.map(|oid| oid.to_string()).into(),
                    entry.target_view_oid.map(|oid| oid.to_string()).into(),
                    entry.owner.clone().into(),
                    entry.updated_at.into(),
                    entry.recovery_payload.clone().into(),
                ],
            ))
            .await?;
        Ok(())
    }

    pub async fn read_journal(&self, op_id: &str) -> Result<Vec<JournalEntry>, StoreError> {
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT journal_id, op_id, phase, pre_view_oid, target_view_oid, owner, \
                 updated_at, recovery_payload FROM operation_journal WHERE op_id = ? \
                 ORDER BY updated_at ASC, journal_id ASC",
                [op_id.into()],
            ))
            .await?;
        rows.into_iter().map(journal_from_row).collect()
    }
}

async fn query_head_rows<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    scope_key: &str,
) -> Result<Vec<(String, u64)>, StoreError> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT op_id, generation FROM operation_head WHERE repo_id = ? AND scope_key = ? \
             ORDER BY op_id",
            [repo_id.into(), scope_key.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let generation = row.try_get_by_index::<i64>(1)?;
            if generation < 0 {
                return Err(StoreError::Validation(format!(
                    "operation head generation cannot be negative: {generation}"
                )));
            }
            Ok((row.try_get_by_index::<String>(0)?, generation as u64))
        })
        .collect()
}

fn journal_from_row(row: QueryResult) -> Result<JournalEntry, StoreError> {
    Ok(JournalEntry {
        journal_id: row.try_get_by_index(0)?,
        op_id: row.try_get_by_index(1)?,
        phase: row.try_get_by_index::<String>(2)?.parse()?,
        pre_view_oid: parse_optional_hash(row.try_get_by_index(3)?)?,
        target_view_oid: parse_optional_hash(row.try_get_by_index(4)?)?,
        owner: row.try_get_by_index(5)?,
        updated_at: row.try_get_by_index(6)?,
        recovery_payload: row.try_get_by_index(7)?,
    })
}

fn parse_optional_hash(value: Option<String>) -> Result<Option<ObjectHash>, StoreError> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| StoreError::InvalidObjectHash(value))
        })
        .transpose()
}

fn validate_parent_ids(parent_ids: &[String]) -> Result<(), StoreError> {
    let mut seen = BTreeSet::new();
    for parent_id in parent_ids {
        if parent_id.is_empty() {
            return Err(StoreError::Validation(
                "operation parent id cannot be empty".to_string(),
            ));
        }
        if !seen.insert(parent_id) {
            return Err(StoreError::Validation(format!(
                "duplicate operation parent id '{parent_id}'"
            )));
        }
    }
    Ok(())
}

fn normalize_heads(heads: &[String]) -> Result<Vec<String>, StoreError> {
    let mut normalized = BTreeSet::new();
    for head in heads {
        if head.is_empty() {
            return Err(StoreError::Validation(
                "operation head id cannot be empty".to_string(),
            ));
        }
        if !normalized.insert(head.clone()) {
            return Err(StoreError::Validation(format!(
                "duplicate operation head id '{head}'"
            )));
        }
    }
    Ok(normalized.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_ancestry_is_transitive() {
        let mut heads = OpHeadsView::new(vec!["c".to_string()]).expect("valid head");
        heads.add_ancestor("c", "b").expect("valid edge");
        heads.add_ancestor("b", "a").expect("valid edge");
        assert!(heads.is_ancestor("a", "c"));
        assert!(!heads.is_ancestor("c", "a"));
    }

    #[test]
    fn duplicate_heads_are_rejected() {
        let result = OpHeadsView::new(vec!["same".to_string(), "same".to_string()]);
        assert!(matches!(result, Err(StoreError::Validation(_))));
    }
}
