//! Lightbox image viewer: full-screen preview with zoom, pan, EXIF details, and keyboard navigation.
//!
//! Loads preview or original resolution images with pinch-zoom and
//! swipe navigation. Displays an EXIF metadata panel and provides
//! download-to-folder and delete-to-trash actions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{CallbackAnimationTarget, TimedAnimation};

use crate::api_client::{ExifInfo, ThumbnailSize};
use crate::library::asset_object::AssetObject;
use crate::library::local_exif::{self, LocalExif};

use super::context_menu::show_asset_context_menu;
use super::download::{
    begin_download_session, finish_download_item, open_local_with_default_app, start_download,
    track_download_item,
};
use super::{LOCAL_ID_PREFIX, LibraryWindowUi, load_source_page, load_texture_oriented};

/// Everything we can learn about a local file off the main thread before
/// handing the data back to GTK. `exif` is the cached EXIF parse; `dims`
/// comes from a pixbuf header-only read; `mtime_iso` falls back when the
/// file has no `DateTimeOriginal` so we always show *some* date.
struct LocalProbe {
    exif: Option<LocalExif>,
    file_size: Option<u64>,
    mtime_iso: Option<String>,
    dims: Option<(u32, u32)>,
}

/// Render a `SystemTime` as an RFC3339 string in the local timezone so the
/// existing `format_datetime_display` pipeline can format it consistently
/// with EXIF-derived timestamps.
fn systemtime_to_rfc3339(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Local};
    let dt: DateTime<Local> = t.into();
    dt.to_rfc3339()
}

/// Project local-file metadata into the API EXIF shape so the existing
/// renderer handles both sources uniformly.
fn exif_from_local(local: &LocalExif, file_size: Option<u64>) -> ExifInfo {
    ExifInfo {
        make: local.make.clone(),
        model: local.model.clone(),
        lens_model: local.lens_model.clone(),
        f_number: local.f_number,
        focal_length: local.focal_length,
        iso: local.iso,
        exposure_time: local.exposure_time.clone(),
        file_size_in_byte: file_size,
        date_time_original: local.date_time_original.clone(),
        city: None,
        state: None,
        country: None,
        latitude: local.latitude,
        longitude: local.longitude,
        description: local.description.clone(),
        exif_image_width: local.image_width,
        exif_image_height: local.image_height,
    }
}

fn original_preview_cache_path(
    cache_dir: &std::path::Path,
    asset_id: &str,
    filename: &str,
) -> std::path::PathBuf {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("bin");
    cache_dir.join(format!("{asset_id}.{ext}"))
}

/// Create a properly-named export copy of a cached file for drag-out.
///
/// Returns `Some(path)` with the original filename visible to file managers.
/// Falls back to `source` if the export directory isn't available.
fn drag_export_path(
    asset_id: &str,
    filename: &str,
    source: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let export_dir = crate::profile::cache_dir()?.join("drag_export");
    let _ = std::fs::create_dir_all(&export_dir);
    let prefix = &asset_id[..8.min(asset_id.len())];
    let export = export_dir.join(format!("{prefix}_{filename}"));
    if export.exists() {
        return Some(export);
    }
    if std::fs::hard_link(source, &export).is_ok() || std::fs::copy(source, &export).is_ok() {
        Some(export)
    } else {
        Some(source.to_path_buf())
    }
}

/// Populate the details sidebar with sectioned EXIF metadata.
///
/// Groups fall into three categories — Camera, Image, Location — each rendered
/// as an `AdwPreferencesGroup` with accent-coloured prefix icons. Empty groups
/// (e.g. an image with no GPS) are skipped so the pane stays compact.
///
/// `taken_label` decides whether the date row reads "Taken" (EXIF
/// DateTimeOriginal — the camera's capture moment) or "Modified" (filesystem
/// mtime fallback when no capture timestamp exists).
fn fill_exif_box(container: &gtk::Box, exif: &crate::api_client::ExifInfo, taken_label: &str) {
    if let Some(group) = build_camera_group(exif) {
        container.append(&group);
    }
    if let Some(group) = build_image_group(exif, taken_label) {
        container.append(&group);
    }
    if let Some(group) = build_location_group(exif) {
        container.append(&group);
    }
    if let Some(desc) = &exif.description
        && !desc.trim().is_empty()
    {
        let note_group = libadwaita::PreferencesGroup::builder()
            .title("Description")
            .build();
        let row = libadwaita::ActionRow::builder()
            .title(desc.as_str())
            .title_lines(0)
            .css_classes(["property"])
            .build();
        note_group.add(&row);
        container.append(&note_group);
    }
}

