//! Consolidated application context passed through the UI and background services.
//!
//! Replaces the growing list of individual `Arc<T>` parameters that were previously
//! threaded through `build_settings_window()` and `open_settings_if_needed()`.

use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

use crate::api_client::ImmichApiClient;
use crate::config::{Config, WatchPathEntry};
use crate::library::state::LibraryState;
use crate::library::thumbnail_cache::ThumbnailCache;
use crate::monitor::MonitorHandle;
use crate::queue_manager::QueueManager;
use crate::state_manager::AppState;
use crate::sync_index::ShardedSyncIndex;

/// Refresh the authoritative server-checksum set on [`AppContext`].
///
/// Enumerates the display galleries *and* the backup watch folders, resolves a
/// checksum for every enumerated file (cheap `sync_index.fresh_checksum` first,
/// falling back to a blocking `compute_sha1_chunked`), then asks the server —
/// via `bulk_existing_asset_ids` — which of those checksums it already has. The
/// resulting set is stored in `ctx.server_checksums` so the tile-badge
/// classifier can read it synchronously.
///
/// The hashing runs on a Tokio blocking thread (identical pattern to the backup
/// page's counter compute), so this is safe to `spawn_local` from the UI thread.
/// On an empty/failed probe the previous set is left untouched (we never clear
/// a good set with an empty one — the classifier's fallback already handles the
/// "unpopulated" case, and clobbering a warm set would flip badges to slash).
pub async fn refresh_server_checksums(ctx: Arc<AppContext>) {
    // 1. Enumerate every local media file the badge could apply to: the display
    //    galleries (Photos timeline) plus the backup watch folders. Dedup by
    //    path so a file in both lists is hashed once.
    let mut locals = crate::library::local_source::enumerate_galleries(ctx.clone()).await;
    locals.extend(crate::library::local_source::enumerate_local(ctx.clone()).await);

    let mut seen_paths: HashSet<std::path::PathBuf> = HashSet::new();
    let paths: Vec<std::path::PathBuf> = locals
        .into_iter()
        .map(|a| a.path)
        .filter(|p| seen_paths.insert(p.clone()))
        .collect();
    if paths.is_empty() {
        return;
    }

    // 2. Resolve a checksum per file off the UI thread. `fresh_checksum` is a
    //    free cache hit when size+mtime are unchanged; otherwise hash on the
    //    blocking pool.
    let sync_index = ctx.sync_index.clone();
    let checksums: Vec<String> = tokio::task::spawn_blocking(move || {
        let mut set: HashSet<String> = HashSet::new();
        for path in paths {
            let checksum = sync_index.fresh_checksum(&path).or_else(|| {
                crate::monitor::compute_sha1_chunked(&path.to_string_lossy()).ok()
            });
            if let Some(checksum) = checksum {
                set.insert(checksum);
            }
        }
        set.into_iter().collect()
    })
    .await
    .unwrap_or_default();
    if checksums.is_empty() {
        return;
    }

    // 3. Ask the server which of those checksums it already holds. The returned
    //    map's keys are exactly the checksums the server has — that IS the
    //    truthful "backed up" set for these files.
    let present = ctx.api_client.bulk_existing_asset_ids(&checksums).await;
    if present.is_empty() {
        // Either nothing is backed up yet, or the probe failed. Don't clobber a
        // previously good set with an empty one — leave the last known state so
        // badges stay as accurate as they were. (First-ever run just keeps the
        // set empty, and the classifier falls back to the index.)
        return;
    }

    let fresh: HashSet<String> = present.into_keys().collect();
    *ctx.server_checksums.write() = fresh;
}

/// TTL'd set of paths Mimick itself just modified, used to suppress the
/// filesystem events those modifications cause.
#[derive(Default)]
pub struct RecentSelfPaths {
    /// Mutex-wrapped map of self-modified paths to the instant they were modified.
    inner: Mutex<HashMap<String, Instant>>,
}

impl RecentSelfPaths {
    /// Time-to-live after which self-modified paths are expired.
    const TTL: Duration = Duration::from_secs(60);

    /// Record a path as being modified by the application itself.
    pub fn mark(&self, path: &str) {
        let mut map = self.inner.lock();
        map.retain(|_, t| t.elapsed() < Self::TTL);
        map.insert(path.to_string(), Instant::now());
    }

    /// Check if a path is within its TTL and consume the record if present.
    pub fn consume(&self, path: &str) -> bool {
        let mut map = self.inner.lock();
        map.retain(|_, t| t.elapsed() < Self::TTL);
        map.remove(path).is_some()
    }
}

