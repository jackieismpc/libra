//! OL-07 raw index facet roundtrip.

use std::{fs, path::PathBuf};

use libra::{
    internal::operation::{
        facet::{FacetCaptureCtx, FacetRegistry, StateFacet},
        facets::IndexFacet,
    },
    utils::client_storage::ClientStorage,
};
use tempfile::tempdir;

#[test]
fn index_bytes_are_restored_exactly() {
    let root = tempdir().expect("root");
    let objects = tempdir().expect("objects");
    let index = root.path().join("index");
    let original = b"index\0intent-to-add\0skip-worktree\0";
    fs::write(&index, original).expect("write");
    let facet = IndexFacet::index(
        PathBuf::from(&index),
        ClientStorage::init_local(objects.path().to_path_buf()),
    );
    let capture = facet.capture(&FacetCaptureCtx::default()).expect("capture");
    fs::write(&index, b"changed").expect("modify");
    facet
        .restore(&capture, &mut Default::default())
        .expect("restore");
    assert_eq!(fs::read(&index).expect("read"), original);
    let mut registry = FacetRegistry::default();
    registry.register(Box::new(facet)).expect("register");
    assert!(registry.fully_restorable(&[capture]));
}
