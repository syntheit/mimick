//! Download flow, video handoff, and transfer-tracking helpers.
//!
//! Streams the original-quality asset from the server into a local file,
//! updating the progress bar and transfer rate display. Video files can
//! optionally be handed off to the system default player after download.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::TransferProgressCallback;
use crate::app_context::AppContext;
use crate::library::state::LibrarySource;
use crate::state_manager::TransferDirection;

use super::LibraryWindowUi;

pub(super) fn begin_download_session(ctx: &Arc<AppContext>, item_label: String) {
    let state_ref = ctx.state.clone();
    let mut state = state_ref.lock();
    let route = state.active_server_route.clone();
    state
        .transfer
        .begin_group(TransferDirection::Download, Some(item_label), route);
}

pub(super) fn track_download_item(
    ctx: &Arc<AppContext>,
    item_id: String,
    item_label: Option<String>,
    total_bytes: Option<u64>,
) -> TransferProgressCallback {
    let state_ref = ctx.state.clone();
    {
        let mut state = state_ref.lock();
        let route = state.active_server_route.clone();
        state.transfer.register_item(
            TransferDirection::Download,
            item_id.clone(),
            total_bytes,
            item_label,
            route,
        );
    }
    Arc::new(move |bytes_done, total_bytes| {
        let mut state = state_ref.lock();
        if let Some(total_bytes) = total_bytes {
            let current = state
                .transfer
                .active_item_totals
                .get(&item_id)
                .copied()
                .unwrap_or(0);
            if current == 0 {
                state.transfer.update_item_total(&item_id, total_bytes);
            }
        }
        let route = state.active_server_route.clone();
        state
            .transfer
            .update_item_bytes(TransferDirection::Download, &item_id, bytes_done, route);
    })
}

pub(super) fn finish_download_item(ctx: &Arc<AppContext>, item_id: &str) {
    let mut state = ctx.state.lock();
    let route = state.active_server_route.clone();
    state
        .transfer
        .finish_item(TransferDirection::Download, item_id, route);
}

/// Hand a local file off to the user's default app via `xdg-open`/equivalent.
/// Used for local videos per the spec — no in-app playback in v1.
pub(super) fn open_local_with_default_app(path: &str) {
    let uri = format!("file://{}", path);
    if let Err(err) =
        gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>)
    {
        log::warn!("Failed to open {}: {}", uri, err);
    }
}

pub(super) fn spawn_video_handoff(ui: Rc<LibraryWindowUi>, asset_id: String, filename: String) {
    glib::MainContext::default().spawn_local(async move {
        let Some(cache_dir) = crate::profile::cache_dir().map(|p| p.join("video")) else {
            return;
        };
        let _ = std::fs::create_dir_all(&cache_dir);
        let safe_name =
            crate::sanitize::safe_filename(&filename).unwrap_or_else(|| asset_id.clone());
        let path = cache_dir.join(&safe_name);
        if !path.exists()
            && let Err(err) = {
                begin_download_session(&ui.ctx, filename.clone());
                let progress =
                    track_download_item(&ui.ctx, asset_id.clone(), Some(filename.clone()), None);
                let result = ui
                    .ctx
                    .api_client
                    .download_original_to_file(&asset_id, &path, Some(progress))
                    .await;
                finish_download_item(&ui.ctx, &asset_id);
                result
            }
        {
            log::warn!("Video handoff failed for {}: {}", asset_id, err);
            return;
        }
        open_local_with_default_app(&path.display().to_string());
    });
}

pub(super) fn start_download(ui: Rc<LibraryWindowUi>, asset_id: String, filename: String) {
    begin_download_session(&ui.ctx, filename.clone());
    start_download_with_session(ui, asset_id, filename, true);
}

pub(super) fn start_download_group(ui: Rc<LibraryWindowUi>, downloads: Vec<(String, String)>) {
    begin_download_session(&ui.ctx, format!("{} items", downloads.len()));
    for (asset_id, filename) in downloads {
        start_download_with_session(ui.clone(), asset_id, filename, false);
    }
}

