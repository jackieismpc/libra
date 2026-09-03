//! Canonical, versioned manifests for repository and working-copy state.

use std::collections::BTreeMap;

use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::utils::client_storage::ClientStorage;

pub const REPO_VIEW_SCHEMA_VERSION: u32 = 2;
pub const WORKSPACE_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

pub type WorkspaceId = String;
pub type FacetName = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadState {
    Symbolic { ref_name: String },
    Detached { oid: ObjectHash },
    Unborn { ref_name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePolicy {
    Tracked,
    TrackedAndUntracked,
    FailClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Full,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct RepoViewV2 {
    pub schema_version: u32,
    pub repo_id: String,
    pub refs_facet_oid: ObjectHash,
    pub workspaces: BTreeMap<WorkspaceId, ObjectHash>,
    pub change_roots: Vec<ObjectHash>,
    pub extension_facets: BTreeMap<FacetName, ObjectHash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct WorkspaceSnapshotV2 {
    pub schema_version: u32,
    pub workspace_id: WorkspaceId,
    pub head: HeadState,
    pub index_tree_oid: ObjectHash,
    pub raw_index_blob_oid: ObjectHash,
    pub working_copy_tree_oid: ObjectHash,
    pub untracked_manifest_oid: ObjectHash,
    pub sparse_facet_oid: Option<ObjectHash>,
    pub sequencer_facet_oid: Option<ObjectHash>,
    pub worktree_generation: u64,
    pub capture_policy: CapturePolicy,
    pub completeness: Completeness,
    pub facet_restore_policies:
        BTreeMap<FacetName, crate::internal::operation::facet::RestorePolicy>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ViewCodecError {
    #[error("invalid canonical manifest JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported {kind} schema version {version}; expected {expected}")]
    UnsupportedSchemaVersion {
        kind: &'static str,
        version: u32,
        expected: u32,
    },
    #[error("manifest object is not a {expected} object")]
    WrongObjectType { expected: &'static str },
    #[error("manifest references missing object {0}")]
    MissingObject(ObjectHash),
    #[error("empty repo_id")]
    EmptyRepoId,
    #[error("empty workspace_id")]
    EmptyWorkspaceId,
    #[error("failed to persist manifest: {0}")]
    Storage(String),
}

impl RepoViewV2 {
    pub fn new(
        repo_id: String,
        refs_facet_oid: ObjectHash,
        workspaces: BTreeMap<WorkspaceId, ObjectHash>,
        change_roots: Vec<ObjectHash>,
        extension_facets: BTreeMap<FacetName, ObjectHash>,
    ) -> Result<Self, ViewCodecError> {
        let view = Self {
            schema_version: REPO_VIEW_SCHEMA_VERSION,
            repo_id,
            refs_facet_oid,
            workspaces,
            change_roots,
            extension_facets,
        };
        view.validate_schema()?;
        Ok(view)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ViewCodecError> {
        self.validate_schema()?;
        serde_json::to_vec(self).map_err(|error| ViewCodecError::InvalidJson(error.to_string()))
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ViewCodecError> {
        let view: Self = serde_json::from_slice(bytes)
            .map_err(|error| ViewCodecError::InvalidJson(error.to_string()))?;
        view.validate_schema()?;
        Ok(view)
    }

    pub fn write_manifest(&self, storage: &ClientStorage) -> Result<ObjectHash, ViewCodecError> {
        let bytes = self.canonical_bytes()?;
        let oid = ObjectHash::from_type_and_data(ObjectType::Blob, &bytes);
        storage
            .put(&oid, &bytes, ObjectType::Blob)
            .map_err(|error| ViewCodecError::Storage(error.to_string()))?;
        Ok(oid)
    }

    pub fn roots(&self) -> Vec<ObjectHash> {
        let mut roots = vec![self.refs_facet_oid];
        roots.extend(self.workspaces.values().copied());
        roots.extend(self.change_roots.iter().copied());
        roots.extend(self.extension_facets.values().copied());
        roots
    }

    pub fn validate_closure<F>(&self, mut contains: F) -> Result<(), ViewCodecError>
    where
        F: FnMut(&ObjectHash) -> bool,
    {
        self.roots()
            .into_iter()
            .find(|oid| !contains(oid))
            .map_or(Ok(()), |oid| Err(ViewCodecError::MissingObject(oid)))
    }

    fn validate_schema(&self) -> Result<(), ViewCodecError> {
        if self.schema_version != REPO_VIEW_SCHEMA_VERSION {
            return Err(ViewCodecError::UnsupportedSchemaVersion {
                kind: "RepoViewV2",
                version: self.schema_version,
                expected: REPO_VIEW_SCHEMA_VERSION,
            });
        }
        if self.repo_id.trim().is_empty() {
            return Err(ViewCodecError::EmptyRepoId);
        }
        Ok(())
    }
}

impl WorkspaceSnapshotV2 {
    pub fn new(
        workspace_id: WorkspaceId,
        head: HeadState,
        index_tree_oid: ObjectHash,
        raw_index_blob_oid: ObjectHash,
        working_copy_tree_oid: ObjectHash,
        untracked_manifest_oid: ObjectHash,
    ) -> Result<Self, ViewCodecError> {
        let snapshot = Self {
            schema_version: WORKSPACE_SNAPSHOT_SCHEMA_VERSION,
            workspace_id,
            head,
            index_tree_oid,
            raw_index_blob_oid,
            working_copy_tree_oid,
            untracked_manifest_oid,
            sparse_facet_oid: None,
            sequencer_facet_oid: None,
            worktree_generation: 0,
            capture_policy: CapturePolicy::Tracked,
            completeness: Completeness::Full,
            facet_restore_policies: BTreeMap::new(),
        };
        snapshot.validate_schema()?;
        Ok(snapshot)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ViewCodecError> {
        self.validate_schema()?;
        serde_json::to_vec(self).map_err(|error| ViewCodecError::InvalidJson(error.to_string()))
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ViewCodecError> {
        let snapshot: Self = serde_json::from_slice(bytes)
            .map_err(|error| ViewCodecError::InvalidJson(error.to_string()))?;
        snapshot.validate_schema()?;
        Ok(snapshot)
    }

    pub fn write_manifest(&self, storage: &ClientStorage) -> Result<ObjectHash, ViewCodecError> {
        let bytes = self.canonical_bytes()?;
        let oid = ObjectHash::from_type_and_data(ObjectType::Blob, &bytes);
        storage
            .put(&oid, &bytes, ObjectType::Blob)
            .map_err(|error| ViewCodecError::Storage(error.to_string()))?;
        Ok(oid)
    }

    pub fn roots(&self) -> Vec<ObjectHash> {
        let mut roots = vec![
            self.index_tree_oid,
            self.raw_index_blob_oid,
            self.working_copy_tree_oid,
            self.untracked_manifest_oid,
        ];
        if let Some(oid) = self.sparse_facet_oid {
            roots.push(oid);
        }
        if let Some(oid) = self.sequencer_facet_oid {
            roots.push(oid);
        }
        if let HeadState::Detached { oid } = self.head {
            roots.push(oid);
        }
        roots
    }

    pub fn validate_closure<F>(&self, mut contains: F) -> Result<(), ViewCodecError>
    where
        F: FnMut(&ObjectHash) -> bool,
    {
        self.roots()
            .into_iter()
            .find(|oid| !contains(oid))
            .map_or(Ok(()), |oid| Err(ViewCodecError::MissingObject(oid)))
    }

    fn validate_schema(&self) -> Result<(), ViewCodecError> {
        if self.schema_version != WORKSPACE_SNAPSHOT_SCHEMA_VERSION {
            return Err(ViewCodecError::UnsupportedSchemaVersion {
                kind: "WorkspaceSnapshotV2",
                version: self.schema_version,
                expected: WORKSPACE_SNAPSHOT_SCHEMA_VERSION,
            });
        }
        if self.workspace_id.trim().is_empty() {
            return Err(ViewCodecError::EmptyWorkspaceId);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::operation::facet::RestorePolicy;

    fn oid(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes(&[byte; 20]).expect("sha1 oid")
    }

    #[test]
    fn repo_view_roundtrips_canonically_and_enumerates_closure() {
        let mut workspaces = BTreeMap::new();
        workspaces.insert("main".to_string(), oid(2));
        let view = RepoViewV2::new(
            "repo".to_string(),
            oid(1),
            workspaces,
            vec![oid(3)],
            BTreeMap::from([("extension".to_string(), oid(4))]),
        )
        .expect("valid view");
        let bytes = view.canonical_bytes().expect("encode");
        assert_eq!(
            RepoViewV2::from_canonical_bytes(&bytes).expect("decode"),
            view
        );
        assert_eq!(view.roots().len(), 4);
        assert!(
            view.validate_closure(|candidate| candidate != &oid(4))
                .is_err()
        );
    }

    #[test]
    fn unknown_schema_versions_fail_closed() {
        let mut value = serde_json::json!({
            "schema_version": 99,
            "repo_id": "repo",
            "refs_facet_oid": oid(1),
            "workspaces": {},
            "change_roots": [],
            "extension_facets": {}
        });
        value["schema_version"] = serde_json::json!(99);
        let error = RepoViewV2::from_canonical_bytes(
            &serde_json::to_vec(&value).expect("encode invalid version"),
        )
        .expect_err("unknown schema must fail");
        assert!(matches!(
            error,
            ViewCodecError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn workspace_snapshot_includes_restore_policy_and_roots() {
        let mut snapshot = WorkspaceSnapshotV2::new(
            "main".to_string(),
            HeadState::Detached { oid: oid(9) },
            oid(1),
            oid(2),
            oid(3),
            oid(4),
        )
        .expect("valid snapshot");
        snapshot
            .facet_restore_policies
            .insert("index".to_string(), RestorePolicy::AutoRestore);
        snapshot.sparse_facet_oid = Some(oid(5));
        assert_eq!(snapshot.roots().len(), 6);
        assert_eq!(
            WorkspaceSnapshotV2::from_canonical_bytes(&snapshot.canonical_bytes().unwrap())
                .unwrap(),
            snapshot
        );
    }
}
