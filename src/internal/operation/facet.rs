//! Uniform capture and restore contracts for mutable repository state.
//!
//! A facet owns one part of repository state.  Registering facets centrally
//! lets snapshot and restore code fail closed when a new mutable state owner
//! has not yet supplied capture/validation/restore semantics.

use std::{
    collections::BTreeMap,
    fmt, fs,
    future::Future,
    path::{Path, PathBuf},
};

use git_internal::{
    hash::ObjectHash,
    internal::object::{blob::Blob, types::ObjectType},
};
use sea_orm::{DatabaseConnection, TransactionTrait};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{store::OperationStoreV2, working_copy::PinnedRequestScope};
use crate::{
    internal::{
        sequencer::{SequenceKind, SequenceState},
        sparse::SparseViewStore,
        worktree_scope::WorktreeScope,
    },
    utils::{atomic_write::write_atomic, client_storage::ClientStorage},
};

const FACET_SCHEMA_VERSION: u32 = 1;
const MAX_RAW_INDEX_BYTES: u64 = 64 * 1024 * 1024;
pub const RAW_INDEX_FACET_NAME: &str = "index";
pub const SEQUENCER_FACET_NAME: &str = "sequencer";
pub const SPARSE_FACET_NAME: &str = "sparse";

/// Stable name of a state facet.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FacetName(String);

impl FacetName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for FacetName {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for FacetName {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for FacetName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How a facet participates in recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePolicy {
    AutoRestore,
    Rebuild,
    NeverRestore,
}

/// A captured facet payload and its bounded metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacetCapture {
    pub facet: FacetName,
    pub schema_version: u32,
    pub payload_oid: Option<ObjectHash>,
    pub meta: serde_json::Value,
}

/// Context supplied to a facet while capturing state.
#[derive(Debug, Default)]
pub struct FacetCaptureCtx {
    pub repo_id: Option<String>,
    pub workspace_id: Option<String>,
}

/// Context supplied to a facet while restoring state.
#[derive(Debug, Default)]
pub struct FacetRestoreCtx {
    pub repo_id: Option<String>,
    pub workspace_id: Option<String>,
}

/// Semantic facet delta used by future `op revert` implementations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacetDiff {
    pub changes: serde_json::Value,
}

/// Errors returned by facet implementations and the registry boundary.
#[derive(Debug, Error)]
pub enum FacetError {
    #[error("facet '{0}' is not registered")]
    Unregistered(FacetName),
    #[error("facet name must not be empty")]
    EmptyName,
    #[error("facet '{facet}' returned a capture for '{returned}'")]
    NameMismatch {
        facet: FacetName,
        returned: FacetName,
    },
    #[error("facet '{facet}' schema version mismatch: expected {expected}, got {actual}")]
    SchemaVersionMismatch {
        facet: FacetName,
        expected: u32,
        actual: u32,
    },
    #[error("facet metadata contains a floating-point number")]
    NonCanonicalMetadata,
    #[error("facet capture is not fully registered")]
    IncompleteCapture,
    #[error("facet capture failed: {0}")]
    Capture(String),
    #[error("facet validation failed: {0}")]
    Validation(String),
    #[error("facet restore failed: {0}")]
    Restore(String),
    #[error("facet diff failed: {0}")]
    Diff(String),
}

/// Registry of every mutable state owner known to the operation layer.
#[derive(Default)]
pub struct FacetRegistry {
    facets: BTreeMap<FacetName, Box<dyn StateFacet>>,
}

