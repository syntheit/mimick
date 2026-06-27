//! Staging view for files opened via "Open With" or the `--upload` CLI flag.
//!
//! Presents externally-provided file paths in a masonry grid with selection
//! checkboxes and Upload / Upload to Album actions. Reuses the existing
//! `MasonryCanvas`, `LibraryAssetModel`, `AssetObject`, `ThumbnailCache`,
//! and `spawn_enqueue` infrastructure.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::LibraryAlbum;
use crate::app_context::AppContext;
use crate::library::asset_object::AssetObject;
use crate::library::masonry::build_grid_view;
use crate::library::style;
use crate::library::upload_picker;
use crate::media_kinds;

/// Build and present the staging window populated with the given file paths.
///
/// When `auto_upload` is true (the `--upload` Desktop Action), an album
/// picker dialog is presented immediately after the window opens.
pub fn build_staging_window(
    app: &libadwaita::Application,
    ctx: Arc<AppContext>,
    files: Vec<PathBuf>,
    auto_upload: bool,
) {
    style::ensure_registered();
    crate::library::register_app_icons();

    let window = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("Mimick - Upload")
        .name("mimick-staging-window")
        .default_width(960)
        .default_height(640)
        .width_request(360)
        .height_request(400)
        .build();

    // --- Header bar ---
    let header = libadwaita::HeaderBar::builder()
        .show_start_title_buttons(true)
        .show_end_title_buttons(true)
        .build();

    let upload_btn = gtk::Button::builder()
        .label("Upload")
        .tooltip_text("Upload selected files to library")
        .css_classes(["suggested-action", "mimick-pressable"])
        .build();
    let upload_album_btn = gtk::Button::builder()
        .label("Album")
        .tooltip_text("Upload selected files to an album")
        .css_classes(["mimick-pressable"])
        .build();
    let select_all_btn = gtk::Button::builder()
        .label("All")
        .tooltip_text("Select all / Deselect all")
        .css_classes(["mimick-pressable"])
        .build();

    header.pack_start(&upload_btn);
    header.pack_start(&upload_album_btn);
    header.pack_end(&select_all_btn);

    let toolbar = libadwaita::ToolbarView::builder().build();
    toolbar.add_top_bar(&header);

    // --- Grid ---
    let select_toggle = gtk::ToggleButton::builder()
        .active(true) // staging view starts in select mode
        .build();
    let narrow = Rc::new(Cell::new(false));
    let grid = build_grid_view(ctx.clone(), select_toggle.clone(), narrow.clone());
    // Always show select checkboxes in staging view.
    grid.canvas.set_select_mode(true);

    // Populate model from file paths.
    let assets = build_staging_assets(&files);
    grid.model.reset_with_objects(assets);

    // --- Status bar ---
    let status_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .hexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    let file_count = grid.model.n_items();
    status_label.set_text(&format!("{} file(s) staged", file_count));

    toolbar.set_content(Some(&grid.scrolled));
    toolbar.add_bottom_bar(&status_label);
    window.set_content(Some(&toolbar));

    // --- Clone captures for closures ---
    let files_rc = Rc::new(files);
    let selection = grid.selection.clone();

    // --- Select All / Deselect All ---
    {
        let sel = selection.clone();
        let lbl = status_label.clone();
        let n = file_count;
        let all_selected = Rc::new(Cell::new(false));
        select_all_btn.connect_clicked(move |btn| {
            if all_selected.get() {
                sel.unselect_all();
                btn.set_icon_name("edit-select-all-symbolic");
                btn.set_tooltip_text(Some("Select all"));
                all_selected.set(false);
                lbl.set_text(&format!("{} file(s) staged", n));
            } else {
                sel.select_all();
                btn.set_icon_name("edit-clear-all-symbolic");
                btn.set_tooltip_text(Some("Deselect all"));
                all_selected.set(true);
                lbl.set_text(&format!("{} of {} selected", n, n));
            }
        });
    }

    // --- Track selection count ---
    {
        let sel = selection.clone();
        let lbl = status_label.clone();
        sel.connect_selection_changed(move |sel, _, _| {
            let total = sel.n_items();
            let selected = count_selected(sel);
            if selected == 0 {
                lbl.set_text(&format!("{} file(s) staged", total));
            } else {
                lbl.set_text(&format!("{} of {} selected", selected, total));
            }
        });
    }

    // --- Upload (library, no album) ---
    {
        let ctx_c = ctx.clone();
        let sel = selection.clone();
        let files_c = files_rc.clone();
        let win = window.clone();
        upload_btn.connect_clicked(move |_| {
            let paths = selected_paths(&sel, &files_c);
            if paths.is_empty() {
                return;
            }
            let win_c = win.clone();
            upload_picker::spawn_enqueue_with_callback(
                ctx_c.clone(),
                None,
                paths,
                move |queued, skipped| show_upload_result(&win_c, queued, skipped),
            );
        });
    }

    // --- Upload to Album ---
    {
        let ctx_c = ctx.clone();
        let sel = selection.clone();
        let files_c = files_rc.clone();
        let win = window.clone();
        upload_album_btn.connect_clicked(move |_| {
            let paths = selected_paths(&sel, &files_c);
            if paths.is_empty() {
                return;
            }
            show_album_picker(win.clone(), ctx_c.clone(), paths);
        });
    }

    window.present();

    // Auto-upload mode: immediately trigger album picker.
    if auto_upload {
        let all_paths = files_rc.to_vec();
        // Select all items first.
        selection.select_all();
        glib::idle_add_local_once(clone!(
            #[strong]
            window,
            #[strong]
            ctx,
            move || {
                show_album_picker(window, ctx, all_paths);
            }
        ));
    }
}

