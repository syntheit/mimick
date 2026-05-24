//! Manual upload entry point.
//!
//! Opens a multi-file picker, hashes each selection on a blocking worker, and
//! enqueues a `FileTask` on the shared `QueueManager`. The Upload header-bar
//! button in the library window invokes [`pick_and_upload`]; the same flow is
//! used for both library and album scopes — the caller resolves the album.

use std::path::PathBuf;
use std::sync::Arc;

use gtk::gio;
use gtk::prelude::*;

use crate::app_context::AppContext;
use crate::monitor::compute_sha1_chunked;
use crate::queue_manager::FileTask;

/// Open the OS file picker and enqueue every chosen file for upload.
///
/// `album` carries the target album id and display name when the caller is in
/// a selected-album view; otherwise pass `None` and files go to the library.
pub fn pick_and_upload(
    parent: &libadwaita::ApplicationWindow,
    ctx: Arc<AppContext>,
    album: Option<(String, String)>,
) {
    let dialog = gtk::FileDialog::builder()
        .title(if album.is_some() {
            "Upload to album"
        } else {
            "Upload to library"
        })
        .modal(true)
        .build();

    let parent = parent.clone();
    dialog.open_multiple(Some(&parent), gio::Cancellable::NONE, move |result| {
        let files = match result {
            Ok(files) => files,
            Err(err) => {
                if !err.matches(gtk::DialogError::Dismissed) {
                    log::warn!("Upload picker failed: {}", err);
                }
                return;
            }
        };
        let paths: Vec<PathBuf> = (0..files.n_items())
            .filter_map(|i| files.item(i))
            .filter_map(|obj| obj.downcast::<gio::File>().ok())
            .filter_map(|file| file.path())
            .collect();
        if paths.is_empty() {
            return;
        }
        spawn_enqueue(ctx.clone(), album.clone(), paths);
    });
}

fn spawn_enqueue(ctx: Arc<AppContext>, album: Option<(String, String)>, paths: Vec<PathBuf>) {
    glib::MainContext::default().spawn_local(async move {
        let mut queued = 0usize;
        for path in paths {
            let Some(path_str) = path.to_str().map(str::to_owned) else {
                log::warn!("Skipping non-UTF8 upload path: {}", path.display());
                continue;
            };
            let watch_path = path
                .parent()
                .and_then(|p| p.to_str())
                .map(str::to_owned)
                .unwrap_or_default();
            let hash_target = path_str.clone();
            let checksum =
                match tokio::task::spawn_blocking(move || compute_sha1_chunked(&hash_target)).await
                {
                    Ok(Ok(c)) => c,
                    Ok(Err(err)) => {
                        log::warn!("Could not checksum upload '{}': {}", path_str, err);
                        continue;
                    }
                    Err(err) => {
                        log::warn!("Checksum task failed for '{}': {}", path_str, err);
                        continue;
                    }
                };
            let sidecar_path =
                crate::sidecar::find_sidecar(&path).map(|p| p.to_string_lossy().into_owned());
            let task = FileTask {
                path: path_str,
                watch_path,
                checksum,
                album_id: album.as_ref().map(|(id, _)| id.clone()),
                album_name: album.as_ref().map(|(_, name)| name.clone()),
                reassociate_only: false,
                // When the user uploads from the library / albums / explore
                // view (album=None) the asset must land album-less; the
                // queue's parent-dir-as-album fallback would otherwise create
                // a junk album named after the user's file system layout.
                skip_album: album.is_none(),
                sidecar_path,
            };
            if ctx.queue_manager.add_to_queue(task).await {
                queued += 1;
            }
        }
        log::info!("Manual upload: queued {} file(s)", queued);
    });
}
