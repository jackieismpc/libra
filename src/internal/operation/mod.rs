//! Version 2 operation-log primitives.

pub mod facet;
pub mod view;

// OL-15 removes this compatibility service. Re-exporting it keeps existing
// command integrations source-compatible while all new code uses v2 types.
pub use facet::{
    FacetCapture, FacetCaptureCtx, FacetError, FacetName, FacetRegistry, RestorePolicy,
};
pub use view::{
    CapturePolicy, Completeness, HeadState, RepoViewV2, WorkspaceId, WorkspaceSnapshotV2,
};

pub use crate::internal::legacy_operation::*;
