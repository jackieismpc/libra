//! Storage trait and implementations for Git object storage.
//!
mod load_cost;
pub mod local;
pub mod remote;
pub mod tiered;

use async_trait::async_trait;
use git_internal::{errors::GitError, hash::ObjectHash, internal::object::types::ObjectType};

/// Abstract storage backend interface for Git objects
#[async_trait]
pub trait Storage: Send + Sync {
    /// Retrieve an object by its hash
    /// Returns the raw decompressed data and the object type.
    /// If the object is not found, returns `GitError::ObjectNotFound`.
    async fn get(&self, hash: &ObjectHash) -> Result<(Vec<u8>, ObjectType), GitError>;

    /// Retrieve an object while enforcing a conservative maximum load cost
    /// before materializing it. Backends that cannot enforce the bound fail
    /// closed instead of downloading or decoding an unbounded payload.
    async fn get_with_limit(
        &self,
        _hash: &ObjectHash,
        limit: u64,
    ) -> Result<(Vec<u8>, ObjectType), GitError> {
        Err(GitError::InvalidObjectInfo(format!(
            "storage backend cannot enforce a {limit}-byte bounded object read"
        )))
    }

    /// Store an object
    /// Takes the object hash, raw decompressed data, and object type.
    /// Returns the storage path or identifier.
    /// This operation should be idempotent.
    async fn put(
        &self,
        hash: &ObjectHash,
        data: &[u8],
        obj_type: ObjectType,
    ) -> Result<String, GitError>;

    /// Check if an object exists
    /// Returns true if the object exists in storage.
    async fn exist(&self, hash: &ObjectHash) -> bool;

    /// Return a conservative upper bound for bytes required to load the object
    /// without materializing it. Packed deltas include their instruction and
    /// base-chain reconstruction cost. `None` means this backend cannot provide
    /// a bounded local answer.
    async fn object_size(&self, _hash: &ObjectHash) -> Result<Option<u64>, GitError> {
        Ok(None)
    }

    /// Batch form of [`Self::object_size`], preserving input order.
    async fn object_sizes(&self, hashes: &[ObjectHash]) -> Result<Vec<Option<u64>>, GitError> {
        let mut sizes = Vec::with_capacity(hashes.len());
        for hash in hashes {
            sizes.push(self.object_size(hash).await?);
        }
        Ok(sizes)
    }

    /// Batch bounded-load preflight that stops once the sum of discovered load
    /// costs exceeds `aggregate_limit`. Implementations should avoid probing
    /// later payloads after the limit is crossed.
    async fn object_sizes_with_total_limit(
        &self,
        hashes: &[ObjectHash],
        aggregate_limit: u64,
    ) -> Result<Vec<Option<u64>>, GitError> {
        let mut sizes = Vec::with_capacity(hashes.len());
        let mut total = 0u64;
        for hash in hashes {
            let size = self.object_size(hash).await?;
            if let Some(size) = size {
                total = total
                    .checked_add(crate::utils::preview_object::charged_bytes(size))
                    .ok_or_else(|| {
                        GitError::InvalidObjectInfo(
                            "preview aggregate cache load cost exceeds u64".to_string(),
                        )
                    })?;
                if total > aggregate_limit {
                    return Err(GitError::InvalidObjectInfo(format!(
                        "preview aggregate cache load cost exceeds {aggregate_limit} bytes"
                    )));
                }
            }
            sizes.push(size);
        }
        Ok(sizes)
    }

    /// Search for objects by hash prefix
    /// Returns a list of object hashes that match the given prefix.
    /// Note: Performance may vary significantly between backends (fast locally, potentially slow remotely).
    async fn search(&self, prefix: &str) -> Vec<ObjectHash>;

    /// Batch existence check — returns one `bool` per input hash, in the same
    /// order (`lore.md` §0.6). Used as a dedup pre-check (e.g. "which of these
    /// objects does the remote already have before I upload?").
    ///
    /// The default runs `exist` sequentially: a correctness fallback with no
    /// speedup. The value is in backend overrides that probe in parallel —
    /// [`remote::RemoteStorage`] fires bounded-concurrency HEAD requests and
    /// [`tiered::TieredStorage`] answers local hits without any round trip and
    /// batches only the remote misses.
    async fn exist_batch(&self, hashes: &[ObjectHash]) -> Vec<bool> {
        let mut results = Vec::with_capacity(hashes.len());
        for hash in hashes {
            results.push(self.exist(hash).await);
        }
        results
    }

