//! OL-04 focused coverage for operation objects, journal rows, and head CAS.

use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
use libra::internal::{
    db,
    operation::{
        JournalEntry, JournalPhase, OperationKind, OperationMetaV2, OperationStatusV2,
        OperationStoreV2, OperationV2, RepoViewV2,
    },
};
use tempfile::TempDir;

fn oid(label: &[u8]) -> ObjectHash {
    ObjectHash::from_type_and_data(ObjectType::Blob, label)
}

fn view() -> RepoViewV2 {
    RepoViewV2 {
        schema_version: 2,
        repo_id: "repo-1".to_string(),
        refs_facet_oid: oid(b"refs"),
        workspaces: Default::default(),
        change_roots: Vec::new(),
        extension_facets: Default::default(),
    }
}

#[tokio::test]
async fn operation_store_round_trips_objects_journal_and_head_cas() {
    let dir = TempDir::new().expect("temporary operation store directory");
    let db_path = dir.path().join("repo.db");
    let object_path = dir.path().join("objects");
    let db = db::create_database(db_path.to_str().expect("UTF-8 database path"))
        .await
        .expect("database initializes");
    let storage = libra::utils::client_storage::ClientStorage::init_local(object_path);
    let store = OperationStoreV2::new_for_repo("repo-1", db, storage);

    let view_oid = store
        .write_view_manifest(&view())
        .await
        .expect("manifest writes");
    assert_eq!(
        store.load_view(&view_oid).await.expect("manifest loads"),
        view()
    );

    let operation = OperationV2 {
        op_id: "op-1".to_string(),
        parent_op_ids: Vec::new(),
        pre_view_oid: view_oid,
        post_view_oid: view_oid,
        kind: OperationKind::Command,
        status: OperationStatusV2::Success,
        metadata: OperationMetaV2 {
            command_name: Some("test".to_string()),
            ..Default::default()
        },
        restores_op_id: None,
        reverts_op_id: None,
        predecessor_map_oid: None,
    };
    store
        .write_operation(&operation)
        .await
        .expect("operation writes");

    store
        .cas_update_op_heads("repo-1", "main", &[], &["op-1".to_string()])
        .await
        .expect("initial head publish");
    assert_eq!(
        store
            .read_heads("repo-1", "main")
            .await
            .expect("heads read"),
        ["op-1"]
    );

    let conflict = store
        .cas_update_op_heads("repo-1", "main", &[], &["op-2".to_string()])
        .await;
    assert!(matches!(
        conflict,
        Err(libra::internal::operation::StoreError::CasConflict { .. })
    ));

    store
        .append_journal(&JournalEntry {
            journal_id: "journal-1".to_string(),
            op_id: "op-1".to_string(),
            phase: JournalPhase::Reserved,
            pre_view_oid: Some(view_oid),
            target_view_oid: None,
            owner: "test".to_string(),
            updated_at: 1,
            recovery_payload: None,
        })
        .await
        .expect("journal writes");
    assert_eq!(
        store
            .read_journal("op-1")
            .await
            .expect("journal reads")
            .len(),
        1
    );

    for (op_id, parent_op_ids) in [
        ("op-2", vec!["op-1".to_string()]),
        ("op-3", vec!["op-2".to_string(), "op-1".to_string()]),
    ] {
        store
            .write_operation(&OperationV2 {
                op_id: op_id.to_string(),
                parent_op_ids,
                pre_view_oid: view_oid,
                post_view_oid: view_oid,
                kind: OperationKind::Reconcile,
                status: OperationStatusV2::Success,
                metadata: OperationMetaV2::default(),
                restores_op_id: None,
                reverts_op_id: None,
                predecessor_map_oid: None,
            })
            .await
            .expect("DAG operation writes");
    }
    store
        .cas_update_op_heads(
            "repo-1",
            "main",
            &["op-1".to_string()],
            &["op-2".to_string()],
        )
        .await
        .expect("DAG head publish");
    store
        .cas_update_op_heads(
            "repo-1",
            "main",
            &["op-2".to_string()],
            &["op-3".to_string()],
        )
        .await
        .expect("multi-parent head publish");
    let heads = store
        .read_heads_view("repo-1", "main")
        .await
        .expect("DAG heads read");
    assert_eq!(heads.head_ids(), ["op-3"]);
    assert!(heads.is_ancestor("op-1", "op-3"));
}
