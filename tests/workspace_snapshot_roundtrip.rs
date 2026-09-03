//! OL-06 working-copy snapshot capture and restore integration coverage.

use std::fs;

use libra::{
    internal::operation::{
        snapshot::WorkspaceSnapshotter,
        view::{CapturePolicy, HeadState},
    },
    utils::client_storage::ClientStorage,
};
use tempfile::tempdir;

#[test]
fn tracked_and_untracked_content_roundtrip() {
    let root = tempdir().expect("root");
    fs::create_dir(root.path().join("nested")).expect("mkdir");
    fs::write(root.path().join("tracked.txt"), b"before").expect("write");
    fs::write(root.path().join("nested/untracked.txt"), b"untracked").expect("write");
    let objects = tempdir().expect("objects");
    let snapshotter =
        WorkspaceSnapshotter::new(ClientStorage::init_local(objects.path().to_path_buf()));
    let capture = snapshotter
        .capture(
            root.path(),
            "workspace",
            HeadState::Unborn {
                ref_name: "main".to_string(),
            },
            CapturePolicy::TrackedAndUntracked,
            1,
        )
        .expect("capture")
        .expect("snapshot");

    fs::write(root.path().join("tracked.txt"), b"changed").expect("modify");
    fs::remove_file(root.path().join("nested/untracked.txt")).expect("remove");
    snapshotter
        .restore(root.path(), &capture.manifest)
        .expect("restore");
    assert_eq!(
        fs::read(root.path().join("tracked.txt")).expect("read"),
        b"before"
    );
    assert_eq!(
        fs::read(root.path().join("nested/untracked.txt")).expect("read"),
        b"untracked"
    );
}