    /// Attempt to repair a missing or corrupted local object by re-fetching it
    /// from a durable tier, verifying that the fetched bytes hash to `hash`, and
    /// writing the object into the local store (`libra fsck --heal`, lore.md §0.4).
    ///
    /// # Returns
    /// * `Ok(true)` — the object was fetched, verified, and healed.
    /// * `Ok(false)` — this backend has no durable tier to heal from, or the
    ///   object is absent from that tier (unrecoverable). Backends MUST NOT
    ///   fabricate objects; only a payload that verifies against `hash` may be
    ///   written.
    ///
    /// The default implementation cannot heal (backends without a paired durable
    /// tier — local-only, remote-only, publish — return `Ok(false)`). Only
    /// [`tiered::TieredStorage`] overrides this.
    async fn heal(&self, _hash: &ObjectHash) -> Result<bool, GitError> {
        Ok(false)
    }

    /// Error-aware existence probe (lore.md 2.9): distinguishes a confirmed
    /// ABSENCE (`Ok(false)`) from a probe FAILURE (`Err` — outage, bad
    /// credentials). The plain `exist` collapses both into `false`, which is
    /// fine for read fallbacks but must never gate a deletion.
    async fn exist_checked(&self, hash: &ObjectHash) -> Result<bool, GitError> {
        Ok(self.exist(hash).await)
    }

    /// Evict verified-durable large objects from the LOCAL tier until under
    /// budget (lore.md 2.9). `Ok(None)` = not a tiered store (nothing
    /// evictable). Deletion is gated on a per-object error-aware durability
    /// probe run immediately before each unlink — an object is never deleted
    /// on a probe ERROR, and a wholly unreachable tier aborts the run.
    async fn evict_local(&self, _request: EvictRequest) -> Result<Option<EvictReport>, GitError> {
        Ok(None)
    }

    /// Physically delete an object's PAYLOAD (lore.md 2.5 obliteration). The
    /// default is a no-op success (a local-only loose store deletes the file
    /// itself in the obliteration driver). Tiered stores override this to purge
    /// the durable-tier blob AND the in-memory LRU entry. Idempotent: deleting
    /// an already-absent payload succeeds.
    async fn delete_payload(&self, _hash: &ObjectHash) -> Result<(), GitError> {
        Ok(())
    }
}

/// Parameters for [`Storage::evict_local`].
#[derive(Debug, Clone)]
pub struct EvictRequest {
    /// Target budget for the local large-object cache (uncompressed bytes —
    /// the same conservative accounting as the in-process LRU).
    pub budget_bytes: u64,
    /// Skip objects materialized within this many seconds (mtime floor).
    pub min_age_secs: u64,
    /// Report what WOULD be evicted (probes still run); delete nothing.
    pub dry_run: bool,
}

/// Outcome of [`Storage::evict_local`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EvictReport {
    /// Loose objects scanned.
    pub scanned: usize,
    /// Objects at/over the large threshold (eviction candidates before
    /// age/budget filters).
    pub candidate_count: usize,
    /// Their summed uncompressed bytes.
    pub candidate_bytes: u64,
    /// Candidates whose durability probe confirmed presence.
    pub verified: usize,
    /// Objects actually evicted (0 under dry-run).
    pub evicted: usize,
    /// Uncompressed bytes reclaimed (would-be reclaimed under dry-run).
    pub reclaimed_bytes: u64,
    /// Skipped: the durable tier CONFIRMED the object absent (push/backup to
    /// make it durable).
    pub skipped_absent: usize,
    /// Skipped: the durability probe ERRORED (outage ≠ absence; never
    /// deleted on error).
    pub skipped_probe_error: usize,
    /// Skipped: younger than the min-age floor.
    pub skipped_recent: usize,
    /// Evicted (or would-be) objects, capped: (oid, uncompressed bytes).
    pub evicted_objects: Vec<(String, u64)>,
}
