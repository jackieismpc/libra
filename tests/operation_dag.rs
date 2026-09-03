//! OL-04 Operation Log v2 DAG and publish-CAS integration coverage.

use git_internal::hash::ObjectHash;
use libra::internal::operation::store::{
    OpHead, OperationKind, OperationMetaV2, OperationStatusV2, OperationStore, OperationV2,
    StoreError,
};
use sea_orm::{ConnectionTrait, Database};
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectHash {
    ObjectHash::from_bytes(&[byte; 20]).expect("oid")
}

async fn store() -> OperationStore {
    let db = Database::connect("sqlite::memory:").await.expect("db");
    db.execute_unprepared(
        "CREATE TABLE operation(op_id TEXT PRIMARY KEY,repo_id TEXT NOT NULL,format_version INTEGER NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,command_name TEXT,description TEXT,args_digest TEXT,actor TEXT,worktree_id TEXT,scope_kind TEXT NOT NULL,pre_view_oid TEXT NOT NULL,post_view_oid TEXT NOT NULL,restores_op_id TEXT,reverts_op_id TEXT,predecessor_map_oid TEXT,causal_context_id TEXT,start_ts INTEGER NOT NULL,end_ts INTEGER); CREATE TABLE operation_parent(op_id TEXT NOT NULL,parent_op_id TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(op_id,parent_op_id)); CREATE TABLE operation_head(repo_id TEXT NOT NULL,scope_key TEXT NOT NULL,op_id TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(repo_id,scope_key,op_id)); CREATE TABLE operation_journal(journal_id TEXT PRIMARY KEY,op_id TEXT NOT NULL,phase TEXT NOT NULL,pre_view_oid TEXT,target_view_oid TEXT,owner TEXT NOT NULL,updated_at INTEGER NOT NULL,recovery_payload TEXT);",
    )
    .await
    .expect("schema");
    let dir = tempdir().expect("temp");
    OperationStore::new(
        db,
        libra::utils::client_storage::ClientStorage::init_local(dir.path().join("objects")),
    )
}

#[tokio::test]
async fn multi_parent_dag_and_cas_conflict_are_retained() {
    let store = store().await;
    let operation = OperationV2 {
        op_id: "merge".to_string(),
        repo_id: "repo".to_string(),
        parent_op_ids: vec!["left".to_string(), "right".to_string()],
        pre_view_oid: oid(1),
        post_view_oid: oid(2),
        kind: OperationKind::Command,
        status: OperationStatusV2::Success,
        metadata: OperationMetaV2 {
            scope_kind: "main".to_string(),
            ..Default::default()
        },
        restores_op_id: None,
        reverts_op_id: None,
        predecessor_map_oid: None,
        start_ts: 1,
        end_ts: Some(2),
    };
    store
        .write_operation(&operation)
        .await
        .expect("write operation");
    assert_eq!(
        store.load_operation("merge").await.expect("load").unwrap(),
        operation
    );

    let first = [OpHead {
        op_id: "left".to_string(),
        generation: 4,
    }];
    store
        .cas_update_op_heads("repo", "main", &[], &first)
        .await
        .expect("first publish");
    let second = [OpHead {
        op_id: "right".to_string(),
        generation: 4,
    }];
    assert!(matches!(
        store
            .cas_update_op_heads("repo", "main", &[], &second)
            .await,
        Err(StoreError::CasConflict { .. })
    ));
    assert_eq!(
        store
            .current_op_heads("repo", "main")
            .await
            .expect("heads")
            .len(),
        2
    );
}