/// Build `AssetObject` entries from local file paths for the staging grid.
fn build_staging_assets(files: &[PathBuf]) -> Vec<AssetObject> {
    files
        .iter()
        .filter(|path| media_kinds::is_supported_path(path))
        .filter_map(|path| {
            let filename = path.file_name()?.to_string_lossy().to_string();
            let mime = media_kinds::mime_for_path(path);
            let kind = media_kinds::asset_kind(mime);
            let asset_type = match kind {
                media_kinds::AssetKind::Image => "IMAGE",
                media_kinds::AssetKind::Video => "VIDEO",
            };
            let id = format!("local::{}", path.display());
            let mtime_str = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(systemtime_to_iso)
                .unwrap_or_default();

            Some(AssetObject::new_local(
                &id,
                &filename,
                mime,
                &mtime_str,
                asset_type,
                &path.to_string_lossy(),
            ))
        })
        .collect()
}

/// Format a `SystemTime` as an ISO 8601 string in the local timezone.
fn systemtime_to_iso(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Local};
    let dt: DateTime<Local> = t.into();
    dt.to_rfc3339()
}

/// Count the number of selected items in a `MultiSelection`.
fn count_selected(sel: &gtk::MultiSelection) -> u32 {
    let bs = sel.selection();
    let mut count = 0u32;
    let n = bs.size();
    for i in 0..n {
        if bs.nth(i as u32) != gtk::INVALID_LIST_POSITION {
            count += 1;
        }
    }
    // Fallback: iterate all items
    if count == 0 && n > 0 {
        count = n as u32;
    }
    count
}

/// Collect file paths for the selected items, or all items if none selected.
fn selected_paths(sel: &gtk::MultiSelection, files: &[PathBuf]) -> Vec<PathBuf> {
    let bs = sel.selection();
    let total = sel.n_items();
    let mut paths = Vec::new();

    // Collect indices of selected items.
    let mut selected_indices = Vec::new();
    let n_ranges = bs.size();
    if n_ranges > 0 {
        for i in 0..total {
            if sel.is_selected(i) {
                selected_indices.push(i as usize);
            }
        }
    }

    if selected_indices.is_empty() {
        // Nothing explicitly selected -- upload all.
        return files
            .iter()
            .filter(|p| media_kinds::is_supported_path(p))
            .cloned()
            .collect();
    }

    for idx in selected_indices {
        if let Some(item) = sel.item(idx as u32).and_downcast::<AssetObject>() {
            let local_path = item.property::<String>("local-path");
            if !local_path.is_empty() {
                paths.push(PathBuf::from(local_path));
            }
        }
    }
    paths
}

/// Present an album picker dialog and upload files to the chosen album.
fn show_album_picker(
    parent: libadwaita::ApplicationWindow,
    ctx: Arc<AppContext>,
    paths: Vec<PathBuf>,
) {
    let dialog = libadwaita::AlertDialog::builder()
        .heading("Upload to Album")
        .body("Loading albums...")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.present(Some(&parent));

    // Fetch albums asynchronously and present the list.
    let ctx_c = ctx.clone();
    let parent_c = parent.clone();
    glib::MainContext::default().spawn_local(async move {
        let albums = match ctx_c.api_client.fetch_library_albums().await {
            Ok(a) => a,
            Err(err) => {
                dialog.force_close();
                let err_dialog = libadwaita::AlertDialog::builder()
                    .heading("Failed to Load Albums")
                    .body(&err)
                    .build();
                err_dialog.add_response("ok", "OK");
                err_dialog.present(Some(&parent_c));
                return;
            }
        };
        dialog.force_close();
        present_album_list(parent_c, ctx_c, albums, paths);
    });
}

