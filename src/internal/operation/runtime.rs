//! Runtime adapter for the OL-09 v2 operation boundary.
//!
//! The v2 migration deliberately replaces the v1 operation tables. This
//! module is the small seam needed by the existing command transaction APIs
//! while their callers are being moved to the v2 boundary. It never creates
//! or queries a v1-only column.

use std::{
    collections::BTreeMap,
    future::Future,
    str::FromStr,
    sync::{Arc, Mutex},
};

use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};

use super::{
    OperationGraphRecord, OperationLogListItem, OperationPage, OperationQueryPage, OperationRecord,
    OperationServiceError, OperationStatus, OperationViewRecord, OperationViewRefRecord,
    OperationViewWorkspaceRecord,
    middleware::{MiddlewareError, MutationClass, OperationMiddleware},
    store::{
        OpHead, OperationKind, OperationMetaV2, OperationStatusV2, OperationStore, OperationV2,
        unix_now,
    },
    view::RepoViewV2,
};
use crate::{
    internal::{
        branch::Branch,
        head::Head,
        model::{operation_head, operation_parent_v2, operation_v2, reference},
    },
    utils::client_storage::ClientStorage,
};

const REFS_FACET_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeRefsFacet {
    schema_version: u32,
    head_kind: String,
    head_target: String,
    refs: Vec<RuntimeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeRef {
    ref_kind: String,
    ref_name: String,
    ref_remote: Option<String>,
    target_oid: String,
}

#[derive(Debug, Clone)]
pub struct CapturedView {
    pub manifest_oid: ObjectHash,
    pub snapshot: super::super::operation_wrapper::OperationViewSnapshot,
}

/// Failure returned by the dispatch adapters. The original command/tool
/// error is retained so adding the operation envelope does not change the
/// user-visible failure semantics.
#[derive(Debug)]
pub enum RuntimeOperationError<E> {
    Middleware(MiddlewareError),
    Action(E),
}

fn middleware_command_name(class: MutationClass) -> &'static str {
    match class {
        MutationClass::ReadOnly => "status",
        MutationClass::WorkingCopy => "add",
        MutationClass::Repository => "commit",
        MutationClass::Ref => "branch",
        MutationClass::Index => "index",
        MutationClass::External => "external",
        MutationClass::InternalWorker => "internal-worker",
    }
}

/// Run one mutating CLI/Agent dispatch through the v2 operation middleware.
///
/// The middleware creates the durable operation before the handler runs. The
/// final view is captured after the handler so the operation row points at
/// the actual post-state, while the middleware still provides the journal,
/// failure status, and head CAS semantics around the action.
#[allow(clippy::too_many_arguments)]
pub async fn run_cli_operation<F, Fut, R, E>(
    db: &DatabaseConnection,
    repo_id: &str,
    scope_key: &str,
    command_name: &str,
    description: &str,
    actor: &str,
    args_digest: Option<String>,
    class: MutationClass,
    action: F,
) -> Result<R, RuntimeOperationError<E>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R, E>>,
    E: Send + 'static,
{
    let schema_is_v2 = is_v2_schema(db).await.map_err(|error| {
        RuntimeOperationError::Middleware(MiddlewareError::Action(error.to_string()))
    })?;
    if !schema_is_v2 {
        return action().await.map_err(RuntimeOperationError::Action);
    }
    if matches!(
        class,
        MutationClass::ReadOnly | MutationClass::InternalWorker
    ) {
        return action().await.map_err(RuntimeOperationError::Action);
    }

    let pre = capture_view(db, repo_id, true, true)
        .await
        .map_err(|error| {
            RuntimeOperationError::Middleware(MiddlewareError::Action(error.to_string()))
        })?;
    let operation_id = uuid::Uuid::now_v7().to_string();
    let storage = ClientStorage::init(crate::utils::path::objects());
    let middleware = OperationMiddleware::new(
        OperationStore::new(db.clone(), storage),
        repo_id,
        scope_key,
        Option::<&std::path::Path>::None,
    );
    let action_error = Arc::new(Mutex::new(None));
    let action_error_slot = Arc::clone(&action_error);
    let result = middleware
        .run_with_operation(
            middleware_command_name(class),
            operation_id.clone(),
            pre.manifest_oid,
            pre.manifest_oid,
            None,
            || async move {
                action().await.map_err(|error| {
                    *action_error_slot
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(error);
                    "dispatch action failed".to_string()
                })
            },
        )
        .await;

    let value = match result {
        Ok(value) => value,
        Err(MiddlewareError::Action(message)) => {
            if let Some(error) = action_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                return Err(RuntimeOperationError::Action(error));
            }
            return Err(RuntimeOperationError::Middleware(MiddlewareError::Action(
                message,
            )));
        }
        Err(error) => return Err(RuntimeOperationError::Middleware(error)),
    };

    let post = capture_view(db, repo_id, true, true)
        .await
        .map_err(|error| {
            RuntimeOperationError::Middleware(MiddlewareError::Action(error.to_string()))
        })?;
    OperationStore::complete_operation_with_conn(
        db,
        &operation_id,
        post.manifest_oid,
        OperationStatusV2::Success,
        unix_now(),
    )
    .await
    .map_err(|error| RuntimeOperationError::Middleware(MiddlewareError::Store(error)))?;
    let scope_kind = if scope_key.trim().is_empty() {
        "main"
    } else {
        "linked"
    };
    OperationStore::update_operation_metadata_with_conn(
        db,
        &operation_id,
        Some(command_name.to_string()),
        Some(description.to_string()),
        args_digest,
        Some(actor.to_string()),
        (!scope_key.is_empty()).then(|| scope_key.to_string()),
        scope_kind.to_string(),
    )
    .await
    .map_err(|error| RuntimeOperationError::Middleware(MiddlewareError::Store(error)))?;
    Ok(value)
}