impl fmt::Debug for FacetRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FacetRegistry")
            .field("facets", &self.facets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl FacetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, facet: Box<dyn StateFacet>) -> Result<(), FacetError> {
        let name = facet.name();
        if name.as_str().trim().is_empty() {
            return Err(FacetError::EmptyName);
        }
        if self.facets.contains_key(&name) {
            return Err(FacetError::Validation(format!(
                "facet '{name}' was registered more than once"
            )));
        }
        self.facets.insert(name, facet);
        Ok(())
    }

    pub fn get(&self, name: &FacetName) -> Option<&dyn StateFacet> {
        self.facets.get(name).map(Box::as_ref)
    }

    pub fn len(&self) -> usize {
        self.facets.len()
    }

    /// Stable, sorted facet names for a capture pass. Keeping enumeration in
    /// the registry makes an unregistered mutable state owner fail closed.
    pub fn names(&self) -> Vec<FacetName> {
        self.facets.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.facets.is_empty()
    }

    /// Capture one registered facet and validate the returned envelope before
    /// it can be included in a fully-restorable snapshot.
    pub fn capture(
        &self,
        name: &FacetName,
        ctx: &FacetCaptureCtx,
    ) -> Result<FacetCapture, FacetError> {
        let facet = self
            .get(name)
            .ok_or_else(|| FacetError::Unregistered(name.clone()))?;
        let capture = facet.capture(ctx)?;
        if capture.facet != *name {
            return Err(FacetError::NameMismatch {
                facet: name.clone(),
                returned: capture.facet,
            });
        }
        self.validate_capture(&capture)?;
        Ok(capture)
    }

    pub fn validate_capture(&self, capture: &FacetCapture) -> Result<(), FacetError> {
        let facet = self
            .get(&capture.facet)
            .ok_or_else(|| FacetError::Unregistered(capture.facet.clone()))?;
        if capture.schema_version != facet.schema_version() {
            return Err(FacetError::SchemaVersionMismatch {
                facet: capture.facet.clone(),
                expected: facet.schema_version(),
                actual: capture.schema_version,
            });
        }
        validate_metadata(&capture.meta)?;
        facet.validate(capture)
    }

    /// Unknown or unregistered facets are never considered fully restorable.
    pub fn is_fully_restorable(&self, captures: &[FacetCapture]) -> bool {
        captures.iter().all(|capture| {
            self.get(&capture.facet).is_some_and(|facet| {
                facet.schema_version() == capture.schema_version
                    && facet.restore_policy() != RestorePolicy::NeverRestore
            })
        })
    }

    pub fn policies(&self, captures: &[FacetCapture]) -> BTreeMap<FacetName, RestorePolicy> {
        captures
            .iter()
            .filter_map(|capture| {
                self.get(&capture.facet)
                    .map(|facet| (capture.facet.clone(), facet.restore_policy()))
            })
            .collect()
    }
}

/// Trait implemented by each mutable state owner.
pub trait StateFacet: Send + Sync {
    fn name(&self) -> FacetName;
    fn schema_version(&self) -> u32;
    fn restore_policy(&self) -> RestorePolicy;
    fn capture(&self, ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError>;
    fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError>;
    fn restore(&self, capture: &FacetCapture, ctx: &mut FacetRestoreCtx) -> Result<(), FacetError>;
    fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError>;
    fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash>;
}

fn store_payload(storage: &ClientStorage, payload: &[u8]) -> Result<ObjectHash, FacetError> {
    let blob = Blob::from_content_bytes(payload.to_vec());
    storage
        .put(&blob.id, payload, ObjectType::Blob)
        .map_err(|error| FacetError::Capture(error.to_string()))?;
    Ok(blob.id)
}

/// Bridge the synchronous StateFacet contract to repository-owned async
/// stores without blocking a multi-thread runtime worker. A current-thread
/// runtime cannot synchronously service a database future while its only
/// worker is blocked by the StateFacet contract, so it fails explicitly.
fn run_db<F, T>(future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => Err(
            "database-backed facets require a multi-thread Tokio runtime for synchronous capture"
                .to_string(),
        ),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?
            .block_on(future),
    }
}

fn load_payload(
    storage: &ClientStorage,
    capture: &FacetCapture,
) -> Result<Option<Vec<u8>>, FacetError> {
    capture
        .payload_oid
        .map(|oid| {
            storage
                .get(&oid)
                .map_err(|error| FacetError::Restore(error.to_string()))
        })
        .transpose()
}

fn validate_payload(
    storage: &ClientStorage,
    capture: &FacetCapture,
    facet: &str,
) -> Result<(), FacetError> {
    let Some(oid) = capture.payload_oid else {
        return Ok(());
    };
    storage
        .get(&oid)
        .map(|_| ())
        .map_err(|error| FacetError::Validation(format!("{facet} payload is unavailable: {error}")))
}