/// Build and show the album selection list dialog.
fn present_album_list(
    parent: libadwaita::ApplicationWindow,
    ctx: Arc<AppContext>,
    albums: Vec<LibraryAlbum>,
    paths: Vec<PathBuf>,
) {
    let parent_for_result = parent.clone();
    let dialog = libadwaita::AlertDialog::builder()
        .heading("Select Album")
        .body("Choose an album for the upload:")
        .build();
    dialog.add_response("cancel", "Cancel");

    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .build();

    for album in &albums {
        let row = libadwaita::ActionRow::builder()
            .title(&album.album_name)
            .subtitle(format!("{} assets", album.asset_count))
            .activatable(true)
            .build();
        listbox.append(&row);
    }

    let scroll = gtk::ScrolledWindow::builder()
        .child(&listbox)
        .min_content_height(200)
        .max_content_height(400)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_extra_child(Some(&scroll));

    let albums_rc = Rc::new(albums);
    let paths_rc = Rc::new(paths);
    let ctx_c = ctx.clone();
    let listbox_ref = listbox.clone();

    dialog.connect_response(
        None,
        clone!(
            #[strong]
            albums_rc,
            #[strong]
            paths_rc,
            #[strong]
            ctx_c,
            #[strong]
            listbox_ref,
            move |_dialog, response| {
                if response == "cancel" {
                    return;
                }
                let Some(row) = listbox_ref.selected_row() else {
                    return;
                };
                let idx = row.index() as usize;
                if let Some(album) = albums_rc.get(idx) {
                    let album_arg = Some((album.id.clone(), album.album_name.clone()));
                    let parent_c = parent_for_result.clone();
                    upload_picker::spawn_enqueue_with_callback(
                        ctx_c.clone(),
                        album_arg,
                        paths_rc.to_vec(),
                        move |queued, skipped| show_upload_result(&parent_c, queued, skipped),
                    );
                }
            }
        ),
    );

    // Add an "Upload" response that the user clicks after selecting a row.
    dialog.add_response("upload", "Upload");
    dialog.set_response_appearance("upload", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("upload"));

    dialog.present(Some(&parent));
}

/// Show a result dialog after upload enqueue completes.
fn show_upload_result(parent: &libadwaita::ApplicationWindow, queued: usize, skipped: usize) {
    let (heading, body) = if skipped == 0 {
        (
            "Upload Queued",
            format!(
                "{} file(s) queued for upload. Progress is visible in the queue inspector.",
                queued
            ),
        )
    } else if queued == 0 {
        (
            "Upload Failed",
            format!(
                "All {} file(s) failed to enqueue. Check the logs for details.",
                skipped
            ),
        )
    } else {
        (
            "Upload Partially Queued",
            format!(
                "{} file(s) queued, {} skipped (hash or path errors).",
                queued, skipped
            ),
        )
    };

    let dialog = libadwaita::AlertDialog::builder()
        .heading(heading)
        .body(&body)
        .build();
    dialog.add_response("ok", "OK");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_staging_assets_filters_unsupported() {
        let files = vec![
            PathBuf::from("/tmp/photo.jpg"),
            PathBuf::from("/tmp/document.pdf"),
            PathBuf::from("/tmp/video.mp4"),
            PathBuf::from("/tmp/readme.txt"),
        ];
        let assets = build_staging_assets(&files);
        // .jpg and .mp4 are supported; .pdf and .txt are not.
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].property::<String>("filename"), "photo.jpg");
        assert_eq!(assets[1].property::<String>("filename"), "video.mp4");
    }

    #[test]
    fn build_staging_assets_sets_asset_type() {
        let files = vec![
            PathBuf::from("/tmp/photo.png"),
            PathBuf::from("/tmp/clip.mkv"),
        ];
        let assets = build_staging_assets(&files);
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].property::<String>("asset-type"), "IMAGE");
        assert_eq!(assets[1].property::<String>("asset-type"), "VIDEO");
    }
}
