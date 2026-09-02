//! OL-07 focused coverage for sequencer and sparse-view facet restoration.

use std::path::Path;

use libra::internal::{
    db,
    operation::{FacetCaptureCtx, FacetRestoreCtx, SequencerFacet, SparseFacet, StateFacet},
    sequencer::{SequenceKind, SequenceState},
    worktree_scope::{RequestScope, WorktreeScope},
};
use tempfile::TempDir;

fn scope(root: &Path) -> RequestScope {
    RequestScope {
        scope: WorktreeScope::Main,
        workdir: root.to_path_buf(),
        gitdir: root.join(".libra"),
        storage: root.to_path_buf(),
        worktree_root: root.to_path_buf(),
    }
}

async fn fixture() -> (
    TempDir,
    RequestScope,
    sea_orm::DatabaseConnection,
    libra::utils::client_storage::ClientStorage,
) {
    let dir = TempDir::new().expect("temporary repository");
    let root = dir.path().join("worktree");
    std::fs::create_dir_all(root.join(".libra")).expect("create gitdir");
    let objects = dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("create object store");
    let db = db::create_database(
        dir.path()
            .join("repo.db")
            .to_str()
            .expect("UTF-8 database path"),
    )
    .await
    .expect("database initializes");
    let storage = libra::utils::client_storage::ClientStorage::init_local(objects);
    (dir, scope(&root), db, storage)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequencer_facet_round_trips_intermediate_state() {
    let (_dir, request_scope, db, storage) = fixture().await;
    let facet = SequencerFacet::new(&request_scope, db, storage);
    let state = SequenceState {
        kind: SequenceKind::Rebase,
        head_name: "refs/heads/topic".to_string(),
        head_orig: "1111111111111111111111111111111111111111".to_string(),
        current_oid: "2222222222222222222222222222222222222222".to_string(),
        todo: vec![
            "3333333333333333333333333333333333333333".to_string(),
            "4444444444444444444444444444444444444444".to_string(),
        ],
        payload: "rebase-options".to_string(),
    };
    facet
        .write_state(Some(state))
        .expect("write sequence state");
    let capture = facet
        .capture(&FacetCaptureCtx::default())
        .expect("capture sequence state");
    assert!(capture.payload_oid.is_some());

    facet.write_state(None).expect("clear sequence state");
    facet
        .restore(&capture, &mut FacetRestoreCtx::default())
        .expect("restore sequence state");
    let restored = facet
        .capture(&FacetCaptureCtx::default())
        .expect("recapture sequence state");
    assert_eq!(restored.payload_oid, capture.payload_oid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sparse_facet_round_trips_toggle_and_ordered_patterns() {
    let (_dir, request_scope, db, storage) = fixture().await;
    let facet = SparseFacet::new(&request_scope, db, storage);
    let patterns = vec!["src/**".to_string(), "!src/generated/**".to_string()];
    facet
        .write_state(true, patterns)
        .expect("write sparse view");
    let capture = facet
        .capture(&FacetCaptureCtx::default())
        .expect("capture sparse view");

    facet
        .write_state(false, Vec::new())
        .expect("clear sparse view");
    facet
        .restore(&capture, &mut FacetRestoreCtx::default())
        .expect("restore sparse view");
    let restored = facet
        .capture(&FacetCaptureCtx::default())
        .expect("recapture sparse view");
    assert_eq!(restored.payload_oid, capture.payload_oid);
}