/// Detect the post-OL-02 schema without selecting a removed v1 column.
pub async fn is_v2_schema<C: ConnectionTrait>(db: &C) -> Result<bool, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "PRAGMA table_info(operation)".to_string(),
        ))
        .await?;
    Ok(rows.iter().any(|row| {
        row.try_get_by_index::<String>(1)
            .is_ok_and(|column| column == "format_version")
    }))
}

/// Capture the stable repository portion of a v2 view.
///
/// The existing restore contract is HEAD plus local and remote refs. This
/// first runtime cut stores that state as a content-addressed refs facet and
/// publishes a v2 repository-view manifest. Workspace facets remain owned by
/// the snapshotter and are added by the later restore work.
pub async fn capture_view<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    include_refs: bool,
    include_remote_tracking: bool,
) -> Result<CapturedView, DbErr> {
    let head = Head::current_result_with_conn(db)
        .await
        .map_err(|error| DbErr::Custom(format!("failed to resolve HEAD: {error}")))?;
    let (head_kind, head_target) = match head {
        Head::Branch(name) => ("branch".to_string(), name),
        Head::Detached(hash) => ("detached".to_string(), hash.to_string()),
    };

    let mut refs = Vec::new();
    if include_refs {
        for branch in Branch::list_branches_result_with_conn(db, None)
            .await
            .map_err(|error| DbErr::Custom(format!("failed to list local branches: {error}")))?
        {
            refs.push(RuntimeRef {
                ref_kind: "branch".to_string(),
                ref_name: branch.name,
                ref_remote: None,
                target_oid: branch.commit.to_string(),
            });
        }
        if include_remote_tracking {
            for remote_ref in reference::Entity::find()
                .filter(reference::Column::Kind.eq(reference::ConfigKind::Branch))
                .filter(reference::Column::Remote.is_not_null())
                .all(db)
                .await?
            {
                let (Some(name), Some(commit)) = (remote_ref.name, remote_ref.commit) else {
                    continue;
                };
                refs.push(RuntimeRef {
                    ref_kind: "remote_branch".to_string(),
                    ref_name: name,
                    ref_remote: remote_ref.remote,
                    target_oid: commit,
                });
            }
        }
    }
    refs.sort_by(|left, right| {
        left.ref_kind
            .cmp(&right.ref_kind)
            .then_with(|| left.ref_name.cmp(&right.ref_name))
            .then_with(|| left.ref_remote.cmp(&right.ref_remote))
            .then_with(|| left.target_oid.cmp(&right.target_oid))
    });

    let facet = RuntimeRefsFacet {
        schema_version: REFS_FACET_SCHEMA_VERSION,
        head_kind: head_kind.clone(),
        head_target: head_target.clone(),
        refs: refs.clone(),
    };
    let facet_bytes = serde_json::to_vec(&facet)
        .map_err(|error| DbErr::Custom(format!("failed to encode refs facet: {error}")))?;
    let storage = ClientStorage::init_local(crate::utils::path::objects());
    let facet_oid = ObjectHash::from_type_and_data(ObjectType::Blob, &facet_bytes);
    storage
        .put_without_index(&facet_oid, &facet_bytes, ObjectType::Blob)
        .map_err(|error| DbErr::Custom(format!("failed to store refs facet: {error}")))?;

    let view = RepoViewV2::new(
        repo_id.to_string(),
        facet_oid,
        BTreeMap::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .map_err(|error| DbErr::Custom(format!("failed to construct repository view: {error}")))?;
    let manifest_bytes = view
        .canonical_bytes()
        .map_err(|error| DbErr::Custom(format!("failed to encode repository view: {error}")))?;
    let manifest_oid = ObjectHash::from_type_and_data(ObjectType::Blob, &manifest_bytes);
    storage
        .put_without_index(&manifest_oid, &manifest_bytes, ObjectType::Blob)
        .map_err(|error| DbErr::Custom(format!("failed to store repository view: {error}")))?;

    let view_id = manifest_oid.to_string();
    let refs = refs
        .into_iter()
        .map(|record| OperationViewRefRecord {
            view_id: view_id.clone(),
            ref_kind: record.ref_kind,
            ref_name: record.ref_name,
            ref_remote: record.ref_remote,
            target_oid: record.target_oid,
        })
        .collect();
    let workspace = vec![OperationViewWorkspaceRecord {
        view_id: view_id.clone(),
        pointer_kind: "head".to_string(),
        pointer_value: head_target.clone(),
    }];
    Ok(CapturedView {
        manifest_oid,
        snapshot: super::super::operation_wrapper::OperationViewSnapshot {
            head_kind,
            head_target,
            refs,
            workspace,
        },
    })
}

fn storage_error(error: impl std::fmt::Display) -> OperationServiceError {
    OperationServiceError::Storage(error.to_string())
}

fn status_from_v2(status: OperationStatusV2) -> OperationStatus {
    match status {
        OperationStatusV2::Running => OperationStatus::Running,
        OperationStatusV2::Success => OperationStatus::Succeeded,
        OperationStatusV2::Failed | OperationStatusV2::Partial | OperationStatusV2::Aborted => {
            OperationStatus::Failed
        }
    }
}

fn operation_record_from_v2(operation: &OperationV2) -> OperationRecord {
    OperationRecord {
        op_id: operation.op_id.clone(),
        repo_id: operation.repo_id.clone(),
        view_id: operation.post_view_oid.to_string(),
        command_name: operation
            .metadata
            .command_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        description: operation.metadata.description.clone().unwrap_or_default(),
        actor: operation.metadata.actor.clone().unwrap_or_default(),
        args_digest: operation.metadata.args_digest.clone(),
        start_ts: operation.start_ts,
        end_ts: operation.end_ts,
        status: status_from_v2(operation.status),
        worktree_id: operation.metadata.worktree_id.clone().unwrap_or_default(),
        scope_provenance: "declared".to_string(),
        restorable: matches!(
            operation.kind,
            OperationKind::Command | OperationKind::Restore
        ),
        control_slot: None,
        claim_owner: None,
        scope_kind: operation.metadata.scope_kind.clone(),
    }
}

fn log_item_from_v2(operation: &OperationV2) -> OperationLogListItem {
    OperationLogListItem {
        op_id: operation.op_id.clone(),
        command_name: operation
            .metadata
            .command_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        description: operation.metadata.description.clone().unwrap_or_default(),
        actor: operation.metadata.actor.clone().unwrap_or_default(),
        end_ts: operation.end_ts,
        status: status_from_v2(operation.status),
    }
}

pub async fn list_operations_by_repo_paginated<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    command_name: Option<&str>,
    query: OperationQueryPage,
) -> Result<OperationPage<OperationLogListItem>, OperationServiceError> {
    if repo_id.trim().is_empty() {
        return Err(OperationServiceError::InvalidArgument(
            "repo_id must not be empty".to_string(),
        ));
    }
    let query = query.normalized();
    let mut count_query =
        operation_v2::Entity::find().filter(operation_v2::Column::RepoId.eq(repo_id));
    let mut list_query =
        operation_v2::Entity::find().filter(operation_v2::Column::RepoId.eq(repo_id));
    if let Some(command_name) = command_name.map(str::trim).filter(|name| !name.is_empty()) {
        count_query = count_query.filter(operation_v2::Column::CommandName.eq(command_name));
        list_query = list_query.filter(operation_v2::Column::CommandName.eq(command_name));
    }
    let total = count_query.count(db).await.map_err(storage_error)?;
    let models = list_query
        .order_by_desc(operation_v2::Column::EndTs)
        .order_by_desc(operation_v2::Column::StartTs)
        .order_by_desc(operation_v2::Column::OpId)
        .offset(query.offset())
        .limit(query.per_page)
        .all(db)
        .await
        .map_err(storage_error)?;
    let mut items = Vec::with_capacity(models.len());
    for model in models {
        let operation = operation_from_model(model, Vec::new()).map_err(storage_error)?;
        items.push(log_item_from_v2(&operation));
    }
    Ok(OperationPage {
        items,
        page: query.page,
        per_page: query.per_page,
        total,
    })
}