fn start_download_with_session(
    ui: Rc<LibraryWindowUi>,
    asset_id: String,
    filename: String,
    show_result_dialog: bool,
) {
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            let Some(target_dir) = ensure_download_target(&ui).await else {
                return;
            };
            let safe_name =
                crate::sanitize::safe_filename(&filename).unwrap_or_else(|| asset_id.clone());
            let output_path = target_dir.join(&safe_name);
            if output_path.exists() {
                let dialog = libadwaita::AlertDialog::builder()
                    .heading("File already exists")
                    .body("Overwrite the existing file or skip this download?")
                    .build();
                dialog.add_response("skip", "Skip");
                dialog.add_response("overwrite", "Overwrite");
                dialog.set_response_appearance(
                    "overwrite",
                    libadwaita::ResponseAppearance::Destructive,
                );
                dialog.connect_response(
                    None,
                    clone!(
                        #[strong]
                        ui,
                        #[strong]
                        asset_id,
                        #[strong]
                        filename,
                        #[strong]
                        show_result_dialog,
                        move |dialog, response| {
                            dialog.close();
                            if response == "overwrite" {
                                spawn_download(
                                    ui.clone(),
                                    asset_id.clone(),
                                    target_dir.join(&filename),
                                    show_result_dialog,
                                );
                            }
                        }
                    ),
                );
                dialog.present(Some(&ui.window));
                return;
            }
            spawn_download(ui, asset_id, output_path, show_result_dialog);
        }
    ));
}

fn spawn_download(
    ui: Rc<LibraryWindowUi>,
    asset_id: String,
    output_path: PathBuf,
    show_result_dialog: bool,
) {
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            let item_label = output_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| output_path.display().to_string());
            let progress =
                track_download_item(&ui.ctx, asset_id.clone(), Some(item_label.clone()), None);
            match ui
                .ctx
                .api_client
                .download_original_to_file(&asset_id, &output_path, Some(progress))
                .await
            {
                Ok(()) => {
                    let session_finished = {
                        finish_download_item(&ui.ctx, &asset_id);
                        !ui.ctx.state.lock().transfer.active
                    };
                    if should_refresh_after_download(&ui) && session_finished {
                        super::refresh_library_after_mutation(ui.clone(), true);
                    }
                    if show_result_dialog {
                        let heading = "Download Complete";
                        let body = format!("Saved {}", output_path.display());
                        let alert = libadwaita::AlertDialog::builder()
                            .heading(heading)
                            .body(&body)
                            .build();
                        alert.add_response("ok", "OK");
                        alert.present(Some(&ui.window));
                    }
                }
                Err(err) => {
                    finish_download_item(&ui.ctx, &asset_id);
                    if show_result_dialog {
                        let alert = libadwaita::AlertDialog::builder()
                            .heading("Download Failed")
                            .body(&err)
                            .build();
                        alert.add_response("ok", "OK");
                        alert.present(Some(&ui.window));
                    }
                }
            }
        }
    ));
}

pub(super) async fn ensure_download_target(ui: &LibraryWindowUi) -> Option<PathBuf> {
    let existing_target = ui.ctx.config.read().data.download_target_path.clone();
    if let Some(path) = existing_target {
        return Some(PathBuf::from(path));
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let dialog = gtk::FileDialog::builder()
        .title("Choose Library Download Folder")
        .build();
    dialog.select_folder(Some(&ui.window), gtk::gio::Cancellable::NONE, move |res| {
        let _ = tx.send(
            res.ok()
                .and_then(|folder| folder.path())
                .map(|path| path.to_path_buf()),
        );
    });

    let path = rx.await.ok().flatten()?;
    {
        let mut config = ui.ctx.config.write();
        config.data.download_target_path = Some(path.to_string_lossy().to_string());
        let _ = config.save();
    }
    Some(path)
}

pub(super) fn should_refresh_after_download(ui: &LibraryWindowUi) -> bool {
    matches!(
        ui.ctx.library_state.lock().source,
        LibrarySource::LocalAll
            | LibrarySource::LocalSearch { .. }
            | LibrarySource::Unified
            | LibrarySource::UnifiedSearch { .. }
            | LibrarySource::AlbumLocal { .. }
            | LibrarySource::AlbumUnified { .. }
    )
}

pub(super) fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec.max(0.0))
    }
}