fn file_diff(from: &FacetCapture, to: &FacetCapture) -> FacetDiff {
    FacetDiff {
        changes: serde_json::json!({
            "from_payload_oid": from.payload_oid,
            "to_payload_oid": to.payload_oid,
        }),
    }
}

fn capture_roots(capture: &FacetCapture) -> Vec<ObjectHash> {
    capture.payload_oid.into_iter().collect()
}

fn restore_file(path: &Path, payload: Option<&[u8]>) -> Result<(), FacetError> {
    match payload {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    FacetError::Restore(format!("could not create facet parent: {error}"))
                })?;
            }
            write_atomic(path, bytes, true)
                .map_err(|error| FacetError::Restore(error.to_string()))?;
        }
        None => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(path).map_err(|error| FacetError::Restore(error.to_string()))?;
            }
            Ok(_) => {
                fs::remove_file(path).map_err(|error| FacetError::Restore(error.to_string()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(FacetError::Restore(error.to_string())),
        },
    }
    Ok(())
}

fn read_optional_file(path: &Path, facet: &str) -> Result<Option<Vec<u8>>, FacetError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FacetError::Capture(format!(
            "could not read {facet} state: {error}"
        ))),
    }
}

fn file_meta(payload: Option<&[u8]>) -> serde_json::Value {
    serde_json::json!({
        "present": payload.is_some(),
        "byte_len": payload.map_or(0, <[u8]>::len),
    })
}

/// Preserves the raw Git index bytes, including stat data and extension bits
/// such as intent-to-add, skip-worktree, and assume-unchanged. OL-10 owns
/// the general restore engine; this facet only owns its exact payload.
pub struct RawIndexFacet {
    path: PathBuf,
    storage: ClientStorage,
}

impl RawIndexFacet {
    pub fn new(scope: &PinnedRequestScope, storage: ClientStorage) -> Self {
        Self::with_path(scope.gitdir.join("index"), storage)
    }

