//! Uniform capture/restore contracts for mutable repository state.

use std::collections::BTreeMap;

use git_internal::hash::ObjectHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type FacetName = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePolicy {
    AutoRestore,
    Rebuild,
    NeverRestore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct FacetCapture {
    pub facet: FacetName,
    pub schema_version: u32,
    pub payload_oid: Option<ObjectHash>,
    pub meta: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct FacetCaptureCtx {
    pub workspace_id: String,
}

#[derive(Debug, Default)]
pub struct FacetRestoreCtx {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetDiff {
    pub facet: FacetName,
    pub from: Option<ObjectHash>,
    pub to: Option<ObjectHash>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FacetError {
    #[error("facet '{0}' is already registered")]
    Duplicate(FacetName),
    #[error("facet '{0}' is not registered")]
    Unregistered(FacetName),
    #[error("facet '{0}' capture is invalid: {1}")]
    InvalidCapture(FacetName, String),
    #[error("facet '{0}' operation failed: {1}")]
    Operation(FacetName, String),
}

pub trait StateFacet: Send + Sync {
    fn name(&self) -> FacetName;
    fn schema_version(&self) -> u32;
    fn restore_policy(&self) -> RestorePolicy;
    fn capture(&self, ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError>;
    fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError>;
    fn restore(
        &self,
        capture: &FacetCapture,
        ctx: &mut FacetRestoreCtx,
    ) -> Result<(), FacetError>;
    fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError>;
    fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash>;
}

#[derive(Default)]
pub struct FacetRegistry {
    facets: BTreeMap<FacetName, Box<dyn StateFacet>>,
}

impl std::fmt::Debug for FacetRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FacetRegistry")
            .field("facets", &self.facets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl FacetRegistry {
    pub fn register(&mut self, facet: Box<dyn StateFacet>) -> Result<(), FacetError> {
        let name = facet.name();
        if self.facets.contains_key(&name) {
            return Err(FacetError::Duplicate(name));
        }
        self.facets.insert(name, facet);
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.facets.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &FacetName> {
        self.facets.keys()
    }

    pub fn capture_all(&self, ctx: &FacetCaptureCtx) -> Result<Vec<FacetCapture>, FacetError> {
        self.facets.values().map(|facet| facet.capture(ctx)).collect()
    }

    pub fn validate_all(&self, captures: &[FacetCapture]) -> Result<(), FacetError> {
        for capture in captures {
            let facet = self
                .facets
                .get(&capture.facet)
                .ok_or_else(|| FacetError::Unregistered(capture.facet.clone()))?;
            if capture.schema_version != facet.schema_version() {
                return Err(FacetError::InvalidCapture(
                    capture.facet.clone(),
                    format!(
                        "schema version {} does not match {}",
                        capture.schema_version,
                        facet.schema_version()
                    ),
                ));
            }
            facet.validate(capture)?;
        }
        Ok(())
    }

    /// A snapshot is fully restorable only when every captured facet is
    /// registered and each capture has a payload. Unknown facets fail closed.
    pub fn fully_restorable(&self, captures: &[FacetCapture]) -> bool {
        self.validate_all(captures).is_ok()
            && captures.iter().all(|capture| {
                capture.payload_oid.is_some()
                    && self
                        .facets
                        .get(&capture.facet)
                        .is_some_and(|facet| facet.restore_policy() != RestorePolicy::NeverRestore)
            })
    }

    pub fn roots(&self, captures: &[FacetCapture]) -> Result<Vec<ObjectHash>, FacetError> {
        self.validate_all(captures)?;
        Ok(captures
            .iter()
            .flat_map(|capture| self.facets[&capture.facet].roots(capture))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFacet {
        name: &'static str,
        policy: RestorePolicy,
        payload: Option<ObjectHash>,
    }

    impl StateFacet for TestFacet {
        fn name(&self) -> FacetName {
            self.name.to_string()
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn restore_policy(&self) -> RestorePolicy {
            self.policy
        }
        fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
            Ok(FacetCapture {
                facet: self.name(),
                schema_version: 1,
                payload_oid: self.payload,
                meta: serde_json::json!({}),
            })
        }
        fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError> {
            if capture.facet != self.name {
                return Err(FacetError::InvalidCapture(
                    capture.facet.clone(),
                    "wrong facet".to_string(),
                ));
            }
            Ok(())
        }
        fn restore(
            &self,
            _capture: &FacetCapture,
            _ctx: &mut FacetRestoreCtx,
        ) -> Result<(), FacetError> {
            Ok(())
        }
        fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError> {
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

    fn oid(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes(&[byte; 20]).expect("sha1 oid")
    }

    #[test]
    fn unknown_facet_is_not_fully_restorable() {
        let registry = FacetRegistry::default();
        let capture = FacetCapture {
            facet: "unknown".to_string(),
            schema_version: 1,
            payload_oid: Some(oid(1)),
            meta: serde_json::json!({}),
        };
        assert!(!registry.fully_restorable(&[capture]));
    }

    #[test]
    fn registered_facets_capture_and_validate() {
        let mut registry = FacetRegistry::default();
        registry
            .register(Box::new(TestFacet {
                name: "index",
                policy: RestorePolicy::AutoRestore,
                payload: Some(oid(2)),
            }))
            .expect("register facet");
        let captures = registry
            .capture_all(&FacetCaptureCtx::default())
            .expect("capture");
        assert!(registry.fully_restorable(&captures));
        assert_eq!(registry.roots(&captures).unwrap(), vec![oid(2)]);
    }

    #[test]
    fn never_restore_facet_is_not_fully_restorable() {
        let mut registry = FacetRegistry::default();
        registry
            .register(Box::new(TestFacet {
                name: "runtime",
                policy: RestorePolicy::NeverRestore,
                payload: Some(oid(3)),
            }))
            .expect("register facet");
        let captures = registry
            .capture_all(&FacetCaptureCtx::default())
            .expect("capture");
        assert!(!registry.fully_restorable(&captures));
    }
}
