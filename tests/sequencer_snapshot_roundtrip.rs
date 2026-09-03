//! OL-07 sequencer facet roundtrip.

use std::fs;

use libra::{
    internal::operation::{
        facet::{FacetCaptureCtx, StateFacet},
        facets::SequencerFacet,
    },
    utils::client_storage::ClientStorage,
};
use tempfile::tempdir;

#[test]
fn sequencer_state_is_restored() {
    let root = tempdir().expect("root");
    let objects = tempdir().expect("objects");
    let state = root.path().join("sequencer/todo");
    fs::create_dir_all(state.parent().expect("parent")).expect("mkdir");
    fs::write(&state, b"pick abc\nreword def\n").expect("write");
    let facet = SequencerFacet::sequencer(
        state.clone(),
        ClientStorage::init_local(objects.path().to_path_buf()),
    );
    let capture = facet.capture(&FacetCaptureCtx::default()).expect("capture");
    fs::write(&state, b"changed\n").expect("modify");
    facet
        .restore(&capture, &mut Default::default())
        .expect("restore");
    assert_eq!(fs::read(state).expect("read"), b"pick abc\nreword def\n");
}
