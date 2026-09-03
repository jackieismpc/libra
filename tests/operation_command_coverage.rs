//! OL-09 representative CLI mutation coverage through the common boundary.

use git_internal::hash::ObjectHash;
use libra::{
    internal::operation::{
        middleware::OperationMiddleware,
        store::{OperationStatusV2, OperationStore},
    },
    utils::client_storage::ClientStorage,
};
use sea_orm::{ConnectionTrait, Database};
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectHash {
    ObjectHash::from_bytes(&[byte; 20]).expect("oid")
}

#[tokio::test]
async fn representative_command_records_pre_and_post_views() {
    let db = Database::connect("sqlite::memory:").await.expect("db");
    db.execute_unprepared("CREATE TABLE operation(op_id TEXT PRIMARY KEY,repo_id TEXT NOT NULL,format_version INTEGER NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,command_name TEXT,description TEXT,args_digest TEXT,actor TEXT,worktree_id TEXT,scope_kind TEXT NOT NULL,pre_view_oid TEXT NOT NULL,post_view_oid TEXT NOT NULL,restores_op_id TEXT,reverts_op_id TEXT,predecessor_map_oid TEXT,causal_context_id TEXT,start_ts INTEGER NOT NULL,end_ts INTEGER); CREATE TABLE operation_parent(op_id TEXT NOT NULL,parent_op_id TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(op_id,parent_op_id)); CREATE TABLE operation_head(repo_id TEXT NOT NULL,scope_key TEXT NOT NULL,op_id TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(repo_id,scope_key,op_id)); CREATE TABLE operation_journal(journal_id TEXT PRIMARY KEY,op_id TEXT NOT NULL,phase TEXT NOT NULL,pre_view_oid TEXT,target_view_oid TEXT,owner TEXT NOT NULL,updated_at INTEGER NOT NULL,recovery_payload TEXT);").await.expect("schema");
    let objects = tempdir().expect("objects");
    let store = OperationStore::new(db, ClientStorage::init_local(objects.path().to_path_buf()));
    let middleware =
        OperationMiddleware::new(store.clone(), "repo", "main", None::<&std::path::Path>);
    middleware
        .run_with_operation("commit", "commit-1", oid(1), oid(2), None, || async {
            Ok::<_, String>(())
        })
        .await
        .expect("run");
    let operation = store
        .load_operation("commit-1")
        .await
        .expect("load")
        .expect("operation");
    assert_eq!(operation.status, OperationStatusV2::Success);
    assert_eq!(operation.pre_view_oid, oid(1));
    assert_eq!(operation.post_view_oid, oid(2));
}
