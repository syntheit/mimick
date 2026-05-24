//! Unified disk-cache maintenance.
//!
//! Every on-disk cache directory the app writes to lives as a subdirectory
//! of `profile::cache_dir()`. This module enumerates them all so that a
//! "clear cache" action and a startup prune apply uniformly across
//! thumbnails, decoded RAW previews, EXIF entries, video downloads,
//! original previews, and open-in handoffs.

use std::path::PathBuf;
use std::time::SystemTime;

/// All cache subdirectories managed by the app, relative to the profile
/// cache root. Adding a new on-disk cache means appending one entry here.
pub const CACHE_SUBDIRS: &[&str] = &[
    "thumbnails",
    "raw_decode",
    "exif",
    "video",
    "preview",
    "open-in",
];

/// Yield to the OS scheduler every N file operations during a prune sweep
/// so the eviction work does not monopolise a core on slow disks.
const YIELD_EVERY: usize = 64;

fn cache_root() -> Option<PathBuf> {
    crate::profile::cache_dir()
}

/// Synchronously delete every managed cache subdirectory. Intended to be
/// called from `tokio::task::spawn_blocking` so the UI thread is not blocked.
pub fn clear_all_blocking() -> Result<(), String> {
    let Some(root) = cache_root() else {
        return Ok(());
    };
    let mut first_error: Option<String> = None;
    for sub in CACHE_SUBDIRS {
        let dir = root.join(sub);
        if !dir.exists() {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(&dir)
            && first_error.is_none()
        {
            first_error = Some(format!("{}: {}", dir.display(), err));
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Walk every managed cache subdirectory and evict the oldest files until
/// the total on-disk usage falls under `cap_bytes`. Intended to run once at
/// startup from a blocking task.
pub fn prune_all_blocking(cap_bytes: u64) {
    let Some(root) = cache_root() else {
        return;
    };
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for sub in CACHE_SUBDIRS {
        collect_files(&root.join(sub), &mut entries, &mut total);
    }
    if total <= cap_bytes {
        return;
    }
    entries.sort_by_key(|(_, _, mtime)| *mtime);
    let mut count: usize = 0;
    for (path, size, _) in entries {
        if total <= cap_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
        count += 1;
        if count.is_multiple_of(YIELD_EVERY) {
            std::thread::yield_now();
        }
    }
}

fn collect_files(
    dir: &std::path::Path,
    out: &mut Vec<(PathBuf, u64, SystemTime)>,
    total: &mut u64,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_files(&path, out, total);
            continue;
        }
        let size = metadata.len();
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        *total = total.saturating_add(size);
        out.push((path, size, mtime));
    }
}