/// Load the v2 operation row using the legacy service's record shape.
///
/// The v1 `OperationService` remains part of the public internal surface
/// until OL-15 removes it. Keeping this conversion here means callers that
/// have not yet moved to the v2 model do not issue a v1-only `operation.view_id`
/// query against a v2 repository.
pub async fn find_operation_record<C: ConnectionTrait>(
    db: &C,
    op_id: &str,
) -> Result<Option<OperationRecord>, OperationServiceError> {
    load_v2_operation(db, op_id)
        .await
        .map(|operation| operation.map(|operation| operation_record_from_v2(&operation)))
}

/// List v2 operation rows using the legacy record shape.
pub async fn list_operation_records<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    limit: u64,
) -> Result<Vec<OperationRecord>, OperationServiceError> {
    if limit == 0 {
        return Err(OperationServiceError::InvalidArgument(
            "limit must be greater than 0".to_string(),
        ));
    }
    let models = operation_v2::Entity::find()
        .filter(operation_v2::Column::RepoId.eq(repo_id))
        .order_by_desc(operation_v2::Column::EndTs)
        .order_by_desc(operation_v2::Column::StartTs)
        .order_by_desc(operation_v2::Column::OpId)
        .limit(limit)
        .all(db)
        .await
        .map_err(storage_error)?;
    models
        .into_iter()
        .map(|model| {
            operation_from_model(model, Vec::new())
                .map(|operation| operation_record_from_v2(&operation))
        })
        .collect()
}

