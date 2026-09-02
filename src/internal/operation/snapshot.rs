//! Working-copy snapshot capture for operation-log v2.
//!
//! Automatic snapshots describe a repository state; they are not commits.
//! In particular, this module never creates a Commit OID or a Change ID.  The
//! working-copy tree and the raw index are content-addressed Git objects, and
//! the operation row records the snapshot transition separately.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use git_internal::{
    hash::ObjectHash,
    internal::{
        index::Index,
        object::{
            ObjectTrait,
            tree::{Tree, TreeItem, TreeItemMode},
            types::ObjectType,
        },
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    FacetCaptureCtx, FacetRegistry, HeadState, OperationKind, OperationMetaV2, OperationStatusV2,
    OperationStoreV2, OperationV2, PinnedRequestScope, StoreError, WorkspaceSnapshotV2,
    WorkspaceStatePointer,
    view::{CapturePolicy, Completeness, WORKSPACE_SNAPSHOT_SCHEMA_VERSION},
};
use crate::{
    internal::{
        head::Head,
        worktree_io::{
            default_worktree_io,
            executor::{ExecutorError, WorktreeIo},
            protocol::{
                CapturedStat, Dirent, IoEvent, IoRequest, ObjectBlobStatus, bytes_to_path,
                io_from_wire, path_to_bytes, relative_worktree_path, unwrap_wire,
            },
        },
    },
    utils::{client_storage::ClientStorage, tree::sort_tree_items_for_git},
};

const DEFAULT_MAX_FILES: usize = 100_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_LOCK_NAME: &str = "workspace-snapshot.lock";
const UNTRACKED_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Result of a capture attempt. `NoChange` deliberately has no operation id:
/// a clean automatic snapshot must not add noise to the operation DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    NoChange,
    Captured {
        operation_id: String,
        snapshot_oid: ObjectHash,
        snapshot: Box<WorkspaceSnapshotV2>,
    },
}

/// A file read during a bounded working-copy scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub bytes: Vec<u8>,
    pub mode: u32,
    pub object_oid: ObjectHash,
    pub tracked: bool,
}

/// The read-only result consumed by the object-writing half of capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub files: BTreeMap<String, ScannedFile>,
    pub index_entries: Vec<IndexEntrySnapshot>,
    pub raw_index: Vec<u8>,
    pub untracked: BTreeSet<String>,
    pub ignored: BTreeSet<String>,
    pub deleted_tracked: BTreeSet<String>,
    pub partial: bool,
    pub partial_paths: BTreeSet<String>,
    pub changed: bool,
}