fn build_camera_group(exif: &crate::api_client::ExifInfo) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Camera")
        .build();
    let mut rows = 0u32;
    if let Some(c) = format_camera(exif) {
        group.add(&accent_row(
            "camera-photo-symbolic",
            "mimick-accent-camera",
            "Body",
            &c,
        ));
        rows += 1;
    }
    if let Some(l) = &exif.lens_model
        && !l.trim().is_empty()
    {
        group.add(&accent_row(
            "view-fullscreen-symbolic",
            "mimick-accent-camera",
            "Lens",
            l,
        ));
        rows += 1;
    }
    if let Some(exposure) = format_exposure(exif) {
        group.add(&accent_row(
            "weather-clear-symbolic",
            "mimick-accent-camera",
            "Exposure",
            &exposure,
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn build_image_group(
    exif: &crate::api_client::ExifInfo,
    taken_label: &str,
) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Image")
        .build();
    let mut rows = 0u32;
    if let (Some(w), Some(h)) = (exif.exif_image_width, exif.exif_image_height) {
        group.add(&accent_row(
            "view-grid-symbolic",
            "mimick-accent-image",
            "Dimensions",
            &format!("{w} × {h}"),
        ));
        rows += 1;
    }
    if let Some(size) = exif.file_size_in_byte {
        group.add(&accent_row(
            "drive-harddisk-symbolic",
            "mimick-accent-image",
            "Size",
            &format_bytes(size),
        ));
        rows += 1;
    }
    if let Some(dt) = &exif.date_time_original
        && !dt.trim().is_empty()
    {
        group.add(&accent_row(
            "x-office-calendar-symbolic",
            "mimick-accent-image",
            taken_label,
            &format_datetime_display(dt),
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn build_location_group(
    exif: &crate::api_client::ExifInfo,
) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Location")
        .build();
    let mut rows = 0u32;
    if let Some(loc) = format_location(exif) {
        group.add(&accent_row(
            "mark-location-symbolic",
            "mimick-accent-location",
            "Place",
            &loc,
        ));
        rows += 1;
    }
    if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude) {
        group.add(&accent_row(
            "find-location-symbolic",
            "mimick-accent-location",
            "Coordinates",
            &format!("{lat:.5}, {lon:.5}"),
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn accent_row(icon: &str, accent_class: &str, title: &str, value: &str) -> libadwaita::ActionRow {
    let prefix = gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(16)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(["mimick-detail-icon", accent_class])
        .build();
    let row = libadwaita::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_lines(2)
        .css_classes(["property"])
        .build();
    row.add_prefix(&prefix);
    row
}

fn format_camera(exif: &crate::api_client::ExifInfo) -> Option<String> {
    match (&exif.make, &exif.model) {
        (Some(m), Some(n)) => {
            let m = m.trim();
            let n = n.trim();
            if n.starts_with(m) {
                Some(n.to_string())
            } else {
                Some(format!("{m} {n}"))
            }
        }
        (Some(m), None) => Some(m.trim().to_string()),
        (None, Some(n)) => Some(n.trim().to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn format_exposure(exif: &crate::api_client::ExifInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(f) = exif.f_number {
        parts.push(format!("ƒ/{f:.1}"));
    }
    if let Some(et) = &exif.exposure_time
        && !et.trim().is_empty()
    {
        parts.push(et.trim().to_string());
    }
    if let Some(iso) = exif.iso {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(focal) = exif.focal_length {
        parts.push(format!("{focal:.0}mm"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn format_location(exif: &crate::api_client::ExifInfo) -> Option<String> {
    let parts: Vec<&str> = [&exif.city, &exif.state, &exif.country]
        .into_iter()
        .filter_map(|s| s.as_deref().map(str::trim).filter(|t| !t.is_empty()))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Format a byte count value into a human-readable size string (e.g. KB, MB, GB).
fn format_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n_f = n as f64;
    if n_f >= KIB * KIB * KIB {
        format!("{:.2} GB", n_f / (KIB * KIB * KIB))
    } else if n_f >= KIB * KIB {
        format!("{:.2} MB", n_f / (KIB * KIB))
    } else if n_f >= KIB {
        format!("{:.1} KB", n_f / KIB)
    } else {
        format!("{} B", n)
    }
}

/// Format an ISO 8601 timestamp for display, converting from UTC to the
/// user's local timezone.
///
/// Immich normalises `date_time_original` and `fileCreatedAt` to UTC before
/// storage, so a photo taken at 19:55:15+05:30 is stored as
/// 2024-01-15T14:25:15.000Z. We parse the UTC value and convert it to the
/// system's local timezone so the displayed time matches what the camera
/// originally recorded. Falls back to the raw string if parsing fails.
fn format_datetime_display(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    // Try offset-aware parse first (handles +05:30, Z, etc.)
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let local: DateTime<Local> = dt.into();
        return local.format("%Y-%m-%d %H:%M:%S UTC%:z").to_string();
    }
    // Fallback: try treating as UTC
    if let Ok(dt) = iso.parse::<DateTime<Utc>>() {
        let local: DateTime<Local> = dt.into();
        return local.format("%Y-%m-%d %H:%M:%S UTC%:z").to_string();
    }
    // Last resort: strip trailing fractional seconds / timezone suffix
    iso.get(..19).unwrap_or(iso).replace('T', " ").to_string()
}

/// Format an ISO 8601 timestamp as a long, human-friendly local string, e.g.
/// "Monday, July 19, 2026 · 3:42 PM". Falls back to the compact display form
/// (and finally the raw string) if the value can't be parsed.
fn format_full_timestamp(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    let local: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.into())
        .ok()
        .or_else(|| iso.parse::<DateTime<Utc>>().ok().map(|dt| dt.into()));
    if let Some(local) = local {
        // %-I / %-M strip leading zeros on Unix. Weekday, month name, day, year.
        return local.format("%A, %B %-d, %Y · %-I:%M %p").to_string();
    }
    let compact = format_datetime_display(iso);
    if compact.trim().is_empty() {
        iso.to_string()
    } else {
        compact
    }
}

/// Short date/time for the lightbox top bar, e.g. "Jul 19, 2026, 3:42 PM".
/// Empty string when the timestamp can't be parsed so the caller can hide it.
fn format_topbar_datetime(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    let local: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.into())
        .ok()
        .or_else(|| iso.parse::<DateTime<Utc>>().ok().map(|dt| dt.into()));
    local
        .map(|l| l.format("%b %-d, %Y · %-I:%M %p").to_string())
        .unwrap_or_default()
}

/// Build a small OpenStreetMap minimap centered on `(lat, lon)` with a marker,
/// ~180px tall at zoom 14. Uses libshumate's `SimpleMap` with an OSM raster
/// tile source and a single marker layer.
fn build_minimap(lat: f64, lon: f64) -> Option<gtk::Widget> {
    use libshumate::prelude::*;

    let map = libshumate::SimpleMap::new();
    map.set_height_request(180);
    map.set_hexpand(true);

    // OSM raster tiles. Immich uses the standard Mapnik tile server layout.
    let source = libshumate::RasterRenderer::new_full_from_url(
        "osm-mapnik",
        "OpenStreetMap",
        "© OpenStreetMap contributors",
        "https://www.openstreetmap.org/copyright",
        0,
        19,
        256,
        libshumate::MapProjection::Mercator,
        "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
    );
    map.set_map_source(Some(&source));

    let viewport = map.viewport()?;
    viewport.set_zoom_level(14.0);
    viewport.set_location(lat, lon);

    // Drop a marker at the coordinates.
    let marker = libshumate::Marker::new();
    marker.set_location(lat, lon);
    let pin = gtk::Image::from_icon_name("mark-location-symbolic");
    pin.set_pixel_size(28);
    pin.add_css_class("error");
    marker.set_child(Some(&pin));

    let marker_layer = libshumate::MarkerLayer::new_full(&viewport, gtk::SelectionMode::None);
    marker_layer.add_marker(&marker);
    map.add_overlay_layer(&marker_layer);

    Some(map.upcast())
}

/// Apply zoom to a lightbox Picture. Zoom is fit-relative: 1.0 = the size the
/// texture would occupy inside `viewer` under Contain layout. >1.0 overflows
/// the viewer for panning. Returns the computed content dimensions when zoomed.
fn apply_lightbox_zoom(
    picture: &gtk::Picture,
    viewer: &gtk::ScrolledWindow,
    zoom: f64,
) -> Option<(f64, f64)> {
    if (zoom - 1.0).abs() < 0.001 {
        picture.set_size_request(-1, -1);
        return None;
    }
    let Some(paintable) = picture.paintable() else {
        picture.set_size_request(-1, -1);
        return None;
    };
    let nw = paintable.intrinsic_width().max(1) as f64;
    let nh = paintable.intrinsic_height().max(1) as f64;
    let viewer_w = viewer.width().max(1) as f64;
    let viewer_h = viewer.height().max(1) as f64;
    let texture_aspect = nw / nh;
    let viewer_aspect = viewer_w / viewer_h;
    let (fit_w, fit_h) = if viewer_aspect > texture_aspect {
        (viewer_h * texture_aspect, viewer_h)
    } else {
        (viewer_w, viewer_w / texture_aspect)
    };
    let cw = fit_w * zoom;
    let ch = fit_h * zoom;
    picture.set_size_request(cw as i32, ch as i32);
    Some((cw, ch))
}

/// Present a simple modal alert anchored to the main window.
fn show_lightbox_alert(ui: &LibraryWindowUi, heading: &str, body: &str) {
    let alert = libadwaita::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    alert.add_response("ok", "OK");
    alert.present(Some(&ui.window));
}

/// Show an album-picker dialog and add `asset_id` to the chosen album.
///
/// Fetches the album list, then presents an `AdwAlertDialog` whose body is a
/// scrollable list of album buttons. Picking one issues the add-to-album call.
fn show_add_to_album_dialog(ui: Rc<LibraryWindowUi>, asset_id: String) {
    glib::MainContext::default().spawn_local(async move {
        let albums = match ui.ctx.api_client.get_all_albums().await {
            Ok(a) => a,
            Err(err) => {
                show_lightbox_alert(&ui, "Add to album", &err);
                return;
            }
        };
        if albums.is_empty() {
            show_lightbox_alert(&ui, "Add to album", "No albums found on the server.");
            return;
        }
        let mut albums = albums;
        albums.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let dialog = libadwaita::AlertDialog::builder()
            .heading("Add to album")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (name, album_id) in &albums {
            let row = libadwaita::ActionRow::builder()
                .title(name)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            let ui = ui.clone();
            let asset_id = asset_id.clone();
            let album_id = album_id.clone();
            let name = name.clone();
            let dialog_weak = dialog.downgrade();
            row.connect_activated(move |_| {
                if let Some(dialog) = dialog_weak.upgrade() {
                    dialog.close();
                }
                let ui = ui.clone();
                let asset_id = asset_id.clone();
                let album_id = album_id.clone();
                let name = name.clone();
                glib::MainContext::default().spawn_local(async move {
                    let ok = ui
                        .ctx
                        .api_client
                        .add_assets_to_album(&album_id, &[asset_id])
                        .await;
                    if !ok {
                        show_lightbox_alert(
                            &ui,
                            "Add to album",
                            &format!("Failed to add to \"{name}\"."),
                        );
                    }
                });
            });
            list.append(&row);
        }
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(240)
            .propagate_natural_height(true)
            .child(&list)
            .build();
        dialog.set_extra_child(Some(&scroller));
        dialog.present(Some(&ui.window));
    });
}

/// Construct and present the Immich-mobile-style fullscreen lightbox.
///
/// The viewer is a black canvas with no chrome by default. A single tap on the
/// image toggles two translucent bars (top: back / date / favorite / menu;
/// bottom: share / add-to-album / delete). Horizontal swipes navigate between
/// photos, a downward swipe dismisses the viewer, and an upward swipe opens the
/// details bottom sheet. Pinch-zoom / pan and inline video playback are kept.
pub(super) fn open_lightbox(ui: Rc<LibraryWindowUi>, position: u32) {
    let Some(_item) = ui.grid.model.item(position).and_downcast::<AssetObject>() else {
        return;
    };

    let page = libadwaita::NavigationPage::builder()
        .title("Photo")
        // Disable the AdwNavigationView edge-swipe-back and auto back button so
        // a horizontal drag navigates prev/next instead of popping the page.
        // The chrome has its own back button; programmatic `nav.pop()` still
        // works regardless of `can-pop` (it only gates the gesture/auto-button).
        .can_pop(false)
        .build();

    // Root bottom sheet: content is the black viewer overlay; the sheet is the
    // swipe-up details drawer.
    let sheet = libadwaita::BottomSheet::builder()
        .can_open(true)
        .can_close(true)
        .modal(true)
        .build();

    let viewer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["mimick-viewer-black"])
        .build();

    // Two picture widgets in a stack so navigation can slide between them.
    let picture_a = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .css_classes(["mimick-lightbox-picture"])
        .build();
    let picture_b = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .css_classes(["mimick-lightbox-picture"])
        .build();
    let pic_stack = gtk::Stack::builder()
        .transition_duration(180)
        .vexpand(true)
        .hexpand(true)
        .build();
    pic_stack.add_named(&picture_a, Some("a"));
    pic_stack.add_named(&picture_b, Some("b"));
    pic_stack.set_visible_child_name("a");
    let scrolled_picture = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&pic_stack)
        .vexpand(true)
        .hexpand(true)
        .kinetic_scrolling(false)
        .build();

    // Spinner overlay: a centered Mimick app icon that rotates while a
    // full-resolution texture is being fetched / decoded.
    let loader_icon = gtk::Image::builder()
        .icon_name("dev.nicx.mimick")
        .pixel_size(72)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["mimick-loader-icon"])
        .build();
    let loader_overlay = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(180)
        .reveal_child(false)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .child(&loader_icon)
        .can_target(false)
        .build();
    let picture_overlay = gtk::Overlay::builder().build();
    picture_overlay.set_child(Some(&scrolled_picture));
    picture_overlay.add_overlay(&loader_overlay);

    let unavailable_title = gtk::Label::builder()
        .label("Preview unavailable")
        .css_classes(["title-3"])
        .build();
    let unavailable_filename = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(42)
        .build();
    let unavailable_mime = gtk::Label::builder().css_classes(["dim-label"]).build();
    let unavailable_open = gtk::Button::builder()
        .label("Open in external app")
        .css_classes(["suggested-action"])
        .build();
    let unavailable_path = Rc::new(RefCell::new(None::<String>));
    unavailable_open.connect_clicked({
        let unavailable_path = unavailable_path.clone();
        move |_| {
            if let Some(path) = unavailable_path.borrow().as_deref() {
                open_local_with_default_app(path);
            }
        }
    });
    let unavailable_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["mimick-preview-unavailable"])
        .build();
    unavailable_card.append(&unavailable_title);
    unavailable_card.append(&unavailable_filename);
    unavailable_card.append(&unavailable_mime);
    unavailable_card.append(&unavailable_open);
    let unavailable_overlay = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(180)
        .reveal_child(false)
        .halign(gtk::Align::Fill)
        .valign(gtk::Align::Fill)
        .child(&unavailable_card)
        .can_target(false)
        .build();
    picture_overlay.add_overlay(&unavailable_overlay);

    // Drag source for exporting the current asset's original file.
    let lightbox_drag_path: Rc<RefCell<Option<std::path::PathBuf>>> = Rc::new(RefCell::new(None));
    {
        let drag_source = gtk::DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::COPY);
        let drag_path = lightbox_drag_path.clone();
        drag_source.connect_prepare(move |_source, _x, _y| {
            let path = drag_path.borrow().clone()?;
            if !path.exists() {
                return None;
            }
            let file = gtk::gio::File::for_path(&path);
            Some(gtk::gdk::ContentProvider::for_value(&file.to_value()))
        });
        picture_overlay.add_controller(drag_source);
    }

    let active_a = Rc::new(Cell::new(true));
    let zoom_level = Rc::new(Cell::new(1.0_f64));
    let resolution_full = ui.ctx.config.read().data.library_preview_full_resolution;

    let video_player = gtk::Video::builder()
        .autoplay(true)
        .vexpand(true)
        .hexpand(true)
        .visible(false)
        .build();
    let media_stack = gtk::Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .transition_type(gtk::StackTransitionType::None)
        .build();
    media_stack.add_named(&picture_overlay, Some("image"));
    media_stack.add_named(&video_player, Some("video"));
    media_stack.set_visible_child_name("image");

    // ── Chrome: top bar and bottom bar (hidden by default) ──────────────
    let back_btn = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back")
        .build();
    let datetime_label = gtk::Label::builder()
        .css_classes(["mimick-lightbox-datetime"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .halign(gtk::Align::Center)
        .build();
    let favorite_btn = gtk::ToggleButton::builder()
        .icon_name("non-starred-symbolic")
        .tooltip_text("Favorite")
        .build();
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Download"), Some("lightbox.download"));
    menu.append(Some("Copy to clipboard"), Some("lightbox.copy"));
    menu.append(Some("Open with…"), Some("lightbox.openwith"));
    menu.append(Some("Full resolution"), Some("lightbox.fullres"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .tooltip_text("More")
        .build();
    let top_actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    top_actions.append(&favorite_btn);
    top_actions.append(&menu_btn);
    let topbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .css_classes(["mimick-lightbox-topbar"])
        .build();
    topbar.append(&back_btn);
    topbar.append(&datetime_label);
    topbar.append(&top_actions);
    let top_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(160)
        .reveal_child(false)
        .valign(gtk::Align::Start)
        .child(&topbar)
        .build();

    let make_action = |icon: &str, label: &str| -> gtk::Button {
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .halign(gtk::Align::Center)
            .build();
        let img = gtk::Image::from_icon_name(icon);
        img.set_pixel_size(20);
        let lbl = gtk::Label::new(Some(label));
        inner.append(&img);
        inner.append(&lbl);
        gtk::Button::builder()
            .child(&inner)
            .hexpand(true)
            .css_classes(["flat"])
            .build()
    };
    let share_btn = make_action("send-to-symbolic", "Share");
    let addalbum_btn = make_action("list-add-symbolic", "Album");
    let delete_btn = make_action("user-trash-symbolic", "Delete");
    let bottombar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .homogeneous(true)
        .css_classes(["mimick-lightbox-bottombar"])
        .build();
    bottombar.append(&share_btn);
    bottombar.append(&addalbum_btn);
    bottombar.append(&delete_btn);
    let bottom_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(160)
        .reveal_child(false)
        .valign(gtk::Align::End)
        .child(&bottombar)
        .build();

    viewer.append(&media_stack);
    // Overlay the chrome bars on top of the viewer.
    let chrome_overlay = gtk::Overlay::builder().build();
    chrome_overlay.set_child(Some(&viewer));
    chrome_overlay.add_overlay(&top_revealer);
    chrome_overlay.add_overlay(&bottom_revealer);

    // ── Details drawer contents ─────────────────────────────────────────
    let details_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(14)
        .margin_bottom(20)
        .margin_start(12)
        .margin_end(12)
        .build();
    let sheet_grabber = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .width_request(36)
        .height_request(4)
        .css_classes(["mimick-sheet-grabber"])
        .build();
    let details_timestamp = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-4"])
        .build();
    let description_group = libadwaita::PreferencesGroup::builder()
        .title("Description")
        .build();
    let description_row = libadwaita::EntryRow::builder()
        .title("Add a description")
        .build();
    description_group.add(&description_row);
    let details_map_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let details_exif = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    let details_loading = gtk::Label::builder()
        .xalign(0.0)
        .label("Loading details…")
        .css_classes(["dim-label"])
        .build();
    details_inner.append(&sheet_grabber);
    details_inner.append(&details_timestamp);
    details_inner.append(&description_group);
    details_inner.append(&details_map_box);
    details_inner.append(&details_loading);
    details_inner.append(&details_exif);
    let details_pane = gtk::ScrolledWindow::builder()
        .child(&details_inner)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(560)
        .build();

    sheet.set_content(Some(&chrome_overlay));
    sheet.set_sheet(Some(&details_pane));
    page.set_child(Some(&sheet));

    // Chrome visibility toggle state.
    let chrome_visible = Rc::new(Cell::new(false));
    let set_chrome = {
        let top_revealer = top_revealer.clone();
        let bottom_revealer = bottom_revealer.clone();
        let chrome_visible = chrome_visible.clone();
        Rc::new(move |show: bool| {
            chrome_visible.set(show);
            top_revealer.set_reveal_child(show);
            bottom_revealer.set_reveal_child(show);
        })
    };

    // Async load generation guard.
    let pos_cell = Rc::new(Cell::new(position));
    let load_gen = Rc::new(Cell::new(0u64));
    let load_into_picture = Rc::new({
        let ui = ui.clone();
        let loader_overlay = loader_overlay.clone();
        let unavailable_overlay = unavailable_overlay.clone();
        let unavailable_filename = unavailable_filename.clone();
        let unavailable_mime = unavailable_mime.clone();
        let unavailable_open = unavailable_open.clone();
        let unavailable_path = unavailable_path.clone();
        let load_gen = load_gen.clone();
        let pic_stack = pic_stack.clone();
        let active_a = active_a.clone();
        let video_player = video_player.clone();
        let media_stack = media_stack.clone();
        move |target: gtk::Picture,
              target_is_a: bool,
              asset_id: String,
              filename: String,
              mime: String,
              local_path: String,
              full_res: bool| {
            let ui = ui.clone();
            let loader = loader_overlay.clone();
            let unavailable_overlay = unavailable_overlay.clone();
            let unavailable_filename = unavailable_filename.clone();
            let unavailable_mime = unavailable_mime.clone();
            let unavailable_open = unavailable_open.clone();
            let unavailable_path = unavailable_path.clone();
            let load_gen = load_gen.clone();
            let pic_stack = pic_stack.clone();
            let active_a = active_a.clone();
            let video_player = video_player.clone();
            let media_stack = media_stack.clone();
            let lightbox_drag_path = lightbox_drag_path.clone();
            let our_gen = load_gen.get().wrapping_add(1);
            load_gen.set(our_gen);
            if media_stack.visible_child_name().as_deref() == Some("video") {
                video_player.set_media_stream(None::<&gtk::MediaStream>);
                media_stack.set_visible_child_name("image");
            }
            unavailable_overlay.set_reveal_child(false);
            unavailable_overlay.set_can_target(false);
            *unavailable_path.borrow_mut() = None;
            *lightbox_drag_path.borrow_mut() = None;
            let is_video =
                crate::media_kinds::asset_kind(&mime) == crate::media_kinds::AssetKind::Video;
            if let Some(texture) = ui
                .ctx
                .thumbnail_cache
                .get_cached(&asset_id, ThumbnailSize::Preview)
            {
                target.set_paintable(Some(&texture));
            }
            let arm_delay_ms: u64 = if local_path.is_empty() { 120 } else { 250 };
            let loader_for_arm = loader.clone();
            let cancel_loader = Rc::new(Cell::new(false));
            let cancel_for_arm = cancel_loader.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(arm_delay_ms), move || {
                if !cancel_for_arm.get() {
                    loader_for_arm.set_reveal_child(true);
                }
                glib::ControlFlow::Break
            });
            glib::MainContext::default().spawn_local(async move {
                let is_current = || load_gen.get() == our_gen;
                let commit_visible = || {
                    let pic_stack = pic_stack.clone();
                    let active_a = active_a.clone();
                    glib::idle_add_local_once(move || {
                        pic_stack.set_visible_child_name(if target_is_a { "a" } else { "b" });
                        active_a.set(target_is_a);
                    });
                };
                let show_unavailable = |path: Option<String>| {
                    unavailable_filename.set_label(&filename);
                    unavailable_mime.set_label(&mime);
                    unavailable_open.set_visible(path.is_some());
                    *unavailable_path.borrow_mut() = path;
                    unavailable_overlay.set_can_target(true);
                    unavailable_overlay.set_reveal_child(true);
                };
                if is_video {
                    let thumb_result = ui
                        .ctx
                        .thumbnail_cache
                        .load_thumbnail(&asset_id, ThumbnailSize::Preview)
                        .await;
                    if !is_current() {
                        return;
                    }
                    if let Ok(texture) = thumb_result {
                        target.set_paintable(Some(&texture));
                    }
                    commit_visible();
                    cancel_loader.set(true);
                    loader.set_reveal_child(false);
                    let uri = if !local_path.is_empty() {
                        Some(format!("file://{}", local_path))
                    } else {
                        ui.ctx.api_client.video_playback_uri(&asset_id).await
                    };
                    if !is_current() {
                        return;
                    }
                    if let Some(uri) = uri {
                        let gio_file = gtk::gio::File::for_uri(&uri);
                        let mf = gtk::MediaFile::for_file(&gio_file);
                        video_player.set_media_stream(Some(mf.upcast_ref::<gtk::MediaStream>()));
                        media_stack.set_visible_child_name("video");
                    }
                    return;
                }
                if !local_path.is_empty() {
                    if let Some(texture) =
                        load_texture_oriented(std::path::Path::new(&local_path)).await
                    {
                        if is_current() {
                            target.set_paintable(Some(&texture));
                            *lightbox_drag_path.borrow_mut() =
                                Some(std::path::PathBuf::from(&local_path));
                            commit_visible();
                            cancel_loader.set(true);
                            loader.set_reveal_child(false);
                        }
                        return;
                    }
                    if asset_id.starts_with(crate::library::LOCAL_ID_PREFIX) {
                        if is_current() {
                            show_unavailable(Some(local_path));
                            commit_visible();
                            cancel_loader.set(true);
                            loader.set_reveal_child(false);
                        }
                        return;
                    }
                }
                if full_res {
                    if let Some(cache_dir) = crate::profile::cache_dir().map(|p| p.join("preview"))
                    {
                        let _ = std::fs::create_dir_all(&cache_dir);
                        let temp = original_preview_cache_path(&cache_dir, &asset_id, &filename);
                        if !temp.exists() {
                            let download_result = {
                                begin_download_session(&ui.ctx, format!("preview {asset_id}"));
                                let progress = track_download_item(
                                    &ui.ctx,
                                    asset_id.clone(),
                                    Some(format!("preview {asset_id}")),
                                    None,
                                );
                                let result = ui
                                    .ctx
                                    .api_client
                                    .download_original_to_file(&asset_id, &temp, Some(progress))
                                    .await;
                                finish_download_item(&ui.ctx, &asset_id);
                                result
                            };
                            if let Err(err) = download_result {
                                log::warn!("Lightbox original fetch failed: {}", err);
                                if is_current() {
                                    show_unavailable(None);
                                    commit_visible();
                                    cancel_loader.set(true);
                                    loader.set_reveal_child(false);
                                }
                                return;
                            }
                        }
                        let decoded = load_texture_oriented(&temp).await;
                        if !is_current() {
                            return;
                        }
                        if let Some(texture) = decoded {
                            target.set_paintable(Some(&texture));
                            *lightbox_drag_path.borrow_mut() =
                                drag_export_path(&asset_id, &filename, &temp);
                        } else {
                            show_unavailable(Some(temp.display().to_string()));
                        }
                        commit_visible();
                    } else if is_current() {
                        show_unavailable(None);
                        commit_visible();
                    }
                } else {
                    let thumb_result = ui
                        .ctx
                        .thumbnail_cache
                        .load_thumbnail(&asset_id, ThumbnailSize::Preview)
                        .await;
                    if !is_current() {
                        return;
                    }
                    match thumb_result {
                        Ok(texture) => target.set_paintable(Some(&texture)),
                        Err(_) => show_unavailable(None),
                    }
                    commit_visible();
                }
                if is_current() {
                    cancel_loader.set(true);
                    loader.set_reveal_child(false);
                }
            });
        }
    });

    // Full-resolution preference stored per-viewer; toggled via the ⋮ menu.
    let full_res_pref = Rc::new(Cell::new(resolution_full));
    // Current asset's remote id / favorite / description for the action wiring.
    let cur_favorite = Rc::new(Cell::new(false));

    // -1 = back/prev (slide right), +1 = forward/next (slide left), 0 = none.
    let nav_dir = Rc::new(Cell::new(0i8));
    let render = Rc::new({
        let ui = ui.clone();
        let pos_cell = pos_cell.clone();
        let load_into_picture = load_into_picture.clone();
        let full_res_pref = full_res_pref.clone();
        let pic_stack = pic_stack.clone();
        let picture_a = picture_a.clone();
        let picture_b = picture_b.clone();
        let scrolled_picture = scrolled_picture.clone();
        let active_a = active_a.clone();
        let zoom_level = zoom_level.clone();
        let nav_dir = nav_dir.clone();
        let datetime_label = datetime_label.clone();
        let favorite_btn = favorite_btn.clone();
        let cur_favorite = cur_favorite.clone();
        let details_timestamp = details_timestamp.clone();
        let description_row = description_row.clone();
        let details_map_box = details_map_box.clone();
        let details_exif = details_exif.clone();
        let details_loading = details_loading.clone();
        move || {
            let pos = pos_cell.get();
            let Some(item) = ui.grid.model.item(pos).and_downcast::<AssetObject>() else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let filename = item.property::<String>("filename");
            let local_path = item.property::<String>("local-path");
            let mime = item.property::<String>("mime-type");
            let created = item.property::<String>("created-at");

            datetime_label.set_label(&format_topbar_datetime(&created));
            details_timestamp.set_label(&format_full_timestamp(&created));

            // Reset per-asset details UI.
            while let Some(c) = details_exif.first_child() {
                details_exif.remove(&c);
            }
            details_exif.set_visible(false);
            while let Some(c) = details_map_box.first_child() {
                details_map_box.remove(&c);
            }
            description_row.set_text("");
            favorite_btn.set_active(false);
            favorite_btn.set_icon_name("non-starred-symbolic");
            cur_favorite.set(false);

            let is_local = !local_path.is_empty() && asset_id.starts_with(LOCAL_ID_PREFIX);
            let is_video =
                crate::media_kinds::asset_kind(&mime) == crate::media_kinds::AssetKind::Video;

            // Load into the inactive picture; commit the slide once ready.
            let target_is_a = !active_a.get();
            let target = if target_is_a {
                picture_a.clone()
            } else {
                picture_b.clone()
            };
            zoom_level.set(1.0);
            apply_lightbox_zoom(&target, &scrolled_picture, 1.0);
            // Clear any leftover drag-to-dismiss translation/opacity so the new
            // photo is centered and fully opaque.
            pic_stack.set_margin_top(0);
            pic_stack.set_opacity(1.0);
            pic_stack.set_transition_type(match nav_dir.get() {
                1 => gtk::StackTransitionType::SlideLeft,
                -1 => gtk::StackTransitionType::SlideRight,
                _ => gtk::StackTransitionType::None,
            });
            // Videos and local files always use their native source; the
            // full-resolution preference only affects remote still images.
            let want_full = full_res_pref.get() && !is_local && !is_video;
            (*load_into_picture)(
                target,
                target_is_a,
                asset_id.clone(),
                filename.clone(),
                mime.clone(),
                local_path.clone(),
                want_full,
            );
            nav_dir.set(0);

            if is_local && !is_video {
                // Local image: parse EXIF on a blocking worker.
                details_loading.set_visible(true);
                let pos_cell_async = pos_cell.clone();
                let details_loading = details_loading.clone();
                let details_exif = details_exif.clone();
                let details_map_box = details_map_box.clone();
                let local_path_async = local_path.clone();
                glib::MainContext::default().spawn_local(async move {
                    let cache_root = local_exif::cache_root();
                    let path_for_blocking = local_path_async.clone();
                    let probed = tokio::task::spawn_blocking(move || {
                        let path = std::path::Path::new(&path_for_blocking);
                        let meta = std::fs::metadata(path).ok();
                        let file_size = meta.as_ref().map(|m| m.len());
                        let mtime_iso = meta
                            .as_ref()
                            .and_then(|m| m.modified().ok())
                            .map(systemtime_to_rfc3339);
                        let dims = gtk::gdk_pixbuf::Pixbuf::file_info(path)
                            .map(|(_, w, h)| (w, h))
                            .and_then(|(w, h)| {
                                let w = u32::try_from(w).ok()?;
                                let h = u32::try_from(h).ok()?;
                                Some((w, h))
                            });
                        let exif = local_exif::load_or_extract(&cache_root, path);
                        LocalProbe {
                            exif,
                            file_size,
                            mtime_iso,
                            dims,
                        }
                    })
                    .await
                    .ok();
                    if pos_cell_async.get() != pos {
                        return;
                    }
                    details_loading.set_visible(false);
                    let Some(probe) = probed else {
                        return;
                    };
                    if probe.exif.is_none()
                        && probe.file_size.is_none()
                        && probe.dims.is_none()
                        && probe.mtime_iso.is_none()
                    {
                        return;
                    }
                    let mut info = probe
                        .exif
                        .as_ref()
                        .map_or_else(LocalExif::default, Clone::clone);
                    if info.image_width.is_none() {
                        info.image_width = probe.dims.map(|(w, _)| w);
                    }
                    if info.image_height.is_none() {
                        info.image_height = probe.dims.map(|(_, h)| h);
                    }
                    let used_mtime_fallback = info.date_time_original.is_none();
                    if used_mtime_fallback {
                        info.date_time_original = probe.mtime_iso.clone();
                    }
                    let taken_label = if used_mtime_fallback {
                        "Modified"
                    } else {
                        "Taken"
                    };
                    let projected = exif_from_local(&info, probe.file_size);
                    if let (Some(lat), Some(lon)) = (projected.latitude, projected.longitude)
                        && let Some(map) = build_minimap(lat, lon)
                    {
                        details_map_box.append(&map);
                    }
                    fill_exif_box(&details_exif, &projected, taken_label);
                    details_exif.set_visible(true);
                });
                return;
            }

            // Remote asset: fetch full details for EXIF, favorite, description.
            details_loading.set_visible(true);
            let pos_cell_async = pos_cell.clone();
            let ui_async = ui.clone();
            let details_loading = details_loading.clone();
            let details_exif = details_exif.clone();
            let details_map_box = details_map_box.clone();
            let description_row = description_row.clone();
            let favorite_btn = favorite_btn.clone();
            let cur_favorite = cur_favorite.clone();
            let asset_id_async = asset_id.clone();
            glib::MainContext::default().spawn_local(async move {
                let result = ui_async
                    .ctx
                    .api_client
                    .fetch_asset_details(&asset_id_async)
                    .await;
                if pos_cell_async.get() != pos {
                    return;
                }
                details_loading.set_visible(false);
                let Ok(details) = result else { return };
                favorite_btn.set_active(details.is_favorite);
                favorite_btn.set_icon_name(if details.is_favorite {
                    "starred-symbolic"
                } else {
                    "non-starred-symbolic"
                });
                cur_favorite.set(details.is_favorite);
                if let Some(desc) = &details.description {
                    description_row.set_text(desc);
                } else if let Some(exif) = &details.exif_info
                    && let Some(desc) = &exif.description
                {
                    description_row.set_text(desc);
                }
                if let Some(exif) = details.exif_info {
                    if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude)
                        && let Some(map) = build_minimap(lat, lon)
                    {
                        details_map_box.append(&map);
                    }
                    fill_exif_box(&details_exif, &exif, "Taken");
                    details_exif.set_visible(true);
                }
            });
        }
    });

    (*render)();

    // ── Navigation helpers ──────────────────────────────────────────────
    let goto_prev = Rc::new(clone!(
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        nav_dir,
        #[strong]
        set_chrome,
        move || {
            let pos = pos_cell.get();
            if pos > 0 {
                pos_cell.set(pos - 1);
                nav_dir.set(-1);
                (*set_chrome)(false);
                (*render)();
            }
        }
    ));
    let goto_next = Rc::new(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        nav_dir,
        #[strong]
        set_chrome,
        move || {
            let pos = pos_cell.get();
            if pos + 1 < ui.grid.model.n_items() {
                pos_cell.set(pos + 1);
                nav_dir.set(1);
                (*set_chrome)(false);
                (*render)();
                return;
            }
            let next_request = ui.ctx.library_state.lock().load_next_page_if_needed();
            let Some(req) = next_request else {
                return;
            };
            let model = ui.grid.model.clone();
            let pos_cell_h = pos_cell.clone();
            let render_h = render.clone();
            let nav_dir_h = nav_dir.clone();
            let prev_count = model.n_items();
            let handler_id = Rc::new(std::cell::RefCell::new(None::<glib::SignalHandlerId>));
            let handler_id_clone = handler_id.clone();
            let id = model.connect_items_changed(move |m, _, _, _| {
                if m.n_items() <= prev_count {
                    return;
                }
                let pos = pos_cell_h.get();
                if pos + 1 < m.n_items() {
                    pos_cell_h.set(pos + 1);
                    nav_dir_h.set(1);
                    (*render_h)();
                }
                if let Some(hid) = handler_id_clone.borrow_mut().take() {
                    m.disconnect(hid);
                }
            });
            *handler_id.borrow_mut() = Some(id);
            load_source_page(ui.clone(), req, true);
        }
    ));

    // Exit the viewer (back button / swipe-down / Escape).
    let exit_viewer = Rc::new(clone!(
        #[strong]
        ui,
        move || {
            ui.nav.pop();
        }
    ));

    back_btn.connect_clicked(clone!(
        #[strong]
        exit_viewer,
        move |_| (*exit_viewer)()
    ));

    // Toggle chrome on single tap (no drag). GestureClick with n_press==1.
    let tap = gtk::GestureClick::new();
    tap.set_button(gtk::gdk::BUTTON_PRIMARY);
    tap.connect_released(clone!(
        #[strong]
        set_chrome,
        #[strong]
        chrome_visible,
        #[strong]
        zoom_level,
        move |gesture, n_press, _, _| {
            // Ignore taps while zoomed in (those pan) and multi-press.
            if n_press != 1 || (zoom_level.get() - 1.0).abs() > 0.01 {
                return;
            }
            (*set_chrome)(!chrome_visible.get());
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    ));
    pic_stack.add_controller(tap);

    // Favorite toggle.
    favorite_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        cur_favorite,
        move |btn| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                return;
            }
            let want = !cur_favorite.get();
            cur_favorite.set(want);
            btn.set_icon_name(if want {
                "starred-symbolic"
            } else {
                "non-starred-symbolic"
            });
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Err(err) = ui.ctx.api_client.set_asset_favorite(&asset_id, want).await {
                    log::warn!("Set favorite failed: {}", err);
                }
            });
        }
    ));

    // Description commit.
    description_row.connect_apply(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |row| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                return;
            }
            let text = row.text().to_string();
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Err(err) = ui.ctx.api_client.set_asset_description(&asset_id, &text).await
                {
                    log::warn!("Set description failed: {}", err);
                }
            });
        }
    ));

    // Delete action (confirm → soft-delete → pop viewer + refresh).
    delete_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        exit_viewer,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                show_lightbox_alert(&ui, "Cannot delete", "This is a local-only asset.");
                return;
            }
            let dialog = libadwaita::AlertDialog::builder()
                .heading("Move to trash?")
                .body("The item can be restored from the Immich trash.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("delete", "Move to trash");
            dialog.set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let ui2 = ui.clone();
            let exit_viewer = exit_viewer.clone();
            dialog.connect_response(None, move |dlg, response| {
                dlg.close();
                if response != "delete" {
                    return;
                }
                let ui = ui2.clone();
                let exit_viewer = exit_viewer.clone();
                let ids = vec![asset_id.clone()];
                glib::MainContext::default().spawn_local(async move {
                    match ui.ctx.api_client.delete_assets(&ids).await {
                        Ok(()) => {
                            (*exit_viewer)();
                            super::refresh_library_after_mutation(ui.clone(), true);
                        }
                        Err(err) => {
                            log::error!("Delete failed: {}", err);
                            show_lightbox_alert(&ui, "Delete failed", &err);
                        }
                    }
                });
            });
            dialog.present(Some(&ui.window));
        }
    ));

    // Add-to-album action.
    addalbum_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                show_lightbox_alert(&ui, "Cannot add", "This is a local-only asset.");
                return;
            }
            show_add_to_album_dialog(ui.clone(), asset_id);
        }
    ));

    // Share action: export the original then hand off to the portal / default.
    share_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let remote_id = item.property::<String>("remote-id");
            let local_path = item.property::<String>("local-path");
            let filename = item.property::<String>("filename");
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                match super::context_menu::ensure_original_asset_path(
                    &ui, &asset_id, &remote_id, &local_path, &filename,
                )
                .await
                {
                    Ok(path) => open_local_with_default_app(&path.display().to_string()),
                    Err(err) => show_lightbox_alert(&ui, "Share failed", &err),
                }
            });
        }
    ));

    // ── Menu actions (download / copy / open-with / full-res) ───────────
    let action_group = gtk::gio::SimpleActionGroup::new();
    let download_action = gtk::gio::SimpleAction::new("download", None);
    download_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            if let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>() {
                let asset_id = item.property::<String>("id");
                let filename = item.property::<String>("filename");
                if !asset_id.starts_with(LOCAL_ID_PREFIX) {
                    start_download(ui.clone(), asset_id, filename);
                }
            }
        }
    ));
    action_group.add_action(&download_action);
    let copy_action = gtk::gio::SimpleAction::new("copy", None);
    copy_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            super::context_menu::copy_current_asset(ui.clone(), pos_cell.get());
        }
    ));
    action_group.add_action(&copy_action);
    let openwith_action = gtk::gio::SimpleAction::new("openwith", None);
    openwith_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            super::context_menu::open_current_asset_in_default_app(ui.clone(), pos_cell.get());
        }
    ));
    action_group.add_action(&openwith_action);
    let fullres_action = gtk::gio::SimpleAction::new_stateful(
        "fullres",
        None,
        &full_res_pref.get().to_variant(),
    );
    fullres_action.connect_activate(clone!(
        #[strong]
        full_res_pref,
        #[strong]
        render,
        move |act, _| {
            let new = !full_res_pref.get();
            full_res_pref.set(new);
            act.set_state(&new.to_variant());
            (*render)();
        }
    ));
    action_group.add_action(&fullres_action);
    page.insert_action_group("lightbox", Some(&action_group));

    // ── Zoom / pan machinery ────────────────────────────────────────────
    let active_picture = clone!(
        #[strong]
        active_a,
        #[strong]
        picture_a,
        #[strong]
        picture_b,
        move || -> gtk::Picture {
            if active_a.get() {
                picture_a.clone()
            } else {
                picture_b.clone()
            }
        }
    );
    let cursor_pos: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(clone!(
        #[strong]
        cursor_pos,
        move |_, x, y| {
            cursor_pos.set(Some((x, y)));
        }
    ));
    motion.connect_leave(clone!(
        #[strong]
        cursor_pos,
        move |_| {
            cursor_pos.set(None);
        }
    ));
    scrolled_picture.add_controller(motion);

    let set_zoom = Rc::new(clone!(
        #[strong]
        zoom_level,
        #[strong]
        active_picture,
        #[strong]
        scrolled_picture,
        #[strong]
        cursor_pos,
        move |z: f64| {
            let z_new = z.clamp(1.0, 10.0);
            let z_old = zoom_level.get();
            if (z_new - z_old).abs() < 0.0001 {
                return;
            }
            let viewer_w = scrolled_picture.width().max(1) as f64;
            let viewer_h = scrolled_picture.height().max(1) as f64;
            let (fx, fy) = cursor_pos
                .get()
                .filter(|&(x, y)| x >= 0.0 && y >= 0.0 && x <= viewer_w && y <= viewer_h)
                .unwrap_or((viewer_w / 2.0, viewer_h / 2.0));
            let hadj = scrolled_picture.hadjustment();
            let vadj = scrolled_picture.vadjustment();
            let scroll_x = hadj.value();
            let scroll_y = vadj.value();
            let ratio = z_new / z_old.max(0.0001);
            let target_scroll_x = (scroll_x + fx) * ratio - fx;
            let target_scroll_y = (scroll_y + fy) * ratio - fy;
            zoom_level.set(z_new);
            let content = apply_lightbox_zoom(&active_picture(), &scrolled_picture, z_new);
            if let Some((cw, ch)) = content {
                hadj.set_upper(cw.max(viewer_w));
                hadj.set_page_size(viewer_w);
                vadj.set_upper(ch.max(viewer_h));
                vadj.set_page_size(viewer_h);
            } else {
                hadj.set_upper(viewer_w);
                hadj.set_page_size(viewer_w);
                vadj.set_upper(viewer_h);
                vadj.set_page_size(viewer_h);
            }
            hadj.set_value(target_scroll_x);
            vadj.set_value(target_scroll_y);
        }
    ));
    let zoom_by = Rc::new(clone!(
        #[strong]
        zoom_level,
        #[strong]
        set_zoom,
        move |factor: f64| {
            (*set_zoom)(zoom_level.get() * factor);
        }
    ));
    let zoom_reset = Rc::new(clone!(
        #[strong]
        set_zoom,
        move || {
            (*set_zoom)(1.0);
        }
    ));

    // Trackpad pinch-to-zoom.
    let pinch = gtk::GestureZoom::new();
    let pinch_start = Rc::new(Cell::new(1.0_f64));
    pinch.connect_begin(clone!(
        #[strong]
        zoom_level,
        #[strong]
        pinch_start,
        move |_, _| {
            pinch_start.set(zoom_level.get());
        }
    ));
    pinch.connect_scale_changed(clone!(
        #[strong]
        pinch_start,
        #[strong]
        set_zoom,
        move |_, scale| {
            (*set_zoom)(pinch_start.get() * scale);
        }
    ));
    scrolled_picture.add_controller(pinch.clone());

    // Click-and-drag panning when zoomed in.
    let drag_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    let pan_drag = gtk::GestureDrag::new();
    pan_drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    pan_drag.connect_drag_begin(clone!(
        #[strong]
        scrolled_picture,
        #[strong]
        drag_start,
        move |_, _, _| {
            let hadj = scrolled_picture.hadjustment();
            let vadj = scrolled_picture.vadjustment();
            drag_start.set((hadj.value(), vadj.value()));
        }
    ));
    pan_drag.connect_drag_update(clone!(
        #[strong]
        scrolled_picture,
        #[strong]
        drag_start,
        #[strong]
        zoom_level,
        move |_, off_x, off_y| {
            // Only pan when zoomed in; otherwise leave scroll alone so the
            // swipe navigation gesture can interpret the motion.
            if (zoom_level.get() - 1.0).abs() < 0.01 {
                return;
            }
            let (sx0, sy0) = drag_start.get();
            scrolled_picture.hadjustment().set_value(sx0 - off_x);
            scrolled_picture.vadjustment().set_value(sy0 - off_y);
        }
    ));
    scrolled_picture.add_controller(pan_drag.clone());
    pan_drag.group_with(&pinch);

    // Double-click: zoom in 2x toward the click position.
    let double_click = gtk::GestureClick::new();
    double_click.set_button(gtk::gdk::BUTTON_PRIMARY);
    double_click.connect_pressed(clone!(
        #[strong]
        cursor_pos,
        #[strong]
        zoom_level,
        #[strong]
        set_zoom,
        move |_, n_press, x, y| {
            if n_press == 2 {
                cursor_pos.set(Some((x, y)));
                let z = zoom_level.get();
                if (z - 1.0).abs() < 0.01 {
                    (*set_zoom)(2.0);
                } else {
                    (*set_zoom)(1.0);
                }
            }
        }
    ));
    scrolled_picture.add_controller(double_click);

    // Right-click: open the standard asset context menu.
    let right_click = gtk::GestureClick::new();
    right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
    right_click.connect_pressed(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        scrolled_picture,
        move |_, _, x, y| {
            show_asset_context_menu(ui.clone(), &scrolled_picture, pos_cell.get(), x, y);
        }
    ));
    scrolled_picture.add_controller(right_click);

    // ── Directional swipe/drag gesture ──────────────────────────────────
    // A CAPTURE-phase drag on the picture stack that interprets the release
    // direction: horizontal = prev/next, down = dismiss, up = details sheet.
    // Attaching at capture and claiming the sequence stops the enclosing
    // AdwNavigationView from treating a horizontal drag as an edge-swipe pop.
    let nav_drag = gtk::GestureDrag::new();
    nav_drag.set_touch_only(false);
    nav_drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let drag_claimed = Rc::new(Cell::new(false));
    // Tracks whether the *current* drag is an interactive drag-to-dismiss
    // (downward-dominant): once set, `drag_update` translates the image with
    // the finger and `drag_end` decides dismiss-vs-snap-back.
    let drag_dismissing = Rc::new(Cell::new(false));
    // Applies a live downward translation + fade to the picture stack.
    let apply_dismiss_offset = Rc::new(clone!(
        #[strong]
        pic_stack,
        #[strong]
        scrolled_picture,
        move |off_y: f64| {
            let h = scrolled_picture.height().max(1) as f64;
            let clamped = off_y.max(0.0);
            pic_stack.set_margin_top(clamped as i32);
            // Fade toward 0.4 as the image is dragged down; never fully hidden
            // while still on-screen so the drag reads as interactive.
            let fade = (clamped / h).min(0.6);
            pic_stack.set_opacity(1.0 - fade);
        }
    ));
    // Resets the translation/opacity so a freshly loaded photo is centered.
    let reset_dismiss_offset = Rc::new(clone!(
        #[strong]
        pic_stack,
        move || {
            pic_stack.set_margin_top(0);
            pic_stack.set_opacity(1.0);
        }
    ));
    nav_drag.connect_drag_update(clone!(
        #[strong]
        zoom_level,
        #[strong]
        drag_claimed,
        #[strong]
        drag_dismissing,
        #[strong]
        apply_dismiss_offset,
        move |gesture, off_x, off_y| {
            // When zoomed in the pan gesture owns the motion.
            if (zoom_level.get() - 1.0).abs() > 0.01 {
                return;
            }
            if !drag_claimed.get() {
                // Claim once the motion is clearly a directional swipe so we
                // pre-empt the NavigationView edge-swipe on horizontal drags.
                if off_x.abs() > 24.0 || off_y.abs() > 24.0 {
                    drag_claimed.set(true);
                    // A downward-dominant drag becomes an interactive dismiss.
                    drag_dismissing.set(off_y > off_x.abs());
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
            // While dismissing, move the image down with the finger and fade.
            if drag_dismissing.get() {
                apply_dismiss_offset(off_y);
            }
        }
    ));
    nav_drag.connect_drag_end(clone!(
        #[strong]
        goto_prev,
        #[strong]
        goto_next,
        #[strong]
        exit_viewer,
        #[strong]
        sheet,
        #[strong]
        zoom_level,
        #[strong]
        drag_claimed,
        #[strong]
        drag_dismissing,
        #[strong]
        pic_stack,
        #[strong]
        scrolled_picture,
        #[strong]
        apply_dismiss_offset,
        #[strong]
        reset_dismiss_offset,
        move |_, off_x, off_y| {
            let _ = drag_claimed.replace(false);
            let was_dismissing = drag_dismissing.replace(false);
            if (zoom_level.get() - 1.0).abs() > 0.01 {
                return;
            }
            const THRESH: f64 = 60.0;
            const DISMISS_THRESH: f64 = 120.0;
            let ax = off_x.abs();
            let ay = off_y.abs();

            if was_dismissing {
                // Interactive downward dismiss: past the threshold, continue
                // the downward fade-out and pop; otherwise snap back to center.
                let h = scrolled_picture.height().max(1) as f64;
                if off_y > DISMISS_THRESH {
                    let target = CallbackAnimationTarget::new(clone!(
                        #[strong]
                        apply_dismiss_offset,
                        move |v| apply_dismiss_offset(v)
                    ));
                    let anim =
                        TimedAnimation::new(&pic_stack, off_y.max(0.0), h, 160, target);
                    anim.set_easing(libadwaita::Easing::EaseInCubic);
                    anim.connect_done(clone!(
                        #[strong]
                        exit_viewer,
                        #[strong]
                        reset_dismiss_offset,
                        move |_| {
                            (*exit_viewer)();
                            reset_dismiss_offset();
                        }
                    ));
                    anim.play();
                } else {
                    // Snap back: animate the offset (and thus opacity) to 0.
                    let target = CallbackAnimationTarget::new(clone!(
                        #[strong]
                        apply_dismiss_offset,
                        move |v| apply_dismiss_offset(v)
                    ));
                    let anim = TimedAnimation::new(&pic_stack, off_y.max(0.0), 0.0, 200, target);
                    anim.set_easing(libadwaita::Easing::EaseOutCubic);
                    anim.connect_done(clone!(
                        #[strong]
                        reset_dismiss_offset,
                        move |_| reset_dismiss_offset()
                    ));
                    anim.play();
                }
                return;
            }

            if ax < THRESH && ay < THRESH {
                return;
            }
            if ax > ay {
                // Horizontal: right drag → prev, left drag → next.
                if off_x > 0.0 {
                    (*goto_prev)();
                } else {
                    (*goto_next)();
                }
            } else if off_y < 0.0 {
                // Upward drag → open details sheet.
                sheet.set_open(true);
            }
            // Downward drags are handled by the interactive-dismiss branch above.
        }
    ));
    pic_stack.add_controller(nav_drag);

    // ── Keyboard shortcuts ──────────────────────────────────────────────
    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(clone!(
        #[strong]
        exit_viewer,
        #[strong]
        goto_prev,
        #[strong]
        goto_next,
        #[strong]
        zoom_by,
        #[strong]
        zoom_reset,
        #[strong]
        set_chrome,
        #[strong]
        chrome_visible,
        #[strong]
        sheet,
        move |_, key, _, mods| {
            let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            match (ctrl, key) {
                (true, gtk::gdk::Key::plus)
                | (true, gtk::gdk::Key::equal)
                | (true, gtk::gdk::Key::KP_Add) => {
                    (*zoom_by)(1.2);
                    glib::Propagation::Stop
                }
                (true, gtk::gdk::Key::minus) | (true, gtk::gdk::Key::KP_Subtract) => {
                    (*zoom_by)(1.0 / 1.2);
                    glib::Propagation::Stop
                }
                (true, gtk::gdk::Key::_0) | (true, gtk::gdk::Key::KP_0) => {
                    (*zoom_reset)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Left) => {
                    (*goto_prev)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Right) => {
                    (*goto_next)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Up) | (false, gtk::gdk::Key::i) | (false, gtk::gdk::Key::I) => {
                    sheet.set_open(true);
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::space) => {
                    (*set_chrome)(!chrome_visible.get());
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Escape) => {
                    (*exit_viewer)();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    page.add_controller(key_controller);

    // Ctrl+wheel zoom on the picture area.
    let zoom_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    zoom_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    zoom_scroll.connect_scroll(clone!(
        #[strong]
        zoom_by,
        move |ctrl, dx, dy| {
            let mods = ctrl.current_event_state();
            if !mods.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            let delta = if dy != 0.0 { dy } else { dx };
            if delta == 0.0 {
                return glib::Propagation::Proceed;
            }
            let factor = if delta < 0.0 { 1.1 } else { 1.0 / 1.1 };
            (*zoom_by)(factor);
            glib::Propagation::Stop
        }
    ));
    scrolled_picture.add_controller(zoom_scroll);

    ui.nav.push(&page);
}

#[cfg(test)]
mod tests {
    use super::original_preview_cache_path;

    #[test]
    fn original_preview_cache_path_keeps_decoder_extension() {
        let cache = std::path::Path::new("/tmp/previews");
        assert_eq!(
            original_preview_cache_path(cache, "remote-id", "PXL_20250516_222429137.dng"),
            cache.join("remote-id.dng")
        );
        assert_eq!(
            original_preview_cache_path(cache, "remote-id", "extensionless"),
            cache.join("remote-id.bin")
        );
    }
}