pub async fn recent_duplicate_candidates<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    worktree_id: Option<&str>,
    command_name: &str,
    args_digest: &str,
    earliest_end_ts: i64,
    limit: u64,
) -> Result<Vec<OperationRecord>, OperationServiceError> {
    if limit == 0 {
        return Err(OperationServiceError::InvalidArgument(
            "limit must be greater than 0".to_string(),
        ));
    }
    let mut query = operation_v2::Entity::find()
        .filter(operation_v2::Column::RepoId.eq(repo_id))
        .filter(operation_v2::Column::CommandName.eq(command_name))
        .filter(operation_v2::Column::ArgsDigest.eq(args_digest))
        .filter(operation_v2::Column::Status.eq(OperationStatusV2::Success.as_str()))
        .filter(operation_v2::Column::EndTs.gte(earliest_end_ts));
    if let Some(worktree_id) = worktree_id {
        query = query.filter(operation_v2::Column::WorktreeId.eq(worktree_id));
    }
    let models = query
        .order_by_desc(operation_v2::Column::EndTs)
        .order_by_desc(operation_v2::Column::StartTs)
        .order_by_desc(operation_v2::Column::OpId)
        .limit(limit)
        .all(db)
        .await
        .map_err(storage_error)?;
    models
        .into_iter()
        .map(|model| {
            operation_from_model(model, Vec::new())
                .map(|operation| operation_record_from_v2(&operation))
                .map_err(storage_error)
        })
        .collect()
}

pub async fn running_control<C: ConnectionTrait>(
    db: &C,
    repo_id: &str,
    worktree_id: &str,
) -> Result<Option<(String, String, i64, Option<String>)>, OperationServiceError> {
    let row = operation_v2::Entity::find()
        .filter(operation_v2::Column::RepoId.eq(repo_id))
        .filter(operation_v2::Column::WorktreeId.eq(worktree_id))
        .filter(operation_v2::Column::Status.eq(OperationStatusV2::Running.as_str()))
        .order_by_asc(operation_v2::Column::StartTs)
        .one(db)
        .await
        .map_err(storage_error)?;
    Ok(row.map(|row| {
        (
            row.op_id,
            row.command_name.unwrap_or_else(|| "unknown".to_string()),
            row.start_ts,
            row.actor,
        )
    }))
}

