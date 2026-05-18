//! Lightbox image viewer: full-screen preview with zoom, pan, EXIF details, and keyboard navigation.

use std::cell::Cell;
use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::ThumbnailSize;
use crate::library::asset_object::AssetObject;

use super::context_menu::show_asset_context_menu;
use super::download::{
    begin_download_session, finish_download_item, start_download, track_download_item,
};
use super::{LOCAL_ID_PREFIX, LibraryWindowUi, load_source_page, load_texture_oriented};

fn fill_exif_box(container: &gtk::Box, exif: &crate::api_client::ExifInfo) {
    let mut rows: Vec<(String, String)> = Vec::new();
    let dims = match (exif.exif_image_width, exif.exif_image_height) {
        (Some(w), Some(h)) => Some(format!("{} × {}", w, h)),
        _ => None,
    };
    if let Some(d) = dims {
        rows.push(("Dimensions".into(), d));
    }
    if let Some(size) = exif.file_size_in_byte {
        rows.push(("Size".into(), format_bytes(size)));
    }
    if let Some(dt) = &exif.date_time_original {
        rows.push(("Taken".into(), format_datetime_display(dt)));
    }
    let camera = match (&exif.make, &exif.model) {
        (Some(m), Some(n)) => Some(format!("{} {}", m, n)),
        (Some(m), None) => Some(m.clone()),
        (None, Some(n)) => Some(n.clone()),
        _ => None,
    };
    if let Some(c) = camera {
        rows.push(("Camera".into(), c));
    }
    if let Some(l) = &exif.lens_model {
        rows.push(("Lens".into(), l.clone()));
    }
    let mut shot = Vec::new();
    if let Some(f) = exif.f_number {
        shot.push(format!("ƒ/{:.1}", f));
    }
    if let Some(et) = &exif.exposure_time {
        shot.push(et.clone());
    }
    if let Some(iso) = exif.iso {
        shot.push(format!("ISO {}", iso));
    }
    if let Some(focal) = exif.focal_length {
        shot.push(format!("{:.0}mm", focal));
    }
    if !shot.is_empty() {
        rows.push(("Exposure".into(), shot.join(" · ")));
    }
    let location = match (&exif.city, &exif.state, &exif.country) {
        (Some(c), Some(s), Some(co)) => Some(format!("{}, {}, {}", c, s, co)),
        (Some(c), None, Some(co)) => Some(format!("{}, {}", c, co)),
        (Some(c), _, _) => Some(c.clone()),
        (_, Some(s), Some(co)) => Some(format!("{}, {}", s, co)),
        (_, _, Some(co)) => Some(co.clone()),
        _ => None,
    };
    if let Some(loc) = location {
        rows.push(("Location".into(), loc));
    }
    if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude) {
        rows.push(("GPS".into(), format!("{:.5}, {:.5}", lat, lon)));
    }
    if let Some(desc) = &exif.description
        && !desc.is_empty()
    {
        rows.push(("Description".into(), desc.clone()));
    }

    for (key, value) in rows {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let k = gtk::Label::builder()
            .label(&key)
            .xalign(0.0)
            .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
            .build();
        let v = gtk::Label::builder()
            .label(&value)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(36)
            .selectable(true)
            .build();
        row.append(&k);
        row.append(&v);
        container.append(&row);
    }
}

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