/// Per-album reconcile lock. Prevents the periodic poller and a manual sync
/// click from racing each other into duplicate downloads / duplicate trash.
#[derive(Default)]
pub struct ReconcileLocks {
    /// Track of album IDs currently undergoing reconciliation.
    inner: Mutex<std::collections::HashSet<String>>,
}

impl ReconcileLocks {
    /// Attempt to acquire a lock for the given album, returning a guard on success.
    pub fn try_acquire(self: &Arc<Self>, album_id: String) -> Option<ReconcileGuard> {
        let mut set = self.inner.lock();
        if !set.insert(album_id.clone()) {
            return None;
        }
        Some(ReconcileGuard {
            locks: self.clone(),
            album_id,
        })
    }
}

/// Active lock guard that frees the album lock when dropped.
pub struct ReconcileGuard {
    /// Reference back to the parent locks list.
    locks: Arc<ReconcileLocks>,
    /// Unique identifier of the locked album.
    album_id: String,
}

impl Drop for ReconcileGuard {
    fn drop(&mut self) {
        self.locks.inner.lock().remove(&self.album_id);
    }
}

/// Two-tick deletion confirmation. For mass deletions, require the same
/// asset to be missing across two consecutive reconciler observations
/// before trashing — defends against transient server-side stale reads.
#[derive(Default)]
pub struct PendingDeletions {
    /// Number of observations recorded per asset ID.
    inner: Mutex<HashMap<String, u32>>,
}

impl PendingDeletions {
    /// Number of consecutive confirmations required to execute trashing.
    pub const REQUIRED_CONFIRMATIONS: u32 = 2;

    /// Increment the verification count for an asset and return if target threshold is reached.
    pub fn confirm(&self, key: &str) -> bool {
        let mut map = self.inner.lock();
        let entry = map.entry(key.to_string()).or_insert(0);
        *entry += 1;
        *entry >= Self::REQUIRED_CONFIRMATIONS
    }

    /// Clear deletion confirmation history for a specific asset ID.
    pub fn clear(&self, key: &str) {
        self.inner.lock().remove(key);
    }
}

/// Shared application context holding all dependency handles that UI and background
/// tasks need. Wrapped in `Arc` at construction time so it can be cloned cheaply.
pub struct AppContext {
    /// Thread-safe configuration data.
    pub config: Arc<RwLock<Config>>,
    /// Thread-safe application state metrics.
    pub state: Arc<Mutex<AppState>>,
    /// Shared Immich API client handle.
    pub api_client: Arc<ImmichApiClient>,
    /// System upload and download worker queue coordinator.
    pub queue_manager: Arc<QueueManager>,
    /// Native file monitor handle.
    pub monitor_handle: Arc<MonitorHandle>,
    /// Concurrent database state check index.
    pub sync_index: Arc<ShardedSyncIndex>,
    /// Thread-safe active watched paths lists.
    pub live_watch_paths: Arc<Mutex<Vec<WatchPathEntry>>>,
    /// Channel sender to trigger manual synchronization catch-ups.
    pub sync_now_tx: UnboundedSender<()>,
    /// LRU thumbnail image cache coordinator.
    pub thumbnail_cache: Arc<ThumbnailCache>,
    /// GTK library view loading state container.
    pub library_state: Arc<Mutex<LibraryState>>,
    /// True if timeline page views are currently active.
    pub library_timeline_active: AtomicBool,
    /// Authenticated user unique identifier, fetched at bootstrap.
    pub current_user_id: Arc<Mutex<Option<String>>>,
    /// Tracking lists of deletions requested by Mimick itself.
    pub expected_self_deletions: Arc<RecentSelfPaths>,
    /// Tracking lists of files downloaded by Mimick itself.
    pub expected_self_downloads: Arc<RecentSelfPaths>,
    /// Active locks to prevent multi-sync conflicts.
    pub reconcile_locks: Arc<ReconcileLocks>,
    /// Deletions waiting to be confirmed across sync sweeps.
    pub pending_deletions: Arc<PendingDeletions>,
    /// Authoritative set of hex checksums the Immich server currently holds,
    /// for any file it can see (uploaded by mimick OR by another client).
    ///
    /// Drives the *truthful* per-tile backup badge (`sync_state == 2` iff a
    /// local file's checksum is in here). Populated off-thread by
    /// `refresh_server_checksums` (enumerate → hash → `bulk_existing_asset_ids`)
    /// at startup, after each backup batch, and when the backup page opens.
    ///
    /// While EMPTY (not yet populated, or the probe failed) the badge classifier
    /// falls back to the local sync-index membership test so badges are never
    /// *worse* than the index-only behaviour.
    pub server_checksums: Arc<RwLock<HashSet<String>>>,
}
