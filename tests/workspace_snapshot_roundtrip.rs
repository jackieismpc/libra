//! OL-06 focused coverage for bounded working-copy snapshot capture.

use std::path::PathBuf;

use git_internal::internal::{
    index::{Index, IndexEntry},
    object::{blob::Blob, types::ObjectType},
};
use libra::internal::{
    db,
    operation::{
        CapturePolicy, FacetRegistry, OperationStoreV2, SnapshotOutcome, WorkspaceSnapshotter,
    },
    worktree_scope::{RequestScope, WorktreeScope},
};
use tempfile::TempDir;

fn scope(root: &std::path::Path) -> RequestScope {
    RequestScope {
        scope: WorktreeScope::Main,
        workdir: root.to_path_buf(),
        gitdir: root.join(".libra"),
        storage: root.to_path_buf(),
        worktree_root: root.to_path_buf(),
    }
}

async fn fixture() -> (TempDir, RequestScope, OperationStoreV2) {
    let dir = TempDir::new().expect("temporary repository");
    let root = dir.path().join("worktree");
    let gitdir = root.join(".libra");
    std::fs::create_dir_all(&gitdir).expect("create gitdir");
    let object_path = dir.path().join("objects");
    std::fs::create_dir_all(&object_path).expect("create object store");
    let db = db::create_database(
        dir.path()
            .join("repo.db")
            .to_str()
            .expect("UTF-8 database path"),
    )
    .await
    .expect("database initializes");
    let storage = libra::utils::client_storage::ClientStorage::init_local(object_path);
    let content = b"tracked baseline\n";
    std::fs::create_dir_all(&root).expect("create worktree");
    std::fs::write(root.join("tracked.txt"), content).expect("write tracked file");
    let blob = Blob::from_content_bytes(content.to_vec());
    storage
        .put(&blob.id, content, ObjectType::Blob)
        .expect("store baseline blob");
    let mut index = Index::new();
    index.add(
        IndexEntry::new_from_file(PathBuf::from("tracked.txt").as_path(), blob.id, &root)
            .expect("create index entry"),
    );
    index.save(gitdir.join("index")).expect("write index");
    let request_scope = scope(&root);
    let store = OperationStoreV2::new_for_repo("repo-test", db, storage);
    (dir, request_scope, store)
}

#[tokio::test]
async fn snapshot_roundtrip_restores_tracked_and_untracked_content() {
    let (_dir, request_scope, store) = fixture().await;
    let mut snapshotter = WorkspaceSnapshotter::new(
        request_scope.clone(),
        store,
        FacetRegistry::new(),
        CapturePolicy::TrackedAndUntracked,
    );
    assert_eq!(
        snapshotter.capture().await.expect("clean baseline capture"),
        SnapshotOutcome::NoChange
    );

    std::fs::write(
        request_scope.worktree_root.join("tracked.txt"),
        b"tracked changed\n",
    )
    .expect("modify tracked file");
    std::fs::write(
        request_scope.worktree_root.join("new.txt"),
        b"untracked content\n",
    )
    .expect("write untracked file");
    let outcome = snapshotter.capture().await.expect("capture changes");
    let SnapshotOutcome::Captured { snapshot, .. } = outcome else {
        panic!("changed files must publish a snapshot")
    };
    assert_eq!(snapshot.capture_policy, CapturePolicy::TrackedAndUntracked);
    assert_eq!(
        snapshot.completeness,
        libra::internal::operation::Completeness::Full
    );
    assert_eq!(
        snapshotter
            .capture()
            .await
            .expect("repeat unchanged capture"),
        SnapshotOutcome::NoChange
    );

    std::fs::write(
        request_scope.worktree_root.join("tracked.txt"),
        b"overwritten\n",
    )
    .expect("overwrite tracked file");
    std::fs::write(
        request_scope.worktree_root.join("new.txt"),
        b"overwritten untracked\n",
    )
    .expect("overwrite untracked file");
    snapshotter
        .restore_working_copy(&snapshot)
        .expect("restore snapshot tree");
    assert_eq!(
        std::fs::read(request_scope.worktree_root.join("tracked.txt")).expect("read tracked"),
        b"tracked changed\n"
    );
    assert_eq!(
        std::fs::read(request_scope.worktree_root.join("new.txt")).expect("read untracked"),
        b"untracked content\n"
    );
}

#[tokio::test]
async fn snapshot_marks_size_limited_file_partial() {
    let (_dir, request_scope, store) = fixture().await;
    std::fs::write(
        request_scope.worktree_root.join("tracked.txt"),
        b"this no longer fits",
    )
    .expect("modify tracked file");
    let mut snapshotter = WorkspaceSnapshotter::new(
        request_scope,
        store,
        FacetRegistry::new(),
        CapturePolicy::Tracked,
    )
    .with_limits(100, 4);
    let outcome = snapshotter
        .capture()
        .await
        .expect("partial capture publishes");
    let SnapshotOutcome::Captured { snapshot, .. } = outcome else {
        panic!("size-limited change must publish a partial snapshot")
    };
    assert_eq!(
        snapshot.completeness,
        libra::internal::operation::Completeness::Partial
    );
}