/// Apply zoom to a lightbox Picture. Zoom is fit-relative: 1.0 = the size the
/// texture would occupy inside `viewer` under Contain layout. > 1.0 overflows
/// the viewer for panning. At 1.0 we restore (-1, -1) so the picture
/// auto-resizes with the window.
fn apply_lightbox_zoom(picture: &gtk::Picture, viewer: &gtk::ScrolledWindow, zoom: f64) {
    if (zoom - 1.0).abs() < 0.001 {
        picture.set_size_request(-1, -1);
        return;
    }
    let Some(paintable) = picture.paintable() else {
        picture.set_size_request(-1, -1);
        return;
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
    picture.set_size_request((fit_w * zoom) as i32, (fit_h * zoom) as i32);
}

pub(super) fn open_lightbox(ui: Rc<LibraryWindowUi>, position: u32) {
    let Some(item) = ui.grid.model.item(position).and_downcast::<AssetObject>() else {
        return;
    };
    let initial_filename = item.property::<String>("filename");

    let page = libadwaita::NavigationPage::builder()
        .title(&initial_filename)
        .can_pop(true)
        .build();
    let toolbar = libadwaita::ToolbarView::builder().build();
    let header = libadwaita::HeaderBar::builder()
        .show_back_button(true)
        .build();
    let prev_btn = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous (Left)")
        .build();
    let next_btn = gtk::Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next (Right)")
        .build();
    let details_btn = gtk::ToggleButton::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Toggle details (I)")
        .active(false)
        .build();
    header.pack_start(&prev_btn);
    header.pack_start(&next_btn);
    header.pack_end(&details_btn);
    toolbar.add_top_bar(&header);

    let body = libadwaita::OverlaySplitView::builder()
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        .enable_show_gesture(true)
        .enable_hide_gesture(true)
        .build();
    let viewer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(16)
        .margin_start(8)
        .margin_end(8)
        .hexpand(true)
        .build();
    // Two picture widgets in a stack so navigation can slide between them.
    let picture_a = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .build();
    let picture_b = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
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
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&pic_stack)
        .vexpand(true)
        .hexpand(true)
        .build();
    let active_a = Rc::new(Cell::new(true));
    let zoom_level = Rc::new(Cell::new(1.0_f64));
    let initial_full = ui.ctx.config.read().data.library_preview_full_resolution;
    let resolution_toggle = gtk::ToggleButton::builder()
        .label(if initial_full { "Original" } else { "Preview" })
        .tooltip_text("Toggle preview vs original full-resolution image")
        .active(initial_full)
        .build();
    let download = gtk::Button::builder().label("Download").build();
    let zoom_out_btn = gtk::Button::builder()
        .icon_name("zoom-out-symbolic")
        .tooltip_text("Zoom out (Ctrl+-)")
        .build();
    let zoom_in_btn = gtk::Button::builder()
        .icon_name("zoom-in-symbolic")
        .tooltip_text("Zoom in (Ctrl++)")
        .build();
    let zoom_reset_btn = gtk::Button::builder()
        .label("100%")
        .tooltip_text("Reset zoom (Ctrl+0)")
        .build();
    let zoom_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(vec!["linked".to_string()])
        .build();
    zoom_group.append(&zoom_out_btn);
    zoom_group.append(&zoom_reset_btn);
    zoom_group.append(&zoom_in_btn);
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let actions_spacer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .build();
    actions.append(&zoom_group);
    actions.append(&actions_spacer);
    actions.append(&resolution_toggle);
    actions.append(&download);
    viewer.append(&scrolled_picture);
    viewer.append(&actions);

    let details_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let details_pane = gtk::ScrolledWindow::builder()
        .child(&details_inner)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .hexpand(false)
        .min_content_width(280)
        .max_content_width(320)
        .css_classes(vec!["mimick-details-pane".to_string()])
        .build();
    let details_filename = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(28)
        .css_classes(vec!["title-3".to_string()])
        .build();
    let details_summary = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(28)
        .build();
    let details_loading = gtk::Label::builder()
        .xalign(0.0)
        .label("Loading details…")
        .css_classes(vec!["dim-label".to_string()])
        .build();
    let details_exif = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .visible(false)
        .build();
    details_inner.append(&details_filename);
    details_inner.append(&details_summary);
    details_inner.append(&details_loading);
    details_inner.append(&details_exif);

    body.set_content(Some(&viewer));
    body.set_sidebar(Some(&details_pane));
    toolbar.set_content(Some(&body));
    page.set_child(Some(&toolbar));

    details_btn
        .bind_property("active", &body, "show-sidebar")
        .sync_create()
        .bidirectional()
        .build();
    ui.split
        .bind_property("collapsed", &body, "collapsed")
        .sync_create()
        .build();

    let pos_cell = Rc::new(Cell::new(position));
    let load_into_picture = Rc::new({
        let ui = ui.clone();
        move |target: gtk::Picture, asset_id: String, local_path: String, full_res: bool| {
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                if !local_path.is_empty() {
                    if let Some(texture) = load_texture_oriented(std::path::Path::new(&local_path))
                    {
                        target.set_paintable(Some(&texture));
                    }
                    return;
                }
                if full_res {
                    if let Some(cache_dir) = crate::profile::cache_dir().map(|p| p.join("preview"))
                    {
                        let _ = std::fs::create_dir_all(&cache_dir);
                        let temp = cache_dir.join(format!("{}.bin", asset_id));
                        if !temp.exists()
                            && let Err(err) = {
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
                            }
                        {
                            log::warn!("Lightbox original fetch failed: {}", err);
                            return;
                        }
                        if let Some(texture) = load_texture_oriented(&temp) {
                            target.set_paintable(Some(&texture));
                        }
                    }
                } else if let Ok(texture) = ui
                    .ctx
                    .thumbnail_cache
                    .load_thumbnail(&asset_id, ThumbnailSize::Preview)
                    .await
                {
                    target.set_paintable(Some(&texture));
                }
            });
        }
    });

    // -1 = back/prev (slide right), +1 = forward/next (slide left), 0 = no transition
    let nav_dir = Rc::new(Cell::new(0i8));
    let render = Rc::new({
        let ui = ui.clone();
        let page = page.clone();
        let pos_cell = pos_cell.clone();
        let load_into_picture = load_into_picture.clone();
        let resolution_toggle = resolution_toggle.clone();
        let download = download.clone();
        let prev_btn = prev_btn.clone();
        let next_btn = next_btn.clone();
        let details_filename = details_filename.clone();
        let details_summary = details_summary.clone();
        let details_loading = details_loading.clone();
        let details_exif = details_exif.clone();
        let pic_stack = pic_stack.clone();
        let picture_a = picture_a.clone();
        let picture_b = picture_b.clone();
        let scrolled_picture = scrolled_picture.clone();
        let active_a = active_a.clone();
        let zoom_level = zoom_level.clone();
        let zoom_reset_btn = zoom_reset_btn.clone();
        let nav_dir = nav_dir.clone();
        move || {
            let pos = pos_cell.get();
            let n = ui.grid.model.n_items();
            let Some(item) = ui.grid.model.item(pos).and_downcast::<AssetObject>() else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let filename = item.property::<String>("filename");
            let local_path = item.property::<String>("local-path");
            let mime = item.property::<String>("mime-type");
            let created = item.property::<String>("created-at");
            let sync_state = item.property::<u32>("sync-state");

            page.set_title(&filename);
            details_filename.set_label(&filename);
            let sync_label = match sync_state {
                2 => "On Immich and locally",
                1 => "Local only",
                _ => "On Immich only",
            };
            details_summary.set_label(&format!(
                "{} · {}\nCreated: {}",
                mime,
                sync_label,
                format_datetime_display(&created)
            ));

            while let Some(c) = details_exif.first_child() {
                details_exif.remove(&c);
            }
            details_exif.set_visible(false);

            prev_btn.set_sensitive(pos > 0);
            next_btn.set_sensitive(pos + 1 < n);

            let is_local = !local_path.is_empty() && asset_id.starts_with(LOCAL_ID_PREFIX);
            resolution_toggle.set_visible(!is_local);
            download.set_visible(!is_local);

            // Pick the *inactive* picture to load into, then transition to it.
            let target_is_a = !active_a.get();
            let target = if target_is_a {
                picture_a.clone()
            } else {
                picture_b.clone()
            };
            zoom_level.set(1.0);
            apply_lightbox_zoom(&target, &scrolled_picture, 1.0);
            zoom_reset_btn.set_label("100%");
            pic_stack.set_transition_type(match nav_dir.get() {
                1 => gtk::StackTransitionType::SlideLeft,
                -1 => gtk::StackTransitionType::SlideRight,
                _ => gtk::StackTransitionType::None,
            });
            (*load_into_picture)(
                target,
                asset_id.clone(),
                local_path,
                resolution_toggle.is_active(),
            );
            pic_stack.set_visible_child_name(if target_is_a { "a" } else { "b" });
            active_a.set(target_is_a);
            nav_dir.set(0);

            if is_local {
                details_loading.set_visible(false);
                return;
            }

            details_loading.set_visible(true);
            let pos_cell_async = pos_cell.clone();
            let ui_async = ui.clone();
            let details_loading = details_loading.clone();
            let details_exif = details_exif.clone();
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
                if let Some(exif) = details.exif_info {
                    fill_exif_box(&details_exif, &exif);
                    details_exif.set_visible(true);
                }
            });
        }
    });

    (*render)();

    prev_btn.connect_clicked(clone!(
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        nav_dir,
        move |_| {
            let pos = pos_cell.get();
            if pos > 0 {
                pos_cell.set(pos - 1);
                nav_dir.set(-1);
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
        next_btn,
        #[strong]
        nav_dir,
        move || {
            let pos = pos_cell.get();
            if pos + 1 < ui.grid.model.n_items() {
                pos_cell.set(pos + 1);
                nav_dir.set(1);
                (*render)();
                return;
            }
            let next_request = ui.ctx.library_state.lock().load_next_page_if_needed();
            let Some(req) = next_request else {
                return;
            };
            next_btn.set_sensitive(false);
            let model = ui.grid.model.clone();
            let pos_cell_h = pos_cell.clone();
            let render_h = render.clone();
            let next_btn_h = next_btn.clone();
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
                next_btn_h.set_sensitive(true);
                if let Some(hid) = handler_id_clone.borrow_mut().take() {
                    m.disconnect(hid);
                }
            });
            *handler_id.borrow_mut() = Some(id);
            load_source_page(ui.clone(), req, true);
        }
    ));

    next_btn.connect_clicked(clone!(
        #[strong]
        goto_next,
        move |_| (*goto_next)()
    ));

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

    // Track cursor position over the picture area so zoom can be focal-point
    // aware. None when the cursor is outside the viewer; falls back to centre.
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
        zoom_reset_btn,
        #[strong]
        cursor_pos,
        move |z: f64| {
            let z_new = z.clamp(1.0, 10.0);
            let z_old = zoom_level.get();
            if (z_new - z_old).abs() < 0.0001 {
                zoom_reset_btn.set_label(&format!("{}%", (z_new * 100.0).round() as i32));
                return;
            }

            // Pick the focal point: cursor if inside the viewer, else centre.
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
            apply_lightbox_zoom(&active_picture(), &scrolled_picture, z_new);
            zoom_reset_btn.set_label(&format!("{}%", (z_new * 100.0).round() as i32));

            // Defer scroll-position update until after layout has run, so the
            // ScrolledWindow's adjustment ranges reflect the new picture size.
            glib::idle_add_local_once(move || {
                hadj.set_value(target_scroll_x);
                vadj.set_value(target_scroll_y);
            });
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

    zoom_in_btn.connect_clicked(clone!(
        #[strong]
        zoom_by,
        move |_| (*zoom_by)(1.2)
    ));
    zoom_out_btn.connect_clicked(clone!(
        #[strong]
        zoom_by,
        move |_| (*zoom_by)(1.0 / 1.2)
    ));
    zoom_reset_btn.connect_clicked(clone!(
        #[strong]
        zoom_reset,
        move |_| (*zoom_reset)()
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
    scrolled_picture.add_controller(pinch);

    // Click-and-drag panning: only acts when zoomed in (otherwise scrollbars
    // have nowhere to scroll to). Snapshots scroll position on begin and
    // applies cumulative offsets on each update.
    let drag_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    drag.connect_drag_begin(clone!(
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
    drag.connect_drag_update(clone!(
        #[strong]
        scrolled_picture,
        #[strong]
        drag_start,
        move |_, off_x, off_y| {
            let (sx0, sy0) = drag_start.get();
            scrolled_picture.hadjustment().set_value(sx0 - off_x);
            scrolled_picture.vadjustment().set_value(sy0 - off_y);
        }
    ));
    scrolled_picture.add_controller(drag);

    // Double-click on the picture: zoom in 2x toward the click position.
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
                (*set_zoom)(zoom_level.get() * 2.0);
            }
        }
    ));
    scrolled_picture.add_controller(double_click);

    // Middle-click: reset zoom to 100%.
    let middle_click = gtk::GestureClick::new();
    middle_click.set_button(gtk::gdk::BUTTON_MIDDLE);
    middle_click.connect_pressed(clone!(
        #[strong]
        zoom_reset,
        move |_, _, _, _| {
            (*zoom_reset)();
        }
    ));
    scrolled_picture.add_controller(middle_click);

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

    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        details_btn,
        #[strong]
        goto_next,
        #[strong]
        nav_dir,
        #[strong]
        zoom_by,
        #[strong]
        zoom_reset,
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
                    let pos = pos_cell.get();
                    if pos > 0 {
                        pos_cell.set(pos - 1);
                        nav_dir.set(-1);
                        (*render)();
                    }
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Right) => {
                    (*goto_next)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::i) | (false, gtk::gdk::Key::I) => {
                    details_btn.set_active(!details_btn.is_active());
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Escape) => {
                    ui.nav.pop();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    page.add_controller(key_controller);

    // Ctrl+wheel zoom on the picture area, captured before the scrolled window
    // can use it for panning. Listening on both axes so trackpad two-finger
    // scrolls (which sometimes emit horizontal deltas) still trigger zoom.
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

    download.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_| {
            let pos = pos_cell.get();
            if let Some(item) = ui.grid.model.item(pos).and_downcast::<AssetObject>() {
                let asset_id = item.property::<String>("id");
                let filename = item.property::<String>("filename");
                if !asset_id.starts_with(LOCAL_ID_PREFIX) {
                    start_download(ui.clone(), asset_id, filename);
                }
            }
        }
    ));

    resolution_toggle.connect_toggled(clone!(
        #[strong]
        render,
        move |btn| {
            btn.set_label(if btn.is_active() {
                "Original"
            } else {
                "Preview"
            });
            (*render)();
        }
    ));

    ui.nav.push(&page);
    super::apply_narrow_recursive(page.upcast_ref(), ui.narrow.get());
}
