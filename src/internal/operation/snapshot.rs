//! Bounded working-copy capture into a v2 workspace snapshot.
//!
//! Capture is a snapshot operation, not a commit: it writes content-addressed
//! blobs and a `WorkspaceSnapshotV2` manifest, but never creates a Commit OID
//! or a Change ID. Directory enumeration is delegated to the read-only
//! `WorktreeIo` executor; policy and byte/file budgets are enforced here.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    internal::{
        operation::view::{CapturePolicy, Completeness, HeadState, WorkspaceSnapshotV2},
        worktree_io::{
            default_worktree_io,
            executor::ExecutorError,
            protocol::{IoEvent, IoRequest, bytes_to_path, path_to_bytes},
        },
    },
    utils::client_storage::ClientStorage,
};

const MAX_FILES: usize = 10_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const SCAN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("snapshot worktree I/O error: {0}")]
    WorktreeIo(String),
    #[error("snapshot object storage error: {0}")]
    Storage(String),
    #[error("snapshot manifest error: {0}")]
    Manifest(String),
    #[error("snapshot path is not a safe relative path: {0}")]
    UnsafePath(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCapture {
    pub manifest: WorkspaceSnapshotV2,
    pub manifest_oid: ObjectHash,
    pub file_count: usize,
    pub byte_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEntry {
    path: Vec<u8>,
    oid: String,
    len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileManifest {
    schema_version: u32,
    files: Vec<FileEntry>,
}

pub struct WorkspaceSnapshotter {
    storage: ClientStorage,
    max_files: usize,
    max_bytes: u64,
}

impl WorkspaceSnapshotter {
    pub fn new(storage: ClientStorage) -> Self {
        Self {
            storage,
            max_files: MAX_FILES,
            max_bytes: MAX_BYTES,
        }
    }

    #[cfg(test)]
    fn with_limits(storage: ClientStorage, max_files: usize, max_bytes: u64) -> Self {
        Self {
            storage,
            max_files,
            max_bytes,
        }
    }

    /// Scan and persist one workspace snapshot. An empty root with no index
    /// has no observable state and returns `Ok(None)`, so callers do not emit
    /// a no-op operation.
    pub fn capture(
        &self,
        root: &Path,
        workspace_id: impl Into<String>,
        head: HeadState,
        capture_policy: CapturePolicy,
        generation: u64,
    ) -> Result<Option<SnapshotCapture>, SnapshotError> {
        let root = root.canonicalize()?;
        let (paths, mut partial) = self.scan_working_copy(&root)?;
        let had_files = !paths.is_empty();
        let mut entries = Vec::new();
        let mut byte_count = 0;
        for path in paths {
            if entries.len() >= self.max_files || byte_count >= self.max_bytes {
                partial = true;
                break;
            }
            let data = fs::read(root.join(&path))?;
            let len = u64::try_from(data.len()).map_err(|_| io::Error::other("file too large"))?;
            if byte_count.saturating_add(len) > self.max_bytes {
                partial = true;
                break;
            }
            byte_count += len;
            let oid = ObjectHash::from_type_and_data(ObjectType::Blob, &data);
            self.storage
                .put(&oid, &data, ObjectType::Blob)
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            entries.push(FileEntry {
                path: path_to_bytes(&path),
                oid: oid.to_string(),
                len,
            });
        }

        let index_data = fs::read(root.join(".git/index")).unwrap_or_default();
        if entries.is_empty() && index_data.is_empty() && !had_files {
            return Ok(None);
        }
        let index_oid = self.put_blob(&index_data)?;
        let files = FileManifest {
            schema_version: 1,
            files: entries,
        };
        let file_manifest_data = serde_json::to_vec(&files)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        let untracked_manifest_oid = self.put_blob(&file_manifest_data)?;
        let working_copy_tree_oid = self.put_blob(&file_manifest_data)?;
        let mut manifest = WorkspaceSnapshotV2::new(
            workspace_id.into(),
            head,
            index_oid,
            index_oid,
            working_copy_tree_oid,
            untracked_manifest_oid,
        )
        .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        manifest.capture_policy = capture_policy;
        manifest.worktree_generation = generation;
        manifest.completeness = if partial || capture_policy == CapturePolicy::FailClosed {
            Completeness::Partial
        } else {
            Completeness::Full
        };
        let manifest_oid = manifest
            .write_manifest(&self.storage)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        Ok(Some(SnapshotCapture {
            manifest,
            manifest_oid,
            file_count: files.files.len(),
            byte_count,
        }))
    }

    /// Restore file content from a snapshot manifest. This intentionally does
    /// not alter refs, index state, or create a Git commit.
    pub fn restore(
        &self,
        root: &Path,
        snapshot: &WorkspaceSnapshotV2,
    ) -> Result<(), SnapshotError> {
        let bytes = self
            .storage
            .get(&snapshot.untracked_manifest_oid)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        let file_manifest: FileManifest = serde_json::from_slice(&bytes)
            .map_err(|error| SnapshotError::Manifest(error.to_string()))?;
        for entry in file_manifest.files {
            let relative = bytes_to_path(&entry.path);
            ensure_safe_relative(&relative)?;
            let oid = entry
                .oid
                .parse()
                .map_err(|_| SnapshotError::Manifest("invalid file object id".to_string()))?;
            let data = self
                .storage
                .get(&oid)
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            if u64::try_from(data.len()).unwrap_or(u64::MAX) != entry.len {
                return Err(SnapshotError::Manifest(format!(
                    "file length changed for {}",
                    relative.display()
                )));
            }
            let destination = root.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, data)?;
        }
        Ok(())
    }

    fn put_blob(&self, data: &[u8]) -> Result<ObjectHash, SnapshotError> {
        let oid = ObjectHash::from_type_and_data(ObjectType::Blob, data);
        self.storage
            .put(&oid, data, ObjectType::Blob)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        Ok(oid)
    }

    fn scan_working_copy(&self, root: &Path) -> Result<(Vec<PathBuf>, bool), SnapshotError> {
        let io = default_worktree_io();
        let mut queue = vec![PathBuf::new()];
        let mut files = Vec::new();
        let mut partial = false;
        while let Some(relative) = queue.pop() {
            let events = io
                .submit(
                    IoRequest::ReadDir {
                        path: path_to_bytes(&relative),
                        root: path_to_bytes(root),
                        remaining: self.max_files.saturating_sub(files.len()).max(1),
                        checkpoint_every: 128,
                    },
                    path_to_bytes(&relative),
                    SCAN_TIMEOUT,
                )
                .map_err(|error: ExecutorError| SnapshotError::WorktreeIo(error.to_string()))?;
            let mut scanned_entries = Vec::new();
            let mut listing = None;
            for event in events {
                match event {
                    IoEvent::RecordDirent(entry) => scanned_entries.push(entry),
                    IoEvent::DoneReadDir { listing: result } => listing = Some(result),
                    _ => {}
                }
            }
            let mut listing = listing.ok_or_else(|| {
                SnapshotError::WorktreeIo("worker returned no directory listing".to_string())
            })?;
            listing.entries = scanned_entries;
            partial |= listing.hit_cap || listing.timed_out || !listing.error_kinds.is_empty();
            for entry in listing.entries {
                let name = bytes_to_path(&entry.name);
                if name == Path::new(".git") || name == Path::new(".libra") {
                    continue;
                }
                let child = relative.join(name);
                if entry.is_dir {
                    queue.push(child);
                } else if entry.is_file && !entry.is_symlink {
                    ensure_safe_relative(&child)?;
                    files.push(child);
                    if files.len() >= self.max_files {
                        partial = true;
                        break;
                    }
                }
            }
        }
        files.sort();
        Ok((files, partial))
    }
}

fn ensure_safe_relative(path: &Path) -> Result<(), SnapshotError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(SnapshotError::UnsafePath(path.display().to_string()));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SnapshotError::UnsafePath(path.display().to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_worktree_does_not_publish() {
        let root = tempfile::tempdir().expect("root");
        let objects = tempfile::tempdir().expect("objects");
        let snapshotter =
            WorkspaceSnapshotter::new(ClientStorage::init_local(objects.path().to_path_buf()));
        assert!(
            snapshotter
                .capture(
                    root.path(),
                    "ws",
                    HeadState::Unborn {
                        ref_name: "main".to_string()
                    },
                    CapturePolicy::TrackedAndUntracked,
                    0
                )
                .expect("capture")
                .is_none()
        );
    }

    #[test]
    fn capture_and_restore_roundtrip() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("nested")).expect("mkdir");
        fs::write(root.path().join("tracked.txt"), b"before").expect("write");
        fs::write(root.path().join("nested/untracked.txt"), b"untracked").expect("write");
        let objects = tempfile::tempdir().expect("objects");
        let snapshotter =
            WorkspaceSnapshotter::new(ClientStorage::init_local(objects.path().to_path_buf()));
        let capture = snapshotter
            .capture(
                root.path(),
                "ws",
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
        assert_eq!(capture.file_count, 2);
    }

    #[test]
    fn capacity_marks_partial() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("one"), b"1234").expect("write");
        let objects = tempfile::tempdir().expect("objects");
        let snapshotter = WorkspaceSnapshotter::with_limits(
            ClientStorage::init_local(objects.path().to_path_buf()),
            1,
            2,
        );
        let capture = snapshotter
            .capture(
                root.path(),
                "ws",
                HeadState::Unborn {
                    ref_name: "main".to_string(),
                },
                CapturePolicy::TrackedAndUntracked,
                1,
            )
            .expect("capture")
            .expect("snapshot");
        assert_eq!(capture.manifest.completeness, Completeness::Partial);
    }
}