    pub fn with_path(path: PathBuf, storage: ClientStorage) -> Self {
        Self { path, storage }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl StateFacet for RawIndexFacet {
    fn name(&self) -> FacetName {
        FacetName::from(RAW_INDEX_FACET_NAME)
    }

    fn schema_version(&self) -> u32 {
        FACET_SCHEMA_VERSION
    }

    fn restore_policy(&self) -> RestorePolicy {
        RestorePolicy::AutoRestore
    }

    fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
        let payload = read_optional_file(&self.path, RAW_INDEX_FACET_NAME)?;
        if let Some(bytes) = payload.as_ref()
            && bytes.len() as u64 > MAX_RAW_INDEX_BYTES
        {
            return Err(FacetError::Capture(format!(
                "raw index exceeds {} byte capture budget",
                MAX_RAW_INDEX_BYTES
            )));
        }
        let payload_oid = payload
            .as_deref()
            .map(|bytes| store_payload(&self.storage, bytes))
            .transpose()?;
        Ok(FacetCapture {
            facet: self.name(),
            schema_version: FACET_SCHEMA_VERSION,
            payload_oid,
            meta: file_meta(payload.as_deref()),
        })
    }

    fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError> {
        if capture.payload_oid.is_none() {
            return Err(FacetError::Validation(
                "raw index capture must contain a payload".to_string(),
            ));
        }
        validate_payload(&self.storage, capture, RAW_INDEX_FACET_NAME)
    }

    fn restore(
        &self,
        capture: &FacetCapture,
        _ctx: &mut FacetRestoreCtx,
    ) -> Result<(), FacetError> {
        let payload = load_payload(&self.storage, capture)?;
        restore_file(&self.path, payload.as_deref())
    }

    fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError> {
        Ok(file_diff(from, to))
    }

    fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash> {
        capture_roots(capture)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SequencerStateEnvelope {
    kind: String,
    head_name: String,
    head_orig: String,
    current_oid: String,
    todo: Vec<String>,
    payload: String,
}

impl From<SequenceState> for SequencerStateEnvelope {
    fn from(state: SequenceState) -> Self {
        Self {
            kind: state.kind.as_str().to_string(),
            head_name: state.head_name,
            head_orig: state.head_orig,
            current_oid: state.current_oid,
            todo: state.todo,
            payload: state.payload,
        }
    }
}

impl TryFrom<SequencerStateEnvelope> for SequenceState {
    type Error = FacetError;

    fn try_from(envelope: SequencerStateEnvelope) -> Result<Self, Self::Error> {
        let kind = match envelope.kind.as_str() {
            "merge" => SequenceKind::Merge,
            "revert" => SequenceKind::Revert,
            "cherry_pick" => SequenceKind::CherryPick,
            "rebase" => SequenceKind::Rebase,
            other => {
                return Err(FacetError::Validation(format!(
                    "unknown sequencer kind '{other}'"
                )));
            }
        };
        Ok(Self {
            kind,
            head_name: envelope.head_name,
            head_orig: envelope.head_orig,
            current_oid: envelope.current_oid,
            todo: envelope.todo,
            payload: envelope.payload,
        })
    }
}

/// Captures the unified `sequence_state` row through the sequencer module's
/// scoped SQL helpers. The adapter never resolves the process cwd itself.
pub struct SequencerFacet {
    scope: WorktreeScope,
    db: DatabaseConnection,
    storage: ClientStorage,
}

impl SequencerFacet {
    pub fn new(scope: &PinnedRequestScope, db: DatabaseConnection, storage: ClientStorage) -> Self {
        Self {
            scope: scope.scope.clone(),
            db,
            storage,
        }
    }

    pub fn from_store(scope: &PinnedRequestScope, store: &OperationStoreV2) -> Self {
        Self::new(scope, store.db().clone(), store.storage().clone())
    }

    /// Test/setup helper that writes the same scoped row consumed by capture.
    pub fn write_state(&self, state: Option<SequenceState>) -> Result<(), FacetError> {
        let db = self.db.clone();
        let scope = self.scope.clone();
        run_db(async move {
            let txn = db.begin().await.map_err(|error| error.to_string())?;
            match state {
                Some(state) => {
                    crate::internal::sequencer::save_for_scope_with_conn(&txn, &scope, &state)
                        .await
                        .map_err(|error| error.to_string())?
                }
                None => crate::internal::sequencer::clear_for_scope_all_with_conn(&txn, &scope)
                    .await
                    .map_err(|error| error.to_string())?,
            }
            txn.commit().await.map_err(|error| error.to_string())
        })
        .map_err(FacetError::Restore)
    }
}

impl StateFacet for SequencerFacet {
    fn name(&self) -> FacetName {
        FacetName::from(SEQUENCER_FACET_NAME)
    }

    fn schema_version(&self) -> u32 {
        FACET_SCHEMA_VERSION
    }

    fn restore_policy(&self) -> RestorePolicy {
        RestorePolicy::AutoRestore
    }

    fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
        let db = self.db.clone();
        let scope = self.scope.clone();
        let state = run_db(async move {
            crate::internal::sequencer::load_for_scope_with_conn(&db, &scope).await
        })
        .map_err(FacetError::Capture)?;
        let payload = state
            .map(SequencerStateEnvelope::from)
            .map(|envelope| serde_json::to_vec(&envelope))
            .transpose()
            .map_err(|error| FacetError::Capture(error.to_string()))?;
        let payload_oid = payload
            .as_deref()
            .map(|bytes| store_payload(&self.storage, bytes))
            .transpose()?;
        Ok(FacetCapture {
            facet: self.name(),
            schema_version: FACET_SCHEMA_VERSION,
            payload_oid,
            meta: file_meta(payload.as_deref()),
        })
    }

    fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError> {
        let Some(payload) = load_payload(&self.storage, capture)
            .map_err(|error| FacetError::Validation(error.to_string()))?
        else {
            return Ok(());
        };
        serde_json::from_slice::<SequencerStateEnvelope>(&payload)
            .map(|_| ())
            .map_err(|error| FacetError::Validation(error.to_string()))
    }

    fn restore(
        &self,
        capture: &FacetCapture,
        _ctx: &mut FacetRestoreCtx,
    ) -> Result<(), FacetError> {
        let state = load_payload(&self.storage, capture)?
            .map(|payload| {
                serde_json::from_slice::<SequencerStateEnvelope>(&payload)
                    .map_err(|error| FacetError::Restore(error.to_string()))
                    .and_then(SequenceState::try_from)
            })
            .transpose()?;
        self.write_state(state)
    }

    fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError> {
        Ok(file_diff(from, to))
    }

    fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash> {
        capture_roots(capture)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SparseStateEnvelope {
    enabled: bool,
    patterns: Vec<String>,
}

/// Captures the sparse-view toggle and ordered pattern list from their owned
/// SQLite tables. Its policy is AutoRestore because these rows are mutable
/// user state rather than a derived display cache.
pub struct SparseFacet {
    scope: WorktreeScope,
    db: DatabaseConnection,
    storage: ClientStorage,
}

impl SparseFacet {
    pub fn new(scope: &PinnedRequestScope, db: DatabaseConnection, storage: ClientStorage) -> Self {
        Self {
            scope: scope.scope.clone(),
            db,
            storage,
        }
    }

    pub fn from_store(scope: &PinnedRequestScope, store: &OperationStoreV2) -> Self {
        Self::new(scope, store.db().clone(), store.storage().clone())
    }

    /// Test/setup helper that writes the same scoped tables consumed by capture.
    pub fn write_state(&self, enabled: bool, patterns: Vec<String>) -> Result<(), FacetError> {
        let db = self.db.clone();
        let scope = self.scope.clone();
        run_db(async move {
            let txn = db.begin().await.map_err(|error| error.to_string())?;
            SparseViewStore::restore_for_scope_with_conn(&txn, &scope, enabled, &patterns).await?;
            txn.commit().await.map_err(|error| error.to_string())
        })
        .map_err(FacetError::Restore)
    }
}

impl StateFacet for SparseFacet {
    fn name(&self) -> FacetName {
        FacetName::from(SPARSE_FACET_NAME)
    }

    fn schema_version(&self) -> u32 {
        FACET_SCHEMA_VERSION
    }

    fn restore_policy(&self) -> RestorePolicy {
        RestorePolicy::AutoRestore
    }

    fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
        let db = self.db.clone();
        let scope = self.scope.clone();
        let (enabled, patterns) =
            run_db(async move { SparseViewStore::state_for_scope_with_conn(&db, &scope).await })
                .map_err(FacetError::Capture)?;
        let payload = serde_json::to_vec(&SparseStateEnvelope { enabled, patterns })
            .map_err(|error| FacetError::Capture(error.to_string()))?;
        let payload_oid = store_payload(&self.storage, &payload)?;
        Ok(FacetCapture {
            facet: self.name(),
            schema_version: FACET_SCHEMA_VERSION,
            payload_oid: Some(payload_oid),
            meta: file_meta(Some(&payload)),
        })
    }

    fn validate(&self, capture: &FacetCapture) -> Result<(), FacetError> {
        let Some(payload) = load_payload(&self.storage, capture)
            .map_err(|error| FacetError::Validation(error.to_string()))?
        else {
            return Err(FacetError::Validation(
                "sparse capture must contain a payload".to_string(),
            ));
        };
        serde_json::from_slice::<SparseStateEnvelope>(&payload)
            .map(|_| ())
            .map_err(|error| FacetError::Validation(error.to_string()))
    }

    fn restore(
        &self,
        capture: &FacetCapture,
        _ctx: &mut FacetRestoreCtx,
    ) -> Result<(), FacetError> {
        let payload = load_payload(&self.storage, capture)?.ok_or_else(|| {
            FacetError::Restore("sparse capture is missing its payload".to_string())
        })?;
        let state = serde_json::from_slice::<SparseStateEnvelope>(&payload)
            .map_err(|error| FacetError::Restore(error.to_string()))?;
        let db = self.db.clone();
        let scope = self.scope.clone();
        run_db(async move {
            let txn = db.begin().await.map_err(|error| error.to_string())?;
            SparseViewStore::restore_for_scope_with_conn(
                &txn,
                &scope,
                state.enabled,
                &state.patterns,
            )
            .await?;
            txn.commit().await.map_err(|error| error.to_string())
        })
        .map_err(FacetError::Restore)
    }

    fn diff(&self, from: &FacetCapture, to: &FacetCapture) -> Result<FacetDiff, FacetError> {
        Ok(file_diff(from, to))
    }

    fn roots(&self, capture: &FacetCapture) -> Vec<ObjectHash> {
        capture_roots(capture)
    }
}

fn validate_metadata(value: &serde_json::Value) -> Result<(), FacetError> {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            Ok(())
        }
        serde_json::Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                Ok(())
            } else {
                Err(FacetError::NonCanonicalMetadata)
            }
        }
        serde_json::Value::Array(values) => values.iter().try_for_each(validate_metadata),
        serde_json::Value::Object(values) => values.values().try_for_each(validate_metadata),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFacet {
        policy: RestorePolicy,
        capture_name: FacetName,
    }