pub async fn abandon<C: ConnectionTrait>(
    db: &C,
    op_id: &str,
    end_ts: i64,
) -> Result<bool, OperationServiceError> {
    let changed = operation_v2::Entity::update_many()
        .col_expr(
            operation_v2::Column::Status,
            sea_orm::sea_query::Expr::value(OperationStatusV2::Failed.as_str()),
        )
        .col_expr(
            operation_v2::Column::EndTs,
            sea_orm::sea_query::Expr::value(end_ts),
        )
        .filter(operation_v2::Column::OpId.eq(op_id))
        .filter(operation_v2::Column::Status.eq(OperationStatusV2::Running.as_str()))
        .exec(db)
        .await
        .map_err(storage_error)?;
    Ok(changed.rows_affected > 0)
}

fn operation_from_model(
    model: operation_v2::Model,
    parent_op_ids: Vec<String>,
) -> Result<OperationV2, OperationServiceError> {
    let parse = |value: &str| {
        ObjectHash::from_str(value)
            .map_err(|_| OperationServiceError::Storage(format!("invalid object id '{value}'")))
    };
    let kind = match model.kind.as_str() {
        "command" => OperationKind::Command,
        "external_snapshot" => OperationKind::ExternalSnapshot,
        "undo" => OperationKind::Undo,
        "redo" => OperationKind::Redo,
        "restore" => OperationKind::Restore,
        "revert" => OperationKind::Revert,
        "reconcile" => OperationKind::Reconcile,
        other => {
            return Err(OperationServiceError::Storage(format!(
                "unknown operation kind '{other}'"
            )));
        }
    };
    let status = match model.status.as_str() {
        "running" => OperationStatusV2::Running,
        "success" => OperationStatusV2::Success,
        "failed" => OperationStatusV2::Failed,
        "partial" => OperationStatusV2::Partial,
        "aborted" => OperationStatusV2::Aborted,
        other => {
            return Err(OperationServiceError::Storage(format!(
                "unknown operation status '{other}'"
            )));
        }
    };
    Ok(OperationV2 {
        op_id: model.op_id,
        repo_id: model.repo_id,
        parent_op_ids,
        pre_view_oid: parse(&model.pre_view_oid)?,
        post_view_oid: parse(&model.post_view_oid)?,
        kind,
        status,

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
            .map(parse)
            .transpose()?,
        start_ts: model.start_ts,
        end_ts: model.end_ts,
    })
}

async fn load_v2_operation<C: ConnectionTrait>(
    db: &C,
    op_id: &str,
) -> Result<Option<OperationV2>, OperationServiceError> {
    let Some(model) = operation_v2::Entity::find_by_id(op_id.to_string())
        .one(db)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };
    let parents = operation_parent_v2::Entity::find()
        .filter(operation_parent_v2::Column::OpId.eq(op_id))
        .order_by_asc(operation_parent_v2::Column::Ordinal)
        .all(db)
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|parent| parent.parent_op_id)
        .collect();
    Ok(Some(operation_from_model(model, parents)?))
}

fn decode_view(
    storage: &ClientStorage,
    operation: &OperationV2,
) -> Result<(OperationViewRecord, Vec<OperationViewRefRecord>), OperationServiceError> {
    let bytes = storage
        .get(&operation.post_view_oid)
        .map_err(storage_error)?;
    let view = RepoViewV2::from_canonical_bytes(&bytes).map_err(storage_error)?;
    let facet_bytes = storage.get(&view.refs_facet_oid).map_err(storage_error)?;
    let facet: RuntimeRefsFacet = serde_json::from_slice(&facet_bytes).map_err(storage_error)?;
    if facet.schema_version != REFS_FACET_SCHEMA_VERSION {
        return Err(OperationServiceError::Storage(format!(
            "unsupported refs facet schema version {}",
            facet.schema_version
        )));
    }
    let view_id = operation.post_view_oid.to_string();
    let view_record = OperationViewRecord {
        view_id: view_id.clone(),
        repo_id: operation.repo_id.clone(),
        head_kind: facet.head_kind,
        head_target: facet.head_target,
        created_at: operation.end_ts.unwrap_or(operation.start_ts),
    };
    let refs = facet
        .refs
        .into_iter()
        .map(|record| OperationViewRefRecord {
            view_id: view_id.clone(),
            ref_kind: record.ref_kind,
            ref_name: record.ref_name,
            ref_remote: record.ref_remote,
            target_oid: record.target_oid,
        })
        .collect();
    Ok((view_record, refs))
}