/// The semantic fields from one stage-0 Git index entry needed to rebuild its
/// tree. The raw bytes remain separately stored so index flags and stat data
/// are restored byte-for-byte by OL-07.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntrySnapshot {
    pub path: String,
    pub mode: u32,
    pub object_oid: ObjectHash,
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("working-copy scan I/O failed: {0}")]
    Io(String),
    #[error("working-copy scan timed out")]
    Timeout,
    #[error("working-copy scan encountered an invalid path '{0}'")]
    InvalidPath(String),
    #[error("working-copy index could not be read: {0}")]
    Index(String),
    #[error("untracked files are not allowed by the fail-closed capture policy")]
    UntrackedDisallowed,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("could not lock the worktree for snapshot capture: {0}")]
    Lock(String),
    #[error("snapshot scan failed: {0}")]
    Scan(#[from] ScanError),
    #[error("snapshot object write failed: {0}")]
    Object(String),
    #[error("snapshot manifest failed: {0}")]
    Manifest(String),
    #[error("snapshot operation publish failed: {0}")]
    Store(#[from] StoreError),
    #[error("snapshot pointer update failed: {0}")]
    Pointer(String),
    #[error("snapshot HEAD capture failed: {0}")]
    Head(String),
    #[error("facet capture failed: {0}")]
    Facet(String),
    #[error("snapshot restore failed: {0}")]
    Restore(String),
}

/// Captures one pinned worktree into Git objects and an operation-log v2 row.
pub struct WorkspaceSnapshotter {
    scope: PinnedRequestScope,
    io: Arc<WorktreeIo>,
    store: OperationStoreV2,
    registry: FacetRegistry,
    capture_policy: CapturePolicy,
    max_files: usize,
    max_file_bytes: u64,
}

impl WorkspaceSnapshotter {
    /// Construct a snapshotter for one request-pinned worktree.
    pub fn new(
        scope: PinnedRequestScope,
        store: OperationStoreV2,
        registry: FacetRegistry,
        capture_policy: CapturePolicy,
    ) -> Self {
        Self {
            scope,
            io: Arc::new(default_worktree_io()),
            store,
            registry,
            capture_policy,
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }

    /// Set scan budgets. A bounded budget produces a partial snapshot when it
    /// omits a file; it never turns an incomplete capture into Full.
    pub fn with_limits(mut self, max_files: usize, max_file_bytes: u64) -> Self {
        self.max_files = max_files.max(1);
        self.max_file_bytes = max_file_bytes.max(1);
        self
    }

    /// Capture under the worktree lock, publish immutable objects before the
    /// operation row, then CAS the operation head and advance the pointer.
    pub async fn capture(&mut self) -> Result<SnapshotOutcome, SnapshotError> {
        let operation_id = uuid::Uuid::now_v7().to_string();
        let _lock = WorktreeLock::acquire(&self.scope).map_err(SnapshotError::Lock)?;
        let scan = match self.scan_working_copy_locked() {
            Ok(scan) => scan,
            Err(error) => {
                self.record_diagnostic(&operation_id, error.to_string())
                    .await;
                return Err(SnapshotError::Scan(error));
            }
        };
        if !scan.changed || self.state_matches_last_snapshot(&scan).await? {
            return Ok(SnapshotOutcome::NoChange);
        }

        let result = self.capture_objects(&operation_id, &scan).await;
        match result {
            Ok(captured) => Ok(captured),
            Err(error) => {
                self.record_diagnostic(&operation_id, error.to_string())
                    .await;
                Err(error)
            }
        }
    }

    /// Scan tracked and untracked files through the bounded WorktreeIo
    /// executor. This public helper takes the same lock as `capture`; the
    /// private variant exists so capture does not acquire the flock twice.
    pub async fn scan_working_copy(&self) -> Result<ScanResult, ScanError> {
        let _lock = WorktreeLock::acquire(&self.scope).map_err(ScanError::Io)?;
        self.scan_working_copy_locked()
    }

    /// Test and recovery helper for the captured tree only. This is not the
    /// OL-10 RestoreEngine: it materializes the immutable working-copy tree
    /// and intentionally does not alter HEAD, refs, index, sequencer, or
    /// sparse state.
    pub fn restore_working_copy(
        &self,
        snapshot: &WorkspaceSnapshotV2,
    ) -> Result<(), SnapshotError> {
        let mut files = Vec::new();
        collect_tree_files(
            self.store.storage(),
            &snapshot.working_copy_tree_oid,
            Path::new(""),
            &mut files,
        )
        .map_err(SnapshotError::Restore)?;
        for (path, mode, bytes) in files {
            let relative = validate_tree_path(&path).map_err(SnapshotError::Restore)?;
            let target = self.scope.worktree_root.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| SnapshotError::Restore(error.to_string()))?;
            }
            if target.exists() || target.symlink_metadata().is_ok() {
                let metadata = target
                    .symlink_metadata()
                    .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                if metadata.is_dir() {
                    fs::remove_dir_all(&target)
                        .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                } else {
                    fs::remove_file(&target)
                        .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                }
            }
            if mode == 0o120000 {
                #[cfg(unix)]
                std::os::unix::fs::symlink(bytes_to_path(&bytes), &target)
                    .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                #[cfg(not(unix))]
                return Err(SnapshotError::Restore(
                    "symbolic-link restore is unsupported on this platform".to_string(),
                ));
            } else {
                fs::write(&target, &bytes)
                    .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                #[cfg(unix)]
                if mode == 0o100755 {
                    use std::os::unix::fs::PermissionsExt;
                    let mut permissions = fs::metadata(&target)
                        .map_err(|error| SnapshotError::Restore(error.to_string()))?
                        .permissions();
                    permissions.set_mode(0o755);
                    fs::set_permissions(&target, permissions)
                        .map_err(|error| SnapshotError::Restore(error.to_string()))?;
                }
            }
        }
        Ok(())
    }

    fn scan_working_copy_locked(&self) -> Result<ScanResult, ScanError> {
        let index_path = self.scope.gitdir.join("index");
        let raw_index = match fs::read(&index_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(ScanError::Index(error.to_string())),
        };
        let index = if raw_index.is_empty() && !index_path.exists() {
            Index::new()
        } else {
            Index::load(&index_path).map_err(|error| ScanError::Index(error.to_string()))?
        };
        let root = path_to_bytes(&self.scope.worktree_root);
        let mut files = BTreeMap::new();
        let mut index_entries = Vec::new();
        let mut deleted_tracked = BTreeSet::new();
        let mut changed = false;
        let mut partial = false;
        let mut partial_paths = BTreeSet::new();
        let mut tracked_names = BTreeSet::new();

        for entry in index.tracked_entries(0) {
            let relative = validate_relative_path(Path::new(&entry.name))?;
            let path = entry.name.clone();
            tracked_names.insert(path.clone());
            index_entries.push(IndexEntrySnapshot {
                path: path.clone(),
                mode: entry.mode,
                object_oid: entry.hash,
            });
            let stat = match self.stat(&root, &relative)? {
                Some(stat) => stat,
                None => {
                    deleted_tracked.insert(path);
                    changed = true;
                    continue;
                }
            };
            if !stat.is_file() && !stat.is_symlink() {
                return Err(ScanError::Io(format!(
                    "tracked path '{}' is not a regular file or symbolic link",
                    entry.name
                )));
            }
            if stat.len() > self.max_file_bytes {
                partial = true;
                partial_paths.insert(path);
                changed = true;
                continue;
            }
            let bytes = match self.read_file(&root, &relative)? {
                Some(bytes) => bytes,
                None => {
                    deleted_tracked.insert(entry.name.clone());
                    changed = true;
                    continue;
                }
            };
            let object_oid = blob_oid(&bytes);
            let mode = mode_from_stat(&stat);
            let stat_matches = stat_matches_index(entry, &stat);
            if !stat_matches || object_oid != entry.hash || mode != normalize_index_mode(entry.mode)
            {
                changed = true;
            }
            files.insert(
                path,
                ScannedFile {
                    bytes,
                    mode,
                    object_oid,
                    tracked: true,
                },
            );
        }

        let mut untracked = BTreeSet::new();
        let mut ignored = BTreeSet::new();
        let mut pending = vec![PathBuf::new()];
        let layers = crate::internal::layer::ExclusionSnapshot::for_scope(&self.scope.scope);
        let mut visited = 0usize;
        while let Some(directory) = pending.pop() {
            if visited >= self.max_files {
                partial = true;
                changed = true;
                partial_paths.insert(directory.to_string_lossy().to_string());
                break;
            }
            let (mut entries, hit_cap) = self.read_dir(&root, &directory)?;
            if hit_cap {
                partial = true;
                changed = true;
                partial_paths.insert(directory.to_string_lossy().to_string());
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            for dirent in entries {
                visited = visited.saturating_add(1);
                let name = crate::internal::worktree_io::protocol::dirent_os(&dirent.name);
                if name == std::ffi::OsStr::new(crate::utils::util::ROOT_DIR)
                    || name == std::ffi::OsStr::new(crate::utils::util::GIT_DIR)
                {
                    continue;
                }
                let relative = directory.join(name);
                let relative = validate_relative_path(&relative)?;
                let path = relative
                    .to_str()
                    .ok_or_else(|| ScanError::InvalidPath(relative.display().to_string()))?
                    .to_string();
                let absolute = self.scope.worktree_root.join(&relative);
                let is_dir = dirent.is_dir && !dirent.is_symlink;
                if crate::utils::util::check_gitignore_with_layers_as_dir(
                    &self.scope.worktree_root,
                    &absolute,
                    &layers,
                    is_dir,
                ) {
                    ignored.insert(path);
                    continue;
                }
                if is_dir {
                    pending.push(relative);
                    continue;
                }
                if !dirent.is_file && !dirent.is_symlink {
                    return Err(ScanError::Io(format!(
                        "unsupported worktree entry '{path}'"
                    )));
                }
                if tracked_names.contains(&path) {
                    continue;
                }
                untracked.insert(path.clone());
                changed = true;
                if self.capture_policy == CapturePolicy::FailClosed {
                    return Err(ScanError::UntrackedDisallowed);
                }
                if self.capture_policy != CapturePolicy::TrackedAndUntracked {
                    continue;
                }
                let stat = match self.stat(&root, &relative)? {
                    Some(stat) => stat,
                    None => {
                        partial = true;
                        partial_paths.insert(path);
                        continue;
                    }
                };
                if stat.len() > self.max_file_bytes {
                    partial = true;
                    partial_paths.insert(path);
                    continue;
                }
                let Some(bytes) = self.read_file(&root, &relative)? else {
                    partial = true;
                    partial_paths.insert(path);
                    continue;
                };
                let object_oid = blob_oid(&bytes);
                files.insert(
                    path,
                    ScannedFile {
                        bytes,
                        mode: mode_from_stat(&stat),
                        object_oid,
                        tracked: false,
                    },
                );
                changed = true;
            }
        }

        Ok(ScanResult {
            files,
            index_entries,
            raw_index,
            untracked,
            ignored,
            deleted_tracked,
            partial,
            partial_paths,
            changed,
        })
    }

    async fn capture_objects(
        &self,
        operation_id: &str,
        scan: &ScanResult,
    ) -> Result<SnapshotOutcome, SnapshotError> {
        if self.store.repo_id().trim().is_empty() {
            return Err(SnapshotError::Store(StoreError::Validation(
                "snapshot operation store repository id cannot be empty".to_string(),
            )));
        }
        let storage = self.store.storage();
        for file in scan.files.values() {
            put_blob(storage, &file.object_oid, &file.bytes)?;
        }
        let index_tree_oid = write_tree(
            storage,
            scan.index_entries.iter().map(|entry| {
                (
                    PathBuf::from(&entry.path),
                    tree_mode_from_index(entry.mode),
                    entry.object_oid,
                )
            }),
        )?;
        let raw_index_blob_oid = put_blob(storage, &blob_oid(&scan.raw_index), &scan.raw_index)?;
        let working_copy_tree_oid = write_tree(
            storage,
            scan.files.iter().map(|(path, file)| {
                (
                    PathBuf::from(path),
                    tree_mode_from_index(file.mode),
                    file.object_oid,
                )
            }),
        )?;
        let untracked_manifest = UntrackedManifest {
            schema_version: UNTRACKED_MANIFEST_SCHEMA_VERSION,
            capture_policy: self.capture_policy,
            untracked: scan.untracked.iter().cloned().collect(),
            ignored: scan.ignored.iter().cloned().collect(),
            deleted_tracked: scan.deleted_tracked.iter().cloned().collect(),
            partial_paths: scan.partial_paths.iter().cloned().collect(),
            files: scan
                .files
                .iter()
                .map(|(path, file)| FileManifestEntry {
                    path: path.clone(),
                    object_oid: file.object_oid,
                    mode: file.mode,
                })
                .collect(),
        };
        let untracked_bytes = serde_json::to_vec(&untracked_manifest)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        let untracked_manifest_oid =
            put_blob(storage, &blob_oid(&untracked_bytes), &untracked_bytes)?;
        let head = capture_head(self.store.db()).await?;

        let mut captures = Vec::new();
        let mut facet_partial = false;
        let facet_ctx = FacetCaptureCtx {
            repo_id: Some(self.store.repo_id().to_string()),
            workspace_id: Some(self.workspace_id()),
        };
        for name in self.registry.names() {
            match self.registry.capture(&name, &facet_ctx) {
                Ok(capture) => captures.push(capture),
                Err(error) => {
                    facet_partial = true;
                    tracing::warn!(facet = %name, error = %error, "workspace snapshot facet capture failed");
                }
            }
        }
        let sparse_facet_oid = captures
            .iter()
            .find(|capture| capture.facet.as_str() == "sparse")
            .and_then(|capture| capture.payload_oid);
        let sequencer_facet_oid = captures
            .iter()
            .find(|capture| capture.facet.as_str() == "sequencer")
            .and_then(|capture| capture.payload_oid);
        let facet_restore_policies = self.registry.policies(&captures);
        let complete = !scan.partial
            && !facet_partial
            && captures.len() == self.registry.len()
            && self.registry.is_fully_restorable(&captures);
        let snapshot = WorkspaceSnapshotV2 {
            schema_version: WORKSPACE_SNAPSHOT_SCHEMA_VERSION,
            workspace_id: self.workspace_id(),
            head: head.clone(),
            index_tree_oid,
            raw_index_blob_oid,
            working_copy_tree_oid,
            untracked_manifest_oid,
            sparse_facet_oid,
            sequencer_facet_oid,
            worktree_generation: self.next_generation().await?,
            capture_policy: self.capture_policy,
            completeness: if complete {
                Completeness::Full
            } else {
                Completeness::Partial
            },
            facet_restore_policies,
        };
        let snapshot_bytes = snapshot
            .to_canonical_bytes()
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        let snapshot_oid = put_blob(storage, &blob_oid(&snapshot_bytes), &snapshot_bytes)?;

        let refs_payload = RefsFacetPayload { head };
        let refs_bytes = serde_json::to_vec(&refs_payload)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        let refs_facet_oid = put_blob(storage, &blob_oid(&refs_bytes), &refs_bytes)?;
        let post_view = super::RepoViewV2 {
            schema_version: super::view::REPO_VIEW_SCHEMA_VERSION,
            repo_id: self.store.repo_id().to_string(),
            refs_facet_oid,
            workspaces: BTreeMap::from([(snapshot.workspace_id.clone(), snapshot_oid)]),
            change_roots: Vec::new(),
            extension_facets: BTreeMap::new(),
        };
        let post_view_oid = self.store.write_view_manifest(&post_view).await?;
        let parent_heads = self
            .store
            .read_heads(self.store.repo_id(), self.scope.scope.storage_key())
            .await?;
        let operation = OperationV2 {
            op_id: operation_id.to_string(),
            parent_op_ids: parent_heads.clone(),
            pre_view_oid: post_view_oid,
            post_view_oid,
            kind: OperationKind::ExternalSnapshot,
            status: if complete {
                OperationStatusV2::Success
            } else {
                OperationStatusV2::Partial
            },
            metadata: OperationMetaV2 {
                command_name: Some("workspace snapshot".to_string()),
                description: Some("automatic working-copy snapshot".to_string()),
                actor: Some("libra-snapshotter".to_string()),
                ..Default::default()
            },
            restores_op_id: None,
            reverts_op_id: None,
            predecessor_map_oid: None,
        };
        self.store.write_operation(&operation).await?;
        if let Err(error) = self
            .store
            .cas_update_op_heads(
                self.store.repo_id(),
                self.scope.scope.storage_key(),
                &parent_heads,
                &[operation_id.to_string()],
            )
            .await
        {
            self.record_diagnostic(operation_id, error.to_string())
                .await;
            return Err(SnapshotError::Store(error));
        }
        let pointer = WorkspaceStatePointer {
            last_op_id: operation_id.to_string(),
            last_snapshot_oid: snapshot_oid,
            generation: snapshot.worktree_generation,
        };
        if let Err(error) = pointer.save(&self.scope).await {
            self.record_diagnostic(operation_id, error.to_string())
                .await;
            return Err(SnapshotError::Pointer(error.to_string()));
        }
        Ok(SnapshotOutcome::Captured {
            operation_id: operation_id.to_string(),
            snapshot_oid,
            snapshot: Box::new(snapshot),
        })
    }

    async fn state_matches_last_snapshot(&self, scan: &ScanResult) -> Result<bool, SnapshotError> {
        let pointer = match WorkspaceStatePointer::load(&self.scope).await {
            Ok(pointer) => pointer,
            Err(error) if error_is_missing_pointer(&error) => return Ok(false),
            Err(error) => return Err(SnapshotError::Pointer(error.to_string())),
        };
        let snapshot_bytes = self
            .store
            .storage()
            .get(&pointer.last_snapshot_oid)
            .map_err(|error| SnapshotError::Object(error.to_string()))?;
        let snapshot = WorkspaceSnapshotV2::from_canonical_bytes(&snapshot_bytes)
            .map_err(|error| SnapshotError::Object(error.to_string()))?;
        if snapshot.capture_policy != self.capture_policy {
            return Ok(false);
        }
        let manifest_bytes = self
            .store
            .storage()
            .get(&snapshot.untracked_manifest_oid)
            .map_err(|error| SnapshotError::Object(error.to_string()))?;
        let manifest = serde_json::from_slice::<UntrackedManifest>(&manifest_bytes)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        let files = scan
            .files
            .iter()
            .map(|(path, file)| FileManifestEntry {
                path: path.clone(),
                object_oid: file.object_oid,
                mode: file.mode,
            })
            .collect::<Vec<_>>();
        Ok(manifest.capture_policy == self.capture_policy
            && manifest.untracked == scan.untracked.iter().cloned().collect::<Vec<_>>()
            && manifest.deleted_tracked == scan.deleted_tracked.iter().cloned().collect::<Vec<_>>()
            && manifest.partial_paths == scan.partial_paths.iter().cloned().collect::<Vec<_>>()
            && manifest.files == files)
    }

    async fn next_generation(&self) -> Result<u64, SnapshotError> {
        match WorkspaceStatePointer::load(&self.scope).await {
            Ok(pointer) => Ok(pointer.generation.saturating_add(1)),
            Err(error) if error_is_missing_pointer(&error) => Ok(1),
            Err(error) => Err(SnapshotError::Pointer(error.to_string())),
        }
    }

    async fn record_diagnostic(&self, operation_id: &str, message: String) {
        let _ = self
            .store
            .append_journal(&super::JournalEntry {
                journal_id: format!("snapshot-{operation_id}"),
                op_id: operation_id.to_string(),
                phase: super::JournalPhase::Reserved,
                pre_view_oid: None,
                target_view_oid: None,
                owner: "workspace-snapshotter".to_string(),
                updated_at: chrono::Utc::now().timestamp_millis(),
                recovery_payload: Some(message),
            })
            .await;
    }

    fn workspace_id(&self) -> String {
        match &self.scope.scope {
            crate::internal::worktree_scope::WorktreeScope::Main => "main".to_string(),
            crate::internal::worktree_scope::WorktreeScope::Linked(id) => id.clone(),
        }
    }

    fn stat(&self, root: &[u8], relative: &Path) -> Result<Option<CapturedStat>, ScanError> {
        let events = self
            .io
            .submit(
                IoRequest::SymlinkMetadata {
                    path: path_to_bytes(relative),
                    root: root.to_vec(),
                },
                path_to_bytes(relative),
                IO_TIMEOUT,
            )
            .map_err(scan_executor_error)?;
        for event in events {
            if let IoEvent::DoneStat { result } = event {
                return match unwrap_wire(result) {
                    Ok(stat) => Ok(Some(stat)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(ScanError::Io(error.to_string())),
                };
            }
        }
        Err(ScanError::Io(
            "worktree stat returned no terminal event".to_string(),
        ))
    }

    fn read_file(&self, root: &[u8], relative: &Path) -> Result<Option<Vec<u8>>, ScanError> {
        let events = self
            .io
            .submit(
                IoRequest::ReadFile {
                    path: path_to_bytes(relative),
                    root: root.to_vec(),
                    byte_limit: self.max_file_bytes,
                },
                path_to_bytes(relative),
                IO_TIMEOUT,
            )
            .map_err(scan_executor_error)?;
        for event in events {
            if let IoEvent::DoneObjectBlob { status, bytes } = event {
                return match status {
                    ObjectBlobStatus::Ok => Ok(bytes),
                    ObjectBlobStatus::Missing => Ok(None),
                    ObjectBlobStatus::TooLarge => Ok(None),
                    other => Err(ScanError::Io(format!(
                        "bounded file read failed: {other:?}"
                    ))),
                };
            }
        }
        Err(ScanError::Io(
            "worktree file read returned no terminal event".to_string(),
        ))
    }

    fn read_dir(&self, root: &[u8], relative: &Path) -> Result<(Vec<Dirent>, bool), ScanError> {
        let path = self.scope.worktree_root.join(relative);
        let relative = relative_worktree_path(root, &path, true)
            .map_err(|error| ScanError::InvalidPath(error.to_string()))?;
        let events = self
            .io
            .submit(
                IoRequest::ReadDir {
                    path: path_to_bytes(&relative),
                    root: root.to_vec(),
                    remaining: self.max_files,
                    checkpoint_every: 32,
                },
                path_to_bytes(&relative),
                IO_TIMEOUT,
            )
            .map_err(scan_executor_error)?;
        let mut entries = Vec::new();
        for event in events {
            match event {
                IoEvent::RecordDirent(dirent) => entries.push(dirent),
                IoEvent::RecordError { kind, raw_os } => {
                    return Err(ScanError::Io(io_from_wire(kind, raw_os).to_string()));
                }
                IoEvent::DoneReadDir { listing } => {
                    let hit_cap = listing.hit_cap;
                    if let Some((kind, raw_os)) = listing.error_kinds.first().copied() {
                        return Err(ScanError::Io(io_from_wire(kind, raw_os).to_string()));
                    }
                    return Ok((entries, hit_cap));
                }
                _ => {}
            }
        }
        Ok((entries, false))
    }
}

#[derive(Debug, Serialize)]
struct RefsFacetPayload {
    head: HeadState,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct FileManifestEntry {
    path: String,
    object_oid: ObjectHash,
    mode: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct UntrackedManifest {
    schema_version: u32,
    capture_policy: CapturePolicy,
    untracked: Vec<String>,
    ignored: Vec<String>,
    deleted_tracked: Vec<String>,
    partial_paths: Vec<String>,
    files: Vec<FileManifestEntry>,
}

async fn capture_head<C: sea_orm::ConnectionTrait>(db: &C) -> Result<HeadState, SnapshotError> {
    match Head::current_result_with_conn(db).await {
        Ok(Head::Branch(name)) => Ok(HeadState::Symbolic {
            reference: if name.starts_with("refs/") {
                name
            } else {
                format!("refs/heads/{name}")
            },
        }),
        Ok(Head::Detached(oid)) => Ok(HeadState::Detached { oid }),
        Err(error) if error.to_string().contains("HEAD reference is missing") => {
            Ok(HeadState::Symbolic {
                reference: "refs/heads/main".to_string(),
            })
        }
        Err(error) => Err(SnapshotError::Head(error.to_string())),
    }
}

fn put_blob(
    storage: &ClientStorage,
    oid: &ObjectHash,
    bytes: &[u8],
) -> Result<ObjectHash, SnapshotError> {
    storage
        .put(oid, bytes, ObjectType::Blob)
        .map_err(|error| SnapshotError::Object(error.to_string()))?;
    Ok(*oid)
}

fn blob_oid(bytes: &[u8]) -> ObjectHash {
    ObjectHash::from_type_and_data(ObjectType::Blob, bytes)
}

fn write_tree(
    storage: &ClientStorage,
    leaves: impl IntoIterator<Item = (PathBuf, TreeItemMode, ObjectHash)>,
) -> Result<ObjectHash, SnapshotError> {
    let mut entries: BTreeMap<PathBuf, Vec<TreeItem>> = BTreeMap::new();
    for (path, mode, oid) in leaves {
        validate_relative_path(&path).map_err(|error| SnapshotError::Object(error.to_string()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                SnapshotError::Object(format!("tree path is not UTF-8: {}", path.display()))
            })?;
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        ensure_tree_ancestors(&mut entries, parent);
        entries
            .entry(parent.to_path_buf())
            .or_default()
            .push(TreeItem::new(mode, oid, name.to_string()));
    }
    write_tree_recursively(storage, Path::new(""), &mut entries)
}

fn ensure_tree_ancestors(entries: &mut BTreeMap<PathBuf, Vec<TreeItem>>, path: &Path) {
    let mut current = Some(path);
    while let Some(current_path) = current {
        if current_path.as_os_str().is_empty() {
            break;
        }
        entries.entry(current_path.to_path_buf()).or_default();
        current = current_path.parent();
    }
}

fn write_tree_recursively(
    storage: &ClientStorage,
    current: &Path,
    entries: &mut BTreeMap<PathBuf, Vec<TreeItem>>,
) -> Result<ObjectHash, SnapshotError> {
    let mut items = entries.remove(current).unwrap_or_default();
    let mut children: Vec<PathBuf> = entries
        .keys()
        .filter(|path| path.parent() == Some(current))
        .cloned()
        .collect();
    children.sort();
    for child in children {
        let name = child
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                SnapshotError::Object(format!("tree path is not UTF-8: {}", child.display()))
            })?;
        let oid = write_tree_recursively(storage, &child, entries)?;
        items.push(TreeItem::new(TreeItemMode::Tree, oid, name.to_string()));
    }
    sort_tree_items_for_git(&mut items);
    let tree = if items.is_empty() {
        Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .map_err(|error| SnapshotError::Object(error.to_string()))?
    } else {
        Tree::from_tree_items(items).map_err(|error| SnapshotError::Object(error.to_string()))?
    };
    let data = tree
        .to_data()
        .map_err(|error| SnapshotError::Object(error.to_string()))?;
    storage
        .put(&tree.id, &data, ObjectType::Tree)
        .map_err(|error| SnapshotError::Object(error.to_string()))?;
    Ok(tree.id)
}

fn collect_tree_files(
    storage: &ClientStorage,
    tree_oid: &ObjectHash,
    prefix: &Path,
    output: &mut Vec<(PathBuf, u32, Vec<u8>)>,
) -> Result<(), String> {
    let bytes = storage.get(tree_oid).map_err(|error| error.to_string())?;
    let tree = Tree::from_bytes(&bytes, *tree_oid).map_err(|error| error.to_string())?;
    for item in &tree.tree_items {
        let path = prefix.join(&item.name);
        if item.mode == TreeItemMode::Tree {
            collect_tree_files(storage, &item.id, &path, output)?;
        } else {
            let bytes = storage.get(&item.id).map_err(|error| error.to_string())?;
            output.push((path, tree_mode_to_raw(item.mode), bytes));
        }
    }
    Ok(())
}

fn validate_tree_path(path: &Path) -> Result<PathBuf, String> {
    validate_relative_path(path).map_err(|error| error.to_string())
}

fn validate_relative_path(path: &Path) -> Result<PathBuf, ScanError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ScanError::InvalidPath(path.display().to_string()));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ScanError::InvalidPath(path.display().to_string()));
    }
    Ok(path.to_path_buf())
}