    impl StateFacet for TestFacet {
        fn name(&self) -> FacetName {
            FacetName::from("test")
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn restore_policy(&self) -> RestorePolicy {
            self.policy
        }

        fn capture(&self, _ctx: &FacetCaptureCtx) -> Result<FacetCapture, FacetError> {
            Ok(FacetCapture {
                facet: self.capture_name.clone(),
                schema_version: 1,
                payload_oid: None,
                meta: serde_json::json!({"count": 1}),
            })
        }

        fn validate(&self, _capture: &FacetCapture) -> Result<(), FacetError> {
            Ok(())
        }

        fn restore(
            &self,
            _capture: &FacetCapture,
            _ctx: &mut FacetRestoreCtx,
        ) -> Result<(), FacetError> {
            Ok(())
        }

        fn diff(&self, _from: &FacetCapture, _to: &FacetCapture) -> Result<FacetDiff, FacetError> {
            Ok(FacetDiff {
                changes: serde_json::json!({}),
            })
        }

        fn roots(&self, _capture: &FacetCapture) -> Vec<ObjectHash> {
            Vec::new()
        }
    }

    #[test]
    fn registry_rejects_unregistered_capture() {
        let registry = FacetRegistry::new();
        let error = registry
            .capture(&FacetName::from("missing"), &FacetCaptureCtx::default())
            .expect_err("unregistered facets must fail closed");
        assert!(matches!(error, FacetError::Unregistered(_)));
    }

