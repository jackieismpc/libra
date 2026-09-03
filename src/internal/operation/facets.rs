//! File-backed index, sequencer, and sparse-state facets.
//!
//! Each adapter owns one controlled path and an object store. The restore
//! policy belongs to the facet (index and sequencer are auto-restorable;
//! sparse state is rebuilt by default), while the registry decides whether a
//! complete snapshot may be advertised as fully restorable.

use std::{fs, path::PathBuf};

use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};

use super::facet::{
    FacetCapture, FacetCaptureCtx, FacetDiff, FacetError, FacetName, FacetRestoreCtx,
    RestorePolicy, StateFacet,
};
use crate::utils::client_storage::ClientStorage;

const FACET_SCHEMA_VERSION: u32 = 1;

#[derive(Clone)]
pub struct FileStateFacet {
    name: FacetName,
    path: PathBuf,
    storage: ClientStorage,
    policy: RestorePolicy,
}

impl FileStateFacet {
    pub fn new(
        name: impl Into<String>,
        path: PathBuf,
        storage: ClientStorage,
        policy: RestorePolicy,
    ) -> Self {
        Self {
            name: name.into(),
            path,
            storage,
            policy,
        }
    }

    fn capture_file(&self) -> Result<FacetCapture, FacetError> {
        let data = fs::read(&self.path)
            .map_err(|error| FacetError::Operation(self.name.clone(), error.to_string()))?;
        let oid = ObjectHash::from_type_and_data(ObjectType::Blob, &data);
        self.storage
            .put(&oid, &data, ObjectType::Blob)
            .map_err(|error| FacetError::Operation(self.name.clone(), error.to_string()))?;
        Ok(FacetCapture {
            facet: self.name.clone(),
            schema_version: FACET_SCHEMA_VERSION,
            payload_oid: Some(oid),
            meta: serde_json::json!({ "path": self.path.display().to_string(), "workspace_id": "redacted" }),
        })
    }

    fn restore_file(&self, capture: &FacetCapture) -> Result<(), FacetError> {
        let Some(oid) = capture.payload_oid else {
            return Err(FacetError::InvalidCapture(
                self.name.clone(),
                "missing payload object".to_string(),
            ));
        };
        let data = self
            .storage
            .get(&oid)
            .map_err(|error| FacetError::Operation(self.name.clone(), error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| FacetError::Operation(self.name.clone(), error.to_string()))?;
        }
        fs::write(&self.path, data)
            .map_err(|error| FacetError::Operation(self.name.clone(), error.to_string()))?;
        Ok(())
    }
}

macro_rules! file_facet {
    ($name:ident, $constructor:ident, $facet_name:literal, $policy:expr) => {
        pub struct $name(FileStateFacet);

        impl $name {
            pub fn $constructor(path: PathBuf, storage: ClientStorage) -> Self {
                Self(FileStateFacet::new($facet_name, path, storage, $policy))
            }
        }

        impl StateFacet for $name {
            fn name(&self) -> FacetName {
                self.0.name()
            }
            fn schema_version(&self) -> u32 {
                FACET_SCHEMA_VERSION
            }
            fn restore_policy(&self) -> RestorePolicy {
                self.0.policy
            }
            fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
                self.0.capture_file()
            }
            fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError> {
                if capture.facet != self.0.name || capture.schema_version != FACET_SCHEMA_VERSION {
                    return Err(FacetError::InvalidCapture(
                        self.0.name.clone(),
                        "facet identity mismatch".to_string(),
                    ));
                }
                Ok(())
            }
            fn restore(
                &self,
                capture: &FacetCapture,
                _ctx: &mut FacetRestoreCtx,
            ) -> Result<(), FacetError> {
                self.0.restore_file(capture)
            }
            fn diff(
                &self,
                from: &FacetCapture,
                to: &FacetCapture,
            ) -> Result<FacetDiff, FacetError> {
                self.validate(from)?;
                self.validate(to)?;
                Ok(FacetDiff {
                    facet: self.name(),
                    from: from.payload_oid,
                    to: to.payload_oid,
                })
            }
            fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash> {
                capture.payload_oid.into_iter().collect()
            }
        }
    };
}

file_facet!(IndexFacet, index, "index", RestorePolicy::AutoRestore);
file_facet!(
    SequencerFacet,
    sequencer,
    "sequencer",
    RestorePolicy::AutoRestore
);
file_facet!(SparseFacet, sparse, "sparse", RestorePolicy::Rebuild);

impl FileStateFacet {
    fn name(&self) -> FacetName {
        self.name.clone()
    }
}