fn mode_from_stat(stat: &CapturedStat) -> u32 {
    if stat.is_symlink() {
        0o120000
    } else if stat.mode & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

fn normalize_index_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o120000 => 0o120000,
        _ if mode & 0o111 != 0 => 0o100755,
        _ => 0o100644,
    }
}

fn tree_mode_from_index(mode: u32) -> TreeItemMode {
    match normalize_index_mode(mode) {
        0o120000 => TreeItemMode::Link,
        0o100755 => TreeItemMode::BlobExecutable,
        _ => TreeItemMode::Blob,
    }
}

fn tree_mode_to_raw(mode: TreeItemMode) -> u32 {
    match mode {
        TreeItemMode::Link => 0o120000,
        TreeItemMode::BlobExecutable => 0o100755,
        _ => 0o100644,
    }
}

fn stat_matches_index(
    entry: &git_internal::internal::index::IndexEntry,
    stat: &CapturedStat,
) -> bool {
    entry.size as u64 == stat.len()
        && entry.ctime.to_string() == format!("{}:{}", stat.ctime_sec, stat.ctime_nsec)
        && entry.mtime.to_string() == format!("{}:{}", stat.mtime_sec, stat.mtime_nsec)
        && normalize_index_mode(entry.mode) == mode_from_stat(stat)
}

