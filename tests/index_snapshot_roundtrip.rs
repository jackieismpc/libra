//! OL-07 focused coverage for byte-exact raw index facet restoration.

use std::path::Path;

use git_internal::internal::{
    index::{Index, IndexEntry},
    object::{blob::Blob, types::ObjectType},
};
use libra::internal::{
    operation::{FacetCaptureCtx, FacetRegistry, FacetRestoreCtx, RawIndexFacet},
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

#[test]
fn raw_index_facet_restores_exact_index_bytes_and_flags() {
    let dir = TempDir::new().expect("temporary repository");
    let root = dir.path().join("worktree");
    let gitdir = root.join(".libra");
    std::fs::create_dir_all(&gitdir).expect("create gitdir");
    let objects = dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("create object store");
    let storage = libra::utils::client_storage::ClientStorage::init_local(objects);

    let content = b"index payload\n";
    let blob = Blob::from_content_bytes(content.to_vec());
    storage
        .put(&blob.id, content, ObjectType::Blob)
        .expect("store blob");
    let mut entry =
        IndexEntry::new_from_blob("intent.txt".to_string(), blob.id, content.len() as u32);
    entry.flags.assume_valid = true;
    entry.flags.stage = 2;
    let mut index = Index::new();
    index.add(entry);
    let index_path = gitdir.join("index");
    index.save(&index_path).expect("write index");
    let original = std::fs::read(&index_path).expect("read original index");

    let mut registry = FacetRegistry::new();
    registry
        .register(Box::new(RawIndexFacet::new(&scope(&root), storage)))
        .expect("register raw index facet");
    let name = libra::internal::operation::FacetName::from("index");
    let capture = registry
        .capture(&name, &FacetCaptureCtx::default())
        .expect("capture raw index");
    assert!(capture.payload_oid.is_some());
    assert!(registry.is_fully_restorable(std::slice::from_ref(&capture)));

    std::fs::write(&index_path, b"not the original index").expect("mutate index bytes");
    let facet = registry.get(&name).expect("registered facet");
    facet
        .restore(&capture, &mut FacetRestoreCtx::default())
        .expect("restore raw index");
    assert_eq!(
        std::fs::read(index_path).expect("read restored index"),
        original
    );
}