pub async fn load_graph<C: ConnectionTrait>(
    db: &C,
    op_id: &str,
) -> Result<Option<OperationGraphRecord>, OperationServiceError> {
    let Some(operation) = load_v2_operation(db, op_id).await? else {
        return Ok(None);
    };
    let storage = ClientStorage::init(crate::utils::path::objects());
    let (view, refs) = decode_view(&storage, &operation)?;
    let parents = operation
        .parent_op_ids
        .iter()
        .map(|parent_op_id| super::OperationParentRecord {
            op_id: operation.op_id.clone(),
            parent_op_id: parent_op_id.clone(),
        })
        .collect();
    let workspace = vec![OperationViewWorkspaceRecord {
        view_id: view.view_id.clone(),
        pointer_kind: "head".to_string(),
        pointer_value: view.head_target.clone(),
    }];
    Ok(Some(OperationGraphRecord {
        operation: operation_record_from_v2(&operation),
        parents,
        view,
        refs,
        workspace,
    }))
}

/// Persist the v2 equivalent of the old graph-shaped command result.
pub async fn persist_graph<C: ConnectionTrait>(
    db: &C,
    graph: &OperationGraphRecord,
) -> Result<OperationGraphRecord, OperationServiceError> {
    let post_view_oid = ObjectHash::from_str(&graph.view.view_id).map_err(|_| {
        OperationServiceError::Storage(format!("invalid v2 view id '{}'", graph.view.view_id))
    })?;
    let pre_view_oid = if let Some(parent) = graph.parents.first() {
        operation_v2::Entity::find_by_id(parent.parent_op_id.clone())
            .one(db)
            .await
            .map_err(storage_error)?
            .and_then(|row| ObjectHash::from_str(&row.post_view_oid).ok())
            .unwrap_or(post_view_oid)
    } else {
        post_view_oid
    };
    let operation = OperationV2 {
        op_id: graph.operation.op_id.clone(),
        repo_id: graph.operation.repo_id.clone(),
        parent_op_ids: graph
            .parents
            .iter()
            .map(|parent| parent.parent_op_id.clone())
            .collect(),
        pre_view_oid,
        post_view_oid,
        kind: if graph.operation.command_name == "op restore" {
            OperationKind::Restore
        } else {
            OperationKind::Command
        },
        status: match graph.operation.status {
            OperationStatus::Running => OperationStatusV2::Running,
            OperationStatus::Succeeded => OperationStatusV2::Success,
            OperationStatus::Failed => OperationStatusV2::Failed,
            OperationStatus::Canceled => OperationStatusV2::Aborted,
        },
        metadata: OperationMetaV2 {
            command_name: Some(graph.operation.command_name.clone()),
            description: Some(graph.operation.description.clone()),
            args_digest: graph.operation.args_digest.clone(),
            actor: Some(graph.operation.actor.clone()),
            worktree_id: Some(graph.operation.worktree_id.clone()),
            scope_kind: graph.operation.scope_kind.clone(),
            ..OperationMetaV2::default()
        },
        restores_op_id: None,
        reverts_op_id: None,
        predecessor_map_oid: None,
        start_ts: graph.operation.start_ts,
        end_ts: graph.operation.end_ts,
    };
    OperationStore::write_operation_with_conn(db, &operation)
        .await
        .map_err(storage_error)?;
    if operation.status == OperationStatusV2::Success {
        let current = operation_head::Entity::find()
            .filter(operation_head::Column::RepoId.eq(&operation.repo_id))
            .filter(operation_head::Column::ScopeKey.eq(graph.operation.worktree_id.clone()))
            .all(db)
            .await
            .map_err(storage_error)?;
        let generation = current
            .iter()
            .map(|head| head.generation)
            .max()
            .unwrap_or(0)
            + 1;
        OperationStore::replace_op_heads_with_conn(
            db,
            &operation.repo_id,
            &graph.operation.worktree_id,
            &[OpHead {
                op_id: operation.op_id.clone(),
                generation,
            }],
        )
        .await
        .map_err(storage_error)?;
    }
    Ok(graph.clone())
}