    #[test]
    fn never_restore_facet_is_not_fully_restorable() {
        let mut registry = FacetRegistry::new();
        registry
            .register(Box::new(TestFacet {
                policy: RestorePolicy::NeverRestore,
                capture_name: FacetName::from("test"),
            }))
            .expect("register facet");
        let capture = registry
            .capture(&FacetName::from("test"), &FacetCaptureCtx::default())
            .expect("capture facet");
        assert!(!registry.is_fully_restorable(&[capture]));
    }

    #[test]
    fn floating_point_metadata_is_rejected() {
        let mut registry = FacetRegistry::new();
        registry
            .register(Box::new(TestFacet {
                policy: RestorePolicy::AutoRestore,
                capture_name: FacetName::from("test"),
            }))
            .expect("register facet");
        let capture = FacetCapture {
            facet: FacetName::from("test"),
            schema_version: 1,
            payload_oid: None,
            meta: serde_json::json!({"ratio": 1.5}),
        };
        assert!(matches!(
            registry.validate_capture(&capture),
            Err(FacetError::NonCanonicalMetadata)
        ));
    }

    #[test]
    fn registry_rejects_capture_with_wrong_facet_name() {
        let mut registry = FacetRegistry::new();
        registry
            .register(Box::new(TestFacet {
                policy: RestorePolicy::AutoRestore,
                capture_name: FacetName::from("other"),
            }))
            .expect("register facet");
        let error = registry
            .capture(&FacetName::from("test"), &FacetCaptureCtx::default())
            .expect_err("facet name mismatch must fail closed");
        assert!(matches!(error, FacetError::NameMismatch { .. }));
    }
}