fn scan_executor_error(error: ExecutorError) -> ScanError {
    if matches!(error, ExecutorError::DeadlineExpired) {
        ScanError::Timeout
    } else {
        ScanError::Io(error.to_string())
    }
}

fn error_is_missing_pointer(error: &super::PointerError) -> bool {
    matches!(error, super::PointerError::Missing(_))
}

struct WorktreeLock {
    _file: fs::File,
}

impl WorktreeLock {
    fn acquire(scope: &PinnedRequestScope) -> Result<Self, String> {
        let path = scope.gitdir.join(SNAPSHOT_LOCK_NAME);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock is applied to the owned lock descriptor and the
            // descriptor remains alive until WorktreeLock is dropped.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
        }
        Ok(Self { _file: file })
    }
}

impl Drop for WorktreeLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor belongs to this guard and is still open.
            unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_to_git_tree_modes() {
        assert_eq!(tree_mode_from_index(0o100644), TreeItemMode::Blob);
        assert_eq!(tree_mode_from_index(0o100755), TreeItemMode::BlobExecutable);
        assert_eq!(tree_mode_from_index(0o120000), TreeItemMode::Link);
    }

    #[test]
    fn empty_capture_tree_is_canonical_empty_tree() {
        let dir = tempfile::tempdir().expect("temporary object store");
        let storage = ClientStorage::init_local(dir.path().join("objects"));
        let oid = write_tree(&storage, std::iter::empty()).expect("write empty tree");
        assert_eq!(oid, ObjectHash::from_type_and_data(ObjectType::Tree, &[]));
    }
}
