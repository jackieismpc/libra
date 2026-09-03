//! Durable worktree pointer and operation-head staleness classification.
//!
//! A pointer records the operation/view pair last materialized in one
//! workspace. It is deliberately separate from `operation_head`: the latter
//! is the repository's publish point, while this file is the local checkout's
//! acknowledgement of that point. Updating it is atomic so an interrupted
//! mutation cannot leave a partially written pointer.

use std::{fs, io, path::Path, str::FromStr};

use git_internal::hash::ObjectHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::store::OpHead;

const POINTER_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatePointer {
    pub last_op_id: String,
    pub last_snapshot_oid: ObjectHash,
    pub generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    Stale,
    Sibling,
}

#[derive(Debug, Error)]
pub enum PointerError {
    #[error("workspace pointer I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("workspace pointer JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace pointer has invalid snapshot object id '{0}'")]
    InvalidSnapshotOid(String),
    #[error("workspace pointer schema version {0} is unsupported")]
    UnsupportedSchema(u32),
    #[error("workspace pointer operation id must not be empty")]
    EmptyOperationId,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PointerFile {
    schema_version: u32,
    last_op_id: String,
    last_snapshot_oid: String,
    generation: i64,
}

impl WorkspaceStatePointer {
    pub fn new(
        last_op_id: impl Into<String>,
        last_snapshot_oid: ObjectHash,
        generation: i64,
    ) -> Result<Self, PointerError> {
        let last_op_id = last_op_id.into();
        if last_op_id.trim().is_empty() {
            return Err(PointerError::EmptyOperationId);
        }
        Ok(Self {
            last_op_id,
            last_snapshot_oid,
            generation,
        })
    }

    pub fn load(path: &Path) -> Result<Option<Self>, PointerError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file: PointerFile = serde_json::from_slice(&bytes)?;
        if file.schema_version != POINTER_SCHEMA_VERSION {
            return Err(PointerError::UnsupportedSchema(file.schema_version));
        }
        let snapshot_oid = ObjectHash::from_str(&file.last_snapshot_oid)
            .map_err(|_| PointerError::InvalidSnapshotOid(file.last_snapshot_oid.clone()))?;
        Self::new(file.last_op_id, snapshot_oid, file.generation).map(Some)
    }

    pub fn save(&self, path: &Path) -> Result<(), PointerError> {
        if self.last_op_id.trim().is_empty() {
            return Err(PointerError::EmptyOperationId);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = PointerFile {
            schema_version: POINTER_SCHEMA_VERSION,
            last_op_id: self.last_op_id.clone(),
            last_snapshot_oid: self.last_snapshot_oid.to_string(),
            generation: self.generation,
        };
        let bytes = serde_json::to_vec(&file)?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    /// Compare this local pointer with the currently published heads.
    ///
    /// `is_ancestor(a, b)` must answer whether operation `a` is an ancestor
    /// of operation `b`. Generation is used as a cheap guard before walking
    /// the DAG; an equal operation id is always fresh.
    pub fn staleness<F>(&self, heads: &[OpHead], is_ancestor: F) -> Staleness
    where
        F: Fn(&str, &str) -> bool,
    {
        if heads
            .iter()
            .any(|head| head.op_id == self.last_op_id && head.generation == self.generation)
        {
            return Staleness::Fresh;
        }
        if heads.iter().any(|head| {
            head.generation >= self.generation && is_ancestor(&self.last_op_id, &head.op_id)
        }) {
            return Staleness::Stale;
        }
        Staleness::Sibling
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes(&[byte; 20]).expect("oid")
    }

    #[test]
    fn missing_pointer_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp");
        assert!(
            WorkspaceStatePointer::load(&dir.path().join("pointer.json"))
                .expect("load")
                .is_none()
        );
    }

    #[test]
    fn pointer_roundtrip_is_atomic_and_strict() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("state/pointer.json");
        let pointer = WorkspaceStatePointer::new("op-1", oid(1), 3).expect("pointer");
        pointer.save(&path).expect("save");
        assert_eq!(
            WorkspaceStatePointer::load(&path).expect("load"),
            Some(pointer)
        );
        fs::write(&path, br#"{"schema_version":2,"last_op_id":"op-1","last_snapshot_oid":"bad","generation":3,"extra":true}"#).expect("write");
        assert!(matches!(
            WorkspaceStatePointer::load(&path),
            Err(PointerError::Json(_))
        ));
    }

    #[test]
    fn stale_and_sibling_are_distinct() {
        let pointer = WorkspaceStatePointer::new("base", oid(1), 2).expect("pointer");
        let stale = [OpHead {
            op_id: "tip".to_string(),
            generation: 4,
        }];
        assert_eq!(
            pointer.staleness(&stale, |from, to| from == "base" && to == "tip"),
            Staleness::Stale
        );
        let sibling = [OpHead {
            op_id: "other".to_string(),
            generation: 2,
        }];
        assert_eq!(
            pointer.staleness(&sibling, |_, _| false),
            Staleness::Sibling
        );
        let fresh = [OpHead {
            op_id: "base".to_string(),
            generation: 2,
        }];
        assert_eq!(pointer.staleness(&fresh, |_, _| false), Staleness::Fresh);
    }
}
